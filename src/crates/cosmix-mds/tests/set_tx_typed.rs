//! Phase 8d.1 — typed `SqliteSetTx::*` methods.
//!
//! Acceptance bar:
//!
//!   * `ensure_container_by_name` is idempotent: a second call with
//!     the same `(parent, name)` returns the *same* `ContainerId` and
//!     does not write a duplicate row.
//!   * `add_staging_item` writes item + blob_ref + membership + a
//!     `set_change` row in one tx; rollback (closure `Err` or panic)
//!     leaves no trace in any of the four layers.
//!   * Buffered events do not leak on rollback: a subscriber to a
//!     container that was about-to-receive an `ItemAdded` sees
//!     nothing once the closure returns `Err`.
//!   * Buffered events *do* fire on commit, in the same shape the
//!     public `Mds` paths produce.
//!   * `move_item` inside SetTx adopts v1.1 §3 keyword inheritance
//!     and emits the dual `ItemMoved` notifier event.
//!   * `remove_membership` cascades to `item` row + `blob_ref` when
//!     it removes the last membership.
//!
//! These tests are the load-bearing review surface for 8d.1: the
//! "no public Mds re-entry inside with_set_tx" + "no post-rollback
//! ghost events" rules.

use cosmix_mds::bus::MdsEvent;
use cosmix_mds::container::UPLOAD_STAGING_NAME;
use cosmix_mds::{
    BlobHash, ContainerAttrs, ContainerEvent, ContainerId, Flags, ItemId, Mds, SetId, SqliteCasMds,
};
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
}

fn open_with_bus() -> (
    TempDir,
    SqliteCasMds,
    tokio::sync::broadcast::Receiver<MdsEvent>,
) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let (mds, rx) = mds.with_bus_events();
    (d, mds, rx)
}

fn fresh_set(mds: &SqliteCasMds) -> SetId {
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    s
}

fn default_attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::Value::Null,
    }
}

fn put_blob(mds: &SqliteCasMds, payload: &[u8]) -> (BlobHash, u64) {
    let h = mds.put_blob(payload).unwrap();
    (h, payload.len() as u64)
}

fn count_layers(mds: &SqliteCasMds, set: &SetId) -> (i64, i64, i64, i64) {
    // (containers, items, set_changes, blob_refs). Probe through a
    // rolled-back with_set_tx so we hit the real cached connection
    // (with blobs_db ATTACH'd) instead of opening a second handle.
    let mut counts = (0i64, 0i64, 0i64, 0i64);
    let set_id_str = set.0.to_string();
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |h| {
        let c: i64 = h
            .tx()
            .query_row("SELECT COUNT(*) FROM container", [], |r| r.get(0))
            .unwrap();
        let i: i64 = h
            .tx()
            .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
            .unwrap();
        let sc: i64 = h
            .tx()
            .query_row("SELECT COUNT(*) FROM set_change", [], |r| r.get(0))
            .unwrap();
        let br: i64 = h
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM blobs_db.blob_ref WHERE set_id = ?1",
                [&set_id_str],
                |r| r.get(0),
            )
            .unwrap();
        counts = (c, i, sc, br);
        Err(cosmix_mds::Error::Other("probe rollback".into()))
    });
    counts
}

#[test]
fn ensure_container_by_name_is_idempotent() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();

    let (a, b, c) = mds
        .with_set_tx(&set, |h| {
            let a = h
                .ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)
                .unwrap();
            let b = h
                .ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)
                .unwrap();
            let c = h
                .ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)
                .unwrap();
            Ok((a, b, c))
        })
        .unwrap();
    assert_eq!(a, b, "second call returns same ContainerId");
    assert_eq!(b, c, "third call returns same ContainerId");

    // Across-tx idempotence: re-opening the closure must still find
    // the same row (lookup-then-create branch).
    let again: ContainerId = mds
        .with_set_tx(&set, |h| {
            h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)
        })
        .unwrap();
    assert_eq!(again, a, "across-tx call returns same ContainerId");

    // Exactly one row in the table.
    let (containers, _, _, _) = count_layers(&mds, &set);
    assert_eq!(containers, 1, "exactly one container row");
}

