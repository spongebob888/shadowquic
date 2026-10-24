use std::sync::Arc;

use crate::UserName;
use crate::error::{SError, SResult};

use super::socks5::SocksAddr;
use super::{SDecode, SEncode};
use crate::config::AuthUser;
use shadowquic_macros::{SDecode, SEncode};
use tokio::io::{AsyncRead, AsyncReadExt};

pub static SUNNY_QUIC_AUTH_LEN: usize = 64;
pub(crate) type SunnyCredential = Arc<[u8; SUNNY_QUIC_AUTH_LEN]>;

pub const SQ_REQ_CONNECT: u8 = 0x1;
pub const SQ_REQ_BIND: u8 = 0x2;
pub const SQ_REQ_ASSOCIATE_OVER_DATAGRAM: u8 = 0x3;
pub const SQ_REQ_ASSOCIATE_OVER_STREAM: u8 = 0x4;
pub const SQ_REQ_AUTHENTICATE: u8 = 0x5;
pub const SQ_REQ_EXTENSION: u8 = 0xFF;

#[derive(PartialEq)]
#[repr(u8)]
#[derive(SEncode, SDecode)]
pub enum SQReq {
    SQConnect(SocksAddr) = SQ_REQ_CONNECT,
    SQBind(SocksAddr) = SQ_REQ_BIND,
    SQAssociatOverDatagram(SocksAddr) = SQ_REQ_ASSOCIATE_OVER_DATAGRAM,
    SQAssociatOverStream(SocksAddr) = SQ_REQ_ASSOCIATE_OVER_STREAM,
    SQAuthenticate(SunnyCredential) = SQ_REQ_AUTHENTICATE,
    SQExtension(SQExtOpcode) = SQ_REQ_EXTENSION,
}

impl SQReq {
    pub async fn decode_tag<T: AsyncRead + Unpin + Send>(s: &mut T) -> SResult<u8> {
        u8::decode(s).await
    }
    pub async fn decode_with_tag<T: AsyncRead + Unpin + Send>(tag: u8, s: &mut T) -> SResult<Self> {
        let prefix = [tag];
        let mut s = prefix.chain(s);
        Self::decode(&mut s).await
    }
}

#[derive(PartialEq)]
#[repr(u64)]
#[derive(SEncode, SDecode)]
/// SQ Extention Opcode
pub enum SQExtOpcode {
    /// Connection related opcode
    Conn(ExtOpcodeConn) = 0x1,
    /// User related opcode
    User(ExtOpcodeUser) = 0x2,
}
#[derive(PartialEq)]
#[repr(u8)]
#[derive(SEncode, SDecode)]
pub enum ExtOpcodeConn {
    /// Get connection stats
    GetConnStats = 0x0,
}
#[derive(PartialEq, SEncode, SDecode, Clone)]
#[cfg_attr(feature = "restful-api", derive(serde::Serialize))]
#[size_tag] // size tag allow future compatibility by prefixing a size field before the content, so that decoder can skip unknown opcode content based on the size field.
pub struct ConnStats {
    pub lost_packets: u64,
    pub sent_packets: u64,
    /// In unit of milliseconds
    pub rtt: f64,
    pub current_mtu: u16,
}

#[derive(PartialEq)]
#[repr(u8)]
#[derive(SEncode, SDecode)]
pub enum ExtOpcodeUser {
    AddUser(AuthUser) = 0x0,
    RemoveUser(UserName) = 0x1,
    ListUsers = 0x2,
    GetUserStats(UserName) = 0x3,
    KillUserConn(UserName) = 0x4,
    GetAllStats = 0x5,
    ClearUserStats(UserName) = 0x6,
    ClearAllStats = 0x7,
}
#[derive(PartialEq)]
#[repr(u8)]
#[derive(SEncode, SDecode, Debug)]
pub enum SQExtError {
    NotAvailable = 0x0,
    PermissionDenied = 0x1,
    NotFound = 0x2,
    Other(String) = 0xFF,
}

#[derive(PartialEq, SEncode, SDecode, Default)]
#[size_tag]
pub struct UserStats {
    pub tcp_sent: u64,
    pub tcp_recv: u64,
    pub udp_sent: u64,
    pub udp_recv: u64,
    pub tcp_conns: u64,
    pub udp_conns: u64,
    /// Number of online connections, equal to devices connected.
    pub conn_num: u32,
    /// User these stats belong to. Added in v0.3.11
    pub username: UserName,
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

#[tokio::test]
async fn test_encode_req() {
    let req = SQReq::SQAuthenticate(Arc::new([1u8; SUNNY_QUIC_AUTH_LEN]));
    let buf = vec![0u8; 1 + SUNNY_QUIC_AUTH_LEN];
    let mut cursor = std::io::Cursor::new(buf);
    req.encode(&mut cursor).await.unwrap();
    assert_eq!(cursor.into_inner()[0], 0x5);
}

#[tokio::test]
async fn test_macro_expand_req() {
    const TEST_CONST: u8 = 89;
    #[repr(u8)]
    #[derive(SDecode, SEncode, PartialEq)]
    #[allow(dead_code)]
    pub enum Cmd {
        Connect,
        Bind = 0x8,
        AssociatOverDatagram,
        AssociatOverStream = TEST_CONST,
        Authenticate,
    }
}
