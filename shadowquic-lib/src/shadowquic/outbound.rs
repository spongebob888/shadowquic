use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    net::SocketAddr,
    ops::Deref,
    sync::{ Arc}, thread::spawn,
};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender}, Mutex
};

use quinn::{
    ClientConfig, Connection, Endpoint, RecvStream, SendStream, TransportConfig,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::rustls::QuicClientConfig,
};
use rustls::RootCertStore;
use tracing::{debug, debug_span, error, event, info, span, trace, trace_span, Instrument, Level};

use bytes::{Buf,buf::Reader, buf::Writer};

use crate::{
    error::SError, msgs::{
        shadowquic::{
            SQCmd, SQPacketDatagramHeader, SQPacketStreamHeader, SQReq, SQUdpControlHeader,
        },
        socks5::{SDecode, SEncode, SocksAddr},
    }, shadowquic::{handle_udp_recv_overdatagram, handle_udp_send_overdatagram}, AnyUdpRecv, AnyUdpSend, Outbound, UdpSession
};

use super::{handle_udp_packet_recv, inbound::Unsplit, AssociateRecvSession, AssociateSendSession, IDStore, SQConn};

pub struct ShadowQuicClient {
    quic_conn: Option<SQConn>,
    quic_config: quinn::ClientConfig,
    quic_end: Endpoint,
    dst_addr: SocketAddr,
    server_name: String,
    zero_rtt: bool,
    over_stream: bool,
}
impl ShadowQuicClient {
    pub fn new(
        jls_pwd: String,
        jls_iv: String,
        dst_addr: SocketAddr,
        server_name: String,
        alpn: Vec<String>,
        initial_mtu: u16,
        congestion_controller: String,
        zero_rtt: bool,
    ) -> Self {
        let root_store = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let mut crypto = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        crypto.alpn_protocols = alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
        crypto.enable_early_data = zero_rtt;
        crypto.jls_config = rustls::JlsConfig::new(&jls_pwd, &jls_iv);
        let mut tp_cfg = TransportConfig::default();

        tp_cfg
            .max_concurrent_bidi_streams(500u32.into())
            .initial_mtu(initial_mtu);

        match congestion_controller.as_str() {
            "cubic" => tp_cfg.congestion_controller_factory(Arc::new(CubicConfig::default())),
            "newreno" => tp_cfg.congestion_controller_factory(Arc::new(NewRenoConfig::default())),
            "bbr" => tp_cfg.congestion_controller_factory(Arc::new(BbrConfig::default())),
            _ => {
                panic!("Unsupported congestion controller");
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
            dst_addr,
            server_name,
            zero_rtt,
            over_stream: false,
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
            let conn = self.quic_end.connect(self.dst_addr, &self.server_name)?;
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
        let span = debug_span!("quic conn", id = conn.stable_id());
        let over_stream = self.over_stream;
        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    let (mut send, recv) = conn.open_bi().await?;
                    let _span = span!(Level::TRACE, "tcp", stream_id = (send.id().index()));
                    trace!("bistream opened");
                    let enter = _span.enter();
                    let req = SQReq {
                        cmd: SQCmd::Connect,
                        dst: tcp_session.dst.clone(),
                    };
                    req.encode(&mut send).await?;
                    trace!("req header sent");

                    tokio::io::copy_bidirectional(
                        &mut Unsplit { s: send, r: recv },
                        &mut tcp_session.stream,
                    )
                    .await?;
                    trace!("request:{} finished", tcp_session.dst);
                }
                crate::ProxyRequest::Udp(udp_session) => {
                    let (mut send, recv) = conn.open_bi().await?;
                    let _span = span!(Level::TRACE, "udp", stream_id = (send.id().index()));
                    trace!("bistream opened");
                    let enter = _span.enter();
                    let req = SQReq {
                        cmd: if over_stream {
                            SQCmd::AssociatOverStream
                        } else {
                            SQCmd::AssociatOverDatagram
                        },
                        dst: udp_session.dst.clone(),
                    };
                    req.encode(&mut send).await?;
                    let fut2 = handle_udp_recv_overdatagram(recv, udp_session.send.clone(), conn.clone(), over_stream);
                    let fut1 = handle_udp_send_overdatagram(send, udp_session.send, udp_session.recv, conn, over_stream);

                    tokio::try_join!(fut1, fut2)?;
                    trace!("req header sent");
                }
            }
            Ok(()) as Result<(), SError>
        };
        tokio::spawn(async {
            let _ = fut.instrument(span).await.map_err(|x| error!("{}", x));
        });
        Ok(())
    }
}
