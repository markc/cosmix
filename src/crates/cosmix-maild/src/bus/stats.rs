//! `maild.stats.*` Bus action handlers — read-only admin/observability
//! surface for the maild human and Bus manager.
//!
//! The doveadm `mailbox status` / `quota get` / `who` / `stats dump`
//! analogs, exposed over Bus so an operator drives them with `mix -c
//! 'send maild.stats.mailboxes account="a@b.c"'` (or a thin
//! `maildadm.mix` wrapper) instead of a side-channel CLI. Pure reads;
//! no mutation.
//!
//! ## Auth (Phase 1)
//!
//! Permissive, matching the sibling direct verbs (`maild.accounts.*`,
//! `maild.search.*`): any peer reaching the dispatcher invokes these.
//! The WireGuard `/24` is the trust domain. These are reads of
//! per-account mailbox metadata (counts/bytes) and live-session counts,
//! no message content; a Phase-2 narrowing applies the same
//! `PeerIdentity` gate the props surface uses when account-scoped auth
//! lands (the destructive `maild.retention.*` surface carries that gate
//! from the start).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cosmix_client::IncomingCommand;

use crate::db;
use crate::imap::session::AccountSlots;
use crate::mailstore::SqliteMailStore;

const RC_ERROR: u8 = 10;

/// Default and ceiling for the `maild.stats.top` `limit` argument.
const TOP_DEFAULT_LIMIT: usize = 10;
const TOP_MAX_LIMIT: usize = 1000;

/// Per-account JMAP last-seen tracker — the "is this user using webmail"
/// proxy. JMAP is stateless HTTP (bearer/basic auth per request, no
/// session), so there is no live connection to count like IMAP; instead
/// the JMAP request handlers `touch` this map on every successful
/// authentication, and `maild.stats.online` reports how long ago each
/// account was last seen.
///
/// In-memory and `Instant`-based: last-seen is **time since the entry
/// was touched**, so it resets to "never seen" on daemon restart (an
/// honest limitation for a best-effort activity signal — an operator
/// asking "who is on webmail right now" wants recency, not durable
/// history). The map is bounded by the account count and never expires
/// entries; a once-seen account simply reports an ever-growing
/// seconds-ago until restart.
#[derive(Default)]
pub struct JmapActivity {
    inner: Mutex<HashMap<i32, Instant>>,
}

