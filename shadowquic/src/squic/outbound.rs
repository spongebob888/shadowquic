use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, Sender, channel};

use tokio::io::AsyncReadExt;
use tracing::Instrument;
use tracing::{Level, error, info, span, trace};

use crate::config::AuthUser;
use crate::error::SResult;
use crate::msgs::squic::{
    ConnStats, ExtOpcodeConn, ExtOpcodeUser, SQExtError, SQExtOpcode, UserStats,
};
use crate::{
    ProxyRequest,
    error::SError,
    msgs::{SDecode, SEncode, socks5::SocksAddr, squic::SQReq},
    quic::QuicConnection,
    squic::{handle_udp_recv_ctrl, handle_udp_send},
};

use super::{SQConn, SQConnStats, inbound::Unsplit};

/// Upper bound on how long opening a bi-stream for a request may take before the
/// connection is treated as wedged. Wide enough for a legitimate stream-credit
/// wait under healthy load and narrow enough to bound how long the accept loop
/// can be blocked.
///
/// Waiting on the QUIC idle timeout instead does not help: with
/// `keep_alive_interval` set the connection never goes idle, so a peer that
/// stopped granting stream credit keeps the connection alive indefinitely and
/// only this bound can break the stall.
const HANDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`dispatch`] did with the request, and with the connection.
pub enum Dispatch {
    /// Handed to a relay task; the bi-stream is open.
    Sent,
    /// The bi-stream open timed out and the connection was closed, so the caller
    /// must drop it from its cache.
    Wedged,
    /// The bi-stream open timed out but the connection was left in place, because
    /// it is still moving application data.
    StillActive,
}

/// Dispatches `req` under [`HANDLE_TIMEOUT`] and starts its relay.
///
/// [`handle_request`] spawns the relay and returns before it runs, so this bound
/// covers only opening the bi-stream. quinn's `open_bi` waits for the peer to
/// raise `MAX_STREAMS` instead of failing, and every caller awaits the dispatch
/// inline, so without the bound one stuck open stops its caller from accepting
/// anything else — which is how a single wedged peer turns into an instance that
/// silently stops serving.
///
/// On timeout the connection is closed only when no application data moved
/// either. [`data_progress`](QuicConnection::data_progress) counts STREAM and
/// DATAGRAM frames, which keep-alive PINGs and pure ACKs do not touch, so a
/// connection still carrying them is under ordinary stream-credit backpressure
/// from live sessions, and closing it would abort all of them. A transport that
/// cannot report the counter at all is never found silent, so its connection is
/// left in place.
pub async fn dispatch<C: QuicConnection>(
    req: ProxyRequest,
    conn: SQConn<C>,
    over_stream: bool,
) -> Result<Dispatch, SError> {
    let progress_before = conn.data_progress();
    match tokio::time::timeout(
        HANDLE_TIMEOUT,
        handle_request(req, conn.clone(), over_stream),
    )
    .await
    {
        Ok(result) => result.map(|()| Dispatch::Sent),
        Err(_) => {
            if progress_before.is_some() && progress_before == conn.data_progress() {
                error!(
                    "bi-stream open timed out after {}s with no application data moving, dropping the connection",
                    HANDLE_TIMEOUT.as_secs()
                );
                QuicConnection::close(&conn.conn, 0, &[]);
                Ok(Dispatch::Wedged)
            } else {
                error!(
                    "bi-stream open timed out after {}s, connection is still carrying application data",
                    HANDLE_TIMEOUT.as_secs()
                );
                Ok(Dispatch::StillActive)
            }
        }
    }
}

