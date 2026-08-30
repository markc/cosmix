//! Bus integration for cosmix-webd.
//!
//! Per [`_decisions/all-daemons-bus-citizens.md`] +
//! [`_doc/planned/cosmix-webd-bus-citizenship.md`], webd is a full Bus
//! citizen by default. Deps are unconditional (no feature gate — webd
//! is the named exception). The retry-with-backoff connect shape is
//! adopted from day one so webd never inherits the dnsd one-shot trap:
//! initial connect failures and mid-life stream ends both fall back
//! into the same backoff loop.
//!
//! **v0 verb surface — read-only by construction:**
//!
//! * `webd.routes.list` — vhost map snapshot (per-vhost
//!   primary FQDN, aliases, presence-of CMS / JMAP / WS / docs).
//! * `webd.stats` — per-vhost response-class counters
//!   ([`crate::stats::WebdStats`]).
//! * `webd.tls.status` — manual-PEM identity hostnames + ACME plan
//!   summary + per-vhost issuance state (read via the watch mirror
//!   in [`crate::tls_status`]).
//! * `webd.autoconfig.served_domains` — the `served_mail_domains`
//!   allowlist (the autoconfig admission gate).
//!
//! Anything else returns `rc=10` (the caller-error sentinel). v1 write
//! plumbing is deferred — see the plan's "Why no writes in v0".
//!
//! **Goal-(c)-equivalent:** the HTTPS / HTTP serve loops and the C5d
//! provisioner run in sibling tokio tasks; broker outages affect only
//! the Bus surface, never request serving or cert renewal.

pub mod autoconfig;
pub mod listener_verbs;
pub mod props_publisher;
pub mod routes;
pub mod session_verbs;
pub mod stats;
pub mod subscribe_granter;
pub mod tls;
pub mod vhost_verbs;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmix_client::{IncomingCommand, NodedClient};

use crate::NodeState;

/// `rc=10` is the caller-error sentinel: `NodedClient::call` only
/// surfaces `rc >= 10` as an error, so a smaller rc would let a
/// typo'd / write-shaped action return `Ok(...)` and mask the failure
/// at the caller. Same value + rationale as `cosmix-maild` and
/// `cosmix-dnsd` so the mesh-wide wire contract stays uniform.
const RC_CALLER_ERROR: u8 = 10;

/// Bus service name — SPEC-10 R6: the daemon name minus `cosmix-`
/// (`cosmix-webd` → `webd`).
const BUS_SERVICE: &str = "webd";

/// Read a string kwarg by name from either the JSON `args` object OR the
/// Bus `header` map — **args first**, header only as a fallback.
///
/// A bare Mix `send verb k=v` places the operator's kwargs in the command's
/// `args` map, so a header-only read silently misses `fqdn=` / `email=` /
/// `id=` and the verb reports "missing required kwarg" for a call the
/// operator plainly made (2026-07: bit `webd.session.revoke`, then swept
/// across the vhost/listener verbs).
///
/// **Args must win over headers** (not the reverse): noded stamps reserved
/// transport headers — notably `id`, the message correlation id
/// (`noded-<seq>`) — into the header map, and a header-first read let that
/// ambient `id` SHADOW an operator's `id=` kwarg. That was caught by a live
/// operator-cap probe of `webd.listener.enable id=…`, which read the
/// message id instead of the listener id (and made `listener.status id=…`
/// silently match nothing rather than filter). The explicit operator arg is
/// the intent; a same-named header is transport noise. The header fallback
/// survives only for a hypothetical caller that passes a kwarg as a genuine
/// header. A non-string args value (bool, number) is treated as absent here
/// — [`kwarg_bool`] handles the one bool-typed kwarg. Empty strings are
/// absent so `fqdn=` can't bind.
pub(crate) fn kwarg(cmd: &IncomingCommand, key: &str) -> Option<String> {
    if let Some(s) = cmd
        .args
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(s.to_string());
    }
    cmd.header(key)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// First matching string kwarg from a list of candidate keys (each tried
