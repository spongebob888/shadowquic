

use std::net::ToSocketAddrs;

use tokio::{io::{AsyncRead, AsyncWrite}, net::TcpStream};

use crate::{error::SError, Outbound, UdpSocketTrait};

pub struct DirectOut;
impl<T: AsyncRead + AsyncWrite + Unpin, U: UdpSocketTrait> Outbound<T,U> for DirectOut {
    async fn handle(&mut self, req: crate::ProxyRequest<T, U>) -> anyhow::Result<(), crate::error::SError> {
        match req {
            crate::ProxyRequest::Tcp(mut tcp_session) => {
                let dst = tcp_session.dst.to_socket_addrs()?.next().ok_or(SError::DomainResolveFailed)?;
                let mut upstream = TcpStream::connect(dst).await?;
                let(_,_) = tokio::io::copy_bidirectional_with_sizes(&mut tcp_session.stream,
                     &mut upstream, 1024*16,1024*16).await?;

            },
            crate::ProxyRequest::Udp(udp_session) => todo!(),
        }

        Ok(())

    }
}