use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

use bytes::BytesMut;
use tokio::{
    net::{TcpStream, lookup_host},
    sync::Mutex,
};
use tracing::{Instrument, error, info_span, trace};

use crate::{
    Outbound, TcpSession, UdpSession,
    config::{DirectOutCfg, DnsStrategy},
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr, VarVec},
    utils::{
        activity_stream::{Activity, ActivityGuard, half_close_grace, half_close_watchdog},
        dual_socket::DualSocket,
    },
};
use async_trait::async_trait;

#[derive(Clone, Debug, Default)]
pub struct DirectOut {
    pub cfg: DirectOutCfg,
}

#[async_trait]
impl Outbound for DirectOut {
    async fn handle(&self, req: crate::ProxyRequest) -> Result<(), crate::error::SError> {
        let dns_strategy = self.cfg.dns_strategy.clone();
        let half_close_timeout = half_close_grace(self.cfg.half_close_timeout);
        let self_clone = self.clone();

        let fut = async move {
            match req {
                crate::ProxyRequest::Tcp(tcp_session) => {
                    trace!("direct tcp to {}", tcp_session.dst);
                    let dst = tcp_session.dst.to_socket_addrs()?;
                    let dst = apply_dns_strategy(dst, &dns_strategy)
                        .ok_or(SError::DomainResolveFailed(tcp_session.dst.to_string()))?;
                    trace!("resolved to {}", dst);

                    let upstream = TcpStream::connect(dst).await?;
                    let _ = upstream.set_nodelay(true);
                    relay_tcp(tcp_session, upstream, half_close_timeout).await?;
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

/// Relays one TCP session between the QUIC side and the real upstream.
///
/// Returns once the relay is over: either both directions reached EOF, or the
/// watchdog gave the session up after a half-close followed by silence.
///
/// This is the server half of the same bound the clients apply, and it is the
/// half that decides whether the client gets its stream credit back. quinn
/// returns a bi-stream's credit only once **both** ends of the relay have
/// dropped their stream halves (`quinn-proto-jls`, `StreamsState::stream_freed`),
/// and a peer's RESET_STREAM or STOP_SENDING does not free it. This end is the
/// one waiting on the real upstream, so an upstream that answers and then never
/// closes keeps the relay — and the client's credit — alive indefinitely, even
/// after the client has already given its own half up.
async fn relay_tcp(
    mut tcp_session: TcpSession,
    mut upstream: TcpStream,
    half_close_timeout: Option<Duration>,
) -> Result<(), SError> {
    let activity = Activity::new();
    let mut quic_side = ActivityGuard::new(&mut tcp_session.stream, activity.clone());
    let mut upstream_side = ActivityGuard::new(&mut upstream, activity.clone());
    let copy = tokio::io::copy_bidirectional_with_sizes(
        &mut quic_side,
        &mut upstream_side,
        1024 * 16,
        1024 * 16,
    );
    tokio::pin!(copy);

    match half_close_timeout {
        Some(grace) => tokio::select! {
            res = &mut copy => {
                res?;
            }
            _ = half_close_watchdog(&activity, grace) => {
                error!(
                    dst = %tcp_session.dst,
                    "relay half-closed and silent for {}s, closing the session",
                    grace.as_secs()
                );
            }
        },
        None => {
            copy.await?;
        }
    }
    Ok(())
}

#[derive(Default, Clone)]
struct DnsResolve(Arc<Mutex<HashMap<Vec<u8>, SocketAddr>>>);
impl DnsResolve {
    async fn resolve(
        &self,
        socks: SocksAddr,
        strategy: &DnsStrategy,
    ) -> Result<SocketAddr, SError> {
        if let AddrOrDomain::Domain(x) = &socks.addr {
            if let Some(v) = self.0.lock().await.get(&x.contents) {
                Ok(*v)
            } else {
                let s = resolve(&socks, strategy).await?;
                self.0.lock().await.insert(x.contents.clone(), s);
                Ok(s)
            }
        } else {
            Ok(resolve(&socks, strategy).await?)
        }
    }
    async fn inv_resolve(&self, addr: &SocketAddr) -> SocksAddr {
        if let Some(add) = self.0.lock().await.iter().find(|x| x.1 == addr) {
            SocksAddr {
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
    pub fn new(cfg: DirectOutCfg) -> Self {
        Self { cfg }
    }

    async fn handle_udp(&self, udp_session: UdpSession) -> Result<(), SError> {
        trace!(bind_addr = %udp_session.bind_addr,"associating udp");
        let dst =
            udp_session
                .bind_addr
                .to_socket_addrs()?
                .next()
                .ok_or(SError::DomainResolveFailed(
                    udp_session.bind_addr.to_string(),
                ))?;
        let ipv4 = dst.is_ipv4();
        // For unspecified address, we try to bind a dual stack socket first.
        // If it fails, we fallback to single stack socket
        // https://github.com/spongebob888/shadowquic/issues/172
        let socket = if dst.ip().is_unspecified() {
            let socket = DualSocket::new_bind("[::]:0".parse().unwrap(), true)?;
            if socket.dual_stack || !ipv4 {
                trace!("bound to dual stack socket");
                socket
            } else {
                trace!("fallback to single stack socket");
                DualSocket::new_bind(dst, false)?
            }
        } else {
            DualSocket::new_bind(dst, false)?
        };
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
fn apply_dns_strategy<It>(mut ip_list: It, strategy: &DnsStrategy) -> Option<SocketAddr>
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

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const RESPONSE: &[u8] = b"ok";

    /// A connected loopback pair: the first end stands in for the QUIC side of
    /// the session (what `DirectOut` is handed), the second for the client.
    ///
    /// `TcpTrait` is implemented for `TcpStream` only (`src/lib.rs:78`), so a
    /// `tokio::io::duplex` pair cannot be used here.
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (session, _) = listener.accept().await.unwrap();
        (session, client)
    }

    /// An upstream that reads one request, answers it, closes its own write half
    /// and then holds the connection open without ever speaking again. That is
    /// the shape the watchdog exists for: one direction has finished, and the
    /// surviving one is waiting on a peer that never closes.
    async fn spawn_half_closing_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut upstream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = upstream.read(&mut buf).await.unwrap();
            let _ = upstream.write_all(RESPONSE).await;
            let _ = upstream.shutdown().await;
            std::future::pending::<()>().await;
        });
        addr
    }

    /// Starts a relay against the half-closing upstream and drives one exchange
    /// through it, so the session is a real relayed request rather than just an
    /// open socket. Returns the relay task and the client end.
    async fn started_relay(half_close_timeout: u64) -> (JoinHandle<Result<(), SError>>, TcpStream) {
        let upstream_addr = spawn_half_closing_upstream().await;
        let (session, mut client) = tcp_pair().await;
        let upstream = TcpStream::connect(upstream_addr).await.unwrap();
        let tcp_session = TcpSession {
            stream: Box::new(session) as Box<dyn crate::TcpTrait>,
            dst: SocksAddr::from(upstream_addr),
            src_addr: None,
            user_context: Default::default(),
        };

        // Spawned, not merely constructed: an unpolled future would relay
        // nothing, and the exchange below would block forever.
        let relay = tokio::spawn(relay_tcp(
            tcp_session,
            upstream,
            half_close_grace(half_close_timeout),
        ));

        client.write_all(b"req").await.unwrap();
        let mut answer = [0u8; RESPONSE.len()];
        client.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, RESPONSE);

        // The upstream's write half is now closed, so one relay direction has
        // finished and the surviving one is silent from here on.
        (relay, client)
    }

    /// The watchdog must give the session up. Ending the relay is what drops the
    /// session's stream halves, and only that returns the bi-stream's credit to
    /// the peer's stream limit.
    ///
    /// The relay future is awaited directly rather than observed through the
    /// sockets: the relay shuts down the session's *write* half as soon as the
    /// upstream half-closes, so a peer reading the session sees EOF whether or
    /// not the relay ever ends. Reading the socket cannot tell the two apart.
    #[tokio::test]
    async fn relay_ends_when_peer_half_closes_and_stays_silent() {
        let (relay, _client) = started_relay(1).await;

        tokio::time::timeout(Duration::from_secs(5), relay)
            .await
            .expect("relay never ended: the watchdog did not close the session")
            .expect("relay task panicked")
            .expect("relay returned an error");
    }

    /// Control for the test above: with the watchdog disabled the same workload
    /// must keep the relay alive. Otherwise "the relay ended" could be blamed on
    /// something else ending it rather than on the watchdog.
    #[tokio::test]
    async fn relay_outlives_a_silent_peer_when_the_watchdog_is_disabled() {
        let (relay, _client) = started_relay(0).await;

        let result = tokio::time::timeout(Duration::from_secs(3), relay).await;
        assert!(
            result.is_err(),
            "relay ended with the watchdog disabled: {result:?}"
        );
    }

    /// `DirectOutCfg` has a hand-written `Default`; it must agree with the serde
    /// default, or a config loaded from YAML and one built in Rust would
    /// disagree about whether the watchdog is even on.
    #[test]
    fn direct_out_cfg_default_enables_the_watchdog() {
        assert_eq!(DirectOutCfg::default().half_close_timeout, 600);
        assert!(half_close_grace(DirectOutCfg::default().half_close_timeout).is_some());
        assert!(half_close_grace(0).is_none());
    }
}
