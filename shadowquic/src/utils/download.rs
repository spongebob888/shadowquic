//! Bounded HTTP(S) downloads through a managed, tagged internal inbound.

use std::{future::Future, io::Write, pin::Pin, sync::Mutex, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use tokio::sync::{mpsc, watch};
use url::Url;

use super::{
    http::{self, HttpConnection},
    proxy::tcp_request,
    tls::tls_connector,
};
use crate::{AnyTcp, Inbound, ProxyRequest, UserContext, error::SError};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Job = Box<dyn FnOnce(DownloadClient) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

pub(crate) fn parse_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("download URL must be HTTP(S), have a host, and contain no credentials".into());
    }
    Ok(url)
}

struct State {
    job: Option<Job>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Runs one startup job and exposes its HTTP connections to the normal router.
/// Graceful shutdown cancels network I/O and joins the job, including any
/// awaited blocking installation/import. Background work has no client observer.
pub(crate) struct DownloadInbound {
    tag: String,
    tx: mpsc::Sender<ProxyRequest>,
    rx: mpsc::Receiver<ProxyRequest>,
    state: Mutex<State>,
    stop: watch::Sender<bool>,
}

impl DownloadInbound {
    pub(crate) fn new<F, Fut>(tag: String, job: F) -> Self
    where
        F: FnOnce(DownloadClient) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(1);
        Self {
            tag,
            tx,
            rx,
            state: Mutex::new(State {
                job: Some(Box::new(move |client| Box::pin(job(client)))),
                task: None,
            }),
            stop: watch::channel(false).0,
        }
    }
}

impl Drop for DownloadInbound {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.state.get_mut().unwrap().task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl Inbound for DownloadInbound {
    async fn init(&self) -> std::result::Result<(), SError> {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.job.take() {
            let client = DownloadClient {
                tag: self.tag.clone(),
                tx: self.tx.clone(),
                stopped: self.stop.subscribe(),
            };
            state.task = Some(tokio::spawn(job(client)));
        }
        Ok(())
    }

    async fn accept(&mut self) -> std::result::Result<ProxyRequest, SError> {
        self.rx.recv().await.ok_or(SError::InboundUnavailable)
    }

    async fn shutdown(&self) -> std::result::Result<(), SError> {
        self.stop.send_replace(true);
        let task = self.state.lock().unwrap().task.take();
        if let Some(task) = task {
            let _ = task.await;
        }
        Ok(())
    }
}

pub(crate) struct DownloadOptions {
    pub timeout: Duration,
    pub max_size: usize,
}

pub(crate) struct DownloadClient {
    tag: String,
    tx: mpsc::Sender<ProxyRequest>,
    stopped: watch::Receiver<bool>,
}

impl DownloadClient {
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.stopped.borrow()
    }

    /// Streams into the caller's sink so large downloads can stay on disk.
    /// The timeout covers redirects, queueing, TLS, headers, and the response body.
    pub(crate) async fn download(
        &mut self,
        url: &str,
        output: &mut (dyn Write + Send),
        options: DownloadOptions,
    ) -> Result<()> {
        if self.is_cancelled() {
            return Err("download cancelled".into());
        }
        tokio::select! {
            result = tokio::time::timeout(options.timeout, transfer(&self.tag, &self.tx, url, output, options.max_size)) => result?,
            _ = self.stopped.changed() => Err("download cancelled".into()),
        }
    }
}

