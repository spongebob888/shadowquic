//! Routed DNS services, enabled by `dns-server`.
mod cache;
pub mod config;
mod fakeip;
mod integration;
#[cfg(test)]
mod tests;

pub use cache::DnsCache;
pub use fakeip::FakeIp;
pub(crate) use integration::ResolvingOutbound;
#[cfg(any(test, all(feature = "tproxy", target_os = "linux")))]
pub(crate) use integration::RestoringInbound;

use async_trait::async_trait;
use bytes::Bytes;
use simple_dns::{
    CLASS, Name, OPCODE, Packet, PacketFlag, QCLASS, QTYPE, Question, RCODE, ResourceRecord, TYPE,
    rdata::RData,
};
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{
    AnyTcp, Inbound, Outbound, ProxyRequest, TcpSession, TcpTrait, UdpSession, UserContext,
    error::SError, msgs::socks5::SocksAddr,
};

type Result<T> = std::result::Result<T, SError>;
const TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_TTL: u32 = 60;
/// This is the default tag used for the system DNS resolver, which is used when no other resolver is specified.
/// It is always enabled.
pub const DEFAULT_SYSTEM_DNS_TAG: &str = "default-system";
fn dns_error(error: impl std::fmt::Display) -> SError {
    SError::DnsError(error.to_string())
}

fn reverse_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa")
        }
        IpAddr::V6(ip) => {
            let mut name = String::with_capacity(73);
            for digit in format!("{:032x}", u128::from(ip)).chars().rev() {
                name.push(digit);
                name.push('.');
            }
            name.push_str("ip6.arpa");
            name
        }
    }
}

fn ptr_names(packet: &Packet<'_>) -> Result<Vec<String>> {
    // Reverse zones may delegate individual addresses through CNAMEs.
    let mut name = packet
        .questions
        .first()
        .ok_or_else(|| dns_error("missing PTR question"))?
        .qname
        .clone();
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(name.clone()) {
            return Err(dns_error("CNAME loop in reverse lookup"));
        }
        let next = packet.answers.iter().find_map(|record| {
            if record.class == CLASS::IN
                && record.name == name
                && let RData::CNAME(alias) = &record.rdata
            {
                Some(alias.0.clone())
            } else {
                None
            }
        });
        match next {
            Some(next) => name = next,
            None => break,
        }
    }
    let mut names: Vec<String> = Vec::new();
    for record in &packet.answers {
        if record.class == CLASS::IN
            && record.name == name
            && let RData::PTR(ptr) = &record.rdata
        {
            let hostname = ptr.0.to_string();
            if !hostname.is_empty()
                && !names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&hostname))
            {
                names.push(hostname);
            }
        }
    }
    Ok(names)
}

#[async_trait]
pub trait DnsService: Send + Sync {
    async fn exchange(&self, query: &[u8]) -> Result<Vec<u8>>;

    /// Resolve an IP address to hostnames using an IN/PTR query through this
    /// service. Unlike `DnsCache::reverse_lookup_cache`, this can perform I/O.
    /// Returns an error if the exchange fails or there are no matching names.
    async fn reverse_lookup(&self, ip: IpAddr) -> Result<Vec<String>> {
        let reverse_name = reverse_name(ip);
        let mut query = Packet::new_query(0);
        query.set_flags(PacketFlag::RECURSION_DESIRED);
        query.questions.push(Question::new(
            Name::new(&reverse_name).map_err(dns_error)?,
            TYPE::PTR.into(),
            CLASS::IN.into(),
            false,
        ));
        let query = query.build_bytes_vec().map_err(dns_error)?;
        let bytes = self.exchange(&query).await?;
        let reply = validate_response(&query, &bytes)?;
        if reply.has_flags(PacketFlag::TRUNCATION) {
            return Err(dns_error(format!(
                "truncated reverse lookup response for {ip}"
            )));
        }
        if reply.rcode() != RCODE::NoError {
            return Err(dns_error(format!(
                "reverse lookup failed for {ip}: {:?}",
                reply.rcode()
            )));
        }

        let names = ptr_names(&reply)?;
        if names.is_empty() {
            return Err(SError::DomainResolveFailed(ip.to_string()));
        }
        Ok(names)
    }

