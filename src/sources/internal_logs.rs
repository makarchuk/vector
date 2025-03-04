use chrono::Utc;
use codecs::BytesDeserializerConfig;
use futures::{stream, StreamExt};
use lookup::lookup_v2::{parse_value_path, OptionalValuePath};
use lookup::{owned_value_path, path, OwnedValuePath};
use value::Kind;
use vector_config::{configurable_component, NamedComponent};
use vector_core::{
    config::{LegacyKey, LogNamespace},
    schema::Definition,
};

use crate::{
    config::{log_schema, DataType, Output, SourceConfig, SourceContext},
    event::{EstimatedJsonEncodedSizeOf, Event},
    internal_events::{InternalLogsBytesReceived, InternalLogsEventsReceived, StreamClosedError},
    shutdown::ShutdownSignal,
    trace::TraceSubscription,
    SourceSender,
};

/// Configuration for the `internal_logs` source.
#[configurable_component(source("internal_logs"))]
#[derive(Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct InternalLogsConfig {
    /// Overrides the name of the log field used to add the current hostname to each event.
    ///
    /// The value will be the current hostname for wherever Vector is running.
    ///
    /// By default, the [global `log_schema.host_key` option][global_host_key] is used.
    ///
    /// [global_host_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.host_key
    #[serde(default)]
    host_key: Option<OptionalValuePath>,

    /// Overrides the name of the log field used to add the current process ID to each event.
    ///
    /// The value will be the current process ID for Vector itself.
    ///
    /// By default, `"pid"` is used.
    #[serde(default)]
    pid_key: Option<OptionalValuePath>,

    /// The namespace to use for logs. This overrides the global setting.
    #[configurable(metadata(docs::hidden))]
    #[serde(default)]
    log_namespace: Option<bool>,
}

impl_generate_config_from_default!(InternalLogsConfig);

impl InternalLogsConfig {
    /// Generates the `schema::Definition` for this component.
    fn schema_definition(&self, log_namespace: LogNamespace) -> Definition {
        // `host_key` defaults to the `log_schema().host_key()` if it's not configured in the source.
        let host_key = self
            .host_key
            .clone()
            .map_or_else(
                || parse_value_path(log_schema().host_key()).ok(),
                |k| k.path,
            )
            .map(LegacyKey::Overwrite);

        let pid_key = self
            .pid_key
            .clone()
            .and_then(|k| k.path)
            .map(LegacyKey::Overwrite);

        // There is a global and per-source `log_namespace` config.
        // The source config overrides the global setting and is merged here.
        BytesDeserializerConfig
            .schema_definition(log_namespace)
            .with_standard_vector_source_metadata()
            .with_source_metadata(
                InternalLogsConfig::NAME,
                host_key,
                &owned_value_path!("host"),
                Kind::bytes().or_undefined(),
                Some("host"),
            )
            .with_source_metadata(
                InternalLogsConfig::NAME,
                pid_key,
                &owned_value_path!("pid"),
                Kind::integer(),
                None,
            )
    }
}

