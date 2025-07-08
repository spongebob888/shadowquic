use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::{SocksServerCfg, SocksUser};
use crate::error::SError;
use crate::msgs::socks5::{
    self, AddrOrDomain, AuthReq, CmdReq, PasswordAuthReply, PasswordAuthReq, SDecode, SEncode,
    SOCKS5_ADDR_TYPE_DOMAIN_NAME, SOCKS5_ADDR_TYPE_IPV4, SOCKS5_AUTH_METHOD_NONE,
    SOCKS5_AUTH_METHOD_PASSWORD, SOCKS5_CMD_TCP_BIND, SOCKS5_CMD_TCP_CONNECT,
    SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION,
};
use crate::utils::dual_socket::to_ipv4_mapped;
use crate::{Inbound, ProxyRequest, TcpSession, UdpSession};
use async_trait::async_trait;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use anyhow::Result;
use tracing::{Instrument, trace, trace_span};

use super::UdpSocksWrap;

pub struct SocksServer {
    #[allow(dead_code)]
    bind_addr: SocketAddr,
    users: Vec<SocksUser>,
    listener: TcpListener,
}
impl SocksServer {
    pub async fn new(cfg: SocksServerCfg) -> Result<Self, SError> {
        let dual_stack = cfg.bind_addr.is_ipv6();
        let socket = Socket::new(
            // Use socket2 for dualstack for windows compact
            if dual_stack {
                Domain::IPV6
            } else {
                Domain::IPV4
            },
            Type::STREAM,
            Some(Protocol::TCP),
        )?;
        if dual_stack {
            let _ = socket
                .set_only_v6(false)
                .map_err(|e| tracing::warn!("failed to set dual stack for socket: {}", e));
        };
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&cfg.bind_addr.into())?;
        socket.listen(256)?;
        let listener = TcpListener::from_std(socket.into())
            .map_err(|e| SError::SocksError(format!("failed to create TcpListener: {e}")))?;
        Ok(Self {
            bind_addr: cfg.bind_addr,
            listener,
            users: cfg.users,
        })
    }
    pub async fn authenticate(&self, mut stream: TcpStream) -> Result<TcpStream, SError> {
        let auth_req = AuthReq::decode(&mut stream).await?;
        if auth_req.version != SOCKS5_VERSION {
            return Err(SError::ProtocolViolation);
        }
        let methods = auth_req.methods;
        if methods.contents.is_empty() {
            return Err(SError::ProtocolViolation);
        }
        let method = if self.users.is_empty() {
            SOCKS5_AUTH_METHOD_NONE
        } else {
            SOCKS5_AUTH_METHOD_PASSWORD
        };
        if !methods.contents.contains(&method) {
            return Err(SError::SocksError(format!(
                "authentication method not supported:{:?}",
                methods.contents
            )));
        }

        let reply = socks5::AuthReply {
            version: SOCKS5_VERSION,
            method,
        };
        reply.encode(&mut stream).await?;
        if self.users.is_empty() {
            return Ok(stream);
        }
        let auth = PasswordAuthReq::decode(&mut stream).await?;
        if !self.users.contains(&SocksUser {
            username: String::from_utf8(auth.username.contents)
                .map_err(|_| SError::SocksError("invalid UTF-8 in username".to_string()))?,
            password: String::from_utf8(auth.password.contents)
                .map_err(|_| SError::SocksError("invalid UTF-8 in password".to_string()))?,
        }) {
            return Err(SError::SocksError("authentication failed".to_string()));
        }
        let reply = PasswordAuthReply {
            version: 0x01, // authentication version not socks version
            status: SOCKS5_REPLY_SUCCEEDED,
        };
        reply.encode(&mut stream).await?;
        Ok(stream)
    }
    async fn handle_socks(
        &self,
        s: TcpStream,
        local_addr: SocketAddr,
    ) -> Result<(TcpStream, CmdReq, Option<UdpSocket>), SError> {
        let mut s = self.authenticate(s).await?;
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
}

#[async_trait]
impl Inbound for SocksServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let (stream, addr) = self.listener.accept().await?;
        let span = trace_span!("socks", src = addr.to_string());
        // ipv4 may be mapped for dual stack socket
        let local_addr = to_ipv4_mapped(stream.local_addr().unwrap());

        let (s, req, socket) = self
            .handle_socks(stream, local_addr)
            .instrument(span)
            .await?;
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
