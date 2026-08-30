//! The `PropertyStore` trait — atomic record + event commit primitives.
//!
//! SPEC 12 §6.4 / §6.6. Each storage backend implements this trait. The
//! library threads namespace hooks (§6.5) and Bus verbs (§5) above it;
//! the trait itself is the narrow contract for "commit one transition,
//! produce the matching event row, return both atomically".
//!
//! C1 ships only the trait surface. `MemoryStore` (C2) and `SqliteTable`
//! (C3) are the first two implementers.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::lifecycle::Lifecycle;
use crate::namespace::NamespaceName;
use crate::record::{Actor, AuditEpoch, Nseq, Record, RecordEvent, RecordKey, Version};
use cosmix_props_core::value::PropValue;

/// Storage-layer errors. These are the subset of the SPEC 12 §9
/// taxonomy that can arise inside a `PropertyStore` call; each
/// variant maps to one §9 wire `error_code` and the verb layer
/// surfaces it verbatim. The other §9 tokens (`auth_denied`,
/// `hook_error`, `unavailable`) originate above the store — at the
/// Bus layer, the §6.5 hook runner, or the transport — and never
/// appear here. New storage-layer variants require a §9 amendment.
#[derive(Debug, Clone)]
pub enum StoreError {
    /// Maps to wire `error_code: not_found`.
    NotFound,
    /// SPEC 12 §4.4 / §9 — optimistic-concurrency mismatch. Caller's
    /// `if_version` did not match the current version (or the record
    /// did not exist when a non-zero `if_version` was supplied). Maps
    /// to wire `error_code: version_mismatch`; `current` carries the
    /// actual current value so the verb layer can surface it as the
    /// body's `current_version` field per §9.
    VersionMismatch { expected: Version, current: Version },
    /// Backend-level failure (disk error, lock contention, SQL error,
    /// corrupt schema bytes). Maps to wire `error_code: storage_error`.
    Storage { message: String },
    /// SPEC 12 §6.5 invariant violation — caller wrote a substrate-
    /// reserved field (`_lifecycle`), the cardinality didn't match
    /// (singleton key supplied for a collection or vice versa),
    /// or a field failed type/range/regex/`before_set` validation
    /// inside the storage path. Maps to wire `error_code:
    /// validation_error` per §9; the verb layer surfaces the
    /// `fields` body array.
    Validation { message: String },
    /// SPEC 12 §9 — uniqueness constraint violated (e.g. creating a
    /// record whose primary-key field already exists, or a
    /// `unique: true` field collision). Maps to wire `error_code:
    /// conflict`.
    Conflict { message: String },
    /// SPEC 12 §5.5 / §9 — a watch replay request supplied a
    /// `since_nseq` that has aged out of the namespace's
    /// `replay_window` (§6.6). The caller MUST re-list to recover.
    /// Maps to wire `error_code: replay_window_exceeded`.
    ReplayWindowExceeded { since_nseq: Nseq, oldest_nseq: Nseq },
}

