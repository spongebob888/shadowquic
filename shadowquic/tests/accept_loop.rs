//! The server's accept loop must keep serving valid clients while another
//! handshake stalls. Upstream issue #206.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};
use tokio::net::UdpSocket;

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A JLS upstream that never answers.
async fn black_hole() -> SocketAddr {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        let mut packet = [0; 2048];
        while upstream.recv_from(&mut packet).await.is_ok() {}
    });
    addr
}

async fn server(zero_rtt: bool) -> (SocketAddr, ShadowQuicServer) {
    let server_addr = unused_udp_addr();
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "right-password".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: black_hole().await.to_string(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        zero_rtt,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await
    .unwrap();
    server.init().await.unwrap();
    (server_addr, server)
}

fn client(server_addr: SocketAddr, password: &str, alpn: Vec<String>) -> ShadowQuicClient {
    ShadowQuicClient::new(
        ShadowQuicClientCfg {
            tag: "outbound".into(),
            addr: server_addr.to_string(),
            username: "user".into(),
            password: password.into(),
            server_name: "localhost".into(),
            alpn,
            zero_rtt: false,
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            ..Default::default()
        },
        Arc::new(shadowquic::dns::ResolverManager::new()),
    )
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
async fn stalled_handshake(server_zero_rtt: bool) {
    let (server_addr, _server) = server(server_zero_rtt).await;

    // ClientHello larger than one Initial packet.
    let mut alpn: Vec<String> = (0..6).map(|i| format!("{i}").repeat(250)).collect();
    alpn.push("h3".into());
    let stalled = client(
        first_packet_only_relay(server_addr).await,
        "right-password",
        alpn,
    );
    let stalled = tokio::spawn(async move { stalled.get_conn().await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let good = client(server_addr, "right-password", vec!["h3".into()]);
    let conn = tokio::time::timeout(Duration::from_secs(2), good.get_conn())
        .await
        .expect("a stalled handshake must not block a valid client")
        .expect("valid client should connect");
    assert_eq!(conn.authed.wait().await.as_ref().unwrap(), "user");
    stalled.abort();
}

#[tokio::test]
async fn stalled_handshake_does_not_block_valid_client() {
    stalled_handshake(false).await;
}

/// With 0-RTT the server waits for the whole ClientHello: that wait must not
/// block the accept loop either.
#[tokio::test]
async fn stalled_handshake_does_not_block_valid_client_zero_rtt() {
    stalled_handshake(true).await;
}
