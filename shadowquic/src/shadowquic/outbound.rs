use async_trait::async_trait;
use std::{net::ToSocketAddrs, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

use quinn::{
    ClientConfig, Endpoint, MtuDiscoveryConfig, TransportConfig,
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
        socks5::SEncode,
    },
    shadowquic::{handle_udp_recv_ctrl, handle_udp_send},
};

use super::{SQConn, handle_udp_packet_recv, inbound::Unsplit};

pub struct ShadowQuicClient {
    quic_conn: Option<SQConn>,
    #[allow(dead_code)]
    quic_config: quinn::ClientConfig,
    quic_end: Endpoint,
    dst_addr: String,
    server_name: String,
    zero_rtt: bool,
    over_stream: bool,
}
impl ShadowQuicClient {
    pub fn new(cfg: ShadowQuicClientCfg) -> Self {
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
            .initial_mtu(cfg.initial_mtu);

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
            }
            self.quic_conn = Some(SQConn {
                conn,
                id_store: Default::default(),
            });
            let conn = self.quic_conn.as_ref().unwrap().clone();
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
        let _span = span!(Level::TRACE, "bistream", stream_id = (send.id().index()));
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
                    trace!("req header sent");

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
                    let fut2 = handle_udp_recv_ctrl(recv, udp_session.send.clone(), conn.clone());
                    let fut1 = handle_udp_send(
                        send,
                        udp_session.send,
                        udp_session.recv,
                        conn,
                        over_stream,
                    );
                    let fut3 = async {
                        let mut buf = [0u8];
                        udp_session
                            .stream
                            .unwrap()
                            .read_exact(&mut buf)
                            .await
                            .map_err(|_| SError::UDPCtrlStreamClosed)?;
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
