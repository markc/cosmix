//! Storage backend trait + SQLite-per-account implementation.
//!
//! spamlite owns the on-disk schema and corpus operations. This wrapper keeps
//! one shared connection per account, cold-start seeding, WAL-safe legacy
//! promotion, async isolation via `spawn_blocking`, and the idempotent
//! `record_label` stamp transaction.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cosmix_maild_rules::AccountId;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::{Error, Result};
use crate::types::{AccountStats, Label};

#[path = "tokens.rs"]
pub(crate) mod tokens;
use tokens::apply_token_cap;

#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Open or create a per-account connection. The backend is
    /// responsible for cold-start seeding from `default-bayesian.db`
    /// when the account has no prior database.
    async fn open_account(&self, account: &AccountId) -> Result<Arc<dyn AccountConnection>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapOutcome {
    Swapped,
    Conflict(Vec<(String, Label)>),
}

#[async_trait]
pub trait AccountConnection: Send + Sync {
    /// Returns `(word, good_count, spam_count)` for tokens that exist
    /// in the corpus. Tokens not present are silently dropped. The
    /// result is **one entry per input occurrence, in input order** (the
    /// engine's `lookup_tokens` map is projected back onto the slice), so
    /// a token repeated in `tokens` appears repeatedly; consumers that
    /// want distinct words must collapse (the classifier does, into a
    /// `HashMap`).
    async fn token_counts(&self, tokens: &[String]) -> Result<Vec<(String, u64, u64)>>;

    /// Idempotent retrain. The backend records `stamp_id` to ignore
    /// duplicate labels for the same message. `tokens` is the uncapped current
    /// stream; storage applies `cap` and records that policy so a later relabel
    /// untrains the same set. Returns `Some(n)` when a label row was written
    /// or flipped (`n` = tokens trained, which is legitimately 0 for a message
    /// that tokenises to nothing — its class total still moved), and `None`
    /// when the stamp already carried this label and nothing changed. The
    /// distinction is load-bearing: callers count messages by it, and the
    /// engine bumps the class total even for an empty token set.
    async fn record_label(
        &self,
        stamp_id: &str,
        tokens: &[String],
        label: Label,
        cap: u32,
    ) -> Result<Option<u64>>;

    /// Remove a recorded label and reverse its training in one transaction.
    /// `tokens` is the current uncapped token stream; storage reapplies the
    /// cap recorded in `label_meta` so it untrains the same set as
    /// `record_label` trained.
    async fn forget_label(&self, stamp_id: &str, tokens: &[String]) -> Result<Option<Label>>;

    async fn stats(&self) -> Result<AccountStats>;

    /// Total ham + spam message counts (for the classifier's
    /// Robinson-Fisher denominator). Avoids two round-trips through
    /// the higher-level `stats()` plumbing.
    async fn totals(&self) -> Result<(u64, u64)>;

    /// Clear the trained corpus while preserving the live database and
    /// its schema version.
    async fn reset(&self) -> Result<()>;

    /// Write a transactionally consistent sibling copy of the live database.
    /// In-memory connections have no path to snapshot and return `None`.
    async fn snapshot(&self) -> Result<Option<PathBuf>>;

    /// Apply snapshot retention after a rebuild has committed its live swap.
    async fn prune_snapshots(&self, _newest: &Path) -> Result<()> {
        Err(Error::Storage(
            "prune_snapshots is unsupported by this account connection".to_string(),
        ))
    }

    /// Filesystem path for a persistent account database. In-memory and
    /// adapter connections return `None`.
    fn database_path(&self) -> Option<PathBuf> {
        None
    }

    /// Labels written at or after `ts`. Rebuild uses this short read to replay
    /// corrections that landed in the live corpus while its shadow corpus was
    /// being trained.
    async fn labels_since(&self, _ts: i64) -> Result<Vec<(String, Label)>> {
        Err(Error::Storage(
            "labels_since is unsupported by this account connection".to_string(),
        ))
    }

    /// Atomically replace the corpus tables from another SQLite database.
    /// Implementations without persistent SQLite storage return unsupported.
    async fn replace_from(
        &self,
        _source: &Path,
        _since_ts: i64,
        _expected: &[(String, Label)],
    ) -> Result<SwapOutcome> {
        Err(Error::Storage(
            "replace_from is unsupported by this account connection".to_string(),
        ))
    }
}

// ---- SQLite backend ----

/// SQLite-per-account backend rooted at `base_dir`. New per-account
/// databases are seeded from `default_seed` (typically
/// `default-bayesian.db`) when present; otherwise they start empty.
pub struct SqliteBackend {
    base_dir: PathBuf,
    default_seed: Option<PathBuf>,
    cold_floor: u32,
    cache: AsyncMutex<HashMap<String, Arc<SqliteAccountConnection>>>,
}

impl SqliteBackend {
    pub fn new(
        base_dir: impl Into<PathBuf>,
        default_seed: Option<PathBuf>,
        cold_floor: u32,
    ) -> Self {
        Self {
            base_dir: base_dir.into(),
            default_seed,
            cold_floor,
            cache: AsyncMutex::new(HashMap::new()),
        }
    }

    fn account_path(&self, account: &AccountId) -> PathBuf {
        self.base_dir.join(account.as_str()).join("bayes.db")
    }
}

