//! `maild.bayesian.*` Bus action handlers.
//!
//! Four actions:
//! - `maild.bayesian.stats` — per-account corpus stats. Thin wrapper
//!   over `Classifier::stats`. An account that has never trained
//!   surfaces with `cold_start: true` and zero counters (the storage
//!   layer creates the per-account directory lazily — acceptable
//!   under the spec's "WG-trusted metadata" framing).
//! - `maild.bayesian.classify` — debug action that runs
//!   `Classifier::classify` against a caller-supplied raw RFC 5322
//!   message. Synthesises a `ClassifyContext` with `rules_score = 0.0`,
//!   no matched rules, and `trusted = false`. Read-only: never calls
//!   `record_label`.
//! - `maild.bayesian.rebuild` — build a shadow corpus from current folder
//!   state, replay corrections made during the walk, then atomically replace
//!   the live corpus. `\\Junk` is Spam; `\\Trash`, `\\Drafts`, `\\Sent`, and
//!   the internal upload-staging container are ignored; descendants inherit
//!   the nearest ancestor with a special-use role (their own role wins), and
//!   every other mailbox is Ham. Junk wins conflicts.
//! - `maild.bayesian.rebuild_status` — return the latest in-memory rebuild job
//!   for an account, or `idle` when none has run since daemon startup.
//!
//! `wait: false` only validates the account, reserves the job and spawns the
//! work; enumeration, shadow training, replay, snapshot and swap all run in
//! that task. `wait: true` holds the maild Bus dispatcher for the entire job
//! and is subject to the client's 60-second request timeout. It is for tests
//! and small mailboxes; real mailboxes use `wait: false` and poll
//! `rebuild_status`.
//! `allow_empty` defaults to false: an initially empty shadow, or one emptied
//! by replay, fails without touching the live corpus unless the caller opts in.
//!
//! Enumeration is not one MDS snapshot. The fence and replay protect user
//! corrections: moves across the Junk boundary, which write a live label row.
//! Membership-only changes after enumeration — deletion, moves between
//! non-Junk folders, and newly delivered mail — write no label row and are
//! reflected by the next rebuild. The result is folder state as of enumeration,
//! plus every correction made before the swap.
//!
//! `bayesian_rebuild_operators` is an opt-in peer gate. An empty list is open
//! to every Bus peer by deliberate agentic-first default; a non-empty list
//! admits only exact sender matches.
//!
//! All actions return `(rc, body_json)` with the same convention as
//! `bus::rules`: `rc = 0` on success and `rc >= 10` on caller or engine
//! errors. A waited rebuild that aborts or fails returns `rc = 10` with its
//! job body so the counters and last error remain available.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use cosmix_client::IncomingCommand;
use cosmix_maild_bayesian::{
    ClassifyContext, DefaultClassifier, Label, RetrainOutcome, RetrainRequest,
    classifier::Classifier,
    storage::{AccountConnection, SqliteAccountConnection, SwapOutcome},
};
use cosmix_maild_rules::AccountId;
use cosmix_mds::{BlobHash, ContainerId, ItemId, Mds};

use crate::{
    db,
    mailstore::{ListOpts, MailStore, SqliteMailStore, account_id_to_setid},
};

const RC_ERROR: u8 = 10;
const MAX_CONSECUTIVE_ERRORS: u32 = 25;
const MAX_SWAP_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildJob {
    pub state: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub ham_candidates: u64,
    pub spam_candidates: u64,
    pub ham_trained: u64,
    pub spam_trained: u64,
    pub replayed: u64,
    pub already_labeled: u64,
    pub skipped_missing: u64,
    pub conflicts: u64,
    pub errors: u64,
    pub last_error: Option<String>,
    pub snapshot: Option<String>,
    pub ignored_mailboxes: Vec<String>,
}

impl RebuildJob {
    fn running() -> Self {
        Self {
            state: "running".to_string(),
            started_at: unix_secs(),
            finished_at: None,
            ham_candidates: 0,
            spam_candidates: 0,
            ham_trained: 0,
            spam_trained: 0,
            replayed: 0,
            already_labeled: 0,
            skipped_missing: 0,
            conflicts: 0,
            errors: 0,
            last_error: None,
            snapshot: None,
            ignored_mailboxes: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct BayesianBusState {
    pub(crate) jobs: Arc<Mutex<HashMap<String, RebuildJob>>>,
    operators: Arc<Vec<String>>,
}

impl BayesianBusState {
    pub fn new(operators: Vec<String>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            operators: Arc::new(operators),
        }
    }
}

impl Default for BayesianBusState {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// Dispatch a `maild.bayesian.*` command. Returns `(rc, body_json)`.
pub async fn dispatch(
    action: &str,
    cmd: &IncomingCommand,
    classifier: &Arc<DefaultClassifier>,
    db: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    state: &BayesianBusState,
) -> (u8, String) {
    let args = super::resolve_args(cmd);
    match action {
        "stats" => handle_stats(classifier, &args).await,
        "classify" => handle_classify(classifier, &args).await,
        "rebuild" => {
            if !rebuild_authorised(cmd, state) {
                (RC_ERROR, err_body("not an authorised rebuild operator"))
            } else {
                handle_rebuild(classifier, db, mailstore, state, &args).await
            }
        }
        "rebuild_status" => handle_rebuild_status(state, &args),
        other => (
            RC_ERROR,
            err_body(&format!("unknown bayesian action: {other}")),
        ),
    }
}

fn rebuild_authorised(cmd: &IncomingCommand, state: &BayesianBusState) -> bool {
    state.operators.is_empty() || state.operators.iter().any(|operator| operator == &cmd.from)
}

#[derive(serde::Deserialize)]
struct StatsRequest {
    /// Wire form for the account id. Must be a non-negative integer,
    /// either as a JSON number or an all-digits string — see
    /// `parse_account_id`. Anything else is rejected so a peer can't
    /// steer the on-disk `<base>/<id>/bayes.db` path outside the
    /// corpus tree.
    account_id: serde_json::Value,
}

async fn handle_stats(classifier: &DefaultClassifier, args: &serde_json::Value) -> (u8, String) {
    let req: StatsRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => return (RC_ERROR, err_body(&format!("malformed stats request: {e}"))),
    };
    let account = match parse_account_id(&req.account_id) {
        Ok(id) => AccountId::new(id.to_string()),
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    match classifier.stats(&account).await {
        Ok(stats) => match serde_json::to_string(&stats) {
            Ok(body) => (0, body),
            Err(e) => (RC_ERROR, err_body(&format!("serialize AccountStats: {e}"))),
        },
        Err(e) => (RC_ERROR, err_body(&format!("stats failed: {e}"))),
    }
}

#[derive(serde::Deserialize)]
struct ClassifyRequest {
    account_id: serde_json::Value,
    message_b64: String,
}

async fn handle_classify(classifier: &DefaultClassifier, args: &serde_json::Value) -> (u8, String) {
    let req: ClassifyRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                RC_ERROR,
                err_body(&format!("malformed classify request: {e}")),
            );
        }
    };

    let message = match base64::engine::general_purpose::STANDARD.decode(req.message_b64.as_bytes())
    {
        Ok(b) => b,
        Err(e) => return (RC_ERROR, err_body(&format!("message_b64 decode: {e}"))),
    };

    let account = match parse_account_id(&req.account_id) {
        Ok(id) => AccountId::new(id.to_string()),
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let ctx = ClassifyContext {
        message: &message,
        account: &account,
        rules_score: 0.0,
        matched_rules: &[],
        trusted: false,
    };

    match classifier.classify(&ctx).await {
        Ok(verdict) => match serde_json::to_string(&verdict) {
            Ok(body) => (0, body),
            Err(e) => (
                RC_ERROR,
                err_body(&format!("serialize BayesianVerdict: {e}")),
            ),
        },
        Err(e) => (RC_ERROR, err_body(&format!("classify failed: {e}"))),
    }
}

#[derive(serde::Deserialize)]
struct RebuildRequest {
    account_id: serde_json::Value,
    #[serde(default = "default_true")]
    snapshot: bool,
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    allow_empty: bool,
}

#[derive(serde::Deserialize)]
struct RebuildStatusRequest {
    account_id: serde_json::Value,
}

#[derive(Clone, Copy)]
struct RebuildCandidate {
    id: ItemId,
    blob_hash: BlobHash,
    label: Label,
    seen_ham: bool,
    seen_spam: bool,
    conflict_counted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MembershipPolicy {
    Ham,
    Spam,
    Ignored,
}

#[derive(Clone, Copy)]
struct ShadowEntry {
    label: Label,
    blob_hash: BlobHash,
}

struct PreparedCorpus {
    candidates: Vec<RebuildCandidate>,
    ham_candidates: u64,
    spam_candidates: u64,
    conflicts: u64,
    ignored_mailboxes: Vec<String>,
}

#[cfg(test)]
type AfterEnumerationHook = Arc<dyn Fn(&Path, i64) + Send + Sync>;
#[cfg(test)]
type AfterReplayHook = Arc<dyn Fn(&Path, i64) + Send + Sync>;
#[cfg(test)]
type ShadowOpenedHook = Arc<dyn Fn(&Path) + Send + Sync>;

#[derive(Clone)]
struct RebuildOptions {
    snapshot: bool,
    allow_empty: bool,
    #[cfg(test)]
    after_enumeration: Option<AfterEnumerationHook>,
    #[cfg(test)]
    after_replay: Option<AfterReplayHook>,
    #[cfg(test)]
    shadow_opened: Option<ShadowOpenedHook>,
    #[cfg(test)]
    fail_all_reads: bool,
}

impl RebuildOptions {
    fn new(snapshot: bool) -> Self {
        Self {
            snapshot,
            allow_empty: false,
            #[cfg(test)]
            after_enumeration: None,
            #[cfg(test)]
            after_replay: None,
            #[cfg(test)]
            shadow_opened: None,
            #[cfg(test)]
            fail_all_reads: false,
        }
    }

