//! `maild.search.*` Bus action handlers.
//!
//! One action today: `maild.search.rebuild` — Phase 8e gate 3 admin
//! verb that wipes and re-derives the `mail_search` FTS5 projection
//! for one account (or all accounts) from the underlying
//! `mail_envelopes` rows + CAS blob bodies. Useful when the
//! projection is corrupt or absent (failed migration, manual wipe).
//!
//! ## Auth (Phase 1)
//!
//! Permissive: any peer reaching the dispatcher invokes this action.
//! The WireGuard `/24` is the trust domain. Phase 2+ narrows. Same
//! posture as `maild.accounts.seed_mailboxes` (see `accounts.rs` for
//! the rationale).
//!
//! ## Request shape
//!
//! Either an `email:` header OR an `args.email` field narrows the
//! rebuild to one account. Omitting both rebuilds every account in
//! the `accounts` table.
//!
//! ## Response shape
//!
//! ```json
//! {
//!   "results": [
//!     {"email": "a@b.c", "id": 1, "items_rebuilt": 42, "blob_load_failures": 0, "error": null},
//!     {"email": "x@y.z", "id": 2, "items_rebuilt": 0,  "blob_load_failures": 0, "error": "..."}
//!   ],
//!   "failed": 0
//! }
//! ```
//!
//! `rc = 0` iff every target succeeded; `rc = RC_ERROR` if any
//! target failed. Per-account outcomes are always in the body so a
//! single failed account does not lose the successes that ran before
//! it (matches the `seed_mailboxes` convention).

use std::sync::Arc;

use cosmix_client::IncomingCommand;

use crate::db;
use crate::mailstore::SqliteMailStore;

const RC_ERROR: u8 = 10;

/// Dispatch a `maild.search.*` command. Returns `(rc, body_json)`.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    match action {
        "rebuild" => handle_rebuild(cmd, db, mailstore).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown search action: {other}")),
        ),
    }
}

async fn handle_rebuild(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    // Resolve targets. Header wins over args (matches accounts.rs);
    // empty `email` is treated as "not present" so a caller can pass
    // `email: ""` to mean "all accounts" without separate plumbing.
    let email_arg = match cmd.header("email") {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => {
            let args = super::resolve_args(cmd);
            args.get("email")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
    };

    let targets: Vec<(i32, String)> = match email_arg {
        Some(addr) => match db::account::get_by_email(&db.conn, &addr).await {
            Ok(Some(a)) => vec![(a.id, a.email)],
            Ok(None) => {
                return (RC_ERROR, err_body(&format!("account {addr} not found")));
            }
            Err(e) => {
                return (RC_ERROR, err_body(&format!("account lookup failed: {e}")));
            }
        },
        None => match db::account::list(&db.conn).await {
            Ok(accounts) => accounts.into_iter().map(|a| (a.id, a.email)).collect(),
            Err(e) => {
                return (RC_ERROR, err_body(&format!("account list failed: {e}")));
            }
        },
    };

    // Per-account rebuild runs on the blocking pool —
    // `rebuild_search_index` does a single `with_set_tx` that holds
    // the per-set mutex for the duration of the rebuild. Targets are
    // processed sequentially (not concurrently) because the verb is
    // an administrative one-shot, not a hot path; a failure on one
    // account does not stop the others.
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(targets.len());
    let mut failed: usize = 0;
    for (id, email) in targets {
        let ms = mailstore.clone();
        let outcome = tokio::task::spawn_blocking(move || ms.rebuild_search_index(id)).await;
        let entry = match outcome {
            Ok(Ok(report)) => serde_json::json!({
                "email": email,
                "id": id,
                "items_rebuilt": report.items_rebuilt,
                "blob_load_failures": report.blob_load_failures,
                "error": serde_json::Value::Null,
            }),
            Ok(Err(e)) => {
                failed += 1;
                serde_json::json!({
                    "email": email,
                    "id": id,
                    "items_rebuilt": 0u64,
                    "blob_load_failures": 0u64,
                    "error": e.to_string(),
                })
            }
            Err(e) => {
                failed += 1;
                serde_json::json!({
                    "email": email,
                    "id": id,
                    "items_rebuilt": 0u64,
                    "blob_load_failures": 0u64,
                    "error": format!("spawn_blocking join failed: {e}"),
                })
            }
        };
        results.push(entry);
    }

    let rc = if failed == 0 { 0 } else { RC_ERROR };
    let body = serde_json::json!({
        "results": results,
        "failed": failed,
    })
    .to_string();
    (rc, body)
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}
