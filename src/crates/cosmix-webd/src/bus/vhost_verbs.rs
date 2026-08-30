//! C5 — ergonomic Bus verbs for the `webd.vhosts` substrate.
//!
//! Five verbs sit on top of the SPEC-12 `webd.props.*` surface to give
//! operators a single intent per command instead of having to assemble
//! `props.set` / `props.list` / a force-renew kick by hand:
//!
//! * `webd.vhost.add` — assemble flat kwargs into a [`VhostRow`], stamp
//!   `source = "bus_runtime"`, and write via
//!   `Runtime::set_with_origin(.., WriteOrigin::backend())` under the
//!   per-FQDN lock the provisioner's `VhostRemoved` arm also holds.
//!   Capability: `props.write:webd.vhosts`. Backend-origin because
//!   `source` is daemon-owned (see [`crate::vhosts_namespace`]
//!   §"Owner column") — the caller-cap gates the verb invocation, the
//!   storage write goes through the daemon's own service-tier credential.
//! * `webd.vhost.remove` — caller-origin `delete` against the resolved
//!   record key. No daemon-owned-field rule on delete, so a plain
//!   `Runtime::delete` suffices. The provisioner's `after_delete` arm
//!   (`VhostRemoved` event → C4b cleanup) is the only downstream
//!   state-mutation. Capability: `props.write:webd.vhosts`.
//! * `webd.vhost.list` — caller-origin `list`, decorated with a derived
//!   `acme_status` field (`pending` / `valid` / `expiring_soon` /
//!   `failing`) computed from the row's `not_after` and the
//!   provisioner's `vhost_state` (when ACME plans exist). Capability:
//!   `props.read:webd.vhosts`; secret fields gated on `:secrets`.
//! * `webd.acme.renew` — kicks the provisioner's notify channel so the
//!   next sweep ticks immediately, bypassing the renewal-window gate.
//!   Capability: **`webd.acme.renew:webd.vhosts`** (a *narrow* new cap,
//!   not `props.write`). A renew-only operator (e.g. an automated
//!   rotation script) should not also be able to delete vhosts; the
//!   pair pins the cap split. The post-renewal namespace writeback goes
//!   through webd's own service-tier write, not the caller's, so the
//!   cap names "force a renewal" not "perform the writeback."
//! * `webd.acme.status` — returns the provisioner's `vhost_state[fqdn]`
//!   plus the namespace row's `not_after` and the derived
//!   `acme_status`. Capability: `props.read:webd.vhosts`. `last_error`
//!   is redacted (`"<redacted>"`) without `:secrets`.
//!
//! ## Why the verbs live outside the props router
//!
//! The five verbs are ergonomic — they assemble multiple substrate
//! primitives into one operator intent. `vhost.add` must stamp the
//! `source` daemon-owned field, which the caller-cap surface cannot
//! reach; `acme.renew` is a daemon-internal notify kick that has no
//! props.* surface at all. Routing them through the `webd.props.*`
//! dispatcher would either require widening the caller-origin write
//! surface (breaking the daemon-owned-field invariant) or shoehorning
//! a non-substrate kick into the props verb taxonomy (breaking the
//! "props verbs touch substrate state only" mental model).
//!
//! Each verb owns its capability check directly via
//! [`crate::vhosts_namespace::auth_policy`]; the per-FQDN lock is
//! acquired against the same handle the provisioner shares
//! ([`acme_provisioner::FqdnLockMap`]).
//!
//! ## Kwargs convention
//!
//! Mix `send <verb> key=value key2=value2 ...` passes each `key=value`
//! pair as an [`IncomingCommand::headers`] entry. We accept both
//! dotted and underscored forms for the ACME / TLS pairs
//! (`acme.provider` ≡ `acme_provider`) so the spec's canonical example
//! reads naturally while existing operator habits keep working. The
//! dotted form takes precedence when both are present (operator typed
//! the spec form most recently).

use std::sync::Arc;

use cosmix_client::IncomingCommand;
use cosmix_props::{
    Actor, Capability, CapabilitySet, DeleteOpts, MergeMode, PeerIdentity, PropValue, RecordKey,
    SetOpts, StoreError, Version, WriteOrigin,
};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex as TokioMutex;

use super::{kwarg, kwarg_any, kwarg_bool};
use crate::NodeState;
use crate::acme_provisioner::RENEWAL_WINDOW;
use crate::vhosts_namespace::{VhostRow, auth_policy, namespace_name, vhost_row_from_value};

/// Caller-error sentinel — matches [`crate::bus::RC_CALLER_ERROR`].
/// Duplicated here because that constant is private to `bus::mod`; the
/// cap-deny / wire-validation arms in this module need the same shape.
const RC_CALLER_ERROR: u8 = 10;

/// `source` field value the daemon stamps on Bus-runtime vhost writes.
/// Mirrors [`crate::vhosts_bootstrap::SOURCE_BUS_RUNTIME`]'s wire
/// constant; kept local so the verb's intent is self-documenting and a
/// drift between the two stamps surfaces immediately as a namespace
/// hook rejection rather than a silent half-state.
const SOURCE_BUS_RUNTIME: &str = "bus_runtime";

/// Dispatch entry from [`crate::bus::mod`] — `suffix` is the verb name
/// after stripping the `webd.` namespace prefix (e.g. `vhost.add`).
/// Returns the `(rc, body)` shape `NodedClient::respond` expects.
///
/// Read-only by *construction* for any verb the caller doesn't have a
/// cap for; each ergonomic arm owns its own cap check via
/// [`auth_policy`].
pub(crate) async fn dispatch(
    suffix: &str,
    cmd: &IncomingCommand,
    node: &Arc<NodeState>,
) -> Option<(u8, String)> {
    let result = match suffix {
        "vhost.add" => vhost_add(node, cmd).await,
        "vhost.remove" => vhost_remove(node, cmd).await,
        "vhost.list" => vhost_list(node, cmd).await,
        "acme.renew" => acme_renew(node, cmd).await,
        "acme.status" => acme_status(node, cmd).await,
        _ => return None,
    };
    Some(result)
}