    #[cfg(test)]
    fn fail_reads(&self) -> bool {
        self.fail_all_reads
    }

    #[cfg(not(test))]
    fn fail_reads(&self) -> bool {
        false
    }

    fn run_after_enumeration(&self, live_path: &Path, started_at: i64) {
        #[cfg(test)]
        if let Some(hook) = &self.after_enumeration {
            hook(live_path, started_at);
        }
        #[cfg(not(test))]
        let _ = (live_path, started_at);
    }

    fn run_after_replay(&self, live_path: &Path, started_at: i64) {
        #[cfg(test)]
        if let Some(hook) = &self.after_replay {
            hook(live_path, started_at);
        }
        #[cfg(not(test))]
        let _ = (live_path, started_at);
    }

    fn run_shadow_opened(&self, shadow_path: &Path) {
        #[cfg(test)]
        if let Some(hook) = &self.shadow_opened {
            hook(shadow_path);
        }
        #[cfg(not(test))]
        let _ = shadow_path;
    }
}

enum RebuildCompletion {
    Done,
    Aborted,
}

/// Drop guard for `run_rebuild`: every normal exit sets a terminal state
/// first, so this only fires on a panic or task cancellation.
struct RunningGuard {
    state: BayesianBusState,
    account_key: String,
    shadow_path: Option<PathBuf>,
    rebuild_lock: Option<File>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.shadow_path {
            let _ = remove_shadow_files(path);
        }
        let still_running =
            current_job(&self.state, &self.account_key).is_some_and(|job| job.state == "running");
        if still_running {
            finish_failed(
                &self.state,
                &self.account_key,
                "rebuild task ended without a terminal state (panic or cancellation)".to_string(),
            );
        }
    }
}

async fn handle_rebuild(
    classifier: &Arc<DefaultClassifier>,
    database: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    state: &BayesianBusState,
    args: &serde_json::Value,
) -> (u8, String) {
    handle_rebuild_with_options(classifier, database, mailstore, state, args, None).await
}

async fn handle_rebuild_with_options(
    classifier: &Arc<DefaultClassifier>,
    database: &db::Db,
    mailstore: &Arc<SqliteMailStore>,
    state: &BayesianBusState,
    args: &serde_json::Value,
    options_override: Option<RebuildOptions>,
) -> (u8, String) {
    let req: RebuildRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                RC_ERROR,
                err_body(&format!("malformed rebuild request: {e}")),
            );
        }
    };
    let account_i32 = match parse_account_id(&req.account_id) {
        Ok(id) => id,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    match db::account::get_by_id(&database.conn, account_i32).await {
        Ok(Some(_)) => {}
        Ok(None) => return (RC_ERROR, err_body("account not found")),
        Err(e) => {
            return (RC_ERROR, err_body(&format!("account lookup failed: {e}")));
        }
    }
    let account_key = account_i32.to_string();
    let account = AccountId::new(account_key.clone());

    let initial_job = {
        let mut jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs
            .get(&account_key)
            .is_some_and(|job| job.state == "running")
        {
            return (
                RC_ERROR,
                err_body(&format!(
                    "Bayesian rebuild already running for account {account_key}"
                )),
            );
        }
        let job = RebuildJob::running();
        jobs.insert(account_key.clone(), job.clone());
        job
    };

    tracing::info!(
        target: "maild::bayesian",
        account_id = %account_key,
        snapshot = req.snapshot,
        allow_empty = req.allow_empty,
        wait = req.wait,
        "Bayesian corpus rebuild started"
    );

    let classifier = Arc::clone(classifier);
    let mailstore = Arc::clone(mailstore);
    let task_state = state.clone();
    let task_key = account_key.clone();
    let mut options = options_override.unwrap_or_else(|| RebuildOptions::new(req.snapshot));
    options.snapshot = req.snapshot;
    options.allow_empty = req.allow_empty;
    let task = tokio::spawn(async move {
        run_rebuild(
            classifier,
            mailstore,
            task_state,
            account_i32,
            account,
            options,
        )
        .await;
    });

    if !req.wait {
        return serialize_job(&initial_job, false);
    }
    if let Err(e) = task.await
        && current_job(state, &task_key).is_some_and(|job| job.state == "running")
    {
        finish_failed(state, &task_key, format!("rebuild task failed: {e}"));
    }
    current_job_response(state, &account_key, true)
}

fn handle_rebuild_status(state: &BayesianBusState, args: &serde_json::Value) -> (u8, String) {
    let req: RebuildStatusRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                RC_ERROR,
                err_body(&format!("malformed rebuild_status request: {e}")),
            );
        }
    };
    let account = match parse_account_id(&req.account_id) {
        Ok(id) => id,
        Err(e) => return (RC_ERROR, err_body(&e)),
    };
    let account_key = account.to_string();
    let jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
    match jobs.get(&account_key) {
        Some(job) => serialize_job(job, false),
        None => (
            0,
            serde_json::json!({
                "account_id": account_key,
                "state": "idle",
            })
            .to_string(),
        ),
    }
}

fn special_use_policy(special_use: Option<&str>) -> MembershipPolicy {
    match special_use {
        Some("\\Junk") => MembershipPolicy::Spam,
        Some("\\Trash") | Some("\\Drafts") | Some("\\Sent") => MembershipPolicy::Ignored,
        _ => MembershipPolicy::Ham,
    }
}

fn mailbox_policy_map(
    mailboxes: &[crate::mailstore::MailboxRecord],
) -> HashMap<ContainerId, MembershipPolicy> {
    let by_id: HashMap<ContainerId, &crate::mailstore::MailboxRecord> = mailboxes
        .iter()
        .map(|mailbox| (mailbox.id, mailbox))
        .collect();

    mailboxes
        .iter()
        .map(|mailbox| {
            let mut current = Some(mailbox.id);
            let mut visited = HashSet::new();
            let policy = loop {
                let Some(id) = current else {
                    break MembershipPolicy::Ham;
                };
                if !visited.insert(id) {
                    break MembershipPolicy::Ham;
                }
                let Some(ancestor) = by_id.get(&id) else {
                    break MembershipPolicy::Ham;
                };
                if ancestor.attrs.special_use.is_some() {
                    break special_use_policy(ancestor.attrs.special_use.as_deref());
                }
                current = ancestor.parent;
            };
            (mailbox.id, policy)
        })
        .collect()
}

fn label_for_memberships(memberships: impl IntoIterator<Item = MembershipPolicy>) -> Option<Label> {
    let mut saw_ham = false;
    for membership in memberships {
        match membership {
            MembershipPolicy::Spam => return Some(Label::Spam),
            MembershipPolicy::Ham => saw_ham = true,
            MembershipPolicy::Ignored => {}
        }
    }
    saw_ham.then_some(Label::Ham)
}

fn prepare_corpus(mailstore: &SqliteMailStore, account: i32) -> anyhow::Result<PreparedCorpus> {
    let mailboxes = mailstore.list_mailboxes(account)?;
    let policies = mailbox_policy_map(&mailboxes);
    let mut by_item: HashMap<ItemId, RebuildCandidate> = HashMap::new();
    let mut ignored_mailboxes = Vec::new();
    let mut conflicts = 0;

    for mailbox in mailboxes {
        let policy = policies
            .get(&mailbox.id)
            .copied()
            .unwrap_or(MembershipPolicy::Ham);
        let label = match label_for_memberships([policy]) {
            Some(label) => label,
            None => {
                ignored_mailboxes.push(mailbox.name);
                continue;
            }
        };
        for handle in mailstore.list_emails_in_mailbox(account, mailbox.id, ListOpts::default())? {
            let entry = by_item.entry(handle.id).or_insert(RebuildCandidate {
                id: handle.id,
                blob_hash: handle.blob_hash,
                label,
                seen_ham: false,
                seen_spam: false,
                conflict_counted: false,
            });
            match label {
                Label::Ham => entry.seen_ham = true,
                Label::Spam => {
                    entry.seen_spam = true;
                }
            }
            entry.label = label_for_memberships(
                [
                    entry.seen_ham.then_some(MembershipPolicy::Ham),
                    entry.seen_spam.then_some(MembershipPolicy::Spam),
                ]
                .into_iter()
                .flatten(),
            )
            .expect("candidate has at least one trainable membership");
            if entry.seen_ham && entry.seen_spam && !entry.conflict_counted {
                conflicts += 1;
                entry.conflict_counted = true;
            }
        }
    }

    let ham_candidates = by_item
        .values()
        .filter(|candidate| candidate.label == Label::Ham)
        .count() as u64;
    let spam_candidates = by_item.len() as u64 - ham_candidates;
    let mut candidates: Vec<_> = by_item.into_values().collect();
    candidates.sort_by_key(|candidate| candidate.id.0);
    ignored_mailboxes.sort();

    Ok(PreparedCorpus {
        candidates,
        ham_candidates,
        spam_candidates,
        conflicts,
        ignored_mailboxes,
    })
}

