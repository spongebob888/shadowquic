use std::{
    future::Future,
    io::Cursor,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{io::ReadBuf, net::UdpSocket};
use tracing::warn;

use crate::{
    UdpRecv, UdpSend,
    error::SError,
    msgs::socks5::{self, SocksAddr, UdpReqHeader},
};

use crate::msgs::{SDecode, SEncode};
pub mod inbound;
pub mod outbound;

#[derive(Clone)]
pub struct UdpSocksWrap(Arc<UdpSocket>, OnceLock<SocketAddr>); // remote addr
impl UdpRecv for UdpSocksWrap {
    fn poll_recv_from(&mut self, cx: &mut Context<'_>) -> Poll<Result<(Bytes, SocksAddr), SError>> {
        let mut buf = BytesMut::new();
        buf.resize(2000, 0);

        let mut read_buf = ReadBuf::new(&mut buf);
        let dst = ready!(self.0.poll_recv_from(cx, &mut read_buf))?;
        let len = read_buf.filled().len();
        let mut cur = Cursor::new(buf);
        // Decoding an in-memory cursor cannot wait for I/O.
        let mut req = ready!(std::pin::pin!(socks5::UdpReqHeader::decode(&mut cur)).poll(cx))?;
        req.dst.canonicalize_ip();
        if req.frag != 0 {
            warn!(
                event = "socks:fragmented_datagram_dropped",
                "dropping fragmented udp datagram "
            );
            return Poll::Ready(Err(SError::ProtocolUnimpl));
        }
        let headsize: usize = cur.position().try_into().unwrap();
        let buf = cur.into_inner();
        self.1.get_or_init(|| {
            let _ = socket2::SockRef::from(&*self.0).connect(&dst.into());
            dst
        });
        let buf = buf.freeze();
        // assert!(len < buf.len());
        Poll::Ready(Ok((buf.slice(headsize..len), req.dst)))
    }
}
impl UdpSend for UdpSocksWrap {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &Bytes,
        addr: &SocksAddr,
    ) -> Poll<Result<usize, SError>> {
        let payload_len = buf.len();
        let reply = UdpReqHeader {
            rsv: 0,
            frag: 0,
            dst: addr.clone(),
        };
        let mut buf_new = BytesMut::with_capacity(1600);
        let header = Vec::new();
        let mut cur = Cursor::new(header);
        // Encoding into a Vec cannot wait for I/O.
        ready!(std::pin::pin!(reply.encode(&mut cur)).poll(cx))?;
        let header = cur.into_inner();
        buf_new.put(Bytes::from(header));
        buf_new.put_slice(buf);

        ready!(self.0.poll_send(cx, &buf_new))?;
        Poll::Ready(Ok(payload_len))
    }
}
