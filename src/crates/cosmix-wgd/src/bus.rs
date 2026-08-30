//! Read-only Bus surface — the P2 control-plane view.
//!
//! P2 exposes **no writes** (peer allocation / rotation / apply are P3). The
//! verbs are derived/observed reads over the latest reconcile [`Snapshot`]:
//!
//! * `wgd.iface.status`     — interface + reconcile summary (counts, live-ok).
//! * `wgd.peer.status`      — live per-peer `PeerStatus`, keyed by mesh name.
//! * `wgd.topology.snapshot`— the intended peer set + per-peer kernel state.
//! * `wgd.drift`            — the full dry-run drift report.
//!
//! Read-only by construction: any other verb — including a future write-shaped
//! `wgd.peer.*` / `wgd.iface.apply` — returns the caller-error rc so P2 can
//! never silently accept a mutation (the dnsd/maild uniform wire contract).
//!
//! ## Deferred to P2.1 — topic publication (Codex 2026-07-06, review #2)
//!
//! The original P2 ruling had wgd *publish* `wgd.iface.drift` and
//! `wgd.peer.status.changed`. This landing exposes drift/status via the READ
//! verbs above plus a per-tick log ([`crate::runner`]) — the dnsd citizen
//! template is read-verb-only and has no publisher, and the P2 value is
//! derive/dry-run correctness. Active `topic.publish` (the maild-style
//! broadcast-channel + publisher task, publishing **on change**, not every
//! tick) is a scoped **P2.1** follow-up; the verb names / body shapes here are
//! already the ones those topics will carry.
//!
//! The connect/backoff loop mirrors `cosmix-dnsd::bus` verbatim so the two
//! citizens behave identically against broker outages: the reconcile loop runs
//! in a sibling task and is unaffected by broker availability.

use std::time::{Duration, Instant};

use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_wg::PeerStatus;
use serde_json::{Value, json};

use crate::citizen::BUS_SERVICE;
use crate::reconcile::{DriftItem, DriftReport};
use crate::state::{Shared, Snapshot};

/// rc=10 is the caller-error sentinel: `NodedClient::call` only surfaces
/// `rc >= 10` as an error, so a smaller rc would let a typo'd / write-shaped
/// verb return `Ok(...)` and mask the failure at the caller. Same value +
/// rationale as dnsd/maild so the mesh-wide wire contract stays uniform.
const RC_CALLER_ERROR: u8 = 10;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

/// The read-only verbs P2 answers.
const READ_ONLY_VERBS: [&str; 4] = [
    "wgd.iface.status",
    "wgd.peer.status",
    "wgd.topology.snapshot",
    "wgd.drift",
];

/// Connect to the broker and run the read-only dispatch loop. Spawn once with
/// `tokio::spawn(bus::run(shared))`. Retry-with-backoff governs both the
/// initial connect and mid-life disconnects; the reconcile loop is unaffected.
pub async fn run(shared: Shared) {
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
    loop {
        let client = connect_with_backoff(&mut delay, &prov).await;
        let session_started = Instant::now();
        let mut rx = match client.incoming_async().await {
            Some(rx) => rx,
            None => {
                tracing::error!(
                    service = BUS_SERVICE,
                    "incoming_async returned None; closing client and reconnecting"
                );
                client.close().await;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        while let Some(cmd) = rx.recv().await {
            let (rc, body) = dispatch(&cmd, &shared);
            if let Err(e) = client.respond(&cmd, rc, &body).await {
                tracing::warn!(error = %e, command = %cmd.command, "Bus response send failed");
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
            "Bus incoming stream ended; reconnecting with backoff (reconcile loop continues)"
        );
        client.close().await;
        tokio::time::sleep(delay).await;
    }
}

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
                    "broker not available; Bus surface offline until reconnect succeeds (reconcile loop continues)"
                );
                tokio::time::sleep(*delay).await;
                *delay = (*delay * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pure request→(rc, body) router. Read-only by construction: no mutation arm —
/// anything not in [`READ_ONLY_VERBS`] is a caller error (including any future
/// write-shaped verb, which P2 must reject, not silently accept). Split out so
/// it is unit-testable without a broker.
fn dispatch(cmd: &IncomingCommand, shared: &Shared) -> (u8, String) {
    let snap = shared.lock().ok().and_then(|g| g.clone());
    match cmd.command.as_str() {
        "wgd.iface.status" => ok(iface_status_body(snap.as_ref())),
        "wgd.peer.status" => ok(peer_status_body(snap.as_ref())),
        "wgd.topology.snapshot" => ok(topology_body(snap.as_ref())),
        "wgd.drift" => ok(drift_body(snap.as_ref())),
        other => (
            RC_CALLER_ERROR,
            json!({
                "error": "unknown or non-read-only verb (P2 is read-only; writes are P3)",
                "verb": other,
                "read_only_verbs": READ_ONLY_VERBS,
            })
            .to_string(),
        ),
    }
}

fn ok(body: Value) -> (u8, String) {
    (0, body.to_string())
}

/// The "no reconcile has run yet" body — a well-formed not-ready signal rather
/// than an error, so a caller polling at startup gets a clear answer.
fn not_ready() -> Value {
    json!({ "ready": false, "reason": "no reconcile pass has completed yet" })
}

fn peer_status_str(s: &PeerStatus) -> &'static str {
    match s {
        PeerStatus::Pending => "pending",
        PeerStatus::Connected => "connected",
        PeerStatus::Offline => "offline",
    }
}

fn iface_status_body(snap: Option<&Snapshot>) -> Value {
    let Some(snap) = snap else {
        return not_ready();
    };
    let (live_available, synced, drift) = match &snap.live {
        Ok(r) => (true, r.synced.len(), r.drift.len()),
        Err(_) => (false, 0, 0),
    };
    json!({
        "ready": true,
        "iface": snap.iface,
        "mesh": snap.intended.mesh,
        "epoch": snap.intended.epoch,
        "self_name": snap.intended.self_name,
        "self_mesh_ip": snap.intended.self_mesh_ip.to_string(),
        "intended_peer_count": snap.intended.peers.len(),
        "live_available": live_available,
        "live_error": snap.live.as_ref().err(),
        "synced_count": synced,
        "drift_count": drift,
        "in_sync": snap.live.as_ref().map(DriftReport::is_clean).unwrap_or(false),
        "refreshed_at_unix": snap.refreshed_at_unix,
    })
}

fn peer_status_body(snap: Option<&Snapshot>) -> Value {
    let Some(snap) = snap else {
        return not_ready();
    };
    let peers: Vec<Value> = match &snap.live {
        Ok(report) => report
            .synced
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "mesh_ip": p.mesh_ip.to_string(),
                    "pubkey": p.public_key.to_base64(),
                    "state": peer_status_str(&p.status),
                })
            })
            .collect(),
        Err(_) => vec![],
    };
    json!({
        "ready": true,
        "iface": snap.iface,
        "live_available": snap.live.is_ok(),
        "live_error": snap.live.as_ref().err(),
        "peers": peers,
    })
}

