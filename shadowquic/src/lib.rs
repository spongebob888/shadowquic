use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Weak},
};

use bytes::Bytes;
use error::SError;
use msgs::socks5::SocksAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{Instrument, error, info, info_span};

pub mod config;
pub mod direct;
pub mod dns;
mod drop_outbound;
pub mod error;
#[cfg(feature = "mixed")]
pub mod http;
#[cfg(feature = "mixed")]
pub mod mixed;
pub mod msgs;
mod observe;
#[cfg(feature = "plugin")]
pub mod plugin;
pub mod quic;
pub mod shadowquic;
pub mod socks;
pub mod squic;
pub mod sunnyquic;
#[cfg(all(feature = "tproxy", target_os = "linux"))]
pub mod tproxy;
pub mod utils;

pub use msgs::SDecode;
pub use msgs::SEncode;
#[cfg(test)]
mod manager_tests;
pub enum ProxyRequest<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend> {
    Tcp(TcpSession<T>),
    Udp(UdpSession<I, O>),
}

impl ProxyRequest {
    pub fn dst(&self) -> &SocksAddr {
        match self {
            ProxyRequest::Tcp(TcpSession { dst, .. }) => dst,
            ProxyRequest::Udp(UdpSession { dst, .. }) => dst,
        }
    }

    pub(crate) fn set_dst(&mut self, dst: SocksAddr) {
        match self {
            ProxyRequest::Tcp(session) => session.dst = dst,
            ProxyRequest::Udp(session) => session.dst = dst,
        }
    }

    pub(crate) fn set_inbound_tag(&mut self, tag: String) {
        match self {
            ProxyRequest::Tcp(session) => session.user_context.inbound_tag = tag,
            ProxyRequest::Udp(session) => session.user_context.inbound_tag = tag,
        }
    }
}
/// Udp socket only use immutable reference to self
/// So it can be safely wrapped by Arc and cloned to work in duplex way.
#[async_trait]
pub trait UdpSend: Send + Sync + Unpin {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError>; // addr is proxy addr
}
#[async_trait]
pub trait UdpRecv: Send + Sync + Unpin {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError>; // socksaddr is proxy addr
}
pub trait Stoppable: Send + Sync {
    fn stop(&self);
}
pub type UserName = String;
pub struct TcpSession<IO = AnyTcp> {
    pub stream: IO,
    pub dst: SocksAddr,
    /// Address of the client that opened this session, when available.
    pub src_addr: Option<SocketAddr>,
    #[allow(dead_code)]
    user_context: UserContext,
}

pub struct UdpSession<I = AnyUdpRecv, O = AnyUdpSend> {
    pub recv: I,
    pub send: O,
    /// Control stream, should be kept alive during session.
    stream: Option<AnyTcp>,
    bind_addr: SocksAddr,
    /// Destination of the first received datagram.
    /// Only used for routing. The true sending destination is determined per-datagram by UdpRecv.
    dst: SocksAddr,
    /// Address of the client that opened this session, when available.
    /// It is the address of the TCP control stream and
    /// may differs from the source address of the first datagram
    pub src_addr: Option<SocketAddr>,
    #[allow(dead_code)]
    user_context: UserContext,
}
impl UdpSession {
    /// Wait for the first datagram and retain it for the outbound receiver.
    pub(crate) async fn from_recv(
        send: AnyUdpSend,
        mut recv: AnyUdpRecv,
        stream: Option<AnyTcp>,
        bind_addr: SocksAddr,
        user_context: UserContext,
    ) -> Result<Self, SError> {
        let first = recv.recv_from().await?;
        Ok(Self {
            dst: first.1.clone(),
            src_addr: stream
                .as_ref()
                .and_then(|stream| stream.peer_addr())
                .or(user_context.src_addr),
            recv: Box::new(FirstPacketUdpRecv {
                first: Some(first),
                inner: recv,
            }),
            send,
            stream,
            bind_addr,
            user_context,
        })
    }
}

struct FirstPacketUdpRecv {
    first: Option<(Bytes, SocksAddr)>,
    inner: AnyUdpRecv,
}

