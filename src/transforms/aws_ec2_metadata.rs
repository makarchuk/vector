use std::{collections::HashSet, error, fmt, future::ready, pin::Pin, sync::Arc};

use arc_swap::ArcSwap;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{uri::PathAndQuery, Request, StatusCode, Uri};
use hyper::{body::to_bytes as body_to_bytes, Body};
use lookup::lookup_v2::{OptionalTargetPath, OwnedSegment};
use lookup::owned_value_path;
use lookup::OwnedTargetPath;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_with::serde_as;
use snafu::ResultExt as _;
use tokio::time::{sleep, Duration, Instant};
use tracing::Instrument;
use value::Kind;
use vector_config::configurable_component;
use vector_core::config::LogNamespace;

use crate::{
    config::{DataType, Input, Output, ProxyConfig, TransformConfig, TransformContext},
    event::Event,
    http::HttpClient,
    internal_events::{AwsEc2MetadataRefreshError, AwsEc2MetadataRefreshSuccessful},
    schema,
    transforms::{TaskTransform, Transform},
};

const ACCOUNT_ID_KEY: &str = "account-id";
const AMI_ID_KEY: &str = "ami-id";
const AVAILABILITY_ZONE_KEY: &str = "availability-zone";
const INSTANCE_ID_KEY: &str = "instance-id";
const INSTANCE_TYPE_KEY: &str = "instance-type";
const LOCAL_HOSTNAME_KEY: &str = "local-hostname";
const LOCAL_IPV4_KEY: &str = "local-ipv4";
const PUBLIC_HOSTNAME_KEY: &str = "public-hostname";
const PUBLIC_IPV4_KEY: &str = "public-ipv4";
const REGION_KEY: &str = "region";
const SUBNET_ID_KEY: &str = "subnet-id";
const VPC_ID_KEY: &str = "vpc-id";
const ROLE_NAME_KEY: &str = "role-name";

static AVAILABILITY_ZONE: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/placement/availability-zone"));
static LOCAL_HOSTNAME: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/local-hostname"));
static LOCAL_IPV4: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/local-ipv4"));
static PUBLIC_HOSTNAME: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/public-hostname"));
static PUBLIC_IPV4: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/public-ipv4"));
static ROLE_NAME: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/iam/security-credentials/"));
static MAC: Lazy<PathAndQuery> = Lazy::new(|| PathAndQuery::from_static("/latest/meta-data/mac"));
static DYNAMIC_DOCUMENT: Lazy<PathAndQuery> =
    Lazy::new(|| PathAndQuery::from_static("/latest/dynamic/instance-identity/document"));
static DEFAULT_FIELD_ALLOWLIST: &[&str] = &[
    AMI_ID_KEY,
    AVAILABILITY_ZONE_KEY,
    INSTANCE_ID_KEY,
    INSTANCE_TYPE_KEY,
    LOCAL_HOSTNAME_KEY,
    LOCAL_IPV4_KEY,
    PUBLIC_HOSTNAME_KEY,
    PUBLIC_IPV4_KEY,
    REGION_KEY,
    SUBNET_ID_KEY,
    VPC_ID_KEY,
    ROLE_NAME_KEY,
];
static API_TOKEN: Lazy<PathAndQuery> = Lazy::new(|| PathAndQuery::from_static("/latest/api/token"));
static TOKEN_HEADER: Lazy<Bytes> = Lazy::new(|| Bytes::from("X-aws-ec2-metadata-token"));

/// Configuration for the `aws_ec2_metadata` transform.
#[serde_as]
#[configurable_component(transform("aws_ec2_metadata"))]
#[derive(Clone, Debug, Derivative)]
#[derivative(Default)]
pub struct Ec2Metadata {
    /// Overrides the default EC2 metadata endpoint.
    #[serde(alias = "host", default = "default_endpoint")]
    #[derivative(Default(value = "default_endpoint()"))]
    endpoint: String,

