use chrono::Utc;
use codecs::{
    decoding::{DeserializerConfig, FramingConfig},
    StreamDecodingError,
};
use fakedata::logs::*;
use futures::StreamExt;
use rand::seq::SliceRandom;
use snafu::Snafu;
use std::task::Poll;
use tokio::time::{self, Duration};
use tokio_util::codec::FramedRead;
use vector_common::internal_event::{ByteSize, BytesReceived, InternalEventHandle as _, Protocol};
use vector_config::{configurable_component, NamedComponent};
use vector_core::{config::LogNamespace, EstimatedJsonEncodedSizeOf};

use crate::{
    codecs::{Decoder, DecodingConfig},
    config::{Output, SourceConfig, SourceContext},
    internal_events::{DemoLogsEventProcessed, EventsReceived, StreamClosedError},
    serde::{default_decoding, default_framing_message_based},
    shutdown::ShutdownSignal,
    SourceSender,
};

/// Configuration for the `demo_logs` source.
#[configurable_component(source("demo_logs"))]
#[derive(Clone, Debug, Derivative)]
#[derivative(Default)]
#[serde(default)]
pub struct DemoLogsConfig {
    /// The amount of time, in seconds, to pause between each batch of output lines.
    ///
    /// The default is one batch per second. In order to remove the delay and output batches as quickly as possible, set
    /// `interval` to `0.0`.
    #[serde(alias = "batch_interval")]
    #[derivative(Default(value = "default_interval()"))]
    pub interval: f64,

    /// The total number of lines to output.
    ///
    /// By default, the source continuously prints logs (infinitely).
    #[derivative(Default(value = "default_count()"))]
    pub count: usize,

    #[serde(flatten)]
    pub format: OutputFormat,

    #[configurable(derived)]
    #[derivative(Default(value = "default_framing_message_based()"))]
    pub framing: FramingConfig,

    #[configurable(derived)]
    #[derivative(Default(value = "default_decoding()"))]
    pub decoding: DeserializerConfig,

    /// The namespace to use for logs. This overrides the global setting.
    #[serde(default)]
    #[configurable(metadata(docs::hidden))]
    pub log_namespace: Option<bool>,
}

const fn default_interval() -> f64 {
    1.0
}

const fn default_count() -> usize {
    isize::MAX as usize
}

#[derive(Debug, PartialEq, Eq, Snafu)]
pub enum DemoLogsConfigError {
    #[snafu(display("A non-empty list of lines is required for the shuffle format"))]
    ShuffleDemoLogsItemsEmpty,
}

/// Output format configuration.
#[configurable_component]
#[derive(Clone, Debug, Derivative)]
#[derivative(Default)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum OutputFormat {
    /// Lines are chosen at random from the list specified using `lines`.
    Shuffle {
        /// If `true`, each output line starts with an increasing sequence number, beginning with 0.
        #[serde(default)]
        sequence: bool,
        /// The list of lines to output.
        lines: Vec<String>,
    },

    /// Randomly generated logs in [Apache common](\(urls.apache_common)) format.
    ApacheCommon,

    /// Randomly generated logs in [Apache error](\(urls.apache_error)) format.
    ApacheError,

    /// Randomly generated logs in Syslog format ([RFC 5424](\(urls.syslog_5424))).
    #[serde(alias = "rfc5424")]
    Syslog,

    /// Randomly generated logs in Syslog format ([RFC 3164](\(urls.syslog_3164))).
    #[serde(alias = "rfc3164")]
    BsdSyslog,

    /// Randomly generated HTTP server logs in [JSON](\(urls.json)) format.
    #[derivative(Default)]
    Json,
}

impl OutputFormat {
    fn generate_line(&self, n: usize) -> String {
        emit!(DemoLogsEventProcessed);

        match self {
            Self::Shuffle {
                sequence,
                ref lines,
            } => Self::shuffle_generate(*sequence, lines, n),
            Self::ApacheCommon => apache_common_log_line(),
            Self::ApacheError => apache_error_log_line(),
            Self::Syslog => syslog_5424_log_line(),
            Self::BsdSyslog => syslog_3164_log_line(),
            Self::Json => json_log_line(),
        }
    }

    fn shuffle_generate(sequence: bool, lines: &[String], n: usize) -> String {
        // unwrap can be called here because `lines` can't be empty
        let line = lines.choose(&mut rand::thread_rng()).unwrap();

        if sequence {
            format!("{} {}", n, line)
        } else {
            line.into()
        }
    }