/// Handling a proxy request and starting proxy task with given squic connection
pub async fn handle_request<C: QuicConnection>(
    req: ProxyRequest,
    conn: SQConn<C>,
    over_stream: bool,
) -> Result<(), SError> {
    let (mut send, recv, id) = QuicConnection::open_bi(&conn.conn).await?;
    let _span = span!(Level::INFO, "bistream", id = id);
    let conn_clone = conn.clone();
    tokio::spawn(
        async move {
            let _ = print_stats(&conn_clone).await;
        }
        .in_current_span(),
    );
    let fut = async move {
        match req {
            crate::ProxyRequest::Tcp(mut tcp_session) => {
                info!(dst = %tcp_session.dst, "bistream opened for tcp");
                let req = SQReq::SQConnect(tcp_session.dst.clone());
                req.encode(&mut send).await?;
                trace!(dst = %tcp_session.dst, "tcp connect req header sent");

                let u = tokio::io::copy_bidirectional(
                    &mut Unsplit { s: send, r: recv },
                    &mut tcp_session.stream,
                )
                .await?;

                info!(
                    "request:{} finished, upload:{}bytes,download:{}bytes",
                    tcp_session.dst, u.1, u.0
                );
            }

            crate::ProxyRequest::Udp(udp_session) => {
                info!(bind_addr = %udp_session.bind_addr, "bistream opened for udp association");
                let req = if over_stream {
                    SQReq::SQAssociatOverStream(udp_session.bind_addr.clone())
                } else {
                    SQReq::SQAssociatOverDatagram(udp_session.bind_addr.clone())
                };

                req.encode(&mut send).await?;
                trace!("udp associate req header sent");

                let fut2 = handle_udp_recv_ctrl(recv, udp_session.send.clone(), conn.clone());
                let fut1 = handle_udp_send(send, udp_session.recv, conn, over_stream);

                let fut3 = async {
                    if udp_session.stream.is_none() {
                        return Ok(());
                    }
                    let mut buf = [0u8];
                    udp_session
                        .stream
                        .unwrap()
                        .read_exact(&mut buf)
                        .await
                        .map_err(|x| SError::UDPSessionClosed(x.to_string()))?;
                    error!("unexpected data received from socks control stream");
                    Err(SError::UDPSessionClosed(
                        "unexpected data received from socks control stream".into(),
                    )) as Result<(), SError>
                };

                tokio::try_join!(fut1, fut2, fut3)?;
                info!("udp association to {} ended", udp_session.bind_addr.clone());
            }
        }
        Ok(()) as Result<(), SError>
    };
    tokio::spawn(
        async {
            let _ = fut.await.map_err(|x| error!("{}", x));
        }
        .instrument(_span),
    );
    Ok(())
}

/// Helper function to create new stream for proxy dstination
#[allow(dead_code)]
pub async fn connect_tcp<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    dst: SocksAddr,
) -> Result<Unsplit<C::SendStream, C::RecvStream>, crate::error::SError> {
    let conn = sq_conn;

    let (mut send, recv, _id) = conn.open_bi().await?;

    info!(dst = %dst, "bistream opened for tcp");
    let req = SQReq::SQConnect(dst.clone());
    req.encode(&mut send).await?;
    trace!("tcp connect req header sent");

    Ok(Unsplit { s: send, r: recv })
}

pub async fn get_peer_conn_stats<C: QuicConnection>(
    sq_conn: &SQConn<C>,
) -> SResult<Result<ConnStats, SQExtError>> {
    let (mut send, mut recv, _id) = sq_conn.open_bi().await?;
    let req = SQReq::SQExtension(SQExtOpcode::Conn(ExtOpcodeConn::GetConnStats));
    req.encode(&mut send).await?;
    let response = Result::<ConnStats, SQExtError>::decode(&mut recv).await?;
    Ok(response)
}

pub async fn add_user<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    username: &str,
    password: &str,
) -> SResult<Result<(), SQExtError>> {
    send_user_extension(
        sq_conn,
        ExtOpcodeUser::AddUser(AuthUser {
            username: username.to_owned(),
            password: password.to_owned(),
        }),
    )
    .await
}

pub async fn remove_user<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    username: &str,
) -> SResult<Result<(), SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::RemoveUser(username.to_owned())).await
}

pub async fn list_users<C: QuicConnection>(
    sq_conn: &SQConn<C>,
) -> SResult<Result<Vec<String>, SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::ListUsers).await
}

pub async fn get_user_stats<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    username: &str,
) -> SResult<Result<UserStats, SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::GetUserStats(username.to_owned())).await
}

