use async_trait::async_trait;
use bytes::Bytes;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
};

use quinn::{
    ClientConfig, Connection, Endpoint, Incoming, RecvStream, SendStream, ServerConfig,
    TransportConfig, VarInt,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::{
        self,
        rustls::{QuicClientConfig, QuicServerConfig},
    },
};
use rustls::ClientConfig as RustlsClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{RootCertStore, ServerConfig as RustlsServerConfig};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::{
        Notify,
        broadcast::error,
        mpsc::{self, Receiver, Sender, channel},
    },
};
use tracing::{debug, error, event, info, span, trace, trace_span, Instrument, Level, Span};

use crate::{
    Inbound, Outbound, ProxyRequest, TcpSession, TcpTrait, UdpSocketTrait,
    error::SError,
    msgs::{
        shadowquic::{SQCmd, SQReq},
        socks5::{SDecode, SEncode, SocksAddr},
    },
};



struct UdpMux(Receiver<Bytes>);
#[async_trait]
impl UdpSocketTrait for UdpMux {
    async fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> anyhow::Result<(usize, usize, SocketAddr, SocksAddr), SError> {
        todo!()
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> anyhow::Result<usize, SError> {
        todo!()
    }
}

pub type ShadowTcp = Unsplit<SendStream, RecvStream>;

pub struct ShadowQuicServer {
    pub squic_conn: Vec<ShadowQuicConn>,
    pub quic_config: quinn::ServerConfig,
    pub bind_addr: SocketAddr,
    pub zero_rtt: bool,
    pub request_sender: Sender<ProxyRequest>,
    pub request: Receiver<ProxyRequest>,
}

impl ShadowQuicServer {
    pub fn new(
        bind_addr: SocketAddr,
        jls_pwd: String,
        jls_iv: String,
        jls_upstream: String,
        alpn: Vec<String>,
        zero_rtt: bool,
        cogestion_controller: String,
    ) -> Result<Self, SError> {
        let mut crypto: RustlsServerConfig;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        crypto = RustlsServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(priv_key))?;
        crypto.alpn_protocols = alpn.iter().cloned().map(|alpn| alpn.into_bytes()).collect();
        crypto.max_early_data_size = if zero_rtt { u32::MAX } else { 0 };
        crypto.send_half_rtt_data = zero_rtt;

        crypto.jls_config = rustls::JlsServerConfig::new(&jls_pwd, &jls_iv, &jls_upstream);
        let mut tp_cfg = TransportConfig::default();
        tp_cfg.max_concurrent_bidi_streams(1000u32.into());
        match cogestion_controller.as_str() {
            "bbr" => {
                let mut bbr_config = BbrConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(bbr_config))
            }
            "cubic" => {
                let mut cubic_config = CubicConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(cubic_config))
            }
            "newreno" => {
                let mut new_reno = NewRenoConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(new_reno))
            }
            _ => {
                panic!("Unsupported congestion controller");
            }
        };
        let mut config = ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).expect("rustls config can't created"),
        ));
        config.transport_config(Arc::new(tp_cfg));

        let (send, recv) = channel::<ProxyRequest>(10);

        Ok(Self {
            squic_conn: vec![],
            quic_config: config,
            zero_rtt,
            bind_addr,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming(
        incom: Incoming,
        zero_rtt: bool,
        req_sender: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        event!(
            Level::TRACE,
            "Incomming from {} accepted",
            incom.remote_address()
        );
        let conn = incom.accept()?;
        let connection;
        if zero_rtt {
            connection = match conn.into_0rtt() {
                Ok((conn, accepted)) => {
                    let conn_clone = conn.clone();
                    tokio::spawn(async move {
                        debug!("zero rtt accepted:{}", accepted.await);
                        if conn_clone.is_jls() == Some(false) {
                            error!("JLS hijacked or wrong pwd/iv");
                            conn_clone.close(0u8.into(), b"");
                        }
                    });
                    conn
                }
                Err(conn) => conn.await?,
            };
        } else {
            connection = conn.await?;
        }
        if connection.is_jls() == Some(false) {
            error!("JLS hijacked or wrong pwd/iv");
            connection.close(0u8.into(), b"");
            return Err(SError::JlsAuthFailed);
        }
        let sq_conn = ShadowQuicConn {
            quic_conn: connection,
            sessions: Default::default(),
            udp_dispatch_tab: Default::default(),
        };
        let span = trace_span!("quic conn", id = sq_conn.quic_conn.stable_id());
        sq_conn.handle_connection(req_sender).instrument(span).await?;

        Ok(())
    }
}

#[async_trait]
impl Inbound for ShadowQuicServer {
    async fn accept(&mut self) -> Result<crate::ProxyRequest, SError> {
        let req = self
            .request
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        return Ok(req);
    }
    /// Init background job for accepting connection
    async fn init(&self) -> Result<(), SError> {
        let quic_config = self.quic_config.clone();
        let bind_addr = self.bind_addr.clone();
        let zero_rtt = self.zero_rtt;
        let request_sender = self.request_sender.clone();
        let fut = async move {
            let endpoint =
                Endpoint::server(quic_config, bind_addr).expect("Failed to listening on udp");
            loop {
                match endpoint.accept().await {
                    Some(conn) => {
                        tokio::spawn(Self::handle_incoming(
                            conn,
                            zero_rtt,
                            request_sender.clone(),
                        ));
                    }
                    None => {
                        error!("Quic endpoint closed");
                    }
                }
            }
        };
        tokio::spawn(fut);
        Ok(())
    }
}
pub struct ShadowQuicConn {
    pub quic_conn: Connection,
    pub sessions: HashMap<u16, Result<VarInt, Notify>>, // Stream ID one to one corespondance to remote client socks5 udp socket,
    pub udp_dispatch_tab: HashMap<VarInt, Sender<Bytes>>,
}
impl ShadowQuicConn {
    async fn handle_connection(self, req_send: Sender<ProxyRequest>) -> Result<(), SError> {
        let conn = &self.quic_conn;
        while (conn.close_reason().is_none()) {
            select! {
                bi = conn.accept_bi() => {
                    let (send, recv) = bi?;
                    trace!("bi stream accepted");
                    tokio::spawn(Self::handle_bistream(send, recv, req_send.clone()));
                },
                datagram = conn.read_datagram() => {
                    let datagram = datagram?;
                    tokio::spawn(Self::handle_datagram(datagram, req_send.clone()));
                },
            }
        }
        Ok(())
    }
    async fn handle_bistream(
        mut send: SendStream,
        mut recv: RecvStream,
        req_send: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let req = SQReq::decode(&mut recv).await?;
        match req.cmd {
            SQCmd::Connect => {
                let tcp: TcpSession = TcpSession {
                    stream: Box::new(Unsplit { s: send, r: recv }),
                    dst: req.dst,
                };
                req_send
                    .send(ProxyRequest::Tcp(tcp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
            }
            SQCmd::AssociatOverDatagram => todo!(),
            SQCmd::AssociatOverStream => todo!(),
        }
        Ok(())
    }
    async fn handle_datagram(s: Bytes, req_send: Sender<ProxyRequest>) -> Result<(), SError> {
        Ok(())
    }
}
pub struct Unsplit<S, R> {
    pub s: S,
    pub r: R,
}
impl TcpTrait for Unsplit<SendStream, RecvStream> {}

impl<S: AsyncWrite + Unpin, R: AsyncRead + Unpin> AsyncRead for Unsplit<S, R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.as_mut().r).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin, R: AsyncRead + Unpin> AsyncWrite for Unsplit<S, R> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_shutdown(cx)
    }
}
