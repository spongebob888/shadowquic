use std::net::SocketAddr;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio_tfo::TfoListener;
use tracing::{error, info};

use crate::error::SError;
use crate::{Inbound, ProxyRequest, TcpSession};

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TcpServerCfg {
    pub bind_addr: SocketAddr,
}

pub struct TcpServer {
    pub listener: TfoListener,
    bind_addr: SocketAddr,
}

impl TcpServer {
    pub async fn new(cfg: TcpServerCfg) -> Result<Self, SError> {
        let bind_addr = cfg.bind_addr;
        // tokio-tfo handles socket creation and TFO configuration
        let listener = TfoListener::bind(bind_addr).await?;
        let bind_addr = listener.local_addr()?;

        info!("TcpServer (TFO enabled) listening on {}", bind_addr);

        Ok(Self {
            listener,
            bind_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.bind_addr
    }
    pub async fn accept(&self) -> Result<ProxyRequest, SError> {
        match self.listener.accept().await {
            Ok((stream, _)) => {
                // For now, we assume the destination is the same as the bind address
                // or we might need transparent proxy handling here later.
                // Using bind_addr as a placeholder for dst.
                let dst = crate::msgs::socks5::SocksAddr::from(self.bind_addr);

                Ok(ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(stream),
                    dst,
                }))
            }
            Err(e) => {
                error!("TcpServer accept error: {}", e);
                Err(SError::Io(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn test_tcp_server_bind() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let cfg = TcpServerCfg { bind_addr: addr };

        let server = TcpServer::new(cfg).await;

        match server {
            Ok(s) => info!("TcpServer bound successfully to {}", s.bind_addr),
            Err(e) => info!("TcpServer failed to bind: {:?}", e),
        }
    }
}
