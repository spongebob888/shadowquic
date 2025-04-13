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
use tracing::{Level, Span, debug, error, event, info};

use crate::{
    Inbound, Outbound, ProxyRequest, TcpSession, TcpTrait, UdpSocketTrait,
    error::SError,
    msgs::{
        shadowquic::{SQCmd, SQReq},
        socks5::{SDecode, SEncode, SocksAddr},
    },
};

use super::inbound::Unsplit;

pub struct ShadowQuicClient {
    quic_conn: Option<Connection>,
    quic_config: quinn::ClientConfig,
    quic_end: Endpoint,
    dst_addr: SocketAddr,
    server_name: String,
    zero_rtt: bool,
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
        }
    }
    async fn prepare_conn(&mut self) -> Result<(), SError> {
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
#[async_trait]
impl Outbound for ShadowQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;
        let conn = self.quic_conn.as_mut().unwrap().clone();
        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    let (mut send, mut recv) = conn.open_bi().await?;
                    let req = SQReq {
                        cmd: SQCmd::Connect,
                        dst: tcp_session.dst,
                    };
                    req.encode(&mut send).await?;

                    tokio::io::copy_bidirectional(
                        &mut Unsplit { s: send, r: recv },
                        &mut tcp_session.stream,
                    )
                    .await?;
                }
                crate::ProxyRequest::Udp(udp_session) => todo!(),
            }
            Ok(()) as Result<(), SError>
        };
        tokio::spawn(async {
            fut.await.map_err(|x|error!("{}", x));
        });
        Ok(())
    }
}
