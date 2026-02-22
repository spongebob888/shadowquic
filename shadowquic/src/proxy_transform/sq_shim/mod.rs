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

use crate::proxy_transform::prepend_stream;
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

impl SqShimClient {
    pub async fn join(proxy: ProxyRequest, mut stream: AnyTcp) -> SResult<()> {
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

pub struct UdpToTcp<S, R> {
    send: Option<S>,
    recv: Option<R>,

    // Write state
    write_buf: BytesMut,
    send_fut: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = (S, BytesMut, SResult<()>)> + Send>>,
    >,

    // Read state
    read_buf: BytesMut,
    recv_fut:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = (R, SResult<BytesMut>)> + Send>>>,
}

impl<S: UdpSend + 'static, R: UdpRecv + 'static> UdpToTcp<S, R> {
    pub fn new(send: S, recv: R) -> Self {
        Self {
            send: Some(send),
            recv: Some(recv),
            write_buf: BytesMut::new(),
            send_fut: None,
            read_buf: BytesMut::new(),
            recv_fut: None,
        }
    }
}

impl<S: UdpSend + 'static, R: UdpRecv + 'static> AsyncRead for UdpToTcp<S, R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.read_buf.is_empty() {
                let amt = std::cmp::min(buf.remaining(), this.read_buf.len());
                buf.put_slice(&this.read_buf[..amt]);
                use bytes::Buf;
                this.read_buf.advance(amt);
                return std::task::Poll::Ready(Ok(()));
            }

            if let Some(fut) = this.recv_fut.as_mut() {
                let (recv, res) = std::task::ready!(fut.as_mut().poll(cx));
                this.recv_fut = None;
                this.recv = Some(recv);
                match res {
                    Ok(data) => this.read_buf = data,
                    Err(e) => {
                        return std::task::Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::Other,
                            e,
                        )));
                    }
                }
                continue;
            }

            if this.recv_fut.is_none() {
                let mut recv = this.recv.take().expect("recv is missing");
                this.recv_fut = Some(Box::pin(async move {
                    match recv.recv_from().await {
                        Ok((data, addr)) => {
                            let mut buf = Vec::new();
                            if let Err(e) = addr.encode(&mut buf).await {
                                return (recv, Err(e));
                            }
                            if let Err(e) = (data.len() as u16).encode(&mut buf).await {
                                return (recv, Err(e));
                            }
                            use tokio::io::AsyncWriteExt;
                            if let Err(e) = buf.write_all(&data).await {
                                return (recv, Err(e.into()));
                            }
                            // BytesMut::from(Vec) works because Vec implements Into<BytesMut>? No.
                            // BytesMut::from(&buf[..]) works.
                            (recv, Ok(BytesMut::from(buf.as_slice())))
                        }
                        Err(e) => (recv, Err(e)),
                    }
                }));
            }
        }
    }
}

impl<S: UdpSend + 'static, R: UdpRecv + 'static> AsyncWrite for UdpToTcp<S, R> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(fut) = this.send_fut.as_mut() {
            if let std::task::Poll::Ready((send, leftover, res)) = fut.as_mut().poll(cx) {
                this.send_fut = None;
                this.send = Some(send);
                if let Err(e) = res {
                    return std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
                }
                let mut new_buf = leftover;
                new_buf.extend_from_slice(&this.write_buf);
                this.write_buf = new_buf;
            }
        }

        this.write_buf.extend_from_slice(buf);
        let ret_len = buf.len();

        if this.send_fut.is_none() && !this.write_buf.is_empty() {
            let send = this.send.take().expect("send missing");
            let mut data = std::mem::take(&mut this.write_buf);

            this.send_fut = Some(Box::pin(async move {
                loop {
                    let start_len = data.len();
                    let mut slice = &data[..];
                    let addr = match SocksAddr::decode(&mut slice).await {
                        Ok(a) => a,
                        Err(SError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            break;
                        }
                        Err(e) => return (send, data, Err(e)),
                    };

                    let len = match u16::decode(&mut slice).await {
                        Ok(l) => l,
                        Err(SError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            break;
                        }
                        Err(e) => return (send, data, Err(e)),
                    };

                    if slice.len() < len as usize {
                        break;
                    }

                    let mut packet = vec![0u8; len as usize];
                    use tokio::io::AsyncReadExt;
                    if let Err(e) = slice.read_exact(&mut packet).await {
                        return (send, data, Err(e.into()));
                    }

                    if let Err(e) = send.send_to(Bytes::from(packet), addr).await {
                        return (send, data, Err(e));
                    }

                    let consumed = start_len - slice.len();
                    use bytes::Buf;
                    data.advance(consumed);
                    if data.is_empty() {
                        break;
                    } // Optimization
                }
                (send, data, Ok(()))
            }));
        }

        std::task::Poll::Ready(Ok(ret_len))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if let Some(fut) = this.send_fut.as_mut() {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready((send, leftover, res)) => {
                        this.send_fut = None;
                        this.send = Some(send);
                        if let Err(e) = res {
                            return std::task::Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::Other,
                                e,
                            )));
                        }
                        let mut new_buf = leftover;
                        new_buf.extend_from_slice(&this.write_buf);
                        this.write_buf = new_buf;
                    }
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }

            if this.write_buf.is_empty() {
                return std::task::Poll::Ready(Ok(()));
            }

            // Spawn if not empty
            let send = this.send.take().expect("send missing");
            let mut data = std::mem::take(&mut this.write_buf);

            this.send_fut = Some(Box::pin(async move {
                loop {
                    let start_len = data.len();
                    let mut slice = &data[..];
                    let addr = match SocksAddr::decode(&mut slice).await {
                        Ok(a) => a,
                        Err(SError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            break;
                        }
                        Err(e) => return (send, data, Err(e)),
                    };
                    let len = match u16::decode(&mut slice).await {
                        Ok(l) => l,
                        Err(SError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            break;
                        }
                        Err(e) => return (send, data, Err(e)),
                    };
                    if slice.len() < len as usize {
                        break;
                    }
                    let mut packet = vec![0u8; len as usize];
                    use tokio::io::AsyncReadExt;
                    if let Err(e) = slice.read_exact(&mut packet).await {
                        return (send, data, Err(e.into()));
                    }
                    if let Err(e) = send.send_to(Bytes::from(packet), addr).await {
                        return (send, data, Err(e));
                    }
                    let consumed = start_len - slice.len();
                    use bytes::Buf;
                    data.advance(consumed);
                    if data.is_empty() {
                        break;
                    }
                }
                (send, data, Ok(()))
            }));
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[cfg(test)]
mod test_udp_to_tcp;