    /// Sets a prefix for all event fields added by the transform.
    #[configurable(metadata(
        docs::examples = "",
        docs::examples = "ec2",
        docs::examples = "aws.ec2",
    ))]
    namespace: Option<OptionalTargetPath>,

    /// The interval between querying for updated metadata, in seconds.
    #[serde(default = "default_refresh_interval_secs")]
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[derivative(Default(value = "default_refresh_interval_secs()"))]
    refresh_interval_secs: Duration,

    /// A list of metadata fields to include in each transformed event.
    #[serde(default = "default_fields")]
    #[derivative(Default(value = "default_fields()"))]
    #[configurable(metadata(docs::examples = "instance-id", docs::examples = "local-hostname",))]
    fields: Vec<String>,

    /// The timeout for querying the EC2 metadata endpoint, in seconds.
    #[serde(default = "default_refresh_timeout_secs")]
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[derivative(Default(value = "default_refresh_timeout_secs()"))]
    refresh_timeout_secs: Duration,

    #[configurable(derived)]
    #[serde(
        default,
        skip_serializing_if = "crate::serde::skip_serializing_if_default"
    )]
    proxy: ProxyConfig,

    /// Requires the transform to be able to successfully query the EC2 metadata before Vector can start.
    #[serde(default = "default_required")]
    #[derivative(Default(value = "default_required()"))]
    required: bool,
}

fn default_endpoint() -> String {
    String::from("http://169.254.169.254")
}

const fn default_refresh_interval_secs() -> Duration {
    Duration::from_secs(10)
}

const fn default_refresh_timeout_secs() -> Duration {
    Duration::from_secs(1)
}

fn default_fields() -> Vec<String> {
    DEFAULT_FIELD_ALLOWLIST
        .iter()
        .map(|s| s.to_string())
        .collect()
}

const fn default_required() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct Ec2MetadataTransform {
    state: Arc<ArcSwap<Vec<(MetadataKey, Bytes)>>>,
}

#[derive(Debug, Clone)]
struct MetadataKey {
    log_path: OwnedTargetPath,
    metric_tag: String,
}

#[derive(Debug)]
struct Keys {
    account_id_key: MetadataKey,
    ami_id_key: MetadataKey,
    availability_zone_key: MetadataKey,
    instance_id_key: MetadataKey,
    instance_type_key: MetadataKey,
    local_hostname_key: MetadataKey,
    local_ipv4_key: MetadataKey,
    public_hostname_key: MetadataKey,
    public_ipv4_key: MetadataKey,
    region_key: MetadataKey,
    subnet_id_key: MetadataKey,
    vpc_id_key: MetadataKey,
    role_name_key: MetadataKey,
}

impl_generate_config_from_default!(Ec2Metadata);

#[async_trait::async_trait]
impl TransformConfig for Ec2Metadata {
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        let state = Arc::new(ArcSwap::new(Arc::new(vec![])));

        let keys = Keys::new(self.namespace.clone());
        let host = Uri::from_maybe_shared(self.endpoint.clone()).unwrap();
        let refresh_interval = self.refresh_interval_secs;
        let fields = self.fields.clone();
        let refresh_timeout = self.refresh_timeout_secs;
        let required = self.required;

        let proxy = ProxyConfig::merge_with_env(&context.globals.proxy, &self.proxy);
        let http_client = HttpClient::new(None, &proxy)?;

        let mut client = MetadataClient::new(
            http_client,
            host,
            keys,
            Arc::clone(&state),
            refresh_interval,
            refresh_timeout,
            fields,
        );

        // If initial metadata is not required, log and proceed. Otherwise return error.
        if let Err(error) = client.refresh_metadata().await {
            if required {
                return Err(error);
            } else {
                emit!(AwsEc2MetadataRefreshError { error });
            }
        }

        tokio::spawn(
            async move {
                client.run().await;
            }
            // TODO: Once #1338 is done we can fetch the current span
            .instrument(info_span!("aws_ec2_metadata: worker").or_current()),
        );

        Ok(Transform::event_task(Ec2MetadataTransform { state }))
    }

    fn input(&self) -> Input {
        Input::new(DataType::Metric | DataType::Log)
    }

    fn outputs(&self, merged_definition: &schema::Definition, _: LogNamespace) -> Vec<Output> {
        let added_keys = Keys::new(self.namespace.clone());

        let paths = [
            &added_keys.account_id_key.log_path,
            &added_keys.ami_id_key.log_path,
            &added_keys.availability_zone_key.log_path,
            &added_keys.instance_id_key.log_path,
            &added_keys.instance_type_key.log_path,
            &added_keys.local_hostname_key.log_path,
            &added_keys.local_ipv4_key.log_path,
            &added_keys.public_hostname_key.log_path,
            &added_keys.public_ipv4_key.log_path,
            &added_keys.region_key.log_path,
            &added_keys.subnet_id_key.log_path,
            &added_keys.vpc_id_key.log_path,
            &added_keys.role_name_key.log_path,
        ];

        let mut schema_definition = merged_definition.clone();

        for path in paths {
            schema_definition =
                schema_definition.with_field(path, Kind::bytes().or_undefined(), None);
        }

        vec![Output::default(DataType::Metric | DataType::Log)
            .with_schema_definition(schema_definition)]
    }
}

