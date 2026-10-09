use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;

use crate::TcpTrait;
use crate::config::{AuthUser, SocksServerCfg};
use crate::error::{SError, SResult};
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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UdpSocket};

use tokio::sync::mpsc::{Receiver, Sender, channel};
use tracing::{Instrument, error, info};

use super::UdpSocksWrap;

pub struct SocksServer {
    pub cfg: SocksServerCfg,
    request_sender: Sender<ProxyRequest>,
    request_receiver: Receiver<ProxyRequest>,
}

impl SocksServer {
    pub async fn new(cfg: SocksServerCfg) -> Result<Self, SError> {
        let (s, r) = channel(20);
        Ok(Self {
            cfg,
            request_sender: s,
            request_receiver: r,
        })
    }

    pub async fn accept_stream_with_local_addr<S>(
        stream: S,
        local_addr: SocketAddr,
        users: &[AuthUser],
    ) -> Result<ProxyRequest, SError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static + TcpTrait,
    {
        let users_arc = Arc::new(users.to_vec());
        let (s, req, socket) = handle_socks(users_arc, stream, local_addr).await?;
        let src_addr = s.peer_addr().map(to_ipv4_mapped);
        match req.cmd {
            SOCKS5_CMD_TCP_CONNECT => Ok(ProxyRequest::Tcp(TcpSession {
                stream: Box::new(s),
                dst: req.dst,
                src_addr,
                user_context: Default::default(),
            })),
            SOCKS5_CMD_UDP_ASSOCIATE => {
                let socket = Arc::new(socket.unwrap());
                let mut session = UdpSession::from_recv(
                    Arc::new(UdpSocksWrap(socket.clone(), Default::default())),
                    Box::new(UdpSocksWrap(socket, Default::default())),
                    Some(Box::new(s)),
                    req.dst,
                    Default::default(),
                )
                .await?;
                session.src_addr = session.src_addr.map(to_ipv4_mapped);
                Ok(ProxyRequest::Udp(session))
            }
            _ => Err(SError::ProtocolViolation),
        }
    }
}

// Public authentication helper (standalone)
pub async fn authenticate<S>(users: Arc<Vec<AuthUser>>, mut stream: S) -> Result<S, SError>
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
    if users.is_empty() {
        return Ok(stream);
    }
    let auth = PasswordAuthReq::decode(&mut stream).await?;
    let user = AuthUser {
        username: String::from_utf8(auth.username.contents)
            .map_err(|_| SError::SocksError("invalid UTF-8 in username".to_string()))?,
        password: String::from_utf8(auth.password.contents)
            .map_err(|_| SError::SocksError("invalid UTF-8 in password".to_string()))?,
    };
    if !users.contains(&user) {
        return Err(SError::SocksError("authentication failed".to_string()));
    }
    tracing::Span::current().record("user", tracing::field::display(user.username));
    tracing::debug!("socks authentication succeeded");
    let reply = PasswordAuthReply {
        version: 0x01,
        status: SOCKS5_REPLY_SUCCEEDED,
    };
    reply.encode(&mut stream).await?;
    Ok(stream)
}

async fn handle_socks<S>(
    users: Arc<Vec<AuthUser>>,
    s: S,
    local_addr: SocketAddr,
) -> Result<(S, CmdReq, Option<UdpSocket>), SError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut s = authenticate(users, s).await?;
    let mut req = socks5::CmdReq::decode(&mut s).await?;
    req.dst.canonicalize_ip();

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

// Handle a single TCP connection task (upstream style)
async fn handle_tcp(
    users: Arc<Vec<AuthUser>>,
    stream: TcpStream,
    sender: Sender<ProxyRequest>,
) -> SResult<()> {
    let local_addr = to_ipv4_mapped(stream.local_addr().unwrap());

    let (s, req, socket) = handle_socks(users, stream, local_addr)
        .in_current_span()
        .await?;
    let req = match req.cmd {
        SOCKS5_CMD_TCP_CONNECT => {
            info!(dst = %req.dst, "tcp connect request accepted");
            let src_addr = s.peer_addr().ok().map(to_ipv4_mapped);
            ProxyRequest::Tcp(TcpSession {
                stream: Box::new(s) as Box<dyn crate::TcpTrait>,
                dst: req.dst,
                src_addr,
                user_context: Default::default(),
            })
        }
        SOCKS5_CMD_UDP_ASSOCIATE => {
            info!(bind_dst = %req.dst, "udp associate request accepted");
            let socket = Arc::new(socket.unwrap());
            let mut session = UdpSession::from_recv(
                Arc::new(UdpSocksWrap(socket.clone(), Default::default())),
                Box::new(UdpSocksWrap(socket, Default::default())),
                Some(Box::new(s)),
                req.dst,
                Default::default(),
            )
            .await?;
            session.src_addr = session.src_addr.map(to_ipv4_mapped);
            ProxyRequest::Udp(session)
        }
        _ => {
            return Err(SError::ProtocolViolation);
        }
    };
    sender
        .send(req)
        .await
        .map_err(|_| SError::ChannelError("socks request channel closed".into()))
}

#[async_trait]
impl Inbound for SocksServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        let mut recv = self
            .request_receiver
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        recv.user_context_mut().inbound_tag = self.cfg.tag.clone();
        recv.user_context_mut().preferred_outbound = self.cfg.default_outbound.clone();
        Ok(recv)
    }

    async fn init(&self) -> Result<(), SError> {
        let dual_stack = self.cfg.bind_addr.is_ipv6();
        let socket = Socket::new(
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
        socket.bind(&self.cfg.bind_addr.into())?;
        socket.listen(256)?;
        let listener = TcpListener::from_std(socket.into())
            .map_err(|e| SError::SocksError(format!("failed to create TcpListener: {e}")))?;

        let req_send = self.request_sender.clone();
        let users = Arc::new(self.cfg.users.clone());
        let tag = self.cfg.tag.clone();
        let fut = async move {
            loop {
                let (stream, addr) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        error!(%error, "failed to accept socks connection");
                        // Avoid spinning when resource exhaustion prevents accepting.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let span = tracing::info_span!("inbound",
                    tag = %tag,
                    src = %addr,
                    user = tracing::field::Empty,
                    id = tracing::field::Empty,
                );
                // The SOCKS5 replies are written field by field (src/msgs/socks5.rs),
                // so with Nagle enabled every field after the first waits for the peer
                // to acknowledge the previous one. A peer that delays that
                // acknowledgement — Linux does, by up to 40ms — makes every handshake
                // cost that much. Measured on loopback: ~41ms without this line,
                // ~0.4ms with it.
                let _ = stream.set_nodelay(true);
                span.in_scope(|| {
                    tracing::info!("accepted socks connection");
                });

                let users = users.clone();
                let req_send = req_send.clone();
                tokio::spawn(
                    async move {
                        handle_tcp(users, stream, req_send)
                            .await
                            .map_err(|x| error!("failed to handle socks connection: {}", x))
                    }
                    .instrument(span),
                );
            }
        };
        tokio::spawn(fut.in_current_span());

        Ok(())
    }
}
