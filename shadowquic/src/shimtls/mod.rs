use crate::{
    Inbound, Outbound, ProxyRequest,
    config::{ShimTlsClientCfg, ShimTlsServerCfg},
    error::SError,
    proxy_transform::{
        ProxyJoin, ProxyTransform, StreamConnector,
        jls::{inbound::JlsServer, outbound::JlsClient},
        sq_shim::{self, SqShimClient, SqShimServer},
        tcp::{inbound::TcpServer, outbound::TcpClient},
    },
};
use std::sync::Arc;
use tracing::{Instrument, instrument, trace_span};

#[derive(Debug, Clone)]
pub struct ShimTlsClient {
    tcp: TcpClient,
    jls: JlsClient,
    shim: SqShimClient,
}
impl ShimTlsClient {
    pub fn new(cfg: ShimTlsClientCfg) -> Self {
        Self {
            tcp: TcpClient::new(cfg.tcp_cfg),
            jls: JlsClient::new(cfg.jls_cfg),
            shim: SqShimClient {},
        }
    }
}

pub struct ShimTlsServer {
    tcp: Arc<TcpServer>,
    jls: JlsServer,
    shim: SqShimServer,
    proxy_recv: tokio::sync::mpsc::Receiver<ProxyRequest>,
    proxy_send: tokio::sync::mpsc::Sender<ProxyRequest>,
}

impl ShimTlsServer {
    pub async fn new(cfg: ShimTlsServerCfg) -> Result<Self, SError> {
        let (proxy_send, proxy_recv) = tokio::sync::mpsc::channel::<ProxyRequest>(100);
        Ok(Self {
            tcp: TcpServer::new(cfg.tcp_cfg).await?.into(),
            jls: JlsServer::new(cfg.jls_cfg)?,
            shim: SqShimServer {},
            proxy_recv,
            proxy_send,
        })
    }
}

#[async_trait::async_trait]
impl Inbound for ShimTlsServer {
    async fn init(&self) -> Result<(), SError> {
        let jls = self.jls.clone();
        let shim = self.shim.clone();
        let listener = self.tcp.clone();
        let proxy_send = self.proxy_send.clone();
        let fut = async move {
            loop {
                let req = listener.accept().await?;
                let jls = jls.clone();
                let shim = shim.clone();
                let proxy_send = proxy_send.clone();
                tokio::spawn(async move {
                    let req = jls.transform(req).await?;
                    tracing::trace!("jls accepted");
                    let req = shim.transform(req).await?;
                    proxy_send
                        .send(req)
                        .await
                        .map_err(|_| SError::InboundUnavailable)?;
                    tracing::trace!("shimtls accepted");
                    Ok(()) as Result<(), SError>
                });
            }
            Ok(()) as Result<(), SError>
        };
        tokio::spawn(fut);
        Ok(())
    }
    #[instrument(skip(self), name = "shimtls")]
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        tracing::trace!("waiting for shimtls accept");
        let req = self
            .proxy_recv
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        tracing::trace!("shimtls accept proxy request");
        Ok(req)
    }
}

#[async_trait::async_trait]
impl Outbound for ShimTlsClient {
    async fn handle(&mut self, request: ProxyRequest) -> Result<(), SError> {
        let tcp = self.tcp.clone();
        let jls = self.jls.clone();
        tokio::spawn(
            async move {
                let stream = tcp.connect_stream().await?;
                tracing::trace!("tcp connected");
                let stream = jls.connect_stream(stream).await?;
                tracing::trace!("jls connected");
                let _ = SqShimClient {}.join(request, stream).await;
                tracing::trace!("tcp session ended");
                Ok::<(), SError>(())
            }
            .instrument(trace_span!("shimtls", addr = %self.tcp.addr)),
        );
        Ok(())
    }
}
