//! `maild.accounts.*` Bus action handlers.
//!
//! Three actions: `maild.accounts.seed_mailboxes` — idempotent backfill of
//! the six JMAP default mailboxes (Inbox/Drafts/Sent/Junk/Trash/Archive)
//! for one or all accounts; `maild.accounts.seed_content` — idempotent
//! creation of the flat top-level `Posts`/`Pages` content folders for one
//! account (the vtoken email-to-post destination tree); and
//! `maild.accounts.revoke_tokens` — revoke every live bearer token for one
//! account (the operator session-kill primitive). The CLI rewrite (C7
//! follow-up "Bus-only `cosmix-maild account *`") routes
//! `cosmix-maild account seed-mailboxes` here instead of opening MDS
//! and the SQL DB locally; the same call shape is reachable from any
//! WG-mesh peer with the (currently permissive) trust grant.
//!
//! ## Why not `<svc>.props.set` replay?
//!
//! Backfill targets include accounts created before SPEC 12 (Phase 6/7
//! cutover) whose rows live in the `accounts` SQL table but have no
//! `__props_records` row. A `<svc>.props.set` against those accounts
//! would create a fresh substrate record from scratch, requiring the
//! pre-hashed password in the body — which the CLI cannot retrieve
//! (`<svc>.props.get` returns `password: null` per the namespace's
//! secret-field redaction). A bespoke action keeps the legacy-to-
//! substrate induction out of the seed path.
//!
//! ## Auth (Phase 1)
//!
//! Permissive: any peer reaching the dispatcher invokes this action.
//! The WireGuard `/24` is the trust domain. Phase 2+ narrows.

use std::collections::BTreeMap;
use std::sync::Arc;

use cosmix_client::IncomingCommand;
use cosmix_props::record::RecordKey;
use cosmix_props::runtime::Runtime;
use cosmix_props::value::PropValue;
use cosmix_props::{Actor, MergeMode, SetOpts};

use crate::db;
use crate::mailstore::SqliteMailStore;
use crate::props::accounts::seed_default_mailboxes_idempotent;

const RC_ERROR: u8 = 10;

/// Sentinel prefix marking a locked password hash.
///
/// `!` cannot begin a valid bcrypt hash, and `auth::basic::verify` runs
/// `bcrypt::verify(pw, hash).unwrap_or(false)` — a malformed hash fails CLOSED.
/// So a locked account rejects every password while the original hash is kept
/// verbatim behind the prefix, which is what makes `unlock` lossless. (Prefixing
/// beats overwriting the hash: an overwrite would force a password RESET to undo,
/// turning a reversible suspend into a destructive one.)
const LOCK_PREFIX: &str = "!";

/// Dispatch a `maild.accounts.*` command. Returns `(rc, body_json)`.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    accounts_runtime: &Arc<Runtime>,
) -> (u8, String) {
    match action {
        "seed_mailboxes" => handle_seed_mailboxes(cmd, db, mailstore).await,
        "seed_content" => handle_seed_content(cmd, db, mailstore).await,
        "revoke_tokens" => handle_revoke_tokens(cmd, db).await,
        "verify" => handle_verify(cmd, db).await,
        "lock" => handle_set_lock(cmd, db, accounts_runtime, true).await,
        "unlock" => handle_set_lock(cmd, db, accounts_runtime, false).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown accounts action: {other}")),
        ),
    }
}

