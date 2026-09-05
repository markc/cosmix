//! SPEC-12 property substrate for webd: the `listeners` namespace —
//! the runtime kill-switch + guard surface for the per-interface
//! listener set (P3).
//!
//! Each row is one `cosmix_daemon::listen` listener, keyed by its
//! stable `id`. The namespace is the **system of record** for whether
//! a listener is bound (`enabled` — the kill switch) and how it is
//! guarded (rate limit / conn caps / strict-SNI / CIDR ACL). An
//! operator flips a field via `webd.props.set namespace=listeners`
//! (or the ergonomic `webd.listener.{enable,disable}` verbs); a
//! runtime reaction loop ([`crate::listeners_reaction`]) applies the
//! change to the live [`ListenerSetControl`] and writes the observed
//! state back into the daemon-owned columns.
//!
//! ## Owner column split
//!
//! * **caller** — `enabled`, `rate_limit_per_ip`, `max_conns_global`,
//!   `max_conns_per_ip`, `strict_sni`, `allow_cidrs`, `deny_cidrs`.
//!   Writable through `props.set` by an *operator* (see [`auth_policy`]).
//! * **daemon** — `external`, `bound`, `bound_addr`, `active_conns`,
//!   `last_transition`, `last_error` (secret). Written only via
//!   `Runtime::set_with_origin(.., WriteOrigin::backend())` — the
//!   bootstrap seed and the reaction-loop writeback.
//!
//! ## Deliberate AuthPolicy divergence
//!
//! Unlike the permissive every-WG-peer posture of `vhosts`/`accounts`,
//! `props.write:webd.listeners` (the cap that flips `enabled` — a DoS
//! primitive) is granted **only to an operator allowlist** seeded from
//! the L0 `[webd.listeners] operators=[...]` config. read/describe stay
//! open to every WG peer (status is benign). See
//! `_decisions/2026-06-05-cosmix-listener-control.md`.
//!
//! ## Lockout guard
//!
//! [`ListenersNamespaceHooks::before_set`] refuses `enabled=false` on a
//! non-`external` listener — you must not be able to kill the WG/mesh
//! control-path listener and lock yourself off the very channel you'd
//! use to turn it back on. `external` is daemon-owned (seeded at
//! bootstrap from config), so a caller can't flip it to bypass the rule.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use cosmix_daemon::listen::Cidr;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::{
    AuthPolicy, Capability, CapabilitySet, Cardinality, FieldSchema, FieldType, HookCtx, HookError,
    HookFuture, HookHandler, Hooks, MergeMode, NamespaceName, NamespaceSpec, PeerIdentity,
    PropValue, PropertySchema, PropsRouter, Runtime, StorageBackendKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const NAMESPACE: &str = "listeners";

/// Daemon-owned columns — a caller-origin `props.set` naming any of
/// these is rejected by [`ListenersNamespaceHooks::before_set`].
/// Backend-origin writes (`WriteOrigin::backend()`) — the bootstrap
/// seed and the reaction-loop writeback — bypass the check.
const DAEMON_OWNED_FIELDS: &[&str] = &[
    "external",
    "bound",
    "bound_addr",
    "active_conns",
    "last_transition",
    "last_error",
];

/// Wire prefix the operator CLI parses against the validation message
/// (same affordance as vhosts' `OPERATOR_WROTE_DAEMON_FIELD_PREFIX`).
const OPERATOR_WROTE_DAEMON_FIELD_PREFIX: &str = "operator_wrote_daemon_field";

/// Bounded mpsc capacity for the reaction-loop event channel. A node
/// has a handful of listeners; the only burst is the bootstrap seed
/// (one event per listener) plus rare operator toggles. 64 is ample
/// headroom while staying small enough that sustained backpressure
/// surfaces as drop telemetry. Drop recovery is the reaction loop's
/// startup reconcile + drive-off-current-row contract (the namespace
/// is the durable source of truth, not the channel).
pub const LISTENER_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Events the `webd.listeners` lifecycle hooks emit for the reaction
/// loop ([`crate::listeners_reaction`]) to consume.
///
/// Each variant carries only the listener `id` — the reaction loop
/// re-reads the *current* row and reconciles off that (so a dropped or
/// duplicated event is self-healing), making the row payload
/// unnecessary.
#[derive(Debug, Clone)]
pub enum ListenerEvent {
    /// A listener row was created or updated.
    Set { id: String },
    /// A listener row was deleted (tombstone committed).
    Removed { id: String },
}

