use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::TproxyServerCfg;
use crate::error::SError;
use crate::msgs::socks5::{AddrOrDomain, SocksAddr};
use crate::{
    AnyTcp, AnyUdpRecv, AnyUdpSend, Inbound, ProxyRequest, TcpSession, UdpRecv, UdpSend, UdpSession,
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::{Receiver, channel};

use socket2::{Domain, Protocol, Socket, Type};

pub struct TproxyServer {
    _bind_addr: SocketAddr,
    tcp_listener: TcpListener,
    udp_req_rx: Receiver<ProxyRequest>,
}

impl TproxyServer {
    pub async fn new(cfg: TproxyServerCfg) -> Result<Self, SError> {
        let tcp_listener = Self::create_tcp_listener(cfg.bind_addr)?;

        let (udp_req_tx, udp_req_rx) = channel(1024);

        {
            let bind_addr = cfg.bind_addr;
            tokio::spawn(async move {
                if let Err(e) = handle_udp_tproxy(bind_addr, udp_req_tx).await {
                    tracing::error!("tproxy udp listener failed: {}", e);
                }
            });
        }

        Ok(Self {
            _bind_addr: cfg.bind_addr,
            tcp_listener,
            udp_req_rx,
        })
    }

    fn create_tcp_listener(addr: SocketAddr) -> Result<TcpListener, SError> {
        let dual_stack = addr.is_ipv6();
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

        {
            if addr.is_ipv4() || dual_stack {
                let _ = socket.set_ip_transparent_v4(true);
            }
            if addr.is_ipv6() {
                let _ = set_ip_transparent_v6(&socket);
            }
        }

        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;

        TcpListener::from_std(socket.into())
            .map_err(|e| SError::SocksError(format!("failed to create TcpListener: {e}")))
    }
}

#[async_trait]
impl Inbound for TproxyServer {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        tokio::select! {
            res = self.tcp_listener.accept() => {
                let (stream, _) = res?;
                tracing::info!("accepted tcp connection from {}", stream.peer_addr().unwrap());
                let orig_dst = stream.local_addr().map_err(|e| SError::SocksError(e.to_string()))?;
                let dst = SocksAddr {
                    addr: match orig_dst.ip() {
                        std::net::IpAddr::V4(v4) => AddrOrDomain::V4(v4.octets()),
                        std::net::IpAddr::V6(v6) => AddrOrDomain::V6(v6.octets()),
                    },
                    port: orig_dst.port(),
                };
                Ok(ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(stream),
                    dst,
                    user_context: None,
                }))
            }
            Some(req) = self.udp_req_rx.recv() => {
                Ok(req)
            }
        }
    }
}

pub struct TproxyUdpSend {
    client_addr: SocketAddr,
    v4_socket: Arc<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    v6_socket: Arc<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
}

#[async_trait]
impl UdpSend for TproxyUdpSend {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        use etherparse::PacketBuilder;
        use std::future::poll_fn;
        use std::os::fd::AsRawFd;
        use std::task::Poll;

        let src_addr = match addr.addr {
            AddrOrDomain::V4(v4) => {
                SocketAddr::V4(std::net::SocketAddrV4::new(v4.into(), addr.port))
            }
            AddrOrDomain::V6(v6) => {
                SocketAddr::V6(std::net::SocketAddrV6::new(v6.into(), addr.port, 0, 0))
            }
            AddrOrDomain::Domain(_) => {
                return Err(SError::SocksError(
                    "Cannot send to domain from tproxy".into(),
                ));
            }
        };

        let mut packet = Vec::new();
        let builder = match (src_addr, self.client_addr) {
            (SocketAddr::V4(src), SocketAddr::V4(dst)) => {
                PacketBuilder::ipv4(src.ip().octets(), dst.ip().octets(), 64)
                    .udp(src.port(), dst.port())
            }
            (SocketAddr::V6(src), SocketAddr::V6(dst)) => {
                PacketBuilder::ipv6(src.ip().octets(), dst.ip().octets(), 64)
                    .udp(src.port(), dst.port())
            }
            _ => return Err(SError::SocksError("Address family mismatch".into())),
        };

        let packet_size = builder.size(buf.len());
        packet.reserve(packet_size);
        builder
            .write(&mut packet, &buf)
            .map_err(|e| SError::SocksError(e.to_string()))?;

        let afd = if self.client_addr.is_ipv4() {
            &self.v4_socket
        } else {
            &self.v6_socket
        };

        poll_fn(|cx| {
            let mut guard = match std::task::ready!(afd.poll_write_ready(cx)) {
                Ok(g) => g,
                Err(e) => return Poll::Ready(Err(SError::SocksError(e.to_string()))),
            };
            let dest_sockaddr = if self.client_addr.is_ipv6() {
                socket2::SockAddr::from(SocketAddr::new(self.client_addr.ip(), 0))
            } else {
                socket2::SockAddr::from(self.client_addr)
            };

            let errno = unsafe {
                libc::sendto(
                    afd.as_raw_fd(),
                    packet.as_ptr() as *const _,
                    packet.len(),
                    0,
                    dest_sockaddr.as_ptr() as *const _,
                    dest_sockaddr.len(),
                )
            };

            if errno < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    return Poll::Pending;
                }
                return Poll::Ready(Err(SError::SocksError(err.to_string())));
            }
            Poll::Ready(Ok(buf.len()))
        })
        .await
    }
}

