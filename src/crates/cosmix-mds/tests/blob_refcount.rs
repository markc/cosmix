//! Phase 3 sub-step 1: cross-DB refcount + blob_ref bookkeeping.
//!
//! Verifies that `add_item` and `remove_membership` keep
//! `blobs.sqlite` consistent with `data.sqlite` via the per-set
//! ATTACH'd `blobs_db` write path. The bidx is rebuildable per
//! invariant 8, but Phase 3's GC trusts these refcounts to be
//! correct *as written*, so the math has to hold.

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

fn ref_count_rows(c: &Connection, h: &BlobHash) -> i64 {
    c.query_row(
        "SELECT COUNT(*) FROM blob_ref WHERE hash = ?1",
        params![hex(h)],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
}

#[test]
fn add_item_writes_one_ref_per_item_regardless_of_membership_count() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"refcount one-per-item").unwrap();

    let report = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: flags(),
                    added_at: 1,
                },
                Membership {
                    container: archive,
                    flags: flags(),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    assert_eq!(report.placements.len(), 2);

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
    assert_eq!(ref_count_rows(&bdb, &blob), 1);
    let size: i64 = bdb
        .query_row(
            "SELECT size_bytes FROM blob WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(size as u64, mds.blob_size(&blob).unwrap());
}

#[test]
fn copy_and_move_within_set_do_not_touch_refcount() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let trash = mds.create_container(&s, None, "Trash", attrs()).unwrap();
    let blob = mds.put_blob(b"copy/move stable").unwrap();

    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: flags(),
                added_at: 1,
            }],
        )
        .unwrap();
    let item = r.item_id;

    mds.copy_item(&s, &item, &archive, flags()).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
    assert_eq!(ref_count_rows(&bdb, &blob), 1);

    mds.move_item(&s, &item, &archive, &trash, flags()).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
    assert_eq!(ref_count_rows(&bdb, &blob), 1);
}

#[test]
fn remove_membership_decrements_only_when_item_orphaned() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"orphan-on-last-membership").unwrap();

    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: flags(),
                    added_at: 1,
                },
                Membership {
                    container: archive,
                    flags: flags(),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item = r.item_id;

    // Removing the first membership leaves the item alive; refcount
    // tracks items, so it must stay at 1.
    mds.remove_membership(&s, &item, &inbox).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(1));
    assert_eq!(ref_count_rows(&bdb, &blob), 1);

    // Removing the last membership orphans the item; refcount drops
    // to 0 and blob_ref is gone.
    mds.remove_membership(&s, &item, &archive).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob), Some(0));
    assert_eq!(ref_count_rows(&bdb, &blob), 0);
}

#[test]
fn duplicated_content_two_items_two_refs() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    // Two add_item calls with identical bytes → same blob, two
    // distinct items, refcount=2, two blob_ref rows.
    let blob_a = mds.put_blob(b"identical content").unwrap();
    let blob_b = mds.put_blob(b"identical content").unwrap();
    assert_eq!(blob_a, blob_b);

    let r1 = mds
        .add_item(
            &s,
            &blob_a,
            &[Membership {
                container: inbox,
                flags: flags(),
                added_at: 1,
            }],
        )
        .unwrap();
    let r2 = mds
        .add_item(
            &s,
            &blob_b,
            &[Membership {
                container: inbox,
                flags: flags(),
                added_at: 2,
            }],
        )
        .unwrap();
    assert_ne!(r1.item_id, r2.item_id);

    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob_a), Some(2));
    assert_eq!(ref_count_rows(&bdb, &blob_a), 2);

    // Removing one item (via its only membership) drops refcount to
    // 1; the other item still pins the blob.
    mds.remove_membership(&s, &r1.item_id, &inbox).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob_a), Some(1));
    assert_eq!(ref_count_rows(&bdb, &blob_a), 1);

    mds.remove_membership(&s, &r2.item_id, &inbox).unwrap();
    let bdb = blobs_db(d.path());
    assert_eq!(refcount(&bdb, &blob_a), Some(0));
    assert_eq!(ref_count_rows(&bdb, &blob_a), 0);
}

#[test]
fn add_item_first_seen_stable_across_duplicate_content() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"first_seen stability").unwrap();

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
    let first_seen_1: i64 = bdb
        .query_row(
            "SELECT first_seen FROM blob WHERE hash = ?1",
            params![hex(&blob)],
            |r| r.get(0),
        )
        .unwrap();

    // Add again as a separate item with identical bytes; first_seen
    // must not move, last_seen may.
    std::thread::sleep(std::time::Duration::from_millis(2));
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: flags(),
            added_at: 2,
        }],
    )
    .unwrap();
    let bdb = blobs_db(d.path());
    let (first_seen_2, last_seen_2): (i64, i64) = bdb
        .query_row(
            "SELECT first_seen, last_seen FROM blob WHERE hash = ?1",
            params![hex(&blob)],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(first_seen_1, first_seen_2);
    assert!(last_seen_2 >= first_seen_2);
}
