use std::io;

use async_stream::stream;
use bytes::Bytes;
use chrono::Utc;
use codecs::{
    decoding::{DeserializerConfig, FramingConfig},
    StreamDecodingError,
};
use futures::{channel::mpsc, executor, SinkExt, StreamExt};
use lookup::{owned_value_path, path};
use tokio_util::{codec::FramedRead, io::StreamReader};
use value::Kind;
use vector_common::internal_event::{ByteSize, BytesReceived, InternalEventHandle as _, Protocol};
use vector_config::NamedComponent;
use vector_core::config::{LegacyKey, LogNamespace, Output};
use vector_core::event::Event;
use vector_core::EstimatedJsonEncodedSizeOf;

use crate::{
    codecs::{Decoder, DecodingConfig},
    config::log_schema,
    internal_events::{EventsReceived, FileDescriptorReadError, StreamClosedError},
    shutdown::ShutdownSignal,
    SourceSender,
};

#[cfg(all(unix, feature = "sources-file-descriptor"))]
pub mod file_descriptor;
#[cfg(feature = "sources-stdin")]
pub mod stdin;

pub trait FileDescriptorConfig: NamedComponent {
    fn host_key(&self) -> Option<String>;
    fn framing(&self) -> Option<FramingConfig>;
    fn decoding(&self) -> DeserializerConfig;
    fn description(&self) -> String;

    fn source<R>(
        &self,
        reader: R,
        shutdown: ShutdownSignal,
        out: SourceSender,
        log_namespace: LogNamespace,
    ) -> crate::Result<crate::sources::Source>
    where
        R: Send + io::BufRead + 'static,
    {
        let host_key = self
            .host_key()
            .unwrap_or_else(|| log_schema().host_key().to_string());
        let hostname = crate::get_hostname().ok();

        let description = self.description();

        let decoding = self.decoding();
        let framing = self
            .framing()
            .unwrap_or_else(|| decoding.default_stream_framing());
        let decoder = DecodingConfig::new(framing, decoding, log_namespace).build();

        let (sender, receiver) = mpsc::channel(1024);

        // Spawn background thread with blocking I/O to process fd.
        //
        // This is recommended by Tokio, as otherwise the process will not shut down
        // until another newline is entered. See
        // https://github.com/tokio-rs/tokio/blob/a73428252b08bf1436f12e76287acbc4600ca0e5/tokio/src/io/stdin.rs#L33-L42
        std::thread::spawn(move || {
            info!("Capturing {}.", description);
            read_from_fd(reader, sender);
        });

        Ok(Box::pin(process_stream(
            receiver,
            decoder,
            out,
            shutdown,
            host_key,
            Self::NAME,
            hostname,
            log_namespace,
        )))
    }
}

type Sender = mpsc::Sender<std::result::Result<bytes::Bytes, std::io::Error>>;

fn read_from_fd<R>(mut reader: R, mut sender: Sender)
where
    R: Send + io::BufRead + 'static,
{
    loop {
        let (buffer, len) = match reader.fill_buf() {
            Ok(buffer) if buffer.is_empty() => break, // EOF.
            Ok(buffer) => (Ok(Bytes::copy_from_slice(buffer)), buffer.len()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => (Err(error), 0),
        };

        reader.consume(len);

        if executor::block_on(sender.send(buffer)).is_err() {
            // Receiver has closed so we should shutdown.
            break;
        }
    }
}

type Receiver = mpsc::Receiver<std::result::Result<bytes::Bytes, std::io::Error>>;

#[allow(clippy::too_many_arguments)]
async fn process_stream(
    receiver: Receiver,
    decoder: Decoder,
    mut out: SourceSender,
    shutdown: ShutdownSignal,
    host_key: String,
    source_type: &'static str,
    hostname: Option<String>,
    log_namespace: LogNamespace,
) -> Result<(), ()> {
    let bytes_received = register!(BytesReceived::from(Protocol::NONE));
    let stream = receiver.inspect(|result| {
        if let Err(error) = result {
            emit!(FileDescriptorReadError { error: &error });
        }
    });
    let stream = StreamReader::new(stream);
    let mut stream = FramedRead::new(stream, decoder).take_until(shutdown);
    let mut stream = stream! {
        while let Some(result) = stream.next().await {
            match result {
                Ok((events, byte_size)) => {
                    bytes_received.emit(ByteSize(byte_size));
                    emit!(EventsReceived {
                        byte_size: events.estimated_json_encoded_size_of(),
                        count: events.len()
                    });

                    let now = Utc::now();

                    for mut event in events {
                        match event{
                            Event::Log(_) => {
                                let log = event.as_mut_log();

                                log_namespace.insert_standard_vector_source_metadata(
                                    log,
                                    source_type,
                                    now
                                );

                                if let Some(hostname) = &hostname {
                                    log_namespace.insert_source_metadata(
                                        source_type,
                                        log,
                                        Some(LegacyKey::InsertIfEmpty(host_key.as_str())),
                                        path!("host"),
                                        hostname.clone()
                                    );
                                }

                                yield event;
                            },
                            _ => {
                                yield event;
                            }
                        }
                    }
                }
                Err(error) => {
                    // Error is logged by `crate::codecs::Decoder`, no
                    // further handling is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }
    .boxed();

    match out.send_event_stream(&mut stream).await {
        Ok(()) => {
            debug!("Finished sending.");
            Ok(())
        }
        Err(error) => {
            let (count, _) = stream.size_hint();
            emit!(StreamClosedError { error, count });
            Err(())
        }
    }
}

/// Builds the `vector_core::config::Outputs` for stdin and
/// file_descriptor sources.
fn outputs(
    log_namespace: LogNamespace,
    host_key: &Option<String>,
    decoding: &DeserializerConfig,
    source_name: &'static str,
) -> Vec<Output> {
    let host_key_path = host_key.as_ref().map_or_else(
        || owned_value_path!(log_schema().host_key()),
        |x| owned_value_path!(x),
    );

    let schema_definition = decoding
        .schema_definition(log_namespace)
        .with_source_metadata(
            source_name,
            Some(LegacyKey::InsertIfEmpty(host_key_path)),
            &owned_value_path!("host"),
            Kind::bytes(),
            None,
        )
        .with_standard_vector_source_metadata();

    vec![Output::default(decoding.output_type()).with_schema_definition(schema_definition)]
}