    async fn resolve(&self, domain: &str) -> Result<Vec<IpAddr>> {
        let mut result = Vec::new();
        let mut last_error = None;
        for ty in [TYPE::A, TYPE::AAAA] {
            let mut query = Packet::new_query(0);
            query.set_flags(PacketFlag::RECURSION_DESIRED);
            query.questions.push(Question::new(
                Name::new(domain.trim_end_matches('.')).map_err(dns_error)?,
                ty.into(),
                CLASS::IN.into(),
                false,
            ));
            match self
                .exchange(&query.build_bytes_vec().map_err(dns_error)?)
                .await
            {
                Ok(bytes) => {
                    let packet = Packet::parse(&bytes).map_err(dns_error)?;
                    if packet.rcode() == RCODE::NoError && !packet.has_flags(PacketFlag::TRUNCATION)
                    {
                        for ip in cache::addresses(&packet) {
                            if !result.contains(&ip) {
                                result.push(ip);
                            }
                        }
                    }
                }
                Err(error) => last_error = Some(error),
            }
        }
        if result.is_empty() {
            return Err(last_error.unwrap_or_else(|| SError::DomainResolveFailed(domain.into())));
        }
        Ok(result)
    }
}

#[derive(Clone)]
pub(crate) enum Backend {
    Udp(std::net::SocketAddr),
    Tcp(std::net::SocketAddr),
    Tls {
        upstream: std::net::SocketAddr,
        server_name: rustls_jls::pki_types::ServerName<'static>,
        connector: tokio_rustls_jls::TlsConnector,
    },
    FakeIp,
    System,
}

/// Shared resolver handle, also usable as a UDP DNS hijacking outbound.
#[derive(Clone)]
pub struct Resolver {
    pub(crate) tag: String,
    pub(crate) backend: Backend,
    pub(crate) requests: mpsc::Sender<ProxyRequest>,
    pub fake_ip: Option<Arc<FakeIp>>,
    pub(crate) cache: Arc<DnsCache>,
}

/// Local UDP and TCP listeners plus a queue of routable upstream sessions.
pub struct DnsServer {
    pub resolver: Arc<Resolver>,
    requests: mpsc::Receiver<ProxyRequest>,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    pub local_addr: std::net::SocketAddr,
}

impl Drop for DnsServer {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().iter() {
            task.abort();
        }
    }
}

fn tls_connector() -> Result<tokio_rustls_jls::TlsConnector> {
    let roots =
        rustls_jls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = rustls_jls::crypto::CryptoProvider::get_default()
        .cloned()
        .or_else(|| {
            #[cfg(feature = "ring")]
            {
                return Some(Arc::new(rustls_jls::crypto::ring::default_provider()));
            }
            #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
            {
                return Some(Arc::new(rustls_jls::crypto::aws_lc_rs::default_provider()));
            }
            #[allow(unreachable_code)]
            None
        })
        .ok_or_else(|| {
            SError::InvalidConfig(
                "DNS TLS requires a rustls crypto provider (ring or aws-lc-rs)".into(),
            )
        })?;
    let mut config = rustls_jls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(dns_error)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // DNS uses ordinary TLS with certificate verification, not JLS authentication.
    config.jls_config.enable = false;
    Ok(tokio_rustls_jls::TlsConnector::from(Arc::new(config)))
}

