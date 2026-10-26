use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use bytes::BytesMut;
use tokio::{
    io::AsyncReadExt,
    net::{TcpSocket, TcpStream},
    sync::Mutex,
};
use tracing::trace;

use crate::{
    Outbound, UdpSession,
    config::{DirectOutCfg, DnsStrategy, Interface},
    dns::{DnsService, ResolverManager, resolve_destination},
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
    utils::{
        dual_socket::DualSocket,
        socket_opt::{SocketFactory, TcpSocketFactory, UdpSocketFactory},
    },
};
use async_trait::async_trait;

#[cfg(all(test, feature = "shadowquic-quinn"))]
#[path = "udp_failure_tests.rs"]
mod udp_failure_tests;

#[cfg(all(
    test,
    target_os = "linux",
    feature = "tproxy",
    feature = "shadowquic-quinn",
    feature = "dns-server",
    feature = "statistics"
))]
#[path = "tproxy_udp_failure_tests.rs"]
mod tproxy_udp_failure_tests;

#[derive(Clone)]
pub struct DirectOut {
    pub cfg: DirectOutCfg,
    pub(crate) dns: Arc<dyn DnsService>,
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

        match req {
            crate::ProxyRequest::Tcp(mut tcp_session) => {
                let dns = self.dns.clone();
                let fut = async move {
                    trace!("direct tcp to {}", tcp_session.dst);
                    // If dns field is resolved, Direct outbound will be wrapped with ResolvingOutbound.
                    // Here is mainly for backward compatibilty if dns-servers feature is disabled.
                    let dst =
                        resolve_destination(&*dns, &tcp_session.dst, Some(&dns_strategy)).await?;
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
                    Ok(()) as Result<(), SError>
                };
                fut.await?;
            }

            crate::ProxyRequest::Udp(udp_session) => {
                let fut = async move {
                    self_clone.handle_udp(udp_session).await?;
                    Ok(()) as Result<(), SError>
                };
                fut.await?;
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
struct DnsResolve(
    Arc<Mutex<HashMap<SocksAddr, SocketAddr>>>,
    Arc<dyn DnsService>,
);
impl DnsResolve {
    async fn resolve(
        &self,
        socks: &SocksAddr,
        strategy: &DnsStrategy,
    ) -> Result<SocketAddr, SError> {
        if let AddrOrDomain::Domain(_) = socks.addr {
            if let Some(v) = self.0.lock().await.get(socks) {
                Ok(*v)
            } else {
                let s = resolve_destination(&*self.1, socks, Some(strategy)).await?;
                tracing::trace!("{} resolved to {}", socks, s);
                self.0.lock().await.insert(socks.clone(), s);
                Ok(s)
            }
        } else {
            Ok(resolve_destination(&*self.1, socks, Some(strategy)).await?)
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

impl DirectOut {
    pub fn new(cfg: DirectOutCfg, resolver_manager: Arc<ResolverManager>) -> Self {
        #[cfg(feature = "dns-server")]
        let tag = cfg
            .dns
            .as_deref()
            .unwrap_or(crate::dns::DEFAULT_SYSTEM_DNS_TAG);
        #[cfg(not(feature = "dns-server"))]
        let tag = "";
        let dns = resolver_manager
            .resolver(tag)
            .expect("default system resolver exists");
        Self { cfg, dns }
    }

    async fn connect_tcp(&self, dst: SocketAddr) -> Result<TcpStream, SError> {
        let socket = TcpSocketFactory {
            addr: dst.to_string(),
            keep_alive: self.cfg.tcp_keep_alive,
            interface: self.cfg.socket_opt.bind_interface.clone(),
            fw_mark: self.cfg.socket_opt.fw_mark,
            protect_path: None,
            resolver: self.dns.clone(),
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
            resolver: self.dns.clone(),
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

        let dns_cache = DnsResolve(Default::default(), self.dns.clone());
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
                let dst = dns_cache.resolve(&dst, &dns_strategy).await?;
                //trace!("udp resolve to:{}", dst);
                let _siz = upstream_clone.send_to(&buf, &dst).await?;
                //trace!("udp request sent:{}bytes", siz);
            }
            #[allow(unreachable_code)]
            (Ok(()) as Result<(), SError>)
        };
        let fut3 = async {
            let Some(mut s) = udp_session.stream else {
                return Ok(());
            };
            let mut buf = [0u8];
            s.read_exact(&mut buf).await?;
            Err(SError::UDPSessionClosed(
                "udp session closed by control stream".into(),
            ))
        };
        // We can use spawn, but it requirs communication to shutdown the other
        // Flatten spawn handle using try_join! doesn't work. Don't know why
        tokio::try_join!(fut1, fut2, fut3)?;
        Ok(())
    }
}
#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn tcp_keep_alive_defaults_to_disabled_and_applies_to_connected_sockets() {
        assert!(!DirectOutCfg::default().tcp_keep_alive);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        for (field, expected) in [
            ("", false),
            ("tcp-keep-alive: false", false),
            ("tcp-keep-alive: true", true),
        ] {
            let crate::config::OutboundCfg::Direct(cfg) =
                serde_saphyr::from_str(&format!("type: direct\ntag: direct\n{field}")).unwrap()
            else {
                panic!("expected direct outbound")
            };
            assert_eq!(cfg.tcp_keep_alive, expected);
            let direct = DirectOut::new(cfg, Arc::new(ResolverManager::new()));
            let stream = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                direct.connect_tcp(listener.local_addr().unwrap()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                socket2::SockRef::from(&stream).keepalive().unwrap(),
                expected
            );
            listener.accept().await.unwrap();
        }
    }

    #[tokio::test]
    async fn udp_domain_cache_preserves_each_destination_port() {
        let cache = DnsResolve(Default::default(), DirectOut::default().dns);
        for port in [1234, 5678, 1234] {
            let domain = SocksAddr::from_domain("localhost".into(), port);
            let resolved = cache
                .resolve(&domain, &DnsStrategy::Ipv4Only)
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
        use std::net::IpAddr;
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
}
