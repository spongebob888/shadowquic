use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use tokio::sync::{
    SetOnce,
    mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, error, trace_span};

use crate::{
    Inbound, ProxyRequest,
    config::SunnyQuicServerCfg,
    error::SError,
    quic::QuicConnection,
    squic::inbound::{SQServerConn, SunnyQuicUsers},
    sunnyquic::EndServer,
};

use crate::squic::{IDStore, SQConn};

use crate::quic::QuicServer;
pub struct SunnyQuicServer {
    pub endpoint: EndServer,
    users: Arc<ArcSwap<HashMap<crate::msgs::squic::SunnyCredential, String>>>,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

impl SunnyQuicServer {
    pub async fn new(cfg: SunnyQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);
        let endpoint: EndServer = QuicServer::new(&cfg)
            .await
            .expect("Failed to listening on udp");

        Ok(Self {
            endpoint,
            users: Arc::new(ArcSwap::new(Self::gen_users_hash(&cfg))),
            request_sender: send,
            request: recv,
        })
    }

    pub async fn update_config(&self, cfg: &SunnyQuicServerCfg) -> Result<(), SError> {
        QuicServer::update_config(&self.endpoint, cfg).await?;
        self.users.store(Self::gen_users_hash(cfg));
        Ok(())
    }

    async fn handle_incoming<C: QuicConnection>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
        user_hash: SunnyQuicUsers,
    ) -> Result<(), SError> {
        let sq_conn = SQServerConn {
            inner: SQConn {
                conn: incom,
                authed: Arc::new(SetOnce::new()),
                send_id_store: Default::default(),
                recv_id_store: IDStore {
                    id_counter: Default::default(),
                    inner: Default::default(),
                },
            },
            users: user_hash,
        };
        let span = trace_span!("quic", id = sq_conn.inner.peer_id());
        sq_conn
            .handle_connection(req_sender)
            .instrument(span)
            .await?;

        Ok(())
    }
    fn gen_users_hash(cfg: &SunnyQuicServerCfg) -> SunnyQuicUsers {
        let users = HashMap::from_iter(cfg.users.iter().map(|x| {
            let hash = crate::sunnyquic::gen_sunny_user_hash(&x.username, &x.password);
            (hash, x.username.clone())
        }));
        Arc::new(users)
    }
}

#[async_trait]
impl Inbound for SunnyQuicServer {
    async fn accept(&mut self) -> Result<crate::ProxyRequest, SError> {
        let req = self
            .request
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        return Ok(req);
    }
    /// Init background job for accepting connection
    async fn init(&self) -> Result<(), SError> {
        let request_sender = self.request_sender.clone();
        let endpoint = self.endpoint.clone();
        let users = self.users.clone();
        let fut = async move {
            loop {
                match QuicServer::accept(&endpoint).await {
                    Ok(conn) => {
                        let request_sender = request_sender.clone();
                        let user_hash = users.load_full();
                        tokio::spawn(async move {
                            Self::handle_incoming(conn, request_sender, user_hash)
                                .await
                                .map_err(|x| error!("{}", x))
                        });
                    }
                    Err(e) => {
                        error!("Error accepting quic connection: {}", e);
                    }
                }
            }
        };
        tokio::spawn(fut);
        Ok(())
    }
}
