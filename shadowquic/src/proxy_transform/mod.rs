use std::{io::Cursor, pin::Pin};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, Chain};

use crate::{ProxyRequest, TcpTrait, error::SResult};

pub mod sq_shim;
pub mod tls;
pub mod tcp;

#[async_trait::async_trait]
pub trait ProxyTransform {
    async fn transform(&self, proxy: ProxyRequest) -> SResult<ProxyRequest>;
}

impl<U: tokio::io::AsyncRead + Unpin + Send + Sync, T: TcpTrait> TcpTrait for PrependStream<U, T> {}
struct PrependStream<U: AsyncRead, T: TcpTrait>(Chain<U, T>);

fn prepend_stream<T: TcpTrait>(stream: T, buf: Bytes) -> PrependStream<Cursor<Bytes>, T> {
    let cursor = Cursor::new(buf);
    let chain = cursor.chain(stream);

    PrependStream(chain)
}

impl<U: tokio::io::AsyncRead + Unpin, T: TcpTrait> AsyncWrite for PrependStream<U, T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(self.0.get_mut().1).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(self.0.get_mut().1).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(self.0.get_mut().1).poll_shutdown(cx)
    }
}

impl<U: tokio::io::AsyncRead + Unpin, T: TcpTrait> AsyncRead for PrependStream<U, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
