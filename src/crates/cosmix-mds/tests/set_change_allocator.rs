//! Phase 8a — file-backed allocator concurrency for the v1.1
//! `set_state.set_change_seq` counter.
//!
//! In-memory tests can't exercise the WAL writer-lock behaviour the
//! allocator pattern depends on. This integration test opens the
//! same per-set `data.sqlite` from N independent `Connection`s, each
//! racing `BEGIN IMMEDIATE` → `UPDATE set_state SET set_change_seq =
//! set_change_seq + 1 ... RETURNING` → `INSERT INTO set_change` →
//! `COMMIT` for M iterations.
//!
//! What this test would catch:
//!
//!   * an allocator pattern that lets two transactions read the same
//!     pre-image of `set_change_seq` and write the same successor
//!     (would surface as a missing rowid or a UNIQUE/PK collision on
//!     `set_change.set_change_seq`)
//!   * a busy-handler omission that surfaces as `SQLITE_BUSY` returns
//!     leaking out under realistic write contention
//!   * a forgotten `BEGIN IMMEDIATE` (deferred transactions promote on
//!     first write and would race the upgrade)
//!
//! What this test does NOT touch: the `Mds` trait, the `SqliteCasMds`
//! connection cache, or any code outside the schema layer. The
//! allocator pattern is exercised against the raw schema so 8a can
//! ship before `with_set_tx` exists.

use cosmix_mds::{SetId, schema};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;

const NUM_THREADS: usize = 4;
const ITERS_PER_THREAD: usize = 250;
const TOTAL_ROWS: usize = NUM_THREADS * ITERS_PER_THREAD;

/// Open a per-set `data.sqlite` the way `store::open_set_db` would —
/// migrations + seed — but without involving `SqliteCasMds`. Returns
/// the path so additional bare `Connection`s can be opened against
/// the same file.
fn prepare_set_db(set: &SetId) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("data.sqlite");
    let mut conn = Connection::open(&path).expect("open");
    schema::apply_data_migrations(&mut conn).expect("apply data migrations");
    schema::seed_set_state(&conn, set).expect("seed set_state");
    drop(conn);
    (dir, path)
}

/// Configure a worker connection: WAL is already on (set by the v1
/// migration on first open), but each new `Connection` still needs
/// its own busy_timeout so `SQLITE_BUSY` is retried in the SQLite
/// core rather than bubbling up under contention.
fn open_worker(path: &PathBuf) -> Connection {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open worker");
    // 120 s, not 10: this test asserts UNIQUENESS of the allocator under
    // contention, not latency. SQLite's busy handler is backoff-and-retry
    // with no fairness, so under a loaded box (a full-workspace `cargo
    // test` alongside fleet builds, 2026-08-22) one of four writers was
    // starved past 10 s and failed with plain SQLITE_BUSY while the same
    // test finishes in 3-5 s standalone. The wait must be sized to the
    // scheduler, not to the lock. The real fix — a fair in-process write
    // gate so writers queue instead of spinning — is fleet task 45, which
    // puts this back to the production default.
    conn.busy_timeout(Duration::from_secs(120))
        .expect("busy_timeout");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("pragma fk");
    conn
}

/// One allocator + insert iteration. Mirrors the shape MDS mutations
/// will use in 8c: open IMMEDIATE, allocate, write the row, commit.
fn allocate_and_insert(
    conn: &mut Connection,
    set_id_str: &str,
    item_id: &str,
) -> rusqlite::Result<i64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Defensive self-heal: if some future opener forgot to seed,
    // first writer materialises the row. Idempotent against the
    // open-time seed.
    tx.execute(
        "INSERT OR IGNORE INTO set_state (set_id, set_change_seq) VALUES (?1, 0);",
        [set_id_str],
    )?;
    let new_seq: i64 = tx.query_row(
        "UPDATE set_state SET set_change_seq = set_change_seq + 1 \
         WHERE set_id = ?1 RETURNING set_change_seq;",
        [set_id_str],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO set_change \
         (set_change_seq, container_id, container_change_seq, item_id, kind, seq, changed_at) \
         VALUES (?1, 'c1', ?1, ?2, 'ADDED', ?1, 0);",
        rusqlite::params![new_seq, item_id],
    )?;
    tx.commit()?;
    Ok(new_seq)
}

#[test]
fn allocator_is_unique_under_concurrent_writers() {
    let set = SetId(Uuid::nil());
    let set_id_str = set.0.to_string();
    let (_dir, path) = prepare_set_db(&set);
    let path = Arc::new(path);

    let mut handles = Vec::with_capacity(NUM_THREADS);
    for tid in 0..NUM_THREADS {
        let path = Arc::clone(&path);
        let set_id_str = set_id_str.clone();
        handles.push(thread::spawn(move || {
            let mut conn = open_worker(&path);
            let mut allocated = Vec::with_capacity(ITERS_PER_THREAD);
            for i in 0..ITERS_PER_THREAD {
                let item_id = format!("t{tid}-i{i}");
                let seq = allocate_and_insert(&mut conn, &set_id_str, &item_id)
                    .expect("allocate_and_insert");
                allocated.push(seq);
            }
            allocated
        }));
    }

    let mut all = Vec::with_capacity(TOTAL_ROWS);
    for h in handles {
        all.extend(h.join().expect("thread join"));
    }

    // Every worker observed a unique seq, and the union covers 1..=N*M
    // exactly. This is the property that justifies the table existing:
    // if the allocator could drop or duplicate a row under WAL
    // contention, every downstream consumer would be wrong.
    all.sort_unstable();
    let expected: Vec<i64> = (1..=TOTAL_ROWS as i64).collect();
    assert_eq!(all, expected, "allocator produced gaps or duplicates");

    // The on-disk state agrees: TOTAL_ROWS rows in set_change with
    // matching set_change_seq values, and the singleton counter
    // sits at TOTAL_ROWS.
    let conn = Connection::open(path.as_ref()).expect("verify open");
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM set_change;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_count as usize, TOTAL_ROWS);
    let max_seq: i64 = conn
        .query_row("SELECT MAX(set_change_seq) FROM set_change;", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(max_seq as usize, TOTAL_ROWS);
    let counter: i64 = conn
        .query_row(
            "SELECT set_change_seq FROM set_state WHERE set_id = ?1;",
            [&set_id_str],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(counter as usize, TOTAL_ROWS);
}

/// Invariant guard for the defensive `INSERT OR IGNORE` branch: if a
/// future direct-`Connection` opener forgets to call
/// `seed_set_state`, the allocator must still produce a valid first
/// allocation rather than failing on the missing row.
#[test]
fn allocator_self_heals_when_seed_was_skipped() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("data.sqlite");
    let mut conn = Connection::open(&path).expect("open");
    schema::apply_data_migrations(&mut conn).expect("apply data migrations");
    // Deliberately skip seed_set_state to simulate a forgotten opener.
    let set_id_str = Uuid::nil().to_string();
    let seq = allocate_and_insert(&mut conn, &set_id_str, "i1").expect("allocate");
    assert_eq!(seq, 1);
    let counter: i64 = conn
        .query_row(
            "SELECT set_change_seq FROM set_state WHERE set_id = ?1;",
            [&set_id_str],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(counter, 1);
}
