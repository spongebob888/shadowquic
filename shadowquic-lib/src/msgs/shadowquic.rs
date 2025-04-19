use crate::error::SError;

use super::socks5::{SDecode, SEncode, SocksAddr};

#[repr(u8)]
pub enum SQCmd {
    Connect = 0x1,
    Bind = 0x02,
    AssociatOverDatagram = 0x3,
    AssociatOverStream = 0x4,
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

pub struct SQReq {
    pub cmd: SQCmd,
    pub dst: SocksAddr,
}
impl SDecode for SQReq {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        Ok(Self {
            cmd: SQCmd::decode(s).await?,
            dst: SocksAddr::decode(s).await?,
        })
    }
}
impl SEncode for SQReq {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        self.cmd.encode(s).await?;
        self.dst.encode(s).await?;
        Ok(())
    }
}

pub struct SQUdpControlHeader {
    pub dst: SocksAddr,
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
}
pub struct SQPacketStreamHeader {
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
    pub len: u16,
}
pub struct SQPacketDatagramHeader {
    pub id: u16, // id is one to one coresponance a udpsocket and proxy dst
}

impl SDecode for SQUdpControlHeader {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        Ok(Self {
            dst: SocksAddr::decode(s).await?,
            id: u16::decode(s).await?,
        })
    }
}
impl SEncode for SQUdpControlHeader {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        self.dst.encode(s).await?;
        self.id.encode(s).await?;
        Ok(())
    }
}

impl SDecode for SQPacketDatagramHeader {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        Ok(Self {
            id: u16::decode(s).await?,
        })
    }
}
impl SEncode for SQPacketDatagramHeader {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        self.id.encode(s).await?;
        Ok(())
    }
}

impl SDecode for SQPacketStreamHeader {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        Ok(Self {
            id: u16::decode(s).await?,
            len: u16::decode(s).await?,
        })
    }
}
