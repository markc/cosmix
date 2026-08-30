//! Phase 8b — `SqliteCasMds::with_set_tx` + `SqliteSetTx<'a>` as
//! the cross-layer atomicity primitive.
//!
//! The acceptance bar (per the rev-4 amendment and Mark's 8b
//! scope sign-off):
//!
//!   * one `with_set_tx` body can write to a v1 per-set table
//!     (`item`, `container`, …) AND to a v1.1 per-set table
//!     (`set_state`, `set_change`) AND to the box-wide
//!     ATTACH'd `blobs_db.blob_ref` table — committed atomically.
//!   * an `Err` return from the closure rolls back *all three*
//!     layers (v1, v1.1, blobs_db) — none of the writes survive.
//!   * a panic in the closure rolls back the same three layers
//!     and leaves the per-set cache entry usable for the next
//!     caller (parking_lot Mutex doesn't poison).
//!   * the closure's return value passes through unchanged on Ok.
//!
//! These tests deliberately bypass the typed `Mds` trait —
//! `with_set_tx` is the *primitive*, not a typed accessor. 8c/8d
//! will add typed accessors as real callers arrive.

use cosmix_mds::{Mds, SetId, SqliteCasMds};
use std::panic::AssertUnwindSafe;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
}

fn fresh_set(mds: &SqliteCasMds) -> SetId {
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    s
}

/// Insert one row into each of the three layers we care about
/// (v1 `container`, v1.1 `set_change`, blobs_db `blob_ref`).
/// Mirrors the shape a real `MailStore` adapter write will take in
/// 8d. Returns the `set_change_seq` allocated.
fn write_three_layers(
    handle: &mut cosmix_mds::SqliteSetTx<'_>,
    set_id_str: &str,
    cid: &str,
    item_id: &str,
    blob_hash_hex: &str,
) -> rusqlite::Result<i64> {
    let tx = handle.tx();
    // v1 layer — container row. Columns match `schema/v1.sql` exactly;
    // tests that drift from the production schema would mask real
    // breakage, so we write a real row, not a synthetic one.
    tx.execute(
        "INSERT INTO container (id, parent_id, name, attrs, created_at) \
         VALUES (?1, NULL, ?1, '{}', 0);",
        [cid],
    )?;
    // v1.1 layer — allocate set_change_seq + insert set_change row.
    let new_seq: i64 = tx.query_row(
        "UPDATE set_state SET set_change_seq = set_change_seq + 1 \
         WHERE set_id = ?1 RETURNING set_change_seq;",
        [set_id_str],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO set_change \
         (set_change_seq, container_id, container_change_seq, item_id, kind, seq, changed_at) \
         VALUES (?1, ?2, ?1, ?3, 'ADDED', ?1, 0);",
        rusqlite::params![new_seq, cid, item_id],
    )?;
    // blobs_db layer — blob_ref row through the ATTACH.
    // Column order from `schema/blobs_v1.sql`: (hash, set_id, item_id).
    tx.execute(
        "INSERT OR IGNORE INTO blobs_db.blob_ref (hash, set_id, item_id) \
         VALUES (?1, ?2, ?3);",
        [blob_hash_hex, set_id_str, item_id],
    )?;
    Ok(new_seq)
}

/// Counts across the three layers, post-tx. Uses a fresh
/// `Connection` so we observe committed state, not transaction-local
/// state.
fn count_three_layers(mds: &SqliteCasMds, set: &SetId) -> (i64, i64, i64) {
    let (containers, set_changes) = mds_count_per_set(mds, set);
    let blob_refs = mds_count_blob_refs(mds, set);
    (containers, set_changes, blob_refs)
}

fn mds_count_per_set(mds: &SqliteCasMds, set: &SetId) -> (i64, i64) {
    // Reuse the cached per-set Connection (with blobs_db ATTACH'd) by
    // running through `with_set_tx` and *rolling back* a no-op tx.
    // This avoids opening a second handle to the same WAL file.
    let mut counts: (i64, i64) = (0, 0);
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |h| {
        let c: i64 = h
            .tx()
            .query_row("SELECT COUNT(*) FROM container", [], |r| r.get(0))
            .unwrap();
        let sc: i64 = h
            .tx()
            .query_row("SELECT COUNT(*) FROM set_change", [], |r| r.get(0))
            .unwrap();
        counts = (c, sc);
        // Force rollback so this read-only probe doesn't leave a
        // commit fingerprint.
        Err(cosmix_mds::Error::Other("probe rollback".into()))
    });
    counts
}

