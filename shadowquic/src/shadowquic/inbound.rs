use async_trait::async_trait;
use bytes::Bytes;
use std::{pin::Pin, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::mpsc::{Receiver, Sender, channel},
};
use tracing::{Instrument, Level, error, event, info, trace, trace_span};

use crate::{
    Inbound, ProxyRequest, TcpSession, TcpTrait, UdpSession,
    config::ShadowQuicServerCfg,
    error::SError,
    msgs::{
        shadowquic::{SQCmd, SQReq},
        socks5::{SDecode, SocksAddr},
    },
    quic::QuicConnection,
};

use super::{IDStore, SQConn, handle_udp_packet_recv, handle_udp_recv_ctrl, handle_udp_send};

use crate::quic::EndServer;
use crate::quic::QuicServer;
pub struct ShadowQuicServer {
    pub config: ShadowQuicServerCfg,
    request_sender: Sender<ProxyRequest>,
    request: Receiver<ProxyRequest>,
}

impl ShadowQuicServer {
    pub fn new(cfg: ShadowQuicServerCfg) -> Result<Self, SError> {
        let (send, recv) = channel::<ProxyRequest>(10);

        Ok(Self {
            config: cfg,
            request_sender: send,
            request: recv,
        })
    }

    async fn handle_incoming<C: QuicConnection>(
        incom: C,
        req_sender: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let sq_conn = SQServerConn(SQConn {
            conn: incom,
            send_id_store: Default::default(),
            recv_id_store: IDStore {
                id_counter: Default::default(),
                inner: Default::default(),
            },
        });
        let span = trace_span!("quic", id = sq_conn.0.peer_id());
        sq_conn
            .handle_connection(req_sender)
            .instrument(span)
            .await?;

        Ok(())
    }
}

#[async_trait]
impl Inbound for ShadowQuicServer {
    async fn accept(&mut self) -> Result<crate::ProxyRequest, SError> {
        let req = self
            .request
            .recv()
            .await
            .ok_or(SError::InboundUnavailable)?;
        return Ok(req);
    }
    /// Init background job for accepting connection
    async fn init(&self) -> Result<(), SError> {
        let request_sender = self.request_sender.clone();
        let config = self.config.clone();
        let fut = async move {
            let endpoint: EndServer = QuicServer::new(&config)
                .await
                .expect("Failed to listening on udp");
            loop {
                match QuicServer::accept(&endpoint).await {
                    Ok(conn) => {
                        let request_sender = request_sender.clone();
                        let span = trace_span!("quic", id = conn.peer_id());
                        tokio::spawn(
                            async move {
                                Self::handle_incoming(conn, request_sender)
                                    .await
                                    .map_err(|x| error!("{}", x))
                            }
                            .instrument(span)
                            .in_current_span(),
                        );
                    }
                    Err(e) => {
                        error!("Error accepting quic connection: {}", e);
                    }
                }
            }
        };
        tokio::spawn(fut);
        Ok(())
    }
}

#[derive(Clone)]
pub struct SQServerConn<C: QuicConnection>(SQConn<C>);
impl<C: QuicConnection> SQServerConn<C> {
    async fn handle_connection(self, req_send: Sender<ProxyRequest>) -> Result<(), SError> {
        let conn = &self.0;
        event!(
            Level::INFO,
            "incoming from {} accepted",
            conn.remote_address()
        );
        let conn_clone = self.0.clone();
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
        match req.cmd {
            SQCmd::Connect => {
                info!(
                    "connect request: {}->{} accepted",
                    self.0.remote_address(),
                    req.dst.clone()
                );
                let tcp: TcpSession = TcpSession {
                    stream: Box::new(Unsplit { s: send, r: recv }),
                    dst: req.dst,
                };
                req_send
                    .send(ProxyRequest::Tcp(tcp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
            }
            SQCmd::AssociatOverDatagram | SQCmd::AssociatOverStream => {
                info!("association request to {} accepted", req.dst.clone());
                let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
                let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
                let udp: UdpSession = UdpSession {
                    send: Arc::new(udp_send),
                    recv: Box::new(udp_recv),
                    stream: None,
                    dst: req.dst,
                };
                let local_send = Arc::new(local_send);
                req_send
                    .send(ProxyRequest::Udp(udp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
                let fut1 = handle_udp_send(
                    send,
                    Box::new(local_recv),
                    self.0.clone(),
                    req.cmd == SQCmd::AssociatOverStream,
                );
                let fut2 = handle_udp_recv_ctrl(recv, local_send, self.0);
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