#[test]
fn ensure_container_by_name_separates_root_and_child() {
    // A container named "X" with parent=None must NOT collide with a
    // container named "X" under some non-NULL parent. SQLite NULL
    // semantics regress easily here — guard explicitly.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();

    let (root, child) = mds
        .with_set_tx(&set, |h| {
            let root = h.ensure_container_by_name(None, "X", &attrs)?;
            let child = h.ensure_container_by_name(Some(&root), "X", &attrs)?;
            Ok((root, child))
        })
        .unwrap();
    assert_ne!(
        root, child,
        "child 'X' under root must not collide with root 'X'"
    );
    let (containers, _, _, _) = count_layers(&mds, &set);
    assert_eq!(containers, 2, "two distinct rows");
}

#[test]
fn add_staging_item_atomic_on_commit() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"staging payload commit");

    let (cid, item, placement) = mds
        .with_set_tx(&set, |h| {
            let cid = h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)?;
            let (item, placement) = h.add_staging_item(&cid, &hash, size, Flags(0), &[])?;
            Ok((cid, item, placement))
        })
        .unwrap();

    // The membership's seq is exposed directly (no follow-up
    // container_status read needed by callers staging multi-mailbox
    // inserts).
    assert_eq!(placement.container, cid);
    assert_eq!(
        placement.seq.0, 1,
        "first membership in fresh container is seq=1"
    );

    // All four layers landed.
    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 1);
    assert_eq!(items, 1);
    // Two set_change rows: ContainerCreated → no, only Added (membership)
    // is set_change-tracked. Container creation does not allocate
    // set_change_seq. So expect 1.
    assert_eq!(set_changes, 1, "membership add → 1 set_change row");
    assert_eq!(blob_refs, 1, "blob_ref refcount += 1");

    // Item is in the staging container.
    let memberships = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].0, cid);
}

#[test]
fn add_staging_item_rolls_back_on_err() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"staging payload err");

    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        let cid = h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)?;
        h.add_staging_item(&cid, &hash, size, Flags(0), &[])?;
        Err(cosmix_mds::Error::Other("intentional".into()))
    });
    assert!(result.is_err());

    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 0, "container row rolled back");
    assert_eq!(items, 0, "item row rolled back");
    assert_eq!(set_changes, 0, "set_change row rolled back");
    assert_eq!(blob_refs, 0, "blob_ref row rolled back across ATTACH");
}

#[test]
fn add_staging_item_rolls_back_on_panic() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"staging payload panic");

    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
            let cid = h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)?;
            h.add_staging_item(&cid, &hash, size, Flags(0), &[])?;
            panic!("intentional panic mid-staging");
        });
    }));
    assert!(caught.is_err(), "panic must escape the closure");

    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!((containers, items, set_changes, blob_refs), (0, 0, 0, 0));

    // Cache entry must remain usable — a second SetTx against the
    // same SetId works without surprises.
    let (cid, item) = mds
        .with_set_tx(&set, |h| {
            let cid = h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)?;
            let (item, _placement) = h.add_staging_item(&cid, &hash, size, Flags(0), &[])?;
            Ok((cid, item))
        })
        .unwrap();
    let memberships = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].0, cid);
}

