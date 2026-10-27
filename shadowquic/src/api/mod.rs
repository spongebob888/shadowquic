//! Optional REST API. Enable `restful-api` and configure `restful-api`.
//!
//! The memory endpoint additionally requires the independent `jemalloc` feature.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::{Arc, atomic::Ordering},
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
    net::TcpListener,
    sync::{Mutex, watch},
    task::JoinSet,
};

use crate::{
    Outbound, Selectable,
    config::ApiConfig,
    error::SError,
    msgs::socks5::AddrOrDomain,
    observe::Observer,
    utils::{
        http::{self, HttpConnection},
        proxy::tcp_request,
    },
};

#[cfg(feature = "jemalloc")]
mod memory;
#[cfg(feature = "mem-profile")]
mod memory_profile;
pub(crate) mod quality;
pub(crate) mod ui;
pub use quality::SQStatsTrait;

use quality::{ConnQuality, ProbeResult};

type HttpResponse = Response<Full<Bytes>>;

#[derive(Serialize)]
struct ConnectionsSnapshot<'a> {
    connections: Vec<ConnectionSnapshot<'a>>,
}

#[derive(Serialize)]
struct ConnectionSnapshot<'a> {
    id: String,
    protocol: &'static str,
    inbound: &'a str,
    half_closed: bool,
    outbound: Option<String>,
    src: Option<SocketAddr>,
    dst_ip: Option<IpAddr>,
    dst_domain: Option<String>,
    dst_port: Option<u16>,
    upload_bytes: Option<u64>,
    download_bytes: Option<u64>,
    duration_ms: Option<u64>,
    first_response_ms: Option<u64>,
}

struct ApiOutbound {
    outbound: Arc<dyn Outbound>,
    probe_lock: Mutex<()>,
    probe: std::sync::Mutex<Option<ProbeResult>>,
}

