//! Present several bound UDP sockets to one noq endpoint. Only the local IP in
//! noq's metadata is virtual. The peer address, payload and QUIC IDs stay intact.
use std::{
    collections::HashMap,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    num::NonZeroUsize,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};

use crate::utils::dual_socket::ToCanonical;
use noq::{
    AsyncUdpSocket, Runtime, UdpSender,
    udp::{RecvMeta, Transmit},
};

/// Aliases are private to this endpoint and never passed to the OS.
const ALIAS_PREFIX: u128 = 0xfd53_5155_4943_0000u128 << 64;
// Encode IPv4 pktinfo directly. A wildcard bind can receive on entire ranges
// of local IPv4 addresses (for example 127/8), without retaining one entry per IP.
const IPV4_ALIAS_PREFIX: u128 = ALIAS_PREFIX | (1 << 64);

pub(super) fn socket_alias(index: usize) -> IpAddr {
    Ipv6Addr::from(ALIAS_PREFIX | index as u128).into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Route {
    socket: usize,
    source: Option<IpAddr>,
}

#[derive(Debug)]
struct Bindings {
    addrs: Vec<SocketAddr>,
    // The first N aliases select a socket and allow the kernel to choose
    // its source IP. Further aliases preserve pktinfo on wildcard listeners.
    wildcard_routes: RwLock<Vec<Route>>,
}

impl Bindings {
    fn route(&self, src: Option<IpAddr>) -> io::Result<Route> {
        let index = match src {
            None => 0, // The first client Initial has no source IP yet.
            Some(IpAddr::V6(ip)) if u128::from(ip) >> 64 == IPV4_ALIAS_PREFIX >> 64 => {
                let bits = u128::from(ip) as u64;
                let socket = (bits >> 32) as usize;
                if socket >= self.addrs.len() {
                    return Err(io::Error::other("unknown SunnyQUIC socket alias"));
                }
                return Ok(Route {
                    socket,
                    source: Some(Ipv4Addr::from(bits as u32).into()),
                });
            }
            Some(IpAddr::V6(ip)) if u128::from(ip) >> 64 == ALIAS_PREFIX >> 64 => {
                usize::try_from(u128::from(ip) as u64)
                    .map_err(|_| io::Error::other("invalid SunnyQUIC socket alias"))?
            }
            Some(_) => return Err(io::Error::other("invalid SunnyQUIC socket alias")),
        };
        if index < self.addrs.len() {
            return Ok(Route {
                socket: index,
                source: None,
            });
        }
        self.wildcard_routes
            .read()
            .unwrap()
            .get(index - self.addrs.len())
            .copied()
            .ok_or_else(|| io::Error::other("unknown SunnyQUIC socket alias"))
    }
}

// noq normalizes all peer addresses to the endpoint's family. Adapt them again
// at the kernel boundary, including IPv4-mapped addresses for dual-stack sockets.
fn address_family(addr: SocketAddr, ipv6: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if ipv6 => SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port()),
        _ if !ipv6 => addr.to_canonical(),
        _ => addr,
    }
}

#[derive(Debug)]
pub(super) struct MultiSocket {
    sockets: Vec<Box<dyn AsyncUdpSocket>>,
    bindings: Arc<Bindings>,
    ipv6: bool,
    next_recv: usize,
    wildcard_aliases: HashMap<Route, IpAddr>,
}

impl MultiSocket {
    pub(super) fn new(sockets: Vec<UdpSocket>, runtime: &dyn Runtime) -> io::Result<Self> {
        let addrs = sockets
            .iter()
            .map(UdpSocket::local_addr)
            .collect::<io::Result<Vec<_>>>()?;
        let ipv6 = addrs.iter().any(SocketAddr::is_ipv6);
        let sockets = sockets
            .into_iter()
            .map(|socket| runtime.wrap_udp_socket(socket))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            sockets,
            bindings: Arc::new(Bindings {
                addrs,
                wildcard_routes: RwLock::new(Vec::new()),
            }),
            ipv6,
            next_recv: 0,
            wildcard_aliases: HashMap::new(),
        })
    }

    fn received_alias(&mut self, socket: usize, dst_ip: Option<IpAddr>) -> IpAddr {
        if !self.bindings.addrs[socket].ip().is_unspecified() || dst_ip.is_none() {
            return socket_alias(socket);
        }
        if let Some(IpAddr::V4(ip)) = dst_ip {
            let bits = IPV4_ALIAS_PREFIX | ((socket as u128) << 32) | u128::from(u32::from(ip));
            return Ipv6Addr::from(bits).into();
        }
        let route = Route {
            socket,
            source: dst_ip,
        };
        *self.wildcard_aliases.entry(route).or_insert_with(|| {
            let mut routes = self.bindings.wildcard_routes.write().unwrap();
            let alias = socket_alias(self.bindings.addrs.len() + routes.len());
            routes.push(route);
            alias
        })
    }
}

impl AsyncUdpSocket for MultiSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(MultiSender {
            senders: self.sockets.iter().map(|s| s.create_sender()).collect(),
            bindings: self.bindings.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        metas: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut last_error = None;
        let mut pending = false;
        for _ in 0..self.sockets.len() {
            let i = self.next_recv;
            self.next_recv = (i + 1) % self.sockets.len();
            match self.sockets[i].poll_recv(cx, bufs, metas) {
                Poll::Pending => pending = true,
                Poll::Ready(Err(err)) => last_error = Some(err),
                Poll::Ready(Ok(n)) => {
                    for meta in &mut metas[..n] {
                        meta.addr = address_family(meta.addr, self.ipv6);
                        meta.dst_ip = Some(self.received_alias(i, meta.dst_ip));
                    }
                    return Poll::Ready(Ok(n));
                }
            }
        }
        if !pending && let Some(err) = last_error {
            return Poll::Ready(Err(err));
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(address_family(self.bindings.addrs[0], self.ipv6))
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        self.sockets
            .iter()
            .map(|s| s.max_receive_segments())
            .max()
            .unwrap()
    }

    fn may_fragment(&self) -> bool {
        self.sockets.iter().any(|s| s.may_fragment())
    }
}

#[derive(Debug)]
struct MultiSender {
    senders: Vec<Pin<Box<dyn UdpSender>>>,
    bindings: Arc<Bindings>,
}

impl UdpSender for MultiSender {
    fn poll_send(
        mut self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let route = self.bindings.route(transmit.src_ip)?;
        let transmit = Transmit {
            destination: address_family(
                transmit.destination,
                self.bindings.addrs[route.socket].is_ipv6(),
            ),
            src_ip: route.source,
            ..transmit.clone()
        };
        self.senders[route.socket].as_mut().poll_send(&transmit, cx)
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        self.senders
            .iter()
            .map(|s| s.max_transmit_segments())
            .min()
            .unwrap()
    }
}
