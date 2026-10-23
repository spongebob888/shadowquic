#![cfg(feature = "restful-api")]

use std::{net::SocketAddr, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use shadowquic::config::Config;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn call(address: SocketAddr, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    call_with_authorization(address, method, path, body, None).await
}

async fn call_with_authorization(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Value,
    authorization: Option<&str>,
) -> (StatusCode, Value) {
    let stream = TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json");
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    let request = request
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    task.abort();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn unused_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

struct DashboardSource {
    html: Vec<u8>,
    requests: tokio::sync::mpsc::UnboundedSender<String>,
}

#[tokio::test]
#[ignore = "downloads Squirrelboard v0.1.2 from GitHub"]
async fn dashboard_downloads_default_squirrelboard_release() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let temp = tempfile::tempdir().unwrap();
        let api_addr = unused_address().await;
        let root = temp.path().join("ui");
        let config: Config = serde_saphyr::from_str(&format!(
            "inbounds: [{{tag: inbound, type: socks, bind-addr: '127.0.0.1:0'}}]\n\
             outbounds: [{{tag: direct, type: direct}}]\n\
             restful-api:\n  bind-addr: '{api_addr}'\n  ui-path: '{}'",
            root.display()
        ))
        .unwrap();
        let manager = config.build_manager().await.unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(manager.run_until(async {
            let _ = stopped.await;
        }));
        loop {
            assert!(!task.is_finished(), "manager exited before serving UI");
            if TcpStream::connect(api_addr).await.is_ok()
                && fetch_dashboard(api_addr, "GET", "/").await.0 == StatusCode::OK
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, _, html) = fetch_dashboard(api_addr, "GET", "/").await;
        let html = std::str::from_utf8(&html).unwrap();
        assert!(html.contains("<script"));
        assert!(html.contains("<style"));
        assert_eq!(
            html.as_bytes(),
            std::fs::read(root.join("index.html")).unwrap()
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[async_trait::async_trait]
impl shadowquic::Outbound for DashboardSource {
    async fn handle(
        &self,
        request: shadowquic::ProxyRequest,
    ) -> Result<(), shadowquic::error::SError> {
        assert_eq!(request.user_context().inbound_tag, "dashboard-download");
        assert!(request.user_context().observer.upgrade().is_none());
        assert_eq!(request.dst().to_string(), "dashboard.test:80");
        let shadowquic::ProxyRequest::Tcp(mut session) = request else {
            panic!()
        };
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(session.stream.read_u8().await?);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(!head.to_ascii_lowercase().contains("authorization:"));
        let target = head.split_whitespace().nth(1).unwrap();
        self.requests.send(target.into()).unwrap();
        if target == "/start" {
            session
                .stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /index.html\r\nContent-Length: 0\r\n\r\n",
                )
                .await?;
        } else {
            assert_eq!(target, "/index.html");
            session
                .stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        self.html.len()
                    )
                    .as_bytes(),
                )
                .await?;
            session.stream.write_all(&self.html).await?;
        }
        Ok(())
    }
}