impl StoreError {
    pub fn storage(msg: impl Into<String>) -> Self {
        Self::Storage {
            message: msg.into(),
        }
    }
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation {
            message: msg.into(),
        }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict {
            message: msg.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "not_found"),
            Self::VersionMismatch { expected, current } => write!(
                f,
                "version_mismatch: expected {expected:?}, current {current:?}"
            ),
            Self::Storage { message } => write!(f, "storage_error: {message}"),
            Self::Validation { message } => write!(f, "validation_error: {message}"),
            Self::Conflict { message } => write!(f, "conflict: {message}"),
            Self::ReplayWindowExceeded {
                since_nseq,
                oldest_nseq,
            } => write!(
                f,
                "replay_window_exceeded: since_nseq={since_nseq:?}, oldest_nseq={oldest_nseq:?}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// Future alias used by the trait return types.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send + 'a>>;

/// Inputs to `commit_set`. Bundled so the trait signature stays readable
/// and future fields (TTL, conditional-write predicates) extend without
/// breaking every impl.
#[derive(Debug, Clone)]
pub struct SetCommit {
    pub key: RecordKey,
    pub new_value: PropValue,
    /// SPEC 12 §5.3 — `None` ⇒ upsert (omitted `if_version`,
    /// last-write-wins, creates if absent). `Some(v)` ⇒ require current
    /// version to equal `v` (optimistic concurrency); callers wanting
    /// create-new MUST pass `Some(Version::zero())`, which only succeeds
    /// when the record does not yet exist.
    pub expected_version: Option<Version>,
    /// SPEC 12 §5.3 — `Patch` (default, wire `merge: true`) merges
    /// `new_value`'s fields with the existing record; `Replace` (wire
    /// `merge: false`) substitutes the record wholesale. The merge
    /// happens inside the backend transaction so that the read-merge-
    /// write trio is atomic against concurrent writers — the verb
    /// layer MUST NOT do its own read-modify-write before calling
    /// `commit_set`.
    pub merge: MergeMode,
    /// `Some` for Saga namespaces (always `Provisioning` on the initial
    /// set); `None` for Simple.
    pub lifecycle: Option<Lifecycle>,
    pub actor: Actor,
    /// SPEC 12 §5.5 — optional Bus request id of the originating
    /// `<svc>.props.set`. Stored on the resulting `RecordEvent.cause`
    /// so subscribers can correlate `set → complete` pairs for Saga
    /// namespaces and trace caller-side request flows. `None` for
    /// internally-driven sets (none in v0.1).
    pub cause: Option<String>,
    pub ts_ms: i64,
}

/// SPEC 12 §5.3 — PATCH vs PUT semantics on `<svc>.props.set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeMode {
    /// Merge supplied fields with the existing record (wire
    /// `merge: true`). Default.
    #[default]
    Patch,
    /// Replace the entire record with the supplied value (wire
    /// `merge: false`).
    Replace,
}

/// Inputs to `commit_delete`.
#[derive(Debug, Clone)]
pub struct DeleteCommit {
    pub key: RecordKey,
    pub expected_version: Option<Version>,
    pub actor: Actor,
    /// SPEC 12 §5.5 — optional Bus request id of the originating
    /// `<svc>.props.delete`. Stored on the resulting `RecordEvent.cause`.
    pub cause: Option<String>,
    pub ts_ms: i64,
}

/// Inputs to `commit_complete` — saga lifecycle transition.
/// `lifecycle` is either `Active` or `Failed{reason}`; the substrate
/// library forbids `Provisioning` here.
#[derive(Debug, Clone)]
pub struct CompleteCommit {
    pub key: RecordKey,
    pub lifecycle: Lifecycle,
    /// The `version` returned by the originating `commit_set` for this
    /// saga. The store MUST refuse the complete with
    /// `StoreError::VersionMismatch` if the record's current version
    /// differs — preventing a stale `after_set` outcome from being
    /// applied to a row that was overwritten by a concurrent set. The
    /// runtime captures this from `commit_set`'s return and passes it
    /// here.
    pub expected_version: Version,
    /// SPEC 12 §5.5 / §10 — completed events MUST carry
    /// `daemon:<svc>` (the library transition is attributed to the
    /// owning daemon, not the original caller). The dispatcher builds
    /// this via [`Actor::daemon_complete`] before handing the commit
    /// to the store.
    pub actor: Actor,
    /// SPEC 12 §5.5 — preserved `cause` from the originating
    /// `<svc>.props.set` so subscribers can correlate set → complete
    /// pairs across the saga lifecycle. `None` only when the
    /// originating set itself was uncorrelated.
    pub cause: Option<String>,
    pub ts_ms: i64,
}

/// Inputs to `commit_reconcile` — hand-edit recovery synthetic event.
/// Bumps `audit_epoch` and creates a fresh `nseq`. The pre/post values
/// are inputs to the in-memory diff that produces `fields_changed` on
/// the resulting `RecordEvent`; per §10 the resulting row carries no
/// value bytes.
#[derive(Debug, Clone)]
pub struct ReconcileCommit {
    pub key: RecordKey,
    /// The on-disk bytes the substrate observed during reconciliation
    /// (post hand-edit), promoted to the new authoritative value.
    pub new_value: PropValue,
    /// The pre-edit value the sidecar remembered. Used to compute
    /// `fields_changed`; not stored on the event row.
    pub old_value: Option<PropValue>,
    pub ts_ms: i64,
}

/// A read snapshot paired with the namespace's observed `nseq` at the
/// moment the read occurred. SPEC 12 §5.5 requires `list` / `get`
/// responses (structured mode) to carry `nseq:` so the caller can
/// close the list-then-watch race with `since_nseq = nseq`.
///
/// The per-record `Record.nseq` is the last `nseq` at which that
/// specific record changed — it can be arbitrarily stale relative to
/// the namespace's current `nseq` if other records have been written
/// since. `Snapshot::observed_nseq` is the namespace-wide value.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot<T> {
    pub value: T,
    pub observed_nseq: Nseq,
}

