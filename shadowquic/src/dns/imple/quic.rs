//! DNS over QUIC (RFC 9250) over a routed UDP session.
#[cfg(test)]
mod tests;
use std::{
    fmt,
    future::Future,
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::Bytes;
use quinn::{
    AsyncUdpSocket, UdpSender,
    udp::{RecvMeta, Transmit},
};
use tokio::sync::{Mutex, OnceCell, mpsc};

use super::{Result, dns_error, parse_query, read_frame, validate_response, write_frame};
use crate::{
    DnsQuery, ProxyRequest, UdpSession, UserContext, error::SError, msgs::socks5::SocksAddr,
};

type Datagram = (Bytes, SocksAddr);
const QUEUE_SIZE: usize = 16;
const DOQ_PROTOCOL_ERROR: u32 = 2;
const DOQ_REQUEST_CANCELLED: u32 = 3;

/// One upstream connection per resolver; the lock covers connection establishment,
/// never a DNS exchange, so simultaneous queries use independent QUIC streams.
pub(crate) struct DoqClient {
    upstream: SocketAddr,
    server_name: String,
    config: quinn::ClientConfig,
    zero_rtt: bool,
    session: Mutex<Option<Arc<Session>>>,
}

struct Endpoint(quinn::Endpoint);

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Also closes the routed UDP session on failed/cancelled handshakes.
        self.0.close(0u32.into(), b"DNS endpoint closed");
    }
}

struct Session {
    _endpoint: Endpoint,
    connection: quinn::Connection,
    acceptance: Option<Mutex<quinn::ZeroRttAccepted>>,
    accepted: OnceCell<bool>,
}

impl Session {
    async fn early_data_accepted(&self) -> bool {
        *self
            .accepted
            .get_or_init(|| async {
                match &self.acceptance {
                    // Keep the future in place so cancellation cannot lose it.
                    Some(acceptance) => (&mut *acceptance.lock().await).await,
                    None => true,
                }
            })
            .await
    }
}

impl DoqClient {
    pub(super) fn new(
        upstream: SocketAddr,
        server_name: String,
        mut tls: rustls_jls::ClientConfig,
        zero_rtt: bool,
    ) -> Result<Self> {
        tls.jls_config.enable = false;
        tls.enable_early_data = zero_rtt;
        tls.alpn_protocols = vec![b"doq".to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(dns_error)?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .stream_receive_window(8192u32.into())
            .receive_window((64 * 1024u32).into())
            .send_window(64 * 1024)
            .max_concurrent_bidi_streams(0u32.into())
            .max_concurrent_uni_streams(0u32.into())
            .datagram_receive_buffer_size(None)
            .datagram_send_buffer_size(0)
            .enable_segmentation_offload(false)
            .mtu_discovery_config(None)
            .max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
        config.transport_config(Arc::new(transport));
        Ok(Self {
            upstream,
            server_name,
            config,
            zero_rtt,
            session: Mutex::new(None),
        })
    }

    async fn connection(
        &self,
        query: &[u8],
        requests: &mpsc::Sender<ProxyRequest>,
    ) -> Result<Arc<Session>> {
        let mut cached = self.session.lock().await;
        if let Some(session) = cached.as_ref()
            && session.connection.close_reason().is_none()
        {
            return Ok(session.clone());
        }
        // Release closed connections and their UDP queues before reconnecting.
        *cached = None;
        let user_context = UserContext {
            dns_query: parse_query(query)?
                .questions
                .iter()
                .map(|question| DnsQuery {
                    name: question.qname.to_string(),
                    record_type: question.qtype.into(),
                })
                .collect(),
            ..Default::default()
        };
        let (socket, request) = ProxySocket::new(self.upstream, user_context);
        let endpoint = Endpoint(quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Box::new(socket),
            Arc::new(quinn::TokioRuntime),
        )?);
        requests.send(request).await.map_err(dns_error)?;
        let connecting = endpoint
            .0
            .connect_with(self.config.clone(), self.upstream, &self.server_name)
            .map_err(dns_error)?;
        let (connection, acceptance) = if self.zero_rtt {
            match connecting.into_0rtt() {
                Ok((connection, acceptance)) => {
                    tracing::debug!("attempting DNS over QUIC 0-RTT");
                    (connection, Some(Mutex::new(acceptance)))
                }
                Err(connecting) => (connecting.await.map_err(dns_error)?, None),
            }
        } else {
            (connecting.await.map_err(dns_error)?, None)
        };
        let session = Arc::new(Session {
            _endpoint: endpoint,
            connection,
            acceptance,
            accepted: OnceCell::new(),
        });
        *cached = Some(session.clone());
        Ok(session)
    }

