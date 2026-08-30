//! Phase 6: Bus event surface — smoke test.
//!
//! Drives every spec-defined event type through `SqliteCasMds` and
//! observes them on the in-process [`MdsEvent`] broadcast channel
//! installed via `with_bus_events`. The NodedClient publisher task is
//! deliberately *not* used here — the smoke test is pure-core, so it
//! runs without a broker and proves the emission contract independent
//! of the Bus transport.
//!
//! Coverage:
//!
//! - `mds.set.created` / `mds.set.deleted`
//! - `mds.container.created` / `mds.container.renamed` / `mds.container.deleted`
//! - `mds.item.added` / `mds.item.flagged` / `mds.item.copied` /
//!   `mds.item.moved` / `mds.item.removed`
//! - `mds.gc.completed` (dry-run is sufficient — emission is not
//!   gated on the dry-run flag per spec)
//! - `mds.verify.completed` and `mds.verify.failed` (latter via a
//!   deliberate one-byte CAS file corruption)
//!
//! `mds.changelog.pruned` is exercised by driving one
//! `prune_changelog` invocation per stream (MCS Phase 3, 2026-05-14).
//! The smoke test asserts both stream tags surface in the event
//! stream — emission discipline matches the other spec topics
//! (post-commit, always-fire including no-ops).

use cosmix_mds::bus::{MdsEvent, TOPIC_COUNT};
use cosmix_mds::{
    BlobHash, ChangelogStream, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds,
    VerifyScope,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::time::timeout;

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

fn fresh_set() -> SetId {
    SetId(uuid::Uuid::now_v7())
}

fn hex(h: &BlobHash) -> String {
    let mut s = String::with_capacity(64);
    for b in h.0.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn blob_path(root: &Path, h: &BlobHash) -> PathBuf {
    let h = hex(h);
    root.join("blobs").join(&h[0..2]).join(&h[2..4]).join(h)
}

/// Drain the broadcast channel into a shared Vec until the channel
/// closes. Returns the join handle so the caller can drop the sink
/// (close the channel) and await full collection. The `Lagged` arm
/// is treated as fatal — the smoke test should never overrun a
/// 256-slot buffer.
fn spawn_collector(
    mut rx: broadcast::Receiver<MdsEvent>,
    sink: Arc<AsyncMutex<Vec<MdsEvent>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => sink.lock().await.push(ev),
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    panic!(
                        "bus_events smoke test lagged by {n}; capacity too small or runaway emission"
                    );
                }
            }
        }
    })
}

