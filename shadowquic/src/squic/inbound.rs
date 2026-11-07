use bytes::Bytes;
use std::{collections::HashMap, pin::Pin, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::mpsc::{Sender, channel},
};
use tracing::{Instrument, info, info_span, trace};

use crate::{
    AnyUdpRecv, AnyUdpSend, ProxyRequest, StatsContext, Stoppable, TcpSession, TcpTrait,
    UdpSession, UserContext,
    config::AuthUser,
    error::{SError, SResult},
    msgs::{
        SEncode,
        socks5::SocksAddr,
        squic::{
            ExtOpcodeConn, ExtOpcodeUser, SQ_REQ_AUTHENTICATE, SQExtError, SQExtOpcode, SQReq,
            SunnyCredential, UserStats,
        },
    },
    quic::QuicConnection,
    squic::wait_sunny_auth,
    utils::{dual_socket::ToCanonical, log::fetch_add_req_cnt},
};

use super::{SQConn, handle_udp_packet_recv, handle_udp_recv_ctrl, handle_udp_send};

pub type SunnyQuicUsers = Arc<HashMap<SunnyCredential, String>>;

#[async_trait::async_trait]
pub trait UserManager: Send + Sync {
    async fn add_user(&self, user: AuthUser) -> Result<(), SQExtError>;
    async fn remove_user(&self, username: &str) -> Result<(), SQExtError>;
    async fn list_users(&self) -> Result<Vec<String>, SQExtError>;
    async fn get_user_stats(&self, username: &str) -> Result<UserStats, SQExtError>;
    async fn get_all_stats(&self) -> Result<Vec<UserStats>, SQExtError>;
    async fn kill_user_conns(&self, username: &str) -> Result<(), SQExtError>;
    async fn clear_user_stats(&self, username: &str) -> Result<(), SQExtError>;
    async fn clear_all_stats(&self) -> Result<(), SQExtError>;
}

struct TraceCtx {
    req_id: u32,
    inbound_span: tracing::Span,
}

tokio::task_local! {
    static TRACE_CTX: TraceCtx;
}

