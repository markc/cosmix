//! Bus integration for cosmix-maild.
//!
//! Phase 1: broker registration + dispatch skeleton.
//! Phase 2: `maild.rules.*` actions (reload, stats, explain) — see
//! `rules` submodule.
//! Phases 3+: `maild.bayesian.*` actions and the `maild.verdict` topic
//! per `_doc/2026-05-01-cosmix-maild-bus-plan.md`.
//!
//! Broker registration is non-blocking and retrying: if the broker is
//! unreachable at startup — or drops the connection mid-life — maild
//! keeps serving SMTP, IMAP, and JMAP while the Bus surface reconnects
//! with exponential backoff. The shape mirrors `cosmix-webd::bus` and
//! `cosmix-dnsd::bus` so initial connect races and later broker restarts
//! do not leave maild permanently invisible to Bus until process restart.

pub mod accounts;
pub mod bayesian;
pub mod dkim;
pub mod props_publisher;
pub mod retention;
pub mod rules;
pub mod search;
pub mod stats;
pub mod subscribe_granter;
pub mod tls;
pub mod verdict;
pub mod vtoken;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_maild_bayesian::DefaultClassifier;
use cosmix_maild_rules::DefaultRuleEngine;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::namespace::PeerIdentity;
use cosmix_props::runtime::Runtime;
use tokio::sync::broadcast;

use crate::bus::verdict::VerdictEvent;
use crate::db;
use crate::mailstore::SqliteMailStore;
use crate::rule_stats::RuleStats;

/// Resolve a JSON args value from an incoming Bus command in the spec
/// order: `args:` header first (the documented carrier for command
/// parameters), then a non-empty body parsed as JSON, then the
/// already-parsed `cmd.args` (body-derived convenience field on
/// `IncomingCommand`). Returns `Value::Null` if nothing usable is
/// present — callers that need a populated object will surface the
/// shape mismatch in their request deserialization. Shared by every
/// `maild.*` action module so the wire contract stays uniform.
pub(super) fn resolve_args(cmd: &IncomingCommand) -> serde_json::Value {
    if let Some(raw) = cmd.header("args")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(raw)
    {
        return v;
    }
    if !cmd.body.is_empty()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&cmd.body)
    {
        return v;
    }
    cmd.args.clone()
}

/// Strict variant of [`resolve_args`] that surfaces parse errors
/// instead of silently dropping them. Returns the same resolved
/// value as `resolve_args` on success; returns `Err(reason)` when
/// either the `args:` header or a non-empty body is present but is
/// not valid JSON, so the verb can reply `rc=10` instead of
/// running with the default request shape.
///
/// Resolution order is the same as `resolve_args`: `args:` header,
/// then body, then the body-derived `cmd.args` convenience field.
/// Only the first source actually present is consulted; if that
/// source fails to parse, the helper short-circuits with an error
/// — falling through to the next layer would let a malformed
/// header silently mask its own contents.
pub(super) fn try_resolve_args(cmd: &IncomingCommand) -> Result<serde_json::Value, String> {
    if let Some(raw) = cmd.header("args") {
        return serde_json::from_str::<serde_json::Value>(raw)
            .map_err(|e| format!("args header: {e}"));
    }
    if !cmd.body.is_empty() {
        return serde_json::from_str::<serde_json::Value>(&cmd.body)
            .map_err(|e| format!("body: {e}"));
    }
    Ok(cmd.args.clone())
}

/// rc=10 is the unknown-action / caller-error sentinel: `NodedClient::call`
/// only surfaces `rc >= 10` as an error, so a smaller rc would let typos
/// and failed action calls return as `Ok(...)` and mask the failure at
/// the caller. The `rules` submodule uses the same value for parse /
/// validation errors so the wire contract stays uniform.
const RC_UNKNOWN_ACTION: u8 = 10;

/// Bus service name — SPEC-10 R6: the daemon name minus `cosmix-`
/// (`cosmix-maild` → `maild`).
const BUS_SERVICE: &str = "maild";

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A session is "healthy" once it has stayed up for this long. A
/// disconnect after a healthy session resets backoff to
/// [`INITIAL_BACKOFF`]; a disconnect before it does NOT reset — the
/// backoff continues growing from its prior value (capped at
/// [`MAX_BACKOFF`]) so a broker that accepts-then-drops can't drive
/// a tight reconnect/log loop.
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

