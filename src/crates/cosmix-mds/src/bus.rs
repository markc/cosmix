//! Bus event surface.
//!
//! Per `_doc/2026-04-29-cosmix-mds-spec.md` §Bus event surface
//! ("Bus event surface (cosmix feature)") `cosmix-mds` emits 14
//! low-level state-transition events on the Bus bus when the
//! `cosmix` feature is enabled. The taxonomy is defined in the spec
//! table; this module is the single Rust mirror of that table.
//!
//! Module structure:
//!
//! - [`MdsEvent`] enum and the per-topic typed payload structs are
//!   *always* built (no feature gate). Pure-core consumers can wire
//!   an in-process [`EventSink`] to observe events without pulling
//!   in any Bus transport, which is what the smoke tests do.
//! - The `NodedClient`-backed publisher task that translates events
//!   into `topic.publish` RPCs is gated on `feature = "cosmix"`.
//!
//! Emission discipline: every send happens *after* the durable
//! commit that makes the event true, mirroring the in-process
//! [`crate::notifier::Notifier`] convention. For per-blob
//! `mds.verify.failed`, that means after the `blob_verify` ledger
//! update for that blob — the `verify::run` `on_mismatch` callback
//! is invoked from inside that loop, and the store layer wires the
//! callback to forward into the sink.
//!
//! `mds.changelog.pruned` is emitted by [`crate::Mds::prune_changelog`]
//! (MCS Phase 3, 2026-05-14) — one event per stream pruned, with
//! `stream` discriminating between `container_change_set` (v1.4
//! lifecycle stream) and `set_change` (v1.1 set-wide item-event
//! stream). The payload shape was reworked from the v1 dormant
//! `{container_id, rows_removed, new_horizon}` form: the MCS streams
//! are per-set, not per-container, so the legacy `container_id`
//! field was replaced by `{set_id, stream}`. No emission point ever
//! used the v1 shape, so this is a struct-shape change but not a
//! wire-break.

use crate::types::{ChangeToken, ContainerId, ItemId, Seq, SetId};
use serde::Serialize;
use tokio::sync::broadcast;

// -- Topic name constants (single source of truth in code) ----------

pub const TOPIC_SET_CREATED: &str = "mds.set.created";
pub const TOPIC_SET_DELETED: &str = "mds.set.deleted";
pub const TOPIC_CONTAINER_CREATED: &str = "mds.container.created";
pub const TOPIC_CONTAINER_RENAMED: &str = "mds.container.renamed";
pub const TOPIC_CONTAINER_DELETED: &str = "mds.container.deleted";
pub const TOPIC_ITEM_ADDED: &str = "mds.item.added";
pub const TOPIC_ITEM_FLAGGED: &str = "mds.item.flagged";
pub const TOPIC_ITEM_COPIED: &str = "mds.item.copied";
pub const TOPIC_ITEM_MOVED: &str = "mds.item.moved";
pub const TOPIC_ITEM_REMOVED: &str = "mds.item.removed";
pub const TOPIC_GC_COMPLETED: &str = "mds.gc.completed";
pub const TOPIC_VERIFY_COMPLETED: &str = "mds.verify.completed";
pub const TOPIC_VERIFY_FAILED: &str = "mds.verify.failed";
pub const TOPIC_CHANGELOG_PRUNED: &str = "mds.changelog.pruned";

/// Number of distinct topic names in the spec event table. Pinned
/// here so a smoke test can assert against the taxonomy without
/// growing a separate magic number elsewhere.
pub const TOPIC_COUNT: usize = 14;

// -- Per-topic payload structs --------------------------------------
//
// Field order and types track the spec table (§Bus event surface)
// verbatim. Renames here are wire-breaking; treat them like trait
// signature changes.

