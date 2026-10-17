#[cfg(not(target_has_atomic = "64"))]
use portable_atomic::{AtomicU64, Ordering};
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::{AtomicU64, Ordering};

use std::{
    collections::HashMap,
    io::{self, IoSlice},
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, OnceLock, RwLock, Weak},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Mutex,
};
use tracing::info;

use crate::{
    AnyTcp, AnyUdpRecv, AnyUdpSend, ProxyRequest, StatsContext, TcpTrait, UdpRecv, UdpSend,
    UserName,
    error::SError,
    msgs::{socks5::SocksAddr, squic::UserStats},
};

#[derive(Default, Clone)]
pub struct ProxyStatsAtm {
    pub(crate) tcp_sent: Arc<AtomicU64>,
    pub(crate) tcp_recv: Arc<AtomicU64>,
    pub(crate) udp_sent: Arc<AtomicU64>,
    pub(crate) udp_recv: Arc<AtomicU64>,
    pub(crate) tcp_conns: Arc<AtomicU64>,
    pub(crate) udp_conns: Arc<AtomicU64>,
}

impl ProxyStatsAtm {
    fn tcp_counters(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
        (
            self.tcp_recv.clone(),
            self.tcp_sent.clone(),
            self.tcp_conns.clone(),
        )
    }

    fn udp_counters(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
        (
            self.udp_recv.clone(),
            self.udp_sent.clone(),
            self.udp_conns.clone(),
        )
    }

    fn reset(&self) {
        self.tcp_sent.store(0, Ordering::Relaxed);
        self.tcp_recv.store(0, Ordering::Relaxed);
        self.udp_sent.store(0, Ordering::Relaxed);
        self.udp_recv.store(0, Ordering::Relaxed);
    }
}

