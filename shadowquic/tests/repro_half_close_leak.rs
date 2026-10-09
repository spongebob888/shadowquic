//! Reproduction: a relay session whose peer never closes its half leaks its
//! QUIC bi-stream, so a *sequential* workload wedges the proxy.
//!
//! Mechanism under test (see `document/session-stall-and-leak.md`):
//!
//! * the client opens one bi-stream per proxied session and never closes it
//!   while the relay is alive (`src/squic/outbound.rs:31`);
//! * the relay is `tokio::io::copy_bidirectional`
//!   (`src/squic/outbound.rs:48`), which returns only once **both** directions
//!   report EOF (`tokio/src/io/util/copy_bidirectional.rs:54`);
//! * the peer in [`peer_that_never_closes_wedges_the_proxy`] reads one request,
//!   answers it, and then neither closes nor speaks again. The client, having
//!   read the answer, closes its socket. That ends the client->peer direction,
//!   but the peer->client direction is still waiting for a FIN that never
//!   comes, so the relay never returns, `send`/`recv` are never dropped, and
//!   the bi-stream is never closed;
//! * `open_bi()` waits for the peer to raise MAX_STREAMS instead of failing
//!   (`src/quic/mod.rs:51`), and `Manager` awaits `outbound.handle(req)`
//!   inline (`src/lib.rs:305`), so once the peer's stream limit is reached the
//!   accept loop stops;
//! * the peer advertises `max_concurrent_bidi_streams(1000)`
//!   (`src/shadowquic/quinn_wrapper/wrapper.rs:464`);
//! * the peer only announces MAX_STREAMS once a full 1/8 of the window has
//!   been returned (`quinn-proto-jls-0.3.8/src/connection/streams/state.rs:792`),
//!   so the 1000 initial credits are the whole budget: with the leak below, no
//!   stream ever completes, nothing is returned, and the client waits forever
//!   rather than stalling at a slightly lower number.
//!
//! So the wedge is reached by ~1000 *completed and closed* sessions, not by
//! 1000 concurrent ones. Every session below is answered and closed by the
//! client before the next one starts; nothing is concurrent, and the proxy is
//! expected to keep serving forever.
//!
//! [`control_peer_that_closes_does_not_wedge`] is the same workload against a
//! peer that closes its half after answering. It exists so that "the proxy
//! wedged" cannot be blamed on the session count, the loop, or the harness:
//! the only difference between the two is whether the peer closes.
//!
//! The relay bound is configured to `GRACE` on both ends, so a leaked session is
//! reclaimed inside the test's lifetime: with it the proxy serves all
//! `MAX_SESSIONS`; without it (an unfixed tree, or `GRACE = 0`) the leak
//! accumulates and the proxy wedges at ~1000. Sessions are driven in batches
//! separated by more than `GRACE`, so the number of half-closed-but-not-yet-
//! reclaimed sessions at any instant is bounded by `BATCH` rather than by how
//! fast the machine drives the loop; without that a fast machine could open
//! >1000 sessions inside one grace and wedge for a reason unrelated to the
//! defect.
//!
//! Run:
//!     cargo test -p shadowquic --test repro_half_close_leak -- --nocapture
//!
//! On the unfixed tree the first test fails at ~999 sessions with "not served
//! within 5s"; the control passes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, Instant, timeout};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const REQUEST: &[u8] = b"GET / HTTP/1.0\r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok";

/// The peer's advertised `max_concurrent_bidi_streams`, plus a margin so the
/// wedge is certainly reached even if a stats stream briefly holds a slot.
const MAX_SESSIONS: usize = 1100;
/// Per-session budget. A healthy session on loopback takes ~1 ms.
const SESSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The relay bound on both ends, in seconds. Short enough that a leaked session
/// is reclaimed inside the test's run; `relay_half_close.rs` uses the same 1 s.
/// `0` disables the bound and reproduces the unfixed tree.
const GRACE: u64 = 1;
/// Sessions per batch. A batch is followed by a pause longer than `GRACE`, so at
/// most `BATCH` sessions are half-closed-and-unreclaimed at any instant.
const BATCH: usize = 200;

