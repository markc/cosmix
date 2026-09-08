//! Demand lease and coalescing for native pointer observation.

use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const LEASE: Duration = Duration::from_secs(3);
pub(crate) const INTERVAL: Duration = Duration::from_millis(34);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PointerSample {
    pub(crate) version: u32,
    pub(crate) instance: Arc<str>,
    pub(crate) output: Option<String>,
    pub(crate) position: Option<PointerPosition>,
    pub(crate) valid: bool,
    /// Monotonic milliseconds since this compositor observation instance.
    pub(crate) timestamp_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct PointerPosition {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Default)]
pub(crate) struct PointerLease {
    until: Option<Instant>,
    last: Option<Instant>,
    dirty: bool,
}

impl PointerLease {
    pub(crate) fn renew(&mut self, now: Instant) {
        self.until = Some(now + LEASE);
        self.dirty = true;
    }
    pub(crate) fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
    pub(crate) fn changed(&mut self) {
        self.dirty = true;
    }
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        if !self.active(now) || !self.dirty || self.last.is_some_and(|last| now < last + INTERVAL) {
            return false;
        }
        self.last = Some(now);
        self.dirty = false;
        true
    }
    pub(crate) fn deadline(&self, now: Instant) -> Option<Instant> {
        let until = self.until.filter(|until| now < *until)?;
        Some(if self.dirty {
            self.last.map_or(now, |last| (last + INTERVAL).min(until))
        } else {
            until
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_demand_has_no_publication_or_timer() {
        let now = Instant::now();
        let mut lease = PointerLease::default();
        lease.changed();
        assert!(!lease.take(now));
        assert!(lease.deadline(now).is_none());
        lease.renew(now);
        assert!(lease.take(now));
        assert_eq!(lease.deadline(now), Some(now + LEASE));
        assert!(!lease.take(now + LEASE));
        assert!(lease.deadline(now + LEASE).is_none());
    }
    #[test]
    fn rapid_motion_and_renewal_cannot_exceed_rate() {
        let now = Instant::now();
        let mut lease = PointerLease::default();
        lease.renew(now);
        assert!(lease.take(now));
        for millis in 1..34 {
            lease.changed();
            lease.renew(now + Duration::from_millis(millis));
            assert!(!lease.take(now + Duration::from_millis(millis)));
        }
        assert_eq!(lease.deadline(now), Some(now + INTERVAL));
        assert!(lease.take(now + INTERVAL));
        assert!(!lease.take(now + INTERVAL));
    }
    #[test]
    fn renewal_recovers_after_expiry_without_retaining_motion_history() {
        let now = Instant::now();
        let mut lease = PointerLease::default();
        lease.renew(now);
        assert!(lease.take(now));
        for _ in 0..100_000 {
            lease.changed();
        }
        assert!(!lease.take(now + LEASE));
        lease.renew(now + LEASE);
        assert!(lease.take(now + LEASE));
        assert!(!lease.take(now + LEASE));
    }
}
