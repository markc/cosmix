//! Time authority for one runner attempt.
//!
//! Production uses [`SystemClock`], which preserves the old wall-clock and
//! monotonic behaviour. Replay supplies a clock whose time advances from the
//! captured line deltas, so runner deadlines, ledger timestamps and duration
//! accounting consume recorded input instead of consulting the host clock.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// All time reads and waits which can affect a runner attempt.
///
/// `line_arrived` and `timeout_elapsed` are observation hooks: a system clock
/// advances independently and leaves them as no-ops, while a replay clock
/// advances its supplied timeline at those exact runner boundaries.
pub trait RunClock: Send + Sync {
    /// Monotonic time since this clock's arbitrary origin.
    fn monotonic(&self) -> Duration;

    /// Wall time used for ledger-visible timestamps.
    fn wall_now(&self) -> DateTime<Utc>;

    /// One non-empty raw stdout line reached the session reader.
    fn line_arrived(&self) {}

    /// A session receive timed out after the requested logical wait.
    fn timeout_elapsed(&self, _wait: Duration) {}

    /// Polling wait used while reaping the child and its pipe readers.
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The production clock. Its observation hooks deliberately do nothing:
/// real time has already advanced while the subprocess or receiver waited.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl RunClock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn wall_now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