#[test]
fn rolled_back_settx_does_not_publish_ghost_events() {
    // The load-bearing 8d.1 invariant: notifier events buffered in a
    // SqliteSetTx must NOT leak when the closure returns Err. A
    // subscriber to the about-to-receive container must see zero
    // events for the failed transaction.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"ghost-event guard");

    // Subscribe before we start the tx so the channel exists. We need
    // a known container id to subscribe to — provision it in its own
    // committed tx first (this commit fires its own ContainerCreated
    // event, but that goes to the Bus sink, not the per-container
    // notifier we're watching).
    let staging: ContainerId = mds
        .with_set_tx(&set, |h| {
            h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)
        })
        .unwrap();

    let mut rx = mds.subscribe(&set, &staging);

    // Now run an Err-returning closure that would have published an
    // ItemAdded event if the buffer didn't exist.
    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.add_staging_item(&staging, &hash, size, Flags(0), &[])?;
        Err(cosmix_mds::Error::Other("ghost guard".into()))
    });
    assert!(result.is_err());

    // No event must arrive. Wait long enough that a real eager-
    // publish would have surfaced; the channel only has noise if the
    // buffer leaked.
    let r = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { tokio::time::timeout(Duration::from_millis(50), rx.recv()).await });
    assert!(
        r.is_err(),
        "no event must arrive after rolled-back SetTx; got {r:?}"
    );

    // Sanity: a *committed* SetTx then DOES fire the event.
    let item: ItemId = mds
        .with_set_tx(&set, |h| {
            h.add_staging_item(&staging, &hash, size, Flags(0), &[])
                .map(|(id, _placement)| id)
        })
        .unwrap();
    let r = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { tokio::time::timeout(Duration::from_millis(500), rx.recv()).await });
    let event = r.expect("event timeout").expect("recv error");
    match event {
        ContainerEvent::ItemAdded { item_id, .. } => assert_eq!(item_id, item),
        other => panic!("expected ItemAdded, got {other:?}"),
    }
}

#[test]
fn settx_move_item_dual_publishes_on_commit() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"move me");

    let (src, dest, item) = mds
        .with_set_tx(&set, |h| {
            let src = h.ensure_container_by_name(None, "src", &attrs)?;
            let dest = h.ensure_container_by_name(None, "dest", &attrs)?;
            let (item, _placement) = h.add_staging_item(&src, &hash, size, Flags(0), &[])?;
            Ok((src, dest, item))
        })
        .unwrap();

    let mut rx_src = mds.subscribe(&set, &src);
    let mut rx_dest = mds.subscribe(&set, &dest);

    mds.with_set_tx(&set, |h| {
        h.move_item(&item, &src, &dest, Flags(0))?;
        Ok(())
    })
    .unwrap();

    // src observes ItemRemoved + ItemMoved (additive).
    // dest observes ItemAdded + ItemMoved (additive).
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut got_src_removed = false;
    let mut got_src_moved = false;
    let mut got_dest_added = false;
    let mut got_dest_moved = false;
    rt.block_on(async {
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_millis(500), rx_src.recv())
                .await
                .expect("src event timeout")
                .expect("src recv error");
            match ev {
                ContainerEvent::ItemRemoved { item_id, .. } if item_id == item => {
                    got_src_removed = true
                }
                ContainerEvent::ItemMoved {
                    item_id,
                    src: s,
                    dest: d,
                    ..
                } if item_id == item && s == src && d == dest => got_src_moved = true,
                other => panic!("unexpected src event {other:?}"),
            }
        }
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_millis(500), rx_dest.recv())
                .await
                .expect("dest event timeout")
                .expect("dest recv error");
            match ev {
                ContainerEvent::ItemAdded { item_id, .. } if item_id == item => {
                    got_dest_added = true
                }
                ContainerEvent::ItemMoved {
                    item_id,
                    src: s,
                    dest: d,
                    ..
                } if item_id == item && s == src && d == dest => got_dest_moved = true,
                other => panic!("unexpected dest event {other:?}"),
            }
        }
    });
    assert!(got_src_removed && got_src_moved, "src missed an event");
    assert!(got_dest_added && got_dest_moved, "dest missed an event");

    // Membership state matches: dest has it, src does not.
    let memberships = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].0, dest);
}

