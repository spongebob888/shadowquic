use async_trait::async_trait;
use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::sync::{OnceCell, SetOnce};

use super::quinn_wrapper::EndClient;
use tracing::{error, info};

use crate::{
    Outbound,
    config::ShadowQuicClientCfg,
    error::SError,
    quic::QuicClient,
    rebind::{RebindConfig, RebindEndpoint},
    squic::outbound::handle_request,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv};

pub type ShadowQuicConn = SQConn<<EndClient as QuicClient>::C>;

pub struct ShadowQuicClient {
    pub quic_conn: Option<ShadowQuicConn>,
    pub config: ShadowQuicClientCfg,
    pub quic_end: OnceCell<RebindEndpoint<EndClient>>,
}
impl ShadowQuicClient {
    pub fn new(cfg: ShadowQuicClientCfg) -> Self {
        Self {
            quic_conn: None,
            quic_end: OnceCell::new(),
            config: cfg,
        }
    }
    pub async fn init_endpoint(&self, ipv6: bool) -> Result<EndClient, SError> {
        EndClient::new(&self.config, ipv6).await
    }

    fn resolve_addrs(&self) -> Vec<SocketAddr> {
        let addrs: Vec<SocketAddr> = self
            .config
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr failed: {}", self.config.addr))
            .collect();
        if addrs.is_empty() {
            panic!("resolve quic addr failed: {}", self.config.addr);
        }
        addrs
    }

    async fn connect_addr(&self, addr: SocketAddr) -> Result<ShadowQuicConn, SError> {
        let rebind = self
            .quic_end
            .get_or_init(|| async {
                let (end, is_ipv6) = match self.init_endpoint(true).await {
                    Ok(end) => (end, true),
                    Err(_) => {
                        let end = self
                            .init_endpoint(false)
                            .await
                            .expect("error during initialize quic endpoint");
                        (end, false)
                    }
                };

                RebindEndpoint::with_rebind(
                    end,
                    RebindConfig {
                        interval_ms: self.config.rebind_interval,
                        min_ms: self.config.min_rebind_interval,
                        max_ms: self.config.max_rebind_interval,
                    },
                    is_ipv6,
                )
            })
            .await;

        let conn = QuicClient::connect(rebind.endpoint(), addr, &self.config.server_name).await?;

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
        let addrs = self.resolve_addrs();
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
