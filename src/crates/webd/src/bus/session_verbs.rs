//! `webd.session.*` — cookie-path session revocation (2026-07 audit).
//!
//! One verb: `webd.session.revoke email=<account>` bumps the account's
//! session epoch in EVERY vhost CMS DB on this node. Cookies seal the
//! epoch current at login (`session::SessionPayload::epoch`); every
//! cookie-authorized surface re-checks it, so a bump instantly kills all
//! outstanding cookies for that account — surgical, unlike node-key
//! rotation (which nukes every session on the node).
//!
//! This is HALF of a revocation: it kills the cookie path only. The
//! maild bearer path is killed by `maild.accounts.revoke_tokens`. The
//! unified revocation script (`_bin/revoke_account.mix` in the cosmix
//! hub) composes both so an operator can't do half a revocation — call
//! that, not this verb alone (see the `session.rs` module doc).
//!
//! Capability: `webd.session.revoke:webd.vhosts` — a narrow verb-named
//! cap following the `webd.acme.renew` precedent (not `props.write`:
//! session epochs are runtime security state in the per-vhost SQLite,
//! not a substrate row). Phase-1 policy grants it to every WG peer,
//! same as the rest of [`crate::vhosts_namespace::auth_policy`];
//! narrowing later keeps the wire shape.

use std::sync::Arc;

use cosmix_client::IncomingCommand;
use serde_json::json;

use super::kwarg;
use super::vhost_verbs::{caller_error, require_cap};
use crate::NodeState;

/// Dispatch entry from [`crate::bus::run`] — `suffix` is the verb name
/// after stripping the `webd.` prefix. `None` = not ours, fall through.
pub(crate) async fn dispatch(
    suffix: &str,
    cmd: &IncomingCommand,
    node: &Arc<NodeState>,
) -> Option<(u8, String)> {
    match suffix {
        "session.revoke" => Some(session_revoke(node, cmd).await),
        _ => None,
    }
}

/// `webd.session.revoke` — bump `email`'s session epoch in every vhost
/// CMS DB. Response:
/// `{"email", "revoked": [{"vhost", "epoch"}...], "vhosts_without_db": n}`.
/// Idempotent in effect (each call bumps again — every bump only ever
/// *invalidates*; there is no un-revoke). A vhost whose bump fails is
/// reported in-line rather than aborting the sweep: partial coverage is
/// surfaced, not hidden, and the caller can retry.
async fn session_revoke(node: &Arc<NodeState>, cmd: &IncomingCommand) -> (u8, String) {
    if let Err(rsp) = require_cap(cmd, "webd.session.revoke:webd.vhosts") {
        return rsp;
    }
    let email = match kwarg(cmd, "email") {
        Some(s) if !s.is_empty() => s,
        _ => return caller_error("missing required kwarg: email"),
    };

    let dir = node.vhosts.load();
    let mut revoked = Vec::new();
    let mut errors = Vec::new();
    let mut without_db = 0usize;
    for primary in &dir.primaries {
        let Some(db_mtx) = primary.state.db.as_ref() else {
            without_db += 1;
            continue;
        };

        // Take the SAME per-email epoch-cache slot the request path reads
        // ([`crate::current_session_epoch`]) and clear it BEFORE the DB bump.
        // Lock order is slot → DB (matches the request path — no deadlock).
        // Clearing first means an ambiguous DB failure leaves the cache
        // EMPTY: the next request re-reads and fails closed on error, rather
        // than continuing to serve the stale pre-revocation epoch for the TTL.
        let slot = primary.state.session_epoch_cache.slot(&email).await;
        let mut cached = slot.lock().await;
        *cached = None;

        let db = db_mtx.lock().await;
        // Inline (no block_in_place — panics on a current-thread
        // runtime): a one-row upsert + PK SELECT is microseconds.
        let bumped = (|| -> Result<i64, rusqlite::Error> {
            db.execute(
                "INSERT INTO session_epochs (email, epoch) VALUES (?1, 1) \
                 ON CONFLICT(email) DO UPDATE SET epoch = epoch + 1",
                rusqlite::params![email],
            )?;
            db.query_row(
                "SELECT epoch FROM session_epochs WHERE email = ?1",
                rusqlite::params![email],
                |r| r.get::<_, i64>(0),
            )
        })();
        match bumped {
            Ok(epoch) => {
                // Publish the committed epoch while STILL holding the slot, so
                // a request that completes after this revoke sees the NEW
                // epoch with no DB re-read and no stale window.
                *cached = Some(crate::CachedSessionEpoch {
                    epoch,
                    loaded_at: std::time::Instant::now(),
                });
                revoked.push(json!({
                    "vhost": primary.state.fqdn,
                    "epoch": epoch,
                }));
            }
            Err(e) => errors.push(json!({
                "vhost": primary.state.fqdn,
                "error": e.to_string(),
            })),
        }
    }

    // Opportunistic eviction on this rare, privileged path (no slot guard is
    // held here): bound each vhost's epoch cache so a peer revoking many
    // distinct emails can't grow it without limit. Keeps every referenced or
    // still-fresh slot (incl. the epochs just published above); drops only
    // unreferenced empty/expired entries.
    for primary in &dir.primaries {
        primary
            .state
            .session_epoch_cache
            .prune_unreferenced_stale()
            .await;
    }
    tracing::info!(
        email = %email,
        vhosts = revoked.len(),
        errors = errors.len(),
        "session.revoke: bumped session epoch (cookie-path revocation)"
    );
    (
        0,
        json!({
            "email": email,
            "revoked": revoked,
            "errors": errors,
            "vhosts_without_db": without_db,
        })
        .to_string(),
    )
}
