use std::{io, net::SocketAddr, ops::Deref, sync::Arc, time::Duration};

use super::brutal::BrutalConfig;

use async_trait::async_trait;
use bytes::Bytes;
use quinn::rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use quinn::{
    ClientConfig, MtuDiscoveryConfig, SendDatagramError, TransportConfig, VarInt,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, error, info, trace, warn};

use quinn::rustls::ServerConfig as RustlsServerConfig;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};

use quinn::rustls::crypto::ring;

use crate::{
    config::{
        CipherSuitePreference, CongestionControl, ShadowQuicClientCfg, ShadowQuicServerCfg,
        maybe_warn_cipher_suite_on_weak_arch, normalize_cipher_suite_preference,
    },
    error::SResult,
    quic::{
        MAX_DATAGRAM_WINDOW, MAX_SEND_WINDOW, MAX_STREAM_WINDOW, QuicClient, QuicConnection,
        QuicErrorRepr, QuicServer,
    },
    rebind::Rebindable,
};

pub type Connection = quinn::Connection;
#[derive(Clone)]
pub struct Endpoint {
    inner: quinn::Endpoint,
    zero_rtt: bool,
}
impl Deref for Endpoint {
    type Target = quinn::Endpoint;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Rebindable for Endpoint {
    fn rebind(&self, socket: std::net::UdpSocket) -> Result<(), std::io::Error> {
        self.inner.rebind(socket)
    }
}

pub use Endpoint as EndClient;
pub use Endpoint as EndServer;
#[async_trait]
impl QuicConnection for Connection {
    type RecvStream = quinn::RecvStream;
    type SendStream = quinn::SendStream;
    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
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

    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        let (send, recv) = self.accept_bi().await?;

        let rate: f32 =
            (self.stats().path.lost_packets as f32) / ((self.stats().path.sent_packets + 1) as f32);
        info!(
            "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
            rate * 100.0,
            self.rtt(),
            self.stats().path.current_mtu,
        );

        let id = send.id().index();
        Ok((send, recv, id))
    }

    async fn open_uni(&self) -> Result<(Self::SendStream, u64), QuicErrorRepr> {
        let send = self.open_uni().await?;
        let id = send.id().index();
        Ok((send, id))
    }

    async fn accept_uni(&self) -> Result<(Self::RecvStream, u64), QuicErrorRepr> {
        let recv = self.accept_uni().await?;
        let id = recv.id().index();
        Ok((recv, id))
    }

    async fn read_datagram(&self) -> Result<Bytes, QuicErrorRepr> {
        let bytes = self.read_datagram().await?;
        Ok(bytes)
    }

    async fn send_datagram(&self, bytes: Bytes) -> Result<(), QuicErrorRepr> {
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

    fn close_reason(&self) -> Option<QuicErrorRepr> {
        self.close_reason().map(|x| x.into())
    }
    fn remote_address(&self) -> SocketAddr {
        self.remote_address()
    }
    fn peer_id(&self) -> u64 {
        self.stable_id() as u64
    }
    fn close(&self, error_code: u64, reason: &[u8]) {
        self.close(VarInt::from_u64(error_code).unwrap(), reason);
    }
}

#[async_trait]
impl QuicClient for Endpoint {
    type SC = ShadowQuicClientCfg;
    async fn new(cfg: &Self::SC, ipv6: bool) -> SResult<Self> {
        let try_create_dual_stack = || {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_only_v6(false)?;
            let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            Ok(socket) as Result<Socket, io::Error>
        };
        let socket = if let Ok(socket) = try_create_dual_stack() {
            trace!("dual stack udp socket created");
            socket
        } else if ipv6 {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            socket
        } else {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            socket.bind(&bind_addr.into())?;
            socket
        };

        #[cfg(target_os = "android")]
        if let Some(path) = &cfg.protect_path {
            use crate::utils::protect_socket::protect_socket_with_retry;
            use std::os::fd::AsRawFd;

            tracing::debug!("trying protect socket");
            tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                protect_socket_with_retry(path, socket.as_raw_fd()),
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
    async fn connect(&self, addr: SocketAddr, server_name: &str) -> Result<Self::C, QuicErrorRepr> {
        let conn = self.inner.connect(addr, server_name)?;
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
            return Err(QuicErrorRepr::JlsAuthFailed);
        }
        Ok(conn)
    }

    fn new_with_socket(cfg: &Self::SC, socket: std::net::UdpSocket) -> SResult<Self> {
        let runtime =
            quinn::default_runtime().ok_or_else(|| io::Error::other("no async runtime found"))?;
        let mut end =
            quinn::Endpoint::new(quinn::EndpointConfig::default(), None, socket, runtime)?;
        end.set_default_client_config(gen_client_cfg(cfg));
        Ok(Endpoint {
            inner: end,
            zero_rtt: cfg.zero_rtt,
        })
    }

    type C = Connection;
}

fn to_quinn_cipher_suite(suite: &CipherSuitePreference) -> quinn::rustls::SupportedCipherSuite {
    match suite {
        CipherSuitePreference::Chacha20Poly1305 => {
            ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256
        }
        CipherSuitePreference::Aes128Gcm => ring::cipher_suite::TLS13_AES_128_GCM_SHA256,
        CipherSuitePreference::Aes256Gcm => ring::cipher_suite::TLS13_AES_256_GCM_SHA384,
    }
}

pub fn gen_client_cfg(cfg: &ShadowQuicClientCfg) -> quinn::ClientConfig {
    maybe_warn_cipher_suite_on_weak_arch(cfg);

    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };

