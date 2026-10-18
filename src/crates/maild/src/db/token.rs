//! `account_tokens` storage — opaque bearer tokens for JMAP auth.
//!
//! Only the **SHA-256 (hex)** of a token is persisted; the plaintext is
//! returned once at issue and never stored (see the `account_tokens`
//! schema in [`super::SCHEMA`]). All lookups key on that hash, so a DB
//! read can never reconstruct a usable credential. Expiry and revocation
//! are evaluated in SQL (`revoked_at IS NULL AND expires_at >
//! datetime('now')`) so a single indexed query answers "is this token
//! live right now".

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// A token row resolved from a presented token hash. Returned by
/// [`lookup_valid`] (already filtered to live tokens) and surfaced by the
/// `verify` endpoint. The plaintext token is never part of this struct.
#[derive(Debug, Clone)]
pub struct ValidToken {
    pub account_id: i32,
    pub email: String,
    pub expires_at: String,
}

/// What [`insert`] returns to the caller so the issue handler can echo the
/// durable timestamps back to the client. The plaintext token lives only
/// in the caller (it generated it); we store the hash.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub account_id: i32,
    pub issued_at: String,
    pub expires_at: String,
}

/// Insert a token row. `token_hash` is the hex SHA-256 of the opaque
/// plaintext; `ttl_secs` sets `expires_at = datetime('now', '+N seconds')`
/// computed by SQLite so issue/expiry share one clock. Seconds (not days)
/// granularity so callers can mint short-lived service tokens (webd's
/// dev_session/public_read seams want ~1h, not 30d). Returns the durable
/// `issued_at`/`expires_at` the row was stamped with.
pub async fn insert(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    token_hash: &str,
    label: Option<&str>,
    ttl_secs: i64,
) -> Result<IssuedToken> {
    let conn = conn.clone();
    let token_hash = token_hash.to_string();
    let label = label.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || -> Result<IssuedToken> {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // `±N seconds` is interpolated as an integer modifier (ttl_secs is
        // i64, never user text) — SQLite has no bind slot inside a
        // datetime modifier string. The `{:+}` forced sign keeps a
        // negative TTL well-formed (`-1 seconds`, used by the expiry test);
        // a bare `+{n}` would emit the invalid `+-1 seconds` → NULL.
        let expires_modifier = format!("{ttl_secs:+} seconds");
        conn.execute(
            "INSERT INTO account_tokens (account_id, token_hash, label, expires_at) \
             VALUES (?1, ?2, ?3, datetime('now', ?4))",
            params![account_id, token_hash, label, expires_modifier],
        )?;
        let (issued_at, expires_at): (String, String) = conn.query_row(
            "SELECT issued_at, expires_at FROM account_tokens WHERE token_hash = ?1",
            params![token_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(IssuedToken {
            account_id,
            issued_at,
            expires_at,
        })
    })
    .await?
}

/// Resolve a token hash to its account, **only if** the token is live
/// (not revoked, not expired). Returns `None` for unknown, revoked, or
/// expired tokens — the caller cannot distinguish the three (no oracle).
pub async fn lookup_valid(
    conn: &Arc<Mutex<Connection>>,
    token_hash: &str,
) -> Result<Option<ValidToken>> {
    let conn = conn.clone();
    let token_hash = token_hash.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<ValidToken>> {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let row = conn
            .query_row(
                "SELECT a.id, a.email, t.expires_at \
                 FROM account_tokens t \
                 JOIN accounts a ON a.id = t.account_id \
                 WHERE t.token_hash = ?1 \
                   AND t.revoked_at IS NULL \
                   AND t.expires_at > datetime('now')",
                params![token_hash],
                |row| {
                    Ok(ValidToken {
                        account_id: row.get(0)?,
                        email: row.get(1)?,
                        expires_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    })
    .await?
}

/// Revoke a token by hash (set `revoked_at`). Idempotent: returns `true`
/// if a matching row exists (whether or not it was already revoked),
/// `false` if no such token was ever issued. The caller treats "exists"
/// as success so a double-logout is a no-op, while an unknown token is a
/// 401 (never-issued ≠ logged-out).
pub async fn revoke(conn: &Arc<Mutex<Connection>>, token_hash: &str) -> Result<bool> {
    let conn = conn.clone();
    let token_hash = token_hash.to_string();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // Set revoked_at only on the first revoke; the EXISTS check below
        // still reports `true` for an already-revoked row so logout is
        // idempotent without resurrecting/altering the original stamp.
        conn.execute(
            "UPDATE account_tokens SET revoked_at = datetime('now') \
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![token_hash],
        )?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_tokens WHERE token_hash = ?1)",
            params![token_hash],
            |row| row.get(0),
        )?;
        Ok(exists)
    })
    .await?
}

/// Revoke EVERY live token for an account (set `revoked_at` on all rows
/// not already revoked). Returns how many tokens were newly revoked.
/// The operator-facing "kill this account's sessions" primitive — the
/// per-hash `revoke` above is the self-service logout path.
pub async fn revoke_all_for_account(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
) -> Result<usize> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changed = conn.execute(
            "UPDATE account_tokens SET revoked_at = datetime('now') \
             WHERE account_id = ?1 AND revoked_at IS NULL",
            params![account_id],
        )?;
        Ok(changed)
    })
    .await?
}