/// A peer that answers and then keeps the session alive forever. Every
/// completed session leaks exactly one bi-stream, so this wedges at ~1000.
#[tokio::test]
async fn peer_that_never_closes_wedges_the_proxy() {
    init_tracing();
    let ports = Ports::new(11100);
    let accepted = spawn_stack(&ports, Peer::NeverCloses).await;

    if let Err((i, e)) = drive_sessions(&ports, accepted.clone()).await {
        // The incident's socket dump was taken *after* the stall, and showed
        // `Recv-Q 95 on essentially all` of its CLOSE_WAIT sockets. Those are
        // clients that arrived after the accept loop stopped: the inbound's
        // accept loop answered their handshake, their request was never read,
        // and their handler is blocked in `sender.send(req)`. Park a batch of
        // them so the two signatures can be compared directly.
        park_after_stall(&ports, 120).await;
        report_tcp_states(&ports);
        panic!(
            "proxy stopped serving after {i} sequential, fully completed sessions \
             (upstream accepted {} connections). Every client socket was closed, so no \
             session should still be holding stream credit; the peer advertises \
             max_concurrent_bidi_streams = 1000. Failure: {e}",
            accepted.load(Ordering::SeqCst),
        );
    }
}

/// Control: the same sequential workload, against a peer that closes its half
/// once it has answered. The relay then sees EOF on both directions and
/// returns, so no credit is retained and the proxy serves all sessions.
#[tokio::test]
async fn control_peer_that_closes_does_not_wedge() {
    init_tracing();
    let ports = Ports::new(11200);
    let accepted = spawn_stack(&ports, Peer::ClosesAfterAnswer).await;

    if let Err((i, e)) = drive_sessions(&ports, accepted.clone()).await {
        panic!(
            "control run wedged after {i} sessions (upstream accepted {} connections); \
             the peer closed its half, so the relay should have ended every session. \
             Failure: {e}",
            accepted.load(Ordering::SeqCst),
        );
    }
}

struct Ports {
    proxy: u16,
    server: u16,
    upstream: u16,
}

impl Ports {
    fn new(base: u16) -> Self {
        Self {
            proxy: base,
            server: base + 1,
            upstream: base + 2,
        }
    }
}

#[derive(Clone, Copy)]
enum Peer {
    NeverCloses,
    ClosesAfterAnswer,
}

