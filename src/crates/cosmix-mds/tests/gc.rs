//! Phase 3 sub-step 3: two-pass GC.
//!
//! Tests cover the contract from `_doc/2026-04-29-cosmix-mds-plan.md`
//! § "Two-pass GC contract":
//!
//! - empty / no-candidate sweep is a fast no-op,
//! - basic collect: refcount=0 + file present → unlink + row delete,
//! - dry-run performs both passes (with re-checks) but never mutates,
//! - re-reference race: refcount goes back >0 during quiescence →
//!   `skipped_re_referenced` increments and the file survives,
//! - re-touched race: `last_seen` advances during quiescence →
//!   `skipped_re_touched` increments and the file survives,
//! - orphan recovery: row exists but file missing → `orphan_rows_swept`
//!   counts, row is deleted (post-crash recovery shape),
//! - quiescence-wait honored: a delivery commit during the wait
//!   converts a candidate into a re-reference skip,
//! - `pending_rows_observed` surfaces non-empty `refcount_pending`.
//!
//! Tests use `with_gc_quiescence(Duration::from_millis(50))` to keep
//! the suite under a second; the wait *shape* is what the contract
//! tests, not the wall-clock value.

use cosmix_mds::{BlobHash, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let m = SqliteCasMds::open(d.path())
        .unwrap()
        .with_gc_quiescence(Duration::from_millis(50));
    (d, m)
}

fn fresh_set() -> SetId {
    SetId(uuid::Uuid::now_v7())
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

fn flags() -> Flags {
    Flags(0)
}

fn hex(h: &BlobHash) -> String {
    let mut s = String::with_capacity(64);
    for b in h.0.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn blobs_db(root: &Path) -> Connection {
    Connection::open(root.join("blobs.sqlite")).unwrap()
}

fn blob_path(root: &Path, h: &BlobHash) -> std::path::PathBuf {
    let h = hex(h);
    root.join("blobs").join(&h[0..2]).join(&h[2..4]).join(h)
}

fn refcount(c: &Connection, h: &BlobHash) -> Option<i64> {
    c.query_row(
        "SELECT refcount FROM blob WHERE hash = ?1",
        params![hex(h)],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

#[test]
fn gc_with_no_candidates_is_a_fast_noop() {
    let (_d, mds) = open();
    let report = mds.gc(false).unwrap();
    assert_eq!(report.candidates_pass1, 0);
    assert_eq!(report.blobs_deleted, 0);
    assert_eq!(report.bytes_freed, 0);
    assert_eq!(report.skipped_re_referenced, 0);
    assert_eq!(report.skipped_re_touched, 0);
    assert_eq!(report.orphan_rows_swept, 0);
    assert_eq!(report.pending_rows_observed, 0);
}

#[test]
fn gc_collects_unreferenced_blob_after_set_deletion() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"target of GC").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();

    // delete_set drops refcount to 0 but does NOT unlink — that's
    // GC's job.
    let dr = mds.delete_set(&s).unwrap();
    assert_eq!(dr.blob_count_unreffed, 1);
    assert!(
        blob_path(d.path(), &blob).exists(),
        "delete_set must not unlink"
    );
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0));

    let report = mds.gc(false).unwrap();
    assert_eq!(report.candidates_pass1, 1);
    assert_eq!(report.blobs_deleted, 1);
    assert!(report.bytes_freed > 0);
    assert_eq!(report.skipped_re_referenced, 0);
    assert_eq!(report.skipped_re_touched, 0);
    assert_eq!(report.orphan_rows_swept, 0);

    assert!(!blob_path(d.path(), &blob).exists(), "GC failed to unlink");
    let bdb = blobs_db(d.path());
    assert!(refcount(&bdb, &blob).is_none(), "blob row not deleted");
}

#[test]
fn gc_dry_run_does_not_mutate() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"survives dry-run").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    mds.delete_set(&s).unwrap();

    let report = mds.gc(true).unwrap();
    assert_eq!(report.candidates_pass1, 1);
    // Dry-run still reports what *would* have been deleted.
    assert_eq!(report.blobs_deleted, 1);

    // But no on-disk or row mutation.
    assert!(blob_path(d.path(), &blob).exists(), "dry-run unlinked!");
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0), "dry-run deleted row!");
}

#[test]
fn gc_skips_blob_re_referenced_during_quiescence() {
    // Pass 1 captures the candidate; during the quiescence wait, a
    // new add_item bumps refcount back to 1; Pass 2's re-check
    // observes the bump and skips. The file survives.
    let (d, mds) = open();
    let mds = std::sync::Arc::new(mds);
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"raced re-reference").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();

    // Drop refcount to 0 by removing the only membership of the only
    // item carrying this blob.
    let item = {
        let c = blobs_db(d.path());
        c.query_row(
            "SELECT item_id FROM blob_ref WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get::<_, String>(0),
        )
        .map(|s| cosmix_mds::ItemId(uuid::Uuid::parse_str(&s).unwrap()))
        .unwrap()
    };
    mds.remove_membership(&s, &item, &inbox).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0));

    let mds_w = std::sync::Arc::clone(&mds);
    let s_w = s;
    let inbox_w = inbox;
    let waker = std::thread::spawn(move || {
        // Sleep less than the GC quiescence so the re-add lands
        // inside the wait window.
        std::thread::sleep(Duration::from_millis(15));
        let blob = mds_w.put_blob(b"raced re-reference").unwrap();
        mds_w
            .add_item(
                &s_w,
                &blob,
                &[Membership {
                    container: inbox_w,
                    flags: flags(),
                    added_at: 2,
                }],
            )
            .unwrap();
    });

    let report = mds.gc(false).unwrap();
    waker.join().unwrap();

    assert_eq!(report.candidates_pass1, 1);
    assert_eq!(
        report.skipped_re_referenced, 1,
        "expected re-reference skip; report = {report:?}",
    );
    assert_eq!(report.blobs_deleted, 0);
    assert!(
        blob_path(d.path(), &blob).exists(),
        "GC unlinked a re-referenced blob"
    );
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
}

