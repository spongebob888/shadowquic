use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use std::net::SocketAddr;

use crate::error::SError;
use crate::msgs::socks5::{
    self, AddrOrDomain, CmdReq, SDecode, SEncode, SOCKS5_ADDR_TYPE_DOMAIN_NAME,
    SOCKS5_ADDR_TYPE_IPV4, SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT,
    SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION, SocksAddr, UdpReqHeader,
};
use crate::{Inbound, ProxyRequest, TcpSession, UdpSession, UdpSocketTrait};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UdpSocket};

use anyhow::Result;
use tracing::{error, trace};
pub struct SocksServer {
    bind_addr: SocketAddr,
    listener: TcpListener,
}
impl SocksServer {
    pub async fn new(bind_addr: SocketAddr) -> Result<Self, SError> {
        Ok(Self {
            bind_addr,
            listener: TcpListener::bind(bind_addr).await?,
        })
    }
}

async fn handle_socks<T: AsyncRead + AsyncWrite + Unpin>(
    mut s: T,
    local_addr: SocketAddr,
) -> Result<(T, CmdReq, Option<UdpSocket>), SError> {
    let req = socks5::AuthReq::decode(&mut s).await?;
    let reply = socks5::AuthReply {
        version: SOCKS5_VERSION,
        method: SOCKS5_AUTH_METHOD_NONE,
    };
    reply.encode(&mut s).await?;
    let req = socks5::CmdReq::decode(&mut s).await?;

    let addr = match req.dst.addr {
        AddrOrDomain::V4(_) | AddrOrDomain::Domain(_) => AddrOrDomain::V4([0u8, 0u8, 0u8, 0u8]),
        AddrOrDomain::V6(x) => AddrOrDomain::V6(x.map(|_| 0u8)),
    };
    let mut atype = req.dst.atype;
    if atype == SOCKS5_ADDR_TYPE_DOMAIN_NAME {
        atype = SOCKS5_ADDR_TYPE_IPV4;
    }

    let mut reply = socks5::CmdReply {
        version: SOCKS5_VERSION,
        rep: SOCKS5_REPLY_SUCCEEDED,
        rsv: 0u8,
        bind_addr: socks5::SocksAddr {
            atype,
            addr,
            port: 0u16,
        },
    };
    let (reply, socket) = match req.cmd {
        SOCKS5_CMD_TCP_CONNECT => (reply, None),
        SOCKS5_CMD_UDP_ASSOCIATE => {
            let mut local_addr = local_addr;

            local_addr.set_port(0);
            let socket = UdpSocket::bind(local_addr).await?;
            let local_addr = socket.local_addr()?;
            reply.bind_addr = local_addr.into();
            (reply, Some(socket))
        }
        SOCKS5_CMD_TCP_BIND => {
            return Err(SError::ProtocolUnimpl);
        }
        _ => {
            return Err(SError::ProtocolViolation);
        }
    };

    trace!("reply:{:?}", reply);
    reply.encode(&mut s).await?;
    Ok((s, req, socket))
}

pub struct UdpSocksWrap(UdpSocket, OnceLock<SocketAddr>); // remote addr
#[async_trait]
impl UdpSocketTrait for UdpSocksWrap {
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, usize, SocksAddr), SError> {
        let result = self.0.recv_from(buf).await?;
        let mut cur = Cursor::new(buf);
        let req = socks5::UdpReqHeader::decode(&mut cur).await?;
        if req.frag != 0 {
            error!("dropping fragmented udp datagram ");
            return Err(SError::ProtocolUnimpl);
        }
        let headsize: usize = cur.position().try_into().unwrap();
        self.1.get_or_init(|| result.1);
        Ok((headsize, result.0, req.dst))
    }

    async fn send_to(&self, buf: &[u8], addr: SocksAddr) -> Result<usize, SError> {
        let reply = UdpReqHeader {
            rsv: 0,
            frag: 0,
            dst: addr,
        };
        let mut buf_new = Vec::new();
        reply.encode(&mut buf_new).await?;
        trace!("udp reply: {:?}", buf_new);
        buf_new.extend_from_slice(buf);

        Ok(self.0.send_to(&buf_new, self.1.get().unwrap()).await?)
    }
}

#[async_trait]
impl Inbound for SocksServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let (stream, _) = self.listener.accept().await?;
        let local_addr = stream.local_addr().unwrap();

        let (s, req, socket) = handle_socks(stream, local_addr).await?;
        trace!("socks request accepted: {}", req.dst);
        match req.cmd {
            SOCKS5_CMD_TCP_CONNECT => Ok(ProxyRequest::Tcp(TcpSession {
                stream: Box::new(s),
                dst: req.dst,
            })),
            SOCKS5_CMD_UDP_ASSOCIATE => {
                let req = req;
                // // Not sure here
                // match req.dst.addr {
                //     AddrOrDomain::V4([0u8,0u8,0u8,0u8])=>{req.dst.port=0},
                //     AddrOrDomain::V6([0u8,0u8,0u8,0u8,0u8,0u8,0u8,0u8,
                //         0u8,0u8,0u8,0u8,0u8,0u8,0u8,0u8,]) => {req.dst.port=0},
                //     _ => {},
                // };
                Ok(ProxyRequest::Udp(UdpSession {
                    socket: Arc::new(UdpSocksWrap(socket.unwrap(), Default::default())),
                    dst: req.dst,
                    stream: Some(Box::new(s)),
                }))
            }
            _ => {
                return Err(SError::ProtocolViolation);
            }
        }
    }
}
