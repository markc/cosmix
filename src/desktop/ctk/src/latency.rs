//! P6 latency instrumentation — fixed-bucket histograms for the numbers the
//! two-build experiment demands: how far the full Bus+musicd path sits from
//! the in-process-prototype reference (device-buffer-only latency).
//!
//! Alloc-free on the record path (fixed arrays, no boxing) per the
//! instrumentation-zero-cost rule; the report system prints one summary line
//! per interval, and only when new samples arrived.

use std::time::Duration;

/// Bucket upper bounds in microseconds (last bucket is open-ended). Chosen to
/// resolve the interesting band: sub-millisecond round trips up to worst-case
/// three-frame stalls.
const BUCKET_UPPER_US: [u64; 15] = [
    250,
    500,
    1_000,
    2_000,
    4_000,
    8_000,
    12_000,
    16_000,
    24_000,
    32_000,
    48_000,
    64_000,
    96_000,
    128_000,
    u64::MAX,
];

/// A fixed-bucket latency histogram: record in O(buckets), summarize with
/// p50/p99 (bucket-upper-bound resolution), true max and mean.
#[derive(Clone, Debug)]
pub struct LatencyHistogram {
    buckets: [u64; BUCKET_UPPER_US.len()],
    count: u64,
    sum_us: u64,
    max_us: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKET_UPPER_US.len()],
            count: 0,
            sum_us: 0,
            max_us: 0,
        }
    }
}

impl LatencyHistogram {
    pub fn record(&mut self, sample: Duration) {
        let us = u64::try_from(sample.as_micros()).unwrap_or(u64::MAX);
        let slot = BUCKET_UPPER_US
            .iter()
            .position(|upper| us <= *upper)
            .unwrap_or(BUCKET_UPPER_US.len() - 1);
        self.buckets[slot] += 1;
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Upper bound of the bucket containing quantile `q` (0.0..=1.0).
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * q.clamp(0.0, 1.0)).ceil() as u64;
        let mut seen = 0u64;
        for (slot, hits) in self.buckets.iter().enumerate() {
            seen += hits;
            if seen >= target.max(1) {
                // The open-ended tail reports the true max instead of u64::MAX.
                if slot == BUCKET_UPPER_US.len() - 1 {
                    return self.max_us;
                }
                return BUCKET_UPPER_US[slot];
            }
        }
        self.max_us
    }

    pub fn mean_us(&self) -> u64 {
        self.sum_us.checked_div(self.count).unwrap_or(0)
    }

    pub fn max_us(&self) -> u64 {
        self.max_us
    }

    /// `p50≤2.0ms p99≤16.0ms max=21.3ms n=482` (empty histogram → `n=0`).
    pub fn summary(&self) -> String {
        if self.count == 0 {
            return "n=0".to_string();
        }
        format!(
            "p50≤{:.1}ms p99≤{:.1}ms max={:.1}ms n={}",
            self.quantile_us(0.50) as f64 / 1000.0,
            self.quantile_us(0.99) as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_land_in_the_right_buckets() {
        let mut h = LatencyHistogram::default();
        for _ in 0..90 {
            h.record(Duration::from_micros(900)); // ≤1ms bucket
        }
        for _ in 0..10 {
            h.record(Duration::from_millis(20)); // ≤24ms bucket
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.quantile_us(0.50), 1_000);
        assert_eq!(h.quantile_us(0.99), 24_000);
        assert_eq!(h.max_us(), 20_000);
    }

    #[test]
    fn empty_histogram_is_harmless() {
        let h = LatencyHistogram::default();
        assert_eq!(h.quantile_us(0.99), 0);
        assert_eq!(h.summary(), "n=0");
    }

    #[test]
    fn open_tail_reports_true_max() {
        let mut h = LatencyHistogram::default();
        h.record(Duration::from_millis(500));
        assert_eq!(h.quantile_us(0.99), 500_000);
    }
}
