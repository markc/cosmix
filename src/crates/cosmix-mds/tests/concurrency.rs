//! Phase 7a — concurrency stress.
//!
//! 32 OS threads racing add/copy/move/flag/remove against a shared
//! `Arc<SqliteCasMds>` spread over 10 sets. Per-set writes are
//! serialized by the connection mutex inside `SqliteCasMds`; this
//! test exists to (a) prove the locking discipline doesn't deadlock
//! under load, (b) exercise the cross-set parallelism path
//! (different sets touch different mutexes), and (c) catch any
//! invariant breakage that only surfaces under contention.
//!
//! Per-round, after all threads join, the test scans each set's
//! `data.sqlite` directly to assert:
//!
//!   * `MAX(membership.seq) < container.next_seq` per container
//!     (UID monotonicity — also enforced by the unique index, so the
//!     real bug class is "next_seq advanced past where membership
//!     ended up", not duplicates)
//!   * `MAX(membership.change_seq) <= container.change_seq`
//!     (MODSEQ monotonicity)
//!   * `exists_count` matches `COUNT(membership)` per container
//!   * `MAX(container_change.change_seq) <= container.change_seq`
//!   * `rebuild_index()` returns clean (no orphans, idempotent on
//!     second pass)
//!
//! `rebuild_index` runs once per round, not per thread iteration —
//! it locks every set's connection and would dominate runtime if
//! interleaved.
//!
//! Round count is `MDS_STRESS_ITERS` (default 2) so CI gets bounded,
//! deterministic contention while local pressure-testing can crank it.

#[path = "../../../test-support/load_guard.rs"]
mod load_guard;

use cosmix_mds::{
    BlobHash, ContainerAttrs, ContainerId, Flags, Mds, Membership, SetId, SqliteCasMds,
};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

const NUM_SETS: usize = 10;
const NUM_THREADS: usize = 32;
const OPS_PER_THREAD: usize = 20;
const NUM_BLOBS: usize = 16;
const WRITE_GATE_TIMEOUT: &str = "waiting for in-process write gate";
const CORRECTNESS_GATE_TIMEOUT: Duration = Duration::from_secs(60);
const TIMING_GATE_TIMEOUT: Duration = Duration::from_secs(5);

