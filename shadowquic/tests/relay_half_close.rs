//! The relay gives a session up once one direction has finished and the surviving
//! one has then stayed silent for `half_close_timeout`.
//!
//! The local side is deliberately left open and silent after the upstream
//! half-closes, so the only thing that can end the session is the watchdog: a
//! client that goes away would end it through the ordinary path instead.
//!
//! The bound has to be applied to both halves of the relay. quinn returns a
//! bi-stream's credit to the peer only once the application has dropped *both* of
//! the stream's halves, and a peer's RESET_STREAM or STOP_SENDING does not free
//! it, so a bound on the local half alone ends the session while the credit stays
//! consumed. `the_relay_bound_returns_the_stream_credit` covers that; the two
//! tests above it isolate the local watchdog.

// The inbound is a MixedServer, which serves SOCKS and HTTP CONNECT on one port.
// The credit test needs the HTTP side of it (see `one_session`), so the file
// needs the `mixed` feature; without it the module is not built at all.
#![cfg(feature = "mixed")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use shadowquic::config::{
    AuthUser, CongestionControl, DirectOutCfg, JlsUpstream, MixedServerCfg, ShadowQuicClientCfg,
    ShadowQuicServerCfg, default_initial_mtu,
};
use shadowquic::{
    Manager,
    direct::outbound::DirectOut,
    mixed::inbound::MixedServer,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

const REQUEST: &[u8] = b"GET / HTTP/1.0\r\n\r\n";
const ANSWER: &[u8] = b"ok";

/// A started proxy pair. The upstream listener is handed back unaccepted, so each
/// test can put its own peer behind it.
struct Proxy {
    proxy_addr: SocketAddr,
    upstream_listener: TcpListener,
    /// Kept alive so the port the server forwards the client's Initial to stays
    /// bound; a black hole is all the handshake needs.
    _jls_upstream: UdpSocket,
}

impl Proxy {
    fn upstream_addr(&self) -> SocketAddr {
        self.upstream_listener.local_addr().unwrap()
    }
}

/// Starts a shadowquic server and a client in front of `upstream_listener`.
///
/// `client_grace` is the relay bound on the client and `server_grace` the one on
/// the server; 0 disables the watchdog on that side. The inbound is a
/// `MixedServer`, which serves SOCKS and HTTP CONNECT on one port, so a test can
/// use whichever handshake suits it.
async fn proxy_pair(client_grace: u64, server_grace: u64) -> Proxy {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

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
        Manager::single(
            Box::new(sq_server),
            Arc::new(DirectOut::new(DirectOutCfg {
                half_close_timeout: server_grace,
                ..Default::default()
            })),
        )
        .run(),
    );

    let proxy_addr = unused_tcp_addr();
    let inbound = MixedServer::new(MixedServerCfg {
        tag: "test-mixed".into(),
        bind_addr: proxy_addr,
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
        half_close_timeout: client_grace,
        ..Default::default()
    });
    tokio::spawn(Manager::single(Box::new(inbound), Arc::new(sq_client)).run());

    Proxy {
        proxy_addr,
        upstream_listener,
        _jls_upstream: jls_upstream,
    }
}

/// Reads one request, closes its write half, then keeps the read half open and
/// stays silent. Reaching EOF here means the relay was given up.
async fn half_closing_peer(listener: TcpListener, ended: mpsc::Sender<()>) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = [0u8; 128];
    let _ = stream.read(&mut buf).await.unwrap();
    stream.shutdown().await.unwrap();
    let _ = stream.read(&mut buf).await.unwrap();
    let _ = ended.send(()).await;
}

/// Reads one request per connection, answers it, and then neither closes nor
/// speaks again: the peer that makes a relay wait forever.
///
/// `accepted` counts the connections taken, which is how many sessions the server
/// actually dispatched — the number a stalled connection stops growing.
async fn hanging_peer(listener: TcpListener, accepted: Arc<AtomicUsize>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        accepted.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let mut buf = [0u8; 128];
            let _ = stream.read(&mut buf).await.unwrap();
            let _ = stream.write_all(ANSWER).await;
            std::future::pending::<()>().await;
        });
    }
}

