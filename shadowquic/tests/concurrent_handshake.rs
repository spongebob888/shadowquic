use std::{net::SocketAddr, time::Duration};

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};
use tokio::net::UdpSocket;

#[tokio::test]
async fn bad_password_client_does_not_block_valid_client_handshake() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move {
        let mut packet = [0; 2048];
        loop {
            // Act as a UDP black hole so the invalid client's handshake remains
            // in flight for the duration of the valid client's handshake.
            upstream.recv_from(&mut packet).await.unwrap();
        }
    });

    let server_addr = unused_udp_addr();
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "right-password".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: upstream_addr.to_string(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        zero_rtt: false,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await
    .unwrap();
    server.init().await.unwrap();

    let bad_client = client(server_addr, "wrong-password");
    let mut bad_client_task = tokio::spawn(async move { bad_client.get_conn().await });

    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut bad_client_task)
            .await
            .is_err(),
        "wrong-password handshake should still be in flight"
    );

    let good_client = client(server_addr, "right-password");
    let good_conn = tokio::time::timeout(Duration::from_secs(2), good_client.get_conn())
        .await
        .expect("wrong-password client must not block the valid client handshake")
        .expect("valid client should connect and complete its handshake");

    assert_eq!(good_conn.authed.wait().await.as_ref().unwrap(), "user");
    assert!(
        !bad_client_task.is_finished(),
        "valid handshake should finish while the wrong-password handshake is still in flight"
    );

    bad_client_task.abort();
    upstream_task.abort();
}

fn client(server_addr: SocketAddr, password: &str) -> ShadowQuicClient {
    ShadowQuicClient::new(ShadowQuicClientCfg {
        addr: server_addr.to_string(),
        username: "user".into(),
        password: password.into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        zero_rtt: false,
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
}

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