async fn run_rebuild(
    classifier: Arc<DefaultClassifier>,
    mailstore: Arc<SqliteMailStore>,
    state: BayesianBusState,
    account_i32: i32,
    account: AccountId,
    options: RebuildOptions,
) {
    let account_key = account.as_str().to_string();
    // If this task unwinds (panic) or is cancelled before reaching a
    // terminal state, the guard marks the job `failed` so the account is
    // not wedged at `running` until the daemon restarts.
    let mut guard = RunningGuard {
        state: state.clone(),
        account_key: account_key.clone(),
        shadow_path: None,
        rebuild_lock: None,
    };
    let live = match classifier.open_account_connection(&account).await {
        Ok(conn) => conn,
        Err(e) => {
            finish_failed(
                &state,
                &account_key,
                format!("open live Bayesian corpus: {e}"),
            );
            return;
        }
    };
    let Some(live_path) = live.database_path() else {
        finish_failed(
            &state,
            &account_key,
            "Bayesian rebuild requires persistent SQLite storage".to_string(),
        );
        return;
    };
    let account_dir = live_path.parent().unwrap_or_else(|| Path::new("."));
    let lock_path = account_dir.join("rebuild.lock");
    match try_rebuild_lock(&lock_path) {
        Ok(Some(file)) => guard.rebuild_lock = Some(file),
        Ok(None) => {
            finish_failed(
                &state,
                &account_key,
                "rebuild already running for this account (another process holds the lock)"
                    .to_string(),
            );
            return;
        }
        Err(e) => {
            finish_failed(
                &state,
                &account_key,
                format!("open or lock {}: {e}", lock_path.display()),
            );
            return;
        }
    }
    if let Err(e) = remove_stale_shadow_files(account_dir) {
        finish_failed(
            &state,
            &account_key,
            format!("remove stale shadow corpus: {e}"),
        );
        return;
    }
    let shadow_path = account_dir.join(format!(
        "bayes.rebuild-{}-{}.db",
        std::process::id(),
        unix_millis()
    ));
    guard.shadow_path = Some(shadow_path.clone());
    let shadow_open_path = shadow_path.clone();
    let shadow = match tokio::task::spawn_blocking(move || {
        SqliteAccountConnection::open_path(&shadow_open_path, 0)
    })
    .await
    {
        Ok(Ok(conn)) => Arc::new(conn),
        Ok(Err(e)) => {
            finish_failed(&state, &account_key, format!("open shadow corpus: {e}"));
            let _ = remove_shadow_files(&shadow_path);
            return;
        }
        Err(e) => {
            finish_failed(
                &state,
                &account_key,
                format!("open shadow corpus task: {e}"),
            );
            let _ = remove_shadow_files(&shadow_path);
            return;
        }
    };
    options.run_shadow_opened(&shadow_path);

    let result = rebuild_shadow(
        &classifier,
        &mailstore,
        &state,
        account_i32,
        &account,
        live.as_ref(),
        shadow.as_ref(),
        &live_path,
        &shadow_path,
        &options,
    )
    .await;
    drop(shadow);
    let cleanup = remove_shadow_files(&shadow_path);

    match result {
        Ok(RebuildCompletion::Done) => {
            if let Err(e) = cleanup {
                record_job_error(&state, &account_key, format!("delete shadow corpus: {e}"));
            }
            finish_job(&state, &account_key, "done");
        }
        Ok(RebuildCompletion::Aborted) => {
            if let Err(e) = cleanup {
                record_job_error(&state, &account_key, format!("delete shadow corpus: {e}"));
            }
            finish_job(&state, &account_key, "aborted");
        }
        Err(e) => {
            let mut message = e.to_string();
            if let Err(cleanup_error) = cleanup {
                message.push_str(&format!("; delete shadow corpus: {cleanup_error}"));
            }
            finish_failed(&state, &account_key, message);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn rebuild_shadow(
    classifier: &DefaultClassifier,
    mailstore: &Arc<SqliteMailStore>,
    state: &BayesianBusState,
    account_i32: i32,
    account: &AccountId,
    live: &dyn AccountConnection,
    shadow: &dyn AccountConnection,
    live_path: &Path,
    shadow_path: &Path,
    options: &RebuildOptions,
) -> anyhow::Result<RebuildCompletion> {
    let account_key = account.as_str();
    let prepared = {
        let mailstore = Arc::clone(mailstore);
        tokio::task::spawn_blocking(move || prepare_corpus(&mailstore, account_i32))
            .await
            .map_err(|e| anyhow::anyhow!("enumerate rebuild corpus task: {e}"))??
    };
    update_job(state, account_key, |job| {
        job.ham_candidates = prepared.ham_candidates;
        job.spam_candidates = prepared.spam_candidates;
        job.conflicts = prepared.conflicts;
        job.ignored_mailboxes = prepared.ignored_mailboxes.clone();
    });
    let started_at = current_job(state, account_key)
        .map(|job| job.started_at)
        .ok_or_else(|| anyhow::anyhow!("rebuild job disappeared"))?;
    options.run_after_enumeration(live_path, started_at);

    let mut consecutive_errors = 0;
    let mut trained_labels = HashMap::<String, ShadowEntry>::new();

    for candidate in prepared.candidates {
        let bytes = read_candidate(mailstore, candidate.blob_hash, options.fail_reads()).await;
        let message = match bytes {
            Ok(Ok(message)) => message,
            Ok(Err(e)) if is_missing_blob(&e) => {
                consecutive_errors = 0;
                update_job(state, account_key, |job| job.skipped_missing += 1);
                continue;
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                record_job_error(state, account_key, format!("read blob: {e}"));
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Ok(RebuildCompletion::Aborted);
                }
                continue;
            }
            Err(e) => {
                consecutive_errors += 1;
                record_job_error(state, account_key, format!("read blob task: {e}"));
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Ok(RebuildCompletion::Aborted);
                }
                continue;
            }
        };

        let stamp_id = candidate.id.0.to_string();
        let outcome = classifier
            .train_into(
                shadow,
                &RetrainRequest {
                    stamp_id: &stamp_id,
                    account,
                    message: &message,
                    label: candidate.label,
                },
            )
            .await;
        match outcome {
            Ok(RetrainOutcome::Applied) => {
                consecutive_errors = 0;
                update_job(state, account_key, |job| {
                    set_shadow_entry(
                        job,
                        &mut trained_labels,
                        stamp_id,
                        ShadowEntry {
                            label: candidate.label,
                            blob_hash: candidate.blob_hash,
                        },
                    );
                });
            }
            Ok(RetrainOutcome::AlreadyLabeled) => {
                consecutive_errors = 0;
                update_job(state, account_key, |job| job.already_labeled += 1);
            }
            Ok(RetrainOutcome::NoStamp) => {
                consecutive_errors += 1;
                record_job_error(state, account_key, "retrain returned NoStamp".to_string());
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Ok(RebuildCompletion::Aborted);
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                record_job_error(state, account_key, format!("retrain: {e}"));
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Ok(RebuildCompletion::Aborted);
                }
            }
        }
    }

    // Refused empty rebuilds must not create a snapshot or prune a prior
    // rollback copy. Replay can still change the totals, so validate again
    // below and after every conflict.
    validate_shadow_totals(shadow, state, account_key, options.allow_empty).await?;

    // This is a rollback copy of the pre-rebuild live corpus. Take it before
    // the first replay read; retries update only the shadow. Retention pruning
    // happens only after the live swap commits.
    let snapshot = if options.snapshot {
        let snapshot = live
            .snapshot()
            .await
            .map_err(|e| anyhow::anyhow!("snapshot live Bayesian corpus: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("persistent corpus returned no snapshot path"))?;
        update_job(state, account_key, |job| {
            job.snapshot = Some(snapshot.display().to_string());
        });
        Some(snapshot)
    } else {
        None
    };

    let mut corrections = live
        .labels_since(started_at)
        .await
        .map_err(|e| anyhow::anyhow!("read live corrections: {e}"))?;
    let mut replayed_stamps = HashSet::new();
    replay_corrections(
        classifier,
        mailstore,
        state,
        account_i32,
        account,
        shadow,
        &corrections,
        &mut trained_labels,
        &mut replayed_stamps,
    )
    .await?;
    validate_shadow_totals(shadow, state, account_key, options.allow_empty).await?;
    options.run_after_replay(live_path, started_at);

    for attempt in 0..MAX_SWAP_ATTEMPTS {
        match live
            .replace_from(shadow_path, started_at, &corrections)
            .await
            .map_err(|e| anyhow::anyhow!("replace live Bayesian corpus: {e}"))?
        {
            SwapOutcome::Swapped => {
                if let Some(snapshot) = snapshot.as_deref()
                    && let Err(error) = live.prune_snapshots(snapshot).await
                {
                    record_job_error(
                        state,
                        account_key,
                        format!("prune Bayesian snapshots after swap: {error}"),
                    );
                }
                return Ok(RebuildCompletion::Done);
            }
            SwapOutcome::Conflict(current) => {
                if attempt + 1 == MAX_SWAP_ATTEMPTS {
                    return Err(anyhow::anyhow!("live corrections kept landing during swap"));
                }
                replay_corrections(
                    classifier,
                    mailstore,
                    state,
                    account_i32,
                    account,
                    shadow,
                    &current,
                    &mut trained_labels,
                    &mut replayed_stamps,
                )
                .await?;
                validate_shadow_totals(shadow, state, account_key, options.allow_empty).await?;
                corrections = current;
            }
        }
    }
    unreachable!("swap loop returns on success or its final conflict")
}

#[allow(clippy::too_many_arguments)]
async fn replay_corrections(
    classifier: &DefaultClassifier,
    mailstore: &Arc<SqliteMailStore>,
    state: &BayesianBusState,
    account_i32: i32,
    account: &AccountId,
    shadow: &dyn AccountConnection,
    corrections: &[(String, Label)],
    trained_labels: &mut HashMap<String, ShadowEntry>,
    replayed_stamps: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let account_key = account.as_str();
    let policy = {
        let store = Arc::clone(mailstore);
        tokio::task::spawn_blocking(move || current_membership_policy(&store, account_i32))
            .await
            .map_err(|e| anyhow::anyhow!("read replay mailbox policy task: {e}"))??
    };
    let policy = Arc::new(policy);
    let set = account_id_to_setid(account_i32);
    let mut seen_in_batch = HashSet::new();

    for (stamp, _recorded_label) in corrections {
        let item = match uuid::Uuid::parse_str(stamp) {
            Ok(uuid) => ItemId(uuid),
            Err(_) => {
                update_job(state, account_key, |job| job.skipped_missing += 1);
                continue;
            }
        };
        let canonical_stamp = item.0.to_string();
        if !seen_in_batch.insert(canonical_stamp.clone()) {
            continue;
        }
        let prior = trained_labels.get(&canonical_stamp).copied();
        let prior_blob = prior.map(|entry| entry.blob_hash);
        let store = Arc::clone(mailstore);
        let policy = Arc::clone(&policy);
        let resolution = tokio::task::spawn_blocking(move || {
            let memberships = store.mds().item_memberships(&set, &item)?;
            let derived =
                label_for_memberships(memberships.into_iter().map(|(container, _, _)| {
                    policy
                        .get(&container)
                        .copied()
                        .unwrap_or(MembershipPolicy::Ignored)
                }));
            let blob_hash = match derived {
                Some(_) => Some(store.mds().fetch_item_meta(&set, &item)?.blob_hash),
                None => prior_blob,
            };
            let message = match blob_hash {
                Some(hash) => Some((hash, store.read_blob(hash)?)),
                None => None,
            };
            Ok::<_, anyhow::Error>((derived, message))
        })
        .await
        .map_err(|e| anyhow::anyhow!("resolve replay item task: {e}"))?;
        let (derived, message) = match resolution {
            Ok(resolved) => resolved,
            Err(e) if is_missing_blob(&e) && prior.is_none() => {
                update_job(state, account_key, |job| job.skipped_missing += 1);
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("resolve replay item: {e}")),
        };

        match (derived, message, prior) {
            (Some(label), Some((blob_hash, message)), _) => {
                let outcome = classifier
                    .train_into(
                        shadow,
                        &RetrainRequest {
                            stamp_id: &canonical_stamp,
                            account,
                            message: &message,
                            label,
                        },
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("replay correction: {e}"))?;
                let first_replay = replayed_stamps.insert(canonical_stamp.clone());
                update_job(state, account_key, |job| {
                    if first_replay {
                        job.replayed += 1;
                    }
                    if outcome == RetrainOutcome::AlreadyLabeled {
                        job.already_labeled += 1;
                    }
                    set_shadow_entry(
                        job,
                        trained_labels,
                        canonical_stamp,
                        ShadowEntry { label, blob_hash },
                    );
                });
            }
            (None, Some((_, message)), Some(_)) => {
                let forgotten = classifier
                    .forget_from(shadow, &canonical_stamp, &message)
                    .await
                    .map_err(|e| anyhow::anyhow!("forget ignored replay item: {e}"))?;
                if forgotten.is_none() {
                    return Err(anyhow::anyhow!(
                        "shadow lost label for ignored replay item {canonical_stamp}"
                    ));
                }
                let first_replay = replayed_stamps.insert(canonical_stamp.clone());
                update_job(state, account_key, |job| {
                    if first_replay {
                        job.replayed += 1;
                    }
                    remove_shadow_entry(job, trained_labels, &canonical_stamp);
                });
            }
            (None, None, None) => {
                if replayed_stamps.insert(canonical_stamp) {
                    update_job(state, account_key, |job| job.replayed += 1);
                }
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "replay resolution returned an inconsistent message state"
                ));
            }
        }
    }
    Ok(())
}