#[test]
fn settx_remove_last_membership_cascades_to_item_and_blob_ref() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"unmoor me");

    let (cid, item) = mds
        .with_set_tx(&set, |h| {
            let cid = h.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &attrs)?;
            let (item, _placement) = h.add_staging_item(&cid, &hash, size, Flags(0), &[])?;
            Ok((cid, item))
        })
        .unwrap();

    let (_c0, items0, _sc0, br0) = count_layers(&mds, &set);
    assert_eq!(items0, 1);
    assert_eq!(br0, 1);

    mds.with_set_tx(&set, |h| h.remove_membership(&item, &cid))
        .unwrap();

    let (_c1, items1, _sc1, br1) = count_layers(&mds, &set);
    assert_eq!(items1, 0, "item row deleted on last-membership removal");
    assert_eq!(br1, 0, "blob_ref decremented to 0 (row gone)");
}

// ---------------------------------------------------------------------
// add_membership / container_seq_validity — Phase 8d.2 / Task 3.3 prereq.
// `add_membership` is the **post-create** path: an item already exists
// with ≥1 membership, and a new placement is being added (e.g. JMAP
// `Email/set update` adding a mailbox to `mailboxIds`). It emits Bus
// `ItemCopied`. For brand-new multi-mailbox creates use
// `add_item_with_memberships` (one aggregate Bus `ItemAdded`, see the
// next test block); do NOT chain `add_staging_item` + N×`add_membership`
// for a fresh item — that publishes the wrong Bus shape.
// `container_seq_validity` reads the dest's IMAP UIDVALIDITY in the
// same tx that produced the new membership, eliminating the
// follow-up-read-can-see-a-different-validity hazard.
// ---------------------------------------------------------------------

#[test]
fn add_membership_atomic_on_commit_inherits_flags() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"multi-mailbox commit");

    let seen_flag = Flags(0x01); // ItemFlags representing \Seen

    // Per the new docs, `add_staging_item` + `add_membership` for a
    // brand-new item in one tx is the wrong Bus shape. Land the
    // baseline item in `inbox` (with `\Seen`) in its own committed tx
    // first; then in a second tx, add a *post-create* membership in
    // `archive`. This matches the real `add_membership` use case (JMAP
    // `Email/set update` adding a mailbox to an existing email).
    //
    // Per v1.1 §3, archive's flags must inherit `\Seen` regardless of
    // the caller-supplied fallback `Flags(0)`.
    let (inbox, archive, item) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
            let (item, _p_inbox) = h.add_staging_item(&inbox, &hash, size, seen_flag, &[])?;
            Ok((inbox, archive, item))
        })
        .unwrap();
    let p_archive = mds
        .with_set_tx(&set, |h| h.add_membership(&item, &archive, Flags(0)))
        .unwrap();

    assert_eq!(p_archive.container, archive);
    assert_eq!(
        p_archive.seq.0, 1,
        "first membership in fresh archive is seq=1"
    );

    // Both memberships present, both carry `\Seen`.
    let memberships = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memberships.len(), 2);
    let by_cid: std::collections::HashMap<ContainerId, Flags> = memberships
        .into_iter()
        .map(|(cid, flags, _tags)| (cid, flags))
        .collect();
    assert_eq!(by_cid.get(&inbox).copied(), Some(seen_flag));
    assert_eq!(
        by_cid.get(&archive).copied(),
        Some(seen_flag),
        "v1.1 §3: 2nd membership inherits \\Seen from inbox, NOT caller fallback Flags(0)"
    );

    // Layer accounting: 1 item, 2 set_change rows (one per membership),
    // 1 blob_ref (one per item, not per membership).
    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 2);
    assert_eq!(items, 1);
    assert_eq!(set_changes, 2);
    assert_eq!(blob_refs, 1);
}