type Scoreboard = Arc<Mutex<Vec<(Weak<TrackedSendStats>, Weak<TrackedRecvStats>)>>>;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Observer {
    pub user_stats: Arc<Mutex<HashMap<UserName, ProxyStatsAtm>>>,
    pub sq_conns: Arc<Mutex<HashMap<u64, StatsContext>>>,
    pub tcp_scoreboard: Scoreboard,
    pub udp_scoreboard: Scoreboard,
    pub last_cleanup: Arc<Mutex<Instant>>,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            user_stats: Arc::default(),
            sq_conns: Arc::default(),
            tcp_scoreboard: Arc::default(),
            udp_scoreboard: Arc::default(),
            last_cleanup: Arc::new(Mutex::new(Instant::now())),
        }
    }
}
impl Observer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prune expired sessions at most once per interval as requests arrive.
    async fn cleanup(&self) {
        {
            let Ok(mut last_cleanup) = self.last_cleanup.try_lock() else {
                return;
            };
            if last_cleanup.elapsed() < CLEANUP_INTERVAL {
                return;
            }
            *last_cleanup = Instant::now();
        }
        self.sq_conns
            .lock()
            .await
            .retain(|_, ctx| ctx.conn_handle.strong_count() > 0);
        for scoreboard in [&self.tcp_scoreboard, &self.udp_scoreboard] {
            scoreboard
                .lock()
                .await
                .retain(|(send, recv)| send.strong_count() > 0 || recv.strong_count() > 0);
        }
    }

    pub async fn on_new_request(&self, stats_context: &StatsContext) -> ProxyStatsAtm {
        self.cleanup().await;
        if stats_context.conn_handle.strong_count() > 0 {
            self.sq_conns
                .lock()
                .await
                .insert(stats_context.conn_id, stats_context.clone());
        }
        self.user_stats
            .lock()
            .await
            .entry(stats_context.username.clone())
            .or_default()
            .clone()
    }

    pub async fn remove_user(&self, username: &str) {
        {
            let mut user_stats = self.user_stats.lock().await;
            user_stats.remove(username);
        }
        self.close_conn(username).await;
        let mut conns = self.sq_conns.lock().await;
        conns.retain(|_, ctx| ctx.username != username);
    }
    pub async fn close_conn(&self, username: &str) {
        let to_close: Vec<_> = {
            let conns = self.sq_conns.lock().await;
            conns
                .iter()
                .filter(|(_, ctx)| ctx.username == username)
                .map(|(id, ctx)| (*id, ctx.conn_handle.clone()))
                .collect()
        };
        for (id, handle) in to_close {
            if let Some(handle) = handle.upgrade() {
                handle.stop();
                info!(%id, "connection closed by observer");
            }
        }
    }
    pub async fn get_conn_num(&self, username: &str) -> usize {
        let mut conns = self.sq_conns.lock().await;
        conns.retain(|_, ctx| ctx.conn_handle.upgrade().is_some());
        conns
            .iter()
            .filter(|(_, ctx)| ctx.username == username)
            .count()
    }
    pub async fn get_user_stats(&self, username: &str) -> UserStats {
        let conn_num = self.get_conn_num(username).await;
        let user_stats = self.user_stats.lock().await;
        if let Some(stats) = user_stats.get(username) {
            let tcp_recv = stats.tcp_recv.load(Ordering::Relaxed);
            let tcp_sent = stats.tcp_sent.load(Ordering::Relaxed);
            let udp_recv = stats.udp_recv.load(Ordering::Relaxed);
            let udp_sent = stats.udp_sent.load(Ordering::Relaxed);
            UserStats {
                tcp_sent,
                tcp_recv,
                udp_sent,
                udp_recv,
                tcp_conns: stats.tcp_conns.load(Ordering::Relaxed),
                udp_conns: stats.udp_conns.load(Ordering::Relaxed),
                conn_num: conn_num as u32,
                username: username.to_owned(),
            }
        } else {
            UserStats {
                username: username.to_owned(),
                ..Default::default()
            }
        }
    }

    pub async fn clear_user_stats(&self, username: &str) {
        if let Some(stats) = self.user_stats.lock().await.get(username) {
            stats.reset();
        }
    }

    pub async fn clear_all_stats(&self, usernames: &[String]) {
        let user_stats = self.user_stats.lock().await;
        for username in usernames {
            if let Some(stats) = user_stats.get(username) {
                stats.reset();
            }
        }
    }

    /// Restore persisted traffic counters (used at startup).
    /// Connection counters are runtime-only and start from zero.
    pub async fn restore_stats(&self, stats: &[UserStats]) {
        let mut user_stats = self.user_stats.lock().await;
        for s in stats {
            let entry = user_stats.entry(s.username.clone()).or_default();
            entry.tcp_sent.store(s.tcp_sent, Ordering::Relaxed);
            entry.tcp_recv.store(s.tcp_recv, Ordering::Relaxed);
            entry.udp_sent.store(s.udp_sent, Ordering::Relaxed);
            entry.udp_recv.store(s.udp_recv, Ordering::Relaxed);
        }
    }

    pub async fn get_all_stats(&self, usernames: &[String]) -> Vec<UserStats> {
        let mut conns = self.sq_conns.lock().await;
        conns.retain(|_, ctx| ctx.conn_handle.upgrade().is_some());
        let conn_nums = usernames
            .iter()
            .map(|username| {
                let conn_num = conns
                    .iter()
                    .filter(|(_, ctx)| ctx.username == *username)
                    .count() as u32;
                (username.clone(), conn_num)
            })
            .collect::<HashMap<_, _>>();
        drop(conns);

        let user_stats = self.user_stats.lock().await;
        usernames
            .iter()
            .map(|username| {
                user_stats
                    .get(username)
                    .map(|stats| UserStats {
                        tcp_sent: stats.tcp_sent.load(Ordering::Relaxed),
                        tcp_recv: stats.tcp_recv.load(Ordering::Relaxed),
                        udp_sent: stats.udp_sent.load(Ordering::Relaxed),
                        udp_recv: stats.udp_recv.load(Ordering::Relaxed),
                        tcp_conns: stats.tcp_conns.load(Ordering::Relaxed),
                        udp_conns: stats.udp_conns.load(Ordering::Relaxed),
                        conn_num: *conn_nums.get(username).unwrap_or(&0),
                        username: username.clone(),
                    })
                    .unwrap_or_else(|| UserStats {
                        username: username.clone(),
                        ..Default::default()
                    })
            })
            .collect()
    }
}

pub struct TrackedSendStats {
    #[cfg(feature = "restful-api")]
    pub id: u64,
    pub bytes_send: AtomicU64,
    pub dst: RwLock<SocksAddr>,
    /// Filled once the dispatcher has selected an outbound.
    pub outbound: String,

    pub first_send: OnceLock<Instant>,
}

impl TrackedSendStats {
    fn new(dst: SocksAddr, outbound: String, _recv: &TrackedRecvStats) -> Self {
        Self {
            #[cfg(feature = "restful-api")]
            id: _recv.id,
            bytes_send: AtomicU64::new(0),
            dst: RwLock::new(dst),
            outbound,
            first_send: OnceLock::new(),
        }
    }

    fn sent(&self, n: usize) {
        self.bytes_send.fetch_add(n as u64, Ordering::Relaxed);
        if n > 0 {
            self.first_send.get_or_init(Instant::now);
        }
    }
}

pub struct TrackedRecvStats {
    conns: Arc<AtomicU64>,
    pub bytes_recv: AtomicU64,
    pub src_addr: Option<SocketAddr>,
    pub created: Instant,
    #[cfg(feature = "restful-api")]
    pub id: u64,
}

impl TrackedRecvStats {
    fn new(src_addr: Option<SocketAddr>, conns: Arc<AtomicU64>) -> Self {
        conns.fetch_add(1, Ordering::Relaxed);
        Self {
            conns,
            bytes_recv: AtomicU64::new(0),
            src_addr,
            created: Instant::now(),
            #[cfg(feature = "restful-api")]
            id: {
                static NEXT_ID: AtomicU64 = AtomicU64::new(1);
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            },
        }
    }
}

