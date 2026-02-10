use std::{sync::Arc, vec};

use crate::error::SError;

use super::socks5::{SDecode, SEncode, SocksAddr};
use shadowquic_macros::{SDecode, SEncode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub static SUNNY_QUIC_AUTH_LEN: usize = 64;
pub(crate) type SunnyCredential = Arc<[u8; SUNNY_QUIC_AUTH_LEN]>;
#[repr(u8)]
#[derive(SEncode, PartialEq)]
pub enum SQCmd {
    Connect = 0x1,
    Bind = 0x02,
    AssociatOverDatagram = 0x3,
    AssociatOverStream = 0x4,
    Authenticate = 0x5,
}

#[repr(u8)]
#[derive(SDecode, PartialEq)]
pub enum Cmd {
    Connect=0x78,
    Bind,
    AssociatOverDatagram,
    AssociatOverStream ,
    Authenticate,
}

impl SDecode for SQCmd {
    async fn decode<T: tokio::io::AsyncRead + Unpin>(
        s: &mut T,
    ) -> Result<Self, crate::error::SError> {
        let x: u8 = u8::decode(s).await?;
        let cmd = match x {
            0x1 => SQCmd::Connect,
            0x2 => SQCmd::Bind,
            0x3 => SQCmd::AssociatOverDatagram,
            0x4 => SQCmd::AssociatOverStream,
            0x5 => SQCmd::Authenticate,
            _ => return Err(SError::ProtocolViolation),
        };
        Ok(cmd)
    }
}

#[derive(PartialEq)]
#[repr(u8)]
#[derive(SEncode, SDecode)]
pub enum SQReq {
    SQConnect(SocksAddr) = 0x1,
    SQBind(SocksAddr) = 0x2,
    SQAssociatOverDatagram(SocksAddr) = 0x3,
    SQAssociatOverStream(SocksAddr) = 0x4,
    SQAuthenticate(SunnyCredential) = 0x5,
}

// impl SEncode for SQReq {
//     async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
//         match self {
//             SQReq::SQConnect(addr) => {
//                 SQCmd::Connect.encode(s).await?;
//                 addr.encode(s).await?;
//             }
//             SQReq::SQBind(addr) => {
//                 SQCmd::Bind.encode(s).await?;
//                 addr.encode(s).await?;
//             }
//             SQReq::SQAssociatOverDatagram(addr) => {
//                 SQCmd::AssociatOverDatagram.encode(s).await?;
//                 addr.encode(s).await?;
//             }
//             SQReq::SQAssociatOverStream(addr) => {
//                 SQCmd::AssociatOverStream.encode(s).await?;
//                 addr.encode(s).await?;
//             }
//             SQReq::SQAuthenticate(data) => {
//                 SQCmd::Authenticate.encode(s).await?;
//                 s.write_all(&*data).await?;
//             }
//         }
//         Ok(())
//     }
// }

// impl SDecode for SQReq {
//     async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
//         let cmd = SQCmd::decode(s).await?;
//         match cmd {
//             SQCmd::Connect => {
//                 let addr = SocksAddr::decode(s).await?;
//                 Ok(SQReq::SQConnect(addr))
//             }
//             SQCmd::Bind => {
//                 let addr = SocksAddr::decode(s).await?;
//                 Ok(SQReq::SQBind(addr))
//             }
//             SQCmd::AssociatOverDatagram => {
//                 let addr = SocksAddr::decode(s).await?;
//                 Ok(SQReq::SQAssociatOverDatagram(addr))
//             }
//             SQCmd::AssociatOverStream => {
//                 let addr = SocksAddr::decode(s).await?;
//                 Ok(SQReq::SQAssociatOverStream(addr))
//             }
//             SQCmd::Authenticate => {
//                 let mut data = [0u8; SUNNY_QUIC_AUTH_LEN];
//                 s.read_exact(&mut data).await?;
//                 Ok(SQReq::SQAuthenticate(Arc::new(data)))
//             }
//         }
//     }
// }
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

impl SEncode for SunnyCredential {
    async fn encode<T: AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
        s.write_all(&*self).await?;
        Ok(())
    }
}
impl SDecode for SunnyCredential {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut data = [0u8; SUNNY_QUIC_AUTH_LEN];
        s.read_exact(&mut data).await?;
        Ok(Arc::new(data))
    }
}

#[tokio::test]
async fn test_encode_req() {
    let req = SQReq::SQAuthenticate(Arc::new([1u8; SUNNY_QUIC_AUTH_LEN]));
    let mut buf = vec![0u8; 1 + SUNNY_QUIC_AUTH_LEN];
    let mut cursor = std::io::Cursor::new(buf);
    req.encode(&mut cursor).await.unwrap();
    assert_eq!(cursor.into_inner()[0], 0x5);

}