#[derive(Clone)]
pub struct SQServerConn<C: QuicConnection> {
    pub inner: SQConn<C>,
    pub users: SunnyQuicUsers,
    pub user_manager: Option<Arc<dyn UserManager>>,
}
impl<C: QuicConnection> SQServerConn<C> {
    pub async fn handle_connection(
        self: Arc<Self>,
        req_send: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        let conn_clone = self.inner.clone();
        let fut1 = async move { handle_udp_packet_recv(conn_clone).await };

        let fut2 = async move {
            let conn = &self.inner;
            while conn.close_reason().is_none() {
                select! {
                    bi = conn.accept_bi() => {
                        let (send, recv, id) = bi?;
                        let req_id = fetch_add_req_cnt();
                        let span = info_span!("bistream", id = id, req_id);
                        trace!(event = "squic:bidirectional_stream_accepted", "bistream accepted");
                        let self_clone = self.clone();
                        tokio::spawn(
                                TRACE_CTX.scope(TraceCtx { req_id, inbound_span: tracing::Span::current() }, self_clone.handle_bistream(send, recv, req_send.clone()))
                                .instrument(span),
                        );
                    },
                }
            }
            Ok(())
        };
        tokio::try_join!(fut1, fut2)?;
        Ok(())
    }
    async fn handle_bistream(
        self: Arc<Self>,
        send: C::SendStream,
        mut recv: C::RecvStream,
        req_send: Sender<ProxyRequest>,
    ) -> Result<(), SError> {
        // We can direct decode the whole SQReq.
        // Attackers may create malicious extention with long vector
        // to exhaust server memory
        let cmd = SQReq::decode_tag(&mut recv).await?;
        let mut user = None;
        if cmd != SQ_REQ_AUTHENTICATE {
            user = Some(wait_sunny_auth(&self.inner).await?);
        }

        let req = SQReq::decode_with_tag(cmd, &mut recv).await?;
        let (req_id, inbound_span) = TRACE_CTX.with(|ctx| (ctx.req_id, ctx.inbound_span.clone()));
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
                let user = user.unwrap();
                info!(event = "proxy:tcp_request_accepted", dst = %dst, "tcp connect request accepted");
                let tcp: TcpSession = TcpSession {
                    stream: Box::new(Unsplit { s: send, r: recv }),
                    dst,
                    src_addr: Some(self.inner.conn.remote_address().to_canonical()),
                    user_context: UserContext {
                        inbound_tag: String::new(),
                        preferred_outbound: None,
                        dns_query: Vec::new(),
                        stats: Some(StatsContext {
                            username: user,
                            conn_handle: Arc::downgrade(&(self.clone() as Arc<dyn Stoppable>)),
                            conn_id: self.inner.conn.peer_id(),
                        }),
                        req_id: req_id,
                        ..Default::default()
                    },
                };
                req_send
                    .send(ProxyRequest::Tcp(tcp))
                    .await
                    .map_err(|_| SError::OutboundUnavailable)?;
            }
            ref req @ (SQReq::SQAssociatOverDatagram(ref dst)
            | SQReq::SQAssociatOverStream(ref dst)) => {
                let user = user.unwrap();
                info!(event = "proxy:udp_associate_request_accepted", bind_addr = %dst, "udp associate request accepted");
                let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
                let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
                let publish_request = async {
                    let udp = UdpSession::<AnyUdpRecv, AnyUdpSend> {
                        recv: Box::new(udp_recv),
                        send: Arc::new(udp_send),
                        stream: None,
                        bind_addr: dst.clone(),
                        dst: dst.clone(),
                        src_addr: Some(self.inner.conn.remote_address().to_canonical()),
                        user_context: UserContext {
                            inbound_tag: String::new(),
                            preferred_outbound: None,
                            dns_query: Vec::new(),
                            stats: Some(StatsContext {
                                username: user,
                                conn_handle: Arc::downgrade(&(self.clone() as Arc<dyn Stoppable>)),
                                conn_id: self.inner.conn.peer_id(),
                            }),
                            req_id: req_id,
                            ..Default::default()
                        },
                    }
                    .wait_first_packet()
                    .await?;
                    tracing::trace!(event = "proxy:udp_request_accepted", dst = %udp.dst,"udp request accepted");
                    req_send
                        .send(ProxyRequest::Udp(udp))
                        .await
                        .map_err(|_| SError::OutboundUnavailable)
                };
                let local_send = Arc::new(local_send);
                let fut1 = handle_udp_send(
                    send,
                    Box::new(local_recv),
                    self.inner.clone(),
                    req == &SQReq::SQAssociatOverStream(dst.clone()),
                );
                let fut2 = handle_udp_recv_ctrl(recv, local_send, self.inner.clone());
                tokio::try_join!(fut1, fut2, publish_request)?;
            }
            SQReq::SQAuthenticate(passwd_hash) => {
                if let Some(name) = self.users.get(passwd_hash.as_ref()) {
                    inbound_span.record("user", name.as_str());
                    tracing::debug!(
                        event = "security:authentication_succeeded",
                        "sunnyquic authentication succeeded"
                    );
                    self.inner
                        .authed
                        .set(Ok(name.clone()))
                        .expect("repeated authentication!");
                } else {
                    tracing::error!(
                        event = "security:authentication_failed",
                        "authentication failed"
                    );
                    // 263 is tested result by connecting with sunnyquic client to
                    // cloudflare.com:443
                    self.inner.close(263, &[]);
                    return Err(SError::SunnyAuthError("Wrong password/username".into()));
                }
            }
            SQReq::SQExtension(ext_opcode) => {
                self.handle_extension(ext_opcode, send, recv).await?;
            }
            SQReq::SQBind(_) => {
                tracing::error!(
                    event = "proxy:unsupported_command",
                    "bind cmd is not implemented"
                );
                return Err(SError::ProtocolUnimpl);
            }
        }
        Ok(())
    }
    pub(crate) async fn handle_extension(
        &self,
        ext_opcode: SQExtOpcode,
        mut send: C::SendStream,
        mut _recv: C::RecvStream,
    ) -> SResult<()> {
        match ext_opcode {
            SQExtOpcode::Conn(conn_opcode) => match conn_opcode {
                ExtOpcodeConn::GetConnStats => {
                    let stats = self.inner.get_conn_stats().ok_or(SQExtError::NotAvailable);
                    stats.encode(&mut send).await?;
                }
            },
            SQExtOpcode::User(user_opcode) => {
                self.handle_user_extension(user_opcode, &mut send).await?;
            }
        }
        Ok(())
    }

    async fn handle_user_extension(
        &self,
        user_opcode: ExtOpcodeUser,
        send: &mut C::SendStream,
    ) -> SResult<()> {
        let authed_user = wait_sunny_auth(&self.inner).await?;
        if !authed_user.starts_with("admin") {
            (Err::<(), SQExtError>(SQExtError::PermissionDenied))
                .encode(send)
                .await?;
            return Ok(());
        }

        let user_manager = match self.user_manager.as_ref() {
            Some(user_manager) => user_manager,
            None => {
                (Err::<(), SQExtError>(SQExtError::NotAvailable))
                    .encode(send)
                    .await?;
                return Ok(());
            }
        };
        match user_opcode {
            ExtOpcodeUser::AddUser(user) => {
                info!(event = "security:user_add_requested", username = %user.username, "adding user");
                user_manager.add_user(user).await.encode(send).await?;
            }
            ExtOpcodeUser::RemoveUser(username) => {
                info!(event = "security:user_remove_requested", username = %username, "removing user");
                user_manager
                    .remove_user(&username)
                    .await
                    .encode(send)
                    .await?;
            }
            ExtOpcodeUser::ListUsers => {
                user_manager.list_users().await.encode(send).await?;
            }
            ExtOpcodeUser::GetUserStats(username) => {
                info!(event = "security:user_stats_requested", username = %username, "getting user stats");
                user_manager
                    .get_user_stats(&username)
                    .await
                    .encode(send)
                    .await?;
            }
            ExtOpcodeUser::GetAllStats => {
                info!(
                    event = "security:all_user_stats_requested",
                    "getting all user stats"
                );
                user_manager.get_all_stats().await.encode(send).await?;
            }
            ExtOpcodeUser::KillUserConn(username) => {
                info!(event = "security:user_connections_close_requested", username = %username, "killing user connections");
                user_manager
                    .kill_user_conns(&username)
                    .await
                    .encode(send)
                    .await?;
            }
            ExtOpcodeUser::ClearUserStats(username) => {
                info!(event = "security:user_stats_clear_requested", username = %username, "clearing user stats");
                user_manager
                    .clear_user_stats(&username)
                    .await
                    .encode(send)
                    .await?;
            }
            ExtOpcodeUser::ClearAllStats => {
                info!(
                    event = "security:all_user_stats_clear_requested",
                    "clearing all user stats"
                );
                user_manager.clear_all_stats().await.encode(send).await?;
            }
        }
        Ok(())
    }
}

impl<C: QuicConnection> Stoppable for SQServerConn<C> {
    fn stop(&self) {
        self.inner.conn.close(0, &[]);
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
