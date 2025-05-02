use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    ops::Deref,
    sync::Arc,
};

use bytes::BytesMut;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::{TcpStream, UdpSocket, lookup_host},
    sync::Mutex,
};
use tracing::{Instrument, error, trace, trace_span};

use crate::{
    Outbound, UdpSession,
    error::SError,
    msgs::socks5::{AddrOrDomain, SOCKS5_ADDR_TYPE_DOMAIN_NAME, SocksAddr, VarVec},
};
use async_trait::async_trait;

pub struct DirectOut;

#[async_trait]
impl Outbound for DirectOut {
    async fn handle(
        &mut self,
        req: crate::ProxyRequest,
    ) -> anyhow::Result<(), crate::error::SError> {
        let fut = async {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    trace!("direct tcp to {}", tcp_session.dst);
                    let dst = tcp_session
                        .dst
                        .to_socket_addrs()?
                        .next()
                        .ok_or(SError::DomainResolveFailed)?;
                    trace!("resolved to {}", dst);
                    let mut upstream = TcpStream::connect(dst).await?;
                    let (_, _) = tokio::io::copy_bidirectional_with_sizes(
                        &mut tcp_session.stream,
                        &mut upstream,
                        1024 * 16,
                        1024 * 16,
                    )
                    .await?;
                }
                crate::ProxyRequest::Udp(udp_session) => {
                    handle_udp(udp_session).await?;
                }
            }
            Ok(()) as Result<(), SError>
        };
        let span = trace_span!("direct");
        tokio::spawn(
            async {
                let _ = fut.await.map_err(|x| error!("{}", x));
            }
            .instrument(span),
        );

        Ok(())
    }
}

#[derive(Default, Clone)]
struct DnsResolve(Arc<Mutex<HashMap<Vec<u8>, SocketAddr>>>);
impl DnsResolve {
    async fn resolve(&self, socks: SocksAddr, ipv4_only: bool) -> Result<SocketAddr, SError> {
        if let AddrOrDomain::Domain(x) = &socks.addr {
            if let Some(v) = self.0.lock().await.get(&x.contents) {
                Ok(*v)
            } else {
                let s = resolve(&socks, ipv4_only).await?;
                self.0.lock().await.insert(x.contents.clone(), s);
                Ok(s)
            }
        } else {
            Ok(resolve(&socks, ipv4_only).await?)
        }
    }
    async fn inv_resolve(&self, addr: &SocketAddr) -> SocksAddr {
        if let Some(add) = self.0.lock().await.iter().find(|x| x.1 == addr) {
            SocksAddr {
                atype: SOCKS5_ADDR_TYPE_DOMAIN_NAME,
                addr: AddrOrDomain::Domain(VarVec {
                    len: add.0.len() as u8,
                    contents: add.0.clone(),
                }),
                port: addr.port(),
            }
        } else {
            (*addr).into()
        }
    }
}

async fn resolve(socks: &SocksAddr, ipv4_only: bool) -> Result<SocketAddr, SError> {
    let mut s = match socks.addr.clone() {
        crate::msgs::socks5::AddrOrDomain::V4(x) => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::V6(x) => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::Domain(var_vec) => lookup_host((
            String::from_utf8(var_vec.contents).map_err(|_| SError::DomainResolveFailed)?,
            socks.port,
        ))
        .await?
        .find(|x| x.is_ipv4() || (!ipv4_only))
        .ok_or(SError::DomainResolveFailed)?,
    };
    s.set_port(socks.port);
    Ok(s)
}

async fn handle_udp(udp_session: UdpSession) -> Result<(), SError> {
    trace!("associating udp to {}", udp_session.dst);
    let dst = udp_session
        .dst
        .to_socket_addrs()?
        .next()
        .ok_or(SError::DomainResolveFailed)?;
    trace!("resolved to {}", dst);
    let ipv4_only = dst.is_ipv4();

    let socket = DualSocket::new_bind(dst, !ipv4_only)?;

    let upstream = Arc::new(socket);
    let upstream_clone = upstream.clone();
    let mut downstream = udp_session.recv;

    let dns_cache = DnsResolve::default();
    let dns_cache_clone = dns_cache.clone();
    let fut1 = async move {
        loop {
            let mut buf_send = BytesMut::new();
            buf_send.resize(2000, 0);
            //trace!("recv upstream");
            let (len, dst) = upstream.recv_from(&mut buf_send).await?;
            //trace!("udp request reply from:{}", dst);
            let dst = dns_cache_clone.inv_resolve(&dst).await;
            //trace!("udp source inverse resolved to:{}", dst);
            let buf = buf_send.freeze();
            //trace!("udp recved:{} bytes", len);
            let _ = udp_session.send.send_to(buf.slice(..len), dst).await?;
        }
        #[allow(unreachable_code)]
        (Ok(()) as Result<(), SError>)
    };
    let fut2 = async move {
        loop {
            let (buf, dst) = downstream.recv_from().await?;

            //trace!("udp request to:{}", dst);
            let dst = dns_cache.resolve(dst, ipv4_only).await?;
            //trace!("udp resolve to:{}", dst);
            let _siz = upstream_clone.send_to(&buf, &dst).await?;
            //trace!("udp request sent:{}bytes", siz);
        }
        #[allow(unreachable_code)]
        (Ok(()) as Result<(), SError>)
    };
    // We can use spawn, but it requirs communication to shutdown the other
    // Flatten spawn handle using try_join! doesn't work. Don't know why
    tokio::try_join!(fut1, fut2)?;
    Ok(())
}

/// A dual stack UDP socket. In linux dual stack is enabled by default for IPv6 socket,
/// and IPv4 address mapped from/to IPv6 address is done automatically.
/// In windows, Ipv4 mapping must be done manually.
struct DualSocket {
    inner: UdpSocket,
    dual_stack: bool,
}
impl DualSocket {
    fn new_bind(addr: SocketAddr, dual_stack: bool) -> io::Result<Self> {
        //let upstream = UdpSocket::bind(dst).await?;
        let socket = Socket::new(
            // Use socket2 for dualstack for windows compact
            if dual_stack {
                Domain::IPV6
            } else {
                Domain::IPV4
            },
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;
        if dual_stack {
            socket.set_only_v6(false)?;
            // socket.set_reuse_address(true)?;
        };
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;

        let socket = UdpSocket::from_std(socket.into())?;

        Ok(Self {
            inner: socket,
            dual_stack,
        })
    }
    async fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        let ip = match (self.dual_stack, addr.ip()) {
            (true, IpAddr::V4(ipv4_addr)) => IpAddr::V6(ipv4_addr.to_ipv6_mapped()),
            (_, ip) => ip,
        };
        self.inner.send_to(buf, (ip, addr.port())).await
    }
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (len, addr) = self.inner.recv_from(buf).await?;
        let ip = match (self.dual_stack, addr.ip()) {
            (true, ip_addr @ IpAddr::V6(ipv6_addr)) => ipv6_addr
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(ip_addr),
            (_, ip) => ip,
        };
        Ok((len, SocketAddr::new(ip, addr.port())))
    }
}

impl Deref for DualSocket {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
