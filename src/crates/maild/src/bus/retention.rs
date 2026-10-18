//! `maild.retention.*` Bus action verbs — the retention worker's control
//! surface.
//!
//! - `maild.retention.status` — observability (open to every WG peer):
//!   the effective config (armed window, dry-run, cadence) plus the last
//!   sweep's outcome.
//! - `maild.retention.run [account=<email>] [dry_run=<bool>]` —
//!   **destructive**: drives one sweep now. Operator-gated: only an Bus
//!   sender (`cmd.from`) in the configured `retention_operators`
//!   allowlist may invoke it (the same gate as the `props.write`
//!   config-arming cap; empty allowlist ⇒ nobody). This is the Option-B
//!   hook — a systemd timer could call exactly this verb instead of the
//!   in-process loop.

use std::sync::Arc;

use cosmix_client::IncomingCommand;

use crate::retention_worker::{RetentionRunReport, RetentionWorker};

const RC_ERROR: u8 = 10;

/// Shared state for the `maild.retention.*` verbs: the worker handle
/// (its status cell is shared with the spawned loop) and the operator
/// write-allowlist that gates the destructive `run` verb.
#[derive(Clone)]
pub struct RetentionBusState {
    pub worker: Arc<RetentionWorker>,
    pub operators: Arc<Vec<String>>,
}

/// Dispatch a `maild.retention.*` command. Returns `(rc, body_json)`.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    state: &RetentionBusState,
) -> (u8, String) {
    match action {
        "status" => handle_status(state).await,
        "run" => handle_run(cmd, state).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown retention action: {other}")),
        ),
    }
}

/// `maild.retention.status` — current policy + last sweep outcome.
async fn handle_status(state: &RetentionBusState) -> (u8, String) {
    let cfg = match state.worker.current_config().await {
        Ok(c) => c,
        Err(e) => return (RC_ERROR, err_body(&format!("read retention config: {e}"))),
    };
    let (last_sweep_ms, last) = state.worker.status().snapshot();
    let body = serde_json::json!({
        "armed": !cfg.is_disabled(),
        "dry_run": cfg.dry_run,
        "junk_retention_days": cfg.junk_retention_days,
        "trash_retention_days": cfg.trash_retention_days,
        "tick_minutes": cfg.tick_minutes,
        "max_deletes_per_sweep": cfg.max_deletes_per_sweep,
        "armed_accounts": cfg.armed_accounts,
        "armed_account_count": cfg.armed_accounts.len(),
        "operators_configured": !state.operators.is_empty(),
        "last_sweep_epoch_ms": last_sweep_ms,
        "last_sweep": last.as_ref().map(report_json),
    });
    (0, body.to_string())
}

/// `maild.retention.run` — drive one sweep now (operator-gated).
async fn handle_run(cmd: &IncomingCommand, state: &RetentionBusState) -> (u8, String) {
    // Operator gate: the Bus sender must be in the allowlist. An
    // anonymous caller (noded strips `from`) has an empty `from` and is
    // refused; an empty allowlist refuses everyone.
    let from = cmd.from.as_str();
    let authorized = !from.is_empty() && state.operators.iter().any(|o| o == from);
    if !authorized {
        return (
            RC_ERROR,
            err_body(
                "auth_denied: maild.retention.run requires an Bus sender in retention_operators",
            ),
        );
    }

    // Strict args resolution: a present-but-malformed `args:` header or
    // body must FAIL the destructive verb, not silently fall through to
    // config defaults with no account scope.
    let args = match super::try_resolve_args(cmd) {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&format!("invalid args: {e}"))),
    };

    // Optional `dry_run=` override (header or args). Absent ⇒ use config.
    let dry_run_override = match read_bool_arg(cmd, &args, "dry_run") {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };

    // Optional `account=<email>` — resolve to an id, scoping the sweep to
    // one account. Absent ⇒ all accounts.
    let account_email = cmd
        .header("account")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            args.get("account")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let account_override = match account_email {
        None => None,
        Some(email) => {
            match crate::db::account::get_by_email(&state.worker.db().conn, &email).await {
                Ok(Some(a)) => Some(a.id),
                Ok(None) => return (RC_ERROR, err_body(&format!("no such account: {email}"))),
                Err(e) => return (RC_ERROR, err_body(&format!("account lookup failed: {e}"))),
            }
        }
    };

    match state.worker.sweep(dry_run_override, account_override).await {
        Ok(report) => {
            let mut body = report_json(&report);
            if let serde_json::Value::Object(ref mut m) = body {
                m.insert("ok".into(), serde_json::Value::Bool(true));
            }
            (0, body.to_string())
        }
        Err(e) => (RC_ERROR, err_body(&format!("retention sweep failed: {e}"))),
    }
}

fn report_json(r: &RetentionRunReport) -> serde_json::Value {
    serde_json::json!({
        "accounts_swept": r.accounts_swept,
        "junk_removed": r.junk_removed,
        "trash_removed": r.trash_removed,
        "total_removed": r.total_removed(),
        "capped_accounts": r.capped_accounts,
        "gc_ran": r.gc_ran,
        "dry_run": r.dry_run,
        "disabled": r.disabled,
    })
}

/// Read an optional bool arg from header (`"true"`/`"false"`) or args
/// (JSON bool or string). `Ok(None)` when absent; `Err` on a malformed
/// present value.
fn read_bool_arg(
    cmd: &IncomingCommand,
    args: &serde_json::Value,
    key: &str,
) -> Result<Option<bool>, String> {
    if let Some(h) = cmd.header(key) {
        return h
            .parse::<bool>()
            .map(Some)
            .map_err(|_| format!("invalid '{key}': {h} (want true/false)"));
    }
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(serde_json::Value::String(s)) => s
            .parse::<bool>()
            .map(Some)
            .map_err(|_| format!("invalid '{key}': {s} (want true/false)")),
        Some(other) => Err(format!("invalid '{key}': {other} (want a bool)")),
    }
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}