/// Rust projection of one `webd.listeners` row. `deny_unknown_fields`
/// makes a schema bump that adds a column without updating this type
/// fail loudly rather than silently drop it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListenerRow {
    pub id: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    // ── caller-owned guard fields ──────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_ip: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_conns_global: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_conns_per_ip: Option<i64>,
    #[serde(default)]
    pub strict_sni: bool,
    #[serde(default)]
    pub allow_cidrs: Vec<String>,
    #[serde(default)]
    pub deny_cidrs: Vec<String>,
    // ── daemon-owned ───────────────────────────────────────────────
    /// Seeded at bootstrap from the resolved listener's `external`
    /// flag. Drives the lockout guard (an internal listener can't be
    /// killed via the kill switch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_conns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

fn default_enabled() -> bool {
    true
}

/// Property schema — typed columns + secret/owner annotations.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "id".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Listener id — also the substrate row's primary key.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "The kill switch. Default true. enabled=false drops the listener's \
                   socket (kernel frees the port). Refused on a non-external listener."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "rate_limit_per_ip".into(),
            ty: FieldType::I64,
            default: None,
            secret: false,
            help: "Max new connections per source IP per second (token bucket). \
                   Absent = unlimited."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "max_conns_global".into(),
            ty: FieldType::I64,
            default: None,
            secret: false,
            help: "Listener-wide concurrent-connection ceiling. Absent = unlimited.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "max_conns_per_ip".into(),
            ty: FieldType::I64,
            default: None,
            secret: false,
            help: "Per-source-IP concurrent-connection ceiling. Absent = unlimited.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "strict_sni".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Reject no-SNI / unknown-SNI handshakes. Only affects Terminate \
                   (TLS) listeners; a no-op on plain listeners."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "allow_cidrs".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Peer-IP allow list (CIDR strings). Empty = allow all. deny wins.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "deny_cidrs".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Peer-IP deny list (CIDR strings). Checked before allow.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "external".into(),
            ty: FieldType::Bool,
            default: None,
            secret: false,
            help: "Daemon-owned. Whether this is a public/untrusted-facing listener \
                   (seeded from config). An internal listener can't be killed."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "bound".into(),
            ty: FieldType::Bool,
            default: None,
            secret: false,
            help: "Daemon-owned. Whether the listener is currently bound + accepting.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "bound_addr".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Daemon-owned. The kernel-bound address(es), when bound.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "active_conns".into(),
            ty: FieldType::I64,
            default: None,
            secret: false,
            help: "Daemon-owned. Live admitted-and-not-yet-dropped connection count.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "last_transition".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Daemon-owned. RFC 3339 timestamp of the last enable/disable applied.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "last_error".into(),
            ty: FieldType::String,
            default: None,
            secret: true,
            help: "Daemon-owned. Last enable/bind failure message. Secret — can carry \
                   bind-address / interface detail."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// Capability surface. **Operator-tier divergence:** read/describe/audit