/// `maild.accounts.verify` — does `password` authenticate as `email`?
///
/// Exists because the stored hash is unreadable to a client: the namespace
/// declares `password` as `secret: true`, so `props.get` redacts it. Verifying
/// daemon-side keeps the hash off the wire entirely and reuses the SAME
/// `auth::basic::verify` path IMAP/JMAP Basic auth takes, so a "valid" here
/// means the credential really would log in — not that it merely matches some
/// separately-reimplemented rule.
///
/// A wrong password is NOT an error: rc=0 with `valid: false`. rc>=10 is
/// reserved for a genuine failure (DB unreachable), so callers can tell
/// "authentication was answered, negatively" from "the check never ran".
/// An unknown account also reports `valid: false` (no account enumeration).
/// Response: `{"email","valid":<bool>}`.
async fn handle_verify(cmd: &IncomingCommand, db: &db::Db) -> (u8, String) {
    let email = match required_email(cmd) {
        Ok(e) => e,
        Err(body) => return (RC_ERROR, body),
    };
    let password = match super::resolve_args(cmd)
        .get("password")
        .and_then(|v| v.as_str())
    {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return (RC_ERROR, err_body("password arg required")),
    };
    match crate::auth::basic::verify(db, &email, &password).await {
        Ok(account_id) => {
            let valid = account_id.is_some();
            // Log the OUTCOME but never the password. An operator verifying a
            // credential is an audit-worthy event.
            tracing::info!(email = %email, valid, "account password verification");
            (
                0,
                serde_json::json!({ "email": email, "valid": valid }).to_string(),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("verify failed: {e}"))),
    }
}

/// `maild.accounts.lock` / `.unlock` — toggle the [`LOCK_PREFIX`] on the stored
/// hash. Must run daemon-side: the hash has to be READ to be preserved, and no
/// client can read it (secret-field redaction).
///
/// Writes through the accounts props Runtime rather than SQL directly, so the
/// substrate record, the audit log, the `records.changed` fan-out and — crucially
/// — `AccountsHooks::after_set`'s post-commit `clear_verify_cache()` all observe
/// it. A raw SQL UPDATE would leave a locked account still authenticating from
/// the positive Basic-credential cache until its TTL expired.
///
/// Idempotent: `changed: false` when already in the requested state.
/// Response: `{"email","locked":<bool>,"changed":<bool>}`.
async fn handle_set_lock(
    cmd: &IncomingCommand,
    db: &db::Db,
    accounts_runtime: &Arc<Runtime>,
    lock: bool,
) -> (u8, String) {
    let email = match required_email(cmd) {
        Ok(e) => e,
        Err(body) => return (RC_ERROR, body),
    };
    let account = match db::account::get_by_email(&db.conn, &email).await {
        Ok(Some(a)) => a,
        Ok(None) => return (RC_ERROR, err_body(&format!("account {email} not found"))),
        Err(e) => return (RC_ERROR, err_body(&format!("account lookup failed: {e}"))),
    };

    let currently_locked = account.password.starts_with(LOCK_PREFIX);
    if currently_locked == lock {
        return (
            0,
            serde_json::json!({ "email": email, "locked": lock, "changed": false }).to_string(),
        );
    }
    let new_hash = if lock {
        format!("{LOCK_PREFIX}{}", account.password)
    } else {
        // Strip exactly ONE prefix — `strip_prefix`, not `trim_start_matches`,
        // which would eat a run of them.
        account
            .password
            .strip_prefix(LOCK_PREFIX)
            .unwrap_or(&account.password)
            .to_string()
    };

    // Patch ONLY `password`: the substrate merges over the stored record, so
    // name/quota/spam_*/mfa survive untouched. Scalar field, last-write-wins,
    // so no `expected_version` guard.
    let mut fields = BTreeMap::new();
    fields.insert("email".to_string(), PropValue::String(email.clone()));
    fields.insert("password".to_string(), PropValue::String(new_hash));
    let key = RecordKey::collection(crate::props::accounts::namespace_name(), email.clone());
    let cause = if lock {
        "accounts.lock"
    } else {
        "accounts.unlock"
    };
    let res = accounts_runtime
        .set(
            key,
            PropValue::Object(fields),
            SetOpts {
                expected_version: None,
                merge: MergeMode::Patch,
                actor: Actor::service("maild.accounts").expect("valid actor"),
                cause: Some(cause.to_string()),
                ts_ms: chrono::Utc::now().timestamp_millis(),
            },
        )
        .await;
    match res {
        Ok(_) => {
            tracing::info!(email = %email, locked = lock, "account lock state changed");
            (
                0,
                serde_json::json!({ "email": email, "locked": lock, "changed": true }).to_string(),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("{cause} failed: {e}"))),
    }
}

