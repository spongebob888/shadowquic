use async_trait::async_trait;
use std::net::{SocketAddr, ToSocketAddrs};
use tokio::io::copy_bidirectional;
use tokio_tfo::TfoStream;
use tracing::{Instrument, error, info, trace_span};

use crate::{Outbound, ProxyRequest, TcpSession, config::TcpClientCfg, error::SError};

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

    async fn handle_tcp(&self, mut tcp_session: TcpSession) -> Result<(), SError> {
        // If we are acting as a client to a TFO server (like TcpServer we implemented),
        // we should connect to `self.addr`.
        // If `self.addr` is empty or we are in "direct" mode, we might connect to `tcp_session.dst`.
        // But TcpClientCfg has a mandatory `addr`. So we connect to `self.addr`.

        info!("TcpClient connecting to {} with TFO", self.addr);

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

        let mut stream = TfoStream::connect(addr).await?;

        // Disable Nagle's algorithm for lower latency
        stream.set_nodelay(true)?;

        // If we are tunneling, we might need to send the destination address to the server?
        // The current TcpServer implementation in `inbound.rs` assumes the destination is the bind address
        // and doesn't read any header. It just forwards.
        // So this implies a basic TCP tunnel matching the simple `TcpServer`.

        // Just bridge the connections.
        copy_bidirectional(&mut tcp_session.stream, &mut stream)
            .await
            .map_err(SError::Io)?;

        Ok(())
    }
}

#[async_trait]
impl Outbound for TcpClient {
    async fn handle(&mut self, req: ProxyRequest) -> Result<(), SError> {
        let span = trace_span!("tcp_client", addr = %self.addr);
        let client = self.clone();

        // Spawn to handle the connection concurrently, similar to SocksClient
        tokio::spawn(
            async move {
                let result = match req {
                    ProxyRequest::Tcp(session) => client.handle_tcp(session).await,
                    // UDP not supported for simple TCP tunnel yet, or simply ignore
                    ProxyRequest::Udp(_) => {
                        error!("TcpClient does not support UDP");
                        Err(SError::SocksError("UDP not supported by TcpClient".into()))
                    }
                };

                if let Err(e) = result {
                    error!("TcpClient error: {}", e);
                }
            }
            .instrument(span),
        );

        Ok(())
    }
}