fn iters() -> usize {
    std::env::var("MDS_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2)
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

/// Get the correctness gate timeout for the in-process write gate.
///
/// The timeout stays independent of load while the test is runnable. Host load
/// is handled before the test starts: once wall-clock timing is not meaningful,
/// the whole fixed-wait test is skipped instead of inventing a larger deadline
/// that can still misclassify scheduler starvation as an MDS failure.
fn correctness_gate_timeout() -> Duration {
    CORRECTNESS_GATE_TIMEOUT
}

/// These tests prove concurrency invariants, but their only way to distinguish
/// a stuck write gate from a descheduled holder is a wall-clock deadline. Use
/// the shared timing guard for the deadline as well as the timing assertion.
fn skip_for_host_load(load: load_guard::LoadSample, test_name: &str) -> bool {
    if load_guard::should_assert_timing(load) {
        return false;
    }

    println!(
        "WARNING: loadavg1 {:.2} on {} available CPUs ({:.2} per CPU); \
         skipping {test_name} because its fixed write-gate waits cannot \
         distinguish a scheduler stall from an MDS hang",
        load.load1,
        load.parallelism,
        load.load_per_cpu()
    );
    true
}

struct Setup {
    _root: TempDir,
    root_path: PathBuf,
    mds: Arc<SqliteCasMds>,
    sets: Vec<SetId>,
    containers: HashMap<SetId, (ContainerId, ContainerId)>,
    blobs: Vec<BlobHash>,
}

fn setup_round(gate_timeout: Duration) -> Setup {
    let root = TempDir::new().unwrap();
    let root_path = root.path().to_path_buf();
    // The caller chooses either the generous correctness wait or the
    // production-shaped timing wait through the task-45 test knob.
    let mds = Arc::new(SqliteCasMds::open_with_busy_timeout(&root_path, gate_timeout).unwrap());

    let mut sets = Vec::with_capacity(NUM_SETS);
    let mut containers = HashMap::new();
    for _ in 0..NUM_SETS {
        let s = SetId(uuid::Uuid::now_v7());
        mds.create_set(&s).unwrap();
        let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
        let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
        sets.push(s);
        containers.insert(s, (inbox, archive));
    }

    let mut blobs = Vec::with_capacity(NUM_BLOBS);
    for i in 0..NUM_BLOBS as u32 {
        let payload = format!("stress-blob-{i:08}-{}", "x".repeat(64 + (i as usize % 32)));
        blobs.push(mds.put_blob(payload.as_bytes()).unwrap());
    }

    Setup {
        _root: root,
        root_path,
        mds,
        sets,
        containers,
        blobs,
    }
}

/// xorshift32 — deterministic per-thread randomness without pulling
/// `rand` into dev-deps. Seeded from thread index + iteration index
/// so reproducing a flake is deterministic.
fn lcg_next(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

#[derive(Debug, Default)]
struct WorkerReport {
    adds: usize,
    copies: usize,
    moves: usize,
    flags: usize,
    removes: usize,
    gate_retries: usize,
}

#[derive(Clone, Copy)]
enum GateTimeoutPolicy {
    Fail,
    RetryOnce,
}

impl WorkerReport {
    fn merge(&mut self, other: Self) {
        self.adds += other.adds;
        self.copies += other.copies;
        self.moves += other.moves;
        self.flags += other.flags;
        self.removes += other.removes;
        self.gate_retries += other.gate_retries;
    }
}

/// Retry one in-process gate timeout. Gate acquisition happens before
/// the mutation closure starts, so this cannot repeat a partially
/// applied operation. A stuck gate still fails after the second
/// production-sized wait instead of being hidden by a long test-only
/// timeout.
fn retry_gate_once<T>(
    mut operation: impl FnMut() -> Result<T, cosmix_mds::Error>,
) -> (Result<T, cosmix_mds::Error>, bool) {
    match operation() {
        Err(cosmix_mds::Error::Other(msg)) if msg.contains(WRITE_GATE_TIMEOUT) => {
            thread::yield_now();
            (operation(), true)
        }
        result => (result, false),
    }
}

fn run_gate_operation<T>(
    policy: GateTimeoutPolicy,
    mut operation: impl FnMut() -> Result<T, cosmix_mds::Error>,
) -> (Result<T, cosmix_mds::Error>, bool) {
    match policy {
        GateTimeoutPolicy::Fail => (operation(), false),
        GateTimeoutPolicy::RetryOnce => retry_gate_once(operation),
    }
}

fn worker(
    setup: &Setup,
    thread_idx: usize,
    round: usize,
    added_at: &AtomicU64,
    gate_timeout_policy: GateTimeoutPolicy,
) -> WorkerReport {
    let mut rng = ((thread_idx as u32).wrapping_mul(0x9E37_79B9))
        ^ ((round as u32).wrapping_mul(0x85EB_CA6B))
        ^ 0xDEAD_BEEF;
    if rng == 0 {
        rng = 1;
    }
    let mds = &setup.mds;
    let mut report = WorkerReport::default();

    // Each thread keeps its own list of items it added so it can
    // copy/move/flag/remove things it owns. NotFound / AlreadyExists
    // from cross-thread interleaving are treated as benign — another
    // worker may have moved or removed the same row first.
    let mut my_items: Vec<(SetId, ContainerId, cosmix_mds::ItemId)> = Vec::new();

    for _ in 0..OPS_PER_THREAD {
        let r = lcg_next(&mut rng);
        let set_idx = (r as usize) % setup.sets.len();
        let set = setup.sets[set_idx];
        let (inbox, archive) = setup.containers[&set];
        let pick_src = (r >> 8) & 1 == 0;
        let src = if pick_src { inbox } else { archive };

        let op = (r >> 16) % 100;

        if op < 40 || my_items.is_empty() {
            report.adds += 1;
            let blob = &setup.blobs[(r as usize >> 24) % setup.blobs.len()];
            let added = added_at.fetch_add(1, Ordering::Relaxed) as i64;
            let memberships = vec![Membership {
                container: src,
                flags: Flags(0),
                added_at: added,
            }];
            let (result, retried) = run_gate_operation(gate_timeout_policy, || {
                mds.add_item(&set, blob, &memberships)
            });
            report.gate_retries += usize::from(retried);
            match result {
                Ok(report) => {
                    my_items.push((set, src, report.item_id));
                }
                Err(e) => panic!("add_item failed: {e:?}"),
            }
        } else if op < 60 {
            report.copies += 1;
            let pick = (r as usize >> 24) % my_items.len();
            let (s, src_c, item) = my_items[pick];
            let (s_inbox, s_archive) = setup.containers[&s];
            let dst_c = if src_c == s_inbox { s_archive } else { s_inbox };
            let (result, retried) = run_gate_operation(gate_timeout_policy, || {
                mds.copy_item(&s, &item, &dst_c, Flags(0))
            });
            report.gate_retries += usize::from(retried);
            match result {
                Ok(_) => {}
                Err(cosmix_mds::Error::ItemNotFound(_)) => {}
                Err(cosmix_mds::Error::Other(msg)) if msg.contains("already in container") => {}
                Err(e) => panic!("copy_item failed: {e:?}"),
            }
        } else if op < 75 {
            report.moves += 1;
            let pick = (r as usize >> 24) % my_items.len();
            let (s, src_c, item) = my_items[pick];
            let (s_inbox, s_archive) = setup.containers[&s];
            let dst_c = if src_c == s_inbox { s_archive } else { s_inbox };
            let (result, retried) = run_gate_operation(gate_timeout_policy, || {
                mds.move_item(&s, &item, &src_c, &dst_c, Flags(0))
            });
            report.gate_retries += usize::from(retried);
            match result {
                Ok(_) => {
                    my_items[pick].1 = dst_c;
                }
                Err(cosmix_mds::Error::ItemNotFound(_)) => {}
                Err(cosmix_mds::Error::Other(msg))
                    if msg.contains("not in source container")
                        || msg.contains("already in destination container") => {}
                Err(e) => panic!("move_item failed: {e:?}"),
            }
        } else if op < 90 {
            report.flags += 1;
            let pick = (r as usize >> 24) % my_items.len();
            let (s, src_c, item) = my_items[pick];
            let f = Flags((r >> 12) & 0x1F);
            let (result, retried) = run_gate_operation(gate_timeout_policy, || {
                mds.store_flags(&s, &item, &src_c, f)
            });
            report.gate_retries += usize::from(retried);
            match result {
                Ok(_) => {}
                Err(cosmix_mds::Error::ItemNotFound(_)) => {}
                Err(cosmix_mds::Error::Other(msg)) if msg.contains("not in container") => {}
                Err(e) => panic!("store_flags failed: {e:?}"),
            }
        } else {
            report.removes += 1;
            let pick = (r as usize >> 24) % my_items.len();
            let (s, src_c, item) = my_items.swap_remove(pick);
            let (result, retried) = run_gate_operation(gate_timeout_policy, || {
                mds.remove_membership(&s, &item, &src_c)
            });
            report.gate_retries += usize::from(retried);
            match result {
                Ok(_) => {}
                Err(cosmix_mds::Error::ItemNotFound(_)) => {}
                Err(cosmix_mds::Error::Other(msg)) if msg.contains("not in container") => {}
                Err(e) => panic!("remove_membership failed: {e:?}"),
            }
        }
    }

    report
}

fn check_invariants(setup: &Setup) {
    for set in &setup.sets {
        let db_path = setup
            .root_path
            .join("containers")
            .join(set.0.to_string())
            .join("data.sqlite");
        let conn = Connection::open(&db_path).expect("open data.sqlite");

        let mut stmt = conn
            .prepare("SELECT id, next_seq, change_seq, exists_count FROM container")
            .unwrap();
        let containers: Vec<(String, i64, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for (cid, next_seq, change_seq, exists_count) in &containers {
            let max_seq: Option<i64> = conn
                .query_row(
                    "SELECT MAX(seq) FROM membership WHERE container_id = ?1",
                    [cid],
                    |r| r.get(0),
                )
                .unwrap();
            if let Some(ms) = max_seq {
                assert!(
                    ms < *next_seq,
                    "container {cid}: max(membership.seq)={ms} >= next_seq={next_seq}",
                );
            }

            let max_change: Option<i64> = conn
                .query_row(
                    "SELECT MAX(change_seq) FROM membership WHERE container_id = ?1",
                    [cid],
                    |r| r.get(0),
                )
                .unwrap();
            if let Some(mc) = max_change {
                assert!(
                    mc <= *change_seq,
                    "container {cid}: max(membership.change_seq)={mc} > container.change_seq={change_seq}",
                );
            }

            let actual_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM membership WHERE container_id = ?1",
                    [cid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                actual_count, *exists_count,
                "container {cid}: exists_count={exists_count} but membership rows={actual_count}",
            );

            let max_log_change: Option<i64> = conn
                .query_row(
                    "SELECT MAX(change_seq) FROM container_change WHERE container_id = ?1",
                    [cid],
                    |r| r.get(0),
                )
                .unwrap();
            if let Some(mlc) = max_log_change {
                assert!(
                    mlc <= *change_seq,
                    "container {cid}: container_change max change_seq={mlc} > container.change_seq={change_seq}",
                );
            }
        }
    }

    let report = setup.mds.rebuild_index().unwrap();
    assert_eq!(
        report.orphan_blobs_found, 0,
        "rebuild_index found orphan blob files after round: {report:?}",
    );
    let report2 = setup.mds.rebuild_index().unwrap();
    assert_eq!(
        report2.orphan_blobs_found, 0,
        "rebuild_index second pass found orphans: {report2:?}",
    );
    assert_eq!(
        report2.items_indexed, report.items_indexed,
        "items_indexed drifted between consecutive rebuild_index passes: \
         first={}, second={}",
        report.items_indexed, report2.items_indexed,
    );
    assert_eq!(
        report2.blobs_indexed, report.blobs_indexed,
        "blobs_indexed drifted between consecutive rebuild_index passes: \
         first={}, second={}",
        report.blobs_indexed, report2.blobs_indexed,
    );
}

fn run_workers(
    setup: &Arc<Setup>,
    round: usize,
    gate_timeout_policy: GateTimeoutPolicy,
) -> WorkerReport {
    let added_at = Arc::new(AtomicU64::new(1));
    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|t| {
            let setup = Arc::clone(setup);
            let added_at = Arc::clone(&added_at);
            thread::spawn(move || worker(&setup, t, round, &added_at, gate_timeout_policy))
        })
        .collect();

    let mut report = WorkerReport::default();
    for handle in handles {
        report.merge(handle.join().expect("worker panicked"));
    }
    report
}

fn assert_all_mutations_ran(report: &WorkerReport) {
    assert!(
        report.adds > 0
            && report.copies > 0
            && report.moves > 0
            && report.flags > 0
            && report.removes > 0,
        "bounded round did not exercise every mutation path: {report:?}"
    );
}

#[test]
fn n_threads_race_add_copy_move_flag_remove_across_sets() {
    let n = iters();
    let load = load_guard::read_load_sample();
    if skip_for_host_load(load, "n_threads_race_add_copy_move_flag_remove_across_sets") {
        return;
    }
    let gate_timeout = correctness_gate_timeout();

    for round in 0..n {
        let setup = Arc::new(setup_round(gate_timeout));
        let report = run_workers(&setup, round, GateTimeoutPolicy::Fail);
        assert_all_mutations_ran(&report);
        assert_eq!(
            report.gate_retries, 0,
            "the correctness path must fail a real 60s starvation instead of retrying"
        );
        check_invariants(&setup);

        // The production-shaped 5s claim exists only in this load-gated run.
        let timing_setup = Arc::new(setup_round(TIMING_GATE_TIMEOUT));
        let timing_report = run_workers(&timing_setup, round, GateTimeoutPolicy::RetryOnce);
        assert_all_mutations_ran(&timing_report);
        assert_eq!(
            timing_report.gate_retries, 0,
            "timing assertion failed: gate timeout occurred under light load \
             (loadavg1 {:.2} on {} cpus) — \
             this suggests the 5s bound is no longer adequate for the default workload",
            load.load1, load.parallelism
        );
    }
}

#[test]
fn correctness_timeout_remains_flat_when_the_suite_runs() {
    assert_eq!(correctness_gate_timeout(), CORRECTNESS_GATE_TIMEOUT);
}

#[test]
fn concurrency_suite_skips_under_synthetic_fleet_load() {
    let observed_fleet_load = load_guard::LoadSample {
        load1: 50.37,
        parallelism: 18,
    };

    assert!(
        skip_for_host_load(observed_fleet_load, "synthetic concurrency suite"),
        "the load which produced finding 803 must skip fixed write-gate waits"
    );
    assert!(
        !skip_for_host_load(
            load_guard::LoadSample {
                load1: 0.3,
                parallelism: 8,
            },
            "synthetic concurrency suite",
        ),
        "light load must still run the full concurrency suite"
    );
}

#[test]
fn timing_assertion_gated_by_load() {
    assert!(
        load_guard::should_assert_timing(load_guard::LoadSample {
            load1: 0.3,
            parallelism: 8,
        }),
        "timing assertion should run at light load (0.3 on 8 cpus, ratio 0.0375)"
    );
    assert!(
        !load_guard::should_assert_timing(load_guard::LoadSample {
            load1: 0.5,
            parallelism: 1,
        }),
        "timing assertion should be skipped at threshold boundary (0.5 on 1 cpu, ratio 0.5)"
    );
    assert!(
        load_guard::should_assert_timing(load_guard::LoadSample {
            load1: 0.49,
            parallelism: 1,
        }),
        "timing assertion should run just below threshold (0.49 on 1 cpu, ratio 0.49)"
    );
    assert!(
        !load_guard::should_assert_timing(load_guard::LoadSample {
            load1: 20.0,
            parallelism: 18,
        }),
        "timing assertion should be skipped at heavy load (20.0 on 18 cpus, ratio 1.11)"
    );
}

#[test]
fn correctness_path_survives_scheduler_stall() {
    let load = load_guard::read_load_sample();
    if skip_for_host_load(load, "correctness_path_survives_scheduler_stall") {
        return;
    }
    let setup = Arc::new(setup_round(CORRECTNESS_GATE_TIMEOUT));
    let stalled_set = setup.sets[0];
    let stalled_mds = Arc::clone(&setup.mds);
    let (gate_held_tx, gate_held_rx) = mpsc::sync_channel(0);

    // `with_set_tx` enters the closure only after taking the write gate.
    // Sleeping there models a gate holder being descheduled for 10s: a
    // production-sized 5s waiter would go red, while the correctness path
    // must keep waiting and must not retry.
    let stall = thread::spawn(move || {
        stalled_mds.with_set_tx(&stalled_set, |_tx| {
            gate_held_tx.send(()).expect("announce held write gate");
            thread::sleep(Duration::from_secs(10));
            Ok(())
        })
    });
    gate_held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("stall thread did not acquire the write gate");

    let report = run_workers(&setup, 0, GateTimeoutPolicy::Fail);
    stall
        .join()
        .expect("stall thread panicked")
        .expect("stalled transaction failed");

    assert_all_mutations_ran(&report);
    assert_eq!(
        report.gate_retries, 0,
        "correctness run retried after the injected 10s stall"
    );
    check_invariants(&setup);
}
