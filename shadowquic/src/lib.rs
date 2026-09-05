use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use error::SError;
use msgs::socks5::SocksAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tracing::{Instrument, Span, error, info, info_span};

pub mod config;
pub mod direct;
pub mod error;
#[cfg(feature = "mixed")]
pub mod http;
#[cfg(feature = "mixed")]
pub mod mixed;
pub mod msgs;
mod observe;
pub mod quic;
pub mod shadowquic;
pub mod socks;
pub mod squic;
pub mod sunnyquic;
#[cfg(all(feature = "tproxy", target_os = "linux"))]
pub mod tproxy;
pub mod utils;

pub use msgs::SDecode;
pub use msgs::SEncode;
pub enum ProxyRequest<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend> {
    Tcp(TcpSession<T>),
    Udp(UdpSession<I, O>),
}
/// Udp socket only use immutable reference to self
/// So it can be safely wrapped by Arc and cloned to work in duplex way.
#[async_trait]
pub trait UdpSend: Send + Sync + Unpin {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError>; // addr is proxy addr
}
#[async_trait]
pub trait UdpRecv: Send + Sync + Unpin {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError>; // socksaddr is proxy addr
}
pub trait Stoppable: Send + Sync {
    fn stop(&self);
}
pub type UserName = String;
pub struct TcpSession<IO = AnyTcp> {
    pub stream: IO,
    pub dst: SocksAddr,
    #[allow(dead_code)]
    user_context: Option<UserContext>,
}

pub struct UdpSession<I = AnyUdpRecv, O = AnyUdpSend> {
    pub recv: I,
    pub send: O,
    /// Control stream, should be kept alive during session.
    stream: Option<AnyTcp>,
    bind_addr: SocksAddr,
    #[allow(dead_code)]
    user_context: Option<UserContext>,
}
#[derive(Clone)]
pub struct UserContext {
    pub username: UserName,
    pub conn_handle: Weak<dyn Stoppable>,
    pub conn_id: u64,
}

pub type AnyTcp = Box<dyn TcpTrait>;
pub type AnyUdpSend = Arc<dyn UdpSend>;
pub type AnyUdpRecv = Box<dyn UdpRecv>;
pub trait TcpTrait: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl TcpTrait for TcpStream {}

#[async_trait]
pub trait Inbound<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend>: Send + Sync + Unpin {
    /// Returns the next accepted request.
    ///
    /// Returning [`SError::InboundUnavailable`] is terminal: the inbound can
    /// no longer serve traffic (its listener or request channel is gone), so
    /// the manager fails fast and drains the remaining instances. Any other
    /// error is logged and retried with a short backoff — it does not mean
    /// the instance is dead. Per-connection errors must be handled
    /// (and logged) inside the inbound, not surfaced here.
    async fn accept(&mut self) -> Result<ProxyRequest<T, I, O>, SError>;
    /// Prepares the inbound for serving (bind listeners, spawn accept loops).
    /// Called once before the accept loop starts.
    ///
    /// Contract: like [`Outbound::handle`], implementations must stay
    /// async-friendly (no long synchronous blocking), so failures and
    /// shutdown of other instances remain observable. On `Err`, partially
    /// acquired resources should be released; the manager only rolls back
    /// instances initialized *before* this one, and detached background tasks
    /// stop when the process exits after the rollback.
    async fn init(&self) -> Result<(), SError> {
        Ok(())
    }
    /// Called once on graceful shutdown, flush persistent state here.
    ///
    /// Must terminate on its own: the manager bounds the wait with a 10s
    /// drain timeout and aborts stragglers; the forced abort (with its
    /// unflushed state) is logged. Cancellation is cooperative —
    /// tokio cannot preempt a shutdown stuck in synchronous code; a hard,
    /// time-limited process exit remains the supervisor's responsibility.
    async fn shutdown(&self) -> Result<(), SError> {
        Ok(())
    }
}

#[async_trait]
pub trait Outbound<T = AnyTcp, I = AnyUdpRecv, O = AnyUdpSend>: Send + Sync + Unpin {
    /// Handle one accepted proxy request.
    ///
    /// Implementations must not drive the whole session inline: spawn the
    /// per-session work and return promptly, so the caller's accept/shutdown
    /// loop keeps making progress (shutdown observation, panic propagation,
    /// bounded draining). All built-in outbounds follow this contract.
    async fn handle(&mut self, req: ProxyRequest<T, I, O>) -> Result<(), SError>;
}

