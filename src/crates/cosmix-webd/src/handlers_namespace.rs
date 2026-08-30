//! SPEC-12 property substrate for webd: the `handlers` namespace.
//!
//! Slice #3 of the maild/webd trust-split ADR
//! (`_decisions/2026-06-04-maild-webd-trust-split.md` §6). This namespace is
//! the declarative source-of-truth for **embedded-Mix HTTP route
//! handlers**: an operator-authored `.mix` script under a vhost's
//! `www_dir` serves a `(vhost, method, path_pattern)` request, run
//! in-process with a least-privilege sandbox (see [`crate::mix_handler`]).
//!
//! ## Naming — why `handlers`, not `routes`
//!
//! webd already has an ergonomic verb `webd.routes.list` that means
//! something unrelated: a read-only snapshot of the *vhost map* (which
//! sites exist + their `has_cms`/`has_jmap` flags). This namespace is
//! reached on the wire as `webd.props.* namespace=handlers` (the
//! generic SPEC-12 surface), so there is no verb-string collision —
//! but `handlers` is the operator-legible name for "the request
//! handlers I configured" and avoids overloading "routes" against the
//! existing vhost-map verb. (Decision 2026-06-04, Mark.)
//!
//! ## Schema
//!
//! Cardinality `Collection` keyed by a caller-chosen `route_id` (an
//! opaque label such as `blog-index`). The dispatch tuple
//! `(vhost_fqdn, method, path_pattern)` is carried as separate fields;
//! the runtime [`crate::mix_handler::HandlerTable`] de-duplicates that
//! tuple at build time (warn + first-wins) — storage enforces only
//! `route_id` uniqueness, so a duplicate tuple is a config smell the
//! daemon surfaces, not a wire-level rejection.
//!
//! * `route_id: String` — primary key, equal to the wire record key.
//! * `vhost_fqdn: String` — the primary FQDN this handler serves
//!   (alias requests resolve to the same `VhostState`, so a handler on
//!   the primary FQDN also serves its aliases).
//! * `method: Enum(GET|POST|PUT|DELETE|ANY)` — `ANY` matches any method.
//! * `path_pattern: String` — must start with `/`; exact (`/blog`) or a
//!   single trailing glob (`/blog/*`). No other `*` placement.
//! * `handler_kind: Enum(mix)` — only `mix` is wired now; the enum is
//!   kept open for a future `static`/`native` kind.
//! * `handler_ref: String` — a `.mix` path **relative to the vhost's
//!   `www_dir`**, with no leading `/` and no `..` component (validated
//!   here AND re-checked at dispatch against the live `www_dir`).
//! * `enabled: Bool` — default `true`. Disabled rows are skipped at
//!   table-build time.
//!
//! ## AuthPolicy
//!
//! Phase-1 posture (matching `webd.vhosts`): every reachable WG `/24`
//! peer gets the issued read/write/describe/audit cap set on
//! `props.*:webd.handlers`. Cross-mesh authz narrows later without
//! renaming the caps.
//!
//! ## Reload signal
//!
//! `after_set`/`after_delete` fire a `tokio::sync::Notify` — the
//! lightest correct shape for "the handler table changed, rebuild it
//! from a fresh namespace snapshot." `notify_one` is non-blocking and
//! coalescing (multiple writes before the rebuild task wakes collapse
//! to one rebuild, which reads the latest snapshot anyway), so it never
//! blocks the substrate commit thread and never drops a needed reload.
//! The namespace is the durable source of truth; a missed wakeup is
//! impossible as long as the rebuild task loops back to `notified()`.

use std::sync::Arc;

