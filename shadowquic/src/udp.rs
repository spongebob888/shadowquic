use std::{
    future::{Future, poll_fn},
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};

use bytes::Bytes;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::PollSender;

use crate::{error::SError, msgs::socks5::SocksAddr};

/// A UDP sender that can be shared through an `Arc` for duplex operation.
pub trait UdpSend: Send + Sync + Unpin {
    /// Send a datagram to its proxy destination.
    ///
    /// On `Pending`, no datagram has been sent and the caller must retain the
    /// payload and address for the next poll. Only the most recent send waker
    /// needs to be registered; poll concurrent sends from the same task.
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &Bytes,
        addr: &SocksAddr,
    ) -> Poll<Result<usize, SError>>;
}

/// Async convenience methods for every UDP sender, including trait objects.
pub trait UdpSendExt: UdpSend {
    fn send_to(
        &self,
        buf: Bytes,
        addr: SocksAddr,
    ) -> impl Future<Output = Result<usize, SError>> + Send {
        poll_fn(move |cx| self.poll_send_to(cx, &buf, &addr))
    }

    /// Poll once, returning `UdpSenderBusy` if the sender would block.
    fn try_send_to(
        &self,
        buf: Bytes,
        addr: SocksAddr,
    ) -> impl Future<Output = Result<usize, SError>> + Send {
        poll_fn(move |cx| {
            Poll::Ready(match self.poll_send_to(cx, &buf, &addr) {
                Poll::Ready(result) => result,
                Poll::Pending => Err(SError::UdpSenderBusy("sender buffer may be full".into())),
            })
        })
    }
}
impl<T: UdpSend + ?Sized> UdpSendExt for T {}

pub trait UdpRecv: Send + Sync + Unpin {
    /// Receive a datagram and its proxy source address.
    ///
    /// On `Pending`, register the current task for notification when a packet
    /// or error becomes available. Retain any in-progress work between polls.
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr), SError>>;
}

/// Async convenience methods for every UDP receiver, including trait objects.
pub trait UdpRecvExt: UdpRecv {
    fn recv_from(&mut self) -> impl Future<Output = Result<(Bytes, SocksAddr), SError>> + Send {
        poll_fn(|cx| self.poll_recv_from(cx))
    }
}
impl<T: UdpRecv + ?Sized> UdpRecvExt for T {}

impl<T: UdpSend + ?Sized> UdpSend for Arc<T> {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &Bytes,
        addr: &SocksAddr,
    ) -> Poll<Result<usize, SError>> {
        (**self).poll_send_to(cx, buf, addr)
    }
}

impl<T: UdpRecv + ?Sized> UdpRecv for Box<T> {
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr), SError>> {
        (**self).poll_recv_from(cx)
    }
}

/// A bounded UDP channel sender that retains its capacity waiter between polls.
///
/// Wrap an `mpsc::Sender<(Bytes, SocksAddr)>` with `UdpChannelSend::from(sender)`
/// before using it as a [`UdpSend`]. The channel's receiver implements [`UdpRecv`]
/// directly. Import [`UdpSendExt`] and [`UdpRecvExt`] for async operations.
pub struct UdpChannelSend(Mutex<PollSender<(Bytes, SocksAddr)>>);

impl From<Sender<(Bytes, SocksAddr)>> for UdpChannelSend {
    fn from(sender: Sender<(Bytes, SocksAddr)>) -> Self {
        Self(Mutex::new(PollSender::new(sender)))
    }
}

impl UdpSend for UdpChannelSend {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &Bytes,
        addr: &SocksAddr,
    ) -> Poll<Result<usize, SError>> {
        let mut sender = self.0.lock().unwrap();
        ready!(sender.poll_reserve(cx)).map_err(|_| SError::InboundUnavailable)?;
        sender
            .send_item((buf.clone(), addr.clone()))
            .map_err(|_| SError::InboundUnavailable)?;
        Poll::Ready(Ok(buf.len()))
    }
}

impl UdpRecv for Receiver<(Bytes, SocksAddr)> {
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr), SError>> {
        self.poll_recv(cx)
            .map(|packet| packet.ok_or(SError::OutboundUnavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Wake, Waker},
    };

    #[derive(Default)]
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn addr() -> SocksAddr {
        "127.0.0.1:53".parse().unwrap()
    }

    #[tokio::test]
    async fn full_channel_wakes_sender_and_delivers_once() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send((Bytes::from_static(b"first"), addr())).unwrap();
        let sender = UdpChannelSend::from(tx);
        let wake = Arc::new(WakeCount::default());
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let packet = Bytes::from_static(b"second");
        assert!(sender.poll_send_to(&mut cx, &packet, &addr()).is_pending());
        assert_eq!(rx.try_recv().unwrap().0, b"first"[..]);
        assert!(wake.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(
            sender.poll_send_to(&mut cx, &packet, &addr()),
            Poll::Ready(Ok(6))
        ));
        assert_eq!(rx.try_recv().unwrap(), (packet, addr()));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn canceled_send_and_busy_try_send_do_not_deliver_stale_packets() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send((Bytes::new(), addr())).unwrap();
        let sender: crate::AnyUdpSend = Arc::new(UdpChannelSend::from(tx));
        {
            let mut sending =
                std::pin::pin!(sender.send_to(Bytes::from_static(b"canceled"), addr()));
            assert!(
                sending
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert!(matches!(
            sender
                .try_send_to(Bytes::from_static(b"busy"), addr())
                .await,
            Err(SError::UdpSenderBusy(_))
        ));
        rx.try_recv().unwrap();
        sender
            .send_to(Bytes::from_static(b"current"), addr())
            .await
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().0, b"current"[..]);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn empty_receiver_wakes_and_reports_channel_closure() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut receiver: crate::AnyUdpRecv = Box::new(rx);
        let wake = Arc::new(WakeCount::default());
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(receiver.poll_recv_from(&mut cx).is_pending());
        tx.try_send((Bytes::new(), addr())).unwrap();
        assert!(wake.0.load(Ordering::Relaxed) > 0);
        assert_eq!(receiver.recv_from().await.unwrap(), (Bytes::new(), addr()));
        drop(tx);
        assert!(matches!(
            receiver.recv_from().await,
            Err(SError::OutboundUnavailable)
        ));
    }

    #[tokio::test]
    async fn closed_channel_reports_send_error() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let sender = UdpChannelSend::from(tx);
        drop(rx);
        assert!(matches!(
            sender.send_to(Bytes::new(), addr()).await,
            Err(SError::InboundUnavailable)
        ));
        assert!(matches!(
            sender.try_send_to(Bytes::new(), addr()).await,
            Err(SError::InboundUnavailable)
        ));
    }
}