impl TaskTransform<Event> for Ec2MetadataTransform {
    fn transform(
        self: Box<Self>,
        task: Pin<Box<dyn Stream<Item = Event> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Event> + Send>>
    where
        Self: 'static,
    {
        let mut inner = self;
        Box::pin(task.filter_map(move |event| ready(Some(inner.transform_one(event)))))
    }
}

impl Ec2MetadataTransform {
    fn transform_one(&mut self, mut event: Event) -> Event {
        let state = self.state.load();
        match event {
            Event::Log(ref mut log) => {
                state.iter().for_each(|(k, v)| {
                    log.insert(&k.log_path, v.clone());
                });
            }
            Event::Metric(ref mut metric) => {
                state.iter().for_each(|(k, v)| {
                    metric
                        .replace_tag(k.metric_tag.clone(), String::from_utf8_lossy(v).to_string());
                });
            }
            Event::Trace(_) => panic!("Traces are not supported."),
        }
        event
    }
}

struct MetadataClient {
    client: HttpClient<Body>,
    host: Uri,
    token: Option<(Bytes, Instant)>,
    keys: Keys,
    state: Arc<ArcSwap<Vec<(MetadataKey, Bytes)>>>,
    refresh_interval: Duration,
    refresh_timeout: Duration,
    fields: HashSet<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // deserialize all fields
struct IdentityDocument {
    account_id: String,
    architecture: String,
    image_id: String,
    instance_id: String,
    instance_type: String,
    private_ip: String,
    region: String,
    version: String,
}

impl MetadataClient {
    pub fn new(
        client: HttpClient<Body>,
        host: Uri,
        keys: Keys,
        state: Arc<ArcSwap<Vec<(MetadataKey, Bytes)>>>,
        refresh_interval: Duration,
        refresh_timeout: Duration,
        fields: Vec<String>,
    ) -> Self {
        Self {
            client,
            host,
            token: None,
            keys,
            state,
            refresh_interval,
            refresh_timeout,
            fields: fields.into_iter().collect(),
        }
    }

    async fn run(&mut self) {
        loop {
            match self.refresh_metadata().await {
                Ok(_) => {
                    emit!(AwsEc2MetadataRefreshSuccessful);
                }
                Err(error) => {
                    emit!(AwsEc2MetadataRefreshError { error });
                }
            }

            sleep(self.refresh_interval).await;
        }
    }

    pub async fn get_token(&mut self) -> Result<Bytes, crate::Error> {
        if let Some((token, next_refresh)) = self.token.clone() {
            // If the next refresh is greater (in the future) than
            // the current time we can return the token since its still valid
            // otherwise lets refresh it.
            if next_refresh > Instant::now() {
                return Ok(token);
            }
        }

        let mut parts = self.host.clone().into_parts();
        parts.path_and_query = Some(API_TOKEN.clone());
        let uri = Uri::from_parts(parts)?;

        let req = Request::put(uri)
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .body(Body::empty())?;

        let res = tokio::time::timeout(self.refresh_timeout, self.client.send(req))
            .await?
            .map_err(crate::Error::from)
            .and_then(|res| match res.status() {
                StatusCode::OK => Ok(res),
                status_code => Err(UnexpectedHttpStatusError {
                    status: status_code,
                }
                .into()),
            })?;

        let token = body_to_bytes(res.into_body()).await?;

        let next_refresh = Instant::now() + Duration::from_secs(21600);
        self.token = Some((token.clone(), next_refresh));

        Ok(token)
    }

    pub async fn get_document(&mut self) -> Result<Option<IdentityDocument>, crate::Error> {
        self.get_metadata(&DYNAMIC_DOCUMENT)
            .await?
            .map(|body| {
                serde_json::from_slice(&body[..])
                    .context(ParseIdentityDocumentSnafu {})
                    .map_err(Into::into)
            })
            .transpose()
    }