fn mds_count_blob_refs(mds: &SqliteCasMds, set: &SetId) -> i64 {
    let mut count: i64 = 0;
    let set_id_str = set.0.to_string();
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |h| {
        let n: i64 = h
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM blobs_db.blob_ref WHERE set_id = ?1",
                [&set_id_str],
                |r| r.get(0),
            )
            .unwrap();
        count = n;
        Err(cosmix_mds::Error::Other("probe rollback".into()))
    });
    count
}

#[test]
fn with_set_tx_commits_v1_v1_1_and_blobs_db_atomically() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let set_id_str = set.0.to_string();

    let seq = mds
        .with_set_tx(&set, |h| {
            let s =
                write_three_layers(h, &set_id_str, "c-commit-1", "i-commit-1", "deadbeef").unwrap();
            Ok(s)
        })
        .expect("with_set_tx ok");
    assert_eq!(seq, 1, "first allocation should be 1");

    let (c, sc, br) = count_three_layers(&mds, &set);
    assert_eq!(c, 1, "container row committed");
    assert_eq!(sc, 1, "set_change row committed");
    assert_eq!(br, 1, "blob_ref row committed across ATTACH");
}

#[test]
fn with_set_tx_err_rolls_back_all_three_layers() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let set_id_str = set.0.to_string();

    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        write_three_layers(h, &set_id_str, "c-err-1", "i-err-1", "cafef00d").unwrap();
        Err(cosmix_mds::Error::Other("intentional".into()))
    });
    assert!(result.is_err(), "closure error must propagate");

    let (c, sc, br) = count_three_layers(&mds, &set);
    assert_eq!(c, 0, "container row rolled back");
    assert_eq!(sc, 0, "set_change row rolled back");
    assert_eq!(br, 0, "blob_ref row rolled back across ATTACH");

    // Counter must also be unchanged — the allocator's UPDATE
    // RETURNING ran but rolled back, so the next successful tx
    // should still see seq=1.
    let next_seq = mds
        .with_set_tx(&set, |h| {
            let s = write_three_layers(h, &set_id_str, "c-after-err", "i-after-err", "f00dface")
                .unwrap();
            Ok(s)
        })
        .unwrap();
    assert_eq!(next_seq, 1, "rollback must restore counter to pre-tx state");
}

#[test]
fn with_set_tx_panic_rolls_back_all_three_layers_and_cache_remains_usable() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let set_id_str = set.0.to_string();

    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
            write_three_layers(h, &set_id_str, "c-panic-1", "i-panic-1", "abad1dea").unwrap();
            // Panic mid-tx. Transaction's Drop rolls back; parking_lot
            // Mutex doesn't poison; with_conn_mut's guard releases.
            panic!("intentional panic inside with_set_tx");
        });
    }));
    assert!(caught.is_err(), "panic must escape the closure");

    let (c, sc, br) = count_three_layers(&mds, &set);
    assert_eq!(c, 0, "container row rolled back on panic");
    assert_eq!(sc, 0, "set_change row rolled back on panic");
    assert_eq!(br, 0, "blob_ref row rolled back on panic");

    // The cache entry must remain usable — a follow-up with_set_tx
    // should succeed against the same SetId without any "poisoned"
    // / "set not found" surprise.
    let next_seq = mds
        .with_set_tx(&set, |h| {
            let s =
                write_three_layers(h, &set_id_str, "c-after-panic", "i-after-panic", "fadedbee")
                    .unwrap();
            Ok(s)
        })
        .expect("cache entry usable after panic-rollback");
    assert_eq!(next_seq, 1, "panic-rollback restored counter");
    let (c, sc, br) = count_three_layers(&mds, &set);
    assert_eq!((c, sc, br), (1, 1, 1));
}

#[test]
fn with_set_tx_passes_value_through_on_ok() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let v: String = mds
        .with_set_tx(&set, |_h| Ok("hello-from-closure".to_string()))
        .unwrap();
    assert_eq!(v, "hello-from-closure");

    let n: u64 = mds.with_set_tx(&set, |_h| Ok(42u64)).unwrap();
    assert_eq!(n, 42);
}

#[test]
fn with_set_tx_returns_set_not_found_for_unknown_set() {
    let (_d, mds) = open();
    let bogus = SetId(uuid::Uuid::now_v7());
    let err = mds
        .with_set_tx(&bogus, |_h| Ok(()))
        .expect_err("unknown set must fail");
    assert!(
        matches!(err, cosmix_mds::Error::SetNotFound(_)),
        "got {err:?}"
    );
}
