use std::time::Duration;

use shadowquic::Inbound;
use shadowquic::config::{
    AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
    default_initial_mtu,
};
use shadowquic::shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient};
use shadowquic::squic::outbound::get_peer_conn_stats;

use tracing::Level;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::test]
async fn main() {
    let filter = tracing_subscriber::filter::Targets::new().with_target("shadowquic", Level::INFO);

    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();

    // 1. Initialize server
    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: "127.0.0.1:4449".parse().unwrap(),
        users: vec![AuthUser {
            username: "123".into(),
            password: "123".into(),
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
    .await
    .unwrap();

    sq_server.init().await.expect("Failed to initialize server");

    // 2. Initialize client
    let sq_client = ShadowQuicClient::new(ShadowQuicClientCfg {
        password: "123".into(),
        username: "123".into(),
        addr: "127.0.0.1:4449".parse().unwrap(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        zero_rtt: true,
        over_stream: true,
        ..Default::default()
    });

    // Wait a brief moment for the server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Connect client and get SQConn
    let conn = sq_client
        .get_conn()
        .await
        .expect("Failed to connect client");

    // Wait a brief moment to ensure connection handshake is complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Query peer connection stats directly
    let stats_res = get_peer_conn_stats(&conn).await;

    // 5. Verify the result
    assert!(
        stats_res.is_ok(),
        "get_peer_conn_stats returned an error: {:?}",
        stats_res.err()
    );
    let inner_res = stats_res.unwrap();
    assert!(
        inner_res.is_ok(),
        "inner result was an error: {:?}",
        inner_res.err()
    );

    let stats = inner_res.unwrap();
    println!(
        "Retrieved Peer ConnStats: lost_packets={}, sent_packets={}, rtt={}ms, current_mtu={}",
        stats.lost_packets, stats.sent_packets, stats.rtt, stats.current_mtu
    );

    // We should expect reasonable stats values
    assert!(stats.current_mtu > 0, "MTU should be greater than 0");
    assert!(stats.rtt >= 0.0, "RTT should be non-negative");
}
