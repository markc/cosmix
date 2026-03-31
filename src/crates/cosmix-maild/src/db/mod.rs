//! SQLite database layer (rusqlite).

pub mod account;
pub mod blob;
pub mod calendar;
pub mod changelog;
pub mod contact;
pub mod email;
pub mod mailbox;
pub mod thread;
pub mod vacation;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::Connection;

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
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
            Ok(conn)
        }).await??;
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
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        }).await??;
        tracing::info!("Database migrations applied");
        Ok(())
    }
}

const SCHEMA: &str = r#"
-- accounts
CREATE TABLE IF NOT EXISTS accounts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    email           TEXT UNIQUE NOT NULL,
    password        TEXT NOT NULL,
    name            TEXT,
    quota           INTEGER DEFAULT 0,
    spam_enabled    INTEGER DEFAULT 1,
    spam_threshold  REAL DEFAULT 0.5,
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
CREATE TABLE IF NOT EXISTS smtp_queue (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_addr       TEXT NOT NULL,
    to_addrs        TEXT NOT NULL DEFAULT '[]',
    blob_id         TEXT REFERENCES blobs(id),
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
"#;
