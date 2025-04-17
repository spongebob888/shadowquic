use std::{
    error::Error as StdError,
    fmt,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    vec,
};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[rustfmt::skip]
pub mod consts {
    pub const SOCKS5_VERSION:                          u8 = 0x05;

    pub const SOCKS5_AUTH_METHOD_NONE:                 u8 = 0x00;
    pub const SOCKS5_AUTH_METHOD_GSSAPI:               u8 = 0x01;
    pub const SOCKS5_AUTH_METHOD_PASSWORD:             u8 = 0x02;
    pub const SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE:       u8 = 0xff;

    pub const SOCKS5_CMD_TCP_CONNECT:                  u8 = 0x01;
    pub const SOCKS5_CMD_TCP_BIND:                     u8 = 0x02;
    pub const SOCKS5_CMD_UDP_ASSOCIATE:                u8 = 0x03;

    pub const SOCKS5_ADDR_TYPE_IPV4:                   u8 = 0x01;
    pub const SOCKS5_ADDR_TYPE_DOMAIN_NAME:            u8 = 0x03;
    pub const SOCKS5_ADDR_TYPE_IPV6:                   u8 = 0x04;

    pub const SOCKS5_REPLY_SUCCEEDED:                  u8 = 0x00;
    pub const SOCKS5_REPLY_GENERAL_FAILURE:            u8 = 0x01;
    pub const SOCKS5_REPLY_CONNECTION_NOT_ALLOWED:     u8 = 0x02;
    pub const SOCKS5_REPLY_NETWORK_UNREACHABLE:        u8 = 0x03;
    pub const SOCKS5_REPLY_HOST_UNREACHABLE:           u8 = 0x04;
    pub const SOCKS5_REPLY_CONNECTION_REFUSED:         u8 = 0x05;
    pub const SOCKS5_REPLY_TTL_EXPIRED:                u8 = 0x06;
    pub const SOCKS5_REPLY_COMMAND_NOT_SUPPORTED:      u8 = 0x07;
    pub const SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;
}

pub use consts::*;

use crate::error::SError;

pub trait SEncode {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError>;
}
pub trait SDecode
where
    Self: Sized,
{
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError>;
}

#[derive(Clone, Debug)]
pub struct AuthReq {
    pub version: u8,
    pub methods: VarVec,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct VarVec {
    pub len: u8,
    pub contents: Vec<u8>,
}
impl SEncode for VarVec {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = vec![self.len];
        s.write_all(&buf).await?;
        s.write_all((&self.contents[0..self.len as usize])).await?;
        Ok(())
    }
}
impl SDecode for VarVec {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8; 1];
        s.read_exact(&mut buf).await?;
        let mut buf2 = vec![0u8; buf[0] as usize];
        s.read_exact(&mut buf2).await?;
        Ok(Self {
            len: buf[0],
            contents: buf2,
        })
    }
}

impl AuthReq {
    pub async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = vec![self.version];
        s.write_all(&buf).await?;
        self.methods.encode(s).await?;
        Ok(())
    }
    pub async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8; 1];
        s.read_exact(&mut buf).await?;
        let methods = VarVec::decode(s).await?;
        Ok(Self {
            version: buf[0],
            methods,
        })
    }
}

#[derive(Clone, Debug)]
pub struct AuthReply {
    pub version: u8,
    pub method: u8,
}
impl AuthReply {
    pub async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = vec![self.version, self.method];
        s.write_all(&buf).await?;
        Ok(())
    }
    pub async fn decode<T: AsyncRead + Unpin>(
        s: &mut T,
    ) -> Result<Self, Box<dyn StdError + Send + Sync + 'static>> {
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await?;
        Ok(Self {
            version: buf[0],
            method: buf[1],
        })
    }
}

#[derive(Clone, Debug)]
pub struct CmdReq {
    pub version: u8,
    pub cmd: u8,
    pub rsv: u8,
    pub dst: SocksAddr,
}

