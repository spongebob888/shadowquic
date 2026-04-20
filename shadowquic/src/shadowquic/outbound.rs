use async_trait::async_trait;
use std::{
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::Arc,
};
use tokio::sync::{OnceCell, SetOnce};

use super::quinn_wrapper::EndClient;
use tracing::{error, info};

use crate::{
    Outbound, config::ShadowQuicClientCfg, error::SError, quic::QuicClient,
    squic::outbound::handle_request,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv};

pub type ShadowQuicConn = SQConn<<EndClient as QuicClient>::C>;

pub struct ShadowQuicClient {
    pub quic_conn: Option<ShadowQuicConn>,
    pub config: ShadowQuicClientCfg,
    pub quic_end_v4: OnceCell<EndClient>,
    pub quic_end_v6: OnceCell<EndClient>,
    pub local_proxy_addr: OnceCell<std::net::SocketAddr>,
}

impl ShadowQuicClient {
    pub fn new(cfg: ShadowQuicClientCfg) -> Self {
        Self {
            quic_conn: None,
            quic_end_v4: OnceCell::new(),
            quic_end_v6: OnceCell::new(),
            local_proxy_addr: OnceCell::new(),
            config: cfg,
        }
    }

    pub async fn init_endpoint(&self, ipv6: bool) -> Result<EndClient, SError> {
        EndClient::new(&self.config, ipv6).await
    }

    pub fn new_with_socket(cfg: ShadowQuicClientCfg, socket: UdpSocket) -> Result<Self, SError> {
        let local = socket.local_addr()?;
        let end = EndClient::new_with_socket(&cfg, socket)?;

        Ok(if local.is_ipv6() {
            Self {
                quic_conn: None,
                quic_end_v4: OnceCell::new(),
                quic_end_v6: OnceCell::from(end),
                local_proxy_addr: OnceCell::new(),
                config: cfg,
            }
        } else {
            Self {
                quic_conn: None,
                quic_end_v4: OnceCell::from(end),
                quic_end_v6: OnceCell::new(),
                local_proxy_addr: OnceCell::new(),
                config: cfg,
            }
        })
    }

    fn hop_enabled(&self) -> bool {
        let hop_interval = self
            .config
            .hop_interval
            .or(self.config.min_hop_interval)
            .unwrap_or(0);

        crate::utils::udphop::UdpHopAddr::parse(&self.config.addr)
            .map(|addr| addr.hop_enabled_with_interval(hop_interval))
            .unwrap_or(false)
    }

    fn resolve_addrs(&self) -> Vec<SocketAddr> {
        let addrs =
            if let Ok(udphop_addr) = crate::utils::udphop::UdpHopAddr::parse(&self.config.addr) {
                udphop_addr
                    .first_resolved_addrs()
                    .unwrap_or_else(|_| panic!("resolve quic addr failed: {}", self.config.addr))
            } else {
                self.config
                    .addr
                    .to_socket_addrs()
                    .unwrap_or_else(|_| panic!("resolve quic addr failed: {}", self.config.addr))
                    .collect()
            };

        if addrs.is_empty() {
            panic!("resolve quic addr failed: {}", self.config.addr);
        }

        addrs
    }

    async fn get_local_proxy_addr(&self) -> SocketAddr {
        *self
            .local_proxy_addr
            .get_or_init(|| async {
                let hop_interval = self
                    .config
                    .hop_interval
                    .or(self.config.min_hop_interval)
                    .unwrap_or(0);

                if let Ok(udphop_addr) = crate::utils::udphop::UdpHopAddr::parse(&self.config.addr)
                {
                    if udphop_addr.hop_enabled_with_interval(hop_interval) {
                        let min_interval = self
                            .config
                            .min_hop_interval
                            .or(self.config.hop_interval)
                            .unwrap_or(30000);
                        let max_interval = self
                            .config
                            .max_hop_interval
                            .or(self.config.hop_interval)
                            .unwrap_or(30000);

                        match crate::utils::udphop::UdpHopClientProxy::start(
                            &udphop_addr,
                            min_interval,
                            max_interval,
                        )
                        .await
                        {
                            Ok(addr) => return addr,
                            Err(e) => {
                                tracing::error!("Failed to start UDP hop proxy: {}", e);
                            }
                        }
                    }

                    udphop_addr.first_resolved_addr().unwrap_or_else(|_| {
                        panic!("resolve quic addr failed: {}", self.config.addr)
                    })
                } else {
                    self.config
                        .addr
                        .to_socket_addrs()
                        .unwrap_or_else(|_| {
                            panic!("resolve quic addr failed: {}", self.config.addr)
                        })
                        .next()
                        .unwrap_or_else(|| panic!("resolve quic addr failed: {}", self.config.addr))
                }
            })
            .await
    }

    async fn target_addrs(&self) -> Vec<SocketAddr> {
        if self.hop_enabled() {
            vec![self.get_local_proxy_addr().await]
        } else {
            self.resolve_addrs()
        }
    }

    async fn connect_addr(&self, addr: SocketAddr) -> Result<ShadowQuicConn, SError> {
        let end = if addr.is_ipv6() {
            self.quic_end_v6
                .get_or_init(|| async {
                    self.init_endpoint(true)
                        .await
                        .expect("error during initialize quic ipv6 endpoint")
                })
                .await
        } else {
            self.quic_end_v4
                .get_or_init(|| async {
                    self.init_endpoint(false)
                        .await
                        .expect("error during initialize quic ipv4 endpoint")
                })
                .await
        };

        let conn = end.connect(addr, &self.config.server_name).await?;

        let conn = SQConn {
            conn,
            authed: Arc::new(SetOnce::new_with(Some(true))),
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        };

        let conn_clone = conn.clone();
        tokio::spawn(async move {
            let _ = handle_udp_packet_recv(conn_clone)
                .await
                .map_err(|x| error!("handle udp packet recv error: {}", x));
        });

        Ok(conn)
    }

    pub async fn get_conn(&self) -> Result<ShadowQuicConn, SError> {
        let addrs = self.target_addrs().await;
        let mut last_err = None;

        for addr in addrs {
            match self.connect_addr(addr).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    error!("connect to {} failed: {}", addr, e);
                    last_err = Some(e);
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| panic!("no usable quic target address: {}", self.config.addr)))
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
impl Outbound for ShadowQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;

        let conn = self.quic_conn.as_mut().unwrap().clone();

        let over_stream = self.config.over_stream;
        handle_request(req, conn, over_stream).await?;
        Ok(())
    }
}
