use std::iter;

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use codecs::StreamDecodingError;
use lookup::{metadata_path, path};
use tokio_util::codec::Decoder as _;
use vector_core::{
    config::LogNamespace, internal_event::EventsReceived, EstimatedJsonEncodedSizeOf,
};

use crate::{codecs::Decoder, config::log_schema, event::BatchNotifier, event::Event};

pub fn decode_message<'a>(
    mut decoder: Decoder,
    source_type: &'static str,
    message: &[u8],
    timestamp: Option<DateTime<Utc>>,
    batch: &'a Option<BatchNotifier>,
    log_namespace: LogNamespace,
) -> impl Iterator<Item = Event> + 'a {
    let schema = log_schema();

    let mut buffer = BytesMut::with_capacity(message.len());
    buffer.extend_from_slice(message);
    let now = Utc::now();

    iter::from_fn(move || loop {
        break match decoder.decode_eof(&mut buffer) {
            Ok(Some((events, _))) => {
                let count = events.len();
                Some(
                    events
                        .into_iter()
                        .map(move |mut event| {
                            if let Event::Log(ref mut log) = event {
                                log_namespace.insert_vector_metadata(
                                    log,
                                    path!(schema.source_type_key()),
                                    path!("source_type"),
                                    Bytes::from(source_type),
                                );
                                match log_namespace {
                                    LogNamespace::Vector => {
                                        if let Some(timestamp) = timestamp {
                                            log.try_insert(
                                                metadata_path!(source_type, "timestamp"),
                                                timestamp,
                                            );
                                        }

                                        log.insert(
                                            metadata_path!("vector", "ingest_timestamp"),
                                            now,
                                        );
                                    }
                                    LogNamespace::Legacy => {
                                        if let Some(timestamp) = timestamp {
                                            log.try_insert(schema.timestamp_key(), timestamp);
                                        }
                                    }
                                }
                            }
                            event
                        })
                        .fold_finally(
                            0,
                            |size, event: &Event| size + event.estimated_json_encoded_size_of(),
                            move |byte_size| emit!(EventsReceived { byte_size, count }),
                        ),
                )
            }
            Err(error) => {
                // Error is logged by `crate::codecs::Decoder`, no further handling
                // is needed here.
                if error.can_continue() {
                    continue;
                }
                None
            }
            Ok(None) => None,
        };
    })
    .flatten()
    .map(move |event| event.with_batch_notifier_option(batch))
}

trait FoldFinallyExt: Sized {
    /// This adapter applies the `folder` function to every element in
    /// the iterator, much as `Iterator::fold` does. However, instead
    /// of returning the resulting folded value, it calls the
    /// `finally` function after the last element. This function
    /// returns an iterator over the original values.
    fn fold_finally<A, Fo, Fi>(
        self,
        initial: A,
        folder: Fo,
        finally: Fi,
    ) -> FoldFinally<Self, A, Fo, Fi>;
}

impl<I: Iterator + Sized> FoldFinallyExt for I {
    fn fold_finally<A, Fo, Fi>(
        self,
        initial: A,
        folder: Fo,
        finally: Fi,
    ) -> FoldFinally<Self, A, Fo, Fi> {
        FoldFinally {
            inner: self,
            accumulator: initial,
            folder,
            finally,
        }
    }
}

struct FoldFinally<I, A, Fo, Fi> {
    inner: I,
    accumulator: A,
    folder: Fo,
    finally: Fi,
}

impl<I, A, Fo, Fi> Iterator for FoldFinally<I, A, Fo, Fi>
where
    I: Iterator,
    A: Copy,
    Fo: FnMut(A, &I::Item) -> A,
    Fi: Fn(A),
{
    type Item = I::Item;
    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(item) => {
                self.accumulator = (self.folder)(self.accumulator, &item);
                Some(item)
            }
            None => {
                (self.finally)(self.accumulator);
                None
            }
        }
    }
}
