use std::net::{SocketAddr, UdpSocket};

use crate::{
    config::{ShadowQuicClientCfg, ShadowQuicServerCfg},
    error::SResult,
};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use thiserror::Error;

// 4 times larger than quinn default value
// Better decrease the size for portable device
pub const MAX_WINDOW_BASE: u64 = 4 * 12_500_000 * 100 / 1000; // 100ms RTT
pub const MAX_STREAM_WINDOW: u64 = MAX_WINDOW_BASE;
pub const MAX_SEND_WINDOW: u64 = MAX_WINDOW_BASE * 8;
pub const MAX_DATAGRAM_WINDOW: u64 = MAX_WINDOW_BASE * 2;

// #[cfg(feature = "gm-quic")]
// mod gm_quic_wrapper;
// #[cfg(feature = "gm-quic")]
// pub use gm_quic_wrapper::{Connection, EndClient, EndServer, QuicErrorRepr};

#[async_trait]
pub trait QuicClient: Send + Sync {
    type C: QuicConnection;
    type SC: Clone + Send + Sync + 'static;
    async fn new(cfg: &Self::SC, ipv6: bool) -> SResult<Self>
    where
        Self: Sized;
    fn new_with_socket(cfg: &Self::SC, socket: UdpSocket) -> SResult<Self>
    where
        Self: Sized;
    async fn connect(&self, addr: SocketAddr, server_name: &str) -> Result<Self::C, QuicErrorRepr>;
}
#[async_trait]
pub trait QuicServer: Send + Sync {
    type C: QuicConnection;
    type SC: Clone + Send + Sync + 'static;
    async fn new(cfg: &Self::SC) -> SResult<Self>
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

#[derive(Error, Debug)]
pub enum QuicErrorRepr {
    #[error("QUIC Connect Error:{0}")]
    QuicConnect(String),
    #[error("QUIC Connection Error:{0}")]
    QuicConnection(String),
    #[error("QUIC Write Error:{0}")]
    QuicWrite(String),
    #[error("QUIC ReadExact Error:{0}")]
    QuicReadExactError(String),
    #[error("QUIC SendDatagramError:{0}")]
    QuicSendDatagramError(String),
    #[error("JLS Authentication failed")]
    JlsAuthFailed,
}