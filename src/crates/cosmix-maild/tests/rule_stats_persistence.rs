//! End-to-end persistence test for the rule-stats store.
//!
//! Spans the lifecycle the runtime wires up in production:
//!
//! 1. Open the store on a fresh temp path.
//! 2. Build a `RuleStats` from the loaded snapshot (= empty).
//! 3. Record verdicts and per-rule hits as `classify` would.
//! 4. Flush via the store's public `flush_snapshot` (the same path
//!    `flush_loop` drives every interval).
//! 5. Drop the in-memory `RuleStats` + the store handle, simulating
//!    a process restart.
//! 6. Re-open from disk, rebuild `RuleStats`, assert state is
//!    bit-identical to the pre-restart snapshot.
//! 7. Bump one more verdict and rule hit, flush, reopen, verify the
//!    counters compose on top of the restored state (no clobber, no
//!    silent reset).
//!
//! This is the integration analogue of the unit tests in
//! `rule_stats_store::tests`; those exercise the store in isolation,
//! this one exercises the store + `RuleStats` round-trip the runtime
//! actually uses.

use std::sync::Arc;

use cosmix_maild::rule_stats::RuleStats;
use cosmix_maild::rule_stats_store::RuleStatsStore;
use cosmix_maild_rules::{AcceptReason, JunkReason, RuleVerdict};
use tempfile::TempDir;

fn hard_accept() -> RuleVerdict {
    RuleVerdict::HardAccept {
        reason: AcceptReason::AllowlistSender,
        matched_rules: vec![],
    }
}

fn hard_junk() -> RuleVerdict {
    RuleVerdict::HardJunk {
        reason: JunkReason::BlocklistSender,
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

#[tokio::test]
async fn round_trip_through_restart_preserves_counters() {
    let tmp = TempDir::new().unwrap();
    // Mirrors the runtime path shape: `<rule_stats_dir>/stats.db`,
    // where `rule_stats_dir` defaults to `<var>/maild/rules/`. Tests
    // substitute a tempdir for that root.
    let db_path = tmp.path().join("maild/rules/stats.db");

    // ---- First boot: empty start, record some traffic, flush. ----
    let store = Arc::new(RuleStatsStore::open(&db_path).await.unwrap());
    let initial = store.load_snapshot().await.unwrap();
    assert_eq!(initial.verdicts_total, 0);
    assert!(initial.per_rule.is_empty());

    let stats = Arc::new(RuleStats::from_snapshot(initial));
    for _ in 0..5 {
        stats.record(&hard_accept());
    }
    for _ in 0..3 {
        stats.record(&hard_junk());
    }
    for _ in 0..2 {
        stats.record(&continue_v());
    }
    let rule_a = "rule_a".to_string();
    let rule_b = "rule_b".to_string();
    for _ in 0..7 {
        stats.record_rule_hit(&rule_a);
    }
    stats.record_rule_hit(&rule_b);

    let pre_restart = stats.snapshot();
    store.flush_snapshot(&pre_restart).await.unwrap();

    // Simulate process exit: drop in-memory state + store handle.
    drop(stats);
    drop(store);

    // ---- Second boot: re-open, rebuild RuleStats, verify. ----
    let store = Arc::new(RuleStatsStore::open(&db_path).await.unwrap());
    let restored = store.load_snapshot().await.unwrap();
    assert_eq!(
        restored, pre_restart,
        "restored snapshot must match pre-restart snapshot byte-for-byte"
    );

    let stats = Arc::new(RuleStats::from_snapshot(restored));

    // ---- Subsequent activity composes on top of restored state. ----
    stats.record(&hard_accept());
    stats.record_rule_hit(&rule_a);
    let after_more = stats.snapshot();
    assert_eq!(after_more.hard_accept, 6);
    assert_eq!(after_more.verdicts_total, 11);
    assert_eq!(after_more.per_rule.get(&rule_a).copied(), Some(8));
    assert_eq!(after_more.per_rule.get(&rule_b).copied(), Some(1));

    store.flush_snapshot(&after_more).await.unwrap();
    drop(stats);
    drop(store);

    // ---- Third boot: confirm the second-pass increments persisted. ----
    let store = RuleStatsStore::open(&db_path).await.unwrap();
    let final_snap = store.load_snapshot().await.unwrap();
    assert_eq!(final_snap, after_more);
}

#[tokio::test]
async fn missing_parent_directory_is_created() {
    let tmp = TempDir::new().unwrap();
    let nested = tmp.path().join("a/b/c/stats.db");
    assert!(!nested.parent().unwrap().exists());
    let _store = RuleStatsStore::open(&nested).await.unwrap();
    assert!(nested.parent().unwrap().is_dir());
    assert!(nested.is_file());
}
