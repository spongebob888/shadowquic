use std::{io::Cursor, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use tokio::{net::UdpSocket, sync::OnceCell};
use tracing::{debug, warn};

use crate::{
    UdpRecv, UdpSend,
    error::SError,
    msgs::socks5::{self, SocksAddr, UdpReqHeader},
};

use crate::msgs::{SDecode, SEncode};
pub mod inbound;
pub mod outbound;

#[derive(Clone)]
pub struct UdpSocksWrap(Arc<UdpSocket>, OnceCell<SocketAddr>); // remote addr
#[async_trait]
impl UdpRecv for UdpSocksWrap {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let mut buf = BytesMut::new();
        buf.resize(2000, 0);

        let (len, dst) = self.0.recv_from(&mut buf).await?;
        let mut cur = Cursor::new(buf);
        let mut req = socks5::UdpReqHeader::decode(&mut cur).await?;
        req.dst.canonicalize_ip();
        if req.frag != 0 {
            warn!(
                event = "socks:fragmented_datagram_dropped",
                "dropping fragmented udp datagram "
            );
            return Err(SError::ProtocolUnimpl);
        }
        let headsize: usize = cur.position().try_into().unwrap();
        let buf = cur.into_inner();
        self.1
            .get_or_init(|| async {
                let _ = self.0.connect(dst).await;
                dst
            })
            .await;
        let buf = buf.freeze();
        // assert!(len < buf.len());
        Ok((buf.slice(headsize..len), req.dst))
    }
}
#[async_trait]
impl UdpSend for UdpSocksWrap {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let payload_len = buf.len();
        if !addr.is_encodable() {
            // Only this datagram is lost, not the association.
            debug!(
                event = "socks:datagram_dropped",
                "udp datagram dropped: its address name is too long"
            );
            return Ok(0);
        }
        let reply = UdpReqHeader {
            rsv: 0,
            frag: 0,
            dst: addr,
        };
        let mut buf_new = BytesMut::with_capacity(1600);
        let header = Vec::new();
        let mut cur = Cursor::new(header);
        reply.encode(&mut cur).await?;
        let header = cur.into_inner();
        buf_new.put(Bytes::from(header));
        buf_new.put(buf);

        self.0.send(&buf_new).await?;
        Ok(payload_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unsendable_address_is_dropped_not_sent_cut_short() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(peer.local_addr().unwrap()).await.unwrap();
        let wrap = UdpSocksWrap(Arc::new(socket), OnceCell::new());

        let unsendable = SocksAddr::from_domain("a".repeat(300), 53);
        wrap.send_to(Bytes::from_static(b"lost"), unsendable)
            .await
            .expect("the association goes on");
        let dst: SocksAddr = "127.0.0.1:53".parse().unwrap();
        wrap.send_to(Bytes::from_static(b"ping"), dst.clone())
            .await
            .unwrap();

        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), peer.recv(&mut buf))
            .await
            .expect("the datagram to a valid address is sent")
            .unwrap();
        let mut cur = Cursor::new(&buf[..n]);
        let header = UdpReqHeader::decode(&mut cur).await.unwrap();
        assert_eq!(header.dst, dst, "the first datagram sent is the second one");
    }
}
