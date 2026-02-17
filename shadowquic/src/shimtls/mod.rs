use tracing::{Instrument, instrument, trace_span};

use crate::{
    Inbound, Outbound, ProxyRequest,
    config::{ShimTlsClientCfg, ShimTlsServerCfg},
    error::SError,
    proxy_transform::{
        ProxyTransform,
        sq_shim::{self, SqShimClient},
        tls::{inbound::JlsServer, outbound::JlsClient},
    },
    tcp::{inbound::TcpServer, outbound::TcpClient},
};

#[derive(Debug, Clone)]
pub struct ShimTlsClient {
    tcp: TcpClient,
    jls: JlsClient,
    shim: sq_shim::SqShimClient,
}
impl ShimTlsClient {
    pub fn new(cfg: ShimTlsClientCfg) -> Self {
        Self {
            tcp: TcpClient::new(cfg.tcp_cfg),
            jls: JlsClient::new(cfg.jls_cfg),
            shim: sq_shim::SqShimClient {},
        }
    }
}

pub struct ShimTlsServer {
    tcp: TcpServer,
    jls: JlsServer,
    shim: sq_shim::SqShimServer,
}

impl ShimTlsServer {
    pub async fn new(cfg: ShimTlsServerCfg) -> Result<Self, SError> {
        Ok(Self {
            tcp: TcpServer::new(cfg.tcp_cfg).await?,
            jls: JlsServer::new(cfg.jls_cfg)?,
            shim: sq_shim::SqShimServer {},
        })
    }
}

#[async_trait::async_trait]
impl Inbound for ShimTlsServer {
    #[instrument(skip(self), name = "shimtls")]
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let req = self.tcp.accept().await?;
        tracing::trace!("tcp accepted");
        let req = self.jls.transform(req).await?;
        tracing::trace!("jls accepted");
        let req = self.shim.transform(req).await?;
        tracing::trace!("shimtls accepted");
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
                let _ = SqShimClient::join(request, stream).await;
                tracing::trace!("tcp session ended");
                Ok::<(), SError>(())
            }
            .instrument(trace_span!("shimtls", addr = %self.tcp.addr)),
        );
        Ok(())
    }
}
