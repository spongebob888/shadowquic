use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender, channel};

use tokio::io::AsyncReadExt;
use tracing::Instrument;
use tracing::{Level, debug, error, info, span, trace};

use crate::{
    ProxyRequest,
    error::SError,
    msgs::{
        socks5::{SEncode, SocksAddr},
        squic::SQReq,
    },
    quic::QuicConnection,
    squic::{handle_udp_recv_ctrl, handle_udp_send},
};

use super::{SQConn, inbound::Unsplit};
/// Handling a proxy request and starting proxy task with given squic connection
pub async fn handle_request<C: QuicConnection>(
    req: ProxyRequest,
    conn: SQConn<C>,
    over_stream: bool,
) -> Result<(), SError> {
    let (mut send, recv, id) = QuicConnection::open_bi(&conn.conn).await?;
    let _span = span!(Level::TRACE, "bistream", id = id);
    let fut = async move {
        match req {
            crate::ProxyRequest::Tcp(mut tcp_session) => {
                debug!("bistream opened for tcp dst:{}", tcp_session.dst.clone());
                //let _enter = _span.enter();
                let req = SQReq::SQConnect(tcp_session.dst.clone());
                req.encode(&mut send).await?;
                trace!("tcp connect req header sent");

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
                info!("bistream opened for udp dst:{}", udp_session.dst.clone());
                let req = if over_stream {
                    SQReq::SQAssociatOverStream(udp_session.dst.clone())
                } else {
                    SQReq::SQAssociatOverDatagram(udp_session.dst.clone())
                };
                req.encode(&mut send).await?;
                trace!("udp associate req header sent");
                let fut2 = handle_udp_recv_ctrl(recv, udp_session.send.clone(), conn.clone());
                let fut1 = handle_udp_send(send, udp_session.recv, conn, over_stream);
                // control stream, in socks5 inbound, end of control stream
                // means end of udp association.
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
                info!("udp association to {} ended", udp_session.dst.clone());
            }
        }
        Ok(()) as Result<(), SError>
    };
    tokio::spawn(async {
        let _ = fut.instrument(_span).await.map_err(|x| error!("{}", x));
    });
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

    info!("bistream opened for tcp dst:{}", dst.clone());
    //let _enter = _span.enter();
    let req = SQReq::SQConnect(dst.clone());
    req.encode(&mut send).await?;
    trace!("req header sent");

    Ok(Unsplit { s: send, r: recv })
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

    info!("bistream opened for udp dst:{}", dst.clone());

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

    tokio::spawn(async {
        match tokio::try_join!(fut1, fut2) {
            Err(e) => error!("udp association ended due to {}", e),
            Ok(_) => trace!("udp association ended"),
        }
    });

    Ok((udp_send, udp_recv))
}