pub struct ApiManager {
    secret: Option<String>,
    ui_path: Option<std::path::PathBuf>,
    outbounds: HashMap<String, ApiOutbound>,
    sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
    selectable_outbounds: HashMap<String, Arc<dyn Selectable>>,
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

fn response(status: StatusCode, value: impl Serialize) -> HttpResponse {
    let body = serde_json::to_vec(&value).expect("API response is serializable");
    let mut response = Response::new(Full::new(Bytes::from(body)));
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
    pub(crate) fn with_ui_path(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.ui_path = path;
        self
    }

    pub(crate) fn new(
        secret: Option<String>,
        outbounds: HashMap<String, Arc<dyn Outbound>>,
        sq_outbounds: HashMap<String, Arc<dyn SQStatsTrait>>,
        selectable_outbounds: HashMap<String, Arc<dyn Selectable>>,
        observers: HashMap<String, Arc<Observer>>,
        #[cfg(feature = "dns-server")] dns_cache: Option<Arc<crate::dns::DnsCache>>,
    ) -> Self {
        Self {
            secret: secret.filter(|secret| !secret.is_empty()),
            ui_path: None,
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
            selectable_outbounds,
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
        // Browsers send preflights without bearer credentials. Actual requests
        // still pass through route(), including its authentication check.
        let mut response = if request.method() == Method::OPTIONS
            && request.headers().contains_key(hyper::header::ORIGIN)
            && request
                .headers()
                .contains_key(hyper::header::ACCESS_CONTROL_REQUEST_METHOD)
        {
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::NO_CONTENT;
            response.headers_mut().insert(
                hyper::header::ACCESS_CONTROL_ALLOW_METHODS,
                hyper::header::HeaderValue::from_static("GET, POST, PUT, OPTIONS"),
            );
            response.headers_mut().insert(
                hyper::header::ACCESS_CONTROL_ALLOW_HEADERS,
                hyper::header::HeaderValue::from_static("Authorization, Content-Type"),
            );
            response
        } else {
            match self.route(request).await {
                Ok(response) => response,
                Err(error) => error.response(),
            }
        };
        // Include errors so browser clients can read authentication failures
        // and other API errors as well as successful responses.
        response.headers_mut().insert(
            hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            hyper::header::HeaderValue::from_static("*"),
        );
        response
    }

    async fn route(&self, request: Request<Incoming>) -> Result<HttpResponse, ApiError> {
        let path = request.uri().path();
        if let Some(root) = &self.ui_path
            && path != "/api"
            && !path.starts_with("/api/")
        {
            return ui::serve(root, request.method(), path).await;
        }
        if !self.authorized(request.headers()) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid bearer token",
            ));
        }
        #[cfg(feature = "mem-profile")]
        if request.uri().path() == "/api/v1/memory/profile" {
            if request.method() != Method::POST {
                return Err(ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "unsupported method",
                ));
            }
            return memory_profile::dump().await;
        }
        self.route_api(request).await
    }

    async fn route_api(&self, request: Request<Incoming>) -> Result<HttpResponse, ApiError> {
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
                Ok(response(StatusCode::OK, self.get_outbounds().await))
            }
            "/api/v1/connections" => {
                require(Method::GET)?;
                Ok(response(StatusCode::OK, self.get_connections().await))
            }
            "/api/v1/traffic" => {
                require(Method::GET)?;
                Ok(response(StatusCode::OK, self.get_traffic().await))
            }
            "/api/v1/tokio_metric" => {
                require(Method::GET)?;
                Ok(response(StatusCode::OK, self.get_tokio_metric()))
            }
            #[cfg(feature = "jemalloc")]
            "/api/v1/memory" => {
                require(Method::GET)?;
                memory::get().map(|value| response(StatusCode::OK, value))
            }
            #[cfg(feature = "mem-profile")]
            "/api/v1/memory/profile/status" => {
                require(Method::GET)?;
                memory_profile::get_active()
                    .await
                    .map(|value| response(StatusCode::OK, value))
            }
            #[cfg(feature = "mem-profile")]
            "/api/v1/memory/profile/activate" | "/api/v1/memory/profile/deactivate" => {
                require(Method::POST)?;
                memory_profile::set_active(path == "/api/v1/memory/profile/activate")
                    .await
                    .map(|value| response(StatusCode::OK, value))
            }
            _ => {
                let (tag, action) = path
                    .strip_prefix("/api/v1/outbounds/")
                    .and_then(|tail| tail.rsplit_once('/'))
                    .filter(|(tag, action)| {
                        !tag.is_empty()
                            && !tag.contains('/')
                            && matches!(*action, "probe" | "selection")
                    })
                    .ok_or_else(|| {
                        ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown route")
                    })?;
                if action == "selection" && method == Method::GET {
                    require(Method::GET)?;
                } else {
                    require(if action == "selection" {
                        Method::PUT
                    } else {
                        Method::POST
                    })?;
                }
                let tag = decode_tag(tag)?;
                let outbound = self.outbounds.get(&tag).ok_or_else(|| {
                    ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown outbound")
                })?;
                let selector = if action == "selection" {
                    Some(
                        self.selectable_outbounds
                            .get(&tag)
                            .ok_or_else(|| ApiError::invalid("outbound is not a selector"))?,
                    )
                } else {
                    None
                };
                if let Some(selector) = selector
                    && method == Method::GET
                {
                    return Ok(response(StatusCode::OK, selector.state()));
                }
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
                if let Some(selector) = selector {
                    let input: SelectionRequest =
                        serde_json::from_slice(&body).map_err(ApiError::invalid)?;
                    let state = selector
                        .select(&input.selected)
                        .map_err(ApiError::invalid)?;
                    return Ok(response(StatusCode::OK, state));
                }
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
                Ok(response(StatusCode::OK, probe))
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
            let mut entry = json!({"tag": tag, "quality": quality});
            if let Some(selector) = self.selectable_outbounds.get(tag) {
                entry["selector"] = json!(selector.state());
            }
            outbounds.push(entry);
        }
        json!({"outbounds": outbounds})
    }

    async fn get_connections(&self) -> ConnectionsSnapshot<'_> {
        let mut connections = Vec::new();
        for (inbound, observer) in &self.observers {
            for (protocol, scoreboard) in [
                ("tcp", &observer.tcp_scoreboard),
                ("udp", &observer.udp_scoreboard),
            ] {
                // Release the scoreboard lock before formatting entries or reading the DNS cache.
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
                    #[allow(unused_mut)]
                    let (dst_ip, mut dst_domain, dst_port) = match &send {
                        Some(send) => {
                            let dst = send.dst.read().unwrap();
                            let (ip, domain) = match &dst.addr {
                                AddrOrDomain::V4(ip) => (Some(IpAddr::from(*ip)), None),
                                AddrOrDomain::V6(ip) => (Some(IpAddr::from(*ip)), None),
                                AddrOrDomain::Domain(domain) => {
                                    (None, String::from_utf8(domain.contents.clone()).ok())
                                }
                            };
                            (ip, domain, Some(dst.port))
                        }
                        None => (None, None, None),
                    };
                    #[cfg(feature = "dns-server")]
                    if let (Some(ip), Some(cache)) = (dst_ip, &self.dns_cache) {
                        dst_domain = cache.reverse_lookup_cache(ip);
                    }
                    connections.push(ConnectionSnapshot {
                        id: id.to_string(),
                        protocol,
                        inbound,
                        half_closed,
                        outbound: send.as_ref().map(|send| send.outbound.clone()),
                        src: recv.as_ref().and_then(|recv| recv.src_addr),
                        dst_ip,
                        dst_domain,
                        dst_port,
                        upload_bytes: recv
                            .as_ref()
                            .map(|recv| recv.bytes_recv.load(Ordering::Relaxed)),
                        download_bytes: send
                            .as_ref()
                            .map(|send| send.bytes_send.load(Ordering::Relaxed)),
                        duration_ms: recv.as_ref().map(|recv| millis(recv.created.elapsed())),
                        first_response_ms: send.as_ref().zip(recv.as_ref()).and_then(
                            |(send, recv)| {
                                send.first_send.get().map(|first| {
                                    millis(first.saturating_duration_since(recv.created))
                                })
                            },
                        ),
                    });
                }
            }
        }
        connections.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        ConnectionsSnapshot { connections }
    }

    fn get_tokio_metric(&self) -> Value {
        let metrics = tokio::runtime::Handle::current().metrics();
        json!({
            "num_workers": metrics.num_workers(),
            "num_alive_tasks": metrics.num_alive_tasks(),
            "global_queue_depth": metrics.global_queue_depth(),
        })
    }

    async fn get_traffic(&self) -> Value {
        let mut tags: Vec<_> = self.observers.keys().collect();
        tags.sort();
        let mut inbounds = Vec::with_capacity(tags.len());
        for tag in tags {
            let (mut tcp_download_bytes, mut tcp_upload_bytes) = (0u64, 0u64);
            let (mut udp_download_bytes, mut udp_upload_bytes) = (0u64, 0u64);
            let (mut tcp_conns, mut udp_conns) = (0u64, 0u64);
            for stats in self.observers[tag].user_stats.lock().await.values() {
                tcp_download_bytes =
                    tcp_download_bytes.saturating_add(stats.tcp_sent.load(Ordering::Relaxed));
                tcp_upload_bytes =
                    tcp_upload_bytes.saturating_add(stats.tcp_recv.load(Ordering::Relaxed));
                udp_download_bytes =
                    udp_download_bytes.saturating_add(stats.udp_sent.load(Ordering::Relaxed));
                udp_upload_bytes =
                    udp_upload_bytes.saturating_add(stats.udp_recv.load(Ordering::Relaxed));
                tcp_conns = tcp_conns.saturating_add(stats.tcp_conns.load(Ordering::Relaxed));
                udp_conns = udp_conns.saturating_add(stats.udp_conns.load(Ordering::Relaxed));
            }
            inbounds.push(json!({"inbound": tag,
                "tcp_download_bytes": tcp_download_bytes, "tcp_upload_bytes": tcp_upload_bytes,
                "udp_download_bytes": udp_download_bytes, "udp_upload_bytes": udp_upload_bytes,
                "tcp_conns": tcp_conns, "udp_conns": udp_conns}));
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
struct SelectionRequest {
    selected: String,
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

async fn probe_http_outbound(
    url: &url::Url,
    outbound: &dyn Outbound,
    timeout: Duration,
) -> Result<Duration, ApiError> {
    let probe = async {
        let dst = http::destination(url).map_err(ApiError::invalid)?;
        let started = Instant::now();
        let (stream, request, _request_guard) = tcp_request(dst, Default::default(), 16 * 1024);
        let handle = outbound.handle(request);
        let exchange = async {
            let mut connection = HttpConnection::new(stream, Some(32 * 1024))
                .await
                .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, "probe_failed", error))?;
            let request = http::get_request(url)
                .header(hyper::header::CONNECTION, "close")
                .body(Empty::<Bytes>::new())
                .map_err(ApiError::invalid)?;
            let response = connection
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
        tokio::pin!(exchange);
        // Drive session relays alongside HTTP, and still support outbounds
        // which return after spawning their own relay. Both futures stay scoped
        // to the probe so completion, timeout, or cancellation closes the session.
        tokio::select! {
            biased;
            result = handle => {
                result.map_err(|error| match error {
                    SError::ProtocolUnimpl => {
                        ApiError::new(StatusCode::NOT_IMPLEMENTED, "probe_unsupported", error)
                    }
                    _ => ApiError::new(StatusCode::BAD_GATEWAY, "probe_failed", error),
                })?;
                exchange.await
            }
            result = &mut exchange => result,
        }
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
