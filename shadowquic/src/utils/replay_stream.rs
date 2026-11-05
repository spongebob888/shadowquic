use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::TcpTrait;

#[derive(Debug)]
pub struct ReplayStream<S> {
    prefix: io::Cursor<Vec<u8>>,
    inner: S,
    #[cfg(feature = "sniffer")]
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl<S> ReplayStream<S> {
    #[cfg(feature = "sniffer")]
    pub(crate) fn with_permit(
        prefix: Vec<u8>,
        inner: S,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Self {
        Self {
            prefix: io::Cursor::new(prefix),
            inner,
            permit: Some(permit),
        }
    }
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: io::Cursor::new(prefix),
            inner,
            #[cfg(feature = "sniffer")]
            permit: None,
        }
    }
}

impl<S> AsyncRead for ReplayStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let pos = self.prefix.position() as usize;
        let prefix = self.prefix.get_ref();

        if pos < prefix.len() {
            let remain = &prefix[pos..];
            let n = remain.len().min(buf.remaining());
            buf.put_slice(&remain[..n]);
            self.prefix.set_position((pos + n) as u64);
            if pos + n == self.prefix.get_ref().len() {
                self.prefix = io::Cursor::new(Vec::new());
                #[cfg(feature = "sniffer")]
                {
                    self.permit = None;
                }
            }
            return Poll::Ready(Ok(()));
        }

        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for ReplayStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S> TcpTrait for ReplayStream<S>
where
    S: TcpTrait,
{
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.inner.peer_addr()
    }
}
