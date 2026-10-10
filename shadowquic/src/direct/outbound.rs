use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use bytes::BytesMut;
use tokio::{
    net::{TcpSocket, TcpStream, lookup_host},
    sync::Mutex,
};
use tracing::{Instrument, error, info_span, trace};

use crate::{
    Outbound, UdpSession,
    config::{DirectOutCfg, DnsStrategy, Interface},
    dns::{DnsService, ResolverManager},
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
    utils::{
        dual_socket::DualSocket,
        socket_opt::{SocketFactory, TcpSocketFactory, UdpSocketFactory},
    },
};
use async_trait::async_trait;

#[derive(Clone)]
pub struct DirectOut {
    pub cfg: DirectOutCfg,
    pub(crate) addr_resolver: Arc<dyn DnsService>,
}

impl std::fmt::Debug for DirectOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectOut").field("cfg", &self.cfg).finish()
    }
}

impl Default for DirectOut {
    fn default() -> Self {
        Self::new(DirectOutCfg::default(), Arc::new(ResolverManager::new()))
    }
}

#[async_trait]
impl Outbound for DirectOut {
    async fn handle(&self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        let dns_strategy = self.cfg.dns_strategy.clone();
        let self_clone = self.clone();

        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    trace!("direct tcp to {}", tcp_session.dst);
                    let dst = tcp_session.dst.to_socket_addrs()?;
                    let dst = apply_dns_strategy(dst, &dns_strategy)
                        .ok_or(SError::DomainResolveFailed(tcp_session.dst.to_string()))?;
                    trace!("resolved to {}", dst);

                    let mut upstream = self_clone.connect_tcp(dst).await?;
                    let _ = upstream.set_nodelay(true);
                    let (_, _) = crate::utils::copy::copy_bidirectional(
                        &mut tcp_session.stream,
                        &mut upstream,
                        1024 * 16,
                        1024 * 16,
                        self_clone.cfg.half_close_timeout,
                    )
                    .await?;
                }

                crate::ProxyRequest::Udp(udp_session) => {
                    self_clone.handle_udp(udp_session).await?;
                }
            }

            Ok(()) as Result<(), SError>
        };
        let span = info_span!("direct");
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
struct DnsResolve(Arc<Mutex<HashMap<SocksAddr, SocketAddr>>>);
impl DnsResolve {
    async fn resolve(
        &self,
        socks: SocksAddr,
        strategy: &DnsStrategy,
    ) -> Result<SocketAddr, SError> {
        if let AddrOrDomain::Domain(_) = &socks.addr {
            if let Some(v) = self.0.lock().await.get(&socks) {
                Ok(*v)
            } else {
                let s = resolve(&socks, strategy).await?;
                self.0.lock().await.insert(socks, s);
                Ok(s)
            }
        } else {
            Ok(resolve(&socks, strategy).await?)
        }
    }
    async fn inv_resolve(&self, addr: &SocketAddr) -> SocksAddr {
        if let Some(add) = self.0.lock().await.iter().find(|x| x.1 == addr) {
            add.0.clone()
        } else {
            (*addr).into()
        }
    }
}

async fn resolve(socks: &SocksAddr, strategy: &DnsStrategy) -> Result<SocketAddr, SError> {
    let mut s = match socks.addr.clone() {
        crate::msgs::socks5::AddrOrDomain::V4(x) => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::V6(x) => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::Domain(var_vec) => {
            let ip_list = lookup_host((
                String::from_utf8(var_vec.contents)
                    .map_err(|_| SError::DomainResolveFailed(socks.to_string()))?,
                socks.port,
            ))
            .await?;
            apply_dns_strategy(ip_list, strategy)
                .ok_or(SError::DomainResolveFailed(socks.to_string()))?
        }
    };
    s.set_port(socks.port);
    Ok(s)
}

impl DirectOut {
    pub fn new(cfg: DirectOutCfg, resolver_manager: Arc<ResolverManager>) -> Self {
        #[cfg(feature = "dns-server")]
        let tag = crate::dns::DEFAULT_SYSTEM_DNS_TAG;
        #[cfg(not(feature = "dns-server"))]
        let tag = "";
        let addr_resolver = resolver_manager
            .resolver(tag)
            .expect("default system resolver exists");
        Self { cfg, addr_resolver }
    }

    async fn connect_tcp(&self, dst: SocketAddr) -> Result<TcpStream, SError> {
        let socket = TcpSocketFactory {
            addr: dst.to_string(),
            interface: self.cfg.socket_opt.bind_interface.clone(),
            fw_mark: self.cfg.socket_opt.fw_mark,
            protect_path: None,
            resolver: self.addr_resolver.clone(),
        }
        .create_socket()
        .await?;
        let socket = TcpSocket::from_std_stream(socket.into());
        Ok(socket.connect(dst).await?)
    }

    async fn bind_udp(&self, bind_addr: SocketAddr, dst: SocksAddr) -> Result<DualSocket, SError> {
        let interface = match self.cfg.socket_opt.bind_interface.clone() {
            Some(iface) => Some(iface),
            None if !bind_addr.ip().is_unspecified() => Some(Interface::Address(bind_addr)),
            _ => None,
        };
        let socket = UdpSocketFactory {
            addr: dst.to_string(),
            interface,
            fw_mark: self.cfg.socket_opt.fw_mark,
            protect_path: None,
            try_dual_stack: bind_addr.ip().is_unspecified(),
            resolver: self.addr_resolver.clone(),
        }
        .create_socket()
        .await?;
        Ok(DualSocket::from_socket(socket)?)
    }

