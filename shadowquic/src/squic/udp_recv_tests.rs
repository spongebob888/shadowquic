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
    sender: tokio::sync::mpsc::Sender<(Bytes, SocksAddr)>,
}

#[async_trait]
impl UdpSend for FailOnceSender {
    async fn send_to(&self, packet: Bytes, addr: SocksAddr) -> SResult<usize> {
        if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("test delivery failure").into());
        }
        self.sender.send_to(packet, addr).await
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
    let conn = SQConn {
        conn: DatagramConnection(Arc::new(Mutex::new(packets))),
        authed: Arc::new(SetOnce::new_with(Some(Ok("test".into())))),
        send_id_store: Default::default(),
        recv_id_store: IDStore {
            id_counter: Default::default(),
            inner: Default::default(),
        },
        stats: Default::default(),
        stats_refresh: Default::default(),
    };
    let addr: SocksAddr = "127.0.0.1:53".parse().unwrap();
    let (closed, receiver) = tokio::sync::mpsc::channel(1);
    drop(receiver);
    let (healthy, mut received) = tokio::sync::mpsc::channel(4);
    let failing = FailOnceSender {
        failed: Default::default(),
        sender: healthy.clone(),
    };
    for (id, sender) in [
        (1, Arc::new(closed) as AnyUdpSend),
        (2, Arc::new(failing) as AnyUdpSend),
        (3, Arc::new(healthy) as AnyUdpSend),
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
