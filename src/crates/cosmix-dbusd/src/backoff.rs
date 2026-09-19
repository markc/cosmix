//! Pure restart-backoff schedule for supervised adapters.
//!
//! No clock, no runtime: the supervisor feeds in how long a failed launch
//! stayed healthy and reads out the next wait. The values themselves are
//! asserted in tests, not inferred from sleeps.

use std::time::Duration;

/// First backoff delay after a failure.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Ceiling for the exponential growth.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A launch that stayed up at least this long resets the schedule.
pub const HEALTHY_RESET_AFTER: Duration = Duration::from_secs(5 * 60);

/// Failure counter driving the exponential schedule.
///
/// Delays double from [`BACKOFF_INITIAL`] up to [`BACKOFF_MAX`]: 1, 2, 4,
/// 8, 16, 32, then capped at 60 s. A launch that stayed healthy for
/// [`HEALTHY_RESET_AFTER`] or longer before failing resets the count, so
/// an adapter that ran fine for five minutes and then trips starts over
/// at 1 s rather than resuming an old escalation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// Failures recorded since the last reset.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Record a failure and continue the escalation.
    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    /// Record a failure after a launch that stayed healthy for `healthy`.
    /// A long-enough healthy run resets the schedule first.
    pub fn record_failure_after_healthy(&mut self, healthy: Duration) {
        if healthy >= HEALTHY_RESET_AFTER {
            self.failures = 0;
        }
        self.record_failure();
    }

    /// Clear the schedule (deliberate restart, disable/enable cycle).
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// How long to wait before the next launch, given the failures
    /// recorded so far. A schedule with no recorded failure waits
    /// nothing (a deliberate restart launches immediately).
    pub fn next_delay(&self) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        // failures=1 -> 1 s, 2 -> 2 s ... 6 -> 32 s, 7+ -> 60 s cap.
        let doublings = (self.failures - 1).min(6);
        BACKOFF_INITIAL
            .saturating_mul(1_u32 << doublings)
            .min(BACKOFF_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delays_after(failures: u32) -> Vec<Duration> {
        let mut backoff = Backoff::default();
        let mut out = Vec::new();
        for _ in 0..failures {
            backoff.record_failure();
            out.push(backoff.next_delay());
        }
        out
    }

    #[test]
    fn schedule_doubles_from_one_second_to_the_sixty_second_cap() {
        assert_eq!(
            delays_after(9),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(32),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ]
        );
    }

    #[test]
    fn fresh_schedule_starts_at_one_second() {
        assert_eq!(Backoff::default().next_delay(), Duration::from_secs(0));
        let mut backoff = Backoff::default();
        backoff.record_failure();
        assert_eq!(backoff.failures(), 1);
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn five_healthy_minutes_reset_the_schedule() {
        let mut backoff = Backoff::default();
        for _ in 0..5 {
            backoff.record_failure();
        }
        assert_eq!(backoff.next_delay(), Duration::from_secs(16));
        backoff.record_failure_after_healthy(HEALTHY_RESET_AFTER);
        assert_eq!(backoff.failures(), 1);
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn short_healthy_run_does_not_reset_the_schedule() {
        let mut backoff = Backoff::default();
        backoff.record_failure();
        backoff.record_failure_after_healthy(HEALTHY_RESET_AFTER - Duration::from_secs(1));
        assert_eq!(backoff.failures(), 2);
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    }

    #[test]
    fn reset_clears_the_escalation() {
        let mut backoff = Backoff::default();
        for _ in 0..8 {
            backoff.record_failure();
        }
        backoff.reset();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.next_delay(), Duration::from_secs(0));
    }
}