impl CmdReq {
    pub async fn encode<T: AsyncWrite + Unpin>(
        self,
        s: &mut T,
    ) -> Result<(), Box<dyn StdError + Send + Sync + 'static>> {
        let buf = vec![self.version, self.cmd, self.rsv];
        s.write_all(&buf).await?;
        self.dst.encode(s).await?;
        Ok(())
    }
    pub async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8; 3];
        s.read_exact(&mut buf).await?;
        let dst = SocksAddr::decode(s).await?;
        Ok(Self {
            version: buf[0],
            cmd: buf[1],
            rsv: buf[2],
            dst,
        })
    }
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SocksAddr {
    pub atype: u8,
    pub addr: AddrOrDomain,
    pub port: u16,
}
impl fmt::Display for SocksAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write strictly the first element into the supplied output
        // stream: `f`. Returns `fmt::Result` which indicates whether the
        // operation succeeded or failed. Note that `write!` uses syntax which
        // is very similar to `println!`.
        write!(f, "{}:{}", self.addr, self.port)
    }
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum AddrOrDomain {
    V4([u8; 4]),
    V6([u8; 16]),
    Domain(VarVec),
}
impl fmt::Display for AddrOrDomain {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write strictly the first element into the supplied output
        // stream: `f`. Returns `fmt::Result` which indicates whether the
        // operation succeeded or failed. Note that `write!` uses syntax which
        // is very similar to `println!`.
        match &self {
            AddrOrDomain::V4(x) => write!(f, "{}", IpAddr::from(*x))?,
            AddrOrDomain::V6(x) => write!(f, "{}", IpAddr::from(*x))?,
            AddrOrDomain::Domain(var_vec) => write!(
                f,
                "{}",
                String::from_utf8(var_vec.contents.clone()).map_err(|_| fmt::Error)?
            )?,
        }
        Ok(())
    }
}
impl SocksAddr {
    pub async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = vec![self.atype];
        s.write_all(&buf).await?;
        match self.addr {
            AddrOrDomain::V4(x) => s.write_all(&x).await?,
            AddrOrDomain::V6(x) => s.write_all(&x).await?,
            AddrOrDomain::Domain(x) => x.encode(s).await?,
        };
        s.write_u16(self.port).await?;
        Ok(())
    }
    pub async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8; 1];
        s.read_exact(&mut buf).await?;
        let atype = buf[0];
        let mut buf2 = vec![0u8; 1];
        let addr = match buf[0] {
            consts::SOCKS5_ADDR_TYPE_IPV4 => {
                buf2.resize(4, 0);
                s.read_exact(&mut buf2).await?;
                AddrOrDomain::V4(buf2.try_into().unwrap())
            }
            consts::SOCKS5_ADDR_TYPE_IPV6 => {
                buf2.resize(16, 0);
                s.read_exact(&mut buf2).await?;
                AddrOrDomain::V6(buf2.try_into().unwrap())
            }
            consts::SOCKS5_ADDR_TYPE_DOMAIN_NAME => {
                let buf2 = VarVec::decode(s).await?;
                AddrOrDomain::Domain(buf2)
            }
            _ => {
                panic!("Socks Protocol Violated");
            }
        };
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await?;

        let port = u16::from_be_bytes(buf);
        Ok(Self { atype, addr, port })
    }
}

impl From<SocketAddr> for SocksAddr {
    fn from(value: SocketAddr) -> Self {
        match value {
            SocketAddr::V4(socket_addr_v4) => SocksAddr {
                atype: SOCKS5_ADDR_TYPE_IPV4,
                addr: AddrOrDomain::V4(socket_addr_v4.ip().octets()),
                port: socket_addr_v4.port(),
            },
            SocketAddr::V6(socket_addr_v6) => SocksAddr {
                atype: SOCKS5_ADDR_TYPE_IPV6,
                addr: AddrOrDomain::V6(socket_addr_v6.ip().octets()),
                port: socket_addr_v6.port(),
            },
        }
    }
}
impl ToSocketAddrs for SocksAddr {
    type Iter = vec::IntoIter<SocketAddr>;

    fn to_socket_addrs(&self) -> std::io::Result<vec::IntoIter<SocketAddr>> {
        match &self.addr {
            AddrOrDomain::Domain(x) => (
                std::str::from_utf8(&x.contents).expect("Domain Name is not UTF8"),
                self.port,
            )
                .to_socket_addrs(),
            AddrOrDomain::V4(x) => {
                Ok(vec![SocketAddr::from((x.to_owned(), self.port))].into_iter())
            }
            AddrOrDomain::V6(x) => {
                Ok(vec![SocketAddr::from((x.to_owned(), self.port))].into_iter())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CmdReply {
    pub version: u8,
    pub rep: u8,
    pub rsv: u8,
    pub bind_addr: SocksAddr,
}

impl CmdReply {
    pub async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = vec![self.version, self.rep, self.rsv];
        s.write_all(&buf).await?;
        self.bind_addr.encode(s).await?;
        Ok(())
    }
    pub async fn decode<T: AsyncRead + Unpin>(
        s: &mut T,
    ) -> Result<Self, Box<dyn StdError + Send + Sync + 'static>> {
        let mut buf = [0u8; 3];
        s.read_exact(&mut buf).await?;
        let dst = SocksAddr::decode(s).await?;
        Ok(Self {
            version: buf[0],
            rep: buf[1],
            rsv: buf[2],
            bind_addr: dst,
        })
    }
}

pub struct UdpReqHeader {
    pub rsv: u16,
    pub frag: u8,
    pub dst: SocksAddr,
}

impl SDecode for UdpReqHeader {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        Ok(UdpReqHeader {
            rsv: u16::decode(s).await?,
            frag: u8::decode(s).await?,
            dst: SocksAddr::decode(s).await?,
        })
    }
}
impl SEncode for UdpReqHeader {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        self.rsv.encode(s).await?;
        self.frag.encode(s).await?;
        self.dst.encode(s).await?;
        Ok(())
    }
}
impl SDecode for u8 {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8];
        s.read_exact(&mut buf).await?;
        Ok(buf[0])
    }
}

impl SEncode for u8 {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let buf = [self];
        s.write_all(&buf).await?;
        Ok(())
    }
}

impl SDecode for u16 {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await?;

        let val = u16::from_be_bytes(buf);
        Ok(val)
    }
}

impl SEncode for u16 {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        s.write_u16(self).await?;
        Ok(())
    }
}
