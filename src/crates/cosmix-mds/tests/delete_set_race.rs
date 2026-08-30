//! Phase 3 sub-step 3: stale-handle race regression.
//!
//! Without the `Option<Connection>` cache shape, an in-flight
//! `add_item` could clone the per-set `Arc<Mutex<Connection>>` *before*
//! `delete_set` removes the cache entry, then lock and write a
//! phantom `blob_ref` row through the ATTACH'd `blobs_db` *after*
//! `delete_set_blobs` has already swept the set. The fix:
//! `delete_set` takes the per-set Mutex first and `take()`s the
//! `Connection` out of the inner `Option`. Any caller still holding
//! a clone of the Arc that finally acquires the Mutex sees `None`
//! and returns `SetNotFound` instead of silently writing through a
//! still-attached handle.
//!
//! This test races `add_item` against `delete_set` from a separate
//! thread. The exact interleaving is non-deterministic, but the
//! invariant the fix establishes is unconditional: after
//! `delete_set` returns, the blobs.sqlite must not retain any
//! `blob_ref` row for the deleted set, regardless of how the
//! racing add attempts landed.

use cosmix_mds::{BlobHash, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use rusqlite::{Connection, params};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let m = SqliteCasMds::open(d.path()).unwrap();
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

fn blobs_db(root: &std::path::Path) -> Connection {
    Connection::open(root.join("blobs.sqlite")).unwrap()
}

fn blob_ref_rows_for_set(c: &Connection, set: &SetId) -> i64 {
    c.query_row(
        "SELECT COUNT(*) FROM blob_ref WHERE set_id = ?1",
        params![set.0.to_string()],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
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
fn add_item_racing_delete_set_never_leaves_phantom_blob_refs() {
    let (d, mds) = open();
    let mds: Arc<dyn Mds> = Arc::new(mds);
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    // Pre-stage blob hashes so the racing thread does the minimum
    // possible between `set_conn` Arc clone and the actual
    // `arc.lock()` call inside `add_item`.
    let blobs: Vec<BlobHash> = (0..200)
        .map(|i| mds.put_blob(format!("racing #{i}").as_bytes()).unwrap())
        .collect();

    let mds_a = Arc::clone(&mds);
    let s_a = s;
    let inbox_a = inbox;
    let writer = thread::spawn(move || {
        let mut ok = 0u32;
        let mut not_found = 0u32;
        let mut other = 0u32;
        for (i, b) in blobs.iter().enumerate() {
            match mds_a.add_item(
                &s_a,
                b,
                &[Membership {
                    container: inbox_a,
                    flags: flags(),
                    added_at: i as i64 + 1,
                }],
            ) {
                Ok(_) => ok += 1,
                Err(cosmix_mds::Error::SetNotFound(_)) => not_found += 1,
                Err(_) => other += 1,
            }
        }
        (ok, not_found, other)
    });

    // Give the writer a moment to start landing items, then sweep.
    thread::sleep(Duration::from_micros(500));
    let report = mds.delete_set(&s).unwrap();

    let (_ok, _not_found, other) = writer.join().unwrap();
    assert_eq!(other, 0, "writer thread saw unexpected error variants");

    // Hard invariant: `delete_set` swept every blob_ref row for `s`,
    // and no racing `add_item` snuck a fresh row in afterward.
    let bdb = blobs_db(d.path());
    let leftover = blob_ref_rows_for_set(&bdb, &s);
    assert_eq!(
        leftover, 0,
        "phantom blob_ref rows for deleted set: {leftover} (\
         delete_set reported {} unreffed, writer ok={_ok} not_found={_not_found})",
        report.blob_count_unreffed,
    );

    // After delete_set, fresh add_item attempts must reject cleanly.
    let post_blob = mds.put_blob(b"post-delete attempt").unwrap();
    let err = mds
        .add_item(
            &s,
            &post_blob,
            &[Membership {
                container: inbox,
                flags: flags(),
                added_at: 1,
            }],
        )
        .unwrap_err();
    assert!(
        matches!(err, cosmix_mds::Error::SetNotFound(_)),
        "post-delete add_item returned {err:?}, expected SetNotFound",
    );

    // Refcount of the post-delete blob is unaffected (no ref ever
    // recorded). It may not even have a blob row yet since add_item
    // is what upserts it.
    assert!(refcount(&bdb, &post_blob).is_none() || refcount(&bdb, &post_blob) == Some(0));
}

#[test]
fn delete_set_blocks_until_in_flight_add_item_commits() {
    // Sequenced version: a concrete add_item commits *before*
    // delete_set runs, then delete_set sweeps that committed row.
    // No race window here — this just locks down the sequential
    // contract that the race test relies on.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"sequential commit before delete").unwrap();

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

    let bdb = blobs_db(d.path());
    assert_eq!(blob_ref_rows_for_set(&bdb, &s), 1);
    assert_eq!(refcount(&bdb, &blob), Some(1));

    let report = mds.delete_set(&s).unwrap();
    assert_eq!(report.blob_count_unreffed, 1);

    let bdb = blobs_db(d.path());
    assert_eq!(blob_ref_rows_for_set(&bdb, &s), 0);
    assert_eq!(refcount(&bdb, &blob), Some(0));
}
