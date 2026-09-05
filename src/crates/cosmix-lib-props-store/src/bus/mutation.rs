//! SPEC 12 §5–§8 — structured-mode property mutation verbs.
//!
//! [`PropsRouter`] owns a per-service set of [`Runtime`] coordinators
//! (one per registered namespace) and dispatches `<svc>.props.*` Bus
//! requests against them. Each verb:
//!
//! 1. Resolves the `namespace` header to a registered [`Runtime`].
//! 2. Runs the namespace's [`crate::namespace::AuthPolicy`] over the supplied
//!    [`PeerIdentity`] to obtain the caller's [`CapabilitySet`], then
//!    checks the §7.1 capability token for the verb.
//! 3. Parses §8.2 headers + §8.3 body, calls the runtime / store, and
//!    projects the result into the §5.x response shape.
//! 4. Maps [`RuntimeError`] / [`StoreError`] back to the §9 error
//!    taxonomy via [`PropsResponse`] body envelopes.
//!
//! ### Verb scope (C4 + C5 + C10c)
//!
//! - `set`, `delete`, `list`, `get`, `describe` are fully synchronous
//!   request/response (C4).
//! - `watch` (C4 replay shape + C10c live bridge): authorize a broker
//!   subscription on `<svc>.props.records.changed` via the installed
//!   [`SubscribeGranter`], then return events strictly newer than
//!   `since_nseq` and bounded by the post-grant observed cursor.
//! - `audit.watch` (C5 audit-row format + C10c live bridge): same flow
//!   on `<svc>.props.audit`, projecting per-row `audit_digest`.
//! - Broker-side reservation of `<svc>.props.records.changed` and
//!   `<svc>.props.audit` topics (§7.4, §15.5) is C6, in `cosmix-noded`.
//!
//! ### Module boundary
//!
//! This module is pure routing: it consumes [`BusMessage`] (headers +
//! body) and produces [`PropsResponse`]. It does not persist state
//! itself. The live-subscription handoff is mediated through the
//! injected [`SubscribeGranter`] (C10c), so this module remains
//! transport-agnostic and tests can drive the verbs with an in-process
//! granter stub instead of standing up a noded.

use std::collections::BTreeMap;
use std::sync::Arc;

use cosmix_bus::bus::BusMessage;
use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::capability::{Capability, CapabilitySet};
use crate::dispatcher::{audit_topic, records_changed_topic};
use crate::lifecycle::Lifecycle;
use crate::namespace::{
    Cardinality, FieldSchema, NamespaceName, NamespaceSpec, PeerIdentity, PropertySchema,
    SchemaPublic, Validator,
};
use crate::record::{Actor, EventKind, Nseq, Record, RecordEvent, RecordKey, Version};
use crate::runtime::{DeleteOpts, Runtime, RuntimeError, SetOpts, SetOutcome};
use crate::store::{MergeMode, StoreError};
use crate::subscribe_granter::SubscribeGranter;
use cosmix_props_core::bus::PropsResponse;
use cosmix_props_core::value::PropValue;

/// §9 wire `error_code` token vocabulary. Closed in v0.1; new variants
/// require a spec amendment.
#[derive(Debug, Clone, Copy)]
enum ErrorCode {
    AuthDenied,
    NotFound,
    ValidationError,
    Conflict,
    VersionMismatch,
    StorageError,
    HookError,
    Unavailable,
    ReplayWindowExceeded,
    /// SPEC 12 §15.5 / C10c — `watch` or `audit.watch` could not
    /// install the broker-side live subscription via the configured
    /// `SubscribeGranter`. Operational (rc=10): caller MUST treat as
    /// "watch did not take" and retry; replay-only is *not* delivered.
    GrantFailed,
}

impl ErrorCode {
    fn token(self) -> &'static str {
        match self {
            Self::AuthDenied => "auth_denied",
            Self::NotFound => "not_found",
            Self::ValidationError => "validation_error",
            Self::Conflict => "conflict",
            Self::VersionMismatch => "version_mismatch",
            Self::StorageError => "storage_error",
            Self::HookError => "hook_error",
            Self::Unavailable => "unavailable",
            Self::ReplayWindowExceeded => "replay_window_exceeded",
            Self::GrantFailed => "grant_failed",
        }
    }

    /// §9 numeric rc: 10 = operational, 20 = port-degrading.
    fn rc(self) -> i32 {
        match self {
            Self::StorageError | Self::HookError | Self::Unavailable => 20,
            _ => 10,
        }
    }
}

fn err(code: ErrorCode, message: impl Into<String>) -> PropsResponse {
    err_with(code, message, json!({}))
}

fn err_with(code: ErrorCode, message: impl Into<String>, extra: Json) -> PropsResponse {
    let message = message.into();
    let mut body = json!({
        "error_code": code.token(),
        "message": message,
    });
    if let Json::Object(extra_map) = extra
        && let Json::Object(body_map) = &mut body
    {
        for (k, v) in extra_map {
            body_map.insert(k, v);
        }
    }
    let rc = code.rc();
    PropsResponse {
        rc,
        body: body.to_string(),
        error: Some(message),
    }
}

fn ok(body: Json) -> PropsResponse {
    PropsResponse {
        rc: 0,
        body: body.to_string(),
        error: None,
    }
}

/// Per-service registry of property-namespace runtimes. The daemon
/// constructs one of these at startup, registers each of its
/// namespaces, and hands `dispatch` a `<svc>.props.<verb>` request as
/// it arrives from its broker connection.
pub struct PropsRouter {
    service: String,
    runtimes: BTreeMap<NamespaceName, Arc<Runtime>>,
    /// SPEC 12 §15.5 / C10c — when set, `watch` and `audit.watch`
    /// transition the caller to live broker delivery on the reserved
    /// `<svc>.props.records.changed` / `<svc>.props.audit` topic via
    /// this granter (production: `NodedSubscribeGranter` issuing
    /// `noded.props.subscribe_grant`).
    granter: Option<Arc<dyn SubscribeGranter>>,
    /// SPEC 12 §15.5 / C10c — explicit replay-only opt-in for tests
    /// and autoresearch harnesses. Production daemons MUST install a
    /// granter; without one, `watch`/`audit.watch` refuse with
    /// `unavailable` rather than silently downgrading to replay-only.
    /// The opt-in keeps the test path ergonomic while making the
    /// production misconfiguration loud at first request — the
    /// alternative (silent degradation) is the C9 partial-fix anti-
    /// pattern.
    replay_only_acknowledged: bool,
}

impl PropsRouter {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            runtimes: BTreeMap::new(),
            granter: None,
            replay_only_acknowledged: false,
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// Install the watch-mediated subscriber bridge (SPEC 12 C10c).
    /// `watch` / `audit.watch` will then ask `granter` to subscribe the
    /// caller to the reserved topic after the cap check succeeds.
    /// Idempotent: calling `set_granter` twice replaces the prior
    /// granter (production startup wires this exactly once at daemon
    /// init; tests may rebuild routers freely).
    pub fn set_granter(&mut self, granter: Arc<dyn SubscribeGranter>) {
        self.granter = Some(granter);
    }

    /// Acknowledge replay-only mode (tests / autoresearch harnesses).
    /// Without a granter installed, `watch` and `audit.watch` refuse
    /// by default; this flag flips that refusal into a `live: false`
    /// response carrying the replay slice.
    ///
    /// **Compile-time gated** behind `cfg(any(test, feature =
    /// "replay-only-harness"))`. Production daemon builds do not
    /// enable the `replay-only-harness` feature, so the function is
    /// unreachable to production code paths — calling it would fail
    /// to compile. Internal unit tests reach it via `cfg(test)`.
    /// Downstream test harnesses (autoresearch, integration crates)
    /// opt in by adding `cosmix-lib-props-store = { ..., features = [
    /// "replay-only-harness"] }` to their `dev-dependencies`.
    ///
    /// The runtime `tracing::warn!` is defense-in-depth: if the
    /// feature ever leaks into a production build (e.g. via a
    /// transitive enabled-via-dev-dep mistake), the first call
    /// surfaces in daemon logs alongside the §5.5 contract downgrade
    /// rationale.
    #[cfg(any(test, feature = "replay-only-harness"))]
    #[doc(hidden)]
    pub fn allow_replay_only_for_tests(&mut self) {
        tracing::warn!(
            target: "cosmix_lib_props::bus::mutation",
            service = %self.service,
            "PropsRouter::allow_replay_only_for_tests called — \
             watch handlers will not transition callers to live broker delivery. \
             This is a test/harness opt-in; production daemons must call set_granter instead.",
        );
        self.replay_only_acknowledged = true;
    }

    /// Iterate the registered runtimes. The fan-out
    /// [`crate::dispatcher::spawn_dispatcher`] (C9) uses this to spawn one
    /// publish task per namespace at startup; no other surface needs to
    /// walk the registration set, so the iterator yields shared refs
    /// rather than exposing the underlying `BTreeMap`.
    pub fn iter_runtimes(&self) -> impl Iterator<Item = &Arc<Runtime>> {
        self.runtimes.values()
    }

    /// Register a namespace under this router. Trust-boundary check —
    /// the runtime's owning service must match this router's service
    /// so a stray namespace registration cannot be smuggled into
    /// another daemon's verb surface.
    pub fn register(&mut self, runtime: Arc<Runtime>) -> Result<(), String> {
        if runtime.service() != self.service {
            return Err(format!(
                "runtime service {:?} does not match router service {:?}",
                runtime.service(),
                self.service
            ));
        }
        let name = runtime.spec().name.clone();
        if self.runtimes.contains_key(&name) {
            return Err(format!("namespace {} already registered", name.as_str()));
        }
        self.runtimes.insert(name, runtime);
        Ok(())
    }

