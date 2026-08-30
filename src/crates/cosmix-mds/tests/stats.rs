//! Integration tests for `Mds::stats` and `Mds::stats_per_set`.
//!
//! Phase 5 commits the library impl alongside the `cosmix-mds stats`
//! CLI. The CLI tests in `tests/cli.rs` cover the operator surface
//! (output discipline, JSON shape). These tests cover the library
//! contract: counts roll up across sets, dedup_ratio reflects shared
//! blobs, and the public types are populated as documented.

use cosmix_mds::{
    BlobHash, ContainerAttrs, ContainerId, Flags, Mds, Membership, SetId, SqliteCasMds,
};
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

fn flags() -> Flags {
    Flags(0)
}

fn add_one(mds: &SqliteCasMds, set: &SetId, container: ContainerId, body: &[u8]) -> BlobHash {
    let blob = mds.put_blob(body).unwrap();
    mds.add_item(
        set,
        &blob,
        &[Membership {
            container,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    blob
}

#[test]
fn empty_store_reports_zero_with_unit_dedup_ratio() {
    let (_d, mds) = open();
    let s = mds.stats().unwrap();
    assert_eq!(s.set_count, 0);
    assert_eq!(s.container_count, 0);
    assert_eq!(s.item_count, 0);
    assert_eq!(s.blob_count, 0);
    assert_eq!(s.total_bytes, 0);
    // Empty-store dedup is the no-compression baseline (1.0), not
    // a divide-by-zero artifact.
    assert!((s.dedup_ratio - 1.0).abs() < 1e-9);
}

#[test]
fn stats_rolls_up_across_multiple_sets() {
    let (_d, mds) = open();
    let s1 = fresh_set();
    let s2 = fresh_set();
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    let i1 = mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    let _t1 = mds.create_container(&s1, None, "Sent", attrs()).unwrap();
    let i2 = mds.create_container(&s2, None, "INBOX", attrs()).unwrap();
    add_one(&mds, &s1, i1, b"alpha");
    add_one(&mds, &s1, i1, b"beta");
    add_one(&mds, &s2, i2, b"gamma");

    let s = mds.stats().unwrap();
    assert_eq!(s.set_count, 2);
    assert_eq!(s.container_count, 3);
    assert_eq!(s.item_count, 3);
    assert_eq!(s.blob_count, 3);
    let alpha_len = "alpha".len() as u64;
    let beta_len = "beta".len() as u64;
    let gamma_len = "gamma".len() as u64;
    assert_eq!(s.total_bytes, alpha_len + beta_len + gamma_len);
    // Three distinct blobs, three items — no dedup.
    assert!((s.dedup_ratio - 1.0).abs() < 1e-9);
}

#[test]
fn dedup_ratio_reflects_shared_blob_across_sets() {
    let (_d, mds) = open();
    let s1 = fresh_set();
    let s2 = fresh_set();
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    let i1 = mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    let i2 = mds.create_container(&s2, None, "INBOX", attrs()).unwrap();
    // Same body delivered twice — once per set. CAS dedups: one
    // physical blob, two items. Ratio should be 2.0.
    let body = b"shared payload";
    add_one(&mds, &s1, i1, body);
    add_one(&mds, &s2, i2, body);

    let s = mds.stats().unwrap();
    assert_eq!(s.item_count, 2);
    assert_eq!(s.blob_count, 1);
    assert_eq!(s.total_bytes, body.len() as u64);
    // logical = body.len() * 2 ; physical = body.len() ; ratio = 2.0
    assert!((s.dedup_ratio - 2.0).abs() < 1e-9);
}

#[test]
fn stats_per_set_reports_each_set_independently() {
    let (_d, mds) = open();
    let s1 = fresh_set();
    let s2 = fresh_set();
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    let i1 = mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    let _ = mds.create_container(&s1, None, "Sent", attrs()).unwrap();
    let i2 = mds.create_container(&s2, None, "INBOX", attrs()).unwrap();
    add_one(&mds, &s1, i1, b"a");
    add_one(&mds, &s1, i1, b"bb");
    add_one(&mds, &s2, i2, b"ccc");

    let mut per = mds.stats_per_set().unwrap();
    assert_eq!(per.len(), 2);
    per.sort_by_key(|p| p.set_id.0);
    let mut expected = [s1, s2];
    expected.sort_by_key(|s| s.0);
    assert_eq!(per[0].set_id.0, expected[0].0);
    assert_eq!(per[1].set_id.0, expected[1].0);

    // Find each set's row by id and check counts.
    let s1_row = per.iter().find(|p| p.set_id.0 == s1.0).unwrap();
    let s2_row = per.iter().find(|p| p.set_id.0 == s2.0).unwrap();
    assert_eq!(s1_row.container_count, 2);
    assert_eq!(s1_row.item_count, 2);
    assert_eq!(s1_row.blob_count, 2);
    assert_eq!(s1_row.total_bytes, 1 + 2);
    assert_eq!(s2_row.container_count, 1);
    assert_eq!(s2_row.item_count, 1);
    assert_eq!(s2_row.blob_count, 1);
    assert_eq!(s2_row.total_bytes, 3);
}

#[test]
fn stats_per_set_blob_count_is_distinct_within_set() {
    // Same blob delivered twice within the same set → two items,
    // one distinct blob. blob_count is the *distinct* count.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let body = b"twice";
    add_one(&mds, &s, inbox, body);
    add_one(&mds, &s, inbox, body);

    let per = mds.stats_per_set().unwrap();
    assert_eq!(per.len(), 1);
    assert_eq!(per[0].item_count, 2);
    assert_eq!(per[0].blob_count, 1);
    // Logical: each item carries body.len() bytes, hence doubled.
    assert_eq!(per[0].total_bytes, (body.len() as u64) * 2);
}

#[test]
fn stats_skips_set_drained_by_concurrent_delete() {
    // Regression cousin to the verify(Container) lock-order test:
    // here we only assert that a set whose connection has been
    // drained (mid-delete_set) does not surface as an error from
    // stats(). The behaviour is "the snapshot does not see it,"
    // matching how stats handles any other absent set.
    let (_d, mds) = open();
    let s_alive = fresh_set();
    let s_doomed = fresh_set();
    mds.create_set(&s_alive).unwrap();
    mds.create_set(&s_doomed).unwrap();
    mds.create_container(&s_alive, None, "INBOX", attrs())
        .unwrap();
    mds.delete_set(&s_doomed).unwrap();

    let s = mds.stats().unwrap();
    assert_eq!(s.set_count, 1);
    assert_eq!(s.container_count, 1);
    let per = mds.stats_per_set().unwrap();
    assert_eq!(per.len(), 1);
    assert_eq!(per[0].set_id.0, s_alive.0);
}
