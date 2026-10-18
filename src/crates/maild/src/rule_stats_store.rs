//! Restart-durable persistence for [`RuleStats`].
//!
//! One SQLite file at `<rule_stats_dir>/stats.db`. `rule_stats_dir`
//! defaults to `<var>/maild/rules/` (see
//! `config::Config::rule_stats_dir`) — rule stats are global
//! rule-engine state, intentionally disjoint from `spam_db_dir`
//! (per-account Bayesian dbs) so the two can be re-pathed
//! independently. Schema v1 holds three tables — `verdict_counts`,
//! `rule_hits`, `schema_version` — and a single-row contract: each
//! verdict kind contributes one row to `verdict_counts`, each
//! ever-matched rule contributes one row to `rule_hits`. Migrations
//! are idempotent so reopening an existing DB at startup is a no-op.
//!
//! Lifecycle (per `_doc/planned/maild-rules-phase2.md` § Persistence):
//!
//! - **Startup**: `RuleStatsStore::open` → `load_snapshot` →
//!   [`RuleStats::from_snapshot`]. Absent file = clean start.
//! - **Background flush**: a tokio task drives
//!   [`flush_loop`] on `tokio::time::interval(flush_interval)`. Each
//!   tick reads `stats.snapshot()` and writes it through
//!   [`RuleStatsStore::flush_snapshot`].
//! - **Loss window**:
//!   - *Process restart with the host still running* (panic, SIGKILL,
//!     `systemctl restart`): ≤ `flush_interval` of counter increments
//!     lost. The page cache survives the process; the most recent
//!     committed flush is durably visible to the next open.
//!   - *Host crash or power loss*: more than `flush_interval` of
//!     counters can be lost. PRAGMAs (`journal_mode=WAL`,
//!     `synchronous=NORMAL`) prioritise throughput over per-commit
//!     fsync; the WAL may not have been checkpointed when power
//!     failed. Rule-stats are diagnostic counters (not training
//!     data), so the trade is intentional — bumping to
//!     `synchronous=FULL` would impose per-flush fsync latency for
//!     no recovery-critical benefit. The on-disk shape stays
//!     internally consistent in either case: WAL guarantees
//!     `flush_snapshot` is atomic, so partial-snapshot reads cannot
//!     occur.
//!
//! No graceful-shutdown final flush yet — the umbrella runtime has
//! no shutdown plumbing (process exit is the only path today per
//! `runtime.rs` doc). When that lands the flush task gains a
//! `tokio::select!` arm on a shutdown notifier; the file shape and
//! transaction boundary stay unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use crate::rule_stats::{RuleStats, RuleStatsSnapshot};

/// Current on-disk schema version. Bump only via additive
/// migrations; SPEC: rules counters are not training data, so a
/// destructive forced re-init is acceptable if we ever need it.
const CURRENT_SCHEMA_VERSION: i64 = 1;

const VERDICT_KIND_HARD_ACCEPT: &str = "hard_accept";
const VERDICT_KIND_HARD_JUNK: &str = "hard_junk";
const VERDICT_KIND_CONTINUE: &str = "continue_count";