#[async_trait]
impl UdpRecv for FirstPacketUdpRecv {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        if let Some(packet) = self.first.take() {
            Ok(packet)
        } else {
            self.inner.recv_from().await
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DnsQuery {
    pub name: String,
    pub record_type: u16,
}

/// Per-session context, present even when statistics are not tracked.
#[derive(Clone, Default)]
pub struct UserContext {
    pub src_addr: Option<SocketAddr>,
    pub inbound_tag: String,
    pub dns_query: Vec<DnsQuery>,
    pub stats: Option<StatsContext>,
}

/// Authenticated connection metadata used for statistics and connection control.
#[derive(Clone)]
pub struct StatsContext {
    pub username: UserName,
    pub conn_handle: Weak<dyn Stoppable>,
    pub conn_id: u64,
}

pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdpSend = Arc<dyn UdpSend>;
pub type AnyUdpRecv = Box<dyn UdpRecv>;
pub trait TcpTrait: AsyncRead + AsyncWrite + Unpin + Send + Sync {
    fn peer_addr(&self) -> Option<SocketAddr> {
        None
    }
}
impl TcpTrait for TcpStream {
    fn peer_addr(&self) -> Option<SocketAddr> {
        TcpStream::peer_addr(self).ok()
    }
}

#[async_trait]
pub trait Inbound<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend>: Send + Sync + Unpin {
    async fn accept(&mut self) -> Result<ProxyRequest<T, I, O>, SError>;
    async fn init(&self) -> Result<(), SError> {
        Ok(())
    }
    /// Called once on graceful shutdown, flush persistent state here.
    async fn shutdown(&self) -> Result<(), SError> {
        Ok(())
    }
}

#[async_trait]
pub trait Outbound<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend>: Send + Sync + Unpin {
    async fn handle(&self, req: ProxyRequest<T, I, O>) -> Result<(), SError>;
}

#[async_trait]
impl UdpSend for Sender<(Bytes, SocksAddr)> {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let siz = buf.len();
        self.send((buf, addr))
            .await
            .map_err(|_| SError::InboundUnavailable)?;
        Ok(siz)
    }
}
#[async_trait]
impl UdpRecv for Receiver<(Bytes, SocksAddr)> {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let r = self.recv().await.ok_or(SError::OutboundUnavailable)?;
        Ok(r)
    }
}
pub struct Manager {
    pub inbounds: HashMap<String, Box<dyn Inbound>>,
    pub outbounds: HashMap<String, Arc<dyn Outbound>>,
    /// Tag of the first configured outbound, used by every inbound.
    pub default_outbound: String,
    #[cfg(feature = "plugin")]
    pub router: Option<Arc<plugin::router::Router>>,

    #[cfg(feature = "dns-server")]
    pub resolver_manager: Option<Arc<dns::ResolverManager>>,
}

/// Shared routing state for independently scheduled requests.
struct RequestDispatcher {
    outbounds: HashMap<String, Arc<dyn Outbound>>,
    default_outbound: String,
    #[cfg(feature = "plugin")]
    router: Option<Arc<plugin::router::Router>>,
}

impl RequestDispatcher {
    async fn dispatch(&self, req: ProxyRequest) {
        #[cfg(feature = "plugin")]
        let mut req = req;
        #[cfg(feature = "plugin")]
        let outbound_tag = match self.router.as_ref() {
            Some(router) => {
                let mut context = plugin::router::RouteContext::from_request(&req);
                match router
                    .route(&mut context)
                    .instrument(info_span!("route"))
                    .await
                {
                    Ok(outbound_tag) => match context.destination() {
                        Ok(dst) => {
                            req.set_dst(dst);
                            outbound_tag
                        }
                        Err(error) => {
                            error!(%error, "invalid rewritten destination");
                            return;
                        }
                    },
                    Err(error) => {
                        error!(%error, "routing request failed");
                        return;
                    }
                }
            }
            None => self.default_outbound.clone(),
        };
        #[cfg(not(feature = "plugin"))]
        let outbound_tag = self.default_outbound.clone();
        let Some(outbound) = self.outbounds.get(&outbound_tag).cloned() else {
            error!(outbound = %outbound_tag, "router selected an unknown outbound");
            return;
        };
        tracing::debug!(outbound = %outbound_tag, dst = %req.dst(), "routing request");
        if let Err(error) = outbound
            .handle(req)
            .instrument(info_span!("outbound", tag = %outbound_tag))
            .await
        {
            error!(outbound = %outbound_tag, %error, "error handling request");
        }
    }
}