    fn lookup<'a>(&'a self, ns: &str) -> Result<&'a Arc<Runtime>, PropsResponse> {
        let name = NamespaceName::new(ns).map_err(|e| {
            err(
                ErrorCode::ValidationError,
                format!("invalid namespace: {e}"),
            )
        })?;
        self.runtimes.get(&name).ok_or_else(|| {
            err(
                ErrorCode::NotFound,
                format!("namespace not registered: {ns}"),
            )
        })
    }

    /// Entry point. `suffix` is the part of the Bus command name after
    /// `<svc>.props.` — i.e. `"set"`, `"delete"`, `"get"`, `"list"`,
    /// `"describe"`, or `"watch"`.
    pub async fn dispatch(
        &self,
        suffix: &str,
        request: &BusMessage,
        peer: &PeerIdentity,
    ) -> PropsResponse {
        match suffix {
            "set" => self.handle_set(request, peer).await,
            "delete" => self.handle_delete(request, peer).await,
            "get" => self.handle_get(request, peer).await,
            "list" => self.handle_list(request, peer).await,
            "describe" => self.handle_describe(request, peer).await,
            "watch" => self.handle_watch(request, peer).await,
            "audit.watch" => self.handle_audit_watch(request, peer).await,
            other => err(
                ErrorCode::ValidationError,
                format!("unknown props subcommand: {other}"),
            ),
        }
    }

    async fn handle_set(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        if req.get("path").is_some() {
            return err(
                ErrorCode::ValidationError,
                "flat_path_mutation_deferred: <svc>.props.set v0.1 is structured mode only",
            );
        }
        if let Err(e) = refuse_subpath_selectors(req) {
            return e;
        }

        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    "missing required header: namespace",
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();

        if let Err(e) = require_cap(spec, &self.service, peer, "write", None) {
            return e;
        }

        let key = match build_record_key(spec, req.get("key")) {
            Ok(k) => k,
            Err(e) => return e,
        };

        let expected_version = match parse_if_version(req.get("if_version")) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let merge = match parse_merge(req.get("merge")) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let value = match parse_set_body(&req.body) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let opts = SetOpts {
            expected_version,
            merge,
            actor: match peer_actor(peer, &self.service) {
                Ok(actor) => actor,
                Err(e) => return err(ErrorCode::ValidationError, e.to_string()),
            },
            cause: req.get("cause").map(|s| s.to_string()),
            ts_ms: now_ms(),
        };

        match rt.set(key.clone(), value, opts).await {
            Ok(out) => ok(project_set(spec, ns, &key, &out)),
            Err(e) => runtime_error_to_response(e),
        }
    }

    async fn handle_delete(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        if let Err(e) = refuse_flat_path(req) {
            return e;
        }
        if let Err(e) = refuse_subpath_selectors(req) {
            return e;
        }
        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    "missing required header: namespace",
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();

        if let Err(e) = require_cap(spec, &self.service, peer, "write", None) {
            return e;
        }

        // §5.4 — singleton namespaces cannot be deleted (only `set` to
        // defaults). Refuse before parsing the key so the error is
        // shaped against intent, not coincidence.
        if matches!(spec.cardinality, Cardinality::Singleton { .. }) {
            return err(
                ErrorCode::ValidationError,
                "singleton namespaces do not support delete",
            );
        }
        let key = match build_record_key(spec, req.get("key")) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let expected_version = match parse_if_version(req.get("if_version")) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let opts = DeleteOpts {
            expected_version,
            actor: match peer_actor(peer, &self.service) {
                Ok(actor) => actor,
                Err(e) => return err(ErrorCode::ValidationError, e.to_string()),
            },
            cause: req.get("cause").map(|s| s.to_string()),
            ts_ms: now_ms(),
        };

        match rt.delete(key.clone(), opts).await {
            Ok(event) => ok(json!({
                "namespace": ns,
                "key": key.key,
                "deleted": true,
                "tombstone_version": event.version.0,
                "nseq": event.nseq.0,
            })),
            Err(e) => runtime_error_to_response(e),
        }
    }

    async fn handle_get(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        if let Err(e) = refuse_flat_path(req) {
            return e;
        }
        if let Err(e) = refuse_subpath_selectors(req) {
            return e;
        }
        if let Err(e) = refuse_get_body(req) {
            return e;
        }
        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    "missing required header: namespace (flat-path mode is on the SPEC 07 surface)",
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();

        let caps = spec.auth.resolve(peer);
        if !caps.contains(&cap(spec, &self.service, "read", None)) {
            return err(ErrorCode::AuthDenied, "missing capability");
        }
        let with_secrets = caps.contains(&cap(spec, &self.service, "read", Some("secrets")));

        let key = match build_record_key(spec, req.get("key")) {
            Ok(k) => k,
            Err(e) => return e,
        };

        match rt.store().get(&key).await {
            Ok(snap) => ok(project_record_envelope(
                ns,
                &snap.value,
                snap.observed_nseq,
                &spec.schema,
                with_secrets,
            )),
            Err(e) => store_error_to_response(e),
        }
    }

    async fn handle_list(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        if let Err(e) = refuse_flat_path(req) {
            return e;
        }
        if let Err(e) = refuse_unsupported_list_options(req) {
            return e;
        }
        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    "missing required header: namespace (flat-path list is on the SPEC 07 surface)",
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();

        let caps = spec.auth.resolve(peer);
        if !caps.contains(&cap(spec, &self.service, "read", None)) {
            return err(ErrorCode::AuthDenied, "missing capability");
        }
        let with_secrets = caps.contains(&cap(spec, &self.service, "read", Some("secrets")));

        match rt.store().list(&spec.name).await {
            Ok(snap) => {
                let records: Vec<Json> = snap
                    .value
                    .iter()
                    .map(|r| project_record_summary(r, &spec.schema, with_secrets))
                    .collect();
                ok(json!({
                    "namespace": ns,
                    "nseq": snap.observed_nseq.0,
                    "records": records,
                    "next_cursor": Json::Null,
                }))
            }
            Err(e) => store_error_to_response(e),
        }
    }

    async fn handle_describe(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        if let Err(e) = refuse_flat_path(req) {
            return e;
        }
        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    "missing required header: namespace (flat-path describe is on the SPEC 07 surface)",
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();
        let view = req.get("view").unwrap_or("public");

        match view {
            "public" => {
                if matches!(spec.schema_public, SchemaPublic::Deny) {
                    return err(
                        ErrorCode::AuthDenied,
                        "namespace declared schema_public: deny",
                    );
                }
                if let Err(e) = require_cap(spec, &self.service, peer, "describe", Some("public")) {
                    return e;
                }
                ok(project_schema(spec, ns, /* full = */ false))
            }
            "full" => {
                if let Err(e) = require_cap(spec, &self.service, peer, "describe", Some("full")) {
                    return e;
                }
                ok(project_schema(spec, ns, /* full = */ true))
            }
            other => err(
                ErrorCode::ValidationError,
                format!("unknown view: {other} (expected public|full)"),
            ),
        }
    }

    /// SPEC 12 §5.5 — watch verb. After the capability check, if a
    /// [`SubscribeGranter`] is installed (C10c), transition the caller
    /// to live broker delivery on `<svc>.props.records.changed`
    /// filtered to `namespace`, then replay events strictly newer than
    /// `since_nseq` and bounded by the post-grant observed cursor.
    ///
    /// Replay/live ordering is load-bearing (`_doc/planned/spec12-c10-
    /// watch-mediated-subscribe.md` §"Replay/live ordering"):
    ///
    /// 1. cap-check
    /// 2. grant → broker subscription is now live
    /// 3. observed = store.list().observed_nseq (taken AFTER grant)
    /// 4. events = events_since(ns, since_nseq) ∩ (·, observed]
    /// 5. respond { observed_nseq, events, live }
    ///
    /// The grant-then-observed order partitions events: any with
    /// `nseq > observed` is on the live side of the split (eligible
    /// for live delivery via the broker subscription, best-effort —
    /// the dispatcher's "Publish failure handling" section in
    /// [`crate::dispatcher`] is the canonical caveat); any with
    /// `nseq ≤ observed` is in the replay. Overlap is permitted; the
    /// §5.5 contract requires subscribers to de-dup by
    /// `(namespace, key, nseq)`.
    async fn handle_watch(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        self.handle_watch_inner(
            req,
            peer,
            "read",
            records_changed_topic(&self.service),
            "watch",
            project_records_changed,
            true,
        )
        .await
    }

    /// SPEC 12 §5.8 / §10 — `audit.watch` verb. Same flow as
    /// [`Self::handle_watch`] but: gated on `props.audit:<fqn>` (a
    /// capability distinct from `props.read`); projects per-row audit
    /// rows including `audit_digest` (§10 flat shape); transitions
    /// caller to live delivery on `<svc>.props.audit`.
    ///
    /// Passes `emit_caught_up = false`: §5.5 v0.2.1 added the
    /// `caught_up` marker to `<svc>.props.watch` only; §10 was not
    /// amended, so the audit reply shape is unchanged.
    async fn handle_audit_watch(&self, req: &BusMessage, peer: &PeerIdentity) -> PropsResponse {
        self.handle_watch_inner(
            req,
            peer,
            "audit",
            audit_topic(&self.service),
            "audit.watch",
            project_audit_row,
            false,
        )
        .await
    }

    /// Shared watch / audit.watch implementation. The differences
    /// between the two verbs are exactly the capability token, the
    /// reserved topic, the verb name (for diagnostics), the
    /// event-projection function, and whether the §5.5 v0.2.1
    /// `caught_up` marker is embedded in the response (`watch` does;
    /// `audit.watch` does not — §10 was not amended).
    #[allow(clippy::too_many_arguments)] // private helper, two call sites, each tuple is a verb-distinction
    async fn handle_watch_inner(
        &self,
        req: &BusMessage,
        peer: &PeerIdentity,
        cap_token: &str,
        topic: String,
        verb: &str,
        project: fn(&str, &PropertySchema, &RecordEvent) -> Json,
        emit_caught_up: bool,
    ) -> PropsResponse {
        if let Err(e) = refuse_flat_path(req) {
            return e;
        }
        let ns = match req.get("namespace") {
            Some(s) => s,
            None => {
                return err(
                    ErrorCode::ValidationError,
                    format!(
                        "missing required header: namespace ({verb}; flat-path is on the SPEC 07 surface)"
                    ),
                );
            }
        };
        let rt = match self.lookup(ns) {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let spec = rt.spec();

        if let Err(e) = require_cap(spec, &self.service, peer, cap_token, None) {
            return e;
        }

        let since_nseq = match parse_since_nseq(req.get("since_nseq")) {
            Ok(v) => v,
            Err(e) => return e,
        };

        // SPEC 12 C10c — replay/live ordering. Grant must complete
        // BEFORE the observed-cursor snapshot so the broker is already
        // routing publishes for the topic at the moment we record the
        // cursor. The doc's §"Replay/live ordering" walks the proof.
        let live = match &self.granter {
            Some(granter) => {
                // Anonymous peers (no registered service name) cannot
                // be granted a broker subscription — noded's
                // `subscribe_grant` requires `target_peer` to resolve
                // in the connection registry. This is an authorisation
                // failure, not a request-shape error: the caller is
                // authenticated but not in the class of identities
                // eligible for live delivery. Reject with `auth_denied`
                // so wire-level error routing matches the §9 taxonomy.
                let target_peer = match peer.service_name.as_deref() {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        return err(
                            ErrorCode::AuthDenied,
                            format!(
                                "{verb}: anonymous callers cannot receive live deliveries; \
                                 register as a service or omit live-watch usage"
                            ),
                        );
                    }
                };
                if let Err(e) = granter.grant(&topic, target_peer, ns).await {
                    // Log the underlying granter error (broker
                    // unreachable, target_peer_not_connected, transport
                    // detail) so operators can diagnose, but the
                    // peer-visible body carries only the §9 token.
                    // Reflecting raw `e` would leak peer-internal
                    // routing detail (other peers' connection state,
                    // socket paths) to the watching peer.
                    tracing::warn!(
                        target: "cosmix_lib_props::bus::mutation::watch",
                        error_code = "grant_failed",
                        verb = %verb,
                        topic = %topic,
                        namespace = %ns,
                        target_peer = %target_peer,
                        error = %e,
                        "SubscribeGranter::grant failed (peer-visible body redacts upstream detail)",
                    );
                    return err(
                        ErrorCode::GrantFailed,
                        format!("{verb}: grant_failed (see daemon log for upstream detail)"),
                    );
                }
                true
            }
            None if self.replay_only_acknowledged => false,
            None => {
                // Production safety: a router without a granter installed
                // and without explicit replay-only opt-in is a startup
                // wiring miss, not a deliberate degradation. Refuse so
                // the misconfiguration surfaces at first request rather
                // than silently downgrading to a partial guarantee.
                return err(
                    ErrorCode::Unavailable,
                    format!(
                        "{verb}: live delivery not configured — daemon must install a \
                         SubscribeGranter (production) or call \
                         `allow_replay_only_for_tests` (test harnesses)"
                    ),
                );
            }
        };

        let store = rt.store();
        // Post-grant cursor snapshot. Any nseq committed *before* this
        // call may be present in the replay below; any committed
        // *after* it is on the live side of the split and is eligible
        // for live delivery via the broker subscription established by
        // the grant call above (best-effort — see the dispatcher
        // module's "Publish failure handling" section).
        let observed = match store.list(&spec.name).await {
            Ok(snap) => snap.observed_nseq,
            Err(e) => return store_error_to_response(e),
        };
        match store.events_since(&spec.name, since_nseq).await {
            Ok(events) => {
                // Trim to nseq ≤ observed so the client's
                // `observed_nseq` cursor is a faithful upper bound:
                // events past `observed` belong to live delivery and
                // would otherwise duplicate on the client's next watch
                // call (which uses observed_nseq as since_nseq).
                let projected: Vec<Json> = events
                    .iter()
                    .filter(|e| e.nseq <= observed)
                    .map(|e| project(&self.service, &spec.schema, e))
                    .collect();
                // SPEC 12 §5.5 v0.2.1 (2026-05-21): the structured-mode
                // `<svc>.props.watch` reply embeds the `caught_up`
                // control marker so a list-then-watch consumer can
                // close its readiness gate on the replay→live boundary.
                // `nseq` is the server-observed namespace high-water
                // mark at watch-install time — never the caller's
                // `since_nseq`, even when the caller passes
                // `since_nseq >= observed` (the marker still reports
                // `observed`, never above). `audit.watch` (§10) is
                // unamended and omits the marker.
                //
                // On current single-response Bus transport the "reply
                // stream" of §5.5 is realised as this envelope: replay
                // in `events`, then the `caught_up` field, then live
                // events on the granted `<svc>.props.records.changed`
                // subscription. Adapters realising the logical
                // [`crate::WatchStream`] equivalent (e.g. the
                // cross-mesh authz subscriber's production `WgdClient`)
                // walk `events`, emit `CaughtUp`, then drop any live
                // record with `nseq <= caught_up.nseq` to honour the
                // §5.5 "live event ≤ N reaches subscriber before the
                // marker" guarantee — the broker subscription is live
                // *before* the cursor is snapshotted, so a live event
                // may have already been forwarded.
                let mut body = json!({
                    "namespace": ns,
                    "observed_nseq": observed.0,
                    "events": projected,
                    "live": live,
                });
                if emit_caught_up {
                    body["caught_up"] = json!({
                        "event_type": "caught_up",
                        "namespace": ns,
                        "nseq": observed.0,
                    });
                }
                ok(body)
            }
            Err(e) => store_error_to_response(e),
        }
    }
}

