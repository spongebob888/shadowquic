mod common;

use std::{sync::Arc, time::Duration};

use shadowquic::{
    Inbound, Manager,
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
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        tag: "inbound".into(),
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

    let (request_sender, mut requests) = tokio::sync::mpsc::channel(1);
    let manager = Manager::single(
        Box::new(server),
        Arc::new(common::CaptureOutbound(request_sender)),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let manager_task = tokio::spawn(manager.run_until(async {
        let _ = stopped.await;
    }));

    let admin = client_at(STATS_SERVER_ADDR, "admin", "admin-pass");
    let bob = client_at(STATS_SERVER_ADDR, "bob", "bob-pass");
    let bob_conn = bob.get_conn().await.expect("bob should connect");
    let _tcp_stream = connect_tcp(
        &bob_conn,
        SocksAddr::from_domain("example.com".to_owned(), 80),
    )
    .await
    .expect("tcp request should open");
    let accepted_req = tokio::time::timeout(Duration::from_secs(2), requests.recv())
        .await
        .expect("manager should dispatch tcp request")
        .expect("request channel should stay open");

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
    drop(accepted_req);
    let stats = admin
        .get_user_stats("bob")
        .await
        .expect("admin should get bob stats after killing conns");
    assert_eq!(stats.tcp_conns, 0);
    assert_eq!(stats.udp_conns, 0);

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), manager_task)
        .await
        .expect("manager should shut down")
        .expect("manager task should succeed")
        .expect("manager should succeed");
}

fn client(username: &str, password: &str) -> SunnyQuicClient {
    client_at(SERVER_ADDR, username, password)
}

fn client_at(addr: &str, username: &str, password: &str) -> SunnyQuicClient {
    SunnyQuicClient::new(
        SunnyQuicClientCfg {
            tag: "outbound".into(),
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
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    )
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

/// Forwards datagrams between one client and the server. From the server's
/// first reply on, the client's datagrams are held back until `release`
/// fires, which leaves the handshake waiting on the server's side.
#[cfg(feature = "sunnyquic-noq")]
async fn spawn_holding_relay(
    server: std::net::SocketAddr,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let front = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let back = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    back.connect(server).await.unwrap();
    let addr = front.local_addr().unwrap();
    let (holding_tx, holding_rx) = tokio::sync::oneshot::channel();
    let (release_tx, mut release_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let (mut from_client, mut from_server) = ([0u8; 2048], [0u8; 2048]);
        let mut client = None;
        let mut holding_tx = Some(holding_tx);
        let mut released = false;
        let mut held: Vec<Vec<u8>> = Vec::new();
        loop {
            tokio::select! {
                Ok((n, from)) = front.recv_from(&mut from_client) => {
                    client = Some(from);
                    if holding_tx.is_none() && !released {
                        held.push(from_client[..n].to_vec());
                    } else {
                        let _ = back.send(&from_client[..n]).await;
                    }
                }
                Ok(n) = back.recv(&mut from_server) => {
                    if let Some(holding) = holding_tx.take() {
                        let _ = holding.send(());
                    }
                    if let Some(client) = client {
                        let _ = front.send_to(&from_server[..n], client).await;
                    }
                }
                _ = &mut release_rx, if !released => {
                    released = true;
                    for datagram in held.drain(..) {
                        let _ = back.send(&datagram).await;
                    }
                }
            }
        }
    });
    (addr, holding_rx, release_tx)
}

/// A handshake can take long. A user removed while theirs is under way must
/// not be authenticated by a list of users taken when the handshake began.
#[cfg(feature = "sunnyquic-noq")]
#[tokio::test]
async fn user_removed_during_its_handshake_is_not_authenticated() {
    let server_addr = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let server = SunnyQuicServer::new(SunnyQuicServerCfg {
        bind_addr: server_addr,
        users: vec![
            AuthUser {
                username: "admin".into(),
                password: "admin-pass".into(),
            },
            AuthUser {
                username: "alice".into(),
                password: "alice-pass".into(),
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

    let admin = client_at(&server_addr.to_string(), "admin", "admin-pass");
    assert_users(&admin, &["admin", "alice"]).await;

    let (relay_addr, holding, release) = spawn_holding_relay(server_addr).await;
    let alice = tokio::spawn(async move {
        let client = client_at(&relay_addr.to_string(), "alice", "alice-pass");
        let conn = client.get_conn().await.ok()?;
        tokio::time::timeout(Duration::from_secs(2), get_peer_conn_stats(&conn))
            .await
            .ok()?
            .ok()
    });
    tokio::time::timeout(Duration::from_secs(5), holding)
        .await
        .expect("the server should answer the handshake")
        .unwrap();
    assert_eq!(admin.remove_user("alice").await, Ok(()));
    release.send(()).unwrap();

    let response = tokio::time::timeout(Duration::from_secs(10), alice)
        .await
        .expect("alice's attempt should end")
        .unwrap();
    assert!(
        !matches!(response, Some(Ok(_)) | Some(Err(SQExtError::NotAvailable))),
        "a removed user was authenticated"
    );
}