    pub async fn refresh_metadata(&mut self) -> Result<(), crate::Error> {
        let mut new_state = vec![];

        // Fetch all resources, _then_ add them to the state map.
        if let Some(document) = self.get_document().await? {
            if self.fields.contains(ACCOUNT_ID_KEY) {
                new_state.push((self.keys.account_id_key.clone(), document.account_id.into()));
            }

            if self.fields.contains(AMI_ID_KEY) {
                new_state.push((self.keys.ami_id_key.clone(), document.image_id.into()));
            }

            if self.fields.contains(INSTANCE_ID_KEY) {
                new_state.push((
                    self.keys.instance_id_key.clone(),
                    document.instance_id.into(),
                ));
            }

            if self.fields.contains(INSTANCE_TYPE_KEY) {
                new_state.push((
                    self.keys.instance_type_key.clone(),
                    document.instance_type.into(),
                ));
            }

            if self.fields.contains(REGION_KEY) {
                new_state.push((self.keys.region_key.clone(), document.region.into()));
            }

            if self.fields.contains(AVAILABILITY_ZONE_KEY) {
                if let Some(availability_zone) = self.get_metadata(&AVAILABILITY_ZONE).await? {
                    new_state.push((self.keys.availability_zone_key.clone(), availability_zone));
                }
            }

            if self.fields.contains(LOCAL_HOSTNAME_KEY) {
                if let Some(local_hostname) = self.get_metadata(&LOCAL_HOSTNAME).await? {
                    new_state.push((self.keys.local_hostname_key.clone(), local_hostname));
                }
            }

            if self.fields.contains(LOCAL_IPV4_KEY) {
                if let Some(local_ipv4) = self.get_metadata(&LOCAL_IPV4).await? {
                    new_state.push((self.keys.local_ipv4_key.clone(), local_ipv4));
                }
            }

            if self.fields.contains(PUBLIC_HOSTNAME_KEY) {
                if let Some(public_hostname) = self.get_metadata(&PUBLIC_HOSTNAME).await? {
                    new_state.push((self.keys.public_hostname_key.clone(), public_hostname));
                }
            }

            if self.fields.contains(PUBLIC_IPV4_KEY) {
                if let Some(public_ipv4) = self.get_metadata(&PUBLIC_IPV4).await? {
                    new_state.push((self.keys.public_ipv4_key.clone(), public_ipv4));
                }
            }

            if self.fields.contains(SUBNET_ID_KEY) || self.fields.contains(VPC_ID_KEY) {
                if let Some(mac) = self.get_metadata(&MAC).await? {
                    let mac = String::from_utf8_lossy(&mac[..]);

                    if self.fields.contains(SUBNET_ID_KEY) {
                        let subnet_path = format!(
                            "/latest/meta-data/network/interfaces/macs/{}/subnet-id",
                            mac
                        );

                        let subnet_path = subnet_path.parse().context(ParsePathSnafu {
                            value: subnet_path.clone(),
                        })?;

                        if let Some(subnet_id) = self.get_metadata(&subnet_path).await? {
                            new_state.push((self.keys.subnet_id_key.clone(), subnet_id));
                        }
                    }

                    if self.fields.contains(VPC_ID_KEY) {
                        let vpc_path =
                            format!("/latest/meta-data/network/interfaces/macs/{}/vpc-id", mac);

                        let vpc_path = vpc_path.parse().context(ParsePathSnafu {
                            value: vpc_path.clone(),
                        })?;

                        if let Some(vpc_id) = self.get_metadata(&vpc_path).await? {
                            new_state.push((self.keys.vpc_id_key.clone(), vpc_id));
                        }
                    }
                }
            }

            if self.fields.contains(ROLE_NAME_KEY) {
                if let Some(role_names) = self.get_metadata(&ROLE_NAME).await? {
                    let role_names = String::from_utf8_lossy(&role_names[..]);

                    for (i, role_name) in role_names.lines().enumerate() {
                        new_state.push((
                            MetadataKey {
                                log_path: self
                                    .keys
                                    .role_name_key
                                    .log_path
                                    .with_index_appended(i as isize),
                                metric_tag: format!(
                                    "{}[{}]",
                                    self.keys.role_name_key.metric_tag, i
                                ),
                            },
                            role_name.to_string().into(),
                        ));
                    }
                }
            }

            self.state.store(Arc::new(new_state));
        }

        Ok(())
    }

