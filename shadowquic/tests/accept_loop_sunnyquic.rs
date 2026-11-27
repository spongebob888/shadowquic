//! The SunnyQuic server's accept loop must keep serving valid clients while
//! other handshakes fail or stall. Upstream issue #206.
#![cfg(feature = "sunnyquic-noq")]
use std::{net::SocketAddr, sync::Arc, time::Duration};

use shadowquic::{
    Inbound,
    config::{AuthUser, CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg},
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};
use tokio::net::UdpSocket;

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn server() -> (SocketAddr, SunnyQuicServer) {
    let server_addr = unused_udp_addr();
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "pass".into(),
        }],
        alpn: vec!["h3".into()],
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    server.init().await.unwrap();
    (server_addr, server)
}

fn client(server_addr: SocketAddr, trusted: bool, alpn: Vec<String>) -> SunnyQuicClient {
    SunnyQuicClient::new(
        SunnyQuicClientCfg {
            tag: "outbound".into(),
            username: "user".into(),
            password: "pass".into(),
            addr: server_addr.to_string(),
            server_name: "localhost".into(),
            alpn,
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            // Without the test CA the client refuses the server's certificate
            // and the handshake fails.
            cert_path: trusted.then(|| "../assets/certs/MyCA.pem".into()),
            ..Default::default()
        },
        Arc::new(shadowquic::dns::ResolverManager::new()),
    )
}

async fn connect_in_time(server_addr: SocketAddr, limit: Duration, why: &str) {
    let good = client(server_addr, true, vec!["h3".into()]);
    let conn = tokio::time::timeout(limit, good.get_conn())
        .await
        .expect(why)
        .expect("valid client should connect");
    assert_eq!(conn.authed.wait().await.as_ref().unwrap(), "user");
}

#[tokio::test]
async fn many_failed_handshakes_do_not_delay_valid_client() {
    let (server_addr, _server) = server().await;
    let mut bad = Vec::new();
    for _ in 0..20 {
        let c = client(server_addr, false, vec!["h3".into()]);
        bad.push(tokio::spawn(async move { c.get_conn().await.map(|_| ()) }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    connect_in_time(
        server_addr,
        Duration::from_millis(1500),
        "failed handshakes of other clients must not delay a valid client",
    )
    .await;
    for b in bad {
        b.abort();
    }
}

/// Forwards only the first datagram of the client to the server.
async fn first_packet_only_relay(server: SocketAddr) -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut seen = 0u32;
        while let Ok((n, src)) = sock.recv_from(&mut buf).await {
            if src == server {
                continue;
            }
            seen += 1;
            if seen == 1 {
                let _ = sock.send_to(&buf[..n], server).await;
            }
        }
    });
    addr
}

/// A handshake whose ClientHello never completes (its second packet is lost)
/// must not block the accept loop until the handshake times out.
#[tokio::test]
async fn stalled_handshake_does_not_block_valid_client() {
    let (server_addr, _server) = server().await;

    // ClientHello larger than one Initial packet.
    let mut alpn: Vec<String> = (0..6).map(|i| format!("{i}").repeat(250)).collect();
    alpn.push("h3".into());
    let stalled = client(first_packet_only_relay(server_addr).await, true, alpn);
    let stalled = tokio::spawn(async move { stalled.get_conn().await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(200)).await;

    connect_in_time(
        server_addr,
        Duration::from_secs(2),
        "a stalled handshake must not block a valid client",
    )
    .await;
    stalled.abort();
}