impl DnsServer {
    async fn new(
        tag: String,
        bind_addr: std::net::SocketAddr,
        backend: Backend,
        cache: Arc<DnsCache>,
    ) -> Result<Self> {
        let tcp = TcpListener::bind(bind_addr).await?;
        let local_addr = tcp.local_addr()?;
        let udp = Arc::new(UdpSocket::bind(local_addr).await?);
        let (tx, rx) = mpsc::channel(256);
        let resolver = Arc::new(Resolver {
            tag,
            fake_ip: matches!(backend, Backend::FakeIp).then(|| Arc::new(FakeIp::default())),
            backend,
            requests: tx,
            cache,
        });
        let service = resolver.clone();
        let udp_task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            let mut buffer = vec![0; 2000];
            loop {
                tokio::select! {
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                    received = udp.recv_from(&mut buffer), if tasks.len() < 256 => {
                        let Ok((len, peer)) = received else { break };
                        let query = buffer[..len].to_vec();
                        let service = service.clone();
                        let udp = udp.clone();
                        tasks.spawn(async move {
                            if let Ok(reply) = service.answer(&query).await
                                && let Ok(reply) = udp_reply(&query, reply) {
                                let _ = udp.send_to(&reply, peer).await;
                            }
                        });
                    }
                }
            }
        });
        let service = resolver.clone();
        let tcp_task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                    accepted = tcp.accept(), if tasks.len() < 256 => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let service = service.clone();
                        tasks.spawn(async move {
                            while let Ok(Ok(query)) = tokio::time::timeout(TIMEOUT, read_frame(&mut stream)).await {
                                let Ok(reply) = service.answer(&query).await else { break };
                                if !matches!(tokio::time::timeout(TIMEOUT, write_frame(&mut stream, &reply)).await, Ok(Ok(()))) { break; }
                            }
                        });
                    }
                }
            }
        });
        Ok(Self {
            resolver,
            requests: rx,
            tasks: tokio::sync::Mutex::new(vec![udp_task, tcp_task]),
            local_addr,
        })
    }
}

#[async_trait]
impl Inbound for DnsServer {
    async fn accept(&mut self) -> Result<ProxyRequest> {
        self.requests.recv().await.ok_or(SError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<()> {
        let mut tasks = self.tasks.lock().await;
        for task in tasks.iter() {
            task.abort();
        }
        for task in tasks.drain(..) {
            let _ = task.await;
        }
        Ok(())
    }
}
#[async_trait]
impl DnsService for DnsServer {
    async fn exchange(&self, query: &[u8]) -> Result<Vec<u8>> {
        self.resolver.exchange(query).await
    }
}
#[async_trait]
impl Outbound for DnsServer {
    async fn handle(&self, req: ProxyRequest) -> Result<()> {
        self.resolver.handle(req).await
    }
}
#[async_trait]
impl Outbound for Resolver {
    async fn handle(&self, req: ProxyRequest) -> Result<()> {
        let ProxyRequest::Udp(mut session) = req else {
            return Err(SError::ProtocolViolation);
        };
        if session.user_context.inbound_tag == self.tag {
            return Err(dns_error(
                "DNS upstream traffic cannot be hijacked by its own resolver",
            ));
        }
        let resolver = self.clone();
        tokio::spawn(async move {
            while let Ok(Ok((query, dst))) =
                tokio::time::timeout(Duration::from_secs(60), session.recv.recv_from()).await
            {
                // Preserve the intercepted destination as the response source.
                if let Ok(reply) = resolver.answer(&query).await
                    && let Ok(reply) = udp_reply(&query, reply)
                    && session.send.send_to(reply.into(), dst).await.is_err()
                {
                    break;
                }
            }
        });
        Ok(())
    }
}

impl Resolver {
    async fn answer(&self, query: &[u8]) -> Result<Vec<u8>> {
        match self.exchange(query).await {
            Ok(reply) => Ok(reply),
            Err(error) => {
                tracing::debug!(%error, "DNS exchange failed");
                let query = parse_query(query)?;
                let mut reply = reply_for(query);
                *reply.rcode_mut() = RCODE::ServerFailure;
                reply.build_bytes_vec().map_err(dns_error)
            }
        }
    }

    async fn upstream(&self, query: &[u8], tcp: bool) -> Result<Vec<u8>> {
        let dst = match &self.backend {
            Backend::Udp(upstream) | Backend::Tcp(upstream) | Backend::Tls { upstream, .. } => {
                *upstream
            }
            Backend::FakeIp | Backend::System => {
                return Err(dns_error("local resolver has no upstream"));
            }
        };
        let dst: SocksAddr = dst.into();
        if !tcp {
            let (send_query, recv_query) = mpsc::channel(1);
            let (send_reply, mut recv_reply) = mpsc::channel(1);
            send_query
                .send((Bytes::copy_from_slice(query), dst.clone()))
                .await
                .map_err(dns_error)?;
            // Keep the query channel open until the reply arrives.
            self.requests
                .send(ProxyRequest::Udp(UdpSession {
                    recv: Box::new(recv_query),
                    send: Arc::new(send_reply),
                    stream: None,
                    bind_addr: "0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into(),
                    dst: dst.clone(),
                    src_addr: None,
                    user_context: UserContext::default(),
                }))
                .await
                .map_err(dns_error)?;
            let reply = loop {
                let (reply, source) = recv_reply.recv().await.ok_or(SError::OutboundUnavailable)?;
                if source.to_string() == dst.to_string() && validate_response(query, &reply).is_ok()
                {
                    break reply;
                }
            };
            drop(send_query);
            Ok(reply.to_vec())
        } else {
            let (stream, peer) = tokio::io::duplex(5000);
            self.requests
                .send(ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(peer),
                    dst,
                    src_addr: None,
                    user_context: UserContext::default(),
                }))
                .await
                .map_err(dns_error)?;
            let mut stream: AnyTcp = if let Backend::Tls {
                connector,
                server_name,
                ..
            } = &self.backend
            {
                Box::new(connector.connect(server_name.clone(), stream).await?)
            } else {
                Box::new(stream)
            };
            write_frame(&mut stream, query).await?;
            read_frame(&mut stream).await
        }
    }

