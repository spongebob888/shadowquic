#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
use super::FakeIp;
use super::{DnsService, Result, dns_error};
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
use crate::Inbound;
use crate::{
    AnyUdpRecv, AnyUdpSend, Outbound, ProxyRequest, UdpRecv, UdpSend,
    config::DnsStrategy,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

/// Resolve after routing and preserve the original address on UDP replies.
pub(crate) struct ResolvingOutbound {
    pub inner: Arc<dyn Outbound>,
    pub resolver: Arc<dyn DnsService>,
    pub strategy: DnsStrategy,
}

#[async_trait]
impl Outbound for ResolvingOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<()> {
        let inner = self.inner.clone();
        let resolver = self.resolver.clone();
        let strategy = self.strategy.clone();
        // The manager must remain free to route this resolver's upstream sessions.
        tokio::spawn(async move {
            let result = async {
                let req = match req {
                    ProxyRequest::Tcp(mut session) => {
                        session.dst = resolve(&*resolver, &session.dst, &strategy).await?;
                        ProxyRequest::Tcp(session)
                    }
                    ProxyRequest::Udp(mut session) => {
                        let mapping = Arc::new(Mutex::new(HashMap::new()));
                        session.recv = Box::new(ResolvingRecv {
                            inner: session.recv,
                            resolver,
                            strategy,
                            mapping: mapping.clone(),
                        });
                        session.send = Arc::new(MappedSend {
                            inner: session.send,
                            mapping,
                        });
                        ProxyRequest::Udp(session)
                    }
                };
                inner.handle(req).await
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "outbound DNS resolution failed");
            }
        });
        Ok(())
    }
}

async fn resolve(
    resolver: &dyn DnsService,
    addr: &SocksAddr,
    strategy: &DnsStrategy,
) -> Result<SocksAddr> {
    let AddrOrDomain::Domain(domain) = &addr.addr else {
        return Ok(addr.clone());
    };
    let domain = std::str::from_utf8(&domain.contents).map_err(dns_error)?;
    let ips = resolver.resolve(domain).await?;
    crate::direct::outbound::apply_dns_strategy(
        ips.into_iter().map(|ip| SocketAddr::new(ip, addr.port)),
        strategy,
    )
    .map(Into::into)
    .ok_or_else(|| dns_error(format!("no address matching DNS strategy for {domain}")))
}

type Mapping = Arc<Mutex<HashMap<SocksAddr, SocksAddr>>>;
struct ResolvingRecv {
    inner: AnyUdpRecv,
    resolver: Arc<dyn DnsService>,
    strategy: DnsStrategy,
    mapping: Mapping,
}
#[async_trait]
impl UdpRecv for ResolvingRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr)> {
        let (bytes, original) = self.inner.recv_from().await?;
        let resolved = resolve(&*self.resolver, &original, &self.strategy).await?;
        remember(&self.mapping, &resolved, original)?;
        Ok((bytes, resolved))
    }
}
fn remember(mapping: &Mapping, translated: &SocksAddr, original: SocksAddr) -> Result<()> {
    let mut mapping = mapping.lock().unwrap();
    let key = translated.clone();
    if mapping.len() >= 4096 && !mapping.contains_key(&key) {
        return Err(dns_error("too many UDP destinations"));
    }
    mapping.insert(key, original);
    Ok(())
}
struct MappedSend {
    inner: AnyUdpSend,
    mapping: Mapping,
}
#[async_trait]
impl UdpSend for MappedSend {
    async fn send_to(&self, bytes: Bytes, addr: SocksAddr) -> Result<usize> {
        let original = self
            .mapping
            .lock()
            .unwrap()
            .get(&addr)
            .cloned()
            .unwrap_or(addr);
        self.inner.send_to(bytes, original).await
    }
}

/// Wrap only TPROXY inbounds. Both the first routing destination and all later
/// UDP datagrams become domain names; replies retain their synthetic source IP.
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
pub(crate) struct RestoringInbound {
    pub inner: Box<dyn Inbound>,
    pub fake: Arc<FakeIp>,
}
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
#[async_trait]
impl Inbound for RestoringInbound {
    async fn init(&self) -> Result<()> {
        self.inner.init().await
    }
    async fn shutdown(&self) -> Result<()> {
        self.inner.shutdown().await
    }
    async fn accept(&mut self) -> Result<ProxyRequest> {
        let mut req = self.inner.accept().await?;
        let old_dst = req.dst().clone();
        req.set_dst(self.fake.restore(req.dst())?);
        if &old_dst != req.dst() {
            tracing::trace!("fakeip mapping: {} -> {}", old_dst, req.dst());
        }
        if let ProxyRequest::Udp(mut session) = req {
            let mapping = Arc::new(Mutex::new(HashMap::new()));
            session.recv = Box::new(RestoringRecv {
                inner: session.recv,
                fake: self.fake.clone(),
                mapping: mapping.clone(),
            });
            session.send = Arc::new(MappedSend {
                inner: session.send,
                mapping,
            });
            req = ProxyRequest::Udp(session);
        }
        Ok(req)
    }
}
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
struct RestoringRecv {
    inner: AnyUdpRecv,
    fake: Arc<FakeIp>,
    mapping: Mapping,
}
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
#[async_trait]
impl UdpRecv for RestoringRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr)> {
        let (bytes, original) = self.inner.recv_from().await?;
        let restored = self.fake.restore(&original)?;
        remember(&self.mapping, &restored, original)?;
        Ok((bytes, restored))
    }
}
