use codecs::JsonSerializerConfig;
use vector_common::sensitive_string::SensitiveString;
use vector_config::configurable_component;

use super::host_key;
use crate::{
    codecs::EncodingConfig,
    config::{AcknowledgementsConfig, DataType, GenerateConfig, Input, SinkConfig, SinkContext},
    sinks::{
        splunk_hec::{
            common::{
                acknowledgements::HecClientAcknowledgementsConfig, timestamp_key, EndpointTarget,
                SplunkHecDefaultBatchSettings,
            },
            logs::config::HecLogsSinkConfig,
        },
        util::{BatchConfig, Compression, TowerRequestConfig},
        Healthcheck, VectorSink,
    },
    template::Template,
    tls::TlsConfig,
};

const HOST: &str = "https://cloud.humio.com";

/// Configuration for the `humio_logs` sink.
#[configurable_component(sink("humio_logs"))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct HumioLogsConfig {
    /// The Humio ingestion token.
    pub(super) token: SensitiveString,

    /// The base URL of the Humio instance.
    #[serde(alias = "host")]
    pub(super) endpoint: Option<String>,

    /// The source of events sent to this sink.
    ///
    /// Typically the filename the logs originated from. Maps to `@source` in Humio.
    pub(super) source: Option<Template>,

    #[configurable(derived)]
    pub(super) encoding: EncodingConfig,

    /// The type of events sent to this sink. Humio uses this as the name of the parser to use to ingest the data.
    ///
    /// If unset, Humio will default it to none.
    pub(super) event_type: Option<Template>,

    /// Overrides the name of the log field used to grab the hostname to send to Humio.
    ///
    /// By default, the [global `log_schema.host_key` option][global_host_key] is used.
    ///
    /// [global_host_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.host_key
    #[serde(default = "host_key")]
    pub(super) host_key: String,

    /// Event fields to be added to Humio’s extra fields.
    ///
    /// Can be used to tag events by specifying fields starting with `#`.
    ///
    /// For more information, see [Humio’s Format of Data][humio_data_format].
    ///
    /// [humio_data_format]: https://docs.humio.com/integrations/data-shippers/hec/#format-of-data
    #[serde(default)]
    pub(super) indexed_fields: Vec<String>,

    /// Optional name of the repository to ingest into.
    ///
    /// In public-facing APIs, this must (if present) be equal to the repository used to create the ingest token used for authentication.
    ///
    /// In private cluster setups, Humio can be configured to allow these to be different.
    ///
    /// For more information, see [Humio’s Format of Data][humio_data_format].
    ///
    /// [humio_data_format]: https://docs.humio.com/integrations/data-shippers/hec/#format-of-data
    #[serde(default)]
    pub(super) index: Option<Template>,

    #[configurable(derived)]
    #[serde(default)]
    pub(super) compression: Compression,

    #[configurable(derived)]
    #[serde(default)]
    pub(super) request: TowerRequestConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub(super) batch: BatchConfig<SplunkHecDefaultBatchSettings>,

    #[configurable(derived)]
    pub(super) tls: Option<TlsConfig>,

    /// Overrides the name of the log field used to grab the nanosecond-enabled timestamp to send to Humio.
    ///
    /// By default, `@timestamp.nanos` is used.
    #[serde(default = "timestamp_nanos_key")]
    pub(super) timestamp_nanos_key: Option<String>,

    #[configurable(derived)]
    #[serde(
        default,
        deserialize_with = "crate::serde::bool_or_struct",
        skip_serializing_if = "crate::serde::skip_serializing_if_default"
    )]
    pub acknowledgements: AcknowledgementsConfig,

    /// Overrides the name of the log field used to grab the timestamp to send to Humio.
    ///
    /// By default, the [global `log_schema.timestamp_key` option][global_timestamp_key] is used.
    ///
    /// [global_timestamp_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.timestamp_key
    #[serde(default = "timestamp_key")]
    pub(super) timestamp_key: String,
}

pub fn timestamp_nanos_key() -> Option<String> {
    Some("@timestamp.nanos".to_string())
}

impl GenerateConfig for HumioLogsConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            token: "${HUMIO_TOKEN}".to_owned().into(),
            endpoint: None,
            source: None,
            encoding: JsonSerializerConfig::new().into(),
            event_type: None,
            indexed_fields: vec![],
            index: None,
            host_key: host_key(),
            compression: Compression::default(),
            request: TowerRequestConfig::default(),
            batch: BatchConfig::default(),
            tls: None,
            timestamp_nanos_key: None,
            acknowledgements: Default::default(),
            timestamp_key: timestamp_key(),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
impl SinkConfig for HumioLogsConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        self.build_hec_config().build(cx).await
    }

    fn input(&self) -> Input {
        Input::new(self.encoding.config().input_type() & DataType::Log)
    }

    fn acknowledgements(&self) -> &AcknowledgementsConfig {
        &self.acknowledgements
    }
}

