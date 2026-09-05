//! SQLite-backed `PropertyStore` per SPEC 12 §6.4 backend kind
//! `SqliteTable`.
//!
//! **Atomicity contract.** Every mutation runs inside a single
//! `BEGIN IMMEDIATE` transaction that covers (a) the daemon-supplied
//! [`SqliteTableMapping`] write, (b) the substrate's `__props_records`
//! row, and (c) the `__props_events` row. The transaction commits as a
//! whole or rolls back as a whole; the dispatcher (C5) reads only
//! committed rows. The IMMEDIATE behaviour serialises writers at the
//! SQLite level so the `nseq` allocation in `__props_meta` is the
//! single source of truth for ordering. SPEC 12 §6.6 line 1148.
//!
//! **Storage split (C3 design).** The substrate owns metadata —
//! version, nseq, lifecycle, and SoftDelete tombstone — in
//! `__props_records`. Value bytes live wherever the
//! [`SqliteTableMapping`] puts them: the daemon-supplied implementation
//! (e.g. cosmix-maild's `accounts` table) round-trips between
//! domain-shaped columns and the property [`PropValue`]; the
//! library-provided [`JsonValuesMapping`] stores the value as JSON in
//! a substrate-managed `__props_values` table for namespaces that have
//! no pre-existing business table.
//!
//! **SoftDelete tombstones (§5.4).** The substrate ships hard- and
//! soft-delete:
//!
//! - `HardDelete`: `__props_records` row removed, mapping value
//!   removed, tombstone event emitted at `tombstone_version =
//!   prev_version + 1`. A subsequent set restarts at version 1.
//! - `SoftDelete` (default): `__props_records.tombstoned` flag set,
//!   `version` advanced to the tombstone version, mapping value
//!   removed. `get`/`list` skip tombstoned rows (returning
//!   `not_found`). A subsequent set against a tombstone preserves the
//!   version sequence — its `expected_version` MUST match the
//!   tombstone version, not zero. Tombstone TTL eviction is C5+ work;
//!   v0.1 retains tombstones indefinitely.
//!
//! **fields_changed.** Same algorithm as [`crate::memory::MemoryStore`]:
//! top-level changed-key diff against the prior value. Schema-aware
//! secret-field redaction is the dispatcher's job (C5). Rows here
//! always carry `secret_fields_changed_count = 0` and an empty
//! `audit_digest` — both are filled in C5 inside the same commit lock.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value as Json;

use crate::audit::{AuditKey, compute_digest, tombstone_value};
use crate::lifecycle::{LIFECYCLE_FIELD, Lifecycle};
use crate::namespace::{DeleteMode, NamespaceName, NamespaceSpec};
use crate::record::{
    Actor, AuditEpoch, EventKind, FieldPathSegment, Nseq, Record, RecordEvent, RecordKey, Version,
};
use crate::store::{
    CompleteCommit, DeleteCommit, MergeMode, PropertyStore, ReconcileCommit, SetCommit, Snapshot,
    StoreError, StoreFuture,
};
use cosmix_props_core::value::PropValue;

/// Bridge between a daemon's business table and the substrate's
/// property-record JSON shape. The substrate owns metadata
/// (`__props_records` and `__props_events`); the mapping owns value
/// bytes — i.e. the columns of the business table.
///
/// All methods run **inside the substrate's per-commit `BEGIN
/// IMMEDIATE` transaction**, so writes through the mapping share the
/// record-row write's atomicity. Implementations MUST NOT open their
/// own transactions or commit; they MUST NOT panic on otherwise
/// recoverable errors. Lifting errors from the business-table layer
/// is done by returning `rusqlite::Error` — the substrate surfaces
/// them as [`StoreError::Storage`].
///
/// **Idempotent bootstrap.** [`bootstrap`](Self::bootstrap) MAY be
/// called once at namespace registration *or* on every store re-open;
/// `CREATE TABLE IF NOT EXISTS` is the canonical idiom.
///
/// **Reserved field guard.** Mappings MUST NOT emit `_lifecycle` (or
/// any other substrate-reserved field name from §6.5) from `read` or
/// `list`; doing so confuses the diff against incoming patch values.
/// The substrate's own guard in `commit_set` only catches inbound
/// `_lifecycle`; reads are trusted.
pub trait SqliteTableMapping: Send + Sync {
    /// Create the business-table schema if it does not yet exist.
    /// Idempotent: safe to call multiple times.
    fn bootstrap(&self, conn: &Connection) -> rusqlite::Result<()>;

    /// Read the value for a single key. `Ok(None)` for absent keys.
    /// Called from the substrate `get` path (no transaction) and the
    /// mutation paths (inside transaction).
    fn read(&self, conn: &Connection, key: &str) -> rusqlite::Result<Option<PropValue>>;

    /// Enumerate `(key, value)` pairs for all live rows. The substrate
    /// joins this with the metadata in `__props_records` to compose
    /// [`Record`] values; mappings need not return rows in any
    /// particular order.
    fn list(&self, conn: &Connection) -> rusqlite::Result<Vec<(String, PropValue)>>;

    /// Insert-or-update the value for `key`. Called inside the
    /// substrate's commit transaction; idempotent inside the same
    /// commit.
    fn upsert(&self, conn: &Connection, key: &str, value: &PropValue) -> rusqlite::Result<()>;

    /// Remove the value bytes for `key`. No-op if absent. Called from
    /// both `commit_delete` (after which substrate either drops or
    /// tombstones the metadata row) and `commit_reconcile` of a
    /// not-on-disk row (no path exercises that in v0.1).
    fn delete(&self, conn: &Connection, key: &str) -> rusqlite::Result<()>;
}

/// Library-provided default mapping. Stores `PropValue` as JSON in a
/// substrate-managed table `__props_values(namespace, key, value_json)`.
/// Convenient for namespaces that have no pre-existing business table
/// (e.g. ephemeral state, mounted MixData props, tests).
///
/// **Round-trip caveat.** JSON elides the `Int`/`UInt` distinction for
/// values that fit in both — see [`crate::value`]'s docs. Daemons
/// relying on `u64` round-trip identity above `i64::MAX` use the
/// schema-typed boundary; this default mapping is for ordinary cases.
pub struct JsonValuesMapping {
    namespace: NamespaceName,
}

impl JsonValuesMapping {
    pub fn new(namespace: NamespaceName) -> Self {
        Self { namespace }
    }
}

impl SqliteTableMapping for JsonValuesMapping {
    fn bootstrap(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS __props_values (\
                namespace TEXT NOT NULL, \
                key TEXT NOT NULL, \
                value_json TEXT NOT NULL, \
                PRIMARY KEY (namespace, key)\
            );",
        )
    }

    fn read(&self, conn: &Connection, key: &str) -> rusqlite::Result<Option<PropValue>> {
        let row: Option<String> = conn
            .query_row(
                "SELECT value_json FROM __props_values WHERE namespace = ?1 AND key = ?2",
                params![self.namespace.as_str(), key],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some(s) => Ok(Some(serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?)),
        }
    }

    fn list(&self, conn: &Connection) -> rusqlite::Result<Vec<(String, PropValue)>> {
        let mut stmt = conn.prepare(
            "SELECT key, value_json FROM __props_values WHERE namespace = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![self.namespace.as_str()], |r| {
            let k: String = r.get(0)?;
            let v: String = r.get(1)?;
            Ok((k, v))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (k, v) = row?;
            let pv: PropValue = serde_json::from_str(&v).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            out.push((k, pv));
        }
        Ok(out)
    }

    fn upsert(&self, conn: &Connection, key: &str, value: &PropValue) -> rusqlite::Result<()> {
        let s = serde_json::to_string(value)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        conn.execute(
            "INSERT INTO __props_values (namespace, key, value_json) VALUES (?1, ?2, ?3) \
             ON CONFLICT(namespace, key) DO UPDATE SET value_json = excluded.value_json",
            params![self.namespace.as_str(), key, s],
        )?;
        Ok(())
    }

    fn delete(&self, conn: &Connection, key: &str) -> rusqlite::Result<()> {
        conn.execute(
            "DELETE FROM __props_values WHERE namespace = ?1 AND key = ?2",
            params![self.namespace.as_str(), key],
        )?;
        Ok(())
    }
}

