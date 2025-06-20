use std::net::{SocketAddr, UdpSocket};

use crate::{
    config::{ShadowQuicClientCfg, ShadowQuicServerCfg},
    error::{SError, SResult},
};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(feature = "quinn")]
mod quinn_wrapper;
#[cfg(feature = "quinn")]
pub use quinn_wrapper::{Connection, EndClient, EndServer};

#[async_trait]
pub trait QuicClient: Send + Sync {
    type C: QuicConnection;
    fn new(cfg: &ShadowQuicClientCfg, ipv6: bool) -> SResult<Self>
    where
        Self: Sized;
    fn new_with_socket(cfg: &ShadowQuicClientCfg, socket: UdpSocket) -> SResult<Self>
    where
        Self: Sized;
    async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
        zero_rtt: bool,
    ) -> SResult<Self::C>;
}
#[async_trait]
pub trait QuicServer: Send + Sync {
    type C: QuicConnection;
    fn new(cfg: &ShadowQuicServerCfg) -> SResult<Self>
    where
        Self: Sized;
    async fn accept(&self, zero_rtt: bool) -> SResult<Self::C>;
}

#[async_trait]
pub trait QuicConnection: Send + Sync + Clone + 'static {
    type SendStream: AsyncWrite + Unpin + Send + Sync + 'static;
    type RecvStream: AsyncRead + Unpin + Send + Sync + 'static;
    async fn open_bi(&self) -> SResult<(Self::SendStream, Self::RecvStream, u64)>;
    async fn accept_bi(&self) -> SResult<(Self::SendStream, Self::RecvStream, u64)>;
    async fn open_uni(&self) -> SResult<(Self::SendStream, u64)>;
    async fn accept_uni(&self) -> SResult<(Self::RecvStream, u64)>;
    async fn read_datagram(&self) -> SResult<Bytes>;
    async fn send_datagram(&self, bytes: Bytes) -> SResult<()>;
    fn close_reason(&self) -> Option<SError>;
    fn remote_address(&self) -> SocketAddr;
    fn peer_id(&self) -> u64;
}
