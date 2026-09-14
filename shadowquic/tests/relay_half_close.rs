//! The relay gives a session up once one direction has finished and the surviving
//! one has then stayed silent for `half_close_timeout`.
//!
//! The local side is deliberately left open and silent after the upstream
//! half-closes, so the only thing that can end the session is the watchdog: a
//! client that goes away would end it through the ordinary path instead.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use shadowquic::config::{
    AuthUser, CongestionControl, JlsUpstream, ShadowQuicClientCfg, ShadowQuicServerCfg,
    SocksServerCfg, default_initial_mtu,
};
use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::inbound::SocksServer,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

struct Started {
    socks_addr: SocketAddr,
    upstream_addr: SocketAddr,
    /// Fires when the upstream sees its connection closed, which only happens once
    /// something has given the relay up.
    ended: mpsc::Receiver<()>,
    /// Kept alive so the port the server forwards the client's Initial to stays
    /// bound; a black hole is all the handshake needs.
    _jls_upstream: UdpSocket,
}

/// Starts a proxy pair in front of a peer that reads one request, closes only its
/// write half and then never speaks again.
async fn start(half_close_timeout: u64) -> Started {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    let jls_upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = unused_udp_addr();

    let sq_server = ShadowQuicServer::new(ShadowQuicServerCfg {
        bind_addr: server_addr,
        users: vec![AuthUser {
            username: "user".into(),
            password: "password".into(),
        }],
        jls_upstream: JlsUpstream {
            addr: jls_upstream.local_addr().unwrap().to_string(),
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
    tokio::spawn(
        Manager::single(Box::new(sq_server), Arc::new(DirectOut::default())).run(),
    );

    let socks_addr = unused_tcp_addr();
    let socks_server = SocksServer::new(SocksServerCfg {
        tag: "test-socks".into(),
        bind_addr: socks_addr,
        users: vec![],
    })
    .await
    .unwrap();
    let sq_client = ShadowQuicClient::new(ShadowQuicClientCfg {
        addr: server_addr.to_string(),
        username: "user".into(),
        password: "password".into(),
        server_name: "localhost".into(),
        alpn: vec!["h3".into()],
        zero_rtt: false,
        initial_mtu: 1200,
        congestion_control: CongestionControl::Bbr,
        keep_alive_interval: 0,
        half_close_timeout,
        ..Default::default()
    });
    tokio::spawn(Manager::single(Box::new(socks_server), Arc::new(sq_client)).run());

    let (ended_tx, ended) = mpsc::channel(1);
    tokio::spawn(peer(upstream_listener, ended_tx));

    Started {
        socks_addr,
        upstream_addr,
        ended,
        _jls_upstream: jls_upstream,
    }
}

/// Reads one request, closes its write half, then keeps the read half open and
/// stays silent. Reaching EOF here means the relay was given up.
async fn peer(listener: TcpListener, ended: mpsc::Sender<()>) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = [0u8; 128];
    let _ = stream.read(&mut buf).await.unwrap();
    stream.shutdown().await.unwrap();
    let _ = stream.read(&mut buf).await.unwrap();
    let _ = ended.send(()).await;
}

/// Speaks the SOCKS5 no-auth handshake and a CONNECT to `dst`.
async fn socks_connect(socks_addr: SocketAddr, dst: SocketAddr) -> TcpStream {
    let mut stream = connect_retrying(socks_addr).await;

    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        reply,
        [0x05, 0x00],
        "the proxy must select the no-auth method"
    );

    let std::net::IpAddr::V4(ip) = dst.ip() else {
        panic!("this test only uses IPv4 destinations");
    };
    let mut connect = vec![0x05, 0x01, 0x00, 0x01];
    connect.extend_from_slice(&ip.octets());
    connect.extend_from_slice(&dst.port().to_be_bytes());
    stream.write_all(&connect).await.unwrap();

    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        reply[..2],
        [0x05, 0x00],
        "the proxy must accept the connect"
    );
    stream
}

/// The inbound listeners are bound by a spawned task, so retry briefly instead of
/// guessing how long it takes to come up.
async fn connect_retrying(addr: SocketAddr) -> TcpStream {
    for _ in 0..200 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the socks listener at {addr} never came up");
}

fn unused_udp_addr() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn unused_tcp_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[tokio::test]
async fn the_relay_gives_up_a_half_closed_session_that_stays_silent() {
    const GRACE: u64 = 1;
    let mut started = start(GRACE).await;

    let mut local = socks_connect(started.socks_addr, started.upstream_addr).await;
    local.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();

    // The upstream's half-close is relayed, so the response direction ends while
    // the request direction stays open and silent.
    let mut buf = [0u8; 16];
    assert_eq!(
        local.read(&mut buf).await.unwrap(),
        0,
        "the upstream's half-close must be relayed to the local side"
    );

    let gave_up = tokio::time::timeout(Duration::from_secs(GRACE + 5), started.ended.recv()).await;
    assert!(
        gave_up.is_ok(),
        "the relay did not give the session up within {}s of the half-close",
        GRACE + 5
    );
}

#[tokio::test]
async fn a_disabled_timeout_leaves_a_half_closed_session_alone() {
    // Three times the grace of the test above, so anything that ends the session
    // for a reason other than the watchdog is caught here.
    const WATCH_FOR: u64 = 3;
    let mut started = start(0).await;

    let mut local = socks_connect(started.socks_addr, started.upstream_addr).await;
    local.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();

    let mut buf = [0u8; 16];
    assert_eq!(local.read(&mut buf).await.unwrap(), 0);

    let gave_up = tokio::time::timeout(Duration::from_secs(WATCH_FOR), started.ended.recv()).await;
    assert!(
        gave_up.is_err(),
        "with half_close_timeout = 0 nothing may end the session, but something did"
    );
}
