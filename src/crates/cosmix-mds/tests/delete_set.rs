//! Phase 3 sub-step 2: `delete_set` distinct-hash tally.
//!
//! `DeleteReport.blob_count_unreffed` is the count of *distinct
//! blob hashes* whose refcount transitioned all the way to zero —
//! NOT the count of `blob_ref` rows removed. A single set can carry
//! many blob_ref rows for the same hash (one per item with that
//! content), and a hash that another set still pins must not count
//! toward the tally even though its refcount drops.

use cosmix_mds::{BlobHash, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use rusqlite::{Connection, params};
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

fn refcount(c: &Connection, h: &BlobHash) -> Option<i64> {
    c.query_row(
        "SELECT refcount FROM blob WHERE hash = ?1",
        params![hex(h)],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

fn ref_count_rows(c: &Connection, set: &SetId) -> i64 {
    c.query_row(
        "SELECT COUNT(*) FROM blob_ref WHERE set_id = ?1",
        params![set.0.to_string()],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
}

#[test]
fn delete_set_with_three_distinct_blobs_reports_three_unreffed() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    let blobs: Vec<BlobHash> = (0..3)
        .map(|i| {
            mds.put_blob(format!("distinct content #{i}").as_bytes())
                .unwrap()
        })
        .collect();
    for b in &blobs {
        mds.add_item(
            &s,
            b,
            &[Membership {
                container: inbox,
                flags: flags(),
                added_at: 1,
            }],
        )
        .unwrap();
    }

    let report = mds.delete_set(&s).unwrap();
    assert_eq!(report.blob_count_unreffed, 3);

    let bdb = blobs_db(d.path());
    assert_eq!(ref_count_rows(&bdb, &s), 0);
    for b in &blobs {
        assert_eq!(refcount(&bdb, b), Some(0));
    }
}

#[test]
fn delete_set_does_not_count_hashes_pinned_by_another_set() {
    // Two sets share identical content. Deleting the first set must
    // report 0 unreffed (the other set still pins it). Deleting the
    // second set must then report 1.
    let (d, mds) = open();
    let s_a = fresh_set();
    let s_b = fresh_set();
    mds.create_set(&s_a).unwrap();
    mds.create_set(&s_b).unwrap();
    let inbox_a = mds.create_container(&s_a, None, "INBOX", attrs()).unwrap();
    let inbox_b = mds.create_container(&s_b, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"shared across sets").unwrap();

    mds.add_item(
        &s_a,
        &blob,
        &[Membership {
            container: inbox_a,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    mds.add_item(
        &s_b,
        &blob,
        &[Membership {
            container: inbox_b,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(2));

    let r_a = mds.delete_set(&s_a).unwrap();
    assert_eq!(r_a.blob_count_unreffed, 0);
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
    assert_eq!(ref_count_rows(&bdb, &s_a), 0);
    assert_eq!(ref_count_rows(&bdb, &s_b), 1);

    let r_b = mds.delete_set(&s_b).unwrap();
    assert_eq!(r_b.blob_count_unreffed, 1);
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0));
    assert_eq!(ref_count_rows(&bdb, &s_b), 0);
}

#[test]
fn delete_set_with_intra_set_duplicates_collapses_to_distinct_hash() {
    // Same set holds two distinct items with identical content
    // (refcount=2, two blob_ref rows). delete_set must report 1
    // unreffed hash, not 2.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob_1 = mds.put_blob(b"intra-set duplicate").unwrap();
    let blob_2 = mds.put_blob(b"intra-set duplicate").unwrap();
    assert_eq!(blob_1, blob_2);

    mds.add_item(
        &s,
        &blob_1,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    mds.add_item(
        &s,
        &blob_2,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 2,
        }],
    )
    .unwrap();

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob_1), Some(2));
    assert_eq!(ref_count_rows(&bdb, &s), 2);

    let report = mds.delete_set(&s).unwrap();
    assert_eq!(report.blob_count_unreffed, 1);

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob_1), Some(0));
    assert_eq!(ref_count_rows(&bdb, &s), 0);
}

#[test]
fn delete_set_partial_overlap_with_other_set() {
    // Set A: blobs [X, Y]. Set B: blobs [Y, Z]. Deleting A drops
    // X (unique to A) and decrements Y. Tally must be 1.
    let (d, mds) = open();
    let s_a = fresh_set();
    let s_b = fresh_set();
    mds.create_set(&s_a).unwrap();
    mds.create_set(&s_b).unwrap();
    let inbox_a = mds.create_container(&s_a, None, "INBOX", attrs()).unwrap();
    let inbox_b = mds.create_container(&s_b, None, "INBOX", attrs()).unwrap();
    let x = mds.put_blob(b"only-in-A").unwrap();
    let y = mds.put_blob(b"in-both").unwrap();
    let z = mds.put_blob(b"only-in-B").unwrap();

    for (set, container, blob) in [
        (&s_a, &inbox_a, &x),
        (&s_a, &inbox_a, &y),
        (&s_b, &inbox_b, &y),
        (&s_b, &inbox_b, &z),
    ] {
        mds.add_item(
            set,
            blob,
            &[Membership {
                container: *container,
                flags: flags(),
                added_at: 1,
            }],
        )
        .unwrap();
    }

    let report = mds.delete_set(&s_a).unwrap();
    assert_eq!(report.blob_count_unreffed, 1);

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &x), Some(0));
    assert_eq!(refcount(&bdb, &y), Some(1));
    assert_eq!(refcount(&bdb, &z), Some(1));
    assert_eq!(ref_count_rows(&bdb, &s_a), 0);
    assert_eq!(ref_count_rows(&bdb, &s_b), 2);
}

#[test]
fn delete_empty_set_reports_zero() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();

    let report = mds.delete_set(&s).unwrap();
    assert_eq!(report.blob_count_unreffed, 0);

    let bdb = blobs_db(d.path());
    assert_eq!(ref_count_rows(&bdb, &s), 0);
}
