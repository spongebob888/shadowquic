use std::{net::SocketAddr, time::Duration};

use shadowquic::{
    Inbound,
    config::{
        AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
        default_initial_mtu,
    },
    quic::QuicConnection,
    shadowquic::{
        inbound::ShadowQuicServer,
        outbound::{ShadowQuicClient, ShadowQuicConn},
    },
};
use tokio::net::UdpSocket;

/// Well under the observation window, so several keep-alive packets are due.
const KEEP_ALIVE_MS: u32 = 200;

/// Regression for the wedged-connection detector. It decides whether a connection
/// is still doing useful work from [`QuicConnection::data_progress`], so keep-alive
/// PINGs must not advance that counter. If they did, a peer that stopped granting
/// stream credit would keep looking "alive" forever and never be dropped, and the
/// instance would silently stop serving — the failure the detector exists to end.
#[tokio::test]
async fn keep_alive_advances_packets_but_not_application_progress() {
    let server_addr = unused_udp_addr();
    // JLS forwards the client's Initial here; a black hole is enough because the
    // QUIC connection itself terminates on the server.
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();

    let server = ShadowQuicServer::new(server_cfg(server_addr, upstream_addr))
        .await
        .unwrap();
    server.init().await.unwrap();

    let keep_alive = client(server_addr, KEEP_ALIVE_MS);
    let plain = client(server_addr, 0);

    let keep_alive_conn = keep_alive.get_conn().await.unwrap();
    let plain_conn = plain.get_conn().await.unwrap();
    keep_alive_conn.authed.wait().await.as_ref().unwrap();
    plain_conn.authed.wait().await.as_ref().unwrap();

    // Let the handshake tail settle so the baseline is not an ACK still in flight.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let packets_before = sent_packets(&keep_alive_conn);
    let progress_before = keep_alive_conn.data_progress().unwrap();
    let plain_packets_before = sent_packets(&plain_conn);

    // Five keep-alive intervals with no application traffic in either direction.
    tokio::time::sleep(Duration::from_millis(5 * KEEP_ALIVE_MS as u64)).await;

    assert!(
        sent_packets(&keep_alive_conn) > packets_before,
        "keep-alive must put packets on the wire, otherwise this test proves nothing"
    );
    assert_eq!(
        keep_alive_conn.data_progress().unwrap(),
        progress_before,
        "keep-alive PINGs must not be counted as application progress"
    );
    assert_eq!(
        sent_packets(&plain_conn),
        plain_packets_before,
        "with keep-alive off an idle connection sends nothing, which is the only \
         reason the packet counter ever worked as a liveness signal"
    );
}

fn sent_packets(conn: &ShadowQuicConn) -> u64 {
    conn.get_conn_stats().unwrap().sent_packets
}

fn client(server_addr: SocketAddr, keep_alive_interval: u32) -> ShadowQuicClient {
    ShadowQuicClient::new(
        ShadowQuicClientCfg {
            addr: server_addr.to_string(),
            username: "user".into(),
            password: "password".into(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            zero_rtt: false,
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            keep_alive_interval,
            ..Default::default()
        },
        std::sync::Arc::new(shadowquic::dns::ResolverManager::new()),
    )
}

fn server_cfg(server_addr: SocketAddr, upstream_addr: SocketAddr) -> ShadowQuicServerCfg {
    ShadowQuicServerCfg {
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "password".into(),
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
    }
}

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