/// Owned handle to the rule-stats SQLite file. Open once at startup;
/// share via `Arc<RuleStatsStore>` between the flush task and any
/// future admin path that might want to force a flush.
pub struct RuleStatsStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl RuleStatsStore {
    /// Open or create the stats DB at `path`. Parent directory is
    /// created if missing. Schema is migrated to
    /// `CURRENT_SCHEMA_VERSION` idempotently — calling `open` on an
    /// existing DB at the current version is a no-op.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let path_for_blocking = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if let Some(parent) = path_for_blocking.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create rule_stats parent dir {}", parent.display())
                })?;
            }
            let conn = Connection::open(&path_for_blocking)
                .with_context(|| format!("open {}", path_for_blocking.display()))?;
            init_schema(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("rule_stats open spawn_blocking: {e}"))??;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Read the persisted snapshot. Absent rows are treated as 0 —
    /// a fresh DB returns an all-zero snapshot, not an error.
    pub async fn load_snapshot(&self) -> Result<RuleStatsSnapshot> {
        let conn = self.conn.lock().await;
        let hard_accept = read_verdict(&conn, VERDICT_KIND_HARD_ACCEPT)?;
        let hard_junk = read_verdict(&conn, VERDICT_KIND_HARD_JUNK)?;
        let continue_count = read_verdict(&conn, VERDICT_KIND_CONTINUE)?;
        let mut per_rule = std::collections::HashMap::new();
        let mut stmt = conn
            .prepare("SELECT rule_id, count FROM rule_hits")
            .context("prepare rule_hits select")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .context("query rule_hits")?;
        for r in rows {
            let (rule_id, count) = r.context("read rule_hits row")?;
            per_rule.insert(rule_id, count.max(0) as u64);
        }
        Ok(RuleStatsSnapshot {
            verdicts_total: hard_accept + hard_junk + continue_count,
            hard_accept,
            hard_junk,
            continue_count,
            per_rule,
        })
    }

    /// Atomically replace the on-disk snapshot with `snap`. All rows
    /// land in a single transaction so a concurrent restart never
    /// reads a partial update.
    ///
    /// Rule IDs absent from `snap.per_rule` are deleted from disk so
    /// the on-disk shape stays in sync with the in-memory shape — if
    /// a future surface wants to expose `last_hit_at` per rule, that
    /// is the place to extend (schema v2). Today the column exists
    /// (set to the current unix time on each upsert) so the schema
    /// doesn't need a backwards-incompatible bump when we wire it.
    pub async fn flush_snapshot(&self, snap: &RuleStatsSnapshot) -> Result<()> {
        let conn = self.conn.lock().await;
        let tx = conn
            .unchecked_transaction()
            .context("begin flush_snapshot tx")?;
        upsert_verdict(&tx, VERDICT_KIND_HARD_ACCEPT, snap.hard_accept)?;
        upsert_verdict(&tx, VERDICT_KIND_HARD_JUNK, snap.hard_junk)?;
        upsert_verdict(&tx, VERDICT_KIND_CONTINUE, snap.continue_count)?;

        // Delete rule_hits rows not in the new snapshot — keeps the
        // on-disk shape == the in-memory shape. (A future "purge
        // rules removed from pack" admin verb would live elsewhere;
        // RuleStats itself doesn't drop entries, so this delete only
        // fires when an operator hand-edits the DB.)
        let now = unix_secs();
        // Build the IN-clause via a temp table — keeps us under the
        // SQLite variable cap for very large packs and stays in one
        // transaction.
        tx.execute("CREATE TEMP TABLE _rs_keep (rule_id TEXT PRIMARY KEY)", [])
            .context("create _rs_keep")?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO _rs_keep (rule_id) VALUES (?1)")
                .context("prepare _rs_keep insert")?;
            for id in snap.per_rule.keys() {
                stmt.execute(params![id]).context("insert _rs_keep")?;
            }
        }
        tx.execute(
            "DELETE FROM rule_hits WHERE rule_id NOT IN (SELECT rule_id FROM _rs_keep)",
            [],
        )
        .context("prune rule_hits")?;

        {
            let mut upsert = tx
                .prepare(
                    "INSERT INTO rule_hits (rule_id, count, last_hit_at) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(rule_id) DO UPDATE SET \
                       count = excluded.count, last_hit_at = excluded.last_hit_at",
                )
                .context("prepare rule_hits upsert")?;
            for (id, count) in &snap.per_rule {
                upsert
                    .execute(params![id, *count as i64, now])
                    .context("execute rule_hits upsert")?;
            }
        }

        tx.execute("DROP TABLE _rs_keep", [])
            .context("drop _rs_keep")?;
        tx.commit().context("commit flush_snapshot tx")?;
        Ok(())
    }

    /// On-disk path, exposed for diagnostics + tests.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---- Schema ----

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )
    .context("rule_stats PRAGMAs")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS verdict_counts (
            kind  TEXT PRIMARY KEY,
            count INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS rule_hits (
            rule_id     TEXT PRIMARY KEY,
            count       INTEGER NOT NULL DEFAULT 0,
            last_hit_at INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;

         -- `singleton` is a CHECK-enforced constant column so the table
         -- can only ever hold one row. We do NOT rely on
         -- `LIMIT 1` / 'whichever row SQLite returns first' to pick the
         -- live version — that read order is unspecified and would
         -- silently accept a DB that contains both v1 and a forged
         -- future version.
         CREATE TABLE IF NOT EXISTS schema_version (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 0),
            version   INTEGER NOT NULL
         ) WITHOUT ROWID;",
    )
    .context("rule_stats schema init")?;

    // Verify the table really is a singleton — relevant for DBs that
    // pre-date the CHECK constraint, or that were forged out-of-band.
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .context("count schema_version rows")?;
    match row_count {
        0 => {
            conn.execute(
                "INSERT INTO schema_version (singleton, version) VALUES (0, ?1)",
                params![CURRENT_SCHEMA_VERSION],
            )
            .context("insert schema_version")?;
        }
        1 => {
            let v: i64 = conn
                .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
                .context("read schema_version row")?;
            if v != CURRENT_SCHEMA_VERSION {
                return Err(anyhow::anyhow!(
                    "rule_stats schema version {v} on disk is not supported (expected {CURRENT_SCHEMA_VERSION}); refusing to open"
                ));
            }
        }
        n => {
            return Err(anyhow::anyhow!(
                "rule_stats schema_version table holds {n} rows; expected exactly one"
            ));
        }
    }
    Ok(())
}

