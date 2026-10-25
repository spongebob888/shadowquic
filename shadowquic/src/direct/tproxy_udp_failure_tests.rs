//! Real TPROXY recvmsg/raw-reply sockets in an isolated Linux network namespace.
//! Run the ignored tests with CAP_NET_RAW and loopback up. Fake IPs use 127/8
//! so packets reach the listener without changing the host's firewall or routes.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    net::UdpSocket,
    sync::Notify,
    time::{sleep, timeout},
};

use super::udp_failure_tests::{FAILED_DOMAIN, FailingDns, start_server};
use crate::{
    Manager,
    config::{ShadowQuicClientCfg, TproxyServerCfg},
    dns::{FakeIp, ResolverManager, RestoringInbound},
    quic::QuicConnection,
    shadowquic::outbound::ShadowQuicClient,
    tproxy::inbound::TproxyServer,
};

#[tokio::test]
#[ignore = "requires isolated Linux network namespace with CAP_NET_RAW and loopback up"]
async fn tproxy_udp_over_stream_initial_dns_failure_releases_session() {
    check_dns_failure(true, false).await;
}

#[tokio::test]
#[ignore = "requires isolated Linux network namespace with CAP_NET_RAW and loopback up"]
async fn tproxy_udp_over_datagram_initial_dns_failure_releases_session() {
    check_dns_failure(false, false).await;
}

#[tokio::test]
#[ignore = "requires isolated Linux network namespace with CAP_NET_RAW and loopback up"]
async fn tproxy_udp_over_stream_dns_failure_after_reply_releases_session() {
    check_dns_failure(true, true).await;
}

#[tokio::test]
#[ignore = "requires isolated Linux network namespace with CAP_NET_RAW and loopback up"]
async fn tproxy_udp_over_datagram_dns_failure_after_reply_releases_session() {
    check_dns_failure(false, true).await;
}

async fn check_dns_failure(over_stream: bool, established: bool) {
    timeout(Duration::from_secs(10), async {
        let dns = Arc::new(FailingDns(Notify::new()));
        let (server_addr, server_task) = start_server(dns.clone()).await;

        // The specific echo address takes precedence over TPROXY's wildcard
        // listener. Both UDP sockets use SO_REUSEADDR to share the port.
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        socket.set_reuse_address(true).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        let target = UdpSocket::from_std(socket.into()).unwrap();
        let port = target.local_addr().unwrap().port();
        let inbound = TproxyServer::new(TproxyServerCfg {
            tag: "inbound".into(),
            default_outbound: None,
            bind_addr: SocketAddr::from(([0, 0, 0, 0], port)),
        })
        .await
        .unwrap();
        let observer = inbound.observer.clone();
        let fake = Arc::new(
            FakeIp::new(
                "127.77.0.0/16".parse().unwrap(),
                "fd77::/112".parse().unwrap(),
            )
            .unwrap(),
        );
        let failed = SocketAddr::new(fake.allocate(FAILED_DOMAIN, false).unwrap(), port);
        let healthy = SocketAddr::new(fake.allocate("udp-echo.test", false).unwrap(), port);
        let client = Arc::new(ShadowQuicClient::new(
            ShadowQuicClientCfg {
                addr: server_addr.to_string(),
                server_name: "localhost".into(),
                username: "udp-test".into(),
                password: "udp-test-password".into(),
                over_stream,
                half_close_timeout: 0,
                ..Default::default()
            },
            Arc::new(ResolverManager::new()),
        ));
        let client_task = tokio::spawn(
            Manager::single(
                Box::new(RestoringInbound {
                    inner: Box::new(inbound),
                    fake,
                }),
                client.clone(),
            )
            .run(),
        );
        // TproxyServer::new spawns its UDP listener. Let it bind before sending.
        tokio::task::yield_now().await;

        let source = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        if established {
            echo(&source, healthy, &target).await;
            assert_eq!(observer.get_user_stats("").await.udp_conns, 1);
        }
        source.send_to(b"fail", failed).await.unwrap();
        dns.0.notified().await;
        let conn = client.quic_conn.lock().await.clone().unwrap();
        let connection_id = conn.peer_id();

        // There is no TCP control stream on TPROXY. Check the actual lifetimes
        // of both inbound handles and both QUIC destination maps instead.
        loop {
            let released = {
                let entries = observer.udp_scoreboard.lock().await;
                assert_eq!(entries.len(), 1, "one association per source socket");
                entries
                    .iter()
                    .all(|(send, recv)| send.strong_count() == 0 && recv.strong_count() == 0)
            };
            if released
                && conn.send_id_store.inner.read().await.is_empty()
                && conn.recv_id_store.inner.read().await.is_empty()
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(observer.get_user_stats("").await.udp_conns, 0);
        assert!(conn.close_reason().is_none());

        // Reuse the exact source socket: the listener must discard its closed
        // channel and create a new association, without the 300s idle timeout.
        echo(&source, healthy, &target).await;
        assert_eq!(observer.udp_scoreboard.lock().await.len(), 2);
        assert_eq!(observer.get_user_stats("").await.udp_conns, 1);
        assert_eq!(
            client.quic_conn.lock().await.as_ref().unwrap().peer_id(),
            connection_id
        );
        assert!(conn.close_reason().is_none());
        client_task.abort();
        server_task.abort();
    })
    .await
    .expect("TPROXY must release both UDP halves and recreate a session for the same source");
}

async fn echo(source: &UdpSocket, destination: SocketAddr, target: &UdpSocket) {
    source.send_to(b"ping", destination).await.unwrap();
    let mut buf = [0; 64];
    let (len, peer) = target.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..len], b"ping");
    target.send_to(b"pong", peer).await.unwrap();
    let (len, peer) = source.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..len], b"pong");
    assert_eq!(
        peer, destination,
        "raw reply must preserve the fake source IP"
    );
}
