use async_trait::async_trait;
use bytes::Bytes;
use std::{
    net::{ToSocketAddrs, UdpSocket},
    sync::Arc,
};
use tokio::{
    io::AsyncReadExt,
    sync::{
        OnceCell, SetOnce,
        mpsc::{Receiver, Sender, channel},
    },
};

use super::EndClient;
use tracing::{Instrument, Level, debug, error, info, span, trace};

use crate::{
    Outbound,
    config::{ShadowQuicClientCfg, SunnyQuicClientCfg},
    error::SError,
    msgs::{
        socks5::{SEncode, SocksAddr},
        squic::{SQCmd, SQReq},
    },
    quic::{QuicClient, QuicConnection},
    squic::{auth_sunny, handle_udp_recv_ctrl, handle_udp_send},
    sunnyquic::gen_sunny_user_hash,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv, inbound::Unsplit};

pub struct SunnyQuicClient {
    pub quic_conn: Option<SQConn<<EndClient as QuicClient>::C>>,
    pub config: SunnyQuicClientCfg,
    pub quic_end: OnceCell<EndClient>,
}
impl SunnyQuicClient {
    pub fn new(cfg: SunnyQuicClientCfg) -> Self {
        Self {
            quic_conn: None,
            quic_end: OnceCell::new(),
            config: cfg,
        }
    }
    pub async fn init_endpoint(&self, ipv6: bool) -> Result<EndClient, SError> {
        EndClient::new(&self.config, ipv6).await
    }
    pub fn new_with_socket(cfg: SunnyQuicClientCfg, socket: UdpSocket) -> Result<Self, SError> {
        Ok(Self {
            quic_end: OnceCell::from(EndClient::new_with_socket(&cfg, socket)?),
            quic_conn: None,
            config: cfg,
        })
    }

    pub async fn get_conn(&self) -> Result<SQConn<<EndClient as QuicClient>::C>, SError> {
        let addr = self
            .config
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr faile: {}", self.config.addr))
            .next()
            .unwrap_or_else(|| panic!("resolve quic addr faile: {}", self.config.addr));
        let end = self
            .quic_end
            .get_or_init(|| async {
                self.init_endpoint(addr.is_ipv6())
                    .await
                    .expect("error during initialize quic endpoint")
            })
            .await;
        let conn = QuicClient::connect(end, addr, &self.config.server_name).await?;

        let conn = SQConn {
            conn,
            authed: Arc::new(SetOnce::new()),
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        };

        auth_sunny(
            &conn,
            &gen_sunny_user_hash(&self.config.username, &self.config.password),
        )
        .await?;

        let conn_clone = conn.clone();
        tokio::spawn(async move {
            let _ = handle_udp_packet_recv(conn_clone)
                .await
                .map_err(|x| error!("handle udp packet recv error: {}", x));
        });
        Ok(conn)
    }
    async fn prepare_conn(&mut self) -> Result<(), SError> {
        // delete connection if closed.
        self.quic_conn.take_if(|x| {
            x.close_reason().is_some_and(|x| {
                info!("quic connection closed due to {}", x);
                true
            })
        });
        // Creating new connectin
        if self.quic_conn.is_none() {
            self.quic_conn = Some(self.get_conn().await?);
        }
        Ok(())
    }
}
#[async_trait]
impl Outbound for SunnyQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;

        let conn = self.quic_conn.as_mut().unwrap().clone();

        let over_stream = self.config.over_stream;
        let (mut send, recv, id) = QuicConnection::open_bi(&conn.conn).await?;
        let _span = span!(Level::TRACE, "bistream", id = id);
        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    debug!("bistream opened for tcp dst:{}", tcp_session.dst.clone());
                    //let _enter = _span.enter();
                    let req = SQReq::SQConnect(tcp_session.dst.clone());
                    req.encode(&mut send).await?;
                    trace!("tcp connect req header sent");

                    let u = tokio::io::copy_bidirectional(
                        &mut Unsplit { s: send, r: recv },
                        &mut tcp_session.stream,
                    )
                    .await?;
                    info!(
                        "request:{} finished, upload:{}bytes,download:{}bytes",
                        tcp_session.dst, u.1, u.0
                    );
                }
                crate::ProxyRequest::Udp(udp_session) => {
                    info!("bistream opened for udp dst:{}", udp_session.dst.clone());
                    let req = if over_stream {
                        SQReq::SQAssociatOverStream(udp_session.dst.clone())
                    } else {
                        SQReq::SQAssociatOverDatagram(udp_session.dst.clone())
                    };
                    req.encode(&mut send).await?;
                    trace!("udp associate req header sent");
                    let fut2 = handle_udp_recv_ctrl(recv, udp_session.send.clone(), conn.clone());
                    let fut1 = handle_udp_send(send, udp_session.recv, conn, over_stream);
                    // control stream, in socks5 inbound, end of control stream
                    // means end of udp association.
                    let fut3 = async {
                        if udp_session.stream.is_none() {
                            return Ok(());
                        }
                        let mut buf = [0u8];
                        udp_session
                            .stream
                            .unwrap()
                            .read_exact(&mut buf)
                            .await
                            .map_err(|x| SError::UDPSessionClosed(x.to_string()))?;
                        error!("unexpected data received from socks control stream");
                        Err(SError::UDPSessionClosed(
                            "unexpected data received from socks control stream".into(),
                        )) as Result<(), SError>
                    };

                    tokio::try_join!(fut1, fut2, fut3)?;
                    info!("udp association to {} ended", udp_session.dst.clone());
                }
            }
            Ok(()) as Result<(), SError>
        };
        tokio::spawn(async {
            let _ = fut.instrument(_span).await.map_err(|x| error!("{}", x));
        });
        Ok(())
    }
}
