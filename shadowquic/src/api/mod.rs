//! Optional REST API. Enable `restful-api` and configure `restful-api`.
//!
//! The memory endpoint additionally requires the independent `jemalloc` feature.

use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    io,
    net::IpAddr,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::{Method, Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    net::TcpListener,
    sync::{Mutex, oneshot, watch},
    task::JoinSet,
};

use crate::{
    Outbound, ProxyRequest, TcpSession,
    config::ApiConfig,
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
    observe::Observer,
};

#[cfg(feature = "jemalloc")]
mod memory;
pub(crate) mod quality;
pub use quality::SQStatsTrait;

use quality::{ConnQuality, ProbeResult};

type HttpResponse = Response<Full<Bytes>>;

struct ApiOutbound {
    outbound: Arc<dyn Outbound>,
    probe_lock: Mutex<()>,
    probe: std::sync::Mutex<Option<ProbeResult>>,
}

pub struct ApiManager {
    secret: Option<String>,
    outbounds: HashMap<String, ApiOutbound>,
    sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
    observers: HashMap<String, Arc<Observer>>,
    #[cfg(feature = "dns-server")]
    dns_cache: Option<Arc<crate::dns::DnsCache>>,
}

#[derive(Clone, Serialize)]
pub(super) struct ErrorBody {
    code: &'static str,
    message: String,
}

struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl ToString) -> Self {
        Self {
            status,
            body: ErrorBody {
                code,
                message: message.to_string(),
            },
        }
    }

    fn invalid(message: impl ToString) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn response(self) -> HttpResponse {
        let mut response = response(self.status, json!({"error": self.body}));
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                hyper::header::WWW_AUTHENTICATE,
                hyper::header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

fn response(status: StatusCode, value: Value) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::from(value.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    response
}

pub(super) fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn timestamp(time: SystemTime) -> String {
    time::OffsetDateTime::from(time)
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current UTC timestamp is representable as RFC 3339")
}

pub(crate) async fn bind(config: &ApiConfig) -> Result<TcpListener, SError> {
    Ok(TcpListener::bind(config.bind_addr).await?)
}

impl ApiManager {
    pub(crate) fn new(
        secret: Option<String>,
        outbounds: HashMap<String, Arc<dyn Outbound>>,
        sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
        observers: HashMap<String, Arc<Observer>>,
        #[cfg(feature = "dns-server")] dns_cache: Option<Arc<crate::dns::DnsCache>>,
    ) -> Self {
        Self {
            secret: secret.filter(|secret| !secret.is_empty()),
            outbounds: outbounds
                .into_iter()
                .map(|(tag, outbound)| {
                    (
                        tag,
                        ApiOutbound {
                            outbound,
                            probe_lock: Mutex::new(()),
                            probe: std::sync::Mutex::new(None),
                        },
                    )
                })
                .collect(),
            sq_outbounds,
            observers,
            #[cfg(feature = "dns-server")]
            dns_cache,
        }
    }

    pub(crate) async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        mut stopped: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let mut connections = JoinSet::new();
        loop {
            if *stopped.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result { tracing::debug!(%error, "API connection task failed"); }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let api = self.clone();
                    connections.spawn(async move {
                        let service = service_fn(move |request| {
                            let api = api.clone();
                            async move { Ok::<_, Infallible>(api.handle(request).await) }
                        });
                        if let Err(error) = hyper::server::conn::http1::Builder::new()
                            .max_buf_size(32 * 1024)
                            .serve_connection(TokioIo::new(stream), service).await
                        {
                            tracing::debug!(%error, "API HTTP connection closed");
                        }
                    });
                }
            }
        }
        // Aborting connection futures also cancels in-flight probes and releases their guards.
        connections.shutdown().await;
        Ok(())
    }

    async fn handle(&self, request: Request<Incoming>) -> HttpResponse {
        match self.route(request).await {
            Ok(value) => response(StatusCode::OK, value),
            Err(error) => error.response(),
        }
    }

    async fn route(&self, request: Request<Incoming>) -> Result<Value, ApiError> {
        if !self.authorized(request.headers()) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid bearer token",
            ));
        }
        let path = request.uri().path();
        let method = request.method();
        let require = |expected: Method| {
            if method == expected {
                Ok(())
            } else {
                Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "unsupported method",
                ))
            }
        };
        match path {
            "/api/v1/outbounds" => {
                require(Method::GET)?;
                Ok(self.get_outbounds().await)
            }
            "/api/v1/connections" => {
                require(Method::GET)?;
                Ok(self.get_connections().await)
            }
            "/api/v1/traffic" => {
                require(Method::GET)?;
                Ok(self.get_traffic().await)
            }
            #[cfg(feature = "jemalloc")]
            "/api/v1/memory" => {
                require(Method::GET)?;
                memory::get()
            }
            _ => {
                let tag = path
                    .strip_prefix("/api/v1/outbounds/")
                    .and_then(|tail| tail.strip_suffix("/probe"))
                    .filter(|tag| !tag.is_empty() && !tag.contains('/'))
                    .ok_or_else(|| {
                        ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown route")
                    })?;
                require(Method::POST)?;
                let tag = decode_tag(tag)?;
                let outbound = self.outbounds.get(&tag).ok_or_else(|| {
                    ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown outbound")
                })?;
                let content_type = request
                    .headers()
                    .get(hyper::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !content_type.eq_ignore_ascii_case("application/json") {
                    return Err(ApiError::invalid("expected application/json"));
                }
                let body = Limited::new(request.into_body(), 16 * 1024)
                    .collect()
                    .await
                    .map_err(ApiError::invalid)?
                    .to_bytes();
                let input: ProbeRequest =
                    serde_json::from_slice(&body).map_err(ApiError::invalid)?;
                let (url, timeout) = input.validate()?;
                let _guard = outbound.probe_lock.try_lock().map_err(|_| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "probe_in_progress",
                        "a probe is already running for this outbound",
                    )
                })?;
                let result = match probe_http_outbound(&url, &*outbound.outbound, timeout).await {
                    Err(error) if error.status == StatusCode::NOT_IMPLEMENTED => return Err(error),
                    result => result,
                };
                let measured_at = timestamp(SystemTime::now());
                let probe = match &result {
                    Ok(latency) => ProbeResult {
                        url: url.to_string(),
                        status: "ok",
                        latency_ms: Some(millis(*latency)),
                        measured_at,
                        error: None,
                    },
                    Err(error) => ProbeResult {
                        url: url.to_string(),
                        status: if error.status == StatusCode::GATEWAY_TIMEOUT {
                            "timeout"
                        } else {
                            "failed"
                        },
                        latency_ms: None,
                        measured_at,
                        error: Some(error.body.clone()),
                    },
                };
                *outbound.probe.lock().unwrap() = Some(probe.clone());
                result?;
                Ok(json!(probe))
            }
        }
    }

    fn authorized(&self, headers: &hyper::HeaderMap) -> bool {
        let Some(secret) = &self.secret else {
            return true;
        };
        let mut values = headers.get_all(hyper::header::AUTHORIZATION).iter();
        let Some(value) = values.next().and_then(|value| value.to_str().ok()) else {
            return false;
        };
        let Some((scheme, token)) = value.split_once(' ') else {
            return false;
        };
        values.next().is_none()
            && scheme.eq_ignore_ascii_case("Bearer")
            && bool::from(
                secret
                    .as_bytes()
                    .ct_eq(token.trim_start_matches(' ').as_bytes()),
            )
    }

    async fn get_outbounds(&self) -> Value {
        let mut tags: Vec<_> = self.outbounds.keys().collect();
        tags.sort();
        let mut outbounds = Vec::with_capacity(tags.len());
        for tag in tags {
            let outbound = &self.outbounds[tag];
            let sq_conn_stats = match self.sq_outbounds.get(tag) {
                Some(stats) => stats.get_sq_stats().await.map(Into::into),
                None => None,
            };
            let quality = ConnQuality {
                sq_conn_stats,
                probe: outbound.probe.lock().unwrap().clone(),
            };
            outbounds.push(json!({"tag": tag, "quality": quality}));
        }
        json!({"outbounds": outbounds})
    }

    async fn get_connections(&self) -> Value {
        let mut connections = Vec::new();
        for (inbound, observer) in &self.observers {
            for (protocol, scoreboard) in [
                ("tcp", &observer.tcp_scoreboard),
                ("udp", &observer.udp_scoreboard),
            ] {
                let entries = scoreboard.lock().await.clone();
                for (send, recv) in entries {
                    let (send, recv) = (send.upgrade(), recv.upgrade());
                    let Some(id) = recv
                        .as_ref()
                        .map(|recv| recv.id)
                        .or_else(|| send.as_ref().map(|send| send.id))
                    else {
                        continue;
                    };
                    let half_closed = send.is_none() || recv.is_none();
                    let dst = send.as_ref().map(|send| send.dst.read().unwrap().clone());
                    #[allow(unused_mut)]
                    let (dst_ip, mut dst_domain) = match dst.as_ref().map(|dst| &dst.addr) {
                        Some(AddrOrDomain::V4(ip)) => (Some(IpAddr::from(*ip)), None),
                        Some(AddrOrDomain::V6(ip)) => (Some(IpAddr::from(*ip)), None),
                        Some(AddrOrDomain::Domain(domain)) => {
                            (None, String::from_utf8(domain.contents.clone()).ok())
                        }
                        None => (None, None),
                    };
                    #[cfg(feature = "dns-server")]
                    if let (Some(ip), Some(cache)) = (dst_ip, &self.dns_cache) {
                        dst_domain = cache.reverse_lookup_cache(ip);
                    }
                    connections.push(json!({
                        "id": id.to_string(), "protocol": protocol, "inbound": inbound,
                        "half_closed": half_closed,
                        "outbound": send.as_ref().map(|send| &send.outbound),
                        "src": recv.as_ref().and_then(|recv| recv.src_addr).map(|addr| addr.to_string()),
                        "dst_ip": dst_ip.map(|ip| ip.to_string()), "dst_domain": dst_domain,
                        "dst_port": dst.as_ref().map(|dst| dst.port),
                        "recv_bytes": recv.as_ref().map(|recv| recv.bytes_recv.load(Ordering::Relaxed)),
                        "sent_bytes": send.as_ref().map(|send| send.bytes_send.load(Ordering::Relaxed)),
                        "duration_ms": recv.as_ref().map(|recv| millis(recv.created.elapsed())),
                        "first_response_ms": send.as_ref().zip(recv.as_ref()).and_then(|(send, recv)|
                            send.first_send.get().map(|first| millis(first.saturating_duration_since(recv.created)))),
                    }));
                }
            }
        }
        connections.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        json!({"connections": connections})
    }

    async fn get_traffic(&self) -> Value {
        let mut tags: Vec<_> = self.observers.keys().collect();
        tags.sort();
        let mut inbounds = Vec::with_capacity(tags.len());
        for tag in tags {
            let (mut sent_bytes, mut recv_bytes, mut tcp_conns, mut udp_conns) =
                (0u64, 0u64, 0u64, 0u64);
            for stats in self.observers[tag].user_stats.lock().await.values() {
                sent_bytes = sent_bytes
                    .saturating_add(stats.tcp_sent.load(Ordering::Relaxed))
                    .saturating_add(stats.udp_sent.load(Ordering::Relaxed));
                recv_bytes = recv_bytes
                    .saturating_add(stats.tcp_recv.load(Ordering::Relaxed))
                    .saturating_add(stats.udp_recv.load(Ordering::Relaxed));
                tcp_conns = tcp_conns.saturating_add(stats.tcp_conns.load(Ordering::Relaxed));
                udp_conns = udp_conns.saturating_add(stats.udp_conns.load(Ordering::Relaxed));
            }
            inbounds.push(
                json!({"inbound": tag, "sent_bytes": sent_bytes, "recv_bytes": recv_bytes,
                "tcp_conns": tcp_conns, "udp_conns": udp_conns}),
            );
        }
        json!({"inbounds": inbounds})
    }
}

