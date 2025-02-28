mod request_limiter;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::{io, mem::drop, time::Duration};

use bytes::Bytes;
use codecs::StreamDecodingError;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use listenfd::ListenFd;
use smallvec::SmallVec;
use socket2::SockRef;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::sleep,
};
use tokio_util::codec::{Decoder, FramedRead};
use tracing::Instrument;
use vector_common::finalization::AddBatchNotifier;
use vector_core::{config::SourceAcknowledgementsConfig, EstimatedJsonEncodedSizeOf};

use self::request_limiter::RequestLimiter;
use super::SocketListenAddr;
use crate::{
    codecs::ReadyFrames,
    config::SourceContext,
    event::{BatchNotifier, BatchStatus, Event},
    internal_events::{
        ConnectionOpen, DecoderFramingError, OpenGauge, SocketBindError, SocketEventsReceived,
        SocketMode, SocketReceiveError, StreamClosedError, TcpBytesReceived, TcpSendAckError,
        TcpSocketTlsConnectionError,
    },
    shutdown::ShutdownSignal,
    sources::util::AfterReadExt,
    tcp::TcpKeepaliveConfig,
    tls::{CertificateMetadata, MaybeTlsIncomingStream, MaybeTlsListener, MaybeTlsSettings},
    SourceSender,
};

const MAX_IN_FLIGHT_EVENTS_TARGET: usize = 100_000;

