use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Weak},
};

use bytes::Bytes;
use error::SError;
use msgs::socks5::SocksAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, info};

pub mod config;
pub mod direct;
pub mod error;
#[cfg(feature = "mixed")]
pub mod http;
#[cfg(feature = "mixed")]
pub mod mixed;
pub mod msgs;
mod observe;
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
    #[allow(dead_code)]
    user_context: Option<UserContext>,
}

pub struct UdpSession<I = AnyUdpRecv, O = AnyUdpSend> {
    pub recv: I,
    pub send: O,
    /// Control stream, should be kept alive during session.
    stream: Option<AnyTcp>,
    bind_addr: SocksAddr,
    #[allow(dead_code)]
    user_context: Option<UserContext>,
}
#[derive(Clone)]
pub struct UserContext {
    pub username: UserName,
    pub conn_handle: Weak<dyn Stoppable>,
    pub conn_id: u64,
}

pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdpSend = Arc<dyn UdpSend>;
pub type AnyUdpRecv = Box<dyn UdpRecv>;
pub trait TcpTrait: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl TcpTrait for TcpStream {}

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
        }
    }

    pub async fn run(self) -> Result<(), SError> {
        self.run_until(shutdown_signal()).await
    }

    /// Run all listeners until the supplied shutdown future completes.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> Result<(), SError> {
        if self.inbounds.is_empty() || self.outbounds.is_empty() {
            return Err(SError::InvalidConfig(
                "inbounds and outbounds must not be empty".into(),
            ));
        }
        if !self.outbounds.contains_key(&self.default_outbound) {
            return Err(SError::InvalidConfig(
                "default outbound does not exist".into(),
            ));
        }
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
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let mut tasks = tokio::task::JoinSet::new();
        for (tag, mut inbound) in self.inbounds {
            let outbound_tag = self.default_outbound.clone();
            let outbound = self.outbounds[&outbound_tag].clone();
            let mut stopped = stopped.clone();
            tasks.spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = stopped.changed() => break,
                        req = inbound.accept() => match req {
                            Ok(req) => {
                                tokio::select! {
                                    biased;
                                    _ = stopped.changed() => break,
                                    result = outbound.handle(req) => {
                                        if let Err(error) = result {
                                            error!(inbound = %tag, outbound = %outbound_tag, %error, "error handling request");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                error!(inbound = %tag, %error, "error accepting request");
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
                inbound.shutdown().await
            });
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
