use crate::error::SError;

use super::socks5::{SDecode, SEncode, SocksAddr};

#[repr(u8)]
pub enum SQCmd {
    Connect = 0x1,
    AssociatOverDatagram = 0x3,
    AssociatOverStream = 0x4,
}

impl SDecode for SQCmd {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, crate::error::SError> {
        let x:u8 = u8::decode(s).await?;
        let cmd = match x {
            0x1 => SQCmd::Connect,
            0x3 => SQCmd::AssociatOverDatagram,
            0x4 => SQCmd::AssociatOverStream,
            _ => return Err(SError::ProtocolViolation),
        };
        Ok(cmd)
    }
}
impl SEncode for SQCmd {
    async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        let x:u8 = self as u8;
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
        Ok(Self{
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

pub struct SQStreamPacketHeader {
    pub dst: SocksAddr,
    pub len: u16,  // datagram size
}
pub struct SQDatagramPacketHeader {
    pub dst: SocksAddr,
    pub id: u16,  // id is one to one coresponance a udpsocket and proxy dst
}