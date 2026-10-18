//! `WebdBusCallHandler` — the local Bus-named implementation of Mix's call-handler API.
//! backing the `bus_call` builtin in an embedded Mix handler (vtoken C2).
//!
//! It lets an admin route's Mix handler call a maild Bus verb under DELEGATED
//! identity. The value lives entirely in the delegation being RIGHT, so this
//! struct is the choke point:
//!
//! - **Verb allowlist (Rust, before any send):** the route grants exact verbs
//!   (`bus:maild.vtoken.list_opaque`); `bus_verbs` is that set, and `call()` refuses
//!   anything not in it. The Mix script can NEVER reach an ungranted verb.
//! - **Delegation envelope from TRUSTED state:** the actor (the logged-in
//!   admin's email), vhost, route_id, and a fresh request_id come from
//!   [`DelegationInputs`] — webd request state, NOT the Mix args. The handler is
//!   only constructed AFTER webd's Rust-side admin + CSRF gate passes, so it
//!   always emits `role:"admin"`, `csrf_verified:true`. The Mix script supplies
//!   only the verb + args and can name no destination/peer/actor.
//! - **Runtime bridge:** a handler eval runs on an isolated `spawn_blocking`
//!   current-thread runtime, but the broker `NodedClient`'s WebSocket lives on
//!   webd's MAIN runtime. So `call()` SPAWNS the broker send onto the captured
//!   main `Handle` and awaits the reply over a `oneshot` (a oneshot is
//!   runtime-agnostic) — never driving cross-runtime WS I/O directly.
//!
//! maild authorizes on the actor (`bus/vtoken.rs` delegated path): the peer
//! (`cmd.from = webd`) must be allowlisted, the actor must be a real account,
//! and the target must be in the actor's domain. webd is the policy decision
//! point; maild is the binding enforcement point.
//!
use std::collections::BTreeSet;
use std::sync::Arc;

use cosmix_mix::json::{json_to_mix, mix_to_json};
use cosmix_mix::value::Value;
use cosmix_mix::{BusCallFuture, BusCallHandler, MixError, MixResult};

use crate::bus::subscribe_granter::SharedBrokerHandle;

/// The EXPLICIT allowlist of delegated-Bus verbs webd may grant a route. ONLY
/// these implement a DAEMON-SIDE delegated-envelope authorization spine that
/// interprets `$cosmix_delegation` (maild `bus/vtoken.rs`; filesd `delegation.rs`,
/// slice S6a). Any OTHER verb authorizes by mesh-peer / operator rules and would
/// IGNORE the envelope — granting it to a webd admin route would be a
/// confused-deputy hole (e.g. `maild.accounts.*` is callable by any WG peer with
/// no actor/target binding). A new verb joins this list ONLY after its daemon
/// dispatch implements delegated auth. This is the single source of truth; route
/// capability validation rejects everything else.
pub(crate) fn is_delegated_safe_bus_verb(verb: &str) -> bool {
    matches!(
        verb,
        // maild — vtoken C2 delegated path (bus/vtoken.rs).
        "maild.vtoken.mint_opaque"
            | "maild.vtoken.list_opaque"
            | "maild.vtoken.lookup_opaque"
            | "maild.vtoken.disable_opaque"
            // filesd — corpus delegated path (delegation.rs, S6a). `resync` is an
            // operator escape hatch, deliberately NOT web-delegable (least privilege).
            | "filesd.list"
            | "filesd.read"
            | "filesd.search"
            | "filesd.changes"
            | "filesd.save"
            | "filesd.move"
            | "filesd.delete"
            // filesd fs-mode — the generic file-manager capability (the dual-pane
            // FM, D3-A). Every verb interprets the SAME `$cosmix_delegation` envelope
            // via the shared `gate()` + `fsops`; reads AND writes are web-delegable,
            // gated per-place by `writable` and (delete/trash.empty) by confirm:true.
            // The instance is `filesd-fs` — routes MUST pin `bus-svc:filesd-fs`
            // (the `fs.*` prefix doesn't name the service).
            | "fs.places"
            | "fs.list"
            | "fs.stat"
            | "fs.read_blob"
            | "fs.search"
            | "fs.trash.list"
            | "fs.tree"
            | "fs.mkdir"
            | "fs.touch"
            | "fs.write"
            | "fs.copy"
            | "fs.move"
            | "fs.trash"
            | "fs.trash.restore"
            | "fs.trash.empty"
            | "fs.delete"
            // provisiond — the AUTHORITY-FREE exception to the "must implement
            // delegated auth" rule above (provisiond plan Brick 5, Codex 019f4fdb).
            // `provisiond.wake` takes no args, names no target, and only nudges
            // provisiond to drain jobs it independently claims through the D9
            // handshake — its body cannot influence any privileged work, so there is
            // nothing for a confused deputy to be tricked into. The lazy systemd
            // timer already holds equivalent activation authority. The citizen MUST
            // ignore all caller-supplied data. If a future wake arg ever influences
            // job selection/priority/tenant/kind/target, this exception is void and
            // broker-canonical sender validation must be added. Route MUST pin
            // `bus-svc:provisiond`. NOT read-only (below) -> POST + CSRF + admin only.
            | "provisiond.wake"
            // toolsd — the SAME authority-free exception, for the same reasons
            // (toolsd plan, Codex 019f5bc3). `toolsd.wake` takes no args and names no
            // target: it cannot say WHICH tool to run or FOR WHOM. It only nudges the
            // toolsd drain to claim whatever webd has already authorised and queued,
            // through a separately-authenticated, secret-gated seam (`/tools/claim-next`,
            // its OWN queue and OWN credential — it can never reach a provisioning job).
            // The wake citizen holds no credential and ignores all caller-supplied data;
            // its only authority is a polkit rule starting one unit. The backstop timer
            // already holds equivalent activation authority. If a future wake arg ever
            // influences tool selection/params/identity, this exception is void. Route
            // MUST pin `bus-svc:toolsd`. NOT read-only -> POST + admin only.
            | "toolsd.wake"
            // sshm — the SAME authority-free exception. `sshm.wake` takes no args and
            // names no target: it cannot select a job, host, kind or tenant. It only
            // nudges the sshm drain to claim whatever webd has already authorised and
            // durably queued through the separately-authenticated worker seam. The wake
            // citizen holds no credential and ignores all caller-supplied data; its only
            // authority is writing a trigger file a `.path` unit watches (NO polkit —
            // a StartUnit grant cannot constrain the systemd job mode). Unlike
            // provisiond/toolsd, sshm is NOT reached via `bus:sshm.wake` + `bus-svc:sshm`
            // on a route; webd fires it from the declarative `wake:sshm.wake` capability
            // (see `parse_wake_capability`). Kept delegated-safe for symmetry + so a
            // future handler-issued call stays possible; the exception is void the moment
            // a wake arg ever influences job selection/host/kind/tenant.
            | "sshm.wake"
    )
}

