use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::oneshot,
};

use crate::{ProxyRequest, TcpSession, TcpTrait, UserContext, msgs::socks5::SocksAddr};

/// Keeps the routed session alive. Dropping it interrupts pending relay I/O so
/// detached outbound tasks close both directions instead of half-closing.
pub(crate) struct RequestGuard {
    _cancel_on_drop: oneshot::Sender<()>,
}

/// Creates a local stream and its routable TCP request without opening a socket.
pub(crate) fn tcp_request(
    dst: SocksAddr,
    user_context: UserContext,
    buffer_size: usize,
) -> (DuplexStream, ProxyRequest, RequestGuard) {
    let (stream, peer) = tokio::io::duplex(buffer_size);
    let (cancel_on_drop, cancelled) = oneshot::channel();
    let request: ProxyRequest = ProxyRequest::Tcp(TcpSession {
        stream: Box::new(RequestStream {
            inner: peer,
            cancelled,
            closed: false,
        }),
        dst,
        src_addr: None,
        user_context,
    });
    (
        stream,
        request,
        RequestGuard {
            _cancel_on_drop: cancel_on_drop,
        },
    )
}

struct RequestStream {
    inner: DuplexStream,
    cancelled: oneshot::Receiver<()>,
    closed: bool,
}

impl RequestStream {
    fn check_cancelled(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.closed || Pin::new(&mut self.cancelled).poll(cx).is_ready() {
            self.closed = true;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "proxy request finished",
            ));
        }
        Ok(())
    }
}

impl TcpTrait for DuplexStream {}
impl TcpTrait for RequestStream {}

impl AsyncRead for RequestStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for RequestStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn routed_tcp_preserves_metadata_and_cancels_io_when_guard_drops() {
        let dst = SocksAddr::from_domain("upstream.test".into(), 853);
        let context = UserContext {
            inbound_tag: "resolver".into(),
            ..Default::default()
        };
        let (mut client, request, guard) = tcp_request(dst, context, 64);
        let ProxyRequest::Tcp(mut session) = request else {
            panic!("expected TCP")
        };
        assert_eq!(session.dst.to_string(), "upstream.test:853");
        assert_eq!(session.user_context.inbound_tag, "resolver");
        assert!(session.src_addr.is_none());
        client.write_all(b"query").await.unwrap();
        let mut buf = [0; 5];
        session.stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"query");
        session.stream.write_all(b"reply").await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"reply");

        drop(guard);
        assert_eq!(
            session.stream.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            session.stream.write_u8(0).await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }
}