/// header-then-args via [`kwarg`]). Used for the dotted/underscored
/// aliases (`acme.provider` ≡ `acme_provider`).
pub(crate) fn kwarg_any(cmd: &IncomingCommand, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| kwarg(cmd, k))
}

/// Read a boolean kwarg tolerant of both wire shapes: a header/args
/// STRING (`"true"`/`"false"`/`"1"`/`"0"`) or a JSON `args` BOOL (a Mix
/// `enabled=true` sends a real bool). `Ok(None)` = absent (caller
/// applies its default); `Err(value)` = present but unparseable (empty,
/// a stray string, a number), for a clean caller error.
///
/// Args win over headers (see [`kwarg`]): an operator's explicit `enabled`
/// beats any ambient same-named header. A PRESENT args value is
/// authoritative — a real JSON bool, or a string parsed strictly (empty /
/// garbage → `Err`, so `enabled=` stays a caller error). A JSON `null` or
/// an absent key falls back to the header, which is likewise strict.
pub(crate) fn kwarg_bool(cmd: &IncomingCommand, key: &str) -> Result<Option<bool>, String> {
    match cmd.args.get(key) {
        Some(serde_json::Value::Bool(b)) => return Ok(Some(*b)),
        Some(serde_json::Value::String(s)) => return parse_bool_kwarg(s),
        Some(serde_json::Value::Null) | None => {} // fall through to header
        Some(other) => return Err(other.to_string()),
    }
    match cmd.header(key) {
        Some(h) => parse_bool_kwarg(h),
        None => Ok(None),
    }
}

/// Strict `true`/`false`/`1`/`0` → bool; anything else (incl. empty) is
/// `Err(value)`.
fn parse_bool_kwarg(s: &str) -> Result<Option<bool>, String> {
    match s {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        other => Err(other.to_string()),
    }
}

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A session is "healthy" once it has stayed up for this long. A
/// disconnect after a healthy session resets backoff to
/// [`INITIAL_BACKOFF`]; a disconnect before it does NOT reset — the
/// backoff continues growing from its prior value (capped at
/// [`MAX_BACKOFF`]) so a broker that accepts-then-drops can't drive
/// a tight reconnect/log loop.
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