/// Read-only delegated-Bus verbs (no state change). The complement within
/// [`is_delegated_safe_bus_verb`] mutates. CENTRALIZED here (Codex C2) — the
/// CSRF policy must NOT be inferred from the HTTP method or route path. A verb
/// not listed read-only is treated as MUTATING (fail-safe).
pub(crate) fn is_read_only_bus_verb(verb: &str) -> bool {
    matches!(
        verb,
        "maild.vtoken.list_opaque"
            | "maild.vtoken.lookup_opaque"
            | "filesd.list"
            | "filesd.read"
            | "filesd.search"
            | "filesd.changes"
            // fs-mode reads (the rest of `fs.*` mutates → token-checked off-POST).
            | "fs.places"
            | "fs.list"
            | "fs.stat"
            | "fs.read_blob"
            | "fs.search"
            | "fs.trash.list"
            | "fs.tree"
    )
}

/// Whether a route granting `bus_verbs` must present a matching `x-csrf-token`
/// for an HTTP request of `method` (Codex C2 ruling C). An explicit token is
/// required ONLY when a MUTATING verb is reachable by a method OTHER than
/// `POST`:
/// - `POST` + mutating verb → NO token: an SSR plain-form `POST` relies on the
///   SameSite session cookie + the admin gate, exactly as every existing webd
///   SSR admin handler does (a cross-site `POST` drops the Lax cookie → no
///   session → denied).
/// - `GET`/`HEAD` + only read-only verbs → no token (the SSR list/lookup page).
/// - `GET` + a mutating grant → token required (Lax sends the cookie on a
///   cross-site top-level GET) — effectively blocking a mutate-via-GET route.
/// - `PUT`/`PATCH`/`DELETE`/`OPTIONS` + a mutating grant → token required
///   (these are fetch/JSON paths, not plain HTML forms — keep the stronger
///   posture).
pub(crate) fn bus_route_requires_csrf(bus_verbs: &BTreeSet<String>, method: &str) -> bool {
    let has_mutating = bus_verbs.iter().any(|v| !is_read_only_bus_verb(v));
    has_mutating && method != "POST"
}