/// → every WG peer; `props.write:webd.listeners` and the `:secrets`
/// read → only a peer whose `service_name` (the Bus sender, `cmd.from`)
/// is in `operators`. Empty `operators` ⇒ no remote write (daemon
/// backend-origin writes are unaffected — they don't flow through
/// `AuthPolicy`). Caps are named stably; the gate tightens when the
/// cross-mesh principal model lands.
pub fn auth_policy(service: &str, operators: Vec<String>) -> AuthPolicy {
    let read_public =
        Capability::new(format!("props.read:{service}.listeners")).expect("non-empty capability");
    let read_secrets = Capability::new(format!("props.read:{service}.listeners:secrets"))
        .expect("non-empty capability");
    let describe_public = Capability::new(format!("props.describe:{service}.listeners:public"))
        .expect("non-empty capability");
    let describe_full = Capability::new(format!("props.describe:{service}.listeners:full"))
        .expect("non-empty capability");
    let audit =
        Capability::new(format!("props.audit:{service}.listeners")).expect("non-empty capability");
    let write =
        Capability::new(format!("props.write:{service}.listeners")).expect("non-empty capability");
    AuthPolicy::new(move |peer: &PeerIdentity| {
        let mut caps = vec![
            read_public.clone(),
            describe_public.clone(),
            describe_full.clone(),
            audit.clone(),
        ];
        let is_operator = peer
            .service_name
            .as_deref()
            .is_some_and(|s| operators.iter().any(|o| o == s));
        if is_operator {
            caps.push(write.clone());
            caps.push(read_secrets.clone());
        }
        caps.into_iter().collect::<CapabilitySet>()
    })
}

/// Build the `NamespaceSpec` — collection cardinality keyed on `id`,
/// SqliteTable storage, `require_version = true` (OCC for the
/// caller-flip vs daemon-writeback race; SPEC 12 §4.4).
pub fn spec(service: &str, hooks: Hooks, operators: Vec<String>) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "id".to_string(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.auth = auth_policy(service, operators);
    s.hooks = hooks;
    s.require_version = true;
    s
}

/// Canonical [`NamespaceName`] for `webd.listeners`.
pub(crate) fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Hook handler — emits [`ListenerEvent`]s to the reaction loop.
pub struct ListenersNamespaceHooks {
    events_tx: mpsc::Sender<ListenerEvent>,
}

impl ListenersNamespaceHooks {
    pub fn new(events_tx: mpsc::Sender<ListenerEvent>) -> Self {
        Self { events_tx }
    }
}

/// Materialise the effective row a `before_set` validates against —
/// Patch shallow-merges the body onto `ctx.old`; Replace is the body.
fn effective_row(ctx: &HookCtx) -> Option<BTreeMap<String, PropValue>> {
    let new_obj = match &ctx.new {
        Some(PropValue::Object(m)) => m,
        _ => return None,
    };
    match (&ctx.merge, &ctx.old) {
        (Some(MergeMode::Patch), Some(PropValue::Object(old_obj))) => {
            let mut merged = old_obj.clone();
            for (k, v) in new_obj {
                merged.insert(k.clone(), v.clone());
            }
            Some(merged)
        }
        _ => Some(new_obj.clone()),
    }
}