impl HumioLogsConfig {
    fn build_hec_config(&self) -> HecLogsSinkConfig {
        let endpoint = self.endpoint.clone().unwrap_or_else(|| HOST.to_string());

        HecLogsSinkConfig {
            default_token: self.token.clone(),
            endpoint,
            host_key: self.host_key.clone(),
            indexed_fields: self.indexed_fields.clone(),
            index: self.index.clone(),
            sourcetype: self.event_type.clone(),
            source: self.source.clone(),
            timestamp_nanos_key: self.timestamp_nanos_key.clone(),
            encoding: self.encoding.clone(),
            compression: self.compression,
            batch: self.batch,
            request: self.request,
            tls: self.tls.clone(),
            acknowledgements: HecClientAcknowledgementsConfig {
                indexer_acknowledgements_enabled: false,
                ..Default::default()
            },
            timestamp_key: timestamp_key(),
            endpoint_target: EndpointTarget::Event,
            auto_extract_timestamp: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<HumioLogsConfig>();
    }
}

#[cfg(test)]
#[cfg(feature = "humio-integration-tests")]
mod integration_tests {
    use std::{collections::HashMap, convert::TryFrom};

    use chrono::{TimeZone, Utc};
    use futures::{future::ready, stream};
    use indoc::indoc;
    use serde::Deserialize;
    use serde_json::{json, Value as JsonValue};
    use tokio::time::Duration;

    use super::*;
    use crate::{
        config::{log_schema, SinkConfig, SinkContext},
        event::LogEvent,
        sinks::util::Compression,
        test_util::{
            components::{run_and_assert_sink_compliance, HTTP_SINK_TAGS},
            random_string,
        },
    };

    fn humio_address() -> String {
        std::env::var("HUMIO_ADDRESS").unwrap_or_else(|_| "http://localhost:8080".into())
    }

    #[tokio::test]
    async fn humio_insert_message() {
        wait_ready().await;

        let cx = SinkContext::new_test();

        let repo = create_repository().await;

        let config = config(&repo.default_ingest_token);

        let (sink, _) = config.build(cx).await.unwrap();

        let message = random_string(100);
        let host = "192.168.1.1".to_string();
        let mut event = LogEvent::from(message.clone());
        event.insert(log_schema().host_key(), host.clone());

        let ts = Utc.timestamp_nanos(Utc::now().timestamp_millis() * 1_000_000 + 132_456);
        event.insert(log_schema().timestamp_key(), ts);

        run_and_assert_sink_compliance(sink, stream::once(ready(event)), &HTTP_SINK_TAGS).await;

        let entry = find_entry(repo.name.as_str(), message.as_str()).await;

        assert_eq!(
            message,
            entry
                .fields
                .get("message")
                .expect("no message key")
                .as_str()
                .unwrap()
        );
        assert!(
            entry.error.is_none(),
            "Humio encountered an error parsing this message: {}",
            entry
                .error_msg
                .unwrap_or_else(|| "no error message".to_string())
        );
        assert_eq!(Some(host), entry.host);
        assert_eq!("132456", entry.timestamp_nanos);
    }

    #[tokio::test]
    async fn humio_insert_source() {
        wait_ready().await;

        let cx = SinkContext::new_test();

        let repo = create_repository().await;

        let mut config = config(&repo.default_ingest_token);
        config.source = Template::try_from("/var/log/syslog".to_string()).ok();

        let (sink, _) = config.build(cx).await.unwrap();

        let message = random_string(100);
        let event = LogEvent::from(message.clone());
        run_and_assert_sink_compliance(sink, stream::once(ready(event)), &HTTP_SINK_TAGS).await;

        let entry = find_entry(repo.name.as_str(), message.as_str()).await;

        assert_eq!(entry.source, Some("/var/log/syslog".to_owned()));
        assert!(
            entry.error.is_none(),
            "Humio encountered an error parsing this message: {}",
            entry
                .error_msg
                .unwrap_or_else(|| "no error message".to_string())
        );
    }

    #[tokio::test]
    async fn humio_type() {
        wait_ready().await;

        let repo = create_repository().await;

        // sets type
        {
            let mut config = config(&repo.default_ingest_token);
            config.event_type = Template::try_from("json".to_string()).ok();

            let (sink, _) = config.build(SinkContext::new_test()).await.unwrap();

            let message = random_string(100);
            let mut event = LogEvent::from(message.clone());
            // Humio expects to find an @timestamp field for JSON lines
            // https://docs.humio.com/ingesting-data/parsers/built-in-parsers/#json
            event.insert("@timestamp", Utc::now().to_rfc3339());

            run_and_assert_sink_compliance(sink, stream::once(ready(event)), &HTTP_SINK_TAGS).await;

            let entry = find_entry(repo.name.as_str(), message.as_str()).await;

            assert_eq!(entry.humio_type, "json");
            assert!(
                entry.error.is_none(),
                "Humio encountered an error parsing this message: {}",
                entry
                    .error_msg
                    .unwrap_or_else(|| "no error message".to_string())
            );
        }

        // defaults to none
        {
            let config = config(&repo.default_ingest_token);

            let (sink, _) = config.build(SinkContext::new_test()).await.unwrap();

            let message = random_string(100);
            let event = LogEvent::from(message.clone());

            run_and_assert_sink_compliance(sink, stream::once(ready(event)), &HTTP_SINK_TAGS).await;

            let entry = find_entry(repo.name.as_str(), message.as_str()).await;

            assert_eq!(entry.humio_type, "none");
        }
    }

