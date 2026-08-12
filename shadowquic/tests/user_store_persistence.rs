use std::{path::PathBuf, time::Duration};

use shadowquic::{
    Inbound, Manager,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    direct::outbound::DirectOut,
    msgs::socks5::SocksAddr,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    squic::{inbound::UserManager, outbound::connect_tcp},
    utils::user_store::{PersistedUser, UserStore, load_store, save_store},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const RESTORE_SERVER_ADDR: &str = "127.0.0.1:24458";
const FLUSH_SERVER_ADDR: &str = "127.0.0.1:24468";
const TCP_BYTES: usize = 4096;

fn store_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "shadowquic-user-store-{}-{}.yaml",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn server_cfg(addr: &str, store: PathBuf, flush_interval: u64) -> ShadowQuicServerCfg {
    ShadowQuicServerCfg {
        bind_addr: addr.parse().unwrap(),
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
        user_store: Some(store),
        store_flush_interval: flush_interval,
        ..Default::default()
    }
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

#[tokio::test]
async fn user_store_restores_users_and_stats_and_persists_api_changes() {
    let path = store_path("restore");

    // Simulate a previous run: a user added via API with accumulated traffic.
    save_store(
        &path,
        &UserStore {
            users: vec![PersistedUser {
                username: "carol".into(),
                password: "carol-pass".into(),
                tcp_sent: 111,
                tcp_recv: 222,
                udp_sent: 333,
                udp_recv: 444,
            }],
        },
    )
    .unwrap();

    let server = ShadowQuicServer::new(server_cfg(RESTORE_SERVER_ADDR, path.clone(), 0))
        .await
        .unwrap();
    server.init().await.expect("server init failed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let admin = client_at(RESTORE_SERVER_ADDR, "admin", "admin-pass");

    // Store-only user is merged into the auth set.
    let mut users = admin.list_users().await.unwrap();
    users.sort();
    assert_eq!(users, vec!["admin", "bob", "carol"]);
    client_at(RESTORE_SERVER_ADDR, "carol", "carol-pass")
        .get_conn()
        .await
        .expect("restored user should connect");

    // Persisted traffic counters are restored.
    let stats = admin.get_user_stats("carol").await.unwrap();
    assert_eq!(stats.tcp_sent, 111);
    assert_eq!(stats.tcp_recv, 222);
    assert_eq!(stats.udp_sent, 333);
    assert_eq!(stats.udp_recv, 444);

    // add-user via API is flushed to disk immediately.
    admin
        .add_user(AuthUser {
            username: "alice".into(),
            password: "alice-pass".into(),
        })
        .await
        .unwrap();
    let store = load_store(&path).unwrap().expect("store file should exist");
    let mut stored: Vec<_> = store.users.iter().map(|u| u.username.as_str()).collect();
    stored.sort();
    assert_eq!(stored, vec!["admin", "alice", "bob", "carol"]);
    let carol = store
        .users
        .iter()
        .find(|u| u.username == "carol")
        .expect("carol should be persisted");
    assert_eq!(carol.tcp_sent, 111);
    assert_eq!(carol.udp_recv, 444);

    // remove-user via API is flushed to disk immediately.
    admin.remove_user("carol").await.unwrap();
    let store = load_store(&path).unwrap().expect("store file should exist");
    assert!(!store.users.iter().any(|u| u.username == "carol"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn user_store_periodic_flush_writes_traffic_stats() {
    let path = store_path("flush");

    let tcp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("tcp echo listener should bind");
    let tcp_addr = tcp_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = tcp_listener.accept().await.unwrap();
        let mut buf = vec![0; TCP_BYTES];
        stream.read_exact(&mut buf).await.unwrap();
        stream.write_all(&buf).await.unwrap();
    });

    let sq_server = ShadowQuicServer::new(server_cfg(FLUSH_SERVER_ADDR, path.clone(), 1))
        .await
        .unwrap();
    let server = Manager {
        inbound: Box::new(sq_server),
        outbound: Box::<DirectOut>::default(),
    };
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bob = client_at(FLUSH_SERVER_ADDR, "bob", "bob-pass");
    let bob_conn = bob.get_conn().await.expect("bob should connect");
    let payload = vec![0x5a; TCP_BYTES];
    let mut tcp_stream = connect_tcp(&bob_conn, SocksAddr::from(tcp_addr))
        .await
        .expect("tcp request should open");
    tcp_stream.write_all(&payload).await.unwrap();
    tcp_stream.flush().await.unwrap();
    let mut echo = vec![0; TCP_BYTES];
    tcp_stream.read_exact(&mut echo).await.unwrap();
    assert_eq!(echo, payload);

    // The periodic flush (1s interval) should persist bob's traffic to disk.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Some(store)) = load_store(&path)
                && let Some(bob) = store.users.iter().find(|u| u.username == "bob")
                && bob.tcp_sent == TCP_BYTES as u64
                && bob.tcp_recv == TCP_BYTES as u64
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("periodic flush should persist traffic stats");

    let _ = std::fs::remove_file(&path);
}