// ---------------- helpers ----------------

/// §9: `validation_error` when a structured verb receives the flat-path
/// `path:` header alongside `namespace:`. The two addressing modes are
/// mutually exclusive; we refuse early so the rest of the handler can
/// assume structured mode.
fn refuse_flat_path(req: &BusMessage) -> Result<(), PropsResponse> {
    if req.get("path").is_some() {
        return Err(err(
            ErrorCode::ValidationError,
            "mixed addressing: `path` header conflicts with `namespace` (SPEC 12 §9)",
        ));
    }
    Ok(())
}

/// `field_path` is reserved for the post-v0.1.2 sub-record surface. Any
/// appearance — empty or not — must be refused so callers don't silently
/// get whole-record semantics from a request that asked for a slice.
fn refuse_subpath_selectors(req: &BusMessage) -> Result<(), PropsResponse> {
    if req.get("field_path").is_some() {
        return Err(err(
            ErrorCode::ValidationError,
            "field_path_deferred: sub-record addressing is not implemented in v0.1.2",
        ));
    }
    Ok(())
}

/// `get` accepts a JSON body in later revisions (e.g. `{"fields": [...]}`
/// projection). v0.1.2 does not implement projection; refuse any
/// non-empty body so the caller doesn't silently receive a whole-record
/// envelope when they asked for a slice.
fn refuse_get_body(req: &BusMessage) -> Result<(), PropsResponse> {
    if !req.body.is_empty() {
        return Err(err(
            ErrorCode::ValidationError,
            "get_projection_deferred: body-driven field projection is not implemented in v0.1.2",
        ));
    }
    Ok(())
}

/// Pagination, ordering, and per-record projection are deferred. The
/// list verb currently returns every record under the namespace; rather
/// than silently ignoring the caller's intent we refuse the headers and
/// body fields that would otherwise change shape.
fn refuse_unsupported_list_options(req: &BusMessage) -> Result<(), PropsResponse> {
    for hdr in ["cursor", "limit", "order_by", "filter", "fields"] {
        if req.get(hdr).is_some() {
            return Err(err(
                ErrorCode::ValidationError,
                format!("list_option_deferred: `{hdr}` is not implemented in v0.1.2"),
            ));
        }
    }
    if !req.body.is_empty() {
        return Err(err(
            ErrorCode::ValidationError,
            "list_option_deferred: request body (filter/fields) is not implemented in v0.1.2",
        ));
    }
    Ok(())
}

fn cap(spec: &NamespaceSpec, service: &str, action: &str, scope: Option<&str>) -> Capability {
    let fqn = spec.fqn(service);
    let s = match scope {
        Some(scope) => format!("props.{action}:{fqn}:{scope}"),
        None => format!("props.{action}:{fqn}"),
    };
    Capability::new(s).expect("non-empty capability")
}

fn require_cap(
    spec: &NamespaceSpec,
    service: &str,
    peer: &PeerIdentity,
    action: &str,
    scope: Option<&str>,
) -> Result<CapabilitySet, PropsResponse> {
    let caps = spec.auth.resolve(peer);
    if caps.contains(&cap(spec, service, action, scope)) {
        Ok(caps)
    } else {
        Err(err(ErrorCode::AuthDenied, "missing capability"))
    }
}

fn build_record_key(
    spec: &NamespaceSpec,
    key_hdr: Option<&str>,
) -> Result<RecordKey, PropsResponse> {
    match &spec.cardinality {
        Cardinality::Singleton { .. } => match key_hdr {
            None | Some("") => Ok(RecordKey::singleton(spec.name.clone())),
            Some(_) => Err(err(
                ErrorCode::ValidationError,
                "singleton namespace: key header must be empty",
            )),
        },
        Cardinality::Collection { .. } => match key_hdr {
            Some(k) if !k.is_empty() => Ok(RecordKey::collection(spec.name.clone(), k)),
            _ => Err(err(
                ErrorCode::ValidationError,
                "collection namespace: missing required header `key`",
            )),
        },
    }
}

fn parse_if_version(hdr: Option<&str>) -> Result<Option<Version>, PropsResponse> {
    match hdr {
        None => Ok(None),
        Some(s) => s.parse::<u64>().map(|n| Some(Version(n))).map_err(|_| {
            err(
                ErrorCode::ValidationError,
                format!("invalid if_version: {s:?}"),
            )
        }),
    }
}

fn parse_since_nseq(hdr: Option<&str>) -> Result<Nseq, PropsResponse> {
    match hdr {
        None => Ok(Nseq::zero()),
        Some(s) => s.parse::<u64>().map(Nseq).map_err(|_| {
            err(
                ErrorCode::ValidationError,
                format!("invalid since_nseq: {s:?}"),
            )
        }),
    }
}

fn parse_merge(hdr: Option<&str>) -> Result<MergeMode, PropsResponse> {
    // §5.3 spec table: `true` => Patch, `false` => Replace, default true.
    match hdr {
        None => Ok(MergeMode::Patch),
        Some("true") | Some("patch") => Ok(MergeMode::Patch),
        Some("false") | Some("replace") => Ok(MergeMode::Replace),
        Some(other) => Err(err(
            ErrorCode::ValidationError,
            format!("invalid merge: {other:?} (expected true|false|patch|replace)"),
        )),
    }
}

fn parse_set_body(body: &str) -> Result<PropValue, PropsResponse> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(err(
            ErrorCode::ValidationError,
            "set requires a JSON object body",
        ));
    }
    let v: PropValue = serde_json::from_str(trimmed).map_err(|e| {
        err(
            ErrorCode::ValidationError,
            format!("invalid JSON body: {e}"),
        )
    })?;
    if !matches!(v, PropValue::Object(_)) {
        return Err(err(
            ErrorCode::ValidationError,
            "set body must be a JSON object",
        ));
    }
    Ok(v)
}

