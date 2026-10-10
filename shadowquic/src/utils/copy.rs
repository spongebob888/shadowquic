use std::{
    future::{Future, poll_fn},
    io,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Copy until both directions finish, or a half-closed stream becomes idle.
/// The caller must drop both streams on return, including on timeout.
pub(crate) async fn copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
    a_buffer: usize,
    b_buffer: usize,
    half_close_timeout: u64,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    if half_close_timeout == 0 {
        return tokio::io::copy_bidirectional_with_sizes(a, b, a_buffer, b_buffer).await;
    }
    let eof = AtomicBool::new(false);
    let activity = AtomicBool::new(false);
    let mut a = Tracked {
        inner: a,
        eof: &eof,
        activity: &activity,
    };
    let mut b = Tracked {
        inner: b,
        eof: &eof,
        activity: &activity,
    };
    let copy = tokio::io::copy_bidirectional_with_sizes(&mut a, &mut b, a_buffer, b_buffer);
    tokio::pin!(copy);
    let duration = Duration::from_millis(half_close_timeout);
    let timer = tokio::time::sleep(duration);
    tokio::pin!(timer);
    let mut started = false;
    poll_fn(|cx| {
        if let Poll::Ready(result) = copy.as_mut().poll(cx) {
            return Poll::Ready(result);
        }
        let active = activity.swap(false, Relaxed);
        if eof.load(Relaxed) {
            if !started || active {
                timer.as_mut().reset(tokio::time::Instant::now() + duration);
                started = true;
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "half-closed stream inactivity timeout",
                )));
            }
        }
        Poll::Pending
    })
    .await
}

// Atomics keep the copy future Send without locking on every read/write.
struct Tracked<'a, T: ?Sized> {
    inner: &'a mut T,
    eof: &'a AtomicBool,
    activity: &'a AtomicBool,
}

impl<T: AsyncRead + Unpin + ?Sized> AsyncRead for Tracked<'_, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let remaining = buf.remaining();
        let result = Pin::new(&mut *self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            if buf.filled().len() > before {
                self.activity.store(true, Relaxed);
            } else if remaining > 0 {
                self.eof.store(true, Relaxed);
            }
        }
        result
    }
}

impl<T: AsyncWrite + Unpin + ?Sized> AsyncWrite for Tracked<'_, T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut *self.inner).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.activity.store(true, Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    fn relay(
        timeout: u64,
    ) -> (
        DuplexStream,
        DuplexStream,
        tokio::task::JoinHandle<io::Result<(u64, u64)>>,
    ) {
        let (client, mut a) = duplex(64);
        let (mut b, server) = duplex(64);
        let task =
            tokio::spawn(async move { copy_bidirectional(&mut a, &mut b, 16, 16, timeout).await });
        (client, server, task)
    }

    async fn advance(ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn half_close_times_out_in_either_direction() {
        for reverse in [false, true] {
            let (mut client, mut server, task) = relay(100);
            if reverse {
                std::mem::swap(&mut client, &mut server);
            }
            client.shutdown().await.unwrap();
            assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
            advance(99).await;
            assert!(!task.is_finished());
            advance(1).await;
            assert_eq!(
                task.await.unwrap().unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
            assert!(server.write_all(b"closed").await.is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn open_idle_stream_does_not_time_out() {
        let (mut client, mut server, task) = relay(100);
        tokio::task::yield_now().await;
        advance(1000).await;
        assert!(!task.is_finished());
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap().unwrap(), (5, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn response_activity_resets_timeout_and_preserves_counts() {
        for reverse in [false, true] {
            let (mut client, mut server, task) = relay(100);
            if reverse {
                std::mem::swap(&mut client, &mut server);
            }
            client.write_all(b"request").await.unwrap();
            client.shutdown().await.unwrap();
            let mut request = Vec::new();
            server.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request");
            for _ in 0..4 {
                advance(75).await;
                server.write_all(b"reply").await.unwrap();
                let mut buf = [0; 5];
                client.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"reply");
                assert!(!task.is_finished());
            }
            server.shutdown().await.unwrap();
            let expected = if reverse { (20, 7) } else { (7, 20) };
            assert_eq!(task.await.unwrap().unwrap(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_restarts_after_response_activity_stops() {
        let (mut client, mut server, task) = relay(100);
        client.shutdown().await.unwrap();
        assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
        advance(75).await;
        server.write_all(b"x").await.unwrap();
        client.read_exact(&mut [0]).await.unwrap();
        advance(99).await;
        assert!(!task.is_finished());
        advance(1).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn eof_starts_timeout_even_when_shutdown_is_pending() {
        struct PendingShutdown(DuplexStream);
        impl AsyncRead for PendingShutdown {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.0).poll_read(cx, buf)
            }
        }
        impl AsyncWrite for PendingShutdown {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.0).poll_write(cx, buf)
            }
            fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.0).poll_flush(cx)
            }
            fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Pending
            }
        }
        let (mut client, mut a) = duplex(64);
        let (b, _server) = duplex(64);
        let mut b = PendingShutdown(b);
        client.shutdown().await.unwrap();
        let task =
            tokio::spawn(async move { copy_bidirectional(&mut a, &mut b, 16, 16, 100).await });
        tokio::task::yield_now().await;
        advance(100).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_disables_half_close_timeout() {
        let (mut client, mut server, task) = relay(0);
        client.shutdown().await.unwrap();
        assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
        advance(1_000_000).await;
        assert!(!task.is_finished());
        server.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap().unwrap(), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn half_close_times_out_with_backpressured_response() {
        let (mut client, mut server, task) = relay(100);
        client.shutdown().await.unwrap();
        assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
        let writer = tokio::spawn(async move { server.write_all(&[1; 1024]).await });
        tokio::task::yield_now().await;
        advance(100).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(writer.await.unwrap().is_err());
    }
}
