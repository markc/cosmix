//! Opt-in, in-memory, revisioned write store — the generic control-write
//! facility (Q8).
//!
//! This is **not** the heavyweight persistent SPEC-12 mutation store
//! (`cosmix-lib-props-store`, cos repo): no on-disk backend, no audit HMAC,
//! no namespace schema. It is a lightweight in-memory ledger a daemon opts
//! into to accept *revisioned* control writes against its property paths and
//! publish coalesced `props.changed` events. Read-only `PropTree` consumers
//! and `bus::dispatch_props` are untouched — a daemon holds a
//! [`RevWriteStore`] alongside (or behind) its `PropTree` only if it wants
//! writes.
//!
//! ## What it standardises
//!
//! - **A global monotonic revision counter.** Every accepted write bumps it;
//!   the value assigned is that write's authoritative revision. Revisions
//!   never repeat and never decrease, so any observer can safely *ignore an
//!   update whose revision is not newer than the last it saw* (see
//!   [`accept_if_newer`]).
//! - **Per-path canonical state**: the current `{value, revision, source_id,
//!   op_id}` for each written path.
//! - **Server-receive-order authority.** [`RevWriteStore::apply`] processes
//!   writes in call order; there is no reordering. The last accepted write to
//!   a path wins.
//! - **Optimistic concurrency** via `if_revision`: a write may require the
//!   path to currently sit at an expected revision; a mismatch is
//!   [`RevWriteResponse::Rejected`] carrying the store's *current* revision +
//!   value so the caller can rebase.
//! - **Per-path coalescing** for the change stream: many writes to one path
//!   between drains collapse to that path's terminal state in
//!   [`RevWriteStore::drain_changed`].
//! - **A guaranteed terminal own-op echo**: `apply` always returns a
//!   [`RevWriteAck`] echoing the accepted `revision` + `canonical_value` +
//!   `op_id`, so a writer receives a definitive confirmation of *its own* op
//!   even when the broadcast stream later coalesces that path away under a
//!   newer write.
//!
//! ## What it does not do
//!
//! No domain validation, quantisation, or canonicalisation — the facility is
//! generic (no `mixer.v1` knowledge). A domain layer validates/quantises the
//! value (e.g. via `cosmix-mixer-schema::validate_write` +
//! `canonical_repr`) and hands `apply` the already-canonical
//! [`PropValue`]; the store stores and echoes exactly that. It is also not
//! internally synchronised: wrap it in a `Mutex`/`RwLock` for shared use.

use crate::path::PropPath;
use crate::value::PropValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A revisioned control write against a single path.
///
/// `if_revision` gates the write on the path currently sitting at an expected
/// revision (optimistic concurrency); `None` = unconditional.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevWriteRequest {
    pub path: PropPath,
    pub value: PropValue,
    /// Caller-chosen correlation id echoed back on the ack. NOT an idempotency
    /// key — retries are not deduplicated; every `apply()` is a distinct write.
    pub op_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub if_revision: Option<u64>,
}

impl RevWriteRequest {
    /// Unconditional write.
    pub fn new(path: PropPath, value: PropValue, op_id: impl Into<String>) -> Self {
        Self {
            path,
            value,
            op_id: op_id.into(),
            if_revision: None,
        }
    }

    /// Gate the write on an expected current revision.
    pub fn if_revision(mut self, rev: u64) -> Self {
        self.if_revision = Some(rev);
        self
    }
}

/// Acknowledgement of an accepted write: the authoritative revision plus the
/// canonical value the store now holds. This is the terminal own-op echo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevWriteAck {
    pub revision: u64,
    pub path: PropPath,
    pub canonical_value: PropValue,
    /// Authenticated source identity of the writer, supplied to `apply`.
    pub source_id: String,
    pub op_id: String,
}

/// A rejected write carries the store's current authoritative revision + value
/// so the caller can rebase and retry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevWriteReject {
    pub path: PropPath,
    pub op_id: String,
    pub current_revision: u64,
    pub current_value: PropValue,
    pub reason: String,
}

/// The response to a [`RevWriteRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum RevWriteResponse {
    Accepted(RevWriteAck),
    Rejected(RevWriteReject),
}

impl RevWriteResponse {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
    pub fn ack(&self) -> Option<&RevWriteAck> {
        match self {
            Self::Accepted(a) => Some(a),
            Self::Rejected(_) => None,
        }
    }
    pub fn reject(&self) -> Option<&RevWriteReject> {
        match self {
            Self::Rejected(r) => Some(r),
            Self::Accepted(_) => None,
        }
    }
}

