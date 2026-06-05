use async_trait::async_trait;
use std::sync::Arc;

use tokio::sync::{
    SetOnce,
    mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, error, trace_span};

use crate::{
    Inbound, ProxyRequest, config::ShadowQuicServerCfg, error::SError, quic::AuthedConn,
    quic::QuicConnection, squic::inbound::SQServerConn,
};

use crate::squic::{IDStore, SQConn};

use super::quinn_wrapper::EndServer;
use crate::quic::QuicServer;
pub struct ShadowQuicServer {
    pub endpoint: EndServer,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

impl ShadowQuicServer {
    pub async fn new(cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);

        let endpoint: EndServer = QuicServer::new(&cfg)
            .await
            .expect("Failed to listening on udp");

        Ok(Self {
            endpoint,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming<C: QuicConnection + AuthedConn>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
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
        };
        let span = trace_span!("quic", id = sq_conn.inner.peer_id(), user = %user);
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
        let fut = async move {
            loop {
                match QuicServer::accept(&endpoint).await {
                    Ok(conn) => {
                        let request_sender = request_sender.clone();
                        tokio::spawn(async move {
                            Self::handle_incoming(conn, request_sender)
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