    let builder = if let Some(cipher_suite_preference) = &cfg.cipher_suite_preference {
        let normalized = normalize_cipher_suite_preference(cipher_suite_preference);

        let mut provider = ring::default_provider();
        provider.cipher_suites = normalized.iter().map(to_quinn_cipher_suite).collect();

        quinn::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .unwrap()
    } else {
        quinn::rustls::ClientConfig::builder()
    };

    let mut crypto = builder
        .with_root_certificates(root_store)
        .with_no_client_auth();

    crypto.alpn_protocols = cfg.alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
    crypto.enable_early_data = cfg.zero_rtt;
    crypto.jls_config = quinn::rustls::jls::JlsClientConfig::new(&cfg.password, &cfg.username);
    let mut tp_cfg = TransportConfig::default();

    let mtudis = if cfg.mtu_discovery {
        let mut mtudis = MtuDiscoveryConfig::default();
        mtudis.black_hole_cooldown(Duration::from_secs(120));
        mtudis.interval(Duration::from_secs(90));
        Some(mtudis)
    } else {
        None
    };

    tp_cfg
        .max_concurrent_bidi_streams(500u32.into())
        .max_concurrent_uni_streams(500u32.into())
        .mtu_discovery_config(mtudis)
        .min_mtu(cfg.min_mtu)
        .initial_mtu(cfg.initial_mtu)
        .enable_segmentation_offload(cfg.gso);

    if !cfg.gso {
        tracing::warn!("disabling QUIC segmentation offload (GSO)");
    }

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
        CongestionControl::Brutal(ref brutal) => {
            tracing::info!(?brutal, "using brutal congestion control");
            let brutal_config = BrutalConfig::new(
                brutal.bandwidth,
                brutal.min_window,
                brutal.cwnd_gain,
                brutal.min_ack_rate,
                brutal.min_sample_count,
                brutal.ack_compensate,
            );
            tp_cfg.congestion_controller_factory(Arc::new(brutal_config))
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
    type SC = ShadowQuicServerCfg;
    async fn new(cfg: &Self::SC) -> SResult<Self> {
        let mut crypto: RustlsServerConfig;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        crypto =
            RustlsServerConfig::builder_with_protocol_versions(&[&quinn::rustls::version::TLS13])
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

        let mut jls_config = quinn::rustls::jls::JlsServerConfig::default();
        for user in &cfg.users {
            jls_config = jls_config.add_user(user.password.clone(), user.username.clone());
        }
        if let Some(sni) = &cfg.server_name {
            jls_config = jls_config.with_server_name(sni.clone());
        }
        jls_config = jls_config
            .with_rate_limit(cfg.jls_upstream.rate_limit)
            .with_upstream_addr(cfg.jls_upstream.addr.clone())
            .enable(true);
        crypto.jls_config = jls_config.into();

        let mut tp_cfg = TransportConfig::default();

        let mtudis = if cfg.mtu_discovery {
            let mut mtudis = MtuDiscoveryConfig::default();
            mtudis.black_hole_cooldown(Duration::from_secs(120));
            mtudis.interval(Duration::from_secs(90));
            Some(mtudis)
        } else {
            None
        };

        tp_cfg
            .max_concurrent_bidi_streams(1000u32.into())
            .max_concurrent_uni_streams(1000u32.into())
            .mtu_discovery_config(mtudis)
            .min_mtu(cfg.min_mtu)
            .initial_mtu(cfg.initial_mtu)
            .enable_segmentation_offload(cfg.gso);

        if !cfg.gso {
            tracing::warn!("disabling QUIC segmentation offload (GSO)");
        }

        match cfg.congestion_control {
            CongestionControl::Brutal(ref brutal) => {
                tracing::info!(?brutal, "using brutal congestion control");
                let brutal_config = BrutalConfig::new(
                    brutal.bandwidth,
                    brutal.min_window,
                    brutal.cwnd_gain,
                    brutal.min_ack_rate,
                    brutal.min_sample_count,
                    brutal.ack_compensate,
                );
                tp_cfg.congestion_controller_factory(Arc::new(brutal_config))
            }
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

        let endpoint = quinn::Endpoint::server(config, cfg.bind_addr)?;
        Ok(Endpoint {
            inner: endpoint,
            zero_rtt: cfg.zero_rtt,
        })
    }
    async fn accept(&self) -> Result<Self::C, QuicErrorRepr> {
        match self.deref().accept().await {
            Some(conn) => {
                let conn = conn.accept()?;
                let connection = if self.zero_rtt {
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
                    return Err(QuicErrorRepr::JlsAuthFailed);
                }
                Ok(connection)
            }
            None => {
                panic!("Quic endpoint closed");
            }
        }
    }
}

impl From<quinn::ConnectionError> for QuicErrorRepr {
    fn from(value: quinn::ConnectionError) -> Self {
        QuicErrorRepr::QuicConnection(format!("{}", value))
    }
}

impl From<quinn::ConnectError> for QuicErrorRepr {
    fn from(value: quinn::ConnectError) -> Self {
        QuicErrorRepr::QuicConnect(format!("{}", value))
    }
}

impl From<quinn::SendDatagramError> for QuicErrorRepr {
    fn from(value: quinn::SendDatagramError) -> Self {
        QuicErrorRepr::QuicSendDatagramError(format!("{}", value))
    }
}