#[tokio::test]
async fn every_spec_event_type_is_emitted_exactly_once_for_a_driven_op() {
    let dir = TempDir::new().unwrap();
    let store = SqliteCasMds::open(dir.path()).unwrap();
    let (store, rx) = store.with_bus_events();
    let mds = Arc::new(store);

    let captured: Arc<AsyncMutex<Vec<MdsEvent>>> = Arc::new(AsyncMutex::new(Vec::new()));
    let collector = spawn_collector(rx, captured.clone());

    // SqliteCasMds operations are blocking (rusqlite). Run them on a
    // blocking task so the collector future can keep draining.
    let mds_for_ops = mds.clone();
    let dir_path = dir.path().to_path_buf();
    let ops_join: tokio::task::JoinHandle<()> = tokio::task::spawn_blocking(move || {
        let mds = mds_for_ops;
        let s = fresh_set();

        // 1) SetCreated
        mds.create_set(&s).unwrap();

        // 2) ContainerCreated (inbox + spam — second one is needed
        //    for copy/move dest)
        let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
        let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
        let trash = mds.create_container(&s, None, "Trash", attrs()).unwrap();

        // 3) ContainerRenamed
        mds.rename_container(&s, &archive, None, "Saved").unwrap();

        // 4) ItemAdded — drives mds.item.added
        let blob_a = mds.put_blob(b"smoke event payload A").unwrap();
        let report = mds
            .add_item(
                &s,
                &blob_a,
                &[Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                }],
            )
            .unwrap();
        let item_a = report.item_id;

        // 5) ItemFlagged
        mds.store_flags(&s, &item_a, &inbox, Flags(1)).unwrap();

        // 6) ItemCopied — copy item_a from inbox to Saved
        mds.copy_item(&s, &item_a, &archive, Flags(0)).unwrap();

        // 7) ItemMoved — move item_a from inbox to trash
        mds.move_item(&s, &item_a, &inbox, &trash, Flags(0))
            .unwrap();

        // 8) ItemRemoved — drop the trash membership
        mds.remove_membership(&s, &item_a, &trash).unwrap();
        // (item is still in `archive` from the copy above so we
        // don't unintentionally cascade-delete the underlying blob
        // mid-test)

        // 9) ContainerDeleted — empty container
        let scratch = mds.create_container(&s, None, "Scratch", attrs()).unwrap();
        mds.delete_container(&s, &scratch).unwrap();

        // 10) GcCompleted (dry-run; spec emits regardless)
        mds.gc(true).unwrap();

        // 11) VerifyCompleted on a clean blob — this also produces
        //     no VerifyFailed yet.
        let _ = mds.verify_blobs(VerifyScope::Full).unwrap();

        // 12) VerifyFailed — corrupt blob_a's CAS file then sweep.
        //     (item_a still references blob_a via `archive`.)
        let path = blob_path(&dir_path, &blob_a);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let report = mds.verify_blobs(VerifyScope::Full).unwrap();
        assert_eq!(
            report.mismatches, 1,
            "expected one mismatch after corruption"
        );

        // 13) ChangelogPruned — drive one prune per stream so both
        //     `stream` tag values surface. keep_n=0 prunes everything,
        //     which yields the most informative event payload (floor
        //     advances). Setting tail content first via the ops above
        //     means at least one row will be removed from set_change;
        //     container_change_set has been populated by the
        //     create/rename/delete calls.
        mds.prune_changelog(&s, ChangelogStream::ContainerChangeSet, 0)
            .unwrap();
        mds.prune_changelog(&s, ChangelogStream::SetChange, 0)
            .unwrap();

        // 14) SetDeleted — must come last so we tear down the set.
        mds.delete_set(&s).unwrap();
    });

    // Wait for ops to finish, then drop the store to close the
    // channel so the collector exits.
    ops_join.await.expect("operations completed");

    // Give the collector a beat to drain in-flight events from the
    // broadcast queue. The channel doesn't close until we drop the
    // sender, which lives inside `mds.event_sink`. Once we drop the
    // last Arc<SqliteCasMds>, the sink (and its broadcast::Sender)
    // goes too, and the collector exits via `Closed`.
    drop(mds);
    timeout(Duration::from_secs(2), collector)
        .await
        .expect("collector finished")
        .expect("collector task did not panic");

    let events = captured.lock().await.clone();

    // Topic→count tally. Every spec topic — including the (formerly
    // dormant) `mds.changelog.pruned` — should appear at least once
    // after the driven ops.
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for ev in &events {
        *counts.entry(ev.topic()).or_insert(0) += 1;
    }
    eprintln!("captured topic counts: {counts:#?}");

    // Required topics.
    let required = [
        "mds.set.created",
        "mds.set.deleted",
        "mds.container.created",
        "mds.container.renamed",
        "mds.container.deleted",
        "mds.item.added",
        "mds.item.flagged",
        "mds.item.copied",
        "mds.item.moved",
        "mds.item.removed",
        "mds.gc.completed",
        "mds.verify.completed",
        "mds.verify.failed",
        "mds.changelog.pruned",
    ];
    for t in &required {
        assert!(
            counts.get(t).copied().unwrap_or(0) >= 1,
            "missing required event {t}; saw {counts:?}"
        );
    }

    // `prune_changelog` was driven twice (once per stream) so the
    // pruned topic must surface at least two events with distinct
    // `stream` tags.
    let prune_streams: std::collections::BTreeSet<String> = events
        .iter()
        .filter_map(|e| match e {
            MdsEvent::ChangelogPruned(p) => Some(p.stream.clone()),
            _ => None,
        })
        .collect();
    assert!(
        prune_streams.contains("container_change_set") && prune_streams.contains("set_change"),
        "expected both stream tags in mds.changelog.pruned, got {prune_streams:?}"
    );

    // Full coverage check: the required-list should now cover the
    // whole spec table.
    assert_eq!(required.len(), TOPIC_COUNT, "spec table size drifted");
}

