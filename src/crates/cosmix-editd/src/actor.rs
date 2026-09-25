//! Per-buffer actors (plan D10, §3.5).
//!
//! One tokio task per buffer processes that buffer's commands in receive order
//! (the single-authority OT order). Its inbox holds `ACTOR_INBOX` commands; the
//! router `try_send`s and answers RESOURCE_LIMIT `busy` when it is full. File I/O
//! (including the initial load) runs as `spawn_blocking` jobs awaited inside
//! this sequence. Watcher notifications never enter the inbox: they set a
//! "recheck disk" flag and wake a `Notify`, so a burst costs one `stat`.
//!
//! # Contract: op_id dedup (frozen; plan §3.5)
//! Owned here, not in the core (the core only records and echoes `op_id`).
//! - Key = [`DedupKey`] `(caller_key, origin, verb, op_id)`, all fixed BEFORE
//!   any lane selection — a retried `edit.undo origin:"*"` hits the cached
//!   reply even if another lane is now newest.
//! - Only SUCCESSFUL replies are cached; a refused request can be retried with
//!   the same `op_id`.
//! - The cache holds the full encoded compact reply, `DEDUP_ENTRIES` per buffer,
//!   LRU; a hit returns it with `duplicate: true` and applies nothing.
//! - The cache dies with the buffer and the epoch: after a restart a retry gets
//!   NOT_FOUND `epoch_mismatch`, never a double apply.
//! - Two anonymous callers collide only if they claim the same origin AND reuse
//!   an `op_id` ("op_ids must be unique per caller").

use crate::caller::CallerKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub caller: CallerKey,
    /// The resolved claimed origin (`kind:label`), not the undo lane.
    pub origin: String,
    pub verb: String,
    pub op_id: String,
}

/// LRU of successful encoded replies. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct DedupCache {
    pub entries: std::collections::VecDeque<(DedupKey, String)>,
}

impl DedupCache {
    /// The cached reply body for `key`, if any.
    pub fn get(&self, key: &DedupKey) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}