    pub(super) async fn exchange(
        &self,
        query: &[u8],
        requests: &mpsc::Sender<ProxyRequest>,
    ) -> Result<Vec<u8>> {
        // Retry once if a cached connection closes while opening/using a stream.
        for attempt in 0..2 {
            let session = self.connection(query, requests).await?;
            let early = session.acceptance.is_some() && session.accepted.get().is_none();
            let mut result = exchange_stream(&session.connection, query).await;
            if early {
                let accepted = session.early_data_accepted().await;
                tracing::debug!(accepted, "DNS over QUIC 0-RTT handshake completed");
                if !accepted && result.is_err() && session.connection.close_reason().is_none() {
                    // Rejected early streams cannot be reused. Repeat the query on
                    // a fresh 1-RTT stream of the authenticated connection.
                    result = exchange_stream(&session.connection, query).await;
                }
            }
            match result {
                Ok(reply) => return Ok(reply),
                Err(_)
                    if attempt == 0
                        && !matches!(
                            session.connection.close_reason(),
                            None | Some(quinn::ConnectionError::LocallyClosed)
                        ) => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }
}

struct Transaction {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    complete: bool,
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.send.reset(DOQ_REQUEST_CANCELLED.into());
            let _ = self.recv.stop(DOQ_REQUEST_CANCELLED.into());
        }
    }
}

async fn exchange_stream(connection: &quinn::Connection, query: &[u8]) -> Result<Vec<u8>> {
    let (send, recv) = connection.open_bi().await.map_err(dns_error)?;
    let mut stream = Transaction {
        send,
        recv,
        complete: false,
    };
    let mut wire_query = query.to_vec();
    wire_query[..2].fill(0);
    write_frame(&mut stream.send, &wire_query).await?;
    stream.send.finish().map_err(dns_error)?;
    let mut reply = match read_frame(&mut stream.recv).await {
        Ok(reply) => reply,
        Err(error) => {
            if matches!(&error, SError::DnsError(_))
                || matches!(&error, SError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof)
            {
                connection.close(DOQ_PROTOCOL_ERROR.into(), b"invalid DNS frame");
            }
            return Err(error);
        }
    };
    // Ordinary queries have exactly one response followed by FIN. read_frame
    // allocates only the advertised DNS length, including for large DNS replies.
    if stream
        .recv
        .read(&mut [0; 1])
        .await
        .map_err(dns_error)?
        .is_some()
        || validate_response(&wire_query, &reply).is_err()
    {
        connection.close(DOQ_PROTOCOL_ERROR.into(), b"invalid DNS response");
        return Err(dns_error("invalid DNS over QUIC response"));
    }
    reply[..2].copy_from_slice(&query[..2]);
    stream.complete = true;
    Ok(reply)
}

/// No OS socket: Quinn datagrams become the two directions of a ProxyRequest.
#[derive(Debug)]
struct ProxySocket {
    send: mpsc::Sender<Datagram>,
    recv: mpsc::Receiver<Datagram>,
    upstream: SocketAddr,
    local_addr: SocketAddr,
}

impl ProxySocket {
    fn new(upstream: SocketAddr, user_context: UserContext) -> (Self, ProxyRequest) {
        let (send, recv_query) = mpsc::channel(QUEUE_SIZE);
        let (send_reply, recv) = mpsc::channel(QUEUE_SIZE);
        let local_addr: SocketAddr = if upstream.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        }
        .parse()
        .unwrap();
        let request: ProxyRequest = ProxyRequest::Udp(UdpSession {
            recv: Box::new(recv_query),
            send: Arc::new(send_reply),
            stream: None,
            bind_addr: local_addr.into(),
            dst: upstream.into(),
            src_addr: None,
            user_context,
        });
        (
            Self {
                send,
                recv,
                upstream,
                local_addr,
            },
            request,
        )
    }
}

impl AsyncUdpSocket for ProxySocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(ProxySender {
            send: self.send.clone(),
            pending: None,
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Bound the work when a proxy delivers unexpected sources or oversize packets.
        for _ in 0..QUEUE_SIZE {
            let Some((packet, source)) = ready!(self.recv.poll_recv(cx)) else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "DNS UDP session closed",
                )));
            };
            if source != self.upstream.into() || packet.len() > bufs[0].len() {
                continue;
            }
            bufs[0][..packet.len()].copy_from_slice(&packet);
            meta[0] = RecvMeta::default();
            meta[0].addr = self.upstream;
            meta[0].len = packet.len();
            meta[0].stride = packet.len();
            return Poll::Ready(Ok(1));
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

type PendingSend =
    Pin<Box<dyn Future<Output = io::Result<mpsc::OwnedPermit<Datagram>>> + Send + Sync>>;

struct ProxySender {
    send: mpsc::Sender<Datagram>,
    pending: Option<PendingSend>,
}

impl fmt::Debug for ProxySender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxySender").finish_non_exhaustive()
    }
}

impl UdpSender for ProxySender {
    fn poll_send(
        mut self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending.is_none() {
            let send = self.send.clone();
            self.pending = Some(Box::pin(async move {
                send.reserve_owned().await.map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "DNS UDP session closed")
                })
            }));
        }
        let permit = ready!(self.pending.as_mut().unwrap().as_mut().poll(cx));
        self.pending = None;
        permit?.send((
            Bytes::copy_from_slice(transmit.contents),
            transmit.destination.into(),
        ));
        Poll::Ready(Ok(()))
    }
}
