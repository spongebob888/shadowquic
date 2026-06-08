use std::{net::SocketAddr, time::Duration};

use bytes::Bytes;
use shadowquic::{
    Inbound, Manager,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    direct::outbound::DirectOut,
    msgs::socks5::SocksAddr,
    msgs::squic::SQExtError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    squic::{
        inbound::UserManager,
        outbound::{associate_udp, connect_tcp},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};

const SERVER_ADDR: &str = "127.0.0.1:4458";
const STATS_SERVER_ADDR: &str = "127.0.0.1:4468";
const TRAFFIC_STATS_SERVER_ADDR: &str = "127.0.0.1:4478";
const TCP_STATS_BYTES: usize = 4096;
const UDP_STATS_BYTES: usize = 777;

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
            AuthUser {
                username: "admin_bob".into(),
                password: "admin-bob-pass".into(),
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

    let admin = client("admin", "admin-pass");
    assert_users(&admin, &["admin", "bob", "admin_bob"]).await;

    assert_eq!(add_user(&admin, "alice", "alice-pass").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob", "alice"]).await;
    assert_connects("alice", "alice-pass").await;

    assert_eq!(add_user(&admin, "alice", "alice-new-pass").await, Ok(()));
    assert_users(&admin, &["admin", "bob", "admin_bob", "alice"]).await;
    assert_connects("alice", "alice-new-pass").await;

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
async fn shadowquic_user_api_get_stats_and_kill_user_conns() {
    let mut server = ShadowQuicServer::new(ShadowQuicServerCfg {
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

    let admin = client_at(STATS_SERVER_ADDR, "admin", "admin-pass");
    let bob = client_at(STATS_SERVER_ADDR, "bob", "bob-pass");
    let bob_conn = bob.get_conn().await.expect("bob should connect");
    let _tcp_stream = connect_tcp(
        &bob_conn,
        SocksAddr::from_domain("example.com".to_owned(), 80),
    )
    .await
    .expect("tcp request should open");
    let accepted_req = tokio::time::timeout(Duration::from_secs(2), server.accept())
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
    drop(accepted_req);
    let stats = admin
        .get_user_stats("bob")
        .await
        .expect("admin should get bob stats after killing conns");
    assert_eq!(stats.tcp_conns, 0);
    assert_eq!(stats.udp_conns, 0);
}

#[tokio::test]
async fn shadowquic_user_api_get_stats_tracks_tcp_and_udp_bytes() {
    let tcp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("tcp echo listener should bind");
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let udp_socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("udp echo socket should bind");
    let udp_addr = udp_socket.local_addr().unwrap();

    tokio::spawn(tcp_echo_once(tcp_listener, TCP_STATS_BYTES));
    tokio::spawn(udp_echo_once(udp_socket, UDP_STATS_BYTES));

    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: TRAFFIC_STATS_SERVER_ADDR.parse().unwrap(),
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

    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::<DirectOut>::default(),
    };
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let admin = client_at(TRAFFIC_STATS_SERVER_ADDR, "admin", "admin-pass");
    let bob = client_at(TRAFFIC_STATS_SERVER_ADDR, "bob", "bob-pass");
    let bob_conn = bob.get_conn().await.expect("bob should connect");

    let tcp_payload = vec![0x5a; TCP_STATS_BYTES];
    let mut tcp_stream = connect_tcp(&bob_conn, SocksAddr::from(tcp_addr))
        .await
        .expect("tcp request should open");
    tcp_stream.write_all(&tcp_payload).await.unwrap();
    tcp_stream.flush().await.unwrap();
    let mut tcp_echo = vec![0; TCP_STATS_BYTES];
    tcp_stream.read_exact(&mut tcp_echo).await.unwrap();
    assert_eq!(tcp_echo, tcp_payload);

    let udp_payload = Bytes::from(vec![0xa5; UDP_STATS_BYTES]);
    let udp_bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let (udp_send, mut udp_recv) = associate_udp(&bob_conn, SocksAddr::from(udp_bind), true)
        .await
        .expect("udp association should open");
    udp_send
        .send((udp_payload.clone(), SocksAddr::from(udp_addr)))
        .await
        .expect("udp payload should send");
    let (udp_echo, udp_echo_addr) = tokio::time::timeout(Duration::from_secs(2), udp_recv.recv())
        .await
        .expect("udp echo should arrive before timeout")
        .expect("udp echo channel should stay open");
    assert_eq!(udp_echo, udp_payload);
    assert_eq!(udp_echo_addr, SocksAddr::from(udp_addr));

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let stats = admin
                .get_user_stats("bob")
                .await
                .expect("admin should get bob stats");
            if stats.tcp_recv == TCP_STATS_BYTES as u64
                && stats.tcp_sent == TCP_STATS_BYTES as u64
                && stats.udp_recv == UDP_STATS_BYTES as u64
                && stats.udp_sent == UDP_STATS_BYTES as u64
            {
                assert_eq!(stats.conn_num, 1);
                assert_eq!(stats.tcp_conns, 1);
                assert_eq!(stats.udp_conns, 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("traffic stats should be updated");
}

fn client(username: &str, password: &str) -> ShadowQuicClient {
    client_at(SERVER_ADDR, username, password)
}

fn client_at(addr: &str, username: &str, password: &str) -> ShadowQuicClient {
    ShadowQuicClient::new(ShadowQuicClientCfg {
        username: username.into(),
        password: password.into(),
        addr: addr.into(),
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

async fn assert_connection_closed(conn: &shadowquic::shadowquic::outbound::ShadowQuicConn) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while conn.close_reason().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connection should be closed");
}

async fn assert_connects(username: &str, password: &str) {
    let client = client(username, password);
    client
        .get_conn()
        .await
        .unwrap_or_else(|error| panic!("{username} should connect: {error}"));
}

async fn add_user(
    client: &ShadowQuicClient,
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

async fn assert_users(client: &ShadowQuicClient, expected: &[&str]) {
    let mut users = client.list_users().await.unwrap();
    users.sort();
    let mut expected = expected
        .iter()
        .map(|username| username.to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(users, expected);
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

async fn tcp_echo_once(listener: TcpListener, len: usize) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = vec![0; len];
    stream.read_exact(&mut buf).await.unwrap();
    stream.write_all(&buf).await.unwrap();
}

async fn udp_echo_once(socket: UdpSocket, len: usize) {
    let mut buf = vec![0; len];
    let (read_len, addr) = socket.recv_from(&mut buf).await.unwrap();
    assert_eq!(read_len, len);
    socket.send_to(&buf, addr).await.unwrap();
}
