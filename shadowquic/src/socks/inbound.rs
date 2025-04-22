use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::config::SocksServerCfg;
use crate::error::SError;
use crate::msgs::socks5::{
    self, AddrOrDomain, CmdReq, SDecode, SEncode, SOCKS5_ADDR_TYPE_DOMAIN_NAME,
    SOCKS5_ADDR_TYPE_IPV4, SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT,
    SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION, SocksAddr, UdpReqHeader,
};
use crate::{Inbound, ProxyRequest, TcpSession, UdpRecv, UdpSend, UdpSession};
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UdpSocket};

use anyhow::Result;
use tracing::{Instrument, trace, trace_span, warn};

pub struct SocksServer {
    #[allow(dead_code)]
    bind_addr: SocketAddr,
    listener: TcpListener,
}
impl SocksServer {
    pub async fn new(cfg: SocksServerCfg) -> Result<Self, SError> {
        Ok(Self {
            bind_addr: cfg.bind_addr,
            listener: TcpListener::bind(cfg.bind_addr).await?,
        })
    }
}

async fn handle_socks<T: AsyncRead + AsyncWrite + Unpin>(
    mut s: T,
    local_addr: SocketAddr,
) -> Result<(T, CmdReq, Option<UdpSocket>), SError> {
    let _req = socks5::AuthReq::decode(&mut s).await?;
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

    reply.encode(&mut s).await?;
    trace!("socks request accepted: {}", req.dst);
    Ok((s, req, socket))
}

pub struct UdpSocksWrap(Arc<UdpSocket>, OnceCell<SocketAddr>); // remote addr
#[async_trait]
impl UdpRecv for UdpSocksWrap {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let mut buf = BytesMut::new();
        buf.resize(2000, 0);

        let (len, dst) = self.0.recv_from(&mut buf).await?;
        let mut cur = Cursor::new(buf);
        let req = socks5::UdpReqHeader::decode(&mut cur).await?;
        if req.frag != 0 {
            warn!("dropping fragmented udp datagram ");
            return Err(SError::ProtocolUnimpl);
        }
        let headsize: usize = cur.position().try_into().unwrap();
        let buf = cur.into_inner();
        self.1
            .get_or_init(|| async {
                let _ = self.0.connect(dst).await;
                dst
            })
            .await;
        let buf = buf.freeze();
        // assert!(len < buf.len());
        Ok((buf.slice(headsize..len), req.dst))
    }
}
#[async_trait]
impl UdpSend for UdpSocksWrap {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let reply = UdpReqHeader {
            rsv: 0,
            frag: 0,
            dst: addr,
        };
        let mut buf_new = BytesMut::with_capacity(1600);
        let header = Vec::new();
        let mut cur = Cursor::new(header);
        reply.encode(&mut cur).await?;
        let header = cur.into_inner();
        buf_new.put(Bytes::from(header));
        buf_new.put(buf);

        Ok(self.0.send(&buf_new).await?)
    }
}

#[async_trait]
impl Inbound for SocksServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let (stream, addr) = self.listener.accept().await?;
        let span = trace_span!("socks", src = addr.to_string());
        let local_addr = stream.local_addr().unwrap();

        let (s, req, socket) = handle_socks(stream, local_addr).instrument(span).await?;
        match req.cmd {
            SOCKS5_CMD_TCP_CONNECT => Ok(ProxyRequest::Tcp(TcpSession {
                stream: Box::new(s),
                dst: req.dst,
            })),
            SOCKS5_CMD_UDP_ASSOCIATE => {
                let socket = Arc::new(socket.unwrap());
                Ok(ProxyRequest::Udp(UdpSession {
                    send: Arc::new(UdpSocksWrap(socket.clone(), Default::default())),
                    recv: Box::new(UdpSocksWrap(socket, Default::default())),
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
