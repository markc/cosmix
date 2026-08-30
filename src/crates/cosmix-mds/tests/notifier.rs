//! Phase 2: subscribe / publish wake-up tests.
//!
//! Per spec §subscribe the v1 contract is "wakes, doesn't deadlock"
//! within a 5-second deadline; sub-50ms p99 latency is a v1.x tuning
//! topic. These tests assert wake-ups arrive within 5s and that the
//! event payload matches the mutation that fired it.

use cosmix_mds::{ContainerAttrs, ContainerEvent, Flags, Mds, Membership, SetId, SqliteCasMds};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

fn unread() -> Flags {
    Flags(0)
}
fn seen() -> Flags {
    Flags(1)
}

fn open() -> (TempDir, Arc<SqliteCasMds>) {
    let d = TempDir::new().unwrap();
    let m = Arc::new(SqliteCasMds::open(d.path()).unwrap());
    (d, m)
}

const DEADLINE: Duration = Duration::from_secs(5);

#[tokio::test]
async fn subscribe_wakes_on_add_item() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    let mut rx = mds.subscribe(&s, &inbox);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        let blob = mds2.put_blob(b"wake me").unwrap();
        mds2.add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 1,
            }],
        )
        .unwrap()
    });

    let event = timeout(DEADLINE, rx.recv())
        .await
        .expect("subscribe did not wake within 5s")
        .expect("channel closed unexpectedly");

    let report = writer.await.unwrap();
    match event {
        ContainerEvent::ItemAdded {
            item_id,
            seq,
            change_seq,
        } => {
            assert_eq!(item_id, report.item_id);
            assert_eq!(seq, report.placements[0].seq);
            assert_eq!(change_seq, report.placements[0].change_seq);
        }
        other => panic!("expected ItemAdded, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_wakes_on_store_flags() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"flag me").unwrap();
    let report = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = report.item_id;

    let mut rx = mds.subscribe(&s, &inbox);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        mds2.store_flags(&s, &item_id, &inbox, seen()).unwrap()
    });

    let event = timeout(DEADLINE, rx.recv())
        .await
        .expect("subscribe did not wake on store_flags")
        .expect("channel closed unexpectedly");

    let token = writer.await.unwrap();
    match event {
        ContainerEvent::FlagsChanged {
            item_id: id,
            change_seq,
        } => {
            assert_eq!(id, item_id);
            assert_eq!(change_seq, token);
        }
        other => panic!("expected FlagsChanged, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_wakes_on_remove_membership() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"bye").unwrap();
    let report = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: seen(),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = report.item_id;
    let added_seq = report.placements[0].seq;

    let mut rx = mds.subscribe(&s, &inbox);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        mds2.remove_membership(&s, &item_id, &inbox).unwrap();
    });

    let event = timeout(DEADLINE, rx.recv())
        .await
        .expect("subscribe did not wake on remove")
        .expect("channel closed unexpectedly");
    writer.await.unwrap();

    match event {
        ContainerEvent::ItemRemoved {
            item_id: id, seq, ..
        } => {
            assert_eq!(id, item_id);
            assert_eq!(seq, added_seq);
        }
        other => panic!("expected ItemRemoved, got {other:?}"),
    }
}

#[tokio::test]
async fn move_publishes_removed_on_src_and_added_on_dest() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"travelling").unwrap();
    let report = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = report.item_id;

    let mut rx_src = mds.subscribe(&s, &inbox);
    let mut rx_dst = mds.subscribe(&s, &archive);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        mds2.move_item(&s, &item_id, &inbox, &archive, unread())
            .unwrap()
    });

    let ev_src = timeout(DEADLINE, rx_src.recv()).await.unwrap().unwrap();
    let ev_dst = timeout(DEADLINE, rx_dst.recv()).await.unwrap().unwrap();
    let _ = writer.await.unwrap();

    assert!(matches!(ev_src, ContainerEvent::ItemRemoved { item_id: id, .. } if id == item_id));
    assert!(matches!(ev_dst, ContainerEvent::ItemAdded { item_id: id, .. } if id == item_id));
}

#[tokio::test]
async fn subscribe_isolates_per_container() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let other = mds.create_container(&s, None, "Other", attrs()).unwrap();

    let mut rx_other = mds.subscribe(&s, &other);

    let blob = mds.put_blob(b"only in inbox").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: unread(),
            added_at: 1,
        }],
    )
    .unwrap();

    // The Other channel should see no traffic — recv should time out.
    let res = timeout(Duration::from_millis(200), rx_other.recv()).await;
    assert!(
        res.is_err(),
        "other container received an event it shouldn't have"
    );
}

#[tokio::test]
async fn delete_container_closes_subscriber_channel() {
    let (_d, mds) = open();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let trash = mds.create_container(&s, None, "Trash", attrs()).unwrap();

    let mut rx = mds.subscribe(&s, &trash);
    mds.delete_container(&s, &trash).unwrap();

    // After channel close, recv should return Err(Closed) — not block.
    let res = timeout(DEADLINE, rx.recv()).await.unwrap();
    assert!(res.is_err(), "expected channel-closed error, got {res:?}");
}