/// Connect to the broker and run the dispatch loop. Spawn this once at
/// startup with `tokio::spawn(bus::run(...))`. The handler holds shared
/// references to the rule engine, stats counters, classifier, hostname
/// (for the explain action's synthesized auth header), props router,
/// subscribe granter, verdict broadcast, sqlite-backed `db::Db` +
/// `SqliteMailStore`, and the substrate `Runtime` over the
/// `maild.account_overrides` namespace (the explain handler reads this
/// to resolve per-account overrides; see [`rules::dispatch`]).
///
/// **Retry-with-backoff governs both initial connect and mid-life
/// disconnect**. On `incoming_async` stream end we log and fall back
/// into the connect loop rather than exiting `run`; the daemon never
/// gives up re-registering as long as `bus::run` is alive. Mail serving
/// runs in sibling tasks and is unaffected by broker availability.
///
/// Backoff state is preserved across reconnects. It is only reset to
/// [`INITIAL_BACKOFF`] after a session lasted at least
/// [`HEALTHY_SESSION_THRESHOLD`] — a broker that accepts registration
/// and immediately drops us therefore keeps growing the delay rather
/// than driving a tight reconnect/log loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rule_engine: Arc<DefaultRuleEngine>,
    rule_stats: Arc<RuleStats>,
    classifier: Arc<DefaultClassifier>,
    verdict_tx: broadcast::Sender<VerdictEvent>,
    hostname: String,
    props_router: Arc<PropsRouter>,
    subscribe_granter: Arc<subscribe_granter::NodedSubscribeGranter>,
    db: db::Db,
    mailstore: Arc<SqliteMailStore>,
    overrides_runtime: Arc<Runtime>,
    accounts_runtime: Arc<Runtime>,
    dkim_state: dkim::DkimState,
    tls_state: tls::TlsReloadState,
    stats_state: stats::StatsState,
    retention_state: retention::RetentionBusState,
    vtoken_state: vtoken::VtokenBusState,
    bayesian_state: bayesian::BayesianBusState,
) {
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
    let mut dispatchers_spawned = false;
    let broker_handle = subscribe_granter.broker_handle();

    loop {
        let client = connect_with_backoff(&mut delay, &prov).await;
        let client = Arc::new(client);
        // Install the broker handle into the `NodedSubscribeGranter`
        // that PropsRouter already holds. BEFORE this call, watch verbs
        // that require live delivery return `rc=10 grant_failed`; AFTER
        // it they reach noded's `noded.props.subscribe_grant`. Install
        // BEFORE `dispatch_loop` accepts traffic so the first inbound
        // `maild.props.watch` already finds the client wired.
        subscribe_granter.install_client(client.clone());

        if !dispatchers_spawned {
            // Verdict publisher is daemon-lifetime and reads the shared
            // broker handle per publish, so it automatically follows
            // reconnects and only needs to be spawned once.
            verdict::spawn_publisher_task(broker_handle.clone(), verdict_tx.subscribe());

            // SPEC 12 §5.5 / §10 — publisher half of the live fan-out.
            // One dispatcher per registered property runtime; the
            // publisher is shared via `Arc` and reads the broker
            // handle on every publish. Spawning happens BEFORE the
            // dispatch_loop accepts requests so the first commit on the
            // very first poll already has a live tail. The substrate's
            // per-runtime `events_signal`
            // (`tokio::sync::Notify`) closes the drain-then-park race;
            // commits that fire while the dispatcher is mid-publish
            // store a permit and wake it the next iteration.
            //
            // End-to-end "join live" is reachable through the watch
            // verbs: `<svc>.props.watch` / `.audit.watch` (C4/C5)
            // authorize the broker subscription first via the
            // `SubscribeGranter` (C10c) bound to the
            // `NodedSubscribeGranter` (C10d) wired in `run` above,
            // then snapshot the post-grant cursor and return the
            // bounded replay tail. The grant-then-observed order is
            // load-bearing: any event with `nseq > observed` is on
            // the live side of the replay/live split — eligible for
            // best-effort live delivery by this dispatcher; any event
            // with `nseq ≤ observed` is in the watch reply's replay.
            // The dispatcher's records.changed / audit projections and
            // the watch replay projections share the same byte shape
            // (the `project_records_changed` / `project_audit_row`
            // helpers in `cosmix_props::bus::mutation`), so a
            // client resuming from a stale `since_nseq` cannot
            // distinguish events delivered live from events delivered
            // via replay.
            //
            // Handles are intentionally not held — dispatchers are
            // daemon-lifetime tasks, and the `bus::run` future is
            // itself daemon-lifetime, so process exit reclaims them;
            // explicit shutdown is reserved for tests where the
            // runtime/store outlive the dispatcher.
            let publisher: Arc<dyn cosmix_props::DispatchPublisher> = Arc::new(
                props_publisher::NodedPropsPublisher::new(broker_handle.clone()),
            );
            for runtime in props_router.iter_runtimes() {
                // Dropping the returned handle detaches the task —
                // tokio keeps it running, and no caller-side cleanup
                // hook is needed because process exit reaps it.
                //
                // Initial cursor capture goes through `store().list(...)`,
                // which can fail (sqlite locked, IO error). On failure
                // we log and skip *this* namespace; sibling namespaces
                // still register dispatchers and the SMTP/JMAP paths
                // continue unaffected. Subscribers can still recover
                // via the replay verbs on the affected namespace, so
                // this matches the "Bus surface degraded, not down"
                // posture used elsewhere in this module.
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
        match dispatch_loop(
            client.clone(),
            rule_engine.clone(),
            rule_stats.clone(),
            classifier.clone(),
            hostname.clone(),
            props_router.clone(),
            db.clone(),
            mailstore.clone(),
            overrides_runtime.clone(),
            accounts_runtime.clone(),
            dkim_state.clone(),
            tls_state.clone(),
            stats_state.clone(),
            retention_state.clone(),
            vtoken_state.clone(),
            bayesian_state.clone(),
        )
        .await
        {
            DispatchLoopExit::ReceiverUnavailable => {
                tracing::error!(
                    service = BUS_SERVICE,
                    "incoming_async returned None — receiver already taken; \
                     closing client and reconnecting"
                );
                subscribe_granter.clear_client();
                client.close().await;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_BACKOFF);
                continue;
            }
            DispatchLoopExit::StreamEnded => {}
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
            "Bus incoming stream ended; reconnecting with backoff \
             (mail serving continues)"
        );
        subscribe_granter.clear_client();
        client.close().await;
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
                     (mail serving continues)"
                );
                tokio::time::sleep(*delay).await;
                *delay = (*delay * 2).min(MAX_BACKOFF);
            }
        }
    }
}