/// Resolves when a shutdown signal is received (Ctrl-C, plus SIGTERM on unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

impl Manager {
    /// Construct a manager for one inbound/outbound pair.
    pub fn single(inbound: Box<dyn Inbound>, outbound: Arc<dyn Outbound>) -> Self {
        Self {
            inbounds: HashMap::from([("inbound".into(), inbound)]),
            outbounds: HashMap::from([("outbound".into(), outbound)]),
            default_outbound: "outbound".into(),
            #[cfg(feature = "plugin")]
            router: None,
            #[cfg(feature = "dns-server")]
            resolver_manager: None,
        }
    }

    pub async fn run(self) -> Result<(), SError> {
        self.run_until(shutdown_signal()).await
    }

    /// Run listeners and dispatch accepted requests concurrently until shutdown.
    /// Pending dispatch tasks are cancelled before shutting down their inbound.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> Result<(), SError> {
        for (tag, inbound) in &self.inbounds {
            if let Err(error) = inbound.init().await {
                error!(inbound = %tag, %error, "inbound initialization failed");
                for (tag, inbound) in &self.inbounds {
                    if let Err(error) = inbound.shutdown().await {
                        error!(inbound = %tag, %error, "inbound shutdown failed");
                    }
                }
                return Err(error);
            }
        }
        let dispatcher = Arc::new(RequestDispatcher {
            outbounds: self.outbounds,
            default_outbound: self.default_outbound,
            #[cfg(feature = "plugin")]
            router: self.router,
        });
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let mut tasks = tokio::task::JoinSet::new();
        for (tag, mut inbound) in self.inbounds {
            let inbound_span = info_span!("inbound",
             tag = %tag,
             src = tracing::field::Empty,
             user = tracing::field::Empty,
             id = tracing::field::Empty, // mainly for quic id
            );
            let dispatcher = dispatcher.clone();
            let mut stopped = stopped.clone();
            tasks.spawn(async move {
                let mut requests = tokio::task::JoinSet::new();
                'accepting: loop {
                    // Completing a dispatch must not cancel a partially accepted request.
                    let req = {
                        let accept = inbound.accept();
                        tokio::pin!(accept);
                        loop {
                            tokio::select! {
                                biased;
                                _ = stopped.changed() => break 'accepting,
                                Some(result) = requests.join_next(), if !requests.is_empty() => {
                                    if let Err(error) = result {
                                        error!(inbound = %tag, %error, "request task failed");
                                    }
                                }
                                req = &mut accept => break req,
                            }
                        }
                    };
                    match req {
                        Ok(mut req) => {
                            req.set_inbound_tag(tag.clone());
                            let dispatcher = dispatcher.clone();
                            requests.spawn(async move {
                                dispatcher.dispatch(req).await;
                            }.in_current_span());
                        }
                        Err(error) => {
                            error!(inbound = %tag, %error, "error accepting request");
                            tokio::select! {
                                biased;
                                _ = stopped.changed() => break,
                                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                            }
                        }
                    }
                }
                requests.shutdown().await;
                inbound.shutdown().await
            }.instrument(inbound_span));
        }
        tokio::pin!(shutdown);
        let mut result = Ok(());
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown requested, persisting users and stats");
            }
            task = tasks.join_next() => {
                result = match task {
                    Some(Ok(Err(error))) => Err(error),
                    Some(Err(error)) => Err(SError::Io(std::io::Error::other(error))),
                    _ => Err(SError::InboundUnavailable),
                };
            }
        }
        let _ = stop.send(true);
        while let Some(task) = tasks.join_next().await {
            let task_result = match task {
                Ok(result) => result,
                Err(error) => Err(SError::Io(std::io::Error::other(error))),
            };
            if let Err(error) = task_result {
                error!(%error, "inbound shutdown failed");
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        result
    }
}