/// `webd.vhost.add` — assemble flat kwargs into a [`VhostRow`] and write
/// through the backend-origin path.
///
/// Lock-ordering contract: acquire the per-FQDN lock **before** the
/// `set_with_origin` call (mirroring the provisioner's
/// `VhostRemoved` arm) and hold it through the namespace write so a
/// `vhost.add` racing a `VhostRemoved` cleanup serialises against the
/// same lock identity. See `_doc/planned/webd-vhosts-phase3.md`
/// §"Per-fqdn serialization lock".
async fn vhost_add(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(cmd, "props.write:webd.vhosts") {
        return rsp;
    }

    // Required kwargs.
    let fqdn = match kwarg(cmd, "fqdn") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: fqdn"),
    };
    let www_dir = match kwarg(cmd, "www_dir") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: www_dir"),
    };

    // Optional ACME trio — accept both dotted (spec form) and
    // underscored (DB-column) keys; dotted wins when both supplied.
    let acme_provider = kwarg_any(cmd, &["acme.provider", "acme_provider"]);
    let acme_challenge = kwarg_any(cmd, &["acme.challenge", "acme_challenge"]);
    let acme_contact_email = kwarg_any(cmd, &["acme.contact_email", "acme_contact_email"]);
    // Optional manual-TLS pair.
    let tls_cert_path = kwarg_any(cmd, &["tls.cert_path", "tls_cert_path"]);
    let tls_key_path = kwarg_any(cmd, &["tls.key_path", "tls_key_path"]);
    // Optional `enabled` flag (default true; the namespace hook also
    // defaults this, but stamping it here keeps the wire row complete
    // for the Patch merge).
    let enabled = match kwarg_bool(cmd, "enabled") {
        Ok(Some(b)) => b,
        Ok(None) => true,
        Err(other) => {
            return caller_error(&format!(
                "invalid enabled kwarg {other:?}; expected true/false/1/0",
            ));
        }
    };

    let Some(runtime) = node.vhosts_runtime.as_ref() else {
        return server_error("webd.vhosts runtime not attached — bootstrap node or wiring bug");
    };
    let Some(locks) = node.vhost_key_locks.as_ref() else {
        return server_error("vhost_key_locks not attached — bootstrap node or wiring bug");
    };

    // Acquire the per-FQDN lock identity the provisioner's
    // VhostRemoved arm also acquires. Same lock pattern as
    // `acme_provisioner.rs::handle_vhost_removed` — lookup-or-insert
    // under the outer mutex, clone the inner Arc, drop the outer
    // before awaiting the inner.
    let key_lock = {
        let mut map = locks.lock().await;
        map.entry(fqdn.clone())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    };
    let _key_guard = key_lock.lock().await;

    // Build the wire row. `source` is stamped here (daemon-owned) and
    // every absent column is left out — `MergeMode::Patch` keeps the
    // existing values for any future schema-expansion fields.
    let mut body: BTreeMap<String, PropValue> = BTreeMap::new();
    body.insert("fqdn".into(), PropValue::String(fqdn.clone()));
    body.insert("www_dir".into(), PropValue::String(www_dir));
    body.insert("enabled".into(), PropValue::Bool(enabled));
    body.insert(
        "source".into(),
        PropValue::String(SOURCE_BUS_RUNTIME.into()),
    );
    // Aliases — the namespace hook (rule 3) rejects non-empty; stamp
    // an explicit empty list so a future Patch can't carry a stale
    // alias set from an older row shape.
    body.insert("aliases".into(), PropValue::List(Vec::new()));

    if let Some(p) = acme_provider {
        body.insert("acme_provider".into(), PropValue::String(p));
    }
    if let Some(c) = acme_challenge {
        body.insert("acme_challenge".into(), PropValue::String(c));
    }
    if let Some(e) = acme_contact_email {
        body.insert("acme_contact_email".into(), PropValue::String(e));
    }
    if let Some(c) = tls_cert_path {
        body.insert("tls_cert_path".into(), PropValue::String(c));
    }
    if let Some(k) = tls_key_path {
        body.insert("tls_key_path".into(), PropValue::String(k));
    }

    let ns = namespace_name();
    let key = RecordKey::collection(ns.clone(), fqdn.clone());

    // OCC anchor — `require_version=true` on the spec means a write
    // without `expected_version` is rejected. `webd.vhosts` is a
    // SoftDelete namespace (default), so `get()` would return NotFound
    // for a tombstoned row even though `commit_set` validates against
    // the tombstone version (SPEC 12 §5.4). `version_anchor` is
    // tombstone-aware: live → live version, tombstone → tombstone
    // version, absent → None → Version::zero(). This is what makes
    // `vhost.remove` → `vhost.add` a working operator flow rather than
    // a permanent OCC trap.
    let expected_version = match runtime.store().version_anchor(&key).await {
        Ok(Some(v)) => v,
        Ok(None) => Version::zero(),
        Err(other) => {
            return server_error(&format!("fetching prior vhost row for OCC anchor: {other}"));
        }
    };

    let ts_ms = now_ms();
    let outcome = runtime
        .set_with_origin(
            key,
            PropValue::Object(body),
            SetOpts {
                expected_version: Some(expected_version),
                merge: MergeMode::Patch,
                actor: Actor::service("webd"),
                cause: Some("vhost.add".into()),
                ts_ms,
            },
            WriteOrigin::backend(),
        )
        .await;
    match outcome {
        Ok(_) => (
            0,
            json!({
                "ok": true,
                "fqdn": fqdn,
                "source": SOURCE_BUS_RUNTIME,
            })
            .to_string(),
        ),
        Err(e) => caller_error(&format!("vhost.add failed: {e}")),
    }
}

/// `webd.vhost.remove` — caller-origin `delete`. Fires the namespace's
/// `after_delete` hook → `VhostRemoved` event → C4b cleanup
/// (archive `acme/<fqdn>/live/`, drop the plan, rebuild arc-swaps).
async fn vhost_remove(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(cmd, "props.write:webd.vhosts") {
        return rsp;
    }
    let fqdn = match kwarg(cmd, "fqdn") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: fqdn"),
    };

    let Some(runtime) = node.vhosts_runtime.as_ref() else {
        return server_error("webd.vhosts runtime not attached — bootstrap node or wiring bug");
    };

    let ns = namespace_name();
    let key = RecordKey::collection(ns.clone(), fqdn.clone());

    // OCC anchor — same `require_version=true` rule applies to delete.
    // A missing row returns rc=10 (not server_error) — `vhost.remove`
    // of a non-existent fqdn is operator error, not substrate breakage.
    let expected_version = match runtime.store().get(&key).await {
        Ok(snap) => snap.value.version,
        Err(StoreError::NotFound) => {
            return caller_error(&format!(
                "vhost {fqdn:?} not found in webd.vhosts namespace",
            ));
        }
        Err(other) => {
            return server_error(&format!(
                "fetching vhost row for delete OCC anchor: {other}"
            ));
        }
    };

    let ts_ms = now_ms();
    let outcome = runtime
        .delete_with_origin(
            key,
            DeleteOpts {
                expected_version: Some(expected_version),
                actor: Actor::service("webd"),
                cause: Some("vhost.remove".into()),
                ts_ms,
            },
            // Caller-origin: no daemon-owned-field rule on delete, and
            // the row is going away entirely.
            WriteOrigin::caller(),
        )
        .await;
    match outcome {
        Ok(_) => (
            0,
            json!({
                "ok": true,
                "fqdn": fqdn,
            })
            .to_string(),
        ),
        Err(e) => caller_error(&format!("vhost.remove failed: {e}")),
    }
}

/// `webd.vhost.list` — `runtime.store().list` + per-row `acme_status`
/// decoration. The substrate's secret-redaction layer is bypassed here
/// (we serialise the typed [`VhostRow`] directly), so this verb
/// performs the same redaction inline based on the caller's
/// `:secrets` cap. Operators who need the full substrate envelope
/// (audit nseq, version) should continue to use `webd.props.list`.
async fn vhost_list(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(cmd, "props.read:webd.vhosts") {
        return rsp;
    }
    let with_secrets = has_cap(cmd, "props.read:webd.vhosts:secrets");

    let Some(runtime) = node.vhosts_runtime.as_ref() else {
        return server_error("webd.vhosts runtime not attached — bootstrap node or wiring bug");
    };

    let snap = match runtime.store().list(&namespace_name()).await {
        Ok(s) => s,
        Err(e) => return server_error(&format!("listing webd.vhosts: {e}")),
    };

    let tls_snap = node.tls_status_rx.borrow().clone();
    let now = OffsetDateTime::now_utc().format(&Rfc3339).ok();
    let now_unix = OffsetDateTime::now_utc().unix_timestamp();

    let mut rows_out: Vec<JsonValue> = Vec::with_capacity(snap.value.len());
    for record in snap.value {
        let row = match vhost_row_from_value(&record.value) {
            Ok(r) => r,
            Err(e) => {
                return server_error(&format!(
                    "webd.vhosts row {key:?} failed VhostRow deserialisation: {e}",
                    key = record.key.key,
                ));
            }
        };
        let acme_status = derive_acme_status(&row, &tls_snap, now_unix);
        rows_out.push(project_row(&row, &acme_status, with_secrets));
    }

    (
        0,
        json!({
            "rows": rows_out,
            "count": rows_out.len(),
            "now": now,
        })
        .to_string(),
    )
}

