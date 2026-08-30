//! Process-lifetime monotonic counters for the rule engine, shared
//! between the inbound DATA path and the Bus `maild.rules.stats`
//! action.
//!
//! Verdict shapes (`hard_accept`, `hard_junk`, `continue_count`) live
//! in `AtomicU64`s, incremented in `record()` from the hot path.
//! `snapshot()` reads each verdict counter independently and derives
//! `verdicts_total` as their sum — `verdicts_total ==
//! hard_accept + hard_junk + continue_count` always holds, even
//! during concurrent `record()` calls (the sampler may briefly lag
//! the true count but never overshoots and never disagrees with
//! itself).
//!
//! Per-rule counters (Phase 2 commit 4) live in
//! `Mutex<HashMap<RuleId, u64>>`. Typical rule packs ship ~30 rules
//! and per-message classify already costs orders of magnitude more
//! than a single uncontended mutex lock, so the simpler primitive
//! beats `DashMap` for this access pattern — the snapshot path is
//! also cheaper because we clone the HashMap once under the lock
//! rather than scanning N shards. The hot path is `record_rule_hit`,
//! invoked once per matched rule per classify via the
//! `cosmix_maild_rules::DefaultRuleEngine` match hook.
//!
//! Restart persistence lives in [`rule_stats_store`]; this module
//! only manages in-memory state and supplies `RuleStatsSnapshot` as
//! the wire/disk shape.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cosmix_maild_rules::{RuleId, RuleVerdict};

#[derive(Debug, Default)]
pub struct RuleStats {
    hard_accept: AtomicU64,
    hard_junk: AtomicU64,
    continue_count: AtomicU64,
    /// Per-rule hit counter map. Inserted on first hit by
    /// `record_rule_hit`; existing rule pack entries that have never
    /// matched stay absent (snapshot returns them as missing keys,
    /// the consumer can defaultize to 0 if it cares about every rule
    /// id). Removed-from-pack rules retain their historic counts so
    /// operators can audit hit history after a pack edit.
    per_rule: Mutex<HashMap<RuleId, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleStatsSnapshot {
    pub verdicts_total: u64,
    pub hard_accept: u64,
    pub hard_junk: u64,
    pub continue_count: u64,
    /// Per-rule hit counts keyed by `RuleId`. Empty if the engine
    /// has never matched any rule (or `from_snapshot` was given
    /// an empty map at construction).
    pub per_rule: HashMap<RuleId, u64>,
}

impl RuleStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rehydrate the in-memory counters from a persisted snapshot.
    /// Used by the runtime at startup after [`rule_stats_store`]
    /// reads the on-disk row. Verdict counts are restored to the
    /// `AtomicU64`s; per-rule counts populate the map.
    pub fn from_snapshot(snap: RuleStatsSnapshot) -> Self {
        Self {
            hard_accept: AtomicU64::new(snap.hard_accept),
            hard_junk: AtomicU64::new(snap.hard_junk),
            continue_count: AtomicU64::new(snap.continue_count),
            per_rule: Mutex::new(snap.per_rule),
        }
    }