/// The test drives ~1100 sessions whose peer never closes, so it holds a socket
/// per session for the whole run and needs far more than the common 1024 soft
/// `ulimit -n`. Raise the soft limit to the hard limit so the test measures the
/// leak and not the shell's limit; a no-op where the hard limit is already
/// reached, or on a platform without `setrlimit`.
fn raise_fd_limit() {
    #[cfg(unix)]
    // SAFETY: `getrlimit`/`setrlimit` only read and write this process's own
    // resource limits, and the pointer is to a local.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < limit.rlim_max
        {
            limit.rlim_cur = limit.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

/// Brings up the peer, the shadowquic server and the proxy, and returns the
/// counter of connections the peer has accepted.
async fn spawn_stack(ports: &Ports, peer: Peer) -> Arc<AtomicUsize> {
    raise_fd_limit();
    let accepted = Arc::new(AtomicUsize::new(0));
    spawn_peer(ports.upstream, accepted.clone(), peer).await;
    spawn_shadowquic_server(ports.server).await;
    spawn_proxy(ports).await;
    accepted
}

/// Sessions are strictly sequential: each one is answered and its client socket
/// closed before the next is opened. Nothing is left running on the client
/// side, so the proxy has no reason to hold anything. Every `BATCH` sessions the
/// loop pauses longer than `GRACE`, so the bound reclaims the batch before the
/// next one starts and the live leak count cannot grow with machine speed.
/// Returns `Err((index, reason))` at the first session the proxy fails to answer.
async fn drive_sessions(
    ports: &Ports,
    accepted: Arc<AtomicUsize>,
) -> Result<usize, (usize, String)> {
    let started = Instant::now();
    for i in 0..MAX_SESSIONS {
        if let Err(e) = one_session(ports.proxy, ports.upstream).await {
            eprintln!(
                "--- wedged at session {i} after {:?}: {e}",
                started.elapsed()
            );
            report_tcp_states(ports);
            return Err((i, e));
        }
        if (i + 1) % BATCH == 0 {
            // Let the bound reclaim this batch before opening the next one.
            tokio::time::sleep(Duration::from_secs(GRACE) + Duration::from_millis(300)).await;
        }
        if i % 250 == 0 {
            eprintln!(
                "--- {i} sessions done, {} upstream connections accepted, {:?} elapsed",
                accepted.load(Ordering::SeqCst),
                started.elapsed()
            );
        }
    }
    eprintln!(
        "--- all {MAX_SESSIONS} sessions served in {:?}, {} upstream connections accepted",
        started.elapsed(),
        accepted.load(Ordering::SeqCst)
    );
    Ok(MAX_SESSIONS)
}

/// Counts the process's TCP sockets by side and state for the ports this test
/// uses, the way the incident's `netstat -anp` dump was read. The incident
/// counted sockets whose *local* port was the affected listener's port, so the
/// `local=proxy(...)` lines are the comparable ones.
///
/// This test runs both ends in one process, so every session shows up on four
/// sockets: the proxy's accepted socket (`local=proxy`), the test's own client
/// socket (`remote=proxy`), the server's outbound socket to the peer
/// (`remote=upstream`) and the peer's accepted socket (`local=upstream`).
fn report_tcp_states(ports: &Ports) {
    let named = [
        ("proxy", ports.proxy),
        ("upstream", ports.upstream),
        ("server", ports.server),
    ];
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 {
                continue;
            }
            let port_of = |field: &str| {
                field
                    .rsplit_once(':')
                    .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
            };
            let state = tcp_state_name(u16::from_str_radix(f[3], 16).unwrap_or(0));
            for (name, port) in named {
                for (side, field) in [("local", f[1]), ("remote", f[2])] {
                    if port_of(field) == Some(port) {
                        let key = (format!("{side}={name}({port})"), state);
                        *counts.entry(key).or_default() += 1;
                    }
                }
            }
        }
    }
    eprintln!("--- TCP sockets by side and state");
    for ((side, state), n) in counts {
        eprintln!("---   {side:<22} {state:<12} {n}");
    }

    // The incident's key detail was `Recv-Q 95 on essentially all of them`:
    // every stuck socket still held the payload the client sent after the
    // handshake. That only happens for connections that stalled *before* the
    // relay read them, so the distribution says whether a CLOSE_WAIT pool is a
    // leaked-relay pool or a post-stall backlog.
    let mut unread: BTreeMap<u64, usize> = BTreeMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 || u16::from_str_radix(f[3], 16) != Ok(0x08) {
                continue;
            }
            let port_of = |field: &str| {
                field
                    .rsplit_once(':')
                    .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
            };
            if port_of(f[1]) != Some(ports.proxy) {
                continue;
            }
            let rx = f[4]
                .rsplit_once(':')
                .and_then(|(_, q)| u64::from_str_radix(q, 16).ok())
                .unwrap_or(0);
            *unread.entry(rx).or_default() += 1;
        }
    }
    eprintln!("--- CLOSE_WAIT on the proxy listener, by unread bytes (Recv-Q)");
    for (bytes, n) in unread {
        eprintln!("---   Recv-Q {bytes:<6} {n}");
    }
}

fn tcp_state_name(state: u16) -> &'static str {
    match state {
        0x01 => "ESTABLISHED",
        0x02 => "SYN_SENT",
        0x03 => "SYN_RECV",
        0x04 => "FIN_WAIT1",
        0x05 => "FIN_WAIT2",
        0x06 => "TIME_WAIT",
        0x07 => "CLOSE",
        0x08 => "CLOSE_WAIT",
        0x09 => "LAST_ACK",
        0x0a => "LISTEN",
        0x0b => "CLOSING",
        _ => "UNKNOWN",
    }
}

/// Connections that arrive *after* the stall, the way the incident's clients
/// did: the handshake is answered by the inbound's accept loop, the request is
/// sent, and nothing ever reads it.
async fn park_after_stall(ports: &Ports, count: usize) {
    for _ in 0..count {
        let Ok(mut sock) = open_tunnel(ports.proxy, ports.upstream).await else {
            continue;
        };
        let _ = sock.write_all(REQUEST).await;
        drop(sock);
    }
}

/// Performs the HTTP CONNECT handshake through the proxy and hands back the
/// tunnel socket. The proxy answers CONNECT with a single write
/// (src/http/inbound.rs:70), so the handshake has no Nagle/delayed-ACK stall;
/// the SOCKS inbound writes its reply field by field (src/msgs/socks5.rs) and
/// costs an extra ~40 ms per session.
async fn open_tunnel(proxy_port: u16, upstream_port: u16) -> Result<TcpStream, String> {
    let mut sock = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .map_err(|e| format!("tcp connect failed: {e}"))?;

    let connect = format!(
        "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1:{upstream_port}\r\n\r\n"
    );
    sock.write_all(connect.as_bytes())
        .await
        .map_err(|e| format!("connect write failed: {e}"))?;

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        timeout(SESSION_TIMEOUT, sock.read_exact(&mut byte))
            .await
            .map_err(|_| "no connect reply within the session timeout".to_string())?
            .map_err(|e| format!("connect reply read failed: {e}"))?;
        head.push(byte[0]);
        if head.len() > 1024 {
            return Err("connect reply never terminated".to_string());
        }
    }
    Ok(sock)
}

