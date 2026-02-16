use super::UdpToTcp;
use crate::{
    UdpRecv, UdpSend,
    error::SError,
    msgs::{SDecode, SEncode, socks5::SocksAddr},
};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

struct MockUdpSend {
    tx: mpsc::Sender<(Bytes, SocksAddr)>,
}

#[async_trait::async_trait]
impl UdpSend for MockUdpSend {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let len = buf.len();
        self.tx
            .send((buf, addr))
            .await
            .map_err(|_| SError::InboundUnavailable)?;
        Ok(len)
    }
}

struct MockUdpRecv {
    rx: mpsc::Receiver<(Bytes, SocksAddr)>,
}

#[async_trait::async_trait]
impl UdpRecv for MockUdpRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        self.rx.recv().await.ok_or(SError::InboundUnavailable)
    }
}

#[tokio::test]
async fn test_write_to_udp() {
    let (tx, mut rx) = mpsc::channel(1);
    let send = MockUdpSend { tx };
    let (_, rx_dummy) = mpsc::channel(1);
    let recv = MockUdpRecv { rx: rx_dummy };

    let mut stream = UdpToTcp::new(send, recv);

    let addr = SocksAddr::from_domain("example.com".to_string(), 80);
    let data = Bytes::from("hello world");

    // Construct the stream payload: [Addr][Len][Data]
    let mut buf = Vec::new();
    addr.encode(&mut buf).await.unwrap();
    (data.len() as u16).encode(&mut buf).await.unwrap();
    buf.extend_from_slice(&data);

    // Write to stream
    stream.write_all(&buf).await.unwrap();
    stream.flush().await.unwrap();

    // Verify mock received the packet
    let (got_data, got_addr) = rx.recv().await.unwrap();
    assert_eq!(got_data, data);
    assert_eq!(got_addr, addr);
}

#[tokio::test]
async fn test_read_from_udp() {
    let (tx_dummy, _) = mpsc::channel(1);
    let send = MockUdpSend { tx: tx_dummy };
    let (tx, rx) = mpsc::channel(1);
    let recv = MockUdpRecv { rx };

    let mut stream = UdpToTcp::new(send, recv);

    let addr = SocksAddr::from_domain("example.com".to_string(), 80);
    let data = Bytes::from("response data");

    // Enqueue packet
    tx.send((data.clone(), addr.clone())).await.unwrap();

    // Read from stream
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();

    // Verify content
    let mut slice = &buf[..n];
    let got_addr = SocksAddr::decode(&mut slice).await.unwrap();
    let got_len = u16::decode(&mut slice).await.unwrap();

    let mut got_data = vec![0u8; got_len as usize];
    slice.read_exact(&mut got_data).await.unwrap();

    assert_eq!(got_addr, addr);
    assert_eq!(got_len, data.len() as u16);
    assert_eq!(Bytes::from(got_data), data);
}