impl Drop for TrackedRecvStats {
    fn drop(&mut self) {
        self.conns.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct TrackedTcp {
    inner: AnyTcp,
    global_bytes_recv: Arc<AtomicU64>,
    global_bytes_sent: Arc<AtomicU64>,
    tracked_send_local: Arc<TrackedSendStats>,
    tracked_recv_local: Arc<TrackedRecvStats>,
}

impl TrackedTcp {
    fn new(
        inner: AnyTcp,
        stats: &ProxyStatsAtm,
        src: Option<SocketAddr>,
        dst: SocksAddr,
        outbound: String,
    ) -> Self {
        let (global_bytes_recv, global_bytes_sent, tcp_conns) = stats.tcp_counters();
        let recv = Arc::new(TrackedRecvStats::new(src, tcp_conns));
        Self {
            inner,
            global_bytes_recv,
            global_bytes_sent,
            tracked_send_local: Arc::new(TrackedSendStats::new(dst, outbound, &recv)),
            tracked_recv_local: recv,
        }
    }
}

impl AsyncRead for TrackedTcp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &poll {
            let n = buf.filled().len().saturating_sub(filled_before);
            self.global_bytes_recv
                .fetch_add(n as u64, Ordering::Relaxed);
            self.tracked_recv_local
                .bytes_recv
                .fetch_add(n as u64, Ordering::Relaxed);
        }

        poll
    }
}

impl AsyncWrite for TrackedTcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);

        if let Poll::Ready(Ok(n)) = &poll {
            self.global_bytes_sent
                .fetch_add(*n as u64, Ordering::Relaxed);
            self.tracked_send_local.sent(*n);
        }

        poll
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);

        if let Poll::Ready(Ok(n)) = &poll {
            self.global_bytes_sent
                .fetch_add(*n as u64, Ordering::Relaxed);
            self.tracked_send_local.sent(*n);
        }

        poll
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl TcpTrait for TrackedTcp {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.inner.peer_addr()
    }
}

struct TrackedUdpRecv {
    inner: AnyUdpRecv,
    global_bytes_recv: Arc<AtomicU64>,
    tracked_recv_local: Arc<TrackedRecvStats>,
}

#[async_trait]
impl UdpRecv for TrackedUdpRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let (bytes, addr) = self.inner.recv_from().await?;
        self.global_bytes_recv
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.tracked_recv_local
            .bytes_recv
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok((bytes, addr))
    }
}

struct TrackedUdpSend {
    inner: AnyUdpSend,
    global_bytes_sent: Arc<AtomicU64>,
    tracked_send_local: Arc<TrackedSendStats>,
}

#[async_trait]
impl UdpSend for TrackedUdpSend {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let len = self.inner.send_to(buf, addr).await?;
        self.global_bytes_sent
            .fetch_add(len as u64, Ordering::Relaxed);
        self.tracked_send_local.sent(len);
        Ok(len)
    }
}

impl Observer {
    pub(crate) async fn wrap_request(&self, req: ProxyRequest, outbound: String) -> ProxyRequest {
        let stats = match &req.user_context().stats {
            Some(context) => self.on_new_request(context).await,
            None => {
                self.cleanup().await;
                self.user_stats
                    .lock()
                    .await
                    .entry(String::new())
                    .or_default()
                    .clone()
            }
        };
        match req {
            ProxyRequest::Tcp(mut tcp) => {
                let tracked =
                    TrackedTcp::new(tcp.stream, &stats, tcp.src_addr, tcp.dst.clone(), outbound);
                self.tcp_scoreboard.lock().await.push((
                    Arc::downgrade(&tracked.tracked_send_local),
                    Arc::downgrade(&tracked.tracked_recv_local),
                ));
                tcp.stream = Box::new(tracked);
                ProxyRequest::Tcp(tcp)
            }
            ProxyRequest::Udp(mut udp) => {
                let (udp_recv, udp_sent, udp_conns) = stats.udp_counters();
                let mut scoreboard = self.udp_scoreboard.lock().await;
                let recv = Arc::new(TrackedRecvStats::new(udp.src_addr, udp_conns));
                let send = Arc::new(TrackedSendStats::new(udp.dst.clone(), outbound, &recv));
                scoreboard.push((Arc::downgrade(&send), Arc::downgrade(&recv)));
                udp.recv = Box::new(TrackedUdpRecv {
                    inner: udp.recv,
                    global_bytes_recv: udp_recv,
                    tracked_recv_local: recv,
                });
                udp.send = Arc::new(TrackedUdpSend {
                    inner: udp.send,
                    global_bytes_sent: udp_sent,
                    tracked_send_local: send,
                });
                ProxyRequest::Udp(udp)
            }
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
