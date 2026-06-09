use std::time::Duration;

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        SunnyQuicClientCfg, SunnyQuicServerCfg, default_initial_mtu,
    },
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};

#[tokio::test]
async fn shadowquic_client_sqconn_records_authenticated_username() {
    let username = "authed-shadowquic-user";
    let password = "authed-shadowquic-password";
    let bind_addr = "127.0.0.1:4491".parse().unwrap();

    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr,
        users: vec![AuthUser {
            username: username.into(),
            password: password.into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "localhost:443".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await.unwrap();
    server.init().await.unwrap();

    let client = ShadowQuicClient::new(ShadowQuicClientCfg {
        addr: bind_addr.to_string(),
        username: username.into(),
        password: password.into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    });

    let conn = client.get_conn().await.unwrap();

    assert_eq!(conn.authed.wait().await.as_ref().unwrap(), username);
}

#[tokio::test]
async fn sunnyquic_client_sqconn_records_authenticated_username() {
    let username = "authed-sunnyquic-user";
    let password = "authed-sunnyquic-password";
    let bind_addr = "127.0.0.1:4492".parse().unwrap();

    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        bind_addr,
        users: vec![AuthUser {
            username: username.into(),
            password: password.into(),
        }],
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await.unwrap();
    server.init().await.unwrap();

    let client = SunnyQuicClient::new(SunnyQuicClientCfg {
        addr: bind_addr.to_string(),
        username: username.into(),
        password: password.into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        zero_rtt: true,
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        cert_path: Some("../assets/certs/MyCA.pem".into()),
        ..Default::default()
    });

    let conn = client.get_conn().await.unwrap();
    let authed = tokio::time::timeout(Duration::from_secs(3), conn.authed.wait())
        .await
        .unwrap();

    assert_eq!(authed.as_ref().unwrap(), username);
}
