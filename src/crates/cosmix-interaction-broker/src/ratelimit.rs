//! Per-origin token-bucket rate limiting plus an aggregate flood backstop
//! (notify.v1 §6).
//!
//! Over-budget notifications from one origin are throttled (and logged by the
//! caller) rather than delivered — bounding the worst case for a spoofed passive
//! notify to annoyance, alongside the non-spoofable origin chrome.
//! Per-origin buckets are the fairness mechanism. The aggregate bucket is only
//! damage control for alias-churn floods: free self-selected service names mean
//! named-principal fairness remains impossible until the B-2/B-3 control-plane
//! authority layer supplies durable principals.
//!
//! Time is **injected** (`now_ms`): this core calls no clock, so tests drive it
//! deterministically. Callers may pass a monotonic or wall-clock timestamp;
//! regressions are clamped to the bucket's high-water mark and cannot mint a
//! later catch-up refill.

use std::collections::HashMap;

/// Fully refilled buckets idle beyond this window carry no useful history.
const BUCKET_IDLE_TTL_MS: u64 = 15 * 60 * 1_000;
/// Hard memory bound under registered-service name churn.
const MAX_BUCKETS: usize = 4_096;
/// No runtime config surface exists in notify.v1 yet, so this aggregate
/// backstop is a named policy constant. Burst 30/refill 5/s sits above expected
/// legitimate aggregate traffic while bounding alias-churn floods at 5/s.
pub const DEFAULT_GLOBAL_RATE: RateConfig = RateConfig {
    capacity: 30.0,
    refill_per_sec: 5.0,
};

/// Tunable bucket parameters. Defaults: burst of 5, refilling one token per
/// second (i.e. a sustained ~1 notify/s per origin, tolerating short bursts).
#[derive(Debug, Clone, Copy)]
pub struct RateConfig {
    /// Maximum tokens a bucket holds (the burst allowance).
    pub capacity: f64,
    /// Tokens replenished per second.
    pub refill_per_sec: f64,
}

impl Default for RateConfig {
    fn default() -> Self {
        RateConfig {
            capacity: 5.0,
            refill_per_sec: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

/// Per-origin fairness buckets plus one aggregate flood backstop shared by
/// every origin. One [`try_consume`](RateLimiter::try_consume) call per delivery
/// attempt; a `false` result means throttle.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    config: RateConfig,
    global_config: RateConfig,
    global: Bucket,
    buckets: HashMap<String, Bucket>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RateConfig::default())
    }
}

impl RateLimiter {
    pub fn new(config: RateConfig) -> Self {
        Self::new_with_global(config, DEFAULT_GLOBAL_RATE)
    }

    /// Construct with explicit per-origin and aggregate budgets. Production v1
    /// uses [`Self::new`]; this seam keeps policy tests deterministic.
    pub fn new_with_global(config: RateConfig, global_config: RateConfig) -> Self {
        RateLimiter {
            config,
            global_config,
            global: Bucket {
                tokens: global_config.capacity,
                last_ms: 0,
            },
            buckets: HashMap::new(),
        }
    }