/// Connect to the broker and run the read-only dispatch loop. Spawn
/// once at startup with `tokio::spawn(bus::run(node.clone()))`. Holds
/// a shared reference to the live [`NodeState`] (for vhost map +
/// per-vhost [`crate::stats::WebdStats`] reads).
///
/// **Retry-with-backoff governs both initial connect and mid-life
/// disconnect** (the dnsd-trap fix taken verbatim from
/// [`_doc/planned/cosmix-dnsd-broker-reregister.md`]). On
/// `incoming_async` stream end we log and fall back into the connect
/// loop rather than exiting `run`; the daemon never gives up
/// re-registering as long as `bus::run` is alive.
///
/// Backoff state is preserved across reconnects. It is only reset to
/// [`INITIAL_BACKOFF`] after a session lasted at least
/// [`HEALTHY_SESSION_THRESHOLD`] — a broker that accepts registration
/// and immediately drops us therefore keeps growing the delay rather
/// than driving a tight reconnect/log loop.
pub async fn run(node: Arc<NodeState>) {
    // Built ONCE so started_at is the true process start and survives the
    // reconnect loop (re-sent on every register). Version-discovery contract.
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    let mut delay = INITIAL_BACKOFF;
    // Dispatcher spawn vs. broker-handle refresh have different
    // cadences:
    //
    //   * `cosmix_props::spawn_dispatcher` returns a daemon-lifetime
    //     task that owns its own Notify drain; a second dispatcher
    //     against the same runtime would double-publish every
    //     commit. So we spawn dispatchers **once**, on the first
    //     successful connect, and never again — gated by `dispatchers_spawned`.
    //
    //   * The granter + publisher share a refreshable
    //     `Arc<ArcSwapOption<NodedClient>>` (`node.broker_handle`).
    //     On **every** successful connect we `store(Some(client))`
    //     and on every session-end we `store(None)` so publishes
    //     during the reconnect gap fail fast with a typed error
    //     rather than calling into a closed Arc and waiting on
    //     I/O timeouts.
    //
    // The dispatchers themselves hold an `Arc<dyn DispatchPublisher>`
    // whose inner `NodedPropsPublisher` reads `broker_handle.load_full()`
    // on every publish — so the once-spawned tasks naturally pick up
    // the refreshed client without re-spawning.
    let mut dispatchers_spawned = false;
    loop {
        let client = connect_with_backoff(&mut delay, &prov).await;
        let client_arc = Arc::new(client);
        // Refresh the shared broker handle — the granter and the
        // publisher both load from this cell, so a single store
        // re-arms watch grants and live publishes for the new
        // session.
        node.props_subscribe_granter
            .install_client(client_arc.clone());

        if !dispatchers_spawned {
            // SPEC 12 §5.5 / §10 — publisher half of the live fan-out.
            // One dispatcher per registered property runtime; the
            // publisher is shared via `Arc` and reads the broker
            // handle on every publish (so reconnects propagate
            // automatically — see module-level commentary on
            // `NodedPropsPublisher`).
            //
            // Spawning happens BEFORE the dispatch loop accepts
            // requests so the first commit on the very first poll
            // already has a live tail.
            //
            // Handles are intentionally not held — dispatchers are
            // daemon-lifetime tasks and `bus::run` is itself
            // daemon-lifetime, so process exit reclaims them.
            let publisher: Arc<dyn cosmix_props::DispatchPublisher> = Arc::new(
                props_publisher::NodedPropsPublisher::new(node.broker_handle.clone()),
            );
            for runtime in node.props_router.iter_runtimes() {
                match cosmix_props::spawn_dispatcher(runtime.clone(), Arc::clone(&publisher)).await
                {
                    Ok(_detached) => {
                        tracing::info!(
                            namespace = %runtime.spec().name.as_str(),
                            "spawned props fan-out dispatcher",
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            namespace = %runtime.spec().name.as_str(),
                            error = %e,
                            "props fan-out dispatcher startup failed; \
                             live records.changed/audit topics will not \
                             fire for this namespace — subscribers must \
                             use the replay verbs",
                        );
                    }
                }
            }
            dispatchers_spawned = true;
        }
        let session_started = Instant::now();
        let mut rx = match client_arc.incoming_async().await {
            Some(rx) => rx,
            None => {
                tracing::error!(
                    service = BUS_SERVICE,
                    "incoming_async returned None — receiver already taken; \
                     closing client and reconnecting"
                );
                // `Drop` does not abort the reader or close the
                // socket; `close().await` does, so the broker reaps
                // the registered name immediately rather than holding
                // an orphan connection until the next inbound frame.
                //
                // Clear the shared broker handle so publishes during
                // the reconnect gap fail fast with a typed
                // `client_unavailable` error instead of racing a
                // closed Arc through `send_with_headers`.
                node.props_subscribe_granter.clear_client();
                client_arc.close().await;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        while let Some(cmd) = rx.recv().await {
            let (rc, body) = if let Some(suffix) = cmd.command.strip_prefix("webd.props.") {
                // SPEC-12 verb surface. PropsRouter::dispatch handles
                // its own auth check + error projection — anything not
                // in the dispatch arms (typo, missing namespace) comes
                // back as a `not_found` or `validation_error` from the
                // router rather than the ergonomic-side
                // unknown-action sentinel.
                crate::vhosts_namespace::dispatch_props(&node.props_router, suffix, &cmd).await
            } else if let Some(ergonomic_suffix) = cmd.command.strip_prefix("webd.") {
                // C5 ergonomic verbs surface — `vhost.add` /
                // `vhost.remove` / `vhost.list` / `acme.renew` /
                // `acme.status`. Returns `Some((rc, body))` for one
                // of those five suffixes; `None` for anything else
                // (e.g. `webd.routes.list`, typo, unknown action),
                // which falls through to the synchronous `dispatch`
                // below so the read-only-by-construction shape +
                // `rc=10` unknown-action sentinel keep working
                // unchanged for the non-vhost verbs.
                // Try the vhost verbs, then the P3 listener verbs
                // (`listener.{enable,disable,status}`); fall through to
                // the synchronous `dispatch` for anything else.
                match vhost_verbs::dispatch(ergonomic_suffix, &cmd, &node).await {
                    Some(result) => result,
                    None => match listener_verbs::dispatch(ergonomic_suffix, &cmd, &node).await {
                        Some(result) => result,
                        // B2 — the mutating `tls.reload` verb (async:
                        // file IO + reload lock). Read-only `tls.status`
                        // returns `None` here and falls through to the
                        // synchronous `dispatch` below, unchanged.
                        None => match tls::dispatch(ergonomic_suffix, &cmd, &node).await {
                            Some(result) => result,
                            // `session.revoke` — cookie-path revocation
                            // (2026-07 audit); async (per-vhost DB locks).
                            None => {
                                match session_verbs::dispatch(ergonomic_suffix, &cmd, &node).await {
                                    Some(result) => result,
                                    None => dispatch(&cmd, &node),
                                }
                            }
                        },
                    },
                }
            } else {
                dispatch(&cmd, &node)
            };
            if let Err(e) = client_arc.respond(&cmd, rc, &body).await {
                tracing::warn!(
                    error = %e,
                    command = %cmd.command,
                    "Bus response send failed"
                );
            }
        }
        let session_lifetime = session_started.elapsed();
        if session_lifetime >= HEALTHY_SESSION_THRESHOLD {
            delay = INITIAL_BACKOFF;
        } else {
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        tracing::warn!(
            service = BUS_SERVICE,
            session_lifetime_seconds = session_lifetime.as_secs(),
            next_retry_in_seconds = delay.as_secs(),
            "Bus incoming stream ended; reconnecting with backoff"
        );
        // Same rationale as the `incoming_async None` branch above:
        // publishes during the reconnect gap should fail fast, not
        // race a closed Arc.
        node.props_subscribe_granter.clear_client();
        client_arc.close().await;
        tokio::time::sleep(delay).await;
    }
}

/// Loop until `NodedClient::connect_default` succeeds, advancing the
/// caller's `delay` (1s → 60s exponential, cap at 60s) on each
/// failure. The caller owns the backoff state so it can preserve it
/// across mid-life disconnects — see [`run`] for the session-lifetime
/// heuristic that decides when to reset.
async fn connect_with_backoff(
    delay: &mut Duration,
    prov: &cosmix_bus::RegisterProvenance,
) -> NodedClient {
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(
            BUS_SERVICE,
            prov.clone(),
        )
        .await
        {
            Ok(c) => {
                tracing::info!(service = BUS_SERVICE, "registered as Bus service");
                return c;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_seconds = delay.as_secs(),
                    "broker not available; Bus surface offline until reconnect succeeds \
                     (request serving and ACME renewal continue unaffected)"
                );
                tokio::time::sleep(*delay).await;
                *delay = (*delay * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pure request→(rc, body) router. Read-only by construction: there
/// is no mutation arm — anything that is not one of the listed read
/// actions is a caller error (this includes any future write-shaped
/// `webd.*.set` / `webd.acme.*` verb, which v0 must reject, not
/// silently accept). Split out from [`run`] so it is unit-testable
/// without a broker.
fn dispatch(cmd: &IncomingCommand, node: &Arc<NodeState>) -> (u8, String) {
    match cmd.command.as_str() {
        "webd.routes.list" => (0, routes::list_body(node)),
        "webd.stats" => (0, stats::stats_body(node)),
        "webd.tls.status" => (0, tls::status_body(node)),
        "webd.autoconfig.served_domains" => (0, autoconfig::served_domains_body(node)),
        other => (
            RC_CALLER_ERROR,
            serde_json::json!({
                "error": "unknown or non-read-only action",
                "action": other,
                "read_only_actions": [
                    "webd.routes.list",
                    "webd.stats",
                    "webd.tls.status",
                    "webd.autoconfig.served_domains",
                ],
            })
            .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use tokio::sync::RwLock;

    fn cmd(command: &str) -> IncomingCommand {
        IncomingCommand {
            from: String::new(),
            command: command.to_string(),
            id: None,
            args: serde_json::Value::Null,
            body: String::new(),
            headers: Default::default(),
        }
    }

    /// Build a command carrying `headers` + a JSON-object `args` — the
    /// two kwarg-delivery channels the helpers reconcile.
    fn cmd_kw(headers: &[(&str, &str)], args: serde_json::Value) -> IncomingCommand {
        let mut c = cmd("webd.x");
        for (k, v) in headers {
            c.headers.insert((*k).to_string(), (*v).to_string());
        }
        c.args = args;
        c
    }

    #[test]
    fn kwarg_prefers_args_then_falls_back_to_header() {
        // args-only (the bare `send verb k=v` path — the gap that bit the
        // vhost/listener/session verbs before this sweep).
        assert_eq!(
            kwarg(
                &cmd_kw(&[], serde_json::json!({ "fqdn": "a.test" })),
                "fqdn"
            )
            .as_deref(),
            Some("a.test")
        );
        // header-only fallback.
        assert_eq!(
            kwarg(
                &cmd_kw(&[("fqdn", "b.test")], serde_json::Value::Null),
                "fqdn"
            )
            .as_deref(),
            Some("b.test")
        );
        // both → ARGS wins (the explicit operator kwarg beats a same-named
        // ambient header).
        assert_eq!(
            kwarg(
                &cmd_kw(&[("fqdn", "hdr")], serde_json::json!({ "fqdn": "args" })),
                "fqdn"
            )
            .as_deref(),
            Some("args")
        );
        // empty args string + empty header + absent → None.
        assert_eq!(
            kwarg(&cmd_kw(&[("fqdn", "")], serde_json::json!({})), "fqdn"),
            None
        );
        assert_eq!(
            kwarg(&cmd_kw(&[], serde_json::json!({ "fqdn": "" })), "fqdn"),
            None
        );
        assert_eq!(kwarg(&cmd_kw(&[], serde_json::Value::Null), "fqdn"), None);
    }

    /// Regression for the live-probe finding: noded stamps the message
    /// correlation id into a reserved `id` HEADER, which a header-first read
    /// let shadow an operator's `id=` args kwarg. Args-first must return the
    /// OPERATOR's id, never the transport `noded-<seq>`.
    #[test]
    fn kwarg_id_arg_beats_reserved_message_id_header() {
        let cmd = cmd_kw(
            &[("id", "noded-2244")],            // the ambient message-id header
            serde_json::json!({ "id": "lan" }), // the operator's id=lan
        );
        assert_eq!(kwarg(&cmd, "id").as_deref(), Some("lan"));
    }

    #[test]
    fn kwarg_any_takes_first_present_alias() {
        // dotted absent, underscored present (in args).
        let c = cmd_kw(&[], serde_json::json!({ "acme_provider": "le" }));
        assert_eq!(
            kwarg_any(&c, &["acme.provider", "acme_provider"]).as_deref(),
            Some("le")
        );
        // first candidate wins when both present.
        let c = cmd_kw(
            &[("acme.provider", "dotted")],
            serde_json::json!({ "acme_provider": "under" }),
        );
        assert_eq!(
            kwarg_any(&c, &["acme.provider", "acme_provider"]).as_deref(),
            Some("dotted")
        );
        assert_eq!(
            kwarg_any(&cmd_kw(&[], serde_json::Value::Null), &["a", "b"]),
            None
        );
    }

    #[test]
    fn kwarg_bool_accepts_string_and_json_bool() {
        // JSON bool in args (Mix `enabled=true` sends a real bool).
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[], serde_json::json!({ "enabled": true })),
                "enabled"
            ),
            Ok(Some(true))
        );
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[], serde_json::json!({ "enabled": false })),
                "enabled"
            ),
            Ok(Some(false))
        );
        // String forms via header or args.
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[("enabled", "1")], serde_json::Value::Null),
                "enabled"
            ),
            Ok(Some(true))
        );
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[], serde_json::json!({ "enabled": "false" })),
                "enabled"
            ),
            Ok(Some(false))
        );
        // Absent → None (caller applies its default).
        assert_eq!(
            kwarg_bool(&cmd_kw(&[], serde_json::Value::Null), "enabled"),
            Ok(None)
        );
        // Present but unparseable → Err(value).
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[("enabled", "yes")], serde_json::Value::Null),
                "enabled"
            ),
            Err("yes".to_string())
        );
        // A PRESENT-but-empty header is authoritative and errors — it does
        // NOT silently default (pre-sweep parity: `enabled=` was a caller
        // error, not `enabled=true`).
        assert_eq!(
            kwarg_bool(
                &cmd_kw(&[("enabled", "")], serde_json::Value::Null),
                "enabled"
            ),
            Err(String::new())
        );
        // An args NUMBER is unparseable, not a silent default.
        assert_eq!(
            kwarg_bool(&cmd_kw(&[], serde_json::json!({ "enabled": 1 })), "enabled"),
            Err("1".to_string())
        );
        // An args value beats a same-named header (args-first).
        assert_eq!(
            kwarg_bool(
                &cmd_kw(
                    &[("enabled", "false")],
                    serde_json::json!({ "enabled": true })
                ),
                "enabled"
            ),
            Ok(Some(true))
        );
    }

    /// Minimal `NodeState` for dispatch tests. Empty vhost map, no
    /// MX resolver, no TLS config — all four read verbs return
    /// well-formed JSON over empty inputs, and the unknown-action arm
    /// exercises the `rc=10` invariant without needing any populated
    /// state. The `tls_status_rx` is seeded from a default
    /// [`crate::tls_status::TlsStatusSnapshot`] (no manual identities,
    /// no ACME) — `webd.tls.status` then returns a valid JSON object
    /// describing "this node has no TLS surface."
    fn empty_node() -> Arc<NodeState> {
        let (_tx, rx) =
            tokio::sync::watch::channel(crate::tls_status::TlsStatusSnapshot::default());
        Arc::new(NodeState {
            service_jmap_tokens: Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            login_throttle: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            login_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            // C3b: an empty `VhostDirectory` matches the prior empty-
            // HashMap test shape. None of the `rc=10` / ergonomic-verb
            // tests in this module dispatch through the host router or
            // `webd.routes.list`; an empty directory keeps the body
            // builders' iteration paths exercised (`primaries.iter()`
            // over an empty Vec) without needing test fixture vhosts.
            vhosts: Arc::new(arc_swap::ArcSwap::from(Arc::new(
                crate::vhost_directory::VhostDirectory::empty(),
            ))),
            http_client: reqwest::Client::builder().build().expect("reqwest client"),
            session: Arc::new(crate::session::SessionSealer::ephemeral()),
            served_mail_domains: HashSet::new(),
            mx: None,
            autoconfig_mail_host: None,
            acme_challenges: Arc::new(RwLock::new(HashMap::new())),
            tls_status_rx: rx,
            // The ergonomic-verb dispatch tests below never enter the
            // `webd.props.*` path, so an empty router suffices. The
            // subscribe-granter is similarly unreachable; we still
            // construct one so the struct is well-formed (the
            // `Arc<NodeState>` field is non-`Option`).
            props_router: Arc::new(cosmix_props::PropsRouter::new("webd")),
            props_subscribe_granter: Arc::new(
                crate::bus::subscribe_granter::NodedSubscribeGranter::new(
                    crate::bus::subscribe_granter::new_broker_handle(),
                ),
            ),
            broker_handle: crate::bus::subscribe_granter::new_broker_handle(),
            // These verb-dispatch unit tests cover only the sync
            // `dispatch` function (`webd.routes.list` / `.stats` /
            // `.tls.status` / `.autoconfig.served_domains`), never
            // the async C5 ergonomic verbs. Leave the runtime + lock
            // map + notify unattached; the C5 fixture in
            // `bus::vhost_verbs::tests` wires its own.
            vhosts_runtime: None,
            listeners_runtime: None,
            listeners_operators: Vec::new(),
            vhost_key_locks: None,
            acme_notify: None,
            acme_force_renew_queue: None,
            // These dispatch tests never reach static serving / handlers.
            handlers: Arc::new(arc_swap::ArcSwap::from(Arc::new(
                crate::mix_handler::HandlerTable::default(),
            ))),
            handler_ast_cache: crate::mix_handler::new_ast_cache(),
            // These sync-dispatch tests never reach `tls.reload`.
            tls_reload: None,
        })
    }

    #[test]
    fn known_read_only_verbs_return_rc_zero() {
        let node = empty_node();
        for verb in [
            "webd.routes.list",
            "webd.stats",
            "webd.tls.status",
            "webd.autoconfig.served_domains",
        ] {
            let (rc, body) = dispatch(&cmd(verb), &node);
            assert_eq!(rc, 0, "{verb} must return rc=0");
            // Body must parse as JSON — wire contract.
            let _: serde_json::Value =
                serde_json::from_str(&body).expect("response body is valid JSON");
        }
    }

    #[test]
    fn tls_status_default_shape_is_empty_manual_no_acme() {
        // Snapshot fields must be present and stable on an empty
        // node so consumers can rely on the wire shape without
        // wrapping every field in an Option check.
        let node = empty_node();
        let (_rc, body) = dispatch(&cmd("webd.tls.status"), &node);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert!(
            v.get("manual_identities")
                .and_then(|x| x.as_array())
                .is_some(),
            "manual_identities must be present as an array"
        );
        assert_eq!(
            v["manual_identities"].as_array().unwrap().len(),
            0,
            "no manual identities on an empty node"
        );
        // `acme` is `Option<...>`; serde renders `None` as JSON null,
        // so the key is present with value null — the consumer
        // contract is "the field exists, branch on null".
        assert!(v.get("acme").is_some(), "acme key must be present");
        assert!(v["acme"].is_null(), "acme must be null on a non-ACME node");
    }

    #[test]
    fn autoconfig_served_domains_default_is_empty_count_zero() {
        let node = empty_node();
        let (_rc, body) = dispatch(&cmd("webd.autoconfig.served_domains"), &node);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["count"], 0);
        assert_eq!(v["domains"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unknown_and_write_shaped_actions_are_rejected_read_only() {
        let node = empty_node();
        // Plausible-future *write* verbs that do NOT hit the C5
        // ergonomic surface (those go through `vhost_verbs::dispatch`
        // upstream in `run()`) must be rejected by the sync
        // `dispatch` shim, not silently accepted. `webd.acme.renew`
        // is omitted from this list because it IS a real C5 verb and
        // would otherwise hit the read-only-by-construction trap —
        // its cap-deny / arg-validation paths are covered by V-V
        // tests in [`vhost_verbs::tests`]. The replacement typo
        // (`webd.acme.renew_yesterday`) pins the next plausible-but-
        // typo'd write shape.
        for bad in [
            "webd.routes.set",
            "webd.tls.reload",
            "webd.acme.renew_yesterday",
            "webd.bogus",
            "maild.rules.stats",
        ] {
            let (rc, body) = dispatch(&cmd(bad), &node);
            assert_eq!(rc, RC_CALLER_ERROR, "{bad} must be a caller error");
            let v: serde_json::Value =
                serde_json::from_str(&body).expect("response body is valid JSON");
            assert_eq!(v["action"], bad);
        }
    }

    #[test]
    fn rc_caller_error_is_in_error_range() {
        // NodedClient::call only surfaces rc >= 10 as an error. If
        // RC_CALLER_ERROR ever drops below that threshold, callers
        // will silently see Ok({"error": ...}) instead of an error
        // and typos in action names will go undetected.
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(
                RC_CALLER_ERROR >= 10,
                "rc={RC_CALLER_ERROR} would be treated as success by NodedClient::call"
            );
        }
    }
}