impl JmapActivity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record that `account_id` just made an authenticated JMAP request.
    /// Best-effort: a poisoned lock is silently skipped rather than
    /// panicking a request handler over an observability side-channel.
    pub fn touch(&self, account_id: i32) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(account_id, Instant::now());
        }
    }

    /// Snapshot `(account_id, seconds_since_last_seen)` for every
    /// tracked account. Elapsed is computed at snapshot time.
    pub fn snapshot(&self) -> Vec<(i32, u64)> {
        match self.inner.lock() {
            Ok(g) => g
                .iter()
                .map(|(&id, t)| (id, t.elapsed().as_secs()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Read-only state the `maild.stats.*` surface needs beyond the db /
/// mailstore: the live IMAP per-account connection counter (backs
/// `online`), the per-account JMAP last-seen tracker (also `online`),
/// and the daemon start instant (backs `server` uptime). Cheap to
/// clone — `slots` / `jmap_activity` are `Arc`s, `started_at` is `Copy`.
#[derive(Clone)]
pub struct StatsState {
    pub slots: Arc<AccountSlots>,
    pub jmap_activity: Arc<JmapActivity>,
    pub started_at: Instant,
}

/// Dispatch a `maild.stats.*` command. Returns `(rc, body_json)`.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    state: &StatsState,
) -> (u8, String) {
    match action {
        "mailboxes" => handle_mailboxes(cmd, db, mailstore).await,
        "account" => handle_account(cmd, db, mailstore).await,
        "online" => handle_online(db, state).await,
        "server" => handle_server(db, mailstore, state).await,
        "top" => handle_top(cmd, db, mailstore).await,
        other => (
            RC_ERROR,
            err_body(&format!("unknown stats action: {other}")),
        ),
    }
}

/// Resolve the account-address argument to an account row. Accepts the
/// address under either `account` or `email`, as a header (header wins)
/// or an `args.*` field — `account=<email>` is the advertised shape,
/// `email=` the sibling-verb convention. Returns `Err((rc, body))`
/// ready to return on any miss.
async fn resolve_account(
    cmd: &IncomingCommand,
    db: &db::Db,
) -> Result<db::account::Account, (u8, String)> {
    // Filter empties per-candidate so an empty `account` doesn't mask a
    // valid `email` (and vice-versa) in either the header or args path.
    let from_header = cmd
        .header("account")
        .filter(|s| !s.is_empty())
        .or_else(|| cmd.header("email").filter(|s| !s.is_empty()))
        .map(str::to_string);
    let email = match from_header {
        Some(s) => s,
        None => {
            let args = super::resolve_args(cmd);
            ["account", "email"]
                .iter()
                .find_map(|k| {
                    args.get(*k)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                })
                .map(str::to_string)
                .ok_or_else(|| (RC_ERROR, err_body("missing required 'account' argument")))?
        }
    };
    match db::account::get_by_email(&db.conn, &email).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err((RC_ERROR, err_body(&format!("no such account: {email}")))),
        Err(e) => Err((RC_ERROR, err_body(&format!("account lookup failed: {e}")))),
    }
}

/// `maild.stats.mailboxes account=<email>` — per-folder
/// `{name, role, total, unread, size_bytes}` for one account.
///
/// ```json
/// {"account": "a@b.c",
///  "mailboxes": [
///    {"name": "Inbox", "role": "inbox", "total": 42, "unread": 3, "size_bytes": 105000},
///    {"name": "Junk",  "role": "junk",  "total": 9,  "unread": 9, "size_bytes": 22000}
///  ]}
/// ```
async fn handle_mailboxes(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    let account = match resolve_account(cmd, db).await {
        Ok(a) => a,
        Err(e) => return e,
    };
    // The mailstore read is synchronous SQLite + an O(messages) scan;
    // offload it so a large account can't stall the Bus dispatch loop
    // (matches accounts.rs / search.rs).
    let ms = Arc::clone(mailstore);
    let id = account.id;
    let stats = match tokio::task::spawn_blocking(move || ms.mailbox_stats(id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return (RC_ERROR, err_body(&format!("mailbox_stats failed: {e}"))),
        Err(e) => return (RC_ERROR, err_body(&format!("stats task failed: {e}"))),
    };
    let mailboxes: Vec<serde_json::Value> = stats
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "role": m.role.map(|r| r.as_jmap_str()),
                "total": m.total_emails,
                "unread": m.unread_emails,
                "size_bytes": m.size_bytes,
            })
        })
        .collect();
    let body = serde_json::json!({
        "account": account.email,
        "mailboxes": mailboxes,
    });
    (0, body.to_string())
}