#[async_trait]
impl UdpSend for Sender<(Bytes, SocksAddr)> {
    async fn send_to(&self, buf: Bytes, addr: SocksAddr) -> Result<usize, SError> {
        let siz = buf.len();
        self.send((buf, addr))
            .await
            .map_err(|_| SError::InboundUnavailable)?;
        Ok(siz)
    }
}
#[async_trait]
impl UdpRecv for Receiver<(Bytes, SocksAddr)> {
    async fn recv_from(&mut self) -> Result<(Bytes, SocksAddr), SError> {
        let r = self.recv().await.ok_or(SError::OutboundUnavailable)?;
        Ok(r)
    }
}
/// One proxy instance: traffic accepted by `inbound` is forwarded to `outbound`.
pub struct Instance {
    pub inbound: Box<dyn Inbound>,
    pub outbound: Box<dyn Outbound>,
}

impl Instance {
    pub fn new(inbound: Box<dyn Inbound>, outbound: Box<dyn Outbound>) -> Self {
        Self { inbound, outbound }
    }
}

pub struct Manager {
    pub instances: Vec<Instance>,
}

/// Resolves when a shutdown signal is received (Ctrl-C, plus SIGTERM on unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

impl Manager {
    /// Creates a manager with a single inbound/outbound pair.
    pub fn new(inbound: Box<dyn Inbound>, outbound: Box<dyn Outbound>) -> Self {
        Self {
            instances: vec![Instance::new(inbound, outbound)],
        }
    }

    /// Creates a manager running multiple inbound/outbound pairs concurrently.
    ///
    /// # Panics
    /// Panics in debug builds if `instances` is empty; `run` rejects it in
    /// release builds.
    pub fn with_instances(instances: Vec<Instance>) -> Self {
        debug_assert!(
            !instances.is_empty(),
            "Manager requires at least one instance"
        );
        Self { instances }
    }

