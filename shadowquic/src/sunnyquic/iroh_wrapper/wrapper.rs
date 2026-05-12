use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use super::brutal::BrutalConfig;
use async_trait::async_trait;
use bytes::Bytes;
use iroh_quinn::{
    ClientConfig, MtuDiscoveryConfig, SendDatagramError, TransportConfig, VarInt,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
};
use rustls::{
    RootCertStore,
    crypto::ring,
    pki_types::{CertificateDer, pem::PemObject},
};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, trace, warn};

use rustls::ServerConfig as RustlsServerConfig;

use iroh_quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};

use crate::{
    config::{
        CipherSuitePreference, CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg,
        maybe_warn_cipher_suite_on_weak_arch, normalize_cipher_suite_preference,
    },
    error::{SError, SResult},
    quic::{
        MAX_DATAGRAM_WINDOW, MAX_SEND_WINDOW, MAX_STREAM_WINDOW, QuicClient, QuicConnection,
        QuicErrorRepr, QuicServer,
    },
    rebind::Rebindable,
    sunnyquic::dynamic_cert::DynamicCertResolver,
};

pub type Connection = iroh_quinn::Connection;
#[derive(Clone)]
pub struct Endpoint<SC> {
    inner: iroh_quinn::Endpoint,
    cfg: Arc<SC>,
}
impl<SC> Deref for Endpoint<SC> {
    type Target = iroh_quinn::Endpoint;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<SC> Rebindable for Endpoint<SC> {
    fn rebind(&self, socket: std::net::UdpSocket) -> Result<(), std::io::Error> {
        self.inner.rebind(socket)
    }
}

pub type EndClient = Endpoint<SunnyQuicClientCfg>;
pub type EndServer = Endpoint<SunnyQuicServerCfg>;
#[async_trait]
impl QuicConnection for Connection {
    type RecvStream = iroh_quinn::RecvStream;
    type SendStream = iroh_quinn::SendStream;
    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        // let rate: f32 =
        //     (self.stats().path.lost_packets as f32) / ((self.stats().path.sent_packets + 1) as f32);
        // info!(
        //     "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
        //     rate * 100.0,
        //     self.rtt(),
        //     self.stats().path.current_mtu,
        // );
        let (send, recv) = self.open_bi().await?;

        let id = send.id().index();
        Ok((send, recv, id))
    }

    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        let (send, recv) = self.accept_bi().await?;

