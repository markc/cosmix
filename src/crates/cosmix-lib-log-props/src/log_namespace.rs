//! SPEC 12 property-substrate registration for the reserved `log`
//! namespace.
//!
//! Citizen-only (`cosmix` feature). The fully-qualified name surfaced
//! through `<svc>.props.*` is `<svc>.log`, where `<svc>` is the
//! service the host `PropsRouter` was constructed with.
//!
//! `Singleton { canonical_key: "current" }`. Schema mirrors the plan
//! `_doc/planned/cosmix-lib-log.md` §4 table:
//!
//! | Field | Type | Mutable | Notes |
//! |-------|------|---------|-------|
//! | `level` | enum<none,error,warn,info,debug> | yes | effective global default |
//! | `filter` | string | yes | EnvFilter syntax — `before_set` parses it |
//! | `format` | enum<human,json> | yes (substrate-level) | startup-frozen in practice; reads back for introspection |
//! | `applied_at` | string (RFC 3339 UTC) | yes | plan §4: written by watcher — **deferred to P3+** |
//! | `applied_by` | enum<cli,env,props,default> | yes | plan §4: origin — **deferred to P3+** |
//! | `source` | string | yes | plan §4: literal directive accepted — **deferred to P3+** |
//!
//! All six fields are substrate-mutable in v0.1; the plan §4 marks
//! the audit-trail trio (`applied_at`, `applied_by`, `source`) as
//! read-only / watcher-written but **P1 does not implement the
//! writeback loop**. Writing back from inside the watcher creates a
//! feedback-loop hazard (the watcher's own `props.set` re-wakes the
//! watcher) that v0.1 SPEC-12 lacks the primitives to suppress
//! safely (no per-write origin tag, no per-field immutability).
//! Until then the trio remains substrate-mutable and reads back the
//! last value any peer wrote — typically empty strings and
//! `applied_by="default"`. Tightening to writer-class enforcement
//! is a P3+ amendment.
//!
//! `before_set` validates:
//!   1. `level` is one of the five frozen enum variants.
//!   2. `format` is one of the two frozen enum variants.
//!   3. `applied_by` (when present) is one of the four frozen
//!      enum variants — `FieldType::Enum` is descriptive in v0.1
//!      and does not enforce variant membership at commit time.
//!   4. `filter` parses via `tracing_subscriber::EnvFilter::try_new`.
//!      An empty `filter` is accepted (means "no per-target overrides").
//!
//! `before_delete` rejects every delete: the singleton row is the
//! daemon's logging configuration and must persist across restarts.
//! Operators that want "back to defaults" issue a Replace with the
//! defaults body, matching the engine_config pattern.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::runtime::Runtime;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::value::PropValue;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

/// Unqualified namespace name (the substrate yields `<svc>.log` once
/// the router's service prefix is composed).
pub const NAMESPACE: &str = "log";

/// Singleton canonical key — wire requests carry `key: ""`, the
/// substrate fills in `"current"` on responses.
pub const CANONICAL_KEY: &str = "current";

/// `before_set` rejects malformed `EnvFilter` directives behind this
/// prefix so call-sites can match on it without scraping the full
/// message. Plan §4.1: "props.set with a malformed filter returns
/// `Err` from the verb; the live filter is unchanged."
pub const INVALID_FILTER_PREFIX: &str = "log_filter_invalid:";

/// `before_set` rejects unknown enum variants for the three enum-typed
/// fields. `FieldType::Enum` is descriptive in v0.1, not enforced at
/// commit time — the hook is the only enforcer.
pub const UNKNOWN_ENUM_VARIANT_PREFIX: &str = "log_unknown_enum_variant:";

/// `before_delete` rejects every delete on this singleton row.
pub const UNDELETABLE_PREFIX: &str = "log_undeletable:";

/// What `register_log_namespace` hands back. The host service holds it
/// for the lifetime of the process and passes it to [`attach_props`].
///
/// It bundles the substrate `Runtime` with a **dedicated** wake `Notify`
/// owned by this namespace. The watcher MUST NOT park on
/// `Runtime::events_signal()` — that notify is single-consumer
/// (`notify_one`) and already drained by the props dispatcher, so a
/// shared wait races the dispatcher and the live filter swap silently
/// stalls. Instead, the namespace's `after_set` hook (which fires only
/// for `<svc>.log` writes) pulses this private `Notify`, guaranteeing the
/// watcher is the one woken on every accepted log-row change.
#[derive(Clone)]
pub struct LogPropsRuntime {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) signal: Arc<Notify>,
}

impl LogPropsRuntime {
    /// The underlying substrate `Runtime` for the `<svc>.log` namespace.
    /// Daemons normally just pass the whole `LogPropsRuntime` to
    /// [`attach_props`]; this accessor is for callers that need the raw
    /// runtime (e.g. tests driving writes through the substrate path).
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }
}

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Frozen `level` variants. Mirrors `LogLevel` in `opts.rs` but lives
/// here as plain strings because the substrate schema speaks in
/// `FieldType::Enum { variants: Vec<String> }`.
pub fn level_variants() -> Vec<String> {
    vec![
        "none".into(),
        "error".into(),
        "warn".into(),
        "info".into(),
        "debug".into(),
    ]
}

