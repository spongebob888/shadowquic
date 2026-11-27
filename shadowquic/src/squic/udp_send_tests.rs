//! One datagram that cannot be sent must not end the association it is in.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::{
    Manager, Outbound, ProxyRequest, UdpSession,
    config::{AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg},
    direct::outbound::DirectOut,
    dns::ResolverManager,
    msgs::socks5::SocksAddr,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};

type Packet = (Bytes, SocksAddr);

async fn spawn_udp_echo() -> SocketAddr {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = socket.recv_from(&mut buf).await {
            let _ = socket.send_to(&buf[..n], peer).await;
        }
    });
    addr
}

async fn association_survives_an_unsendable_destination(over_stream: bool) {
    let server_addr = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "pass".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: "127.0.0.1:9".into(),
            ..Default::default()
        },
        alpn: vec!["h3".into()],
        congestion_control: CongestionControl::Bbr,
        ..Default::default()
    })
    .await
    .unwrap();
    tokio::spawn(Manager::single(Box::new(server), Arc::new(DirectOut::default())).run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            tag: "outbound".into(),
            username: "user".into(),
            password: "pass".into(),
            addr: server_addr.to_string(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            over_stream,
            congestion_control: CongestionControl::Bbr,
            ..Default::default()
        },
        Arc::new(ResolverManager::new()),
    );
    let (to_proxy, from_app) = mpsc::channel::<Packet>(4);
    let (to_app, mut from_proxy) = mpsc::channel::<Packet>(4);
    let unspecified: SocksAddr = "0.0.0.0:0".parse().unwrap();
    let session = UdpSession {
        recv: Box::new(from_app) as crate::AnyUdpRecv,
        send: Arc::new(to_app) as crate::AnyUdpSend,
        stream: None,
        bind_addr: unspecified.clone(),
        dst: unspecified,
        src_addr: None,
        user_context: Default::default(),
    };
    tokio::spawn(async move { client.handle(ProxyRequest::Udp(session)).await });

    let echo = spawn_udp_echo().await;
    // A name longer than the 255 bytes its length prefix can state.
    let unsendable = SocksAddr::from_domain("a".repeat(300), 53);
    to_proxy
        .send((Bytes::from_static(b"lost"), unsendable))
        .await
        .unwrap();
    to_proxy
        .send((Bytes::from_static(b"ping"), echo.into()))
        .await
        .unwrap();
    let (reply, _) = tokio::time::timeout(Duration::from_secs(5), from_proxy.recv())
        .await
        .expect("the association stopped relaying")
        .expect("the association ended");
    assert_eq!(&reply[..], b"ping");
}

#[tokio::test]
async fn datagram_association_survives_an_unsendable_destination() {
    association_survives_an_unsendable_destination(false).await;
}

#[tokio::test]
async fn stream_association_survives_an_unsendable_destination() {
    association_survives_an_unsendable_destination(true).await;
}