/// `maild.stats.account account=<email>` — account-wide rollup plus the
/// configured quota (doveadm `quota get`). `message_count` / `total_bytes`
/// are **distinct** (a multi-homed message counts once), so they read as
/// physical storage rather than a sum of folder occupancy.
///
/// ```json
/// {"account": "a@b.c", "mailbox_count": 6, "message_count": 51,
///  "total_bytes": 127000, "quota_bytes": 0}
/// ```
/// `quota_bytes` is the account's configured quota (`0` = unlimited /
/// unset); it is reported for visibility — maild does not enforce it.
async fn handle_account(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    let account = match resolve_account(cmd, db).await {
        Ok(a) => a,
        Err(e) => return e,
    };
    // Offload the synchronous mailstore scan (see handle_mailboxes).
    let ms = Arc::clone(mailstore);
    let id = account.id;
    let stat = match tokio::task::spawn_blocking(move || ms.account_stats(id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return (RC_ERROR, err_body(&format!("account_stats failed: {e}"))),
        Err(e) => return (RC_ERROR, err_body(&format!("stats task failed: {e}"))),
    };
    let quota_bytes = account.quota.max(0) as u64;
    let body = serde_json::json!({
        "account": account.email,
        "mailbox_count": stat.mailbox_count,
        "message_count": stat.message_count,
        "total_bytes": stat.total_bytes,
        "quota_bytes": quota_bytes,
    });
    (0, body.to_string())
}

/// `maild.stats.online` — live IMAP sessions per account (doveadm
/// `who`). Reads the [`AccountSlots`] snapshot the IMAP listener
/// maintains; only accounts with ≥1 open connection appear. Each
/// account id is resolved back to its address for legibility (a row
/// whose account was deleted mid-session reports `account: null` but
/// keeps its id + count).
///
/// JMAP is **stateless HTTP** — there is no live connection to count, so
/// the `jmap` section reports a best-effort last-seen proxy instead
/// (seconds since each account's most recent authenticated JMAP
/// request; in-memory, resets on daemon restart). The `imap` section is
/// an exact live-connection count.
///
/// ```json
/// {"imap": {
///    "accounts": [{"account": "a@b.c", "account_id": 5, "connections": 2}],
///    "total_connections": 2,
///    "distinct_accounts": 1
///  },
///  "jmap": {
///    "accounts": [{"account": "a@b.c", "account_id": 5, "last_seen_secs_ago": 12}],
///    "tracked_accounts": 1
///  }}
/// ```
async fn handle_online(db: &db::Db, state: &StatsState) -> (u8, String) {
    let mut imap_snap = state.slots.snapshot().await;
    // Deterministic order (by account id) so repeated reads and tests
    // are stable; the underlying map iteration order is not.
    imap_snap.sort_by_key(|(id, _)| *id);
    let mut jmap_snap = state.jmap_activity.snapshot();
    jmap_snap.sort_by_key(|(id, _)| *id);

    // Resolve each distinct account id → address once, shared across
    // both sections (an account both IMAP-connected and JMAP-active
    // appears in both but is looked up a single time). A lookup miss
    // (account deleted while a session lingers, or a transient db
    // error) maps to `null` rather than dropping the row.
    let mut emails: HashMap<i32, Option<String>> = HashMap::new();
    for id in imap_snap
        .iter()
        .map(|(id, _)| *id)
        .chain(jmap_snap.iter().map(|(id, _)| *id))
    {
        if emails.contains_key(&id) {
            continue;
        }
        let email = match db::account::get_by_id(&db.conn, id).await {
            Ok(Some(a)) => Some(a.email),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(account_id = id, error = %e, "stats.online id lookup failed");
                None
            }
        };
        emails.insert(id, email);
    }

    let total_connections: u64 = imap_snap.iter().map(|(_, n)| *n as u64).sum();
    let imap_accounts: Vec<serde_json::Value> = imap_snap
        .iter()
        .map(|(id, count)| {
            serde_json::json!({
                "account": emails.get(id).cloned().flatten(),
                "account_id": id,
                "connections": count,
            })
        })
        .collect();
    let jmap_accounts: Vec<serde_json::Value> = jmap_snap
        .iter()
        .map(|(id, secs)| {
            serde_json::json!({
                "account": emails.get(id).cloned().flatten(),
                "account_id": id,
                "last_seen_secs_ago": secs,
            })
        })
        .collect();

    let body = serde_json::json!({
        "imap": {
            "accounts": imap_accounts,
            "total_connections": total_connections,
            "distinct_accounts": imap_snap.len(),
        },
        "jmap": {
            "accounts": jmap_accounts,
            "tracked_accounts": jmap_snap.len(),
        }
    });
    (0, body.to_string())
}

/// `maild.stats.server` — server-wide rollup (doveadm `stats dump`):
/// account count, total distinct messages + bytes across every account,
/// live IMAP connections, and daemon uptime.
///
/// `message_count` / `total_bytes` sum each account's deduped
/// [`crate::mailstore::AccountStat`] (a multi-homed message counts once
/// *per account*; sets are per-account so there is no cross-account
/// sharing to dedupe). The whole-fleet mailstore scan is offloaded via
/// `spawn_blocking` so a large deployment can't stall the Bus dispatch
/// loop. A per-account scan error is logged and that account is skipped
/// rather than failing the whole rollup.
///
/// ```json
/// {"account_count": 12, "message_count": 4096, "total_bytes": 91000000,
///  "imap_connections": 3, "uptime_seconds": 86400}
/// ```
async fn handle_server(
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    state: &StatsState,
) -> (u8, String) {
    let accounts = match db::account::list(&db.conn).await {
        Ok(a) => a,
        Err(e) => return (RC_ERROR, err_body(&format!("account list failed: {e}"))),
    };
    let account_count = accounts.len() as u64;
    let ids: Vec<i32> = accounts.iter().map(|a| a.id).collect();

    // One blocking task for the whole-fleet aggregation: each
    // `account_stats` is a synchronous O(messages) mailstore scan, so
    // summing across every account off the async runtime keeps the
    // dispatch loop responsive.
    let ms = Arc::clone(mailstore);
    let rollup = tokio::task::spawn_blocking(move || {
        let mut message_count: u64 = 0;
        let mut total_bytes: u64 = 0;
        for id in ids {
            match ms.account_stats(id) {
                Ok(s) => {
                    message_count += s.message_count;
                    total_bytes += s.total_bytes;
                }
                // A single account's scan failing (e.g. a set wedged
                // mid-rebuild) must not zero the whole server rollup;
                // log and skip it.
                Err(e) => {
                    tracing::warn!(account_id = id, error = %e, "stats.server account scan skipped");
                }
            }
        }
        (message_count, total_bytes)
    })
    .await;
    let (message_count, total_bytes) = match rollup {
        Ok(t) => t,
        Err(e) => {
            return (
                RC_ERROR,
                err_body(&format!("server stats task failed: {e}")),
            );
        }
    };

    let imap_connections: u64 = state
        .slots
        .snapshot()
        .await
        .iter()
        .map(|(_, n)| *n as u64)
        .sum();
    let uptime_seconds = state.started_at.elapsed().as_secs();

    let body = serde_json::json!({
        "account_count": account_count,
        "message_count": message_count,
        "total_bytes": total_bytes,
        "imap_connections": imap_connections,
        "uptime_seconds": uptime_seconds,
    });
    (0, body.to_string())
}