/// Per-namespace substrate config the store consults inside commits.
/// Subset of [`NamespaceSpec`] — lifecycle and require_version are
/// enforced above the store in [`crate::runtime::Runtime`], but the
/// store needs `delete_mode` (HardDelete vs SoftDelete dispatch) and
/// holds onto `tombstone_ttl` so the C5+ tombstone-sweep pass can
/// consult the spec value without re-reading registration.
struct NamespaceReg {
    delete_mode: DeleteMode,
    /// SPEC 12 §5.4 retention bound for SoftDelete tombstones.
    /// **v0.1 does not yet expire tombstones**; the field is captured
    /// here so the sweep task added in C5+ can read it without
    /// re-deriving from a stored NamespaceSpec. Ignored for
    /// `HardDelete`. See module-level note.
    #[allow(dead_code)]
    tombstone_ttl: std::time::Duration,
    mapping: Arc<dyn SqliteTableMapping>,
    /// SPEC 12 §10 per-namespace audit key. Loaded from
    /// `__props_audit_keys` on first registration; generated and
    /// persisted on first ever bootstrap. Used inside every commit
    /// transaction to compute the row's `audit_digest`.
    audit_key: AuditKey,
}

/// SQLite-backed `PropertyStore`. Single `Connection` behind a
/// `std::sync::Mutex` (matching cosmix-mds's per-set discipline at
/// store granularity here, since each store owns one logical DB).
/// SPEC 12 §6.4 SqliteTable.
pub struct SqliteStore {
    service: String,
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    namespaces: HashMap<NamespaceName, NamespaceReg>,
}

impl SqliteStore {
    /// Open a store against the given connection. The caller chooses
    /// connection options (`PRAGMA journal_mode`, busy-timeout, etc.);
    /// the substrate sets only what it needs for correctness:
    /// `foreign_keys = ON`. Namespaces are registered afterwards via
    /// [`Self::register_namespace`].
    pub fn new(service: impl Into<String>, conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             CREATE TABLE IF NOT EXISTS __props_meta (\
                namespace TEXT PRIMARY KEY, \
                nseq INTEGER NOT NULL, \
                audit_epoch INTEGER NOT NULL\
             );\n\
             CREATE TABLE IF NOT EXISTS __props_records (\
                namespace TEXT NOT NULL, \
                key TEXT NOT NULL, \
                version INTEGER NOT NULL, \
                nseq INTEGER NOT NULL, \
                lifecycle_kind TEXT, \
                lifecycle_reason TEXT, \
                tombstoned INTEGER NOT NULL DEFAULT 0, \
                tombstone_at_ms INTEGER, \
                PRIMARY KEY (namespace, key)\
             );\n\
             CREATE TABLE IF NOT EXISTS __props_events (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                namespace TEXT NOT NULL, \
                nseq INTEGER NOT NULL, \
                version INTEGER NOT NULL, \
                audit_epoch INTEGER NOT NULL, \
                kind TEXT NOT NULL, \
                verb TEXT NOT NULL, \
                key TEXT NOT NULL, \
                field_path TEXT NOT NULL, \
                lifecycle_kind TEXT, \
                lifecycle_reason TEXT, \
                actor TEXT NOT NULL, \
                fields_changed TEXT NOT NULL, \
                secret_fields_changed_count INTEGER NOT NULL, \
                at_ms INTEGER NOT NULL, \
                cause TEXT, \
                audit_digest TEXT NOT NULL DEFAULT '', \
                UNIQUE (namespace, nseq)\
             );\n\
             CREATE INDEX IF NOT EXISTS idx_props_events_ns_nseq \
                ON __props_events (namespace, nseq);\n\
             CREATE TABLE IF NOT EXISTS __props_audit_keys (\
                namespace TEXT PRIMARY KEY, \
                key_bytes BLOB NOT NULL\
             );",
        )
        .map_err(sql_err)?;

        Ok(Self {
            service: service.into(),
            inner: Mutex::new(Inner {
                conn,
                namespaces: HashMap::new(),
            }),
        })
    }

    /// The service name this store was constructed with.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Register a namespace's storage adapter. Idempotent against the
    /// same `(name, delete_mode)` pair; re-registering with a different
    /// delete_mode returns `StoreError::Validation` (substrate
    /// invariant: a namespace's delete contract is immutable across a
    /// daemon's lifetime).
    pub fn register_namespace(
        &self,
        spec: &NamespaceSpec,
        mapping: Arc<dyn SqliteTableMapping>,
    ) -> Result<(), StoreError> {
        // SPEC 12 §5.4 line 525-526: HardDelete MUST NOT declare
        // `require_version: true`. Without retained version state
        // across recreate, a stale `if_version` cannot be
        // distinguished from a same-key-new-incarnation record.
        if matches!(spec.delete_mode, DeleteMode::HardDelete) && spec.require_version {
            return Err(StoreError::validation(format!(
                "namespace {:?}: HardDelete is incompatible with require_version=true (§5.4)",
                spec.name.as_str()
            )));
        }
        let mut inner = self.inner.lock().expect("sqlite store poisoned");
        if let Some(prev) = inner.namespaces.get(&spec.name)
            && prev.delete_mode != spec.delete_mode
        {
            return Err(StoreError::validation(format!(
                "namespace {:?} already registered with different delete_mode",
                spec.name.as_str()
            )));
        }
        mapping.bootstrap(&inner.conn).map_err(sql_err)?;
        // Initialise meta row (idempotent).
        inner
            .conn
            .execute(
                "INSERT INTO __props_meta (namespace, nseq, audit_epoch) VALUES (?1, 0, 0) \
                 ON CONFLICT(namespace) DO NOTHING",
                params![spec.name.as_str()],
            )
            .map_err(sql_err)?;
        // SPEC 12 §10 — load or generate the per-namespace audit key.
        // The key persists across daemon restarts so historical digests
        // remain verifiable for the namespace's lifetime; the spec
        // notes that v0.1 does not rotate the key automatically.
        let audit_key = load_or_generate_audit_key(&inner.conn, &spec.name)?;
        inner.namespaces.insert(
            spec.name.clone(),
            NamespaceReg {
                delete_mode: spec.delete_mode,
                tombstone_ttl: spec.tombstone_ttl,
                mapping,
                audit_key,
            },
        );
        Ok(())
    }

    /// Borrow-friendly accessor for the namespace's audit key. Callers
    /// MUST keep the returned key inside the owning service — it is the
    /// privileged half of `<svc>.props.audit` verification and never
    /// leaves the daemon over the wire.
    pub fn audit_key(&self, namespace: &NamespaceName) -> Option<AuditKey> {
        let inner = self.inner.lock().expect("sqlite store poisoned");
        inner
            .namespaces
            .get(namespace)
            .map(|reg| reg.audit_key.clone())
    }
}