/// `webd.acme.renew` — queue the given fqdn for force-renewal on the
/// next sweep and wake the renewal loop. Returns
/// `{ok:true, state:"pending"}` synchronously; the caller polls
/// `webd.acme.status` (or watches `webd.props.audit.watch`) for the
/// outcome.
///
/// Force-renew bypasses **both** the per-vhost cooldown gate and the
/// 30-day renewal-window gate inside `tick_once`. Disabled-state and
/// apex-policy gates are deliberately NOT bypassed — those represent
/// operator policy, not scheduler timing, and a force-renew of a
/// disabled vhost would be a footgun. A bare notify (without queueing)
/// would only break the interval wait, not the gates — the failure
/// mode that earlier returned `ok:true` while issuance was silently
/// skipped on every tick.
///
/// ## Pre-queue validation
///
/// Before queueing, the verb reads the namespace row internally and
/// rejects four classes of inputs that `tick_once` would silently
/// no-op on:
///
/// * fqdn typo / tombstoned row → `caller_error("vhost {fqdn} not
///   found")` — row is absent (hard-deleted) or soft-deleted.
/// * `enabled=false` → `caller_error("vhost {fqdn} is disabled")` —
///   `tick_once` skips disabled plans by policy.
/// * `acme_provider.is_none()` → `caller_error("vhost {fqdn} has no
///   ACME provider")` — manual-TLS-only or HTTP-only rows produce no
///   plan; the queue entry self-clears on the next tick with no
///   visible error.
///
/// The internal read uses the runtime's store directly (no
/// `props.read` cap on the caller is required) — this is validation
/// under the narrow renew authority, not disclosure. The verb's
/// response carries no row fields beyond the echoed `fqdn`.
async fn acme_renew(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    // Narrow cap — NOT `props.write`. See module docstring.
    if let Err(rsp) = require_cap(cmd, "webd.acme.renew:webd.vhosts") {
        return rsp;
    }
    let fqdn = match kwarg(cmd, "fqdn") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: fqdn"),
    };

    let Some(runtime) = node.vhosts_runtime.as_ref() else {
        return server_error("webd.vhosts runtime not attached — bootstrap node or wiring bug");
    };

    // Internal namespace read for validation. NotFound covers both
    // never-existed (typo) and soft-deleted (tombstoned) — `get()`
    // hides tombstones, which is exactly the failure shape we want to
    // surface as caller_error here.
    let key = RecordKey::collection(namespace_name(), fqdn.clone());
    let row = match runtime.store().get(&key).await {
        Ok(snap) => match vhost_row_from_value(&snap.value.value) {
            Ok(r) => r,
            Err(e) => {
                return server_error(&format!(
                    "row {fqdn:?} failed VhostRow deserialisation: {e}",
                ));
            }
        },
        Err(StoreError::NotFound) => {
            return caller_error(&format!(
                "vhost {fqdn:?} not found in webd.vhosts namespace",
            ));
        }
        Err(other) => {
            return server_error(&format!("fetching webd.vhosts row {fqdn:?}: {other}"));
        }
    };
    if !row.enabled {
        return caller_error(&format!("vhost {fqdn:?} is disabled — force-renew refused",));
    }
    if row.acme_provider.is_none() {
        return caller_error(&format!(
            "vhost {fqdn:?} has no acme_provider — manual-TLS-only or HTTP-only vhost cannot force-renew",
        ));
    }

    let (Some(notify), Some(queue)) = (
        node.acme_notify.as_ref(),
        node.acme_force_renew_queue.as_ref(),
    ) else {
        return server_error("no ACME provisioner attached on this node — renew has no surface");
    };
    queue.lock().await.insert(fqdn.clone());
    notify.notify_one();
    tracing::info!(
        target: "webd::bus::vhost_verbs",
        fqdn = %fqdn,
        "acme.renew: queued for force-renewal + provisioner notified",
    );
    (
        0,
        json!({
            "ok": true,
            "fqdn": fqdn,
            "state": "pending",
        })
        .to_string(),
    )
}