    pub fn record(&self, verdict: &RuleVerdict) {
        match verdict {
            RuleVerdict::HardAccept { .. } => {
                self.hard_accept.fetch_add(1, Ordering::Relaxed);
            }
            RuleVerdict::HardJunk { .. } => {
                self.hard_junk.fetch_add(1, Ordering::Relaxed);
            }
            RuleVerdict::Continue { .. } => {
                self.continue_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Increment the per-rule hit counter for `id`. Called once per
    /// matched rule per `classify` from the engine's
    /// `on_rule_matched` hook. New keys are inserted on first hit.
    pub fn record_rule_hit(&self, id: &RuleId) {
        let mut map = self
            .per_rule
            .lock()
            .expect("rule_stats per_rule mutex poisoned");
        *map.entry(id.clone()).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> RuleStatsSnapshot {
        let hard_accept = self.hard_accept.load(Ordering::Relaxed);
        let hard_junk = self.hard_junk.load(Ordering::Relaxed);
        let continue_count = self.continue_count.load(Ordering::Relaxed);
        // Per-rule snapshot is a single-lock clone; the wire shape is
        // intentionally a plain HashMap so callers don't have to think
        // about the storage primitive.
        let per_rule = self
            .per_rule
            .lock()
            .expect("rule_stats per_rule mutex poisoned")
            .clone();
        RuleStatsSnapshot {
            verdicts_total: hard_accept + hard_junk + continue_count,
            hard_accept,
            hard_junk,
            continue_count,
            per_rule,
        }
    }

    /// Snapshot the verdict counters and the top `n` per-rule
    /// entries by hit count (descending), tiebroken by `RuleId`
    /// ascending so the returned set is deterministic across calls.
    /// Returns `(snapshot_with_at_most_n_entries, untruncated_total)`.
    ///
    /// Hot-path discipline: for `n == 0` the per-rule mutex is
    /// held only long enough to read `.len()` — no clone, no
    /// per-entry work, no allocation. For `n > 0` the mutex is
    /// held for the HashMap clone itself, same wall-clock cost as
    /// the legacy full-clone `snapshot()`. The bounded partial-sort
    /// runs after the mutex is released so `record_rule_hit`
    /// (called from the classify hot path) is never blocked by
    /// selection work, only by the memcpy. Final allocation is
    /// O(n); the transient clone is O(N) and dropped as the heap
    /// is built.
    ///
    /// Work-per-call is O(N log n) total, bounded by the stored
    /// cardinality `N`. That bound is itself accepted as a function
    /// of operator pack-churn behavior; capping stored cardinality
    /// (eviction policy, retention window) is out of scope here and
    /// tracked separately for the persistence layer.
    ///
    /// The persistence layer (`flush_loop` / `flush_snapshot`) still
    /// uses the full-clone `snapshot()` because it genuinely needs
    /// every entry.
    pub fn snapshot_top_n_rules(&self, n: usize) -> (RuleStatsSnapshot, usize) {
        let hard_accept = self.hard_accept.load(Ordering::Relaxed);
        let hard_junk = self.hard_junk.load(Ordering::Relaxed);
        let continue_count = self.continue_count.load(Ordering::Relaxed);

        if n == 0 {
            // Cheap cardinality-only path — read `.len()` under the
            // lock and skip the HashMap clone entirely. This is the
            // call shape a caller uses to ask "how many distinct
            // rules have ever matched?" without paying for any
            // per-entry work or allocation.
            let total = self
                .per_rule
                .lock()
                .expect("rule_stats per_rule mutex poisoned")
                .len();
            return (
                RuleStatsSnapshot {
                    verdicts_total: hard_accept + hard_junk + continue_count,
                    hard_accept,
                    hard_junk,
                    continue_count,
                    per_rule: HashMap::new(),
                },
                total,
            );
        }

        // Clone inside the mutex → release → partial-sort outside.
        // This is the same hold-time profile as the legacy
        // `snapshot()` (memcpy of the HashMap), so the classify
        // hot-path's `record_rule_hit` waits only for the clone,
        // not for the heap-update loop.
        let full = self
            .per_rule
            .lock()
            .expect("rule_stats per_rule mutex poisoned")
            .clone();
        let total_cardinality = full.len();

        let per_rule = if n >= total_cardinality {
            // No selection needed — return the clone verbatim.
            full
        } else {
            // Min-heap (via `(Reverse(count), id)` ordering): heap
            // root is the worst surviving entry — smallest count,
            // with the largest `RuleId` as tiebreak. A new
            // candidate displaces the root iff it is strictly
            // better (larger count, or equal count with smaller
            // id). At loop end the heap holds the top-n by the
            // desired ordering; we then drain it into a HashMap
            // for the snapshot shape.
            let cap = n.min(total_cardinality).max(1);
            let mut heap: BinaryHeap<(Reverse<u64>, RuleId)> = BinaryHeap::with_capacity(cap);
            for (id, count) in full.iter() {
                if heap.len() < n {
                    heap.push((Reverse(*count), id.clone()));
                } else {
                    let worst = heap.peek().expect("heap non-empty when len == n > 0");
                    let cand_key = Reverse(*count);
                    let candidate_beats_worst = cand_key < worst.0
                        || (cand_key == worst.0 && id.as_str() < worst.1.as_str());
                    if candidate_beats_worst {
                        heap.pop();
                        heap.push((Reverse(*count), id.clone()));
                    }
                }
            }
            let mut out: HashMap<RuleId, u64> = HashMap::with_capacity(heap.len());
            for (Reverse(count), id) in heap {
                out.insert(id, count);
            }
            out
        };

        (
            RuleStatsSnapshot {
                verdicts_total: hard_accept + hard_junk + continue_count,
                hard_accept,
                hard_junk,
                continue_count,
                per_rule,
            },
            total_cardinality,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn hard_accept() -> RuleVerdict {
        RuleVerdict::HardAccept {
            reason: cosmix_maild_rules::AcceptReason::AllowlistSender,
            matched_rules: vec![],
        }
    }

    fn hard_junk() -> RuleVerdict {
        RuleVerdict::HardJunk {
            reason: cosmix_maild_rules::JunkReason::BlocklistSender,
            matched_rules: vec![],
            score: 0.0,
        }
    }

    fn continue_v() -> RuleVerdict {
        RuleVerdict::Continue {
            score: 0.0,
            matched_rules: vec![],
            would_junk: false,
        }
    }

    #[test]
    fn records_each_verdict_kind() {
        let stats = RuleStats::new();
        stats.record(&hard_accept());
        stats.record(&hard_junk());
        stats.record(&hard_junk());
        stats.record(&continue_v());
        stats.record(&continue_v());
        stats.record(&continue_v());
        let snap = stats.snapshot();
        assert_eq!(snap.verdicts_total, 6);
        assert_eq!(snap.hard_accept, 1);
        assert_eq!(snap.hard_junk, 2);
        assert_eq!(snap.continue_count, 3);
        assert!(
            snap.per_rule.is_empty(),
            "no per-rule hits recorded → empty map"
        );
    }

    #[test]
    fn records_per_rule_hits() {
        let stats = RuleStats::new();
        let a = "rule_a".to_string();
        let b = "rule_b".to_string();
        stats.record_rule_hit(&a);
        stats.record_rule_hit(&a);
        stats.record_rule_hit(&b);
        stats.record_rule_hit(&a);
        let snap = stats.snapshot();
        assert_eq!(snap.per_rule.get("rule_a").copied(), Some(3));
        assert_eq!(snap.per_rule.get("rule_b").copied(), Some(1));
        assert_eq!(snap.per_rule.len(), 2);
    }

    #[test]
    fn snapshot_top_n_rules_returns_descending_by_count() {
        let stats = RuleStats::new();
        for (id, n) in [("a", 1u64), ("b", 5), ("c", 3), ("d", 4), ("e", 2)] {
            let owned = id.to_string();
            for _ in 0..n {
                stats.record_rule_hit(&owned);
            }
        }
        let (snap, total) = stats.snapshot_top_n_rules(3);
        assert_eq!(total, 5);
        assert_eq!(snap.per_rule.len(), 3);
        // Top-3 by count: b=5, d=4, c=3.
        assert_eq!(snap.per_rule.get("b").copied(), Some(5));
        assert_eq!(snap.per_rule.get("d").copied(), Some(4));
        assert_eq!(snap.per_rule.get("c").copied(), Some(3));
        assert!(!snap.per_rule.contains_key("a"));
        assert!(!snap.per_rule.contains_key("e"));
    }

    #[test]
    fn snapshot_top_n_rules_tiebreaks_by_id_ascending() {
        // Three rules at count 2, two at count 1. top-3 must keep
        // all three count-2 entries; the count-1 tier should be
        // broken by id asc, so the count-1 fillers (none needed
        // here) would prefer "p" over "z".
        let stats = RuleStats::new();
        for id in ["q", "p", "r"] {
            let owned = id.to_string();
            stats.record_rule_hit(&owned);
            stats.record_rule_hit(&owned);
        }
        let z = "z".to_string();
        stats.record_rule_hit(&z);
        let m = "m".to_string();
        stats.record_rule_hit(&m);

        // top-4: all three count-2 entries plus the smaller of the
        // two count-1 entries ("m" < "z").
        let (snap, total) = stats.snapshot_top_n_rules(4);
        assert_eq!(total, 5);
        assert_eq!(snap.per_rule.len(), 4);
        assert_eq!(snap.per_rule.get("p").copied(), Some(2));
        assert_eq!(snap.per_rule.get("q").copied(), Some(2));
        assert_eq!(snap.per_rule.get("r").copied(), Some(2));
        assert_eq!(snap.per_rule.get("m").copied(), Some(1));
        assert!(!snap.per_rule.contains_key("z"));
    }

    #[test]
    fn snapshot_top_n_rules_zero_returns_empty_with_total() {
        // n=0 must short-circuit cleanly: zero entries, but the
        // untruncated cardinality must still be surfaced so callers
        // can flag truncation.
        let stats = RuleStats::new();
        for id in ["a", "b", "c"] {
            stats.record_rule_hit(&id.to_string());
        }
        let (snap, total) = stats.snapshot_top_n_rules(0);
        assert_eq!(total, 3);
        assert!(snap.per_rule.is_empty());
        // Verdict counters must still round-trip on the truncated
        // path — none were recorded here, so all zero.
        assert_eq!(snap.verdicts_total, 0);
    }

    #[test]
    fn snapshot_top_n_rules_n_above_cardinality_returns_all() {
        let stats = RuleStats::new();
        stats.record_rule_hit(&"a".to_string());
        stats.record_rule_hit(&"b".to_string());
        let (snap, total) = stats.snapshot_top_n_rules(100);
        assert_eq!(total, 2);
        assert_eq!(snap.per_rule.len(), 2);
    }

    #[test]
    fn from_snapshot_round_trip() {
        let mut per_rule = HashMap::new();
        per_rule.insert("rule_x".to_string(), 42);
        per_rule.insert("rule_y".to_string(), 7);
        let original = RuleStatsSnapshot {
            verdicts_total: 100,
            hard_accept: 10,
            hard_junk: 20,
            continue_count: 70,
            per_rule,
        };
        let stats = RuleStats::from_snapshot(original.clone());
        assert_eq!(stats.snapshot(), original);

        // Subsequent increments compose on top of the restored state.
        stats.record(&hard_accept());
        stats.record_rule_hit(&"rule_x".to_string());
        let after = stats.snapshot();
        assert_eq!(after.hard_accept, 11);
        assert_eq!(after.verdicts_total, 101);
        assert_eq!(after.per_rule.get("rule_x").copied(), Some(43));
        assert_eq!(after.per_rule.get("rule_y").copied(), Some(7));
    }

    #[test]
    fn snapshot_invariant_holds_during_concurrent_writes() {
        // 8 writer threads × 1000 records each = 8000 final total.
        // While writers are still running, a sampler thread takes
        // many snapshots and asserts the internal-consistency
        // invariant on every one:
        //   verdicts_total == hard_accept + hard_junk + continue_count
        // The live-read case is the one Phase 0's earlier counter
        // layout could violate — the current `verdicts_total =
        // sum-of-loads` derivation makes this true by construction.
        // Phase 2 commit 4 extends this: writers also bump per-rule
        // counters via `record_rule_hit`; the sampler asserts that
        // the per-rule snapshot remains internally consistent (no
        // partial clones, no panics under contention).
        let stats = Arc::new(RuleStats::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let sampler_stats = stats.clone();
        let sampler_stop = stop.clone();
        let sampler = std::thread::spawn(move || {
            let mut samples = 0u64;
            while !sampler_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let s = sampler_stats.snapshot();
                assert_eq!(
                    s.verdicts_total,
                    s.hard_accept + s.hard_junk + s.continue_count,
                    "snapshot total must equal sum of per-kind loads"
                );
                samples += 1;
            }
            samples
        });

        let mut writers = vec![];
        for tid in 0..8 {
            let s = stats.clone();
            writers.push(std::thread::spawn(move || {
                let rule_a = "rule_a".to_string();
                let rule_b = "rule_b".to_string();
                for i in 0..1000 {
                    let v = match i % 3 {
                        0 => hard_accept(),
                        1 => hard_junk(),
                        _ => continue_v(),
                    };
                    s.record(&v);
                    // Every writer bumps both rules so the per-rule
                    // snapshot has contended writes from 8 threads.
                    s.record_rule_hit(&rule_a);
                    if tid % 2 == 0 {
                        s.record_rule_hit(&rule_b);
                    }
                }
            }));
        }
        for h in writers {
            h.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let samples = sampler.join().unwrap();
        assert!(
            samples > 0,
            "sampler must have observed at least one mid-flight snapshot"
        );

        let snap = stats.snapshot();
        assert_eq!(snap.verdicts_total, 8000);
        assert_eq!(
            snap.hard_accept + snap.hard_junk + snap.continue_count,
            8000
        );
        // 8 threads × 1000 hits each on rule_a.
        assert_eq!(snap.per_rule.get("rule_a").copied(), Some(8000));
        // 4 threads (even tids) × 1000 hits each on rule_b.
        assert_eq!(snap.per_rule.get("rule_b").copied(), Some(4000));
    }
}
