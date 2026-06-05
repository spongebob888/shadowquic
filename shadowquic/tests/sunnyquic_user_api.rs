use std::time::Duration;

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg, default_initial_mtu,
    },
    msgs::squic::SQExtError,
    squic::outbound::get_peer_conn_stats,
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};

const SERVER_ADDR: &str = "127.0.0.1:4459";

#[tokio::test]
async fn sunnyquic_user_api_add_remove_list_and_permissions() {
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
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
        alpn: vec!["h3".into()],
        zero_rtt: false,
        gso: false,
        initial_mtu: default_initial_mtu(),
        congestion_control: CongestionControl::Bbr,
        server_name: "localhost".into(),
        cert_path: "../assets/certs/localhost.crt".into(),
        key_path: "../assets/certs/localhost.key".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    server.init().await.expect("server init failed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut admin = client("admin", "admin-pass");
    assert_users(&mut admin, &["admin", "bob"]).await;

    assert_eq!(admin.add_user("alice", "alice-pass").await.unwrap(), Ok(()));
    assert_users(&mut admin, &["admin", "bob", "alice"]).await;
    assert_authenticated("alice", "alice-pass").await;

    assert_eq!(
        admin.add_user("alice", "alice-new-pass").await.unwrap(),
        Ok(())
    );
    assert_users(&mut admin, &["admin", "bob", "alice"]).await;
    assert_authenticated("alice", "alice-new-pass").await;
    assert_rejected_or_timeout("alice", "alice-pass").await;

    assert_eq!(admin.remove_user("alice").await.unwrap(), Ok(()));
    assert_users(&mut admin, &["admin", "bob"]).await;
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
    assert_eq!(
        bob.list_users().await.unwrap(),
        Err(SQExtError::PermissionDenied)
    );
}

fn client(username: &str, password: &str) -> SunnyQuicClient {
    SunnyQuicClient::new(SunnyQuicClientCfg {
        username: username.into(),
        password: password.into(),
        addr: SERVER_ADDR.into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        cert_path: Some("../assets/certs/MyCA.pem".into()),
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: false,
        gso: false,
        over_stream: true,
        ..Default::default()
    })
}

async fn assert_users(client: &mut SunnyQuicClient, expected: &[&str]) {
    let mut users = client.list_users().await.unwrap().unwrap();
    users.sort();
    let mut expected = expected
        .iter()
        .map(|username| username.to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(users, expected);
}

async fn assert_authenticated(username: &str, password: &str) {
    let client = client(username, password);
    let conn = client
        .get_conn()
        .await
        .unwrap_or_else(|error| panic!("{username} should connect: {error}"));
    let response = tokio::time::timeout(Duration::from_secs(2), get_peer_conn_stats(&conn))
        .await
        .unwrap_or_else(|_| panic!("{username} should authenticate before timeout"))
        .unwrap_or_else(|error| panic!("{username} stats request should complete: {error}"));
    assert!(
        response.is_ok() || response == Err(SQExtError::NotAvailable),
        "{username} should get an authenticated extension response"
    );
}

async fn assert_rejected_or_timeout(username: &str, password: &str) {
    let client = client(username, password);
    let conn = match tokio::time::timeout(Duration::from_secs(2), client.get_conn()).await {
        Err(_) => return,
        Ok(Err(_)) => return,
        Ok(Ok(conn)) => conn,
    };

    let response = tokio::time::timeout(Duration::from_secs(2), get_peer_conn_stats(&conn)).await;
    assert!(
        response.is_err() || matches!(response, Ok(Err(_))),
        "{username} should be rejected or time out"
    );
}