impl<T> Snapshot<T> {
    pub fn new(value: T, observed_nseq: Nseq) -> Self {
        Self {
            value,
            observed_nseq,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Snapshot<U> {
        Snapshot {
            value: f(self.value),
            observed_nseq: self.observed_nseq,
        }
    }
}

/// Atomic storage primitive: commit one state transition + its event
/// row, return both. Implementers MUST guarantee either both writes are
/// visible to subsequent reads or neither is.
pub trait PropertyStore: Send + Sync {
    /// Read one record paired with the namespace's snapshot `nseq`.
    /// Returns `Err(NotFound)` for absent keys. Per §5.5 the
    /// `observed_nseq` is the namespace-wide cursor at read time —
    /// callers feed it back as `since_nseq` on `<svc>.props.watch`.
    fn get<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Snapshot<Record>>;

    /// Enumerate all records in a namespace paired with the namespace's
    /// snapshot `nseq`. Collection ⇒ many, singleton ⇒ at most one.
    /// Per §5.5 the `observed_nseq` MUST be the same read tx as the
    /// listed rows (i.e. atomic against concurrent writes); the
    /// caller feeds it back as `since_nseq` on watch to guarantee
    /// gap-free replay.
    fn list<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, Snapshot<Vec<Record>>>;

    /// Atomic record write + event-history append for a `Set` transition.
    /// Returns the new `Record` and the just-appended `RecordEvent`.
    fn commit_set<'a>(&'a self, op: SetCommit) -> StoreFuture<'a, (Record, RecordEvent)>;

    /// Atomic record removal + event-history append for a `Delete`
    /// transition. Returns only the event row; the record is gone.
    fn commit_delete<'a>(&'a self, op: DeleteCommit) -> StoreFuture<'a, RecordEvent>;

