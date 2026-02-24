use std::io;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp;
use tokio::select;
use tokio::sync::Mutex;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::RwLock,
};

use crate::proxy_transform::{ProxyJoin, prepend_stream};
use crate::{AnyTcp, UdpRecv, UdpSession};
use crate::{
    AnyUdpSend, ProxyRequest, SDecode, SEncode, TcpSession, UdpSend,
    error::{SError, SResult},
    msgs::{socks5::SocksAddr, squic::SQReq},
    proxy_transform::ProxyTransform,
};

#[derive(Debug, Clone)]
pub(crate) struct SqShimServer {}

#[async_trait::async_trait]
impl ProxyTransform for SqShimServer {
    async fn transform(&self, mut proxy: ProxyRequest) -> SResult<ProxyRequest> {
        let mut tcp_session = match proxy {
            ProxyRequest::Tcp(tcp_session) => tcp_session,
            ProxyRequest::Udp(udp_session) => {
                return Err(SError::ProtocolViolation(
                    "udp proxy request can't be unwrapped by shim server".into(),
                ));
            }
        };

        let req = SQReq::decode(&mut tcp_session.stream).await?;
        tracing::trace!("received SQReq");
        match req {
            SQReq::SQConnect(socks_addr) => {
                tcp_session.dst = socks_addr;
                Ok(ProxyRequest::Tcp(tcp_session))
            }
            SQReq::SQBind(socks_addr) => unimplemented!(),
            SQReq::SQAssociatOverDatagram(socks_addr) => unimplemented!(),
            SQReq::SQAssociatOverStream(socks_addr) => {
                let (reader, writer) = tokio::io::split(tcp_session.stream);
                Ok(ProxyRequest::Udp(UdpSession {
                    recv: Box::new(TcpToUdpRecv(reader)),
                    send: Arc::new(TcpToUdpSend(Mutex::new(writer))),
                    stream: None,
                    bind_addr: socks_addr,
                }))
            }
            SQReq::SQAuthenticate(_) => unimplemented!(),
        }
    }
}
#[derive(Debug, Clone)]

pub(crate) struct SqShimClient;

#[async_trait::async_trait]
impl ProxyJoin for SqShimClient {
    async fn join(&self, proxy: ProxyRequest, mut stream: AnyTcp) -> SResult<()> {
        match proxy {
            ProxyRequest::Tcp(mut tcp_session) => {
                let req = SQReq::SQConnect(tcp_session.dst.clone());
                req.encode(&mut stream).await?;
                tracing::trace!("sent SQReq");
                tokio::io::copy_bidirectional(&mut tcp_session.stream, &mut stream).await?;
                Ok(())
            }
            ProxyRequest::Udp(mut udp_session) => loop {
                select! {
                    Ok((bytes, addr)) = udp_session.recv.recv_from() => {
                        addr.encode(&mut stream).await?;
                        (bytes.len() as u16).encode(&mut stream).await?;
                        stream.write_all(&bytes).await?;
                    }
                    Ok((bytes, addr)) = async {
                        let addr = SocksAddr::decode(&mut stream).await?;
                        let len = u16::decode(&mut stream).await?;
                        let mut buf = vec![0; len as usize];
                        stream.read_exact(&mut buf).await?;
                        Ok::<(Bytes, SocksAddr), SError>((Bytes::from(buf), addr))
                    } => {
                        udp_session.send.send_to(bytes, addr).await?;
                    }
                }
            },
        }
    }
}

struct TcpToUdpSend<T: AsyncWrite>(Mutex<T>);
struct TcpToUdpRecv<T: AsyncRead>(T);

#[async_trait::async_trait]
impl<T: AsyncWrite + Unpin + Send + Sync> UdpSend for TcpToUdpSend<T> {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let mut stream = self.0.lock().await;
        addr.encode(&mut *stream).await?;
        (buf.len() as u16).encode(&mut *stream).await?;
        stream.write_all(&buf).await?;
        Ok(buf.len())
    }
}

#[async_trait::async_trait]
impl<T: AsyncRead + Unpin + Send + Sync> UdpRecv for TcpToUdpRecv<T> {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let addr = SocksAddr::decode(&mut self.0).await?;
        let len = u16::decode(&mut self.0).await?;
        let mut buf = vec![0; len as usize];
        self.0.read_exact(&mut buf).await?;
        Ok((Bytes::from(buf), addr))
    }
}

#[cfg(test)]
mod test_udp_to_tcp;