use anyhow::Result;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::{
    Actor, AuthPolicy, Capability, CapabilitySet, Cardinality, FieldSchema, FieldType, HookCtx,
    HookError, HookFuture, HookHandler, Hooks, MergeMode, NamespaceName, NamespaceSpec,
    PeerIdentity, PropValue, PropertySchema, PropsRouter, RecordKey, Runtime, SetOpts,
    StorageBackendKind, WriteOrigin,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::Notify;

const NAMESPACE: &str = "handlers";

/// `method` enum variants. `ANY` is the match-all wildcard.
fn method_variants() -> Vec<String> {
    vec![
        "GET".into(),
        "POST".into(),
        "PUT".into(),
        "DELETE".into(),
        "ANY".into(),
    ]
}

/// `handler_kind` enum variants. Only `mix` is wired in slice #3; the
/// enum is kept open so a future `static`/`native` kind is an additive
/// schema change rather than a new namespace.
fn handler_kind_variants() -> Vec<String> {
    vec!["mix".into()]
}

/// Rust projection of one substrate row. `deny_unknown_fields` makes a
/// future schema bump that adds a column fail loudly here rather than
/// silently dropping it (same guard as `vhosts_namespace::VhostRow`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HandlerRow {
    pub route_id: String,
    pub vhost_fqdn: String,
    pub method: String,
    pub path_pattern: String,
    pub handler_kind: String,
    pub handler_ref: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Opt-in capability tokens granted to this handler beyond the
    /// read-only default: `"db"` (per-vhost SQLite via `db_query`/`db_exec`),
    /// `"jmap"` (the vhost maild upstream via `jmap()`), and `"public_read"`
    /// (webd-internal anonymous content credential — no sandbox widening).
    /// Validated against `KNOWN_CAPABILITIES` in `before_set`. Default empty.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

/// Capability tokens a route may opt into (`capabilities` field). `public_read`
/// is webd-internal (it does NOT widen the Mix sandbox): it lets an ANONYMOUS
/// request to this route back `jmap()` with the vhost's `public_read_*` content
/// credential — only meaningful on a vhost that sets `public_read_*`. `net`
/// grants `CapabilityClass::Network` (outbound http builtins) to THIS route's
/// handler only — audited handlers talking to external HTTP APIs (payment
/// gateways, registrar SOAP). `public_cache` (webd-internal, GET-only, no
/// sandbox change): declares this route's ANONYMOUS render is a pure function
/// of `(host, path, query)` — no cookie / `$CSRF` / auth variance — so webd may
/// cache + replay it to concurrent anonymous readers. webd renders a cacheable
/// hit cookie-stripped with `$CSRF=nil` and refuses to store any `Set-Cookie` /
/// non-200 response; grant ONLY to audited public content routes (never a route
/// that embeds a per-visitor token). Ignored on non-GET rows.
const KNOWN_CAPABILITIES: &[&str] = &["db", "jmap", "public_read", "public_cache", "net"];

/// Whether a capability token is accepted: a static [`KNOWN_CAPABILITIES`]
/// entry; a delegated-Bus verb grant `bus:<verb>` (the `bus:` prefix + one verb
/// from the EXPLICIT delegated-safe allowlist —
/// `bus_call_handler::is_delegated_safe_bus_verb`; NO wildcards, ONLY verbs with
/// a daemon-side delegated-envelope authz spine, else a confused-deputy hole);
/// or the per-route target-service binding `bus-svc:<service>` (which Bus service
/// the granted verbs route to, e.g. `filesd-notes` — needed when the verb prefix
/// isn't the instance service name); or `db-schema:<name>`, which grants THIS
/// route access to an ATTACHed aux database schema (`WebdVhostConfig.aux_dbs`).
/// The plain `db` capability reaches `main` only; a co-hosted handler without
/// the matching `db-schema:<name>` grant is denied at every statement by a
/// SQLite authorizer, so aux schemas on the shared connection stay tenant-scoped.
fn is_known_capability(tok: &str) -> bool {
    KNOWN_CAPABILITIES.contains(&tok)
        || tok
            .strip_prefix("bus:")
            .is_some_and(crate::bus_call_handler::is_delegated_safe_bus_verb)
        || tok
            .strip_prefix("bus-svc:")
            .is_some_and(is_valid_bus_service)
        || tok
            .strip_prefix("db-schema:")
            .is_some_and(crate::is_valid_schema_name)
        // `wake:<verb>` — an authority-free accelerator wake webd fires ITSELF,
        // best-effort, after the route returns a non-error (`< 400`) response for
        // an authenticated session (decoupled from `bus-svc:`, so a route pinned to
        // another service for its real work can still nudge a queue drain).
        // Restricted to the EXACT accelerator wake verbs (no args, no target) —
        // never the broader delegated-safe set.
        || tok
            .strip_prefix("wake:")
            .is_some_and(crate::bus_call_handler::is_accelerator_wake_verb)
}

/// A valid `bus-svc:` target-service name: non-empty, ≤64 bytes, ASCII
/// lowercase/digit/hyphen only (e.g. `maild`, `filesd-notes`). NO dots — a
/// dotted name collides with the mesh's `<service>.<node>.bus` routing.
fn is_valid_bus_service(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Phase-1 schema. Field order matches the module docstring's column
/// order so a regression that drops a field reads as a single-row diff.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "route_id".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Caller-chosen opaque label; also the substrate row's primary key.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "vhost_fqdn".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Primary FQDN this handler serves. Alias requests resolve to the same \
                   vhost, so a handler on the primary also serves its aliases."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "method".into(),
            ty: FieldType::Enum {
                variants: method_variants(),
            },
            default: None,
            secret: false,
            help: "GET | POST | PUT | DELETE | ANY. ANY matches any request method.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "path_pattern".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Must start with '/'. Exact ('/blog') or single trailing glob ('/blog/*')."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "handler_kind".into(),
            ty: FieldType::Enum {
                variants: handler_kind_variants(),
            },
            default: None,
            secret: false,
            help: "mix (only kind wired now). Reserved-open for future static/native kinds.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "handler_ref".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "A .mix path relative to the vhost's www_dir. No leading '/', no '..'.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "Default true. Disabled handlers are skipped at table-build time.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "capabilities".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Opt-in capabilities beyond read-only: 'db' (per-vhost SQLite via \
                   db_query/db_exec), 'jmap' (the vhost maild upstream via jmap()), \
                   'public_read' (webd-internal — lets an anonymous request back jmap() \
                   with the vhost's public_read_* content credential; no sandbox widening), \
                   'net' (outbound HTTP builtins http_get/http_post/http_request/dns_lookup \
                   for THIS route only — payment-gateway/registrar handlers), \
                   'db-schema:<name>' (access to an ATTACHed aux database schema; \
                   'db' alone reaches main only). Plus 'bus:<verb>' + 'bus-svc:<service>' \
                   delegated-Bus grants. Default empty."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// Capability surface — Phase-1 posture: every reachable peer (WG `/24`
/// trust domain) gets the full read/write/describe/audit set, matching
/// `webd.vhosts`. Cross-mesh authz narrows later without renaming caps.
pub fn auth_policy(service: &str) -> AuthPolicy {
    let read = Capability::from(format!("props.read:{service}.handlers"));
    let write = Capability::from(format!("props.write:{service}.handlers"));
    let describe_public = Capability::from(format!("props.describe:{service}.handlers:public"));
    let describe_full = Capability::from(format!("props.describe:{service}.handlers:full"));
    let audit = Capability::from(format!("props.audit:{service}.handlers"));
    let caps: CapabilitySet = [read, write, describe_public, describe_full, audit]
        .into_iter()
        .collect();
    AuthPolicy::new(move |_peer: &PeerIdentity| caps.clone())
}

/// Build the `NamespaceSpec`. Collection cardinality keyed on
/// `route_id`, shared `__props_values` storage, validating hooks,
/// `require_version = true` (SPEC-12 §4.4 optimistic concurrency — two
/// operators editing the same `route_id` can't lose an update).
pub fn spec(service: &str, hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "route_id".to_string(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.auth = auth_policy(service);
    s.hooks = hooks;
    s.require_version = true;
    s
}

/// Canonical [`NamespaceName`] for `webd.handlers`.
pub(crate) fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Hook handler for `webd.handlers`. Carries the reload `Notify` the
/// rebuild task in `main.rs` waits on.
pub struct HandlersHooks {
    reload: Arc<Notify>,
}

impl HandlersHooks {
    pub fn new(reload: Arc<Notify>) -> Self {
        Self { reload }
    }
}

/// Materialise the effective row a `before_set` validates against. For
/// a `Patch` over an existing Object, shallow-merge the body on top of
/// the prior; otherwise the body IS the effective row. Returns `None`
/// if the body isn't an Object (the caller rejects that case).
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

/// `&str` from an Object field, or `None` if absent / not a String.
fn field_as_str<'a>(obj: &'a BTreeMap<String, PropValue>, key: &str) -> Option<&'a str> {
    match obj.get(key) {
        Some(PropValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Validate a `handler_ref`: non-empty, relative (no leading `/`), no
/// `..` path component, `.mix` suffix. The dispatch path re-checks
/// traversal against the live `www_dir` (canonicalised) — this is the
/// early, operator-legible rejection.
fn validate_handler_ref(handler_ref: &str) -> Result<(), String> {
    if handler_ref.is_empty() {
        return Err("handler_ref must not be empty".into());
    }
    if handler_ref.starts_with('/') {
        return Err("handler_ref must be relative to the vhost www_dir (no leading '/')".into());
    }
    if handler_ref.contains('\0') {
        return Err("handler_ref must not contain a NUL byte".into());
    }
    if handler_ref.split('/').any(|seg| seg == "..") {
        return Err("handler_ref must not contain a '..' path component".into());
    }
    if !handler_ref.ends_with(".mix") {
        return Err("handler_ref must name a .mix script".into());
    }
    Ok(())
}

/// Validate a `path_pattern`: starts with `/`; if it contains `*`, it
/// must be a single trailing `/*` glob (matching the runtime matcher in
/// [`crate::mix_handler`]).
fn validate_path_pattern(path_pattern: &str) -> Result<(), String> {
    if !path_pattern.starts_with('/') {
        return Err("path_pattern must start with '/'".into());
    }
    let stars = path_pattern.matches('*').count();
    if stars > 0 && !(stars == 1 && path_pattern.ends_with("/*")) {
        return Err(
            "path_pattern may only use a single trailing '/*' glob (e.g. '/blog/*'); \
             no other '*' placement is supported"
                .into(),
        );
    }
    Ok(())
}

impl HookHandler for HandlersHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let new_obj = match &ctx.new {
                Some(PropValue::Object(m)) => m,
                Some(other) => {
                    return Err(HookError::validation(format!(
                        "webd.handlers row must be an Object, got {}",
                        other.type_name(),
                    )));
                }
                None => return Ok(()),
            };

            // Rule 1 — `route_id` body field must match the record key
            // (the substrate uses the wire key as the canonical id).
            if let Some(route_id) = field_as_str(new_obj, "route_id")
                && route_id != ctx.key.key.as_str()
            {
                return Err(HookError::validation(format!(
                    "route_id body field {route_id:?} does not match record key {:?}; \
                     either omit the body field or align them",
                    ctx.key.key,
                )));
            }

            // The cross-field rules run against the merged effective row
            // so a Patch can't sneak past by carrying only one half of a
            // violation (mirrors webd.vhosts).
            let effective = effective_row(ctx)
                .expect("ctx.new was validated as Object above; effective_row must produce Some");

            // Rule 2 — method enum. lib-props' FieldType::Enum is a
            // placeholder shape today, so the hook is the real
            // enforcement point (same posture as webd.vhosts dns01).
            if let Some(method) = field_as_str(&effective, "method") {
                if !method_variants().iter().any(|v| v == method) {
                    return Err(HookError::validation(format!(
                        "method {method:?} must be one of GET|POST|PUT|DELETE|ANY",
                    )));
                }
            } else {
                return Err(HookError::validation("method is required"));
            }

            // Rule 3 — handler_kind enum (only `mix` wired).
            match field_as_str(&effective, "handler_kind") {
                Some("mix") => {}
                Some(other) => {
                    return Err(HookError::validation(format!(
                        "handler_kind {other:?} is not implemented; only 'mix' is wired in \
                         slice #3 (the enum is reserved-open for future static/native kinds)",
                    )));
                }
                None => return Err(HookError::validation("handler_kind is required")),
            }

            // Rule 4 — path_pattern.
            match field_as_str(&effective, "path_pattern") {
                Some(pp) => validate_path_pattern(pp).map_err(HookError::validation)?,
                None => return Err(HookError::validation("path_pattern is required")),
            }

            // Rule 5 — handler_ref (traversal/early-reject; dispatch
            // re-checks against the live www_dir).
            match field_as_str(&effective, "handler_ref") {
                Some(hr) => validate_handler_ref(hr).map_err(HookError::validation)?,
                None => return Err(HookError::validation("handler_ref is required")),
            }

            // Rule 5b — capabilities tokens must be known. An unknown
            // token (typo / a capability this daemon version doesn't
            // wire) is rejected so it can't silently grant nothing.
            if let Some(PropValue::List(items)) = effective.get("capabilities") {
                for item in items {
                    match item {
                        PropValue::String(tok) if is_known_capability(tok) => {}
                        PropValue::String(tok) => {
                            return Err(HookError::validation(format!(
                                "unknown capability {tok:?}; known: {KNOWN_CAPABILITIES:?} \
                                 or an exact delegated-Bus verb grant \"bus:maild.<verb>\"",
                            )));
                        }
                        other => {
                            return Err(HookError::validation(format!(
                                "capabilities entries must be strings, got {}",
                                other.type_name(),
                            )));
                        }
                    }
                }
            }

            // Rule 6 — row-shape gate. The merged effective row MUST
            // deserialise to the typed `HandlerRow`; without this a
            // caller could commit a row missing a non-optional field
            // (vhost_fqdn) that the table builder then can't read.
            // `deny_unknown_fields` also catches operator typos here
            // rather than silently at table-build time.
            let effective_value = PropValue::Object(effective);
            if let Err(e) = handler_row_from_value(&effective_value) {
                return Err(HookError::validation(format!(
                    "row shape rejected before commit: {e}. See \
                     handlers_namespace.rs::HandlerRow for the required field set.",
                )));
            }

            Ok(())
        })
    }

    fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Non-blocking, coalescing reload kick. The rebuild task
            // reads a fresh snapshot, so we carry no payload.
            self.reload.notify_one();
            Ok(())
        })
    }

    fn after_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            self.reload.notify_one();
            Ok(())
        })
    }
}

