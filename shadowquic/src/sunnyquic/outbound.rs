use async_trait::async_trait;
use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::sync::{OnceCell, SetOnce};

use super::EndClient;
use tracing::{error, info};

use crate::{
    Outbound,
    config::SunnyQuicClientCfg,
    error::SError,
    quic::{QuicClient, QuicConnection},
    rebind::{RebindConfig, RebindEndpoint},
    squic::{auth_sunny, outbound::handle_request},
    sunnyquic::gen_sunny_user_hash,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv};

pub type SunnyQuicConn = SQConn<<EndClient as QuicClient>::C>;

pub struct SunnyQuicClient {
    pub quic_conn: Option<SunnyQuicConn>,
    pub config: SunnyQuicClientCfg,
    pub quic_end: OnceCell<RebindEndpoint<EndClient>>,
}

impl SunnyQuicClient {
    pub fn new(cfg: SunnyQuicClientCfg) -> Self {
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

    async fn connect_addr(&self, addr: SocketAddr) -> Result<SunnyQuicConn, SError> {
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
            authed: Arc::new(SetOnce::new()),
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        };

        let username = self.config.username.clone();
        let password = self.config.password.clone();
        let conn_clone = conn.clone();
        tokio::spawn(async move {
            let _ = auth_sunny(&conn_clone, gen_sunny_user_hash(&username, &password))
                .await
                .map_err(|x| error!("authentication failed: {}", x));
            let _ = handle_udp_packet_recv(conn_clone)
                .await
                .map_err(|x| error!("handle udp packet recv error: {}", x));
        });
        Ok(conn)
    }

    pub async fn get_conn(&self) -> Result<SunnyQuicConn, SError> {
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
            QuicConnection::close_reason(&x.conn).is_some_and(|x| {
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
impl Outbound for SunnyQuicClient {
    async fn handle(&mut self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        self.prepare_conn().await?;

        let conn = self.quic_conn.as_mut().unwrap().clone();

        let over_stream = self.config.over_stream;
        handle_request(req, conn, over_stream).await?;
        Ok(())
    }
}
