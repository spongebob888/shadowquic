use super::*;
use crate::quic::QuicErrorRepr;
use async_trait::async_trait;
use std::{collections::VecDeque, future::pending, net::SocketAddr};

#[derive(Clone)]
struct DatagramConnection(Arc<Mutex<VecDeque<Bytes>>>);

#[async_trait]
impl QuicConnection for DatagramConnection {
    type SendStream = tokio::io::DuplexStream;
    type RecvStream = tokio::io::DuplexStream;

    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        unreachable!()
    }
    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream, u64), QuicErrorRepr> {
        unreachable!()
    }
    async fn open_uni(&self) -> Result<(Self::SendStream, u64), QuicErrorRepr> {
        unreachable!()
    }
    async fn accept_uni(&self) -> Result<(Self::RecvStream, u64), QuicErrorRepr> {
        pending().await
    }
    async fn read_datagram(&self) -> Result<Bytes, QuicErrorRepr> {
        self.0
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| QuicErrorRepr::QuicConnection("test connection closed".into()))
    }
    async fn send_datagram(&self, _: Bytes) -> Result<(), QuicErrorRepr> {
        unreachable!()
    }
    fn close(&self, _: u64, _: &[u8]) {
        unreachable!()
    }
    fn close_reason(&self) -> Option<QuicErrorRepr> {
        None
    }
    fn remote_address(&self) -> SocketAddr {
        "127.0.0.1:1234".parse().unwrap()
    }
    fn peer_id(&self) -> u64 {
        0
    }
}

struct FailOnceSender {
    failed: std::sync::atomic::AtomicBool,
    sender: crate::UdpChannelSend,
}

impl UdpSend for FailOnceSender {
    fn poll_send_to(
        &self,
        cx: &mut std::task::Context<'_>,
        packet: &Bytes,
        addr: &SocksAddr,
    ) -> std::task::Poll<SResult<usize>> {
        if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return std::task::Poll::Ready(Err(
                std::io::Error::other("test delivery failure").into()
            ));
        }
        self.sender.poll_send_to(cx, packet, addr)
    }
}

#[tokio::test]
async fn delivery_errors_do_not_stop_shared_datagram_receiver() {
    let mut packets = VecDeque::new();
    // A closed association, a transient socket error, another association,
    // and a retry on the socket that previously failed.
    for (id, payload) in [(1, "closed"), (2, "failed"), (3, "healthy"), (2, "retry")] {
        let mut packet = Vec::new();
        SQPacketDatagramHeader { id }
            .encode(&mut packet)
            .await
            .unwrap();
        packet.extend_from_slice(payload.as_bytes());
        packets.push_back(Bytes::from(packet));
    }
    let conn = SQConn::new(
        DatagramConnection(Arc::new(Mutex::new(packets))),
        SetOnce::new_with(Some(Ok("test".into()))),
    );
    let addr: SocksAddr = "127.0.0.1:53".parse().unwrap();
    let (closed, receiver) = tokio::sync::mpsc::channel(1);
    drop(receiver);
    let (healthy, mut received) = tokio::sync::mpsc::channel(4);
    let failing = FailOnceSender {
        failed: Default::default(),
        sender: healthy.clone().into(),
    };
    for (id, sender) in [
        (
            1,
            Arc::new(crate::UdpChannelSend::from(closed)) as AnyUdpSend,
        ),
        (2, Arc::new(failing) as AnyUdpSend),
        (
            3,
            Arc::new(crate::UdpChannelSend::from(healthy)) as AnyUdpSend,
        ),
    ] {
        conn.recv_id_store
            .store_socket(id, (sender, addr.clone()))
            .await;
    }

    let error = tokio::time::timeout(Duration::from_secs(2), handle_udp_packet_recv(conn))
        .await
        .expect("receive loop must finish when transport closes")
        .unwrap_err();
    // Delivery failures are recoverable, but transport errors still end the loop.
    assert!(
        error.to_string().contains("test connection closed"),
        "{error}"
    );
    assert_eq!(
        received.try_recv().unwrap(),
        (Bytes::from_static(b"healthy"), addr.clone())
    );
    assert_eq!(
        received.try_recv().unwrap(),
        (Bytes::from_static(b"retry"), addr)
    );
    assert!(received.try_recv().is_err());
}
