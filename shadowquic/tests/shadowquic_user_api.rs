use std::time::Duration;

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    msgs::squic::SQExtError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};

const SERVER_ADDR: &str = "127.0.0.1:4458";

#[tokio::test]
async fn shadowquic_user_api_add_remove_and_permissions() {
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: SERVER_ADDR.parse().unwrap(),
        users: vec![
            AuthUser {
                username: "admin".into(),
                password: "admin-pass".into(),
            },
            AuthUser {
                username: "bob".into(),
                password: "bob-pass".into(),
            },
        ],
        jls_upstream: JlsUpstream {
            addr: "localhost:443".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        zero_rtt: false,
        gso: false,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await
    .unwrap();

    server.init().await.expect("server init failed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut admin = client("admin", "admin-pass");
    assert_eq!(admin.add_user("alice", "alice-pass").await.unwrap(), Ok(()));
    assert_connects("alice", "alice-pass").await;

    assert_eq!(
        admin.add_user("alice", "alice-new-pass").await.unwrap(),
        Ok(())
    );
    assert_connects("alice", "alice-new-pass").await;

    assert_eq!(admin.remove_user("alice").await.unwrap(), Ok(()));
    assert_rejected_or_timeout("alice", "alice-new-pass").await;
    assert_eq!(
        admin.remove_user("alice").await.unwrap(),
        Err(SQExtError::NotFound)
    );

    let mut bob = client("bob", "bob-pass");
    assert_eq!(
        bob.add_user("mallory", "mallory-pass").await.unwrap(),
        Err(SQExtError::PermissionDenied)
    );
    assert_eq!(
        bob.remove_user("admin").await.unwrap(),
        Err(SQExtError::PermissionDenied)
    );
}

fn client(username: &str, password: &str) -> ShadowQuicClient {
    ShadowQuicClient::new(ShadowQuicClientCfg {
        username: username.into(),
        password: password.into(),
        addr: SERVER_ADDR.into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: false,
        gso: false,
        over_stream: true,
        ..Default::default()
    })
}

async fn assert_connects(username: &str, password: &str) {
    let client = client(username, password);
    client
        .get_conn()
        .await
        .unwrap_or_else(|error| panic!("{username} should connect: {error}"));
}

async fn assert_rejected_or_timeout(username: &str, password: &str) {
    let client = client(username, password);
    let conn = match tokio::time::timeout(Duration::from_secs(2), client.get_conn()).await {
        Err(_) => return,
        Ok(Err(_)) => return,
        Ok(Ok(conn)) => conn,
    };

    let rejected = tokio::time::timeout(Duration::from_millis(500), async {
        while conn.close_reason().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    assert!(
        rejected.is_ok(),
        "{username} should be rejected or closed after deletion"
    );
}