fn field_as_str<'a>(obj: &'a BTreeMap<String, PropValue>, key: &str) -> Option<&'a str> {
    match obj.get(key) {
        Some(PropValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn field_as_bool(obj: &BTreeMap<String, PropValue>, key: &str) -> Option<bool> {
    match obj.get(key) {
        Some(PropValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Validate every CIDR string in a list field, returning the offending
/// entry on the first parse error.
fn check_cidr_list(obj: &BTreeMap<String, PropValue>, key: &str) -> Result<(), String> {
    if let Some(PropValue::List(items)) = obj.get(key) {
        for item in items {
            if let PropValue::String(s) = item
                && let Err(e) = Cidr::parse(s)
            {
                return Err(format!("{key} entry {s:?}: {e}"));
            }
        }
    }
    Ok(())
}

impl HookHandler for ListenersNamespaceHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let new_obj = match &ctx.new {
                Some(PropValue::Object(m)) => m,
                Some(other) => {
                    return Err(HookError::validation(format!(
                        "webd.listeners row must be an Object, got {}",
                        other.type_name(),
                    )));
                }
                None => return Ok(()),
            };

            // Rule 0 — callers must Patch (merge onto the daemon-seeded
            // row). A caller Replace (or non-merge write) could DROP
            // daemon-owned columns (`external`/`bound`/...) simply by
            // omitting them — a gap the present-field-only daemon-field
            // gate (rule 1) can't catch. Daemon-origin writes
            // (bootstrap, the reaction loop) are exempt; they own those
            // columns.
            if ctx.origin.is_caller() && ctx.merge != Some(MergeMode::Patch) {
                return Err(HookError::validation(
                    "webd.listeners caller writes must use merge=patch — a replace \
                     would drop daemon-owned columns (external/bound/last_error/...); \
                     patch only the field(s) you intend to change",
                ));
            }

            // Rule 1 — daemon-owned columns are caller-immutable.
            if ctx.origin.is_caller() {
                for &field in DAEMON_OWNED_FIELDS {
                    if new_obj.contains_key(field) {
                        return Err(HookError::validation(format!(
                            "{OPERATOR_WROTE_DAEMON_FIELD_PREFIX}: \
                             {field} is owned by webd and cannot be set through \
                             props.set; the daemon writes it via the backend path \
                             (bootstrap seed + the listener reaction loop)",
                        )));
                    }
                }
            }

            // Rule 2 — `id` body field, if present, must match the key.
            if let Some(id) = field_as_str(new_obj, "id")
                && id != ctx.key.key.as_str()
            {
                return Err(HookError::validation(format!(
                    "id body field {id:?} does not match record key {:?}",
                    ctx.key.key,
                )));
            }

            // Rule 3 — CIDR strings must parse (caught at write time,
            // not silently dropped at guard-build time).
            check_cidr_list(new_obj, "allow_cidrs").map_err(HookError::validation)?;
            check_cidr_list(new_obj, "deny_cidrs").map_err(HookError::validation)?;

            // Rule 4 — LOCKOUT GUARD. Evaluate the merged effective row
            // so a Patch that flips only `enabled` is judged against the
            // listener's (daemon-owned, caller-immutable) `external`
            // flag. Refuse to disable a non-external listener — that's
            // the mesh control path; killing it locks you out.
            let effective = effective_row(ctx)
                .expect("ctx.new was validated as Object above; effective_row must produce Some");
            let enabled = field_as_bool(&effective, "enabled").unwrap_or(true); // schema default
            let external = field_as_bool(&effective, "external").unwrap_or(false);
            if !enabled && !external {
                return Err(HookError::validation(
                    "cannot disable a non-external listener — it is (or includes) the \
                     WG/mesh control path, and the kill switch must not be able to \
                     lock you off the channel you'd use to re-enable it. The kill \
                     switch governs external (public-facing) listeners only.",
                ));
            }

            // Rule 5 — row-shape gate: the merged effective row MUST
            // deserialise to the typed `ListenerRow` (catches an
            // operator typo via `deny_unknown_fields`, and guards the
            // after_set producer against an orphaned malformed row).
            let effective_value = PropValue::Object(effective);
            if let Err(e) = listener_row_from_value(&effective_value) {
                return Err(HookError::validation(format!(
                    "row shape rejected before commit: {e}. See \
                     listeners_namespace.rs::ListenerRow for the field set + \
                     deny_unknown_fields rule",
                )));
            }

            Ok(())
        })
    }

    fn after_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Emit ONLY for caller-origin writes (an operator flip via
            // a verb or direct props.set). Backend-origin writes are
            // the daemon's own bookkeeping — the bootstrap seed (the
            // reaction loop's startup pass reconciles those) and the
            // reaction loop's own status writeback. Emitting on the
            // latter would create an infinite reconcile→writeback→event
            // loop.
            if ctx.origin.is_caller() {
                emit(
                    &self.events_tx,
                    ListenerEvent::Set {
                        id: ctx.key.key.to_string(),
                    },
                    &ctx.key.key,
                );
            }
            Ok(())
        })
    }

    fn before_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Refuse caller-origin deletes. A `webd.listeners` row is
            // config-defined + daemon-owned; deleting it erases the L1
            // kill-switch/guard state, and bootstrap would reseed
            // `enabled=true` from config on the next restart — silently
            // resurrecting a listener an operator had killed. To remove
            // a listener, drop its `[[webd.listener]]` block and
            // restart. Backend deletes (none today; reserved for a
            // future orphan sweep) are allowed.
            if ctx.origin.is_caller() {
                return Err(HookError::validation(
                    "webd.listeners rows are config-defined and daemon-owned — they \
                     cannot be deleted via props.delete (that would erase the kill \
                     switch / guard state and let bootstrap reseed enabled=true on \
                     restart). Remove the [[webd.listener]] block and restart instead.",
                ));
            }
            Ok(())
        })
    }

    fn after_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Caller deletes are rejected in before_delete; only a
            // backend delete reaches here. Emit so a future
            // orphan-sweep is observable; gate on origin for symmetry.
            if ctx.origin.is_caller() {
                emit(
                    &self.events_tx,
                    ListenerEvent::Removed {
                        id: ctx.key.key.to_string(),
                    },
                    &ctx.key.key,
                );
            }
            Ok(())
        })
    }
}

