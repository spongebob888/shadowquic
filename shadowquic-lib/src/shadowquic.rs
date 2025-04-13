use bytes::Bytes;
use std::{collections::{HashMap, HashSet}, net::SocketAddr, pin::Pin, sync::{ Arc}};

use quinn::{congestion::{BbrConfig, CubicConfig, NewRenoConfig}, crypto::{self, rustls::QuicServerConfig}, Connection, Endpoint, Incoming, RecvStream, SendStream, ServerConfig, TransportConfig, VarInt};
use rustls::ServerConfig as RustlsServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::{io::{AsyncRead, AsyncWrite}, select, sync::{broadcast::error, mpsc::{self, channel, Receiver, Sender}, Notify}};
use tracing::{debug, error, event, info, Level, Span};

use crate::{error::SError, msgs::{shadowquic::{SQCmd, SQReq}, socks5::{SDecode, SEncode, SocksAddr}}, Inbound, Outbound, ProxyRequest, TcpSession, UdpSocketTrait};

pub struct ShadowQuicClient {
    quic_conn: Option<Connection>,
    quic_config: quinn::ClientConfig,
    quic_end: Endpoint,
    dst_addr: SocketAddr,
    server_name: String,
    zero_rtt: bool,
}
impl ShadowQuicClient {
    async fn prepare_conn(&mut self) -> Result<(),SError>{
        self.quic_conn.take_if(|x| {
            x.close_reason().is_some_and(|x| {
                info!("Quic connection closed due to {}", x);
                true
            })
        });
        if self.quic_conn.is_none() {
            let conn = self.quic_end.connect(self.dst_addr, &self.server_name)?;
            let conn = if self.zero_rtt {
                let conn = match conn.into_0rtt() {
                    Ok((x, accepted)) => {
                        let conn_clone = x.clone();
                        tokio::spawn(async move {
                            debug!("zero rtt accepted:{}", accepted.await);
                            if conn_clone.is_jls() == Some(false) {
                                error!("JLS hijacked or wrong pwd/iv");
                                conn_clone.close(0u8.into(), b"");
                            }
                        });
                        x
                    }
                    Err(e) => {
                        let x = e.await?;
                        if x.is_jls() == Some(false) {
                            error!("JLS hijacked or wrong pwd/iv");
                            x.close(0u8.into(), b"");
                        }
                        x
                    }
                };
                conn
            } else {
                let x = conn.await?;
                if x.is_jls() == Some(false) {
                    error!("JLS hijacked or wrong pwd/iv");
                    x.close(0u8.into(), b"");
                }
                x
            };
            self.quic_conn = Some(conn);
        }
        Ok(())
    }
}
impl<T: AsyncRead + AsyncWrite + Unpin, U: UdpSocketTrait> Outbound<T, U> for ShadowQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest<T, U>) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;
        let conn = self.quic_conn.as_mut().unwrap();
        match req {
            crate::ProxyRequest::Tcp(mut tcp_session) => {
                let (mut send,mut recv) = conn.open_bi().await?;
                let req = SQReq {cmd: SQCmd::Connect, dst: tcp_session.dst };
                req.encode(&mut send).await?;

                tokio::io::copy_bidirectional(&mut Unsplit{s:send, r:recv}, &mut tcp_session.stream).await?;
            }
            crate::ProxyRequest::Udp(udp_session) => todo!(),
        }
        Ok(())
    }
}

struct UdpMux(Receiver<Bytes>);
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