    async fn get_metadata(&mut self, path: &PathAndQuery) -> Result<Option<Bytes>, crate::Error> {
        let token = self
            .get_token()
            .await
            .with_context(|_| FetchTokenSnafu {})?;

        let mut parts = self.host.clone().into_parts();

        parts.path_and_query = Some(path.clone());

        let uri = Uri::from_parts(parts)?;

        debug!(message = "Sending metadata request.", %uri);

        let req = Request::get(uri)
            .header(TOKEN_HEADER.as_ref(), token.as_ref())
            .body(Body::empty())?;

        match tokio::time::timeout(self.refresh_timeout, self.client.send(req))
            .await?
            .map_err(crate::Error::from)
            .and_then(|res| match res.status() {
                StatusCode::OK => Ok(Some(res)),
                StatusCode::NOT_FOUND => Ok(None),
                status_code => Err(UnexpectedHttpStatusError {
                    status: status_code,
                }
                .into()),
            })? {
            Some(res) => {
                let body = body_to_bytes(res.into_body()).await?;
                Ok(Some(body))
            }
            None => Ok(None),
        }
    }
}

// This creates a simplified string from the namespace. Since the namespace is technically
// a target path, it can contain syntax that is undesirable for a metric tag (such as prefix, quotes, etc)
// This is mainly used for backwards compatibility.
// see: https://github.com/vectordotdev/vector/issues/14931
fn create_metric_namespace(namespace: &OwnedTargetPath) -> String {
    let mut output = String::new();
    for segment in &namespace.path.segments {
        if !output.is_empty() {
            output += ".";
        }
        match segment {
            OwnedSegment::Field(field) => {
                output += field;
            }
            OwnedSegment::Index(i) => {
                output += &i.to_string();
            }
            OwnedSegment::Coalesce(fields) => {
                if let Some(first) = fields.first() {
                    output += first;
                }
            }
        }
    }
    output
}

fn create_key(namespace: &Option<OwnedTargetPath>, key: &str) -> MetadataKey {
    if let Some(namespace) = namespace {
        MetadataKey {
            log_path: namespace.with_field_appended(key),
            metric_tag: format!("{}.{}", create_metric_namespace(namespace), key),
        }
    } else {
        MetadataKey {
            log_path: OwnedTargetPath::event(owned_value_path!(key)),
            metric_tag: key.to_owned(),
        }
    }
}

impl Keys {
    pub fn new(namespace: Option<OptionalTargetPath>) -> Self {
        let namespace = namespace.and_then(|namespace| namespace.path);

        Keys {
            account_id_key: create_key(&namespace, ACCOUNT_ID_KEY),
            ami_id_key: create_key(&namespace, AMI_ID_KEY),
            availability_zone_key: create_key(&namespace, AVAILABILITY_ZONE_KEY),
            instance_id_key: create_key(&namespace, INSTANCE_ID_KEY),
            instance_type_key: create_key(&namespace, INSTANCE_TYPE_KEY),
            local_hostname_key: create_key(&namespace, LOCAL_HOSTNAME_KEY),
            local_ipv4_key: create_key(&namespace, LOCAL_IPV4_KEY),
            public_hostname_key: create_key(&namespace, PUBLIC_HOSTNAME_KEY),
            public_ipv4_key: create_key(&namespace, PUBLIC_IPV4_KEY),
            region_key: create_key(&namespace, REGION_KEY),
            subnet_id_key: create_key(&namespace, SUBNET_ID_KEY),
            vpc_id_key: create_key(&namespace, VPC_ID_KEY),
            role_name_key: create_key(&namespace, ROLE_NAME_KEY),
        }
    }
}

#[derive(Debug)]
struct UnexpectedHttpStatusError {
    status: http::StatusCode,
}

impl fmt::Display for UnexpectedHttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "got unexpected status code: {}", self.status)
    }
}

impl error::Error for UnexpectedHttpStatusError {}

