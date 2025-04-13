use std::io::Cursor;
use std::{error::Error, net::SocketAddr};

use direct::DirectOut;
use error::SError;
use msgs::socks5::{
    self, AddrOrDomain, CmdReq, SDecode, SOCKS5_ADDR_TYPE_DOMAIN_NAME, SOCKS5_ADDR_TYPE_IPV4,
    SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT, SOCKS5_CMD_UDP_ASSOCIATE,
    SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION, SocksAddr,
};
use shadowquic::ShadowQuicServer;
use socks::SocksServer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use anyhow::Result;
use tracing::{info, trace};

pub mod error;
pub mod msgs;
pub mod shadowquic;
pub mod socks;
pub mod direct;

pub enum ProxyRequest<T: AsyncRead + AsyncWrite + Unpin, U: UdpSocketTrait> {
    Tcp(TcpSession<T>),
    Udp(UdpSession<U>),
}
pub trait UdpSocketTrait {
    async fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(usize, usize, SocketAddr, SocksAddr), SError>;
    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize, SError>;
}
pub struct TcpSession<IO: AsyncRead + AsyncWrite + Unpin> {
    stream: IO,
    dst: SocksAddr,
}

pub struct UdpSession<IO: UdpSocketTrait> {
    socket: IO,
    dst: SocksAddr,
}


pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdp = Box<dyn UdpSocketTrait>;
trait TcpTrait: AsyncRead + AsyncWrite + Unpin {}
pub trait Inbound<T: AsyncRead + AsyncWrite + Unpin, U: UdpSocketTrait> {
    async fn accept(&mut self) -> Result<ProxyRequest<T, U>, SError>;
}

pub trait Outbound<T: AsyncRead + AsyncWrite + Unpin, U: UdpSocketTrait> {
    async fn handle(&mut self, req: ProxyRequest<T, U>) -> Result<(), SError>;
}

// pub struct Manager {
//     pub inbound: Box<dyn Inbound<AnyTcp, AnyUdp>>>,
//     pub outbound: Box<dyn Outbound> ,
// }