    /// Atomic `_lifecycle` flip + event-history append for the
    /// `Complete` transition. Library-internal; not reachable from the
    /// wire. The record must currently carry `Lifecycle::Provisioning`
    /// or the impl returns `StoreError::Validation`.
    fn commit_complete<'a>(&'a self, op: CompleteCommit) -> StoreFuture<'a, (Record, RecordEvent)>;

    /// Synthetic reconcile event — promote the on-disk bytes to the
    /// authoritative value, bump `audit_epoch`, append the event row.
    fn commit_reconcile<'a>(
        &'a self,
        op: ReconcileCommit,
    ) -> StoreFuture<'a, (Record, RecordEvent)>;

    /// Tail the event history for a namespace, returning events with
    /// `nseq > since_nseq` (strictly greater than — matches the SPEC
    /// 12 §5.5 wire contract for watch replay). Pass `Nseq(0)` to
    /// replay from the start of the retained window. Returns
    /// committed rows only; in-flight transactions MUST NOT be
    /// visible. Returns `StoreError::ReplayWindowExceeded` if
    /// `since_nseq` has aged out of the namespace's `replay_window`
    /// (§6.6); callers MUST re-list and retry from the snapshot's
    /// `nseq`.
    ///
    /// **Cross-call visibility contract (C10c, load-bearing).** Watch
    /// replay invokes `list()` to capture `observed_nseq`, then calls
    /// `events_since(ns, since)`. Every event whose `nseq` is `≤
    /// observed_nseq` returned by the prior `list()` MUST be visible
    /// to this `events_since` call. Equivalently: each commit appends
    /// to the event log and advances the snapshot cursor in a single
    /// linearisation step. Backends that violate this (e.g. cursor
    /// advanced before event row visible) would leave a permanent
    /// `nseq` hole in the replay+live union that no client retry can
    /// recover. The provided `MemoryStore` and `SqliteStore` satisfy
    /// this via per-namespace lock and single-transaction commit
    /// (writer uses `BEGIN IMMEDIATE`; both `list` and `events_since`
    /// open fresh read transactions that see all prior committed
    /// state, so any commit that advanced the cursor is also visible
    /// in the event log on the next read). New backends MUST
    /// implement the same barrier.
    fn events_since<'a>(
        &'a self,
        namespace: &'a NamespaceName,
        since_nseq: Nseq,
    ) -> StoreFuture<'a, Vec<RecordEvent>>;

    /// The current audit epoch for a namespace (bumped on reconcile).
    fn audit_epoch<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, AuditEpoch>;

    /// Tombstone-aware OCC anchor for the next write to `key`.
    ///
    /// Returns `Some(version)` if a record OR a tombstone exists at this
    /// key — that version is what `commit_set` will compare
    /// `expected_version` against. Returns `None` only when the key is
    /// truly absent (never written, or hard-deleted).
    ///
    /// SPEC 12 §5.4: `get()` hides tombstones (returns `NotFound`) but
    /// `commit_set` still validates `expected_version` against the
    /// tombstone version. Callers that want to write to a freshly-deleted
    /// key in a SoftDelete namespace MUST anchor against this method,
    /// not against `get()`'s `NotFound`. Use:
    /// - `None`           ⇒ pass `Version::zero()` (create-new).
    /// - `Some(version)`  ⇒ pass `version` (covers live or tombstone).
    fn version_anchor<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Option<Version>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: the trait is object-safe via boxed futures.
    #[allow(dead_code)]
    fn assert_object_safe(_: &dyn PropertyStore) {}

    #[test]
    fn store_error_display() {
        assert_eq!(StoreError::NotFound.to_string(), "not_found");
        assert_eq!(
            StoreError::VersionMismatch {
                expected: Version(1),
                current: Version(2)
            }
            .to_string(),
            "version_mismatch: expected Version(1), current Version(2)"
        );
        assert_eq!(
            StoreError::storage("disk full").to_string(),
            "storage_error: disk full"
        );
        assert_eq!(
            StoreError::validation("can't write _lifecycle").to_string(),
            "validation_error: can't write _lifecycle"
        );
        assert_eq!(
            StoreError::conflict("primary key already exists").to_string(),
            "conflict: primary key already exists"
        );
        assert_eq!(
            StoreError::ReplayWindowExceeded {
                since_nseq: Nseq(5),
                oldest_nseq: Nseq(100)
            }
            .to_string(),
            "replay_window_exceeded: since_nseq=Nseq(5), oldest_nseq=Nseq(100)"
        );
    }

    #[test]
    fn snapshot_map_preserves_observed_nseq() {
        let s = Snapshot::new(7i32, Nseq(42));
        let s2 = s.map(|n| n.to_string());
        assert_eq!(s2.value, "7");
        assert_eq!(s2.observed_nseq, Nseq(42));
    }
}
