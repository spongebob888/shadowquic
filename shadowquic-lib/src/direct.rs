use std::net::ToSocketAddrs;

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UdpSocket},
    spawn,
};
use tracing::{Instrument, error, span, trace, trace_span};

use crate::{Outbound, UdpSocketTrait, error::SError};
use async_trait::async_trait;

pub struct DirectOut;

#[async_trait]
impl Outbound for DirectOut {
    async fn handle(
        &mut self,
        req: crate::ProxyRequest,
    ) -> anyhow::Result<(), crate::error::SError> {
        let fut = async {
            match req {
                crate::ProxyRequest::Tcp(mut tcp_session) => {
                    trace!("direct tcp to {}", tcp_session.dst);
                    let dst = tcp_session
                        .dst
                        .to_socket_addrs()?
                        .next()
                        .ok_or(SError::DomainResolveFailed)?;
                    trace!("resolved to {}", dst);
                    let mut upstream = TcpStream::connect(dst).await?;
                    let (_, _) = tokio::io::copy_bidirectional_with_sizes(
                        &mut tcp_session.stream,
                        &mut upstream,
                        1024 * 16,
                        1024 * 16,
                    )
                    .await?;
                }
                crate::ProxyRequest::Udp(mut udp_session) => {
                    trace!("direct tcp to {}", udp_session.dst);
                    let dst = udp_session
                        .dst
                        .to_socket_addrs()?
                        .next()
                        .ok_or(SError::DomainResolveFailed)?;
                    trace!("resolved to {}", dst);
                    let upstream = UdpSocket::bind(dst).await?;
                    let mut buf_recv = [0u8; 1024*10];
                    let mut buf_send = [0u8; 1024*10];
                    tokio::select! {
                       r = udp_session.socket.recv_from(&mut buf_recv) => {

                        let (headlen, len, dst) = r?;
                        let dst = dst
                        .to_socket_addrs()?
                        .next()
                        .ok_or(SError::DomainResolveFailed)?;

                        upstream.send_to(&buf_recv[headlen..len], dst).await?;
                       }
                       r = upstream.recv_from(&mut buf_send) => {

                        let (len, dst) = r?;

                        udp_session.socket.send_to(&buf_recv[..len], dst.into()).await?;
                       }
                    }
                    
                },
            }
            Ok(()) as Result<(), SError>
        };
        let span = trace_span!("direct");
        tokio::spawn(
            async {
                let _ = fut.await.map_err(|x| error!("{}", x));
            }
            .instrument(span),
        );

        Ok(())
    }
}
