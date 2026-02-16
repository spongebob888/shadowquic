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
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let req = self.tcp.accept().await?;
        tracing::trace!("unwraping jls");
        let req = self.jls.transform(req).await?;
        tracing::trace!("unwraping shim");
        let req = self.shim.transform(req).await?;
        Ok(req)
    }
}

#[async_trait::async_trait]
impl Outbound for ShimTlsClient {
    async fn handle(&mut self, request: ProxyRequest) -> Result<(), SError> {
        let stream = self.tcp.connect_stream().await?;
        let stream = self.jls.connect_stream(stream).await?;
        tokio::spawn(async move {
            let _ = SqShimClient::join(request, stream).await;
        });
        Ok(())
    }
}