async fn transfer(
    tag: &str,
    tx: &mpsc::Sender<ProxyRequest>,
    value: &str,
    output: &mut (dyn Write + Send),
    limit: usize,
) -> Result<()> {
    let mut url = parse_url(value)?;
    for _ in 0..10 {
        let host = url
            .host_str()
            .ok_or("missing download host")?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let context = UserContext {
            inbound_tag: tag.to_owned(),
            ..Default::default()
        };
        let (client, request, _guard) = tcp_request(http::destination(&url)?, context, 16 * 1024);
        tx.send(request).await?;
        let stream: AnyTcp = if url.scheme() == "https" {
            let name = rustls_jls::pki_types::ServerName::try_from(host.to_owned())?;
            Box::new(tls_connector()?.connect(name, client).await?)
        } else {
            Box::new(client)
        };
        let mut connection = HttpConnection::new(stream, Some(32 * 1024)).await?;
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
                .ok_or("download redirect without location")?
                .to_str()?;
            url = parse_url(url.join(location)?.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("download HTTP status {}", response.status()).into());
        }
        let mut size = 0usize;
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            if let Ok(bytes) = frame?.into_data() {
                if bytes.len() > limit - size {
                    return Err(format!("download exceeds {limit} bytes").into());
                }
                output.write_all(&bytes)?;
                size += bytes.len();
            }
        }
        output.flush()?;
        return Ok(());
    }
    Err("too many download redirects".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::oneshot,
    };

    #[test]
    fn download_urls_require_http_hosts_without_credentials() {
        for url in ["https://example.test/archive.zip", "http://[::1]:8080/file"] {
            assert!(parse_url(url).is_ok(), "{url}");
        }
        for url in [
            "bad",
            "file:///tmp/file",
            "ftp://example.test/file",
            "https://user:pass@example.test/file",
        ] {
            assert!(parse_url(url).is_err(), "{url}");
        }
    }

    async fn response_to_download(response: &[u8], max_size: usize) -> (Result<()>, Vec<u8>) {
        let (done, result) = oneshot::channel();
        let mut inbound =
            DownloadInbound::new("download-test".into(), move |mut client| async move {
                let mut bytes = Vec::new();
                let result = client
                    .download(
                        "http://example.test/file?version=1",
                        &mut bytes,
                        DownloadOptions {
                            timeout: Duration::from_secs(2),
                            max_size,
                        },
                    )
                    .await;
                let _ = done.send((result, bytes));
            });
        inbound.init().await.unwrap();
        inbound.init().await.unwrap();
        let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
            panic!()
        };
        assert_eq!(session.user_context.inbound_tag, "download-test");
        assert!(session.user_context.observer.upgrade().is_none());
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(session.stream.read_u8().await.unwrap());
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with("GET /file?version=1 HTTP/1.1\r\n"));
        session.stream.write_all(response).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), result)
            .await
            .unwrap()
            .unwrap();
        inbound.shutdown().await.unwrap();
        result
    }

    #[tokio::test]
    async fn body_limits_cover_content_length_and_chunked_responses() {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nabcdefgh".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n".as_slice(),
        ] {
            let (result, body) = response_to_download(response, 8).await;
            result.unwrap();
            assert_eq!(body, b"abcdefgh");
            let (result, body) = response_to_download(response, 7).await;
            assert!(result.unwrap_err().to_string().contains("exceeds 7 bytes"));
            assert!(body.len() <= 7);
        }
        let (result, body) =
            response_to_download(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n", 8).await;
        assert!(result.unwrap_err().to_string().contains("404"));
        assert!(body.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_closes_a_stalled_routed_connection() {
        let (done, result) = oneshot::channel();
        let mut inbound =
            DownloadInbound::new("download-test".into(), move |mut client| async move {
                let result = client
                    .download(
                        "http://example.test/file",
                        &mut Vec::new(),
                        DownloadOptions {
                            timeout: Duration::from_secs(1),
                            max_size: 8,
                        },
                    )
                    .await;
                let _ = done.send(result);
            });
        inbound.init().await.unwrap();
        let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
            panic!()
        };
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            result
                .await
                .unwrap()
                .unwrap_err()
                .is::<tokio::time::error::Elapsed>()
        );
        assert!(session.stream.read_u8().await.is_err());
        inbound.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_joins_blocking_postprocessing() {
        let (started, ready) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let inbound = DownloadInbound::new("download-test".into(), move |_| async move {
            tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
            .unwrap();
        });
        inbound.init().await.unwrap();
        ready.await.unwrap();
        let mut shutdown = Box::pin(inbound.shutdown());
        std::future::poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
    }
}
