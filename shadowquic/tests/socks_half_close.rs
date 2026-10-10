use fast_socks5::client::{Config, Socks5Stream};
use shadowquic::config::{
    AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
    SocksServerCfg, default_initial_mtu,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Duration;

use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};

use tracing::{Level, level_filters::LevelFilter, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const SOCKS_SERVER: &str = "127.0.0.1:1085";
const QUIC_SERVER: &str = "127.0.0.1:4465";
const TARGET_PORT: u16 = 1465;

#[tokio::test]
async fn socks_client_half_close_keeps_direct_outbound_read_open() {
    init_tracing();
    spawn_proxies().await;

    let target = tokio::spawn(target_peer(TARGET_PORT));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut config = Config::default();
    config.set_skip_auth(false);
    let mut stream = Socks5Stream::connect(SOCKS_SERVER, "127.0.0.1".into(), TARGET_PORT, config)
        .await
        .unwrap();

    stream.write_all(b"ping").await.unwrap();
    // Half-close the SOCKS client's TCP stream. The write half is closed,
    // but the read half must stay open for the target's reply.
    stream.shutdown().await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("timed out waiting for the target's reply after half-close")
        .unwrap();
    assert_eq!(response, b"pong-after-half-close");

    // Propagate any assertion failure from the target peer.
    target.await.unwrap().unwrap();
}

async fn target_peer(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let (mut stream, _addr) = listener.accept().await?;

    // Wait for EOF. It arrives only if the direct outbound propagates the
    // SOCKS client's half-close instead of fully closing the connection.
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut received))
        .await
        .expect("timed out waiting for the direct outbound to half-close")?;
    assert_eq!(received, b"ping");

    // The direct outbound's read half must still be open. If it had closed
    // the connection entirely, this write would fail.
    stream.write_all(b"pong-after-half-close").await?;
    stream.shutdown().await?;
    Ok(())
}

async fn spawn_proxies() {
    let socks_server = SocksServer::new(SocksServerCfg {
        tag: "inbound".into(),
        default_outbound: None,
        bind_addr: SOCKS_SERVER.parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();
    let sq_client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            password: "123".into(),
            username: "123".into(),
            addr: QUIC_SERVER.parse().unwrap(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            zero_rtt: true,
            over_stream: true,
            ..Default::default()
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    );

    let client = Manager::single(Box::new(socks_server), std::sync::Arc::new(sq_client));

    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: "[::]:4465".parse().unwrap(),
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
    let direct_client = DirectOut::default();
    let server = Manager::single(Box::new(sq_server), std::sync::Arc::new(direct_client));

    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::spawn(client.run());
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn init_tracing() {
    let filter = tracing_subscriber::filter::Targets::new()
        .with_target("socks_half_close", Level::TRACE)
        .with_target("shadowquic", LevelFilter::TRACE);
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();
    trace!("running socks half-close test");
}