enum DispatchLoopExit {
    ReceiverUnavailable,
    StreamEnded,
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_loop(
    client: Arc<NodedClient>,
    rule_engine: Arc<DefaultRuleEngine>,
    rule_stats: Arc<RuleStats>,
    classifier: Arc<DefaultClassifier>,
    hostname: String,
    props_router: Arc<PropsRouter>,
    db: db::Db,
    mailstore: Arc<SqliteMailStore>,
    overrides_runtime: Arc<Runtime>,
    accounts_runtime: Arc<Runtime>,
    dkim_state: dkim::DkimState,
    tls_state: tls::TlsReloadState,
    stats_state: stats::StatsState,
    retention_state: retention::RetentionBusState,
    vtoken_state: vtoken::VtokenBusState,
    bayesian_state: bayesian::BayesianBusState,
) -> DispatchLoopExit {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => {
            return DispatchLoopExit::ReceiverUnavailable;
        }
    };

    while let Some(cmd) = rx.recv().await {
        let (rc, body) = if let Some(action) = cmd.command.strip_prefix("maild.rules.") {
            rules::dispatch(
                action,
                &cmd,
                &rule_engine,
                &rule_stats,
                &hostname,
                &overrides_runtime,
                &db,
            )
            .await
        } else if let Some(action) = cmd.command.strip_prefix("maild.bayesian.") {
            bayesian::dispatch(action, &cmd, &classifier, &db, &mailstore, &bayesian_state).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.accounts.") {
            accounts::dispatch(action, &cmd, &db, &mailstore, &accounts_runtime).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.search.") {
            search::dispatch(action, &cmd, &db, &mailstore).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.stats.") {
            stats::dispatch(action, &cmd, &db, &mailstore, &stats_state).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.retention.") {
            retention::dispatch(action, &cmd, &retention_state).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.vtoken.") {
            vtoken::dispatch(action, &cmd, &vtoken_state).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.dkim.") {
            dkim::dispatch(action, &cmd, &dkim_state).await
        } else if let Some(action) = cmd.command.strip_prefix("maild.tls.") {
            tls::dispatch(action, &cmd, &tls_state).await
        } else if let Some(suffix) = cmd.command.strip_prefix("maild.props.") {
            dispatch_props(&props_router, suffix, &cmd).await
        } else {
            (RC_UNKNOWN_ACTION, unknown_action_body(&cmd.command))
        };
        if let Err(e) = client.respond(&cmd, rc, &body).await {
            tracing::warn!(error = %e, command = %cmd.command, "Bus response send failed");
        }
    }
    DispatchLoopExit::StreamEnded
}

