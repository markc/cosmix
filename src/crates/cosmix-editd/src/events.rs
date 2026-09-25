//! The ordered publisher (plan §4.6).
//!
//! One task publishes `edit.changed` and `edit.props.changed` via
//! `noded topic.publish` with headers `name=<topic>`, `retain=false` (powerd
//! transport). Inner frame command: `edit.changed` / `props.changed`.
//!
//! # Contract: loss is always announced (frozen)
//! - An event whose encoded size exceeds `MAX_EVENT_BYTES` is replaced by
//!   `resync {buffers: [b], reason: "oversized", rev}`.
//! - Pending events are budgeted by `MAX_PUBLISH_QUEUE_BYTES`. A dropped event
//!   (budget or send failure) marks its buffer `resync_pending`; pending
//!   resyncs are published AHEAD of later events and retried with backoff
//!   (`RESYNC_BACKOFF_BASE_MS` ×2, cap `RESYNC_BACKOFF_CAP_MS`) until delivered.
//! - On every supervised-client reconnect edge (`subscribe_state`) publish
//!   `resync {buffers: "all", reason: "reconnect"}`.
//! - `event_seq` is daemon-session monotonic; every event carries `epoch`.
//!
//! Mirror rule (documented for clients): apply `edit` events whose `base_rev`
//! equals the mirror's rev, in list order; on a `base_rev` mismatch, an
//! `event_seq` gap, a `resync` naming the buffer (or `all`) or an epoch change,
//! refetch with `edit.get snapshot:true`.

use std::collections::BTreeSet;

use cosmix_edit_core::wire::BufferId;

/// Buffers owed a `resync`. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct ResyncPending {
    pub buffers: BTreeSet<BufferId>,
    pub all: bool,
}