/// `try_send` an event, never blocking the commit thread. On
/// Full/Closed log + drop — the namespace is the durable source of
/// truth and the reaction loop reconciles off current row state.
fn emit(tx: &mpsc::Sender<ListenerEvent>, event: ListenerEvent, id: &str) {
    match tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(
                target: "webd::listeners_namespace",
                id = %id,
                capacity = LISTENER_EVENT_CHANNEL_CAPACITY,
                "listener event channel full — dropped; reaction loop reconciles off \
                 current namespace state",
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::error!(
                target: "webd::listeners_namespace",
                id = %id,
                "listener event channel closed — reaction loop receiver dropped; \
                 namespace state is canonical but won't be applied until restart \
                 (startup-wiring bug)",
            );
        }
    }
}

/// Wire the `listeners` namespace into the router + store and build the
/// reaction event channel. The returned `Receiver` is moved into the
/// reaction loop ([`crate::listeners_reaction`]).
pub fn register_listeners_namespace(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    operators: Vec<String>,
) -> Result<(Arc<Runtime>, mpsc::Receiver<ListenerEvent>)> {
    let service = router.service().to_string();
    let (events_tx, events_rx) = mpsc::channel(LISTENER_EVENT_CHANNEL_CAPACITY);
    let hooks = Hooks::new(ListenersNamespaceHooks::new(events_tx));
    let spec = spec(&service, hooks, operators);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register listeners namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register listeners runtime on router: {e}"))?;
    Ok((runtime, events_rx))
}

/// Construct a `ListenerRow` from a substrate `PropValue::Object`.
pub fn listener_row_from_value(v: &PropValue) -> serde_json::Result<ListenerRow> {
    let json = serde_json::to_value(v)?;
    serde_json::from_value(json)
}

