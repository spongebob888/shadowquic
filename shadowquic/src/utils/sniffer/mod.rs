//! Passive, bounded hostname inspection before Lua routing.
mod http;
mod quic;
#[cfg(test)]
mod tests;
mod tls;

use super::replay_stream::ReplayStream;
use crate::{
    AnyTcp, AnyUdpRecv, ProxyRequest, UdpRecv,
    config::{SniffProtocol, SnifferCfg, SnifferRule},
    error::SError,
    msgs::socks5::SocksAddr,
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
#[cfg_attr(feature = "restful-api", derive(serde::Serialize))]
pub struct SniffResult {
    pub domain: String,
    pub protocol: SniffProtocol,
    /// Protocol version as advertised by the client, e.g. `"1.1"` or `"2"`.
    pub version: &'static str,
}
impl mlua::UserData for SniffResult {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("domain", |_, this| Ok(this.domain.clone()));
        fields.add_field_method_get("type", |_, this| Ok(this.protocol.as_str()));
        fields.add_field_method_get("version", |_, this| Ok(this.version));
    }
}

/// A best-effort inspection failure. Logged without changing routing metadata.
#[derive(Debug, thiserror::Error)]
pub enum SnifferError {
    #[error("inspection timed out")]
    Timeout,
    #[error("inspection byte, reassembly, packet, or frame limit reached")]
    Limit,
    #[error("concurrent inspection budget exhausted")]
    ConcurrentLimit,
    #[error("inspection memory budget exhausted")]
    MemoryLimit,
}

/// Incremental parser state shared by all protocol parsers.
pub(super) enum Parse {
    NeedMore,
    Error(SnifferError),
    Done(Option<SniffResult>),
}

/// Result of an admitted inspection; I/O errors are reported separately.
type Outcome = Result<Option<SniffResult>, SnifferError>;

/// Datagrams collected for one UDP inspection.
const MAX_DATAGRAMS: usize = 5;

/// Protocols enabled by every rule matching a request.
#[derive(Clone, Copy, Default)]
struct Protocols {
    http: bool,
    tls: bool,
    quic: bool,
}
impl Protocols {
    fn matching(rules: &[SnifferRule], port: u16) -> Self {
        let mut protocols = Self::default();
        for rule in rules
            .iter()
            .filter(|r| r.ports.is_empty() || r.ports.contains(&port))
        {
            for protocol in &rule.protocols {
                match protocol {
                    SniffProtocol::Http => protocols.http = true,
                    SniffProtocol::Tls => protocols.tls = true,
                    SniffProtocol::Quic => protocols.quic = true,
                }
            }
        }
        protocols
    }
    fn any(self) -> bool {
        self.http || self.tls || self.quic
    }
}

/// TCP parser chosen from the first byte of the stream.
enum TcpParser {
    Tls(tls::Parser),
    Http(http::Parser),
}
impl TcpParser {
    fn select(first: u8, protocols: Protocols) -> Option<Self> {
        if first == 22 && protocols.tls {
            Some(Self::Tls(tls::Parser::default()))
        } else if protocols.http {
            Some(Self::Http(http::Parser::default()))
        } else {
            None
        }
    }
    fn parse(&mut self, wire: &[u8], limit: usize) -> Parse {
        match self {
            Self::Tls(parser) => parser.parse(wire, limit),
            Self::Http(parser) => parser.parse(wire),
        }
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

    /// Gate, inspect, and log one request. Only I/O errors are returned.
    pub(crate) async fn sniff(&self, mut request: ProxyRequest) -> Result<ProxyRequest, SError> {
        request.user_context_mut().sniffer = None;
        let udp = matches!(request, ProxyRequest::Udp(_));
        let rules = if udp {
            &self.config.udp
        } else {
            &self.config.tcp
        };
        let protocols = Protocols::matching(rules, request.dst().port);
        if !protocols.any() {
            return Ok(request);
        }
        let Ok(_active) = self.concurrent.try_acquire() else {
            tracing::debug!(event = "sniffer:skipped", error = %SnifferError::ConcurrentLimit, "inspection skipped");
            return Ok(request);
        };
        let Ok(memory) = self
            .memory
            .clone()
            .try_acquire_many_owned(self.config.reservation_bytes(udp) as u32)
        else {
            tracing::debug!(event = "sniffer:skipped", error = %SnifferError::MemoryLimit, "inspection skipped");
            return Ok(request);
        };
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.config.timeout);
        let (mut request, outcome) = match request {
            ProxyRequest::Tcp(mut session) => {
                let mut prefix = Vec::with_capacity(self.config.max_bytes);
                let outcome = self
                    .sniff_tcp(&mut session.stream, protocols, deadline, &mut prefix)
                    .await;
                if !prefix.is_empty() {
                    session.stream =
                        Box::new(ReplayStream::with_permit(prefix, session.stream, memory));
                }
                (ProxyRequest::Tcp(session), outcome)
            }
            ProxyRequest::Udp(mut session) => {
                let mut buffered = VecDeque::new();
                let outcome = self
                    .sniff_udp(&mut session.recv, deadline, &mut buffered)
                    .await;
                if !buffered.is_empty() {
                    session.recv = Box::new(ReplayDatagram {
                        buffered,
                        inner: session.recv,
                        permit: Some(memory),
                    });
                }
                (ProxyRequest::Udp(session), outcome)
            }
        };
        let elapsed = started.elapsed();
        match outcome {
            Ok(Ok(Some(sniff))) => {
                tracing::debug!(event = "sniffer:completed", domain = %sniff.domain, protocol = %sniff.protocol, version = sniff.version, ?elapsed, "sniff completed");
                request.user_context_mut().sniffer = Some(sniff);
            }
            Ok(Ok(None)) => {
                tracing::debug!(event = "sniffer:no_match", ?elapsed, "sniffed nothing");
            }
            Ok(Err(error)) => {
                tracing::debug!(event = "sniffer:failed", %error, ?elapsed, "inspection failed; continuing without hostname");
            }
            Err(error) => {
                tracing::warn!(event = "sniffer:failed", %error, ?elapsed, "sniff failed");
                return Err(error);
            }
        }
        Ok(request)
    }