#[test]
fn add_membership_rolls_back_on_err() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"multi-mailbox err");

    // First, land a baseline item with one membership in its own
    // committed tx.
    let (inbox, item) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let (item, _p) = h.add_staging_item(&inbox, &hash, size, Flags(0), &[])?;
            Ok((inbox, item))
        })
        .unwrap();

    let (_c0, items0, sc0, br0) = count_layers(&mds, &set);
    assert_eq!(items0, 1);
    assert_eq!(sc0, 1);
    assert_eq!(br0, 1);

    // Now, in a SECOND tx, add a membership and force rollback. The
    // baseline rows must remain; the second-membership row must not
    // appear.
    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
        h.add_membership(&item, &archive, Flags(0))?;
        Err(cosmix_mds::Error::Other("rollback guard".into()))
    });
    assert!(result.is_err());

    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 1, "archive container creation rolled back");
    assert_eq!(items, 1, "baseline item preserved");
    assert_eq!(set_changes, 1, "no second set_change row");
    assert_eq!(blob_refs, 1);

    // The item still has only the inbox membership.
    let memberships = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].0, inbox);
}

#[test]
fn add_membership_does_not_publish_ghost_events_on_rollback() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"ghost-multi");

    let (inbox, archive, item) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
            let (item, _p) = h.add_staging_item(&inbox, &hash, size, Flags(0), &[])?;
            Ok((inbox, archive, item))
        })
        .unwrap();

    // Drain the inbox subscriber of the baseline ItemAdded event so we
    // start the rollback guard from a known-empty channel.
    let mut rx_inbox = mds.subscribe(&set, &inbox);
    let mut rx_archive = mds.subscribe(&set, &archive);

    // Rolled-back add_membership must NOT publish ItemAdded on archive.
    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.add_membership(&item, &archive, Flags(0))?;
        Err(cosmix_mds::Error::Other("ghost guard".into()))
    });
    assert!(result.is_err());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let r = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(50), rx_archive.recv()).await
    });
    assert!(
        r.is_err(),
        "rolled-back add_membership must not publish ItemAdded on archive; got {r:?}"
    );

    // The inbox subscriber must also see nothing — add_membership only
    // publishes on the dest, but a future bug that fanned out to all
    // memberships would surface here.
    let r = rt
        .block_on(async { tokio::time::timeout(Duration::from_millis(50), rx_inbox.recv()).await });
    assert!(
        r.is_err(),
        "rolled-back add_membership must not publish on inbox; got {r:?}"
    );

    // Sanity: a committed add_membership DOES fire on the dest.
    mds.with_set_tx(&set, |h| h.add_membership(&item, &archive, Flags(0)))
        .unwrap();
    let r = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(500), rx_archive.recv()).await
    });
    let event = r.expect("event timeout").expect("recv error");
    match event {
        ContainerEvent::ItemAdded { item_id, .. } => assert_eq!(item_id, item),
        other => panic!("expected ItemAdded, got {other:?}"),
    }
}

#[test]
fn add_membership_rejects_unknown_item() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();

    let bogus_item = ItemId(uuid::Uuid::now_v7());

    let err = mds
        .with_set_tx(&set, |h| {
            let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
            h.add_membership(&bogus_item, &archive, Flags(0))?;
            Ok(())
        })
        .unwrap_err();
    match err {
        cosmix_mds::Error::ItemNotFound(_) => (),
        other => panic!("expected ItemNotFound, got {other:?}"),
    }
}

#[test]
fn add_membership_rejects_unknown_container() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"unknown-container");

    let item = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            h.add_staging_item(&inbox, &hash, size, Flags(0), &[])
                .map(|(id, _p)| id)
        })
        .unwrap();

    let bogus = ContainerId(uuid::Uuid::now_v7());
    let err = mds
        .with_set_tx(&set, |h| h.add_membership(&item, &bogus, Flags(0)))
        .unwrap_err();
    match err {
        cosmix_mds::Error::ContainerNotFound(_) => (),
        other => panic!("expected ContainerNotFound, got {other:?}"),
    }
}

#[test]
fn add_membership_rejects_duplicate() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"dup-membership");

    let (inbox, item) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let (item, _p) = h.add_staging_item(&inbox, &hash, size, Flags(0), &[])?;
            Ok((inbox, item))
        })
        .unwrap();

    let err = mds
        .with_set_tx(&set, |h| h.add_membership(&item, &inbox, Flags(0)))
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("already in container"),
        "expected duplicate-membership error, got {msg}"
    );
}

