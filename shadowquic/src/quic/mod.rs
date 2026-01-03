use std::net::{SocketAddr, UdpSocket};

use crate::{
    config::{ShadowQuicClientCfg, ShadowQuicServerCfg},
    error::SResult,
};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(feature = "quinn")]
mod quinn_wrapper;
#[cfg(feature = "quinn")]
pub use quinn_wrapper::{Connection, EndClient, EndServer, QuicErrorRepr};

#[cfg(feature = "gm-quic")]
mod gm_quic_wrapper;
#[cfg(feature = "gm-quic")]
pub use gm_quic_wrapper::{Connection, EndClient, EndServer, QuicErrorRepr};

#[async_trait]
pub trait QuicClient: Send + Sync {
    type C: QuicConnection;
    async fn new(cfg: &ShadowQuicClientCfg, ipv6: bool) -> SResult<Self>
    where
        Self: Sized;
    fn new_with_socket(cfg: &ShadowQuicClientCfg, socket: UdpSocket) -> SResult<Self>
    where
        Self: Sized;
    async fn connect(&self, addr: SocketAddr, server_name: &str) -> Result<Self::C, QuicErrorRepr>;
}
#[async_trait]
pub trait QuicServer: Send + Sync {
    type C: QuicConnection;
    async fn new(cfg: &ShadowQuicServerCfg) -> SResult<Self>
    where
        Self: Sized;
    async fn accept(&self) -> Result<Self::C, QuicErrorRepr>;
}

#[async_trait]
pub trait QuicConnection: Send + Sync + Clone + 'static {
    type SendStream: AsyncWrite + Unpin + Send + Sync + 'static;
    type RecvStream: AsyncRead + Unpin + Send + Sync + 'static;
    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr>;
    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr>;
    async fn open_uni(&self) -> Result<(Self::SendStream, u64), QuicErrorRepr>;
    async fn accept_uni(&self) -> Result<(Self::RecvStream, u64), QuicErrorRepr>;
    async fn read_datagram(&self) -> Result<Bytes, QuicErrorRepr>;
    async fn send_datagram(&self, bytes: Bytes) -> Result<(), QuicErrorRepr>;
    fn close_reason(&self) -> Option<QuicErrorRepr>;
    fn remote_address(&self) -> SocketAddr;
    fn peer_id(&self) -> u64;
}