#[test]
fn gc_skips_blob_re_touched_during_quiescence() {
    // Pass 1 captures a `last_seen=L0` candidate; during quiescence
    // a redundant `put_blob` bumps `last_seen` (the upsert in
    // `add_blob_ref_in_tx` updates last_seen on conflict — but the
    // simpler way to bump last_seen without raising refcount is to
    // touch it directly). Pass 2 observes last_seen advanced and
    // skips.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"raced touch").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    let item = {
        let c = blobs_db(d.path());
        c.query_row(
            "SELECT item_id FROM blob_ref WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get::<_, String>(0),
        )
        .map(|s| cosmix_mds::ItemId(uuid::Uuid::parse_str(&s).unwrap()))
        .unwrap()
    };
    mds.remove_membership(&s, &item, &inbox).unwrap();

    // Use a timer thread to bump last_seen mid-quiescence by writing
    // to the row directly. (We open a parallel handle to blobs.sqlite
    // because the in-process connection is locked by the GC thread.)
    let root = d.path().to_path_buf();
    let blob_hex = hex(&blob);
    let waker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(15));
        let c = Connection::open(root.join("blobs.sqlite")).unwrap();
        c.execute(
            "UPDATE blob SET last_seen = last_seen + 1000 WHERE hash = ?1",
            params![blob_hex],
        )
        .unwrap();
    });

    let report = mds.gc(false).unwrap();
    waker.join().unwrap();

    assert_eq!(report.candidates_pass1, 1);
    assert_eq!(
        report.skipped_re_touched, 1,
        "expected last_seen-advance skip; report = {report:?}",
    );
    assert_eq!(report.blobs_deleted, 0);
    assert!(
        blob_path(d.path(), &blob).exists(),
        "GC unlinked a re-touched blob"
    );
}

#[test]
fn gc_sweeps_orphan_rows_with_missing_files() {
    // Simulate post-crash recovery: a previous Pass 2 unlinked the
    // file but died before the row delete. Next sweep's Pass 2
    // observes file missing → counts as orphan_rows_swept and
    // deletes the dangling row.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"orphan recovery target").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    let item = {
        let c = blobs_db(d.path());
        c.query_row(
            "SELECT item_id FROM blob_ref WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get::<_, String>(0),
        )
        .map(|s| cosmix_mds::ItemId(uuid::Uuid::parse_str(&s).unwrap()))
        .unwrap()
    };
    mds.remove_membership(&s, &item, &inbox).unwrap();

    // Manually unlink the file to simulate the post-crash state.
    let path = blob_path(d.path(), &blob);
    assert!(path.exists());
    std::fs::remove_file(&path).unwrap();

    let report = mds.gc(false).unwrap();
    assert_eq!(report.candidates_pass1, 1);
    assert_eq!(
        report.orphan_rows_swept, 1,
        "expected orphan sweep; report = {report:?}",
    );
    assert_eq!(report.blobs_deleted, 0);

    // Row deleted; bytes_freed not incremented (we did not unlink
    // anything *this* sweep).
    let bdb = blobs_db(d.path());
    assert!(refcount(&bdb, &blob).is_none(), "dangling row not swept");
    assert_eq!(report.bytes_freed, 0);
}

#[test]
fn gc_dry_run_does_not_delete_orphan_rows() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"dry-run orphan").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    let item = {
        let c = blobs_db(d.path());
        c.query_row(
            "SELECT item_id FROM blob_ref WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get::<_, String>(0),
        )
        .map(|s| cosmix_mds::ItemId(uuid::Uuid::parse_str(&s).unwrap()))
        .unwrap()
    };
    mds.remove_membership(&s, &item, &inbox).unwrap();
    std::fs::remove_file(blob_path(d.path(), &blob)).unwrap();

    let report = mds.gc(true).unwrap();
    assert_eq!(report.orphan_rows_swept, 1);

    // Dry-run: row must still be there.
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0), "dry-run swept the row!");
}

#[test]
fn pending_rows_observed_surfaces_non_empty_refcount_pending() {
    let (d, mds) = open();
    // Inject two synthetic refcount_pending rows directly. v1 has
    // no legitimate producer, so this is exactly the "future
    // maintainer wired a producer" canary the warn log + counter
    // exist for. Schema is `(seq AUTOINCREMENT, op INT, hash TEXT,
    // set_id TEXT, item_id TEXT, queued_at INT)` per
    // `src/schema/blobs_v1.sql`.
    let c = blobs_db(d.path());
    let now = 1_700_000_000_000_i64;
    let zero_hash = "0".repeat(64);
    let zero_uuid = "00000000-0000-0000-0000-000000000000";
    c.execute(
        "INSERT INTO refcount_pending (op, hash, set_id, item_id, queued_at) \
         VALUES (1, ?1, ?2, ?2, ?3), (-1, ?1, ?2, ?2, ?3)",
        params![zero_hash, zero_uuid, now],
    )
    .expect("inject refcount_pending rows");
    drop(c);

    let report = mds.gc(false).unwrap();
    assert_eq!(
        report.pending_rows_observed, 2,
        "refcount_pending sanity counter did not surface the injected rows; \
         report = {report:?}",
    );
}