#[test]
fn container_seq_validity_in_tx_returns_row_value() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();

    let (cid, validity) = mds
        .with_set_tx(&set, |h| {
            let cid = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let v = h.container_seq_validity(&cid)?;
            Ok((cid, v))
        })
        .unwrap();

    // We don't pin the exact value (it's a wallclock ms), but it must
    // be non-zero and re-reading it in a second tx must return the
    // same value (it's frozen at row-creation time).
    assert!(validity > 0, "seq_validity is set at container insertion");
    let again = mds
        .with_set_tx(&set, |h| h.container_seq_validity(&cid))
        .unwrap();
    assert_eq!(again, validity);
}

#[test]
fn container_seq_validity_in_tx_unknown_container_errors() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);

    let bogus = ContainerId(uuid::Uuid::now_v7());
    let err = mds
        .with_set_tx(&set, |h| h.container_seq_validity(&bogus))
        .unwrap_err();
    match err {
        cosmix_mds::Error::ContainerNotFound(_) => (),
        other => panic!("expected ContainerNotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// add_item_with_memberships — Phase 8d.2 / Task 3.3 substrate.
// Bus shape: ONE aggregate `MdsEvent::ItemAdded` with parallel
// container_ids[]/change_seqs[] arrays, mirroring public `add_item`
// (NOT one ItemAdded + N ItemCopied — the multi-mailbox JMAP create
// is a single creation event from the user's view).
// ---------------------------------------------------------------------

fn drain_bus_until_first_item_event(
    rx: &mut tokio::sync::broadcast::Receiver<MdsEvent>,
    item: ItemId,
) -> MdsEvent {
    // Skip any events before the one we care about (e.g.
    // `ContainerCreated` from `ensure_container_by_name` commits).
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        loop {
            let ev = tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .expect("bus event timeout")
                .expect("bus recv error");
            match &ev {
                MdsEvent::ItemAdded(p) if p.item_id == item => return ev,
                MdsEvent::ItemCopied(p) if p.item_id == item => return ev,
                _ => continue,
            }
        }
    })
}

#[test]
fn add_item_with_memberships_emits_one_aggregate_bus_itemadded() {
    let (_d, mds, mut rx) = open_with_bus();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"three-mailbox aggregate");

    let (inbox, archive, label, item, placements) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
            let label = h.ensure_container_by_name(None, "label", &attrs)?;
            let (item, placements) = h.add_item_with_memberships(
                &hash,
                size,
                &[(inbox, Flags(0)), (archive, Flags(0)), (label, Flags(0))],
                &[],
            )?;
            Ok((inbox, archive, label, item, placements))
        })
        .unwrap();

    assert_eq!(placements.len(), 3);
    assert_eq!(placements[0].container, inbox);
    assert_eq!(placements[1].container, archive);
    assert_eq!(placements[2].container, label);

    // Layer accounting: 1 item, 3 memberships → 3 set_change rows, 1 blob_ref.
    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 3);
    assert_eq!(items, 1);
    assert_eq!(set_changes, 3);
    assert_eq!(blob_refs, 1);

    // The next item-keyed Bus event must be the aggregate ItemAdded
    // — NOT an ItemCopied. Codex caught the prior shape (single
    // ItemAdded for staging container + 2 ItemCopied for the rest)
    // as a downstream-misclassification hazard.
    let ev = drain_bus_until_first_item_event(&mut rx, item);
    match ev {
        MdsEvent::ItemAdded(p) => {
            assert_eq!(p.item_id, item);
            assert_eq!(
                p.container_ids,
                vec![inbox, archive, label],
                "aggregate ItemAdded must carry all dest containers"
            );
            assert_eq!(p.change_seqs.len(), 3);
        }
        other => panic!("expected aggregate ItemAdded, got {other:?}"),
    }
}