pub async fn get_all_stats<C: QuicConnection>(
    sq_conn: &SQConn<C>,
) -> SResult<Result<Vec<UserStats>, SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::GetAllStats).await
}

pub async fn kill_user_conns<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    username: &str,
) -> SResult<Result<(), SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::KillUserConn(username.to_owned())).await
}

pub async fn clear_user_stats<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    username: &str,
) -> SResult<Result<(), SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::ClearUserStats(username.to_owned())).await
}

pub async fn clear_all_stats<C: QuicConnection>(
    sq_conn: &SQConn<C>,
) -> SResult<Result<(), SQExtError>> {
    send_user_extension(sq_conn, ExtOpcodeUser::ClearAllStats).await
}

async fn send_user_extension<C: QuicConnection, R: SDecode>(
    sq_conn: &SQConn<C>,
    opcode: ExtOpcodeUser,
) -> SResult<Result<R, SQExtError>> {
    let (mut send, mut recv, _id) = sq_conn.open_bi().await?;
    let req = SQReq::SQExtension(SQExtOpcode::User(opcode));
    req.encode(&mut send).await?;
    let response = Result::<R, SQExtError>::decode(&mut recv).await?;
    Ok(response)
}

async fn print_stats<C: QuicConnection>(sq_conn: &SQConn<C>) -> SResult<()> {
    let Ok(mut cached_stats) = sq_conn.stats.try_lock() else {
        return Ok(()); // if we can't get the lock, it means another task is already printing stats, so we skip this one.
    };
    if cached_stats
        .as_ref()
        .is_some_and(|stats| stats.time.elapsed() < Duration::from_secs(10))
    {
        return Ok(());
    }

    let uplink = sq_conn.get_conn_stats().ok_or(SError::ProtocolUnimpl)?;
    log_conn_stats(&uplink, "uplink");

    let downlink =
        match tokio::time::timeout(Duration::from_secs(10), get_peer_conn_stats(sq_conn)).await {
            Ok(Ok(Ok(stats))) => stats,
            _ => {
                trace!("failed to get peer conn stats. Api may not be implemented");
                return Err(SError::ProtocolUnimpl);
            }
        };
    log_conn_stats(&downlink, "downlink");

    *cached_stats = Some(SQConnStats {
        uplink,
        downlink,
        time: Instant::now(),
    });
    Ok(())
}

fn log_conn_stats(stats: &ConnStats, direction: &'static str) {
    info!(
        packet_loss_rate=%format!("{:.2}%", stats.lost_packets as f32 / (stats.sent_packets + 1) as f32 * 100.0),
        rtt = %format!("{:.1}ms", stats.rtt),
        mtu = stats.current_mtu,
        "{} connection stats",
        direction
    );
}

/// associate a udp socket in the remote server
/// return a socket-like send, recv handle.
#[allow(dead_code)]
pub async fn associate_udp<C: QuicConnection>(
    sq_conn: &SQConn<C>,
    dst: SocksAddr,
    over_stream: bool,
) -> Result<(Sender<(Bytes, SocksAddr)>, Receiver<(Bytes, SocksAddr)>), SError> {
    let conn = sq_conn;

    let (mut send, recv, _id) = conn.open_bi().await?;

    info!(bind_addr = %dst, "bistream opened for udp association");

    let req = if over_stream {
        SQReq::SQAssociatOverStream(dst.clone())
    } else {
        SQReq::SQAssociatOverDatagram(dst.clone())
    };
    req.encode(&mut send).await?;
    let (local_send, udp_recv) = channel::<(Bytes, SocksAddr)>(10);
    let (udp_send, local_recv) = channel::<(Bytes, SocksAddr)>(10);
    let local_send = Arc::new(local_send);
    let fut2 = handle_udp_recv_ctrl(recv, local_send, conn.clone());
    let fut1 = handle_udp_send(send, Box::new(local_recv), conn.clone(), over_stream);

    tokio::spawn(
        async {
            match tokio::try_join!(fut1, fut2) {
                Err(e) => error!("udp association ended due to {}", e),
                Ok(_) => trace!("udp association ended"),
            }
        }
        .in_current_span(),
    );

    Ok((udp_send, udp_recv))
}