/// `maild.stats.top [by=size|count] [limit=N]` — the largest accounts
/// by storage bytes (`by=size`, the default) or distinct message count
/// (`by=count`), capped at `limit` (default 10, max 1000). The doveadm
/// `mailbox status` sweep analog, at account granularity.
///
/// Each account's figure is its deduped [`crate::mailstore::AccountStat`]
/// (a multi-homed message counts once), so the ranking reflects physical
/// storage, not summed folder occupancy. The whole-fleet scan is
/// offloaded via `spawn_blocking`; a per-account scan error is logged
/// and that account is ranked as zero rather than failing the verb.
///
/// ```json
/// {"by": "size", "limit": 10, "accounts": [
///    {"account": "big@b.c", "account_id": 3, "message_count": 900, "total_bytes": 5_000_000}
/// ]}
/// ```
async fn handle_top(
    cmd: &IncomingCommand,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
) -> (u8, String) {
    // Strict args resolution: a present-but-malformed `args:` header or
    // body is an error, not a silent fall-through to defaults (matches
    // the `try_resolve_args` sibling verbs).
    let args = match super::try_resolve_args(cmd) {
        Ok(v) => v,
        Err(e) => return (RC_ERROR, err_body(&format!("invalid args: {e}"))),
    };
    // `by`: header wins (always a string); else args, which must be a
    // string or absent — a present non-string (`{"by":5}`) is rejected,
    // not silently defaulted. Only size|count.
    let by = if let Some(h) = cmd.header("by") {
        h.to_string()
    } else {
        match args.get("by") {
            None | Some(serde_json::Value::Null) => "size".to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => return (RC_ERROR, err_body(&format!("invalid 'by': {other}"))),
        }
    };
    if by != "size" && by != "count" {
        return (RC_ERROR, err_body("'by' must be 'size' or 'count'"));
    }
    // `limit`: header (string) or args (string|number), default 10,
    // clamped to [1, TOP_MAX_LIMIT].
    let limit = match resolve_limit(cmd, &args) {
        Ok(l) => l,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };

    let accounts = match db::account::list(&db.conn).await {
        Ok(a) => a,
        Err(e) => return (RC_ERROR, err_body(&format!("account list failed: {e}"))),
    };
    // id → email from the list itself (no extra lookups).
    let email_by_id: HashMap<i32, String> =
        accounts.iter().map(|a| (a.id, a.email.clone())).collect();
    let ids: Vec<i32> = accounts.iter().map(|a| a.id).collect();

    let ms = Arc::clone(mailstore);
    let by_size = by == "size";
    let ranked = tokio::task::spawn_blocking(move || {
        let mut rows: Vec<(i32, u64, u64)> = ids
            .into_iter()
            .map(|id| match ms.account_stats(id) {
                Ok(s) => (id, s.message_count, s.total_bytes),
                Err(e) => {
                    tracing::warn!(account_id = id, error = %e, "stats.top account scan skipped");
                    (id, 0, 0)
                }
            })
            .collect();
        // Sort descending by the chosen metric; tie-break on the other
        // metric then id so the order is deterministic.
        rows.sort_by(|a, b| {
            let (am, bm) = if by_size { (a.2, b.2) } else { (a.1, b.1) };
            let (as_, bs) = if by_size { (a.1, b.1) } else { (a.2, b.2) };
            bm.cmp(&am).then(bs.cmp(&as_)).then(a.0.cmp(&b.0))
        });
        rows
    })
    .await;
    let mut rows = match ranked {
        Ok(r) => r,
        Err(e) => return (RC_ERROR, err_body(&format!("top stats task failed: {e}"))),
    };
    rows.truncate(limit);

    let out: Vec<serde_json::Value> = rows
        .iter()
        .map(|(id, message_count, total_bytes)| {
            serde_json::json!({
                "account": email_by_id.get(id),
                "account_id": id,
                "message_count": message_count,
                "total_bytes": total_bytes,
            })
        })
        .collect();
    let body = serde_json::json!({
        "by": by,
        "limit": limit,
        "accounts": out,
    });
    (0, body.to_string())
}

