use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    inbound::{JlsServer, JlsServerCfg},
    outbound::{JlsClient, JlsClientCfg},
};
use crate::{
    TcpTrait,
    config::{AuthUser, JlsUpstream},
};

impl TcpTrait for tokio::io::DuplexStream {}
#[tokio::test]
async fn test_jls_transform() {
    let _ = rustls_jls::crypto::ring::default_provider().install_default();
    let (client_stream, server_stream) = tokio::io::duplex(1024);

    let server_cfg = JlsServerCfg {
        users: vec![AuthUser {
            username: "user".to_string(),
            password: "password".to_string(),
        }],
        server_name: Some("localhost".to_string()),
        jls_upstream: JlsUpstream {
            addr: "127.0.0.1:8080".to_string(),
            rate_limit: 1024,
        },
        alpn: vec!["h3".to_string()],
        zero_rtt: true,
    };

    let server_transform = JlsServer::new(server_cfg).unwrap();

    let client_cfg = JlsClientCfg {
        username: "user".to_string(),
        password: "password".to_string(),
        server_name: "localhost".to_string(),
        alpn: vec!["h3".to_string()],
        zero_rtt: true,
        #[cfg(target_os = "android")]
        protect_path: None,
    };

    let client_transform = JlsClient::new(client_cfg);

    tokio::spawn(async move {
        let mut stream = server_transform
            .transform_tcp(Box::new(server_stream))
            .await
            .expect("server transform failed");
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await.expect("server read failed");
        assert_eq!(&buf[..n], b"hello world");
        stream
            .write_all(b"hello world back")
            .await
            .expect("server write failed");
    });

    let mut stream = client_transform
        .connect_stream(Box::new(client_stream))
        .await
        .expect("client transform failed");
    stream
        .write_all(b"hello world")
        .await
        .expect("client write failed");
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.expect("client read failed");
    assert_eq!(&buf[..n], b"hello world back");
}