/// Frozen `format` variants. Mirrors `LogFormat`.
pub fn format_variants() -> Vec<String> {
    vec!["human".into(), "json".into()]
}

/// Frozen `applied_by` variants.
pub fn applied_by_variants() -> Vec<String> {
    vec!["cli".into(), "env".into(), "props".into(), "default".into()]
}

/// SPEC 12 §4.3 schema. Defaults are conservative: `level=info`,
/// `filter=""`, `format=human`, audit-trail trio = `"default"` /
/// empty strings. The defaults exist so a Patch over a bootstrap row
/// that omits a field has a deterministic effective value rather
/// than failing on a half-shaped record.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "level".into(),
            ty: FieldType::Enum {
                variants: level_variants(),
            },
            default: Some(PropValue::String("info".into())),
            secret: false,
            help: "Effective global default level — one of none|error|warn|info|debug".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "filter".into(),
            ty: FieldType::String,
            default: Some(PropValue::String(String::new())),
            secret: false,
            help: "Per-target overrides in tracing-subscriber EnvFilter syntax (empty = none)"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "format".into(),
            ty: FieldType::Enum {
                variants: format_variants(),
            },
            default: Some(PropValue::String("human".into())),
            secret: false,
            help: "Output format — one of human|json. Changing mid-run does not re-init sinks."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "applied_at".into(),
            ty: FieldType::String,
            default: Some(PropValue::String(String::new())),
            secret: false,
            help: "RFC 3339 UTC timestamp of the last filter swap (empty before first swap)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "applied_by".into(),
            ty: FieldType::Enum {
                variants: applied_by_variants(),
            },
            default: Some(PropValue::String("default".into())),
            secret: false,
            help: "Origin of the currently applied filter — one of cli|env|props|default".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "source".into(),
            ty: FieldType::String,
            default: Some(PropValue::String(String::new())),
            secret: false,
            help: "Literal directive tracing-subscriber accepted (reflects what is running)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// Plan §4 five-cap surface. The capabilities are scoped to the
/// fully-qualified namespace `<svc>.log` and resolved by the host
/// service's `PropsRouter` from `peer` identity. v0.1 grants every
/// reachable peer (WireGuard `/24` trust domain) every cap, matching
/// the maild Phase 1/2/3 / webd vhosts+handlers posture; the unified
/// namespace-narrowing work narrows write/audit at a later phase
/// without changing the cap names.
///
/// SECURITY NOTE (Codex review, deferred not dismissed): `props.write`
/// on this namespace is a **log-blinding primitive** — a peer that can
/// set `level=none` silences the daemon's own incident logging
/// (anti-forensics). In the current v0.1 trust model that is no
/// higher-stakes than the already-uniformly-open writes to accounts /
/// engine_config / vhosts, so it is intentionally NOT gated one-off
/// here. When the substrate-wide write narrowing lands (per-namespace
/// operator/capability gating, and cross-mesh `cross_mesh_exposable`
/// registration in `_doc/planned/cosmix-cross-mesh-authz.md`),
/// `<svc>.log` write/audit MUST be covered and `<svc>.log` write MUST
/// NOT be cross-mesh-exposable by default.
pub fn auth_policy(service: &str) -> AuthPolicy {
    let read = Capability::new(format!("props.read:{service}.log")).expect("non-empty capability");
    let describe_public = Capability::new(format!("props.describe:{service}.log:public"))
        .expect("non-empty capability");
    let describe_full = Capability::new(format!("props.describe:{service}.log:full"))
        .expect("non-empty capability");
    let write =
        Capability::new(format!("props.write:{service}.log")).expect("non-empty capability");
    let audit =
        Capability::new(format!("props.audit:{service}.log")).expect("non-empty capability");
    let caps: CapabilitySet = [read, describe_public, describe_full, write, audit]
        .into_iter()
        .collect();
    AuthPolicy::new(move |_peer: &PeerIdentity| caps.clone())
}