/// One proxied request, start to finish: CONNECT through the proxy, send a
/// request, read the whole answer, close the client socket. Returns `Err` once
/// the proxy stops answering.
async fn one_session(proxy_port: u16, upstream_port: u16) -> Result<(), String> {
    let mut sock = open_tunnel(proxy_port, upstream_port).await?;

    sock.write_all(REQUEST)
        .await
        .map_err(|e| format!("request write failed: {e}"))?;

    // Read exactly the answer, so the socket's receive queue is empty when it
    // is dropped and the close is a clean FIN rather than a RST.
    let mut buf = vec![0u8; RESPONSE.len()];
    timeout(SESSION_TIMEOUT, sock.read_exact(&mut buf))
        .await
        .map_err(|_| "no answer within the session timeout".to_string())?
        .map_err(|e| format!("answer read failed: {e}"))?;
    assert_eq!(buf, RESPONSE, "unexpected answer body");

    // Dropping closes the client socket. This is the half the relay never
    // notices, because it is still waiting for the *peer's* half.
    drop(sock);
    Ok(())
}

/// The upstream peer. Reads one request, answers it, and then either closes its
/// half or holds it open forever.
async fn spawn_peer(port: u16, accepted: Arc<AtomicUsize>, peer: Peer) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                if stream.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let _ = stream.write_all(RESPONSE).await;
                let _ = stream.flush().await;
                match peer {
                    Peer::ClosesAfterAnswer => {
                        let _ = stream.shutdown().await;
                    }
                    Peer::NeverCloses => {
                        // Never read, never close: the relay's peer->client
                        // direction waits on this forever.
                        std::future::pending::<()>().await;
                    }
                }
            });
        }
    });
}

async fn spawn_shadowquic_server(server_port: u16) {
    let server = ShadowQuicServer::new(ShadowQuicServerCfg {
        tag: "inbound".into(),
        bind_addr: format!("[::]:{server_port}").parse().unwrap(),
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

    tokio::spawn(
        Manager::single(
            Box::new(server),
            Arc::new(DirectOut::new(
                DirectOutCfg {
                    half_close_timeout: GRACE,
                    ..Default::default()
                },
                Arc::new(shadowquic::dns::ResolverManager::new()),
            )),
        )
        .run(),
    );
    // The server listens on UDP, so there is nothing to poll for; give it a
    // moment to bind.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn spawn_proxy(ports: &Ports) {
    let inbound = MixedServer::new(MixedServerCfg {
        tag: "inbound".into(),
        default_outbound: None,
        bind_addr: format!("127.0.0.1:{}", ports.proxy).parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let client = ShadowQuicClient::new(
        ShadowQuicClientCfg {
            password: "123".into(),
            username: "123".into(),
            addr: format!("127.0.0.1:{}", ports.server).parse().unwrap(),
            server_name: "localhost".into(),
            alpn: vec!["h3".into()],
            initial_mtu: 1200,
            congestion_control: CongestionControl::Bbr,
            zero_rtt: true,
            over_stream: true,
            // The incident ran with keep-alive on, so the 30 s idle timer can never
            // end the stall on its own.
            keep_alive_interval: 5000,
            // Short, so the bound reclaims a leaked session inside the test's run.
            half_close_timeout: GRACE,
            ..Default::default()
        },
        Arc::new(shadowquic::dns::ResolverManager::new()),
    );

    tokio::spawn(Manager::single(Box::new(inbound), Arc::new(client)).run());
    wait_until_serving(ports).await;
}

/// Drives one real request until the proxy answers, so the test never races the
/// QUIC handshake or the listener bind.
async fn wait_until_serving(ports: &Ports) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while one_session(ports.proxy, ports.upstream).await.is_err() {
        assert!(Instant::now() < deadline, "proxy never started serving");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("shadowquic=warn"));
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .try_init();
}