#[test]
fn add_item_with_memberships_rolls_back_bus_sink_on_err() {
    let (_d, mds, mut rx) = open_with_bus();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"agg rollback");

    // Drain any pre-existing events from set_state-init / put_blob.
    let rt = tokio::runtime::Runtime::new().unwrap();
    while rt
        .block_on(async { tokio::time::timeout(Duration::from_millis(20), rx.recv()).await })
        .is_ok()
    {}

    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
        let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
        h.add_item_with_memberships(&hash, size, &[(inbox, Flags(0)), (archive, Flags(0))], &[])?;
        Err(cosmix_mds::Error::Other("bus rollback guard".into()))
    });
    assert!(result.is_err());

    // No ItemAdded must have made it onto the Bus sink. Wait long
    // enough that an eager-publish would surface.
    let r = rt.block_on(async { tokio::time::timeout(Duration::from_millis(50), rx.recv()).await });
    match r {
        Err(_) => (), // timeout — no events, as required
        Ok(Ok(MdsEvent::ItemAdded(p))) => {
            panic!("rolled-back add_item_with_memberships leaked Bus ItemAdded: {p:?}")
        }
        Ok(Ok(MdsEvent::ItemCopied(p))) => {
            panic!("rolled-back add_item_with_memberships leaked Bus ItemCopied: {p:?}")
        }
        Ok(Ok(other)) => panic!("unexpected non-item Bus event after rollback: {other:?}"),
        Ok(Err(e)) => panic!("bus recv error: {e:?}"),
    }

    // Layer accounting: rollback rolled back container creation too.
    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!((containers, items, set_changes, blob_refs), (0, 0, 0, 0));
}

#[test]
fn add_item_with_memberships_rejects_empty() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let (hash, size) = put_blob(&mds, b"empty mempers");

    let err = mds
        .with_set_tx(&set, |h| h.add_item_with_memberships(&hash, size, &[], &[]))
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("at least one membership required"),
        "expected empty-memberships error, got {msg}"
    );
}

#[test]
fn add_item_with_memberships_rejects_unknown_container() {
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"unknown-dest aggregate");

    let bogus = ContainerId(uuid::Uuid::now_v7());
    let err = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            h.add_item_with_memberships(&hash, size, &[(inbox, Flags(0)), (bogus, Flags(0))], &[])
        })
        .unwrap_err();
    match err {
        cosmix_mds::Error::ContainerNotFound(_) => (),
        other => panic!("expected ContainerNotFound, got {other:?}"),
    }

    // No rows persisted, no item, no blob_ref.
    let (containers, items, set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!((containers, items, set_changes, blob_refs), (0, 0, 0, 0));
}

