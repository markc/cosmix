//! MCS Phase 3 — `prune_changelog` + `changelog_floor` semantics.
//!
//! Pins the durable-retention contract relied on by
//! `cosmix-maild::mailstore::mailbox_changes` stale-token rejection:
//!
//! - Fresh DB: both floors are 0 (no prune has run).
//! - keep_n >= row_count: no-op; floor unchanged.
//! - keep_n < row_count: DELETE rows with seq <= cutoff, floor =
//!   cutoff (the highest deleted seq).
//! - keep_n == 0: prune everything; floor = previous max seq.
//! - Streams are independent: pruning one does not move the other's
//!   floor.
//! - Bus `mds.changelog.pruned` is emitted post-commit with the right
//!   stream tag and the new floor (covered by bus_events smoke test).

use cosmix_mds::{ChangelogStream, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use tempfile::TempDir;

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

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

/// Drive a handful of mutations so both streams have populated rows.
/// Returns the set id plus the current max seqs as observed.
fn populate(mds: &SqliteCasMds) -> SetId {
    let s = fresh_set(mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    mds.create_container(&s, None, "Archive", attrs()).unwrap();
    mds.rename_container(&s, &inbox, None, "INBOX2").unwrap();

    for i in 0..5 {
        let blob = mds.put_blob(format!("payload-{i}").as_bytes()).unwrap();
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: i as i64,
            }],
        )
        .unwrap();
    }
    s
}

#[test]
fn fresh_set_has_zero_floors() {
    let (_d, mds) = open();
    let s = fresh_set(&mds);
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::ContainerChangeSet)
            .unwrap(),
        0
    );
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::SetChange).unwrap(),
        0
    );
}

#[test]
fn prune_empty_stream_is_noop() {
    let (_d, mds) = open();
    let s = fresh_set(&mds);
    // No mutations driven — both streams empty, floors stay zero.
    let r = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 10)
        .unwrap();
    assert_eq!(r.rows_removed, 0);
    assert_eq!(r.new_floor, 0);

    let r = mds
        .prune_changelog(&s, ChangelogStream::SetChange, 10)
        .unwrap();
    assert_eq!(r.rows_removed, 0);
    assert_eq!(r.new_floor, 0);
}

#[test]
fn prune_keep_more_than_rows_is_noop() {
    let (_d, mds) = open();
    let s = populate(&mds);
    let before_cc = mds
        .changelog_floor(&s, ChangelogStream::ContainerChangeSet)
        .unwrap();
    let before_sc = mds.changelog_floor(&s, ChangelogStream::SetChange).unwrap();

    let r = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 1_000)
        .unwrap();
    assert_eq!(r.rows_removed, 0, "keep_n above row count must not delete");
    assert_eq!(r.new_floor, before_cc);

    let r = mds
        .prune_changelog(&s, ChangelogStream::SetChange, 1_000)
        .unwrap();
    assert_eq!(r.rows_removed, 0);
    assert_eq!(r.new_floor, before_sc);
}

#[test]
fn prune_keep_zero_drops_everything_and_floor_jumps_to_old_max() {
    let (_d, mds) = open();
    let s = populate(&mds);

    let r = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 0)
        .unwrap();
    assert!(
        r.rows_removed >= 3,
        "expected >=3 lifecycle rows, got {}",
        r.rows_removed
    );
    assert!(r.new_floor >= r.rows_removed);

    // Floor is now persistent and readable.
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::ContainerChangeSet)
            .unwrap(),
        r.new_floor
    );

    // Re-running keep_n=0 on the now-empty table is a no-op: nothing
    // to delete, floor doesn't move backward.
    let r2 = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 0)
        .unwrap();
    assert_eq!(r2.rows_removed, 0);
    assert_eq!(r2.new_floor, r.new_floor);
}

#[test]
fn prune_partial_keeps_newest_n_and_floor_equals_cutoff() {
    let (_d, mds) = open();
    let s = populate(&mds);

    // Keep the newest 2 lifecycle rows; older ones go.
    let r = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 2)
        .unwrap();
    assert!(r.rows_removed >= 1, "expected to delete at least one row");
    let floor = r.new_floor;
    assert!(floor > 0);
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::ContainerChangeSet)
            .unwrap(),
        floor
    );

    // A second prune with the same keep_n is now a no-op (the
    // remaining 2 rows are the newest).
    let r2 = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 2)
        .unwrap();
    assert_eq!(r2.rows_removed, 0);
    assert_eq!(r2.new_floor, floor);
}

#[test]
fn prune_streams_are_independent() {
    let (_d, mds) = open();
    let s = populate(&mds);

    let before_sc = mds.changelog_floor(&s, ChangelogStream::SetChange).unwrap();
    let cc = mds
        .prune_changelog(&s, ChangelogStream::ContainerChangeSet, 0)
        .unwrap();
    assert!(cc.new_floor > 0);

    // SetChange floor must not have moved.
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::SetChange).unwrap(),
        before_sc
    );

    // And vice versa.
    let before_cc = mds
        .changelog_floor(&s, ChangelogStream::ContainerChangeSet)
        .unwrap();
    let sc = mds
        .prune_changelog(&s, ChangelogStream::SetChange, 0)
        .unwrap();
    assert!(sc.new_floor > 0);
    assert_eq!(
        mds.changelog_floor(&s, ChangelogStream::ContainerChangeSet)
            .unwrap(),
        before_cc
    );
}

#[test]
fn changelog_floor_unknown_set_returns_set_not_found() {
    let (_d, mds) = open();
    let bogus = SetId(uuid::Uuid::now_v7());
    let err = mds
        .changelog_floor(&bogus, ChangelogStream::SetChange)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("set not found"), "got {msg}");
}

#[test]
fn floor_survives_set_handle_reopen() {
    // The floor is durable on `set_state` — a second open of the
    // same root must see the pruned floor.
    let d = TempDir::new().unwrap();
    let s = {
        let mds = SqliteCasMds::open(d.path()).unwrap();
        let s = populate(&mds);
        mds.prune_changelog(&s, ChangelogStream::SetChange, 1)
            .unwrap();
        s
    };
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let floor = mds.changelog_floor(&s, ChangelogStream::SetChange).unwrap();
    assert!(floor > 0, "floor should be durable across reopens");
}
