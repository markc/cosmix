//! Ergonomic Bus verbs for the `webd.listeners` kill-switch namespace
//! (P3): `webd.listener.{enable,disable,status} id=<id>`.
//!
//! `enable`/`disable` are thin wrappers that `props.set` the `enabled`
//! field — **L1 is the system of record**, and the reaction loop
//! ([`crate::listeners_reaction`]) applies the change to the live
//! listener set and writes the observed `bound` state back. The verb
//! therefore confirms only that L1 was written; an operator reads
//! `webd.listener.status` (or the `_bin/webd_listener.mix` script
//! read-back) to confirm the listener actually bound/unbound.
//!
//! Cap model mirrors the namespace's operator-tier [`auth_policy`]:
//! `props.write:webd.listeners` (the privileged cut-traffic cap) is
//! granted only to a sender in the L0 operators allowlist; read/status
//! is open to every WG peer (secret `last_error` redacted without the
//! `:secrets` read cap).

use std::collections::BTreeMap;
use std::sync::Arc;

use cosmix_client::IncomingCommand;
use cosmix_props::{
    Actor, Capability, CapabilitySet, MergeMode, PeerIdentity, PropValue, RecordKey, SetOpts,
    WriteOrigin,
};
use serde_json::json;

use super::kwarg;
use crate::NodeState;
use crate::listeners_namespace::{auth_policy, namespace_name, snapshot_rows};

/// Caller-error sentinel — matches [`crate::bus::RC_CALLER_ERROR`].
const RC_CALLER_ERROR: u8 = 10;

/// Route a `webd.`-stripped suffix to a listener verb, or `None` if it
/// isn't one (so the caller falls through to the next dispatcher).
pub(crate) async fn dispatch(
    suffix: &str,
    cmd: &IncomingCommand,
    node: &Arc<NodeState>,
) -> Option<(u8, String)> {
    let result = match suffix {
        "listener.enable" => set_enabled(node, cmd, true).await,
        "listener.disable" => set_enabled(node, cmd, false).await,
        "listener.status" => status(node, cmd).await,
        _ => return None,
    };
    Some(result)
}

/// `webd.listener.{enable,disable}` — operator-gated `props.set` of the
/// `enabled` field. The reaction loop does the actual bind/unbind. The
/// namespace's lockout guard (before_set rule 4) rejects disabling a
/// non-external listener even for an operator — that rejection surfaces
/// here as a caller error.
async fn set_enabled(node: &Arc<NodeState>, cmd: &IncomingCommand, enabled: bool) -> (u8, String) {
    if let Err(rsp) = require_cap(node, cmd, "props.write:webd.listeners") {
        return rsp;
    }
    let id = match kwarg(cmd, "id") {
        Some(s) => s,
        None => return caller_error("missing required kwarg: id"),
    };
    let Some(runtime) = node.listeners_runtime.as_ref() else {
        return server_error("webd.listeners runtime not attached — bootstrap node or wiring bug");
    };

    let key = RecordKey::collection(namespace_name(), id.clone());
    let cause = if enabled {
        "listener.enable"
    } else {
        "listener.disable"
    };

    // Caller-origin Patch of the (caller-owned) `enabled` field — fires
    // the namespace's after_set, which the reaction loop consumes and
    // applies to the live listener set. (Caller-origin so the operator
    // intent is what triggers reconciliation; the verb's own cap-check
    // above is the gate, and we write no daemon-owned columns.) Bounded
    // OCC retry: the reaction loop's frequent backend status writeback
    // can bump the version between our anchor read and our write, which
    // would otherwise surface as a spurious version_mismatch.
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let expected_version = match runtime.store().version_anchor(&key).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                return caller_error(&format!(
                    "listener {id:?} not found in webd.listeners namespace",
                ));
            }
            Err(e) => return server_error(&format!("fetching listener row for OCC anchor: {e}")),
        };
        let mut body: BTreeMap<String, PropValue> = BTreeMap::new();
        body.insert("enabled".into(), PropValue::Bool(enabled));
        match runtime
            .set_with_origin(
                key.clone(),
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(expected_version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd").expect("valid actor"),
                    cause: Some(cause.into()),
                    ts_ms: now_ms(),
                },
                WriteOrigin::caller(),
            )
            .await
        {
            Ok(_) => {
                return (
                    0,
                    json!({
                        "ok": true,
                        "id": id,
                        "enabled": enabled,
                        "note": "applied asynchronously by the reaction loop — read \
                                 webd.listener.status to confirm `bound`",
                    })
                    .to_string(),
                );
            }
            Err(e) => {
                if attempt + 1 < ATTEMPTS {
                    // Most likely a concurrent reaction-loop status
                    // writeback bumped the version — re-read + retry.
                    continue;
                }
                return caller_error(&format!("listener.{cause} failed: {e}"));
            }
        }
    }
    unreachable!("retry loop returns on success or final error")
}

