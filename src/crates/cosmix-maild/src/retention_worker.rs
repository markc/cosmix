//! In-process retention worker — periodically trims aged Junk/Trash
//! memberships across every account, the dovecot `expunge … savedbefore
//! Nd` cron analog. Mirrors the [`crate::mailstore::expiry::ExpiryWorker`]
//! shape: a `tokio::time::interval` loop, one pass per tick, per-account
//! failures logged and skipped, supervised by maild's own restart cycle.
//!
//! The destructive work lives in the well-tested mailstore primitive
//! [`crate::mailstore::SqliteMailStore::retention_sweep_account`]; this
//! worker is the glue that reads the [`crate::props::retention`] config,
//! enumerates accounts (the `Account.id → SetId` derivation is one-way,
//! so accounts come from the db, not `mds.list_sets`), aggregates the
//! per-account reports, and — only after a pass that actually removed a
//! membership and is NOT a dry run — calls `mds.gc()` once to reclaim the
//! now-orphaned CAS blobs (removal makes them GC-eligible; nothing else
//! in the daemon reclaims them).
//!
//! **Ships inert.** The config defaults are `0`-day windows + `dry_run =
//! true`, so the loop runs but deletes nothing until an operator arms a
//! window AND clears dry-run.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cosmix_mds::Mds;
use cosmix_props::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::db::Db;
use crate::mailstore::{RetentionParams, SqliteMailStore};
use crate::props::retention::{self, RetentionConfig};

/// One retention pass's aggregated outcome across all swept accounts.
/// In a dry run the `*_removed` counts are what WOULD be removed.
#[derive(Debug, Clone, Default)]
pub struct RetentionRunReport {
    pub accounts_swept: u64,
    pub junk_removed: u64,
    pub trash_removed: u64,
    /// Accounts that hit the per-sweep cap (more aged than the cap
    /// allowed; the remainder bleeds down on the next tick).
    pub capped_accounts: u64,
    /// True if a post-sweep `mds.gc()` ran (real removals, not dry-run).
    pub gc_ran: bool,
    pub dry_run: bool,
    /// True when the policy was disabled (no armed window) — a no-op pass.
    pub disabled: bool,
}

impl RetentionRunReport {
    pub fn total_removed(&self) -> u64 {
        self.junk_removed + self.trash_removed
    }
}

#[derive(Default)]
struct StatusSnapshot {
    last_sweep_ms: Option<i64>,
    last: Option<RetentionRunReport>,
}

/// Shared last-sweep status for the `maild.retention.status` verb. The
/// spawned loop and the Bus-dispatch handle share one `Arc<RetentionStatus>`
/// (via `RetentionWorker::clone`), so the verb reports what the loop did.
#[derive(Default)]
pub struct RetentionStatus {
    inner: Mutex<StatusSnapshot>,
}

impl RetentionStatus {
    fn record(&self, now_ms: i64, report: &RetentionRunReport) {
        if let Ok(mut g) = self.inner.lock() {
            g.last_sweep_ms = Some(now_ms);
            g.last = Some(report.clone());
        }
    }

    /// `(last_sweep_epoch_ms, last_report)`.
    pub fn snapshot(&self) -> (Option<i64>, Option<RetentionRunReport>) {
        match self.inner.lock() {
            Ok(g) => (g.last_sweep_ms, g.last.clone()),
            Err(_) => (None, None),
        }
    }
}

/// The retention worker. Cheap to clone — all heavy fields are `Arc`s /
/// a `Db` handle (itself `Arc`-backed) — so one clone drives the spawned
/// loop while another backs the `maild.retention.*` Bus verbs, both
/// sharing the same status cell.
#[derive(Clone)]
pub struct RetentionWorker {
    mailstore: Arc<SqliteMailStore>,
    db: Db,
    runtime: Arc<Runtime>,
    tick: Duration,
    status: Arc<RetentionStatus>,
}