#[derive(Debug, Clone, Serialize)]
pub struct SetCreated {
    pub set_id: SetId,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetDeleted {
    pub set_id: SetId,
    pub blob_count_unreffed: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerCreated {
    pub set_id: SetId,
    pub container_id: ContainerId,
    pub parent_id: Option<ContainerId>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerRenamed {
    pub set_id: SetId,
    pub container_id: ContainerId,
    pub old_name: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerDeleted {
    pub set_id: SetId,
    pub container_id: ContainerId,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemAdded {
    pub set_id: SetId,
    pub item_id: ItemId,
    /// Lowercase 64-char hex of the BLAKE3 blob hash. Spec wire
    /// form is the hex string, not the raw 32 bytes.
    pub blob_hash: String,
    pub container_ids: Vec<ContainerId>,
    pub change_seqs: Vec<ChangeToken>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemFlagged {
    pub set_id: SetId,
    pub item_id: ItemId,
    pub container_id: ContainerId,
    /// Raw `Flags` bits as u32 — wire form is the integer, not a
    /// decoded set of flag names.
    pub old_flags: u32,
    pub new_flags: u32,
    pub change_seq: ChangeToken,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemCopied {
    pub set_id: SetId,
    pub item_id: ItemId,
    pub dest: ContainerId,
    pub seq_dest: Seq,
    pub change_seq_dest: ChangeToken,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemMoved {
    pub set_id: SetId,
    pub item_id: ItemId,
    pub src: ContainerId,
    pub dest: ContainerId,
    pub seq_dest: Seq,
    pub change_seq_src: ChangeToken,
    pub change_seq_dest: ChangeToken,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemRemoved {
    pub set_id: SetId,
    pub item_id: ItemId,
    pub container_id: ContainerId,
    pub seq: Seq,
    pub change_seq: ChangeToken,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcCompleted {
    pub blobs_deleted: u64,
    pub bytes_freed: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyCompleted {
    /// Operator-readable scope tag: `"full"`, `"since"`, or
    /// `"container"`. Matches the `VerifyScope` variants.
    pub scope: String,
    pub blobs_checked: u64,
    pub mismatches: u64,
    pub duration_ms: u64,
}

/// Per-blob mismatch event. Spec payload is `{hash, status}` only —
/// no `set_id` because `verify_blobs` can find a corrupted blob
/// referenced by zero, one, or several sets, and a single id would
/// be either unavailable or misleading. If subscribers ever need
/// routing, the cleaner addition is `set_ids: Vec<SetId>` resolved
/// by a separate lookup, not retrofitting one id here.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyFailed {
    pub hash: String,
    /// `1` = bytes hashed wrong (true bitrot); `2` = CAS file
    /// missing. Mirrors `blob_verify.status`.
    pub status: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangelogPruned {
    pub set_id: SetId,
    /// One of `"container_change_set"` or `"set_change"`. Stable
    /// strings produced by [`crate::ChangelogStream::as_str`] — pinned
    /// at the wire boundary because the Bus payload is structurally
    /// typed (JSON), not enum-typed.
    pub stream: String,
    pub rows_removed: u64,
    /// Highest seq that was deleted; rows with seq <= new_floor are
    /// no longer in the stream. Reads use `seq > since`, so clients
    /// whose `since` is strictly below `new_floor` (i.e. `since <
    /// new_floor`) receive `cannotCalculateChanges` on the next read.
    /// A client at `since == new_floor` still answers from the
    /// surviving rows.
    pub new_floor: u64,
}

/// Unified event channel payload. Each variant carries the typed
/// payload for one of the spec topics. The associated topic name is
/// returned by [`MdsEvent::topic`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "topic", content = "payload")]
pub enum MdsEvent {
    SetCreated(SetCreated),
    SetDeleted(SetDeleted),
    ContainerCreated(ContainerCreated),
    ContainerRenamed(ContainerRenamed),
    ContainerDeleted(ContainerDeleted),
    ItemAdded(ItemAdded),
    ItemFlagged(ItemFlagged),
    ItemCopied(ItemCopied),
    ItemMoved(ItemMoved),
    ItemRemoved(ItemRemoved),
    GcCompleted(GcCompleted),
    VerifyCompleted(VerifyCompleted),
    VerifyFailed(VerifyFailed),
    /// Dormant in v1 — retention sweep is not implemented. Kept in
    /// the enum for taxonomy completeness; `mds` never constructs
    /// this variant in v1.
    ChangelogPruned(ChangelogPruned),
}

impl MdsEvent {
    /// Spec topic name for this event. Used by the publisher task to
    /// build the `topic.publish` `name` header.
    pub fn topic(&self) -> &'static str {
        match self {
            MdsEvent::SetCreated(_) => TOPIC_SET_CREATED,
            MdsEvent::SetDeleted(_) => TOPIC_SET_DELETED,
            MdsEvent::ContainerCreated(_) => TOPIC_CONTAINER_CREATED,
            MdsEvent::ContainerRenamed(_) => TOPIC_CONTAINER_RENAMED,
            MdsEvent::ContainerDeleted(_) => TOPIC_CONTAINER_DELETED,
            MdsEvent::ItemAdded(_) => TOPIC_ITEM_ADDED,
            MdsEvent::ItemFlagged(_) => TOPIC_ITEM_FLAGGED,
            MdsEvent::ItemCopied(_) => TOPIC_ITEM_COPIED,
            MdsEvent::ItemMoved(_) => TOPIC_ITEM_MOVED,
            MdsEvent::ItemRemoved(_) => TOPIC_ITEM_REMOVED,
            MdsEvent::GcCompleted(_) => TOPIC_GC_COMPLETED,
            MdsEvent::VerifyCompleted(_) => TOPIC_VERIFY_COMPLETED,
            MdsEvent::VerifyFailed(_) => TOPIC_VERIFY_FAILED,
            MdsEvent::ChangelogPruned(_) => TOPIC_CHANGELOG_PRUNED,
        }
    }

    /// Serialize just the payload (not the wrapper enum) as JSON
    /// for the inner Bus message body. The wrapper's `tag`/`content`
    /// shape is internal to the in-process channel; subscribers see
    /// only the bare payload object on the wire.
    pub fn payload_json(&self) -> serde_json::Value {
        match self {
            MdsEvent::SetCreated(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::SetDeleted(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ContainerCreated(p) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
            MdsEvent::ContainerRenamed(p) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
            MdsEvent::ContainerDeleted(p) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
            MdsEvent::ItemAdded(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ItemFlagged(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ItemCopied(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ItemMoved(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ItemRemoved(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::GcCompleted(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::VerifyCompleted(p) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
            MdsEvent::VerifyFailed(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            MdsEvent::ChangelogPruned(p) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
        }
    }
}

// -- EventSink ------------------------------------------------------

/// Default channel capacity. 256 mirrors the in-process notifier and
/// the maild verdict publisher; consistent with "wakes, doesn't
/// deadlock" — slow subscribers will see `Lagged` and the publisher
/// task will warn and continue from latest.
pub const DEFAULT_CAPACITY: usize = 256;

/// Cheap, cloneable handle that the store layer hands to per-set
/// operations. `none()` is a true no-op — all `emit` calls compile
/// down to a single `Option::is_none` check, so passing a sink into
/// every operation costs nothing on `core` builds where Bus is not
/// wired.
#[derive(Clone, Default)]
pub struct EventSink {
    tx: Option<broadcast::Sender<MdsEvent>>,
}

impl EventSink {
    pub fn none() -> Self {
        Self { tx: None }
    }

    pub fn from_sender(tx: broadcast::Sender<MdsEvent>) -> Self {
        Self { tx: Some(tx) }
    }

    /// Construct a sink with a fresh broadcast channel of the given
    /// capacity. Returns the receiver so a publisher task (or test
    /// observer) can subscribe immediately. The underlying
    /// `broadcast::Sender` keeps the channel alive even when this
    /// receiver is dropped; new subscribers obtain receivers via
    /// [`EventSink::subscribe`].
    pub fn with_capacity(capacity: usize) -> (Self, broadcast::Receiver<MdsEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        (Self { tx: Some(tx) }, rx)
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<MdsEvent>> {
        self.tx.as_ref().map(|t| t.subscribe())
    }

    /// Send an event. No-op if the sink was constructed via
    /// [`EventSink::none`] (the common case in `core` builds).
    /// Slow subscribers may have lagged — `broadcast::send` returns
    /// `Err` if all receivers are gone, which is fine: we don't
    /// care, the publisher exited cleanly.
    pub fn emit(&self, event: MdsEvent) {
        if let Some(t) = &self.tx {
            let _ = t.send(event);
        }
    }
}

// -- NodedClient publisher (cosmix feature) ---------------------------

#[cfg(feature = "cosmix")]
mod publisher {
    use super::*;
    use cosmix_bus::bus::BusMessage;
    use cosmix_client::NodedClient;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Spawn the long-lived publisher task. Drains the
    /// [`MdsEvent`] broadcast channel and forwards each event as a
    /// `topic.publish` RPC against the broker. The wire shape mirrors
    /// `cosmix-maild::bus::verdict`:
    ///
    /// - inner [`BusMessage`] carries `command = <topic>` so
    ///   subscribers can route with `on <topic> do …`.
    /// - body is the bare payload JSON (no enum wrapper).
    /// - outer `topic.publish` carries `name = <topic>` and
    ///   `retain = "false"` headers. `retain=false` is a hard
    ///   requirement: noded's default is `retain=true`, which
    ///   would leak the last-emitted state-transition envelope to
    ///   any peer that subscribes after the fact and violate the
    ///   "no historical replay" intent of these events.
    ///
    /// On `Lagged(n)` the task warns and continues from latest; on
    /// `Closed` it exits cleanly.
    pub fn spawn_publisher_task(
        client: Arc<NodedClient>,
        mut rx: broadcast::Receiver<MdsEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let topic = event.topic();
                        let body = serde_json::to_string(&event.payload_json())
                            .unwrap_or_else(|_| "{}".to_string());
                        let mut inner = BusMessage::new();
                        inner.set("command", topic);
                        inner.body = body;
                        let inner_wire = inner.to_wire();
                        let mut headers = BTreeMap::new();
                        headers.insert("name".to_string(), topic.to_string());
                        // Hard requirement; see module doc.
                        headers.insert("retain".to_string(), "false".to_string());
                        if let Err(e) = client
                            .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
                            .await
                        {
                            tracing::warn!(
                                target: "cosmix_mds",
                                topic = topic,
                                error = %e,
                                "mds Bus topic.publish failed"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            target: "cosmix_mds",
                            lagged = n,
                            "mds Bus subscriber lagged; continuing from latest"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            target: "cosmix_mds",
                            "mds Bus broadcast channel closed; publisher exiting"
                        );
                        return;
                    }
                }
            }
        })
    }
}

#[cfg(feature = "cosmix")]
pub use publisher::spawn_publisher_task;

// -- Tests (payload shape) ------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample_set() -> SetId {
        SetId(Uuid::nil())
    }

    fn sample_container() -> ContainerId {
        ContainerId(Uuid::from_u128(1))
    }

    #[test]
    fn topic_count_matches_spec_table() {
        // If the spec table grows or shrinks, this guard surfaces it
        // before the publisher quietly drops a new topic on the floor.
        let topics = [
            TOPIC_SET_CREATED,
            TOPIC_SET_DELETED,
            TOPIC_CONTAINER_CREATED,
            TOPIC_CONTAINER_RENAMED,
            TOPIC_CONTAINER_DELETED,
            TOPIC_ITEM_ADDED,
            TOPIC_ITEM_FLAGGED,
            TOPIC_ITEM_COPIED,
            TOPIC_ITEM_MOVED,
            TOPIC_ITEM_REMOVED,
            TOPIC_GC_COMPLETED,
            TOPIC_VERIFY_COMPLETED,
            TOPIC_VERIFY_FAILED,
            TOPIC_CHANGELOG_PRUNED,
        ];
        assert_eq!(topics.len(), TOPIC_COUNT);
        // No duplicates.
        let mut sorted = topics;
        sorted.sort();
        for win in sorted.windows(2) {
            assert_ne!(win[0], win[1], "duplicate topic: {}", win[0]);
        }
    }

    #[test]
    fn every_variant_has_unique_topic() {
        // Pin variant→topic mapping so a future copy/paste mistake
        // (two variants returning the same constant) shows up here.
        let events = [
            MdsEvent::SetCreated(SetCreated {
                set_id: sample_set(),
            }),
            MdsEvent::SetDeleted(SetDeleted {
                set_id: sample_set(),
                blob_count_unreffed: 0,
                duration_ms: 0,
            }),
            MdsEvent::ContainerCreated(ContainerCreated {
                set_id: sample_set(),
                container_id: sample_container(),
                parent_id: None,
                name: "Inbox".into(),
            }),
            MdsEvent::ContainerRenamed(ContainerRenamed {
                set_id: sample_set(),
                container_id: sample_container(),
                old_name: "a".into(),
                new_name: "b".into(),
            }),
            MdsEvent::ContainerDeleted(ContainerDeleted {
                set_id: sample_set(),
                container_id: sample_container(),
            }),
            MdsEvent::ItemAdded(ItemAdded {
                set_id: sample_set(),
                item_id: ItemId(Uuid::nil()),
                blob_hash: "00".repeat(32),
                container_ids: vec![sample_container()],
                change_seqs: vec![ChangeToken(1)],
            }),
            MdsEvent::ItemFlagged(ItemFlagged {
                set_id: sample_set(),
                item_id: ItemId(Uuid::nil()),
                container_id: sample_container(),
                old_flags: 0,
                new_flags: 1,
                change_seq: ChangeToken(2),
            }),
            MdsEvent::ItemCopied(ItemCopied {
                set_id: sample_set(),
                item_id: ItemId(Uuid::nil()),
                dest: sample_container(),
                seq_dest: Seq(1),
                change_seq_dest: ChangeToken(3),
            }),
            MdsEvent::ItemMoved(ItemMoved {
                set_id: sample_set(),
                item_id: ItemId(Uuid::nil()),
                src: sample_container(),
                dest: sample_container(),
                seq_dest: Seq(2),
                change_seq_src: ChangeToken(4),
                change_seq_dest: ChangeToken(5),
            }),
            MdsEvent::ItemRemoved(ItemRemoved {
                set_id: sample_set(),
                item_id: ItemId(Uuid::nil()),
                container_id: sample_container(),
                seq: Seq(1),
                change_seq: ChangeToken(6),
            }),
            MdsEvent::GcCompleted(GcCompleted {
                blobs_deleted: 0,
                bytes_freed: 0,
                duration_ms: 0,
            }),
            MdsEvent::VerifyCompleted(VerifyCompleted {
                scope: "full".into(),
                blobs_checked: 0,
                mismatches: 0,
                duration_ms: 0,
            }),
            MdsEvent::VerifyFailed(VerifyFailed {
                hash: "ab".repeat(32),
                status: 1,
            }),
            MdsEvent::ChangelogPruned(ChangelogPruned {
                set_id: sample_set(),
                stream: "container_change_set".into(),
                rows_removed: 0,
                new_floor: 0,
            }),
        ];
        assert_eq!(events.len(), TOPIC_COUNT);
        let mut seen = std::collections::HashSet::new();
        for e in &events {
            assert!(
                seen.insert(e.topic()),
                "duplicate variant→topic: {}",
                e.topic()
            );
        }
    }

    #[test]
    fn item_added_payload_uses_hex_blob_hash() {
        // Wire form must be hex, not raw bytes — subscribers treat
        // the field as a string.
        let ev = MdsEvent::ItemAdded(ItemAdded {
            set_id: sample_set(),
            item_id: ItemId(Uuid::nil()),
            blob_hash: "ab".repeat(32),
            container_ids: vec![sample_container()],
            change_seqs: vec![ChangeToken(1)],
        });
        let v = ev.payload_json();
        assert!(v["blob_hash"].is_string());
        assert_eq!(v["blob_hash"].as_str().unwrap().len(), 64);
        assert!(v["container_ids"].is_array());
        assert!(v["change_seqs"].is_array());
    }

    #[test]
    fn verify_failed_payload_is_hash_and_status_only() {
        // Spec is `{hash, status}`. No set_id field — that's
        // deliberate (see VerifyFailed doc).
        let ev = MdsEvent::VerifyFailed(VerifyFailed {
            hash: "cd".repeat(32),
            status: 2,
        });
        let v = ev.payload_json();
        let obj = v.as_object().expect("payload is object");
        assert_eq!(obj.len(), 2, "payload had unexpected fields: {obj:?}");
        assert!(obj.contains_key("hash"));
        assert!(obj.contains_key("status"));
    }

    #[test]
    fn item_flagged_payload_serializes_flags_as_u32() {
        let ev = MdsEvent::ItemFlagged(ItemFlagged {
            set_id: sample_set(),
            item_id: ItemId(Uuid::nil()),
            container_id: sample_container(),
            old_flags: 0,
            new_flags: 0x1004,
            change_seq: ChangeToken(7),
        });
        let v = ev.payload_json();
        assert_eq!(v["old_flags"], 0);
        assert_eq!(v["new_flags"], 0x1004);
    }

    #[tokio::test]
    async fn event_sink_emit_round_trips() {
        let (sink, mut rx) = EventSink::with_capacity(8);
        sink.emit(MdsEvent::SetCreated(SetCreated {
            set_id: sample_set(),
        }));
        let got = rx.recv().await.expect("channel still open");
        assert_eq!(got.topic(), TOPIC_SET_CREATED);
    }

    #[tokio::test]
    async fn event_sink_none_is_silent_noop() {
        let sink = EventSink::none();
        // Subscribe must be None for the no-op sink.
        assert!(sink.subscribe().is_none());
        // Emit must not panic.
        sink.emit(MdsEvent::SetCreated(SetCreated {
            set_id: sample_set(),
        }));
    }
}
