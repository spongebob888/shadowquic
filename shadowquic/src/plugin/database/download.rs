use super::{Result, Slot, import_database};
use crate::{
    AnyTcp, Inbound, ProxyRequest,
    config::RouterDatabaseCfg,
    error::SError,
    utils::{
        http::{self, HttpConnection},
        proxy::tcp_request,
        tls::tls_connector,
    },
};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
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
                    import_database(&import_cfg, source.path())
                })
                .await?
            }
            .await;
            match result {
                Ok(db) => {
                    *slot.value.write().unwrap() = Ok(db);
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
        let dst = http::destination(&url)?;
        let (client, request, _request_guard) = tcp_request(dst, Default::default(), 64 * 1024);
        tx.send(request).await?;
        let stream: AnyTcp = if url.scheme() == "https" {
            let name = rustls_jls::pki_types::ServerName::try_from(host.to_owned())?;
            Box::new(tls_connector()?.connect(name, client).await?)
        } else {
            Box::new(client)
        };
        let mut connection = HttpConnection::new(stream, None).await?;
        let request = http::get_request(&url)
            .header(
                "User-Agent",
                concat!("shadowquic/", env!("CARGO_PKG_VERSION")),
            )
            .body(Empty::<Bytes>::new())?;
        let response = connection.send_request(request).await?;
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