    /// Attempt to spend one per-origin token and one aggregate token at
    /// `now_ms`. Returns `true` only when both were available. A fresh origin
    /// starts with a full bucket; failed attempts consume neither budget.
    pub fn try_consume(&mut self, origin: &str, now_ms: u64) -> bool {
        self.buckets
            .retain(|_, bucket| now_ms.saturating_sub(bucket.last_ms) <= BUCKET_IDLE_TTL_MS);
        if !self.buckets.contains_key(origin) && self.buckets.len() >= MAX_BUCKETS {
            let oldest = self
                .buckets
                .iter()
                .min_by(|(left_name, left), (right_name, right)| {
                    left.last_ms
                        .cmp(&right.last_ms)
                        .then_with(|| left_name.cmp(right_name))
                })
                .map(|(name, _)| name.clone());
            if let Some(oldest) = oldest {
                self.buckets.remove(&oldest);
            }
        }
        let origin_config = self.config;
        let origin_bucket = self.buckets.entry(origin.to_string()).or_insert(Bucket {
            tokens: origin_config.capacity,
            last_ms: now_ms,
        });
        refill(origin_bucket, origin_config, now_ms);
        refill(&mut self.global, self.global_config, now_ms);

        if origin_bucket.tokens >= 1.0 && self.global.tokens >= 1.0 {
            origin_bucket.tokens -= 1.0;
            self.global.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drop a bucket's state (e.g. an origin with no live notifications). Purely
    /// housekeeping; a dropped origin simply starts full again next time.
    pub fn forget(&mut self, origin: &str) {
        self.buckets.remove(origin);
    }
}

fn refill(bucket: &mut Bucket, config: RateConfig, now_ms: u64) {
    // A non-monotonic clock contributes zero and cannot move the high-water
    // mark backwards; recovery to the previous timestamp cannot mint a second
    // refill interval.
    let elapsed_ms = now_ms.saturating_sub(bucket.last_ms);
    let replenished = (elapsed_ms as f64 / 1_000.0) * config.refill_per_sec;
    bucket.tokens = (bucket.tokens + replenished).min(config.capacity);
    bucket.last_ms = bucket.last_ms.max(now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_up_to_capacity_then_throttles() {
        let mut rl = RateLimiter::new(RateConfig {
            capacity: 3.0,
            refill_per_sec: 1.0,
        });
        assert!(rl.try_consume("musicd", 0));
        assert!(rl.try_consume("musicd", 0));
        assert!(rl.try_consume("musicd", 0));
        // fourth in the same instant is over budget
        assert!(!rl.try_consume("musicd", 0));
    }

    #[test]
    fn refills_over_time() {
        let mut rl = RateLimiter::new(RateConfig {
            capacity: 2.0,
            refill_per_sec: 1.0,
        });
        assert!(rl.try_consume("x", 0));
        assert!(rl.try_consume("x", 0));
        assert!(!rl.try_consume("x", 0));
        // one second later, one token is back
        assert!(rl.try_consume("x", 1000));
        assert!(!rl.try_consume("x", 1000));
    }

    #[test]
    fn buckets_are_per_origin() {
        let mut rl = RateLimiter::new(RateConfig {
            capacity: 1.0,
            refill_per_sec: 1.0,
        });
        assert!(rl.try_consume("a", 0));
        assert!(!rl.try_consume("a", 0));
        // a different origin is unaffected
        assert!(rl.try_consume("b", 0));
    }

    #[test]
    fn aggregate_backstop_bounds_alias_churn_at_production_policy() {
        let mut rl = RateLimiter::default();
        for index in 0..DEFAULT_GLOBAL_RATE.capacity as usize {
            assert!(rl.try_consume(&format!("alias-{index}"), 0));
        }
        assert!(!rl.try_consume("alias-overflow", 0));
        assert!(!rl.try_consume("alias-overflow", 199));
        assert!(rl.try_consume("alias-overflow", 200));
    }

    #[test]
    fn two_origins_at_normal_rates_are_not_throttled_by_backstop() {
        let mut rl = RateLimiter::default();
        for second in 0..120 {
            let now_ms = second * 1_000;
            assert!(rl.try_consume("maild", now_ms));
            assert!(rl.try_consume("musicd", now_ms));
        }
    }

    #[test]
    fn non_monotonic_clock_does_not_overfill() {
        let mut rl = RateLimiter::new(RateConfig {
            capacity: 2.0,
            refill_per_sec: 1.0,
        });
        assert!(rl.try_consume("x", 10_000));
        assert!(rl.try_consume("x", 10_000));
        // clock jumps backwards: no free refill
        assert!(!rl.try_consume("x", 5_000));
        // returning to the prior high-water mark is not a second refill window
        assert!(!rl.try_consume("x", 10_000));
        assert!(rl.try_consume("x", 11_000));
    }

    #[test]
    fn idle_buckets_expire() {
        let mut rl = RateLimiter::new(RateConfig::default());
        assert!(rl.try_consume("old", 0));
        assert!(rl.try_consume("current", BUCKET_IDLE_TTL_MS + 1));
        assert!(!rl.buckets.contains_key("old"));
        assert!(rl.buckets.contains_key("current"));
    }

    #[test]
    fn service_name_churn_is_cardinality_bounded() {
        let mut rl = RateLimiter::new_with_global(
            RateConfig::default(),
            RateConfig {
                capacity: (MAX_BUCKETS + 1) as f64,
                refill_per_sec: 0.0,
            },
        );
        for index in 0..=MAX_BUCKETS {
            assert!(rl.try_consume(&format!("service-{index}"), index as u64));
        }
        assert_eq!(rl.buckets.len(), MAX_BUCKETS);
        assert!(!rl.buckets.contains_key("service-0"));
        assert!(rl.buckets.contains_key(&format!("service-{MAX_BUCKETS}")));
    }
}