/// SPEC 07 §3.5.1 — derive the actor variant from the peer identity.
/// Service peers register a `service_name`; the broker is the source of
/// truth for that field per §7.3. Human operators carry `unix_uid` and
/// are surfaced as `operator:uid-<n>` until a SPEC 10 signed principal
/// arrives. Anonymous peers (neither service nor unix) fall back to the
/// owning service token — this is benign because the
/// capability check has already happened by the time we reach here, so
/// only authorised callers ever materialise an event row.
fn peer_actor(
    peer: &PeerIdentity,
    fallback_service: &str,
) -> Result<Actor, crate::record::ActorError> {
    if let Some(svc) = peer.service_name.as_deref() {
        return Actor::service(svc);
    }
    if let Some(principal) = peer.signed_ident.as_deref() {
        return Actor::operator(principal);
    }
    if let Some(uid) = peer.unix_uid {
        return Actor::operator(format!("uid-{uid}"));
    }
    // SPEC 07 §3.5.1 service form — used by tests where the peer is
    // synthesised in-process without a broker-attached identity.
    Actor::service(fallback_service)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------- response projections ----------------

fn project_set(spec: &NamespaceSpec, ns: &str, key: &RecordKey, out: &SetOutcome) -> Json {
    let created = out.set_event.kind == EventKind::Created;
    match (out.complete_event.as_ref(), spec.lifecycle) {
        (Some(complete), crate::namespace::NamespaceLifecycle::Saga) => {
            let lifecycle = complete
                .lifecycle
                .as_ref()
                .map(Lifecycle::as_state_str)
                .unwrap_or("active");
            let reason = complete
                .lifecycle
                .as_ref()
                .and_then(Lifecycle::reason)
                .map(|s| s.to_string());
            let mut body = json!({
                "namespace": ns,
                "key": key.key,
                "set_version": out.set_event.version.0,
                "set_nseq": out.set_event.nseq.0,
                "complete_version": complete.version.0,
                "complete_nseq": complete.nseq.0,
                "lifecycle": lifecycle,
                "created": created,
            });
            if let (Some(reason), Json::Object(o)) = (reason, &mut body) {
                o.insert("reason".to_string(), Json::String(reason));
            }
            body
        }
        _ => {
            let mut body = json!({
                "namespace": ns,
                "key": key.key,
                "version": out.set_event.version.0,
                "nseq": out.set_event.nseq.0,
                "created": created,
            });
            if !out.warnings.is_empty()
                && let Json::Object(o) = &mut body
            {
                let arr: Vec<Json> = out.warnings.iter().cloned().map(Json::String).collect();
                o.insert("warnings".to_string(), Json::Array(arr));
            }
            body
        }
    }
}

fn project_record_envelope(
    ns: &str,
    record: &Record,
    observed_nseq: Nseq,
    schema: &PropertySchema,
    with_secrets: bool,
) -> Json {
    json!({
        "namespace": ns,
        "key": record.key.key,
        "version": record.version.0,
        "nseq": observed_nseq.0,
        "fields": project_fields(&record.value, schema, with_secrets),
    })
}

fn project_record_summary(record: &Record, schema: &PropertySchema, with_secrets: bool) -> Json {
    json!({
        "key": record.key.key,
        "version": record.version.0,
        "fields": project_fields(&record.value, schema, with_secrets),
    })
}

/// SPEC 12 §8.3 — record envelope's `fields`. When `with_secrets` is
/// false, fields declared `secret: true` in the schema are emitted as
/// `null` (the §8.3 wire example pins this shape for password_hash:
/// "secret, redacted"). When true, raw values pass through.
///
/// **Stored bytes are projected verbatim** — `project_fields` does NOT
/// fill schema defaults for absent fields. This is load-bearing for
/// SPEC 12 §10 audit-digest reproducibility: the commit-time digest is
/// computed over the canonical serialisation of the *stored* value, and
/// a verifier re-derives it via `<svc>.props.get` + the namespace audit
/// key (`audit.rs` line 67). Filling defaults on the read path would
/// break that re-derivation for any pre-amendment row whose schema has
/// since gained `since:` fields with defaults — the verifier would
/// canonicalise a 12-field projection against an audit row whose
/// digest was committed over a 9-field stored value.
///
/// Per-namespace read-default is therefore the typed read helper's
/// responsibility, not the substrate's. See
/// `cosmix-maild/src/props/engine_config.rs::record_to_engine_config`
/// for the engine_config v0.2.0 amendment's reference implementation:
/// the engine consumer fills `EngineConfig::default()` values for
/// absent amendment fields, while the substrate's `props.get` returns
/// the literal on-disk shape (sparse for pre-amendment rows).
///
/// Densification of stored rows is *not* automatic. `before_set`
/// validators return `Result<(), Error>` — the substrate commits the
/// raw client value, not a transformed one (see
/// `runtime.rs::commit_set` and `sqlite.rs`'s `merge_patch_object`).
/// Concretely: a Replace materially densifies because strict Replace
/// requires the full field-set; a Patch only densifies the fields the
/// patch explicitly carries. A Patch that touches only legacy fields
/// leaves the row sparse, and `props.get` continues to return the
/// literal sparse shape — the typed read helper supplies the defaults
/// for engine consumers. SPEC 12 §12 ("read as if the default were
/// present") is satisfied at the typed-consumer boundary, not at the
/// substrate wire boundary, by design: substrate-wide default-fill
/// would break the audit-digest contract above.
fn project_fields(value: &PropValue, schema: &PropertySchema, with_secrets: bool) -> Json {
    let mut out = Json::from(value);
    if with_secrets {
        return out;
    }
    if let Json::Object(map) = &mut out {
        for name in schema.secret_field_names() {
            if map.contains_key(name) {
                map.insert(name.to_string(), Json::Null);
            }
        }
    }
    out
}

/// Project the records.changed wire body (§5.5) from a stored
/// [`RecordEvent`]. Used by the `watch` verb's replay slice (C4) and
/// by the live fan-out publisher in [`crate::dispatcher`] (C9) so the
/// replay shape and the live-publish shape are byte-identical at the
/// projection seam — with the watch-mediated subscriber bridge
/// (C10c/C10d) in place, a client resuming from a stale `since_nseq`
/// cannot distinguish events delivered live from events delivered via
/// replay.
///
/// Discrepancies from the raw event row:
/// - `verb` is derived from `kind` + service (already in the row, but
///   we re-derive here so replay matches the live shape even when the
///   row's `verb` is the storage column written at commit time).
/// - `ts` (renamed from the row's `at` field per §6.6 → §5.5 mapping).
/// - `lifecycle` flat-projects `{state,reason?}` and is emitted only
///   on `Completed` events from Saga namespaces (§5.5 line 601).
/// - `secret_fields_changed_count` is emitted only when non-zero.
/// - `audit_digest` is intentionally absent — §5.5 the records.changed
///   topic is gated on `props.read`, separately from the
///   `<svc>.props.audit` topic which is gated on the stronger
///   `props.audit` capability.
pub(crate) fn project_records_changed(
    service: &str,
    schema: &PropertySchema,
    event: &RecordEvent,
) -> Json {
    let mut body = json!({
        "namespace": event.key.namespace.as_str(),
        "key": event.key.key,
        "kind": serde_kind(event.kind),
        "verb": event.kind.verb_for(service),
        "nseq": event.nseq.0,
        "version": event.version.0,
        "audit_epoch": event.audit_epoch.0,
        "actor": event.actor.as_str(),
        "ts": event.at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    if let Json::Object(map) = &mut body {
        // §5.5 — `fields_changed` is non-secret names only; the
        // storage row already filters secret names out, but redact
        // defensively here in case the row was written by an older
        // backend.
        let visible: Vec<Json> = event
            .fields_changed
            .iter()
            .filter(|f| !schema.fields.iter().any(|s| s.name == **f && s.secret))
            .cloned()
            .map(Json::String)
            .collect();
        if !visible.is_empty() {
            map.insert("fields_changed".to_string(), Json::Array(visible));
        }
        if event.secret_fields_changed_count > 0 {
            map.insert(
                "secret_fields_changed_count".to_string(),
                Json::Number(event.secret_fields_changed_count.into()),
            );
        }
        if event.kind == EventKind::Completed
            && let Some(lc) = &event.lifecycle
        {
            map.insert(
                "lifecycle".to_string(),
                Json::String(lc.as_state_str().to_string()),
            );
            if let Some(reason) = lc.reason() {
                map.insert("reason".to_string(), Json::String(reason.to_string()));
            }
        }
        if let Some(cause) = &event.cause {
            map.insert("cause".to_string(), Json::String(cause.clone()));
        }
    }
    body
}

/// SPEC 12 §10 — flat audit-row projection. Distinct from
/// [`project_records_changed`] in three ways:
///  - timestamp field is `at`, not `ts` (§10);
///  - `audit_digest` is present (it is the whole point of the audit
///    stream — §10);
///  - the `kind`/`verb`/`fields_changed` columns are present, but
///    `secret_fields_changed_count`, `lifecycle`, and `cause` are
///    NOT — audit rows are intentionally lean and reproducible from
///    a `<svc>.props.get` snapshot (§10).
///
/// `fields_changed` is filtered against the namespace schema so secret
/// field names never appear in the audit stream — the storage backend
/// stores names verbatim (it has no schema handle), and the spec
/// names this projection as the boundary that redacts them (§10:
/// "secret field names MUST NOT appear in audit rows").
///
/// Visibility: `pub(crate)` so the C9 live fan-out publisher in
/// [`crate::dispatcher`] can reuse the same projection helper as the
/// `audit.watch` verb's replay slice (C5). Replay and live-publish
/// shapes must remain byte-identical at the projection seam — with
/// the watch-mediated subscriber bridge (C10c/C10d) in place, a
/// client resuming from a stale `since_nseq` cannot distinguish rows
/// delivered live from rows delivered via replay.
pub(crate) fn project_audit_row(
    service: &str,
    schema: &PropertySchema,
    event: &RecordEvent,
) -> Json {
    let mut body = json!({
        "namespace": event.key.namespace.as_str(),
        "key": event.key.key,
        "verb": event.kind.verb_for(service),
        "version": event.version.0,
        "nseq": event.nseq.0,
        "audit_epoch": event.audit_epoch.0,
        "actor": event.actor.as_str(),
        "at": event.at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "audit_digest": event.audit_digest,
    });
    if let Json::Object(map) = &mut body {
        let visible: Vec<Json> = event
            .fields_changed
            .iter()
            .filter(|f| !schema.fields.iter().any(|s| s.name == **f && s.secret))
            .cloned()
            .map(Json::String)
            .collect();
        if !visible.is_empty() {
            map.insert("fields_changed".to_string(), Json::Array(visible));
        }
    }
    body
}

fn serde_kind(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Created => "created",
        EventKind::Updated => "updated",
        EventKind::Deleted => "deleted",
        EventKind::Completed => "completed",
        EventKind::Reconciled => "reconciled",
    }
}

// ---------------- describe projection ----------------

fn project_schema(spec: &NamespaceSpec, ns: &str, full: bool) -> Json {
    let cardinality_token = match &spec.cardinality {
        Cardinality::Singleton { .. } => "singleton",
        Cardinality::Collection { .. } => "collection",
    };
    let mut body = json!({
        "namespace": ns,
        "schema_version": spec.since_version.to_string(),
        "cardinality": cardinality_token,
        "fields": project_field_list(&spec.schema, &spec.validators, full),
    });
    if let (Cardinality::Collection { primary_key_field }, Json::Object(map)) =
        (&spec.cardinality, &mut body)
    {
        map.insert(
            "primary_key_field".to_string(),
            Json::String(primary_key_field.clone()),
        );
    }
    body
}

#[derive(Serialize)]
struct FieldWire<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    ty: Json,
    secret: bool,
    default: Json,
    validators: Vec<String>,
    help: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<String>,
}

fn project_field_list(schema: &PropertySchema, validators: &[Validator], full: bool) -> Json {
    let fields: Vec<Json> = schema
        .fields
        .iter()
        .filter(|f| full || !f.secret)
        .map(|f| {
            let validator_tokens = project_field_validators(f, validators, full);
            let wire = FieldWire {
                name: &f.name,
                ty: serde_json::to_value(&f.ty).unwrap_or(Json::Null),
                secret: f.secret,
                default: f.default.as_ref().map(Json::from).unwrap_or(Json::Null),
                validators: validator_tokens,
                help: &f.help,
                since: f.since.as_ref().map(ToString::to_string),
                until: f.until.as_ref().map(ToString::to_string),
            };
            serde_json::to_value(&wire).unwrap_or(Json::Null)
        })
        .collect();
    Json::Array(fields)
}

/// Per-field validator token list. v0.1 namespace validators are
/// declared globally on the spec (not per-field), so this returns the
/// global token list verbatim in `full` view and replaces
/// `validator_secret` names with `"<redacted>"` in `public` view. The
/// per-field-vs-global validator distinction will harden in v0.2 when
/// `#[derive(Property)]` arrives; for now we expose what we have.
fn project_field_validators(
    _field: &FieldSchema,
    validators: &[Validator],
    full: bool,
) -> Vec<String> {
    validators
        .iter()
        .map(|v| {
            if !full && v.validator_secret {
                "<redacted>".to_string()
            } else {
                v.name.to_string()
            }
        })
        .collect()
}

// ---------------- error mapping ----------------

fn runtime_error_to_response(e: RuntimeError) -> PropsResponse {
    match e {
        RuntimeError::Hook(h) => hook_error_to_response(h),
        RuntimeError::Store(s) => store_error_to_response(s),
    }
}

fn hook_error_to_response(e: crate::hooks::HookError) -> PropsResponse {
    use crate::hooks::HookError;
    match e {
        HookError::Validation { message } => err(ErrorCode::ValidationError, message),
        HookError::Hook { message } => err(ErrorCode::HookError, message),
    }
}

fn store_error_to_response(e: StoreError) -> PropsResponse {
    match e {
        StoreError::NotFound => err(ErrorCode::NotFound, "not_found"),
        StoreError::VersionMismatch { expected, current } => err_with(
            ErrorCode::VersionMismatch,
            format!(
                "if_version mismatch: expected {}, current {}",
                expected.0, current.0
            ),
            json!({
                "expected_version": expected.0,
                "current_version": current.0,
            }),
        ),
        StoreError::Storage { message } => err(ErrorCode::StorageError, message),
        StoreError::Validation { message } => err(ErrorCode::ValidationError, message),
        StoreError::Conflict { message } => err(ErrorCode::Conflict, message),
        StoreError::ReplayWindowExceeded {
            since_nseq,
            oldest_nseq,
        } => err_with(
            ErrorCode::ReplayWindowExceeded,
            format!(
                "since_nseq {} outside replay window (oldest {})",
                since_nseq.0, oldest_nseq.0
            ),
            json!({
                "since_nseq": since_nseq.0,
                "oldest_nseq": oldest_nseq.0,
            }),
        ),
    }
}

// `unavailable` is the dispatcher's responsibility (router doesn't see
// "service shutting down" — that's a broker concern). The token is in
// the vocabulary so daemons can surface it through `err_with` if they
// need to, but the property substrate itself never emits it.
#[allow(dead_code)]
fn unavailable() -> PropsResponse {
    err(ErrorCode::Unavailable, "service unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::Hooks;
    use crate::memory::MemoryStore;
    use crate::namespace::{
        AuthPolicy, Cardinality, FieldType, NamespaceLifecycle, NamespaceName, NamespaceSpec,
        PropertySchema, StorageBackendKind,
    };
    use crate::store::PropertyStore;

    fn ns_name() -> NamespaceName {
        NamespaceName::new("accounts").unwrap()
    }

    /// Permissive AuthPolicy fixture: grant every capability shape the
    /// `accounts` namespace exposes. Used so individual test cases don't
    /// have to enumerate cap strings.
    fn allow_all_resolve(_p: &PeerIdentity) -> CapabilitySet {
        let strings = [
            "props.read:maild.accounts",
            "props.read:maild.accounts:secrets",
            "props.write:maild.accounts",
            "props.describe:maild.accounts:public",
            "props.describe:maild.accounts:full",
            "props.audit:maild.accounts",
        ];
        strings
            .iter()
            .map(|s| Capability::new(*s).expect("non-empty capability"))
            .collect::<CapabilitySet>()
    }

    /// Capability-issuing fixture parameterised on a fixed set.
    fn caps_from(list: &[&str]) -> CapabilitySet {
        list.iter()
            .map(|s| Capability::new(*s).expect("non-empty capability"))
            .collect()
    }

    fn spec_simple() -> NamespaceSpec {
        let mut schema = PropertySchema::default();
        schema.fields.push(FieldSchema {
            name: "email".into(),
            ty: FieldType::String,
            secret: false,
            default: None,
            help: "email address".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        });
        schema.fields.push(FieldSchema {
            name: "password_hash".into(),
            ty: FieldType::String,
            secret: true,
            default: None,
            help: "argon2 hash".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        });
        let mut spec = NamespaceSpec::new(
            ns_name(),
            schema,
            Cardinality::Collection {
                primary_key_field: "email".into(),
            },
            StorageBackendKind::Memory,
        );
        spec.auth = AuthPolicy::new(allow_all_resolve);
        spec.hooks = Hooks::noop();
        spec
    }

    fn router_with(spec: NamespaceSpec) -> (PropsRouter, Arc<dyn PropertyStore>) {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Arc::new(Runtime::new("maild", spec, Arc::clone(&store)));
        let mut router = PropsRouter::new("maild");
        // Default test fixture is replay-only; tests that exercise the
        // granter path call `set_granter` after construction (which
        // takes precedence). Tests that exercise the production refusal
        // path (no granter + no opt-in) build `PropsRouter::new` by hand.
        router.allow_replay_only_for_tests();
        router.register(rt).unwrap();
        (router, store)
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            unix_uid: Some(1000),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn malformed_peer_actor_refused_before_mutation() {
        let (router, store) = router_with(spec_simple());
        for peer in [
            PeerIdentity {
                service_name: Some("bad service".into()),
                ..Default::default()
            },
            PeerIdentity {
                signed_ident: Some(String::new()),
                ..Default::default()
            },
        ] {
            for verb in ["set", "delete"] {
                let response = router
                    .dispatch(
                        verb,
                        &req(
                            &[("namespace", "accounts"), ("key", "alice@example.org")],
                            r#"{"email":"alice@example.org"}"#,
                        ),
                        &peer,
                    )
                    .await;
                assert_eq!(body_json(&response)["error_code"], "validation_error");
                assert!(
                    body_json(&response)["message"]
                        .as_str()
                        .unwrap()
                        .contains("actor")
                );
            }
        }
        assert!(
            store
                .events_since(&ns_name(), Nseq::zero())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn peer_actor_preserves_existing_identity_forms() {
        let cases = [
            (
                PeerIdentity {
                    service_name: Some("maild.dkim".into()),
                    ..Default::default()
                },
                "maild.dkim",
            ),
            (
                PeerIdentity {
                    signed_ident: Some("mesh:node.example".into()),
                    ..Default::default()
                },
                "operator:mesh:node.example",
            ),
            (peer(), "operator:uid-1000"),
            (PeerIdentity::default(), "maild"),
        ];
        for (peer, expected) in cases {
            assert_eq!(peer_actor(&peer, "maild").unwrap().as_str(), expected);
        }
    }

    fn req(headers: &[(&str, &str)], body: &str) -> BusMessage {
        let mut m = BusMessage::new();
        for (k, v) in headers {
            m.set(k, v);
        }
        m.body = body.to_string();
        m
    }

    fn body_json(r: &PropsResponse) -> Json {
        serde_json::from_str(&r.body).expect("response body is JSON")
    }

    #[tokio::test]
    async fn set_creates_record_in_simple_ns() {
        let (router, store) = router_with(spec_simple());
        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["namespace"], "accounts");
        assert_eq!(body["key"], "u@h");
        assert_eq!(body["version"], 1);
        assert_eq!(body["created"], true);

        let key = RecordKey::collection(ns_name(), "u@h");
        let snap = store.get(&key).await.unwrap();
        assert_eq!(snap.value.version, Version(1));
    }

    #[tokio::test]
    async fn set_missing_namespace_validation_error() {
        let (router, _) = router_with(spec_simple());
        let r = router.dispatch("set", &req(&[], r#"{}"#), &peer()).await;
        assert_eq!(r.rc, 10);
        let body = body_json(&r);
        assert_eq!(body["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn set_unknown_namespace_not_found() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "set",
                &req(&[("namespace", "themes"), ("key", "x")], r#"{}"#),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "not_found");
    }

    #[tokio::test]
    async fn set_without_capability_auth_denied() {
        // Build a runtime whose policy grants only read, no write.
        let mut spec = spec_simple();
        fn read_only(_p: &PeerIdentity) -> CapabilitySet {
            caps_from(&["props.read:maild.accounts"])
        }
        spec.auth = AuthPolicy::new(read_only);
        let (router, _) = router_with(spec);

        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "auth_denied");
    }

    #[tokio::test]
    async fn set_flat_path_mutation_refused() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch("set", &req(&[("path", "config.bind")], r#"{}"#), &peer())
            .await;
        assert_eq!(r.rc, 10);
        let body = body_json(&r);
        assert_eq!(body["error_code"], "validation_error");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("flat_path_mutation_deferred"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn set_field_path_refused() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("field_path", "email"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        let body = body_json(&r);
        assert_eq!(body["error_code"], "validation_error");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("field_path_deferred"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn delete_refuses_flat_path_and_field_path() {
        let (router, _) = router_with(spec_simple());
        // Seed a record so we'd otherwise reach the runtime.
        router
            .dispatch(
                "set",
                &req(
                    &[("namespace", "accounts"), ("key", "u@h")],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        // Mixed-mode delete is refused.
        let r = router
            .dispatch(
                "delete",
                &req(
                    &[("namespace", "accounts"), ("path", "x.y"), ("key", "u@h")],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
        // Sub-path delete is refused.
        let r = router
            .dispatch(
                "delete",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("field_path", "email"),
                    ],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("field_path_deferred")
        );
    }

    #[tokio::test]
    async fn empty_field_path_header_still_refused() {
        // Presence of `field_path:` — even empty — must be refused so
        // callers don't read whole-record semantics from a slice-shaped
        // request.
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "get",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("field_path", ""),
                    ],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("field_path_deferred")
        );
    }

    #[tokio::test]
    async fn get_refuses_non_empty_body() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "get",
                &req(
                    &[("namespace", "accounts"), ("key", "u@h")],
                    r#"{"fields":["email"]}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("get_projection_deferred")
        );
    }

    #[tokio::test]
    async fn get_refuses_mixed_addressing_and_field_path() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "get",
                &req(
                    &[("namespace", "accounts"), ("path", "foo"), ("key", "u@h")],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("mixed addressing")
        );

        let r = router
            .dispatch(
                "get",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("field_path", "email"),
                    ],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("field_path_deferred")
        );
    }

    #[tokio::test]
    async fn list_refuses_pagination_and_projection_options() {
        let (router, _) = router_with(spec_simple());
        for hdr in ["cursor", "limit", "order_by", "filter", "fields"] {
            let r = router
                .dispatch(
                    "list",
                    &req(&[("namespace", "accounts"), (hdr, "x")], ""),
                    &peer(),
                )
                .await;
            assert_eq!(r.rc, 10, "header {hdr} should be refused");
            assert!(
                body_json(&r)["message"]
                    .as_str()
                    .unwrap()
                    .contains("list_option_deferred"),
                "header {hdr}: got {}",
                r.body
            );
        }
        // Non-empty body is also refused.
        let r = router
            .dispatch(
                "list",
                &req(&[("namespace", "accounts")], r#"{"filter":{"a":1}}"#),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert!(
            body_json(&r)["message"]
                .as_str()
                .unwrap()
                .contains("list_option_deferred")
        );
    }

    #[tokio::test]
    async fn describe_refuses_mixed_addressing() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "describe",
                &req(&[("namespace", "accounts"), ("path", "foo")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn watch_refuses_mixed_addressing() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("path", "foo")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn error_envelope_carries_message_in_bus_error_field() {
        // §9 wire: Bus `error:` header is the human-readable message,
        // body carries `error_code` as the machine-readable token.
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch("set", &req(&[("path", "x")], "{}"), &peer())
            .await;
        let body = body_json(&r);
        assert_eq!(body["error_code"], "validation_error");
        assert_eq!(
            r.error.as_deref(),
            body["message"].as_str(),
            "PropsResponse.error must mirror body.message (the human-readable description)"
        );
    }

    #[tokio::test]
    async fn set_version_mismatch_carries_current_version() {
        let (router, _) = router_with(spec_simple());
        // Seed.
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        // Retry with stale if_version.
        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h","display":"Alice"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        let body = body_json(&r);
        assert_eq!(body["error_code"], "version_mismatch");
        assert_eq!(body["expected_version"], 0);
        assert_eq!(body["current_version"], 1);
    }

    #[tokio::test]
    async fn set_saga_response_carries_complete_pair() {
        let mut spec = spec_simple();
        spec.lifecycle = NamespaceLifecycle::Saga;
        let (router, _) = router_with(spec);

        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["set_version"], 1);
        assert_eq!(body["complete_version"], 2);
        assert_eq!(body["lifecycle"], "active");
        assert_eq!(body["created"], true);
        // Reason absent on success.
        assert!(body.get("reason").is_none());
    }

    #[tokio::test]
    async fn delete_returns_tombstone_version() {
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "delete",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "1"),
                    ],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["deleted"], true);
        assert_eq!(body["tombstone_version"], 2);
    }

    #[tokio::test]
    async fn delete_singleton_namespace_refused() {
        let mut spec = spec_simple();
        spec.cardinality = Cardinality::Singleton {
            canonical_key: "".into(),
        };
        let (router, _) = router_with(spec);
        let r = router
            .dispatch("delete", &req(&[("namespace", "accounts")], ""), &peer())
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn get_redacts_secret_fields_without_secrets_cap() {
        let mut spec = spec_simple();
        // Restrict to plain read (no `:secrets`).
        fn read_only(_p: &PeerIdentity) -> CapabilitySet {
            caps_from(&["props.read:maild.accounts", "props.write:maild.accounts"])
        }
        spec.auth = AuthPolicy::new(read_only);
        let (router, _) = router_with(spec);

        // Seed a record carrying both visible and secret fields.
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h","password_hash":"argon2:$x$"}"#,
                ),
                &peer(),
            )
            .await;

        let r = router
            .dispatch(
                "get",
                &req(&[("namespace", "accounts"), ("key", "u@h")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["fields"]["email"], "u@h");
        assert_eq!(body["fields"]["password_hash"], Json::Null);
    }

    #[tokio::test]
    async fn get_with_secrets_cap_emits_secret_values() {
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h","password_hash":"argon2:$x$"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "get",
                &req(&[("namespace", "accounts"), ("key", "u@h")], ""),
                &peer(),
            )
            .await;
        let body = body_json(&r);
        assert_eq!(body["fields"]["password_hash"], "argon2:$x$");
    }

    #[tokio::test]
    async fn get_not_found_maps_to_not_found_code() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "get",
                &req(&[("namespace", "accounts"), ("key", "nope")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "not_found");
    }

    #[tokio::test]
    async fn list_returns_records_with_observed_nseq() {
        let (router, _) = router_with(spec_simple());
        for k in ["a@h", "b@h"] {
            router
                .dispatch(
                    "set",
                    &req(
                        &[("namespace", "accounts"), ("key", k), ("if_version", "0")],
                        &format!(r#"{{"email":"{k}"}}"#),
                    ),
                    &peer(),
                )
                .await;
        }
        let r = router
            .dispatch("list", &req(&[("namespace", "accounts")], ""), &peer())
            .await;
        assert_eq!(r.rc, 0);
        let body = body_json(&r);
        assert_eq!(body["nseq"], 2);
        let records = body["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .all(|r| r["fields"]["password_hash"].is_null()
                    || r["fields"]["password_hash"] == Json::Null)
        );
    }

    #[tokio::test]
    async fn describe_public_omits_secret_fields() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "describe",
                &req(&[("namespace", "accounts"), ("view", "public")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        let fields = body["fields"].as_array().unwrap();
        let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"email"));
        assert!(!names.contains(&"password_hash"));
        assert_eq!(body["cardinality"], "collection");
        assert_eq!(body["primary_key_field"], "email");
    }

    #[tokio::test]
    async fn describe_full_includes_secret_fields() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "describe",
                &req(&[("namespace", "accounts"), ("view", "full")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0);
        let body = body_json(&r);
        let names: Vec<&str> = body["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"password_hash"));
    }

    #[tokio::test]
    async fn describe_public_denied_when_schema_public_deny() {
        let mut spec = spec_simple();
        spec.schema_public = SchemaPublic::Deny;
        let (router, _) = router_with(spec);
        let r = router
            .dispatch(
                "describe",
                &req(&[("namespace", "accounts"), ("view", "public")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "auth_denied");
    }

    #[tokio::test]
    async fn watch_replay_returns_events_since_cursor() {
        let (router, _) = router_with(spec_simple());
        for k in ["a@h", "b@h", "c@h"] {
            router
                .dispatch(
                    "set",
                    &req(
                        &[("namespace", "accounts"), ("key", k), ("if_version", "0")],
                        &format!(r#"{{"email":"{k}"}}"#),
                    ),
                    &peer(),
                )
                .await;
        }
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "1")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["observed_nseq"], 3);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "should replay events at nseq 2,3 only");
        assert_eq!(events[0]["nseq"], 2);
        assert_eq!(events[0]["verb"], "maild.props.set");
        assert_eq!(events[0]["kind"], "created");
    }

    #[tokio::test]
    async fn watch_without_read_cap_auth_denied() {
        let mut spec = spec_simple();
        fn deny(_p: &PeerIdentity) -> CapabilitySet {
            CapabilitySet::empty()
        }
        spec.auth = AuthPolicy::new(deny);
        let (router, _) = router_with(spec);
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "auth_denied");
    }

    // ---------------- §5.5 v0.2.1 caught_up marker ----------------
    //
    // The watch reply embeds a single `caught_up` control marker so a
    // list-then-watch consumer can close its readiness gate on the
    // replay→live boundary. Body shape is fixed by §5.5:
    //   { "event_type": "caught_up", "namespace": "<ns>", "nseq": <N> }
    // where `N` is the server-observed namespace high-water mark at
    // watch-install time. `audit.watch` (§10) is unamended.

    fn assert_caught_up(body: &serde_json::Value, ns: &str, nseq: u64) {
        let m = body
            .get("caught_up")
            .unwrap_or_else(|| panic!("watch reply missing caught_up: {body}"));
        assert_eq!(m["event_type"], "caught_up");
        assert_eq!(m["namespace"], ns);
        assert_eq!(
            m["nseq"], nseq,
            "caught_up.nseq must equal observed_nseq: {m}"
        );
    }

    #[tokio::test]
    async fn watch_emits_caught_up_with_present_since_nseq_and_replay() {
        let (router, _) = router_with(spec_simple());
        for k in ["a@h", "b@h", "c@h"] {
            router
                .dispatch(
                    "set",
                    &req(
                        &[("namespace", "accounts"), ("key", k), ("if_version", "0")],
                        &format!(r#"{{"email":"{k}"}}"#),
                    ),
                    &peer(),
                )
                .await;
        }
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "1")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["observed_nseq"], 3);
        // `events` stays homogeneous: record-event-shaped rows only,
        // never a caught_up element appended into the array (Codex
        // arbitration: a heterogeneous union would contaminate the
        // replay/live byte-identical invariant).
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        for e in events {
            assert!(
                e.get("event_type").is_none() || e["event_type"].as_str() != Some("caught_up"),
                "caught_up must not appear inside events: {e}"
            );
        }
        assert_caught_up(&body, "accounts", 3);
    }

    #[tokio::test]
    async fn watch_emits_caught_up_when_since_nseq_clamps_to_observed() {
        // Caller passes `since_nseq >= observed_nseq` (live-tail-only
        // intent). §5.5 v0.2.1: `caught_up.nseq` is the server-observed
        // high-water mark, NEVER above it — even though the caller's
        // since is at-or-past it, the marker reports `observed`.
        let (router, _) = router_with(spec_simple());
        for k in ["a@h", "b@h"] {
            router
                .dispatch(
                    "set",
                    &req(
                        &[("namespace", "accounts"), ("key", k), ("if_version", "0")],
                        &format!(r#"{{"email":"{k}"}}"#),
                    ),
                    &peer(),
                )
                .await;
        }
        // observed = 2; pass since_nseq = 5 (well above current).
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "5")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["observed_nseq"], 2);
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
        assert_caught_up(&body, "accounts", 2);
    }

    #[tokio::test]
    async fn watch_emits_caught_up_when_since_nseq_absent_empty_namespace() {
        // No prior commits, no since_nseq header → live-tail join with
        // nothing to replay. §5.5 v0.2.1: the marker still fires once,
        // reporting the current high-water mark (here, 0).
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch("watch", &req(&[("namespace", "accounts")], ""), &peer())
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["observed_nseq"], 0);
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
        assert_caught_up(&body, "accounts", 0);
    }

    #[tokio::test]
    async fn audit_watch_does_not_emit_caught_up() {
        // §5.5 v0.2.1 amends `<svc>.props.watch` only; §10 (audit) was
        // not amended. The audit reply shape is unchanged — no marker.
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert!(
            body.get("caught_up").is_none(),
            "audit.watch must not embed caught_up: {body}"
        );
    }

    #[tokio::test]
    async fn unknown_subcommand_rejected() {
        let (router, _) = router_with(spec_simple());
        let r = router.dispatch("frobnicate", &req(&[], ""), &peer()).await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn register_rejects_runtime_for_different_service() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("themed"));
        let rt = Arc::new(Runtime::new("themed", spec_simple(), Arc::clone(&store)));
        let mut router = PropsRouter::new("maild");
        let err = router.register(rt).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[tokio::test]
    async fn register_rejects_duplicate_namespace() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt1 = Arc::new(Runtime::new("maild", spec_simple(), Arc::clone(&store)));
        let rt2 = Arc::new(Runtime::new("maild", spec_simple(), Arc::clone(&store)));
        let mut router = PropsRouter::new("maild");
        router.register(rt1).unwrap();
        let err = router.register(rt2).unwrap_err();
        assert!(err.contains("already registered"));
    }

    #[tokio::test]
    async fn set_body_must_be_object() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"["not","an","object"]"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn set_invalid_if_version_validation_error() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "not-a-number"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn records_changed_does_not_leak_audit_digest() {
        // §5.5: the passive `records.changed` topic deliberately omits
        // `audit_digest` so anyone with `props.read` can subscribe
        // without gaining the privileged column that gates tamper
        // verification. The `watch` verb is the replay surface for the
        // same projection.
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0);
        let events = body_json(&r)["events"].clone();
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].get("audit_digest").is_none(),
            "records.changed/watch MUST NOT include audit_digest"
        );
    }

    #[tokio::test]
    async fn audit_watch_redacts_secret_field_names() {
        // §10: secret field names MUST NOT appear in audit rows. The
        // storage backend stores `fields_changed` verbatim (it has no
        // schema handle); the projection is the redaction boundary.
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h","password_hash":"$argon2id$..."}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0);
        let events = body_json(&r)["events"].clone();
        let row = &events.as_array().unwrap()[0];
        let names: Vec<String> = row
            .get("fields_changed")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert!(names.contains(&"email".to_string()));
        assert!(
            !names.contains(&"password_hash".to_string()),
            "secret field name must be redacted from audit row, got {names:?}"
        );
    }

    #[tokio::test]
    async fn audit_watch_returns_rows_with_digest() {
        // §10: `<svc>.props.audit.watch` carries the keyed HMAC per row.
        // Distinct from `watch` in three observable ways: timestamp is
        // `at` (not `ts`), `audit_digest` is present, and `kind`/
        // `secret_fields_changed_count` are absent (audit rows are
        // intentionally lean — see project_audit_row).
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        let row = &events[0];
        assert!(row.get("at").is_some(), "audit row uses `at` (SPEC 12 §10)");
        assert!(row.get("ts").is_none(), "audit row must not use `ts`");
        let digest = row["audit_digest"].as_str().unwrap();
        assert_eq!(digest.len(), 64, "hex-encoded HMAC-SHA256 is 64 chars");
        assert_eq!(row["verb"], "maild.props.set");
        assert_eq!(row["version"], 1);
        assert_eq!(row["nseq"], 1);
    }

    #[tokio::test]
    async fn audit_watch_requires_audit_capability_not_read() {
        // §7.1: `props.audit:<fqn>` is its own capability. A peer with
        // `props.read` MUST be refused; only `props.audit` (or `*`) lets
        // them subscribe to the keyed-HMAC stream.
        let mut spec = spec_simple();
        fn read_only(_p: &PeerIdentity) -> CapabilitySet {
            ["props.read:maild.accounts"]
                .iter()
                .map(|s| Capability::new(*s).expect("non-empty capability"))
                .collect()
        }
        spec.auth = AuthPolicy::new(read_only);
        let (router, _) = router_with(spec);
        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "auth_denied");
    }

    #[tokio::test]
    async fn audit_watch_refuses_mixed_addressing() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "audit.watch",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("path", "/some/flat/path"),
                        ("since_nseq", "0"),
                    ],
                    "",
                ),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "validation_error");
    }

    #[tokio::test]
    async fn audit_digest_reproducible_from_get_snapshot() {
        // §10 verifier protocol: a daemon holding the namespace audit
        // key can re-derive the row's digest from a `<svc>.props.get`
        // response. This regression-guards the canonical-serialisation
        // contract between commit-time digest computation and the
        // post-write record state observable via get.
        use crate::audit::compute_digest;
        use crate::memory::MemoryStore;

        let mem = Arc::new(MemoryStore::new("maild"));
        let rt = Arc::new(Runtime::new(
            "maild",
            spec_simple(),
            Arc::clone(&mem) as Arc<dyn PropertyStore>,
        ));
        let mut router = PropsRouter::new("maild");
        router.allow_replay_only_for_tests();
        router.register(Arc::clone(&rt)).unwrap();

        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;

        // Pull the keyed HMAC out of audit.watch.
        let watch_resp = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        let row = body_json(&watch_resp)["events"][0].clone();
        let wire_digest = row["audit_digest"].as_str().unwrap().to_string();
        let nseq = Nseq(row["nseq"].as_u64().unwrap());

        // Re-derive from the current record value (post-commit state).
        let snap = mem.list(&ns_name()).await.unwrap();
        let rec = snap
            .value
            .iter()
            .find(|r| r.key.key == "u@h")
            .expect("record present");
        let key = mem.audit_key(&ns_name()).expect("audit key present");
        let recomputed = compute_digest(&key, &rec.value, nseq);
        assert_eq!(
            wire_digest, recomputed,
            "audit_digest must reproduce from <svc>.props.get + namespace audit key"
        );
    }

    #[tokio::test]
    async fn audit_epoch_breaks_continuity_after_reconcile() {
        // §11 — reconcile is the only path that bumps `audit_epoch`;
        // HMAC continuity is intentionally broken across the boundary.
        // The substrate models this by re-deriving the digest under the
        // new audit_epoch's row. The verifier sees the epoch change
        // alongside the digest change as the cue that prior digests no
        // longer chain.
        use crate::memory::MemoryStore;
        use crate::store::ReconcileCommit;

        let mem = Arc::new(MemoryStore::new("maild"));
        let store: Arc<dyn PropertyStore> = Arc::clone(&mem) as _;
        let rt = Arc::new(Runtime::new("maild", spec_simple(), Arc::clone(&store)));
        let mut router = PropsRouter::new("maild");
        router.allow_replay_only_for_tests();
        router.register(Arc::clone(&rt)).unwrap();

        // Initial set (audit_epoch = 0).
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;

        // Externally reconcile (only path that bumps audit_epoch).
        let key = RecordKey::collection(ns_name(), "u@h");
        let old = PropValue::Object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("email".to_string(), PropValue::String("u@h".to_string()));
            m
        });
        let new = PropValue::Object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("email".to_string(), PropValue::String("v@h".to_string()));
            m
        });
        store
            .commit_reconcile(ReconcileCommit {
                key,
                old_value: Some(old),
                new_value: new,
                ts_ms: 0,
            })
            .await
            .unwrap();

        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        let events = body_json(&r)["events"].clone();
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 2, "set + reconcile");
        assert_eq!(events[0]["audit_epoch"], 0, "set before reconcile");
        assert_eq!(events[1]["audit_epoch"], 1, "reconcile bumps epoch");
        assert_ne!(
            events[0]["audit_digest"], events[1]["audit_digest"],
            "different rows must yield different digests"
        );
    }

    // ----------------- SPEC 12 §15.5 / C10c granter integration ------------

    /// Test fixture: records every `grant` call as
    /// `(topic, target_peer, namespace)` tuples in a shared Mutex so
    /// tests can assert the call shape. When `fail_with` is `Some`,
    /// `grant` returns an `Err` carrying that message instead of
    /// recording; this drives the `grant_failed` rc=10 path.
    #[derive(Default)]
    struct RecorderGranter {
        calls: tokio::sync::Mutex<Vec<(String, String, String)>>,
        fail_with: Option<String>,
    }

    impl RecorderGranter {
        fn new() -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                calls: tokio::sync::Mutex::new(Vec::new()),
                fail_with: Some(msg.to_string()),
            }
        }

        async fn recorded(&self) -> Vec<(String, String, String)> {
            self.calls.lock().await.clone()
        }
    }

    impl crate::subscribe_granter::SubscribeGranter for RecorderGranter {
        fn grant<'a>(
            &'a self,
            topic: &'a str,
            target_peer: &'a str,
            namespace: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async move {
                if let Some(msg) = &self.fail_with {
                    return Err(anyhow::anyhow!(msg.clone()));
                }
                self.calls.lock().await.push((
                    topic.to_string(),
                    target_peer.to_string(),
                    namespace.to_string(),
                ));
                Ok(())
            })
        }
    }

    /// Peer fixture carrying a registered service name so the granter
    /// path's "anonymous caller" guard does not trip in the happy-path
    /// tests below.
    fn service_peer(name: &str) -> PeerIdentity {
        PeerIdentity {
            service_name: Some(name.to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn watch_invokes_granter_with_records_changed_topic() {
        let (mut router, _) = router_with(spec_simple());
        let granter = Arc::new(RecorderGranter::new());
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);

        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer("webd"),
            )
            .await;

        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["live"], true);
        assert_eq!(body["namespace"], "accounts");

        let calls = granter.recorded().await;
        assert_eq!(calls.len(), 1, "exactly one grant per watch");
        assert_eq!(
            calls[0].0, "maild.props.records.changed",
            "records-changed topic for `watch`"
        );
        assert_eq!(calls[0].1, "webd", "target_peer is the calling service");
        assert_eq!(calls[0].2, "accounts", "namespace filter");
    }

    #[tokio::test]
    async fn audit_watch_invokes_granter_with_audit_topic() {
        let (mut router, _) = router_with(spec_simple());
        let granter = Arc::new(RecorderGranter::new());
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);

        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer("auditd"),
            )
            .await;

        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["live"], true);

        let calls = granter.recorded().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0, "maild.props.audit",
            "audit topic for `audit.watch`"
        );
        assert_eq!(calls[0].1, "auditd");
        assert_eq!(calls[0].2, "accounts");
    }

    #[tokio::test]
    async fn watch_with_failing_granter_returns_grant_failed() {
        // Spec line 262–271: grant failure must surface as rc=10
        // (`grant_failed`) — NOT rc=0 with the replay slice. Returning
        // replay-on-failure would be the partial-fix pattern (caller
        // thinks they have a subscription, broker has nothing routed).
        let (mut router, _) = router_with(spec_simple());
        let granter = Arc::new(RecorderGranter::failing("broker connection lost"));
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);

        // Seed a record so replay would have something to return if
        // we wrongly fell back to replay-only on grant failure.
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &service_peer("webd"),
            )
            .await;

        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer("webd"),
            )
            .await;

        assert_eq!(r.rc, 10, "grant failure is operational rc=10");
        let body = body_json(&r);
        assert_eq!(body["error_code"], "grant_failed");
        assert!(
            body.get("events").is_none(),
            "must NOT leak replay slice on grant failure"
        );
        // The peer-visible body must NOT carry the raw granter error
        // text (which can include peer-internal routing detail like
        // target_peer_not_connected or socket paths). The upstream
        // detail is recovered from daemon logs.
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("broker connection lost"),
            "raw granter error text must not leak to the peer-visible body"
        );
        assert!(
            message.contains("grant_failed"),
            "body message names the §9 token: {}",
            message
        );
    }

    #[tokio::test]
    async fn watch_with_replay_only_opt_in_emits_live_false() {
        // Autoresearch / test path: router opts in to replay-only mode
        // (no granter installed). Body advertises `live: false` so the
        // caller knows not to expect broker-delivered live events.
        // `router_with` calls `allow_replay_only_for_tests()` by default.
        let (router, _) = router_with(spec_simple());
        router
            .dispatch(
                "set",
                &req(
                    &[
                        ("namespace", "accounts"),
                        ("key", "u@h"),
                        ("if_version", "0"),
                    ],
                    r#"{"email":"u@h"}"#,
                ),
                &peer(),
            )
            .await;
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0);
        let body = body_json(&r);
        assert_eq!(
            body["live"], false,
            "replay-only opt-in must advertise live=false"
        );
        assert_eq!(
            body["events"].as_array().unwrap().len(),
            1,
            "replay still returned in replay-only mode"
        );
    }

    #[tokio::test]
    async fn audit_watch_with_replay_only_opt_in_emits_live_false() {
        let (router, _) = router_with(spec_simple());
        let r = router
            .dispatch(
                "audit.watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;
        assert_eq!(r.rc, 0);
        assert_eq!(body_json(&r)["live"], false);
    }

    #[tokio::test]
    async fn watch_without_granter_and_no_opt_in_refuses() {
        // Production safety: PropsRouter without a granter installed
        // and without explicit replay-only opt-in is a startup wiring
        // miss, not a deliberate degradation. The handler must refuse
        // at first request so the misconfiguration surfaces — silent
        // degradation would be the C9 partial-fix anti-pattern.
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let rt = Arc::new(Runtime::new("maild", spec_simple(), Arc::clone(&store)));
        let mut router = PropsRouter::new("maild");
        router.register(rt).unwrap();
        // No `set_granter`. No `allow_replay_only_for_tests`.
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer("webd"),
            )
            .await;
        assert_eq!(r.rc, 20, "missing granter is rc=20 (port-degrading)");
        let body = body_json(&r);
        assert_eq!(body["error_code"], "unavailable");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("SubscribeGranter"),
            "error message names the missing wiring step"
        );
    }

    #[tokio::test]
    async fn watch_with_anonymous_peer_rejected_when_granter_installed() {
        // Anonymous caller (no registered service_name) cannot receive
        // live deliveries — noded's subscribe_grant requires a
        // connection-registry hit on target_peer. This is an
        // *authorisation* failure (authenticated as unix uid only, not
        // eligible for the live-delivery identity class), so we surface
        // `auth_denied` per §9, not `validation_error`.
        let (mut router, _) = router_with(spec_simple());
        let granter = Arc::new(RecorderGranter::new());
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);

        // peer() returns a unix_uid-only PeerIdentity with no service_name.
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &peer(),
            )
            .await;

        assert_eq!(r.rc, 10);
        let body = body_json(&r);
        assert_eq!(body["error_code"], "auth_denied");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("anonymous"),
            "message should explain the anonymous-peer rejection"
        );
        assert!(
            granter.recorded().await.is_empty(),
            "granter must not be called for anonymous peer"
        );
    }

    #[tokio::test]
    async fn watch_with_empty_service_name_rejected_when_granter_installed() {
        // Defense against an attacker / buggy transport that attaches
        // `service_name: Some("")` — the guard checks `s.is_empty()`
        // explicitly so an empty-string identity does not bypass the
        // anonymous-peer rejection.
        let (mut router, _) = router_with(spec_simple());
        let granter = Arc::new(RecorderGranter::new());
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);
        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer(""),
            )
            .await;
        assert_eq!(r.rc, 10);
        assert_eq!(body_json(&r)["error_code"], "auth_denied");
        assert!(granter.recorded().await.is_empty());
    }

    #[tokio::test]
    async fn watch_grant_precedes_cursor_snapshot() {
        // Spec "Replay/live ordering" / handle_watch_inner doc — the
        // grant call MUST complete before the observed_nseq snapshot
        // so the broker is routing publishes for the topic at the
        // moment we record the cursor. We can verify the temporal
        // ordering by having the granter perform a `set` during its
        // grant call: the new record's nseq must appear in the replay
        // body (i.e. observed snapshot taken AFTER grant returns).
        let (mut router, _) = router_with(spec_simple());

        struct AdvancingGranter {
            router_for_advance: tokio::sync::Mutex<Option<Arc<PropsRouter>>>,
        }
        impl crate::subscribe_granter::SubscribeGranter for AdvancingGranter {
            fn grant<'a>(
                &'a self,
                _topic: &'a str,
                _target_peer: &'a str,
                _ns: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
            {
                Box::pin(async move {
                    let r = self.router_for_advance.lock().await.clone();
                    if let Some(router) = r {
                        router
                            .dispatch(
                                "set",
                                &req(
                                    &[
                                        ("namespace", "accounts"),
                                        ("key", "during@grant"),
                                        ("if_version", "0"),
                                    ],
                                    r#"{"email":"during@grant"}"#,
                                ),
                                &service_peer("webd"),
                            )
                            .await;
                    }
                    Ok(())
                })
            }
        }

        let granter = Arc::new(AdvancingGranter {
            router_for_advance: tokio::sync::Mutex::new(None),
        });
        router.set_granter(Arc::clone(&granter) as Arc<dyn SubscribeGranter>);

        let router = Arc::new(router);
        *granter.router_for_advance.lock().await = Some(Arc::clone(&router));

        let r = router
            .dispatch(
                "watch",
                &req(&[("namespace", "accounts"), ("since_nseq", "0")], ""),
                &service_peer("webd"),
            )
            .await;
        assert_eq!(r.rc, 0, "body: {}", r.body);
        let body = body_json(&r);
        assert_eq!(body["observed_nseq"], 1, "observed taken AFTER grant");
        let events = body["events"].as_array().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the grant-time set must be visible in the post-grant replay"
        );
        assert_eq!(events[0]["nseq"], 1);
    }
}