    /// Runs all instances until a shutdown signal (Ctrl-C / SIGTERM).
    ///
    /// Instances are **not** fault-isolated: if any instance's task exits
    /// unexpectedly (or panics, or its `accept` reports
    /// `InboundUnavailable`), the remaining instances are shut down
    /// gracefully (flushing their persistent state) and `Err` is returned so
    /// the caller can exit the process. Other accept errors are logged and
    /// retried with backoff. Run the proxy under a supervisor (systemd,
    /// procd, ...) that restarts it on failure.
    ///
    /// Guarantees are bounded by cooperative cancellation: the drain timeout
    /// aborts tasks that still yield at `await` points, but tokio cannot
    /// preempt a future stuck in synchronous code, and a task abort is not a
    /// thread kill. A hard, time-limited process exit remains the
    /// supervisor's responsibility (e.g. systemd `TimeoutStopSec`).
    ///
    /// During startup, each already-initialized instance's rollback shutdown
    /// is bounded by [`DRAIN_TIMEOUT`] individually, so worst-case rollback
    /// time grows linearly with the number of instances.
    ///
    /// When `run` returns, the instance tasks have been joined, but detached
    /// background tasks spawned by the inbounds (accept loops, flush timers)
    /// keep their sockets and state alive until the process exits. A `run`
    /// return means the manager stopped dispatching new requests — it does
    /// NOT mean background tasks or in-flight connections have stopped. CLI
    /// callers exit immediately; library callers must shut down their
    /// runtime or process afterwards.
    ///
    /// The final flush is best-effort: in-flight sessions may still update
    /// stats after an instance's `shutdown` has persisted, and deltas after
    /// the last flush are lost.
    ///
    /// `run` returns at most one error: the root cause of the first instance
    /// failure, recorded before that instance starts its cleanup. Errors
    /// secondary to it — survivors' shutdown errors, the forced abort after
    /// the drain timeout — are logged, not returned.
    pub async fn run(self) -> Result<(), SError> {
        if self.instances.is_empty() {
            return Err(SError::Instance("no instances to run".into()));
        }
        for (i, inst) in self.instances.iter().enumerate() {
            if let Err(e) = inst.inbound.init().await {
                error!(instance = i, "instance init failed: {}", e);
                // Roll back instances initialized so far (best effort), e.g. to
                // flush their persistent state, and surface the init error.
                // Each shutdown is bounded so one hung inbound cannot stall
                // startup forever.
                for (j, prev) in self.instances[..i].iter().enumerate() {
                    match tokio::time::timeout(DRAIN_TIMEOUT, prev.inbound.shutdown()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!(instance = j, "error during rollback shutdown: {}", e)
                        }
                        Err(_) => {
                            error!(
                                instance = j,
                                "rollback shutdown timed out after {}s",
                                DRAIN_TIMEOUT.as_secs()
                            )
                        }
                    }
                }
                return Err(SError::Instance(format!("instance {i} init failed: {e}")));
            }
        }
        info!("running {} instance(s)", self.instances.len());

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // Root cause of the first instance failure, recorded by the failing
        // instance BEFORE it starts any cleanup and before it broadcasts.
        // This slot is the single source of run()'s error: the manager
        // returns it regardless of task exit order (survivors that fail
        // their own shutdown and exit first must not mask it), and every
        // other error it encounters is logged and dropped.
        let first_failure: Arc<Mutex<Option<SError>>> = Arc::new(Mutex::new(None));
        let multi = self.instances.len() > 1;
        let mut tasks = JoinSet::new();
        for (i, mut inst) in self.instances.into_iter().enumerate() {
            let mut shutdown_rx = shutdown_rx.clone();
            let shutdown_tx = shutdown_tx.clone();
            let first_failure = first_failure.clone();
            // The instance index only serves to tell chains apart; skip the
            // span for the common single-instance case to keep logs terse.
            let span = if multi {
                info_span!("instance", n = i)
            } else {
                Span::none()
            };
            let task = async move {
                // Backoff for non-terminal accept errors, so a persistently
                // erroring inbound cannot hot-loop the manager.
                let mut backoff = Duration::from_millis(10);
                loop {
                    // biased: once the shutdown signal is out, it strictly
                    // outranks accepting new requests.
                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => {
                            inst.inbound.shutdown().await?;
                            return Ok(());
                        }
                        req = inst.inbound.accept() => match req {
                            Ok(req) => {
                                backoff = Duration::from_millis(10);
                                if let Err(e) = inst.outbound.handle(req).await {
                                    error!("error during handling request: {}", e)
                                }
                            }
                            Err(e @ SError::InboundUnavailable) => {
                                // Terminal: this instance can no longer serve.
                                // Record the root cause first, then notify all
                                // instances to begin shutting down immediately
                                // (the manager's drain starts once it notices
                                // the failure), and only then flush this
                                // instance's persistent state best-effort,
                                // bounded so a hung shutdown delays our exit by
                                // at most FAIL_CLEANUP_TIMEOUT. Cleanup errors
                                // are only logged; the original accept error is
                                // preserved via `first_failure`.
                                //
                                // Single-instance logs stay unprefixed,
                                // matching the span policy; the prefix is
                                // reused for this instance's cleanup errors
                                // below.
                                let prefix = if multi {
                                    format!("instance {i} ")
                                } else {
                                    String::new()
                                };
                                let root = format!("{prefix}accept failed: {e}");
                                error!("{root}");
                                {
                                    let mut slot = first_failure.lock().unwrap();
                                    if slot.is_none() {
                                        *slot = Some(SError::Instance(root.clone()));
                                    }
                                }
                                let _ = shutdown_tx.send(true);
                                match tokio::time::timeout(
                                    FAIL_CLEANUP_TIMEOUT,
                                    inst.inbound.shutdown(),
                                )
                                .await
                                {
                                    Ok(Ok(())) => {}
                                    Ok(Err(cleanup)) => {
                                        error!("{prefix}shutdown after accept failure: {cleanup}")
                                    }
                                    Err(_) => error!(
                                        "{prefix}shutdown timed out after {}s, aborting cleanup",
                                        FAIL_CLEANUP_TIMEOUT.as_secs()
                                    ),
                                }
                                return Err(SError::Instance(root));
                            }
                            Err(e) => {
                                // Retriable: log, back off, and keep serving.
                                // Per-connection failures must be contained
                                // inside the inbound; whatever surfaces here
                                // does not mean the instance is dead.
                                if multi {
                                    error!("instance {i} accept error: {}", e);
                                } else {
                                    error!("accept error: {}", e);
                                }
                                // Back off, but keep observing shutdown so a
                                // signal is not delayed by up to the full
                                // sleep.
                                tokio::select! {
                                    biased;
                                    _ = shutdown_rx.changed() => {
                                        inst.inbound.shutdown().await?;
                                        return Ok(());
                                    }
                                    _ = tokio::time::sleep(backoff) => {}
                                }
                                backoff = (backoff * 2).min(Duration::from_secs(1));
                            }
                        }
                    }
                }
            };
            tasks.spawn(task.instrument(span));
        }

        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received, persisting users and stats");
                let _ = shutdown_tx.send(true);
                // Drain-time shutdown errors are secondary and logged only;
                // a task panic found during the drain is recorded into the
                // slot instead, so a failure racing the signal still becomes
                // run()'s root cause instead of a silent Ok(()).
                drain_instances(&mut tasks, &first_failure).await;
                match first_failure.lock().unwrap().take() {
                    Some(root) => Err(root),
                    None => Ok(()),
                }
            }
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                // A task exited before the OS signal. Record the first
                // failure — a panic or an unexpected exit; a terminal accept
                // failure has already recorded itself — then drain. The
                // recorded root is the only error run() returns; everything
                // else is logged and dropped.
                {
                    let mut slot = first_failure.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(match res {
                            Ok(Ok(())) => {
                                SError::Instance("an instance stopped unexpectedly".into())
                            }
                            Ok(Err(e)) => e,
                            Err(join_err) => SError::Instance(format!(
                                "instance task failed: {join_err}"
                            )),
                        });
                    }
                }
                let _ = shutdown_tx.send(true);
                // Bounded drain: joins the remaining tasks — including the
                // failing instance if it is still finishing its own cleanup.
                drain_instances(&mut tasks, &first_failure).await;
                Err(first_failure
                    .lock()
                    .unwrap()
                    .take()
                    .expect("root cause recorded above"))
            }
        }
    }
}

