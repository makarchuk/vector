//! The sink for the `AMQP` sink that wires together the main stream that takes the
//! event and sends it to `AMQP`.
use crate::{
    codecs::Transformer, event::Event, internal_events::TemplateRenderingError,
    sinks::util::builder::SinkBuilderExt, template::Template,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures_util::stream::BoxStream;
use lapin::options::ConfirmSelectOptions;
use serde::Serialize;
use std::sync::Arc;
use tower::ServiceBuilder;
use vector_buffers::EventCount;
use vector_core::{sink::StreamSink, ByteSizeOf, EstimatedJsonEncodedSizeOf};

use super::{
    config::AmqpSinkConfig, encoder::AmqpEncoder, request_builder::AmqpRequestBuilder,
    service::AmqpService, BuildError,
};

/// Stores the event together with the rendered exchange and routing_key values.
/// This is passed into the `RequestBuilder` which then splits it out into the event
/// and metadata containing the exchange and routing_key.
/// This event needs to be created prior to building the request so we can filter out
/// any events that error whilst redndering the templates.
#[derive(Serialize)]
pub(super) struct AmqpEvent {
    pub(super) event: Event,
    pub(super) exchange: String,
    pub(super) routing_key: String,
}

impl EventCount for AmqpEvent {
    fn event_count(&self) -> usize {
        // An AmqpEvent represents one event.
        1
    }
}

impl ByteSizeOf for AmqpEvent {
    fn allocated_bytes(&self) -> usize {
        self.event.size_of()
    }
}

impl EstimatedJsonEncodedSizeOf for AmqpEvent {
    fn estimated_json_encoded_size_of(&self) -> usize {
        self.event.estimated_json_encoded_size_of()
    }
}

pub(super) struct AmqpSink {
    pub(super) channel: Arc<lapin::Channel>,
    exchange: Template,
    routing_key: Option<Template>,
    transformer: Transformer,
    encoder: crate::codecs::Encoder<()>,
}

impl AmqpSink {
    pub(super) async fn new(config: AmqpSinkConfig) -> crate::Result<Self> {
        let (_, channel) = config
            .connection
            .connect()
            .await
            .map_err(|e| BuildError::AmqpCreateFailed { source: e })?;

        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(|e| BuildError::AmqpCreateFailed {
                source: Box::new(e),
            })?;

        let transformer = config.encoding.transformer();
        let serializer = config.encoding.build()?;
        let encoder = crate::codecs::Encoder::<()>::new(serializer);

        Ok(AmqpSink {
            channel: Arc::new(channel),
            exchange: config.exchange,
            routing_key: config.routing_key,
            transformer,
            encoder,
        })
    }

    /// Transforms an event into an `AMQP` event by rendering the required template fields.
    /// Returns None if there is an error whilst rendering.
    fn make_amqp_event(&self, event: Event) -> Option<AmqpEvent> {
        let exchange = self
            .exchange
            .render_string(&event)
            .map_err(|missing_keys| {
                emit!(TemplateRenderingError {
                    error: missing_keys,
                    field: Some("exchange"),
                    drop_event: true,
                })
            })
            .ok()?;

        let routing_key = match &self.routing_key {
            None => String::new(),
            Some(key) => key
                .render_string(&event)
                .map_err(|missing_keys| {
                    emit!(TemplateRenderingError {
                        error: missing_keys,
                        field: Some("routing_key"),
                        drop_event: true,
                    })
                })
                .ok()?,
        };

        Some(AmqpEvent {
            event,
            exchange,
            routing_key,
        })
    }

    async fn run_inner(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        let request_builder = AmqpRequestBuilder {
            encoder: AmqpEncoder {
                encoder: self.encoder.clone(),
                transformer: self.transformer.clone(),
            },
        };
        let service = ServiceBuilder::new().service(AmqpService {
            channel: Arc::clone(&self.channel),
        });

        let sink = input
            .filter_map(|event| std::future::ready(self.make_amqp_event(event)))
            .request_builder(None, request_builder)
            .filter_map(|request| async move {
                match request {
                    Err(e) => {
                        error!("Failed to build AMQP request: {:?}.", e);
                        None
                    }
                    Ok(req) => Some(req),
                }
            })
            .into_driver(service);

        sink.run().await
    }
}

#[async_trait]
impl StreamSink<Event> for AmqpSink {
    async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.run_inner(input).await
    }
}
