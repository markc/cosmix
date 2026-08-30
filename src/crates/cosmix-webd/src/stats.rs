//! Per-vhost response-class counters — backs `webd.stats`.
//!
//! Shape: five `AtomicU64` buckets matching the HTTP status-class
//! partition (`2xx`, `3xx`, `4xx`, `5xx`, `other` — covers `1xx`
//! informational, WebSocket `101`, and any future non-2xx..5xx). One
//! `WebdStats` instance per `VhostState`; aliases that share a
//! `VhostState` share the same counters by construction.
//!
//! All loads/stores are `Relaxed`: these are independent statistical
//! counters with no cross-counter invariant to protect (a reader may
//! see counts from slightly different instants — acceptable for a
//! health/observability surface, and far cheaper than seq-cst on the
//! hot per-request path).
//!
//! `total` is computed from the bucket sums at snapshot time, not
//! stored as a separate counter — same anti-skew shape `DnsStats`
//! uses.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct WebdStats {
    responses_2xx: AtomicU64,
    responses_3xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    responses_other: AtomicU64,
}

impl WebdStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the bucket matching `status` (raw `u16` from
    /// `Response::status().as_u16()`). Called once per request from
    /// the per-vhost router's stats middleware.
    pub fn record(&self, status: u16) {
        let bucket = match status {
            200..=299 => &self.responses_2xx,
            300..=399 => &self.responses_3xx,
            400..=499 => &self.responses_4xx,
            500..=599 => &self.responses_5xx,
            _ => &self.responses_other,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    /// `webd.stats` payload (per vhost). `total` is summed from the
    /// same relaxed loads so it is always exactly the sum of the
    /// buckets in this snapshot — no skew between `total` and the
    /// parts.
    pub fn snapshot_json(&self) -> serde_json::Value {
        let r2 = self.responses_2xx.load(Ordering::Relaxed);
        let r3 = self.responses_3xx.load(Ordering::Relaxed);
        let r4 = self.responses_4xx.load(Ordering::Relaxed);
        let r5 = self.responses_5xx.load(Ordering::Relaxed);
        let ro = self.responses_other.load(Ordering::Relaxed);
        let total = r2 + r3 + r4 + r5 + ro;
        serde_json::json!({
            "responses_2xx": r2,
            "responses_3xx": r3,
            "responses_4xx": r4,
            "responses_5xx": r5,
            "responses_other": ro,
            "total": total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_buckets_each_class_and_total_is_the_sum() {
        let s = WebdStats::new();
        s.record(200);
        s.record(204);
        s.record(301);
        s.record(404);
        s.record(500);
        // 101 Switching Protocols (WebSocket upgrade) lands in `other`.
        s.record(101);
        let j = s.snapshot_json();
        assert_eq!(j["responses_2xx"], 2);
        assert_eq!(j["responses_3xx"], 1);
        assert_eq!(j["responses_4xx"], 1);
        assert_eq!(j["responses_5xx"], 1);
        assert_eq!(j["responses_other"], 1);
        assert_eq!(j["total"], 6, "total must equal the sum of buckets");
    }

    #[test]
    fn unexpected_status_goes_to_other_not_dropped() {
        // 0 and 999 are not real HTTP statuses but the bucket logic
        // must still account for them (the `other` catch-all is the
        // never-silently-drop guarantee).
        let s = WebdStats::new();
        s.record(0);
        s.record(999);
        let j = s.snapshot_json();
        assert_eq!(j["responses_other"], 2);
        assert_eq!(j["total"], 2);
    }
}