    /// create a new test config with the given ingest token
    fn config(token: &str) -> super::HumioLogsConfig {
        let mut batch = BatchConfig::default();
        batch.max_events = Some(1);

        HumioLogsConfig {
            token: token.to_string().into(),
            endpoint: Some(humio_address()),
            source: None,
            encoding: JsonSerializerConfig::new().into(),
            event_type: None,
            host_key: log_schema().host_key().to_string(),
            indexed_fields: vec![],
            index: None,
            compression: Compression::None,
            request: TowerRequestConfig::default(),
            batch,
            tls: None,
            timestamp_nanos_key: timestamp_nanos_key(),
            acknowledgements: Default::default(),
            timestamp_key: Default::default(),
        }
    }

    async fn wait_ready() {
        crate::test_util::retry_until(
            || async {
                reqwest::get(format!("{}/api/v1/status", humio_address()))
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|res| {
                        if res.status().is_success() {
                            Ok(())
                        } else {
                            Err("server not ready...".into())
                        }
                    })
            },
            Duration::from_secs(1),
            Duration::from_secs(30),
        )
        .await;
    }

    /// create a new test humio repository to publish to
    async fn create_repository() -> HumioRepository {
        let client = reqwest::Client::builder().build().unwrap();

        // https://docs.humio.com/api/graphql/
        let graphql_url = format!("{}/graphql", humio_address());

        let name = random_string(50);

        let params = json!({
        "query": format!(
            indoc!{ r#"
                mutation {{
                  createRepository(name:"{}") {{
                    repository {{
                      name
                      type
                      ingestTokens {{
                        name
                        token
                      }}
                    }}
                  }}
                }}
            "#},
            name
        ),
        });

        let res = client
            .post(&graphql_url)
            .json(&params)
            .send()
            .await
            .unwrap();

        let json: JsonValue = res.json().await.unwrap();
        let repository = &json["data"]["createRepository"]["repository"];

        let token = repository["ingestTokens"].as_array().unwrap()[0]["token"]
            .as_str()
            .unwrap()
            .to_string();

        HumioRepository {
            name: repository["name"].as_str().unwrap().to_string(),
            default_ingest_token: token,
        }
    }

    /// fetch event from the repository that has a matching message value
    async fn find_entry(repository_name: &str, message: &str) -> HumioLog {
        let client = reqwest::Client::builder().build().unwrap();

        // https://docs.humio.com/api/using-the-search-api-with-humio
        let search_url = format!(
            "{}/api/v1/repositories/{}/query",
            humio_address(),
            repository_name
        );
        let search_query = format!(r#"message="{}""#, message);

        // events are not available to search API immediately
        // poll up 200 times for event to show up
        for _ in 0..200usize {
            let res = client
                .post(&search_url)
                .json(&json!({
                    "queryString": search_query,
                }))
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .unwrap();

            let logs: Vec<HumioLog> = res.json().await.unwrap();

            if !logs.is_empty() {
                return logs[0].clone();
            }
        }
        panic!(
            "did not find event in Humio repository {} with message {}",
            repository_name, message
        );
    }

    #[derive(Debug)]
    struct HumioRepository {
        name: String,
        default_ingest_token: String,
    }

    #[derive(Clone, Deserialize)]
    #[allow(dead_code)] // deserialize all fields
    struct HumioLog {
        #[serde(rename = "#repo")]
        humio_repo: String,

        #[serde(rename = "#type")]
        humio_type: String,

        #[serde(rename = "@error")]
        error: Option<String>,

        #[serde(rename = "@error_msg")]
        error_msg: Option<String>,

        #[serde(rename = "@rawstring")]
        rawstring: String,

        #[serde(rename = "@id")]
        id: String,

        #[serde(rename = "@timestamp")]
        timestamp_millis: u64,

        #[serde(rename = "@timestamp.nanos")]
        timestamp_nanos: String,

        #[serde(rename = "@timezone")]
        timezone: String,

        #[serde(rename = "@source")]
        source: Option<String>,

        #[serde(rename = "@host")]
        host: Option<String>,

        // fields parsed from ingested log
        #[serde(flatten)]
        fields: HashMap<String, JsonValue>,
    }
}