fn read_verdict(conn: &Connection, kind: &str) -> Result<u64> {
    // `QueryReturnedNoRows` is the only error we treat as "fresh DB,
    // start at 0". Anything else (corrupt row, missing column, IO)
    // propagates so the next flush does NOT overwrite real on-disk
    // totals with a zeroed view.
    match conn.query_row(
        "SELECT count FROM verdict_counts WHERE kind = ?1",
        params![kind],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(n) => Ok(n.max(0) as u64),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(e) => Err(e).with_context(|| format!("read verdict_counts row {kind}")),
    }
}

fn upsert_verdict(tx: &rusqlite::Transaction<'_>, kind: &str, count: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO verdict_counts (kind, count) VALUES (?1, ?2) \
         ON CONFLICT(kind) DO UPDATE SET count = excluded.count",
        params![kind, count as i64],
    )
    .with_context(|| format!("upsert verdict {kind}"))?;
    Ok(())
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- Background flush task ----

/// Drive periodic flushes of `stats` to `store` on a fixed cadence.
/// Returns when the future is dropped (task abort) — there is no
/// graceful-shutdown final-flush hook yet (see module docstring).
///
/// A single skipped flush due to a tick coalescing under load is
/// fine: the next tick writes the up-to-date snapshot. We use the
/// `Skip` MissedTickBehavior so a long-running flush doesn't cause
/// a burst of catch-up ticks afterwards.
pub async fn flush_loop(
    stats: Arc<RuleStats>,
    store: Arc<RuleStatsStore>,
    flush_interval: Duration,
) {
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Discard the immediate first tick — startup-load just happened,
    // so an immediate write would be a no-op against the just-read
    // snapshot. Waiting one period also gives the engine a chance to
    // record real activity before we touch the file.
    interval.tick().await;
    loop {
        interval.tick().await;
        let snap = stats.snapshot();
        if let Err(e) = store.flush_snapshot(&snap).await {
            tracing::warn!(
                target: "rule_stats",
                error = %e,
                path = %store.path().display(),
                "rule_stats periodic flush failed (will retry on next interval)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn snap(ha: u64, hj: u64, cc: u64, per_rule: &[(&str, u64)]) -> RuleStatsSnapshot {
        let mut map = HashMap::new();
        for (k, v) in per_rule {
            map.insert((*k).to_string(), *v);
        }
        RuleStatsSnapshot {
            verdicts_total: ha + hj + cc,
            hard_accept: ha,
            hard_junk: hj,
            continue_count: cc,
            per_rule: map,
        }
    }

    #[tokio::test]
    async fn fresh_db_returns_empty_snapshot() {
        let tmp = TempDir::new().unwrap();
        let store = RuleStatsStore::open(tmp.path().join("_rules/stats.db"))
            .await
            .unwrap();
        let s = store.load_snapshot().await.unwrap();
        assert_eq!(s.verdicts_total, 0);
        assert_eq!(s.hard_accept, 0);
        assert_eq!(s.hard_junk, 0);
        assert_eq!(s.continue_count, 0);
        assert!(s.per_rule.is_empty());
    }

    #[tokio::test]
    async fn write_then_reload_preserves_counts() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("_rules/stats.db");
        let store = RuleStatsStore::open(&path).await.unwrap();
        let s1 = snap(3, 7, 11, &[("rule_a", 5), ("rule_b", 2)]);
        store.flush_snapshot(&s1).await.unwrap();
        drop(store);

        // Re-open from disk.
        let store = RuleStatsStore::open(&path).await.unwrap();
        let s2 = store.load_snapshot().await.unwrap();
        assert_eq!(s2, s1);
    }

    #[tokio::test]
    async fn flush_replaces_rule_set() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stats.db");
        let store = RuleStatsStore::open(&path).await.unwrap();
        store
            .flush_snapshot(&snap(0, 0, 0, &[("rule_old", 10), ("rule_keep", 1)]))
            .await
            .unwrap();
        store
            .flush_snapshot(&snap(0, 0, 0, &[("rule_keep", 1), ("rule_new", 4)]))
            .await
            .unwrap();
        let loaded = store.load_snapshot().await.unwrap();
        assert_eq!(loaded.per_rule.get("rule_keep").copied(), Some(1));
        assert_eq!(loaded.per_rule.get("rule_new").copied(), Some(4));
        assert!(
            !loaded.per_rule.contains_key("rule_old"),
            "rule absent from new snapshot must be pruned: {:?}",
            loaded.per_rule,
        );
    }

    #[tokio::test]
    async fn schema_init_is_idempotent() {
        // Opening the same path twice in a row must not double-insert
        // schema_version or otherwise mutate state.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stats.db");
        let _ = RuleStatsStore::open(&path).await.unwrap();
        let _ = RuleStatsStore::open(&path).await.unwrap();
        let store = RuleStatsStore::open(&path).await.unwrap();
        let n: i64 = store
            .conn
            .lock()
            .await
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 1, "schema_version table must hold exactly one row");
    }

    #[tokio::test]
    async fn newer_unknown_schema_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stats.db");
        // Forge a v999 file directly. The `CREATE TABLE IF NOT EXISTS`
        // in `init_schema` is a no-op against this pre-existing
        // shape, so the singleton-version row we install here is the
        // only row `init_schema` will see.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (
                   singleton INTEGER PRIMARY KEY CHECK (singleton = 0),
                   version   INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 INSERT INTO schema_version (singleton, version) VALUES (0, 999);",
            )
            .unwrap();
        }
        let r = RuleStatsStore::open(&path).await;
        assert!(r.is_err(), "open must refuse an unknown future schema");
    }

    #[tokio::test]
    async fn forged_duplicate_schema_version_rows_are_refused() {
        // Even if a future bug or hand-edit somehow gets two rows
        // into `schema_version` (e.g. an unwise migration), opening
        // the DB must refuse rather than pick "whichever row SQLite
        // returns first". The CHECK constraint blocks an additional
        // row via INSERT-statements, so simulate the failure mode by
        // creating a table WITHOUT the constraint and dropping two
        // rows in.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stats.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (
                   singleton INTEGER PRIMARY KEY,
                   version   INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 INSERT INTO schema_version (singleton, version) VALUES (0, 1);
                 INSERT INTO schema_version (singleton, version) VALUES (1, 999);",
            )
            .unwrap();
        }
        let r = RuleStatsStore::open(&path).await;
        assert!(
            r.is_err(),
            "open must refuse a schema_version table with more than one row"
        );
    }
}
