#[cfg(not(target_has_atomic = "64"))]
use portable_atomic::{AtomicU64, Ordering};
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::{AtomicU64, Ordering};

use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Monotonic millisecond clock shared by every activity guard in the process.
///
/// Never returns 0. `half_closed_millis` uses 0 to mean "no half-close yet", so a
/// timestamp of 0 would be indistinguishable from that state and would disarm the
/// watchdog for a session whose half-close landed in the epoch's first
/// millisecond.
fn now_millis() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

#[derive(Debug)]
struct ActivityState {
    /// Last time a byte moved through either guarded end.
    last_millis: AtomicU64,
    /// When the first of the two ends finished; 0 while both are still open.
    half_closed_millis: AtomicU64,
}

/// Progress and half-close bookkeeping shared by the two ends of one relay.
///
/// `copy_bidirectional` returns only once both directions reach EOF, so a
/// session whose peer never closes its half never ends, and the QUIC bi-stream
/// it holds is never returned to the peer's stream limit. Once that limit is
/// exhausted, every new request on the connection blocks in `open_bi`.
///
/// Giving up on any relay that merely went quiet would also end healthy idle
/// sessions, so the deadline only starts once one direction has finished. From
/// that point the surviving direction is waiting for the peer to close, and a
/// peer that stays silent is indistinguishable from one that never will.
#[derive(Debug, Clone)]
pub struct Activity {
    state: Arc<ActivityState>,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ActivityState {
                last_millis: AtomicU64::new(now_millis()),
                half_closed_millis: AtomicU64::new(0),
            }),
        }
    }

    fn touch(&self) {
        self.state
            .last_millis
            .store(now_millis(), Ordering::Relaxed);
    }

    fn half_closed(&self) {
        let _ = self.state.half_closed_millis.compare_exchange(
            0,
            now_millis(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// How long the surviving direction has been silent since the first one
    /// finished, or `None` while both are still open.
    pub fn quiet_since_half_close(&self) -> Option<Duration> {
        let half_closed = self.state.half_closed_millis.load(Ordering::Relaxed);
        if half_closed == 0 {
            return None;
        }
        let last = self.state.last_millis.load(Ordering::Relaxed);
        Some(Duration::from_millis(
            now_millis().saturating_sub(last.max(half_closed)),
        ))
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps a stream, recording every byte that moves through it and the moment
/// its write half is shut down.
///
/// Both ends of a relay are guarded: whichever end is shut down first tells us
/// which direction finished, since `copy_bidirectional` shuts down the writer
/// of the opposite direction once a reader reaches EOF.
#[derive(Debug)]
pub struct ActivityGuard<S> {
    inner: S,
    activity: Activity,
}

impl<S> ActivityGuard<S> {
    pub fn new(inner: S, activity: Activity) -> Self {
        Self { inner, activity }
    }
}

impl<S> AsyncRead for ActivityGuard<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        let moved = match &poll {
            Poll::Ready(Ok(())) => buf.filled().len() - before,
            _ => 0,
        };
        if moved > 0 {
            this.activity.touch();
        }
        poll
    }
}

impl<S> AsyncWrite for ActivityGuard<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        let moved = match &poll {
            Poll::Ready(Ok(n)) => *n,
            _ => 0,
        };
        if moved > 0 {
            this.activity.touch();
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_shutdown(cx);
        if let Poll::Ready(Ok(())) = &poll {
            this.activity.half_closed();
        }
        poll
    }
}

/// Resolves once one direction has finished and the other has stayed completely
/// silent for `quiet`.
///
/// Polling in bounded steps keeps the wake-up that ends a session within `step`
/// of the deadline, whatever `quiet` is.
pub async fn half_close_watchdog(activity: &Activity, quiet: Duration) {
    let step = quiet.min(Duration::from_secs(30));
    loop {
        tokio::time::sleep(step).await;
        if activity
            .quiet_since_half_close()
            .is_some_and(|silent| silent >= quiet)
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// These tests drive a paused clock, but must not compare an exact millisecond:
    /// `now_millis` reads a process-global epoch, so another test running on its own
    /// runtime in parallel can shift its sub-millisecond part. Every assertion about
    /// elapsed silence is therefore a window, wide enough that a one-millisecond
    /// artifact cannot decide the outcome.
    const TOLERANCE: Duration = Duration::from_secs(1);

    /// The silence measured since the first half-close.
    fn quiet(activity: &Activity) -> Duration {
        activity
            .quiet_since_half_close()
            .expect("a session that half-closed must report its silence")
    }

    fn assert_still_quiet(activity: &Activity, at_least: Duration, why: &str) {
        let measured = quiet(activity);
        assert!(
            measured + TOLERANCE >= at_least,
            "{why}: expected about {at_least:?} of silence, measured {measured:?}"
        );
    }

    fn assert_restarted(activity: &Activity, why: &str) {
        let measured = quiet(activity);
        assert!(
            measured <= TOLERANCE,
            "{why}: the silence should have restarted, but it measured {measured:?}"
        );
    }

    /// A session with both directions still open has no deadline at all, however
    /// long it stays quiet. This is what keeps an idle interactive login alive.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_does_not_apply_before_a_half_close() {
        let activity = Activity::new();
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(activity.quiet_since_half_close(), None);
    }

    /// Silence is counted from the first half-close onwards, and any byte moved
    /// after it restarts the countdown.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_restarts_on_every_byte_after_the_half_close() {
        let activity = Activity::new();
        activity.half_closed();

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_still_quiet(&activity, Duration::from_secs(10), "ten seconds passed");

        activity.touch();
        assert_restarted(&activity, "a byte moved after the half-close");

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_still_quiet(
            &activity,
            Duration::from_secs(10),
            "the deadline ran again after being restarted",
        );
    }

    /// The watchdog needs both conditions: one direction finished, and the
    /// surviving one then stayed silent for the whole grace.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_waits_for_both_a_half_close_and_the_grace() {
        let grace = Duration::from_secs(600);
        let activity = Activity::new();

        // While both directions are open the watchdog loops forever, so bound it and
        // require it to still be running several grace periods later.
        let not_armed =
            tokio::time::timeout(3 * grace, half_close_watchdog(&activity, grace)).await;
        assert!(
            not_armed.is_err(),
            "a session with both directions still open must never be given up on"
        );

        activity.half_closed();
        let armed_at = tokio::time::Instant::now();
        half_close_watchdog(&activity, grace).await;
        let waited = armed_at.elapsed();
        assert!(
            waited >= grace,
            "the watchdog fired after {waited:?}, before the {grace:?} grace had elapsed"
        );
    }

    /// Only bytes that actually moved count as activity. An EOF completes without
    /// filling the buffer, so it must not be mistaken for the peer coming back.
    #[tokio::test(start_paused = true)]
    async fn an_eof_read_moves_no_bytes_and_does_not_restart_the_deadline() {
        let (mine, theirs) = tokio::io::duplex(64);
        let activity = Activity::new();
        let mut guarded = ActivityGuard::new(mine, activity.clone());

        activity.half_closed();
        tokio::time::advance(Duration::from_secs(5)).await;
        drop(theirs);

        let mut buf = [0u8; 8];
        assert_eq!(guarded.read(&mut buf).await.unwrap(), 0);
        assert_still_quiet(
            &activity,
            Duration::from_secs(5),
            "reading EOF resumed no traffic, so the deadline must keep running",
        );
    }

    /// The guard is what feeds the activity record: bytes through either half are
    /// activity.
    #[tokio::test(start_paused = true)]
    async fn the_guard_records_bytes_moving_in_either_direction() {
        let (mine, mut theirs) = tokio::io::duplex(64);
        let activity = Activity::new();
        let mut guarded = ActivityGuard::new(mine, activity.clone());
        activity.half_closed();

        tokio::time::advance(Duration::from_secs(30)).await;
        guarded.write_all(b"hello").await.unwrap();
        assert_restarted(&activity, "a byte written through the guard");

        tokio::time::advance(Duration::from_secs(30)).await;
        theirs.write_all(b"world").await.unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(guarded.read(&mut buf).await.unwrap(), 5);
        assert_restarted(&activity, "a byte read through the guard");
    }

    /// Shutting down the guarded write half is what arms the deadline, and the
    /// moment is recorded once rather than on every shutdown.
    #[tokio::test(start_paused = true)]
    async fn shutting_down_the_guarded_writer_arms_the_deadline_once() {
        let (mine, _theirs) = tokio::io::duplex(64);
        let activity = Activity::new();
        let mut guarded = ActivityGuard::new(mine, activity.clone());
        assert_eq!(activity.quiet_since_half_close(), None);

        guarded.shutdown().await.unwrap();
        assert_restarted(
            &activity,
            "shutting the write half down must arm the deadline",
        );

        tokio::time::advance(Duration::from_secs(10)).await;
        let _ = guarded.shutdown().await;
        assert_still_quiet(
            &activity,
            Duration::from_secs(10),
            "the half-close is recorded once, not on every shutdown",
        );
    }
}
