#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
use super::FakeIp;
use super::{DnsService, Result, dns_error};
#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
use crate::Inbound;
use crate::{
    AnyUdpRecv, AnyUdpSend, Outbound, ProxyRequest, UdpRecv, UdpSend, config::DnsStrategy,
    dns::resolve_destination, msgs::socks5::SocksAddr,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};
use tokio_util::sync::ReusableBoxFuture;

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

        let req = match req {
            ProxyRequest::Tcp(mut session) => {
                session.dst = resolve_destination(&*resolver, &session.dst, Some(&strategy))
                    .await?
                    .into();
                ProxyRequest::Tcp(session)
            }
            ProxyRequest::Udp(mut session) => {
                let mapping = Arc::new(Mutex::new(HashMap::new()));
                session.dst = resolve_destination(&*resolver, &session.dst, Some(&strategy))
                    .await?
                    .into();
                session.recv = Box::new(ResolvingRecv {
                    inner: session.recv,
                    resolver,
                    strategy,
                    mapping: mapping.clone(),
                    pending: None,
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
}

type Mapping = Arc<Mutex<HashMap<SocksAddr, SocksAddr>>>;
struct ResolvingRecv {
    inner: AnyUdpRecv,
    resolver: Arc<dyn DnsService>,
    strategy: DnsStrategy,
    mapping: Mapping,
    pending: Option<ResolvingPacket>,
}
type ResolvingPacket = ReusableBoxFuture<'static, Result<(Bytes, SocksAddr)>>;

impl UdpRecv for ResolvingRecv {
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr)>> {
        if self.pending.is_none() {
            let (bytes, original) = ready!(self.inner.poll_recv_from(cx))?;
            let resolver = self.resolver.clone();
            let strategy = self.strategy.clone();
            let mapping = self.mapping.clone();
            self.pending = Some(ReusableBoxFuture::new(async move {
                let resolved = resolve_destination(&*resolver, &original, Some(&strategy))
                    .await?
                    .into();
                remember(&mapping, &resolved, original)?;
                Ok((bytes, resolved))
            }));
        }
        let result = ready!(self.pending.as_mut().unwrap().poll(cx));
        self.pending = None;
        Poll::Ready(result)
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
impl UdpSend for MappedSend {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        bytes: &Bytes,
        addr: &SocksAddr,
    ) -> Poll<Result<usize>> {
        let original = self
            .mapping
            .lock()
            .unwrap()
            .get(addr)
            .cloned()
            .unwrap_or_else(|| addr.clone());
        self.inner.poll_send_to(cx, bytes, &original)
    }
}

/// Wrap transparent TPROXY and TUN inbounds. Both the first routing destination and all later
/// UDP datagrams become domain names; replies retain their synthetic source IP.
#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
pub(crate) struct RestoringInbound {
    pub inner: Box<dyn Inbound>,
    pub fake: Arc<FakeIp>,
}
#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
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
            tracing::trace!(
                req_id = req.user_context().req_id,
                event = "dns:fakeip_mapped",
                "fakeip mapping: {} -> {}",
                old_dst,
                req.dst()
            );
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
#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
struct RestoringRecv {
    inner: AnyUdpRecv,
    fake: Arc<FakeIp>,
    mapping: Mapping,
}
#[cfg(any(
    test,
    all(feature = "tproxy", target_os = "linux"),
    all(
        feature = "tun",
        any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows",
            target_os = "android"
        )
    )
))]
impl UdpRecv for RestoringRecv {
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr)>> {
        let (bytes, original) = ready!(self.inner.poll_recv_from(cx))?;
        let restored = self.fake.restore(&original)?;
        remember(&self.mapping, &restored, original)?;
        Poll::Ready(Ok((bytes, restored)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UdpRecvExt;
    use std::{
        future::Future,
        net::{IpAddr, Ipv4Addr},
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    struct DelayedResolver {
        ready: tokio::sync::Semaphore,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DnsService for DelayedResolver {
        async fn exchange(&self, _: &[u8]) -> Result<Vec<u8>> {
            unreachable!()
        }

        async fn resolve(&self, _: &str) -> Result<Vec<IpAddr>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.ready.acquire().await.unwrap().forget();
            Ok(vec![Ipv4Addr::LOCALHOST.into()])
        }
    }

    #[tokio::test]
    async fn pending_resolution_survives_canceled_receive_without_losing_packets() {
        let resolver = Arc::new(DelayedResolver {
            ready: tokio::sync::Semaphore::new(0),
            calls: AtomicUsize::new(0),
        });
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let mapping = Mapping::default();
        let mut receiver = ResolvingRecv {
            inner: Box::new(rx),
            resolver: resolver.clone(),
            strategy: DnsStrategy::Ipv4Only,
            mapping: mapping.clone(),
            pending: None,
        };
        for port in [53, 5353] {
            tx.send((
                Bytes::from_static(b"query"),
                SocksAddr::from_domain("test.example".into(), port),
            ))
            .await
            .unwrap();
        }
        for (index, port) in [53, 5353].into_iter().enumerate() {
            {
                let mut receiving = std::pin::pin!(receiver.recv_from());
                assert!(
                    receiving
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                // A repeated poll must keep the same lookup and packet.
                assert!(
                    receiving
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
            assert_eq!(resolver.calls.load(Ordering::Relaxed), index + 1);
            resolver.ready.add_permits(1);
            let (bytes, resolved) = receiver.recv_from().await.unwrap();
            assert_eq!(bytes, b"query"[..]);
            assert_eq!(resolved.to_string(), format!("127.0.0.1:{port}"));
            assert_eq!(
                mapping.lock().unwrap().get(&resolved),
                Some(&SocksAddr::from_domain("test.example".into(), port))
            );
        }
    }
}