/// One coalesced change for the daemon to publish as a `props.changed` event:
/// the terminal state of a path since the last drain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangedProp {
    pub path: PropPath,
    pub revision: u64,
    pub canonical_value: PropValue,
    pub source_id: String,
    pub op_id: String,
}

/// Current authoritative state of one path.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    value: PropValue,
    revision: u64,
    source_id: String,
    op_id: String,
}

/// An opt-in in-memory revisioned write store. Not internally synchronised —
/// wrap in a `Mutex`/`RwLock` for concurrent use. See the module docs.
#[derive(Debug, Default)]
pub struct RevWriteStore {
    /// Global monotonic revision; the last value assigned to an accepted write.
    revision: u64,
    entries: BTreeMap<PropPath, Entry>,
    /// Coalesced set of paths changed since the last drain (value is unit;
    /// the terminal state is read from `entries` at drain time).
    dirty: BTreeMap<PropPath, ()>,
}

impl RevWriteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current global revision — the value assigned to the most recent
    /// accepted write (0 before any write).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The current authoritative revision of a single path (0 if the path has
    /// never been written or seeded).
    pub fn path_revision(&self, path: &PropPath) -> u64 {
        self.entries.get(path).map(|e| e.revision).unwrap_or(0)
    }

    /// The current canonical value of a path, if any.
    pub fn get(&self, path: &PropPath) -> Option<&PropValue> {
        self.entries.get(path).map(|e| &e.value)
    }

    /// Seed an initial value for a path at revision 0 **without** bumping the
    /// global counter or marking it dirty — for a daemon installing its
    /// startup defaults so `if_revision`-gated writes and reject echoes have a
    /// canonical value to compare against. A no-op if the path already exists.
    pub fn seed(&mut self, path: PropPath, value: PropValue, source_id: impl Into<String>) {
        self.entries.entry(path).or_insert(Entry {
            value,
            revision: 0,
            source_id: source_id.into(),
            op_id: String::new(),
        });
    }

    /// Apply one write. Server receive order is authoritative: this is the
    /// serialisation point. On accept the global revision is bumped, the path's
    /// state replaced, the path marked dirty (coalescing), and a terminal
    /// [`RevWriteAck`] returned. On an `if_revision` mismatch the write is
    /// rejected with the store's current revision + value.
    ///
    /// `source_id` is the authenticated identity of the writer (the caller
    /// resolves it from the Bus envelope / session); the store records and
    /// echoes it but does not interpret it.
    pub fn apply(
        &mut self,
        req: RevWriteRequest,
        source_id: impl Into<String>,
    ) -> RevWriteResponse {
        let source_id = source_id.into();
        let cur_rev = self.path_revision(&req.path);

        if let Some(expected) = req.if_revision
            && expected != cur_rev
        {
            let current_value = self
                .entries
                .get(&req.path)
                .map(|e| e.value.clone())
                .unwrap_or(PropValue::Null);
            return RevWriteResponse::Rejected(RevWriteReject {
                path: req.path,
                op_id: req.op_id,
                current_revision: cur_rev,
                current_value,
                reason: format!("if_revision {expected} does not match current revision {cur_rev}"),
            });
        }

        // Accept. Checked increment — u64 will not wrap in any real lifetime,
        // but a wrap would silently break monotonicity, so treat it as the
        // invariant violation it is rather than eventually rolling over.
        self.revision = self
            .revision
            .checked_add(1)
            .expect("RevWriteStore revision counter overflowed u64");
        let rev = self.revision;
        self.entries.insert(
            req.path.clone(),
            Entry {
                value: req.value.clone(),
                revision: rev,
                source_id: source_id.clone(),
                op_id: req.op_id.clone(),
            },
        );
        // Coalesce: repeated writes to a path collapse to one dirty entry;
        // the terminal state is read from `entries` at drain time.
        self.dirty.insert(req.path.clone(), ());

        RevWriteResponse::Accepted(RevWriteAck {
            revision: rev,
            path: req.path,
            canonical_value: req.value,
            source_id,
            op_id: req.op_id,
        })
    }

    /// True if any paths are pending in the change stream.
    pub fn has_changes(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Drain the coalesced change set: the terminal state (latest value,
    /// revision, source, op) of every path written since the last drain,
    /// ordered by path. Clears the pending set. This is what a daemon publishes
    /// as `props.changed`.
    pub fn drain_changed(&mut self) -> Vec<ChangedProp> {
        let dirty = std::mem::take(&mut self.dirty);
        dirty
            .into_keys()
            .filter_map(|p| {
                self.entries.get(&p).map(|e| ChangedProp {
                    path: p,
                    revision: e.revision,
                    canonical_value: e.value.clone(),
                    source_id: e.source_id.clone(),
                    op_id: e.op_id.clone(),
                })
            })
            .collect()
    }
}

/// Client-side merge helper implementing the "ignore older revisions" rule: a
/// consumer keeps a per-path cache of the newest [`ChangedProp`] it has seen;
/// an incoming change is applied only if its revision is strictly newer than
/// the cached one. Returns `true` if it was applied.
///
/// The store's monotonic revisions guarantee this is safe against duplicate or
/// out-of-order delivery of the change stream.
pub fn accept_if_newer(cache: &mut BTreeMap<PropPath, ChangedProp>, incoming: ChangedProp) -> bool {
    match cache.get(&incoming.path) {
        Some(existing) if existing.revision >= incoming.revision => false,
        _ => {
            cache.insert(incoming.path.clone(), incoming);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> PropPath {
        PropPath::new(s).unwrap()
    }

    fn req(p: &str, v: impl Into<PropValue>, op: &str) -> RevWriteRequest {
        RevWriteRequest::new(path(p), v.into(), op)
    }

    #[test]
    fn revision_is_monotonic_across_paths() {
        let mut s = RevWriteStore::new();
        assert_eq!(s.revision(), 0);
        let r1 = s.apply(req("mixer.master.fader", -6.0, "op1"), "sess-a");
        let r2 = s.apply(req("mixer.channels.0.mute", true, "op2"), "sess-b");
        let r3 = s.apply(req("mixer.master.fader", -3.0, "op3"), "sess-a");
        assert_eq!(r1.ack().unwrap().revision, 1);
        assert_eq!(r2.ack().unwrap().revision, 2);
        assert_eq!(r3.ack().unwrap().revision, 3);
        assert_eq!(s.revision(), 3);
        // Per-path revision tracks the last accepted write to that path.
        assert_eq!(s.path_revision(&path("mixer.master.fader")), 3);
        assert_eq!(s.path_revision(&path("mixer.channels.0.mute")), 2);
        assert_eq!(s.path_revision(&path("never.written")), 0);
    }

    #[test]
    fn if_revision_match_accepts() {
        let mut s = RevWriteStore::new();
        s.apply(req("a.b", 1.0, "op1"), "src"); // rev 1
        let ok = s.apply(req("a.b", 2.0, "op2").if_revision(1), "src");
        assert!(ok.is_accepted());
        assert_eq!(ok.ack().unwrap().revision, 2);
    }

    #[test]
    fn if_revision_mismatch_rejects_with_current_state() {
        let mut s = RevWriteStore::new();
        s.apply(req("a.b", 1.0, "op1"), "src"); // rev 1
        s.apply(req("a.b", 2.0, "op2"), "src"); // rev 2
        // Client thought it was still at rev 1.
        let rej = s.apply(req("a.b", 9.0, "op3").if_revision(1), "src");
        assert!(!rej.is_accepted());
        let r = rej.reject().unwrap();
        assert_eq!(r.op_id, "op3");
        assert_eq!(r.current_revision, 2);
        assert_eq!(r.current_value, PropValue::from(2.0));
        // The rejected write did not mutate the store.
        assert_eq!(s.revision(), 2);
        assert_eq!(s.get(&path("a.b")), Some(&PropValue::from(2.0)));
    }

    #[test]
    fn if_revision_zero_gates_first_write() {
        let mut s = RevWriteStore::new();
        // Path unseeded/unwritten -> current revision 0; expecting 0 accepts.
        let ok = s.apply(req("fresh.leaf", 5.0, "op1").if_revision(0), "src");
        assert!(ok.is_accepted());
        // A second create-only write (expects 0) now conflicts.
        let rej = s.apply(req("fresh.leaf", 6.0, "op2").if_revision(0), "src");
        let r = rej.reject().unwrap();
        assert_eq!(r.current_revision, 1);
        assert_eq!(r.current_value, PropValue::from(5.0));
    }

    #[test]
    fn seed_does_not_bump_revision_but_provides_reject_value() {
        let mut s = RevWriteStore::new();
        s.seed(path("a.b"), PropValue::from(0.0), "default");
        assert_eq!(s.revision(), 0);
        assert_eq!(s.path_revision(&path("a.b")), 0);
        // A stale-expectation write against the seeded path echoes the seed.
        let rej = s.apply(req("a.b", 1.0, "op1").if_revision(7), "src");
        let r = rej.reject().unwrap();
        assert_eq!(r.current_revision, 0);
        assert_eq!(r.current_value, PropValue::from(0.0));
    }

    #[test]
    fn coalescing_collapses_repeated_writes_to_terminal_state() {
        let mut s = RevWriteStore::new();
        s.apply(req("a.b", 1.0, "op1"), "src");
        s.apply(req("a.b", 2.0, "op2"), "src");
        s.apply(req("a.b", 3.0, "op3"), "src");
        s.apply(req("c.d", 9.0, "op4"), "src");
        let changed = s.drain_changed();
        // Two distinct paths, ordered by path; a.b coalesced to its terminal
        // value + revision (op3 / rev 3), not three separate events.
        assert_eq!(changed.len(), 2);
        assert_eq!(changed[0].path, path("a.b"));
        assert_eq!(changed[0].revision, 3);
        assert_eq!(changed[0].canonical_value, PropValue::from(3.0));
        assert_eq!(changed[0].op_id, "op3");
        assert_eq!(changed[1].path, path("c.d"));
        assert_eq!(changed[1].revision, 4);
        // Drained -> nothing pending.
        assert!(!s.has_changes());
        assert!(s.drain_changed().is_empty());
    }

    #[test]
    fn terminal_own_op_echo_survives_later_coalescing() {
        let mut s = RevWriteStore::new();
        // op-A writes the path and gets a terminal ack echoing its own op.
        let a = s.apply(req("a.b", 1.0, "op-A"), "sess-a");
        let ack_a = a.ack().unwrap();
        assert_eq!(ack_a.op_id, "op-A");
        assert_eq!(ack_a.revision, 1);
        assert_eq!(ack_a.canonical_value, PropValue::from(1.0));
        assert_eq!(ack_a.source_id, "sess-a");
        // A later op-B by another source coalesces the path in the broadcast
        // stream, but op-A already received its definitive own-op echo above.
        let b = s.apply(req("a.b", 2.0, "op-B"), "sess-b");
        assert_eq!(b.ack().unwrap().op_id, "op-B");
        let changed = s.drain_changed();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].op_id, "op-B"); // coalesced to newest
        assert_eq!(changed[0].source_id, "sess-b");
    }

    #[test]
    fn older_revision_ignored_by_client_merge() {
        let mut cache: BTreeMap<PropPath, ChangedProp> = BTreeMap::new();
        let newer = ChangedProp {
            path: path("a.b"),
            revision: 5,
            canonical_value: PropValue::from(2.0),
            source_id: "s".into(),
            op_id: "op5".into(),
        };
        let older = ChangedProp {
            path: path("a.b"),
            revision: 3,
            canonical_value: PropValue::from(1.0),
            source_id: "s".into(),
            op_id: "op3".into(),
        };
        assert!(accept_if_newer(&mut cache, newer));
        // Replayed / out-of-order older revision is ignored.
        assert!(!accept_if_newer(&mut cache, older));
        assert_eq!(cache[&path("a.b")].revision, 5);
        // An equal revision (duplicate delivery) is also ignored.
        let dup = ChangedProp {
            path: path("a.b"),
            revision: 5,
            canonical_value: PropValue::from(2.0),
            source_id: "s".into(),
            op_id: "op5".into(),
        };
        assert!(!accept_if_newer(&mut cache, dup));
    }

    #[test]
    fn response_serialises_with_status_tag() {
        let mut s = RevWriteStore::new();
        let ok = s.apply(req("a.b", 1.0, "op1"), "src");
        let j = serde_json::to_value(&ok).unwrap();
        assert_eq!(j["status"], "accepted");
        assert_eq!(j["revision"], 1);
        assert_eq!(j["canonical_value"], 1.0);
        assert_eq!(j["op_id"], "op1");

        let rej = s.apply(req("a.b", 2.0, "op2").if_revision(99), "src");
        let j = serde_json::to_value(&rej).unwrap();
        assert_eq!(j["status"], "rejected");
        assert_eq!(j["current_revision"], 1);
    }
}
