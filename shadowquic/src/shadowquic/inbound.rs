use async_trait::async_trait;
use std::sync::Arc;

use tokio::sync::{
    SetOnce,
    mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, error, trace_span};

use crate::{
    Inbound, ProxyRequest, config::ShadowQuicServerCfg, error::SError, quic::QuicConnection,
    squic::inbound::SQServerConn,
};

use crate::squic::{IDStore, SQConn};

use super::quinn_wrapper::EndServer;
use crate::quic::QuicServer;
pub struct ShadowQuicServer {
    pub config: ShadowQuicServerCfg,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

impl ShadowQuicServer {
    pub fn new(cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);

        Ok(Self {
            config: cfg,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming<C: QuicConnection>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let sq_conn = SQServerConn {
            inner: SQConn {
                conn: incom,
                authed: Arc::new(SetOnce::new_with(Some(true))),
                send_id_store: Default::default(),
                recv_id_store: IDStore {
                    id_counter: Default::default(),
                    inner: Default::default(),
                },
            },
            users: Arc::new(Default::default()),
        };
        let span = trace_span!("quic", id = sq_conn.inner.peer_id());
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
        let config = self.config.clone();
        let fut = async move {
            let endpoint: EndServer = QuicServer::new(&config)
                .await
                .expect("Failed to listening on udp");
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
