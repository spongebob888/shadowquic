use std::io::Cursor;
use std::sync::Arc;
use std::{error::Error, net::SocketAddr};

use crate::shadowquic::inbound::ShadowQuicServer;
use direct::DirectOut;
use error::SError;
use msgs::socks5::{
    self, AddrOrDomain, CmdReq, SDecode, SOCKS5_ADDR_TYPE_DOMAIN_NAME, SOCKS5_ADDR_TYPE_IPV4,
    SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT, SOCKS5_CMD_UDP_ASSOCIATE,
    SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION, SocksAddr,
};
use socks::inbound::SocksServer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, trace};

pub mod direct;
pub mod error;
pub mod msgs;
pub mod shadowquic;
pub mod socks;

pub enum ProxyRequest<T = AnyTcp, U = AnyUdp> {
    Tcp(TcpSession<T>),
    Udp(UdpSession<U>),
}
/// Udp socket only use immutable reference to self
/// So it can be safely wrapped by Arc and cloned to work in duplex way.
#[async_trait]
pub trait UdpSocketTrait: Send + Sync + Unpin {
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, usize, SocksAddr), SError>; // headsize, totalsize, proxy addr
    async fn send_to(&self, buf: &[u8], addr: SocksAddr) -> Result<usize, SError>; // addr is proxy addr
}
pub struct TcpSession<IO = AnyTcp> {
    stream: IO,
    dst: SocksAddr,
}

pub struct UdpSession<IO = AnyUdp> {
    socket: IO,
    stream: Option<AnyTcp>,
    dst: SocksAddr,
}

pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdp = Box<dyn UdpSocketTrait>;
pub trait TcpTrait: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl TcpTrait for TcpStream {}

#[async_trait]
pub trait Inbound<T = AnyTcp, U = AnyUdp>: Send + Sync + Unpin {
    async fn accept(&mut self) -> Result<ProxyRequest<T, U>, SError>;
    async fn init(&self) -> Result<(), SError> {
        Ok(())
    }
}

#[async_trait]
pub trait Outbound<T = AnyTcp, U = AnyUdp>: Send + Sync + Unpin {
    async fn handle(&mut self, req: ProxyRequest<T, U>) -> Result<(), SError>;
}

pub struct Manager {
    pub inbound: Box<dyn Inbound>,
    pub outbound: Box<dyn Outbound>,
}

impl Manager {
    pub async fn run(mut self) -> Result<(), SError> {
        self.inbound.init().await?;
        let mut inbound = self.inbound;
        let mut outbound = self.outbound;
        loop {
            let req = inbound.accept().await?;
            outbound.handle(req).await.unwrap();
        }

        Ok(())
    }
}
