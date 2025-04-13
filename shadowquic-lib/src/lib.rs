use std::io::Cursor;
use std::sync::Arc;
use std::{error::Error, net::SocketAddr};

use direct::DirectOut;
use error::SError;
use msgs::socks5::{
    self, AddrOrDomain, CmdReq, SDecode, SOCKS5_ADDR_TYPE_DOMAIN_NAME, SOCKS5_ADDR_TYPE_IPV4,
    SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT, SOCKS5_CMD_UDP_ASSOCIATE,
    SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION, SocksAddr,
};
use crate::shadowquic::inbound::ShadowQuicServer;
use socks::inbound::SocksServer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use anyhow::Result;
use tracing::{info, trace};
use async_trait::async_trait;

pub mod error;
pub mod msgs;
pub mod shadowquic;
pub mod socks;
pub mod direct;

pub enum ProxyRequest<T = AnyTcp,U = AnyUdp> {
    Tcp(TcpSession<T>),
    Udp(UdpSession<U>),
}
#[async_trait]
pub trait UdpSocketTrait: Send + Sync + Unpin {
    async fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(usize, usize, SocketAddr, SocksAddr), SError>;
    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize, SError>;
}
pub struct TcpSession<IO=AnyTcp> {
    stream: IO,
    dst: SocksAddr,
}

pub struct UdpSession<IO=AnyUdp> {
    socket: IO,
    dst: SocksAddr,
}


pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdp = Box<dyn UdpSocketTrait>;
pub trait TcpTrait: AsyncRead + AsyncWrite + Unpin + Send + Sync{}
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
    pub outbound: Box<dyn Outbound> ,
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
