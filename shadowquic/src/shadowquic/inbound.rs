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
    msgs::squic::{SQExtError, UserStats},
    observe::Observer,
    quic::{AuthedConn, QuicConnection},
    squic::inbound::{SQServerConn, UserManager},
    utils::user_store,
};

use crate::squic::{IDStore, SQConn};

use super::quinn_wrapper::EndServer;
use crate::quic::QuicServer;
pub struct ShadowQuicServer {
    pub endpoint: EndServer,
    user_manager: Arc<ShadowQuicUserManager>,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
    observer: Arc<Observer>,
}

struct ShadowQuicUserManager {
    endpoint: EndServer,
    config: RwLock<ShadowQuicServerCfg>,
    observer: Arc<Observer>,
}

impl ShadowQuicUserManager {
    /// Persist current users and traffic stats to the configured store file.
    async fn persist(&self) {
        let config = self.config.read().await;
        let Some(path) = config.user_store.clone() else {
            return;
        };
        let users = config.users.clone();
        drop(config);
        let store = user_store::build_store(&users, &self.observer).await;
        if let Err(error) = user_store::save_store(&path, &store) {
            error!("failed to persist user store: {}", error);
        }
    }
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
        drop(config);
        self.persist().await;
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
        drop(config);
        self.observer.remove_user(username).await;
        self.persist().await;
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

    async fn get_user_stats(&self, username: &str) -> Result<UserStats, SQExtError> {
        let config = self.config.read().await;
        if !config.users.iter().any(|user| user.username == username) {
            return Err(SQExtError::NotFound);
        }
        drop(config);
        Ok(self.observer.get_user_stats(username).await)
    }

    async fn get_all_stats(&self) -> Result<Vec<UserStats>, SQExtError> {
        let config = self.config.read().await;
        let usernames = config
            .users
            .iter()
            .map(|user| user.username.clone())
            .collect::<Vec<_>>();
        drop(config);
        Ok(self.observer.get_all_stats(&usernames).await)
    }

    async fn kill_user_conns(&self, username: &str) -> Result<(), SQExtError> {
        let config = self.config.read().await;
        if !config.users.iter().any(|user| user.username == username) {
            return Err(SQExtError::NotFound);
        }
        drop(config);
        self.observer.close_conn(username).await;
        Ok(())
    }

    async fn clear_user_stats(&self, username: &str) -> Result<(), SQExtError> {
        let config = self.config.read().await;
        if !config.users.iter().any(|user| user.username == username) {
            return Err(SQExtError::NotFound);
        }
        drop(config);
        self.observer.clear_user_stats(username).await;
        self.persist().await;
        Ok(())
    }

    async fn clear_all_stats(&self) -> Result<(), SQExtError> {
        let config = self.config.read().await;
        let usernames = config
            .users
            .iter()
            .map(|user| user.username.clone())
            .collect::<Vec<_>>();
        drop(config);
        self.observer.clear_all_stats(&usernames).await;
        self.persist().await;
        Ok(())
    }
}

impl ShadowQuicServer {
    pub async fn new(mut cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);

        let store = match &cfg.user_store {
            Some(path) => user_store::load_store(path)?,
            None => None,
        };
        if let Some(store) = &store {
            user_store::merge_users(&mut cfg.users, store);
        }

        let endpoint: EndServer = QuicServer::new(&cfg)
            .await
            .expect("Failed to listening on udp");
        let observer = Arc::new(Observer::new());
        if let Some(store) = &store {
            observer
                .restore_stats(&user_store::stored_stats(store))
                .await;
        }
        let user_manager = Arc::new(ShadowQuicUserManager {
            endpoint: endpoint.clone(),
            config: RwLock::new(cfg),
            observer: observer.clone(),
        });

        Ok(Self {
            endpoint,
            user_manager,
            request_sender: send,
            request: recv,
            observer,
        })
    }

    /// Periodically flush users and traffic stats to the user store,
    /// so stats survive unexpected crashes.
    async fn spawn_store_flush(&self) {
        let config = self.user_manager.config.read().await;
        let interval_secs = config.store_flush_interval;
        let store_enabled = config.user_store.is_some();
        drop(config);
        if !store_enabled || interval_secs == 0 {
            return;
        }
        let user_manager = self.user_manager.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(interval_secs);
            loop {
                tokio::time::sleep(interval).await;
                user_manager.persist().await;
            }
        });
    }

    async fn handle_incoming<C: QuicConnection + AuthedConn>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
        user_manager: Arc<dyn UserManager>,
    ) -> Result<(), SError> {
        let user = incom
            .authed_user()
            .ok_or(SError::SunnyAuthError("User not authenticated".into()))?;
        let sq_conn = SQServerConn {
            inner: SQConn {
                conn: incom,
                authed: Arc::new(SetOnce::new_with(Some(Ok(user.clone())))),
                send_id_store: Default::default(),
                recv_id_store: IDStore {
                    id_counter: Default::default(),
                    inner: Default::default(),
                },
            },
            users: Arc::new(Default::default()),
            user_manager: Some(user_manager),
        };
        let span = info_span!("quic", id = sq_conn.inner.peer_id(), user = %user);
        let sq_conn = Arc::new(sq_conn);
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
        Ok(self.observer.wrap_request(req).await)
    }
    /// Init background job for accepting connection
    async fn init(&self) -> Result<(), SError> {
        let request_sender = self.request_sender.clone();
        let endpoint = self.endpoint.clone();
        let user_manager = self.user_manager.clone();
        let fut = async move {
            loop {
                match QuicServer::accept(&endpoint).await {
                    Ok(conn) => {
                        let request_sender = request_sender.clone();
                        let user_manager = user_manager.clone();
                        tokio::spawn(async move {
                            Self::handle_incoming(conn, request_sender, user_manager)
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
        self.spawn_store_flush().await;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), SError> {
        self.user_manager.persist().await;
        Ok(())
    }
}