#[derive(Debug, snafu::Snafu)]
enum Ec2MetadataError {
    #[snafu(display("Unable to fetch metadata authentication token: {}.", source))]
    FetchToken { source: crate::Error },
    #[snafu(display("Unable to parse identity document: {}.", source))]
    ParseIdentityDocument { source: serde_json::Error },
    #[snafu(display("Unable to parse metadata path {}, {}.", value, source))]
    ParsePath {
        value: String,
        source: http::uri::InvalidUri,
    },
}

#[cfg(feature = "aws-ec2-metadata-integration-tests")]
#[cfg(test)]
mod integration_tests {
    use lookup::lookup_v2::{OwnedSegment, OwnedValuePath};
    use lookup::{event_path, PathPrefix};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::{
        event::{metric, LogEvent, Metric},
        test_util::{components::assert_transform_compliance, next_addr},
        transforms::test::create_topology,
    };
    use warp::Filter;

    fn ec2_metadata_address() -> String {
        std::env::var("EC2_METADATA_ADDRESS").unwrap_or_else(|_| "http://localhost:8111".into())
    }

    fn expected_log_fields() -> Vec<(OwnedValuePath, &'static str)> {
        vec![
            (
                vec![OwnedSegment::field(AVAILABILITY_ZONE_KEY)].into(),
                "ww-region-1a",
            ),
            (
                vec![OwnedSegment::field(PUBLIC_IPV4_KEY)].into(),
                "192.1.1.1",
            ),
            (
                vec![OwnedSegment::field(PUBLIC_HOSTNAME_KEY)].into(),
                "mock-public-hostname",
            ),
            (
                vec![OwnedSegment::field(LOCAL_IPV4_KEY)].into(),
                "192.1.1.2",
            ),
            (
                vec![OwnedSegment::field(LOCAL_HOSTNAME_KEY)].into(),
                "mock-hostname",
            ),
            (
                vec![OwnedSegment::field(INSTANCE_ID_KEY)].into(),
                "i-096fba6d03d36d262",
            ),
            (
                vec![OwnedSegment::field(ACCOUNT_ID_KEY)].into(),
                "071959437513",
            ),
            (
                vec![OwnedSegment::field(AMI_ID_KEY)].into(),
                "ami-05f27d4d6770a43d2",
            ),
            (
                vec![OwnedSegment::field(INSTANCE_TYPE_KEY)].into(),
                "t2.micro",
            ),
            (vec![OwnedSegment::field(REGION_KEY)].into(), "us-east-1"),
            (vec![OwnedSegment::field(VPC_ID_KEY)].into(), "mock-vpc-id"),
            (
                vec![OwnedSegment::field(SUBNET_ID_KEY)].into(),
                "mock-subnet-id",
            ),
            (owned_value_path!("role-name", 0), "mock-user"),
        ]
    }

    fn expected_metric_fields() -> Vec<(&'static str, &'static str)> {
        vec![
            (AVAILABILITY_ZONE_KEY, "ww-region-1a"),
            (PUBLIC_IPV4_KEY, "192.1.1.1"),
            (PUBLIC_HOSTNAME_KEY, "mock-public-hostname"),
            (LOCAL_IPV4_KEY, "192.1.1.2"),
            (LOCAL_HOSTNAME_KEY, "mock-hostname"),
            (INSTANCE_ID_KEY, "i-096fba6d03d36d262"),
            (ACCOUNT_ID_KEY, "071959437513"),
            (AMI_ID_KEY, "ami-05f27d4d6770a43d2"),
            (INSTANCE_TYPE_KEY, "t2.micro"),
            (REGION_KEY, "us-east-1"),
            (VPC_ID_KEY, "mock-vpc-id"),
            (SUBNET_ID_KEY, "mock-subnet-id"),
            ("role-name[0]", "mock-user"),
        ]
    }

