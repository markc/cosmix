//! Production wiring for the maild HTTP/SMTP stack.
//!
//! `build_runtime(cfg, opts)` performs the long startup sequence that
//! used to live inline in `main.rs::Command::Serve`: open the SQLite
//! and MDS stores, register the SPEC 12 property namespaces, bootstrap
//! the rule-engine config from substrate, build the inbound DATA
//! pipeline, and assemble the axum router. Bin and integration tests
//! share the same entry point so the e2e fixture cannot drift from
//! the real daemon.
//!
//! The function returns a [`BuiltMaild`] holding the axum [`Router`]
//! and SMTP listener addresses; the caller binds the JMAP listener
//! and drives `axum::serve` itself. The struct retains owned handles
//! to the property-substrate runtimes so they stay registered while
//! the daemon runs. It does NOT own the SMTP accept tasks or the
//! expiry worker — those are detached on the runtime and survive a
//! `drop(built)`. Process exit is the only shutdown path today;
//! per-test isolation relies on each test getting a fresh `TempDir`
//! root and a fresh ephemeral port.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use tokio::task::JoinHandle;

use crate::auth;
use crate::bus;
use crate::config::Config;
use crate::db;
use crate::imap;
use crate::jmap;
use crate::mailstore;
use crate::props;
use crate::retention_worker;
use crate::rule_stats;
use crate::rule_stats_store;
use crate::smtp;

/// Opt-in / opt-out switches for [`build_runtime`]. Production
/// (`Command::Serve`) uses the defaults; integration tests disable
/// the Bus broker spawn because there is no broker in the test
/// environment and a failing connect would otherwise spam logs.
#[derive(Debug, Clone)]
pub struct RuntimeOpts {
    /// When `true` (default), spawn the Bus broker registration task.
    /// When `false`, the rule engine, classifier, and JMAP/SMTP
    /// surfaces are wired but no `noded` connect is attempted, so the
    /// substrate's live-fan-out is dark and `props.watch` returns
    /// `grant_failed`.
    pub enable_bus: bool,
    /// When `true`, suppress the SMTP outbound delivery worker. Used
    /// by the Thunderbird e2e fixture (and any future test that
    /// enqueues an RFC-2606 / fake remote): the production worker
    /// resolves MX records via real DNS and would hang the test on
    /// non-existent domains. Production always leaves this `false`.
    pub disable_outbound_delivery: bool,
    /// Test-only MX override map, passed through to
    /// [`smtp::SmtpConfig::test_mx_overrides`]. Used by the multi-
    /// vhost outbound integration test to dial a fake MX on
    /// `127.0.0.1` for the per-domain DKIM/HELO wire assertions;
    /// production always leaves this empty.
    pub test_mx_overrides: std::collections::HashMap<String, std::net::SocketAddr>,
}

impl Default for RuntimeOpts {
    fn default() -> Self {
        Self {
            enable_bus: true,
            disable_outbound_delivery: false,
            test_mx_overrides: std::collections::HashMap::new(),
        }
    }
}

/// Result of [`build_runtime`]. Hold this for the lifetime of the
/// serving process. Dropping it releases the property-substrate
/// runtimes only; the SMTP accept loops and the expiry worker are
/// detached tokio tasks and continue running until the tokio runtime
/// itself shuts down.
pub struct BuiltMaild {
    /// Shared application state (DB, MDS-backed mailstore, classifier,
    /// state-change broadcast). Cloned into HTTP handlers via the
    /// axum router; tests can borrow it for direct introspection.
    pub app_state: Arc<jmap::AppState>,
    /// Axum router with the JMAP HTTP routes already wired to
    /// `app_state`. Caller binds a listener and runs `axum::serve`.
    pub router: Router,
    /// SMTP listener addresses (resolved after bind, so a `:0` config
    /// surfaces the assigned port).
    pub smtp_handle: smtp::SmtpHandle,
    /// IMAPS listener addresses (resolved after bind). Empty when
    /// IMAPS is not configured.
    pub imap_handle: imap::ImapHandle,
    _expiry_handle: JoinHandle<()>,
    /// IMAP junk-boundary retrain drain worker — applies
    /// `mail_retrain_outbox` rows out-of-band so IMAP MOVE/COPY
    /// across the `\Junk` boundary trains spam/ham at JMAP parity.
    _retrain_handle: JoinHandle<()>,
    /// Phase 2 commit 4 — periodic flush of `rule_stats` to
    /// `<rule_stats_dir>/stats.db`. Detached; survives
    /// `drop(BuiltMaild)` only via the tokio runtime, the same way
    /// the SMTP accept loops do.
    _rule_stats_flush_handle: JoinHandle<()>,
    /// The membership-retention sweep loop (Junk/Trash age-trim).
    _retention_handle: JoinHandle<()>,
    _overrides_runtime: Arc<cosmix_props::runtime::Runtime>,
    _engine_config_runtime: Arc<cosmix_props::runtime::Runtime>,
    // Held so the `maild.retention` namespace stays registered for the
    // process lifetime; the retention worker (Phase 1) reads config
    // through this runtime.
    _retention_runtime: Arc<cosmix_props::runtime::Runtime>,
    accounts_runtime_lock: Arc<std::sync::OnceLock<Arc<cosmix_props::runtime::Runtime>>>,
    _engine_lock: Arc<std::sync::OnceLock<Arc<cosmix_maild_rules::DefaultRuleEngine>>>,
    /// Phase 5a TLS hot-swap state. Cloned out of the `bus::run` move
    /// path so integration tests can dispatch `maild.tls.reload` with
    /// `enable_bus = false` (no broker). Production wiring shares the
    /// same `Arc`-backed slot / cache via the clone in `bus::run`, so
    /// reloads issued through this handle and reloads issued through
    /// the broker hit the same state. Cheap to clone (every field is
    /// already `Arc`-shaped).
    tls_state: bus::tls::TlsReloadState,
    /// SPEC 12 `maild.log` substrate runtime. The serve path passes
    /// this to `cosmix_log_props::attach_props` so an operator's
    /// `props.set maild.log { level: "debug" }` swaps the live filter.
    log_runtime: cosmix_log_props::LogPropsRuntime,
    _bus_task: Option<JoinHandle<()>>,
}