type ShadowTcp = Unsplit<SendStream,RecvStream>;
pub struct ShadowQuicServer {
    pub squic_conn: Vec<ShadowQuicConn>,
    pub quic_config: quinn::ServerConfig,
    pub bind_addr: SocketAddr,
    pub zero_rtt: bool,
    pub request_sender: Sender<ProxyRequest<ShadowTcp, UdpMux>>,
    pub request: Receiver<ProxyRequest<ShadowTcp, UdpMux>>,
}
impl ShadowQuicServer {
    fn new(
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
        crypto.alpn_protocols = alpn.iter()
        .cloned()
        .map(|alpn| alpn.into_bytes())
        .collect();
        crypto.max_early_data_size = if zero_rtt {u32::MAX} else {0};
        crypto.send_half_rtt_data = zero_rtt;

        crypto.jls_config = rustls::JlsServerConfig::new(
            &jls_pwd,
            &jls_iv,
            &jls_upstream,
        );
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
        
        let(send,recv) = channel::<ProxyRequest<Unsplit<SendStream, RecvStream>, UdpMux>>(10);

        Ok(Self { 
            squic_conn: vec![], 
            quic_config: config,
            zero_rtt,
            bind_addr,
            request_sender: send,
            request: recv,
            })
    }
    /// Init background job for accepting connection
    async fn init(&self) -> Result<(),SError>{
        let fut1 = async {
            let endpoint = Endpoint::server(self.quic_config.clone(), self.bind_addr).expect("Failed listen on udp");
            loop {
                match endpoint.accept().await {
                    Some(conn) => {
                        tokio::spawn(
                            Self::handle_incoming(conn, self.zero_rtt, self.request_sender.clone())
                        );
                    },
                    None => {
                        error!("Quic endpoint closed");
                    },
                }
            }
        };
        Ok(())
    }
    async fn handle_incoming(incom: Incoming, zero_rtt: bool, req_sender: Sender<ProxyRequest<ShadowTcp, UdpMux>>) -> Result<(),SError> {
            event!(Level::TRACE, "Incomming from {} accepted",incom.remote_address());
            let conn = incom.accept()?;
            let connection;
            if zero_rtt {
                        connection = match conn.into_0rtt() {
                            Ok((conn, accepted))  => {
                                let conn_clone = conn.clone();
                                tokio::spawn(async move {
                                    debug!("zero rtt accepted:{}", accepted.await);
                                    if conn_clone.is_jls() == Some(false) {
                                        error!("JLS hijacked or wrong pwd/iv");
                                        conn_clone.close(0u8.into(), b"");
                                    }
                                });
                                conn
                            },
                            Err(conn) => {
                                conn.await?
                            }
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
            sq_conn.handle_connection(req_sender).await?;
            
        
          Ok(())
    }

}
impl Inbound<ShadowTcp, UdpMux> for ShadowQuicServer {
    async fn accept(&mut self) -> Result<crate::ProxyRequest<ShadowTcp, UdpMux>, SError> {
        let req = self.request.recv().await.ok_or(SError::InboundUnavailable)?;
        return Ok(req);
    }
}
pub struct ShadowQuicConn {
    pub quic_conn: Connection,
    pub sessions: HashMap<u16, Result<VarInt,Notify>>, // Stream ID one to one corespondance to remote client socks5 udp socket,
    pub udp_dispatch_tab: HashMap<VarInt, Sender<Bytes>>,
}
impl ShadowQuicConn {
    async fn handle_connection(self, req_send:Sender<ProxyRequest<ShadowTcp, UdpMux>>) -> Result<(),SError> {
        let conn = &self.quic_conn;
        while(conn.close_reason().is_none()) {
            select! {
                bi = conn.accept_bi() => {
                    let (send, recv) = bi?;
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
    async fn handle_bistream(mut send: SendStream, mut recv: RecvStream, req_send:Sender<ProxyRequest<ShadowTcp, UdpMux>>) -> Result<(),SError> {
        let req = SQReq::decode(&mut recv).await?;
        match req.cmd {
            SQCmd::Connect => {
                let tcp = TcpSession {
                    stream: Unsplit{s:send, r:recv},
                    dst: req.dst,
                };
                req_send.send(ProxyRequest::Tcp(tcp)).await.map_err(|_|SError::OutboundUnavailable)?;
            },
            SQCmd::AssociatOverDatagram => todo!(),
            SQCmd::AssociatOverStream => todo!(),
        }
        Ok(())
    }
    async fn handle_datagram(s:Bytes, req_send:Sender<ProxyRequest<ShadowTcp, UdpMux>>) -> Result<(),SError> {

        Ok(())
    }
}
struct Unsplit<S,R> {
    s:S,
    r:R,
}

impl<S:AsyncWrite + Unpin, R:AsyncRead + Unpin> AsyncRead for Unsplit<S,R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.as_mut().r).poll_read(cx, buf)
    }
}

impl<S:AsyncWrite + Unpin, R:AsyncRead + Unpin> AsyncWrite for Unsplit<S,R> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_shutdown(cx)
    }
}

