use async_trait::async_trait;
use bytes::Bytes;
use std::{
    io,
    net::{ToSocketAddrs, UdpSocket},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::mpsc::{Receiver, Sender, channel},
};

use quinn::{
    ClientConfig, Endpoint, MtuDiscoveryConfig, RecvStream, SendStream, TransportConfig,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::rustls::QuicClientConfig,
};
use rustls::RootCertStore;
use tracing::{Instrument, Level, debug, error, info, span, trace};

use crate::{
    Outbound,
    config::{CongestionControl, ShadowQuicClientCfg},
    error::SError,
    msgs::{
        shadowquic::{SQCmd, SQReq},
        socks5::{SEncode, SocksAddr},
    },
    shadowquic::{handle_udp_recv_ctrl, handle_udp_send},
};

use super::{IDStore, SQConn, handle_udp_packet_recv, inbound::Unsplit};

pub struct ShadowQuicClient {
    pub quic_conn: Option<SQConn>,
    #[allow(dead_code)]
    pub quic_config: quinn::ClientConfig,
    pub quic_end: Endpoint,
    pub dst_addr: String,
    pub server_name: String,
    pub zero_rtt: bool,
    pub over_stream: bool,
}
impl ShadowQuicClient {
    pub fn new(cfg: ShadowQuicClientCfg) -> Self {
        let config = Self::gen_quic_cfg(&cfg);
        let mut end =
            Endpoint::client("[::]:0".parse().unwrap()).expect("Can't create quic endpoint");
        end.set_default_client_config(config.clone());

        Self {
            quic_conn: None,
            quic_config: config,
            quic_end: end,
            dst_addr: cfg.addr,
            server_name: cfg.server_name,
            zero_rtt: cfg.zero_rtt,
            over_stream: cfg.over_stream,
        }
    }
    pub fn new_with_socket(cfg: ShadowQuicClientCfg, socket: UdpSocket) -> Self {
        let config = Self::gen_quic_cfg(&cfg);
        let runtime = quinn::default_runtime()
            .ok_or_else(|| io::Error::other("no async runtime found"))
            .unwrap();
        let mut end = Endpoint::new(quinn::EndpointConfig::default(), None, socket, runtime)
            .expect("Can't create quic endpoint");
        end.set_default_client_config(config.clone());

        Self {
            quic_conn: None,
            quic_config: config,
            quic_end: end,
            dst_addr: cfg.addr,
            server_name: cfg.server_name,
            zero_rtt: cfg.zero_rtt,
            over_stream: cfg.over_stream,
        }
    }
    pub fn gen_quic_cfg(cfg: &ShadowQuicClientCfg) -> quinn::ClientConfig {
        let root_store = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let mut crypto = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        crypto.alpn_protocols = cfg.alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
        crypto.enable_early_data = cfg.zero_rtt;
        crypto.jls_config = rustls::JlsConfig::new(&cfg.jls_pwd, &cfg.jls_iv);
        let mut tp_cfg = TransportConfig::default();

        let mut mtudis = MtuDiscoveryConfig::default();
        mtudis.black_hole_cooldown(Duration::from_secs(120));
        mtudis.interval(Duration::from_secs(90));

        tp_cfg
            .max_concurrent_bidi_streams(500u32.into())
            .max_concurrent_uni_streams(500u32.into())
            .mtu_discovery_config(Some(mtudis))
            .min_mtu(cfg.min_mtu)
            .initial_mtu(cfg.initial_mtu);

        // Only increase receive window to maximize download speed
        tp_cfg.stream_receive_window(super::MAX_STREAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_receive_buffer_size(Some(super::MAX_DATAGRAM_WINDOW as usize));
        tp_cfg.keep_alive_interval(if cfg.keep_alive_interval > 0 {
            Some(Duration::from_millis(cfg.keep_alive_interval as u64))
        } else {
            None
        });

        match cfg.congestion_control {
            CongestionControl::Cubic => {
                tp_cfg.congestion_controller_factory(Arc::new(CubicConfig::default()))
            }
            CongestionControl::NewReno => {
                tp_cfg.congestion_controller_factory(Arc::new(NewRenoConfig::default()))
            }
            CongestionControl::Bbr => {
                tp_cfg.congestion_controller_factory(Arc::new(BbrConfig::default()))
            }
        };
        let mut config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto).expect("rustls config can't created"),
        ));

        config.transport_config(Arc::new(tp_cfg));
        config
    }

    pub async fn get_conn(&self) -> Result<SQConn, SError> {
        let addr = self
            .dst_addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr faile: {}", self.dst_addr))
            .next()
            .unwrap_or_else(|| panic!("resolve quic addr faile: {}", self.dst_addr));
        let conn = self.quic_end.connect(addr, &self.server_name)?;
        let conn = if self.zero_rtt {
            match conn.into_0rtt() {
                Ok((x, accepted)) => {
                    let conn_clone = x.clone();
                    tokio::spawn(async move {
                        debug!("zero rtt accepted: {}", accepted.await);
                        if conn_clone.is_jls() == Some(false) {
                            error!("JLS hijacked or wrong pwd/iv");
                            conn_clone.close(0u8.into(), b"");
                        }
                    });
                    trace!("trying 0-rtt quic connection");
                    x
                }
                Err(e) => {
                    let x = e.await?;
                    trace!("1-rtt quic connection established");
                    x
                }
            }
        } else {
            let x = conn.await?;
            trace!("1-rtt quic connection established");
            x
        };
        if conn.is_jls() == Some(false) {
            error!("JLS hijacked or wrong pwd/iv");
            conn.close(0u8.into(), b"");
            return Err(SError::JlsAuthFailed);
        }
        let conn = SQConn {
            conn,
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        };

        tokio::spawn(handle_udp_packet_recv(conn.clone()));
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

            let conn: SQConn = self.quic_conn.as_ref().unwrap().clone();
            tokio::spawn(handle_udp_packet_recv(conn));
        }
        Ok(())
    }
}
#[async_trait]
impl Outbound for ShadowQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;

        let conn = self.quic_conn.as_mut().unwrap().clone();

        let rate: f32 =
            (conn.stats().path.lost_packets as f32) / ((conn.stats().path.sent_packets + 1) as f32);
        info!(
            "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
            rate * 100.0,
            conn.rtt(),
            conn.stats().path.current_mtu,
        );
        let over_stream = self.over_stream;
        let (mut send, recv) = conn.open_bi().await?;
        let _span = span!(Level::TRACE, "bistream", id = (send.id().index()));
        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    info!("bistream opened for tcp dst:{}", tcp_session.dst.clone());
                    //let _enter = _span.enter();
                    let req = SQReq {
                        cmd: SQCmd::Connect,
                        dst: tcp_session.dst.clone(),
                    };
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
                    let req = SQReq {
                        cmd: if over_stream {
                            SQCmd::AssociatOverStream
                        } else {
                            SQCmd::AssociatOverDatagram
                        },
                        dst: udp_session.dst.clone(),
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
                            .map_err(|_| SError::UDPCtrlStreamClosed)?;
                        error!("unexpected data received from socks control stream");
                        Err(SError::UDPCtrlStreamClosed) as Result<(), SError>
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

/// Helper function to create new stream for proxy dstination
#[allow(dead_code)]
pub async fn connect_tcp(
    sq_conn: &SQConn,
    dst: SocksAddr,
) -> Result<Unsplit<SendStream, RecvStream>, crate::error::SError> {
    let conn = sq_conn;

    let rate: f32 =
        (conn.stats().path.lost_packets as f32) / ((conn.stats().path.sent_packets + 1) as f32);
    info!(
        "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
        rate * 100.0,
        conn.rtt(),
        conn.stats().path.current_mtu,
    );
    let (mut send, recv) = conn.open_bi().await?;

    info!("bistream opened for tcp dst:{}", dst.clone());
    //let _enter = _span.enter();
    let req = SQReq {
        cmd: SQCmd::Connect,
        dst,
    };
    req.encode(&mut send).await?;
    trace!("req header sent");

    Ok(Unsplit { s: send, r: recv })
}

/// associate a udp socket in the remote server
/// return a socket-like send, recv handle.
#[allow(dead_code)]
pub async fn associate_udp(
    sq_conn: &SQConn,
    dst: SocksAddr,
    over_stream: bool,
) -> Result<(Sender<(Bytes, SocksAddr)>, Receiver<(Bytes, SocksAddr)>), SError> {
    let conn = sq_conn;

    let rate: f32 =
        (conn.stats().path.lost_packets as f32) / ((conn.stats().path.sent_packets + 1) as f32);
    info!(
        "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
        rate * 100.0,
        conn.rtt(),
        conn.stats().path.current_mtu,
    );
    let (mut send, recv) = conn.open_bi().await?;

    info!("bistream opened for udp dst:{}", dst.clone());

    let req = SQReq {
        cmd: if over_stream {
            SQCmd::AssociatOverStream
        } else {
            SQCmd::AssociatOverDatagram
        },
        dst: dst.clone(),
    };
    req.encode(&mut send).await?;
    let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
    let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
    let local_send = Arc::new(local_send);
    let fut2 = handle_udp_recv_ctrl(recv, local_send, conn.clone());
    let fut1 = handle_udp_send(send, Box::new(local_recv), conn.clone(), over_stream);

    tokio::spawn(async {
        match tokio::try_join!(fut1, fut2) {
            Err(e) => error!("udp association ended due to {}", e),
            Ok(_) => trace!("udp association ended"),
        }
    });

    Ok((udp_send, udp_recv))
}