impl RetentionWorker {
    /// `tick_minutes` is read from config at startup; a change to it
    /// takes effect on the next daemon restart (the interval is fixed
    /// once the loop starts — the per-sweep config read still picks up
    /// window / dry-run changes live).
    pub fn new(
        mailstore: Arc<SqliteMailStore>,
        db: Db,
        runtime: Arc<Runtime>,
        tick_minutes: u64,
    ) -> Self {
        let tick = Duration::from_secs(tick_minutes.max(1) * 60);
        Self {
            mailstore,
            db,
            runtime,
            tick,
            status: Arc::new(RetentionStatus::default()),
        }
    }

    pub fn status(&self) -> &Arc<RetentionStatus> {
        &self.status
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Current effective config (for the `status` verb).
    pub async fn current_config(&self) -> Result<RetentionConfig> {
        retention::read_config(&self.runtime).await
    }

    /// Spawn the periodic loop as a detached tokio task.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run_loop())
    }

    async fn run_loop(self) {
        let mut tick = interval(self.tick);
        // Skip the immediate t=0 tick so startup isn't pinned to a sweep.
        tick.tick().await;
        loop {
            tick.tick().await;
            match self.sweep(None, None).await {
                Ok(r) if r.disabled => {
                    debug!(target: "maild::retention", "sweep: retention disabled (no armed window)")
                }
                Ok(r) => info!(
                    target: "maild::retention",
                    accounts = r.accounts_swept,
                    junk_removed = r.junk_removed,
                    trash_removed = r.trash_removed,
                    capped_accounts = r.capped_accounts,
                    dry_run = r.dry_run,
                    gc_ran = r.gc_ran,
                    "retention sweep"
                ),
                Err(e) => warn!(target: "maild::retention", "retention sweep failed: {e:#}"),
            }
        }
    }