/// Resolve the Bus service a route's delegated verbs route to. An explicit
/// `bus-svc:<service>` route capability ALWAYS wins (the way a per-corpus filesd
/// instance — `filesd-notes` — is named, since the verb prefix `filesd` is not
/// the instance service). With no explicit value, derive from the verbs' shared
/// first dotted segment (`maild.vtoken.list` → `maild`) — backward-compatible
/// for the maild routes. `None` (reject) when the granted verbs span more than
/// one prefix and no explicit service pins it (an ambiguous misconfiguration) or
/// the set is empty. Chosen at route-bind time, NEVER from request args.
pub(crate) fn resolve_target_service(
    explicit: Option<&str>,
    bus_verbs: &BTreeSet<String>,
) -> Option<String> {
    // A non-empty explicit value wins; an empty one is treated as absent (derive).
    if let Some(s) = explicit.filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    let mut prefixes = bus_verbs.iter().filter_map(|v| v.split('.').next());
    let first = prefixes.next()?;
    if !prefixes.all(|p| p == first) {
        return None;
    }
    // The fs-mode prefix `fs` NEVER names a service (the instance is `filesd-fs`), so
    // an `fs.*` route that forgot to pin `bus-svc:filesd-fs` would otherwise derive a
    // phantom target `fs`. Reject it → the misconfiguration fails loudly (grants
    // dropped, fail closed) instead of silently routing nowhere.
    if first == "fs" {
        return None;
    }
    Some(first.to_string())
}

/// The trusted, per-request delegation context webd injects into every
/// delegated call. Built from request state (session email, matched vhost +
/// route, a fresh request id) — NEVER from the Mix args.
#[derive(Clone, Debug)]
pub struct DelegationInputs {
    /// The authenticated admin's email (the `$SESSION` identity).
    pub actor: String,
    /// The vhost fqdn the request matched.
    pub vhost: String,
    /// The matched route id (audit correlation + route attribution).
    pub route_id: String,
    /// A per-request id for audit correlation.
    pub request_id: String,
}

/// Per-request handler. Holds the shared broker handle + the captured MAIN
/// runtime handle (for the cross-runtime bridge), the route's exact-verb grant
/// set, and the trusted delegation inputs.
pub(crate) struct WebdBusCallHandler {
    broker: SharedBrokerHandle,
    main_handle: tokio::runtime::Handle,
    bus_verbs: Arc<BTreeSet<String>>,
    inputs: DelegationInputs,
    /// The Bus service the granted verbs route to (e.g. `maild`, `filesd-notes`).
    /// One route = one target service (chosen before the call, never from args).
    service: String,
    /// ACCELERATOR-ONLY mode (a dev_session caller — see `build_bus_injection`).
    /// The caller has NO cookie-backed session, so it gets a deliberately tiny
    /// slice of the seam: ONLY [`is_accelerator_wake_verb`], and ONLY with empty
    /// args. It does NOT inherit the broader delegated-safe allowlist.
    accelerator_only: bool,
}

impl WebdBusCallHandler {
    pub(crate) fn new(
        broker: SharedBrokerHandle,
        main_handle: tokio::runtime::Handle,
        bus_verbs: Arc<BTreeSet<String>>,
        inputs: DelegationInputs,
        service: String,
        accelerator_only: bool,
    ) -> Self {
        Self {
            broker,
            main_handle,
            bus_verbs,
            inputs,
            service,
            accelerator_only,
        }
    }
}

/// The ACCELERATOR verbs: authority-free wakes that carry no args and name no
/// target, and therefore cannot select work, a tool, a tenant or an identity.
/// They only nudge a daemon to drain a queue it independently claims through a
/// separately-authenticated seam — so calling one confers no authority beyond
/// "go and look at the queue you were already allowed to look at".
///
/// EXACT NAMES ONLY — deliberately not a `*.wake` suffix pattern, so a future
/// `something.wake` that DOES take args cannot silently inherit this (Codex
/// 019f5bc3). Adding a verb here is a security decision, not a naming accident.
///
/// This list is the ONLY thing a dev_session (cookieless auto-login) caller can
/// reach over the bus seam. It must never grow to include a verb that acts on
/// caller-supplied data.
pub(crate) fn is_accelerator_wake_verb(verb: &str) -> bool {
    matches!(verb, "provisiond.wake" | "toolsd.wake" | "sshm.wake")
}

/// Parse a `wake:<verb>` route capability into its `(verb, service)`.
///
/// webd fires this wake ITSELF, best-effort, after the route returns a non-error
/// (`< 400`) response for an authenticated session (see
/// [`fire_wake_after_response`]) — decoupled from the route's primary
/// `bus-svc:` pin, so a route that talks to (say) `filesd-fs` for its real work
/// can still nudge `sshm`. The verb MUST be an [`is_accelerator_wake_verb`]
/// (authority-free, no args, names no target) and its service must resolve from
/// the verb prefix; anything else returns `None` (fail closed — the grant is
/// dropped and logged by the caller).
pub(crate) fn parse_wake_capability(cap: &str) -> Option<(String, String)> {
    let verb = cap.strip_prefix("wake:")?;
    if !is_accelerator_wake_verb(verb) {
        return None;
    }
    let one: BTreeSet<String> = std::iter::once(verb.to_string()).collect();
    let svc = resolve_target_service(None, &one)?;
    Some((verb.to_string(), svc))
}