        // let rate: f32 =
        //     (self.stats().path.lost_packets as f32) / ((self.stats().path.sent_packets + 1) as f32);
        // info!(
        //     "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
        //     rate * 100.0,
        //     self.rtt(),
        //     self.stats().path.current_mtu,
        // );

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
impl QuicClient for Endpoint<SunnyQuicClientCfg> {
    type SC = SunnyQuicClientCfg;
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
        let mut accepted_0rtt = None;
        let conn = if self.cfg.zero_rtt {
            match conn.into_0rtt() {
                Ok((x, accepted)) => {
                    let _conn_clone = x.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                    accepted_0rtt = Some(rx);
                    tokio::spawn(async move {
                        debug!("zero rtt accepted: {}", accepted.await);
                        tx.send(()).unwrap_or(());
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
        tokio::spawn(add_extra_path(
            conn.clone(),
            self.cfg.extra_paths.clone(),
            accepted_0rtt,
        ));
        Ok(conn)
    }

    fn new_with_socket(cfg: &Self::SC, socket: std::net::UdpSocket) -> SResult<Self> {
        let runtime = iroh_quinn::default_runtime()
            .ok_or_else(|| io::Error::other("no async runtime found"))?;
        let end = iroh_quinn::Endpoint::new(
            iroh_quinn::EndpointConfig::default(),
            None,
            socket,
            runtime,
        )?;
        end.set_default_client_config(gen_client_cfg(cfg));
        Ok(Endpoint {
            inner: end,
            cfg: Arc::new(cfg.to_owned()),
        })
    }

    type C = Connection;
}

async fn add_extra_path(
    conn: Connection,
    extra_paths: Vec<String>,
    mut accepted_0rtt: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), QuicErrorRepr> {
    for path in extra_paths {
        let mut addrs_iter = path.to_socket_addrs().map_err(|e| {
            QuicErrorRepr::QuicConnect(format!("invalid multipath address {}: {}", path, e))
        })?;
        let path_addr = addrs_iter.next().ok_or_else(|| {
            QuicErrorRepr::QuicConnect(format!("no valid socket address found for {}", path))
        })?;
        // We must wait for server hello before knowning whether multipath is enabled
        if let Some(x) = &mut accepted_0rtt {
            let _ = x.await;
        }
        if !conn.is_multipath_enabled() {
            warn!(
                "multipath not enabled in quic connection, can't add path {}",
                path
            );
            break;
        }
        let conn = conn.clone();
        let path = path.clone();
        let fut = async move {
            for ii in 0..5 {
                let to_open = conn.open_path_ensure(path_addr, Default::default()).await;
                match to_open {
                    Ok(p) => {
                        debug!("added multipath path to {path}: {:?}", p.id());
                        break;
                    }
                    Err(e) => {
                        if ii == 4 {
                            warn!("failed to add multipath path {}: {e}", path);
                            break;
                        }
                        debug!("failed to add multipath path {path}: {e}, try again");

                        tokio::time::sleep(Duration::from_millis(2000)).await;
                    }
                }
            }
        };
        tokio::spawn(fut);
    }
    Ok(())
}

fn to_rustls_cipher_suite(suite: &CipherSuitePreference) -> rustls::SupportedCipherSuite {
    match suite {
        CipherSuitePreference::Chacha20Poly1305 => {
            ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256
        }
        CipherSuitePreference::Aes128Gcm => ring::cipher_suite::TLS13_AES_128_GCM_SHA256,
        CipherSuitePreference::Aes256Gcm => ring::cipher_suite::TLS13_AES_256_GCM_SHA384,
    }
}

pub fn gen_client_cfg(cfg: &SunnyQuicClientCfg) -> iroh_quinn::ClientConfig {
    maybe_warn_cipher_suite_on_weak_arch(cfg);

    let mut root_store = RootCertStore::empty();
    for cert in
        rustls_native_certs::load_native_certs().expect("failed to load OS root certificates")
    {
        root_store.add(cert).unwrap();
    }

    if let Some(path) = &cfg.cert_path {
        let der_cert = CertificateDer::pem_file_iter(path)
            .unwrap_or_else(|_| panic!("certificate not found:{:?}", path))
            .filter_map(|x| x.ok());
        root_store.add_parsable_certificates(der_cert);
    }

    let builder = if let Some(cipher_suite_preference) = &cfg.cipher_suite_preference {
        let normalized = normalize_cipher_suite_preference(cipher_suite_preference);
        let mut provider = ring::default_provider();
        provider.cipher_suites = normalized.iter().map(to_rustls_cipher_suite).collect();

        rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
    } else {
        rustls::ClientConfig::builder()
    };

    let mut crypto = builder
        .with_root_certificates(root_store)
        .with_no_client_auth();

    crypto.alpn_protocols = cfg.alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
    crypto.enable_early_data = cfg.zero_rtt;
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
        .enable_segmentation_offload(cfg.gso)
        .initial_mtu(cfg.initial_mtu);

    // Only increase receive window to maximize download speed
    tp_cfg.stream_receive_window(MAX_STREAM_WINDOW.try_into().unwrap());
    tp_cfg.datagram_receive_buffer_size(Some(MAX_DATAGRAM_WINDOW as usize));
    tp_cfg.keep_alive_interval(if cfg.keep_alive_interval > 0 {
        Some(Duration::from_millis(cfg.keep_alive_interval as u64))
    } else {
        None
    });
    tp_cfg.max_concurrent_multipath_paths(cfg.max_path_num);

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
impl QuicServer for Endpoint<SunnyQuicServerCfg> {
    type C = Connection;
    type SC = SunnyQuicServerCfg;
    async fn new(cfg: &Self::SC) -> SResult<Self> {
        let mut crypto: RustlsServerConfig;

        let resolver = DynamicCertResolver::new(&cfg.key_path.clone(), &cfg.cert_path.clone())?;

        tokio::spawn(
            resolver
                .clone()
                .watch_cert_and_update(cfg.key_path.clone(), cfg.cert_path.clone()),
        );
        crypto = RustlsServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver.clone()));
        crypto.alpn_protocols = cfg
            .alpn
            .iter()
            .cloned()
            .map(|alpn| alpn.into_bytes())
            .collect();
        crypto.max_early_data_size = if cfg.zero_rtt { u32::MAX } else { 0 };
        crypto.send_half_rtt_data = cfg.zero_rtt;

        for _user in &cfg.users {}

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
            .enable_segmentation_offload(cfg.gso)
            .initial_mtu(cfg.initial_mtu);
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
        let mut config = iroh_quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).expect("rustls config can't created"),
        ));
        tp_cfg.send_window(MAX_SEND_WINDOW);
        tp_cfg.stream_receive_window(MAX_STREAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_send_buffer_size(MAX_DATAGRAM_WINDOW.try_into().unwrap());
        tp_cfg.datagram_receive_buffer_size(Some(MAX_DATAGRAM_WINDOW as usize));
        tp_cfg.max_concurrent_multipath_paths(cfg.max_path_num);

        config.transport_config(Arc::new(tp_cfg));

        let endpoint = iroh_quinn::Endpoint::server(config, cfg.bind_addr)?;
        Ok(Endpoint {
            inner: endpoint,
            cfg: Arc::new(cfg.to_owned()),
        })
    }
    async fn accept(&self) -> Result<Self::C, QuicErrorRepr> {
        match self.deref().accept().await {
            Some(conn) => {
                let conn = conn.accept()?;
                let connection = if self.cfg.zero_rtt {
                    match conn.into_0rtt() {
                        Ok((conn, accepted)) => {
                            let _conn_clone = conn.clone();
                            tokio::spawn(async move {
                                debug!("zero rtt accepted:{}", accepted.await);
                            });
                            conn
                        }
                        Err(conn) => conn.await?,
                    }
                } else {
                    conn.await?
                };
                Ok(connection)
            }
            None => {
                panic!("Quic endpoint closed");
            }
        }
    }
}

impl From<iroh_quinn::ConnectionError> for QuicErrorRepr {
    fn from(value: iroh_quinn::ConnectionError) -> Self {
        QuicErrorRepr::QuicConnection(format!("{}", value))
    }
}

impl From<iroh_quinn::ConnectError> for QuicErrorRepr {
    fn from(value: iroh_quinn::ConnectError) -> Self {
        QuicErrorRepr::QuicConnect(format!("{}", value))
    }
}

impl From<iroh_quinn::SendDatagramError> for QuicErrorRepr {
    fn from(value: iroh_quinn::SendDatagramError) -> Self {
        QuicErrorRepr::QuicSendDatagramError(format!("{}", value))
    }
}

impl From<rustls::Error> for SError {
    fn from(value: rustls::Error) -> Self {
        SError::RustlsError(value.to_string())
    }
}