    async fn handle_udp(&self, udp_session: UdpSession) -> Result<(), SError> {
        trace!(bind_addr = %udp_session.bind_addr,"associating udp");
        let bind_addr =
            udp_session
                .bind_addr
                .to_socket_addrs()?
                .next()
                .ok_or(SError::DomainResolveFailed(
                    udp_session.bind_addr.to_string(),
                ))?;
        let socket = self.bind_udp(bind_addr, udp_session.dst).await?;
        let upstream = Arc::new(socket);
        let upstream_clone = upstream.clone();
        let mut downstream = udp_session.recv;

        let dns_cache = DnsResolve::default();
        let dns_cache_clone = dns_cache.clone();
        let dns_strategy = self.cfg.dns_strategy.clone();
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
                let dst = dns_cache.resolve(dst, &dns_strategy).await?;
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
}
pub(crate) fn apply_dns_strategy<It>(mut ip_list: It, strategy: &DnsStrategy) -> Option<SocketAddr>
where
    It: Iterator<Item = SocketAddr>,
{
    match strategy {
        DnsStrategy::Ipv4Only => ip_list.find(|addr| addr.is_ipv4()),
        DnsStrategy::Ipv6Only => ip_list.find(|addr| addr.is_ipv6()),
        DnsStrategy::PreferIpv4 => {
            let mut first = None;
            for ip in ip_list {
                if ip.is_ipv4() {
                    return Some(ip);
                }
                if first.is_none() {
                    first = Some(ip);
                }
            }
            first
        }
        DnsStrategy::PreferIpv6 => {
            let mut first = None;
            for ip in ip_list {
                if ip.is_ipv6() {
                    return Some(ip);
                }
                if first.is_none() {
                    first = Some(ip);
                }
            }
            first
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn udp_domain_cache_preserves_each_destination_port() {
        let cache = DnsResolve::default();
        for port in [1234, 5678, 1234] {
            let domain = SocksAddr::from_domain("localhost".into(), port);
            let resolved = cache
                .resolve(domain.clone(), &DnsStrategy::Ipv4Only)
                .await
                .unwrap();
            assert_eq!(resolved.port(), port);
            assert_eq!(cache.inv_resolve(&resolved).await, domain);
        }
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn tcp_and_udp_use_configured_source_address() {
        use crate::config::{Interface, SocketOpt};
        use tokio::net::{TcpListener, UdpSocket};
        use tokio::time::{Duration, timeout};

        let source: IpAddr = "127.0.0.2".parse().unwrap();
        let direct = DirectOut::new(
            DirectOutCfg {
                socket_opt: SocketOpt {
                    bind_interface: Some(Interface::Address(SocketAddr::new(source, 0))),
                    ..Default::default()
                },
                ..Default::default()
            },
            Arc::new(ResolverManager::new()),
        );
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stream = direct
                .connect_tcp(listener.local_addr().unwrap())
                .await
                .unwrap();
            assert_eq!(stream.local_addr().unwrap().ip(), source);
            let (_, peer) = listener.accept().await.unwrap();
            assert_eq!(peer.ip(), source);

            let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let socket = direct
                .bind_udp(
                    "0.0.0.0:0".parse().unwrap(),
                    SocksAddr::from(receiver.local_addr().unwrap()),
                )
                .await
                .unwrap();
            assert_eq!(socket.local_addr().unwrap().ip(), source);
            socket
                .send_to(b"ping", &receiver.local_addr().unwrap())
                .await
                .unwrap();
            let mut buf = [0; 4];
            let (_, peer) = receiver.recv_from(&mut buf).await.unwrap();
            assert_eq!(peer.ip(), source);
            receiver.send_to(b"pong", peer).await.unwrap();
            let (_, peer) = socket.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf, b"pong");
            assert_eq!(peer, receiver.local_addr().unwrap());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn default_udp_socket_maps_ipv4_replies() {
        use tokio::net::UdpSocket;
        use tokio::time::{Duration, timeout};

        timeout(Duration::from_secs(5), async {
            let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let socket = DirectOut::default()
                .bind_udp(
                    "0.0.0.0:0".parse().unwrap(),
                    SocksAddr::from(receiver.local_addr().unwrap()),
                )
                .await
                .unwrap();
            socket
                .send_to(b"ping", &receiver.local_addr().unwrap())
                .await
                .unwrap();
            let mut buf = [0; 4];
            let (_, peer) = receiver.recv_from(&mut buf).await.unwrap();
            receiver.send_to(b"pong", peer).await.unwrap();
            let (_, peer) = socket.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf, b"pong");
            assert_eq!(peer, receiver.local_addr().unwrap());
        })
        .await
        .unwrap();
    }

    fn make_addrs() -> Vec<SocketAddr> {
        vec![
            // 127.0.0.1:8080 (IPv4)
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            // ::1:8080 (IPv6)
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080),
        ]
    }

    #[test]
    fn test_apply_dns_strategy_ipv4_only() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::Ipv4Only);
        assert_eq!(result, Some(addrs[0]));
    }

    #[test]
    fn test_apply_dns_strategy_ipv6_only() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::Ipv6Only);
        assert_eq!(result, Some(addrs[1]));
    }

    #[test]
    fn test_apply_dns_strategy_prefer_ipv4() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::PreferIpv4);
        assert_eq!(result, Some(addrs[0]));
    }

    #[test]
    fn test_apply_dns_strategy_prefer_ipv6() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::PreferIpv6);
        assert_eq!(result, Some(addrs[1]));
    }

    #[test]
    fn test_apply_dns_strategy_empty() {
        let addrs: Vec<SocketAddr> = vec![];
        let result = apply_dns_strategy(addrs.into_iter(), &DnsStrategy::PreferIpv4);
        assert_eq!(result, None);
    }
}
