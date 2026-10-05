use super::{RedbDatabase, Result, Slot};
use crate::{
    AnyTcp, Inbound, ProxyRequest, TcpSession, TcpTrait, UserContext, config::RouterDatabaseCfg,
    error::SError, msgs::socks5::SocksAddr,
};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use url::Url;

pub(super) fn parse_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("database URL must be HTTP(S), have a host, and contain no credentials".into());
    }
    Ok(url)
}

pub(super) struct DownloadInbound {
    cfg: RouterDatabaseCfg,
    slot: Arc<Slot>,
    tx: mpsc::Sender<ProxyRequest>,
    rx: mpsc::Receiver<ProxyRequest>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stop: tokio::sync::watch::Sender<bool>,
}
impl DownloadInbound {
    pub(super) fn new(cfg: RouterDatabaseCfg, slot: Arc<Slot>) -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            cfg,
            slot,
            tx,
            rx,
            task: Mutex::new(None),
            stop: tokio::sync::watch::channel(false).0,
        }
    }
}
impl Drop for DownloadInbound {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
#[async_trait]
impl Inbound for DownloadInbound {
    async fn init(&self) -> std::result::Result<(), SError> {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return Ok(());
        }
        let cfg = self.cfg.clone();
        let slot = self.slot.clone();
        let tx = self.tx.clone();
        let mut stopped = self.stop.subscribe();
        *task = Some(tokio::spawn(async move {
            let result = async {
                let source = tokio::select! {
                    result = tokio::time::timeout(Duration::from_secs(300), download(&cfg, tx)) => result??,
                    _ = stopped.changed() => return Err("database download cancelled".into()),
                };
                let import_cfg = cfg.clone();
                tokio::task::spawn_blocking(move || {
                    RedbDatabase::import(&import_cfg, source.path())
                })
                .await?
            }
            .await;
            match result {
                Ok(db) => {
                    *slot.value.write().unwrap() = Ok(Arc::new(db));
                    tracing::info!(tag = %cfg.tag(), "router database ready");
                }
                Err(error) => {
                    *slot.value.write().unwrap() = Err(error.to_string());
                    tracing::error!(tag = %cfg.tag(), %error, "router database download/import failed");
                }
            }
        }));
        Ok(())
    }
    async fn accept(&mut self) -> std::result::Result<ProxyRequest, SError> {
        let mut req = self.rx.recv().await.ok_or(SError::InboundUnavailable)?;
        req.user_context_mut().inbound_tag = self.cfg.tag().to_owned();
        Ok(req)
    }
    async fn shutdown(&self) -> std::result::Result<(), SError> {
        self.stop.send_replace(true);
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            // A blocking import cannot be aborted. Join it before releasing the
            // inbound so it cannot publish/open a database after shutdown returns.
            let _ = task.await;
        }
        Ok(())
    }
}
// Keep the transport private to this module rather than changing public TCP APIs.
struct DownloadStream(tokio::io::DuplexStream);
impl tokio::io::AsyncRead for DownloadStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for DownloadStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl TcpTrait for DownloadStream {}
#[cfg(not(feature = "dns-server"))]
impl TcpTrait for tokio_rustls_jls::client::TlsStream<tokio::io::DuplexStream> {}

async fn download(
    cfg: &RouterDatabaseCfg,
    tx: mpsc::Sender<ProxyRequest>,
) -> Result<tempfile::NamedTempFile> {
    let mut url = parse_url(cfg.url())?;
    for _ in 0..10 {
        let host = url
            .host_str()
            .ok_or("missing download host")?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = url.port_or_known_default().ok_or("missing download port")?;
        let dst = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => std::net::SocketAddr::new(ip, port).into(),
            Err(_) => SocksAddr::from_domain(host.to_owned(), port),
        };
        let (client, server) = tokio::io::duplex(64 * 1024);
        tx.send(ProxyRequest::Tcp(TcpSession {
            stream: Box::new(DownloadStream(server)),
            dst,
            src_addr: None,
            user_context: UserContext {
                ..Default::default()
            },
        }))
        .await?;
        let stream: AnyTcp = if url.scheme() == "https" {
            let roots = rustls_jls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            let provider = rustls_jls::crypto::CryptoProvider::get_default()
                .cloned()
                .or_else(|| {
                    #[cfg(feature = "ring")]
                    {
                        Some(Arc::new(rustls_jls::crypto::ring::default_provider()))
                    }
                    #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
                    {
                        Some(Arc::new(rustls_jls::crypto::aws_lc_rs::default_provider()))
                    }
                    #[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
                    {
                        None
                    }
                })
                .ok_or("HTTPS database downloads require a TLS crypto provider")?;
            let mut tls = rustls_jls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            // Database sources use ordinary HTTPS with certificate verification.
            tls.jls_config.enable = false;
            let name = rustls_jls::pki_types::ServerName::try_from(host.to_owned())?;
            Box::new(
                tokio_rustls_jls::TlsConnector::from(Arc::new(tls))
                    .connect(name, client)
                    .await?,
            )
        } else {
            Box::new(DownloadStream(client))
        };
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        // Dropping the guard also cancels the connection on timeout/shutdown.
        struct Connection(tokio::task::JoinHandle<()>);
        impl Drop for Connection {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _connection = Connection(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        let target = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let request = hyper::Request::builder()
            .uri(target)
            .header("Host", authority)
            .header(
                "User-Agent",
                concat!("shadowquic/", env!("CARGO_PKG_VERSION")),
            )
            .body(Empty::<Bytes>::new())?;
        let response = sender.send_request(request).await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .ok_or("redirect without location")?
                .to_str()?;
            url = parse_url(url.join(location)?.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("database download HTTP status {}", response.status()).into());
        }
        let mut source = tempfile::NamedTempFile::new()?;
        let mut body = response.into_body();
        let mut size = 0usize;
        while let Some(frame) = body.frame().await {
            if let Ok(bytes) = frame?.into_data() {
                size = size
                    .checked_add(bytes.len())
                    .ok_or("database download too large")?;
                if size > 512 * 1024 * 1024 {
                    return Err("database download exceeds 512 MiB".into());
                }
                source.write_all(&bytes)?;
            }
        }
        source.flush()?;
        return Ok(source);
    }
    Err("too many database download redirects".into())
}
