use std::io;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::RwLock,
};

use crate::proxy_transform::prepend_stream;
use crate::{
    AnyUdpSend, ProxyRequest, SDecode, SEncode, TcpSession, UdpSend,
    error::{SError, SResult},
    msgs::{socks5::SocksAddr, squic::SQReq},
    proxy_transform::ProxyTransform,
};
use crate::{UdpRecv, UdpSession};

struct SqShimServer {}

#[async_trait::async_trait]
impl ProxyTransform for SqShimServer {
    async fn transform(&self, mut proxy: ProxyRequest) -> SResult<ProxyRequest> {
        let mut tcp_session = match proxy {
            ProxyRequest::Tcp(tcp_session) => tcp_session,
            ProxyRequest::Udp(udp_session) => return Err(SError::ProtocolViolation),
        };

        let req = SQReq::decode(&mut tcp_session.stream).await?;
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

struct SqShimClient;

#[async_trait::async_trait]
impl ProxyTransform for SqShimClient {
    async fn transform(&self, mut proxy: ProxyRequest) -> SResult<ProxyRequest> {
        let mut vec = Vec::new();

        match proxy {
            ProxyRequest::Tcp(tcp_session) => {
                let req = SQReq::SQConnect(tcp_session.dst.clone());
                req.encode(&mut vec).await?;
                Ok(ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(prepend_stream(tcp_session.stream, Bytes::from(vec))),
                    dst: tcp_session.dst,
                }))
            }
            ProxyRequest::Udp(udp_session) => {
                let req = SQReq::SQAssociatOverDatagram(udp_session.bind_addr);
                req.encode(&mut vec).await?;
                Ok(ProxyRequest::Tcp(TcpSession {
                    stream: todo!(),
                    dst: todo!(),
                }))
            }
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

// struct UdpToTcp<S: UdpSend, T: UdpRecv> {
//     udp_send: Option<S>,
//     udp_recv: Option<T>,
//     send_fut: Option<
//         std::pin::Pin<Box<dyn std::future::Future<Output = (Result<usize, SError>, S)> + Send>>,
//     >,
//     recv_fut: Option<
//         std::pin::Pin<Box<dyn std::future::Future<Output = (Result<(Bytes, T), SError>)> + Send>>,
//     >,
// }
