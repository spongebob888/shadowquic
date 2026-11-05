//! Passive, bounded hostname inspection before Lua routing.
mod http;
mod quic;
#[cfg(test)]
mod tests;
mod tls;

use super::replay_stream::ReplayStream;
use crate::{
    AnyUdpRecv, ProxyRequest, UdpRecv,
    config::{SniffProtocol, SnifferCfg},
    error::SError,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore},
};

/// Client-provided metadata, not authentication of the destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SniffResult {
    pub domain: String,
    pub protocol: &'static str,
    pub version: String,
}
impl mlua::UserData for SniffResult {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("domain", |_, this| Ok(this.domain.clone()));
        fields.add_field_method_get("type", |_, this| Ok(this.protocol));
        fields.add_field_method_get("version", |_, this| Ok(this.version.clone()));
    }
}

pub(crate) struct Sniffer {
    config: SnifferCfg,
    concurrent: Semaphore,
    memory: Arc<Semaphore>,
    quic_suite: Option<rustls_jls::quic::Suite>,
}
impl Sniffer {
    pub(crate) fn new(config: SnifferCfg) -> Result<Self, SError> {
        config.validate()?;
        let quic_suite = if config.udp.is_empty() {
            None
        } else {
            Some(quic::suite().ok_or_else(|| {
                SError::InvalidConfig(
                    "QUIC sniffing requires a rustls-jls AES-128-GCM provider (ring or aws-lc-rs)"
                        .into(),
                )
            })?)
        };
        Ok(Self {
            concurrent: Semaphore::new(config.max_concurrent),
            memory: Arc::new(Semaphore::new(config.max_buffered_bytes)),
            config,
            quic_suite,
        })
    }
    pub(crate) async fn sniff(&self, mut request: ProxyRequest) -> Result<ProxyRequest, SError> {
        request.user_context_mut().sniffer = None;
        let udp = matches!(request, ProxyRequest::Udp(_));
        let rules = if udp {
            &self.config.udp
        } else {
            &self.config.tcp
        };
        let mut http = false;
        let mut tls = false;
        let mut quic = false;
        for rule in rules
            .iter()
            .filter(|r| r.ports.is_empty() || r.ports.contains(&request.dst().port))
        {
            for protocol in &rule.protocols {
                match protocol {
                    SniffProtocol::Http => http = true,
                    SniffProtocol::Tls => tls = true,
                    SniffProtocol::Quic => quic = true,
                }
            }
        }
        if !http && !tls && !quic {
            return Ok(request);
        }
        let Ok(_active) = self.concurrent.try_acquire() else {
            return Ok(request);
        };
        let Ok(memory) = self
            .memory
            .clone()
            .try_acquire_many_owned((4 * self.config.max_bytes) as u32)
        else {
            return Ok(request);
        };
        let deadline = Duration::from_millis(self.config.timeout);
        match request {
            ProxyRequest::Tcp(mut session) => {
                let mut prefix = Vec::with_capacity(self.config.max_bytes);
                let mut parser = tls::Parser::default();
                let mut http_parser = http::Parser::default();
                let result = tokio::time::timeout(deadline, async {
                    loop {
                        let parsed = if prefix.first() == Some(&22) && tls {
                            parser.parse(&prefix, self.config.max_bytes)
                        } else if http {
                            http_parser.parse(&prefix)
                        } else if prefix.is_empty() {
                            tls::Parse::NeedMore
                        } else {
                            tls::Parse::Done(None)
                        };
                        match parsed {
                            tls::Parse::Done(result) => return Ok::<_, std::io::Error>(result),
                            tls::Parse::NeedMore => {}
                        }
                        let remaining = self.config.max_bytes - prefix.len();
                        if remaining == 0
                            || (&mut session.stream)
                                .take(remaining as u64)
                                .read_buf(&mut prefix)
                                .await?
                                == 0
                        {
                            return Ok(None);
                        }
                    }
                })
                .await;
                session.user_context.sniffer = match result {
                    Ok(result) => result?,
                    Err(_) => None,
                };
                drop(parser);
                if !prefix.is_empty() {
                    session.stream =
                        Box::new(ReplayStream::with_permit(prefix, session.stream, memory));
                }
                Ok(ProxyRequest::Tcp(session))
            }
            ProxyRequest::Udp(mut session) => {
                let first = match tokio::time::timeout(deadline, session.recv.recv_from()).await {
                    Ok(result) => result?,
                    Err(_) => return Ok(ProxyRequest::Udp(session)),
                };
                if first.0.len() <= self.config.max_bytes {
                    session.user_context.sniffer = quic::parse(
                        &first.0,
                        self.config.max_bytes,
                        self.quic_suite.as_ref().expect("validated UDP protocol"),
                    );
                }
                session.recv = Box::new(ReplayDatagram {
                    first: Some(first),
                    inner: session.recv,
                    permit: Some(memory),
                });
                Ok(ProxyRequest::Udp(session))
            }
        }
    }
}
struct ReplayDatagram {
    first: Option<(bytes::Bytes, crate::msgs::socks5::SocksAddr)>,
    inner: AnyUdpRecv,
    permit: Option<OwnedSemaphorePermit>,
}
#[async_trait::async_trait]
impl UdpRecv for ReplayDatagram {
    async fn recv_from(
        &mut self,
    ) -> Result<(bytes::Bytes, crate::msgs::socks5::SocksAddr), SError> {
        if let Some(first) = self.first.take() {
            self.permit = None;
            return Ok(first);
        }
        self.inner.recv_from().await
    }
}