/// Fire a best-effort, authority-free accelerator wake AFTER a route returned a
/// non-error (`< 400`) response for an authenticated session. Spawned detached on
/// the MAIN runtime with a short deadline: it NEVER blocks or fails the request,
/// and a lost wake costs only latency — the target daemon's backstop timer
/// recovers the queued work. Args are always empty (a wake carries no selector);
/// the trusted envelope mirrors a handler-issued wake so the seam sees a
/// well-formed body (the wake citizen ignores it).
pub(crate) fn fire_wake_after_response(
    broker: SharedBrokerHandle,
    main_handle: &tokio::runtime::Handle,
    service: String,
    verb: String,
    inputs: DelegationInputs,
) {
    let body = match build_delegated_body(&inputs, &Value::Nil) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "webd::bus", %verb, error = %e,
                "post-response wake: could not build envelope; skipping (backstop timer recovers)"
            );
            return;
        }
    };
    main_handle.spawn(async move {
        let fired = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            match broker.load_full() {
                Some(client) => client
                    .call(&service, &verb, body)
                    .await
                    .map_err(|e| e.to_string()),
                None => Err("broker unavailable (no live noded connection)".to_string()),
            }
        })
        .await;
        match fired {
            Ok(Ok(_)) => tracing::debug!(
                target: "webd::bus", %verb, "post-response accelerator wake fired"
            ),
            Ok(Err(e)) => tracing::warn!(
                target: "webd::bus", %verb, error = %e,
                "post-response wake not accepted (backstop timer will recover the work)"
            ),
            Err(_) => tracing::warn!(
                target: "webd::bus", %verb,
                "post-response wake timed out after 2s (backstop timer will recover the work)"
            ),
        }
    });
}

/// Whether a Mix `args` value is "no arguments" — nil, or an empty map/list.
/// An authority-free wake must carry NOTHING: the moment a wake can be handed
/// data, it stops being authority-free and this whole exception is void.
fn args_are_empty(args: &Value) -> bool {
    match args {
        Value::Nil => true,
        Value::Map(m) => m.is_empty(),
        Value::List(l) => l.is_empty(),
        _ => false,
    }
}

fn rt_err(msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        msg: msg.into(),
        span: None,
    }
}

/// Build the wrapped delegated body `{ "$cosmix_delegation": {…trusted…},
/// "args": <mix args as json> }`. The envelope always carries `role:"admin"` +
/// `csrf_verified:true` because the handler is only constructed once webd's
/// Rust-side admin + CSRF gate has passed. Pure (testable).
fn build_delegated_body(
    inputs: &DelegationInputs,
    args: &Value,
) -> Result<serde_json::Value, String> {
    // mix_to_json is fallible since cosmix-lib-mix 0.21.0: a non-finite
    // number in the args has no JSON representation and errors loudly
    // (previously a silent 0). Propagate — the caller surfaces it as a
    // catchable bus_call error.
    let args_json = mix_to_json(args)?;
    Ok(serde_json::json!({
        "$cosmix_delegation": {
            "version": 1,
            "actor": inputs.actor,
            "vhost": inputs.vhost,
            "route_id": inputs.route_id,
            "role": "admin",
            "csrf_verified": true,
            "request_id": inputs.request_id,
        },
        "args": args_json,
    }))
}

