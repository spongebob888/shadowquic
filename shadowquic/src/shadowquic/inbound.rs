use async_trait::async_trait;
use std::sync::Arc;

use tokio::sync::{
    RwLock, SetOnce,
    mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, error, info_span};

use crate::{
    Inbound, ProxyRequest,
    config::{AuthUser, ShadowQuicServerCfg},
    error::SError,
    msgs::squic::SQExtError,
    quic::{AuthedConn, QuicConnection},
    squic::inbound::{SQServerConn, ServerConnManger, UserManager},
};

use crate::squic::{IDStore, SQConn};

use super::quinn_wrapper::EndServer;
use crate::quic::QuicServer;
pub struct ShadowQuicServer {
    pub endpoint: EndServer,
    user_manager: Arc<ShadowQuicUserManager>,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

struct ShadowQuicUserManager {
    endpoint: EndServer,
    config: RwLock<ShadowQuicServerCfg>,
    conn_manager: Arc<ServerConnManger<<EndServer as QuicServer>::C>>,
}

#[async_trait]
impl UserManager for ShadowQuicUserManager {
    async fn add_user(&self, user: AuthUser) -> Result<(), SQExtError> {
        let mut config = self.config.write().await;
        let old_config = config.clone();
        if let Some(existing_user) = config
            .users
            .iter_mut()
            .find(|existing_user| existing_user.username == user.username)
        {
            existing_user.password = user.password;
        } else {
            config.users.push(user);
        }

        if let Err(error) = QuicServer::update_config(&self.endpoint, &config).await {
            *config = old_config;
            tracing::error!("failed to add user: {}", error);
            return Err(SQExtError::Other(error.to_string()));
        }
        Ok(())
    }

    async fn remove_user(&self, username: &str) -> Result<(), SQExtError> {
        let mut config = self.config.write().await;
        let old_config = config.clone();
        let old_len = config.users.len();
        config.users.retain(|user| user.username != username);
        if config.users.len() == old_len {
            return Err(SQExtError::NotFound);
        }

        if let Err(error) = QuicServer::update_config(&self.endpoint, &config).await {
            *config = old_config;
            tracing::error!("failed to remove user: {}", error);
            return Err(SQExtError::Other(error.to_string()));
        }
        Ok(())
    }

    async fn list_users(&self) -> Result<Vec<String>, SQExtError> {
        let config = self.config.read().await;
        Ok(config
            .users
            .iter()
            .map(|user| user.username.clone())
            .collect())
    }
}

impl ShadowQuicServer {
    pub async fn new(cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);

        let endpoint: EndServer = QuicServer::new(&cfg)
            .await
            .expect("Failed to listening on udp");
        let user_manager = Arc::new(ShadowQuicUserManager {
            endpoint: endpoint.clone(),
            config: RwLock::new(cfg),
            conn_manager: Arc::new(ServerConnManger::new()),
        });

        Ok(Self {
            endpoint,
            user_manager,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming<C: QuicConnection + AuthedConn>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
        user_manager: Arc<dyn UserManager>,
        conn_manager: Arc<ServerConnManger<C>>,
    ) -> Result<(), SError> {
        let user = incom
            .authed_user()
            .ok_or(SError::SunnyAuthError("User not authenticated".into()))?;
        let sq_conn = Arc::new(SQServerConn {
            inner: SQConn::new(incom, Ok(user.clone())),
            users: Arc::new(Default::default()),
            user_manager: Some(user_manager),
            conn_manager: Some(conn_manager.clone()),
        });
        let span = info_span!("quic", id = sq_conn.inner.peer_id(), user = %user);
        conn_manager.on_new_conn(sq_conn.clone()).await;
        let stats= conn_manager.get_user_stats(&user).await.unwrap();
        let _ = sq_conn.inner.stats.set(stats);
        sq_conn
            .handle_connection(req_sender)
            .instrument(span)
            .await?;

        Ok(())
    }
}

#[async_trait]
impl Inbound for ShadowQuicServer {
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
        let user_manager = self.user_manager.clone();
        let conn_manager = self.user_manager.conn_manager.clone();
        let fut = async move {
            loop {
                match QuicServer::accept(&endpoint).await {
                    Ok(conn) => {
                        let request_sender = request_sender.clone();
                        let user_manager = user_manager.clone();
                        let conn_manager = conn_manager.clone();
                        tokio::spawn(async move {
                            Self::handle_incoming(conn, request_sender, user_manager, conn_manager)
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
