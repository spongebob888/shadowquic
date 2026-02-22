use async_trait::async_trait;
use serde::Deserialize;
use std::net::{SocketAddr, ToSocketAddrs};
use tokio::{io::copy_bidirectional, net::TcpStream};
use tokio_tfo::TfoStream;
use tracing::{Instrument, error, info, trace_span};

use crate::{AnyTcp, Outbound, ProxyRequest, TcpSession, error::SError};

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TcpClientCfg {
    pub addr: String,
}

#[derive(Debug, Clone)]
pub struct TcpClient {
    // If the config has an address, we might use it as a gateway or bind address,
    // but for a transparent TCP proxy, we usually connect to the destination in the request.
    // However, if the user provides `addr`, standard behavior for "client" in this codebase
    // (like SocksClient) is to connect to that address (the proxy server).
    // But since this is "TcpClient" (likely "TFO Client" connecting to "TFO Server"),
    // it likely connects to a specific upstream TFO server.
    pub addr: String,
}

impl TcpClient {
    pub fn new(cfg: TcpClientCfg) -> Self {
        Self { addr: cfg.addr }
    }

    pub(crate) async fn connect_stream(&self) -> Result<AnyTcp, SError> {
        // If we are acting as a client to a TFO server (like TcpServer we implemented),
        // we should connect to `self.addr`.
        // If `self.addr` is empty or we are in "direct" mode, we might connect to `tcp_session.dst`.
        // But TcpClientCfg has a mandatory `addr`. So we connect to `self.addr`.

        info!("tcp connecting to {} with TFO", self.addr);

        // Resolve address if needed, but TfoStream::connect takes SocketAddr.
        // TfoStream::connect uses TFO if available.
        let addr = self
            .addr
            .to_socket_addrs()
            .map_err(|e| SError::Io(e))?
            .next()
            .ok_or(SError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid address",
            )))?;

        let stream = TfoStream::connect(addr).await?;

        // Disable Nagle's algorithm for lower latency
        stream.set_nodelay(true)?;

        Ok(Box::new(stream))
    }
}
