use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use quinn::{
    ClientConfig, Endpoint, MtuDiscoveryConfig, SendDatagramError, TransportConfig,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::rustls::QuicClientConfig,
};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, error, info, trace, warn};

use rustls::ServerConfig as RustlsServerConfig;

use quinn::crypto::rustls::QuicServerConfig;

use crate::{
    config::{CongestionControl, ShadowQuicClientCfg, ShadowQuicServerCfg},
    error::{SError, SResult},
    quic::{QuicClient, QuicConnection, QuicServer},
    shadowquic::{MAX_DATAGRAM_WINDOW, MAX_SEND_WINDOW, MAX_STREAM_WINDOW},
};

pub use quinn::Connection;
pub use quinn::Endpoint as EndClient;
pub use quinn::Endpoint as EndServer;
#[async_trait]
impl QuicConnection for Connection {
    type RecvStream = quinn::RecvStream;
    type SendStream = quinn::SendStream;
    async fn open_bi(&self) -> SResult<(Self::SendStream, Self::RecvStream, u64)> {
        let rate: f32 =
            (self.stats().path.lost_packets as f32) / ((self.stats().path.sent_packets + 1) as f32);
        info!(
            "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
            rate * 100.0,
            self.rtt(),
            self.stats().path.current_mtu,
        );
        let (send, recv) = self.open_bi().await?;

        let id = send.id().index();
        Ok((send, recv, id))
    }

    async fn accept_bi(&self) -> SResult<(Self::SendStream, Self::RecvStream, u64)> {
        let (send, recv) = self.accept_bi().await?;

        let id = send.id().index();
        Ok((send, recv, id))
    }

    async fn open_uni(&self) -> SResult<(Self::SendStream, u64)> {
        let send = self.open_uni().await?;
        let id = send.id().index();
        Ok((send, id))
    }

    async fn accept_uni(&self) -> SResult<(Self::RecvStream, u64)> {
        let recv = self.accept_uni().await?;
        let id = recv.id().index();
        Ok((recv, id))
    }

    async fn read_datagram(&self) -> SResult<Bytes> {
        let bytes = self.read_datagram().await?;
        Ok(bytes)
    }

    async fn send_datagram(&self, bytes: Bytes) -> SResult<()> {
        let len = bytes.len();
        match self.send_datagram(bytes) {
            Ok(_) => (),
            Err(SendDatagramError::TooLarge) => warn!(
                "datagram too large:{}>{}",
                len,
                self.max_datagram_size().unwrap()
            ),
            e => e?,
        }
        Ok(())
    }

    fn close_reason(&self) -> Option<SError> {
        self.close_reason().map(|x| x.into())
    }
    fn remote_address(&self) -> SocketAddr {
        self.remote_address()
    }
    fn peer_id(&self) -> u64 {
        self.stable_id() as u64
    }
}

#[async_trait]
impl QuicClient for Endpoint {
    fn new(cfg: &ShadowQuicClientCfg, ipv6: bool) -> SResult<Self> {
        let socket;
        if ipv6 {
            socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
            if let Err(e) = socket.set_only_v6(false) {
                tracing::warn!(%e, "unable to make socket dual-stack");
            }
            socket.bind(&bind_addr.into())?;
        } else {
            socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
        }

        #[cfg(target_os = "android")]
        if let Some(path) = &self.protect_path {
            use crate::utils::protect_socket::protect_socket;
            use std::os::fd::AsRawFd;

            tracing::debug!("trying protect socket");
            tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                protect_socket(path, socket.as_raw_fd()),
            )
            .await
            .map_err(|_| io::Error::other("protecting socket timeout"))
            .and_then(|x| x)
            .map_err(|e| {
                tracing::error!("error during protecing socket:{}", e);
                e
            })?;
        }

        Self::new_with_socket(cfg, socket.into())
    }
    async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
        zero_rtt: bool,
    ) -> SResult<Self::C> {
        let conn = self.connect(addr, server_name)?;
        let conn = if zero_rtt {
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
        Ok(conn)
    }

    fn new_with_socket(cfg: &ShadowQuicClientCfg, socket: std::net::UdpSocket) -> SResult<Self> {
        let runtime =
            quinn::default_runtime().ok_or_else(|| io::Error::other("no async runtime found"))?;
        let mut end = Endpoint::new(quinn::EndpointConfig::default(), None, socket, runtime)?;
        end.set_default_client_config(gen_quic_cfg(cfg));
        Ok(end)
    }

    type C = Connection;
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
    tp_cfg.stream_receive_window(MAX_STREAM_WINDOW.try_into().unwrap());
    tp_cfg.datagram_receive_buffer_size(Some(MAX_DATAGRAM_WINDOW as usize));
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

#[async_trait]
impl QuicServer for Endpoint {
    type C = Connection;
    fn new(cfg: &ShadowQuicServerCfg) -> SResult<Self> {
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
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).expect("rustls config can't created"),
        ));
        tp_cfg.send_window(MAX_SEND_WINDOW);
        tp_cfg.stream_receive_window(MAX_STREAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_send_buffer_size(MAX_DATAGRAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_receive_buffer_size(Some(MAX_DATAGRAM_WINDOW as usize));

        config.transport_config(Arc::new(tp_cfg));

        let endpoint = Endpoint::server(config, cfg.bind_addr)?;
        Ok(endpoint)
    }
    async fn accept(&self, zero_rtt: bool) -> SResult<Self::C> {
        match self.accept().await {
            Some(conn) => {
                let conn = conn.accept()?;
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
                Ok(connection)
            }
            None => {
                panic!("Quic endpoint closed");
            }
        }
    }
}
