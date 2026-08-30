//! Record addressing, versioning, and event-history shapes.
//!
//! SPEC 12 §4.1 (address shape), §5.5 / §6.6 (event row), §10 (audit).
//!
//! A `Record` is the daemon-visible value at a `RecordKey`; `RecordEvent`
//! is the durable history row appended in the same transaction as the
//! record write. `AuditRow` is the audit-stream projection of the same
//! event, carrying the keyed-HMAC `audit_digest` (computed in C5).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::lifecycle::Lifecycle;
use crate::namespace::NamespaceName;
use cosmix_props_core::value::PropValue;

/// Optimistic-concurrency version per record, bumped on every state
/// transition (set, delete-tombstone, saga complete, reconcile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(pub u64);

impl Version {
    pub fn zero() -> Self {
        Self(0)
    }
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Monotonic per-namespace event sequence — dispatcher resume cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Nseq(pub u64);

impl Nseq {
    pub fn zero() -> Self {
        Self(0)
    }
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Audit epoch, bumped on hand-edit reconciliation. HMAC continuity is
/// broken across an epoch boundary by design (SPEC 12 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditEpoch(pub u64);

impl AuditEpoch {
    pub fn zero() -> Self {
        Self(0)
    }
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// SPEC 12 §4.1 — one segment of a `field_path`. Serializes as a JSON
/// string (`Field`) or JSON integer (`Index`); a `field_path` on the
/// wire is the JSON array `[seg, seg, ...]`. This shape supports
/// addressing list elements (`["allowed_senders", 0]`) and nested
/// struct fields (`["profile", "display_name"]`) unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldPathSegment {
    Field(String),
    Index(u64),
}

impl FieldPathSegment {
    pub fn field(s: impl Into<String>) -> Self {
        Self::Field(s.into())
    }
    pub fn index(i: u64) -> Self {
        Self::Index(i)
    }
}

/// SPEC 12 §4.1 — structured record address.
///
/// On the wire this materialises as three Bus headers: `namespace:`
/// (the unqualified name), `key:` (UTF-8 string, `""` for singletons),
/// `field_path:` (JSON array of strings and integers; omitted ⇒ whole
/// record).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordKey {
    pub namespace: NamespaceName,
    /// Record key. `""` for singleton namespaces — the substrate fills
    /// the namespace's declared `canonical_key` into responses.
    #[serde(default)]
    pub key: String,
    /// JSON-array sub-selector inside the record; empty for whole-record
    /// operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_path: Vec<FieldPathSegment>,
}

impl RecordKey {
    pub fn singleton(namespace: NamespaceName) -> Self {
        Self {
            namespace,
            key: String::new(),
            field_path: Vec::new(),
        }
    }

    pub fn collection(namespace: NamespaceName, key: impl Into<String>) -> Self {
        Self {
            namespace,
            key: key.into(),
            field_path: Vec::new(),
        }
    }

    pub fn with_field_path(mut self, p: Vec<FieldPathSegment>) -> Self {
        self.field_path = p;
        self
    }

    pub fn is_whole_record(&self) -> bool {
        self.field_path.is_empty()
    }
}

/// The current value of a record, with substrate-managed metadata.
/// `lifecycle` is populated iff the owning namespace is `Saga`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub key: RecordKey,
    pub value: PropValue,
    pub version: Version,
    pub nseq: Nseq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,
}

/// SPEC 12 §5.5 / §6.6 — `kind` body field on records.changed events.
/// Not to be confused with the storage-history `verb` string
/// (`<svc>.props.{set,delete,complete,reconcile}`), which is derived
/// from `kind` plus the owning service via [`EventKind::verb_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Caller-driven `<svc>.props.set` that materialised a new record.
    Created,
    /// Caller-driven `<svc>.props.set` against an existing record.
    Updated,
    /// Caller-driven `<svc>.props.delete`.
    Deleted,
    /// Library-internal saga lifecycle transition (Provisioning →
    /// Active|Failed). Wire verb is `<svc>.props.complete`.
    Completed,
    /// Synthetic startup-reconciliation event after hand-edit
    /// divergence. Wire verb is `<svc>.props.reconcile`. Bumps
    /// `audit_epoch`.
    Reconciled,
}

