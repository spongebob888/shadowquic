use std::sync::atomic::{AtomicU32, Ordering};

static REQ_CNT: AtomicU32 = AtomicU32::new(0);

pub(crate) fn fetch_add_req_cnt() -> u32 {
    REQ_CNT.fetch_add(1, Ordering::Relaxed)
}

tokio::task_local! {
    pub(crate) static REQ_ID: u32;
}

/// At most one log line per 100 ms, for lines that anyone on the network can
/// provoke, such as failed handshakes. Each place that logs has a limit of its
/// own, so that a flood in one place does not silence another.
///
/// 32-bit atomics only: 32-bit MIPS targets have no 64-bit ones. The clock
/// wraps after 49 days; the wrapping difference keeps working across that.
pub(crate) struct LogRateLimit {
    last_ms: AtomicU32,
    held_back: AtomicU32,
}

impl LogRateLimit {
    pub(crate) const fn new() -> Self {
        Self {
            last_ms: AtomicU32::new(0),
            held_back: AtomicU32::new(0),
        }
    }

    /// `Some(n)` when a line may be logged now, where `n` lines were held
    /// back since the last one.
    pub(crate) fn allow(&self) -> Option<u32> {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
        // Starts at 100 ms so that the first line is let through.
        self.allow_at((elapsed.as_millis() as u32).wrapping_add(100))
    }

    fn allow_at(&self, now_ms: u32) -> Option<u32> {
        let last = self.last_ms.load(Ordering::Relaxed);
        if now_ms.wrapping_sub(last) >= 100
            && self
                .last_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            Some(self.held_back.swap(0, Ordering::Relaxed))
        } else {
            self.held_back.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogRateLimit;

    #[test]
    fn lines_held_back_are_counted_and_limits_are_independent() {
        let (a, b) = (LogRateLimit::new(), LogRateLimit::new());
        assert_eq!(a.allow_at(100), Some(0));
        assert_eq!(a.allow_at(150), None);
        assert_eq!(a.allow_at(199), None);
        // A flood in one place leaves the other free to log.
        assert_eq!(b.allow_at(150), Some(0));
        assert_eq!(a.allow_at(200), Some(2));
        assert_eq!(a.allow_at(300), Some(0));
    }

    #[test]
    fn limit_keeps_working_when_the_clock_wraps() {
        let limit = LogRateLimit::new();
        assert_eq!(limit.allow_at(u32::MAX - 50), Some(0));
        assert_eq!(limit.allow_at(20), None);
        assert_eq!(limit.allow_at(49), Some(1));
    }
}
