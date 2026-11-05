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
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
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
            .try_acquire_many_owned(self.config.reservation_bytes(udp) as u32)
        else {
            return Ok(request);
        };
        let started = Instant::now();
        let deadline = Duration::from_millis(self.config.timeout);
        let result: Result<ProxyRequest, SError> = async {
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
                    let expires = tokio::time::Instant::now() + deadline;
                    let mut buffered = VecDeque::new();
                    let mut parser = quic::Parser::default();
                    for _ in 0..5 {
                        let packet = match tokio::time::timeout_at(
                            expires,
                            session.recv.recv_from(),
                        )
                        .await
                        {
                            Ok(result) => result?,
                            Err(_) => break,
                        };
                        let same_destination = buffered.front().is_none_or(
                            |first: &(bytes::Bytes, crate::SocksAddr)| first.1 == packet.1,
                        );
                        buffered.push_back(packet);
                        if !same_destination {
                            break;
                        }
                        let packet = &buffered.back().expect("just buffered").0;
                        match parser.parse(
                            packet,
                            self.config.max_bytes,
                            self.quic_suite.as_ref().expect("validated UDP protocol"),
                        ) {
                            tls::Parse::NeedMore => {}
                            tls::Parse::Done(result) => {
                                session.user_context.sniffer = result;
                                break;
                            }
                        }
                    }
                    drop(parser);
                    if !buffered.is_empty() {
                        session.recv = Box::new(ReplayDatagram {
                            buffered,
                            inner: session.recv,
                            permit: Some(memory),
                        });
                    }
                    Ok(ProxyRequest::Udp(session))
                }
            }
        }
        .await;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let network = if udp { "udp" } else { "tcp" };
        match &result {
            Ok(request) => tracing::debug!(
                network,
                dst = %request.dst(),
                result = ?request.user_context().sniffer,
                elapsed_ms,
                "sniff completed"
            ),
            Err(error) => tracing::debug!(network, %error, elapsed_ms, "sniff failed"),
        }
        result
    }
}
struct ReplayDatagram {
    buffered: VecDeque<(bytes::Bytes, crate::msgs::socks5::SocksAddr)>,
    inner: AnyUdpRecv,
    permit: Option<OwnedSemaphorePermit>,
}
#[async_trait::async_trait]
impl UdpRecv for ReplayDatagram {
    async fn recv_from(
        &mut self,
    ) -> Result<(bytes::Bytes, crate::msgs::socks5::SocksAddr), SError> {
        if let Some(first) = self.buffered.pop_front() {
            if self.buffered.is_empty() {
                self.permit = None;
            }
            return Ok(first);
        }
        self.inner.recv_from().await
    }
}