impl BusCallHandler for WebdBusCallHandler {
    fn call<'a>(&'a self, verb: &'a str, args: &'a Value) -> BusCallFuture<'a, MixResult<Value>> {
        // Verb allowlist — enforced in RUST, before any send, at BOTH the route
        // membership AND the global delegated-safe allowlist. The second check
        // is defence-in-depth: a stale/injected `bus:` grant persisted in cms.db
        // (e.g. from before the write-time allowlist, or a tooling bug) would
        // collect into `bus_verbs`, but it can still never reach maild — only a
        // verb with a delegated-envelope authz spine is ever sent.
        if !is_delegated_safe_bus_verb(verb) || !self.bus_verbs.contains(verb) {
            let verb = verb.to_string();
            return Box::pin(async move {
                Err(rt_err(format!(
                    "bus_call: verb {verb:?} is not a granted, delegated-safe verb for this route"
                )))
            });
        }
        // The authority-free wake exception is valid ONLY while every wake takes no
        // args. Enforce that invariant for every caller, not merely dev_session: no
        // job, host, kind or tenant selector may cross this Rust boundary.
        if is_accelerator_wake_verb(verb) && !args_are_empty(args) {
            let verb = verb.to_string();
            return Box::pin(async move {
                Err(rt_err(format!(
                    "bus_call: authority-free wake {verb:?} must carry no arguments"
                )))
            });
        }
        // ACCELERATOR-ONLY (dev_session caller): a SEPARATE, strictly smaller gate
        // that does NOT inherit the allowlist checked above. Only an authority-free
        // wake, and only with zero args — so a cookieless dev_session can never
        // reach vtoken/fs/db-mutating verbs, nor hand data to the one verb it can
        // reach. Both halves are enforced here in Rust, before any send.
        if self.accelerator_only && !is_accelerator_wake_verb(verb) {
            let verb = verb.to_string();
            return Box::pin(async move {
                Err(rt_err(format!(
                    "bus_call: verb {verb:?} is not an argument-free accelerator wake \
                     (this caller has no cookie-backed session)"
                )))
            });
        }
        // Build the trusted-state envelope NOW (sync), before crossing runtimes.
        let body = match build_delegated_body(&self.inputs, args) {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("bus_call: args not JSON-encodable: {e}");
                return Box::pin(async move { Err(rt_err(msg)) });
            }
        };
        let broker = self.broker.clone();
        let main_handle = self.main_handle.clone();
        let service = self.service.clone();
        let verb = verb.to_string();
        Box::pin(async move {
            // Bridge: drive the broker send on the MAIN runtime (its WS lives
            // there), await the reply over a runtime-agnostic oneshot.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let svc = service.clone();
            main_handle.spawn(async move {
                let result = match broker.load_full() {
                    Some(client) => client
                        .call(&svc, &verb, body)
                        .await
                        .map_err(|e| e.to_string()),
                    None => Err("broker unavailable (no live noded connection)".to_string()),
                };
                let _ = tx.send(result);
            });
            match rx.await {
                Ok(Ok(reply)) => Ok(json_to_mix(reply)),
                // A daemon rc>=10 (auth_denied / not-found / …) or a transport
                // failure both arrive here as an Err — the Mix handler can
                // `try`/`catch` to render gracefully.
                Ok(Err(e)) => Err(rt_err(format!("bus_call({service}) failed: {e}"))),
                Err(_) => Err(rt_err("bus_call: broker dispatch task dropped")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mix::IndexMap;

    fn inputs() -> DelegationInputs {
        DelegationInputs {
            actor: "admin@example.org".to_string(),
            vhost: "example.org".to_string(),
            route_id: "admin-vtokens".to_string(),
            request_id: "req-123".to_string(),
        }
    }

    fn verbs(v: &[&str]) -> Arc<BTreeSet<String>> {
        Arc::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn build_body_wraps_trusted_envelope_and_args() {
        let mut m = IndexMap::new();
        m.insert(
            "account".to_string(),
            Value::String("blog@example.org".into()),
        );
        // A hostile arg trying to override the envelope — must land under `args`,
        // never the top-level envelope.
        m.insert("actor".to_string(), Value::String("evil@other.org".into()));
        let body = build_delegated_body(&inputs(), &Value::map(m)).unwrap();
        let env = &body["$cosmix_delegation"];
        assert_eq!(env["version"], serde_json::json!(1));
        assert_eq!(env["actor"], serde_json::json!("admin@example.org"));
        assert_eq!(env["role"], serde_json::json!("admin"));
        assert_eq!(env["csrf_verified"], serde_json::json!(true));
        assert_eq!(env["vhost"], serde_json::json!("example.org"));
        assert_eq!(env["route_id"], serde_json::json!("admin-vtokens"));
        // The smuggled `actor` is plain data under `args`, NOT the envelope.
        assert_eq!(body["args"]["actor"], serde_json::json!("evil@other.org"));
        assert_eq!(
            body["args"]["account"],
            serde_json::json!("blog@example.org")
        );
    }

    #[tokio::test]
    async fn ungranted_verb_is_refused_before_any_send() {
        // No broker connection installed — but an ungranted verb must be refused
        // BEFORE the bridge is reached, so this never touches the broker.
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["maild.vtoken.list_opaque"]),
            inputs(),
            "maild".to_string(),
            false,
        );
        let err = h
            .call("maild.vtoken.mint_opaque", &Value::map(IndexMap::new()))
            .await
            .expect_err("an ungranted verb must be refused");
        assert!(
            format!("{err}").contains("not a granted, delegated-safe verb"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn handler_refuses_a_non_delegated_safe_verb_even_if_granted() {
        // Defence-in-depth: even if a stale/unsafe verb somehow landed in
        // `bus_verbs`, the handler re-checks the global allowlist and refuses
        // it before any send — `maild.accounts.*` can never reach maild here.
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["maild.accounts.seed_mailboxes"]),
            inputs(),
            "maild".to_string(),
            false,
        );
        let err = h
            .call(
                "maild.accounts.seed_mailboxes",
                &Value::map(IndexMap::new()),
            )
            .await
            .expect_err("a non-delegated-safe verb must be refused");
        assert!(format!("{err}").contains("delegated-safe"), "{err}");
    }

    #[tokio::test]
    async fn granted_verb_with_no_broker_surfaces_unavailable() {
        // A granted verb passes the allowlist, reaches the bridge, and — with no
        // installed client — returns the broker-unavailable error (not a panic).
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["maild.vtoken.list_opaque"]),
            inputs(),
            "maild".to_string(),
            false,
        );
        let err = h
            .call("maild.vtoken.list_opaque", &Value::map(IndexMap::new()))
            .await
            .expect_err("no broker → unavailable");
        assert!(format!("{err}").contains("broker unavailable"), "{err}");
    }

    // ── accelerator-only mode (a cookieless dev_session caller) ──────────────
    //
    // The negative tests Codex required: this mode is a strictly SMALLER gate that
    // does NOT inherit the delegated-safe allowlist. A dev_session may nudge a
    // drain and nothing else — it can reach no vtoken, no filesystem, no future
    // delegated verb, and it cannot hand ARGS even to the verb it can reach.

    #[test]
    fn accelerator_wake_verbs_are_an_exact_closed_list() {
        assert!(is_accelerator_wake_verb("provisiond.wake"));
        assert!(is_accelerator_wake_verb("toolsd.wake"));
        assert!(is_accelerator_wake_verb("sshm.wake"));
        // NOT a `*.wake` suffix pattern — a future wake that takes args must not
        // silently inherit the exception by virtue of its name.
        assert!(!is_accelerator_wake_verb("evil.wake"));
        assert!(!is_accelerator_wake_verb("maild.vtoken.mint_opaque"));
        assert!(!is_accelerator_wake_verb("fs.write"));
        // Every accelerator must also be delegated-safe (the outer gate still runs).
        assert!(is_delegated_safe_bus_verb("provisiond.wake"));
        assert!(is_delegated_safe_bus_verb("toolsd.wake"));
        assert!(is_delegated_safe_bus_verb("sshm.wake"));
    }

    #[tokio::test]
    async fn accelerator_only_refuses_every_non_wake_verb_even_when_granted() {
        // The route GRANTS these verbs and they ARE delegated-safe — the ONLY thing
        // refusing them is accelerator_only. This is the test that proves a
        // dev_session cannot ride the seam to maild or the filesystem.
        for verb in ["maild.vtoken.mint_opaque", "fs.write", "filesd.delete"] {
            let h = WebdBusCallHandler::new(
                crate::bus::subscribe_granter::new_broker_handle(),
                tokio::runtime::Handle::current(),
                verbs(&[verb]),
                inputs(),
                "maild".to_string(),
                true, // accelerator_only
            );
            let err = h
                .call(verb, &Value::map(IndexMap::new()))
                .await
                .expect_err("accelerator-only must refuse a non-wake verb");
            assert!(
                format!("{err}").contains("accelerator wake"),
                "{verb}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn accelerator_only_refuses_a_wake_carrying_args() {
        // A wake with a body is no longer authority-free: it could select work, a
        // tool, a tenant. Refuse it rather than let the exception rot.
        let mut args = IndexMap::new();
        args.insert("tool".to_string(), Value::String("shwho".into()));
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["toolsd.wake"]),
            inputs(),
            "toolsd".to_string(),
            true,
        );
        let err = h
            .call("toolsd.wake", &Value::map(args))
            .await
            .expect_err("an accelerator wake must carry no args");
        assert!(
            format!("{err}").contains("must carry no arguments"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn delegated_sshm_wake_refuses_args_for_a_cookie_backed_session() {
        // Empty args are part of the authority-free contract for EVERY caller. A real
        // admin session must not be able to smuggle a host/job selector either.
        let mut args = IndexMap::new();
        args.insert("host".to_string(), Value::String("alpha".into()));
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["sshm.wake"]),
            inputs(),
            "sshm".to_string(),
            false,
        );
        let err = h
            .call("sshm.wake", &Value::map(args))
            .await
            .expect_err("sshm.wake must reject args for every caller");
        assert!(
            format!("{err}").contains("must carry no arguments"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn accelerator_only_allows_an_argument_free_wake_through_to_the_bridge() {
        // The positive case: an empty-args wake passes the gate and reaches the
        // broker bridge (which, with no client installed, reports unavailable —
        // proving it got PAST the gate rather than being refused by it).
        let h = WebdBusCallHandler::new(
            crate::bus::subscribe_granter::new_broker_handle(),
            tokio::runtime::Handle::current(),
            verbs(&["toolsd.wake"]),
            inputs(),
            "toolsd".to_string(),
            true,
        );
        let err = h
            .call("toolsd.wake", &Value::map(IndexMap::new()))
            .await
            .expect_err("no broker → unavailable");
        assert!(format!("{err}").contains("broker unavailable"), "{err}");
    }

    #[test]
    fn csrf_policy_is_verb_aware_not_just_method_aware() {
        let read = verbs(&["maild.vtoken.list_opaque", "maild.vtoken.lookup_opaque"]);
        let write = verbs(&["maild.vtoken.list_opaque", "maild.vtoken.disable_opaque"]);
        // Read-only grants → never need a token (no state change), any method.
        assert!(!bus_route_requires_csrf(&read, "GET"));
        assert!(!bus_route_requires_csrf(&read, "HEAD"));
        assert!(!bus_route_requires_csrf(&read, "POST"));
        assert!(!bus_route_requires_csrf(&read, "OPTIONS"));
        // POST + mutating → NO explicit token (SameSite + admin gate carry it,
        // matching every existing SSR admin form).
        assert!(!bus_route_requires_csrf(&write, "POST"));
        // A mutating grant reachable by a NON-POST method → token required: a
        // Lax-cookie GET, or the fetch/JSON methods.
        assert!(bus_route_requires_csrf(&write, "GET"));
        assert!(bus_route_requires_csrf(&write, "DELETE"));
        assert!(bus_route_requires_csrf(&write, "OPTIONS"));
        // An unknown verb is treated as mutating (fail-safe).
        assert!(bus_route_requires_csrf(
            &verbs(&["maild.vtoken.future"]),
            "GET"
        ));
        assert!(!bus_route_requires_csrf(
            &verbs(&["maild.vtoken.future"]),
            "POST"
        ));
        // Read-only classifier.
        assert!(is_read_only_bus_verb("maild.vtoken.list_opaque"));
        assert!(is_read_only_bus_verb("maild.vtoken.lookup_opaque"));
        assert!(!is_read_only_bus_verb("maild.vtoken.mint_opaque"));
        assert!(!is_read_only_bus_verb("maild.vtoken.disable_opaque"));
    }

    #[test]
    fn provisiond_wake_is_delegated_safe_but_mutating() {
        // Brick 5: the authority-free wake is in the delegated-safe allowlist (a
        // granted route may call it) but is NOT read-only, so it stays in the
        // mutating class — a Lax-cookie GET or any non-POST needs a token, and it can
        // never ride a tokenless read-only GET.
        assert!(is_delegated_safe_bus_verb("provisiond.wake"));
        assert!(!is_read_only_bus_verb("provisiond.wake"));
        let wake = verbs(&["provisiond.wake"]);
        assert!(!bus_route_requires_csrf(&wake, "POST"));
        assert!(bus_route_requires_csrf(&wake, "GET"));
        assert!(bus_route_requires_csrf(&wake, "DELETE"));
    }

    #[test]
    fn toolsd_wake_is_delegated_safe_but_mutating() {
        // Same authority-free exception as provisiond.wake, same class: a granted route
        // may call it, but it is NOT read-only, so it can never ride a tokenless
        // read-only GET. The /tools/run route is POST, which is why it needs no token.
        assert!(is_delegated_safe_bus_verb("toolsd.wake"));
        assert!(!is_read_only_bus_verb("toolsd.wake"));
        let wake = verbs(&["toolsd.wake"]);
        assert!(!bus_route_requires_csrf(&wake, "POST"));
        assert!(bus_route_requires_csrf(&wake, "GET"));
        assert!(bus_route_requires_csrf(&wake, "DELETE"));
    }

    #[test]
    fn delegated_safe_allowlist_is_the_vtoken_and_filesd_verbs() {
        for ok in [
            // maild vtoken C2
            "maild.vtoken.mint_opaque",
            "maild.vtoken.list_opaque",
            "maild.vtoken.lookup_opaque",
            "maild.vtoken.disable_opaque",
            // filesd corpus (S6a daemon-side delegated-envelope authz)
            "filesd.list",
            "filesd.read",
            "filesd.search",
            "filesd.changes",
            "filesd.save",
            "filesd.move",
            "filesd.delete",
            // filesd fs-mode (the file manager) — reads + writes are all delegable.
            "fs.places",
            "fs.list",
            "fs.stat",
            "fs.read_blob",
            "fs.search",
            "fs.trash.list",
            "fs.mkdir",
            "fs.touch",
            "fs.write",
            "fs.copy",
            "fs.move",
            "fs.trash",
            "fs.trash.restore",
            "fs.trash.empty",
            "fs.delete",
            // the authority-free wakes (no args, no target)
            "provisiond.wake",
            "toolsd.wake",
            "sshm.wake",
        ] {
            assert!(
                is_delegated_safe_bus_verb(ok),
                "{ok} should be delegated-safe"
            );
        }
        // Non-allowlisted verbs (no delegated-envelope authz) are refused —
        // including legacy segmented vtoken verbs and filesd's operator-only
        // `resync` escape hatch (deliberately NOT web-delegable).
        for bad in [
            "maild.accounts.seed_mailboxes",
            "maild.retention.run",
            "maild.vtoken.seed",
            "maild.vtoken.add",
            "noded.info",
            "filesd.resync",
            "filesd.props.list",
            "filesd",
            // fs-mode props/operator surfaces are NOT web-delegable; bare prefixes
            // and unknown verbs are refused.
            "fs.props.list",
            "fs.resync",
            "fs",
            "fs.bogus",
        ] {
            assert!(
                !is_delegated_safe_bus_verb(bad),
                "{bad} must NOT be delegated-safe"
            );
        }
    }

    #[test]
    fn filesd_read_verbs_are_read_only_writes_are_not() {
        for ro in [
            "filesd.list",
            "filesd.read",
            "filesd.search",
            "filesd.changes",
        ] {
            assert!(is_read_only_bus_verb(ro), "{ro} should be read-only");
        }
        for rw in ["filesd.save", "filesd.move", "filesd.delete"] {
            assert!(
                !is_read_only_bus_verb(rw),
                "{rw} must be treated as mutating"
            );
        }
    }

    #[test]
    fn fs_mode_read_verbs_are_read_only_writes_are_not() {
        for ro in [
            "fs.places",
            "fs.list",
            "fs.stat",
            "fs.read_blob",
            "fs.search",
            "fs.trash.list",
        ] {
            assert!(is_read_only_bus_verb(ro), "{ro} should be read-only");
            assert!(is_delegated_safe_bus_verb(ro));
        }
        for rw in [
            "fs.mkdir",
            "fs.touch",
            "fs.write",
            "fs.copy",
            "fs.move",
            "fs.trash",
            "fs.trash.restore",
            "fs.trash.empty",
            "fs.delete",
        ] {
            assert!(
                !is_read_only_bus_verb(rw),
                "{rw} must be treated as mutating"
            );
            assert!(is_delegated_safe_bus_verb(rw));
        }
    }

    #[test]
    fn resolve_target_service_explicit_wins_else_derives_prefix() {
        // Explicit bus-svc always wins (the way filesd-<corpus> instances are named).
        assert_eq!(
            resolve_target_service(
                Some("filesd-notes"),
                &verbs(&["filesd.list", "filesd.save"])
            ),
            Some("filesd-notes".to_string())
        );
        // No explicit → derive from the verbs' shared prefix (maild compat).
        assert_eq!(
            resolve_target_service(
                None,
                &verbs(&["maild.vtoken.list_opaque", "maild.vtoken.mint_opaque"])
            ),
            Some("maild".to_string())
        );
        // Mixed prefixes with no explicit service → ambiguous → None (reject).
        assert_eq!(
            resolve_target_service(None, &verbs(&["maild.vtoken.list_opaque", "filesd.list"])),
            None
        );
        // Empty explicit string is ignored (treated as absent → derive).
        assert_eq!(
            resolve_target_service(Some(""), &verbs(&["filesd.list"])),
            Some("filesd".to_string())
        );
        // Empty verb set → None.
        assert_eq!(resolve_target_service(None, &verbs(&[])), None);
        // fs-mode: an explicit `filesd-fs` pin works…
        assert_eq!(
            resolve_target_service(Some("filesd-fs"), &verbs(&["fs.list", "fs.move"])),
            Some("filesd-fs".to_string())
        );
        // …but an UNPINNED fs.* route must be rejected (the prefix `fs` is never a
        // real service), so the misconfiguration fails closed rather than routing to
        // a phantom `fs` target.
        assert_eq!(
            resolve_target_service(None, &verbs(&["fs.list", "fs.stat"])),
            None
        );
        assert_eq!(resolve_target_service(Some(""), &verbs(&["fs.list"])), None);
    }
}
