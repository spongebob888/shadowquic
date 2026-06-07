use std::{
    collections::HashMap,
    io::{self, IoSlice},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Mutex,
};

use crate::{
    AnyTcp, AnyUdpRecv, AnyUdpSend, ProxyRequest, TcpSession, TcpTrait, UdpRecv, UdpSend,
    UdpSession, UserName, error::SError, msgs::socks5::SocksAddr,
};

#[derive(Default)]
pub struct ProxyStats {
    tcp_sent: Arc<AtomicU64>,
    tcp_recv: Arc<AtomicU64>,
    udp_sent: Arc<AtomicU64>,
    udp_recv: Arc<AtomicU64>,
    tcp_conns: Arc<AtomicU64>,
    udp_conns: Arc<AtomicU64>,
}

impl ProxyStats {
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
}

pub struct Observer {
    pub user_stats: Arc<Mutex<HashMap<UserName, ProxyStats>>>,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            user_stats: Default::default(),
        }
    }
}

struct TrackedTcp {
    inner: AnyTcp,
    bytes_recv: Arc<AtomicU64>,
    bytes_sent: Arc<AtomicU64>,
    tcp_conns: Arc<AtomicU64>,
}

impl TrackedTcp {
    fn new(
        inner: AnyTcp,
        bytes_recv: Arc<AtomicU64>,
        bytes_sent: Arc<AtomicU64>,
        tcp_conns: Arc<AtomicU64>,
    ) -> Self {
        tcp_conns.fetch_add(1, Ordering::Relaxed);
        Self {
            inner,
            bytes_recv,
            bytes_sent,
            tcp_conns,
        }
    }
}

impl Drop for TrackedTcp {
    fn drop(&mut self) {
        self.tcp_conns.fetch_sub(1, Ordering::Relaxed);
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
            self.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
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
            self.bytes_sent.fetch_add(*n as u64, Ordering::Relaxed);
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
            self.bytes_sent.fetch_add(*n as u64, Ordering::Relaxed);
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

impl TcpTrait for TrackedTcp {}

struct TrackedUdpRecv {
    inner: AnyUdpRecv,
    bytes_recv: Arc<AtomicU64>,
    udp_conns: Arc<AtomicU64>,
}

impl Drop for TrackedUdpRecv {
    fn drop(&mut self) {
        self.udp_conns.fetch_sub(1, Ordering::Relaxed);
    }
}

#[async_trait]
impl UdpRecv for TrackedUdpRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let (bytes, addr) = self.inner.recv_from().await?;
        self.bytes_recv
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok((bytes, addr))
    }
}

struct TrackedUdpSend {
    inner: AnyUdpSend,
    bytes_sent: Arc<AtomicU64>,
}

#[async_trait]
impl UdpSend for TrackedUdpSend {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let len = self.inner.send_to(buf, addr).await?;
        self.bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
        Ok(len)
    }
}

impl Observer {
    pub(crate) async fn wrap_request(&mut self, req: ProxyRequest) -> ProxyRequest {
        match req {
            ProxyRequest::Tcp(tcp) => {
                let Some(user_context) = tcp.user_context else {
                    return ProxyRequest::Tcp(tcp);
                };
                let username = user_context.username.clone();
                let mut user_stats = self.user_stats.lock().await;
                let stats = user_stats.entry(username).or_default();
                let (tcp_recv, tcp_sent, tcp_conns) = stats.tcp_counters();
                drop(user_stats);

                ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(TrackedTcp::new(tcp.stream, tcp_recv, tcp_sent, tcp_conns))
                        as AnyTcp,
                    dst: tcp.dst,
                    user_context: Some(user_context),
                })
            }
            ProxyRequest::Udp(udp) => {
                let Some(user_context) = udp.user_context else {
                    return ProxyRequest::Udp(udp);
                };
                let username = user_context.username.clone();
                let mut user_stats = self.user_stats.lock().await;
                let stats = user_stats.entry(username).or_default();
                let (udp_recv, udp_sent, udp_conns) = stats.udp_counters();
                udp_conns.fetch_add(1, Ordering::Relaxed);
                drop(user_stats);

                ProxyRequest::Udp(UdpSession {
                    recv: Box::new(TrackedUdpRecv {
                        inner: udp.recv,
                        bytes_recv: udp_recv,
                        udp_conns,
                    }) as AnyUdpRecv,
                    send: Arc::new(TrackedUdpSend {
                        inner: udp.send,
                        bytes_sent: udp_sent,
                    }) as AnyUdpSend,
                    stream: udp.stream,
                    bind_addr: udp.bind_addr,
                    user_context: Some(user_context),
                })
            }
        }
    }
}