impl BuiltMaild {
    /// Substrate runtime for the `maild.accounts` namespace.
    /// Integration tests use this to create accounts through the full
    /// substrate write path (so `after_set` seeds the MDS set, the
    /// default mailboxes, and the per-account personal collections).
    pub fn accounts_runtime(&self) -> Arc<cosmix_props::runtime::Runtime> {
        self.accounts_runtime_lock
            .get()
            .expect("accounts_runtime_lock populated by build_runtime")
            .clone()
    }

    /// SPEC 12 `maild.log` substrate runtime. The serve path passes
    /// this to `cosmix_log_props::attach_props` so a live filter swap
    /// follows `props.set maild.log { level: "debug" }`.
    pub fn log_runtime(&self) -> cosmix_log_props::LogPropsRuntime {
        self.log_runtime.clone()
    }

    /// Phase 5a TLS hot-swap handle. Integration tests dispatch
    /// `maild.tls.reload` against this so the substrate path is
    /// exercised end-to-end without spinning up a noded broker.
    pub fn tls_state(&self) -> &bus::tls::TlsReloadState {
        &self.tls_state
    }
}

/// C9 opaque-vtoken namespace pre-flight (see the call site in
/// [`build_runtime`]). Scans every existing `maild.accounts` and `maild.aliases`
/// row for a local-part that is opaque-token-shaped
/// ([`crate::vtoken::is_opaque_shaped`]). Returns `true` only when none collide,
/// so opaque RCPT acceptance is safe to enable; any collision — or a store read
/// error — returns `false` (fail-safe, logged loudly) so an opaque RCPT can
/// never silently swallow a real mailbox. The account/alias *creation* guards
/// keep new collisions out; this catches any that predate them.
async fn preflight_opaque_namespace_clear(
    accounts_runtime: &std::sync::Arc<cosmix_props::runtime::Runtime>,
    aliases_runtime: &std::sync::Arc<cosmix_props::runtime::Runtime>,
) -> bool {
    let mut collisions: Vec<String> = Vec::new();
    let sources = [
        (
            "account",
            accounts_runtime,
            props::accounts::namespace_name(),
        ),
        ("alias", aliases_runtime, props::aliases::namespace_name()),
    ];
    for (label, runtime, ns) in sources {
        match runtime.store().list(&ns).await {
            Ok(snap) => {
                for record in &snap.value {
                    let key = &record.key.key;
                    let local = key.rsplit_once('@').map(|(l, _)| l).unwrap_or(key.as_str());
                    if crate::vtoken::is_opaque_shaped(local) {
                        collisions.push(format!("{label}:{key}"));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    namespace = label,
                    error = %e,
                    "C9 opaque pre-flight: namespace list failed; disabling opaque vtoken RCPT \
                     (fail-safe)"
                );
                return false;
            }
        }
    }
    if collisions.is_empty() {
        tracing::info!("C9 opaque pre-flight: clean; opaque vtoken RCPT acceptance enabled");
        true
    } else {
        tracing::warn!(
            ?collisions,
            "C9 opaque pre-flight: existing account/alias local-parts collide with the opaque \
             vtoken shape; DISABLING opaque vtoken RCPT acceptance (fail-safe). Rename/remove \
             the listed addresses and restart to enable."
        );
        false
    }
}

/// Build the runtime: open stores, register namespaces, bootstrap the
/// engine, wire SMTP and JMAP surfaces. Mirrors the prior inline
/// sequence in `main.rs::Command::Serve` 1:1 — see git history at
/// commit prior to this extraction for the original implementation.
pub async fn build_runtime(cfg: &Config, opts: RuntimeOpts) -> Result<BuiltMaild> {
    // Daemon start instant — backs `maild.stats.server` uptime. Captured
    // at the top of the build so it reports time-since-startup, not
    // time-since-Bus-registration.
    let started_at = std::time::Instant::now();

    let database = db::Db::connect(&cfg.database_path, &cfg.blob_dir).await?;
    database.migrate().await?;

    // Inbound DATA filter pipeline (auth → rules → bayesian).
    let resolver = Arc::new(
        cosmix_maild_auth::resolver::DnsResolver::new(
            cosmix_maild_auth::resolver::ResolverChoice::default(),
        )
        .map_err(|e| anyhow::anyhow!("dns resolver init: {e}"))?,
    );
    let auth_cfg = cosmix_maild_auth::VerifierConfig {
        host_identity: cfg.hostname.clone(),
        ..Default::default()
    };
    let mail_auth = Arc::new(cosmix_maild_auth::MailAuthVerifier::new(resolver, auth_cfg));

    // SPEC 12 Phase 3 C4 — engine handle is wired to the substrate
    // row at startup. The handle is constructed empty so the
    // substrate registration below can take it; the engine is built
    // AFTER the substrate row is read so boot config reflects
    // durable state, not a stale construction-time default.
    let engine_lock: Arc<std::sync::OnceLock<Arc<cosmix_maild_rules::DefaultRuleEngine>>> =
        Arc::new(std::sync::OnceLock::new());

    // Phase 2 commit 4 — restart-durable rule counters. Lives under a
    // dedicated `rule_stats_dir` (default `<var>/maild/rules/`) so
    // global rule-engine counters do not share a root with
    // per-account Bayesian dbs (`spam_db_dir`); the two can be
    // re-pathed independently.
    let rule_stats_db_path = cfg.rule_stats_dir().join("stats.db");
    let rule_stats_store_handle =
        Arc::new(rule_stats_store::RuleStatsStore::open(&rule_stats_db_path).await?);
    let initial_rule_stats = rule_stats_store_handle.load_snapshot().await?;
    tracing::info!(
        path = %rule_stats_db_path.display(),
        verdicts_total = initial_rule_stats.verdicts_total,
        per_rule_keys = initial_rule_stats.per_rule.len(),
        "rule_stats restored from disk"
    );
    let rule_stats = Arc::new(rule_stats::RuleStats::from_snapshot(initial_rule_stats));

    // vtoken registry — operational config (the framework storage-split
    // rule: config → plain SQLite, not mds), a dedicated file beside the
    // main maild DB. Opened once, shared via `Arc`, read O(1) on the
    // inbound hot path (parse $TO → look up user_id → resolve service).
    let vtoken_db_path = std::path::Path::new(&cfg.database_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("vtokens.db");
    let vtoken_store = Arc::new(crate::vtoken::VtokenStore::open(&vtoken_db_path).await?);
    tracing::info!(path = %vtoken_db_path.display(), "vtoken registry opened");

    // Classifier config was previously hardcoded to `::default()`, so nothing in
    // maild.conf could reach it. The base-rate prior has to be switchable from
    // config or it cannot be shadow-evaluated, which is the only way it will ever
    // be trusted enough to enable. Every other field keeps its default.
    let bayes_defaults = cosmix_maild_bayesian::ClassifierConfig::default();
    let bayes_cfg = cosmix_maild_bayesian::ClassifierConfig {
        base_rate_prior: cfg.spam_base_rate_prior.unwrap_or(false),
        base_rate_pseudocount: cfg
            .spam_base_rate_pseudocount
            .unwrap_or(bayes_defaults.base_rate_pseudocount),
        base_rate_min: cfg
            .spam_base_rate_min
            .unwrap_or(bayes_defaults.base_rate_min),
        base_rate_max: cfg
            .spam_base_rate_max
            .unwrap_or(bayes_defaults.base_rate_max),
        ..bayes_defaults
    };
    if bayes_cfg.base_rate_prior {
        // Loud on purpose: this changes how every unknown token is scored, so it
        // must never be a silent property of a host nobody remembers configuring.
        tracing::warn!(
            pseudocount = bayes_cfg.base_rate_pseudocount,
            min = bayes_cfg.base_rate_min,
            max = bayes_cfg.base_rate_max,
            "EXPERIMENTAL: Bayesian base-rate prior is ENABLED — the Robinson centre \
             is the corpus's observed spam rate, not the fixed 0.5"
        );
    }
    let bayes_backend: Arc<dyn cosmix_maild_bayesian::storage::StorageBackend> =
        Arc::new(cosmix_maild_bayesian::storage::SqliteBackend::new(
            PathBuf::from(cfg.spam_db_dir()),
            cfg.spam_baseline_db.as_ref().map(PathBuf::from),
            bayes_cfg.cold_start_floor,
        ));
    let classifier = Arc::new(cosmix_maild_bayesian::DefaultClassifier::new(
        bayes_cfg,
        bayes_backend,
    ));

    // MailStore over cosmix-mds.
    let mds = Arc::new(cosmix_mds::SqliteCasMds::open(&cfg.mds_dir)?);
    let mailstore = Arc::new(mailstore::SqliteMailStore::new(mds.clone()));
    tracing::info!(mds_dir = %cfg.mds_dir, "MailStore opened");

    let _expiry_handle = mailstore::expiry::ExpiryWorker::new(mds.clone()).spawn();
    tracing::info!("upload-staging expiry worker started");

    // IMAP junk-boundary retrain drain. `move_message`/`copy_message`
    // enqueue `mail_retrain_outbox` rows on a `\Junk` boundary cross
    // (IMAP can't retrain inline the way JMAP does); this worker
    // applies them via the same `classifier.retrain()` path JMAP
    // uses, so Thunderbird drag-to/from-Junk trains spam/ham at
    // parity. See `mailstore::retrain` for the rowid-ordered drain.
    let _retrain_handle =
        mailstore::retrain::RetrainOutboxWorker::new(mds, classifier.clone()).spawn();
    tracing::info!("imap junk-boundary retrain worker started");

    // Verdict broadcast channel.
    let (verdict_tx, _) = tokio::sync::broadcast::channel::<bus::verdict::VerdictEvent>(
        bus::verdict::CHANNEL_CAPACITY,
    );

    // SPEC 12 property substrate. Second rusqlite connection on the
    // same database file; PRAGMAs keep the two connections
    // serialised at the SQLite layer.
    let props_conn = {
        let path = cfg.database_path.clone();
        tokio::task::spawn_blocking(move || -> Result<rusqlite::Connection> {
            let conn = rusqlite::Connection::open(&path)?;
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; \
                 PRAGMA foreign_keys=ON; \
                 PRAGMA busy_timeout=5000;",
            )?;
            Ok(conn)
        })
        .await??
    };
    let props_store = Arc::new(
        cosmix_props::sqlite::SqliteStore::new("maild", props_conn)
            .map_err(|e| anyhow::anyhow!("open property store: {e}"))?,
    );
    let mut props_router = cosmix_props::bus::mutation::PropsRouter::new("maild");
    // SPEC 12 Phase 2 register order: account_overrides first, accounts
    // second, then populate the OnceLock so account_overrides::before_set
    // can resolve the accounts runtime.
    let accounts_runtime_lock = std::sync::Arc::new(std::sync::OnceLock::new());
    // `maild.aliases` registers after accounts (its `before_set` reads
    // accounts), but accounts' `before_set` reciprocally reads aliases to
    // reject an account that shadows an alias key — so the lock is created
    // here, handed to accounts now, and populated after aliases registers.
    let aliases_runtime_lock = std::sync::Arc::new(std::sync::OnceLock::new());
    let overrides_runtime = props::account_overrides::register(
        &mut props_router,
        &props_store,
        accounts_runtime_lock.clone(),
    )?;
    let accounts_runtime = props::accounts::register(
        &mut props_router,
        &props_store,
        mailstore.clone(),
        database.conn.clone(),
        overrides_runtime.clone(),
        aliases_runtime_lock.clone(),
    )?;
    accounts_runtime_lock
        .set(accounts_runtime.clone())
        .map_err(|_| {
            anyhow::anyhow!("accounts_runtime_lock already populated (double-register?)")
        })?;
    // SPEC 12 Phase 3 C3/C4 — `maild.engine_config` singleton.
    let engine_config_runtime =
        props::engine_config::register(&mut props_router, &props_store, engine_lock.clone())?;
    // SPEC 12 — `maild.retention` singleton (the in-process retention
    // worker's config surface). Ships fully inert (0-day windows,
    // dry_run:true) so registering it changes no behaviour; the worker
    // that consumes it lands in retention Phase 1. No bootstrap-write:
    // an absent row reads as the inert defaults.
    let retention_runtime = props::retention::register(
        &mut props_router,
        &props_store,
        cfg.retention_operators.clone(),
    )?;
    // Retention worker — the in-process Junk/Trash age-trim loop. Reads
    // its cadence from the just-registered config (60 min default if the
    // row is absent/unreadable). Spawned unconditionally like the
    // expiry/retrain workers; ships INERT (0-day windows + dry_run:true),
    // so it loops but deletes nothing until an operator arms it. One
    // `RetentionWorker` clone drives the loop; another (`retention_state`)
    // backs the `maild.retention.*` verbs — both share the status cell.
    let retention_tick_minutes = props::retention::read_config(&retention_runtime)
        .await
        .map(|c| c.tick_minutes)
        .unwrap_or(60);
    let retention_worker = Arc::new(retention_worker::RetentionWorker::new(
        mailstore.clone(),
        database.clone(),
        retention_runtime.clone(),
        retention_tick_minutes,
    ));
    let retention_state = bus::retention::RetentionBusState {
        worker: retention_worker.clone(),
        operators: Arc::new(cfg.retention_operators.clone()),
    };
    let vtoken_state = bus::vtoken::VtokenBusState {
        store: vtoken_store.clone(),
        operators: Arc::new(cfg.vtoken_operators.clone()),
        delegated_peers: Arc::new(cfg.vtoken_delegated_peers.clone()),
        db: database.conn.clone(),
    };
    let _retention_handle = (*retention_worker).clone().spawn();
    tracing::info!(
        tick_minutes = retention_tick_minutes,
        "membership retention worker started (inert until armed)"
    );
    let (engine_cfg, needs_materialise) =
        props::engine_config::bootstrap_from_store(&props_store).await?;
    tracing::info!(
        row_existed = !needs_materialise,
        "engine_config bootstrap: initial engine cfg read from substrate"
    );
    let (mut rule_engine_inner, rule_failures) = if let Some(path) = cfg.rules_pack_path.as_deref()
    {
        tracing::info!(path = %path, "loading rule pack from disk");
        cosmix_maild_rules::DefaultRuleEngine::with_pack_path(engine_cfg, path)
            .map_err(|e| anyhow::anyhow!("rule pack load from {path}: {e}"))?
    } else {
        cosmix_maild_rules::DefaultRuleEngine::with_pack_str(
            engine_cfg,
            cosmix_maild_rules::default_pack_str(),
        )
        .map_err(|e| anyhow::anyhow!("rule pack load: {e}"))?
    };
    for (id, err) in &rule_failures {
        tracing::warn!(rule = %id, error = %err, "rule pack: rule failed to compile");
    }
    // Phase 2 commit 4 — install the per-rule match hook BEFORE the
    // engine is sealed behind `Arc`. The hook bumps `rule_stats`
    // per-rule counters on every classify match (not explain — the
    // engine gates `fire_hook` by call site).
    {
        let stats_for_hook = rule_stats.clone();
        let hook: cosmix_maild_rules::RuleMatchHook =
            std::sync::Arc::new(move |id: &cosmix_maild_rules::RuleId| {
                stats_for_hook.record_rule_hit(id)
            });
        rule_engine_inner.set_rule_match_hook(Some(hook));
    }
    let rule_engine = Arc::new(rule_engine_inner);

    // Phase 2 commit 4 — periodic flush task. ≤ `flush_interval` of
    // counter increments are lost on a clean process restart; host or
    // power loss can lose more (PRAGMA `synchronous=NORMAL`). Rule
    // stats are diagnostic counters not training data — the engine
    // tolerates both windows. No graceful-shutdown final-flush hook
    // yet (see `rule_stats_store` module docstring for the full
    // durability discussion).
    let flush_interval = std::time::Duration::from_secs(cfg.rule_stats_flush_interval_secs());
    let _rule_stats_flush_handle: JoinHandle<()> = tokio::spawn(rule_stats_store::flush_loop(
        rule_stats.clone(),
        rule_stats_store_handle.clone(),
        flush_interval,
    ));
    engine_lock
        .set(rule_engine.clone())
        .map_err(|_| anyhow::anyhow!("engine_lock already populated (double-register?)"))?;
    if needs_materialise {
        use cosmix_props::record::{Actor, RecordKey, Version};
        use cosmix_props::runtime::SetOpts;
        use cosmix_props::store::MergeMode;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        engine_config_runtime
            .set(
                RecordKey::singleton(props::engine_config::namespace_name()),
                props::engine_config::defaults_value(),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("maild").expect("valid actor"),
                    cause: Some("engine_config bootstrap".into()),
                    ts_ms: now_ms,
                },
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("engine_config bootstrap: materialise defaults row: {e}")
            })?;
        tracing::info!("engine_config bootstrap: defaults row materialised");
    }
    // Phase 1 step 5 — `maild.tls_identities` is a read-only
    // projection of the SNI resolver state. Registered AFTER
    // engine_config so the substrate's namespace order matches the
    // ready log; reconcile is idempotent across restarts so the
    // namespace can be registered every boot without leaking stale
    // rows.
    let tls_identities_runtime = props::tls_identities::register(
        &mut props_router,
        &props_store,
        &cfg.resolve_tls_identities(),
        cfg.strict_sni(),
    )
    .await?;
    // Umbrella Phase 2 — `maild.domains` collection. Legible-only in
    // Phase 2; consumers (RCPT TO gate, HELO selection, DKIM signer,
    // TLS resolver) plumb in over Phases 3–5.
    let domains_runtime = props::domains::register(&mut props_router, &props_store)?;
    // SPEC 12 `maild.aliases` (Phase 1 local-target aliases). Reads
    // accounts for the key-not-account / target-is-account guards. The
    // `aliases_runtime_lock` created above is populated here so accounts'
    // reciprocal collision check (account ∉ alias keys) can read it.
    let aliases_runtime =
        props::aliases::register(&mut props_router, &props_store, accounts_runtime.clone())?;
    aliases_runtime_lock
        .set(aliases_runtime.clone())
        .map_err(|_| {
            anyhow::anyhow!("aliases_runtime_lock already populated (double-register?)")
        })?;
    // Best-effort alias/account reconciliation diagnostic — surfaces any
    // shadow (alias key is also an account) or dangling (target not an
    // account) row a concurrent-write race could have left, so it can't
    // sit silently across a restart. Log-and-continue on read error.
    if let Err(e) = props::aliases::reconcile_diagnostic(&aliases_runtime, &accounts_runtime).await
    {
        tracing::warn!(error = %e, "maild.aliases reconciliation diagnostic failed");
    }
    // C9 opaque-vtoken pre-flight: an opaque RCPT (single-segment HMAC namespace)
    // is only accepted if NO existing account/alias local-part already collides
    // with the opaque token shape — otherwise an opaque RCPT could silently
    // swallow a real mailbox. Fail-safe: any collision (or a read error)
    // disables opaque acceptance for this run; segmented vtokens are unaffected.
    let opaque_rcpt_enabled =
        preflight_opaque_namespace_clear(&accounts_runtime, &aliases_runtime).await;
    // SPEC 12 reserved `maild.log` namespace — live `EnvFilter` swap
    // driven by `<svc>.props.set maild.log { level: "debug" }`. The
    // returned runtime is held in `BuiltMaild`; main's serve path calls
    // `cosmix_log_props::attach_props` once the tokio runtime + the
    // `LogHandle` are both live (the watcher spawns a task).
    let log_runtime = cosmix_log_props::register_log_namespace(&mut props_router, &props_store)?;
    // Best-effort startup diagnostic — informational two-sided diff
    // between domains carrying accounts and `maild.domains` row keys.
    // Log-and-continue on error so a substrate read failure on first
    // boot does not block a daemon that hasn't processed mail yet.
    match database.distinct_account_domains().await {
        Ok(accounts_domains) => {
            if let Err(e) =
                props::domains::reconcile_diagnostic(accounts_domains, &props_store).await
            {
                tracing::warn!(error = %e, "maild.domains reconciliation diagnostic failed");
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "distinct_account_domains failed; skipping maild.domains reconciliation diagnostic"
            );
        }
    }
    // SPEC 12 §15.5 / C10d — install the production SubscribeGranter
    // BEFORE the router is sealed behind Arc.
    let subscribe_granter = Arc::new(bus::subscribe_granter::NodedSubscribeGranter::new(
        bus::subscribe_granter::new_broker_handle(),
    ));
    props_router.set_granter(Arc::clone(&subscribe_granter)
        as Arc<dyn cosmix_props::subscribe_granter::SubscribeGranter>);
    let props_router = Arc::new(props_router);
    tracing::info!(
        "property substrate ready: maild.accounts + maild.account_overrides + \
         maild.engine_config + maild.tls_identities + maild.domains registered"
    );

    // Build the per-domain DKIM signer from `[[dkim.domain]]` rows
    // unioned with the substrate-managed selectors in `maild.domains`.
    // Empty on both sides → `None` → legacy single-key path stays in
    // charge. The result is wrapped in an `Arc<ArcSwap<...>>` so the
    // Phase 4 verb surface (`maild.dkim.{generate,rotate,retire}`) can
    // swap the signer atomically without restarting the daemon. The
    // slot is allocated BEFORE `bus::run` so Bus dispatch holds a
    // clone for live rebuilds; the initial value is stored in-place
    // once `rebuild_signer_from_substrate` returns.
    let baseline_dkim_configs: Vec<cosmix_maild_auth::DkimSignerConfig> =
        cfg.resolve_dkim_signer_configs()?;
    let baseline_dkim_configs = Arc::new(baseline_dkim_configs);
    let dkim_key_root: std::path::PathBuf = cfg
        .dkim
        .key_root
        .clone()
        .unwrap_or_else(crate::config::default_dkim_key_root);
    let mail_auth_signer: Arc<arc_swap::ArcSwap<Option<Arc<cosmix_maild_auth::MailAuthSigner>>>> =
        Arc::new(arc_swap::ArcSwap::from_pointee(None));
    {
        let built = crate::dkim_rebuild::rebuild_signer_from_substrate(
            &baseline_dkim_configs,
            &domains_runtime,
            &dkim_key_root,
        )
        .await?;
        if built.is_some() {
            tracing::info!(
                baseline_rows = baseline_dkim_configs.len(),
                "loaded DKIM signer rows — outbound DKIM uses MailAuthSigner"
            );
        }
        mail_auth_signer.store(Arc::new(built));
    }

    let dkim_state = bus::dkim::DkimState::new(
        mail_auth_signer.clone(),
        baseline_dkim_configs.clone(),
        domains_runtime.clone(),
        dkim_key_root.clone(),
    );

    // Umbrella Phase 5a — TLS hot-swap slot + ServerConfig cache. The
    // slot is primed at startup from `cfg.resolve_tls_identities()`;
    // commit-2's `maild.tls.reload` verb stores a freshly-built
    // resolver into the same slot and clears the cache. Both the SMTP
    // and IMAP listeners share one slot + one cache, so a single
    // reload atomically rotates certs for both surfaces. Construction
    // happens BEFORE `bus::run` so `TlsReloadState` can hold clones of
    // the slot, cache, and substrate runtimes for hot-swap.
    let tls_identities = cfg.resolve_tls_identities();
    let baseline_tls_identities = Arc::new(tls_identities.clone());
    let initial_resolver = if tls_identities.is_empty() {
        None
    } else {
        Some(Arc::new(crate::tls::SniCertResolver::from_config(
            &tls_identities,
            cfg.strict_sni(),
        )?))
    };
    let tls_slot = crate::tls::new_tls_slot(initial_resolver);
    let tls_config_cache = Arc::new(crate::tls::ServerConfigCache::new());

    let tls_root: std::path::PathBuf = cfg
        .tls_key_root
        .clone()
        .unwrap_or_else(crate::config::default_tls_key_root);
    let tls_state = bus::tls::TlsReloadState::new(
        tls_slot.clone(),
        tls_config_cache.clone(),
        baseline_tls_identities,
        domains_runtime.clone(),
        tls_identities_runtime.clone(),
        tls_root,
        cfg.strict_sni(),
    );

    // Per-account IMAP connection counter, shared between the IMAP
    // listener (which increments/decrements it per session) and the
    // `maild.stats.online` / `.server` Bus surface (which snapshots it).
    // Created here, BEFORE `bus::run`, so both consumers hold the same
    // `Arc` — `imap::start` below receives this exact instance rather
    // than minting its own.
    let imap_slots = crate::imap::session::AccountSlots::new();
    // Per-account JMAP last-seen tracker, shared between the JMAP HTTP
    // handlers (which `touch` it on each authenticated request) and the
    // `maild.stats.online` Bus verb (which snapshots it). One `Arc`,
    // same as `imap_slots`.
    let jmap_activity = bus::stats::JmapActivity::new();
    let stats_state = bus::stats::StatsState {
        slots: imap_slots.clone(),
        jmap_activity: jmap_activity.clone(),
        started_at,
    };
    let bayesian_state =
        bus::bayesian::BayesianBusState::new(cfg.bayesian_rebuild_operators.clone());

    // Clone `tls_state` BEFORE the `bus::run` move path so the
    // reload handle survives whether or not Bus is wired. Both
    // branches still consume the original; the clone is what
    // `BuiltMaild::tls_state()` returns.
    let tls_state_for_built = tls_state.clone();
    let _bus_task = if opts.enable_bus {
        Some(tokio::spawn(bus::run(
            rule_engine.clone(),
            rule_stats.clone(),
            classifier.clone(),
            verdict_tx.clone(),
            cfg.hostname.clone(),
            props_router,
            subscribe_granter,
            database.clone(),
            mailstore.clone(),
            overrides_runtime.clone(),
            accounts_runtime.clone(),
            dkim_state,
            tls_state,
            stats_state,
            retention_state,
            vtoken_state,
            bayesian_state,
        )))
    } else {
        let _ = props_router;
        let _ = subscribe_granter;
        let _ = dkim_state;
        let _ = tls_state;
        let _ = stats_state;
        let _ = retention_state;
        let _ = vtoken_state;
        let _ = bayesian_state;
        None
    };

    // SMTP listeners (returns resolved bound addrs for :0 configs).
    let smtp_config = smtp::SmtpConfig {
        hostname: cfg.hostname.clone(),
        listen_inbound: cfg
            .smtp_inbound
            .as_ref()
            .map(|s| s.to_vec())
            .unwrap_or_default(),
        require_starttls_inbound: cfg.require_starttls_inbound.clone(),
        listen_smtps: cfg
            .smtp_smtps
            .as_ref()
            .map(|s| s.to_vec())
            .unwrap_or_default(),
        outbound_bind: cfg
            .smtp_outbound_bind
            .iter()
            .filter_map(|s| match s.parse::<std::net::IpAddr>() {
                Ok(ip) => Some(ip),
                Err(e) => {
                    tracing::warn!(addr = %s, error = %e, "ignoring unparseable smtp_outbound_bind entry");
                    None
                }
            })
            .collect(),
        max_message_size: cfg.max_message_size.unwrap_or(25 * 1024 * 1024),
        opaque_rcpt_enabled,
        dkim_selector: cfg.dkim_selector.clone(),
        dkim_private_key: cfg.dkim_private_key.clone(),
        mail_auth_signer,
        tls_slot: tls_slot.clone(),
        tls_config_cache: tls_config_cache.clone(),
        inbound_filter: cfg.inbound_filter.clone(),
        disable_outbound_delivery: opts.disable_outbound_delivery,
        test_mx_overrides: opts.test_mx_overrides.clone(),
    };
    let smtp_handle = smtp::start(
        database.clone(),
        smtp_config,
        mail_auth.clone(),
        rule_engine.clone(),
        rule_stats.clone(),
        classifier.clone(),
        mailstore.clone(),
        verdict_tx,
        overrides_runtime.clone(),
        domains_runtime.clone(),
        aliases_runtime.clone(),
        vtoken_store.clone(),
    )
    .await?;

    // IMAPS listener (Phase 1 — opt-in via `imap_imaps` TOML key).
    let imap_cfg = imap::ImapConfig::from_config(cfg, tls_slot, tls_config_cache);
    let imap_handle =
        imap::start(database.clone(), mailstore.clone(), imap_cfg, imap_slots).await?;

    // State change broadcast channel for EventSource push.
    let (state_tx, _) = tokio::sync::broadcast::channel::<jmap::StateChange>(256);

    let app_state = Arc::new(jmap::AppState {
        db: database,
        base_url: cfg.base_url.clone(),
        classifier: classifier.clone(),
        state_tx,
        mailstore,
        domains_runtime: domains_runtime.clone(),
        aliases_runtime: aliases_runtime.clone(),
        jmap_activity,
    });

    // `allow_headers(Any)` emits `Access-Control-Allow-Headers: *`, which
    // by the Fetch spec does NOT authorise the `Authorization` header —
    // so a cross-origin browser JMAP client doing HTTP Basic auth would be
    // blocked at preflight. List the headers explicitly (incl.
    // Authorization) so browser SPAs can call JMAP cross-origin. Origin
    // stays `*` (non-credentialed: auth rides an explicit header, not
    // cookies).
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(tower_http::cors::Any)
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ]);

    let router = Router::new()
        .route("/.well-known/jmap", axum::routing::get(jmap::session))
        .route("/jmap", axum::routing::post(jmap::api))
        .route(
            "/jmap/blob/{blobId}",
            axum::routing::get(jmap::blob_download),
        )
        .route(
            "/jmap/upload/{accountId}",
            axum::routing::post(jmap::blob_upload),
        )
        .route("/jmap/eventsource", axum::routing::get(jmap::eventsource))
        // CORS applies to the JMAP routes only (browser clients). It is
        // layered BEFORE merging the DAV routes so it does not wrap them:
        // tower-http's CorsLayer auto-answers every OPTIONS with 200, which
        // would shadow the DAV `OPTIONS` capability handler (RFC 4918). DAV
        // clients are native apps and need no CORS.
        .layer(cors)
        // Body limit must cover the session-advertised maxSizeUpload
        // (50 MB); axum's DefaultBodyLimit is otherwise 2 MB, which 413s
        // any blob upload larger than that (hit importing a 4.4 MB
        // message, 2026-07-24). Same placement rationale as `cors`.
        .layer(axum::extract::DefaultBodyLimit::max(50_000_000))
        // SSR PIM Phase 2 — bearer-token issue/verify/revoke. Issue is
        // Basic-authenticated (exchange password→token); verify/revoke
        // are Bearer-authenticated (self-scoped). Same router, same TLS
        // as /jmap; webd's login handler is the primary caller of issue.
        // These are SERVER-TO-SERVER (webd → maild), so they are added
        // AFTER `.layer(cors)` to escape the wildcard CORS above — no
        // browser origin should be able to mint or revoke tokens
        // cross-origin (same exclusion the DAV merge relies on).
        .route(
            "/auth/tokens/issue",
            axum::routing::post(auth::tokens::issue),
        )
        .route(
            "/auth/tokens/verify",
            axum::routing::post(auth::tokens::verify),
        )
        .route(
            "/auth/tokens/revoke",
            axum::routing::post(auth::tokens::revoke),
        )
        .merge(crate::dav::router())
        .with_state(app_state.clone());

    Ok(BuiltMaild {
        app_state,
        router,
        smtp_handle,
        imap_handle,
        _expiry_handle,
        _retrain_handle,
        _rule_stats_flush_handle,
        _retention_handle,
        _overrides_runtime: overrides_runtime,
        _engine_config_runtime: engine_config_runtime,
        _retention_runtime: retention_runtime,
        accounts_runtime_lock,
        _engine_lock: engine_lock,
        tls_state: tls_state_for_built,
        log_runtime,
        _bus_task,
    })
}