async fn handle_udp_tproxy(
    bind_addr: SocketAddr,
    req_tx: Sender<ProxyRequest>,
) -> Result<(), SError> {
    use std::collections::HashMap;
    use unix_udp_sock::UdpSocket as UnixUdpSocket;

    let dual_stack = bind_addr.is_ipv6();
    let socket = Socket::new(
        if dual_stack {
            Domain::IPV6
        } else {
            Domain::IPV4
        },
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;

    if dual_stack {
        let _ = socket.set_only_v6(false);
    }
    socket.set_reuse_address(true)?;

    if bind_addr.is_ipv4() || dual_stack {
        let _ = socket.set_ip_transparent_v4(true);
    }
    if bind_addr.is_ipv6() {
        let _ = set_ip_transparent_v6(&socket);
    }
    socket.set_nonblocking(true)?;
    socket.set_broadcast(true)?;

    let _ = set_socket_option(&socket, libc::IPPROTO_IP, libc::IP_RECVORIGDSTADDR, 1);
    if dual_stack {
        let _ = set_socket_option(&socket, libc::IPPROTO_IPV6, libc::IPV6_RECVORIGDSTADDR, 1);
    }

    socket.bind(&bind_addr.into())?;

    let listener = Arc::new(
        UnixUdpSocket::from_std(socket.into()).map_err(|e| SError::SocksError(e.to_string()))?,
    );

    let v4_raw = new_unbound_socket(SocketAddr::from(([0, 0, 0, 0], 0)))?;
    let v6_raw = new_unbound_socket(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)))?;

    let v4_socket = Arc::new(
        tokio::io::unix::AsyncFd::new(v4_raw.into())
            .map_err(|e| SError::SocksError(e.to_string()))?,
    );
    let v6_socket = Arc::new(
        tokio::io::unix::AsyncFd::new(v6_raw.into())
            .map_err(|e| SError::SocksError(e.to_string()))?,
    );

    let mut sessions: HashMap<SocketAddr, Sender<(Bytes, SocksAddr)>> = HashMap::new();
    let mut buf = vec![0u8; 65536];

    loop {
        match listener.recv_msg(&mut buf).await {
            Ok(meta) => {
                let orig_dst = match meta.orig_dst {
                    Some(d) => d.to_canonical(),
                    None => continue,
                };

                let client_addr = meta.addr.to_canonical();

                sessions.retain(|_, tx| !tx.is_closed());

                let tx = if let Some(tx) = sessions.get(&client_addr) {
                    tx.clone()
                } else {
                    tracing::info!("accepted udp connection from {}", client_addr);
                    let (tx, rx) = channel(1024);
                    let send = Arc::new(TproxyUdpSend {
                        client_addr,
                        v4_socket: v4_socket.clone(),
                        v6_socket: v6_socket.clone(),
                    });

                    let req: ProxyRequest<AnyTcp, AnyUdpRecv, AnyUdpSend> =
                        ProxyRequest::Udp(UdpSession {
                            send: send as Arc<dyn UdpSend>,
                            recv: Box::new(rx) as Box<dyn UdpRecv>,
                            stream: None,
                            bind_addr: SocksAddr {
                                addr: match orig_dst.ip() {
                                    std::net::IpAddr::V4(_) => AddrOrDomain::V4([0, 0, 0, 0]),
                                    std::net::IpAddr::V6(_) => AddrOrDomain::V6([0u8; 16]),
                                },
                                port: 0,
                            },
                            user_context: None,
                        });

                    if req_tx.send(req).await.is_err() {
                        break;
                    }
                    sessions.insert(client_addr, tx.clone());
                    tx
                };

                let data = Bytes::copy_from_slice(&buf[0..meta.len]);
                let dst_socks = SocksAddr {
                    addr: match orig_dst.ip() {
                        std::net::IpAddr::V4(v4) => AddrOrDomain::V4(v4.octets()),
                        std::net::IpAddr::V6(v6) => AddrOrDomain::V6(v6.octets()),
                    },
                    port: orig_dst.port(),
                };
                let _ = tx.send((data, dst_socks)).await;
            }
            Err(_) => {
                continue;
            }
        }
    }
    Ok(())
}

fn new_unbound_socket(family_hint: SocketAddr) -> Result<socket2::Socket, SError> {
    let socket = if family_hint.is_ipv4() {
        socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::RAW,
            Some(libc::IPPROTO_RAW.into()),
        )
    } else {
        socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::RAW,
            Some(libc::IPPROTO_RAW.into()),
        )
    }
    .map_err(|e| SError::SocksError(e.to_string()))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| SError::SocksError(e.to_string()))?;
    Ok(socket)
}

fn set_ip_transparent_v6(socket: &Socket) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    let opt = libc::IPV6_TRANSPARENT;
    let level = libc::IPPROTO_IPV6;
    let enable: libc::c_int = 1;

    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &enable as *const _ as *const _,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        )
    };

    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_socket_option(socket: &Socket, level: i32, opt: i32, val: i32) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    let enable: libc::c_int = val;

    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &enable as *const _ as *const _,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        )
    };

    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

trait ToCanonical {
    fn to_canonical(&self) -> Self;
}

impl ToCanonical for SocketAddr {
    fn to_canonical(&self) -> Self {
        match self {
            SocketAddr::V4(_) => *self,
            SocketAddr::V6(addr) => {
                if let Some(v4) = addr.ip().to_ipv4_mapped() {
                    SocketAddr::new(std::net::IpAddr::V4(v4), addr.port())
                } else {
                    *self
                }
            }
        }
    }
}