fn decode_tag(tag: &str) -> Result<String, ApiError> {
    let bytes = tag.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(ApiError::invalid("invalid percent-encoded outbound tag"));
        }
    }
    percent_encoding::percent_decode_str(tag)
        .decode_utf8()
        .map(|tag| tag.into_owned())
        .map_err(ApiError::invalid)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeRequest {
    url: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    10_000
}

impl ProbeRequest {
    fn validate(self) -> Result<(url::Url, Duration), ApiError> {
        let url = url::Url::parse(&self.url).map_err(ApiError::invalid)?;
        if url.scheme() != "http"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url
                .host_str()
                .is_some_and(|host| host.len() > u8::MAX as usize)
        {
            return Err(ApiError::invalid(
                "expected an absolute HTTP URL without credentials",
            ));
        }
        if !(1..=60_000).contains(&self.timeout_ms) {
            return Err(ApiError::invalid("timeout_ms must be between 1 and 60000"));
        }
        Ok((url, Duration::from_millis(self.timeout_ms)))
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct ProbeStream {
    inner: DuplexStream,
    cancelled: oneshot::Receiver<()>,
    closed: bool,
}

impl ProbeStream {
    fn check_cancelled(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.closed || Pin::new(&mut self.cancelled).poll(cx).is_ready() {
            self.closed = true;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "probe finished",
            ));
        }
        Ok(())
    }
}

