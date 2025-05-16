use async_trait::async_trait;
use bytes::Bytes;
use std::{net::SocketAddr, pin::Pin, sync::Arc, time::Duration};

use quinn::{
    Endpoint, Incoming, MtuDiscoveryConfig, RecvStream, SendStream, ServerConfig, TransportConfig,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::rustls::QuicServerConfig,
};
use rustls::ServerConfig as RustlsServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, Level, debug, error, event, info, trace, trace_span};

use crate::{
    Inbound, ProxyRequest, TcpSession, TcpTrait, UdpSession,
    config::{CongestionControl, ShadowQuicServerCfg},
    error::SError,
    msgs::{
        shadowquic::{SQCmd, SQReq},
        socks5::{SDecode, SocksAddr},
    },
};

use super::{IDStore, SQConn, handle_udp_packet_recv, handle_udp_recv_ctrl, handle_udp_send};

pub struct ShadowQuicServer {
    pub squic_conn: Vec<SQServerConn>,
    pub quic_config: quinn::ServerConfig,
    pub bind_addr: SocketAddr,
    pub zero_rtt: bool,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

impl ShadowQuicServer {
    pub fn new(cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let mut crypto: RustlsServerConfig;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        crypto = RustlsServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(priv_key))?;
        crypto.alpn_protocols = cfg
            .alpn
            .iter()
            .cloned()
            .map(|alpn| alpn.into_bytes())
            .collect();
        crypto.max_early_data_size = if cfg.zero_rtt { u32::MAX } else { 0 };
        crypto.send_half_rtt_data = cfg.zero_rtt;

        crypto.jls_config =
            rustls::JlsServerConfig::new(&cfg.jls_pwd, &cfg.jls_iv, &cfg.jls_upstream);
        let mut tp_cfg = TransportConfig::default();

        let mut mtudis = MtuDiscoveryConfig::default();
        mtudis.black_hole_cooldown(Duration::from_secs(120));
        mtudis.interval(Duration::from_secs(90));

        tp_cfg
            .max_concurrent_bidi_streams(1000u32.into())
            .max_concurrent_uni_streams(1000u32.into())
            .mtu_discovery_config(Some(mtudis))
            .min_mtu(cfg.min_mtu)
            .initial_mtu(cfg.initial_mtu);
        match cfg.congestion_control {
            CongestionControl::Bbr => {
                let bbr_config = BbrConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(bbr_config))
            }
            CongestionControl::Cubic => {
                let cubic_config = CubicConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(cubic_config))
            }
            CongestionControl::NewReno => {
                let new_reno = NewRenoConfig::default();
                tp_cfg.congestion_controller_factory(Arc::new(new_reno))
            }
        };
        let mut config = ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).expect("rustls config can't created"),
        ));
        tp_cfg.send_window(super::MAX_SEND_WINDOW);
        tp_cfg.stream_receive_window(super::MAX_STREAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_send_buffer_size(super::MAX_DATAGRAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_receive_buffer_size(Some(super::MAX_DATAGRAM_WINDOW as usize));

        config.transport_config(Arc::new(tp_cfg));

        let (send, recv) = channel::<ProxyRequest>(10);

        Ok(Self {
            squic_conn: vec![],
            quic_config: config,
            zero_rtt: cfg.zero_rtt,
            bind_addr: cfg.bind_addr,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming(
        incom: Incoming,
        zero_rtt: bool,
        req_sender: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let conn = incom.accept()?;
        let connection = if zero_rtt {
            match conn.into_0rtt() {
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
            }
        } else {
            conn.await?
        };
        if connection.is_jls() == Some(false) {
            error!("JLS hijacked or wrong pwd/iv");
            connection.close(0u8.into(), b"");
            return Err(SError::JlsAuthFailed);
        }
        let sq_conn = SQServerConn(SQConn {
            conn: connection,
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        });
        let span = trace_span!("quic", id = sq_conn.0.stable_id());
        sq_conn
            .handle_connection(req_sender)
            .instrument(span)
            .await?;

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
        let bind_addr = self.bind_addr;
        let zero_rtt = self.zero_rtt;
        let request_sender = self.request_sender.clone();
        let fut = async move {
            let endpoint =
                Endpoint::server(quic_config, bind_addr).expect("Failed to listening on udp");
            loop {
                match endpoint.accept().await {
                    Some(conn) => {
                        let request_sender = request_sender.clone();
                        tokio::spawn(async move {
                            Self::handle_incoming(conn, zero_rtt, request_sender)
                                .await
                                .map_err(|x| error!("{}", x))
                        });
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

#[derive(Clone)]
pub struct SQServerConn(SQConn);
impl SQServerConn {
    async fn handle_connection(self, req_send: Sender<ProxyRequest>) -> Result<(), SError> {
        let conn = &self.0;
        event!(
            Level::TRACE,
            "incomming from {} accepted",
            conn.remote_address()
        );
        let conn_clone = self.0.clone();
        tokio::spawn(async move {
            let _ = handle_udp_packet_recv(conn_clone).in_current_span().await;
        });

        while conn.close_reason().is_none() {
            select! {
                bi = conn.accept_bi() => {
                    let (send, recv) = bi?;
                    let span = trace_span!("bistream", id = send.id().index());
                    trace!("bistream accepted");
                    tokio::spawn(self.clone().handle_bistream(send, recv, req_send.clone()).instrument(span).in_current_span());
                },
            }
        }
        Ok(())
    }
    async fn handle_bistream(
        self,
        send: SendStream,
        mut recv: RecvStream,
        req_send: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let req = SQReq::decode(&mut recv).await?;

        let rate: f32 = (self.0.conn.stats().path.lost_packets as f32)
            / ((self.0.conn.stats().path.sent_packets + 1) as f32);
        info!(
            "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
            rate * 100.0,
            self.0.conn.rtt(),
            self.0.conn.stats().path.current_mtu,
        );
        match req.cmd {
            SQCmd::Connect => {
                info!("connect request to {} accepted", req.dst.clone());
                let tcp: TcpSession = TcpSession {
                    stream: Box::new(Unsplit { s: send, r: recv }),
                    dst: req.dst,
                };
                req_send
                    .send(ProxyRequest::Tcp(tcp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
            }
            SQCmd::AssociatOverDatagram | SQCmd::AssociatOverStream => {
                info!("association request to {} accepted", req.dst.clone());
                let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
                let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
                let udp: UdpSession = UdpSession {
                    send: Arc::new(udp_send),
                    recv: Box::new(udp_recv),
                    stream: None,
                    dst: req.dst,
                };
                let local_send = Arc::new(local_send);
                req_send
                    .send(ProxyRequest::Udp(udp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
                let fut1 = handle_udp_send(
                    send,
                    Box::new(local_recv),
                    self.0.clone(),
                    req.cmd == SQCmd::AssociatOverStream,
                );
                let fut2 = handle_udp_recv_ctrl(recv, local_send, self.0);
                tokio::try_join!(fut1, fut2)?;
            }
            _ => {}
        }
        Ok(())
    }
}
#[derive(Debug)]
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