/// Resolve the `limit` argument for `maild.stats.top`: header (string)
/// or args (string or JSON number), default [`TOP_DEFAULT_LIMIT`],
/// clamped to `[1, TOP_MAX_LIMIT]`. A present-but-unparseable value is
/// an error rather than a silent fallback.
fn resolve_limit(cmd: &IncomingCommand, args: &serde_json::Value) -> Result<usize, String> {
    if let Some(h) = cmd.header("limit") {
        let n: usize = h.parse().map_err(|_| format!("invalid 'limit': {h}"))?;
        return Ok(n.clamp(1, TOP_MAX_LIMIT));
    }
    match args.get("limit") {
        None => Ok(TOP_DEFAULT_LIMIT),
        Some(serde_json::Value::Null) => Ok(TOP_DEFAULT_LIMIT),
        Some(serde_json::Value::Number(n)) => {
            // `as_u64()` rejects negative/fractional; clamp as u64
            // BEFORE the usize cast so the result can't wrap on a
            // 32-bit target.
            let v = n.as_u64().ok_or_else(|| format!("invalid 'limit': {n}"))?;
            Ok(v.clamp(1, TOP_MAX_LIMIT as u64) as usize)
        }
        Some(serde_json::Value::String(s)) => {
            let n: usize = s.parse().map_err(|_| format!("invalid 'limit': {s}"))?;
            Ok(n.clamp(1, TOP_MAX_LIMIT))
        }
        Some(other) => Err(format!("invalid 'limit': {other}")),
    }
}

/// `{"error": "..."}` body for the error rc path.
fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(headers: &[(&str, &str)], args: serde_json::Value) -> IncomingCommand {
        let mut h = std::collections::BTreeMap::new();
        for (k, v) in headers {
            h.insert(k.to_string(), v.to_string());
        }
        IncomingCommand {
            command: "maild.stats.top".to_string(),
            from: String::new(),
            id: None,
            args,
            headers: h,
            body: String::new(),
        }
    }

    #[test]
    fn jmap_activity_tracks_and_snapshots() {
        let act = JmapActivity::new();
        assert!(act.snapshot().is_empty());
        act.touch(4);
        act.touch(9);
        let mut snap = act.snapshot();
        snap.sort_by_key(|(id, _)| *id);
        let ids: Vec<i32> = snap.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![4, 9]);
        // Re-touch is idempotent on the key set (still two accounts).
        act.touch(4);
        assert_eq!(act.snapshot().len(), 2);
    }

    #[test]
    fn resolve_limit_defaults_and_clamps() {
        // Absent → default.
        let c = cmd(&[], serde_json::Value::Null);
        assert_eq!(
            resolve_limit(&c, &super::super::resolve_args(&c)).unwrap(),
            TOP_DEFAULT_LIMIT
        );

        // Header numeric string, within range.
        let c = cmd(&[("limit", "3")], serde_json::Value::Null);
        assert_eq!(resolve_limit(&c, &serde_json::Value::Null).unwrap(), 3);

        // Header zero clamps up to 1.
        let c = cmd(&[("limit", "0")], serde_json::Value::Null);
        assert_eq!(resolve_limit(&c, &serde_json::Value::Null).unwrap(), 1);

        // Over-max clamps down.
        let c = cmd(&[("limit", "99999")], serde_json::Value::Null);
        assert_eq!(
            resolve_limit(&c, &serde_json::Value::Null).unwrap(),
            TOP_MAX_LIMIT
        );

        // args JSON number path.
        let args = serde_json::json!({"limit": 7});
        let c = cmd(&[], args.clone());
        assert_eq!(resolve_limit(&c, &args).unwrap(), 7);

        // Present-but-garbage header → error, not silent default.
        let c = cmd(&[("limit", "abc")], serde_json::Value::Null);
        assert!(resolve_limit(&c, &serde_json::Value::Null).is_err());
    }
}