    fn make_metric() -> Metric {
        Metric::new(
            "event",
            metric::MetricKind::Incremental,
            metric::MetricValue::Counter { value: 1.0 },
        )
    }

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<Ec2Metadata>();
    }

    #[tokio::test]
    async fn enrich_log() {
        assert_transform_compliance(async {
            let mut fields = default_fields();
            fields.extend(vec![String::from(ACCOUNT_ID_KEY)].into_iter());

            let transform_config = Ec2Metadata {
                endpoint: ec2_metadata_address(),
                fields,
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) =
                create_topology(ReceiverStream::new(rx), transform_config).await;

            // We need to sleep to let the background task fetch the data.
            sleep(Duration::from_secs(1)).await;

            let log = LogEvent::default();
            let mut expected_log = log.clone();
            for (k, v) in expected_log_fields().iter().cloned() {
                expected_log.insert((PathPrefix::Event, &k), v);
            }

            tx.send(log.into()).await.unwrap();

            let event = out.recv().await.unwrap();
            assert_eq!(event.into_log(), expected_log);

            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout() {
        let addr = next_addr();

        async fn sleepy() -> Result<impl warp::Reply, std::convert::Infallible> {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok("I waited 3 seconds!")
        }

        let slow = warp::any().and_then(sleepy);
        let server = warp::serve(slow).bind(addr);
        let _server = tokio::spawn(server);

        let config = Ec2Metadata {
            endpoint: format!("http://{}", addr),
            refresh_timeout_secs: Duration::from_secs(1),
            ..Default::default()
        };

        match config.build(&TransformContext::default()).await {
            Ok(_) => panic!("expected timeout failure"),
            // cannot create tokio::time::error::Elapsed to compare with since constructor is
            // private
            Err(err) => assert_eq!(
                err.to_string(),
                "Unable to fetch metadata authentication token: deadline has elapsed."
            ),
        }
    }

    // validates the configuration setting 'required'=false allows vector to run
    #[tokio::test(flavor = "multi_thread")]
    async fn not_required() {
        let addr = next_addr();

        async fn sleepy() -> Result<impl warp::Reply, std::convert::Infallible> {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok("I waited 3 seconds!")
        }

        let slow = warp::any().and_then(sleepy);
        let server = warp::serve(slow).bind(addr);
        let _server = tokio::spawn(server);

        let config = Ec2Metadata {
            endpoint: format!("http://{}", addr),
            refresh_timeout_secs: Duration::from_secs(1),
            required: false,
            ..Default::default()
        };

        assert!(
            config.build(&TransformContext::default()).await.is_ok(),
            "expected no failure because 'required' config value set to false"
        );
    }

    #[tokio::test]
    async fn enrich_metric() {
        assert_transform_compliance(async {
            let mut fields = default_fields();
            fields.extend(vec![String::from(ACCOUNT_ID_KEY)].into_iter());

            let transform_config = Ec2Metadata {
                endpoint: ec2_metadata_address(),
                fields,
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) =
                create_topology(ReceiverStream::new(rx), transform_config).await;

            // We need to sleep to let the background task fetch the data.
            sleep(Duration::from_secs(1)).await;

            let metric = make_metric();
            let mut expected_metric = metric.clone();
            for (k, v) in expected_metric_fields().iter() {
                expected_metric.replace_tag(k.to_string(), v.to_string());
            }

            tx.send(metric.into()).await.unwrap();

            let event = out.recv().await.unwrap();
            assert_eq!(event.into_metric(), expected_metric);

            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None);
        })
        .await;
    }

    #[tokio::test]
    async fn fields_log() {
        assert_transform_compliance(async {
            let transform_config = Ec2Metadata {
                endpoint: ec2_metadata_address(),
                fields: vec![PUBLIC_IPV4_KEY.into(), REGION_KEY.into()],
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) =
                create_topology(ReceiverStream::new(rx), transform_config).await;

            // We need to sleep to let the background task fetch the data.
            sleep(Duration::from_secs(1)).await;

            let log = LogEvent::default();
            let mut expected_log = log.clone();
            expected_log.insert(format!("\"{}\"", PUBLIC_IPV4_KEY).as_str(), "192.1.1.1");
            expected_log.insert(format!("\"{}\"", REGION_KEY).as_str(), "us-east-1");

            tx.send(log.into()).await.unwrap();

            let event = out.recv().await.unwrap();
            assert_eq!(event.into_log(), expected_log);

            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None);
        })
        .await;
    }

    #[tokio::test]
    async fn fields_metric() {
        assert_transform_compliance(async {
            let transform_config = Ec2Metadata {
                endpoint: ec2_metadata_address(),
                fields: vec![PUBLIC_IPV4_KEY.into(), REGION_KEY.into()],
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) =
                create_topology(ReceiverStream::new(rx), transform_config).await;

            // We need to sleep to let the background task fetch the data.
            sleep(Duration::from_secs(1)).await;

            let metric = make_metric();
            let mut expected_metric = metric.clone();
            expected_metric.replace_tag(PUBLIC_IPV4_KEY.to_string(), "192.1.1.1".to_string());
            expected_metric.replace_tag(REGION_KEY.to_string(), "us-east-1".to_string());

            tx.send(metric.into()).await.unwrap();

            let event = out.recv().await.unwrap();
            assert_eq!(event.into_metric(), expected_metric);

            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None);
        })
        .await;
    }

    #[tokio::test]
    async fn namespace_log() {
        {
            assert_transform_compliance(async {
                let transform_config = Ec2Metadata {
                    endpoint: ec2_metadata_address(),
                    namespace: Some(
                        OwnedTargetPath::event(owned_value_path!("ec2", "metadata")).into(),
                    ),
                    ..Default::default()
                };

                let (tx, rx) = mpsc::channel(1);
                let (topology, mut out) =
                    create_topology(ReceiverStream::new(rx), transform_config).await;

                // We need to sleep to let the background task fetch the data.
                sleep(Duration::from_secs(1)).await;

                let log = LogEvent::default();

                tx.send(log.into()).await.unwrap();

                let event = out.recv().await.unwrap();

                assert_eq!(
                    event.as_log().get("ec2.metadata.\"availability-zone\""),
                    Some(&"ww-region-1a".into())
                );

                drop(tx);
                topology.stop().await;
                assert_eq!(out.recv().await, None);
            })
            .await;
        }

        {
            assert_transform_compliance(async {
                // Set an empty namespace to ensure we don't prepend one.
                let transform_config = Ec2Metadata {
                    endpoint: ec2_metadata_address(),
                    namespace: Some(OptionalTargetPath::none()),
                    ..Default::default()
                };

                let (tx, rx) = mpsc::channel(1);
                let (topology, mut out) =
                    create_topology(ReceiverStream::new(rx), transform_config).await;

                // We need to sleep to let the background task fetch the data.
                sleep(Duration::from_secs(1)).await;

                let log = LogEvent::default();

                tx.send(log.into()).await.unwrap();

                let event = out.recv().await.unwrap();
                assert_eq!(
                    event.as_log().get(event_path!(AVAILABILITY_ZONE_KEY)),
                    Some(&"ww-region-1a".into())
                );

                drop(tx);
                topology.stop().await;
                assert_eq!(out.recv().await, None);
            })
            .await;
        }
    }

    #[tokio::test]
    async fn namespace_metric() {
        {
            assert_transform_compliance(async {
                let transform_config = Ec2Metadata {
                    endpoint: ec2_metadata_address(),
                    namespace: Some(
                        OwnedTargetPath::event(owned_value_path!("ec2", "metadata")).into(),
                    ),
                    ..Default::default()
                };

                let (tx, rx) = mpsc::channel(1);
                let (topology, mut out) =
                    create_topology(ReceiverStream::new(rx), transform_config).await;

                // We need to sleep to let the background task fetch the data.
                sleep(Duration::from_secs(1)).await;

                let metric = make_metric();

                tx.send(metric.into()).await.unwrap();

                let event = out.recv().await.unwrap();
                assert_eq!(
                    event
                        .as_metric()
                        .tag_value("ec2.metadata.availability-zone"),
                    Some("ww-region-1a".to_string())
                );

                drop(tx);
                topology.stop().await;
                assert_eq!(out.recv().await, None);
            })
            .await;
        }

        {
            assert_transform_compliance(async {
                // Set an empty namespace to ensure we don't prepend one.
                let transform_config = Ec2Metadata {
                    endpoint: ec2_metadata_address(),
                    namespace: Some(OptionalTargetPath::none()),
                    ..Default::default()
                };

                let (tx, rx) = mpsc::channel(1);
                let (topology, mut out) =
                    create_topology(ReceiverStream::new(rx), transform_config).await;

                // We need to sleep to let the background task fetch the data.
                sleep(Duration::from_secs(1)).await;

                let metric = make_metric();

                tx.send(metric.into()).await.unwrap();

                let event = out.recv().await.unwrap();
                assert_eq!(
                    event.as_metric().tag_value(AVAILABILITY_ZONE_KEY),
                    Some("ww-region-1a".to_string())
                );

                drop(tx);
                topology.stop().await;
                assert_eq!(out.recv().await, None);
            })
            .await;
        }
    }
}