/// Speaks the SOCKS5 no-auth handshake and a CONNECT to `dst`.
async fn socks_connect(proxy_addr: SocketAddr, dst: SocketAddr) -> TcpStream {
    let mut stream = connect_retrying(proxy_addr).await;

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

/// Speaks an HTTP CONNECT through the proxy to `dst`.
async fn http_connect(proxy_addr: SocketAddr, dst: SocketAddr) -> TcpStream {
    let mut stream = connect_retrying(proxy_addr).await;

    let request = format!("CONNECT {dst} HTTP/1.1\r\nHost: {dst}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "the proxy must accept the connect: {}",
        String::from_utf8_lossy(&head)
    );
    stream
}

/// The inbound listener is bound by a spawned task, so retry briefly instead of
/// guessing how long it takes to come up.
async fn connect_retrying(addr: SocketAddr) -> TcpStream {
    for _ in 0..200 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the inbound listener at {addr} never came up");
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
    // The server-side bound is off here: this test isolates the client-side
    // watchdog, and the server's relay ends anyway once the client's halves are
    // reset.
    let proxy = proxy_pair(GRACE, 0).await;
    let upstream_addr = proxy.upstream_addr();
    let (ended_tx, mut ended) = mpsc::channel(1);
    tokio::spawn(half_closing_peer(proxy.upstream_listener, ended_tx));

    let mut local = socks_connect(proxy.proxy_addr, upstream_addr).await;
    local.write_all(REQUEST).await.unwrap();

    // The upstream's half-close is relayed, so the response direction ends while
    // the request direction stays open and silent.
    let mut buf = [0u8; 16];
    assert_eq!(
        local.read(&mut buf).await.unwrap(),
        0,
        "the upstream's half-close must be relayed to the local side"
    );

    let gave_up = tokio::time::timeout(Duration::from_secs(GRACE + 5), ended.recv()).await;
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
    let proxy = proxy_pair(0, 0).await;
    let upstream_addr = proxy.upstream_addr();
    let (ended_tx, mut ended) = mpsc::channel(1);
    tokio::spawn(half_closing_peer(proxy.upstream_listener, ended_tx));

    let mut local = socks_connect(proxy.proxy_addr, upstream_addr).await;
    local.write_all(REQUEST).await.unwrap();

    let mut buf = [0u8; 16];
    assert_eq!(local.read(&mut buf).await.unwrap(), 0);

    let gave_up = tokio::time::timeout(Duration::from_secs(WATCH_FOR), ended.recv()).await;
    assert!(
        gave_up.is_err(),
        "with half_close_timeout = 0 nothing may end the session, but something did"
    );
}

/// The bound must return the session's stream credit, not merely end the local
/// half of the session.
///
/// The end holding the other half is the server's relay, which is the one waiting
/// on the real peer: a peer that answers and then never closes keeps it alive, so
/// a bound on the client alone ends the client's session while the credit stays
/// consumed. The peer advertises `max_concurrent_bidi_streams` = 1000, so a
/// connection that leaks one credit per session stops dispatching after 1000 of
/// them.
///
/// Sessions here are sequential, and each is answered and closed before the next
/// one starts, so nothing is concurrent and no session may be left unserved.
#[tokio::test]
async fn the_relay_bound_returns_the_stream_credit() {
    const GRACE: u64 = 1;
    /// Comfortably past the 1000-credit limit, so a leak of one credit per
    /// session would stall the connection well before the end.
    const SESSIONS: usize = 1200;
    /// Sessions are opened in batches separated by more than the grace, so the
    /// number open at once stays at the batch size rather than growing with
    /// however fast the machine happens to drive the loop.
    const BATCH: usize = 300;
    const SESSION_BUDGET: Duration = Duration::from_secs(5);

    let accepted = Arc::new(AtomicUsize::new(0));
    let proxy = proxy_pair(GRACE, GRACE).await;
    let upstream_addr = proxy.upstream_addr();
    tokio::spawn(hanging_peer(proxy.upstream_listener, accepted.clone()));

    for i in 0..SESSIONS {
        if let Err(e) = one_session(proxy.proxy_addr, upstream_addr, SESSION_BUDGET).await {
            panic!(
                "session {i} of {SESSIONS} was not served: {e}. The peer had taken {} \
                 connections; a relay that never returns its stream credit stops dispatching \
                 once it has leaked the peer's 1000-stream limit.",
                accepted.load(Ordering::SeqCst)
            );
        }
        if (i + 1) % BATCH == 0 {
            // Let every watchdog in this batch fire before opening the next one.
            tokio::time::sleep(Duration::from_millis(GRACE * 1000 + 300)).await;
        }
    }
}

/// One proxied request, start to finish, under a single deadline.
///
/// The budget covers the whole session, not just the answer: the CONNECT
/// handshake is written by the inbound's own accept loop and the answer only
/// comes back once the request has been dispatched, so they are separate places
/// to wait, and a stall in either one has to report which session went unserved
/// rather than hang the test.
///
/// HTTP CONNECT rather than SOCKS because this test drives over a thousand
/// sessions: the SOCKS inbound writes its CONNECT reply field by field, which
/// Nagle plus the peer's delayed ACK turns into roughly 40ms per session.
async fn one_session(
    proxy_addr: SocketAddr,
    dst: SocketAddr,
    budget: Duration,
) -> Result<(), String> {
    tokio::time::timeout(budget, one_session_inner(proxy_addr, dst))
        .await
        .map_err(|_| format!("not served within {budget:?}"))?
}

async fn one_session_inner(proxy_addr: SocketAddr, dst: SocketAddr) -> Result<(), String> {
    let mut stream = http_connect(proxy_addr, dst).await;
    stream
        .write_all(REQUEST)
        .await
        .map_err(|e| format!("request write failed: {e}"))?;

    let mut answer = [0u8; ANSWER.len()];
    stream
        .read_exact(&mut answer)
        .await
        .map_err(|e| format!("answer read failed: {e}"))?;
    assert_eq!(&answer, ANSWER, "unexpected answer body");

    // Dropping closes the client socket: the half-close the relay waits on.
    drop(stream);
    Ok(())
}