/// `webd.listener.status [id=<id>]` — project the namespace rows.
/// `last_error` (secret) is included only with the `:secrets` read cap.
async fn status(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(node, cmd, "props.read:webd.listeners") {
        return rsp;
    }
    let Some(runtime) = node.listeners_runtime.as_ref() else {
        return server_error("webd.listeners runtime not attached — bootstrap node or wiring bug");
    };
    let with_secrets = has_cap(node, cmd, "props.read:webd.listeners:secrets");
    let id_filter = kwarg(cmd, "id");

    let rows = match snapshot_rows(runtime).await {
        Ok(r) => r,
        Err(e) => return server_error(&format!("listing webd.listeners: {e}")),
    };
    let listeners: Vec<serde_json::Value> = rows
        .into_iter()
        .filter(|r| id_filter.as_ref().is_none_or(|f| &r.id == f))
        .map(|r| {
            let mut j = json!({
                "id": r.id,
                "enabled": r.enabled,
                "external": r.external,
                "bound": r.bound,
                "bound_addr": r.bound_addr,
                "active_conns": r.active_conns,
                "strict_sni": r.strict_sni,
                "rate_limit_per_ip": r.rate_limit_per_ip,
                "max_conns_global": r.max_conns_global,
                "max_conns_per_ip": r.max_conns_per_ip,
                "allow_cidrs": r.allow_cidrs,
                "deny_cidrs": r.deny_cidrs,
                "last_transition": r.last_transition,
            });
            if with_secrets {
                j["last_error"] = json!(r.last_error);
            }
            j
        })
        .collect();
    (0, json!({ "listeners": listeners }).to_string())
}

// ── helpers ──────────────────────────────────────────────────────────

/// Resolve the caller's capability set against the operator-tier
/// [`auth_policy`] — same authoritative source the props router uses,
/// seeded with this node's L0 operators allowlist.
fn has_cap(node: &Arc<NodeState>, cmd: &IncomingCommand, cap: &str) -> bool {
    let peer = peer_from_cmd(cmd);
    let caps: CapabilitySet = auth_policy("webd", node.listeners_operators.clone()).resolve(&peer);
    Capability::new(cap).is_ok_and(|cap| caps.contains(&cap))
}

pub(crate) fn require_cap(
    node: &Arc<NodeState>,
    cmd: &IncomingCommand,
    cap: &str,
) -> Result<(), (u8, String)> {
    if has_cap(node, cmd, cap) {
        Ok(())
    } else {
        Err((
            RC_CALLER_ERROR,
            json!({ "error": "auth_denied", "missing_capability": cap }).to_string(),
        ))
    }
}

/// noded forwards the registered sender as `service_name`; Unix/WG
/// fields stay `None` (noded doesn't surface them today).
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

fn caller_error(msg: &str) -> (u8, String) {
    (RC_CALLER_ERROR, json!({ "error": msg }).to_string())
}

fn server_error(msg: &str) -> (u8, String) {
    (
        RC_CALLER_ERROR,
        json!({ "error": msg, "kind": "server" }).to_string(),
    )
}

fn now_ms() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() * 1000
}
