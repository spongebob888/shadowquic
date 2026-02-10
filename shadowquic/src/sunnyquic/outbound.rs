use async_trait::async_trait;
use std::{
    net::{ToSocketAddrs, UdpSocket},
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
    squic::{auth_sunny, outbound::handle_request},
    sunnyquic::gen_sunny_user_hash,
};

use crate::squic::{IDStore, SQConn, handle_udp_packet_recv};

pub type SunnyQuicConn = SQConn<<EndClient as QuicClient>::C>;

pub struct SunnyQuicClient {
    pub quic_conn: Option<SunnyQuicConn>,
    pub config: SunnyQuicClientCfg,
    pub quic_end: OnceCell<EndClient>,
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
    pub fn new_with_socket(cfg: SunnyQuicClientCfg, socket: UdpSocket) -> Result<Self, SError> {
        Ok(Self {
            quic_end: OnceCell::from(EndClient::new_with_socket(&cfg, socket)?),
            quic_conn: None,
            config: cfg,
        })
    }

    pub async fn get_conn(&self) -> Result<SunnyQuicConn, SError> {
        let addr = self
            .config
            .addr
            .to_socket_addrs()
            .unwrap_or_else(|_| panic!("resolve quic addr faile: {}", self.config.addr))
            .next()
            .unwrap_or_else(|| panic!("resolve quic addr faile: {}", self.config.addr));
        let end = self
            .quic_end
            .get_or_init(|| async {
                match self.init_endpoint(true).await {
                    Ok(ep) => ep,
                    Err(_) => self
                        .init_endpoint(false)
                        .await
                        .expect("error during initialize quic endpoint"),
                }
            })
            .await;
        let conn = QuicClient::connect(end, addr, &self.config.server_name).await?;

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
