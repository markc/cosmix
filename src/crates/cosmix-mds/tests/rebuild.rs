//! Integration tests for `Mds::rebuild_index`.
//!
//! Phase 5 ships the library impl alongside the `cosmix-mds
//! rebuild-index` CLI. CLI shape is covered in `tests/cli.rs`.
//! These tests pin the library contract: counters reflect every
//! per-set data.sqlite, blob/blob_ref are reconstructed from
//! item rows, blob_verify rows are preserved, and orphan CAS
//! files are counted.

use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use rusqlite::Connection;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
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

#[test]
fn rebuild_on_empty_root_reports_zero_counters() {
    let (_d, mds) = open();
    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 0);
    assert_eq!(r.items_indexed, 0);
    assert_eq!(r.blobs_indexed, 0);
    assert_eq!(r.orphan_blobs_found, 0);
}

#[test]
fn rebuild_counts_items_and_blobs_across_sets() {
    let (_d, mds) = open();
    let s1 = fresh_set();
    let s2 = fresh_set();
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    let c1 = mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    let c2 = mds.create_container(&s2, None, "INBOX", attrs()).unwrap();

    let b1 = mds.put_blob(b"alpha").unwrap();
    let b2 = mds.put_blob(b"bravo").unwrap();
    let b3 = mds.put_blob(b"charlie").unwrap();

    // s1: two items referencing distinct blobs.
    for h in [&b1, &b2] {
        mds.add_item(
            &s1,
            h,
            &[Membership {
                container: c1,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    }
    // s2: one item.
    mds.add_item(
        &s2,
        &b3,
        &[Membership {
            container: c2,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 2);
    assert_eq!(r.items_indexed, 3);
    assert_eq!(r.blobs_indexed, 3);
    assert_eq!(r.orphan_blobs_found, 0);
}

#[test]
fn rebuild_dedupes_shared_blob_within_set() {
    // Two items with identical bytes ⇒ one blob, two blob_ref rows
    // (one per item), refcount = 2.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"shared").unwrap();
    for _ in 0..2 {
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    }

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 2);
    assert_eq!(r.blobs_indexed, 1);
    assert_eq!(r.orphan_blobs_found, 0);

    // Cross-check the rebuilt refcount + blob_ref shape.
    drop(mds);
    let conn = Connection::open(d.path().join("blobs.sqlite")).unwrap();
    let rc: i64 = conn
        .query_row("SELECT refcount FROM blob", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rc, 2);
    let n_refs: i64 = conn
        .query_row("SELECT COUNT(*) FROM blob_ref", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_refs, 2);
}

#[test]
fn rebuild_recovers_from_corrupted_blobs_sqlite() {
    // Build a populated set, then deliberately wipe blob/blob_ref
    // and stuff garbage rows. rebuild-index must restore the
    // truth from data.sqlite.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"truth").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: c,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    {
        let conn = Connection::open(d.path().join("blobs.sqlite")).unwrap();
        conn.execute("DELETE FROM blob", []).unwrap();
        conn.execute("DELETE FROM blob_ref", []).unwrap();
        conn.execute(
            "INSERT INTO blob (hash, size_bytes, first_seen, last_seen, refcount) \
             VALUES ('deadbeef', 999, 1, 1, 99)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blob_ref (hash, set_id, item_id) \
             VALUES ('deadbeef', 'bogus-set', 'bogus-item')",
            [],
        )
        .unwrap();
    }

    let mds = SqliteCasMds::open(d.path()).unwrap();
    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 1);
    assert_eq!(r.blobs_indexed, 1);

    // Garbage row is gone; only the real blob survived.
    drop(mds);
    let conn = Connection::open(d.path().join("blobs.sqlite")).unwrap();
    let n_blob: i64 = conn
        .query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_blob, 1);
    let bad: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blob WHERE hash = 'deadbeef'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bad, 0);
    let n_ref: i64 = conn
        .query_row("SELECT COUNT(*) FROM blob_ref", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_ref, 1);
}