/// Snapshot every committed row as a typed `Vec<ListenerRow>`. Used at
/// startup to seed each `ListenerSpec`'s `enabled` + guard from L1
/// (the L0-seed/L1-authoritative keystone) and by the reaction loop.
/// Fail-fast on deserialisation (a malformed committed row is a
/// substrate-invariant violation — `before_set` rule 5 rejects bad
/// shapes on write).
pub async fn snapshot_rows(runtime: &Runtime) -> Result<Vec<ListenerRow>> {
    let snap = runtime
        .store()
        .list(&namespace_name())
        .await
        .map_err(|e| anyhow::anyhow!("webd.listeners list failed: {e}"))?;
    let mut rows: Vec<ListenerRow> = Vec::with_capacity(snap.value.len());
    for record in snap.value {
        let row = listener_row_from_value(&record.value).map_err(|e| {
            anyhow::anyhow!(
                "webd.listeners row {key:?} failed ListenerRow deserialisation: {e}",
                key = record.key.key,
            )
        })?;
        if record.key.key.as_str() != row.id {
            return Err(anyhow::anyhow!(
                "webd.listeners row key/body mismatch — key={:?} but id={:?}",
                record.key.key,
                row.id,
            ));
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::{Actor, RecordKey, Version, WriteOrigin};
    use rusqlite::Connection;

    fn open_test_store() -> Arc<SqliteStore> {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        Arc::new(SqliteStore::new("webd", conn).expect("store"))
    }

    fn hooks() -> (ListenersNamespaceHooks, mpsc::Receiver<ListenerEvent>) {
        let (tx, rx) = mpsc::channel(LISTENER_EVENT_CHANNEL_CAPACITY);
        (ListenersNamespaceHooks::new(tx), rx)
    }

    fn obj(pairs: &[(&str, PropValue)]) -> BTreeMap<String, PropValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn caller_replace(id: &str, body: BTreeMap<String, PropValue>) -> HookCtx {
        HookCtx {
            key: RecordKey::collection(namespace_name(), id.to_string()),
            old: None,
            new: Some(PropValue::Object(body)),
            version: Version::zero(),
            actor: Actor::operator("test-operator").expect("valid actor"),
            merge: Some(MergeMode::Replace),
            origin: WriteOrigin::caller(),
        }
    }

    fn caller_patch(
        id: &str,
        old: BTreeMap<String, PropValue>,
        new: BTreeMap<String, PropValue>,
    ) -> HookCtx {
        HookCtx {
            key: RecordKey::collection(namespace_name(), id.to_string()),
            old: Some(PropValue::Object(old)),
            new: Some(PropValue::Object(new)),
            version: Version::zero(),
            actor: Actor::operator("test-operator").expect("valid actor"),
            merge: Some(MergeMode::Patch),
            origin: WriteOrigin::caller(),
        }
    }

    #[test]
    fn register_clean() {
        let store = open_test_store();
        let mut router = PropsRouter::new("webd");
        let (runtime, _rx) =
            register_listeners_namespace(&mut router, &store, vec![]).expect("clean register");
        assert_eq!(runtime.spec().name.as_str(), NAMESPACE);
    }

    #[test]
    fn auth_policy_gates_write_on_operator_allowlist() {
        let policy = auth_policy("webd", vec!["op-node".to_string()]);
        let write = Capability::new("props.write:webd.listeners".to_string())
            .expect("non-empty capability");
        let read =
            Capability::new("props.read:webd.listeners".to_string()).expect("non-empty capability");

        // Operator sender → write + read.
        let op = PeerIdentity {
            service_name: Some("op-node".to_string()),
            ..Default::default()
        };
        let caps = policy.resolve(&op);
        assert!(caps.contains(&write), "operator must get write");
        assert!(caps.contains(&read), "operator must get read");

        // Non-operator sender → read only, NO write.
        let other = PeerIdentity {
            service_name: Some("random-node".to_string()),
            ..Default::default()
        };
        let caps = policy.resolve(&other);
        assert!(caps.contains(&read), "every peer gets read");
        assert!(!caps.contains(&write), "non-operator must NOT get write");

        // Empty allowlist → nobody writes.
        let none = auth_policy("webd", vec![]);
        assert!(
            !none.resolve(&op).contains(&write),
            "empty allowlist denies all writes"
        );
    }

    fn seeded(id: &str, external: bool) -> BTreeMap<String, PropValue> {
        obj(&[
            ("id", PropValue::String(id.into())),
            ("enabled", PropValue::Bool(true)),
            ("external", PropValue::Bool(external)),
        ])
    }

    #[tokio::test]
    async fn before_set_rejects_caller_replace() {
        // Rule 0: a caller Replace could drop daemon-owned columns by
        // omission — must be refused; callers patch.
        let (h, _rx) = hooks();
        let ctx = caller_replace(
            "public",
            obj(&[
                ("id", PropValue::String("public".into())),
                ("enabled", PropValue::Bool(true)),
            ]),
        );
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("caller replace must be rejected");
        assert!(
            format!("{err:?}").contains("merge=patch"),
            "expected merge=patch rejection, got {err:?}",
        );
    }

    #[tokio::test]
    async fn before_set_rejects_caller_write_to_daemon_field() {
        let (h, _rx) = hooks();
        let ctx = caller_patch(
            "public",
            seeded("public", true),
            obj(&[("bound", PropValue::Bool(true))]), // daemon-owned
        );
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("daemon field must be rejected");
        assert!(
            format!("{err:?}").contains(OPERATOR_WROTE_DAEMON_FIELD_PREFIX),
            "expected daemon-field rejection, got {err:?}",
        );
    }

    #[tokio::test]
    async fn before_set_backend_origin_may_seed_external() {
        // The bootstrap seed (backend origin) sets the daemon-owned
        // `external` flag — must pass.
        let (h, _rx) = hooks();
        let mut ctx = caller_replace(
            "public",
            obj(&[
                ("id", PropValue::String("public".into())),
                ("enabled", PropValue::Bool(true)),
                ("external", PropValue::Bool(true)),
            ]),
        );
        ctx.origin = WriteOrigin::backend();
        h.before_set(&ctx)
            .await
            .expect("backend seed of external is allowed");
    }

    #[tokio::test]
    async fn before_set_lockout_refuses_disabling_internal_listener() {
        // Internal (external=false) listener, operator patches
        // enabled=false → REJECTED (can't kill the mesh control path).
        let (h, _rx) = hooks();
        let old = obj(&[
            ("id", PropValue::String("wg".into())),
            ("enabled", PropValue::Bool(true)),
            ("external", PropValue::Bool(false)),
        ]);
        let ctx = caller_patch("wg", old, obj(&[("enabled", PropValue::Bool(false))]));
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("disabling internal listener must be refused");
        assert!(
            format!("{err:?}").contains("non-external"),
            "expected lockout message, got {err:?}",
        );
    }

    #[tokio::test]
    async fn before_set_lockout_allows_disabling_external_listener() {
        // External listener can be killed.
        let (h, _rx) = hooks();
        let old = obj(&[
            ("id", PropValue::String("public".into())),
            ("enabled", PropValue::Bool(true)),
            ("external", PropValue::Bool(true)),
        ]);
        let ctx = caller_patch("public", old, obj(&[("enabled", PropValue::Bool(false))]));
        h.before_set(&ctx)
            .await
            .expect("disabling an external listener is allowed");
    }

    #[tokio::test]
    async fn before_set_rejects_unparseable_cidr() {
        let (h, _rx) = hooks();
        let ctx = caller_patch(
            "public",
            seeded("public", true),
            obj(&[(
                "deny_cidrs",
                PropValue::List(vec![PropValue::String("not-a-cidr".into())]),
            )]),
        );
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("bad CIDR must be rejected");
        assert!(
            format!("{err:?}").contains("deny_cidrs"),
            "expected CIDR rejection, got {err:?}",
        );
    }

    #[tokio::test]
    async fn before_set_rejects_unknown_field() {
        let (h, _rx) = hooks();
        let ctx = caller_patch(
            "public",
            seeded("public", true),
            obj(&[("typo_field", PropValue::Bool(true))]),
        );
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("unknown field must be rejected");
        assert!(
            format!("{err:?}").contains("row shape"),
            "expected row-shape rejection, got {err:?}",
        );
    }

    #[test]
    fn listener_row_serde_round_trips() {
        let row = ListenerRow {
            id: "public".into(),
            enabled: false,
            rate_limit_per_ip: Some(20),
            max_conns_global: Some(1000),
            max_conns_per_ip: Some(50),
            strict_sni: true,
            allow_cidrs: vec!["192.0.2.0/24".into()],
            deny_cidrs: vec!["198.51.100.5/32".into()],
            external: Some(true),
            bound: Some(false),
            bound_addr: Some("203.0.113.5:443".into()),
            active_conns: Some(0),
            last_transition: Some("2026-06-05T00:00:00Z".into()),
            last_error: None,
        };
        let pv = serde_json::to_value(&row).unwrap();
        let back: ListenerRow = serde_json::from_value(pv).unwrap();
        assert_eq!(row, back);
    }
}
