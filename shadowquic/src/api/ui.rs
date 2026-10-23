//! Locally served dashboard and its routed startup download.

use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};

use super::{ApiError, HttpResponse};
use crate::{
    config::ApiConfig,
    utils::download::{DownloadInbound, DownloadOptions, Result},
};

const MAX_UI_SIZE: u64 = 32 * 1024 * 1024;

pub(crate) fn download_inbound(cfg: ApiConfig) -> DownloadInbound {
    DownloadInbound::new(cfg.tag.clone(), move |mut client| async move {
        let result: Result<()> = async {
            let path = cfg.ui_path.clone().ok_or("missing ui-path")?;
            if tokio::fs::metadata(path.join("index.html"))
                .await
                .is_ok_and(|metadata| metadata.is_file())
            {
                return Ok(());
            }
            let mut html = Vec::new();
            client
                .download(
                    &cfg.ui_download_url,
                    &mut html,
                    DownloadOptions {
                        timeout: Duration::from_secs(120),
                        max_size: MAX_UI_SIZE as usize,
                    },
                )
                .await?;
            // Join installation even during shutdown so it cannot publish
            // files after the inbound has finished shutting down.
            tokio::task::spawn_blocking(move || install(&html, &path)).await??;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => tracing::info!(tag = %cfg.tag, "dashboard ready"),
            Err(_) if client.is_cancelled() => {}
            Err(error) => tracing::error!(tag = %cfg.tag, %error, "dashboard download failed"),
        }
    })
}

fn install(bytes: &[u8], path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(path)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    // Publish the complete HTML atomically without replacing an existing page.
    temporary.persist_noclobber(path.join("index.html"))?;
    Ok(())
}

pub(super) async fn serve(
    root: &Path,
    method: &Method,
    path: &str,
) -> std::result::Result<HttpResponse, ApiError> {
    if method != Method::GET && method != Method::HEAD {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "dashboard supports GET and HEAD",
        ));
    }
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .map_err(|_| ApiError::invalid("invalid dashboard path"))?;
    if decoded.contains(['\\', '\0']) || decoded.split('/').any(|part| part == "..") {
        return Err(ApiError::invalid("invalid dashboard path"));
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| not_ready())?;
    let relative = decoded.trim_start_matches('/');
    let file = if relative.is_empty() {
        root.join("index.html")
    } else {
        root.join(relative)
    };
    let file = match tokio::fs::canonicalize(&file).await {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && Path::new(relative).extension().is_none() =>
        {
            tokio::fs::canonicalize(root.join("index.html"))
                .await
                .map_err(|_| not_ready())?
        }
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "dashboard file not found",
            ));
        }
    };
    if !file.starts_with(&root) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "dashboard file not found",
        ));
    }
    let metadata = tokio::fs::metadata(&file).await.map_err(|_| not_ready())?;
    if !metadata.is_file() || metadata.len() > MAX_UI_SIZE {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "dashboard file not found",
        ));
    }
    let body = if method == Method::HEAD {
        Bytes::new()
    } else {
        Bytes::from(tokio::fs::read(&file).await.map_err(|_| not_ready())?)
    };
    let mut response = Response::new(Full::new(body));
    let content_type = match file.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    response
        .headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    response
        .headers_mut()
        .insert("content-length", metadata.len().into());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    Ok(response)
}

fn not_ready() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "dashboard_not_ready",
        "dashboard is not available yet",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Inbound, ProxyRequest};
    use http_body_util::BodyExt;

    #[test]
    fn html_install_preserves_existing_files_and_does_not_replace_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("ui");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/app.js"), b"script").unwrap();
        install(b"index", &root).unwrap();
        assert_eq!(std::fs::read(root.join("index.html")).unwrap(), b"index");
        assert_eq!(
            std::fs::read(root.join("assets/app.js")).unwrap(),
            b"script"
        );
        assert!(install(b"replacement", &root).is_err());
        assert_eq!(std::fs::read(root.join("index.html")).unwrap(), b"index");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn static_files_support_spa_head_and_mime_types_without_path_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("ui");
        install(b"index", &root).unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/app.js"), b"script").unwrap();
        for (path, expected, mime) in [
            ("/", b"index".as_slice(), "text/html; charset=utf-8"),
            (
                "/connections",
                b"index".as_slice(),
                "text/html; charset=utf-8",
            ),
            (
                "/assets/app.js",
                b"script".as_slice(),
                "text/javascript; charset=utf-8",
            ),
        ] {
            let response = serve(&root, &Method::GET, path)
                .await
                .unwrap_or_else(|e| panic!("{}", e.body.message));
            assert_eq!(response.headers()["content-type"], mime);
            assert_eq!(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .as_ref(),
                expected
            );
        }
        let response = serve(&root, &Method::HEAD, "/")
            .await
            .unwrap_or_else(|e| panic!("{}", e.body.message));
        assert_eq!(response.headers()["content-length"], "5");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        for path in ["/../outside", "/%2e%2e/outside", "/..%5coutside", "/%00"] {
            assert!(serve(&root, &Method::GET, path).await.is_err(), "{path}");
        }
        assert!(serve(&root, &Method::GET, "/missing.js").await.is_err());
        assert!(serve(&root, &Method::POST, "/").await.is_err());
        #[cfg(unix)]
        {
            std::fs::write(temp.path().join("private.html"), "private").unwrap();
            std::os::unix::fs::symlink(temp.path().join("private.html"), root.join("escape.html"))
                .unwrap();
            assert!(serve(&root, &Method::GET, "/escape.html").await.is_err());
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_a_pending_routed_download() {
        use tokio::io::AsyncReadExt;
        let temp = tempfile::tempdir().unwrap();
        let mut cfg: ApiConfig = serde_saphyr::from_str("bind-addr: '127.0.0.1:0'\ntag: ui-download\nui-download-url: http://example.test/index.html").unwrap();
        cfg.ui_path = Some(temp.path().join("ui"));
        let mut inbound = download_inbound(cfg);
        inbound.init().await.unwrap();
        let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
            panic!()
        };
        assert_eq!(session.user_context.inbound_tag, "ui-download");
        tokio::time::timeout(Duration::from_secs(2), inbound.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(session.stream.read_u8().await.is_err());
        assert!(!temp.path().join("ui").exists());
    }
}
