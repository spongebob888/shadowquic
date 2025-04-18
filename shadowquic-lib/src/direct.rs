use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use bytes::BytesMut;
use tokio::{
    net::{TcpStream, UdpSocket, lookup_host},
    sync::Mutex,
};
use tracing::{Instrument, error, trace, trace_span};

use crate::{
    Outbound, UdpSession,
    error::SError,
    msgs::socks5::{AddrOrDomain, SOCKS5_ADDR_TYPE_DOMAIN_NAME, SocksAddr, VarVec},
};
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
                crate::ProxyRequest::Udp(udp_session) => {
                    handle_udp(udp_session).await?;
                }
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

#[derive(Default, Clone)]
struct DnsResolve(Arc<Mutex<HashMap<Vec<u8>, SocketAddr>>>);
impl DnsResolve {
    async fn resolve(&self, socks: SocksAddr) -> Result<SocketAddr, SError> {
        if let AddrOrDomain::Domain(x) = &socks.addr {
            if let Some(v) = self.0.lock().await.get(&x.contents) {
                Ok(*v)
            } else {
                let s = resolve(&socks).await?;
                self.0.lock().await.insert(x.contents.clone(), s);
                Ok(s)
            }
        } else {
            Ok(resolve(&socks).await?)
        }
    }
    async fn inv_resolve(&self, addr: &SocketAddr) -> SocksAddr {
        if let Some(add) = self.0.lock().await.iter().find(|x| x.1 == addr) {
            SocksAddr {
                atype: SOCKS5_ADDR_TYPE_DOMAIN_NAME,
                addr: AddrOrDomain::Domain(VarVec {
                    len: add.0.len() as u8,
                    contents: add.0.clone(),
                }),
                port: addr.port(),
            }
        } else {
            (*addr).into()
        }
    }
}

async fn resolve(socks: &SocksAddr) -> Result<SocketAddr, SError> {
    let mut s = match socks.addr.clone() {
        crate::msgs::socks5::AddrOrDomain::V4(x) => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::V6(x) => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(x)), 0)
        }
        crate::msgs::socks5::AddrOrDomain::Domain(var_vec) => lookup_host((
            String::from_utf8(var_vec.contents).map_err(|_| SError::DomainResolveFailed)?,
            socks.port,
        ))
        .await?
        .next()
        .ok_or(SError::DomainResolveFailed)?,
    };
    s.set_port(socks.port);
    Ok(s)
}

async fn handle_udp(udp_session: UdpSession) -> Result<(), SError> {
    trace!("associating udp to {}", udp_session.dst);
    let dst = udp_session
        .dst
        .to_socket_addrs()?
        .next()
        .ok_or(SError::DomainResolveFailed)?;
    trace!("resolved to {}", dst);
    //let upstream = UdpSocket::bind(dst).await?;
    let upstream = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let upstream_clone = upstream.clone();
    let mut downstream = udp_session.recv;

    let dns_cache = DnsResolve::default();
    let dns_cache_clone = dns_cache.clone();
    let fut1 = async move {
        loop {
            let mut buf_send = BytesMut::new();
            buf_send.resize(1600, 0);
            trace!("recv upstream");
            let (len, dst) = upstream.recv_from(&mut buf_send).await?;
            trace!("udp request reply from:{}", dst);
            let dst = dns_cache_clone.inv_resolve(&dst).await;
            trace!("udp source inverse resolved to:{}", dst);
            let buf = buf_send.freeze();
            trace!("udp recved:{} bytes", len);
            let len = udp_session.send.send_to(buf.slice(..len), dst).await?;
        }
        Ok(()) as Result<(), SError>
    };
    tokio::spawn(fut1);
    loop {
        let (buf, dst) = downstream.recv_from().await?;

        trace!("udp request to:{}", dst);
        let dst = dns_cache.resolve(dst).await?;
        trace!("udp resolve to:{}", dst);
        let siz = upstream_clone.send_to(&buf, dst).await?;
        trace!("udp request sent:{}bytes", siz);
    }
}