/// Maximum time instances get to shut down gracefully (flush persistent
/// state) before they are aborted. Bounded so a hung `Inbound::shutdown`
/// or a custom `Outbound::handle` that blocks on a live session cannot
/// stall process exit forever.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for a failing instance's own post-failure cleanup (its final
/// flush after a terminal `InboundUnavailable` accept error). Kept strictly
/// below [`DRAIN_TIMEOUT`] to leave the global drain room to join this
/// task; deadlines alone do not determine join order, so the root accept
/// failure is additionally recorded in `first_failure` before cleanup
/// starts and can never be masked by drain ordering.
const FAIL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Records a panicked instance task as the root failure if the slot is still
/// empty. A panic is an unexpected instance failure, not a secondary
/// shutdown error: it must become `run()`'s root cause even when it only
/// surfaces during the drain (e.g. the OS signal branch won the select race
/// at the same moment). Shutdown `Err`s stay log-only by contract.
fn record_panic(first_failure: &Mutex<Option<SError>>, join_err: &JoinError) {
    if join_err.is_panic() {
        let mut slot = first_failure.lock().unwrap();
        if slot.is_none() {
            *slot = Some(SError::Instance(format!(
                "instance task failed: {join_err}"
            )));
        }
    }
}

/// Waits for all instance tasks to finish, logging (and dropping) any
/// per-instance shutdown errors — they are secondary to the root failure
/// recorded in the manager, except panics, which [`record_panic`] promotes.
/// The shutdown signal must already be sent.
///
/// Graceful draining is bounded by [`DRAIN_TIMEOUT`]; instances still
/// running when it expires are aborted (the forced abort and its unflushed
/// state are logged).
async fn drain_instances(
    tasks: &mut JoinSet<Result<(), SError>>,
    first_failure: &Arc<Mutex<Option<SError>>>,
) {
    let drain = async {
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("error during instance shutdown: {}", e),
                Err(join_err) => {
                    record_panic(first_failure, &join_err);
                    error!("error during instance shutdown: {}", join_err);
                }
            }
        }
    };
    if tokio::time::timeout(DRAIN_TIMEOUT, drain).await.is_err() {
        error!(
            "graceful shutdown timed out after {}s, aborting remaining instances",
            DRAIN_TIMEOUT.as_secs()
        );
        tasks.abort_all();
        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("error during instance shutdown: {}", e),
                Err(join_err) if join_err.is_cancelled() => {
                    error!("instance aborted after shutdown timeout")
                }
                Err(join_err) => {
                    record_panic(first_failure, &join_err);
                    error!("instance task failed: {}", join_err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draining must be bounded: a task that never finishes is aborted after
    /// [`DRAIN_TIMEOUT`] instead of hanging forever. The forced abort is
    /// logged, not returned. The paused tokio clock keeps the
    /// [`DRAIN_TIMEOUT`] wait instant.
    #[tokio::test(start_paused = true)]
    async fn drain_timeout_aborts_stragglers() {
        let mut tasks: JoinSet<Result<(), SError>> = JoinSet::new();
        tasks.spawn(std::future::pending());
        let first_failure: Arc<Mutex<Option<SError>>> = Arc::new(Mutex::new(None));
        drain_instances(&mut tasks, &first_failure).await;
        assert!(first_failure.lock().unwrap().is_none());
    }

    /// A task panic surfacing during the drain (the signal branch won the
    /// outer select race at the same moment) must still become the root
    /// cause instead of `run()` silently returning `Ok(())`. Shutdown `Err`s
    /// stay log-only by contract.
    #[tokio::test]
    async fn drain_records_task_panic_as_root_cause() {
        let mut tasks: JoinSet<Result<(), SError>> = JoinSet::new();
        tasks.spawn(async {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok::<(), SError>(())
        });
        tasks.spawn(async { Err(SError::Instance("shutdown failed".into())) });
        let first_failure: Arc<Mutex<Option<SError>>> = Arc::new(Mutex::new(None));
        drain_instances(&mut tasks, &first_failure).await;
        let root = first_failure
            .lock()
            .unwrap()
            .take()
            .expect("panic must be recorded as root cause");
        assert!(matches!(root, SError::Instance(_)));
        assert!(!root.to_string().contains("shutdown failed"));
    }
}