impl crate::TcpTrait for ProbeStream {}

impl AsyncRead for ProbeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProbeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_cancelled(cx)?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn probe_http_outbound(
    url: &url::Url,
    outbound: &dyn Outbound,
    timeout: Duration,
) -> Result<Duration, ApiError> {
    let probe = async {
        let port = url.port_or_known_default().unwrap();
        let addr = match url.host().unwrap() {
            url::Host::Ipv4(ip) => AddrOrDomain::V4(ip.octets()),
            url::Host::Ipv6(ip) => AddrOrDomain::V6(ip.octets()),
            url::Host::Domain(domain) => AddrOrDomain::Domain(domain.as_bytes().to_vec().into()),
        };
        let started = Instant::now();
        let (stream, peer) = tokio::io::duplex(16 * 1024);
        // Dropping this sender interrupts pending proxy I/O with an error, so
        // detached outbound relays close both directions instead of half-closing.
        let (_cancel_on_drop, cancelled) = oneshot::channel();
        outbound
            .handle(ProxyRequest::Tcp(TcpSession {
                stream: Box::new(ProbeStream {
                    inner: peer,
                    cancelled,
                    closed: false,
                }),
                dst: SocksAddr { addr, port },
                src_addr: None,
                user_context: Default::default(),
            }))
            .await
            .map_err(|error| match error {
                SError::ProtocolUnimpl => {
                    ApiError::new(StatusCode::NOT_IMPLEMENTED, "probe_unsupported", error)
                }
                _ => ApiError::new(StatusCode::BAD_GATEWAY, "probe_failed", error),
            })?;
        let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
            .max_buf_size(32 * 1024)
            .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
            .await
            .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, "probe_failed", error))?;
        let _driver = AbortOnDrop(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        let mut target = url.path().to_owned();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let request = Request::builder()
            .method(Method::GET)
            .uri(target)
            .header(hyper::header::HOST, authority)
            .header(hyper::header::CONNECTION, "close")
            .body(Empty::<Bytes>::new())
            .map_err(ApiError::invalid)?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, "probe_failed", error))?;
        let latency = started.elapsed();
        if !response.status().is_success() {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "probe_failed",
                format!("HTTP status {}", response.status()),
            ));
        }
        Ok(latency)
    };
    tokio::time::timeout(timeout, probe).await.map_err(|_| {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "probe_timeout",
            "outbound probe timed out",
        )
    })?
}

#[cfg(test)]
mod tests;
