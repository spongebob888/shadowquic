use std::net::SocketAddr;

use shadowquic::Manager;
use shadowquic::config::{AuthUser, MixedServerCfg, SocksClientCfg, SocksServerCfg};
use shadowquic::direct::outbound::DirectOut;
use shadowquic::mixed::inbound::MixedServer;
use shadowquic::socks::inbound::SocksServer;
use shadowquic::socks::outbound::SocksClient;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;

use tracing::{Level, debug, info, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::test]
async fn test_http_head_forward() {
    init_tracing();

    let target_addr = spawn_http_target().await;
    spawn_mixed_proxy_chain().await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect("127.0.0.1:11080").await.unwrap();

    let req = format!(
        "HEAD http://{}/hello HTTP/1.1\r\n\
         Host: {}\r\n\
         Proxy-Connection: keep-alive\r\n\
         User-Agent: test-http-head-forward\r\n\
         \r\n",
        target_addr, target_addr
    );

    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let resp = read_to_end(&mut stream).await;
    let text = String::from_utf8_lossy(&resp);

    info!("HEAD response:\n{}", text);

    assert!(
        text.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {}",
        text
    );
    assert!(
        text.contains("X-Method: HEAD"),
        "response should contain X-Method: HEAD, got: {}",
        text
    );
    assert!(
        text.contains("X-Path: /hello"),
        "response should contain X-Path: /hello, got: {}",
        text
    );
}

#[tokio::test]
async fn test_http_get_forward() {
    init_tracing();

    let target_addr = spawn_http_target().await;
    spawn_mixed_proxy_chain().await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect("127.0.0.1:11080").await.unwrap();

    let req = format!(
        "GET http://{}/hello?name=shadowquic HTTP/1.1\r\n\
         Host: {}\r\n\
         Proxy-Connection: keep-alive\r\n\
         User-Agent: test-http-get-forward\r\n\
         \r\n",
        target_addr, target_addr
    );

    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let resp = read_to_end(&mut stream).await;
    let text = String::from_utf8_lossy(&resp);

    info!("GET response:\n{}", text);

    assert!(
        text.starts_with("HTTP/1.1 200 OK"),
        "unexpected response: {}",
        text
    );
    assert!(
        text.contains("X-Method: GET"),
        "response should contain X-Method: GET, got: {}",
        text
    );
    assert!(
        text.contains("X-Path: /hello?name=shadowquic"),
        "response should contain rewritten origin-form path, got: {}",
        text
    );
    assert!(
        text.contains("X-Proxy-Connection-Seen: false"),
        "proxy header should be stripped, got: {}",
        text
    );
}

#[tokio::test]
async fn test_http_connect_tunnel() {
    init_tracing();

    let echo_addr = spawn_echo_target().await;
    spawn_mixed_proxy_chain().await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect("127.0.0.1:11080").await.unwrap();

    let req = format!(
        "CONNECT {} HTTP/1.1\r\n\
         Host: {}\r\n\
         \r\n",
        echo_addr, echo_addr
    );

    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut head = vec![0u8; 1024];
    let n = stream.read(&mut head).await.unwrap();
    let text = String::from_utf8_lossy(&head[..n]);

    info!("CONNECT response:\n{}", text);

    assert!(
        text.starts_with("HTTP/1.1 200 Connection Established"),
        "unexpected CONNECT response: {}",
        text
    );

    let payload = b"hello through connect tunnel";
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();

    let mut recv = vec![0u8; payload.len()];
    stream.read_exact(&mut recv).await.unwrap();

    assert_eq!(&recv, payload);
}

fn init_tracing() {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("shadowquic", tracing::level_filters::LevelFilter::TRACE)
                .with_target(
                    "http_proxy_test",
                    tracing::level_filters::LevelFilter::TRACE,
                ),
        )
        .try_init();

    trace!("tracing initialized");
}

async fn spawn_mixed_proxy_chain() {
    // entry proxy: mixed inbound -> socks outbound
    let mixed_server = MixedServer::new(MixedServerCfg {
        bind_addr: "127.0.0.1:11080".parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let socks_client = SocksClient::new(SocksClientCfg {
        addr: "[::1]:11094".into(),
        username: Some("test".into()),
        password: Some("test".into()),
    });

    let client = Manager {
        inbound: Box::new(mixed_server),
        outbound: Box::new(socks_client),
    };

    // upstream proxy: socks inbound -> direct outbound
    let socks_server = SocksServer::new(SocksServerCfg {
        bind_addr: "[::1]:11094".parse().unwrap(),
        users: vec![AuthUser {
            username: "test".into(),
            password: "test".into(),
        }],
    })
    .await
    .unwrap();

    let direct = DirectOut::default();

    let server = Manager {
        inbound: Box::new(socks_server),
        outbound: Box::new(direct),
    };

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn spawn_http_target() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, peer) = listener.accept().await.unwrap();
            debug!("http target accepted connection from {}", peer);

            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];

                loop {
                    let n = stream.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if buf.len() > 16 * 1024 {
                        return;
                    }
                }

                let req = String::from_utf8_lossy(&buf);
                debug!("http target got request:\n{}", req);

                let mut lines = req.split("\r\n");
                let request_line = lines.next().unwrap_or_default();

                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();

                let proxy_connection_seen = req
                    .lines()
                    .any(|l| l.to_ascii_lowercase().starts_with("proxy-connection:"));

                let body = if method.eq_ignore_ascii_case("HEAD") {
                    Vec::new()
                } else {
                    b"hello-from-target".to_vec()
                };

                let resp = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     X-Method: {}\r\n\
                     X-Path: {}\r\n\
                     X-Proxy-Connection-Seen: {}\r\n\
                     \r\n",
                    body.len(),
                    method,
                    path,
                    proxy_connection_seen,
                );

                stream.write_all(resp.as_bytes()).await.unwrap();
                if !body.is_empty() {
                    stream.write_all(&body).await.unwrap();
                }
                stream.flush().await.unwrap();
            });
        }
    });

    addr
}

async fn spawn_echo_target() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (mut stream, peer) = listener.accept().await.unwrap();
            debug!("echo target accepted connection from {}", peer);

            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).await.unwrap();
                    stream.flush().await.unwrap();
                }
            });
        }
    });

    addr
}

async fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];

    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => panic!("read failed: {e}"),
            Err(_) => break,
        }
    }

    out
}
