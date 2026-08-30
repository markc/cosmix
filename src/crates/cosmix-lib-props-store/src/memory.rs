//! In-memory `PropertyStore` for tests and pure-in-process daemons.
//!
//! SPEC 12 §6.4 backend kind `Memory`. No persistence. Each namespace
//! gets a `BTreeMap<key, Record>` and a `Vec<RecordEvent>` sidecar; nseq
//! and audit_epoch counters are namespace-scoped. All operations
//! serialise on a single global mutex over the namespace map so
//! concurrent commits observe a total order — which is the invariant
//! the dispatcher's outbox tail (C5) relies on.
//!
//! **Scope (C2).** Implements the trait surface. Does not enforce
//! `replay_window` (events accumulate indefinitely) and is **hard-delete
//! only**: regardless of the namespace's `spec.delete_mode`, `commit_delete`
//! removes the row immediately and emits a `Deleted` event. The
//! `SoftDelete` tombstone retention semantics in SPEC 12 §5.4 (preserving
//! the deleted row with a retention window so `if_version` can refuse
//! recreates) require persistent storage and ship with the C3 sqlite
//! backend. Daemons that need soft-delete must use the sqlite backend;
//! using `MemoryStore` against a `SoftDelete` namespace is acceptable
//! only in tests and in-process daemons that do not depend on tombstone
//! retention. Per-row audit_digest is also left empty here; C5 fills it
//! inside the same commit lock.
//!
//! **fields_changed computation.** This backend stores top-level
//! changed-key diff against the prior `value`. It does not consult the
//! namespace's `PropertySchema` to filter `secret: true` fields out —
//! that is the dispatcher's job at the records.changed projection (C5).
//! `secret_fields_changed_count` is therefore always 0 on rows written
//! by this backend; C5 will add a schema-aware computation step that
//! splits the diff before it is durable.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::audit::{AuditKey, compute_digest, tombstone_value};
use crate::lifecycle::{LIFECYCLE_FIELD, Lifecycle};
use crate::namespace::NamespaceName;
use crate::record::{Actor, AuditEpoch, EventKind, Nseq, Record, RecordEvent, RecordKey, Version};
use crate::store::{
    CompleteCommit, DeleteCommit, MergeMode, PropertyStore, ReconcileCommit, SetCommit, Snapshot,
    StoreError, StoreFuture,
};
use cosmix_props_core::value::PropValue;

/// Pure in-process `PropertyStore`. Owned by one daemon, parameterised
/// by that daemon's service name so the event-history rows carry the
/// correct `<svc>.props.*` verb at commit time per SPEC 12 §6.6.
pub struct MemoryStore {
    service: String,
    namespaces: Mutex<HashMap<NamespaceName, NamespaceState>>,
}

struct NamespaceState {
    records: BTreeMap<String, Record>,
    events: Vec<RecordEvent>,
    nseq: Nseq,
    audit_epoch: AuditEpoch,
    /// SPEC 12 §10 audit key — generated lazily when the namespace
    /// first appears (which, for MemoryStore, is on first write since
    /// the backend has no explicit register_namespace step).
    audit_key: AuditKey,
}

impl NamespaceState {
    fn new() -> Self {
        Self {
            records: BTreeMap::new(),
            events: Vec::new(),
            nseq: Nseq::zero(),
            audit_epoch: AuditEpoch::zero(),
            audit_key: AuditKey::generate(),
        }
    }
}

impl MemoryStore {
    /// `service` is the owning daemon's registered service name — the
    /// `<svc>` in `<svc>.props.set`. Used to compose [`RecordEvent::verb`]
    /// once per commit, per §6.6 line 1155.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            namespaces: Mutex::new(HashMap::new()),
        }
    }

    /// The service name this store was constructed with.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// SPEC 12 §10 — clone of the per-namespace audit key. Returns
    /// `None` if the namespace has never been written to (the
    /// `MemoryStore` allocates a key lazily on first commit). Intended
    /// for in-process verifiers (tamper-detection) and tests; never
    /// exposed via any Bus verb.
    pub fn audit_key(&self, namespace: &NamespaceName) -> Option<AuditKey> {
        let state = self.namespaces.lock().expect("memory store poisoned");
        state.get(namespace).map(|ns| ns.audit_key.clone())
    }
}

fn ts_to_dt(ts_ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ts_ms)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is representable"))
}

