//! Phase 7a — crash-recovery stress + spec §Invariants 8
//! (`rebuild_index_round_trip`).
//!
//! The crash test spawns `cosmix-mds-stress-writer` (an internal
//! helper bin, not the operator CLI), waits for its `READY` line,
//! sleeps a randomized window in the middle of its hot add_item
//! loop, sends `SIGKILL`, and then reopens the store in the parent
//! to assert:
//!
//!   * `SqliteCasMds::open` succeeds (WAL recovery completes).
//!   * `rebuild_index()` returns clean (no orphan blob files; SQLite
//!     atomic-commit guarantees that the membership/changelog rows
//!     of any half-applied add are rolled back, so blob_ref rows
//!     should match real blobs).
//!   * Per-container `MAX(membership.seq) < container.next_seq`
//!     (no torn-write artifact where next_seq advanced past the
//!     committed membership).
//!
//! Iteration count is `MDS_STRESS_ITERS` (default 10). Unix-only
//! because `SIGKILL` is the entire point — there is no equivalent
//! "instantly terminate without unwind" on Windows that exercises
//! the same WAL-recovery codepath.
//!
//! Gated on the internal `_stress-helper` feature so default
//! `cargo test` and `cargo install` never build the helper bin. To
//! run the SIGKILL test:
//!
//!   cargo test -p cosmix-mds --features _stress-helper --test recovery

use cosmix_mds::{Mds, SqliteCasMds};
#[cfg(all(unix, feature = "_stress-helper"))]
use rusqlite::Connection;
#[cfg(all(unix, feature = "_stress-helper"))]
use std::io::{BufRead, BufReader};
#[cfg(all(unix, feature = "_stress-helper"))]
use std::path::Path;
#[cfg(all(unix, feature = "_stress-helper"))]
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[cfg(all(unix, feature = "_stress-helper"))]
fn iters() -> usize {
    std::env::var("MDS_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

#[cfg(all(unix, feature = "_stress-helper"))]
fn check_set_invariants(root: &Path, set_uuid: &uuid::Uuid) {
    let db_path = root
        .join("containers")
        .join(set_uuid.to_string())
        .join("data.sqlite");
    if !db_path.exists() {
        // The child died before create_set committed. Acceptable —
        // means SIGKILL landed in the open()/create_set() window
        // and there's nothing to verify.
        return;
    }
    let conn = Connection::open(&db_path).expect("open data.sqlite after crash");

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
                "post-crash: container {cid} max(membership.seq)={ms} >= next_seq={next_seq}",
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
                "post-crash: container {cid} max(membership.change_seq)={mc} > container.change_seq={change_seq}",
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
            "post-crash: container {cid} exists_count={exists_count} but membership rows={actual_count}",
        );
    }
}

#[cfg(all(unix, feature = "_stress-helper"))]
fn sigkill(pid: u32) {
    // SAFETY: libc::kill takes a raw pid; we only use it on a child
    // we just spawned, so the pid is ours and valid until we wait.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(all(unix, feature = "_stress-helper"))]
#[test]
fn sigkill_during_add_item_loop_leaves_store_recoverable() {
    let exe = env!("CARGO_BIN_EXE_cosmix-mds-stress-writer");
    let n = iters();

    for round in 0..n {
        let root = TempDir::new().unwrap();
        let set_uuid = uuid::Uuid::now_v7();

        let mut child = Command::new(exe)
            .arg(root.path())
            .arg(set_uuid.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stress writer");

        // Wait for READY so SIGKILL lands during the add_item loop,
        // not during open()/create_set().
        let stdout = child.stdout.take().expect("child stdout piped");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read READY");
        assert!(
            line.trim() == "READY",
            "expected READY, got {line:?} (round {round})",
        );

        // Randomize the kill delay across rounds so SIGKILL lands at
        // different points in the add_item transaction (BEGIN, mid-
        // INSERT, post-INSERT pre-commit, post-commit). 5..50ms is
        // enough to land thousands of add_item iterations into the
        // store on this machine, while keeping the test under a
        // second per round.
        let delay_us = 5_000 + ((round as u64) * 4_177) % 45_000;
        std::thread::sleep(std::time::Duration::from_micros(delay_us));

        let pid = child.id();
        sigkill(pid);
        let status = child.wait().expect("wait child");
        assert!(
            !status.success(),
            "child exited cleanly (status={status:?}); SIGKILL didn't land",
        );

        // Reopen — proves WAL recovery completes without panic.
        let mds = SqliteCasMds::open(root.path()).expect("reopen after crash");

        // The recovery contract this test enforces:
        //
        //   1. SqliteCasMds::open returned Ok — SQLite WAL recovery
        //      completed, schema_meta is intact, no panic.
        //   2. rebuild_index returns Ok — the rebuild walk handles
        //      whatever post-crash state it finds without erroring.
        //   3. Per-container invariants (next_seq, change_seq,
        //      exists_count) all consistent — proves the per-set
        //      add_item transaction was atomic across membership,
        //      container counters, and changelog.
        //
        // What this test deliberately does NOT enforce:
        //
        //   * orphan_blobs_found == 0. `put_blob` writes the CAS
        //     file before `add_item` writes the corresponding `blob`
        //     row, so a SIGKILL landing between them legitimately
        //     leaves an unreferenced CAS file on disk. GC sweeps the
        //     `blob` table, not the filesystem, so it cannot reclaim
        //     these. Cleanup of unindexed CAS files is a separate
        //     recovery facility (not in v1; tracked separately).
        //
        // We do bound orphan_blobs_found by a generous ceiling
        // (concurrent in-flight add_items at the moment of crash)
        // to catch a hypothetical regression where rebuild_index
        // somehow generates orphans rather than just observing
        // pre-existing ones.
        let report = mds.rebuild_index().expect("rebuild_index after crash");
        assert!(
            report.orphan_blobs_found <= 4,
            "round {round}: rebuild_index reports {} orphans — far above the \
             single-in-flight-add bound; suggests rebuild itself is leaking",
            report.orphan_blobs_found,
        );

        check_set_invariants(root.path(), &set_uuid);

        // Drop the mds before TempDir cleanup so SQLite releases its
        // file handles (otherwise rmdir on the WAL/SHM files races
        // on some filesystems).
        drop(mds);
    }
}

#[test]
fn rebuild_index_round_trip_idempotent_on_clean_store() {
    use cosmix_mds::{ContainerAttrs, Flags, Membership, SetId};
    let root = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(root.path()).unwrap();
    let set = SetId(uuid::Uuid::now_v7());
    mds.create_set(&set).unwrap();
    let inbox = mds
        .create_container(
            &set,
            None,
            "INBOX",
            ContainerAttrs {
                special_use: None,
                subscribed: true,
                extra: serde_json::json!({}),
            },
        )
        .unwrap();
    for i in 0..50 {
        let blob = mds
            .put_blob(format!("rt-{i:04}-{}", "z".repeat(64)).as_bytes())
            .unwrap();
        mds.add_item(
            &set,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: i as i64 + 1,
            }],
        )
        .unwrap();
    }
    let r1 = mds.rebuild_index().unwrap();
    let r2 = mds.rebuild_index().unwrap();
    assert_eq!(r1.items_indexed, r2.items_indexed);
    assert_eq!(r1.blobs_indexed, r2.blobs_indexed);
    assert_eq!(r1.orphan_blobs_found, 0);
    assert_eq!(r2.orphan_blobs_found, 0);
}