/// `webd.acme.status fqdn=<host>` — provisioner `vhost_state[fqdn]` +
/// namespace `not_after` + derived `acme_status`. `last_error` is
/// redacted (`"<redacted>"`) unless the caller holds `:secrets`.
async fn acme_status(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(cmd, "props.read:webd.vhosts") {
        return rsp;
    }
    let with_secrets = has_cap(cmd, "props.read:webd.vhosts:secrets");

    let fqdn = match kwarg(cmd, "fqdn") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: fqdn"),
    };

    let Some(runtime) = node.vhosts_runtime.as_ref() else {
        return server_error("webd.vhosts runtime not attached — bootstrap node or wiring bug");
    };

    let key = RecordKey::collection(namespace_name(), fqdn.clone());
    let row = match runtime.store().get(&key).await {
        Ok(snap) => match vhost_row_from_value(&snap.value.value) {
            Ok(r) => r,
            Err(e) => {
                return server_error(&format!(
                    "row {fqdn:?} failed VhostRow deserialisation: {e}",
                ));
            }
        },
        Err(StoreError::NotFound) => {
            return caller_error(&format!(
                "vhost {fqdn:?} not found in webd.vhosts namespace",
            ));
        }
        Err(other) => {
            return server_error(&format!("fetching webd.vhosts row {fqdn:?}: {other}"));
        }
    };

    let tls_snap = node.tls_status_rx.borrow().clone();
    let now_unix = OffsetDateTime::now_utc().unix_timestamp();
    let acme_status = derive_acme_status(&row, &tls_snap, now_unix);

    // Vhost provisioner state — `None` on no-ACME nodes, falsy
    // defaults when the row exists but the provisioner hasn't ticked
    // it yet.
    let vhost_state = tls_snap
        .acme
        .as_ref()
        .and_then(|a| a.vhost_state.get(&fqdn).cloned());

    let last_error_field = match (
        with_secrets,
        vhost_state.as_ref().and_then(|s| s.last_error.clone()),
    ) {
        (true, opt) => opt.map(JsonValue::String).unwrap_or(JsonValue::Null),
        (false, Some(_)) => JsonValue::String("<redacted>".into()),
        (false, None) => JsonValue::Null,
    };

    let body = json!({
        "fqdn": fqdn,
        "acme_status": acme_status,
        "not_after": row.not_after,
        "last_attempt": row.last_attempt,
        "last_error_count": vhost_state.as_ref().map(|s| s.last_error_count),
        "last_error": last_error_field,
        "next_attempt_after": vhost_state.as_ref().and_then(|s| s.next_attempt_after_rfc3339.clone()),
        "issued": vhost_state.as_ref().map(|s| s.issued).unwrap_or(false),
    });
    (0, body.to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the [`PeerIdentity`] noded forwards to the broker. Mirrors
/// [`crate::vhosts_namespace::dispatch_props`] — `service_name` carries
/// the registered sender; Unix peer creds + WireGuard fields stay
/// `None` because noded doesn't surface them today.
fn peer_from_cmd(cmd: &IncomingCommand) -> PeerIdentity {
    PeerIdentity {
        service_name: if cmd.from.is_empty() {
            None
        } else {
            Some(cmd.from.clone())
        },
        ..Default::default()
    }
}

/// True iff the caller's resolved capability set contains `cap`.
fn has_cap(cmd: &IncomingCommand, cap: &str) -> bool {
    let peer = peer_from_cmd(cmd);
    let caps = resolve_caps_for_dispatch(&peer);
    caps.contains(&Capability::from(cap.to_string()))
}

/// Resolve the dispatch-time capability set for `peer`.
///
/// In production the answer is `auth_policy("webd").resolve(peer)` —
/// the same default policy the SPEC-12 `webd.props.*` router consults
/// (the `vhosts_namespace::spec()` wires `auth_policy` straight into
/// the `NamespaceSpec`). The ergonomic verbs in this module reach
/// outside the props router (they assemble multiple substrate
/// primitives into one intent), so they perform their own cap
/// resolution against the same policy and the same `PeerIdentity`
/// shape — every cap-gated decision in the daemon goes through the
/// same authoritative source.
///
/// Under `cfg(test)` the V-V cap-split tests need to drive each verb
/// arm with a hand-crafted cap set (no `props.write`, no `:secrets`,
/// the narrow `webd.acme.renew` alone, etc.) without standing up a
/// real peer-identity chain. A thread-local override lets a test
/// install a custom [`AuthPolicy`] that resolves to whatever
/// capability set the assertion needs; `clear_auth_policy_for_test`
/// resets to the default policy at end-of-test. The override is
/// `cfg(test)`-only — production builds compile the direct
/// `auth_policy("webd").resolve(peer)` branch and never read the
/// override.
fn resolve_caps_for_dispatch(peer: &PeerIdentity) -> CapabilitySet {
    #[cfg(test)]
    {
        if let Some(caps) =
            AUTH_POLICY_OVERRIDE.with(|cell| cell.borrow().as_ref().map(|p| p.resolve(peer)))
        {
            return caps;
        }
    }
    auth_policy("webd").resolve(peer)
}

// Thread-local override for `resolve_caps_for_dispatch`. See that
// function's docstring for the design intent. `thread_local!` (not a
// global) so parallel-running `#[tokio::test(flavor = "current_thread")]`s
// don't share state — each test's tokio runtime pins to its own
// thread for the duration of the test.
#[cfg(test)]
thread_local! {
    static AUTH_POLICY_OVERRIDE: std::cell::RefCell<Option<cosmix_props::AuthPolicy>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_auth_policy_for_test(p: cosmix_props::AuthPolicy) {
    AUTH_POLICY_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(p));
}

#[cfg(test)]
fn clear_auth_policy_for_test() {
    AUTH_POLICY_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

/// Cap-check helper — returns `Err((rc, body))` to short-circuit a
/// verb arm with `auth_denied`.
pub(crate) fn require_cap(cmd: &IncomingCommand, cap: &str) -> Result<(), (u8, String)> {
    if has_cap(cmd, cap) {
        Ok(())
    } else {
        Err((
            RC_CALLER_ERROR,
            json!({
                "error": "auth_denied",
                "missing_capability": cap,
            })
            .to_string(),
        ))
    }
}

pub(crate) fn caller_error(msg: &str) -> (u8, String) {
    (
        RC_CALLER_ERROR,
        json!({
            "error": msg,
        })
        .to_string(),
    )
}

fn server_error(msg: &str) -> (u8, String) {
    // Same rc=10 sentinel — `NodedClient::call` doesn't distinguish a
    // server-side failure from a caller error at the wire level (both
    // surface as `Err` above the rc>=10 threshold). The body's
    // `kind: "server"` makes the distinction visible to operators
    // reading the response payload.
    (
        RC_CALLER_ERROR,
        json!({
            "error": msg,
            "kind": "server",
        })
        .to_string(),
    )
}

/// Single millisecond wall-clock stamp threaded into the substrate
/// write. The substrate audit row uses this for `cause_ts`.
fn now_ms() -> i64 {
    let now = OffsetDateTime::now_utc();
    i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Derive `acme_status`:
///
/// * `failing` — provisioner has a non-zero `last_error_count`.
/// * `pending` — ACME mode configured but no `not_after` stamped yet
///   (issued for the first time, not yet successful).
/// * `valid` — `not_after - now > RENEWAL_WINDOW`.
/// * `expiring_soon` — `not_after - now <= RENEWAL_WINDOW`.
/// * `n/a` — manual-TLS or HTTP-only row (no ACME mode at all).
fn derive_acme_status(
    row: &VhostRow,
    tls_snap: &crate::tls_status::TlsStatusSnapshot,
    now_unix: i64,
) -> String {
    if row.acme_provider.is_none() {
        return "n/a".to_string();
    }
    let backoff = tls_snap
        .acme
        .as_ref()
        .and_then(|a| a.vhost_state.get(&row.fqdn))
        .map(|s| s.last_error_count)
        .unwrap_or(0);
    if backoff > 0 {
        return "failing".to_string();
    }
    let Some(not_after_str) = row.not_after.as_ref() else {
        return "pending".to_string();
    };
    let Ok(not_after) = OffsetDateTime::parse(not_after_str, &Rfc3339) else {
        return "pending".to_string();
    };
    let remaining = not_after.unix_timestamp() - now_unix;
    if remaining > RENEWAL_WINDOW.as_secs() as i64 {
        "valid".to_string()
    } else {
        "expiring_soon".to_string()
    }
}

/// Render a [`VhostRow`] to JSON for the verb response, redacting the
/// three secret-annotated fields (`cert_blob_id`, `key_blob_id`,
/// `last_error`) when the caller lacks `:secrets`.
fn project_row(row: &VhostRow, acme_status: &str, with_secrets: bool) -> JsonValue {
    let redact = |v: &Option<String>| -> JsonValue {
        match (with_secrets, v) {
            (true, opt) => opt
                .as_ref()
                .map(|s| JsonValue::String(s.clone()))
                .unwrap_or(JsonValue::Null),
            (false, Some(_)) => JsonValue::String("<redacted>".into()),
            (false, None) => JsonValue::Null,
        }
    };
    json!({
        "fqdn": row.fqdn,
        "enabled": row.enabled,
        "www_dir": row.www_dir,
        "aliases": row.aliases,
        "source": row.source,
        "acme_provider": row.acme_provider,
        "acme_challenge": row.acme_challenge,
        "acme_contact_email": row.acme_contact_email,
        "tls_cert_path": row.tls_cert_path,
        "tls_key_path": row.tls_key_path,
        "cert_blob_id": redact(&row.cert_blob_id),
        "key_blob_id": redact(&row.key_blob_id),
        "not_after": row.not_after,
        "last_attempt": row.last_attempt,
        "last_error_count": row.last_error_count,
        "last_error": redact(&row.last_error),
        "acme_status": acme_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme_provisioner::FqdnLockMap;
    use crate::tls_status::{AcmeStatusSnapshot, AcmeVhostStateSnapshot, TlsStatusSnapshot};
    use crate::vhosts_namespace::register_vhosts_namespace;
    use arc_swap::ArcSwap;
    use cosmix_props::sqlite::SqliteStore;
    use cosmix_props::{AuthPolicy, PropsRouter};
    use rusqlite::Connection;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use tokio::sync::RwLock;

    fn cmd_with_headers(verb: &str, headers: &[(&str, &str)]) -> IncomingCommand {
        let mut hdrs: std::collections::BTreeMap<String, String> = Default::default();
        for (k, v) in headers {
            hdrs.insert((*k).to_string(), (*v).to_string());
        }
        IncomingCommand {
            from: String::new(),
            command: verb.to_string(),
            id: None,
            args: serde_json::Value::Null,
            body: String::new(),
            headers: hdrs,
        }
    }

    /// Build a command whose kwargs arrive in the JSON `args` object with
    /// NO headers — the shape a bare Mix `send verb k=v` actually produces
    /// (2026-07 sweep). Confirms per-verb that the kwarg readers pull from
    /// `args`, not only headers.
    fn cmd_with_args(verb: &str, args: JsonValue) -> IncomingCommand {
        IncomingCommand {
            from: String::new(),
            command: verb.to_string(),
            id: None,
            args,
            body: String::new(),
            headers: Default::default(),
        }
    }

    /// Build a NodeState whose `vhosts_runtime` is wired against an
    /// in-memory SqliteStore + per-FQDN lock map + notify channel.
    /// `auth_policy` is overridden via [`override_auth_policy`] so the
    /// V-V cap-split tests can hand the dispatcher a peer with a
    /// narrowed cap set without going through PeerIdentity.
    async fn build_node_with_caps(caps: &[&str]) -> Arc<NodeState> {
        build_node_with_caps_ops(caps, &[]).await
    }

    /// As [`build_node_with_caps`], but also seeds the L0
    /// `listeners_operators` allowlist — the listener write-cap is granted
    /// only to a peer whose `service_name` is in it (see
    /// `listeners_namespace::auth_policy`), so a test that must reach a
    /// listener MUTATION arm needs both this AND a matching `cmd.from`.
    async fn build_node_with_caps_ops(caps: &[&str], operators: &[&str]) -> Arc<NodeState> {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        let store = Arc::new(SqliteStore::new("webd", conn).expect("store"));
        let mut router = PropsRouter::new("webd");
        let (runtime, _events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");
        // Override the spec's AuthPolicy after registration so the test
        // cap set is what verb dispatch sees. We can't replace the
        // `NamespaceSpec` already wired into the runtime, but the
        // verbs read `vhosts_namespace::auth_policy("webd")` directly
        // — so we don't need to. Instead the verb dispatch's
        // `has_cap` reads the same default policy. For V-V tests we
        // *only* need to test the cap-deny / cap-admit shapes; we do
        // that by stubbing the `auth_policy` call indirectly via
        // [`AUTH_POLICY_OVERRIDE`] (see `set_auth_policy_for_test`).
        set_auth_policy_for_test(custom_policy(caps));

        let (_tx, tls_status_rx) = tokio::sync::watch::channel(TlsStatusSnapshot::default());
        let key_locks: FqdnLockMap = Arc::new(TokioMutex::new(HashMap::new()));
        let notify = Arc::new(tokio::sync::Notify::new());

        Arc::new(NodeState {
            service_jmap_tokens: Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            login_throttle: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            login_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            vhosts: Arc::new(ArcSwap::from(Arc::new(
                crate::vhost_directory::VhostDirectory::empty(),
            ))),
            http_client: reqwest::Client::builder().build().expect("reqwest"),
            session: Arc::new(crate::session::SessionSealer::ephemeral()),
            served_mail_domains: HashSet::new(),
            mx: None,
            autoconfig_mail_host: None,
            acme_challenges: Arc::new(RwLock::new(HashMap::new())),
            tls_status_rx,
            props_router: Arc::new(router),
            props_subscribe_granter: Arc::new(
                crate::bus::subscribe_granter::NodedSubscribeGranter::new(
                    crate::bus::subscribe_granter::new_broker_handle(),
                ),
            ),
            broker_handle: crate::bus::subscribe_granter::new_broker_handle(),
            vhosts_runtime: Some(runtime),
            listeners_runtime: None,
            listeners_operators: operators.iter().map(|s| (*s).to_string()).collect(),
            vhost_key_locks: Some(key_locks),
            acme_notify: Some(notify),
            // Tests that exercise force-renew share the fixture but
            // don't all need a populated queue; `Some(empty)` is the
            // correct default so the verb's
            // `(notify, queue)` both-Some guard passes and the verb
            // exercises the queue-then-notify path end-to-end.
            acme_force_renew_queue: Some(Arc::new(tokio::sync::Mutex::new(
                std::collections::HashSet::new(),
            ))),
            // C5 verb tests don't exercise embedded handlers.
            handlers: Arc::new(ArcSwap::from(Arc::new(
                crate::mix_handler::HandlerTable::default(),
            ))),
            handler_ast_cache: crate::mix_handler::new_ast_cache(),
            tls_reload: None,
        })
    }

    /// Build a CapabilitySet from a slice of cap strings.
    fn caps_from(list: &[&str]) -> CapabilitySet {
        list.iter()
            .map(|s| Capability::from((*s).to_string()))
            .collect()
    }

    /// Build an AuthPolicy that returns the given cap set for any peer.
    fn custom_policy(list: &[&str]) -> AuthPolicy {
        let caps = caps_from(list);
        AuthPolicy::new(move |_peer: &PeerIdentity| caps.clone())
    }

    // `set_auth_policy_for_test` / `clear_auth_policy_for_test` live
    // at parent module scope so `resolve_caps_for_dispatch` (the
    // production hot path under `#[cfg(test)]`) can read the same
    // thread-local. See [`resolve_caps_for_dispatch`].
    use super::{clear_auth_policy_for_test, set_auth_policy_for_test};

    /// Sanity: the default `auth_policy("webd")` grants all 7 caps
    /// declared by [`crate::vhosts_namespace::auth_policy`] — the 6
    /// substrate-shaped caps from C1c plus the C5-added narrow
    /// renew cap. The V-V cap-split tests below depend on the
    /// override returning *only* the listed caps; this test pins the
    /// baseline so a default-policy change can't silently widen the
    /// V-V test admission.
    ///
    /// The renew cap is granted to every WG peer in v0.1, matching
    /// the existing posture for the other webd caps and the
    /// maild Phase 1/2/3 default (`_doc/planned/webd-vhosts-phase3.md`
    /// §"AuthPolicy"). Cross-mesh authz narrows it later without
    /// renaming the cap.
    #[test]
    fn default_auth_policy_lists_all_caps() {
        let caps = auth_policy("webd").resolve(&PeerIdentity::default());
        for w in [
            "props.read:webd.vhosts",
            "props.read:webd.vhosts:secrets",
            "props.describe:webd.vhosts:public",
            "props.describe:webd.vhosts:full",
            "props.audit:webd.vhosts",
            "props.write:webd.vhosts",
            "webd.acme.renew:webd.vhosts",
        ] {
            assert!(
                caps.contains(&Capability::from(w.to_string())),
                "default policy must declare {w}",
            );
        }
        clear_auth_policy_for_test();
    }

    /// V-V — `vhost.add` happy path with full ACME trio. Acquires the
    /// per-fqdn lock, writes through backend-origin, stamps
    /// source=bus_runtime. Verifies the row lands in the namespace
    /// with the stamped source.
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_add_happy_path_writes_bus_runtime_row() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "props.read:webd.vhosts"]).await;
        let cmd = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "p3test.example.com"),
                ("www_dir", "/srv/p3test"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, body) = vhost_add(&node, &cmd).await;
        assert_eq!(rc, 0, "happy path rc=0; body={body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["fqdn"], "p3test.example.com");
        assert_eq!(v["source"], "bus_runtime");

        // Read it back via store().get and assert source.
        let runtime = node.vhosts_runtime.as_ref().unwrap();
        let key = RecordKey::collection(namespace_name(), "p3test.example.com".to_string());
        let snap = runtime.store().get(&key).await.expect("row present");
        if let PropValue::Object(m) = &snap.value.value {
            assert_eq!(
                m.get("source"),
                Some(&PropValue::String("bus_runtime".into())),
                "source must be stamped bus_runtime",
            );
            assert_eq!(
                m.get("fqdn"),
                Some(&PropValue::String("p3test.example.com".to_string())),
            );
        } else {
            panic!("row body is not an Object: {:?}", snap.value.value);
        }
        clear_auth_policy_for_test();
    }

    /// 2026-07 kwarg sweep — `vhost.add` with EVERY kwarg delivered in the
    /// JSON `args` object (no headers), the shape a bare Mix `send k=v`
    /// produces. Proves per-verb that `fqdn`/`www_dir` (kwarg), a dotted
    /// `acme.provider` (kwarg_any), and `enabled=false` as a real JSON bool
    /// (kwarg_bool) all read from `args` — the exact channel the pre-sweep
    /// header-only reads missed. The discriminating assertion is
    /// `enabled == false`: a kwarg_bool that failed to read the JSON bool
    /// would default it back to `true`.
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_add_reads_all_kwargs_from_args_object() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "props.read:webd.vhosts"]).await;
        let cmd = cmd_with_args(
            "webd.vhost.add",
            serde_json::json!({
                "fqdn": "argswired.example.com",
                "www_dir": "/srv/argswired",
                "acme.provider": "letsencrypt_staging",
                "acme.challenge": "http01",
                "acme.contact_email": "ops@example.com",
                "enabled": false,
            }),
        );
        let (rc, body) = vhost_add(&node, &cmd).await;
        assert_eq!(rc, 0, "args-delivered add rc=0; body={body}");

        let runtime = node.vhosts_runtime.as_ref().unwrap();
        let key = RecordKey::collection(namespace_name(), "argswired.example.com".to_string());
        let snap = runtime.store().get(&key).await.expect("row present");
        let PropValue::Object(m) = &snap.value.value else {
            panic!("row body is not an Object: {:?}", snap.value.value);
        };
        assert_eq!(
            m.get("www_dir"),
            Some(&PropValue::String("/srv/argswired".into())),
            "www_dir read from args",
        );
        assert_eq!(
            m.get("acme_provider"),
            Some(&PropValue::String("letsencrypt_staging".into())),
            "dotted acme.provider read from args via kwarg_any",
        );
        assert_eq!(
            m.get("enabled"),
            Some(&PropValue::Bool(false)),
            "enabled=false JSON bool read from args via kwarg_bool — NOT defaulted to true",
        );
        clear_auth_policy_for_test();
    }

    /// 2026-07 kwarg sweep — `listener.enable` reads its `id` from the
    /// `args` object (no header). The live probe couldn't reach this arm
    /// (it sits behind the operator write-cap), so pin it here: with the
    /// cap granted, an args-delivered `id` must get PAST the "missing
    /// required kwarg: id" gate to the not-found lookup — proving the read,
    /// not the header coincidence.
    #[tokio::test(flavor = "current_thread")]
    async fn listener_enable_reads_id_from_args_object() {
        // Operator "op-tester" holds the listener write-cap (allowlist +
        // matching cmd.from). listener_verbs uses its OWN auth_policy keyed
        // on listeners_operators — NOT the vhost thread-local override — so
        // the cap must be granted this way.
        let node = build_node_with_caps_ops(&[], &["op-tester"]).await;
        let mut cmd = cmd_with_args(
            "webd.listener.enable",
            serde_json::json!({ "id": "nonexistent-listener" }),
        );
        cmd.from = "op-tester".to_string();
        // Replicate the PRODUCTION wire shape: noded stamps the message
        // correlation id into a reserved `id` HEADER. A header-first kwarg
        // read let this shadow the operator's args `id=` (caught by a live
        // operator-cap probe — the unit test missed it until this header was
        // added). The not-found body below must echo the OPERATOR's id, not
        // this `noded-9999`.
        cmd.headers
            .insert("id".to_string(), "noded-9999".to_string());
        let (rc, body) = super::super::listener_verbs::dispatch("listener.enable", &cmd, &node)
            .await
            .expect("listener.enable is a listener verb");
        // The id read (kwarg) sits AFTER the cap check but BEFORE the
        // runtime check, so with the cap granted the response must be the
        // "not attached" (runtime=None in this fixture) — proving the id was
        // read from args — and must NEVER be "missing required kwarg: id"
        // (which would mean the args-delivered id was dropped) or
        // "auth_denied" (cap not granted → id read never reached).
        assert_eq!(rc, 10, "body={body}");
        assert!(
            !body.contains("missing required kwarg") && !body.contains("auth_denied"),
            "id must be read from args past the cap gate; got {body}",
        );
        assert!(
            body.contains("not attached"),
            "expected not-attached after the id read; got {body}",
        );
    }

    /// The reserved-`id`-header collision at the LISTENER STATUS verb (which
    /// DOES reach the id read without a mutation cap): with the ambient
    /// message-id header present AND an operator `id=` in args, the filter
    /// must use the OPERATOR's id. Pre-fix, `status id=lan` silently filtered
    /// by `noded-<seq>` and returned an empty list — a coincidence that read
    /// like success. Here the operator id matches no listener, so the list is
    /// empty either way; the pin is that dispatch SUCCEEDS with the operator
    /// id honoured, exercised via the real dispatch path.
    #[tokio::test(flavor = "current_thread")]
    async fn listener_status_id_filter_uses_operator_id_not_message_id() {
        let node = build_node_with_caps_ops(&["props.read:webd.listeners"], &[]).await;
        // status needs a listeners runtime; without one it's "not attached".
        // The point here is purely that the id read prefers args — assert via
        // kwarg directly on the production-shaped command, since status with
        // no runtime short-circuits before building the filter.
        let mut cmd = cmd_with_args("webd.listener.status", serde_json::json!({ "id": "lan" }));
        cmd.headers
            .insert("id".to_string(), "noded-1234".to_string());
        assert_eq!(
            super::super::kwarg(&cmd, "id").as_deref(),
            Some("lan"),
            "status must filter by the operator id, not the message-id header",
        );
        let _ = node; // node built to mirror the dispatch fixture shape
    }

    /// V-V — `vhost.add` without `props.write:webd.vhosts` rejected.
    /// Pin the cap-split: a read-only caller cannot create vhosts.
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_add_without_write_cap_denied() {
        let node = build_node_with_caps(&["props.read:webd.vhosts"]).await;
        let cmd = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "denied.example.com"),
                ("www_dir", "/srv/denied"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, body) = vhost_add(&node, &cmd).await;
        assert_eq!(rc, RC_CALLER_ERROR);
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "auth_denied");
        assert_eq!(v["missing_capability"], "props.write:webd.vhosts");
        // Negative: no row was written.
        let runtime = node.vhosts_runtime.as_ref().unwrap();
        let key = RecordKey::collection(namespace_name(), "denied.example.com".to_string());
        assert!(
            matches!(runtime.store().get(&key).await, Err(StoreError::NotFound)),
            "denied vhost.add must not write a row",
        );
        clear_auth_policy_for_test();
    }

    /// V-V — `vhost.remove` happy path. Add a row, remove it,
    /// confirm the row is gone.
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_remove_happy_path_drops_row() {
        let node = build_node_with_caps(&["props.write:webd.vhosts"]).await;
        // Seed: vhost.add first.
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "remove.example.com"),
                ("www_dir", "/srv/remove"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0);

        let rm = cmd_with_headers("webd.vhost.remove", &[("fqdn", "remove.example.com")]);
        let (rc, body) = vhost_remove(&node, &rm).await;
        assert_eq!(rc, 0, "remove happy path rc=0; body={body}");

        let runtime = node.vhosts_runtime.as_ref().unwrap();
        let key = RecordKey::collection(namespace_name(), "remove.example.com".to_string());
        assert!(
            matches!(runtime.store().get(&key).await, Err(StoreError::NotFound)),
            "row must be gone after vhost.remove",
        );
        clear_auth_policy_for_test();
    }

    /// Regression for the soft-delete re-add OCC trap: after
    /// `vhost.remove` leaves a tombstone, `vhost.add` of the same fqdn
    /// must succeed by anchoring against the tombstone version (SPEC 12
    /// §5.4) rather than `Version::zero()`. Without
    /// `Store::version_anchor`, the second add would fail with
    /// `VersionMismatch{expected: 0, current: <tombstone_version>}` and
    /// the operator would have no in-band recovery path.
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_add_after_remove_recreates_via_tombstone_anchor() {
        let node = build_node_with_caps(&["props.write:webd.vhosts"]).await;
        let add1 = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "readd.example.com"),
                ("www_dir", "/srv/readd"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add1).await;
        assert_eq!(rc, 0, "initial add must succeed");

        let rm = cmd_with_headers("webd.vhost.remove", &[("fqdn", "readd.example.com")]);
        let (rc, _) = vhost_remove(&node, &rm).await;
        assert_eq!(rc, 0, "remove must succeed");

        let runtime = node.vhosts_runtime.as_ref().unwrap();
        let key = RecordKey::collection(namespace_name(), "readd.example.com".to_string());
        assert!(
            matches!(runtime.store().get(&key).await, Err(StoreError::NotFound)),
            "soft-deleted row must be hidden from get",
        );
        // But the tombstone version IS present and must be observable
        // via the new substrate API — this is what makes re-add work.
        let anchor = runtime
            .store()
            .version_anchor(&key)
            .await
            .expect("anchor query");
        assert!(
            anchor.is_some(),
            "tombstone must surface a version_anchor (got None)",
        );

        let add2 = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "readd.example.com"),
                ("www_dir", "/srv/readd-v2"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, body) = vhost_add(&node, &add2).await;
        assert_eq!(
            rc, 0,
            "re-add after remove must succeed via tombstone anchor; body={body}",
        );
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        // The recreated row is live again.
        let snap = runtime.store().get(&key).await.expect("re-add live row");
        if let PropValue::Object(m) = &snap.value.value {
            assert_eq!(
                m.get("www_dir"),
                Some(&PropValue::String("/srv/readd-v2".into())),
                "patch-merge picked up the v2 www_dir",
            );
        } else {
            panic!("row body is not an Object: {:?}", snap.value.value);
        }
        clear_auth_policy_for_test();
    }

    /// V-V — `vhost.list` happy path returns the row with derived
    /// `acme_status = "pending"` (no `not_after` stamped yet).
    #[tokio::test(flavor = "current_thread")]
    async fn vhost_list_happy_path_decorates_acme_status_pending() {
        let node =
            build_node_with_caps(&["props.read:webd.vhosts", "props.write:webd.vhosts"]).await;
        // Seed.
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "list.example.com"),
                ("www_dir", "/srv/list"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0);

        let cmd = cmd_with_headers("webd.vhost.list", &[]);
        let (rc, body) = vhost_list(&node, &cmd).await;
        assert_eq!(rc, 0, "list happy path rc=0; body={body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["count"], 1);
        let row = &v["rows"][0];
        assert_eq!(row["fqdn"], "list.example.com");
        assert_eq!(row["acme_status"], "pending");
        // Without :secrets, cert_blob_id stays redacted/null. The
        // freshly-added row has no cert_blob_id either way (no
        // issuance yet), so we just assert the field exists.
        assert!(row.get("cert_blob_id").is_some());
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` happy path BOTH queues the fqdn for
    /// force-renewal AND fires the notify. The pair is what makes the
    /// verb actually bypass the renewal-window/cooldown gates on the
    /// next sweep; a bare notify (the pre-fix behaviour) would wake
    /// the loop without queueing, leaving timing gates intact and
    /// silently no-op'ing the operator request.
    ///
    /// The verb now also pre-validates the fqdn against the namespace
    /// row (since the post-fix verb rejects unknown/disabled/non-ACME
    /// fqdns rather than queueing a guaranteed-no-op), so the test
    /// seeds an enabled ACME row before invoking renew. The seed uses
    /// the same `vhost.add` path the operator would, which requires
    /// the `props.write:webd.vhosts` cap to be in scope alongside the
    /// narrow renew cap for this test only.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_happy_path_queues_then_notifies() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "webd.acme.renew:webd.vhosts"]).await;
        // Seed the row so the new pre-queue validation passes.
        let seed = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "kick.example.com"),
                ("www_dir", "/srv/kick"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &seed).await;
        assert_eq!(rc, 0, "seed must land for renew validation to pass");

        let notify = node.acme_notify.clone().expect("notify wired");
        let queue = node.acme_force_renew_queue.clone().expect("queue wired");
        // Spawn a waiter so we can assert the notify_one fired.
        let waiter = tokio::spawn(async move {
            notify.notified().await;
        });
        let cmd = cmd_with_headers("webd.acme.renew", &[("fqdn", "kick.example.com")]);
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(rc, 0, "renew happy path rc=0; body={body}");
        // The waiter should resolve quickly. tokio::time::timeout
        // guards against a regression where notify_one is no-op'd.
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("notify_one must wake a notified() waiter")
            .expect("waiter task ran");
        // The fqdn must be in the force-renew queue at this point —
        // tick_once drains it later, but until then it's observable
        // here (the test holds an Arc clone of the same Mutex).
        let snapshot = queue.lock().await.clone();
        assert!(
            snapshot.contains("kick.example.com"),
            "queue must hold the requested fqdn (snapshot={snapshot:?})",
        );
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["state"], "pending");
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` with `props.write` but NOT the narrow
    /// `webd.acme.renew:webd.vhosts` cap is denied. This is the
    /// cap-split pin: a write-cap holder cannot force renewals
    /// outside the renewal-window gate.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_with_write_cap_only_denied() {
        let node = build_node_with_caps(&["props.write:webd.vhosts"]).await;
        let cmd = cmd_with_headers("webd.acme.renew", &[("fqdn", "denied.example.com")]);
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(rc, RC_CALLER_ERROR);
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "auth_denied");
        assert_eq!(v["missing_capability"], "webd.acme.renew:webd.vhosts");
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` on an unknown fqdn (typo or never-added) is
    /// rejected as `caller_error`, not silently queued. Without this
    /// gate, `tick_once` would find no matching plan and self-clear
    /// the queue entry, leaving the operator with an `ok:true`
    /// response and no renewal ever occurring. The same arm also
    /// rejects tombstoned (soft-deleted) rows since `store().get`
    /// returns `NotFound` for them — soft-delete is intentional in
    /// this namespace and a tombstoned vhost has no live plan to
    /// renew.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_unknown_fqdn_caller_error() {
        let node = build_node_with_caps(&["webd.acme.renew:webd.vhosts"]).await;
        let queue = node.acme_force_renew_queue.clone().expect("queue wired");
        let cmd = cmd_with_headers(
            "webd.acme.renew",
            &[("fqdn", "typo-or-tombstoned.example.com")],
        );
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(rc, RC_CALLER_ERROR, "unknown fqdn must reject; body={body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        let err = v["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("not found"),
            "error should mention not found; got {err}",
        );
        // Negative: nothing was queued.
        assert!(
            queue.lock().await.is_empty(),
            "queue must remain empty after rejected renew",
        );
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` on a soft-deleted (tombstoned) row is
    /// rejected by the same `NotFound` arm as the typo case. Pinned
    /// separately so a future change to soft-delete semantics (e.g.
    /// surfacing tombstones via `get`) doesn't silently re-open the
    /// no-op path.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_tombstoned_fqdn_caller_error() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "webd.acme.renew:webd.vhosts"]).await;
        let queue = node.acme_force_renew_queue.clone().expect("queue wired");
        // Add then remove → tombstone in the SqliteStore.
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "tombstone.example.com"),
                ("www_dir", "/srv/tombstone"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0);
        let rm = cmd_with_headers("webd.vhost.remove", &[("fqdn", "tombstone.example.com")]);
        let (rc, _) = vhost_remove(&node, &rm).await;
        assert_eq!(rc, 0);

        let cmd = cmd_with_headers("webd.acme.renew", &[("fqdn", "tombstone.example.com")]);
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(
            rc, RC_CALLER_ERROR,
            "tombstoned row must reject; body={body}"
        );
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not found"),
            "tombstone goes through the same NotFound arm",
        );
        assert!(
            queue.lock().await.is_empty(),
            "queue must remain empty after tombstone rejection",
        );
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` on a disabled (`enabled=false`) row is
    /// rejected. Disabled is operator policy, not scheduler timing,
    /// so force-renew honours it (consistent with `tick_once`'s
    /// disabled-state gate which the BLOCKER 1 fix did NOT bypass).
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_disabled_fqdn_caller_error() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "webd.acme.renew:webd.vhosts"]).await;
        let queue = node.acme_force_renew_queue.clone().expect("queue wired");
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "disabled.example.com"),
                ("www_dir", "/srv/disabled"),
                ("enabled", "false"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0);

        let cmd = cmd_with_headers("webd.acme.renew", &[("fqdn", "disabled.example.com")]);
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(rc, RC_CALLER_ERROR, "disabled row must reject; body={body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"].as_str().unwrap_or_default().contains("disabled"),
            "error must mention disabled",
        );
        assert!(
            queue.lock().await.is_empty(),
            "queue must remain empty after disabled rejection",
        );
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.renew` on a row with no `acme_provider` (manual-TLS
    /// or HTTP-only vhost) is rejected. `tick_once` produces no plan
    /// for these rows, so a queued entry would self-clear with no
    /// visible error — the verb must surface the configuration
    /// mismatch to the operator instead.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_renew_no_acme_provider_caller_error() {
        let node =
            build_node_with_caps(&["props.write:webd.vhosts", "webd.acme.renew:webd.vhosts"]).await;
        let queue = node.acme_force_renew_queue.clone().expect("queue wired");
        // Manual-TLS vhost: no acme_provider, but cert/key pair so the
        // namespace hook accepts an enabled row. This is the operator
        // shape the no-acme rejection arm has to surface — running
        // `acme.renew` on a row that has no ACME plan to renew.
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "manual-tls.example.com"),
                ("www_dir", "/srv/manual-tls"),
                ("tls.cert_path", "/etc/cosmix/manual-tls/cert.pem"),
                ("tls.key_path", "/etc/cosmix/manual-tls/key.pem"),
            ],
        );
        let (rc, body) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0, "manual-TLS vhost.add must succeed; body={body}");

        let cmd = cmd_with_headers("webd.acme.renew", &[("fqdn", "manual-tls.example.com")]);
        let (rc, body) = acme_renew(&node, &cmd).await;
        assert_eq!(
            rc, RC_CALLER_ERROR,
            "no-acme-provider row must reject; body={body}",
        );
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("acme_provider"),
            "error must mention acme_provider",
        );
        assert!(
            queue.lock().await.is_empty(),
            "queue must remain empty after no-acme-provider rejection",
        );
        clear_auth_policy_for_test();
    }

    /// V-V — `acme.status` happy path returns the derived status +
    /// row's `not_after` + redacted `last_error` without `:secrets`.
    #[tokio::test(flavor = "current_thread")]
    async fn acme_status_happy_path_redacts_last_error() {
        let node =
            build_node_with_caps(&["props.read:webd.vhosts", "props.write:webd.vhosts"]).await;
        // Seed a vhost row.
        let add = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "status.example.com"),
                ("www_dir", "/srv/status"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, _) = vhost_add(&node, &add).await;
        assert_eq!(rc, 0);

        // Seed the tls_status watch channel with a failing vhost_state.
        let (tx, rx) = tokio::sync::watch::channel(TlsStatusSnapshot {
            manual_identities: Vec::new(),
            acme: Some(AcmeStatusSnapshot {
                plans: Vec::new(),
                vhost_state: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "status.example.com".into(),
                        AcmeVhostStateSnapshot {
                            issued: false,
                            last_error: Some("secret-leaking-error-body".into()),
                            last_error_count: 2,
                            next_attempt_after_rfc3339: Some("2026-05-26T12:34:56Z".into()),
                        },
                    );
                    m
                },
            }),
        });
        // Rebuild the NodeState with the seeded rx (the helper
        // hands back a default-empty one).
        let node2 = {
            let runtime = node.vhosts_runtime.clone();
            let key_locks = node.vhost_key_locks.clone();
            let notify = node.acme_notify.clone();
            let force_queue = node.acme_force_renew_queue.clone();
            Arc::new(NodeState {
                service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                login_throttle: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                login_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                vhosts: node.vhosts.clone(),
                http_client: node.http_client.clone(),
                session: node.session.clone(),
                served_mail_domains: node.served_mail_domains.clone(),
                mx: None,
                autoconfig_mail_host: node.autoconfig_mail_host.clone(),
                acme_challenges: node.acme_challenges.clone(),
                tls_status_rx: rx,
                props_router: node.props_router.clone(),
                props_subscribe_granter: node.props_subscribe_granter.clone(),
                broker_handle: node.broker_handle.clone(),
                vhosts_runtime: runtime,
                listeners_runtime: None,
                listeners_operators: Vec::new(),
                vhost_key_locks: key_locks,
                acme_notify: notify,
                acme_force_renew_queue: force_queue,
                handlers: Arc::new(ArcSwap::from(Arc::new(
                    crate::mix_handler::HandlerTable::default(),
                ))),
                handler_ast_cache: crate::mix_handler::new_ast_cache(),
                tls_reload: None,
            })
        };
        let _tx = tx; // keep sender alive for the rx to remain live

        let cmd = cmd_with_headers("webd.acme.status", &[("fqdn", "status.example.com")]);
        let (rc, body) = acme_status(&node2, &cmd).await;
        assert_eq!(rc, 0, "status happy path rc=0; body={body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(v["fqdn"], "status.example.com");
        assert_eq!(
            v["last_error"], "<redacted>",
            "last_error must be redacted without :secrets cap"
        );
        assert_eq!(v["last_error_count"], 2);
        assert_eq!(v["acme_status"], "failing"); // last_error_count > 0
        clear_auth_policy_for_test();
    }

    /// V-V cap-split admit pin (the inverse of the renew-deny
    /// test): a holder of `props.write:webd.vhosts` *can* add a new
    /// ACME vhost, even though that consumes LE rate-limit budget.
    /// The cap honestly admits this scope.
    #[tokio::test(flavor = "current_thread")]
    async fn props_write_cap_holder_can_add_new_acme_vhost_consuming_rate() {
        let node = build_node_with_caps(&["props.write:webd.vhosts"]).await;
        let cmd = cmd_with_headers(
            "webd.vhost.add",
            &[
                ("fqdn", "ratebudget.example.com"),
                ("www_dir", "/srv/ratebudget"),
                ("acme.provider", "letsencrypt_staging"),
                ("acme.challenge", "http01"),
                ("acme.contact_email", "ops@example.com"),
            ],
        );
        let (rc, body) = vhost_add(&node, &cmd).await;
        assert_eq!(
            rc, 0,
            "props.write holder admitted for ACME vhost.add; body={body}"
        );
        clear_auth_policy_for_test();
    }
}