#[test]
fn add_item_with_memberships_prevalidates_before_writing() {
    // Codex review (round 2) MAJOR: if prevalidation is missing, the
    // helper inserts the item row + blob_ref before iterating
    // memberships and discovering an unknown container. A caller that
    // catches the Err inside `with_set_tx` and returns Ok would commit
    // a partial item with no memberships.
    //
    // Pin the prevalidation invariant: the helper must reject the
    // unknown-container case BEFORE any write so even an Err-swallowing
    // caller commits a clean tx.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"prevalidation-before-writes");

    // Pre-create the inbox container in a committed tx so this run's
    // tx contains *only* the failing add_item_with_memberships call.
    let inbox = mds
        .with_set_tx(&set, |h| h.ensure_container_by_name(None, "inbox", &attrs))
        .unwrap();

    let bogus = ContainerId(uuid::Uuid::now_v7());

    // Caller swallows the Err inside with_set_tx and returns Ok. If
    // prevalidation runs after item/blob_ref inserts, the tx commits
    // a partial item.
    let _ = mds.with_set_tx(&set, |h| {
        let r =
            h.add_item_with_memberships(&hash, size, &[(inbox, Flags(0)), (bogus, Flags(0))], &[]);
        match r {
            Err(cosmix_mds::Error::ContainerNotFound(_)) => Ok(()),
            Err(other) => panic!("expected ContainerNotFound, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    });

    // Inbox stays (it was created in a separate committed tx). No
    // item row, no blob_ref row, no membership row from the failing
    // call.
    let (containers, items, _set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 1, "inbox should survive prevalidation reject");
    assert_eq!(items, 0, "no item row from prevalidated-reject call");
    assert_eq!(blob_refs, 0, "no blob_ref from prevalidated-reject call");
}

#[test]
fn add_item_with_memberships_rejects_duplicate_container() {
    // Codex review (round 2): prevalidation must also catch duplicate
    // containers in the slice before any write — the underlying
    // UNIQUE(item_id, container_id) would surface as a SQL constraint
    // post-insert, which a caller catching that Err inside with_set_tx
    // could turn into a partial commit.
    let (_d, mds) = open();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"duplicate-container guard");

    let inbox = mds
        .with_set_tx(&set, |h| h.ensure_container_by_name(None, "inbox", &attrs))
        .unwrap();

    let _ = mds.with_set_tx(&set, |h| {
        let r =
            h.add_item_with_memberships(&hash, size, &[(inbox, Flags(0)), (inbox, Flags(0))], &[]);
        match r {
            Err(cosmix_mds::Error::Other(s)) if s.contains("duplicate container") => Ok(()),
            Err(other) => panic!("expected duplicate-container Other, got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    });

    let (containers, items, _set_changes, blob_refs) = count_layers(&mds, &set);
    assert_eq!(containers, 1);
    assert_eq!(items, 0);
    assert_eq!(blob_refs, 0);
}

#[test]
fn add_membership_bus_sink_rollback_does_not_leak() {
    // Codex review (round 1): the no-ghost notifier test covers the
    // notifier path but not the Bus sink path, which is a distinct
    // observer surface. Pin the Bus-sink rollback invariant explicitly
    // so a future eager-emit regression in `add_membership_in_tx`
    // surfaces immediately.
    let (_d, mds, mut rx) = open_with_bus();
    let set = fresh_set(&mds);
    let attrs = default_attrs();
    let (hash, size) = put_blob(&mds, b"bus-sink rollback");

    // Land the baseline item in a committed tx, then drain any Bus
    // events that fired during setup so the rollback assertion runs
    // from a known-empty channel.
    let (_inbox, archive, item) = mds
        .with_set_tx(&set, |h| {
            let inbox = h.ensure_container_by_name(None, "inbox", &attrs)?;
            let archive = h.ensure_container_by_name(None, "archive", &attrs)?;
            let (item, _p) = h.add_staging_item(&inbox, &hash, size, Flags(0), &[])?;
            Ok((inbox, archive, item))
        })
        .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    while rt
        .block_on(async { tokio::time::timeout(Duration::from_millis(20), rx.recv()).await })
        .is_ok()
    {}

    // Rolled-back add_membership: NO Bus event must surface.
    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |h| {
        h.add_membership(&item, &archive, Flags(0))?;
        Err(cosmix_mds::Error::Other("bus-rollback guard".into()))
    });
    assert!(result.is_err());

    let r = rt.block_on(async { tokio::time::timeout(Duration::from_millis(50), rx.recv()).await });
    match r {
        Err(_) => (),
        Ok(Ok(MdsEvent::ItemCopied(p))) => {
            panic!("rolled-back add_membership leaked Bus ItemCopied: {p:?}")
        }
        Ok(Ok(other)) => panic!("unexpected Bus event after rollback: {other:?}"),
        Ok(Err(e)) => panic!("bus recv error: {e:?}"),
    }

    // Sanity: a *committed* add_membership DOES emit ItemCopied.
    mds.with_set_tx(&set, |h| h.add_membership(&item, &archive, Flags(0)))
        .unwrap();

    let r =
        rt.block_on(async { tokio::time::timeout(Duration::from_millis(500), rx.recv()).await });
    let ev = r.expect("bus timeout").expect("bus recv error");
    match ev {
        MdsEvent::ItemCopied(p) => {
            assert_eq!(p.item_id, item);
            assert_eq!(p.dest, archive);
        }
        other => panic!("expected ItemCopied, got {other:?}"),
    }
}