/// SPEC 12 §5.3 patch merge — top-level field union on object values.
/// Non-object pairs replace wholesale (patch is meaningless on a scalar).
fn merge_patch(old: &PropValue, new: PropValue) -> PropValue {
    match (old, new) {
        (PropValue::Object(o), PropValue::Object(n)) => {
            let mut out = o.clone();
            for (k, v) in n {
                out.insert(k, v);
            }
            PropValue::Object(out)
        }
        (_, new) => new,
    }
}

/// Top-level changed-key diff between old and new record values. For
/// object/object the union of keys whose values differ; for any other
/// shape pair, empty (whole-value transitions have no per-field
/// breakdown at this layer — the dispatcher's secret-aware projection
/// in C5 may refine this).
fn changed_top_level_fields(old: Option<&PropValue>, new: &PropValue) -> Vec<String> {
    match (old, new) {
        (None, PropValue::Object(n)) => n.keys().cloned().collect(),
        (Some(PropValue::Object(o)), PropValue::Object(n)) => {
            let mut diff: Vec<String> = Vec::new();
            let keys: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            for k in keys {
                if o.get(k) != n.get(k) {
                    diff.push(k.clone());
                }
            }
            diff
        }
        _ => Vec::new(),
    }
}

impl PropertyStore for MemoryStore {
    fn get<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Snapshot<Record>> {
        Box::pin(async move {
            let state = self.namespaces.lock().expect("memory store poisoned");
            let ns_state = state.get(&key.namespace).ok_or(StoreError::NotFound)?;
            let record = ns_state
                .records
                .get(&key.key)
                .ok_or(StoreError::NotFound)?
                .clone();
            Ok(Snapshot::new(record, ns_state.nseq))
        })
    }

    fn list<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, Snapshot<Vec<Record>>> {
        Box::pin(async move {
            let state = self.namespaces.lock().expect("memory store poisoned");
            match state.get(namespace) {
                None => Ok(Snapshot::new(Vec::new(), Nseq::zero())),
                Some(ns_state) => {
                    let records: Vec<Record> = ns_state.records.values().cloned().collect();
                    Ok(Snapshot::new(records, ns_state.nseq))
                }
            }
        })
    }