async fn try_bind_tcp_listener(
    addr: SocketListenAddr,
    mut listenfd: ListenFd,
    tls: &MaybeTlsSettings,
) -> crate::Result<MaybeTlsListener> {
    match addr {
        SocketListenAddr::SocketAddr(addr) => tls.bind(&addr).await.map_err(Into::into),
        SocketListenAddr::SystemdFd(offset) => match listenfd.take_tcp_listener(offset)? {
            Some(listener) => TcpListener::from_std(listener)
                .map(Into::into)
                .map_err(Into::into),
            None => {
                Err(io::Error::new(io::ErrorKind::AddrInUse, "systemd fd already consumed").into())
            }
        },
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TcpSourceAck {
    Ack,
    Error,
    Reject,
}

pub trait TcpSourceAcker {
    fn build_ack(self, ack: TcpSourceAck) -> Option<Bytes>;
}

pub struct TcpNullAcker;

impl TcpSourceAcker for TcpNullAcker {
    // This function builds an acknowledgement from the source data in
    // the acker and the given acknowledgement code.
    fn build_ack(self, _ack: TcpSourceAck) -> Option<Bytes> {
        None
    }
}

pub trait TcpSource: Clone + Send + Sync + 'static
where
    <<Self as TcpSource>::Decoder as tokio_util::codec::Decoder>::Item: std::marker::Send,
{
    // Should be default: `std::io::Error`.
    // Right now this is unstable: https://github.com/rust-lang/rust/issues/29661
    type Error: From<io::Error>
        + StreamDecodingError
        + std::fmt::Debug
        + std::fmt::Display
        + Send
        + Unpin;
    type Item: Into<SmallVec<[Event; 1]>> + Send + Unpin;
    type Decoder: Decoder<Item = (Self::Item, usize), Error = Self::Error> + Send + 'static;
    type Acker: TcpSourceAcker + Send;

    fn decoder(&self) -> Self::Decoder;

    fn handle_events(&self, _events: &mut [Event], _host: std::net::SocketAddr) {}

    fn build_acker(&self, item: &[Self::Item]) -> Self::Acker;

    #[allow(clippy::too_many_arguments)]
    fn run(
        self,
        addr: SocketListenAddr,
        keepalive: Option<TcpKeepaliveConfig>,
        shutdown_timeout_secs: u64,
        tls: MaybeTlsSettings,
        tls_client_metadata_key: Option<String>,
        receive_buffer_bytes: Option<usize>,
        cx: SourceContext,
        acknowledgements: SourceAcknowledgementsConfig,
        max_connections: Option<u32>,
    ) -> crate::Result<crate::sources::Source> {
        let acknowledgements = cx.do_acknowledgements(acknowledgements);

        Ok(Box::pin(async move {
            let listenfd = ListenFd::from_env();
            let listener = try_bind_tcp_listener(addr, listenfd, &tls)
                .await
                .map_err(|error| {
                    emit!(SocketBindError {
                        mode: SocketMode::Tcp,
                        error: &error,
                    })
                })?;

            info!(
                message = "Listening.",
                addr = %listener
                    .local_addr()
                    .map(SocketListenAddr::SocketAddr)
                    .unwrap_or(addr)
            );

            let tripwire = cx.shutdown.clone();
            let tripwire = async move {
                let _ = tripwire.await;
                sleep(Duration::from_secs(shutdown_timeout_secs)).await;
            }
            .shared();

            let connection_gauge = OpenGauge::new();
            let shutdown_clone = cx.shutdown.clone();

            let request_limiter =
                RequestLimiter::new(MAX_IN_FLIGHT_EVENTS_TARGET, crate::num_threads());

            listener
                .accept_stream_limited(max_connections)
                .take_until(shutdown_clone)
                .for_each(move |(connection, tcp_connection_permit)| {
                    let shutdown_signal = cx.shutdown.clone();
                    let tripwire = tripwire.clone();
                    let source = self.clone();
                    let out = cx.out.clone();
                    let connection_gauge = connection_gauge.clone();
                    let request_limiter = request_limiter.clone();
                    let tls_client_metadata_key = tls_client_metadata_key.clone();

                    async move {
                        let socket = match connection {
                            Ok(socket) => socket,
                            Err(error) => {
                                emit!(SocketReceiveError {
                                    mode: SocketMode::Tcp,
                                    error: &error
                                });
                                return;
                            }
                        };

                        let peer_addr = socket.peer_addr();
                        let span = info_span!("connection", %peer_addr);

                        let tripwire = tripwire
                            .map(move |_| {
                                info!(
                                    message = "Resetting connection (still open after seconds).",
                                    seconds = ?shutdown_timeout_secs
                                );
                            })
                            .boxed();

                        span.clone().in_scope(|| {
                            debug!(message = "Accepted a new connection.", peer_addr = %peer_addr);

                            let open_token =
                                connection_gauge.open(|count| emit!(ConnectionOpen { count }));

                            let fut = handle_stream(
                                shutdown_signal,
                                socket,
                                keepalive,
                                receive_buffer_bytes,
                                source,
                                tripwire,
                                peer_addr,
                                out,
                                acknowledgements,
                                request_limiter,
                                tls_client_metadata_key.clone(),
                            );

                            tokio::spawn(
                                fut.map(move |()| {
                                    drop(open_token);
                                    drop(tcp_connection_permit);
                                })
                                .instrument(span.or_current()),
                            );
                        });
                    }
                })
                .map(Ok)
                .await
        }))
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_stream<T>(
    mut shutdown_signal: ShutdownSignal,
    mut socket: MaybeTlsIncomingStream<TcpStream>,
    keepalive: Option<TcpKeepaliveConfig>,
    receive_buffer_bytes: Option<usize>,
    source: T,
    mut tripwire: BoxFuture<'static, ()>,
    peer_addr: SocketAddr,
    mut out: SourceSender,
    acknowledgements: bool,
    request_limiter: RequestLimiter,
    tls_client_metadata_key: Option<String>,
) where
    <<T as TcpSource>::Decoder as tokio_util::codec::Decoder>::Item: std::marker::Send,
    T: TcpSource,
{
    tokio::select! {
        result = socket.handshake() => {
            if let Err(error) = result {
                emit!(TcpSocketTlsConnectionError { error });
                return;
            }
        },
        _ = &mut shutdown_signal => {
            return;
        }
    };

    if let Some(keepalive) = keepalive {
        if let Err(error) = socket.set_keepalive(keepalive) {
            warn!(message = "Failed configuring TCP keepalive.", %error);
        }
    }

    if let Some(receive_buffer_bytes) = receive_buffer_bytes {
        if let Err(error) = socket.set_receive_buffer_bytes(receive_buffer_bytes) {
            warn!(message = "Failed configuring receive buffer size on TCP socket.", %error);
        }
    }

    let socket = socket.after_read(move |byte_size| {
        emit!(TcpBytesReceived {
            byte_size,
            peer_addr
        });
    });

    let certificate_metadata = socket
        .get_ref()
        .ssl_stream()
        .and_then(|stream| stream.ssl().peer_certificate())
        .map(CertificateMetadata::from);

    let reader = FramedRead::new(socket, source.decoder());
    let mut reader = ReadyFrames::new(reader);

    loop {
        let mut permit = tokio::select! {
            _ = &mut tripwire => break,
            _ = &mut shutdown_signal => {
                if close_socket(reader.get_ref().get_ref().get_ref()) {
                    break;
                }
                None
            },
            permit = request_limiter.acquire() => {
                Some(permit)
            }
            else => break,
        };

        let timeout = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(timeout);

        tokio::select! {
            _ = &mut tripwire => break,
            _ = &mut shutdown_signal => {
                if close_socket(reader.get_ref().get_ref().get_ref()) {
                    break;
                }
            },
            _ = &mut timeout => {
                // This connection is currently holding a permit, but has not received data for some time. Release
                // the permit to let another connection try
                continue;
            }
            res = reader.next() => {
                match res {
                    Some(Ok((frames, _byte_size))) => {
                        let _num_frames = frames.len();
                        let acker = source.build_acker(&frames);
                        let (batch, receiver) = BatchNotifier::maybe_new_with_receiver(acknowledgements);

                        let mut events = frames.into_iter().flat_map(Into::into).collect::<Vec<Event>>();
                        let count = events.len();

                        emit!(SocketEventsReceived {
                            mode: SocketMode::Tcp,
                            byte_size: events.estimated_json_encoded_size_of(),
                            count,
                        });

                        if let Some(permit) = &mut permit {
                            // Note that this is intentionally not the "number of events in a single request", but rather
                            // the "number of events currently available". This may contain events from multiple events,
                            // but it should always contain all events from each request.
                            permit.decoding_finished(events.len());
                        }

                        if let Some(batch) = batch {
                            for event in &mut events {
                                event.add_batch_notifier(batch.clone());
                            }
                        }

                        if let Some(tls_client_metadata_key) = &tls_client_metadata_key {
                            if let Some(certificate_metadata) = &certificate_metadata {
                                let mut metadata: BTreeMap<String, value::Value> = BTreeMap::new();
                                metadata.insert("subject".to_string(), certificate_metadata.subject().into());
                                for event in &mut events {
                                    let log = event.as_mut_log();
                                    log.insert(&tls_client_metadata_key[..], value::Value::from(metadata.clone()));
                                }
                            }
                        }

                        source.handle_events(&mut events, peer_addr);
                        match out.send_batch(events).await {
                            Ok(_) => {
                                let ack = match receiver {
                                    None => TcpSourceAck::Ack,
                                    Some(receiver) =>
                                        match receiver.await {
                                            BatchStatus::Delivered => TcpSourceAck::Ack,
                                            BatchStatus::Errored => {TcpSourceAck::Error},
                                            BatchStatus::Rejected => {
                                                // Sinks are responsible for emitting ComponentEventsDropped.
                                                TcpSourceAck::Reject
                                            }
                                        }
                                };
                                if let Some(ack_bytes) = acker.build_ack(ack){
                                    let stream = reader.get_mut().get_mut();
                                    if let Err(error) = stream.write_all(&ack_bytes).await {
                                        emit!(TcpSendAckError{ error });
                                        break;
                                    }
                                }
                                if ack != TcpSourceAck::Ack {
                                    break;
                                }
                            }
                            Err(error) => {
                                emit!(StreamClosedError { error, count });
                                break;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if !<<T as TcpSource>::Error as StreamDecodingError>::can_continue(&error) {
                            emit!(DecoderFramingError { error });
                            break;
                        }
                    }
                    None => {
                        debug!("Connection closed.");
                        break
                    },
                }
            }
            else => break,
        }

        drop(permit);
    }
}

fn close_socket(socket: &MaybeTlsIncomingStream<TcpStream>) -> bool {
    debug!("Start graceful shutdown.");
    // Close our write part of TCP socket to signal the other side
    // that it should stop writing and close the channel.
    if let Some(stream) = socket.get_ref() {
        let socket = SockRef::from(stream);
        if let Err(error) = socket.shutdown(std::net::Shutdown::Write) {
            warn!(message = "Failed in signalling to the other side to close the TCP channel.", %error);
        }
        false
    } else {
        // Connection hasn't yet been established so we are done here.
        debug!("Closing connection that hasn't yet been fully established.");
        true
    }
}