    async fn local(&self, query: Packet<'_>) -> Result<Vec<u8>> {
        let question = query.questions[0].clone();
        let mut reply = reply_for(query);
        if question.qclass != QCLASS::CLASS(CLASS::IN) {
            *reply.rcode_mut() = RCODE::NotImplemented;
        } else if matches!(question.qtype, QTYPE::TYPE(TYPE::A | TYPE::AAAA)) {
            let ipv6 = question.qtype == QTYPE::TYPE(TYPE::AAAA);
            let name = question.qname.to_string();
            let ips = if let Some(fake) = &self.fake_ip {
                vec![fake.allocate(&name, ipv6)?]
            } else {
                tokio::net::lookup_host((name.as_str(), 0))
                    .await?
                    .map(|addr| addr.ip())
                    .collect()
            };
            let mut seen = std::collections::HashSet::new();
            for ip in ips {
                if ip.is_ipv6() != ipv6 || !seen.insert(ip) {
                    continue;
                }
                let data = match ip {
                    IpAddr::V4(ip) => RData::A(ip.into()),
                    IpAddr::V6(ip) => RData::AAAA(ip.into()),
                };
                reply.answers.push(ResourceRecord::new(
                    question.qname.clone(),
                    CLASS::IN,
                    LOCAL_TTL,
                    data,
                ));
            }
        } else if matches!(self.backend, Backend::System) {
            *reply.rcode_mut() = RCODE::NotImplemented;
        }
        reply.build_bytes_vec().map_err(dns_error)
    }
}

#[async_trait]
impl DnsService for Resolver {
    async fn exchange(&self, query: &[u8]) -> Result<Vec<u8>> {
        let packet = parse_query(query)?;
        if let Some(cached) = self.cache.get(query)? {
            return Ok(cached);
        }
        let reply = tokio::time::timeout(TIMEOUT, async {
            match &self.backend {
                Backend::System | Backend::FakeIp => self.local(packet).await,
                Backend::Tcp(_) | Backend::Tls { .. } => self.upstream(query, true).await,
                Backend::Udp(_) => {
                    let reply = self.upstream(query, false).await?;
                    if Packet::parse(&reply)
                        .map_err(dns_error)?
                        .has_flags(PacketFlag::TRUNCATION)
                    {
                        self.upstream(query, true).await
                    } else {
                        Ok(reply)
                    }
                }
            }
        })
        .await
        .map_err(|_| dns_error("query timed out"))??;
        let packet = validate_response(query, &reply)?;
        self.cache.insert(query, packet);
        Ok(reply)
    }
}

