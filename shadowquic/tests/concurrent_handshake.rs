use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};
use tokio::net::UdpSocket;
use tracing::{Level, info, level_filters::LevelFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// A forwarded packet must reach the upstream well within this window.
/// The JLS forward path relays the client's Initial at accept time, so a working
/// forward arrives in milliseconds. The wrong-password client's PTO retransmission
/// does not fire until ~1s, so if the upstream still has received nothing by this
/// deadline the Initial was dropped by the forward path and the test must fail.
const FIRST_FORWARD_WINDOW: Duration = Duration::from_millis(500);

#[tokio::test]
async fn bad_password_client_does_not_block_valid_client_handshake() {
    let filter = tracing_subscriber::filter::Targets::new()
        // Enable the `INFO` level for anything in `my_crate`
        .with_target("concurrent_handshake", Level::TRACE)
        .with_target("quinn", LevelFilter::WARN)
        .with_target("quinn_jls", LevelFilter::TRACE)
        .with_target("quinn_proto_jls", LevelFilter::TRACE)
        .with_target("shadowquic", LevelFilter::TRACE);

    // Enable the `DEBUG` level for a specific module.

    // Build a new subscriber with the `fmt` layer using the `Targets`
    // filter we constructed above.
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    // When the upstream first receives a forwarded packet; None until then.
    let first_recv: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let first_recv_task = first_recv.clone();
    let upstream_task = tokio::spawn(async move {
        let mut packet = [0; 2048];
        loop {
            // Act as a UDP black hole so the invalid client's handshake remains
            // in flight for the duration of the valid client's handshake.

            info!("upstream waiting for packet");
            upstream.recv_from(&mut packet).await.unwrap();
            let mut first = first_recv_task.lock().unwrap();
            if first.is_none() {
                *first = Some(Instant::now());
            }
            drop(first);
            info!("upstream received packet, dropping it");
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
    let bad_client_started = Instant::now();
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

    // The wrong-password handshake must be relayed to the upstream as soon as the
    // server accepts it. Wait for the first forwarded packet within a window far
    // below the client's PTO interval (~1s). If the upstream receives nothing in
    // this window, the Initial was silently dropped by the JLS forward path
    // (quinn-jls `insert_forward_conn` sends on a fresh socket before tokio has
    // registered it, so the first datagram is lost and only the ~1s PTO
    // retransmission would ever arrive): the test must fail on that extra delay.
    let deadline = bad_client_started + FIRST_FORWARD_WINDOW;
    loop {
        if first_recv.lock().unwrap().is_some() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "upstream received no forwarded packet within {:?} of the wrong-password \
                 handshake start: the client's Initial was dropped by the JLS forward path, \
                 so the upstream only sees the ~1s PTO retransmission",
                FIRST_FORWARD_WINDOW
            );
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(10))).await;
    }
    info!(
        "upstream first packet arrived after {:?}",
        first_recv
            .lock()
            .unwrap()
            .unwrap()
            .duration_since(bad_client_started)
    );

    bad_client_task.abort();
    upstream_task.abort();
}

fn client(server_addr: SocketAddr, password: &str) -> ShadowQuicClient {
    ShadowQuicClient::new(
        ShadowQuicClientCfg {
            addr: server_addr.to_string(),
            username: "user".into(),
            password: password.into(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            zero_rtt: false,
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            ..Default::default()
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    )
}

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