#[async_trait]
impl StorageBackend for SqliteBackend {
    async fn open_account(&self, account: &AccountId) -> Result<Arc<dyn AccountConnection>> {
        let key = account.as_str().to_string();
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&key) {
                return Ok(c.clone() as Arc<dyn AccountConnection>);
            }
        }

        let path = self.account_path(account);
        let seed = self.default_seed.clone();
        let cold_floor = self.cold_floor;

        let conn = tokio::task::spawn_blocking(move || -> Result<SqliteAccountConnection> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Storage(format!("mkdir {}: {e}", parent.display())))?;
            }

            // Cold-start: prefer a legacy spamlite-format `db.sqlite`
            // adjacent to the target if one exists (the schema is
            // binary-compatible per the module doc), so per-account
            // corpora trained via the legacy SpamFilter path are
            // promoted in place rather than stranded. Falling back to
            // the global baseline seed only applies to truly fresh
            // accounts.
            //
            // Promotion goes through SQLite (open + VACUUM INTO), not
            // raw fs::copy, because spamlite enables WAL mode — recent
            // training can live in `db.sqlite-wal` and would be lost
            // by a byte-level copy. VACUUM INTO writes a transactionally
            // consistent snapshot that includes any committed WAL pages.
            if !path.exists() {
                let legacy = path.with_file_name("db.sqlite");
                if legacy.exists() {
                    let legacy_for_msg = legacy.clone();
                    let path_for_msg = path.clone();
                    let src = Connection::open(&legacy).map_err(|e| {
                        Error::Storage(format!("legacy open {}: {e}", legacy.display()))
                    })?;
                    src.execute("VACUUM INTO ?1", params![path.to_string_lossy().as_ref()])
                        .map_err(|e| {
                            Error::Storage(format!("legacy promote {}: {e}", legacy.display()))
                        })?;
                    drop(src);
                    tracing::info!(
                        target: "bayesian",
                        legacy = %legacy_for_msg.display(),
                        promoted_to = %path_for_msg.display(),
                        "promoted legacy db.sqlite to bayes.db (WAL-safe)"
                    );
                } else if let Some(s) = seed.as_ref().filter(|p| p.exists()) {
                    std::fs::copy(s, &path)
                        .map_err(|e| Error::Storage(format!("seed {}: {e}", s.display())))?;
                }
            }

            let conn = Connection::open(&path)?;
            spamlite::storage::schema::init(&conn)?;
            init_maild_schema(&conn)?;
            Ok(SqliteAccountConnection {
                conn: Arc::new(Mutex::new(conn)),
                cold_floor,
                path: Some(path),
            })
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))??;

        let arc = Arc::new(conn);
        let mut cache = self.cache.lock().await;
        cache.insert(key, arc.clone());
        Ok(arc as Arc<dyn AccountConnection>)
    }
}

pub struct SqliteAccountConnection {
    conn: Arc<Mutex<Connection>>,
    cold_floor: u32,
    path: Option<PathBuf>,
}

impl SqliteAccountConnection {
    /// Open or create a single-account DB at `path`. Bypasses the
    /// backend cache; used by tooling and tests.
    pub fn open_path(path: &Path, cold_floor: u32) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Storage(format!("mkdir {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        spamlite::storage::schema::init(&conn)?;
        init_maild_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cold_floor,
            path: (path != Path::new(":memory:")).then(|| path.to_path_buf()),
        })
    }
}