fn current_membership_policy(
    mailstore: &SqliteMailStore,
    account: i32,
) -> anyhow::Result<HashMap<ContainerId, MembershipPolicy>> {
    let mailboxes = mailstore.list_mailboxes(account)?;
    Ok(mailbox_policy_map(&mailboxes))
}

fn set_shadow_entry(
    job: &mut RebuildJob,
    entries: &mut HashMap<String, ShadowEntry>,
    stamp: String,
    next: ShadowEntry,
) {
    let prior = entries.insert(stamp, next);
    if prior.is_some_and(|entry| entry.label == next.label) {
        return;
    }
    if let Some(prior) = prior {
        decrement_trained(job, prior.label);
    }
    increment_trained(job, next.label);
}

fn remove_shadow_entry(
    job: &mut RebuildJob,
    entries: &mut HashMap<String, ShadowEntry>,
    stamp: &str,
) {
    if let Some(prior) = entries.remove(stamp) {
        decrement_trained(job, prior.label);
    }
}

fn increment_trained(job: &mut RebuildJob, label: Label) {
    match label {
        Label::Ham => job.ham_trained += 1,
        Label::Spam => job.spam_trained += 1,
    }
}

fn decrement_trained(job: &mut RebuildJob, label: Label) {
    match label {
        Label::Ham => job.ham_trained -= 1,
        Label::Spam => job.spam_trained -= 1,
    }
}

async fn validate_shadow_totals(
    shadow: &dyn AccountConnection,
    state: &BayesianBusState,
    account_key: &str,
    allow_empty: bool,
) -> anyhow::Result<()> {
    let totals = shadow
        .totals()
        .await
        .map_err(|e| anyhow::anyhow!("validate shadow totals: {e}"))?;
    let job = current_job(state, account_key)
        .ok_or_else(|| anyhow::anyhow!("rebuild job disappeared"))?;
    if totals != (job.ham_trained, job.spam_trained) {
        return Err(anyhow::anyhow!(
            "shadow totals mismatch: database has ({}, {}), job counted ({}, {})",
            totals.0,
            totals.1,
            job.ham_trained,
            job.spam_trained
        ));
    }
    if totals == (0, 0) && !allow_empty {
        return Err(anyhow::anyhow!(
            "rebuild produced an empty corpus; live corpus left untouched (pass allow_empty: true to replace it anyway)"
        ));
    }
    Ok(())
}

async fn read_candidate(
    mailstore: &Arc<SqliteMailStore>,
    hash: BlobHash,
    fail: bool,
) -> Result<anyhow::Result<Vec<u8>>, tokio::task::JoinError> {
    let mailstore = Arc::clone(mailstore);
    tokio::task::spawn_blocking(move || {
        if fail {
            anyhow::bail!("synthetic rebuild read failure");
        }
        mailstore.read_blob(hash)
    })
    .await
}