/// The `email` header, falling back to `args.email` — the shape every handler
/// in this module accepts.
fn required_email(cmd: &IncomingCommand) -> Result<String, String> {
    if let Some(s) = cmd.header("email")
        && !s.is_empty()
    {
        return Ok(s.to_string());
    }
    match super::resolve_args(cmd)
        .get("email")
        .and_then(|v| v.as_str())
    {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(err_body("email header/arg required")),
    }
}

/// `maild.accounts.revoke_tokens` — revoke EVERY live bearer token for the
/// account named by the `email` header (or `args.email`). The operator
/// "kill this account's sessions" primitive, composed by the unified
/// revocation script together with webd's `webd.session.revoke` (the
/// cookie-path epoch bump) so a revocation always hits BOTH authorities.
/// Response: `{"email","id","revoked":<n>}` (`revoked` counts tokens newly
/// revoked; already-revoked/expired rows don't count — idempotent).
async fn handle_revoke_tokens(cmd: &IncomingCommand, db: &db::Db) -> (u8, String) {
    let email = match cmd.header("email") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match super::resolve_args(cmd)
            .get("email")
            .and_then(|v| v.as_str())
        {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return (RC_ERROR, err_body("email header/arg required")),
        },
    };
    let account = match db::account::get_by_email(&db.conn, &email).await {
        Ok(Some(a)) => a,
        Ok(None) => return (RC_ERROR, err_body(&format!("account {email} not found"))),
        Err(e) => return (RC_ERROR, err_body(&format!("account lookup failed: {e}"))),
    };
    match db::token::revoke_all_for_account(&db.conn, account.id).await {
        Ok(revoked) => {
            tracing::info!(email = %email, revoked, "revoked all bearer tokens for account");
            (
                0,
                serde_json::json!({ "email": email, "id": account.id, "revoked": revoked })
                    .to_string(),
            )
        }
        Err(e) => (RC_ERROR, err_body(&format!("token revoke failed: {e}"))),
    }
}

/// `maild.accounts.seed_content` — idempotently create the flat top-level
/// content folders (`Posts`, `Pages`) for the account named by the
/// `email` header (or `args.email`). Permissive (any mesh peer), like
/// `seed_mailboxes` — it only creates empty public-content folders.
/// Response: `{"email","id","created":[...]}`.
async fn handle_seed_content(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    let email = match cmd.header("email") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match super::resolve_args(cmd)
            .get("email")
            .and_then(|v| v.as_str())
        {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return (RC_ERROR, err_body("email header/arg required")),
        },
    };
    let account = match db::account::get_by_email(&db.conn, &email).await {
        Ok(Some(a)) => a,
        Ok(None) => return (RC_ERROR, err_body(&format!("account {email} not found"))),
        Err(e) => return (RC_ERROR, err_body(&format!("account lookup failed: {e}"))),
    };
    let ms = mailstore.clone();
    let id = account.id;
    match tokio::task::spawn_blocking(move || {
        crate::props::accounts::seed_content_folders_idempotent(&ms, id)
    })
    .await
    {
        Ok(Ok(created)) => (
            0,
            serde_json::json!({ "email": email, "id": id, "created": created }).to_string(),
        ),
        Ok(Err(e)) => (RC_ERROR, err_body(&format!("seed_content failed: {e}"))),
        Err(e) => (
            RC_ERROR,
            err_body(&format!("seed_content task panicked: {e}")),
        ),
    }
}

