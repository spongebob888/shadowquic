use std::{
    io::{self, IoSlice},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug, Clone)]
pub struct TrackedRead<T> {
    inner: T,
    bytes_read: Arc<AtomicU64>,
    tracked_sessions: Option<Arc<AtomicU64>>,
}

impl<T> TrackedRead<T> {
    pub fn new(inner: T) -> Self {
        Self::with_counter(inner, Arc::new(AtomicU64::new(0)), None)
    }

    pub fn with_counter(inner: T, bytes_read: Arc<AtomicU64>, tracked_sessions: Option<Arc<AtomicU64>>) -> Self {
        tracked_sessions.as_ref().map(|counter| counter.fetch_add(1, Ordering::Relaxed));
        Self { inner, bytes_read, tracked_sessions: (tracked_sessions) }
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    pub fn counter(&self) -> Arc<AtomicU64> {
        self.bytes_read.clone()
    }

    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

}

impl<T> Drop for TrackedRead<T> {
    fn drop(&mut self) {
        if let Some(counter) = &self.tracked_sessions {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for TrackedRead<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &poll {
            let n = buf.filled().len().saturating_sub(filled_before);
            self.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        }

        poll
    }
}

#[derive(Debug, Clone)]
pub struct TrackedWrite<T> {
    inner: T,
    bytes_written: Arc<AtomicU64>,
}

impl<T> TrackedWrite<T> {
    pub fn new(inner: T) -> Self {
        Self::with_counter(inner, Arc::new(AtomicU64::new(0)))
    }

    pub fn with_counter(inner: T, bytes_written: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            bytes_written,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    pub fn counter(&self) -> Arc<AtomicU64> {
        self.bytes_written.clone()
    }

    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TrackedWrite<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);

        if let Poll::Ready(Ok(n)) = &poll {
            self.bytes_written.fetch_add(*n as u64, Ordering::Relaxed);
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
            self.bytes_written.fetch_add(*n as u64, Ordering::Relaxed);
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