    /// Run one sweep. `dry_run_override` (the `run` verb's `dry_run=`)
    /// forces dry-run on/off for this pass regardless of config;
    /// `account_override` (the `run` verb's `account=` resolved to an id)
    /// limits the pass to a single account. Public so the
    /// `maild.retention.run` verb and tests can drive it directly.
    pub async fn sweep(
        &self,
        dry_run_override: Option<bool>,
        account_override: Option<i32>,
    ) -> Result<RetentionRunReport> {
        let cfg = self.current_config().await?;
        let dry_run = dry_run_override.unwrap_or(cfg.dry_run);
        let now = now_ms();

        if cfg.is_disabled() {
            let report = RetentionRunReport {
                dry_run,
                disabled: true,
                ..Default::default()
            };
            self.status.record(now, &report);
            return Ok(report);
        }

        let params = RetentionParams {
            junk_retention_days: cfg.junk_retention_days,
            trash_retention_days: cfg.trash_retention_days,
            max_deletes_per_sweep: cfg.max_deletes_per_sweep,
            dry_run,
        };

        // The armed allowlist is THE per-account gate (LOCKED decision:
        // no fleet auto-default). Sweep only accounts whose email is
        // opted in — and, if the run verb targeted one account, only that
        // one. `is_disabled` already short-circuited an empty allowlist,
        // so `armed` is non-empty here.
        let armed: std::collections::HashSet<String> = cfg.armed_accounts.iter().cloned().collect();
        let targets: Vec<(i32, String)> = crate::db::account::list(&self.db.conn)
            .await?
            .into_iter()
            .filter(|a| armed.contains(&a.email))
            .filter(|a| account_override.is_none_or(|id| a.id == id))
            .map(|a| (a.id, a.email))
            .collect();

        let ms = Arc::clone(&self.mailstore);
        // One blocking task for the whole pass: each `retention_sweep_account`
        // is a synchronous `with_set_tx`, and the post-sweep `gc()` is a
        // store scan — all off the async runtime so the dispatch loop
        // stays responsive.
        let report = tokio::task::spawn_blocking(move || {
            let mut agg = RetentionRunReport {
                dry_run,
                ..Default::default()
            };
            for (id, email) in targets {
                match ms.retention_sweep_account(id, &params, now) {
                    Ok(r) => {
                        agg.accounts_swept += 1;
                        agg.junk_removed += r.junk_removed;
                        agg.trash_removed += r.trash_removed;
                        if r.junk_capped || r.trash_capped {
                            agg.capped_accounts += 1;
                        }
                        // Per-account audit line whenever something was
                        // (or, in dry-run, would be) removed — the
                        // granular record an operator wants alongside the
                        // props audit.watch of config changes.
                        if r.junk_removed > 0 || r.trash_removed > 0 {
                            info!(
                                target: "maild::retention::audit",
                                account = %email,
                                junk_removed = r.junk_removed,
                                trash_removed = r.trash_removed,
                                junk_capped = r.junk_capped,
                                trash_capped = r.trash_capped,
                                dry_run,
                                "retention removed memberships"
                            );
                        }
                    }
                    // One bad account never stalls the fleet sweep.
                    Err(e) => warn!(
                        target: "maild::retention",
                        account_id = id,
                        "retention sweep account failed: {e:#}"
                    ),
                }
            }
            // Reclaim freed CAS blobs ONCE, and only when a real (non-dry)
            // pass actually removed a membership — an idle or dry-run pass
            // skips the (two-pass) store scan entirely.
            if !dry_run && agg.total_removed() > 0 {
                match ms.mds().gc(false) {
                    Ok(_) => {
                        agg.gc_ran = true;
                        info!(target: "maild::retention", "post-sweep gc complete");
                    }
                    Err(e) => warn!(target: "maild::retention", "post-sweep gc failed: {e:#}"),
                }
            }
            agg
        })
        .await?;

        self.status.record(now, &report);
        Ok(report)
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use cosmix_mds::{Flags, Mds, Membership, SqliteCasMds};
    use cosmix_props::bus::mutation::PropsRouter;
    use cosmix_props::record::{Actor, RecordKey};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::sqlite::SqliteStore;
    use cosmix_props::store::MergeMode;
    use cosmix_props::value::PropValue;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::mailstore::account_id_to_setid;
    use crate::props::retention;

    const DAY_MS: i64 = 86_400_000;
    const ACCT: i32 = 1;

    /// Full worker fixture: a db (with one seeded account), a mailstore
    /// (with account 1's set provisioned), and a live `maild.retention`
    /// runtime — all aligned on account id 1.
    struct Fixture {
        worker: RetentionWorker,
        runtime: Arc<Runtime>,
        mds: Arc<SqliteCasMds>,
        _tmp: TempDir,
        _dbtmp: TempDir,
    }

    async fn fixture(operators: Vec<String>) -> Fixture {
        // Accounts db.
        let dbtmp = TempDir::new().unwrap();
        let db_path = dbtmp.path().join("mail.db");
        let blob_dir = dbtmp.path().join("blob");
        let db = Db::connect(db_path.to_str().unwrap(), blob_dir.to_str().unwrap())
            .await
            .unwrap();
        db.migrate().await.unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, ?3)",
                rusqlite::params![ACCT, "u@example.com", "x"],
            )
            .unwrap();
        }

        // Mailstore + account 1's set.
        let tmp = TempDir::new().unwrap();
        // Drop the GC quiescence wait (default 60s) so the post-sweep
        // gc() in `armed_real_sweep_*` doesn't make the test sleep a
        // minute. Production reads the wait from the env (clamped ≥5s).
        let mds = Arc::new(
            SqliteCasMds::open(tmp.path())
                .unwrap()
                .with_gc_quiescence(Duration::ZERO),
        );
        let store = Arc::new(SqliteMailStore::new(Arc::clone(&mds)));
        store.ensure_account_set(ACCT).unwrap();

        // Retention config runtime.
        let conn = Connection::open_in_memory().unwrap();
        let pstore = Arc::new(SqliteStore::new("maild", conn).unwrap());
        let mut router = PropsRouter::new("maild");
        let runtime = retention::register(&mut router, &pstore, operators.clone()).unwrap();

        let worker = RetentionWorker::new(store, db, Arc::clone(&runtime), 60);
        Fixture {
            worker,
            runtime,
            mds,
            _tmp: tmp,
            _dbtmp: dbtmp,
        }
    }

    /// Arm the config via the substrate (partial Replace — absent fields
    /// read as defaults). `armed` is the per-account opt-in allowlist.
    async fn arm(runtime: &Runtime, junk_days: u64, dry_run: bool, armed: &[&str]) {
        let key = RecordKey::singleton(retention::namespace_name());
        let mut m = BTreeMap::new();
        m.insert("junk_retention_days".into(), PropValue::UInt(junk_days));
        m.insert("dry_run".into(), PropValue::Bool(dry_run));
        m.insert(
            "armed_accounts".into(),
            PropValue::List(
                armed
                    .iter()
                    .map(|s| PropValue::String(s.to_string()))
                    .collect(),
            ),
        );
        let opts = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::service("test").expect("valid actor"),
            cause: None,
            ts_ms: 0,
        };
        runtime.set(key, PropValue::Object(m), opts).await.unwrap();
    }

    fn mk_junk(mds: &SqliteCasMds) -> cosmix_mds::ContainerId {
        let attrs = cosmix_mds::ContainerAttrs {
            special_use: Some("\\Junk".into()),
            subscribed: false,
            extra: serde_json::json!({}),
        };
        mds.create_container(&account_id_to_setid(ACCT), None, "Junk", attrs)
            .unwrap()
    }

    fn add_aged(mds: &SqliteCasMds, junk: cosmix_mds::ContainerId, age_days: i64) {
        let set = account_id_to_setid(ACCT);
        let now = now_ms();
        let hash = mds.put_blob(format!("p{age_days}").as_bytes()).unwrap();
        let m = Membership {
            container: junk,
            flags: Flags(0),
            added_at: now - age_days * DAY_MS,
        };
        mds.add_item(&set, &hash, &[m]).unwrap();
    }

    fn junk_count(mds: &SqliteCasMds, junk: cosmix_mds::ContainerId) -> i64 {
        let set = account_id_to_setid(ACCT);
        let mut out = 0i64;
        let _: cosmix_mds::Result<()> = mds.with_set_tx(&set, |tx| {
            out = tx
                .tx()
                .query_row(
                    "SELECT COUNT(*) FROM membership WHERE container_id = ?1",
                    rusqlite::params![junk.0.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            Err(cosmix_mds::Error::Other("probe".into()))
        });
        out
    }

    #[tokio::test]
    async fn disabled_config_is_noop() {
        let fx = fixture(Vec::new()).await;
        // No arm → 0-day windows → disabled.
        let r = fx.worker.sweep(None, None).await.unwrap();
        assert!(r.disabled);
        assert_eq!(r.total_removed(), 0);
        assert!(!r.gc_ran);
    }

    #[tokio::test]
    async fn dry_run_default_removes_nothing_no_gc() {
        let fx = fixture(Vec::new()).await;
        let junk = mk_junk(&fx.mds);
        add_aged(&fx.mds, junk, 30);
        // Arm a 7-day window but leave dry_run TRUE (the shipped default).
        arm(&fx.runtime, 7, true, &["u@example.com"]).await;

        let r = fx.worker.sweep(None, None).await.unwrap();
        assert!(!r.disabled);
        assert!(r.dry_run);
        assert_eq!(r.junk_removed, 1, "reports what WOULD be removed");
        assert!(!r.gc_ran, "dry-run must not gc");
        assert_eq!(junk_count(&fx.mds, junk), 1, "dry-run removed nothing");
    }

    #[tokio::test]
    async fn armed_real_sweep_removes_and_runs_gc() {
        let fx = fixture(Vec::new()).await;
        let junk = mk_junk(&fx.mds);
        add_aged(&fx.mds, junk, 30);
        add_aged(&fx.mds, junk, 1); // fresh — survives a 7-day window
        arm(&fx.runtime, 7, false, &["u@example.com"]).await;

        let r = fx.worker.sweep(None, None).await.unwrap();
        assert_eq!(r.accounts_swept, 1);
        assert_eq!(r.junk_removed, 1);
        assert!(r.gc_ran, "a real removal triggers post-sweep gc");
        assert_eq!(junk_count(&fx.mds, junk), 1, "only the aged item removed");
    }

    #[tokio::test]
    async fn account_not_in_armed_allowlist_is_skipped() {
        // THE per-account gate: a window armed for real (days>0,
        // dry_run:false) must NOT touch an account that isn't opted in —
        // no fleet auto-default.
        let fx = fixture(Vec::new()).await;
        let junk = mk_junk(&fx.mds);
        add_aged(&fx.mds, junk, 30);
        // Armed for real, but the allowlist names a DIFFERENT account.
        arm(&fx.runtime, 7, false, &["someone-else@example.com"]).await;

        let r = fx.worker.sweep(None, None).await.unwrap();
        assert_eq!(r.accounts_swept, 0, "non-armed account not swept");
        assert_eq!(r.total_removed(), 0);
        assert!(!r.gc_ran);
        assert_eq!(
            junk_count(&fx.mds, junk),
            1,
            "aged mail preserved (not armed)"
        );
    }

    #[tokio::test]
    async fn run_verb_dry_run_override_forces_dry() {
        // Config armed for real, but the run verb forces dry_run=true.
        let fx = fixture(Vec::new()).await;
        let junk = mk_junk(&fx.mds);
        add_aged(&fx.mds, junk, 30);
        arm(&fx.runtime, 7, false, &["u@example.com"]).await;

        let r = fx.worker.sweep(Some(true), None).await.unwrap();
        assert!(r.dry_run);
        assert_eq!(r.junk_removed, 1);
        assert!(!r.gc_ran);
        assert_eq!(junk_count(&fx.mds, junk), 1, "override kept it dry");
    }

    #[tokio::test]
    async fn status_snapshot_records_last_sweep() {
        let fx = fixture(Vec::new()).await;
        assert_eq!(fx.worker.status().snapshot().0, None, "no sweep yet");
        let _ = fx.worker.sweep(None, None).await.unwrap();
        let (last_ms, last) = fx.worker.status().snapshot();
        assert!(last_ms.is_some());
        assert!(last.is_some());
    }

    #[tokio::test]
    async fn run_verb_operator_gate() {
        use crate::bus::retention::{RetentionBusState, dispatch};
        use cosmix_client::IncomingCommand;

        let fx = fixture(vec!["ops-node".to_string()]).await;
        let state = RetentionBusState {
            worker: Arc::new(fx.worker.clone()),
            operators: Arc::new(vec!["ops-node".to_string()]),
        };

        let mk = |from: &str| IncomingCommand {
            command: "maild.retention.run".into(),
            from: from.to_string(),
            id: None,
            args: serde_json::Value::Null,
            headers: std::collections::BTreeMap::new(),
            body: String::new(),
        };

        // Non-operator sender → auth_denied.
        let (rc, body) = dispatch("run", &mk("random-node"), &state).await;
        assert_eq!(rc, RC_ERROR_TEST);
        assert!(body.contains("auth_denied"), "body: {body}");

        // Anonymous (empty from) → auth_denied.
        let (rc, _) = dispatch("run", &mk(""), &state).await;
        assert_eq!(rc, RC_ERROR_TEST);

        // Named operator → allowed (disabled config ⇒ rc 0, no-op).
        let (rc, body) = dispatch("run", &mk("ops-node"), &state).await;
        assert_eq!(rc, 0, "body: {body}");

        // status verb is open (no gate).
        let (rc, _) = dispatch("status", &mk(""), &state).await;
        assert_eq!(rc, 0);
    }

    // Local mirror of the bus module's error rc for the gate assertions.
    const RC_ERROR_TEST: u8 = 10;
}