impl EventKind {
    /// Full wire verb for this kind under the named service. Examples:
    /// `EventKind::Created.verb_for("maild") == "maild.props.set"`,
    /// `EventKind::Reconciled.verb_for("maild") == "maild.props.reconcile"`.
    pub fn verb_for(&self, service: &str) -> String {
        let suffix = match self {
            Self::Created | Self::Updated => "set",
            Self::Deleted => "delete",
            Self::Completed => "complete",
            Self::Reconciled => "reconcile",
        };
        format!("{service}.props.{suffix}")
    }

    /// True for the two library-internal transitions that are not
    /// reachable from the wire (`Completed`, `Reconciled`).
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Completed | Self::Reconciled)
    }
}

/// SPEC 12 §10 / §5.5 — actor variant string attached to every event
/// and audit row. The variant alphabet is fixed by SPEC 07 §3.5.1 plus
/// SPEC 12's two special tokens; encoded as a flat string on the wire.
///
/// Concrete forms:
/// - `<svc>` — e.g. `maild`, when a service peer originated the call.
/// - `<runtime>:<uuid>[:<seq>]` — SPEC 07 runtime peer identity.
/// - `operator:<principal>` — human operator via SPEC 07.
/// - `daemon:<svc>` — owning daemon performing the library-internal
///   saga `complete` transition.
/// - `daemon:reconciliation` — substrate reconciliation pass on
///   startup hand-edit detection (§11).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Actor(String);