    fn commit_set<'a>(&'a self, op: SetCommit) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            // SPEC 12 §4.1 — sub-path mutation is not a storage-layer
            // primitive. `commit_set` operates on whole records; a
            // non-empty `field_path` would bypass the `_lifecycle` guard
            // below (and the patch-merge field accounting) by deep-
            // writing a top-level reserved key. Refuse before locking.
            if !op.key.is_whole_record() {
                return Err(StoreError::validation(
                    "commit_set requires record-root key (empty field_path)",
                ));
            }

            let mut state = self.namespaces.lock().expect("memory store poisoned");
            let ns_state = state
                .entry(op.key.namespace.clone())
                .or_insert_with(NamespaceState::new);

            // SPEC 12 §6.5 reserved field guard: daemons MUST NOT write
            // `_lifecycle` directly via set; the substrate flips it via
            // commit_complete only. Refuse if the inbound value carries
            // it at the top level.
            if let PropValue::Object(ref m) = op.new_value
                && m.contains_key(LIFECYCLE_FIELD)
            {
                return Err(StoreError::validation(format!(
                    "field {LIFECYCLE_FIELD:?} is substrate-reserved (§6.5)"
                )));
            }

            let existing = ns_state.records.get(&op.key.key).cloned();
            let prev_version = existing
                .as_ref()
                .map(|r| r.version)
                .unwrap_or(Version::zero());

            // SPEC 12 §4.4 optimistic concurrency. `Some(Version::zero())`
            // is the create-new sentinel — succeeds only when no record
            // exists yet (prev_version == Version::zero()).
            if let Some(expected) = op.expected_version
                && expected != prev_version
            {
                return Err(StoreError::VersionMismatch {
                    expected,
                    current: prev_version,
                });
            }

            let final_value = match (op.merge, existing.as_ref()) {
                (MergeMode::Replace, _) | (MergeMode::Patch, None) => op.new_value.clone(),
                (MergeMode::Patch, Some(r)) => merge_patch(&r.value, op.new_value.clone()),
            };

            let new_version = prev_version.next();
            let new_nseq = ns_state.nseq.next();
            let kind = if existing.is_some() {
                EventKind::Updated
            } else {
                EventKind::Created
            };
            let fields_changed =
                changed_top_level_fields(existing.as_ref().map(|r| &r.value), &final_value);

            let record = Record {
                key: op.key.clone(),
                value: final_value,
                version: new_version,
                nseq: new_nseq,
                lifecycle: op.lifecycle.clone(),
            };

            let audit_digest = compute_digest(&ns_state.audit_key, &record.value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: new_version,
                audit_epoch: ns_state.audit_epoch,
                kind,
                verb: kind.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: op.lifecycle,
                actor: op.actor,
                fields_changed,
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: op.cause,
                audit_digest,
            };

            ns_state.records.insert(op.key.key.clone(), record.clone());
            ns_state.events.push(event.clone());
            ns_state.nseq = new_nseq;

            Ok((record, event))
        })
    }

    fn commit_delete<'a>(&'a self, op: DeleteCommit) -> StoreFuture<'a, RecordEvent> {
        Box::pin(async move {
            // Sub-path delete (e.g. removing one element of a list) is
            // not a storage primitive; the substrate models deletion at
            // whole-record granularity only.
            if !op.key.is_whole_record() {
                return Err(StoreError::validation(
                    "commit_delete requires record-root key (empty field_path)",
                ));
            }

            let mut state = self.namespaces.lock().expect("memory store poisoned");
            let ns_state = state
                .get_mut(&op.key.namespace)
                .ok_or(StoreError::NotFound)?;
            let existing = ns_state
                .records
                .get(&op.key.key)
                .ok_or(StoreError::NotFound)?;

            if let Some(expected) = op.expected_version
                && expected != existing.version
            {
                return Err(StoreError::VersionMismatch {
                    expected,
                    current: existing.version,
                });
            }

            let new_nseq = ns_state.nseq.next();
            // Tombstone bumps the version one past the prior so that an
            // immediate recreate at the same key (in hard-delete mode)
            // does not look like a no-op replay. SoftDelete tombstone
            // retention is a C3-sqlite concern.
            let tombstone_version = existing.version.next();

            let audit_digest = compute_digest(&ns_state.audit_key, &tombstone_value(), new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: tombstone_version,
                audit_epoch: ns_state.audit_epoch,
                kind: EventKind::Deleted,
                verb: EventKind::Deleted.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: None,
                actor: op.actor,
                fields_changed: Vec::new(),
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: op.cause,
                audit_digest,
            };

            ns_state.records.remove(&op.key.key);
            ns_state.events.push(event.clone());
            ns_state.nseq = new_nseq;

            Ok(event)
        })
    }

    fn commit_complete<'a>(&'a self, op: CompleteCommit) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            let mut state = self.namespaces.lock().expect("memory store poisoned");
            let ns_state = state
                .get_mut(&op.key.namespace)
                .ok_or(StoreError::NotFound)?;
            let record = ns_state
                .records
                .get_mut(&op.key.key)
                .ok_or(StoreError::NotFound)?;

            // Version check runs FIRST so a stale saga complete from a
            // racing writer surfaces as `VersionMismatch` (the
            // diagnostic that tells the caller "another writer
            // displaced your row") rather than as a lifecycle
            // `Validation` (which would imply substrate misuse).
            if record.version != op.expected_version {
                return Err(StoreError::VersionMismatch {
                    expected: op.expected_version,
                    current: record.version,
                });
            }
            // SPEC 12 §6.5 — complete requires Provisioning. Library-
            // internal contract; misuse is a substrate bug, surfaced as
            // validation_error so it shows up loudly in tests.
            if !matches!(record.lifecycle, Some(Lifecycle::Provisioning)) {
                return Err(StoreError::validation(
                    "commit_complete requires record.lifecycle = Provisioning",
                ));
            }
            if matches!(op.lifecycle, Lifecycle::Provisioning) {
                return Err(StoreError::validation(
                    "commit_complete target lifecycle MUST be Active or Failed",
                ));
            }

            record.lifecycle = Some(op.lifecycle.clone());
            record.version = record.version.next();
            let new_nseq = ns_state.nseq.next();
            record.nseq = new_nseq;
            ns_state.nseq = new_nseq;

            let audit_digest = compute_digest(&ns_state.audit_key, &record.value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: record.version,
                audit_epoch: ns_state.audit_epoch,
                kind: EventKind::Completed,
                verb: EventKind::Completed.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: Some(op.lifecycle),
                actor: op.actor,
                // The only field that "changed" is the substrate-managed
                // `_lifecycle`; the daemon-visible value is unchanged.
                fields_changed: vec![LIFECYCLE_FIELD.to_string()],
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: op.cause,
                audit_digest,
            };

            let snapshot = record.clone();
            ns_state.events.push(event.clone());
            Ok((snapshot, event))
        })
    }

    fn commit_reconcile<'a>(
        &'a self,
        op: ReconcileCommit,
    ) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            let mut state = self.namespaces.lock().expect("memory store poisoned");
            let ns_state = state
                .entry(op.key.namespace.clone())
                .or_insert_with(NamespaceState::new);

            let existing = ns_state.records.get(&op.key.key).cloned();
            let prev_version = existing
                .as_ref()
                .map(|r| r.version)
                .unwrap_or(Version::zero());
            let preserved_lifecycle = existing.as_ref().and_then(|r| r.lifecycle.clone());

            // Reconcile is the ONLY path that bumps audit_epoch (§11).
            ns_state.audit_epoch = ns_state.audit_epoch.next();

            let new_version = prev_version.next();
            let new_nseq = ns_state.nseq.next();
            let fields_changed = changed_top_level_fields(op.old_value.as_ref(), &op.new_value);

            let record = Record {
                key: op.key.clone(),
                value: op.new_value.clone(),
                version: new_version,
                nseq: new_nseq,
                lifecycle: preserved_lifecycle.clone(),
            };

            let audit_digest = compute_digest(&ns_state.audit_key, &record.value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: new_version,
                audit_epoch: ns_state.audit_epoch,
                kind: EventKind::Reconciled,
                verb: EventKind::Reconciled.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: preserved_lifecycle,
                actor: Actor::reconciliation(),
                fields_changed,
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: Some("external_edit_detected".to_string()),
                audit_digest,
            };

            ns_state.records.insert(op.key.key.clone(), record.clone());
            ns_state.events.push(event.clone());
            ns_state.nseq = new_nseq;

            Ok((record, event))
        })
    }

    fn events_since<'a>(
        &'a self,
        namespace: &'a NamespaceName,
        since_nseq: Nseq,
    ) -> StoreFuture<'a, Vec<RecordEvent>> {
        Box::pin(async move {
            let state = self.namespaces.lock().expect("memory store poisoned");
            match state.get(namespace) {
                None => Ok(Vec::new()),
                Some(ns_state) => Ok(ns_state
                    .events
                    .iter()
                    .filter(|e| e.nseq > since_nseq)
                    .cloned()
                    .collect()),
            }
        })
    }

    fn audit_epoch<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, AuditEpoch> {
        Box::pin(async move {
            let state = self.namespaces.lock().expect("memory store poisoned");
            Ok(state
                .get(namespace)
                .map(|s| s.audit_epoch)
                .unwrap_or(AuditEpoch::zero()))
        })
    }

    fn version_anchor<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Option<Version>> {
        // MemoryStore is HardDelete-only (records.remove on delete), so
        // an absent record is genuinely absent — no tombstone state to
        // surface. Live → Some(version); absent → None.
        Box::pin(async move {
            let state = self.namespaces.lock().expect("memory store poisoned");
            Ok(state
                .get(&key.namespace)
                .and_then(|ns| ns.records.get(&key.key))
                .map(|r| r.version))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> NamespaceName {
        NamespaceName::new("accounts").unwrap()
    }

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PropValue::Object(m)
    }

    fn set_commit(
        key: RecordKey,
        value: PropValue,
        lifecycle: Option<Lifecycle>,
        expected: Option<Version>,
    ) -> SetCommit {
        SetCommit {
            key,
            new_value: value,
            expected_version: expected,
            merge: MergeMode::Patch,
            lifecycle,
            actor: Actor::service("maild"),
            cause: None,
            ts_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn simple_set_returns_created_then_updated() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "a@b.example");

        let (rec1, ev1) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("a@b.example"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(rec1.version, Version(1));
        assert_eq!(ev1.kind, EventKind::Created);
        assert_eq!(ev1.verb, "maild.props.set");
        assert!(ev1.fields_changed.contains(&"email".to_string()));

        let (rec2, ev2) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("display", PropValue::from("A"))]),
                None,
                Some(Version(1)),
            ))
            .await
            .unwrap();
        assert_eq!(rec2.version, Version(2));
        assert_eq!(ev2.kind, EventKind::Updated);
        // Patch merge preserves prior fields.
        let merged = rec2.value.as_object().unwrap();
        assert_eq!(merged["email"], PropValue::from("a@b.example"));
        assert_eq!(merged["display"], PropValue::from("A"));
        // Only the newly-set key shows up in fields_changed.
        assert_eq!(ev2.fields_changed, vec!["display".to_string()]);
    }

    #[tokio::test]
    async fn version_mismatch_refused() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "x@y");
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("v", PropValue::from(1_i64))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let err = store
            .commit_set(set_commit(
                key,
                obj(&[("v", PropValue::from(2_i64))]),
                None,
                Some(Version(5)),
            ))
            .await
            .unwrap_err();
        match err {
            StoreError::VersionMismatch { expected, current } => {
                assert_eq!(expected, Version(5));
                assert_eq!(current, Version(1));
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reserved_lifecycle_field_refused() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        let mut m = BTreeMap::new();
        m.insert(LIFECYCLE_FIELD.to_string(), PropValue::from("active"));
        let err = store
            .commit_set(set_commit(
                key,
                PropValue::Object(m),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Validation { .. }));
    }

    #[tokio::test]
    async fn replace_drops_unmentioned_fields() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        store
            .commit_set(SetCommit {
                merge: MergeMode::Patch,
                ..set_commit(
                    key.clone(),
                    obj(&[
                        ("email", PropValue::from("u@h")),
                        ("display", PropValue::from("Initial")),
                    ]),
                    None,
                    Some(Version::zero()),
                )
            })
            .await
            .unwrap();
        let (rec, ev) = store
            .commit_set(SetCommit {
                merge: MergeMode::Replace,
                ..set_commit(
                    key.clone(),
                    obj(&[("email", PropValue::from("u@h"))]),
                    None,
                    Some(Version(1)),
                )
            })
            .await
            .unwrap();
        let m = rec.value.as_object().unwrap();
        assert!(
            m.get("display").is_none(),
            "replace should drop unmentioned field"
        );
        assert_eq!(ev.kind, EventKind::Updated);
        assert!(ev.fields_changed.contains(&"display".to_string()));
    }

    #[tokio::test]
    async fn delete_removes_record_and_emits_tombstone() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let ev = store
            .commit_delete(DeleteCommit {
                key: key.clone(),
                expected_version: Some(Version(1)),
                actor: Actor::service("maild"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(ev.kind, EventKind::Deleted);
        assert_eq!(ev.verb, "maild.props.delete");
        assert!(ev.fields_changed.is_empty());

        let err = store.get(&key).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    #[tokio::test]
    async fn saga_set_records_provisioning_then_completes_active() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        let (rec, ev) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                Some(Lifecycle::Provisioning),
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(rec.lifecycle, Some(Lifecycle::Provisioning));
        assert_eq!(ev.lifecycle, Some(Lifecycle::Provisioning));

        let (rec2, ev2) = store
            .commit_complete(CompleteCommit {
                key: key.clone(),
                lifecycle: Lifecycle::Active,
                expected_version: Version(1),
                actor: Actor::daemon_complete("maild"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(rec2.lifecycle, Some(Lifecycle::Active));
        assert_eq!(ev2.kind, EventKind::Completed);
        assert_eq!(ev2.verb, "maild.props.complete");
        assert_eq!(ev2.fields_changed, vec![LIFECYCLE_FIELD.to_string()]);
        assert!(rec2.version > rec.version);
        assert!(ev2.nseq > ev.nseq);
    }

    #[tokio::test]
    async fn saga_complete_failed_carries_reason() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                Some(Lifecycle::Provisioning),
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let (rec, ev) = store
            .commit_complete(CompleteCommit {
                key: key.clone(),
                lifecycle: Lifecycle::Failed {
                    reason: "mds seed failed".into(),
                },
                expected_version: Version(1),
                actor: Actor::daemon_complete("maild"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert!(matches!(
            rec.lifecycle,
            Some(Lifecycle::Failed { ref reason }) if reason == "mds seed failed"
        ));
        assert!(matches!(
            ev.lifecycle,
            Some(Lifecycle::Failed { ref reason }) if reason == "mds seed failed"
        ));
    }

    #[tokio::test]
    async fn complete_refused_without_provisioning() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        // Simple set — no lifecycle.
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("v", PropValue::from(1_i64))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let err = store
            .commit_complete(CompleteCommit {
                key,
                lifecycle: Lifecycle::Active,
                expected_version: Version(1),
                actor: Actor::daemon_complete("maild"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Validation { .. }));
    }

    #[tokio::test]
    async fn reconcile_bumps_audit_epoch_only() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(store.audit_epoch(&ns()).await.unwrap(), AuditEpoch::zero());

        let (rec, ev) = store
            .commit_reconcile(ReconcileCommit {
                key: key.clone(),
                new_value: obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("hand-edited")),
                ]),
                old_value: Some(obj(&[("email", PropValue::from("u@h"))])),
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(ev.kind, EventKind::Reconciled);
        assert_eq!(ev.audit_epoch, AuditEpoch(1));
        assert_eq!(rec.audit_epoch_via_event(&ev), AuditEpoch(1));
        assert_eq!(ev.actor.as_str(), "daemon:reconciliation");
        assert!(ev.fields_changed.contains(&"display".to_string()));

        // A subsequent normal set MUST NOT bump audit_epoch.
        let (_, ev2) = store
            .commit_set(set_commit(
                key,
                obj(&[("display", PropValue::from("renamed"))]),
                None,
                Some(rec.version),
            ))
            .await
            .unwrap();
        assert_eq!(ev2.audit_epoch, AuditEpoch(1));
        assert_eq!(store.audit_epoch(&ns()).await.unwrap(), AuditEpoch(1));
    }

    #[tokio::test]
    async fn events_since_strictly_greater() {
        let store = MemoryStore::new("maild");
        let key = RecordKey::collection(ns(), "u@h");
        for v in 0..3u64 {
            store
                .commit_set(set_commit(
                    key.clone(),
                    obj(&[("v", PropValue::from(v as i64))]),
                    None,
                    Some(Version(v)),
                ))
                .await
                .unwrap();
        }
        let all = store.events_since(&ns(), Nseq::zero()).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].nseq, Nseq(1));
        let after_first = store.events_since(&ns(), Nseq(1)).await.unwrap();
        assert_eq!(after_first.len(), 2);
        assert_eq!(after_first[0].nseq, Nseq(2));
        // since > current nseq returns empty.
        let empty = store.events_since(&ns(), Nseq(99)).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn list_includes_observed_nseq() {
        let store = MemoryStore::new("maild");
        let k1 = RecordKey::collection(ns(), "a");
        let k2 = RecordKey::collection(ns(), "b");
        store
            .commit_set(set_commit(
                k1,
                obj(&[("v", PropValue::from(1_i64))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        store
            .commit_set(set_commit(
                k2,
                obj(&[("v", PropValue::from(2_i64))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let snap = store.list(&ns()).await.unwrap();
        assert_eq!(snap.value.len(), 2);
        assert_eq!(snap.observed_nseq, Nseq(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sets_produce_monotonic_nseq() {
        use std::sync::Arc;

        let store = Arc::new(MemoryStore::new("maild"));
        let mut handles = Vec::new();
        for i in 0..16u64 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let key = RecordKey::collection(ns(), format!("user{i}"));
                store
                    .commit_set(set_commit(
                        key,
                        obj(&[("v", PropValue::from(i as i64))]),
                        None,
                        Some(Version::zero()),
                    ))
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let events = store.events_since(&ns(), Nseq::zero()).await.unwrap();
        assert_eq!(events.len(), 16);
        // Strictly monotonic, no gaps.
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.nseq, Nseq((i as u64) + 1));
        }
        // Every event's audit_epoch is the zero epoch — no reconcile happened.
        assert!(events.iter().all(|e| e.audit_epoch == AuditEpoch::zero()));
    }

    // Helper-only — not exposed on Record itself since audit_epoch is
    // namespace-scoped, not per-record.
    trait RecordAuditAccess {
        fn audit_epoch_via_event(&self, ev: &RecordEvent) -> AuditEpoch;
    }
    impl RecordAuditAccess for Record {
        fn audit_epoch_via_event(&self, ev: &RecordEvent) -> AuditEpoch {
            ev.audit_epoch
        }
    }
}
