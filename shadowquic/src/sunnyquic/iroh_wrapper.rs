use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use iroh_quinn::{
    ClientConfig, MtuDiscoveryConfig, SendDatagramError, TransportConfig,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
    crypto::rustls::QuicClientConfig,
};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, trace, warn};

use rustls::ServerConfig as RustlsServerConfig;

use iroh_quinn::crypto::rustls::QuicServerConfig;

use crate::{
    config::{CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg},
    error::{SError, SResult},
    quic::{
        MAX_DATAGRAM_WINDOW, MAX_SEND_WINDOW, MAX_STREAM_WINDOW, QuicClient, QuicConnection,
        QuicErrorRepr, QuicServer,
    },
};

pub type Connection = iroh_quinn::Connection;
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
}

#[async_trait]
impl QuicClient for Endpoint<SunnyQuicClientCfg> {
    type SC = SunnyQuicClientCfg;
    async fn new(cfg: &Self::SC, ipv6: bool) -> SResult<Self> {
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
        let conn = if self.cfg.zero_rtt {
            match conn.into_0rtt() {
                Ok((x, accepted)) => {
                    let _conn_clone = x.clone();
                    tokio::spawn(async move {
                        debug!("zero rtt accepted: {}", accepted.await);
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
        for path in &self.cfg.extra_paths {
            let mut addrs_iter = path.to_socket_addrs().map_err(|e| {
                QuicErrorRepr::QuicConnect(format!("invalid multipath address {}: {}", path, e))
            })?;
            let path_addr = addrs_iter.next().ok_or_else(|| {
                QuicErrorRepr::QuicConnect(format!("no valid socket address found for {}", path))
            })?;
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
                        Ok(path) => {
                            debug!("added multipath path: {:?}", path.id());
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

pub fn gen_client_cfg(cfg: &SunnyQuicClientCfg) -> iroh_quinn::ClientConfig {
    let mut root_store = RootCertStore::empty();
    for cert in
        rustls_native_certs::load_native_certs().expect("failed to load OS root certificates")
    {
        root_store.add(cert).unwrap();
    }

    if let Some(path) = &cfg.cert_path {
        let der_cert = CertificateDer::from_pem_file(path)
            .unwrap_or_else(|_| panic!("certificate not found:{:?}", path));
        root_store.add_parsable_certificates([der_cert]);
    }

    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    crypto.alpn_protocols = cfg.alpn.iter().map(|x| x.to_owned().into_bytes()).collect();
    crypto.enable_early_data = cfg.zero_rtt;
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

        let cert_der = CertificateDer::pem_file_iter(&cfg.cert_path)
            .map_err(|x| SError::RustlsError(x.to_string()))?
            .filter_map(|x| x.ok())
            .collect();
        let priv_key = PrivateKeyDer::from_pem_file(&cfg.key_path)
            .map_err(|x| SError::RustlsError(x.to_string()))?;

        crypto = RustlsServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(cert_der, priv_key)?;
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
