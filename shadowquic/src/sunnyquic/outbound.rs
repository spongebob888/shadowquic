use async_trait::async_trait;
use rand::Rng;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::SetOnce;

use super::EndClient;
use tracing::{debug, error, info, warn};

use crate::{
    Outbound,
    config::{
        MAX_DRAINING_PATHS, MIN_PORT_HOP_INTERVAL, PORT_HOP_DRAIN_TIMEOUT, SunnyQuicClientCfg,
    },
    error::SError,
    quic::{QuicClient, QuicConnection},
    squic::{auth_sunny, outbound::handle_request},
    sunnyquic::gen_sunny_user_hash,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv};

pub type SunnyQuicConn = SQConn<<EndClient as QuicClient>::C>;

struct DrainingPath {
    end: EndClient,
    conn: SunnyQuicConn,
    addr: SocketAddr,
    since: Instant,
}

pub struct SunnyQuicClient {
    pub quic_conn: Option<SunnyQuicConn>,
    pub config: SunnyQuicClientCfg,
    pub quic_end: Option<EndClient>,
    current_addr: Option<SocketAddr>,
    draining: Vec<DrainingPath>,

    /// Flag indicating that a port hop should be performed on the next prepare_conn call
    hop_requested: Arc<AtomicBool>,
    /// graceful shutdown signal sender for hop timer
    hop_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for SunnyQuicClient {
    fn drop(&mut self) {
        if let Some(tx) = self.hop_shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

static PORT_HOP_WARN: AtomicBool = AtomicBool::new(false);

impl SunnyQuicClient {
    pub fn new(cfg: SunnyQuicClientCfg) -> Self {
        let hop_requested = Arc::new(AtomicBool::new(false));
        let (hop_shutdown_tx, mut hop_shutdown_rx) = tokio::sync::oneshot::channel();

        if let Some(port_hop) = &cfg.port_hop {
            let hop_requested_clone = hop_requested.clone();
            let interval = port_hop.interval.max(MIN_PORT_HOP_INTERVAL);

            if !PORT_HOP_WARN.swap(true, Ordering::Relaxed) {
                warn!(
                    "port hop enabled: interval {}s, range: {}-{}",
                    interval, port_hop.range.start, port_hop.range.end
                );
            }

            tokio::spawn(async move {
                let mark_pending = || {
                    hop_requested_clone.swap(true, Ordering::SeqCst);
                };

                mark_pending();

                loop {
                    let wait_time = {
                        let mut rng = rand::rng();
                        rng.random_range(MIN_PORT_HOP_INTERVAL..=interval)
                    };
                    // debug!("scheduled port hop request in {} seconds", wait_time);

                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(wait_time)) => {
                            mark_pending();
                        }
                        _ = &mut hop_shutdown_rx => {
                            break;
                        }
                    }
                }
            });
        }

        Self {
            quic_conn: None,
            quic_end: None,
            current_addr: None,
            draining: Vec::new(),
            config: cfg,
            hop_requested,
            hop_shutdown_tx: Some(hop_shutdown_tx),
        }
    }

    pub async fn init_endpoint(&self, ipv6: bool) -> Result<EndClient, SError> {
        EndClient::new(&self.config, ipv6).await
    }

    pub fn new_with_socket(cfg: SunnyQuicClientCfg, socket: UdpSocket) -> Result<Self, SError> {
        let mut client = Self::new(cfg);
        client.quic_end = Some(EndClient::new_with_socket(&client.config, socket)?);
        Ok(client)
    }

    fn resolve_base_addr(&self) -> SocketAddr {
        self.config
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr faile: {}", self.config.addr))
            .next()
            .unwrap_or_else(|| panic!("resolve quic addr faile: {}", self.config.addr))
    }

    fn select_hop_addr(&self) -> SocketAddr {
        let base_addr = self.resolve_base_addr();
        let ip = base_addr.ip();
        let base_port = base_addr.port();

        let target_port = match &self.config.port_hop {
            Some(port_hop) => {
                let mut rng = rand::rng();
                let (start, end) = (port_hop.range.start, port_hop.range.end);
                rng.random_range(start..=end)
            }
            None => base_port,
        };

        SocketAddr::new(ip, target_port)
    }

    async fn build_endpoint(&self) -> Result<EndClient, SError> {
        match self.init_endpoint(true).await {
            Ok(ep) => Ok(ep),
            Err(_) => self.init_endpoint(false).await,
        }
    }

    fn wrap_conn(raw: <EndClient as QuicClient>::C) -> SunnyQuicConn {
        SQConn {
            conn: raw,
            authed: Arc::new(SetOnce::new()),
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        }
    }

    fn spawn_recv_tasks(&self, conn: SunnyQuicConn) {
        let username = self.config.username.clone();
        let password = self.config.password.clone();

        tokio::spawn(async move {
            let _ = auth_sunny(&conn, gen_sunny_user_hash(&username, &password))
                .await
                .map_err(|x| error!("authentication failed: {}", x));

            let _ = handle_udp_packet_recv(conn)
                .await
                .map_err(|x| error!("handle udp packet recv error: {}", x));
        });
    }

    async fn connect_with_endpoint(
        &self,
        end: &EndClient,
        addr: SocketAddr,
    ) -> Result<SunnyQuicConn, SError> {
        let raw = QuicClient::connect(end, addr, &self.config.server_name).await?;
        let conn = Self::wrap_conn(raw);
        self.spawn_recv_tasks(conn.clone());
        Ok(conn)
    }

    async fn build_path(&self, addr: SocketAddr) -> Result<(EndClient, SunnyQuicConn), SError> {
        let end = self.build_endpoint().await?;
        let conn = self.connect_with_endpoint(&end, addr).await?;
        Ok((end, conn))
    }

    fn cleanup_draining(&mut self) {
        self.draining.retain(|path| {
            let _keep_endpoint_alive = &path.end;

            if let Some(reason) = QuicConnection::close_reason(&path.conn.conn) {
                debug!(
                    "drained quic connection to {} closed due to {}",
                    path.addr, reason
                );
                return false;
            }

            if path.since.elapsed() >= PORT_HOP_DRAIN_TIMEOUT {
                debug!(
                    "closing drained quic connection to {} after {:?}",
                    path.addr, PORT_HOP_DRAIN_TIMEOUT
                );
                path.conn.conn.close(0u8.into(), b"port hop drain timeout");
                return false;
            }

            true
        });

        while self.draining.len() > MAX_DRAINING_PATHS {
            let old = self.draining.remove(0);
            debug!(
                "closing oldest drained quic connection to {} because draining set exceeds {}",
                old.addr, MAX_DRAINING_PATHS
            );
            old.conn
                .conn
                .close(0u8.into(), b"port hop draining overflow");
        }
    }

    pub async fn get_conn(&self) -> Result<SunnyQuicConn, SError> {
        self.quic_conn
            .clone()
            .ok_or_else(|| std::io::Error::other("quic connection not prepared").into())
    }

    async fn hop_port(&mut self) -> Result<(), SError> {
        let addr = self.select_hop_addr();
        info!("starting soft port hop to server {}", addr);

        // 1. Build new path first. If this fails, the old path remains intact.
        let (new_end, new_conn) = self.build_path(addr).await?;

        // 2. Move old active path into draining instead of closing it immediately.
        if let Some(old_conn) = self.quic_conn.take() {
            if let Some(old_end) = self.quic_end.take() {
                let old_addr = self
                    .current_addr
                    .unwrap_or_else(|| self.resolve_base_addr());
                debug!("moving old quic path {} into draining set", old_addr);

                self.draining.push(DrainingPath {
                    end: old_end,
                    conn: old_conn,
                    addr: old_addr,
                    since: Instant::now(),
                });
            } else {
                // Should not normally happen, but if it does, we cannot keep the old path alive safely.
                warn!(
                    "active quic connection exists without endpoint; closing old path during hop"
                );
                old_conn
                    .conn
                    .close(0u8.into(), b"port hop missing endpoint");
            }
        }

        // 3. Switch new path to active.
        self.quic_end = Some(new_end);
        self.quic_conn = Some(new_conn);
        self.current_addr = Some(addr);

        // 4. Opportunistic cleanup.
        self.cleanup_draining();

        debug!("soft port hopped to server {}", addr);
        Ok(())
    }

    async fn ensure_active_conn(&mut self) -> Result<(), SError> {
        if self.quic_conn.is_some() {
            return Ok(());
        }

        let addr = self
            .current_addr
            .unwrap_or_else(|| self.resolve_base_addr());

        let conn = if let Some(end) = self.quic_end.as_ref() {
            Some(self.connect_with_endpoint(end, addr).await?)
        } else {
            None
        };

        if let Some(conn) = conn {
            self.quic_conn = Some(conn);
            self.current_addr = Some(addr);
            return Ok(());
        }

        let (end, conn) = self.build_path(addr).await?;
        self.quic_end = Some(end);
        self.quic_conn = Some(conn);
        self.current_addr = Some(addr);
        Ok(())
    }

    async fn prepare_conn(&mut self) -> Result<(), SError> {
        // Clean up old drained paths first.
        self.cleanup_draining();

        // If active connection is already closed, drop only the active connection.
        // Keep the active endpoint so normal reconnect behavior stays unchanged.
        let active_closed_reason = self
            .quic_conn
            .as_ref()
            .and_then(|x| QuicConnection::close_reason(&x.conn));

        if let Some(reason) = active_closed_reason {
            debug!("active quic connection closed due to {}", reason);
            self.quic_conn = None;
        }

        // Process pending hop request using make-before-break.
        if self.hop_requested.swap(false, Ordering::SeqCst) {
            match self.hop_port().await {
                Ok(()) => {}
                Err(e) => error!("hop_port failed: {}", e),
            }
        }

        // Ensure there is an active connection.
        self.ensure_active_conn().await?;

        Ok(())
    }
}

#[async_trait]
impl Outbound for SunnyQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;

        let conn = self.quic_conn.as_ref().unwrap().clone();
        let over_stream = self.config.over_stream;

        handle_request(req, conn, over_stream).await?;
        Ok(())
    }
}
