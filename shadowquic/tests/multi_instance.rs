use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fast_socks5::client::Config as SocksClientConfig;
use fast_socks5::client::Socks5Stream;
use shadowquic::config::{
    Config, DirectOutCfg, InboundCfg, InstanceCfg, LogLevel, OutboundCfg, ShadowQuicServerCfg,
    SocksServerCfg,
};
use shadowquic::direct::outbound::DirectOut;
use shadowquic::error::SError;
use shadowquic::socks::inbound::SocksServer;
use shadowquic::{Inbound, Instance, Manager, Outbound, ProxyRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::Notify;

/// Spawns a TCP echo server on a random port, returns its port.
async fn spawn_echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

fn socks_client_config() -> SocksClientConfig {
    let mut config = SocksClientConfig::default();
    config.set_skip_auth(false);
    config
}

/// Connects to `echo_port` through the socks5 inbound on `proxy_port` and
/// asserts the payload round-trips.
async fn assert_roundtrip(proxy_port: u16, echo_port: u16, payload: &[u8]) {
    let mut stream = Socks5Stream::connect(
        format!("127.0.0.1:{proxy_port}"),
        "127.0.0.1".into(),
        echo_port,
        socks_client_config(),
    )
    .await
    .unwrap();

    stream.write_all(payload).await.unwrap();
    let mut buf = vec![0u8; payload.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, payload);
}

#[tokio::test]
async fn test_multi_instance_runtime() {
    let (port_a, port_b) = (21021u16, 21022u16);
    let echo_port = spawn_echo().await;

    let in_a = SocksServer::new(SocksServerCfg {
        bind_addr: format!("127.0.0.1:{port_a}").parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();
    let in_b = SocksServer::new(SocksServerCfg {
        bind_addr: format!("127.0.0.1:{port_b}").parse().unwrap(),
        users: vec![],
    })
    .await
    .unwrap();

    let manager = Manager::with_instances(vec![
        Instance::new(Box::new(in_a), Box::new(DirectOut::default())),
        Instance::new(Box::new(in_b), Box::new(DirectOut::default())),
    ]);
    tokio::spawn(manager.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both inbounds serve concurrently in the same process.
    assert_roundtrip(port_a, echo_port, b"hello from instance a").await;
    assert_roundtrip(port_b, echo_port, b"hello from instance b").await;
    assert_roundtrip(port_a, echo_port, b"still alive a").await;
    assert_roundtrip(port_b, echo_port, b"still alive b").await;
}

#[tokio::test]
async fn test_multi_instance_config() {
    let echo_port = spawn_echo().await;
    let yaml = r###"
instances:
    - inbound:
          type: socks
          bind-addr: "127.0.0.1:21031"
      outbound:
          type: direct
          dns-strategy: prefer-ipv4
    - inbound:
          type: socks
          bind-addr: "127.0.0.1:21032"
      outbound:
          type: direct
log-level: info
"###;
    let cfg: Config = serde_saphyr::from_str(yaml).expect("yaml parsed failed");
    assert_eq!(cfg.instances.len(), 2);
    let manager = cfg.build_manager().await.expect("build manager failed");
    tokio::spawn(manager.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_roundtrip(21031, echo_port, b"cfg instance 1").await;
    assert_roundtrip(21032, echo_port, b"cfg instance 2").await;
}

#[tokio::test]
async fn test_legacy_config_still_works() {
    let echo_port = spawn_echo().await;
    let yaml = r###"
inbound:
    type: socks
    bind-addr: "127.0.0.1:21041"
outbound:
    type: direct
    dns-strategy: prefer-ipv4
log-level: info
"###;
    let cfg: Config = serde_saphyr::from_str(yaml).expect("yaml parsed failed");
    assert_eq!(cfg.instances.len(), 1);
    let manager = cfg.build_manager().await.expect("build manager failed");
    tokio::spawn(manager.run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_roundtrip(21041, echo_port, b"legacy config").await;
}

/// Outbound stub for lifecycle tests: accepts requests and does nothing.
struct NopOutbound;

#[async_trait]
impl Outbound for NopOutbound {
    async fn handle(&mut self, _req: ProxyRequest) -> Result<(), SError> {
        Ok(())
    }
}

/// Inbound that panics on the first `accept`, simulating an instance crash.
struct PanicInbound;

#[async_trait]
impl Inbound for PanicInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        panic!("instance crashed");
    }
}

/// Inbound whose `accept` never returns and which counts `shutdown` calls.
struct PendingInbound {
    shutdown_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Inbound for PendingInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        std::future::pending().await
    }
    async fn shutdown(&self) -> Result<(), SError> {
        self.shutdown_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Inbound whose `init` always fails.
struct FailInitInbound;

#[async_trait]
impl Inbound for FailInitInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        std::future::pending().await
    }
    async fn init(&self) -> Result<(), SError> {
        Err(SError::InboundUnavailable)
    }
}

/// Shutdown behavior of the failing instance, so its own cleanup can be
/// exercised: complete, fail, or hang until released (the manager's cleanup
/// budget then aborts it).
enum FailAcceptShutdown {
    Ok,
    Err,
    Hang {
        /// Notified once `shutdown` has been entered.
        started: Arc<Notify>,
        /// Releases the hung shutdown (never fired when the budget should
        /// expire first).
        release: Arc<Notify>,
    },
}

/// Inbound whose `accept` fails immediately (terminal error semantics) and
/// whose `shutdown` behavior is configurable.
struct FailAcceptInbound {
    shutdown_count: Arc<AtomicUsize>,
    behavior: FailAcceptShutdown,
}

#[async_trait]
impl Inbound for FailAcceptInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        Err(SError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<(), SError> {
        self.shutdown_count.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            FailAcceptShutdown::Ok => Ok(()),
            FailAcceptShutdown::Err => Err(SError::InboundUnavailable),
            FailAcceptShutdown::Hang { started, release } => {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    }
}

/// Inbound that announces entering `shutdown` and then waits on a gate before
/// completing it, so tests can observe shutdowns in flight at a fixed
/// virtual time.
struct GatedShutdownInbound {
    shutdown_count: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl Inbound for GatedShutdownInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        std::future::pending().await
    }
    async fn shutdown(&self) -> Result<(), SError> {
        self.shutdown_count.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.gate.notified().await;
        Ok(())
    }
}

/// Inbound whose `shutdown` fails with a distinct error (a survivor's own
/// cleanup failure). It announces entering `shutdown` and waits on a gate, so
/// tests can release it at a controlled point and pin the exit order.
struct ErrShutdownInbound {
    shutdown_count: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl Inbound for ErrShutdownInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        std::future::pending().await
    }
    async fn shutdown(&self) -> Result<(), SError> {
        self.shutdown_count.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.gate.notified().await;
        Err(SError::SocksError("survivor cleanup failed".into()))
    }
}

/// Inbound whose `shutdown` never returns (e.g. a hung flush).
struct HungShutdownInbound;

#[async_trait]
impl Inbound for HungShutdownInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        std::future::pending().await
    }
    async fn shutdown(&self) -> Result<(), SError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn test_instance_panic_triggers_graceful_shutdown_of_others() {
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let manager = Manager::with_instances(vec![
        Instance::new(Box::new(PanicInbound), Box::new(NopOutbound)),
        Instance::new(
            Box::new(PendingInbound {
                shutdown_count: shutdown_count.clone(),
            }),
            Box::new(NopOutbound),
        ),
    ]);

    // run() must surface the instance failure ...
    let result = tokio::time::timeout(Duration::from_secs(30), manager.run())
        .await
        .expect("run() must return after an instance crash, not hang");
    assert!(result.is_err());

    // ... and the surviving instance must have been shut down gracefully,
    // not silently aborted with the JoinSet.
    assert_eq!(
        shutdown_count.load(Ordering::SeqCst),
        1,
        "surviving instance was not shut down gracefully"
    );
}

#[tokio::test]
async fn test_init_failure_rolls_back_initialized_instances() {
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let manager = Manager::with_instances(vec![
        Instance::new(
            Box::new(PendingInbound {
                shutdown_count: shutdown_count.clone(),
            }),
            Box::new(NopOutbound),
        ),
        Instance::new(Box::new(FailInitInbound), Box::new(NopOutbound)),
    ]);

    let result = tokio::time::timeout(Duration::from_secs(30), manager.run())
        .await
        .expect("run() must return after an init failure, not hang");
    assert!(result.is_err());
    assert_eq!(
        shutdown_count.load(Ordering::SeqCst),
        1,
        "already-initialized instance was not rolled back"
    );
}

#[tokio::test]
async fn test_empty_instances_is_rejected() {
    // Field-literal construction bypasses `with_instances`' debug_assert;
    // run() must still reject an empty manager instead of idling forever.
    let manager = Manager { instances: vec![] };
    let result = manager.run().await;
    assert!(
        matches!(result, Err(SError::Instance(_))),
        "empty manager must be rejected with SError::Instance, got {result:?}"
    );
}

/// A hung `shutdown` during rollback must not stall startup: the rollback is
/// bounded by the drain timeout and the init error still surfaces. The paused
/// tokio clock keeps the DRAIN_TIMEOUT wait instant.
#[tokio::test(start_paused = true)]
async fn test_rollback_shutdown_timeout_does_not_hang_startup() {
    let manager = Manager::with_instances(vec![
        Instance::new(Box::new(HungShutdownInbound), Box::new(NopOutbound)),
        Instance::new(Box::new(FailInitInbound), Box::new(NopOutbound)),
    ]);
    let result = tokio::time::timeout(Duration::from_secs(60), manager.run())
        .await
        .expect("run() must return even when a rollback shutdown hangs");
    match result {
        Err(SError::Instance(msg)) => {
            assert!(
                msg.contains("instance 1 init failed"),
                "init failure must surface, got: {msg}"
            );
        }
        other => panic!("expected instance init failure, got {other:?}"),
    }
}

/// Config-build helper: a shadowquic/direct instance list, so construction
/// failures (store loading, UDP bind) can be exercised without full servers.
fn shadowquic_config(binds: &[(u16, Option<PathBuf>)]) -> Config {
    Config {
        instances: binds
            .iter()
            .map(|&(port, ref store)| InstanceCfg {
                inbound: InboundCfg::ShadowQuic(ShadowQuicServerCfg {
                    bind_addr: format!("127.0.0.1:{port}").parse().unwrap(),
                    user_store: store.clone(),
                    ..Default::default()
                }),
                outbound: OutboundCfg::Direct(DirectOutCfg::default()),
            })
            .collect(),
        log_level: LogLevel::default(),
    }
}

/// `Manager` is not `Debug`, so `unwrap_err` is unavailable; assert failure
/// explicitly and return the error for message checks.
async fn build_manager_err(cfg: Config) -> SError {
    match cfg.build_manager().await {
        Ok(_) => panic!("build_manager must fail for this config"),
        Err(e) => e,
    }
}

/// A construction failure must be reported with the instance index and
/// direction instead of a bare error. A directory as the `user-store` path
/// makes `ShadowQuicServer::new` fail deterministically while loading the
/// store, before any socket is bound.
#[tokio::test]
async fn test_build_failure_error_carries_instance_index() {
    let cfg = shadowquic_config(&[(24478, None), (24479, Some(std::env::temp_dir()))]);
    let err = build_manager_err(cfg).await;
    assert!(
        err.to_string().contains("instance 1 inbound build failed"),
        "got: {err}"
    );
}

/// Two instances sharing one user-store path would overwrite each other's
/// users and traffic stats on every flush (whole-file snapshot, last writer
/// wins). This must be rejected at config-build time, before anything binds.
#[tokio::test]
async fn test_duplicate_user_store_paths_are_rejected() {
    let store = std::env::temp_dir().join(format!(
        "shadowquic-multi-store-{}.yaml",
        std::process::id()
    ));
    let cfg = shadowquic_config(&[
        (24488, Some(store.clone())),
        (24489, Some(store.clone())),
        (24490, Some(store)),
    ]);
    let err = build_manager_err(cfg).await;
    let msg = err.to_string();
    assert!(msg.contains("user-store"), "got: {msg}");
    assert!(msg.contains("instances 0 and 1"), "got: {msg}");
}

/// A UDP bind conflict during construction surfaces as an indexed build
/// error instead of a panic, keeping rollback/exit on the error path.
#[tokio::test]
async fn test_quic_bind_conflict_fails_build_with_index() {
    let _guard = UdpSocket::bind("127.0.0.1:24498").await.unwrap();
    let cfg = shadowquic_config(&[(24498, None)]);
    let err = build_manager_err(cfg).await;
    assert!(
        err.to_string().contains("instance 0 inbound build failed"),
        "got: {err}"
    );
}

/// A terminal `accept` failure must fail fast — not spin on repeated errors —
/// shut the remaining instances down gracefully, AND still run the failing
/// instance's own shutdown (final flush) exactly once before it exits.
#[tokio::test]
async fn test_accept_error_is_terminal_and_shuts_down_survivors() {
    let survivor_count = Arc::new(AtomicUsize::new(0));
    let failing_count = Arc::new(AtomicUsize::new(0));
    let manager = Manager::with_instances(vec![
        Instance::new(
            Box::new(PendingInbound {
                shutdown_count: survivor_count.clone(),
            }),
            Box::new(NopOutbound),
        ),
        Instance::new(
            Box::new(FailAcceptInbound {
                shutdown_count: failing_count.clone(),
                behavior: FailAcceptShutdown::Ok,
            }),
            Box::new(NopOutbound),
        ),
    ]);
    let result = tokio::time::timeout(Duration::from_secs(30), manager.run())
        .await
        .expect("run() must return when an instance accept fails terminally");
    match result {
        Err(SError::Instance(msg)) => {
            assert!(msg.contains("instance 1 accept failed"), "got: {msg}");
        }
        other => panic!("expected instance accept failure, got {other:?}"),
    }
    assert_eq!(
        survivor_count.load(Ordering::SeqCst),
        1,
        "surviving instance was not shut down exactly once"
    );
    assert_eq!(
        failing_count.load(Ordering::SeqCst),
        1,
        "failing instance skipped its own shutdown (final flush)"
    );
}

/// The failure broadcast must reach survivors while the failing instance is
/// still inside its own cleanup — not only after its cleanup budget expires.
/// At virtual time zero the survivor must already be in shutdown; only then
/// may time advance past the budget, and the root accept error must still be
/// the returned cause regardless of task exit order.
#[tokio::test]
async fn test_accept_failure_broadcast_reaches_survivors_during_cleanup() {
    tokio::time::pause();

    let survivor_count = Arc::new(AtomicUsize::new(0));
    let failing_count = Arc::new(AtomicUsize::new(0));
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_release = Arc::new(Notify::new()); // never fired: budget expires first
    let survivor_entered = Arc::new(Notify::new());
    let survivor_gate = Arc::new(Notify::new());

    let manager = Manager::with_instances(vec![
        Instance::new(
            Box::new(GatedShutdownInbound {
                shutdown_count: survivor_count.clone(),
                entered: survivor_entered.clone(),
                gate: survivor_gate.clone(),
            }),
            Box::new(NopOutbound),
        ),
        Instance::new(
            Box::new(FailAcceptInbound {
                shutdown_count: failing_count.clone(),
                behavior: FailAcceptShutdown::Hang {
                    started: cleanup_started.clone(),
                    release: cleanup_release.clone(),
                },
            }),
            Box::new(NopOutbound),
        ),
    ]);
    let run = tokio::spawn(manager.run());
    // Virtual-time anchor: everything below must happen before the first
    // advance() (paused time still auto-advances to the next timer when the
    // runtime goes idle, so elapsing here would mean the broadcast was
    // delayed until after the cleanup budget).
    let start = tokio::time::Instant::now();

    // The failing instance entered its own cleanup (broadcast already sent).
    cleanup_started.notified().await;
    // No virtual time has passed: the survivor must already be in shutdown,
    // well within the failing instance's cleanup budget.
    survivor_entered.notified().await;
    assert_eq!(
        start.elapsed(),
        Duration::ZERO,
        "broadcast must reach the survivor before any virtual time passes"
    );
    assert_eq!(failing_count.load(Ordering::SeqCst), 1);
    assert_eq!(survivor_count.load(Ordering::SeqCst), 1);

    // Let the survivor finish, then let the cleanup budget (5s) expire; the
    // root accept error must still be the result.
    survivor_gate.notify_one();
    tokio::time::advance(Duration::from_secs(6)).await;
    let result = run.await.expect("run task must not panic");
    match result {
        Err(SError::Instance(msg)) => {
            assert!(msg.contains("instance 1 accept failed"), "got: {msg}");
        }
        other => panic!("expected instance accept failure, got {other:?}"),
    }
    assert_eq!(
        survivor_count.load(Ordering::SeqCst),
        1,
        "survivor shutdown must run exactly once"
    );
    assert_eq!(
        failing_count.load(Ordering::SeqCst),
        1,
        "failing instance shutdown must run exactly once"
    );
}

/// A survivor whose own shutdown fails and which exits FIRST must not mask
/// the root accept error: the root cause is recorded before the failing
/// instance starts cleanup, not derived from task exit order.
#[tokio::test]
async fn test_survivor_cleanup_error_does_not_mask_root_accept_failure() {
    let survivor_count = Arc::new(AtomicUsize::new(0));
    let failing_count = Arc::new(AtomicUsize::new(0));
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_release = Arc::new(Notify::new());
    let survivor_entered = Arc::new(Notify::new());
    let survivor_gate = Arc::new(Notify::new());
    let manager = Manager::with_instances(vec![
        Instance::new(
            Box::new(ErrShutdownInbound {
                shutdown_count: survivor_count.clone(),
                entered: survivor_entered.clone(),
                gate: survivor_gate.clone(),
            }),
            Box::new(NopOutbound),
        ),
        Instance::new(
            Box::new(FailAcceptInbound {
                shutdown_count: failing_count.clone(),
                behavior: FailAcceptShutdown::Hang {
                    started: cleanup_started.clone(),
                    release: cleanup_release.clone(),
                },
            }),
            Box::new(NopOutbound),
        ),
    ]);
    let run = tokio::spawn(manager.run());

    // The failing instance is in its own cleanup, and the survivor is in its
    // (failing) shutdown; both are parked on gates.
    cleanup_started.notified().await;
    survivor_entered.notified().await;

    // Release ONLY the survivor: it fails its shutdown and its task exits
    // with Err, while the failing instance is still parked in cleanup. Settle
    // so the manager consumes the survivor's exit before anything else.
    survivor_gate.notify_one();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now let the failing instance finish its cleanup and exit.
    cleanup_release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("run() must return even when a survivor's shutdown fails");
    match result.expect("run task must not panic") {
        Err(SError::Instance(msg)) => {
            assert!(
                msg.contains("instance 1 accept failed"),
                "root cause masked, got: {msg}"
            );
            assert!(
                !msg.contains("survivor cleanup"),
                "cleanup error must not be the root cause, got: {msg}"
            );
        }
        other => panic!("expected instance accept failure, got {other:?}"),
    }
    assert_eq!(survivor_count.load(Ordering::SeqCst), 1);
    assert_eq!(failing_count.load(Ordering::SeqCst), 1);
}

/// Inbound whose `accept` fails a few times with a non-terminal error and
/// then serves nothing — the manager must retry, not fail fast.
struct TransientAcceptInbound {
    error_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Inbound for TransientAcceptInbound {
    async fn accept(&mut self) -> Result<ProxyRequest, SError> {
        if self.error_count.load(Ordering::SeqCst) < 3 {
            self.error_count.fetch_add(1, Ordering::SeqCst);
            Err(SError::SocksError("transient accept error".into()))
        } else {
            std::future::pending().await
        }
    }
}

/// Accept errors other than `InboundUnavailable` are retried with backoff,
/// not treated as terminal: the manager keeps the instance alive. The paused
/// tokio clock makes the backoff waits instant.
#[tokio::test(start_paused = true)]
async fn test_non_terminal_accept_errors_are_retried() {
    let error_count = Arc::new(AtomicUsize::new(0));
    let manager = Manager::with_instances(vec![Instance::new(
        Box::new(TransientAcceptInbound {
            error_count: error_count.clone(),
        }),
        Box::new(NopOutbound),
    )]);
    let mut run = tokio::spawn(manager.run());

    // Three transient errors (10/20/40ms backoffs) all elapse under the
    // paused clock.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        error_count.load(Ordering::SeqCst),
        3,
        "transient accept errors must be retried"
    );

    // The instance must still be running: the errors did not fail the
    // manager.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut run)
            .await
            .is_err(),
        "non-terminal accept errors must not fail the manager"
    );
    assert_eq!(error_count.load(Ordering::SeqCst), 3);
}

/// A cleanup error during the failing instance's own shutdown must be logged
/// but must not mask the original accept error.
#[tokio::test]
async fn test_accept_failure_cleanup_error_preserves_root_cause() {
    let failing_count = Arc::new(AtomicUsize::new(0));
    let manager = Manager::with_instances(vec![Instance::new(
        Box::new(FailAcceptInbound {
            shutdown_count: failing_count.clone(),
            behavior: FailAcceptShutdown::Err,
        }),
        Box::new(NopOutbound),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(30), manager.run())
        .await
        .expect("run() must return when an instance accept fails terminally");
    match result {
        Err(SError::Instance(msg)) => {
            // Single-instance run: the root cause is unprefixed, matching
            // the no-span logging policy.
            assert!(msg.contains("accept failed"), "got: {msg}");
            assert!(!msg.contains("instance"), "got: {msg}");
        }
        other => panic!("expected instance accept failure, got {other:?}"),
    }
    assert_eq!(failing_count.load(Ordering::SeqCst), 1);
}
