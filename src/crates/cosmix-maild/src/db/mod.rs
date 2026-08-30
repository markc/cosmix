//! SQLite database layer (rusqlite).

pub mod account;
pub mod blob;
pub mod calendar;
pub mod changelog;
pub mod contact;
pub mod token;
pub mod tombstone;
pub mod vacation;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::Connection;

/// One side of a conditional-request header (`If-Match` / `If-None-Match`).
#[derive(Debug, Clone)]
pub enum IfClause {
    /// `*` — matches any existing object.
    Star,
    /// A list of entity-tags (verbatim, including any `W/` weak prefix).
    Tags(Vec<String>),
}

/// A DAV write precondition parsed from `If-Match` / `If-None-Match`.
/// Evaluated **inside** the same DB lock as the write (see
/// `conditional_upsert_*` / `conditional_delete_*`) so the
/// read-check-write is atomic — no TOCTOU window between checking the
/// ETag and committing the change.
#[derive(Debug, Clone, Default)]
pub struct DavPrecondition {
    pub if_match: Option<IfClause>,
    pub if_none_match: Option<IfClause>,
}

impl DavPrecondition {
    /// May the write proceed? `current_etag` is the strong ETag of the
    /// existing row (`None` if absent). `If-Match` uses the **strong**
    /// comparator (a `W/` tag never matches, RFC 7232 §3.1); `If-None-
    /// Match` uses the **weak** comparator (§3.2).
    pub fn allows(&self, current_etag: Option<&str>, exists: bool) -> bool {
        if let Some(clause) = &self.if_none_match {
            match clause {
                IfClause::Star => {
                    if exists {
                        return false;
                    }
                }
                IfClause::Tags(tags) => {
                    if let Some(cur) = current_etag
                        && tags.iter().any(|t| weak_eq(t, cur))
                    {
                        return false;
                    }
                }
            }
        }
        if let Some(clause) = &self.if_match {
            match clause {
                IfClause::Star => {
                    if !exists {
                        return false;
                    }
                }
                IfClause::Tags(tags) => match current_etag {
                    Some(cur) if tags.iter().any(|t| strong_eq(t, cur)) => {}
                    _ => return false,
                },
            }
        }
        true
    }
}

/// Strong comparison (RFC 7232 §2.3.2): neither operand weak, octet-equal.
/// Our minted tags are always strong, so a `W/`-prefixed request tag can
/// never strong-match.
fn strong_eq(tag: &str, current: &str) -> bool {
    !tag.starts_with("W/") && tag == current
}

/// Weak comparison: ignore a `W/` prefix on either side, then octet-equal.
fn weak_eq(tag: &str, current: &str) -> bool {
    let t = tag.strip_prefix("W/").unwrap_or(tag);
    let c = current.strip_prefix("W/").unwrap_or(current);
    t == c
}

/// Outcome of a conditional upsert.
pub enum UpsertOutcome {
    Created(uuid::Uuid),
    Updated(uuid::Uuid),
    /// An `If-Match`/`If-None-Match` guard failed → 412.
    PreconditionFailed,
    /// The target collection doesn't exist for this account → 404.
    CollectionMissing,
}

/// Outcome of a conditional delete.
pub enum DeleteOutcome {
    Deleted(uuid::Uuid),
    NotFound,
    PreconditionFailed,
}

/// Application database state.
#[derive(Clone)]
pub struct Db {
    pub conn: Arc<Mutex<Connection>>,
    pub blob_dir: std::path::PathBuf,
}

impl Db {
    pub async fn connect(database_path: &str, blob_dir: &str) -> Result<Self> {
        let path = database_path.to_string();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            // Ensure parent directory exists
            if let Some(parent) = std::path::Path::new(&path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )?;
            Ok(conn)
        })
        .await??;
        let blob_dir = std::path::PathBuf::from(blob_dir);
        std::fs::create_dir_all(&blob_dir)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            blob_dir,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
            conn.execute_batch(SCHEMA)?;
            // Task 5.4b: smtp_queue.blob_hash column for CAS BlobHash
            // (hex-encoded). Pre-existing deploys have only blob_id;
            // ALTER TABLE ADD COLUMN with a duplicate-column tolerance
            // brings them current without a separate migration tool.
            // SQLite reports duplicate as `SqliteFailure(_, "duplicate
            // column name: blob_hash")` — match the message rather than
            // an error code (rusqlite surfaces a generic SqliteFailure).
            if let Err(e) = conn.execute("ALTER TABLE smtp_queue ADD COLUMN blob_hash TEXT", [])
                && !e.to_string().contains("duplicate column name")
            {
                return Err(e.into());
            }
            // P3 (email-2FA): per-account opt-in second-factor flag. Pre-existing
            // accounts tables lack the column, so the COALESCE(mfa_enabled, 0)
            // SELECTs in props/accounts.rs would fail without it; an additive
            // ALTER with duplicate-column tolerance (mirrors blob_hash above)
            // brings deployed DBs current. DEFAULT 0 = 2FA off (opt-in).
            if let Err(e) = conn.execute(
                "ALTER TABLE accounts ADD COLUMN mfa_enabled INTEGER DEFAULT 0",
                [],
            ) && !e.to_string().contains("duplicate column name")
            {
                return Err(e.into());
            }
            Ok(())
        })
        .await??;
        tracing::info!("Database migrations applied");
        Ok(())
    }

    /// Distinct lowercased FQDNs appearing in the `accounts.email`
    /// column. Used by the Umbrella Phase 2 startup reconciliation
    /// diagnostic (`maild.domains` rows vs. account-bearing domains).
    /// Diagnostic-only — the daemon never auto-creates or auto-deletes
    /// substrate rows from this set.
    ///
    /// The `instr` / `substr` split is SQLite-portable; lower() matches
    /// the substrate's lowercase-key invariant so set-difference works.
    /// Rows with no `@` are filtered with `email LIKE '%@%'`.
    pub async fn distinct_account_domains(&self) -> Result<BTreeSet<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<BTreeSet<String>> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT lower(substr(email, instr(email, '@') + 1)) \
                 FROM accounts \
                 WHERE email LIKE '%@%' \
                 ORDER BY 1",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = BTreeSet::new();
            for r in rows {
                out.insert(r?);
            }
            Ok(out)
        })
        .await?
    }
}

