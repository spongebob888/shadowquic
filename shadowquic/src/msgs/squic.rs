use crate::error::SError;

use super::socks5::{SDecode, SEncode, SocksAddr};
use shadowquic_macros::{SDecode, SEncode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub static SUN_QUIC_AUTH_LEN: usize = 256;
#[repr(u8)]
#[derive(PartialEq)]
pub enum SQCmd {
    Connect = 0x1,
    Bind = 0x02,
    AssociatOverDatagram = 0x3,
    AssociatOverStream = 0x4,
    Authenticate = 0x5,
}

impl SDecode for SQCmd {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(
        s: &mut T,
    ) -> Result<Self, crate::error::SError> {
        let x: u8 = u8::decode(s).await?;
        let cmd = match x {
            0x1 => SQCmd::Connect,
            0x02 => SQCmd::Bind,
            0x3 => SQCmd::AssociatOverDatagram,
            0x4 => SQCmd::AssociatOverStream,
            0x5 => SQCmd::Authenticate,
            _ => return Err(SError::ProtocolViolation),
        };
        Ok(cmd)
    }
}
impl SEncode for SQCmd {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let x: u8 = self as u8;
        x.encode(s).await?;
        Ok(())
    }
}

#[derive(PartialEq)]
pub enum SQReq {
    SQConnect(SocksAddr),
    SQBind(SocksAddr),
    SQAssociatOverDatagram(SocksAddr),
    SQAssociatOverStream(SocksAddr),
    SQAuthenticate([u8; SUN_QUIC_AUTH_LEN]),
}


impl SEncode for SQReq {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        match self {
            SQReq::SQConnect(addr) => {
                SQCmd::Connect.encode(s).await?;
                addr.encode(s).await?;
            }
            SQReq::SQBind(addr) => {
                SQCmd::Bind.encode(s).await?;
                addr.encode(s).await?;
            }
            SQReq::SQAssociatOverDatagram(addr) => {
                SQCmd::AssociatOverDatagram.encode(s).await?;
                addr.encode(s).await?;
            }
            SQReq::SQAssociatOverStream(addr) => {
                SQCmd::AssociatOverStream.encode(s).await?;
                addr.encode(s).await?;
            }
            SQReq::SQAuthenticate(data) => {
                SQCmd::Authenticate.encode(s).await?;
                s.write_all(&data).await?;
            }
        }
        Ok(())
    }
}

impl SDecode for SQReq {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let cmd = SQCmd::decode(s).await?;
        match cmd {
            SQCmd::Connect => {
                let addr = SocksAddr::decode(s).await?;
                Ok(SQReq::SQConnect(addr))
            }
            SQCmd::Bind => {
                let addr = SocksAddr::decode(s).await?;
                Ok(SQReq::SQBind(addr))
            }
            SQCmd::AssociatOverDatagram => {
                let addr = SocksAddr::decode(s).await?;
                Ok(SQReq::SQAssociatOverDatagram(addr))
            }
            SQCmd::AssociatOverStream => {
                let addr = SocksAddr::decode(s).await?;
                Ok(SQReq::SQAssociatOverStream(addr))
            }
            SQCmd::Authenticate => {
                let mut data = [0u8; SUN_QUIC_AUTH_LEN];
                s.read_exact(&mut data).await?;
                Ok(SQReq::SQAuthenticate(data))
            }
        }
    }
}
#[derive(SEncode, SDecode)]
pub struct SQUdpControlHeader {
    pub dst: SocksAddr,
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
}

#[derive(SEncode, SDecode)]
pub struct SQPacketStreamHeader {
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
    pub len: u16,
}

#[derive(SEncode, SDecode, Clone)]
pub struct SQPacketDatagramHeader {
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
}