async fn fetch_dashboard(
    address: SocketAddr,
    method: &str,
    path: &str,
) -> (StatusCode, hyper::HeaderMap, Bytes) {
    let stream = TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let response = sender
        .send_request(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (head, body) = response.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    task.abort();
    (head.status, head.headers, body)
}

#[tokio::test]
async fn dashboard_startup_routes_redirects_serves_html_and_reuses_cached_file() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("ui");
        let html = b"<html>dashboard</html>".to_vec();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        for startup in 0..2 {
            let api_addr = unused_address().await;
            let router = if cfg!(feature = "plugin") {
                "default-outbound: blocked\n  src: 'return function(ctx) if ctx.inbound_tag == \"dashboard-download\" then return \"download\" end return \"blocked\" end'"
            } else { "default-outbound: download" };
            let config: Config = serde_saphyr::from_str(&format!(
                "inbounds: [{{tag: inbound, type: socks, bind-addr: '127.0.0.1:0'}}]\n\
                 outbounds: [{{tag: blocked, type: drop}}, {{tag: download, type: direct}}]\n\
                 router:\n  {router}\n\
                 restful-api:\n  bind-addr: '{api_addr}'\n  tag: dashboard-download\n  secret: test-secret\n  ui-path: '{}'\n  ui-download-url: http://dashboard.test/start",
                root.display()
            )).unwrap();
            let mut manager = config.build_manager().await.unwrap();
            assert!(manager.inbounds.contains_key("dashboard-download"));
            assert!(!manager.observers.contains_key("dashboard-download"));
            manager.outbounds.insert("download".into(), std::sync::Arc::new(DashboardSource { html: html.clone(), requests: requests.clone() }));
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(manager.run_until(async { let _ = stopped.await; }));
            loop {
                assert!(!task.is_finished(), "manager exited before serving UI");
                if TcpStream::connect(api_addr).await.is_ok()
                    && fetch_dashboard(api_addr, "GET", "/").await.0 == StatusCode::OK { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let (status, headers, body) = fetch_dashboard(api_addr, "GET", "/").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers["content-type"], "text/html; charset=utf-8");
            assert_eq!(body.as_ref(), b"<html>dashboard</html>");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            let (_, headers, body) = fetch_dashboard(api_addr, "HEAD", "/").await;
            assert_eq!(headers["content-length"], "22");
            assert!(body.is_empty());
            assert_eq!(call(api_addr, "GET", "/api/v1/traffic", Value::Null).await.0, StatusCode::UNAUTHORIZED);
            let (status, traffic) = call_with_authorization(api_addr, "GET", "/api/v1/traffic", Value::Null, Some("Bearer test-secret")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(traffic["inbounds"].as_array().unwrap().len(), 1);
            if startup == 0 {
                assert_eq!(received.recv().await.unwrap(), "/start");
                assert_eq!(received.recv().await.unwrap(), "/index.html");
            }
            assert!(received.try_recv().is_err());
            stop.send(()).unwrap();
            task.await.unwrap().unwrap();
        }
        // Omitting ui-path never registers a download inbound.
        let config: Config = serde_saphyr::from_str("inbounds: [{tag: inbound, type: socks, bind-addr: '127.0.0.1:0'}]\noutbounds: [{tag: direct, type: direct}]\nrestful-api: {bind-addr: '127.0.0.1:0', tag: dashboard-download}").unwrap();
        assert!(!config.build_manager().await.unwrap().inbounds.contains_key("dashboard-download"));
    }).await.unwrap();
}