/// Bridge an [`IncomingCommand`] into the [`PropsRouter`]'s
/// [`BusMessage`] + [`PeerIdentity`] dispatch surface and project the
/// returned [`cosmix_props::bus::PropsResponse`] to the `(rc, body)`
/// tuple `NodedClient::respond` expects.
///
/// PeerIdentity carries the registered sender as `service_name` so the
/// substrate's auth policy and the broker-side reservation gate
/// (SPEC 12 §15.5, landed in C6) see the same identity. Unix peer
/// credentials and WireGuard fields stay `None` here — those land
/// when noded exposes its peer-identity surface to clients.
async fn dispatch_props(router: &PropsRouter, suffix: &str, cmd: &IncomingCommand) -> (u8, String) {
    let mut msg = cosmix_bus::bus::BusMessage::new();
    for (k, v) in &cmd.headers {
        msg.set(k, v);
    }
    msg.body = cmd.body.clone();
    let peer = PeerIdentity {
        service_name: if cmd.from.is_empty() {
            None
        } else {
            Some(cmd.from.clone())
        },
        ..Default::default()
    };
    let resp = router.dispatch(suffix, &msg, &peer).await;
    // PropsResponse.rc is the §9 numeric rc (0 / 10 / 20). NodedClient
    // expects u8; both 10 and 20 fit and surface as errors above the
    // rc>=10 threshold. Clamp the i32→u8 cast.
    let rc: u8 = u8::try_from(resp.rc.max(0)).unwrap_or(RC_UNKNOWN_ACTION);
    (rc, resp.body)
}

fn unknown_action_body(command: &str) -> String {
    serde_json::json!({ "error": format!("unknown action: {command}") }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_action_rc_is_in_error_range() {
        // NodedClient::call only surfaces rc >= 10 as an error. If the
        // unknown-action fallback ever drops below that threshold,
        // callers will silently see Ok({"error": ...}) instead of an
        // error and typos in action names will go undetected. This test
        // exists precisely to fail if someone edits RC_UNKNOWN_ACTION
        // below the threshold; the constant assertion is intentional.
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(
                RC_UNKNOWN_ACTION >= 10,
                "rc={RC_UNKNOWN_ACTION} would be treated as success by NodedClient::call"
            );
        }
    }

    #[test]
    fn unknown_action_body_is_valid_json_with_error_field() {
        let body = unknown_action_body("maild.bogus");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed.get("error").and_then(|v| v.as_str()),
            Some("unknown action: maild.bogus"),
        );
    }

    #[test]
    fn unknown_action_body_escapes_quotes_in_command_name() {
        // Defense in depth: a malformed command name with a quote
        // must not break the JSON shape (serde_json handles this,
        // but pin the behaviour against the format!-based version
        // we considered earlier).
        let body = unknown_action_body(r#"weird"name"#);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("error").is_some());
    }
}