pub(crate) const SCHEMA: &str = r#"
-- accounts
CREATE TABLE IF NOT EXISTS accounts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    email           TEXT UNIQUE NOT NULL,
    password        TEXT NOT NULL,
    name            TEXT,
    quota           INTEGER DEFAULT 0,
    spam_enabled    INTEGER DEFAULT 1,
    spam_threshold  REAL DEFAULT 0.5,
    mfa_enabled     INTEGER DEFAULT 0,
    created_at      TEXT DEFAULT (datetime('now'))
);

-- mailboxes
CREATE TABLE IF NOT EXISTS mailboxes (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    parent_id       TEXT REFERENCES mailboxes(id),
    role            TEXT,
    sort_order      INTEGER DEFAULT 0,
    UNIQUE(account_id, parent_id, name)
);

-- threads
CREATE TABLE IF NOT EXISTS threads (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE
);

-- blobs
CREATE TABLE IF NOT EXISTS blobs (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    size            INTEGER NOT NULL,
    hash            TEXT NOT NULL,
    created_at      TEXT DEFAULT (datetime('now'))
);

-- emails
CREATE TABLE IF NOT EXISTS emails (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id       TEXT NOT NULL REFERENCES threads(id),
    mailbox_ids     TEXT NOT NULL DEFAULT '[]',
    blob_id         TEXT NOT NULL REFERENCES blobs(id),
    size            INTEGER NOT NULL,
    received_at     TEXT NOT NULL DEFAULT (datetime('now')),
    message_id      TEXT,
    in_reply_to     TEXT,
    subject         TEXT,
    from_addr       TEXT,
    to_addr         TEXT,
    cc_addr         TEXT,
    date            TEXT,
    preview         TEXT,
    has_attachment   INTEGER DEFAULT 0,
    keywords        TEXT DEFAULT '{}',
    spam_score      REAL,
    spam_verdict    TEXT,
    created_at      TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_emails_account_received ON emails (account_id, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_thread ON emails (thread_id);
CREATE INDEX IF NOT EXISTS idx_emails_message_id ON emails (message_id);

-- changelog
CREATE TABLE IF NOT EXISTS changelog (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    object_type     TEXT NOT NULL,
    object_id       TEXT NOT NULL,
    change_type     TEXT NOT NULL,
    changed_at      TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_changelog_account_type ON changelog (account_id, object_type, id);

-- smtp_queue
-- `blob_id` (legacy): per-account UUID into the legacy `blobs` table;
-- retained for in-flight pre-Task-5.4b entries that drain through
-- `db::blob::load`. Phase 6 deletes both `blobs` and this column once
-- legacy callers are gone.
-- `blob_hash` (Task 5.4b): hex-encoded `cosmix_mds::BlobHash` into the
-- shared CAS; new entries write only this. Reader (`smtp/delivery.rs`)
-- prefers `blob_hash` and falls back to `blob_id` for legacy rows.
CREATE TABLE IF NOT EXISTS smtp_queue (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_addr       TEXT NOT NULL,
    to_addrs        TEXT NOT NULL DEFAULT '[]',
    blob_id         TEXT REFERENCES blobs(id),
    blob_hash       TEXT,
    attempts        INTEGER DEFAULT 0,
    next_retry      TEXT DEFAULT (datetime('now')),
    last_error      TEXT,
    created_at      TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_queue_retry ON smtp_queue (next_retry);

-- calendars
CREATE TABLE IF NOT EXISTS calendars (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    color           TEXT,
    description     TEXT,
    is_visible      INTEGER DEFAULT 1,
    default_alerts  TEXT,
    timezone        TEXT DEFAULT 'UTC',
    sort_order      INTEGER DEFAULT 0,
    UNIQUE(account_id, name)
);

-- calendar_events
CREATE TABLE IF NOT EXISTS calendar_events (
    id              TEXT PRIMARY KEY,
    calendar_id     TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uid             TEXT NOT NULL,
    data            TEXT NOT NULL,
    title           TEXT,
    start_dt        TEXT,
    end_dt          TEXT,
    updated_at      TEXT DEFAULT (datetime('now')),
    UNIQUE(calendar_id, uid)
);

CREATE INDEX IF NOT EXISTS idx_events_account ON calendar_events (account_id);
CREATE INDEX IF NOT EXISTS idx_events_range ON calendar_events (calendar_id, start_dt, end_dt);

-- addressbooks
CREATE TABLE IF NOT EXISTS addressbooks (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    description     TEXT,
    sort_order      INTEGER DEFAULT 0,
    UNIQUE(account_id, name)
);

-- contacts
CREATE TABLE IF NOT EXISTS contacts (
    id              TEXT PRIMARY KEY,
    addressbook_id  TEXT NOT NULL REFERENCES addressbooks(id) ON DELETE CASCADE,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uid             TEXT NOT NULL,
    data            TEXT NOT NULL,
    full_name       TEXT,
    email           TEXT,
    company         TEXT,
    updated_at      TEXT DEFAULT (datetime('now')),
    UNIQUE(addressbook_id, uid)
);

CREATE INDEX IF NOT EXISTS idx_contacts_account ON contacts (account_id);
CREATE INDEX IF NOT EXISTS idx_contacts_name ON contacts (full_name);

-- DAV tombstones: when a calendar event / contact is deleted, its live
-- row (and thus its collection_id + uid) is gone, but RFC 6578
-- sync-collection must still report the removed member by href. A
-- tombstone, written in the same transaction as the delete, retains the
-- mapping object_id -> (collection_id, uid) so sync can rebuild the href.
CREATE TABLE IF NOT EXISTS dav_tombstones (
    object_id       TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    object_type     TEXT NOT NULL,
    collection_id   TEXT NOT NULL,
    uid             TEXT NOT NULL,
    deleted_at      TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_tombstones_acct ON dav_tombstones (account_id, object_type);

-- FTS5 for email search
CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(
    subject, preview, from_addr, content=emails, content_rowid=rowid
);

-- email_submissions — JMAP EmailSubmission tracking
CREATE TABLE IF NOT EXISTS email_submissions (
    id              TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email_id        TEXT NOT NULL,
    identity_id     TEXT,
    envelope_from   TEXT NOT NULL,
    envelope_to     TEXT NOT NULL DEFAULT '[]',
    send_at         TEXT NOT NULL DEFAULT (datetime('now')),
    undo_status     TEXT NOT NULL DEFAULT 'final',
    delivery_status TEXT NOT NULL DEFAULT '{}',
    queue_id        INTEGER,
    created_at      TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_submissions_account ON email_submissions (account_id);

-- vacation_responses — JMAP VacationResponse (RFC 8621 §8)
CREATE TABLE IF NOT EXISTS vacation_responses (
    id              TEXT PRIMARY KEY DEFAULT 'singleton',
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    is_enabled      INTEGER NOT NULL DEFAULT 0,
    from_date       TEXT,
    to_date         TEXT,
    subject         TEXT,
    text_body       TEXT,
    html_body       TEXT,
    UNIQUE(account_id)
);

-- vacation_replies — per-(account, sender) auto-reply dedup (RFC 3834
-- §2: at most one response per sender per period, here 7 days). Bounds
-- the backscatter/reflection amplification a forged MAIL FROM could
-- otherwise farm out of an enabled vacation responder (2026-07 audit).
CREATE TABLE IF NOT EXISTS vacation_replies (
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    sender          TEXT NOT NULL,
    sent_at         TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (account_id, sender)
);

-- account_tokens — opaque bearer tokens for JMAP auth (SSR PIM Phase 2).
-- The plaintext token (32 random bytes, base64url-no-pad) is returned ONCE
-- at issue and never stored; only its SHA-256 (hex) lives here, so a DB
-- read cannot reconstruct a usable credential. Lookup is by `token_hash`
-- (the index key derived from the presented token). Revoke sets
-- `revoked_at` rather than deleting the row, preserving an audit trail.
-- A row authorises `/jmap` while `revoked_at IS NULL AND expires_at >
-- datetime('now')`. Multiple rows per account = multi-device.
CREATE TABLE IF NOT EXISTS account_tokens (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash      TEXT UNIQUE NOT NULL,
    label           TEXT,
    issued_at       TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at      TEXT NOT NULL,
    revoked_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_account_tokens_account ON account_tokens (account_id);
"#;
