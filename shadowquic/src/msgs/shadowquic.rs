use crate::error::SError;

use super::socks5::{SDecode, SEncode, SocksAddr};
use shadowquic_macros::{SDecode, SEncode};

#[repr(u8)]
#[derive(PartialEq)]
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

#[derive(SEncode, SDecode)]
pub struct SQReq {
    pub cmd: SQCmd,
    pub dst: SocksAddr,
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