impl Actor {
    /// Construct from a SPEC 07 §3.5.1 actor-variant string verbatim.
    pub fn from_token(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Service-peer form (bare service name, e.g. `maild`).
    pub fn service(svc: impl AsRef<str>) -> Self {
        Self(svc.as_ref().to_string())
    }

    /// Human operator form `operator:<principal>`.
    pub fn operator(principal: impl AsRef<str>) -> Self {
        Self(format!("operator:{}", principal.as_ref()))
    }

    /// Saga `complete` transition emitted by the owning daemon:
    /// `daemon:<svc>`.
    pub fn daemon_complete(svc: impl AsRef<str>) -> Self {
        Self(format!("daemon:{}", svc.as_ref()))
    }

    /// Synthetic reconcile event attribution: `daemon:reconciliation`.
    pub fn reconciliation() -> Self {
        Self("daemon:reconciliation".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One durable transition appended to the namespace's event history.
/// SPEC 12 §6.6 — committed atomically with the matching record write
/// (or, for `Completed`/`Reconciled`, with the matching `_lifecycle` /
/// reconciled-bytes update). Row is immutable once written; subsequent
/// changes to the same key produce new rows with higher `nseq`.
///
/// **No value bytes.** Per §5.5, records.changed bodies are deltas —
/// subscribers wanting state call `<svc>.props.get`. Per §10, audit
/// bodies "contain no value bytes; secret-field redaction is
/// automatic." Neither projection needs `old`/`new`, so the row does
/// not carry them. Diff computation (e.g. `fields_changed` on
/// reconcile) happens at commit time from the in-memory inputs
/// ([`crate::store::ReconcileCommit`]) and is materialised into
/// `fields_changed` here; the value bytes themselves do not survive
/// into the durable row or any fan-out.
///
/// **Storage row, not wire body.** `RecordEvent` is the durable
/// event-history row returned by [`crate::store::PropertyStore::
/// events_since`]. The dispatcher (C5) projects it to two distinct
/// wire shapes:
///
/// - `<svc>.props.records.changed` body (§5.5) — adds the
///   service-prefixed `verb` string (`maild.props.set` etc., derived
///   from `kind` via [`EventKind::verb_for`]); drops `audit_digest`;
///   flattens [`Lifecycle`] into `{lifecycle: "active"|"failed",
///   reason?: string}` **only on `Completed` events**, and omits
///   the field entirely on `Created`/`Updated`/`Deleted`/`Reconciled`
///   even when the storage row's `lifecycle` is `Some(_)`.
/// - `<svc>.props.audit` body (§10) — projected via
///   [`AuditRow`], which carries the service-prefixed `verb`
///   string and `audit_digest`; never carries `lifecycle`.
///
/// Serialisation here flattens `namespace`, `key`, and `field_path`
/// to the top level so the on-disk JSON aligns with the wire body
/// for the common shared subset. `lifecycle` is stored in its
/// internal enum form so reconciliation can replay accurate saga
/// state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordEvent {
    pub nseq: Nseq,
    pub version: Version,
    pub audit_epoch: AuditEpoch,
    /// The `kind` body field on records.changed (§5.5).
    pub kind: EventKind,
    /// SPEC 12 §6.6 — the service-prefixed verb string the storage
    /// backend writes at commit time (one of `<svc>.props.set`,
    /// `<svc>.props.delete`, `<svc>.props.complete`,
    /// `<svc>.props.reconcile`). Storing it on the row keeps the
    /// event-history rows self-describing for audit replay even if
    /// the owning daemon's registered name changes between writes
    /// and reads. Composed via [`EventKind::verb_for`] at commit
    /// time.
    pub verb: String,
    #[serde(flatten)]
    pub key: RecordKey,
    /// Saga state at the moment this row was committed. Stored on
    /// every saga row (including `Provisioning` on the initial set)
    /// so reconciliation and audit replay see accurate state; the
    /// dispatcher restricts wire-side `lifecycle` emission to
    /// `Completed` events per §5.5 line "Present only on `completed`
    /// events from `Saga` namespaces."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,
    pub actor: Actor,
    /// Changed non-secret field names. Per §5.5, secret field names
    /// MUST NOT appear here — they are surfaced as the count below.
    /// Absent on `Deleted` events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields_changed: Vec<String>,
    /// Number of secret fields touched by this transition, per §5.5.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub secret_fields_changed_count: u32,
    /// Wall-clock timestamp at commit, RFC 3339. Serialised as `at`
    /// to match the §6.6 event-history row contract (line 1161); the
    /// dispatcher renames this to `ts` when projecting to the §5.5
    /// records.changed body and leaves it as `at` for the §10 audit
    /// projection.
    #[serde(rename = "at")]
    pub at: DateTime<Utc>,
    /// SPEC 12 §5.5 — optional Bus id of the originating request,
    /// preserved on caller-driven events (set/delete) and on
    /// `complete` events (so subscribers can correlate set → complete
    /// pairs). For `reconciled` events the substrate fills
    /// `external_edit_detected` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// SPEC 12 §10 — hex-encoded HMAC-SHA256 over
    /// `canonical_serialise(record) || nseq_bytes` keyed by the
    /// namespace's audit key. Computed by the storage backend at
    /// commit time so the event-history outbox is the single source
    /// of truth: the dispatcher projects this row to both
    /// `<svc>.props.records.changed` (drops the digest) and
    /// `<svc>.props.audit` (carries the digest as `audit_digest`).
    /// Persisting the digest on the row preserves the as-written
    /// HMAC even after the record is mutated or deleted, so audit
    /// replay via `events_since` reproduces each historical row
    /// byte-for-byte. The digest itself is only cryptographically
    /// verifiable against the **current** record state at the
    /// highest-nseq row, since neither the audit body nor the
    /// event-history row stores value bytes (see [`AuditRow`]).
    ///
    /// C1 stores the field as an empty string; the C5 storage layer
    /// fills it inside the same transaction as the record write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audit_digest: String,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Audit-stream projection per SPEC 12 §10. Flat body fields:
/// `namespace`, `key`, `verb`, `version`, `nseq`, `audit_epoch`,
/// `actor`, `at`, `fields_changed`, `audit_digest`.
///
/// `AuditRow` is the projection type the C5 dispatcher constructs
/// per audit emission from a stored [`RecordEvent`]; `verb`,
/// `audit_digest`, and the `at` timestamp are all read verbatim
/// from the persisted `RecordEvent` row (per §6.6 the event-history
/// row carries each of these). The storage layer does not persist a
/// separate audit row — `events_since` returns the same rows, and
/// audit projections are reconstructed from them.
///
/// **No prev_digest chaining.** Per §10 the audit body carries a
/// **single keyed HMAC**:
///
/// ```text
/// audit_digest = HMAC-SHA256(
///     key  = namespace_audit_key,
///     data = canonical_serialise(record) || nseq_bytes
/// )
/// ```
///
/// The spec is explicit (§10, §16.5): there is no `prev_digest` —
/// each entry's HMAC is computed independently against the post-commit
/// record state and the row's `nseq`. The §6.6 phrase "chain under the
/// same `audit_epoch`" refers to **sequential ordering by `nseq`**
/// (immutable, monotonic, all under the same epoch counter), not to a
/// cryptographic chain. The §11 phrase "HMAC continuity ... across the
/// bump as broken" means each verified digest depends on the record
/// bytes; after a reconcile bumps `audit_epoch` and replaces those
/// bytes, prior digests cannot be re-derived against the new
/// authoritative state.
///
/// Tamper detection is privileged: a verifier with the namespace audit
/// key re-derives `audit_digest` against the current authoritative
/// record state via `<svc>.props.get` (§10 line 1505). Only the
/// highest-nseq audit row for a given key can be re-verified this
/// way: per §10 the audit stream carries no value bytes, and
/// `RecordEvent` rows in the event history likewise carry no value
/// bytes (§5.5 deltas, §10 redaction), so the canonical record
/// bytes that fed an older row's HMAC are not recoverable once a
/// subsequent write supersedes them. Historical rows remain
/// immutable for ordering/inspection but are not cryptographically
/// re-verifiable. Passive audit subscribers cannot verify any row,
/// by design — see §16.5 design rationale.
///
/// `at` on the wire is the RFC 3339 timestamp; the field is named `at`
/// per §10 (records.changed uses `ts` per §5.5 — distinct fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRow {
    pub namespace: NamespaceName,
    pub key: String,
    /// Service-prefixed verb string, e.g. `maild.props.set`. Copied
    /// verbatim from the stored [`RecordEvent::verb`] field; the
    /// storage backend computed it once at commit time via
    /// [`EventKind::verb_for`] so audit replay reproduces the
    /// row as written regardless of any later service renames.
    pub verb: String,
    pub version: Version,
    pub nseq: Nseq,
    pub audit_epoch: AuditEpoch,
    pub actor: Actor,
    /// Per §10 the field is named `at` on the wire (vs `ts` on the
    /// records.changed body).
    #[serde(rename = "at")]
    pub at: DateTime<Utc>,
    /// Per §10: array of changed field names. Absent on delete events;
    /// for `set`/`complete`/`reconcile` see §10 / §5.5. Secret field
    /// names MUST NOT appear here — they are surfaced as a count on
    /// records.changed bodies only, not on audit (§10 omits the count).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields_changed: Vec<String>,
    /// Hex-encoded HMAC-SHA256 over the namespace's canonical
    /// serialisation of the record (or delete-tombstone marker, or
    /// lifecycle transition pair, or pre/post reconciliation pair)
    /// concatenated with `nseq_bytes`, keyed by the namespace's audit
    /// key. Populated in C5; placeholder empty string in C1.
    #[serde(default)]
    pub audit_digest: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::NamespaceName;

    fn ns() -> NamespaceName {
        NamespaceName::new("accounts").unwrap()
    }

    fn ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn version_and_nseq_monotonic() {
        assert_eq!(Version::zero().next(), Version(1));
        assert_eq!(Version(7).next(), Version(8));
        assert_eq!(Nseq(3).next(), Nseq(4));
        assert_eq!(AuditEpoch::zero().next(), AuditEpoch(1));
    }

    #[test]
    fn field_path_segment_serializes_untagged() {
        let segs = vec![
            FieldPathSegment::field("allowed_senders"),
            FieldPathSegment::index(0),
        ];
        let j = serde_json::to_value(&segs).unwrap();
        let arr = j.as_array().unwrap();
        assert_eq!(arr[0], "allowed_senders");
        assert_eq!(arr[1], 0);
        // Round-trips.
        let back: Vec<FieldPathSegment> = serde_json::from_value(j).unwrap();
        assert_eq!(back, segs);
    }

    #[test]
    fn collection_key_round_trip() {
        let k = RecordKey::collection(ns(), "user@host.example");
        let j = serde_json::to_value(&k).unwrap();
        assert_eq!(j["namespace"], "accounts");
        assert_eq!(j["key"], "user@host.example");
        // Empty field_path is elided on the wire.
        assert!(j.get("field_path").is_none());
        let back: RecordKey = serde_json::from_value(j).unwrap();
        assert_eq!(back.key, "user@host.example");
        assert!(back.is_whole_record());
    }

    #[test]
    fn singleton_key_is_empty_string() {
        let k = RecordKey::singleton(NamespaceName::new("theme").unwrap());
        let j = serde_json::to_value(&k).unwrap();
        // Singleton wire key is `""`, not absent.
        assert_eq!(j["key"], "");
        assert!(j.get("field_path").is_none());
    }

    #[test]
    fn field_path_serializes_as_mixed_array() {
        let k = RecordKey::collection(ns(), "u@h").with_field_path(vec![
            FieldPathSegment::field("allowed_senders"),
            FieldPathSegment::index(2),
        ]);
        let j = serde_json::to_value(&k).unwrap();
        let arr = j["field_path"].as_array().unwrap();
        assert_eq!(arr[0], "allowed_senders");
        assert_eq!(arr[1], 2);
    }

    #[test]
    fn event_kind_verb_composition() {
        assert_eq!(EventKind::Created.verb_for("maild"), "maild.props.set");
        assert_eq!(EventKind::Updated.verb_for("maild"), "maild.props.set");
        assert_eq!(EventKind::Deleted.verb_for("maild"), "maild.props.delete");
        assert_eq!(
            EventKind::Completed.verb_for("maild"),
            "maild.props.complete"
        );
        assert_eq!(
            EventKind::Reconciled.verb_for("maild"),
            "maild.props.reconcile"
        );
        assert!(EventKind::Completed.is_internal());
        assert!(EventKind::Reconciled.is_internal());
        assert!(!EventKind::Created.is_internal());
    }

    #[test]
    fn actor_serializes_as_flat_string() {
        let cases = [
            (Actor::service("maild"), "maild"),
            (Actor::operator("markc"), "operator:markc"),
            (Actor::daemon_complete("maild"), "daemon:maild"),
            (Actor::reconciliation(), "daemon:reconciliation"),
            (
                Actor::from_token("runtime:11111111-2222-3333-4444-555555555555:7"),
                "runtime:11111111-2222-3333-4444-555555555555:7",
            ),
        ];
        for (a, want) in cases {
            let j = serde_json::to_value(&a).unwrap();
            assert_eq!(j, want, "actor {a:?}");
            let back: Actor = serde_json::from_value(j).unwrap();
            assert_eq!(back, a);
        }
    }

    #[test]
    fn record_event_round_trip() {
        let ev = RecordEvent {
            nseq: Nseq(1),
            version: Version(1),
            audit_epoch: AuditEpoch::zero(),
            kind: EventKind::Created,
            verb: EventKind::Created.verb_for("maild"),
            key: RecordKey::collection(ns(), "test@example.com"),
            lifecycle: Some(Lifecycle::Provisioning),
            actor: Actor::operator("markc"),
            fields_changed: vec!["email".into(), "spam_enabled".into()],
            secret_fields_changed_count: 1,
            at: ts(),
            cause: None,
            audit_digest: "deadbeefcafe".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: RecordEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        let j: serde_json::Value = serde_json::from_str(&s).unwrap();
        // §5.5 deltas, §10 audit: no value bytes on the row.
        assert!(j.get("old").is_none());
        assert!(j.get("new").is_none());
        assert!(j.get("cause").is_none());
        // §10 digest persists on the row for faithful audit replay.
        assert_eq!(j["audit_digest"], "deadbeefcafe");
        // §6.6 line 1161: event-history row carries `at`, not `ts`.
        // The dispatcher renames to `ts` when projecting to §5.5
        // records.changed bodies.
        assert_eq!(j["at"].as_str().unwrap(), "2026-05-11T12:00:00Z");
        assert!(
            j.get("ts").is_none(),
            "event-history row serialises as `at`, dispatcher remaps to `ts` on the records.changed projection"
        );
        assert_eq!(j["kind"], "created");
        // §6.6: event-history row carries the service-prefixed verb.
        assert_eq!(j["verb"], "maild.props.set");
        assert_eq!(j["secret_fields_changed_count"], 1);
        // §5.5 records.changed body has flat namespace + key at top
        // level (the row's RecordKey is flattened on the wire).
        assert_eq!(j["namespace"], "accounts");
        assert_eq!(j["key"], "test@example.com");
        assert!(
            j.get("key").is_some_and(|v| v.is_string()),
            "key must be a flat string on the wire, not a nested RecordKey object"
        );
    }

    #[test]
    fn record_event_elides_empty_secret_count_and_fields() {
        let ev = RecordEvent {
            nseq: Nseq(2),
            version: Version(2),
            audit_epoch: AuditEpoch::zero(),
            kind: EventKind::Deleted,
            verb: EventKind::Deleted.verb_for("maild"),
            key: RecordKey::collection(ns(), "u@h"),
            lifecycle: None,
            actor: Actor::service("maild"),
            fields_changed: Vec::new(),
            secret_fields_changed_count: 0,
            at: ts(),
            cause: None,
            audit_digest: String::new(),
        };
        let j: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert!(j.get("fields_changed").is_none());
        assert!(j.get("secret_fields_changed_count").is_none());
        assert!(j.get("old").is_none());
        assert!(j.get("new").is_none());
        // Empty digest elided on the C1 placeholder row.
        assert!(j.get("audit_digest").is_none());
        assert_eq!(j["kind"], "deleted");
    }

    #[test]
    fn audit_row_flat_wire_shape() {
        let row = AuditRow {
            namespace: ns(),
            key: "u@h".into(),
            verb: "maild.props.set".into(),
            version: Version(1),
            nseq: Nseq(1),
            audit_epoch: AuditEpoch::zero(),
            actor: Actor::service("maild"),
            at: ts(),
            fields_changed: vec!["email".into()],
            audit_digest: "deadbeef".into(),
        };
        let j = serde_json::to_value(&row).unwrap();
        // SPEC 12 §10 flat field set.
        assert_eq!(j["namespace"], "accounts");
        assert_eq!(j["key"], "u@h");
        assert_eq!(j["verb"], "maild.props.set");
        assert_eq!(j["version"], 1);
        assert_eq!(j["nseq"], 1);
        assert_eq!(j["audit_epoch"], 0);
        assert_eq!(j["actor"], "maild");
        // §10 names the timestamp field `at` (records.changed uses `ts`).
        assert_eq!(j["at"], "2026-05-11T12:00:00Z");
        assert!(
            j.get("ts").is_none(),
            "audit must serialise as `at`, not `ts`"
        );
        assert_eq!(j["fields_changed"][0], "email");
        assert_eq!(j["audit_digest"], "deadbeef");
        // §10: no chained prev_digest; no inner RecordEvent nesting.
        assert!(j.get("prev_digest").is_none());
        assert!(j.get("event").is_none());
        // No records.changed-only fields leak into the audit body.
        assert!(j.get("kind").is_none());
        assert!(j.get("old").is_none());
        assert!(j.get("new").is_none());
        assert!(j.get("lifecycle").is_none());
        assert!(j.get("secret_fields_changed_count").is_none());
        let back: AuditRow = serde_json::from_value(j).unwrap();
        assert_eq!(back, row);
    }
}
