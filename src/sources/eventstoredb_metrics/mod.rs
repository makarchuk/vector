use std::time::Duration;

use futures::{FutureExt, StreamExt};
use http::Uri;
use hyper::{Body, Request};
use tokio_stream::wrappers::IntervalStream;
use vector_common::internal_event::{ByteSize, BytesReceived, InternalEventHandle as _, Protocol};
use vector_config::configurable_component;
use vector_core::config::LogNamespace;
use vector_core::EstimatedJsonEncodedSizeOf;

use self::types::Stats;
use crate::{
    config::{self, Output, SourceConfig, SourceContext},
    http::HttpClient,
    internal_events::{
        EventStoreDbMetricsHttpError, EventStoreDbStatsParsingError, EventsReceived,
        StreamClosedError,
    },
    tls::TlsSettings,
};

pub mod types;

/// Configuration for the `eventstoredb_metrics` source.
#[configurable_component(source("eventstoredb_metrics"))]
#[derive(Clone, Debug, Default)]
pub struct EventStoreDbConfig {
    /// Endpoints to scrape stats from.
    #[serde(default = "default_endpoint")]
    endpoint: String,

    /// The interval between scrapes, in seconds.
    #[serde(default = "default_scrape_interval_secs")]
    scrape_interval_secs: u64,

    /// Overrides the default namespace for the metrics emitted by the source.
    ///
    /// By default, `eventstoredb` is used.
    default_namespace: Option<String>,
}

const fn default_scrape_interval_secs() -> u64 {
    15
}

pub fn default_endpoint() -> String {
    "https://localhost:2113/stats".to_string()
}

impl_generate_config_from_default!(EventStoreDbConfig);

#[async_trait::async_trait]
impl SourceConfig for EventStoreDbConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        eventstoredb(
            self.endpoint.clone(),
            self.scrape_interval_secs,
            self.default_namespace.clone(),
            cx,
        )
    }

    fn outputs(&self, _global_log_namespace: LogNamespace) -> Vec<Output> {
        vec![Output::default(config::DataType::Metric)]
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

fn eventstoredb(
    endpoint: String,
    interval: u64,
    namespace: Option<String>,
    mut cx: SourceContext,
) -> crate::Result<super::Source> {
    let mut ticks = IntervalStream::new(tokio::time::interval(Duration::from_secs(interval)))
        .take_until(cx.shutdown);
    let tls_settings = TlsSettings::from_options(&None)?;
    let client = HttpClient::new(tls_settings, &cx.proxy)?;
    let url: Uri = endpoint.as_str().parse()?;

    let bytes_received = register!(BytesReceived::from(Protocol::HTTP));

    Ok(Box::pin(
        async move {
            while ticks.next().await.is_some() {
                let req = Request::get(&url)
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .expect("Building request should be infallible.");

                match client.send(req).await {
                    Err(error) => {
                        emit!(EventStoreDbMetricsHttpError {
                            error: error.into(),
                        });
                        continue;
                    }

                    Ok(resp) => {
                        let bytes = match hyper::body::to_bytes(resp.into_body()).await {
                            Ok(b) => b,
                            Err(error) => {
                                emit!(EventStoreDbMetricsHttpError {
                                    error: error.into(),
                                });
                                continue;
                            }
                        };
                        bytes_received.emit(ByteSize(bytes.len()));

                        match serde_json::from_slice::<Stats>(bytes.as_ref()) {
                            Err(error) => {
                                emit!(EventStoreDbStatsParsingError { error });
                                continue;
                            }

                            Ok(stats) => {
                                let metrics = stats.metrics(namespace.clone());
                                let count = metrics.len();
                                let byte_size = metrics.estimated_json_encoded_size_of();

                                emit!(EventsReceived { count, byte_size });

                                if let Err(error) = cx.out.send_batch(metrics).await {
                                    emit!(StreamClosedError { count, error });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        .map(Ok)
        .boxed(),
    ))
}

#[cfg(all(test, feature = "eventstoredb_metrics-integration-tests"))]
mod integration_tests {
    use tokio::time::Duration;

    use super::*;
    use crate::test_util::components::{run_and_assert_source_compliance, SOURCE_TAGS};

    const EVENTSTOREDB_SCRAPE_ADDRESS: &str = "http://localhost:2113/stats";

    #[tokio::test]
    async fn scrape_something() {
        let config = EventStoreDbConfig {
            endpoint: EVENTSTOREDB_SCRAPE_ADDRESS.to_owned(),
            scrape_interval_secs: 1,
            default_namespace: None,
        };

        let events =
            run_and_assert_source_compliance(config, Duration::from_secs(5), &SOURCE_TAGS).await;
        assert!(!events.is_empty());
    }
}