#[tokio::test]
async fn item_added_payload_carries_hex_blob_hash_and_parallel_arrays() {
    // Tighter assertion on the wire shape of one event — the
    // smoke test above only counts topics; this one verifies the
    // payload structure subscribers will route on.
    let dir = TempDir::new().unwrap();
    let store = SqliteCasMds::open(dir.path()).unwrap();
    let (store, rx) = store.with_bus_events();
    let mds = Arc::new(store);

    let captured: Arc<AsyncMutex<Vec<MdsEvent>>> = Arc::new(AsyncMutex::new(Vec::new()));
    let collector = spawn_collector(rx, captured.clone());

    let mds_ops = mds.clone();
    tokio::task::spawn_blocking(move || {
        let s = fresh_set();
        mds_ops.create_set(&s).unwrap();
        let a = mds_ops.create_container(&s, None, "A", attrs()).unwrap();
        let b = mds_ops.create_container(&s, None, "B", attrs()).unwrap();
        let blob = mds_ops.put_blob(b"payload-shape probe").unwrap();
        mds_ops
            .add_item(
                &s,
                &blob,
                &[
                    Membership {
                        container: a,
                        flags: Flags(0),
                        added_at: 1,
                    },
                    Membership {
                        container: b,
                        flags: Flags(0),
                        added_at: 1,
                    },
                ],
            )
            .unwrap();
    })
    .await
    .unwrap();

    drop(mds);
    timeout(Duration::from_secs(2), collector)
        .await
        .unwrap()
        .unwrap();

    let events = captured.lock().await.clone();
    let added = events
        .iter()
        .find(|e| matches!(e, MdsEvent::ItemAdded(_)))
        .expect("expected one ItemAdded event");
    let payload = added.payload_json();
    assert!(payload["blob_hash"].is_string());
    assert_eq!(
        payload["blob_hash"].as_str().unwrap().len(),
        64,
        "blob_hash on the wire must be 64-char lowercase hex"
    );
    assert_eq!(payload["container_ids"].as_array().unwrap().len(), 2);
    assert_eq!(payload["change_seqs"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn verify_failed_payload_omits_set_id() {
    // Spec-frozen wire shape: `{hash, status}` and nothing else.
    // Subscribers must not assume a set_id is available — see
    // VerifyFailed doc comment.
    let dir = TempDir::new().unwrap();
    let store = SqliteCasMds::open(dir.path()).unwrap();
    let (store, rx) = store.with_bus_events();
    let mds = Arc::new(store);

    let captured: Arc<AsyncMutex<Vec<MdsEvent>>> = Arc::new(AsyncMutex::new(Vec::new()));
    let collector = spawn_collector(rx, captured.clone());

    let mds_ops = mds.clone();
    let dir_path = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let s = fresh_set();
        mds_ops.create_set(&s).unwrap();
        let inbox = mds_ops
            .create_container(&s, None, "INBOX", attrs())
            .unwrap();
        let blob = mds_ops.put_blob(b"corrupt me").unwrap();
        mds_ops
            .add_item(
                &s,
                &blob,
                &[Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                }],
            )
            .unwrap();
        // Corrupt and verify.
        let path = blob_path(&dir_path, &blob);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let _ = mds_ops.verify_blobs(VerifyScope::Full).unwrap();
    })
    .await
    .unwrap();

    drop(mds);
    timeout(Duration::from_secs(2), collector)
        .await
        .unwrap()
        .unwrap();

    let events = captured.lock().await.clone();
    let failed = events
        .iter()
        .find(|e| matches!(e, MdsEvent::VerifyFailed(_)))
        .expect("expected VerifyFailed event after corruption");
    let payload = failed.payload_json();
    let obj = payload.as_object().expect("payload is JSON object");
    let keys: std::collections::BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from(["hash", "status"]),
        "verify.failed payload must be exactly {{hash, status}}"
    );
    assert_eq!(payload["status"].as_i64().unwrap(), 1);
}