pub fn spec(service: &str, hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Singleton {
            canonical_key: CANONICAL_KEY.into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.lifecycle = NamespaceLifecycle::Simple;
    s.auth = auth_policy(service);
    s.hooks = hooks;
    s
}

/// Hook handler — validates enum variants and EnvFilter syntax on
/// every set, rejects every delete, and (on a committed set) pulses the
/// namespace-private wake `Notify` so the live-filter watcher re-reads
/// and swaps deterministically.
pub struct LogNamespaceHooks {
    signal: Arc<Notify>,
}

impl LogNamespaceHooks {
    pub fn new(signal: Arc<Notify>) -> Self {
        Self { signal }
    }
}

impl HookHandler for LogNamespaceHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let Some(new_value) = ctx.new.as_ref() else {
                return Ok(());
            };
            let obj = match new_value.as_object() {
                Some(o) => o,
                None => {
                    return Err(HookError::validation(format!(
                        "log namespace body must be an object (got {})",
                        new_value.type_name()
                    )));
                }
            };
            validate_enum(obj, "level", &level_variants())?;
            validate_enum(obj, "format", &format_variants())?;
            validate_enum(obj, "applied_by", &applied_by_variants())?;
            validate_filter(obj)?;
            Ok(())
        })
    }

    fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        // The row is now durably committed. Pulse the namespace-private
        // wake so the watcher (parked on this same `Notify`) re-reads the
        // committed row and swaps the live filter. `notify_one` coalesces
        // — if the watcher is mid-read, the stored permit makes it loop
        // once more and converge on the latest row. Unlike the shared
        // `Runtime::events_signal()`, nothing else drains this notify, so
        // the watcher is always the woken party.
        self.signal.notify_one();
        Box::pin(async { Ok(()) })
    }

    fn before_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            Err(HookError::validation(format!(
                "{UNDELETABLE_PREFIX} log namespace is a singleton; reset via props.set"
            )))
        })
    }
}

fn validate_enum(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    variants: &[String],
) -> Result<(), HookError> {
    match obj.get(field) {
        None => Ok(()),
        Some(PropValue::String(s)) => {
            if variants.iter().any(|v| v == s) {
                Ok(())
            } else {
                Err(HookError::validation(format!(
                    "{UNKNOWN_ENUM_VARIANT_PREFIX} {field}={s:?} not in {variants:?}"
                )))
            }
        }
        Some(other) => Err(HookError::validation(format!(
            "{field} must be a string enum variant (got {})",
            other.type_name()
        ))),
    }
}

fn validate_filter(obj: &BTreeMap<String, PropValue>) -> Result<(), HookError> {
    match obj.get("filter") {
        None => Ok(()),
        Some(PropValue::String(s)) if s.is_empty() => Ok(()),
        Some(PropValue::String(s)) => EnvFilter::try_new(s)
            .map(|_| ())
            .map_err(|e| HookError::validation(format!("{INVALID_FILTER_PREFIX} {s:?}: {e}"))),
        Some(other) => Err(HookError::validation(format!(
            "filter must be a string (got {})",
            other.type_name()
        ))),
    }
}

/// Wire the reserved `log` namespace into the host service's
/// substrate. Citizen-only.
///
/// The router's service name (set at `PropsRouter::new("<svc>")`)
/// determines the fully-qualified namespace `<svc>.log` and the five
/// capability names. The returned `LogPropsRuntime` (= `Arc<Runtime>`)
/// is what `LogHandle::attach_props` subscribes to — the host
/// service must hold it for the lifetime of the process.
///
/// Idempotent against the same `(service, store)` pair via
/// `SqliteStore::register_namespace`'s idempotence guarantee; a second
/// call with a different `delete_mode` is rejected by the store, but
/// the spec built here is fixed at `DeleteMode::SoftDelete` (the
/// `NamespaceSpec` default) so collisions only matter against an
/// unrelated namespace claiming the name `log` first.
pub fn register_log_namespace(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
) -> Result<LogPropsRuntime> {
    let service = router.service().to_string();
    // Namespace-private wake shared between the `after_set` hook (pulses)
    // and the watcher (parks). See `LogPropsRuntime`'s doc for why this
    // must NOT be `Runtime::events_signal()`.
    let signal = Arc::new(Notify::new());
    let hooks = Hooks::new(LogNamespaceHooks::new(signal.clone()));
    let spec = spec(&service, hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register log namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register log runtime on router: {e}"))?;
    Ok(LogPropsRuntime { runtime, signal })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, RecordKey, Version};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::store::MergeMode;
    use rusqlite::Connection;
    use std::time::Duration;

    /// Regression guard for the live-swap BLOCKER: a committed
    /// `<svc>.log` write must pulse the namespace-PRIVATE `Notify` that
    /// the watcher parks on — NOT `Runtime::events_signal()`, which the
    /// props dispatcher already drains single-consumer. If `after_set`
    /// ever stops pulsing `LogPropsRuntime::signal`, the agent-operable
    /// live level swap silently stalls and this test fails on the
    /// timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn after_set_pulses_the_private_watcher_signal() {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("init store schema"));
        let mut router = PropsRouter::new("maild");
        let lp = register_log_namespace(&mut router, &store).expect("register log namespace");

        let value = {
            let mut m = BTreeMap::new();
            m.insert("level".to_string(), PropValue::String("debug".to_string()));
            PropValue::Object(m)
        };
        lp.runtime()
            .set(
                RecordKey::singleton(namespace_name()),
                value,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::operator("test").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("valid log row commits");

        // `notify_one` stored a permit during `after_set`, so this is
        // immediately ready. The timeout converts "watcher would hang"
        // into a hard test failure.
        tokio::time::timeout(Duration::from_secs(2), lp.signal.notified())
            .await
            .expect("after_set must have pulsed the private watcher signal");
    }
}
