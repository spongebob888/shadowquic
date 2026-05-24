use std::io;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::OnceCell;

use crate::{AnyUdpRecv, UdpRecv, error::SError, msgs::socks5::SocksAddr};

struct UdpRecvDst {
    recv: AnyUdpRecv,
    /// Destination address of the first packet, it may not be the true destination. Since udp can send to any destination.
    first_dst: OnceCell<SocksAddr>,
    buffer: Option<Bytes>,
}

impl UdpRecvDst {
    pub fn new(recv: AnyUdpRecv) -> Self {
        Self {
            recv,
            buffer: None,
            first_dst: OnceCell::new(),
        }
    }
    async fn get_dst(&mut self) -> Result<SocksAddr, SError> {
        if let Some(dst) = self.first_dst.get() {
            Ok(dst.clone())
        } else {
            let (buf, dst) = self.recv.recv_from().await?;
            // This should be safe, since get_dst is exclusive
            self.first_dst
                .set(dst.clone())
                .map_err(|_| SError::Io(io::Error::other("dst already set")))?;
            self.buffer = Some(buf);
            Ok(dst)
        }
    }
}
pub(crate) async fn get_udp_first_dst(udp: AnyUdpRecv) -> Result<(AnyUdpRecv, SocksAddr), SError> {
    let mut udp = UdpRecvDst::new(udp);
    let dst = udp.get_dst().await?;
    Ok((Box::new(udp), dst))
}

#[async_trait]
impl UdpRecv for UdpRecvDst {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        if let Some(buf) = self.buffer.take() {
            let dst = self
                .first_dst
                .get()
                .expect("buffer and dst must be set the same time")
                .clone();
            return Ok((buf, dst));
        }
        self.recv.recv_from().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::SocketAddr;

    /// A simple in-memory `UdpRecv` that yields a pre-loaded queue of
    /// `(payload, dst)` pairs, then returns an error to mark exhaustion.
    struct MockRecv {
        queue: VecDeque<(Bytes, SocksAddr)>,
    }

    #[async_trait]
    impl UdpRecv for MockRecv {
        async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
            self.queue
                .pop_front()
                .ok_or_else(|| SError::Io(io::Error::other("exhausted")))
        }
    }

    fn addr(port: u16) -> SocksAddr {
        let s: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        SocksAddr::from(s)
    }

    /// Verify that wrapping a recv with `get_udp_first_dst`:
    /// 1. Returns the destination of the first packet.
    /// 2. Does NOT drop the first packet — it must still be readable.
    /// 3. Does NOT duplicate the first packet — every payload appears exactly once.
    /// 4. Preserves order of subsequent packets.
    #[tokio::test]
    async fn first_dst_preserves_data_exactly_once() {
        let packets: Vec<(Bytes, SocksAddr)> = vec![
            (Bytes::from_static(b"first"), addr(1111)),
            (Bytes::from_static(b"second"), addr(2222)),
            (Bytes::from_static(b"third"), addr(3333)),
        ];
        let mock = MockRecv {
            queue: packets.clone().into(),
        };
        let recv: AnyUdpRecv = Box::new(mock);

        let (mut wrapped, first_dst) = get_udp_first_dst(recv)
            .await
            .expect("get_udp_first_dst should succeed");

        // (1) destination of first packet is reported correctly
        assert_eq!(first_dst, packets[0].1);

        // (2)+(3)+(4) drain the wrapped recv and compare against the original sequence
        let mut drained: Vec<(Bytes, SocksAddr)> = Vec::new();
        while let Ok(item) = wrapped.recv_from().await {
            drained.push(item);
        }

        assert_eq!(
            drained, packets,
            "packets must be delivered in original order, with none dropped or repeated"
        );
    }

    /// Even if no further packets are read, the buffered first packet must
    /// still be retrievable exactly once and not silently consumed by `get_dst`.
    #[tokio::test]
    async fn first_packet_is_buffered_not_dropped() {
        let mock = MockRecv {
            queue: vec![(Bytes::from_static(b"only"), addr(4242))].into(),
        };
        let recv: AnyUdpRecv = Box::new(mock);

        let (mut wrapped, dst) = get_udp_first_dst(recv).await.unwrap();
        assert_eq!(dst, addr(4242));

        let (buf, dst2) = wrapped
            .recv_from()
            .await
            .expect("first packet must be readable");
        assert_eq!(buf.as_ref(), b"only");
        assert_eq!(dst2, addr(4242));

        // Second read drains the underlying mock, which is now empty -> error.
        assert!(
            wrapped.recv_from().await.is_err(),
            "first packet must not repeat"
        );
    }
}