/// Register the `handlers` namespace into the supplied router + store
/// and return the runtime handle plus the reload `Notify` the rebuild
/// task waits on.
///
/// Idempotent against the same `(service, store)` pair via
/// `SqliteStore::register_namespace`. No row seeding — the handler set
/// is operator-authored at runtime via `webd.props.set
/// namespace=handlers`; at startup the table is built from whatever
/// rows were previously persisted (an empty namespace yields an empty
/// table and the static-file fallback is unchanged).
pub fn register_handlers_namespace(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
) -> Result<(Arc<Runtime>, Arc<Notify>)> {
    let service = router.service().to_string();
    let reload = Arc::new(Notify::new());
    let hooks = Hooks::new(HandlersHooks::new(reload.clone()));
    let spec = spec(&service, hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register handlers namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register handlers runtime on router: {e}"))?;
    Ok((runtime, reload))
}

/// Construct a `HandlerRow` from a substrate `PropValue::Object`.
pub fn handler_row_from_value(v: &PropValue) -> serde_json::Result<HandlerRow> {
    let json = serde_json::to_value(v)?;
    serde_json::from_value(json)
}

/// Snapshot every committed row as a typed `Vec<HandlerRow>`.
///
/// Reads via `Runtime::store().list(..)` so the snapshot is atomic
/// against concurrent writes. **Fail-fast on deserialisation:** a row
/// that fails `handler_row_from_value` is a substrate-invariant
/// violation (rule 6 rejects bad shapes on write), so it indicates a
/// substrate bug or a schema downgrade — the operator must see it
/// rather than have a handler silently vanish from the table.
pub async fn snapshot_rows(runtime: &Runtime) -> Result<Vec<HandlerRow>> {
    let snap = runtime
        .store()
        .list(&namespace_name())
        .await
        .map_err(|e| anyhow::anyhow!("webd.handlers list failed: {e}"))?;
    let mut rows: Vec<HandlerRow> = Vec::with_capacity(snap.value.len());
    for record in snap.value {
        let row = handler_row_from_value(&record.value).map_err(|e| {
            anyhow::anyhow!(
                "webd.handlers row {key:?} failed HandlerRow deserialisation: {e}. \
                 before_set rule 6 rejects bad shapes on write, so a malformed committed \
                 row indicates a substrate bug or schema downgrade.",
                key = record.key.key,
            )
        })?;
        if record.key.key.as_str() != row.route_id {
            return Err(anyhow::anyhow!(
                "webd.handlers row key/body mismatch — record.key.key={:?} but \
                 route_id={:?}. before_set rule 1 rejects this on write.",
                record.key.key,
                row.route_id,
            ));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One-shot AMP→Bus rename migration for durable `webd.handlers` rows.
///
/// Rows written by a pre-rename daemon carry delegated-call grants
/// spelled `amp:<verb>` / `amp-svc:<service>`. The renamed daemon only
/// recognises the `bus:` / `bus-svc:` prefixes, so left in place those
/// grants are silently inert (the route loses delegated Bus access) —
/// and because `is_known_capability` no longer accepts the `amp:`
/// spellings, any future patch to such a row would fail hook
/// validation, leaving it un-editable. This pass rewrites both
/// prefixes in the stored capability list through the backend-origin
/// path, before the handler table first builds. Idempotent: a store
/// with no legacy grants is a no-op.
///
/// Returns the number of rows migrated.
pub async fn migrate_amp_capability_rows(runtime: &Runtime, ts_ms: i64) -> Result<usize> {
    let namespace = namespace_name();
    let snap = runtime
        .store()
        .list(&namespace)
        .await
        .map_err(|e| anyhow::anyhow!("amp→bus migration: webd.handlers list failed: {e}"))?;
    let mut migrated = 0usize;
    for record in snap.value {
        let PropValue::Object(obj) = &record.value else {
            continue;
        };
        let Some(PropValue::List(items)) = obj.get("capabilities") else {
            continue;
        };
        let mut changed = false;
        let rewritten: Vec<PropValue> = items
            .iter()
            .map(|item| match item {
                PropValue::String(tok) => {
                    let new_tok = if let Some(verb) = tok.strip_prefix("amp:") {
                        format!("bus:{verb}")
                    } else if let Some(svc) = tok.strip_prefix("amp-svc:") {
                        format!("bus-svc:{svc}")
                    } else {
                        return item.clone();
                    };
                    changed = true;
                    PropValue::String(new_tok)
                }
                other => other.clone(),
            })
            .collect();
        if !changed {
            continue;
        }
        let route_id = record.key.key.clone();
        let mut patch = BTreeMap::new();
        patch.insert("capabilities".to_string(), PropValue::List(rewritten));
        // A HOOK refusal degrades, never aborts: a legacy grant that
        // today's hook refuses (e.g. a verb no longer on the
        // delegated-safe allowlist) leaves the row exactly as
        // pre-migration — grants inert, row un-editable — which must
        // not brick daemon startup. Loud ERROR so the operator sees
        // the row that needs hand-repair. Storage/OCC errors are NOT
        // that case — a version race or SQL failure says nothing about
        // the row's grants, and swallowing it would silently strand a
        // migratable row — so they propagate and abort startup.
        let write = runtime
            .set_with_origin(
                RecordKey::collection(namespace.clone(), route_id.clone()),
                PropValue::Object(patch),
                SetOpts {
                    expected_version: Some(record.version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("amp→bus rename migration".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await;
        match write {
            Ok(_) => {
                tracing::info!(
                    target: "webd::handlers_namespace",
                    route_id = %route_id,
                    "amp→bus migration: rewrote legacy amp:/amp-svc: capability grants",
                );
                migrated += 1;
            }
            Err(cosmix_props::RuntimeError::Hook(e)) => {
                tracing::error!(
                    target: "webd::handlers_namespace",
                    route_id = %route_id,
                    error = %e,
                    "amp→bus migration: hook refused the rewritten row; row left \
                     as-is (its legacy amp: grants stay inert and the row cannot \
                     be edited until repaired by hand)",
                );
            }
            Err(e @ cosmix_props::RuntimeError::Store(_)) => {
                return Err(anyhow::anyhow!(
                    "amp→bus migration: storage/OCC failure rewriting \
                     webd.handlers row route_id={route_id:?}: {e}"
                ));
            }
        }
    }
    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::{Actor, RecordKey, SetOpts, Version, WriteOrigin};
    use rusqlite::Connection;

    fn open_test_store() -> Arc<SqliteStore> {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        Arc::new(SqliteStore::new("webd", conn).expect("store"))
    }

    fn new_router() -> PropsRouter {
        PropsRouter::new("webd")
    }

    /// A valid handler row body keyed by `route_id`.
    fn valid_row(route_id: &str) -> PropValue {
        let mut m = BTreeMap::new();
        m.insert("route_id".into(), PropValue::String(route_id.into()));
        m.insert("vhost_fqdn".into(), PropValue::String("example.com".into()));
        m.insert("method".into(), PropValue::String("GET".into()));
        m.insert("path_pattern".into(), PropValue::String("/hello".into()));
        m.insert("handler_kind".into(), PropValue::String("mix".into()));
        m.insert(
            "handler_ref".into(),
            PropValue::String("handlers/hello.mix".into()),
        );
        m.insert("enabled".into(), PropValue::Bool(true));
        PropValue::Object(m)
    }

    fn register(store: &Arc<SqliteStore>) -> (Arc<Runtime>, Arc<Notify>) {
        let mut router = new_router();
        register_handlers_namespace(&mut router, store).expect("register handlers namespace")
    }

    async fn set_caller(runtime: &Runtime, route_id: &str, body: PropValue) -> Result<(), String> {
        // require_version=true → a create passes expected_version =
        // zero (no prior row). Mirrors the webd.vhosts caller dance.
        let key = RecordKey::collection(namespace_name(), route_id);
        runtime
            .set_with_origin(
                key,
                body,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("webd"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
                WriteOrigin::caller(),
            )
            .await
            .map(|_| ())
            .map_err(|e| format!("{e}"))
    }

    #[test]
    fn register_clean() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        assert_eq!(runtime.spec().name.as_str(), NAMESPACE);
    }

    #[tokio::test]
    async fn valid_route_accepted_and_round_trips() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        set_caller(&runtime, "hello", valid_row("hello"))
            .await
            .expect("valid row accepted");
        let rows = snapshot_rows(&runtime).await.expect("snapshot");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].route_id, "hello");
        assert_eq!(rows[0].method, "GET");
        assert_eq!(rows[0].path_pattern, "/hello");
        assert!(rows[0].enabled);
    }

    #[tokio::test]
    async fn bad_method_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert("method".into(), PropValue::String("PATCH".into()));
        }
        let err = set_caller(&runtime, "x", body)
            .await
            .expect_err("PATCH rejected");
        assert!(err.contains("method"), "got: {err}");
    }

    #[tokio::test]
    async fn traversal_handler_ref_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert(
                "handler_ref".into(),
                PropValue::String("../../etc/passwd.mix".into()),
            );
        }
        let err = set_caller(&runtime, "x", body)
            .await
            .expect_err("traversal handler_ref rejected");
        assert!(err.contains(".."), "got: {err}");
    }

    #[tokio::test]
    async fn non_mix_handler_ref_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert(
                "handler_ref".into(),
                PropValue::String("handlers/hello.sh".into()),
            );
        }
        let err = set_caller(&runtime, "x", body)
            .await
            .expect_err("non-.mix handler_ref rejected");
        assert!(err.contains(".mix"), "got: {err}");
    }

    #[tokio::test]
    async fn bad_glob_path_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert("path_pattern".into(), PropValue::String("/a/*/b".into()));
        }
        let err = set_caller(&runtime, "x", body)
            .await
            .expect_err("mid-path glob rejected");
        assert!(err.contains("glob"), "got: {err}");
    }

    #[test]
    fn bus_verb_capability_validation() {
        // Static capabilities.
        for c in ["db", "jmap", "public_read"] {
            assert!(is_known_capability(c), "{c} should be known");
        }
        // The EXACT delegated-safe verb allowlist (the only accepted bus grants).
        for ok in [
            "bus:maild.vtoken.mint_opaque",
            "bus:maild.vtoken.list_opaque",
            "bus:maild.vtoken.lookup_opaque",
            "bus:maild.vtoken.disable_opaque",
        ] {
            assert!(is_known_capability(ok), "{ok:?} should be accepted");
        }
        // Rejected: wildcards; ANY maild verb outside the delegated-safe
        // allowlist (the confused-deputy hole — these have no delegated-envelope
        // authz); non-maild target; malformed; junk.
        for bad in [
            "bus:maild.vtoken.*",
            "bus:maild.vtoken.seed", // not in the allowlist
            "bus:maild.vtoken.add",  // legacy segmented verbs, removed in 0.4.0
            "bus:maild.vtoken.list",
            "bus:maild.vtoken.lookup",
            "bus:maild.vtoken.rotate",
            "bus:maild.vtoken.disable",
            "bus:maild.accounts.seed", // mesh-peer-authorized, envelope-ignoring
            "bus:maild.retention.run",
            "bus:noded.info",
            "bus:maild",
            "bus:",
            "bus:maild..list",
            "bus:Maild.vtoken.list",
            "ampx",
            "admin",
            "",
        ] {
            assert!(!is_known_capability(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn wake_capability_validation() {
        // Accepted: EXACTLY the argument-free accelerator wake verbs.
        for ok in ["wake:sshm.wake", "wake:provisiond.wake", "wake:toolsd.wake"] {
            assert!(is_known_capability(ok), "{ok:?} should be accepted");
        }
        // Rejected: a non-accelerator (even delegated-safe) verb, a bare/unknown
        // wake, malformed, and the empty case — a wake grant must never widen past
        // the closed accelerator list.
        for bad in [
            "wake:maild.vtoken.mint_opaque", // delegated-safe but NOT an accelerator
            "wake:evil.wake",                // not in the closed list
            "wake:sshm.scan",                // not a wake at all
            "wake:sshm",
            "wake:",
            "wake",
        ] {
            assert!(!is_known_capability(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn db_schema_capability_validation() {
        // Accepted: well-formed schema identifiers.
        for ok in ["db-schema:sshm", "db-schema:aux1", "db-schema:_x"] {
            assert!(is_known_capability(ok), "{ok:?} should be accepted");
        }
        // Rejected: reserved names, bad charset, empty, and anything that would
        // be unsafe to interpolate into `ATTACH ... AS <name>`.
        for bad in [
            "db-schema:main",
            "db-schema:temp",
            "db-schema:",
            "db-schema:1x",     // must not start with a digit
            "db-schema:Sshm",   // uppercase not allowed
            "db-schema:a b",    // space
            "db-schema:a;drop", // injection shape
            "db-schema:a.b",    // dotted
        ] {
            assert!(!is_known_capability(bad), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn unknown_capability_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert(
                "capabilities".into(),
                PropValue::List(vec![PropValue::String("bogus".into())]),
            );
        }
        let err = set_caller(&runtime, "x", body)
            .await
            .expect_err("unknown capability token rejected");
        assert!(err.contains("capability"), "got: {err}");
    }

    #[tokio::test]
    async fn db_capability_accepted() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert(
                "capabilities".into(),
                PropValue::List(vec![PropValue::String("db".into())]),
            );
        }
        set_caller(&runtime, "x", body)
            .await
            .expect("the 'db' capability token is accepted");
        let rows = snapshot_rows(&runtime).await.expect("snapshot");
        assert_eq!(rows[0].capabilities, vec!["db".to_string()]);
    }

    #[tokio::test]
    async fn net_capability_accepted() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        let mut body = valid_row("x");
        if let PropValue::Object(m) = &mut body {
            m.insert(
                "capabilities".into(),
                PropValue::List(vec![PropValue::String("net".into())]),
            );
        }
        set_caller(&runtime, "x", body)
            .await
            .expect("the 'net' capability token is accepted");
        let rows = snapshot_rows(&runtime).await.expect("snapshot");
        assert_eq!(rows[0].capabilities, vec!["net".to_string()]);
    }

    #[tokio::test]
    async fn route_id_key_mismatch_rejected() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);
        // body route_id="a" but wire key="b"
        let err = set_caller(&runtime, "b", valid_row("a"))
            .await
            .expect_err("route_id/key mismatch rejected");
        assert!(err.contains("route_id"), "got: {err}");
    }

    #[tokio::test]
    async fn after_set_fires_reload() {
        let store = open_test_store();
        let (runtime, reload) = register(&store);
        set_caller(&runtime, "hello", valid_row("hello"))
            .await
            .expect("valid row accepted");
        // notify_one stored a permit; notified() returns immediately.
        tokio::time::timeout(std::time::Duration::from_secs(2), reload.notified())
            .await
            .expect("reload notified after set");
    }

    /// AMP→Bus migration — legacy `amp:`/`amp-svc:` capability grants
    /// (written by a pre-rename daemon; seeded via raw `commit_set`
    /// because the renamed hook rejects the spellings) are rewritten to
    /// `bus:`/`bus-svc:`, non-prefixed capabilities and all other
    /// fields survive, and the pass is idempotent.
    #[tokio::test]
    async fn migrate_amp_capability_rows_rewrites_and_is_idempotent() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);

        let PropValue::Object(mut body) = valid_row("legacy-route") else {
            unreachable!("valid_row returns an Object");
        };
        body.insert(
            "capabilities".into(),
            PropValue::List(vec![
                PropValue::String("db".into()),
                PropValue::String("amp:filesd.list".into()),
                PropValue::String("amp-svc:filesd-fs".into()),
            ]),
        );
        runtime
            .store()
            .commit_set(cosmix_props::SetCommit {
                key: RecordKey::collection(namespace_name(), "legacy-route".to_string()),
                new_value: PropValue::Object(body),
                expected_version: Some(Version::zero()),
                merge: MergeMode::Replace,
                lifecycle: None,
                actor: Actor::service("webd"),
                cause: Some("test seed (pre-rename daemon)".into()),
                ts_ms: 1_000,
            })
            .await
            .expect("raw-seed legacy amp-grant row");

        let migrated = migrate_amp_capability_rows(&runtime, 2_000)
            .await
            .expect("migration pass");
        assert_eq!(migrated, 1, "exactly one legacy row migrated");

        let rows = snapshot_rows(&runtime).await.expect("snapshot parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].capabilities,
            vec![
                "db".to_string(),
                "bus:filesd.list".to_string(),
                "bus-svc:filesd-fs".to_string(),
            ],
            "amp prefixes rewritten, order and non-prefixed caps preserved"
        );
        assert_eq!(rows[0].route_id, "legacy-route");
        assert_eq!(rows[0].handler_ref, "handlers/hello.mix");

        // Second pass is a no-op and must not rewrite the row.
        let snap = runtime.store().list(&namespace_name()).await.expect("list");
        let v_after = snap.value[0].version;
        let migrated_again = migrate_amp_capability_rows(&runtime, 3_000)
            .await
            .expect("second migration pass");
        assert_eq!(migrated_again, 0, "idempotent: nothing left to migrate");
        let snap = runtime.store().list(&namespace_name()).await.expect("list");
        assert_eq!(
            snap.value[0].version, v_after,
            "no-op pass must not rewrite the row"
        );
    }

    /// AMP→Bus migration degrade path — a legacy grant today's hook
    /// refuses (verb absent from the delegated-safe allowlist) leaves
    /// its row as-is WITHOUT aborting the pass; a valid legacy row in
    /// the same store still migrates.
    #[tokio::test]
    async fn migrate_amp_capability_rows_hook_refusal_degrades_per_row() {
        let store = open_test_store();
        let (runtime, _reload) = register(&store);

        for (route_id, grant) in [
            ("refused-route", "amp:filesd.resync"),
            ("valid-route", "amp:filesd.list"),
        ] {
            let PropValue::Object(mut body) = valid_row(route_id) else {
                unreachable!("valid_row returns an Object");
            };
            body.insert(
                "capabilities".into(),
                PropValue::List(vec![PropValue::String(grant.into())]),
            );
            runtime
                .store()
                .commit_set(cosmix_props::SetCommit {
                    key: RecordKey::collection(namespace_name(), route_id.to_string()),
                    new_value: PropValue::Object(body),
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    lifecycle: None,
                    actor: Actor::service("webd"),
                    cause: Some("test seed (pre-rename daemon)".into()),
                    ts_ms: 1_000,
                })
                .await
                .expect("raw-seed legacy row");
        }

        // `filesd.resync` is deliberately NOT web-delegable, so the
        // rewritten `bus:filesd.resync` fails hook validation — that
        // row must be left untouched while the valid row migrates and
        // the pass still returns Ok.
        let migrated = migrate_amp_capability_rows(&runtime, 2_000)
            .await
            .expect("migration pass must not abort on a hook refusal");
        assert_eq!(migrated, 1, "only the valid legacy row migrates");

        let snap = runtime.store().list(&namespace_name()).await.expect("list");
        for record in snap.value {
            let PropValue::Object(obj) = &record.value else {
                panic!("row must be an Object");
            };
            let Some(PropValue::List(caps)) = obj.get("capabilities") else {
                panic!("row must carry capabilities");
            };
            let caps: Vec<&str> = caps
                .iter()
                .map(|c| match c {
                    PropValue::String(s) => s.as_str(),
                    other => panic!("capability must be a string, got {other:?}"),
                })
                .collect();
            match record.key.key.as_str() {
                "refused-route" => assert_eq!(
                    caps,
                    vec!["amp:filesd.resync"],
                    "refused row left exactly as pre-migration"
                ),
                "valid-route" => assert_eq!(caps, vec!["bus:filesd.list"], "valid row migrated"),
                other => panic!("unexpected row {other:?}"),
            }
        }
    }
}
