use std::net::SocketAddr;
use std::sync::Arc;

use crate::TcpTrait;
use crate::config::{AuthUser, SocksServerCfg};
use crate::error::SError;
use crate::msgs::socks5::{
    self, AddrOrDomain, AuthReq, CmdReq, PasswordAuthReply, PasswordAuthReq,
    SOCKS5_AUTH_METHOD_NONE, SOCKS5_AUTH_METHOD_PASSWORD, SOCKS5_CMD_TCP_BIND,
    SOCKS5_CMD_TCP_CONNECT, SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_REPLY_SUCCEEDED, SOCKS5_VERSION,
};
use crate::msgs::{SDecode, SEncode};
use crate::utils::dual_socket::to_ipv4_mapped;
use crate::{Inbound, ProxyRequest, TcpSession, UdpSession};
use async_trait::async_trait;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, UdpSocket};

use anyhow::Result;
use tracing::{Instrument, info, info_span};

use super::UdpSocksWrap;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct SocksServer {
    #[allow(dead_code)]
    bind_addr: SocketAddr,
    users: Vec<AuthUser>,
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

    async fn authenticate<S>(mut stream: S, users: &[AuthUser]) -> Result<S, SError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let auth_req = AuthReq::decode(&mut stream).await?;
        if auth_req.version != SOCKS5_VERSION {
            return Err(SError::ProtocolViolation);
        }

        let methods = auth_req.methods;
        if methods.contents.is_empty() {
            return Err(SError::ProtocolViolation);
        }

        let method = if users.is_empty() {
            SOCKS5_AUTH_METHOD_NONE
        } else {
            SOCKS5_AUTH_METHOD_PASSWORD
        };

        if !methods.contents.contains(&method) {
            let reply = socks5::AuthReply {
                version: SOCKS5_VERSION,
                method: 0xFF,
            };
            reply.encode(&mut stream).await?;
            return Err(SError::SocksError(format!(
                "authentication method not supported: {:?}",
                methods.contents
            )));
        }

        let reply = socks5::AuthReply {
            version: SOCKS5_VERSION,
            method,
        };
        reply.encode(&mut stream).await?;

        if users.is_empty() {
            return Ok(stream);
        }

        let auth = PasswordAuthReq::decode(&mut stream).await?;

        let username = String::from_utf8(auth.username.contents)
            .map_err(|_| SError::SocksError("invalid UTF-8 in username".to_string()))?;
        let password = String::from_utf8(auth.password.contents)
            .map_err(|_| SError::SocksError("invalid UTF-8 in password".to_string()))?;

        let ok = users
            .iter()
            .any(|u| u.username == username && u.password == password);

        if !ok {
            let reply = PasswordAuthReply {
                version: 0x01,
                status: 0x01,
            };
            reply.encode(&mut stream).await?;
            return Err(SError::SocksError("authentication failed".to_string()));
        }

        let reply = PasswordAuthReply {
            version: 0x01, // authentication version not socks version
            status: SOCKS5_REPLY_SUCCEEDED,
        };
        reply.encode(&mut stream).await?;

        Ok(stream)
    }

    async fn handle_socks<S>(
        mut s: S,
        local_addr: SocketAddr,
        users: &[AuthUser],
    ) -> Result<(S, CmdReq, Option<UdpSocket>), SError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        s = Self::authenticate(s, users).await?;
        let req = socks5::CmdReq::decode(&mut s).await?;

        let addr = match req.dst.addr {
            AddrOrDomain::V4(_) | AddrOrDomain::Domain(_) => AddrOrDomain::V4([0u8, 0u8, 0u8, 0u8]),
            AddrOrDomain::V6(x) => AddrOrDomain::V6(x.map(|_| 0u8)),
        };

        let mut reply = socks5::CmdReply {
            version: SOCKS5_VERSION,
            rep: SOCKS5_REPLY_SUCCEEDED,
            rsv: 0u8,
            bind_addr: socks5::SocksAddr { addr, port: 0u16 },
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
        Ok((s, req, socket))
    }

    pub async fn accept_stream_with_local_addr<S>(
        stream: S,
        local_addr: SocketAddr,
        users: &[AuthUser],
    ) -> Result<ProxyRequest, SError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static + TcpTrait,
    {
        let (s, req, socket) = SocksServer::handle_socks(stream, local_addr, users).await?;
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
                    bind_addr: req.dst,
                    stream: Some(Box::new(s)),
                }))
            }
            _ => Err(SError::ProtocolViolation),
        }
    }
}

#[async_trait]
impl Inbound for SocksServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let (stream, addr) = self.listener.accept().await?;
        let span = info_span!("socks", src = %addr);
        let _enter = span.enter();
        // ipv4 may be mapped for dual stack socket
        let local_addr = to_ipv4_mapped(stream.local_addr().unwrap());

        let (s, req, socket) = SocksServer::handle_socks(stream, local_addr, &self.users)
            .in_current_span()
            .await?;
        match req.cmd {
            SOCKS5_CMD_TCP_CONNECT => {
                info!(dst = %req.dst, "tcp connect request accepted");
                Ok(ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(s),
                    dst: req.dst,
                }))
            }
            SOCKS5_CMD_UDP_ASSOCIATE => {
                info!(bind_dst = %req.dst, "udp associate request accepted");
                let socket = Arc::new(socket.unwrap());
                Ok(ProxyRequest::Udp(UdpSession {
                    send: Arc::new(UdpSocksWrap(socket.clone(), Default::default())),
                    recv: Box::new(UdpSocksWrap(socket, Default::default())),
                    bind_addr: req.dst,
                    stream: Some(Box::new(s)),
                }))
            }
            _ => {
                return Err(SError::ProtocolViolation);
            }
        }
    }
}
