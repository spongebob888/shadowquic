use std::time::Duration;

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, SunnyQuicClientCfg, SunnyQuicServerCfg, default_initial_mtu,
    },
    msgs::{socks5::SocksAddr, squic::SQExtError},
    squic::{
        inbound::UserManager,
        outbound::{connect_tcp, get_peer_conn_stats},
    },
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};

const SERVER_ADDR: &str = "127.0.0.1:4459";
const STATS_SERVER_ADDR: &str = "127.0.0.1:4469";

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
            AuthUser {
                username: "admin_bob".into(),
                password: "admin-bob-pass".into(),
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

    let admin = client("admin", "admin-pass");
    assert_users(&admin, &["admin", "bob", "admin_bob"]).await;

    assert_eq!(add_user(&admin, "alice", "alice-pass").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob", "alice"]).await;
    assert_authenticated("alice", "alice-pass").await;

    assert_eq!(add_user(&admin, "alice", "alice-new-pass").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob", "alice"]).await;
    assert_authenticated("alice", "alice-new-pass").await;
    assert_rejected_or_timeout("alice", "alice-pass").await;

    assert_eq!(admin.remove_user("alice").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob"]).await;
    assert_rejected_or_timeout("alice", "alice-new-pass").await;
    assert_eq!(admin.remove_user("alice").await, Err(SQExtError::NotFound),);

    let admin_bob = client("admin_bob", "admin-bob-pass");
    assert_eq!(add_user(&admin_bob, "carol", "carol-pass").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob", "carol"]).await;
    assert_eq!(admin.remove_user("carol").await, Ok(()));

    let bob = client("bob", "bob-pass");
    assert_eq!(
        add_user(&bob, "mallory", "mallory-pass").await,
        Err(SQExtError::PermissionDenied)
    );
    assert_eq!(
        bob.remove_user("admin").await,
        Err(SQExtError::PermissionDenied),
    );
    assert_eq!(bob.list_users().await, Err(SQExtError::PermissionDenied),);
}

#[tokio::test]
async fn sunnyquic_user_api_get_stats_and_kill_user_conns() {
    let mut server = SunnyQuicServer::new(SunnyQuicServerCfg {
        bind_addr: STATS_SERVER_ADDR.parse().unwrap(),
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

    let admin = client_at(STATS_SERVER_ADDR, "admin", "admin-pass");
    let bob = client_at(STATS_SERVER_ADDR, "bob", "bob-pass");
    let bob_conn = bob.get_conn().await.expect("bob should connect");
    let _tcp_stream = connect_tcp(
        &bob_conn,
        SocksAddr::from_domain("example.com".to_owned(), 80),
    )
    .await
    .expect("tcp request should open");
    let _accepted_req = tokio::time::timeout(Duration::from_secs(2), server.accept())
        .await
        .expect("server should observe tcp request")
        .expect("server accept should succeed");

    let stats = admin
        .get_user_stats("bob")
        .await
        .expect("admin should get bob stats");
    assert_eq!(stats.conn_num, 1);
    assert_eq!(stats.tcp_conns, 1);
    let all_stats = admin
        .get_all_stats()
        .await
        .expect("admin should get all stats");
    let bob_stats = all_stats
        .iter()
        .find(|stats| stats.username == "bob")
        .expect("all stats should include bob");
    assert_eq!(bob_stats.conn_num, 1);
    assert_eq!(bob_stats.tcp_conns, 1);
    assert!(all_stats.iter().any(|stats| stats.username == "admin"));
    assert!(matches!(
        admin.get_user_stats("missing").await,
        Err(SQExtError::NotFound)
    ));
    assert_eq!(
        admin.kill_user_conns("missing").await,
        Err(SQExtError::NotFound)
    );

    assert!(matches!(
        bob.get_user_stats("bob").await,
        Err(SQExtError::PermissionDenied)
    ));
    assert!(matches!(
        bob.get_all_stats().await,
        Err(SQExtError::PermissionDenied)
    ));
    assert_eq!(
        bob.kill_user_conns("bob").await,
        Err(SQExtError::PermissionDenied)
    );

    admin
        .kill_user_conns("bob")
        .await
        .expect("admin should kill bob conns");
    assert_connection_closed(&bob_conn).await;
}

fn client(username: &str, password: &str) -> SunnyQuicClient {
    client_at(SERVER_ADDR, username, password)
}

fn client_at(addr: &str, username: &str, password: &str) -> SunnyQuicClient {
    SunnyQuicClient::new(SunnyQuicClientCfg {
        username: username.into(),
        password: password.into(),
        addr: addr.into(),
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

async fn assert_connection_closed(conn: &shadowquic::sunnyquic::outbound::SunnyQuicConn) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while conn.close_reason().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connection should be closed");
}

async fn add_user(
    client: &SunnyQuicClient,
    username: &str,
    password: &str,
) -> Result<(), SQExtError> {
    client
        .add_user(AuthUser {
            username: username.to_owned(),
            password: password.to_owned(),
        })
        .await
}

async fn assert_users(client: &SunnyQuicClient, expected: &[&str]) {
    let mut users = client.list_users().await.unwrap();
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