fn try_rebuild_lock(path: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: flock receives a live file descriptor and no pointer arguments.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

fn remove_stale_shadow_files(account_dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(account_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_rebuild_shadow_name(name) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn is_rebuild_shadow_name(name: &str) -> bool {
    [".db", ".db-wal", ".db-shm"].into_iter().any(|suffix| {
        name.strip_prefix("bayes.rebuild-")
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(|identity| !identity.is_empty())
    })
}

fn remove_shadow_files(path: &Path) -> std::io::Result<()> {
    for candidate in [
        path.to_path_buf(),
        sqlite_sidecar(path, "-wal"),
        sqlite_sidecar(path, "-shm"),
    ] {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn is_missing_blob(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<cosmix_mds::Error>(),
        Some(cosmix_mds::Error::BlobNotFound(_) | cosmix_mds::Error::ItemNotFound(_))
    )
}

fn update_job(state: &BayesianBusState, account_key: &str, update: impl FnOnce(&mut RebuildJob)) {
    let mut jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(job) = jobs.get_mut(account_key) {
        update(job);
    }
}

fn record_job_error(state: &BayesianBusState, account_key: &str, error: String) {
    update_job(state, account_key, |job| {
        job.errors += 1;
        job.last_error = Some(error);
    });
}

fn finish_failed(state: &BayesianBusState, account_key: &str, error: String) {
    update_job(state, account_key, |job| {
        job.state = "failed".to_string();
        job.finished_at = Some(unix_secs());
        job.errors += 1;
        job.last_error = Some(error.clone());
    });
    tracing::info!(
        target: "maild::bayesian",
        account_id = %account_key,
        state = "failed",
        error = %error,
        "Bayesian corpus rebuild finished"
    );
}

fn finish_job(state: &BayesianBusState, account_key: &str, terminal_state: &str) {
    update_job(state, account_key, |job| {
        job.state = terminal_state.to_string();
        job.finished_at = Some(unix_secs());
    });
    if let Some(job) = current_job(state, account_key) {
        tracing::info!(
            target: "maild::bayesian",
            account_id = %account_key,
            state = %job.state,
            ham_trained = job.ham_trained,
            spam_trained = job.spam_trained,
            replayed = job.replayed,
            errors = job.errors,
            "Bayesian corpus rebuild finished"
        );
    }
}

fn current_job(state: &BayesianBusState, account_key: &str) -> Option<RebuildJob> {
    state
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(account_key)
        .cloned()
}

fn current_job_response(
    state: &BayesianBusState,
    account_key: &str,
    error_on_terminal_failure: bool,
) -> (u8, String) {
    let jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
    match jobs.get(account_key) {
        Some(job) => serialize_job(job, error_on_terminal_failure),
        None => (RC_ERROR, err_body("rebuild job disappeared")),
    }
}

fn serialize_job(job: &RebuildJob, error_on_terminal_failure: bool) -> (u8, String) {
    match serde_json::to_string(job) {
        Ok(body) => {
            let rc = if error_on_terminal_failure
                && matches!(job.state.as_str(), "aborted" | "failed")
            {
                RC_ERROR
            } else {
                0
            };
            (rc, body)
        }
        Err(e) => (RC_ERROR, err_body(&format!("serialize rebuild job: {e}"))),
    }
}

fn default_true() -> bool {
    true
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

/// Validate and canonicalise the wire `account_id`. The on-disk Bayesian
/// layout is `<base>/<id>/bayes.db` and `open_account` creates the
/// `<id>` directory; an arbitrary string would let a mesh peer write
/// outside the corpus tree (e.g. `"../../etc"`). Constrain to a
/// non-negative integer in either JSON-number or all-ASCII-digit
/// string form — that matches the SMTP-side `account.id.to_string()`
/// path used elsewhere.
fn parse_account_id(v: &serde_json::Value) -> Result<i32, String> {
    let raw = match v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| format!("account_id must be a non-negative integer, got {n}"))?,
        serde_json::Value::String(s) => {
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                s.parse::<u64>()
                    .map_err(|_| format!("account_id is outside maild's integer range: {s:?}"))?
            } else {
                return Err(format!(
                    "account_id must be a non-negative integer, got {s:?}"
                ));
            }
        }
        other => {
            return Err(format!(
                "account_id must be an integer or digit-string, got {other}"
            ));
        }
    };
    i32::try_from(raw)
        .map_err(|_| "account_id is outside maild's signed 32-bit account range".to_string())
}

fn err_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_maild_bayesian::{
        ClassifierConfig, Label, Result as BayResult,
        storage::{AccountConnection, SqliteAccountConnection, SqliteBackend, StorageBackend},
    };
    use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SqliteCasMds};
    use rusqlite::Connection;
    use std::path::Path;
    use tempfile::TempDir;

    /// Backend that hands back a single in-memory connection regardless
    /// of which account is requested. Hermetic — never writes to disk.
    struct SingleConnBackend(Arc<SqliteAccountConnection>);

    #[async_trait::async_trait]
    impl StorageBackend for SingleConnBackend {
        async fn open_account(
            &self,
            _account: &AccountId,
        ) -> BayResult<Arc<dyn AccountConnection>> {
            Ok(self.0.clone() as Arc<dyn AccountConnection>)
        }
    }

    async fn classifier_with_corpus(spam: u32, ham: u32) -> Arc<DefaultClassifier> {
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap();
        let conn = Arc::new(conn);
        for i in 0..ham {
            conn.record_label(
                &format!("h-{i}"),
                &["b:meeting".into(), "b:agenda".into()],
                Label::Ham,
                0,
            )
            .await
            .unwrap();
        }
        for i in 0..spam {
            conn.record_label(
                &format!("s-{i}"),
                &["b:viagra".into(), "b:discount".into()],
                Label::Spam,
                0,
            )
            .await
            .unwrap();
        }
        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConnBackend(conn));
        // Tests pin cold_start_floor + spam_threshold deterministically.
        let cfg = ClassifierConfig {
            cold_start_floor: 0,
            spam_threshold: 0.5,
            ..ClassifierConfig::default()
        };
        Arc::new(DefaultClassifier::new(cfg, backend))
    }

    fn empty_classifier() -> Arc<DefaultClassifier> {
        // Default cold_start_floor (100) so an untrained account reads as cold_start=true.
        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 100).unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConnBackend(Arc::new(conn)));
        Arc::new(DefaultClassifier::new(ClassifierConfig::default(), backend))
    }

    fn temp_mailstore() -> (TempDir, Arc<SqliteCasMds>, Arc<SqliteMailStore>) {
        let dir = TempDir::new().unwrap();
        let mds_root = dir.path().join("mds");
        std::fs::create_dir(&mds_root).unwrap();
        let mds = Arc::new(SqliteCasMds::open(&mds_root).unwrap());
        let store = Arc::new(SqliteMailStore::new(Arc::clone(&mds)));
        (dir, mds, store)
    }

    fn disk_classifier(base: &Path) -> Arc<DefaultClassifier> {
        let backend: Arc<dyn StorageBackend> = Arc::new(SqliteBackend::new(base, None, 0));
        Arc::new(DefaultClassifier::new(ClassifierConfig::default(), backend))
    }

    fn database_with_accounts(ids: &[i32]) -> db::Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        for id in ids {
            conn.execute(
                "INSERT INTO accounts (id, email, password) VALUES (?1, ?2, 'x')",
                rusqlite::params![id, format!("account-{id}@example.com")],
            )
            .unwrap();
        }
        db::Db {
            conn: Arc::new(Mutex::new(conn)),
            blob_dir: std::env::temp_dir(),
        }
    }

    fn create_mailbox(
        mds: &SqliteCasMds,
        set: &cosmix_mds::SetId,
        name: &str,
        special_use: Option<&str>,
    ) -> cosmix_mds::ContainerId {
        mds.create_container(
            set,
            None,
            name,
            ContainerAttrs {
                special_use: special_use.map(str::to_string),
                subscribed: false,
                extra: serde_json::json!({}),
            },
        )
        .unwrap()
    }

    fn create_child_mailbox(
        mds: &SqliteCasMds,
        set: &cosmix_mds::SetId,
        parent: cosmix_mds::ContainerId,
        name: &str,
        special_use: Option<&str>,
    ) -> cosmix_mds::ContainerId {
        mds.create_container(
            set,
            Some(&parent),
            name,
            ContainerAttrs {
                special_use: special_use.map(str::to_string),
                subscribed: false,
                extra: serde_json::json!({}),
            },
        )
        .unwrap()
    }

    fn add_message(
        mds: &SqliteCasMds,
        set: &cosmix_mds::SetId,
        mailbox: cosmix_mds::ContainerId,
        message: &[u8],
    ) -> ItemId {
        let hash = mds.put_blob(message).unwrap();
        mds.add_item(
            set,
            &hash,
            &[Membership {
                container: mailbox,
                flags: Flags(0),
                added_at: 0,
            }],
        )
        .unwrap()
        .item_id
    }

    fn cmd_with_args(parsed: serde_json::Value) -> IncomingCommand {
        IncomingCommand {
            from: "test".into(),
            command: "maild.bayesian.x".into(),
            id: None,
            args: parsed.clone(),
            body: parsed.to_string(),
            headers: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn label_serializes_as_spam_or_ham_string() {
        // The plan pins this: Label must serialize as "Spam" / "Ham"
        // so the Bus wire matches the spec prose. Default serde
        // unit-variant repr produces those strings — this test
        // catches a future #[serde(rename_all = ...)] regression.
        let s = serde_json::to_string(&Label::Spam).unwrap();
        assert_eq!(s, "\"Spam\"");
        let h = serde_json::to_string(&Label::Ham).unwrap();
        assert_eq!(h, "\"Ham\"");
    }

    #[tokio::test]
    async fn stats_for_unknown_account_returns_cold_start_zero_values() {
        let cls = empty_classifier();
        let args = serde_json::json!({"account_id": 99});
        let (rc, body) = handle_stats(&cls, &args).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["spam_messages"], 0);
        assert_eq!(v["ham_messages"], 0);
        assert_eq!(v["cold_start"], true);
    }

    #[tokio::test]
    async fn stats_with_trained_corpus_reports_counts() {
        let cls = classifier_with_corpus(3, 5).await;
        let args = serde_json::json!({"account_id": "42"});
        let (rc, body) = handle_stats(&cls, &args).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["spam_messages"], 3);
        assert_eq!(v["ham_messages"], 5);
        assert_eq!(v["cold_start"], false);
    }

    #[tokio::test]
    async fn stats_rejects_non_integer_account_id() {
        // A peer must not be able to steer `<base>/<id>/bayes.db` out
        // of the corpus tree by passing a path-shaped account id.
        let cls = empty_classifier();
        for bad in [
            serde_json::json!({"account_id": "../../etc"}),
            serde_json::json!({"account_id": "alice"}),
            serde_json::json!({"account_id": ""}),
            serde_json::json!({"account_id": -1}),
            serde_json::json!({"account_id": 1.5}),
        ] {
            let (rc, body) = handle_stats(&cls, &bad).await;
            assert_eq!(rc, RC_ERROR, "expected error for {bad}, body was: {body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                v["error"].as_str().unwrap().contains("account_id"),
                "expected account_id error for {bad}, got {body}",
            );
        }
    }

    #[tokio::test]
    async fn stats_rejects_missing_account_id() {
        let cls = empty_classifier();
        let args = serde_json::json!({});
        let (rc, body) = handle_stats(&cls, &args).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("malformed"));
    }

    #[tokio::test]
    async fn classify_returns_bayesian_verdict_shape_with_contributions() {
        let cls = classifier_with_corpus(10, 10).await;
        let raw = b"From: x@evil\r\nSubject: BUY\r\n\r\nviagra discount\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let args = serde_json::json!({
            "account_id": 7,
            "message_b64": b64,
        });
        let (rc, body) = handle_classify(&cls, &args).await;
        assert_eq!(rc, 0, "body was: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // Shape contract from the spec.
        assert!(v["label"].is_string());
        assert!(v["score"].is_number());
        assert!(v["threshold"].is_number());
        assert!(v["cold_start"].is_boolean());
        // Plan §Phase 3: wire field is `contributions`, not `top_tokens`.
        assert!(v["contributions"].is_array());
    }

    #[tokio::test]
    async fn classify_rejects_bad_base64() {
        let cls = empty_classifier();
        let args = serde_json::json!({
            "account_id": 7,
            "message_b64": "***not base64***",
        });
        let (rc, body) = handle_classify(&cls, &args).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("decode"));
    }

    #[tokio::test]
    async fn dispatch_unknown_action_returns_rc_error() {
        let cls = empty_classifier();
        let (_dir, _mds, store) = temp_mailstore();
        let database = database_with_accounts(&[]);
        let state = BayesianBusState::default();
        let cmd = cmd_with_args(serde_json::json!({}));
        let (rc, body) = dispatch("bogus", &cmd, &cls, &database, &store, &state).await;
        assert_eq!(rc, RC_ERROR);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("unknown bayesian action")
        );
    }

    #[tokio::test]
    async fn dispatch_stats_resolves_args_from_args_header() {
        let cls = classifier_with_corpus(2, 3).await;
        let (_dir, _mds, store) = temp_mailstore();
        let database = database_with_accounts(&[]);
        let state = BayesianBusState::default();
        let body_json = serde_json::json!({}).to_string();
        let header_args = serde_json::json!({"account_id": "42"}).to_string();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("args".to_string(), header_args);
        let cmd = IncomingCommand {
            from: "test".into(),
            command: "maild.bayesian.stats".into(),
            id: None,
            args: serde_json::Value::Null,
            body: body_json,
            headers,
        };
        let (rc, body) = dispatch("stats", &cmd, &cls, &database, &store, &state).await;
        assert_eq!(rc, 0, "body was: {body}");
    }

    #[tokio::test]
    async fn rebuild_operator_allowlist_is_opt_in() {
        let (dir, _mds, store) = temp_mailstore();
        store.ensure_account_set(1).unwrap();
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
            "allow_empty": true,
        });
        let command = IncomingCommand {
            from: "peer-any".to_string(),
            command: "maild.bayesian.rebuild".to_string(),
            id: None,
            args: args.clone(),
            body: args.to_string(),
            headers: std::collections::BTreeMap::new(),
        };

        let open_state = BayesianBusState::default();
        let (rc, body) = dispatch("rebuild", &command, &cls, &database, &store, &open_state).await;
        assert_eq!(rc, 0, "empty allowlist refused peer: {body}");

        let restricted_state = BayesianBusState::new(vec!["operator-one".to_string()]);
        let (rc, body) = dispatch(
            "rebuild",
            &command,
            &cls,
            &database,
            &store,
            &restricted_state,
        )
        .await;
        assert_eq!(rc, RC_ERROR);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "not an authorised rebuild operator"
        );
    }

    #[tokio::test]
    async fn rebuild_from_folder_state_is_idempotent_and_snapshots() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = create_mailbox(&mds, &set, "Junk", Some("\\Junk"));
        let _sent = create_mailbox(&mds, &set, "Sent", Some("\\Sent"));
        let _trash = create_mailbox(&mds, &set, "Trash", Some("\\Trash"));
        let _drafts = create_mailbox(&mds, &set, "Drafts", Some("\\Drafts"));
        let projects = create_mailbox(&mds, &set, "Projects", None);
        let inbox_child = mds
            .create_container(
                &set,
                Some(&inbox),
                "Receipts",
                ContainerAttrs {
                    special_use: None,
                    subscribed: false,
                    extra: serde_json::json!({}),
                },
            )
            .unwrap();
        add_message(
            &mds,
            &set,
            inbox,
            b"From: friend1@example.com\r\nSubject: Meeting one\r\n\r\nAgenda one\r\n",
        );
        add_message(
            &mds,
            &set,
            inbox,
            b"From: friend2@example.com\r\nSubject: Meeting two\r\n\r\nAgenda two\r\n",
        );
        add_message(
            &mds,
            &set,
            junk,
            b"From: offer@example.com\r\nSubject: Discount\r\n\r\nBuy now\r\n",
        );
        add_message(
            &mds,
            &set,
            projects,
            b"From: colleague@example.com\r\nSubject: Project\r\n\r\nNotes\r\n",
        );
        add_message(
            &mds,
            &set,
            inbox_child,
            b"From: shop@example.com\r\nSubject: Receipt\r\n\r\nPaid\r\n",
        );

        let bayes_dir = dir.path().join("bayes");
        let account_dir = bayes_dir.join("1");
        std::fs::create_dir_all(&account_dir).unwrap();
        for sequence in [100_u64, 200, 300] {
            std::fs::write(
                account_dir.join(format!("bayes.pre-rebuild-{sequence}.db")),
                b"older rollback copy",
            )
            .unwrap();
        }
        let cls = disk_classifier(&bayes_dir);
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": "0001",
            "snapshot": true,
            "wait": true,
        });
        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;
        assert_eq!(rc, 0, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "done");
        assert_eq!(job["ham_candidates"], 4);
        assert_eq!(job["spam_candidates"], 1);
        assert_eq!(job["ham_trained"], 4);
        assert_eq!(job["spam_trained"], 1);
        assert_eq!(job["errors"], 0);
        assert_eq!(
            job["ignored_mailboxes"],
            serde_json::json!(["Drafts", "Sent", "Trash"])
        );
        let snapshot = job["snapshot"].as_str().unwrap();
        assert!(Path::new(snapshot).exists(), "snapshot missing: {snapshot}");
        let retained_snapshots = std::fs::read_dir(&account_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bayes.pre-rebuild-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(retained_snapshots.len(), 2);
        assert!(retained_snapshots.contains(&PathBuf::from(snapshot)));
        assert!(retained_snapshots.contains(&account_dir.join("bayes.pre-rebuild-300.db")));

        let account = AccountId::new("1".to_string());
        let stats = cls.stats(&account).await.unwrap();
        assert_eq!((stats.ham_messages, stats.spam_messages), (4, 1));
        let raw = Connection::open(bayes_dir.join("1/bayes.db")).unwrap();
        assert!(!bayes_dir.join("0001").exists());
        let priority_rows: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM labels l
                 JOIN label_meta lm ON lm.stamp_id = l.stamp_id
                 WHERE lm.cap_mode = 'priority'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(priority_rows, 5);
        drop(raw);

        let second_args = serde_json::json!({
            "account_id": "1",
            "snapshot": false,
            "wait": true,
        });
        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &second_args).await;
        assert_eq!(rc, 0, "body was: {body}");
        let second: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(second["state"], "done");
        assert_eq!(second["ham_trained"], 4);
        assert_eq!(second["spam_trained"], 1);
        let stats = cls.stats(&account).await.unwrap();
        assert_eq!((stats.ham_messages, stats.spam_messages), (4, 1));
    }

    #[test]
    fn prepare_corpus_makes_junk_win_conflicts() {
        let (_dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = create_mailbox(&mds, &set, "Junk", Some("\\Junk"));
        let hash = mds.put_blob(b"conflict").unwrap();
        mds.add_item(
            &set,
            &hash,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: junk,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap();

        let prepared = prepare_corpus(&store, 1).unwrap();
        assert_eq!(prepared.ham_candidates, 0);
        assert_eq!(prepared.spam_candidates, 1);
        assert_eq!(prepared.conflicts, 1);
        assert_eq!(prepared.candidates[0].label, Label::Spam);
    }

    #[test]
    fn prepare_corpus_inherits_nearest_special_use_ancestor() {
        let (_dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = create_mailbox(&mds, &set, "Junk", Some("\\Junk"));
        let trash = create_mailbox(&mds, &set, "Trash", Some("\\Trash"));
        let user = create_mailbox(&mds, &set, "Projects", None);
        let inbox_sub = create_child_mailbox(&mds, &set, inbox, "Inbox Sub", None);
        let junk_sub = create_child_mailbox(&mds, &set, junk, "Junk Sub", None);
        let trash_sub = create_child_mailbox(&mds, &set, trash, "Trash Sub", None);

        let inbox_item = add_message(&mds, &set, inbox_sub, b"inbox child");
        let junk_item = add_message(&mds, &set, junk_sub, b"junk child");
        let user_item = add_message(&mds, &set, user, b"user folder");
        add_message(&mds, &set, trash_sub, b"ignored trash child");

        let prepared = prepare_corpus(&store, 1).unwrap();
        let labels: HashMap<ItemId, Label> = prepared
            .candidates
            .iter()
            .map(|candidate| (candidate.id, candidate.label))
            .collect();
        assert_eq!(labels.get(&junk_item), Some(&Label::Spam));
        assert_eq!(labels.get(&inbox_item), Some(&Label::Ham));
        assert_eq!(labels.get(&user_item), Some(&Label::Ham));
        assert_eq!(prepared.ham_candidates, 2);
        assert_eq!(prepared.spam_candidates, 1);
        assert!(
            prepared
                .ignored_mailboxes
                .contains(&"Trash Sub".to_string())
        );
    }

    #[tokio::test]
    async fn empty_corpus_requires_explicit_opt_in_and_preserves_live_by_default() {
        let (dir, _mds, store) = temp_mailstore();
        store.ensure_account_set(1).unwrap();
        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let account = AccountId::new("1".to_string());
        cls.retrain(&RetrainRequest {
            stamp_id: "live-sentinel",
            account: &account,
            message: b"Subject: Live sentinel\r\n\r\nKeep until explicitly cleared\r\n",
            label: Label::Spam,
        })
        .await
        .unwrap();
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let guarded_args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
        });

        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &guarded_args).await;

        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(
            job["last_error"],
            "rebuild produced an empty corpus; live corpus left untouched (pass allow_empty: true to replace it anyway)"
        );
        let retained = cls.stats(&account).await.unwrap();
        assert_eq!((retained.ham_messages, retained.spam_messages), (0, 1));

        let allowed_args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
            "allow_empty": true,
        });
        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &allowed_args).await;

        assert_eq!(rc, 0, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "done");
        let cleared = cls.stats(&account).await.unwrap();
        assert_eq!((cleared.ham_messages, cleared.spam_messages), (0, 0));
    }

    #[tokio::test]
    async fn ignored_only_empty_rebuild_does_not_snapshot_or_prune() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let trash = create_mailbox(&mds, &set, "Trash", Some("\\Trash"));
        add_message(&mds, &set, trash, b"ignored folder message");

        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let account = AccountId::new("1".to_string());
        cls.retrain(&RetrainRequest {
            stamp_id: "live-sentinel",
            account: &account,
            message: b"Subject: Existing live corpus\r\n\r\nKeep this\r\n",
            label: Label::Ham,
        })
        .await
        .unwrap();

        let account_dir = bayes_dir.join("1");
        for sequence in [100_u64, 200, 300] {
            std::fs::write(
                account_dir.join(format!("bayes.pre-rebuild-{sequence}.db")),
                format!("snapshot-{sequence}"),
            )
            .unwrap();
        }
        let snapshot_state = || {
            let mut files = std::fs::read_dir(&account_dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("bayes.pre-rebuild-")
                })
                .map(|entry| (entry.file_name(), std::fs::read(entry.path()).unwrap()))
                .collect::<Vec<_>>();
            files.sort_by(|left, right| left.0.cmp(&right.0));
            files
        };
        let before = snapshot_state();
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": true,
            "wait": true,
        });

        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;

        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(job["snapshot"], serde_json::Value::Null);
        assert_eq!(job["ignored_mailboxes"], serde_json::json!(["Trash"]));
        assert_eq!(snapshot_state(), before);
        let retained = cls.stats(&account).await.unwrap();
        assert_eq!((retained.ham_messages, retained.spam_messages), (1, 0));
    }

    #[tokio::test]
    async fn running_rebuild_refuses_a_second_job() {
        let (dir, _mds, store) = temp_mailstore();
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        state
            .jobs
            .lock()
            .unwrap()
            .insert("1".to_string(), RebuildJob::running());

        let args = serde_json::json!({"account_id": "0001", "wait": true});
        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;
        assert_eq!(rc, RC_ERROR);
        assert!(body.contains("already running"));
        assert!(!dir.path().join("bayes/0001").exists());
    }

    #[tokio::test]
    async fn rebuild_refuses_unknown_account_before_creating_state() {
        let (dir, _mds, store) = temp_mailstore();
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({"account_id": 77, "wait": true});

        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;

        assert_eq!(rc, RC_ERROR);
        assert!(body.contains("account not found"));
        assert!(state.jobs.lock().unwrap().is_empty());
        assert!(!dir.path().join("bayes/77").exists());
    }

    #[tokio::test]
    async fn non_waiting_rebuild_returns_reserved_job_before_enumeration() {
        let (dir, _mds, store) = temp_mailstore();
        store.ensure_account_set(1).unwrap();
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": false,
        });

        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;

        assert_eq!(rc, 0, "body was: {body}");
        let returned: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(returned["state"], "running");
        assert_eq!(returned["ham_candidates"], 0);
        assert_eq!(returned["spam_candidates"], 0);
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            loop {
                if current_job(&state, "1").is_some_and(|job| job.state != "running") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn held_cross_process_lock_fails_without_touching_live_corpus() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        add_message(
            &mds,
            &set,
            inbox,
            b"Subject: Folder candidate\r\n\r\nMust not replace live\r\n",
        );
        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let account = AccountId::new("1".to_string());
        cls.retrain(&RetrainRequest {
            stamp_id: "live-sentinel",
            account: &account,
            message: b"Subject: Live sentinel\r\n\r\nKeep this corpus\r\n",
            label: Label::Spam,
        })
        .await
        .unwrap();
        let before = cls.stats(&account).await.unwrap();
        let lock_path = bayes_dir.join("1/rebuild.lock");
        let _held_lock = try_rebuild_lock(&lock_path).unwrap().unwrap();
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
            "allow_empty": true,
        });

        let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;

        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(
            job["last_error"],
            "rebuild already running for this account (another process holds the lock)"
        );
        let after = cls.stats(&account).await.unwrap();
        assert_eq!(
            (after.ham_messages, after.spam_messages),
            (before.ham_messages, before.spam_messages)
        );
    }

    #[tokio::test]
    async fn stale_shadows_are_removed_under_lock_and_job_name_is_unique() {
        let (dir, _mds, store) = temp_mailstore();
        store.ensure_account_set(1).unwrap();
        let account_dir = dir.path().join("bayes/1");
        std::fs::create_dir_all(&account_dir).unwrap();
        for name in [
            "bayes.rebuild-orphan.db",
            "bayes.rebuild-orphan.db-wal",
            "bayes.rebuild-orphan.db-shm",
        ] {
            std::fs::write(account_dir.join(name), b"orphan").unwrap();
        }
        let fixed_legacy = account_dir.join("bayes.rebuild.db");
        std::fs::write(&fixed_legacy, b"not-pattern-matched").unwrap();
        let seen_shadow = Arc::new(Mutex::new(None::<PathBuf>));
        let hook_seen = Arc::clone(&seen_shadow);
        let hook_dir = account_dir.clone();
        let mut options = RebuildOptions::new(false);
        options.shadow_opened = Some(Arc::new(move |shadow| {
            assert!(shadow.exists());
            assert!(is_rebuild_shadow_name(
                shadow.file_name().unwrap().to_string_lossy().as_ref()
            ));
            assert!(
                !hook_dir.join("bayes.rebuild-orphan.db").exists()
                    && !hook_dir.join("bayes.rebuild-orphan.db-wal").exists()
                    && !hook_dir.join("bayes.rebuild-orphan.db-shm").exists()
            );
            *hook_seen.lock().unwrap() = Some(shadow.to_path_buf());
        }));
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
            "allow_empty": true,
        });

        let (rc, body) =
            handle_rebuild_with_options(&cls, &database, &store, &state, &args, Some(options))
                .await;

        assert_eq!(rc, 0, "body was: {body}");
        let own_shadow = seen_shadow.lock().unwrap().clone().unwrap();
        assert!(!own_shadow.exists());
        assert!(
            fixed_legacy.exists(),
            "cleanup exceeded the allowlisted pattern"
        );
    }

    #[tokio::test]
    async fn panicking_rebuild_marks_failed_once_and_removes_own_shadow() {
        let (dir, _mds, store) = temp_mailstore();
        store.ensure_account_set(1).unwrap();
        let seen_shadow = Arc::new(Mutex::new(None::<PathBuf>));
        let hook_seen = Arc::clone(&seen_shadow);
        let mut options = RebuildOptions::new(false);
        options.shadow_opened = Some(Arc::new(move |shadow| {
            *hook_seen.lock().unwrap() = Some(shadow.to_path_buf());
            panic!("synthetic rebuild panic");
        }));
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
        });

        let (rc, body) =
            handle_rebuild_with_options(&cls, &database, &store, &state, &args, Some(options))
                .await;

        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(job["errors"], 1);
        assert!(
            job["last_error"]
                .as_str()
                .unwrap()
                .contains("panic or cancellation")
        );
        let shadow = seen_shadow.lock().unwrap().clone().unwrap();
        assert!(!shadow.exists());
        assert!(!sqlite_sidecar(&shadow, "-wal").exists());
        assert!(!sqlite_sidecar(&shadow, "-shm").exists());
    }

    #[tokio::test]
    async fn aborted_rebuild_preserves_live_corpus_and_removes_shadow() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        for index in 0..MAX_CONSECUTIVE_ERRORS {
            add_message(
                &mds,
                &set,
                inbox,
                format!(
                    "From: sender-{index}@example.com\r\nSubject: Message {index}\r\n\r\nBody\r\n"
                )
                .as_bytes(),
            );
        }
        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let account = AccountId::new("1".to_string());
        cls.retrain(&RetrainRequest {
            stamp_id: "live-before",
            account: &account,
            message: b"From: friend@example.com\r\n\r\nLive corpus\r\n",
            label: Label::Ham,
        })
        .await
        .unwrap();
        let before = cls.stats(&account).await.unwrap();
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let mut options = RebuildOptions::new(false);
        options.fail_all_reads = true;
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
        });

        let (rc, body) =
            handle_rebuild_with_options(&cls, &database, &store, &state, &args, Some(options))
                .await;

        assert_eq!(rc, RC_ERROR, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "aborted");
        assert_eq!(job["errors"], MAX_CONSECUTIVE_ERRORS);
        let after = cls.stats(&account).await.unwrap();
        assert_eq!(
            (after.ham_messages, after.spam_messages),
            (before.ham_messages, before.spam_messages)
        );
        assert!(
            std::fs::read_dir(bayes_dir.join("1"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !is_rebuild_shadow_name(entry.file_name().to_string_lossy().as_ref()))
        );
    }

    #[tokio::test]
    async fn replay_rederives_trash_as_ignored_and_junk_as_spam() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = create_mailbox(&mds, &set, "Junk", Some("\\Junk"));
        let trash = create_mailbox(&mds, &set, "Trash", Some("\\Trash"));
        let now_trashed = add_message(
            &mds,
            &set,
            inbox,
            b"Subject: Discarded\r\n\r\nNo longer training\r\n",
        );
        let now_junk = add_message(
            &mds,
            &set,
            inbox,
            b"Subject: Suspicious\r\n\r\nCurrent junk state wins\r\n",
        );
        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let hook_mds = Arc::clone(&mds);
        let hook_set = set;
        let trashed_stamp = now_trashed.0.to_string();
        let junk_stamp = now_junk.0.to_string();
        let mut options = RebuildOptions::new(false);
        options.after_enumeration = Some(Arc::new(move |live_path, started_at| {
            hook_mds
                .move_item(&hook_set, &now_trashed, &inbox, &trash, Flags(0))
                .unwrap();
            hook_mds
                .move_item(&hook_set, &now_junk, &inbox, &junk, Flags(0))
                .unwrap();
            let conn = Connection::open(live_path).unwrap();
            conn.execute(
                "INSERT INTO labels (stamp_id, label, ts) VALUES (?1, 0, ?3), (?2, 0, ?3)",
                rusqlite::params![trashed_stamp, junk_stamp, started_at],
            )
            .unwrap();
        }));
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
        });

        let (rc, body) =
            handle_rebuild_with_options(&cls, &database, &store, &state, &args, Some(options))
                .await;

        assert_eq!(rc, 0, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "done");
        assert_eq!(job["replayed"], 2);
        assert_eq!(job["ham_trained"], 0);
        assert_eq!(job["spam_trained"], 1);
        let raw = Connection::open(bayes_dir.join("1/bayes.db")).unwrap();
        let trashed_rows: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM labels WHERE stamp_id = ?1",
                rusqlite::params![now_trashed.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trashed_rows, 0);
        let junk_label: i64 = raw
            .query_row(
                "SELECT label FROM labels WHERE stamp_id = ?1",
                rusqlite::params![now_junk.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(junk_label, 1);
    }

    #[tokio::test]
    async fn swap_conflict_replays_late_correction_from_current_folder_state() {
        let (dir, mds, store) = temp_mailstore();
        let set = store.ensure_account_set(1).unwrap();
        let inbox = create_mailbox(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = create_mailbox(&mds, &set, "Junk", Some("\\Junk"));
        let corrected = add_message(
            &mds,
            &set,
            inbox,
            b"Subject: Late correction\r\n\r\nConflict replay\r\n",
        );
        let bayes_dir = dir.path().join("bayes");
        let cls = disk_classifier(&bayes_dir);
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        let hook_mds = Arc::clone(&mds);
        let stamp = corrected.0.to_string();
        let mut options = RebuildOptions::new(false);
        options.after_replay = Some(Arc::new(move |live_path, started_at| {
            hook_mds
                .move_item(&set, &corrected, &inbox, &junk, Flags(0))
                .unwrap();
            Connection::open(live_path)
                .unwrap()
                .execute(
                    "INSERT INTO labels (stamp_id, label, ts) VALUES (?1, 0, ?2)",
                    rusqlite::params![stamp, started_at],
                )
                .unwrap();
        }));
        let args = serde_json::json!({
            "account_id": 1,
            "snapshot": false,
            "wait": true,
        });

        let (rc, body) =
            handle_rebuild_with_options(&cls, &database, &store, &state, &args, Some(options))
                .await;

        assert_eq!(rc, 0, "body was: {body}");
        let job: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(job["state"], "done");
        assert_eq!(job["replayed"], 1);
        assert_eq!(job["ham_trained"], 0);
        assert_eq!(job["spam_trained"], 1);
        let label: i64 = Connection::open(bayes_dir.join("1/bayes.db"))
            .unwrap()
            .query_row(
                "SELECT label FROM labels WHERE stamp_id = ?1",
                rusqlite::params![corrected.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(label, 1);
    }

    #[test]
    fn rebuild_status_for_unknown_account_is_idle() {
        let state = BayesianBusState::default();
        let (rc, body) = handle_rebuild_status(&state, &serde_json::json!({"account_id": "42"}));
        assert_eq!(rc, 0);
        let status: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            status,
            serde_json::json!({"account_id": "42", "state": "idle"})
        );
    }

    #[tokio::test]
    async fn rebuild_verbs_reject_non_integer_account_id() {
        let (dir, _mds, store) = temp_mailstore();
        let cls = disk_classifier(&dir.path().join("bayes"));
        let database = database_with_accounts(&[1]);
        let state = BayesianBusState::default();
        for bad in [
            serde_json::json!("../../etc"),
            serde_json::json!("alice"),
            serde_json::json!(""),
            serde_json::json!(-1),
            serde_json::json!(1.5),
        ] {
            let args = serde_json::json!({"account_id": bad, "wait": true});
            let (rc, body) = handle_rebuild(&cls, &database, &store, &state, &args).await;
            assert_eq!(rc, RC_ERROR, "body was: {body}");
            assert!(body.contains("account_id"));

            let (rc, body) = handle_rebuild_status(&state, &args);
            assert_eq!(rc, RC_ERROR, "body was: {body}");
            assert!(body.contains("account_id"));
        }
    }
}