fn topology_body(snap: Option<&Snapshot>) -> Value {
    let Some(snap) = snap else {
        return not_ready();
    };
    let set = &snap.intended;
    let peers: Vec<Value> = set
        .peers
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "mesh_ip": p.mesh_ip.to_string(),
                "allowed_ip": format!("{}/{}", p.allowed_ip.network, p.allowed_ip.prefix_len),
                "acceptable_pubkeys": p.acceptable_pubkeys.iter().map(|k| k.to_base64()).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "ready": true,
        "mesh": set.mesh,
        "subnet": format!("{}/{}", set.subnet.network, set.subnet.prefix_len),
        "epoch": set.epoch,
        "self": { "name": set.self_name, "mesh_ip": set.self_mesh_ip.to_string() },
        "intended_peers": peers,
        "live_available": snap.live.is_ok(),
        "drift": drift_items_json(snap.live.as_ref().ok()),
    })
}

fn drift_body(snap: Option<&Snapshot>) -> Value {
    let Some(snap) = snap else {
        return not_ready();
    };
    match &snap.live {
        Ok(report) => json!({
            "ready": true,
            "live_available": true,
            "mesh": report.mesh,
            "epoch": report.epoch,
            "in_sync": report.is_clean(),
            "drift": drift_items_json(Some(report)),
        }),
        Err(reason) => json!({
            "ready": true,
            "live_available": false,
            "live_error": reason,
            "drift": [],
        }),
    }
}

fn drift_items_json(report: Option<&DriftReport>) -> Vec<Value> {
    let Some(report) = report else {
        return vec![];
    };
    report.drift.iter().map(drift_item_json).collect()
}

fn drift_item_json(item: &DriftItem) -> Value {
    match item {
        DriftItem::Missing { name, mesh_ip } => json!({
            "kind": "missing", "name": name, "mesh_ip": mesh_ip.to_string(),
            "action_p3": "add",
        }),
        DriftItem::Extra {
            public_key,
            allowed_ips,
        } => json!({
            "kind": "extra", "pubkey": public_key.to_base64(), "allowed_ips": allowed_ips,
            "action_p3": "remove",
        }),
        DriftItem::KeyMismatch {
            name,
            mesh_ip,
            live_pubkey,
        } => json!({
            "kind": "key_mismatch", "name": name, "mesh_ip": mesh_ip.to_string(),
            "live_pubkey": live_pubkey.to_base64(), "action_p3": "rotate",
        }),
        DriftItem::AllowedIpsDrift {
            name,
            mesh_ip,
            live_allowed_ips,
        } => json!({
            "kind": "allowed_ips_drift", "name": name, "mesh_ip": mesh_ip.to_string(),
            "live_allowed_ips": live_allowed_ips, "action_p3": "reset_allowed_ips",
        }),
        DriftItem::DuplicateKernelClaimant { mesh_ip, count } => json!({
            "kind": "duplicate_kernel_claimant", "mesh_ip": mesh_ip.to_string(), "count": count,
            "action_p3": "operator_resolve",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::new_shared;

    fn cmd(command: &str) -> IncomingCommand {
        IncomingCommand {
            from: String::new(),
            command: command.to_string(),
            id: None,
            args: Value::Null,
            body: String::new(),
            headers: Default::default(),
        }
    }

    #[test]
    fn unknown_and_write_shaped_verbs_are_caller_errors() {
        let shared = new_shared();
        for bad in [
            "wgd.peer.allocate",
            "wgd.peer.rotate",
            "wgd.iface.apply",
            "wgd.bogus",
            "dnsd.stats",
        ] {
            let (rc, body) = dispatch(&cmd(bad), &shared);
            assert_eq!(rc, RC_CALLER_ERROR, "{bad} must be a caller error");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["verb"], bad);
        }
    }

    #[test]
    fn read_verbs_report_not_ready_before_first_reconcile() {
        let shared = new_shared();
        for verb in READ_ONLY_VERBS {
            let (rc, body) = dispatch(&cmd(verb), &shared);
            assert_eq!(rc, 0, "{verb} is a valid read verb");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["ready"], false, "{verb} not-ready before first reconcile");
        }
    }
}
