//! An inbound's accepted socket must not have Nagle enabled.
//!
//! The SOCKS5 replies are written field by field (`src/msgs/socks5.rs`), so with
//! Nagle on, every field after the first waits for the peer to acknowledge the
//! previous one. A peer that delays that acknowledgement — Linux does, by up to
//! 40ms — makes every SOCKS connection pay it before it can send anything. The
//! HTTP CONNECT reply is written in a single call (`src/http/inbound.rs`) and is
//! not affected, which is what the comparison below shows.
//!
//! These tests drive the inbound directly and need no outbound: the handshake is
//! answered by the inbound's own accept loop before the request is dispatched, so
//! it completes whatever the outbound is doing.
//!
//! The assertion is a latency bound rather than a socket-option check because the
//! option is not observable from outside the process. What matters is that the
//! handshake is not delayed, and the two are orders of magnitude apart.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use shadowquic::Inbound;
use shadowquic::config::SocksServerCfg;
use shadowquic::socks::inbound::SocksServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Enough handshakes that one scheduling hiccup cannot move the median.
const CONNECTIONS: usize = 15;
/// Comfortably above the ~0.4ms a loopback handshake takes, and far below the
/// ~41ms a delayed acknowledgement adds, so the bound separates them without
/// being sensitive to a loaded machine.
const BOUND: Duration = Duration::from_millis(15);

/// The bound applies to the median, so a single slow sample cannot fail a run.
fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
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
    panic!("the inbound listener at {addr} never came up");
}

/// Drives one full SOCKS5 no-auth handshake and returns how long the negotiation
/// took, excluding the TCP connect.
async fn socks_handshake(addr: SocketAddr, dst: SocketAddr) -> Duration {
    let mut stream = connect_retrying(addr).await;
    let started = Instant::now();

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

    started.elapsed()
}

/// Drives one HTTP CONNECT handshake and returns how long it took, excluding the
/// TCP connect. Its reply is a single write, so it is the control.
#[cfg(feature = "mixed")]
async fn http_handshake(addr: SocketAddr, dst: SocketAddr) -> Duration {
    let mut stream = connect_retrying(addr).await;
    let started = Instant::now();

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

    started.elapsed()
}

fn unused_tcp_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A destination for the CONNECT request. Nothing dials it — no outbound is
/// attached — so any address will do.
fn some_dst() -> SocketAddr {
    "127.0.0.1:9".parse().unwrap()
}

#[tokio::test]
async fn the_socks_inbound_handshake_is_not_delayed() {
    let addr = unused_tcp_addr();
    // Held for the whole test: dropping it closes the request channel, and the
    // handlers would then log an error after the handshake they are here to time.
    let inbound = SocksServer::new(SocksServerCfg {
        bind_addr: addr,
        users: vec![],
    })
    .await
    .unwrap();
    inbound.init().await.unwrap();

    let mut samples = Vec::with_capacity(CONNECTIONS);
    for _ in 0..CONNECTIONS {
        samples.push(socks_handshake(addr, some_dst()).await);
    }
    let median = median(&mut samples);

    eprintln!(
        "socks inbound: median {median:?}, max {:?} over {CONNECTIONS} handshakes",
        samples[CONNECTIONS - 1]
    );
    assert!(
        median < BOUND,
        "the SOCKS5 handshake took {median:?} (bound {BOUND:?}); the accepted socket \
         has Nagle enabled, so the field-by-field reply waits for the peer's \
         delayed acknowledgement"
    );
}

/// The comparison: two handshakes through one inbound, one whose reply is written
/// field by field and one written in a single call. Both must be fast; before the
/// accepted socket had Nagle off, the first was about a hundred times the second.
#[cfg(feature = "mixed")]
#[tokio::test]
async fn the_mixed_inbound_handshake_is_not_delayed() {
    use shadowquic::config::MixedServerCfg;
    use shadowquic::mixed::inbound::MixedServer;

    let addr = unused_tcp_addr();
    let inbound = MixedServer::new(MixedServerCfg {
        bind_addr: addr,
        users: vec![],
    })
    .await
    .unwrap();
    inbound.init().await.unwrap();

    let mut socks = Vec::with_capacity(CONNECTIONS);
    let mut http = Vec::with_capacity(CONNECTIONS);
    for _ in 0..CONNECTIONS {
        socks.push(socks_handshake(addr, some_dst()).await);
        http.push(http_handshake(addr, some_dst()).await);
    }
    let socks_median = median(&mut socks);
    let http_median = median(&mut http);

    eprintln!(
        "mixed inbound: socks median {socks_median:?} (max {:?}), http median \
         {http_median:?} (max {:?}) over {CONNECTIONS} handshakes each",
        socks[CONNECTIONS - 1],
        http[CONNECTIONS - 1]
    );
    assert!(
        socks_median < BOUND,
        "the SOCKS5 handshake took {socks_median:?} (bound {BOUND:?}); the accepted \
         socket has Nagle enabled, so the field-by-field reply waits for the peer's \
         delayed acknowledgement"
    );
    assert!(
        http_median < BOUND,
        "the HTTP CONNECT handshake took {http_median:?} (bound {BOUND:?})"
    );
}