impl TcpTrait for tokio::io::DuplexStream {}
impl TcpTrait for tokio_rustls_jls::client::TlsStream<tokio::io::DuplexStream> {}

fn parse_query(bytes: &[u8]) -> Result<Packet<'_>> {
    let packet = Packet::parse(bytes).map_err(dns_error)?;
    if packet.has_flags(PacketFlag::RESPONSE)
        || packet.opcode() != OPCODE::StandardQuery
        || packet.questions.len() != 1
    {
        return Err(dns_error("expected one standard DNS question"));
    }
    Ok(packet)
}
fn validate_response<'a>(query: &[u8], reply: &'a [u8]) -> Result<Packet<'a>> {
    let query = parse_query(query)?;
    let reply = Packet::parse(reply).map_err(dns_error)?;
    if !reply.has_flags(PacketFlag::RESPONSE)
        || query.id() != reply.id()
        || query.opcode() != reply.opcode()
        || reply.questions.len() != 1
        || query.questions[0].qname != reply.questions[0].qname
        || query.questions[0].qtype != reply.questions[0].qtype
        || query.questions[0].qclass != reply.questions[0].qclass
    {
        return Err(dns_error("upstream response does not match query"));
    }
    Ok(reply)
}
fn reply_for(query: Packet<'_>) -> Packet<'_> {
    let recursion = query.has_flags(PacketFlag::RECURSION_DESIRED);
    let mut reply = Packet::new_reply(query.id());
    reply.questions = query.questions;
    reply.set_flags(PacketFlag::RECURSION_AVAILABLE);
    if recursion {
        reply.set_flags(PacketFlag::RECURSION_DESIRED);
    }
    reply
}
fn udp_reply(query: &[u8], bytes: Vec<u8>) -> Result<Vec<u8>> {
    let query = parse_query(query)?;
    let limit = query
        .opt()
        .map_or(512, |opt| opt.udp_packet_size.max(512) as usize)
        .min(4096);
    if bytes.len() <= limit {
        return Ok(bytes);
    }
    let mut reply = reply_for(query);
    reply.set_flags(PacketFlag::TRUNCATION);
    reply.build_bytes_vec().map_err(dns_error)
}
async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let len = stream.read_u16().await? as usize;
    if len < 12 {
        return Err(dns_error("invalid DNS frame length"));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}
async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
    let len = u16::try_from(bytes.len()).map_err(dns_error)?;
    stream.write_u16(len).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Owns the resolver tag map and the shared DNS cache used by every resolver.
#[derive(Clone)]
pub struct ResolverManager {
    pub(crate) resolvers: Arc<HashMap<String, Arc<Resolver>>>,
    pub(crate) cache: Arc<DnsCache>,
}

impl Default for ResolverManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolverManager {
    pub fn new() -> Self {
        let cache = Arc::new(DnsCache::default());
        let mut resolvers = HashMap::new();
        resolvers.insert(
            DEFAULT_SYSTEM_DNS_TAG.to_string(),
            Arc::new(Resolver {
                tag: DEFAULT_SYSTEM_DNS_TAG.to_string(),
                backend: Backend::System,
                requests: mpsc::channel(1).0,
                fake_ip: None,
                cache: cache.clone(),
            }),
        );
        Self {
            resolvers: Arc::new(resolvers),
            cache,
        }
    }

    pub(crate) fn cache(&self) -> Arc<DnsCache> {
        self.cache.clone()
    }

    pub(crate) fn resolver(&self, tag: &str) -> Option<Arc<Resolver>> {
        self.resolvers.get(tag).cloned()
    }

    pub(crate) fn insert(&mut self, tag: String, resolver: Arc<Resolver>) {
        Arc::make_mut(&mut self.resolvers).insert(tag, resolver);
    }
}