    // Ensures that the `lines` list is non-empty if `Shuffle` is chosen
    pub(self) fn validate(&self) -> Result<(), DemoLogsConfigError> {
        match self {
            Self::Shuffle { lines, .. } => {
                if lines.is_empty() {
                    Err(DemoLogsConfigError::ShuffleDemoLogsItemsEmpty)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

impl DemoLogsConfig {
    #[cfg(test)]
    pub fn repeat(
        lines: Vec<String>,
        count: usize,
        interval: f64,
        log_namespace: Option<bool>,
    ) -> Self {
        Self {
            count,
            interval,
            format: OutputFormat::Shuffle {
                lines,
                sequence: false,
            },
            framing: default_framing_message_based(),
            decoding: default_decoding(),
            log_namespace,
        }
    }
}

async fn demo_logs_source(
    interval: f64,
    count: usize,
    format: OutputFormat,
    decoder: Decoder,
    mut shutdown: ShutdownSignal,
    mut out: SourceSender,
    log_namespace: LogNamespace,
) -> Result<(), ()> {
    let maybe_interval: Option<f64> = (interval != 0.0).then_some(interval);

    let mut interval = maybe_interval.map(|i| time::interval(Duration::from_secs_f64(i)));

    let bytes_received = register!(BytesReceived::from(Protocol::NONE));

    for n in 0..count {
        if matches!(futures::poll!(&mut shutdown), Poll::Ready(_)) {
            break;
        }

        if let Some(interval) = &mut interval {
            interval.tick().await;
        }
        bytes_received.emit(ByteSize(0));

        let line = format.generate_line(n);

        let mut stream = FramedRead::new(line.as_bytes(), decoder.clone());
        while let Some(next) = stream.next().await {
            match next {
                Ok((events, _byte_size)) => {
                    let count = events.len();
                    emit!(EventsReceived {
                        count,
                        byte_size: events.estimated_json_encoded_size_of()
                    });
                    let now = Utc::now();

                    let events = events.into_iter().map(|mut event| {
                        let log = event.as_mut_log();
                        log_namespace.insert_standard_vector_source_metadata(
                            log,
                            DemoLogsConfig::NAME,
                            now,
                        );

                        event
                    });
                    out.send_batch(events).await.map_err(|error| {
                        emit!(StreamClosedError { error, count });
                    })?;
                }
                Err(error) => {
                    // Error is logged by `crate::codecs::Decoder`, no further
                    // handling is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

impl_generate_config_from_default!(DemoLogsConfig);

#[async_trait::async_trait]
impl SourceConfig for DemoLogsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        let log_namespace = cx.log_namespace(self.log_namespace);

        self.format.validate()?;
        let decoder =
            DecodingConfig::new(self.framing.clone(), self.decoding.clone(), log_namespace).build();
        Ok(Box::pin(demo_logs_source(
            self.interval,
            self.count,
            self.format.clone(),
            decoder,
            cx.shutdown,
            cx.out,
            log_namespace,
        )))
    }

    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<Output> {
        // There is a global and per-source `log_namespace` config. The source config overrides the global setting,
        // and is merged here.
        let log_namespace = global_log_namespace.merge(self.log_namespace);

        let schema_definition = self
            .decoding
            .schema_definition(log_namespace)
            .with_standard_vector_source_metadata();

        vec![Output::default(self.decoding.output_type()).with_schema_definition(schema_definition)]
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use futures::{poll, Stream, StreamExt};

    use super::*;
    use crate::{
        config::log_schema,
        event::Event,
        shutdown::ShutdownSignal,
        test_util::components::{assert_source_compliance, SOURCE_TAGS},
        SourceSender,
    };

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<DemoLogsConfig>();
    }

    async fn runit(config: &str) -> impl Stream<Item = Event> {
        assert_source_compliance(&SOURCE_TAGS, async {
            let (tx, rx) = SourceSender::new_test();
            let config: DemoLogsConfig = toml::from_str(config).unwrap();
            let decoder = DecodingConfig::new(
                default_framing_message_based(),
                default_decoding(),
                LogNamespace::Legacy,
            )
            .build();
            demo_logs_source(
                config.interval,
                config.count,
                config.format,
                decoder,
                ShutdownSignal::noop(),
                tx,
                LogNamespace::Legacy,
            )
            .await
            .unwrap();

            rx
        })
        .await
    }

    #[test]
    fn config_shuffle_lines_not_empty() {
        let empty_lines: Vec<String> = Vec::new();

        let errant_config = DemoLogsConfig {
            format: OutputFormat::Shuffle {
                sequence: false,
                lines: empty_lines,
            },
            ..DemoLogsConfig::default()
        };

        assert_eq!(
            errant_config.format.validate(),
            Err(DemoLogsConfigError::ShuffleDemoLogsItemsEmpty)
        );
    }

    #[tokio::test]
    async fn shuffle_demo_logs_copies_lines() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two", "three", "four"]
               count = 5"#,
        )
        .await;

        let lines = &["one", "two", "three", "four"];

        for _ in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(lines.contains(&&*message));
        }

        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_demo_logs_limits_count() {
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_demo_logs_adds_sequence() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               sequence = true
               count = 5"#,
        )
        .await;

        for n in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(message.starts_with(&n.to_string()));
        }

        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_demo_logs_obeys_interval() {
        let start = Instant::now();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               count = 3
               interval = 1.0"#,
        )
        .await;

        for _ in 0..3 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));

        let duration = start.elapsed();
        assert!(duration >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn apache_common_format_generates_output() {
        let mut rx = runit(
            r#"format = "apache_common"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn apache_error_format_generates_output() {
        let mut rx = runit(
            r#"format = "apache_error"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn syslog_5424_format_generates_output() {
        let mut rx = runit(
            r#"format = "syslog"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn syslog_3164_format_generates_output() {
        let mut rx = runit(
            r#"format = "bsd_syslog"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn json_format_generates_output() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "json"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(serde_json::from_str::<serde_json::Value>(&message).is_ok());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }
}