/// `maild.accounts.seed_mailboxes` — optional `email` header narrows
/// to a single account; otherwise every row in the `accounts` table
/// is processed.
///
/// Response body shape:
/// ```json
/// {
///   "results": [
///     {"email": "a@b.c", "id": 1, "created": ["Inbox", "Drafts", ...], "error": null},
///     {"email": "x@y.z", "id": 2, "created": [],                       "error": "..."}
///   ],
///   "failed": 0
/// }
/// ```
///
/// `created` is empty for accounts that were already fully seeded
/// (matches the legacy CLI's `"already seeded"` line).
///
/// `rc` semantics: `rc = 0` if every target succeeded, `rc = RC_ERROR`
/// if any target failed. The caller renders per-account outcomes from
/// the body either way, so a single bad row does not lose the
/// successes that ran before it.
async fn handle_seed_mailboxes(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    // Resolve targets. We accept the email via the `email:` header
    // (matching the `<svc>.props.*` convention) OR — for callers that
    // prefer the JSON-args shape used by `maild.bayesian.*` — via the
    // `args.email` field. Header wins if both are present.
    let email_arg = match cmd.header("email") {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => {
            let args = super::resolve_args(cmd);
            args.get("email")
                .and_then(|v| v.as_str())
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

    // Per-account seeding runs on a blocking pool — `seed_default_mailboxes_idempotent`
    // is fully synchronous and hits both rusqlite and MDS. Each target
    // is independent; a failure on one account does not stop the
    // others (matches the legacy CLI semantics).
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(targets.len());
    let mut failed: usize = 0;
    for (id, email) in targets {
        let ms = mailstore.clone();
        let email_for_task = email.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            // Snapshot the pre-existing roles so we can derive the
            // "newly created" list — the idempotent seeder does not
            // return one (it has no notion of "what was missing");
            // here we re-implement the same logic in terms of the
            // public seeder + a list_mailboxes snapshot before/after.
            //
            // We use the helper from props::accounts so the seeding
            // behaviour (collision rules, race tolerance) stays in
            // one place rather than being re-implemented per caller.
            use crate::mailstore::MailStore as _;
            let before: Vec<crate::mailstore::MailboxRole> = ms
                .list_mailboxes(id)
                .map_err(|e| anyhow::anyhow!("list_mailboxes: {e}"))?
                .into_iter()
                .filter_map(|m| m.role)
                .collect();
            seed_default_mailboxes_idempotent(&ms, id)?;
            let after = ms
                .list_mailboxes(id)
                .map_err(|e| anyhow::anyhow!("list_mailboxes: {e}"))?;
            // Report names of mailboxes whose role landed in `after`
            // but was missing from `before`. Iterating
            // crate::props::accounts::DEFAULT_MAILBOXES rather than
            // `after` keeps the display order stable. `before` is a
            // Vec because MailboxRole isn't Ord; the linear scan over
            // ≤6 entries is fine.
            let created: Vec<&'static str> = crate::props::accounts::DEFAULT_MAILBOXES
                .iter()
                .filter(|(_, role, _)| !before.contains(role))
                .filter(|(_, role, _)| after.iter().any(|m| m.role == Some(*role)))
                .map(|(name, _, _)| *name)
                .collect();
            Ok::<Vec<&'static str>, anyhow::Error>(created)
        })
        .await;

        let entry = match outcome {
            Ok(Ok(created)) => serde_json::json!({
                "email": email_for_task,
                "id": id,
                "created": created,
                "error": serde_json::Value::Null,
            }),
            Ok(Err(e)) => {
                failed += 1;
                serde_json::json!({
                    "email": email_for_task,
                    "id": id,
                    "created": Vec::<&str>::new(),
                    "error": e.to_string(),
                })
            }
            Err(e) => {
                failed += 1;
                serde_json::json!({
                    "email": email_for_task,
                    "id": id,
                    "created": Vec::<&str>::new(),
                    "error": format!("join error: {e}"),
                })
            }
        };
        results.push(entry);
    }

    let body = serde_json::json!({
        "results": results,
        "failed": failed,
    })
    .to_string();
    let rc = if failed == 0 { 0 } else { RC_ERROR };
    (rc, body)
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}