fn load_or_generate_audit_key(
    conn: &Connection,
    namespace: &NamespaceName,
) -> Result<AuditKey, StoreError> {
    let existing: Option<Vec<u8>> = conn
        .query_row(
            "SELECT key_bytes FROM __props_audit_keys WHERE namespace = ?1",
            params![namespace.as_str()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some(bytes) = existing {
        if bytes.len() != 32 {
            return Err(StoreError::storage(format!(
                "__props_audit_keys row for {:?} has {} bytes; expected 32",
                namespace.as_str(),
                bytes.len()
            )));
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        return Ok(AuditKey::from_bytes(k));
    }
    let key = AuditKey::generate();
    conn.execute(
        "INSERT INTO __props_audit_keys (namespace, key_bytes) VALUES (?1, ?2)",
        params![namespace.as_str(), key.as_bytes().as_slice()],
    )
    .map_err(sql_err)?;
    Ok(key)
}

fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::storage(e.to_string())
}

fn ts_to_dt(ts_ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ts_ms)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is representable"))
}

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

fn changed_top_level_fields(old: Option<&PropValue>, new: &PropValue) -> Vec<String> {
    match (old, new) {
        (None, PropValue::Object(n)) => n.keys().cloned().collect(),
        (Some(PropValue::Object(o)), PropValue::Object(n)) => {
            let keys: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            keys.into_iter()
                .filter(|k| o.get(*k) != n.get(*k))
                .cloned()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Encode `Lifecycle` to two SQL columns: kind tag + optional reason.
fn lifecycle_to_cols(lc: Option<&Lifecycle>) -> (Option<&'static str>, Option<String>) {
    match lc {
        None => (None, None),
        Some(Lifecycle::Provisioning) => (Some("provisioning"), None),
        Some(Lifecycle::Active) => (Some("active"), None),
        Some(Lifecycle::Failed { reason }) => (Some("failed"), Some(reason.clone())),
    }
}

fn lifecycle_from_cols(
    kind: Option<String>,
    reason: Option<String>,
) -> Result<Option<Lifecycle>, StoreError> {
    match kind.as_deref() {
        None => Ok(None),
        Some("provisioning") => Ok(Some(Lifecycle::Provisioning)),
        Some("active") => Ok(Some(Lifecycle::Active)),
        Some("failed") => Ok(Some(Lifecycle::Failed {
            reason: reason.unwrap_or_default(),
        })),
        // SPEC 12 §6.5 lifecycle is a closed alphabet at this version.
        // An unknown tag means either DB corruption or a schema
        // mismatch with a future substrate — silently dropping it
        // would make a Saga row look Simple and lose `_lifecycle`
        // state on the next write. Surface loudly.
        Some(other) => Err(StoreError::storage(format!(
            "unknown lifecycle_kind {other:?} in __props_records"
        ))),
    }
}

fn kind_to_str(k: EventKind) -> &'static str {
    match k {
        EventKind::Created => "created",
        EventKind::Updated => "updated",
        EventKind::Deleted => "deleted",
        EventKind::Completed => "completed",
        EventKind::Reconciled => "reconciled",
    }
}

fn kind_from_str(s: &str) -> Result<EventKind, StoreError> {
    match s {
        "created" => Ok(EventKind::Created),
        "updated" => Ok(EventKind::Updated),
        "deleted" => Ok(EventKind::Deleted),
        "completed" => Ok(EventKind::Completed),
        "reconciled" => Ok(EventKind::Reconciled),
        other => Err(StoreError::storage(format!("unknown event kind {other:?}"))),
    }
}

/// Snapshot of a `__props_records` row read inside a transaction.
struct MetaRow {
    version: Version,
    nseq: Nseq,
    lifecycle: Option<Lifecycle>,
    tombstoned: bool,
}

/// Raw column tuple returned from the SQL row callback. Decoupled
/// from [`MetaRow`] so the closure stays `rusqlite::Result`-pure and
/// the lifecycle-tag → `Lifecycle` mapping (which can fail with
/// [`StoreError::Storage`] on an unknown tag) runs outside.
struct MetaCols {
    version: i64,
    nseq: i64,
    lifecycle_kind: Option<String>,
    lifecycle_reason: Option<String>,
    tombstoned: i64,
}

fn read_meta(
    tx: &Transaction<'_>,
    ns: &NamespaceName,
    key: &str,
) -> Result<Option<MetaRow>, StoreError> {
    let raw = tx
        .query_row(
            "SELECT version, nseq, lifecycle_kind, lifecycle_reason, tombstoned \
             FROM __props_records WHERE namespace = ?1 AND key = ?2",
            params![ns.as_str(), key],
            |r| {
                Ok(MetaCols {
                    version: r.get(0)?,
                    nseq: r.get(1)?,
                    lifecycle_kind: r.get(2)?,
                    lifecycle_reason: r.get(3)?,
                    tombstoned: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(sql_err)?;
    match raw {
        None => Ok(None),
        Some(c) => Ok(Some(MetaRow {
            version: Version(c.version as u64),
            nseq: Nseq(c.nseq as u64),
            lifecycle: lifecycle_from_cols(c.lifecycle_kind, c.lifecycle_reason)?,
            tombstoned: c.tombstoned != 0,
        })),
    }
}

fn meta_ns_state(
    tx: &Transaction<'_>,
    ns: &NamespaceName,
) -> Result<(Nseq, AuditEpoch), StoreError> {
    let row = tx
        .query_row(
            "SELECT nseq, audit_epoch FROM __props_meta WHERE namespace = ?1",
            params![ns.as_str()],
            |r| {
                let n: i64 = r.get(0)?;
                let e: i64 = r.get(1)?;
                Ok((Nseq(n as u64), AuditEpoch(e as u64)))
            },
        )
        .optional()
        .map_err(sql_err)?;
    row.ok_or_else(|| StoreError::storage(format!("namespace {:?} not registered", ns.as_str())))
}

fn bump_ns_nseq(
    tx: &Transaction<'_>,
    ns: &NamespaceName,
    new_nseq: Nseq,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE __props_meta SET nseq = ?2 WHERE namespace = ?1",
        params![ns.as_str(), new_nseq.0 as i64],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn bump_ns_audit_epoch(
    tx: &Transaction<'_>,
    ns: &NamespaceName,
    new_epoch: AuditEpoch,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE __props_meta SET audit_epoch = ?2 WHERE namespace = ?1",
        params![ns.as_str(), new_epoch.0 as i64],
    )
    .map_err(sql_err)?;
    Ok(())
}

struct MetaUpsert<'a> {
    ns: &'a NamespaceName,
    key: &'a str,
    version: Version,
    nseq: Nseq,
    lifecycle: Option<&'a Lifecycle>,
    tombstoned: bool,
    tombstone_at_ms: Option<i64>,
}

fn upsert_record_meta(tx: &Transaction<'_>, op: MetaUpsert<'_>) -> Result<(), StoreError> {
    let MetaUpsert {
        ns,
        key,
        version,
        nseq,
        lifecycle,
        tombstoned,
        tombstone_at_ms,
    } = op;
    let (lk, lr) = lifecycle_to_cols(lifecycle);
    tx.execute(
        "INSERT INTO __props_records \
            (namespace, key, version, nseq, lifecycle_kind, lifecycle_reason, tombstoned, tombstone_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(namespace, key) DO UPDATE SET \
            version = excluded.version, \
            nseq = excluded.nseq, \
            lifecycle_kind = excluded.lifecycle_kind, \
            lifecycle_reason = excluded.lifecycle_reason, \
            tombstoned = excluded.tombstoned, \
            tombstone_at_ms = excluded.tombstone_at_ms",
        params![
            ns.as_str(),
            key,
            version.0 as i64,
            nseq.0 as i64,
            lk,
            lr,
            tombstoned as i64,
            tombstone_at_ms,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn insert_event(
    tx: &Transaction<'_>,
    service: &str,
    ns: &NamespaceName,
    event: &RecordEvent,
) -> Result<(), StoreError> {
    let (lk, lr) = lifecycle_to_cols(event.lifecycle.as_ref());
    let fields_changed_json =
        serde_json::to_string(&event.fields_changed).expect("Vec<String> serialises");
    let _ = service; // verb already composed by the caller via EventKind::verb_for
    tx.execute(
        "INSERT INTO __props_events \
            (namespace, nseq, version, audit_epoch, kind, verb, key, field_path, \
             lifecycle_kind, lifecycle_reason, actor, fields_changed, \
             secret_fields_changed_count, at_ms, cause, audit_digest) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            ns.as_str(),
            event.nseq.0 as i64,
            event.version.0 as i64,
            event.audit_epoch.0 as i64,
            kind_to_str(event.kind),
            event.verb,
            event.key.key,
            serde_json::to_string(&event.key.field_path).expect("field path serialises"),
            lk,
            lr,
            event.actor.as_str(),
            fields_changed_json,
            event.secret_fields_changed_count as i64,
            event.at.timestamp_millis(),
            event.cause,
            event.audit_digest,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

impl PropertyStore for SqliteStore {
    fn get<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Snapshot<Record>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(&key.namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let mapping = Arc::clone(&reg.mapping);

            // Wrap the three reads (nseq cursor, record meta, mapping
            // value) in a single deferred transaction so the returned
            // Snapshot's observed_nseq is consistent with the metadata
            // and value rows — even if another writer interleaves on
            // the same database from a separate connection.
            let tx = conn.transaction().map_err(sql_err)?;

            let observed_nseq = {
                let n: i64 = tx
                    .query_row(
                        "SELECT nseq FROM __props_meta WHERE namespace = ?1",
                        params![key.namespace.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(sql_err)?;
                Nseq(n as u64)
            };
            let raw = tx
                .query_row(
                    "SELECT version, nseq, lifecycle_kind, lifecycle_reason, tombstoned \
                     FROM __props_records WHERE namespace = ?1 AND key = ?2",
                    params![key.namespace.as_str(), key.key],
                    |r| {
                        Ok(MetaCols {
                            version: r.get(0)?,
                            nseq: r.get(1)?,
                            lifecycle_kind: r.get(2)?,
                            lifecycle_reason: r.get(3)?,
                            tombstoned: r.get(4)?,
                        })
                    },
                )
                .optional()
                .map_err(sql_err)?;
            let meta = match raw {
                None => return Err(StoreError::NotFound),
                // SPEC 12 §5.4 — get on a tombstone returns not_found.
                Some(c) if c.tombstoned != 0 => return Err(StoreError::NotFound),
                Some(c) => MetaRow {
                    version: Version(c.version as u64),
                    nseq: Nseq(c.nseq as u64),
                    lifecycle: lifecycle_from_cols(c.lifecycle_kind, c.lifecycle_reason)?,
                    tombstoned: false,
                },
            };
            let value = mapping
                .read(&tx, &key.key)
                .map_err(sql_err)?
                .ok_or_else(|| {
                    StoreError::storage(format!(
                        "mapping/record divergence: __props_records has row for {:?} but mapping has none",
                        key.key
                    ))
                })?;
            // Read-only — explicit rollback is cheaper than dropping
            // an unused write lock, but either is semantically fine.
            drop(tx);
            let record = Record {
                key: key.clone(),
                value,
                version: meta.version,
                nseq: meta.nseq,
                lifecycle: meta.lifecycle,
            };
            Ok(Snapshot::new(record, observed_nseq))
        })
    }

    fn list<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, Snapshot<Vec<Record>>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let mapping = Arc::clone(&reg.mapping);

            // Same-snapshot watch-cursor contract: nseq, mapping list,
            // and record rows must all observe the same point in time.
            let tx = conn.transaction().map_err(sql_err)?;

            let observed_nseq = {
                let n: i64 = tx
                    .query_row(
                        "SELECT nseq FROM __props_meta WHERE namespace = ?1",
                        params![namespace.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(sql_err)?;
                Nseq(n as u64)
            };

            let values: Vec<(String, PropValue)> = mapping.list(&tx).map_err(sql_err)?;
            let value_map: BTreeMap<String, PropValue> = values.into_iter().collect();

            let mut stmt = tx
                .prepare(
                    "SELECT key, version, nseq, lifecycle_kind, lifecycle_reason \
                     FROM __props_records WHERE namespace = ?1 AND tombstoned = 0 ORDER BY key",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![namespace.as_str()], |r| {
                    let k: String = r.get(0)?;
                    let v: i64 = r.get(1)?;
                    let n: i64 = r.get(2)?;
                    let lk: Option<String> = r.get(3)?;
                    let lr: Option<String> = r.get(4)?;
                    Ok((k, v, n, lk, lr))
                })
                .map_err(sql_err)?;

            let mut records = Vec::new();
            for row in rows {
                let (k, v, n, lk, lr) = row.map_err(sql_err)?;
                let value = value_map.get(&k).cloned().ok_or_else(|| {
                    StoreError::storage(format!(
                        "mapping/record divergence: missing value for {k:?}"
                    ))
                })?;
                records.push(Record {
                    key: RecordKey {
                        namespace: namespace.clone(),
                        key: k,
                        field_path: Vec::<FieldPathSegment>::new(),
                    },
                    value,
                    version: Version(v as u64),
                    nseq: Nseq(n as u64),
                    lifecycle: lifecycle_from_cols(lk, lr)?,
                });
            }
            Ok(Snapshot::new(records, observed_nseq))
        })
    }

    fn commit_set<'a>(&'a self, op: SetCommit) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            // Same record-root guard as MemoryStore — sub-path mutation
            // is not a storage primitive.
            if !op.key.is_whole_record() {
                return Err(StoreError::validation(
                    "commit_set requires record-root key (empty field_path)",
                ));
            }
            // SPEC 12 §6.5 reserved-field guard.
            if let PropValue::Object(ref m) = op.new_value
                && m.contains_key(LIFECYCLE_FIELD)
            {
                return Err(StoreError::validation(format!(
                    "field {LIFECYCLE_FIELD:?} is substrate-reserved (§6.5)"
                )));
            }

            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(&op.key.namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let mapping = Arc::clone(&reg.mapping);
            let audit_key = reg.audit_key.clone();

            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;

            let (ns_nseq, audit_epoch) = meta_ns_state(&tx, &op.key.namespace)?;
            let meta = read_meta(&tx, &op.key.namespace, &op.key.key)?;
            let prev_version = meta.as_ref().map(|m| m.version).unwrap_or(Version::zero());
            let was_tombstone = meta.as_ref().map(|m| m.tombstoned).unwrap_or(false);

            if let Some(expected) = op.expected_version
                && expected != prev_version
            {
                return Err(StoreError::VersionMismatch {
                    expected,
                    current: prev_version,
                });
            }

            // Prior value: tombstoned rows have no mapping value (delete
            // dropped it), so treat as absent for merge purposes. For a
            // live record (`meta.is_some() && !was_tombstone`) the
            // mapping MUST hold a value, and conversely (`meta.is_none()`)
            // the mapping MUST hold nothing — divergence in either
            // direction is storage corruption, not something to silently
            // heal by promoting the op into a Create or Update. An
            // externally-pre-populated business row must arrive through
            // commit_reconcile, not commit_set.
            let prior_value = match (meta.is_some(), was_tombstone) {
                (true, false) => match mapping.read(&tx, &op.key.key).map_err(sql_err)? {
                    Some(v) => Some(v),
                    None => {
                        return Err(StoreError::storage(format!(
                            "mapping/record divergence: __props_records has live row for {:?} but mapping has none",
                            op.key.key
                        )));
                    }
                },
                // No metadata (or tombstoned ⇒ mapping was wiped on
                // delete): mapping MUST be empty. A non-None read here
                // means an external writer touched the business table
                // without going through commit_*.
                _ => {
                    if mapping.read(&tx, &op.key.key).map_err(sql_err)?.is_some() {
                        return Err(StoreError::storage(format!(
                            "mapping/record divergence: mapping holds a value for {:?} but __props_records has no live row (route external pre-population through commit_reconcile)",
                            op.key.key
                        )));
                    }
                    None
                }
            };

            let final_value = match (op.merge, prior_value.as_ref()) {
                (MergeMode::Replace, _) | (MergeMode::Patch, None) => op.new_value.clone(),
                (MergeMode::Patch, Some(prior)) => merge_patch(prior, op.new_value.clone()),
            };

            let new_version = prev_version.next();
            let new_nseq = ns_nseq.next();
            let kind = if meta.is_some() && !was_tombstone {
                EventKind::Updated
            } else {
                EventKind::Created
            };
            let fields_changed = changed_top_level_fields(prior_value.as_ref(), &final_value);

            mapping
                .upsert(&tx, &op.key.key, &final_value)
                .map_err(sql_err)?;
            upsert_record_meta(
                &tx,
                MetaUpsert {
                    ns: &op.key.namespace,
                    key: &op.key.key,
                    version: new_version,
                    nseq: new_nseq,
                    lifecycle: op.lifecycle.as_ref(),
                    tombstoned: false,
                    tombstone_at_ms: None,
                },
            )?;
            bump_ns_nseq(&tx, &op.key.namespace, new_nseq)?;

            let audit_digest = compute_digest(&audit_key, &final_value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: new_version,
                audit_epoch,
                kind,
                verb: kind.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: op.lifecycle.clone(),
                actor: op.actor,
                fields_changed,
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: op.cause,
                audit_digest,
            };
            insert_event(&tx, &self.service, &op.key.namespace, &event)?;
            tx.commit().map_err(sql_err)?;

            let record = Record {
                key: op.key.clone(),
                value: final_value,
                version: new_version,
                nseq: new_nseq,
                lifecycle: op.lifecycle,
            };
            Ok((record, event))
        })
    }

    fn commit_delete<'a>(&'a self, op: DeleteCommit) -> StoreFuture<'a, RecordEvent> {
        Box::pin(async move {
            if !op.key.is_whole_record() {
                return Err(StoreError::validation(
                    "commit_delete requires record-root key (empty field_path)",
                ));
            }

            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(&op.key.namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let delete_mode = reg.delete_mode;
            let mapping = Arc::clone(&reg.mapping);
            let audit_key = reg.audit_key.clone();

            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;

            let (ns_nseq, audit_epoch) = meta_ns_state(&tx, &op.key.namespace)?;
            let meta =
                read_meta(&tx, &op.key.namespace, &op.key.key)?.ok_or(StoreError::NotFound)?;
            // Deleting an already-tombstoned record is a not_found —
            // the spec treats a tombstone as "gone" from get/delete's
            // perspective; only set against a tombstone observes the
            // preserved version.
            if meta.tombstoned {
                return Err(StoreError::NotFound);
            }

            if let Some(expected) = op.expected_version
                && expected != meta.version
            {
                return Err(StoreError::VersionMismatch {
                    expected,
                    current: meta.version,
                });
            }

            let new_nseq = ns_nseq.next();
            // §5.4 tombstone_version = prev + 1.
            let tombstone_version = meta.version.next();

            // A live (non-tombstoned) record MUST have a mapping value;
            // a missing value here means the substrate metadata and the
            // daemon-supplied mapping have diverged. Surface as storage
            // corruption rather than silently emitting a delete event
            // for a row that was never fully present.
            if mapping.read(&tx, &op.key.key).map_err(sql_err)?.is_none() {
                return Err(StoreError::storage(format!(
                    "mapping/record divergence: __props_records has live row for {:?} but mapping has none",
                    op.key.key
                )));
            }

            mapping.delete(&tx, &op.key.key).map_err(sql_err)?;

            match delete_mode {
                DeleteMode::HardDelete => {
                    tx.execute(
                        "DELETE FROM __props_records WHERE namespace = ?1 AND key = ?2",
                        params![op.key.namespace.as_str(), op.key.key],
                    )
                    .map_err(sql_err)?;
                }
                DeleteMode::SoftDelete => {
                    upsert_record_meta(
                        &tx,
                        MetaUpsert {
                            ns: &op.key.namespace,
                            key: &op.key.key,
                            version: tombstone_version,
                            nseq: new_nseq,
                            lifecycle: None,
                            tombstoned: true,
                            tombstone_at_ms: Some(op.ts_ms),
                        },
                    )?;
                }
            }
            bump_ns_nseq(&tx, &op.key.namespace, new_nseq)?;

            let audit_digest = compute_digest(&audit_key, &tombstone_value(), new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: tombstone_version,
                audit_epoch,
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
            insert_event(&tx, &self.service, &op.key.namespace, &event)?;
            tx.commit().map_err(sql_err)?;

            Ok(event)
        })
    }

    fn commit_complete<'a>(&'a self, op: CompleteCommit) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(&op.key.namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let mapping = Arc::clone(&reg.mapping);
            let audit_key = reg.audit_key.clone();

            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;

            let (ns_nseq, audit_epoch) = meta_ns_state(&tx, &op.key.namespace)?;
            let meta =
                read_meta(&tx, &op.key.namespace, &op.key.key)?.ok_or(StoreError::NotFound)?;
            // SPEC 12 §6.5 — complete runs against a live record only.
            if meta.tombstoned {
                return Err(StoreError::NotFound);
            }

            // Version check FIRST so racing writes surface as
            // VersionMismatch (matches MemoryStore semantics).
            if meta.version != op.expected_version {
                return Err(StoreError::VersionMismatch {
                    expected: op.expected_version,
                    current: meta.version,
                });
            }
            if !matches!(meta.lifecycle, Some(Lifecycle::Provisioning)) {
                return Err(StoreError::validation(
                    "commit_complete requires record.lifecycle = Provisioning",
                ));
            }
            if matches!(op.lifecycle, Lifecycle::Provisioning) {
                return Err(StoreError::validation(
                    "commit_complete target lifecycle MUST be Active or Failed",
                ));
            }

            let new_version = meta.version.next();
            let new_nseq = ns_nseq.next();

            upsert_record_meta(
                &tx,
                MetaUpsert {
                    ns: &op.key.namespace,
                    key: &op.key.key,
                    version: new_version,
                    nseq: new_nseq,
                    lifecycle: Some(&op.lifecycle),
                    tombstoned: false,
                    tombstone_at_ms: None,
                },
            )?;
            bump_ns_nseq(&tx, &op.key.namespace, new_nseq)?;

            // Read back value for the returned Record (unchanged by complete).
            let value = mapping
                .read(&tx, &op.key.key)
                .map_err(sql_err)?
                .ok_or_else(|| {
                    StoreError::storage("mapping/record divergence in commit_complete")
                })?;

            let audit_digest = compute_digest(&audit_key, &value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: new_version,
                audit_epoch,
                kind: EventKind::Completed,
                verb: EventKind::Completed.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: Some(op.lifecycle.clone()),
                actor: op.actor,
                fields_changed: vec![LIFECYCLE_FIELD.to_string()],
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: op.cause,
                audit_digest,
            };
            insert_event(&tx, &self.service, &op.key.namespace, &event)?;
            tx.commit().map_err(sql_err)?;

            let record = Record {
                key: op.key.clone(),
                value,
                version: new_version,
                nseq: new_nseq,
                lifecycle: Some(op.lifecycle),
            };
            Ok((record, event))
        })
    }

    fn commit_reconcile<'a>(
        &'a self,
        op: ReconcileCommit,
    ) -> StoreFuture<'a, (Record, RecordEvent)> {
        Box::pin(async move {
            let mut inner = self.inner.lock().expect("sqlite store poisoned");
            let Inner { conn, namespaces } = &mut *inner;
            let reg = namespaces
                .get(&op.key.namespace)
                .ok_or_else(|| StoreError::storage("namespace not registered"))?;
            let mapping = Arc::clone(&reg.mapping);
            let audit_key = reg.audit_key.clone();

            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;

            let (ns_nseq, audit_epoch) = meta_ns_state(&tx, &op.key.namespace)?;
            let meta = read_meta(&tx, &op.key.namespace, &op.key.key)?;
            let prev_version = meta.as_ref().map(|m| m.version).unwrap_or(Version::zero());
            let preserved_lifecycle = meta.as_ref().and_then(|m| m.lifecycle.clone());
            let was_tombstone = meta.as_ref().map(|m| m.tombstoned).unwrap_or(false);

            // Same mapping/record divergence guard as commit_set /
            // commit_delete. Reconcile is the verb for an observed
            // *value* drift; an externally-deleted row must come
            // through commit_delete, not be silently resurrected here.
            // Absent metadata (first reconcile) and tombstones are
            // legitimate "no prior mapping value" cases and skip the
            // check.
            if meta.is_some()
                && !was_tombstone
                && mapping.read(&tx, &op.key.key).map_err(sql_err)?.is_none()
            {
                return Err(StoreError::storage(format!(
                    "mapping/record divergence: __props_records has live row for {:?} but mapping has none",
                    op.key.key
                )));
            }

            // Reconcile is the ONLY path that bumps audit_epoch (§11).
            let new_audit_epoch = audit_epoch.next();
            bump_ns_audit_epoch(&tx, &op.key.namespace, new_audit_epoch)?;

            let new_version = prev_version.next();
            let new_nseq = ns_nseq.next();
            let fields_changed = changed_top_level_fields(op.old_value.as_ref(), &op.new_value);

            mapping
                .upsert(&tx, &op.key.key, &op.new_value)
                .map_err(sql_err)?;
            upsert_record_meta(
                &tx,
                MetaUpsert {
                    ns: &op.key.namespace,
                    key: &op.key.key,
                    version: new_version,
                    nseq: new_nseq,
                    lifecycle: preserved_lifecycle.as_ref(),
                    tombstoned: false,
                    tombstone_at_ms: None,
                },
            )?;
            bump_ns_nseq(&tx, &op.key.namespace, new_nseq)?;

            let audit_digest = compute_digest(&audit_key, &op.new_value, new_nseq);
            let event = RecordEvent {
                nseq: new_nseq,
                version: new_version,
                audit_epoch: new_audit_epoch,
                kind: EventKind::Reconciled,
                verb: EventKind::Reconciled.verb_for(&self.service),
                key: op.key.clone(),
                lifecycle: preserved_lifecycle.clone(),
                actor: Actor::reconciliation(),
                fields_changed,
                secret_fields_changed_count: 0,
                at: ts_to_dt(op.ts_ms),
                cause: Some("external_edit_detected".to_string()),
                audit_digest,
            };
            insert_event(&tx, &self.service, &op.key.namespace, &event)?;
            tx.commit().map_err(sql_err)?;

            let record = Record {
                key: op.key.clone(),
                value: op.new_value,
                version: new_version,
                nseq: new_nseq,
                lifecycle: preserved_lifecycle,
            };
            Ok((record, event))
        })
    }

    fn events_since<'a>(
        &'a self,
        namespace: &'a NamespaceName,
        since_nseq: Nseq,
    ) -> StoreFuture<'a, Vec<RecordEvent>> {
        Box::pin(async move {
            let inner = self.inner.lock().expect("sqlite store poisoned");
            // Unregistered namespace ⇒ empty event stream (matches
            // MemoryStore — replay before registration is meaningless).
            if !inner.namespaces.contains_key(namespace) {
                return Ok(Vec::new());
            }
            let mut stmt = inner
                .conn
                .prepare(
                    "SELECT nseq, version, audit_epoch, kind, verb, key, field_path, \
                            lifecycle_kind, lifecycle_reason, actor, fields_changed, \
                            secret_fields_changed_count, at_ms, cause, audit_digest \
                     FROM __props_events WHERE namespace = ?1 AND nseq > ?2 ORDER BY nseq",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![namespace.as_str(), since_nseq.0 as i64], |r| {
                    Ok(EventRow {
                        nseq: r.get::<_, i64>(0)?,
                        version: r.get::<_, i64>(1)?,
                        audit_epoch: r.get::<_, i64>(2)?,
                        kind: r.get::<_, String>(3)?,
                        verb: r.get::<_, String>(4)?,
                        key: r.get::<_, String>(5)?,
                        field_path: r.get::<_, String>(6)?,
                        lifecycle_kind: r.get::<_, Option<String>>(7)?,
                        lifecycle_reason: r.get::<_, Option<String>>(8)?,
                        actor: r.get::<_, String>(9)?,
                        fields_changed: r.get::<_, String>(10)?,
                        secret_fields_changed_count: r.get::<_, i64>(11)?,
                        at_ms: r.get::<_, i64>(12)?,
                        cause: r.get::<_, Option<String>>(13)?,
                        audit_digest: r.get::<_, String>(14)?,
                    })
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let row = row.map_err(sql_err)?;
                let field_path: Vec<FieldPathSegment> = serde_json::from_str(&row.field_path)
                    .map_err(|e| StoreError::storage(format!("field_path decode: {e}")))?;
                let fields_changed: Vec<String> = serde_json::from_str(&row.fields_changed)
                    .map_err(|e| StoreError::storage(format!("fields_changed decode: {e}")))?;
                let kind = kind_from_str(&row.kind)?;
                out.push(RecordEvent {
                    nseq: Nseq(row.nseq as u64),
                    version: Version(row.version as u64),
                    audit_epoch: AuditEpoch(row.audit_epoch as u64),
                    kind,
                    verb: row.verb,
                    key: RecordKey {
                        namespace: namespace.clone(),
                        key: row.key,
                        field_path,
                    },
                    lifecycle: lifecycle_from_cols(row.lifecycle_kind, row.lifecycle_reason)?,
                    actor: Actor::from_token(row.actor)
                        .map_err(|e| StoreError::storage(format!("actor decode: {e}")))?,
                    fields_changed,
                    secret_fields_changed_count: row.secret_fields_changed_count as u32,
                    at: ts_to_dt(row.at_ms),
                    cause: row.cause,
                    audit_digest: row.audit_digest,
                });
            }
            Ok(out)
        })
    }

    fn audit_epoch<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, AuditEpoch> {
        Box::pin(async move {
            let inner = self.inner.lock().expect("sqlite store poisoned");
            let row: Option<i64> = inner
                .conn
                .query_row(
                    "SELECT audit_epoch FROM __props_meta WHERE namespace = ?1",
                    params![namespace.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(AuditEpoch(row.unwrap_or(0) as u64))
        })
    }

    fn version_anchor<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Option<Version>> {
        // SoftDelete tombstones retain the meta row (`tombstoned=1`)
        // with the tombstone version. The trait contract requires this
        // method to surface that version so `commit_set` OCC succeeds on
        // a recreate-after-soft-delete flow. Read directly from
        // `__props_records.version` — same column `commit_set` validates
        // against via `read_meta`.
        let key_str = key.key.clone();
        let ns_str = key.namespace.as_str().to_string();
        Box::pin(async move {
            let inner = self.inner.lock().expect("sqlite store poisoned");
            let row: Option<i64> = inner
                .conn
                .query_row(
                    "SELECT version FROM __props_records \
                     WHERE namespace = ?1 AND key = ?2",
                    params![ns_str, key_str],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(row.map(|v| Version(v as u64)))
        })
    }
}

/// Intermediate row read from `__props_events`. Decoupling the row-read
/// from the public event type makes the `query_map` closure
/// `rusqlite::Result`-pure (no JSON decode inside the row callback,
/// which would force a custom error mapping).
struct EventRow {
    nseq: i64,
    version: i64,
    audit_epoch: i64,
    kind: String,
    verb: String,
    key: String,
    field_path: String,
    lifecycle_kind: Option<String>,
    lifecycle_reason: Option<String>,
    actor: String,
    fields_changed: String,
    secret_fields_changed_count: i64,
    at_ms: i64,
    cause: Option<String>,
    audit_digest: String,
}

// silence "unused" warnings if json is dropped in a future refactor
#[allow(dead_code)]
fn _types_used(_: Json) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::{Cardinality, PropertySchema, StorageBackendKind};

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

    fn schema() -> PropertySchema {
        PropertySchema::new(Vec::new())
    }

    fn spec(delete_mode: DeleteMode) -> NamespaceSpec {
        let mut s = NamespaceSpec::new(
            ns(),
            schema(),
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::SqliteTable {
                table: "__props_values".into(),
            },
        );
        s.delete_mode = delete_mode;
        s
    }

    fn open_store(delete_mode: DeleteMode) -> SqliteStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteStore::new("maild", conn).unwrap();
        let mapping = Arc::new(JsonValuesMapping::new(ns()));
        store
            .register_namespace(&spec(delete_mode), mapping)
            .unwrap();
        store
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
            actor: Actor::service("maild").expect("valid actor"),
            cause: None,
            ts_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn simple_set_creates_then_updates() {
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "u@h");
        let (rec1, ev1) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(rec1.version, Version(1));
        assert_eq!(ev1.kind, EventKind::Created);
        assert_eq!(ev1.verb, "maild.props.set");

        let (rec2, ev2) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("display", PropValue::from("Alice"))]),
                None,
                Some(Version(1)),
            ))
            .await
            .unwrap();
        assert_eq!(rec2.version, Version(2));
        assert_eq!(ev2.kind, EventKind::Updated);
        let merged = rec2.value.as_object().unwrap();
        assert_eq!(merged["email"], PropValue::from("u@h"));
        assert_eq!(merged["display"], PropValue::from("Alice"));
    }

    #[tokio::test]
    async fn version_mismatch_refused() {
        let store = open_store(DeleteMode::SoftDelete);
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
        let err = store
            .commit_set(set_commit(
                key,
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version(7)),
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::VersionMismatch { current, .. } if current == Version(1))
        );
    }

    #[tokio::test]
    async fn reserved_lifecycle_field_refused() {
        let store = open_store(DeleteMode::SoftDelete);
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
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "u@h");
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("Init")),
                ]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let (rec, _) = store
            .commit_set(SetCommit {
                merge: MergeMode::Replace,
                ..set_commit(
                    key,
                    obj(&[("email", PropValue::from("u@h"))]),
                    None,
                    Some(Version(1)),
                )
            })
            .await
            .unwrap();
        assert!(rec.value.as_object().unwrap().get("display").is_none());
    }

    #[tokio::test]
    async fn hard_delete_removes_row_and_resets_version_sequence() {
        let store = open_store(DeleteMode::HardDelete);
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
                actor: Actor::service("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(ev.version, Version(2));
        assert!(matches!(
            store.get(&key).await.unwrap_err(),
            StoreError::NotFound
        ));
        // Hard-delete restarts: a create-new (expected_version = 0)
        // succeeds against the just-deleted key.
        let (rec, ev2) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(rec.version, Version(1));
        assert_eq!(ev2.kind, EventKind::Created);
    }

    #[tokio::test]
    async fn soft_delete_preserves_version_sequence_across_recreate() {
        let store = open_store(DeleteMode::SoftDelete);
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
                actor: Actor::service("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        // tombstone_version = prev + 1
        assert_eq!(ev.version, Version(2));

        // get reports not_found (tombstone hidden from reads).
        assert!(matches!(
            store.get(&key).await.unwrap_err(),
            StoreError::NotFound
        ));

        // Creating with expected_version = 0 against a tombstone is
        // refused — the spec preserves the version sequence, not
        // restart-at-zero.
        let err = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::VersionMismatch { current, .. } if current == Version(2))
        );

        // Setting against the tombstone version succeeds and resumes
        // the sequence at tombstone_version + 1.
        let (rec, ev2) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version(2)),
            ))
            .await
            .unwrap();
        assert_eq!(rec.version, Version(3));
        // Mapping was emptied on delete, so the recreate is Created,
        // not Updated.
        assert_eq!(ev2.kind, EventKind::Created);
    }

    #[tokio::test]
    async fn version_anchor_surfaces_live_tombstone_and_absent() {
        let store = open_store(DeleteMode::SoftDelete);
        let live_key = RecordKey::collection(ns(), "live@h");
        let tomb_key = RecordKey::collection(ns(), "tomb@h");
        let gone_key = RecordKey::collection(ns(), "gone@h");

        // absent: never written
        assert_eq!(store.version_anchor(&gone_key).await.unwrap(), None);

        // live: write once, anchor reports the live version
        store
            .commit_set(set_commit(
                live_key.clone(),
                obj(&[("email", PropValue::from("live@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(
            store.version_anchor(&live_key).await.unwrap(),
            Some(Version(1)),
        );

        // tombstone: write then soft-delete; anchor reports the tombstone
        // version so the recreate-after-delete flow can use it as
        // `expected_version` without round-tripping through events_since.
        store
            .commit_set(set_commit(
                tomb_key.clone(),
                obj(&[("email", PropValue::from("tomb@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        store
            .commit_delete(DeleteCommit {
                key: tomb_key.clone(),
                expected_version: Some(Version(1)),
                actor: Actor::service("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert!(matches!(
            store.get(&tomb_key).await.unwrap_err(),
            StoreError::NotFound
        ));
        assert_eq!(
            store.version_anchor(&tomb_key).await.unwrap(),
            Some(Version(2)),
        );

        // Using the surfaced anchor against a tombstone recreates the
        // record per §5.4 — proves the contract end-to-end, not just
        // the column read.
        let (rec, _) = store
            .commit_set(set_commit(
                tomb_key.clone(),
                obj(&[("email", PropValue::from("tomb@h"))]),
                None,
                store.version_anchor(&tomb_key).await.unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(rec.version, Version(3));
    }

    #[tokio::test]
    async fn saga_provisioning_then_complete_active() {
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "u@h");
        let (rec, _) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                Some(Lifecycle::Provisioning),
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        assert_eq!(rec.lifecycle, Some(Lifecycle::Provisioning));

        let (rec2, ev2) = store
            .commit_complete(CompleteCommit {
                key: key.clone(),
                lifecycle: Lifecycle::Active,
                expected_version: Version(1),
                actor: Actor::daemon_complete("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(rec2.lifecycle, Some(Lifecycle::Active));
        assert_eq!(ev2.kind, EventKind::Completed);
        assert_eq!(ev2.fields_changed, vec![LIFECYCLE_FIELD.to_string()]);

        // Round-trip get sees lifecycle.
        let snap = store.get(&key).await.unwrap();
        assert_eq!(snap.value.lifecycle, Some(Lifecycle::Active));
    }

    #[tokio::test]
    async fn saga_complete_failed_persists_reason() {
        let store = open_store(DeleteMode::SoftDelete);
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
        store
            .commit_complete(CompleteCommit {
                key: key.clone(),
                lifecycle: Lifecycle::Failed {
                    reason: "mds seed failed".into(),
                },
                expected_version: Version(1),
                actor: Actor::daemon_complete("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        let snap = store.get(&key).await.unwrap();
        assert!(matches!(
            snap.value.lifecycle,
            Some(Lifecycle::Failed { ref reason }) if reason == "mds seed failed"
        ));
    }

    #[tokio::test]
    async fn complete_version_mismatch_takes_priority_over_lifecycle() {
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "u@h");
        // Simple set (no lifecycle) — but expect a stale version
        // mismatch first, before the lifecycle precondition.
        store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("email", PropValue::from("u@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        let err = store
            .commit_complete(CompleteCommit {
                key: key.clone(),
                lifecycle: Lifecycle::Active,
                expected_version: Version(99),
                actor: Actor::daemon_complete("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::VersionMismatch { current, .. } if current == Version(1))
        );
    }

    #[tokio::test]
    async fn reconcile_bumps_audit_epoch_only() {
        let store = open_store(DeleteMode::SoftDelete);
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
        let (_, ev) = store
            .commit_reconcile(ReconcileCommit {
                key: key.clone(),
                new_value: obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("edit")),
                ]),
                old_value: Some(obj(&[("email", PropValue::from("u@h"))])),
                ts_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(ev.kind, EventKind::Reconciled);
        assert_eq!(ev.audit_epoch, AuditEpoch(1));
        assert_eq!(ev.actor.as_str(), "daemon:reconciliation");
        assert!(ev.fields_changed.contains(&"display".to_string()));
        // A subsequent set must NOT bump audit_epoch.
        let (rec, ev2) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[("display", PropValue::from("renamed"))]),
                None,
                Some(Version(2)),
            ))
            .await
            .unwrap();
        assert_eq!(ev2.audit_epoch, AuditEpoch(1));
        assert_eq!(rec.version, Version(3));
        assert_eq!(store.audit_epoch(&ns()).await.unwrap(), AuditEpoch(1));
    }

    #[tokio::test]
    async fn events_reject_malformed_persisted_actor() {
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "alice@example.org");
        store
            .commit_set(set_commit(
                key,
                obj(&[("v", PropValue::from(1i64))]),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            store.events_since(&ns(), Nseq::zero()).await.unwrap().len(),
            1
        );
        store
            .inner
            .lock()
            .unwrap()
            .conn
            .execute(
                "UPDATE __props_events SET actor = ?1",
                params!["runtime:broken"],
            )
            .unwrap();
        let error = store.events_since(&ns(), Nseq::zero()).await.unwrap_err();
        assert!(matches!(error, StoreError::Storage { .. }));
        assert!(error.to_string().contains("actor decode"));
    }

    #[tokio::test]
    async fn events_since_strictly_greater() {
        let store = open_store(DeleteMode::SoftDelete);
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
        assert_eq!(all[2].nseq, Nseq(3));
        let after_first = store.events_since(&ns(), Nseq(1)).await.unwrap();
        assert_eq!(after_first.len(), 2);
        let none = store.events_since(&ns(), Nseq(99)).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn events_round_trip_field_path_and_metadata() {
        let store = open_store(DeleteMode::SoftDelete);
        let key = RecordKey::collection(ns(), "user@example.com");
        store
            .commit_set(SetCommit {
                cause: Some("req-42".into()),
                ..set_commit(
                    key.clone(),
                    obj(&[("email", PropValue::from("user@example.com"))]),
                    Some(Lifecycle::Provisioning),
                    Some(Version::zero()),
                )
            })
            .await
            .unwrap();
        let events = store.events_since(&ns(), Nseq::zero()).await.unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.kind, EventKind::Created);
        assert_eq!(ev.verb, "maild.props.set");
        assert_eq!(ev.key.key, "user@example.com");
        assert!(ev.key.field_path.is_empty());
        assert_eq!(ev.lifecycle, Some(Lifecycle::Provisioning));
        assert_eq!(ev.actor.as_str(), "maild");
        assert_eq!(ev.cause.as_deref(), Some("req-42"));
        assert!(ev.fields_changed.contains(&"email".to_string()));
    }

    #[tokio::test]
    async fn list_skips_tombstones_and_carries_observed_nseq() {
        let store = open_store(DeleteMode::SoftDelete);
        let k1 = RecordKey::collection(ns(), "a@h");
        let k2 = RecordKey::collection(ns(), "b@h");
        store
            .commit_set(set_commit(
                k1.clone(),
                obj(&[("email", PropValue::from("a@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        store
            .commit_set(set_commit(
                k2.clone(),
                obj(&[("email", PropValue::from("b@h"))]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        store
            .commit_delete(DeleteCommit {
                key: k1,
                expected_version: Some(Version(1)),
                actor: Actor::service("maild").expect("valid actor"),
                cause: None,
                ts_ms: 0,
            })
            .await
            .unwrap();
        let snap = store.list(&ns()).await.unwrap();
        assert_eq!(snap.value.len(), 1);
        assert_eq!(snap.value[0].key.key, "b@h");
        assert_eq!(snap.observed_nseq, Nseq(3));
    }

    #[tokio::test]
    async fn unregistered_namespace_get_is_storage_error() {
        let store = open_store(DeleteMode::SoftDelete);
        let other = RecordKey::collection(NamespaceName::new("other").unwrap(), "x");
        let err = store.get(&other).await.unwrap_err();
        assert!(matches!(err, StoreError::Storage { .. }));
    }

    #[tokio::test]
    async fn double_register_same_delete_mode_is_idempotent() {
        let store = open_store(DeleteMode::SoftDelete);
        // Re-register with the same delete mode — idempotent.
        let mapping = Arc::new(JsonValuesMapping::new(ns()));
        store
            .register_namespace(&spec(DeleteMode::SoftDelete), mapping)
            .unwrap();
    }

    #[tokio::test]
    async fn double_register_different_delete_mode_refused() {
        let store = open_store(DeleteMode::SoftDelete);
        let mapping = Arc::new(JsonValuesMapping::new(ns()));
        let err = store
            .register_namespace(&spec(DeleteMode::HardDelete), mapping)
            .unwrap_err();
        assert!(matches!(err, StoreError::Validation { .. }));
    }

    // -------- Custom mapping test --------

    /// Custom mapping that stores values in a daemon-style business
    /// table with explicit columns, proving the bridge accepts a
    /// non-default shape. Schema: `users(email TEXT PK, display TEXT)`.
    struct UsersTableMapping;
    impl SqliteTableMapping for UsersTableMapping {
        fn bootstrap(&self, conn: &Connection) -> rusqlite::Result<()> {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS users (\
                    email TEXT PRIMARY KEY, \
                    display TEXT\
                 );",
            )
        }
        fn read(&self, conn: &Connection, key: &str) -> rusqlite::Result<Option<PropValue>> {
            let row: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT email, display FROM users WHERE email = ?1",
                    params![key],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .optional()?;
            Ok(row.map(|(email, display)| {
                let mut m = BTreeMap::new();
                m.insert("email".into(), PropValue::from(email));
                if let Some(d) = display {
                    m.insert("display".into(), PropValue::from(d));
                }
                PropValue::Object(m)
            }))
        }
        fn list(&self, conn: &Connection) -> rusqlite::Result<Vec<(String, PropValue)>> {
            let mut stmt = conn.prepare("SELECT email, display FROM users ORDER BY email")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (email, display) = row?;
                let mut m = BTreeMap::new();
                m.insert("email".into(), PropValue::from(email.clone()));
                if let Some(d) = display {
                    m.insert("display".into(), PropValue::from(d));
                }
                out.push((email, PropValue::Object(m)));
            }
            Ok(out)
        }
        fn upsert(&self, conn: &Connection, key: &str, value: &PropValue) -> rusqlite::Result<()> {
            let obj = value.as_object().ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    "users mapping requires object value",
                )))
            })?;
            let display: Option<String> = obj.get("display").and_then(|v| match v {
                PropValue::String(s) => Some(s.clone()),
                _ => None,
            });
            conn.execute(
                "INSERT INTO users (email, display) VALUES (?1, ?2) \
                 ON CONFLICT(email) DO UPDATE SET display = excluded.display",
                params![key, display],
            )?;
            Ok(())
        }
        fn delete(&self, conn: &Connection, key: &str) -> rusqlite::Result<()> {
            conn.execute("DELETE FROM users WHERE email = ?1", params![key])?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn custom_mapping_round_trips_via_business_table() {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteStore::new("maild", conn).unwrap();
        let mut s = spec(DeleteMode::SoftDelete);
        s.storage = StorageBackendKind::SqliteTable {
            table: "users".into(),
        };
        store
            .register_namespace(&s, Arc::new(UsersTableMapping))
            .unwrap();
        let key = RecordKey::collection(ns(), "u@h");
        let (rec, _) = store
            .commit_set(set_commit(
                key.clone(),
                obj(&[
                    ("email", PropValue::from("u@h")),
                    ("display", PropValue::from("User")),
                ]),
                None,
                Some(Version::zero()),
            ))
            .await
            .unwrap();
        // Round-trip through the business table preserves the keys
        // the mapping cares about.
        let m = rec.value.as_object().unwrap();
        assert_eq!(m["email"], PropValue::from("u@h"));
        assert_eq!(m["display"], PropValue::from("User"));
        // List sees the row.
        let snap = store.list(&ns()).await.unwrap();
        assert_eq!(snap.value.len(), 1);
        // The business table really is the storage — direct SQL works.
        let count: i64 = {
            let inner = store.inner.lock().unwrap();
            inner
                .conn
                .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sets_serialise_via_immediate_tx() {
        let conn = Connection::open_in_memory().unwrap();
        let store = Arc::new(SqliteStore::new("maild", conn).unwrap());
        let mapping = Arc::new(JsonValuesMapping::new(ns()));
        store
            .register_namespace(&spec(DeleteMode::SoftDelete), mapping)
            .unwrap();
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
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.nseq, Nseq((i as u64) + 1));
        }
    }
}