#[tokio::test]
async fn configured_selector_switches_proxy_sessions_and_probes() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let socks_addr = unused_address().await;
        let api_addr = unused_address().await;
        let dns = if cfg!(feature = "dns-server") {
            "dns: [{tag: resolver, type: dns-system, bind-addr: '127.0.0.1:0'}]"
        } else {
            ""
        };
        let resolver = if cfg!(feature = "dns-server") {
            ", dns: resolver"
        } else {
            ""
        };
        let config: Config = serde_saphyr::from_str(&format!(
            r#"
inbounds:
  - {{tag: inbound, type: socks, bind-addr: "{socks_addr}"}}
{dns}
outbounds:
  - {{tag: choice, type: selector, outbounds: [direct, blocked]}}
  - {{tag: direct, type: direct{resolver}}}
  - {{tag: blocked, type: drop}}
restful-api:
  bind-addr: "{api_addr}"
"#
        ))
        .unwrap();
        let manager = config.build_manager().await.unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(manager.run_until(async {
            let _ = stopped.await;
        }));
        loop {
            if TcpStream::connect(api_addr).await.is_ok() {
                break;
            }
            assert!(!task.is_finished());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(socks_addr).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.read_exact(&mut [0; 2]).await.unwrap();
        let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
        connect.extend_from_slice(&target.port().to_be_bytes());
        client.write_all(&connect).await.unwrap();
        client.read_exact(&mut [0; 10]).await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        client.write_all(b"before").await.unwrap();
        peer.read_exact(&mut [0; 6]).await.unwrap();
        let path = "/api/v1/outbounds/choice/selection";
        assert_eq!(
            call(api_addr, "PUT", path, json!({"selected": "blocked"}))
                .await
                .0,
            StatusCode::OK
        );
        // Switching does not close the original session or change its target.
        peer.write_all(b"after").await.unwrap();
        let mut response = [0; 5];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"after");
        let probe_body = json!({"url": format!("http://localhost:{}/health", target.port())});
        assert_eq!(
            call(
                api_addr,
                "POST",
                "/api/v1/outbounds/choice/probe",
                probe_body.clone()
            )
            .await
            .0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            call(api_addr, "PUT", path, json!({"selected": "direct"}))
                .await
                .0,
            StatusCode::OK
        );
        let probe = tokio::spawn(call(
            api_addr,
            "POST",
            "/api/v1/outbounds/choice/probe",
            probe_body,
        ));
        let (mut probe_peer, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            probe_peer.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        probe_peer
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(probe.await.unwrap().0, StatusCode::OK);
        let (_, sessions) = call(api_addr, "GET", "/api/v1/connections", Value::Null).await;
        assert_eq!(sessions["connections"].as_array().unwrap().len(), 1);
        assert_eq!(sessions["connections"][0]["outbound"], "choice");
        drop(client);
        drop(peer);
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configured_api_allows_wildcard_binding_with_optional_secret() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for secret in [None, Some(""), Some("integration-secret")] {
            let address = unused_address().await;
            let secret_config = secret
                .map(|secret| format!("  secret: '{secret}'"))
                .unwrap_or_default();
            let config: Config = serde_saphyr::from_str(&format!(
                "inbounds: [{{tag: inbound, type: socks, bind-addr: '127.0.0.1:0'}}]\n\
                 outbounds: [{{tag: direct, type: direct}}]\n\
                 restful-api:\n  bind-addr: '0.0.0.0:{}'\n{secret_config}",
                address.port()
            ))
            .unwrap();
            let manager = config.build_manager().await.unwrap();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(manager.run_until(async {
                let _ = stopped.await;
            }));
            loop {
                if TcpStream::connect(address).await.is_ok() {
                    break;
                }
                assert!(!task.is_finished(), "manager failed before API startup");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let status = call(address, "GET", "/api/v1/traffic", Value::Null).await.0;
            assert_eq!(
                status,
                if secret == Some("integration-secret") {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::OK
                }
            );
            let (status, body) = call_with_authorization(
                address,
                "GET",
                "/api/v1/traffic",
                Value::Null,
                Some("Bearer integration-secret"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["inbounds"][0]["inbound"], "inbound");
            stop.send(()).unwrap();
            task.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configured_api_tracks_socks_traffic_probes_direct_and_exposes_memory() {
    configured_api_tracks_tcp_traffic("socks").await;
}

#[cfg(feature = "mixed")]
#[tokio::test]
async fn configured_api_tracks_mixed_traffic_probes_direct_and_exposes_memory() {
    configured_api_tracks_tcp_traffic("mixed").await;
}

async fn configured_api_tracks_tcp_traffic(inbound_type: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let socks_addr = unused_address().await;
        let api_addr = unused_address().await;
        let dns = if cfg!(feature = "dns-server") {
            "dns: [{tag: resolver, type: dns-system, bind-addr: '127.0.0.1:0'}]"
        } else {
            ""
        };
        let resolver = if cfg!(feature = "dns-server") {
            ", dns: resolver"
        } else {
            ""
        };
        let config: Config = serde_saphyr::from_str(&format!(
            r#"
inbounds:
  - {{tag: inbound, type: {inbound_type}, bind-addr: "{socks_addr}"}}
{dns}
outbounds:
  - {{tag: outbound, type: direct{resolver}}}
restful-api:
  bind-addr: "{api_addr}"
"#
        ))
        .unwrap();
        let manager = config.build_manager().await.unwrap();
        assert_eq!(manager.observers.len(), 1);
        assert!(manager.observers.contains_key("inbound"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(manager.run_until(async {
            let _ = stopped.await;
        }));
        loop {
            if TcpStream::connect(api_addr).await.is_ok() {
                break;
            }
            assert!(!task.is_finished(), "manager failed before API startup");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (status, outbounds) = call(api_addr, "GET", "/api/v1/outbounds", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(outbounds["outbounds"].as_array().unwrap().len(), 1);
        assert_eq!(outbounds["outbounds"][0]["tag"], "outbound");
        assert_eq!(
            call(
                api_addr,
                "POST",
                "/api/v1/outbounds/resolver/probe",
                json!({"url": "http://localhost/"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (status, memory) = call(api_addr, "GET", "/api/v1/memory", Value::Null).await;
        if !cfg!(feature = "jemalloc") {
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(memory["error"]["code"], "not_found");
        } else if cfg!(any(
            target_os = "android",
            all(
                any(target_os = "linux", target_os = "macos"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )) {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(memory["allocator"], "jemalloc");
            let allocated = memory["allocated_bytes"].as_u64().unwrap();
            assert!(allocated > 0);
            assert!(memory["active_bytes"].as_u64().unwrap() >= allocated);
            assert!(memory["resident_bytes"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(socks_addr).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut auth = [0; 2];
        client.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [5, 0]);
        let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
        connect.extend_from_slice(&target.port().to_be_bytes());
        client.write_all(&connect).await.unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        let (mut peer, _) = listener.accept().await.unwrap();
        client.write_all(b"upload").await.unwrap();
        peer.read_exact(&mut [0; 6]).await.unwrap();
        peer.write_all(b"response").await.unwrap();
        client.read_exact(&mut [0; 8]).await.unwrap();
        let (_, sessions) = call(api_addr, "GET", "/api/v1/connections", Value::Null).await;
        assert_eq!(sessions["connections"].as_array().unwrap().len(), 1);
        let connection = &sessions["connections"][0];
        assert_eq!(connection["half_closed"], false);
        assert_eq!(connection["inbound"], "inbound");
        assert_eq!(connection["outbound"], "outbound");
        assert_eq!(connection["dst_ip"], "127.0.0.1");
        assert_eq!(connection["dst_port"], target.port());
        assert_eq!(connection["upload_bytes"], 6);
        assert_eq!(connection["download_bytes"], 8);
        assert!(connection["first_response_ms"].is_u64());
        assert!(connection["duration_ms"].is_u64());
        assert!(connection.get("created_at").is_none());
        let (_, before) = call(api_addr, "GET", "/api/v1/traffic", Value::Null).await;
        assert_eq!(before["inbounds"][0]["tcp_conns"], 1);

        // The probe connects directly through the selected outbound and leaves totals unchanged.
        let probe = tokio::spawn(call(
            api_addr,
            "POST",
            "/api/v1/outbounds/outbound/probe",
            json!({"url": format!("http://localhost:{}/health", target.port())}),
        ));
        let (mut probe_peer, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            probe_peer.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"GET /health HTTP/1.1\r\n"));
        probe_peer
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(probe.await.unwrap().0, StatusCode::OK);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), probe_peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            call(api_addr, "GET", "/api/v1/traffic", Value::Null)
                .await
                .1,
            before
        );
        drop(client);
        drop(peer);
        loop {
            let (_, traffic) = call(api_addr, "GET", "/api/v1/traffic", Value::Null).await;
            if traffic["inbounds"][0]["tcp_conns"] == 0 {
                assert_eq!(
                    traffic["inbounds"][0],
                    json!({
                        "inbound": "inbound",
                        "tcp_upload_bytes": 6, "tcp_download_bytes": 8,
                        "udp_upload_bytes": 0, "udp_download_bytes": 0,
                        "tcp_conns": 0, "udp_conns": 0,
                    })
                );
                break;
            }
            tokio::task::yield_now().await;
        }
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(TcpStream::connect(api_addr).await.is_err());
    })
    .await
    .unwrap();
}
