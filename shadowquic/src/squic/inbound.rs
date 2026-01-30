use async_trait::async_trait;
use bytes::Bytes;
use std::{marker::PhantomData, pin::Pin, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::{OnceCell, SetOnce, mpsc::{Receiver, Sender, channel}},
};
use tracing::{Instrument, Level, error, event, info, trace, trace_span};

use crate::{
    Inbound, ProxyRequest, TcpSession, TcpTrait, UdpSession,
    config::ShadowQuicServerCfg,
    error::SError,
    msgs::{
        squic::{SQCmd, SQReq, SUN_QUIC_AUTH_LEN},
        socks5::{SDecode, SocksAddr},
    },
    quic::QuicConnection,
};

use super::{IDStore, SQConn, handle_udp_packet_recv, handle_udp_recv_ctrl, handle_udp_send};

use crate::quic::QuicServer;

pub type SunQuicUsers = Arc<Vec<(String, [u8; SUN_QUIC_AUTH_LEN])>>;


#[derive(Clone)]
pub struct SQServerConn<C: QuicConnection> {
    pub inner: SQConn<C>,
    pub users: Arc<Vec<(String, [u8; SUN_QUIC_AUTH_LEN])>>,
}
impl<C: QuicConnection> SQServerConn<C> {
    pub async fn handle_connection(self, req_send: Sender<ProxyRequest>) -> Result<(), SError> {
        let conn = &self.inner;
        event!(
            Level::INFO,
            "incoming from {} accepted",
            conn.remote_address()
        );
        let conn_clone = self.inner.clone();
        tokio::spawn(async move {
            let _ = handle_udp_packet_recv(conn_clone).in_current_span().await;
        });

        while conn.close_reason().is_none() {
            select! {
                bi = conn.accept_bi() => {
                    let (send, recv, id) = bi?;
                    let span = trace_span!("bistream", id = id);
                    trace!("bistream accepted");
                    tokio::spawn(self.clone().handle_bistream(send, recv, req_send.clone()).instrument(span).in_current_span());
                },
            }
        }
        Ok(())
    }
    async fn handle_bistream(
        self,
        send: C::SendStream,
        mut recv: C::RecvStream,
        req_send: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let req = SQReq::decode(&mut recv).await?;

        // let rate: f32 = (self.0.conn.stats().path.lost_packets as f32)
        //     / ((self.0.conn.stats().path.sent_packets + 1) as f32);
        // info!(
        //     "packet_loss_rate:{:.2}%, rtt:{:?}, mtu:{}",
        //     rate * 100.0,
        //     self.0.conn.rtt(),
        //     self.0.conn.stats().path.current_mtu,
        // );
        match req {
            SQReq::SQConnect(dst) => {
                info!(
                    "connect request: {}->{} accepted",
                    self.inner.remote_address(),
                    dst.clone()
                );
                let tcp: TcpSession = TcpSession {
                    stream: Box::new(Unsplit { s: send, r: recv }),
                    dst,
                };
                req_send
                    .send(ProxyRequest::Tcp(tcp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
            }
            ref req @ (SQReq::SQAssociatOverDatagram(ref dst) | SQReq::SQAssociatOverStream(ref dst)) => {
                info!("association request to {} accepted", dst.clone());
                let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
                let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
                let udp: UdpSession = UdpSession {
                    send: Arc::new(udp_send),
                    recv: Box::new(udp_recv),
                    stream: None,
                    dst: dst.clone(),
                };
                let local_send = Arc::new(local_send);
                req_send
                    .send(ProxyRequest::Udp(udp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
                let fut1 = handle_udp_send(
                    send,
                    Box::new(local_recv),
                    self.inner.clone(),
                    req == &SQReq::SQAssociatOverStream(dst.clone()),
                );
                let fut2 = handle_udp_recv_ctrl(recv, local_send, self.inner);
                tokio::try_join!(fut1, fut2)?;
            }
            _ => {}
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct Unsplit<S, R> {
    pub s: S,
    pub r: R,
}
impl<S: AsyncWrite + Unpin + Sync + Send, R: AsyncRead + Unpin + Sync + Send> TcpTrait
    for Unsplit<S, R>
{
}

impl<S: AsyncWrite + Unpin, R: AsyncRead + Unpin> AsyncRead for Unsplit<S, R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.as_mut().r).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin, R: AsyncRead + Unpin> AsyncWrite for Unsplit<S, R> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.as_mut().s).poll_shutdown(cx)
    }
}