#[async_trait::async_trait]
impl SourceConfig for InternalLogsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        let host_key = self.host_key.clone().map_or_else(
            || parse_value_path(log_schema().host_key()).ok(),
            |k| k.path,
        );

        let pid_key = self.pid_key.clone().and_then(|k| k.path);

        let subscription = TraceSubscription::subscribe();

        let log_namespace = cx.log_namespace(self.log_namespace);

        Ok(Box::pin(run(
            host_key,
            pid_key,
            subscription,
            cx.out,
            cx.shutdown,
            log_namespace,
        )))
    }

    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<Output> {
        let schema_definition =
            self.schema_definition(global_log_namespace.merge(self.log_namespace));

        vec![Output::default(DataType::Log).with_schema_definition(schema_definition)]
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

async fn run(
    host_key: Option<OwnedValuePath>,
    pid_key: Option<OwnedValuePath>,
    mut subscription: TraceSubscription,
    mut out: SourceSender,
    shutdown: ShutdownSignal,
    log_namespace: LogNamespace,
) -> Result<(), ()> {
    let hostname = crate::get_hostname();
    let pid = std::process::id();

    // Chain any log events that were captured during early buffering to the front,
    // and then continue with the normal stream of internal log events.
    let buffered_events = subscription.buffered_events().await;
    let mut rx = stream::iter(buffered_events.into_iter().flatten())
        .chain(subscription.into_stream())
        .take_until(shutdown);

    // Note: This loop, or anything called within it, MUST NOT generate
    // any logs that don't break the loop, as that could cause an
    // infinite loop since it receives all such logs.
    while let Some(mut log) = rx.next().await {
        let byte_size = log.estimated_json_encoded_size_of();
        // This event doesn't emit any log
        emit!(InternalLogsBytesReceived { byte_size });
        emit!(InternalLogsEventsReceived {
            count: 1,
            byte_size,
        });

        if let Ok(hostname) = &hostname {
            let legacy_host_key = host_key.as_ref().map(LegacyKey::Overwrite);
            log_namespace.insert_source_metadata(
                InternalLogsConfig::NAME,
                &mut log,
                legacy_host_key,
                path!("host"),
                hostname.to_owned(),
            );
        }

        let legacy_pid_key = pid_key.as_ref().map(LegacyKey::Overwrite);
        log_namespace.insert_source_metadata(
            InternalLogsConfig::NAME,
            &mut log,
            legacy_pid_key,
            path!("pid"),
            pid,
        );

        log_namespace.insert_standard_vector_source_metadata(
            &mut log,
            InternalLogsConfig::NAME,
            Utc::now(),
        );

        if let Err(error) = out.send_event(Event::from(log)).await {
            // this wont trigger any infinite loop considering it stops the component
            emit!(StreamClosedError { error, count: 1 });
            return Err(());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use futures::Stream;
    use lookup::LookupBuf;
    use tokio::time::{sleep, Duration};
    use value::kind::Collection;
    use vector_core::event::Value;

    use super::*;
    use crate::{
        event::Event,
        test_util::{
            collect_ready,
            components::{assert_source_compliance, SOURCE_TAGS},
        },
        trace,
    };

    #[test]
    fn generates_config() {
        crate::test_util::test_generate_config::<InternalLogsConfig>();
    }

    // This test is fairly overloaded with different cases.
    //
    // Unfortunately, this can't be easily split out into separate test
    // cases because `consume_early_buffer` (called within the
    // `start_source` helper) panics when called more than once.
    #[tokio::test]
    async fn receives_logs() {
        trace::init(false, false, "debug", 10);
        trace::reset_early_buffer();

        assert_source_compliance(&SOURCE_TAGS, run_test()).await;
    }

    async fn run_test() {
        let test_id: u8 = rand::random();
        let start = chrono::Utc::now();

        error!(message = "Before source started without span.", %test_id);

        let span = error_span!(
            "source",
            component_kind = "source",
            component_id = "foo",
            component_type = "internal_logs",
        );
        let _enter = span.enter();

        error!(message = "Before source started.", %test_id);

        let rx = start_source().await;

        error!(message = "After source started.", %test_id);

        {
            let nested_span = error_span!(
                "nested span",
                component_kind = "bar",
                component_new_field = "baz",
                component_numerical_field = 1,
                ignored_field = "foobarbaz",
            );
            let _enter = nested_span.enter();
            error!(message = "In a nested span.", %test_id);
        }

        sleep(Duration::from_millis(1)).await;
        let mut events = collect_ready(rx).await;
        let test_id = Value::from(test_id.to_string());
        events.retain(|event| event.as_log().get("test_id") == Some(&test_id));

        let end = chrono::Utc::now();

        assert_eq!(events.len(), 4);

        assert_eq!(
            events[0].as_log()["message"],
            "Before source started without span.".into()
        );
        assert_eq!(
            events[1].as_log()["message"],
            "Before source started.".into()
        );
        assert_eq!(
            events[2].as_log()["message"],
            "After source started.".into()
        );
        assert_eq!(events[3].as_log()["message"], "In a nested span.".into());

        for (i, event) in events.iter().enumerate() {
            let log = event.as_log();
            let timestamp = *log["timestamp"]
                .as_timestamp()
                .expect("timestamp isn't a timestamp");
            assert!(timestamp >= start);
            assert!(timestamp <= end);
            assert_eq!(log["metadata.kind"], "event".into());
            assert_eq!(log["metadata.level"], "ERROR".into());
            // The first log event occurs outside our custom span
            if i == 0 {
                assert!(log.get("vector.component_id").is_none());
                assert!(log.get("vector.component_kind").is_none());
                assert!(log.get("vector.component_type").is_none());
            } else if i < 3 {
                assert_eq!(log["vector.component_id"], "foo".into());
                assert_eq!(log["vector.component_kind"], "source".into());
                assert_eq!(log["vector.component_type"], "internal_logs".into());
            } else {
                // The last event occurs in a nested span. Here, we expect
                // parent fields to be preservered (unless overwritten), new
                // fields to be added, and filtered fields to not exist.
                assert_eq!(log["vector.component_id"], "foo".into());
                assert_eq!(log["vector.component_kind"], "bar".into());
                assert_eq!(log["vector.component_type"], "internal_logs".into());
                assert_eq!(log["vector.component_new_field"], "baz".into());
                assert_eq!(log["vector.component_numerical_field"], 1.into());
                assert!(log.get("vector.ignored_field").is_none());
            }
        }
    }

    async fn start_source() -> impl Stream<Item = Event> + Unpin {
        let (tx, rx) = SourceSender::new_test();

        let source = InternalLogsConfig::default()
            .build(SourceContext::new_test(tx, None))
            .await
            .unwrap();
        tokio::spawn(source);
        sleep(Duration::from_millis(1)).await;
        trace::stop_early_buffering();
        rx
    }

    #[test]
    fn output_schema_definition_vector_namespace() {
        let config = InternalLogsConfig::default();

        let definition = config.outputs(LogNamespace::Vector)[0]
            .clone()
            .log_schema_definition
            .unwrap();

        let expected_definition =
            Definition::new_with_default_metadata(Kind::bytes(), [LogNamespace::Vector])
                .with_meaning(LookupBuf::root(), "message")
                .with_metadata_field(&owned_value_path!("vector", "source_type"), Kind::bytes())
                .with_metadata_field(
                    &owned_value_path!(InternalLogsConfig::NAME, "pid"),
                    Kind::integer(),
                )
                .with_metadata_field(
                    &owned_value_path!("vector", "ingest_timestamp"),
                    Kind::timestamp(),
                )
                .with_metadata_field(
                    &owned_value_path!(InternalLogsConfig::NAME, "host"),
                    Kind::bytes().or_undefined(),
                );

        assert_eq!(definition, expected_definition)
    }

    #[test]
    fn output_schema_definition_legacy_namespace() {
        let mut config = InternalLogsConfig::default();

        let pid_key = "pid_a_pid_a_pid_pid_pid";

        config.pid_key = Some(OptionalValuePath::from(owned_value_path!(pid_key)));

        let definition = config.outputs(LogNamespace::Legacy)[0]
            .clone()
            .log_schema_definition
            .unwrap();

        let expected_definition = Definition::new_with_default_metadata(
            Kind::object(Collection::empty()),
            [LogNamespace::Legacy],
        )
        .with_event_field(
            &owned_value_path!("message"),
            Kind::bytes(),
            Some("message"),
        )
        .with_event_field(&owned_value_path!("source_type"), Kind::bytes(), None)
        .with_event_field(&owned_value_path!(pid_key), Kind::integer(), None)
        .with_event_field(&owned_value_path!("timestamp"), Kind::timestamp(), None)
        .with_event_field(
            &owned_value_path!("host"),
            Kind::bytes().or_undefined(),
            Some("host"),
        );

        assert_eq!(definition, expected_definition)
    }
}