#[test]
fn rebuild_preserves_blob_verify_rows() {
    // Simulate prior verify pass that wrote blob_verify entries.
    // rebuild-index must leave them alone — last verify time and
    // status are not derivable and are operationally meaningful.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"verified").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: c,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    // Run verify so blob_verify gets a row.
    mds.verify_blobs(cosmix_mds::VerifyScope::Full).unwrap();
    drop(mds);

    let blobs_db = d.path().join("blobs.sqlite");
    let before: (String, i64, i64) = {
        let conn = Connection::open(&blobs_db).unwrap();
        conn.query_row(
            "SELECT hash, last_verified_at, status FROM blob_verify",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap()
    };

    let mds = SqliteCasMds::open(d.path()).unwrap();
    mds.rebuild_index().unwrap();
    drop(mds);

    let after: (String, i64, i64) = {
        let conn = Connection::open(&blobs_db).unwrap();
        conn.query_row(
            "SELECT hash, last_verified_at, status FROM blob_verify",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(before, after);
}

#[test]
fn rebuild_indexes_truth_when_cas_file_is_missing() {
    // Separation of concerns: rebuild reconstructs the index from
    // data.sqlite (truth), it does NOT verify that referenced CAS
    // bytes are still on disk. A blob whose CAS file vanished
    // still gets a blob row and a blob_ref — `verify_blobs` is
    // what reports the missing bytes (status = 2).
    //
    // It also must NOT inflate `orphan_blobs_found`: orphan means
    // "CAS file with no item referencing it," not "item with no
    // CAS file."
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"will be deleted").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: c,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    // Delete the CAS file. data.sqlite still references the hash.
    let hex: String = blob.0.iter().map(|b| format!("{b:02x}")).collect();
    let path = d
        .path()
        .join("blobs")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(&hex);
    std::fs::remove_file(&path).unwrap();

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 1);
    assert_eq!(r.blobs_indexed, 1);
    assert_eq!(r.orphan_blobs_found, 0, "missing-CAS-file is not an orphan",);

    // The rebuilt index should reflect data.sqlite verbatim: blob
    // row exists for the hash with refcount 1, blob_ref row exists.
    drop(mds);
    let conn = Connection::open(d.path().join("blobs.sqlite")).unwrap();
    let n_blob: i64 = conn
        .query_row("SELECT COUNT(*) FROM blob WHERE hash = ?1", [&hex], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n_blob, 1);
    let rc: i64 = conn
        .query_row("SELECT refcount FROM blob WHERE hash = ?1", [&hex], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rc, 1);
    let n_ref: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blob_ref WHERE hash = ?1",
            [&hex],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_ref, 1);

    // Verify follows up and reports the missing-bytes finding —
    // the separation rebuild relies on actually works.
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let report = mds.verify_blobs(cosmix_mds::VerifyScope::Full).unwrap();
    assert_eq!(report.blobs_checked, 1);
    assert_eq!(report.mismatches_missing, 1);
    assert_eq!(report.mismatches_hash, 0);
}

#[test]
fn rebuild_orphan_count_uses_distinct_hashes_under_dedup() {
    // Combine two failure modes in one fixture so the
    // orphan_blobs_found arithmetic stays honest under dedup:
    //
    //   - Two items in the same set share a single blob (one CAS
    //     file, one blob row, refcount=2). If `blobs_indexed`
    //     accidentally counted item-rows instead of distinct
    //     blobs, the orphan formula would underflow / report -1.
    //   - One unrelated CAS file with no item referencing it.
    //
    // Expected: blobs_indexed = 1 (distinct), orphan_blobs_found = 1.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"shared content").unwrap();
    for _ in 0..2 {
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    }

    // Stage one orphan CAS file (no item references it).
    let orphan_hex = "f".repeat(64);
    let orphan_dir = d
        .path()
        .join("blobs")
        .join(&orphan_hex[0..2])
        .join(&orphan_hex[2..4]);
    std::fs::create_dir_all(&orphan_dir).unwrap();
    std::fs::write(orphan_dir.join(&orphan_hex), b"unreferenced").unwrap();

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 2);
    assert_eq!(
        r.blobs_indexed, 1,
        "blobs_indexed must count distinct hashes"
    );
    assert_eq!(r.orphan_blobs_found, 1);
}

#[test]
fn rebuild_orphan_count_does_not_cancel_against_missing_referenced_file() {
    // Regression for the aggregate-subtraction bug: one item
    // references hash A whose CAS file is missing, plus one
    // unrelated orphan file B. Naive
    // `cas_count - blobs_indexed` = 1 - 1 = 0, and B goes
    // unreported. The fix walks each CAS file and checks
    // membership against the rebuilt blob table directly.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let referenced = mds.put_blob(b"will go missing").unwrap();
    mds.add_item(
        &s,
        &referenced,
        &[Membership {
            container: c,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    // Delete the CAS file backing the only referenced item.
    let ref_hex: String = referenced.0.iter().map(|b| format!("{b:02x}")).collect();
    let ref_path = d
        .path()
        .join("blobs")
        .join(&ref_hex[0..2])
        .join(&ref_hex[2..4])
        .join(&ref_hex);
    std::fs::remove_file(&ref_path).unwrap();

    // Stage one unrelated orphan file under a different shard so
    // the two failure modes compose without sharing any path.
    let orphan_hex = "f".repeat(64);
    let orphan_dir = d
        .path()
        .join("blobs")
        .join(&orphan_hex[0..2])
        .join(&orphan_hex[2..4]);
    std::fs::create_dir_all(&orphan_dir).unwrap();
    std::fs::write(orphan_dir.join(&orphan_hex), b"unreferenced").unwrap();

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 1);
    assert_eq!(r.blobs_indexed, 1);
    assert_eq!(
        r.orphan_blobs_found, 1,
        "orphan must be reported even when a referenced CAS file is also missing",
    );
}

#[test]
fn rebuild_counts_orphan_cas_files() {
    // Stage an extra file in the CAS that no item references.
    // rebuild-index should count it as an orphan but leave it
    // on disk (orphan reaping is `gc`'s job).
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"referenced").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: c,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    // Drop a bogus CAS file with a syntactically valid hash name.
    let orphan_hex = "0".repeat(64);
    let orphan_dir = d
        .path()
        .join("blobs")
        .join(&orphan_hex[0..2])
        .join(&orphan_hex[2..4]);
    std::fs::create_dir_all(&orphan_dir).unwrap();
    std::fs::write(orphan_dir.join(&orphan_hex), b"orphaned bytes").unwrap();

    let r = mds.rebuild_index().unwrap();
    assert_eq!(r.sets_scanned, 1);
    assert_eq!(r.items_indexed, 1);
    assert_eq!(r.blobs_indexed, 1);
    assert_eq!(r.orphan_blobs_found, 1);

    // Orphan file is still on disk — rebuild only reports.
    assert!(orphan_dir.join(&orphan_hex).exists());
}