#[async_trait]
impl AccountConnection for SqliteAccountConnection {
    async fn token_counts(&self, tokens: &[String]) -> Result<Vec<(String, u64, u64)>> {
        let conn = Arc::clone(&self.conn);
        let tokens = tokens.to_vec();
        tokio::task::spawn_blocking(move || {
            // SQLite rolls back abandoned transactions, so a poisoned lock is reusable.
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let known = spamlite::storage::ops::lookup_tokens(&c, &tokens)?;
            Ok(tokens
                .into_iter()
                .filter_map(|word| known.get(&word).map(|&(good, spam)| (word, good, spam)))
                .collect())
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn record_label(
        &self,
        stamp_id: &str,
        tokens: &[String],
        label: Label,
        cap: u32,
    ) -> Result<Option<u64>> {
        let conn = Arc::clone(&self.conn);
        let stamp_id = stamp_id.to_string();
        let tokens = tokens.to_vec();
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let tx = spamlite::storage::ops::begin_immediate(&c)?;
            let prior: Option<i64> = tx
                .query_row(
                    "SELECT label FROM labels WHERE stamp_id = ?1",
                    params![stamp_id],
                    |row| row.get(0),
                )
                .optional()?;
            let is_spam = label == Label::Spam;
            let now = unix_secs();
            let mut current_set = tokens.clone();
            apply_token_cap(&mut current_set, cap);

            match prior {
                None => {
                    spamlite::storage::ops::train(&tx, &current_set, is_spam)?;
                    tx.execute(
                        "INSERT INTO labels (stamp_id, label, ts) VALUES (?1, ?2, ?3)",
                        params![stamp_id, label_int(label), now],
                    )?;
                    upsert_label_meta(&tx, &stamp_id, cap, "priority")?;
                }
                Some(p) if p == label_int(label) => return Ok(None),
                Some(p) => {
                    let from_spam = p == label_int(Label::Spam);
                    let recorded: Option<(u32, String)> = tx
                        .query_row(
                            "SELECT token_cap, cap_mode FROM label_meta WHERE stamp_id = ?1",
                            params![stamp_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    match recorded {
                        Some((recorded_cap, mode)) if mode == "priority" => {
                            let mut prior_set = tokens.clone();
                            apply_token_cap(&mut prior_set, recorded_cap);
                            spamlite::storage::ops::untrain(&tx, &prior_set, from_spam)?;
                        }
                        Some((_, mode)) => {
                            return Err(Error::Storage(format!(
                                "unknown label_meta cap_mode {mode:?}"
                            )));
                        }
                        None => {
                            // A legacy row's trained token set is unknowable (old
                            // cap, old tokenizer); its stale per-token counts stay
                            // (bounded, one message's worth) rather than risk
                            // decrementing evidence other messages own; the
                            // message total is still moved.
                            spamlite::storage::ops::untrain(&tx, &[], from_spam)?;
                        }
                    }
                    spamlite::storage::ops::train(&tx, &current_set, is_spam)?;
                    tx.execute(
                        "UPDATE labels SET label = ?2, ts = ?3 WHERE stamp_id = ?1",
                        params![stamp_id, label_int(label), now],
                    )?;
                    upsert_label_meta(&tx, &stamp_id, cap, "priority")?;
                }
            }

            tx.commit()?;
            Ok(Some(current_set.len() as u64))
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn forget_label(&self, stamp_id: &str, tokens: &[String]) -> Result<Option<Label>> {
        let conn = Arc::clone(&self.conn);
        let stamp_id = stamp_id.to_string();
        let tokens = tokens.to_vec();
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let tx = spamlite::storage::ops::begin_immediate(&c)?;
            let Some(raw_label) = tx
                .query_row(
                    "SELECT label FROM labels WHERE stamp_id = ?1",
                    params![stamp_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let recorded: Option<(u32, String)> = tx
                .query_row(
                    "SELECT token_cap, cap_mode FROM label_meta WHERE stamp_id = ?1",
                    params![stamp_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let prior = label_from_int(raw_label);
            match recorded {
                Some((recorded_cap, mode)) if mode == "priority" => {
                    let mut recorded_set = tokens;
                    apply_token_cap(&mut recorded_set, recorded_cap);
                    spamlite::storage::ops::untrain(&tx, &recorded_set, prior == Label::Spam)?;
                }
                Some((_, mode)) => {
                    return Err(Error::Storage(format!(
                        "unknown label_meta cap_mode {mode:?}"
                    )));
                }
                None => {
                    spamlite::storage::ops::untrain(&tx, &[], prior == Label::Spam)?;
                }
            }
            tx.execute("DELETE FROM labels WHERE stamp_id = ?1", params![stamp_id])?;
            tx.execute(
                "DELETE FROM label_meta WHERE stamp_id = ?1",
                params![stamp_id],
            )?;
            tx.commit()?;
            Ok(Some(prior))
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn stats(&self) -> Result<AccountStats> {
        let conn = Arc::clone(&self.conn);
        let cold_floor = self.cold_floor;
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let counts = spamlite::storage::ops::counts(&c)?;
            let version: String =
                c.query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
                    row.get(0)
                })?;
            Ok(AccountStats {
                spam_messages: counts.total_spam as u32,
                ham_messages: counts.total_good as u32,
                spam_tokens: counts.unique_tokens,
                ham_tokens: counts.unique_tokens,
                cold_start: counts.total_good + counts.total_spam < cold_floor as u64,
                seeded_from: None,
                model_version: version.parse().unwrap_or(0),
            })
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn totals(&self) -> Result<(u64, u64)> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            Ok(spamlite::storage::ops::totals(&c)?)
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn reset(&self) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let tx = spamlite::storage::ops::begin_immediate(&c)?;
            tx.execute("DELETE FROM tokens", [])?;
            tx.execute("DELETE FROM labels", [])?;
            tx.execute("DELETE FROM label_meta", [])?;
            tx.execute(
                "UPDATE meta SET value = '0' WHERE key IN ('total_good', 'total_spam')",
                [],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn snapshot(&self) -> Result<Option<PathBuf>> {
        let Some(path) = self.path.clone() else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let snapshot = parent.join(format!("bayes.pre-rebuild-{}.db", unix_millis()));
            if snapshot.exists() {
                return Err(Error::Storage(format!(
                    "snapshot already exists: {}",
                    snapshot.display()
                )));
            }
            // Use a separate WAL reader so a potentially long VACUUM INTO
            // never takes the cached live connection mutex. Same busy_timeout
            // as every other connection so a WAL-recovery race retries
            // instead of failing the snapshot instantly.
            let c = Connection::open(&path)?;
            c.busy_timeout(std::time::Duration::from_millis(5000))?;
            c.execute(
                "VACUUM INTO ?1",
                params![snapshot.to_string_lossy().as_ref()],
            )?;
            Ok(Some(snapshot))
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn prune_snapshots(&self, newest: &Path) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let newest = newest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            prune_snapshot_files(parent, &newest)
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    fn database_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    async fn labels_since(&self, ts: i64) -> Result<Vec<(String, Label)>> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = c.prepare(
                "SELECT stamp_id, label FROM labels WHERE ts >= ?1 ORDER BY ts, stamp_id",
            )?;
            let rows = stmt.query_map(params![ts], |row| {
                let raw: i64 = row.get(1)?;
                let label = if raw == label_int(Label::Spam) {
                    Label::Spam
                } else {
                    Label::Ham
                };
                Ok((row.get(0)?, label))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }

    async fn replace_from(
        &self,
        source: &Path,
        since_ts: i64,
        expected: &[(String, Label)],
    ) -> Result<SwapOutcome> {
        let conn = Arc::clone(&self.conn);
        let source = source.to_path_buf();
        let expected = expected.to_vec();
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());

            // ATTACH is deliberately outside the write transaction: SQLite
            // does not permit changing attached databases while a transaction
            // is active. Every live-table mutation below is still covered by
            // one BEGIN IMMEDIATE transaction, and DETACH follows commit.
            //
            // Defensive DETACH first: if an earlier swap failed AND its
            // DETACH failed, `src` is still attached on this cached
            // connection and every later ATTACH would fail forever. A
            // "no such database" error here is the normal case and ignored.
            let _ = c.execute_batch("DETACH DATABASE src");
            c.execute(
                "ATTACH DATABASE ?1 AS src",
                params![source.to_string_lossy().as_ref()],
            )?;
            let replace_result = (|| -> Result<SwapOutcome> {
                let tx = spamlite::storage::ops::begin_immediate(&c)?;
                let current = labels_since_tx(&tx, since_ts)?;
                if label_set(&current) != label_set(&expected) {
                    tx.rollback()?;
                    return Ok(SwapOutcome::Conflict(current));
                }
                tx.execute("DELETE FROM tokens", [])?;
                tx.execute("DELETE FROM labels", [])?;
                tx.execute("DELETE FROM label_meta", [])?;
                tx.execute("INSERT INTO tokens SELECT * FROM src.tokens", [])?;
                tx.execute("INSERT INTO labels SELECT * FROM src.labels", [])?;
                tx.execute("INSERT INTO label_meta SELECT * FROM src.label_meta", [])?;
                tx.execute(
                    "UPDATE meta SET value = (SELECT value FROM src.meta WHERE key = 'total_good') \
                     WHERE key = 'total_good'",
                    [],
                )?;
                tx.execute(
                    "UPDATE meta SET value = (SELECT value FROM src.meta WHERE key = 'total_spam') \
                     WHERE key = 'total_spam'",
                    [],
                )?;
                tx.commit()?;
                Ok(SwapOutcome::Swapped)
            })();
            let detach_result = c.execute_batch("DETACH DATABASE src");
            finish_replace_after_detach(replace_result, detach_result)
        })
        .await
        .map_err(|e| Error::Storage(format!("spawn_blocking: {e}")))?
    }
}

fn finish_replace_after_detach(
    replace_result: Result<SwapOutcome>,
    detach_result: rusqlite::Result<()>,
) -> Result<SwapOutcome> {
    match replace_result {
        Ok(SwapOutcome::Swapped) => {
            if let Err(error) = detach_result {
                tracing::warn!(
                    target: "bayesian",
                    %error,
                    "failed to detach rebuild source after committed corpus swap"
                );
            }
            Ok(SwapOutcome::Swapped)
        }
        Ok(outcome @ SwapOutcome::Conflict(_)) => {
            detach_result?;
            Ok(outcome)
        }
        Err(error) => Err(error),
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn prune_snapshot_files(parent: &Path, newest: &Path) -> Result<()> {
    let mut older = std::fs::read_dir(parent)
        .map_err(|e| Error::Storage(format!("read snapshot directory {}: {e}", parent.display())))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bayes.pre-rebuild-") && name.ends_with(".db"))
        })
        .filter(|path| path != newest)
        .collect::<Vec<_>>();
    older.sort_by_key(|path| snapshot_sequence(path));
    older.reverse();
    for old in older.into_iter().skip(1) {
        std::fs::remove_file(&old)
            .map_err(|e| Error::Storage(format!("prune snapshot {}: {e}", old.display())))?;
    }
    Ok(())
}

fn snapshot_sequence(path: &Path) -> u128 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("bayes.pre-rebuild-"))
        .and_then(|name| name.strip_suffix(".db"))
        .and_then(|sequence| sequence.parse().ok())
        .unwrap_or(0)
}

fn label_int(l: Label) -> i64 {
    match l {
        Label::Ham => 0,
        Label::Spam => 1,
    }
}

fn label_from_int(raw: i64) -> Label {
    if raw == label_int(Label::Spam) {
        Label::Spam
    } else {
        Label::Ham
    }
}

fn labels_since_tx(tx: &rusqlite::Transaction<'_>, ts: i64) -> Result<Vec<(String, Label)>> {
    let mut stmt =
        tx.prepare("SELECT stamp_id, label FROM labels WHERE ts >= ?1 ORDER BY ts, stamp_id")?;
    let rows = stmt.query_map(params![ts], |row| {
        Ok((row.get(0)?, label_from_int(row.get::<_, i64>(1)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn label_set(rows: &[(String, Label)]) -> BTreeSet<(String, i64)> {
    rows.iter()
        .map(|(stamp, label)| (stamp.clone(), label_int(*label)))
        .collect()
}

fn init_maild_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS label_meta (
            stamp_id  TEXT PRIMARY KEY,
            token_cap INTEGER NOT NULL,
            cap_mode  TEXT NOT NULL
        ) WITHOUT ROWID;",
    )
}

fn upsert_label_meta(
    tx: &rusqlite::Transaction<'_>,
    stamp_id: &str,
    cap: u32,
    mode: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO label_meta (stamp_id, token_cap, cap_mode) VALUES (?1, ?2, ?3)
         ON CONFLICT(stamp_id) DO UPDATE
         SET token_cap = excluded.token_cap, cap_mode = excluded.cap_mode",
        params![stamp_id, cap, mode],
    )?;
    Ok(())
}

// ---- In-memory backend (kept for unit tests + downstream stubs) ----

pub struct InMemoryBackend;

#[async_trait]
impl StorageBackend for InMemoryBackend {
    async fn open_account(&self, _account: &AccountId) -> Result<Arc<dyn AccountConnection>> {
        Ok(Arc::new(InMemoryConnection))
    }
}

pub struct InMemoryConnection;

#[async_trait]
impl AccountConnection for InMemoryConnection {
    async fn token_counts(&self, _tokens: &[String]) -> Result<Vec<(String, u64, u64)>> {
        Ok(Vec::new())
    }

    async fn record_label(
        &self,
        _stamp_id: &str,
        _tokens: &[String],
        _label: Label,
        _cap: u32,
    ) -> Result<Option<u64>> {
        Ok(None)
    }

    async fn forget_label(&self, _stamp_id: &str, _tokens: &[String]) -> Result<Option<Label>> {
        Ok(None)
    }

    async fn stats(&self) -> Result<AccountStats> {
        Ok(AccountStats {
            cold_start: true,
            ..AccountStats::default()
        })
    }

    async fn totals(&self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }

    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    async fn prune_snapshots(&self, _newest: &Path) -> Result<()> {
        Ok(())
    }

    async fn labels_since(&self, _ts: i64) -> Result<Vec<(String, Label)>> {
        Ok(Vec::new())
    }

    async fn replace_from(
        &self,
        _source: &Path,
        _since_ts: i64,
        _expected: &[(String, Label)],
    ) -> Result<SwapOutcome> {
        Err(Error::Storage(
            "replace_from is unsupported for in-memory storage".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn open_path_installs_five_second_busy_timeout() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-busy-timeout-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let busy_timeout: i64 = conn
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_label_is_idempotent_on_same_label() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-idem-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let words = toks(&["alpha", "beta", "gamma"]);

        conn.record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        let first = conn.totals().await.unwrap();
        // Same label twice — no change.
        let n = conn
            .record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        assert_eq!(n, None);
        let second = conn.totals().await.unwrap();
        assert_eq!(first, second);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn zero_token_message_still_counts_as_one_labelled_message() {
        // The engine bumps the class total for an empty token set, so the
        // return value must say "label written" (Some(0)), not "duplicate"
        // (None) — a rebuild validates shadow totals against the messages it
        // counted, and one degenerate message must not fail the whole job.
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let first = conn
            .record_label("empty", &[], Label::Ham, 0)
            .await
            .unwrap();
        assert_eq!(first, Some(0));
        assert_eq!(conn.totals().await.unwrap(), (1, 0));
        let again = conn
            .record_label("empty", &[], Label::Ham, 0)
            .await
            .unwrap();
        assert_eq!(again, None);
        assert_eq!(conn.totals().await.unwrap(), (1, 0));
        let flipped = conn
            .record_label("empty", &[], Label::Spam, 0)
            .await
            .unwrap();
        assert_eq!(flipped, Some(0));
        assert_eq!(conn.totals().await.unwrap(), (0, 1));
    }

    #[tokio::test]
    async fn reset_clears_corpus_and_allows_priority_retraining() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let words = toks(&["h:from:sender@example.com", "b:meeting"]);
        conn.record_label("ham-before", &words, Label::Ham, 200)
            .await
            .unwrap();
        conn.record_label("spam-before", &words, Label::Spam, 200)
            .await
            .unwrap();
        assert_eq!(conn.totals().await.unwrap(), (1, 1));

        conn.reset().await.unwrap();

        assert_eq!(conn.totals().await.unwrap(), (0, 0));
        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            for table in ["tokens", "labels", "label_meta"] {
                let count: i64 = raw
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 0, "{table} must be empty after reset");
            }
            let version: String = raw
                .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(version, "1");
        }

        conn.record_label("ham-after", &words, Label::Ham, 200)
            .await
            .unwrap();
        assert_eq!(conn.totals().await.unwrap(), (1, 0));
        let meta: (u32, String) = conn
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row(
                "SELECT token_cap, cap_mode FROM label_meta WHERE stamp_id = 'ham-after'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(meta, (200, "priority".to_string()));
    }

    #[tokio::test]
    async fn snapshot_preserves_pre_reset_corpus() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-snapshot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = dir.join("bayes.db");
        let conn = SqliteAccountConnection::open_path(&path, 0).unwrap();
        conn.record_label("ham", &toks(&["b:hello"]), Label::Ham, 200)
            .await
            .unwrap();
        conn.record_label("spam", &toks(&["b:offer"]), Label::Spam, 200)
            .await
            .unwrap();

        let snapshot = conn.snapshot().await.unwrap().unwrap();
        assert!(snapshot.exists());
        conn.reset().await.unwrap();
        assert_eq!(conn.totals().await.unwrap(), (0, 0));

        let snapshot_conn = Connection::open(&snapshot).unwrap();
        assert_eq!(
            spamlite::storage::ops::totals(&snapshot_conn).unwrap(),
            (1, 1)
        );
        let labels: i64 = snapshot_conn
            .query_row("SELECT COUNT(*) FROM labels", [], |row| row.get(0))
            .unwrap();
        assert_eq!(labels, 2);
        let tokens: i64 = snapshot_conn
            .query_row("SELECT COUNT(*) FROM tokens", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tokens, 2);
        let label_meta: i64 = snapshot_conn
            .query_row("SELECT COUNT(*) FROM label_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(label_meta, 2);
        drop(snapshot_conn);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_on_memory_connection_returns_none() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        assert_eq!(conn.snapshot().await.unwrap(), None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn snapshot_does_not_take_cached_live_connection_mutex() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-snapshot-separate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        conn.record_label("ham", &toks(&["b:hello"]), Label::Ham, 200)
            .await
            .unwrap();

        let live_guard = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), conn.snapshot())
            .await
            .expect("snapshot waited for cached live mutex")
            .unwrap()
            .unwrap();
        assert!(snapshot.exists());
        drop(live_guard);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prune_snapshots_keeps_newest_two_after_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-snapshot-collision-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        for timestamp in [100_u64, 200, 300] {
            std::fs::write(
                dir.join(format!("bayes.pre-rebuild-{timestamp}.db")),
                b"sentinel",
            )
            .unwrap();
        }

        let newest = conn.snapshot().await.unwrap().unwrap();
        conn.prune_snapshots(&newest).await.unwrap();
        let mut retained = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bayes.pre-rebuild-"))
            })
            .collect::<Vec<_>>();
        retained.sort();
        assert_eq!(retained.len(), 2);
        assert!(retained.contains(&newest));
        assert!(retained.contains(&dir.join("bayes.pre-rebuild-300.db")));

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn labels_since_filters_and_decodes_labels() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        conn.conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .execute_batch(
                "INSERT INTO labels (stamp_id, label, ts) VALUES ('old', 0, 9);
                 INSERT INTO labels (stamp_id, label, ts) VALUES ('ham', 0, 10);
                 INSERT INTO labels (stamp_id, label, ts) VALUES ('spam', 1, 11);",
            )
            .unwrap();

        assert_eq!(
            conn.labels_since(10).await.unwrap(),
            vec![
                ("ham".to_string(), Label::Ham),
                ("spam".to_string(), Label::Spam),
            ]
        );
    }

    #[tokio::test]
    async fn replace_from_swaps_all_corpus_tables_in_one_live_connection() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-replace-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let live = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let shadow = SqliteAccountConnection::open_path(&dir.join("bayes.rebuild.db"), 0).unwrap();
        live.record_label("old", &toks(&["b:old"]), Label::Ham, 200)
            .await
            .unwrap();
        shadow
            .record_label("new-spam", &toks(&["b:offer"]), Label::Spam, 200)
            .await
            .unwrap();
        shadow
            .record_label("new-ham", &toks(&["b:meeting"]), Label::Ham, 200)
            .await
            .unwrap();

        let expected = live.labels_since(0).await.unwrap();
        assert_eq!(
            live.replace_from(&dir.join("bayes.rebuild.db"), 0, &expected)
                .await
                .unwrap(),
            SwapOutcome::Swapped
        );

        assert_eq!(live.totals().await.unwrap(), (1, 1));
        let raw = live.conn.lock().unwrap_or_else(|e| e.into_inner());
        let old: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM labels WHERE stamp_id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
        for table in ["tokens", "labels", "label_meta"] {
            let count: i64 = raw
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 2, "{table}");
        }
        drop(raw);
        drop(shadow);
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replace_from_reports_correction_set_conflict_without_copying() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-replace-conflict-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let live_path = dir.join("bayes.db");
        let shadow_path = dir.join("bayes.rebuild.db");
        let live = SqliteAccountConnection::open_path(&live_path, 0).unwrap();
        let shadow = SqliteAccountConnection::open_path(&shadow_path, 0).unwrap();
        live.record_label("old", &toks(&["b:old"]), Label::Ham, 200)
            .await
            .unwrap();
        shadow
            .record_label("replacement", &toks(&["b:new"]), Label::Spam, 200)
            .await
            .unwrap();
        let expected = live.labels_since(0).await.unwrap();
        Connection::open(&live_path)
            .unwrap()
            .execute(
                "INSERT INTO labels (stamp_id, label, ts) VALUES ('late', 1, ?1)",
                params![unix_secs()],
            )
            .unwrap();

        let outcome = live.replace_from(&shadow_path, 0, &expected).await.unwrap();
        let SwapOutcome::Conflict(current) = outcome else {
            panic!("late correction must conflict");
        };
        assert!(current.contains(&("late".to_string(), Label::Spam)));
        let raw = live.conn.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            raw.query_row(
                "SELECT COUNT(*) FROM labels WHERE stamp_id = 'replacement'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            raw.query_row(
                "SELECT COUNT(*) FROM labels WHERE stamp_id = 'old'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        drop(raw);
        drop(shadow);
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detach_failure_does_not_override_a_committed_swap() {
        let outcome = finish_replace_after_detach(
            Ok(SwapOutcome::Swapped),
            Err(rusqlite::Error::InvalidQuery),
        )
        .unwrap();
        assert_eq!(outcome, SwapOutcome::Swapped);

        let conflict = SwapOutcome::Conflict(vec![("late".to_string(), Label::Ham)]);
        assert!(
            finish_replace_after_detach(Ok(conflict), Err(rusqlite::Error::InvalidQuery)).is_err()
        );
    }

    #[tokio::test]
    async fn forget_label_reverses_recorded_tokens_and_deletes_metadata() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let tokens = toks(&["b:one", "b:two", "b:three"]);
        conn.record_label("forgotten", &tokens, Label::Spam, 2)
            .await
            .unwrap();
        assert_eq!(conn.totals().await.unwrap(), (0, 1));

        assert_eq!(
            conn.forget_label("forgotten", &tokens).await.unwrap(),
            Some(Label::Spam)
        );

        assert_eq!(conn.totals().await.unwrap(), (0, 0));
        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            for table in ["labels", "label_meta"] {
                assert_eq!(
                    raw.query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE stamp_id = 'forgotten'"),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                    0,
                    "{table}"
                );
            }
        }
        for (_, good, spam) in conn.token_counts(&tokens).await.unwrap() {
            assert_eq!((good, spam), (0, 0));
        }
    }

    #[tokio::test]
    async fn legacy_relabel_moves_total_without_touching_old_token_counts() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let tokens: Vec<String> = (0..300).map(|i| format!("b:token{i:03}")).collect();

        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            let tx = spamlite::storage::ops::begin_immediate(&raw).unwrap();
            spamlite::storage::ops::train(&tx, &tokens, true).unwrap();
            tx.execute(
                "INSERT INTO labels (stamp_id, label, ts) VALUES ('legacy', 1, 0)",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(conn.totals().await.unwrap(), (0, 1));

        conn.record_label("legacy", &tokens, Label::Ham, 50_000)
            .await
            .unwrap();

        let counts = conn.token_counts(&tokens).await.unwrap();
        assert_eq!(counts.len(), 300);
        for (index, (_, good, spam)) in counts.iter().enumerate() {
            assert_eq!(*good, 1, "token {index} must train on the new side");
            assert_eq!(*spam, 1, "legacy untrain touched token {index}");
        }
        assert_eq!(conn.totals().await.unwrap(), (1, 0));
        let meta: (u32, String) = conn
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row(
                "SELECT token_cap, cap_mode FROM label_meta WHERE stamp_id = 'legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(meta, (50_000, "priority".to_string()));
    }

    #[tokio::test]
    async fn new_style_relabel_exactly_reverses_current_set() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let tokens: Vec<String> = (0..300).map(|i| format!("b:token{i:03}")).collect();

        conn.record_label("current", &tokens, Label::Spam, 50_000)
            .await
            .unwrap();
        conn.record_label("current", &tokens, Label::Ham, 50_000)
            .await
            .unwrap();

        let counts = conn.token_counts(&tokens).await.unwrap();
        assert_eq!(counts.len(), tokens.len());
        for (_, good, spam) in counts {
            assert_eq!((good, spam), (1, 0));
        }
        assert_eq!(conn.totals().await.unwrap(), (1, 0));
    }

    #[tokio::test]
    async fn same_label_noop_leaves_label_meta_untouched() {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let tokens = toks(&["b:alpha", "h:from:sender@example.test"]);
        conn.record_label("same", &tokens, Label::Spam, 50_000)
            .await
            .unwrap();
        conn.conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .execute(
                "UPDATE label_meta SET token_cap = 17, cap_mode = 'sentinel'
                 WHERE stamp_id = 'same'",
                [],
            )
            .unwrap();

        let touched = conn
            .record_label("same", &tokens, Label::Spam, 999)
            .await
            .unwrap();
        assert_eq!(touched, None);
        let meta: (u32, String) = conn
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row(
                "SELECT token_cap, cap_mode FROM label_meta WHERE stamp_id = 'same'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(meta, (17, "sentinel".to_string()));
    }

    #[tokio::test]
    async fn record_label_reverses_on_opposite_label() {
        // User flow: message lands in Junk → trained as Spam → user
        // moves it back to Inbox → must retrain as Ham, undoing the
        // earlier spam training. Then move back into Junk → must
        // retrain as Spam again. After the full cycle the net token
        // counts should reflect exactly one Spam training.
        let dir = std::env::temp_dir().join(format!(
            "bayes-reverse-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let words = toks(&["alpha", "beta", "gamma"]);

        // First: train Spam.
        conn.record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        let counts = conn.token_counts(&words).await.unwrap();
        for (_, good, spam) in &counts {
            assert_eq!(*good, 0);
            assert_eq!(*spam, 1);
        }
        assert_eq!(conn.totals().await.unwrap(), (0, 1));

        // Reverse: train Ham — must undo the spam, no double-counting.
        conn.record_label("stamp-A", &words, Label::Ham, 0)
            .await
            .unwrap();
        let counts = conn.token_counts(&words).await.unwrap();
        for (_, good, spam) in &counts {
            assert_eq!(*good, 1, "ham count should be 1 after reversal");
            assert_eq!(*spam, 0, "spam count should be 0 after reversal");
        }
        assert_eq!(conn.totals().await.unwrap(), (1, 0));

        // Reverse again: back to Spam.
        conn.record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        let counts = conn.token_counts(&words).await.unwrap();
        for (_, good, spam) in &counts {
            assert_eq!(*good, 0);
            assert_eq!(*spam, 1);
        }
        assert_eq!(conn.totals().await.unwrap(), (0, 1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_label_reversal_floors_at_zero() {
        // A correction must never push a count below zero, even if the
        // prior training history is inconsistent (e.g. seed corpus
        // already had ham counts that were further decremented).
        let dir = std::env::temp_dir().join(format!(
            "bayes-floor-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let words = toks(&["alpha"]);

        // Manually set a token row with spam=0, then ask to reverse a
        // hypothetical spam training (label flips from Spam to Ham).
        // Inject a fake prior "Spam" label without per-token counts.
        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            raw.execute(
                "INSERT INTO labels (stamp_id, label, ts) VALUES ('stamp-A', 1, 0)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO tokens (word, good, spam, last_seen) VALUES ('alpha', 0, 0, 0)",
                [],
            )
            .unwrap();
        }

        // Now retrain as Ham — reversal must not produce negative.
        conn.record_label("stamp-A", &words, Label::Ham, 0)
            .await
            .unwrap();
        let counts = conn.token_counts(&words).await.unwrap();
        for (_, good, spam) in &counts {
            assert_eq!(*good, 1);
            assert_eq!(*spam, 0, "spam must floor at 0, never go negative");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn opposite_label_busy_failure_leaves_label_and_counts_unchanged() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-atomic-busy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bayes.db");
        let conn = SqliteAccountConnection::open_path(&path, 0).unwrap();
        let words = toks(&["alpha", "beta"]);
        conn.record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            raw.execute_batch("PRAGMA busy_timeout = 200;").unwrap();
        }

        let blocker = Connection::open(&path).unwrap();
        spamlite::storage::schema::init(&blocker).unwrap();
        let blocker_tx = spamlite::storage::ops::begin_immediate(&blocker).unwrap();

        let error = conn
            .record_label("stamp-A", &words, Label::Ham, 0)
            .await
            .expect_err("reserved write lock must make relabel fail with SQLITE_BUSY");
        assert!(
            error.to_string().contains("locked"),
            "unexpected error: {error}"
        );

        let label: i64 = blocker_tx
            .query_row(
                "SELECT label FROM labels WHERE stamp_id = 'stamp-A'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(label, label_int(Label::Spam));
        let seen = spamlite::storage::ops::lookup_tokens(&blocker_tx, &words).unwrap();
        assert_eq!(seen["alpha"], (0, 1));
        assert_eq!(seen["beta"], (0, 1));
        assert_eq!(spamlite::storage::ops::totals(&blocker_tx).unwrap(), (0, 1));

        blocker_tx.rollback().unwrap();
        drop(blocker);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_label_update_rolls_back_relabel_and_totals() {
        let dir = std::env::temp_dir().join(format!(
            "bayes-atomic-trigger-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = SqliteAccountConnection::open_path(&dir.join("bayes.db"), 0).unwrap();
        let words = toks(&["alpha", "beta"]);
        conn.record_label("stamp-A", &words, Label::Spam, 0)
            .await
            .unwrap();
        let counts_before = conn.token_counts(&words).await.unwrap();
        let totals_before = conn.totals().await.unwrap();

        {
            let raw = conn.conn.lock().unwrap_or_else(|e| e.into_inner());
            raw.execute_batch(
                "CREATE TRIGGER fail_label BEFORE UPDATE OF label ON labels \
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();
        }

        let error = conn
            .record_label("stamp-A", &words, Label::Ham, 0)
            .await
            .expect_err("label trigger must abort the relabel transaction");
        assert!(
            error.to_string().contains("forced"),
            "unexpected error: {error}"
        );

        let label: i64 = conn
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row(
                "SELECT label FROM labels WHERE stamp_id = 'stamp-A'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(label, label_int(Label::Spam));
        assert_eq!(conn.token_counts(&words).await.unwrap(), counts_before);
        assert_eq!(conn.totals().await.unwrap(), totals_before);

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn open_account_promotes_legacy_db_sqlite() {
        // Existing per-account corpora trained via the retired
        // SpamFilter live at `<base>/<account>/db.sqlite`. Schema is
        // binary-compatible with `bayes.db`, so cold-start must promote
        // the legacy file in place rather than seeding a fresh corpus.
        let base = std::env::temp_dir().join(format!(
            "bayes-promote-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let account_dir = base.join("42");
        std::fs::create_dir_all(&account_dir).unwrap();

        // Train the legacy file with a known corpus.
        let legacy = account_dir.join("db.sqlite");
        let conn = SqliteAccountConnection::open_path(&legacy, 0).unwrap();
        conn.record_label("legacy-stamp", &toks(&["legacyword"]), Label::Spam, 0)
            .await
            .unwrap();
        let legacy_totals = conn.totals().await.unwrap();
        drop(conn);

        // Open via the backend with no seed; the legacy file should be
        // promoted to bayes.db and the corpus preserved.
        let backend = SqliteBackend::new(&base, None, 0);
        let acc = AccountId::new("42".to_string());
        let opened = backend.open_account(&acc).await.unwrap();
        let promoted_totals = opened.totals().await.unwrap();
        assert_eq!(
            legacy_totals, promoted_totals,
            "legacy corpus must survive promotion"
        );
        assert!(
            account_dir.join("bayes.db").exists(),
            "bayes.db must be created"
        );
        assert!(
            legacy.exists(),
            "legacy db.sqlite is preserved as a recovery point"
        );

        // Verify the trained token actually moved across — not just totals.
        let counts = opened.token_counts(&toks(&["legacyword"])).await.unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].2, 1, "legacy spam count must survive promotion");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn open_account_promotes_wal_data_not_just_main_file() {
        // Spamlite opens corpora in WAL mode, so committed writes can
        // sit in `db.sqlite-wal` until a checkpoint flushes them into
        // the main file. A naive byte-copy of just `db.sqlite` would
        // silently drop those rows. Promotion must go through SQLite
        // so the WAL is included in the snapshot.
        let base = std::env::temp_dir().join(format!(
            "bayes-walpromo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let account_dir = base.join("99");
        std::fs::create_dir_all(&account_dir).unwrap();
        let legacy = account_dir.join("db.sqlite");

        // Open the legacy DB in WAL mode and insert a row, then keep
        // the connection alive so the WAL is *not* checkpointed. The
        // write is committed (visible to readers via WAL) but
        // `db.sqlite` on disk would still be empty/old.
        // spamlite's schema initialiser sets WAL mode itself, so the side-effect of
        // sitting writes in -wal is exercised by the writer's own
        // INSERTs.
        let writer = Connection::open(&legacy).unwrap();
        spamlite::storage::schema::init(&writer).unwrap();
        writer
            .execute(
                "INSERT INTO tokens (word, good, spam, last_seen) VALUES ('walword', 0, 7, 0)",
                [],
            )
            .unwrap();
        writer
            .execute("UPDATE meta SET value = '7' WHERE key = 'total_spam'", [])
            .unwrap();

        // Confirm a sidecar -wal file exists; otherwise the test isn't
        // exercising the failure mode it claims to.
        assert!(
            legacy.with_extension("sqlite-wal").exists()
                || legacy.with_file_name("db.sqlite-wal").exists(),
            "test setup failed to produce a -wal file"
        );

        let backend = SqliteBackend::new(&base, None, 0);
        let opened = backend
            .open_account(&AccountId::new("99".to_string()))
            .await
            .unwrap();

        // The promoted corpus must reflect data that lived in WAL.
        let totals = opened.totals().await.unwrap();
        assert_eq!(
            totals,
            (0, 7),
            "WAL spam total must survive promotion; got {:?}",
            totals
        );
        let counts = opened.token_counts(&toks(&["walword"])).await.unwrap();
        assert_eq!(counts.len(), 1, "WAL token must survive promotion");
        assert_eq!(counts[0].2, 7, "WAL spam count must survive promotion");

        drop(writer);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn open_account_prefers_legacy_over_default_seed() {
        // When both a legacy db.sqlite and a default seed exist, the
        // per-account legacy file wins — otherwise a deployment with a
        // fresh seed would silently lose every account's prior training.
        let base = std::env::temp_dir().join(format!(
            "bayes-prefer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let account_dir = base.join("7");
        std::fs::create_dir_all(&account_dir).unwrap();

        let legacy = account_dir.join("db.sqlite");
        let conn = SqliteAccountConnection::open_path(&legacy, 0).unwrap();
        conn.record_label("legacy-stamp", &toks(&["legacyword"]), Label::Spam, 0)
            .await
            .unwrap();
        drop(conn);

        // Build a separate "seed" corpus that — if used — would
        // overwrite the per-account history with different counts.
        let seed = base.join("default-bayesian.db");
        let seed_conn = SqliteAccountConnection::open_path(&seed, 0).unwrap();
        for i in 0..5 {
            seed_conn
                .record_label(&format!("seed-{i}"), &toks(&["seedword"]), Label::Ham, 0)
                .await
                .unwrap();
        }
        drop(seed_conn);

        let backend = SqliteBackend::new(&base, Some(seed.clone()), 0);
        let opened = backend
            .open_account(&AccountId::new("7".to_string()))
            .await
            .unwrap();

        // The promoted corpus should reflect the legacy file (1 spam,
        // 0 ham), not the seed (0 spam, 5 ham).
        let totals = opened.totals().await.unwrap();
        assert_eq!(totals, (0, 1), "legacy must beat seed; got {:?}", totals);

        let _ = std::fs::remove_dir_all(&base);
    }
}