    /// Read into `prefix` until a parser finishes; every byte read stays in `prefix`.
    async fn sniff_tcp(
        &self,
        stream: &mut AnyTcp,
        protocols: Protocols,
        deadline: tokio::time::Instant,
        prefix: &mut Vec<u8>,
    ) -> Result<Outcome, SError> {
        let limit = self.config.max_bytes;
        let inspect = async {
            let mut parser = None;
            loop {
                if let Some(&first) = prefix.first() {
                    let parser = match &mut parser {
                        Some(parser) => parser,
                        None => match TcpParser::select(first, protocols) {
                            Some(selected) => parser.insert(selected),
                            None => return Ok(Ok(None)),
                        },
                    };
                    match parser.parse(prefix, limit) {
                        Parse::Done(result) => return Ok(Ok(result)),
                        Parse::Error(error) => return Ok(Err(error)),
                        Parse::NeedMore => {}
                    }
                }
                let remaining = limit - prefix.len();
                if remaining == 0 {
                    return Ok(Err(SnifferError::Limit));
                }
                if (&mut *stream)
                    .take(remaining as u64)
                    .read_buf(prefix)
                    .await?
                    == 0
                {
                    return Ok(Ok(None));
                }
            }
        };
        tokio::time::timeout_at(deadline, inspect)
            .await
            .unwrap_or(Ok(Err(SnifferError::Timeout)))
    }

    /// Collect datagrams into `buffered` until the QUIC ClientHello is complete.
    async fn sniff_udp(
        &self,
        recv: &mut AnyUdpRecv,
        deadline: tokio::time::Instant,
        buffered: &mut VecDeque<(bytes::Bytes, SocksAddr)>,
    ) -> Result<Outcome, SError> {
        let suite = self.quic_suite.as_ref().expect("validated UDP protocol");
        let mut parser = quic::Parser::default();
        for _ in 0..MAX_DATAGRAMS {
            let Ok(packet) = tokio::time::timeout_at(deadline, recv.recv_from()).await else {
                return Ok(Err(SnifferError::Timeout));
            };
            let packet = packet?;
            let same_destination = buffered.front().is_none_or(|first| first.1 == packet.1);
            buffered.push_back(packet);
            if !same_destination {
                return Ok(Ok(None));
            }
            let packet = &buffered.back().expect("just buffered").0;
            match parser.parse(packet, self.config.max_bytes, suite) {
                Parse::NeedMore => {}
                Parse::Error(error) => return Ok(Err(error)),
                Parse::Done(result) => return Ok(Ok(result)),
            }
        }
        Ok(Err(SnifferError::Limit))
    }
}

pub(super) fn normalize_host(name: &[u8]) -> Option<String> {
    if name.is_empty() || name.len() > 253 {
        return None;
    }
    for label in name.split(|byte| *byte == b'.') {
        if label.is_empty()
            || label.len() > 63
            || !label[0].is_ascii_alphanumeric()
            || !label[label.len() - 1].is_ascii_alphanumeric()
            || !label
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return None;
        }
    }
    let name = std::str::from_utf8(name).ok()?;
    if name.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

struct ReplayDatagram {
    buffered: VecDeque<(bytes::Bytes, SocksAddr)>,
    inner: AnyUdpRecv,
    permit: Option<OwnedSemaphorePermit>,
}
#[async_trait::async_trait]
impl UdpRecv for ReplayDatagram {
    async fn recv_from(&mut self) -> Result<(bytes::Bytes, SocksAddr), SError> {
        if let Some(first) = self.buffered.pop_front() {
            if self.buffered.is_empty() {
                self.permit = None;
            }
            return Ok(first);
        }
        self.inner.recv_from().await
    }
}
