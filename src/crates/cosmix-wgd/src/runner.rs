//! The reconcile loop: verify → derive → read live → diff → publish a snapshot.
//!
//! This owns the wall-clock read (injected into the pure core as `now_unix`)
//! and the IO. It is dry-run: it produces a [`Snapshot`] and logs drift; it
//! never mutates the kernel or the inventory.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cosmix_wg::iface_name_for_mesh;

use crate::derive::derive_intended;
use crate::live::read_live;
use crate::reconcile::reconcile;
use crate::state::{Shared, Snapshot};
use crate::trust::{TrustPaths, load_and_verify};

/// Unix seconds now, or 0 if the clock is before the epoch (impossible in
/// practice). Used only for liveness + the snapshot timestamp, never for a
/// security decision (those are epoch-driven — §7.5/§16a).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run one reconcile pass and store the resulting snapshot. Returns `Err` only
/// when it cannot reach an intended set (trust or derive failure) — a live-read
/// failure is captured INSIDE the snapshot (non-fatal; the lab interface may
/// simply be down) and is not an `Err`.
pub fn tick(
    paths: &TrustPaths,
    iface_override: Option<&str>,
    self_name: &str,
    shared: &Shared,
) -> Result<(), String> {
    let verified = load_and_verify(paths).map_err(|e| e.to_string())?;
    let payload = &verified.signed.payload;
    let mesh = payload.mesh.clone();
    let epoch = verified.epoch;
    tracing::info!(
        mesh = %mesh,
        epoch,
        via_recovery = verified.via_recovery,
        verified_by = ?verified.verified_by,
        routing_members = verified.routing_view.len(),
        "verified signed inventory (trust root: genesis anchor)"
    );

    // The interface: an explicit override, else the first DNS label of the
    // mesh identity (kernel-validated).
    let iface = match iface_override {
        Some(s) => s.to_string(),
        None => iface_name_for_mesh(&mesh)
            .map_err(|e| format!("deriving iface name from mesh {mesh:?}: {e}"))?,
    };

    let intended = derive_intended(&mesh, &payload.subnet, epoch, &payload.members, self_name)
        .map_err(|e| e.to_string())?;

    for w in &intended.warnings {
        tracing::warn!(?w, "derive warning");
    }

    let now = now_unix();
    let live = match read_live(&iface) {
        Ok(dump) => {
            let report = reconcile(&intended, &dump, now);
            if report.is_clean() {
                tracing::info!(
                    iface = %iface,
                    mesh = %mesh,
                    epoch,
                    synced = report.synced.len(),
                    "reconcile: kernel in sync with the signed inventory (dry-run)"
                );
            } else {
                tracing::warn!(
                    iface = %iface,
                    mesh = %mesh,
                    epoch,
                    synced = report.synced.len(),
                    drift = report.drift.len(),
                    "reconcile: DRIFT between the signed inventory and the kernel (dry-run — NOT converged in P2)"
                );
                for d in &report.drift {
                    tracing::warn!(?d, "drift item");
                }
            }
            Ok(report)
        }
        Err(e) => {
            let reason = e.to_string();
            tracing::warn!(
                iface = %iface,
                error = %reason,
                "reconcile: could not read live kernel state; serving the derived intent only (non-fatal)"
            );
            Err(reason)
        }
    };

    let snap = Snapshot {
        iface,
        intended,
        live,
        refreshed_at_unix: now,
    };
    if let Ok(mut guard) = shared.lock() {
        *guard = Some(snap);
    }
    Ok(())
}

/// The reconcile loop: tick on `interval`, forever. A tick failure (trust or
/// derive error) is logged and retried on the next interval — the daemon keeps
/// serving its last good snapshot and keeps retrying, never exits.
pub async fn run(
    paths: TrustPaths,
    iface_override: Option<String>,
    self_name: String,
    interval: Duration,
    shared: Shared,
) {
    loop {
        if let Err(e) = tick(&paths, iface_override.as_deref(), &self_name, &shared) {
            tracing::warn!(
                error = %e,
                "reconcile tick failed (inventory not yet verifiable / derivable); retrying next interval"
            );
        }
        tokio::time::sleep(interval).await;
    }
}
