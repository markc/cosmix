//! ACME provisioner — landed pieces: P2-C5a + P2-C5b + P2-C5d1 + P2-C5d2.
//!
//! This module accumulates the C5 subseries that lifts the Phase 2
//! ACME-vhost guard in `resolve_node_state`. See
//! `_doc/planned/webd-vhosts-phase2.md` § "Commit 5" for the carving.
//!
//! Landed so far:
//!
//! * **C5a (pure logic):** the on-disk [`MetaV1`] record shape, the
//!   [`BACKOFF_SCHEDULE`] and [`next_attempt_after_failure`] scheduler,
//!   the per-vhost in-memory state ([`ProvisionerVhostState`]), and
//!   the [`AcmeProvisioner`] struct shell.
//! * **C5b (atomic-rename state machine on disk):** the per-vhost
//!   layout (`live/`, `staging/<ts>/`, `live.new/`, `archive/<ts>/`),
//!   the atomic helpers ([`write_atomic_pem`], [`write_meta_json`],
//!   [`read_meta_json`]), the five-step transaction's step (1)
//!   ([`stage_new_issuance`]) and step (3) ([`promote_staged_to_live`]),
//!   step (5) ([`prune_archive`]), and the startup roll-forward
//!   ([`reconcile_on_startup`]). All C5b helpers are pure functions on
//!   `&Path` — no provisioner state, no Bus, no async I/O.
//! * **C5d1 (startup pass + Http01Solver + guard removal):** the
//!   [`Http01Solver`] adapter ([`ProvisionerSolver`]) that publishes
//!   tokens into the shared `acme_challenges` map the C4 :80 route
//!   reads, the [`AcmeProvisioner::startup_pass`] driver that
//!   reconciles, classifies, issues, validates, and promotes
//!   each vhost before listener bind, and the `main()` wiring that
//!   binds :80 *before* the startup pass so HTTP-01 validation is
//!   reachable. The C3 ACME-vhost rejection guard in
//!   `resolve_node_state` is lifted by this commit.
//! * **C5d2 (run_forever supervision loop + tokio spawn):** the
//!   [`AcmeProvisioner::run_forever`] per-tick loop, fed by either a
//!   6h-default `tokio::time::interval` or a `tokio::sync::Notify`
//!   kick exposed via [`AcmeProvisioner::notify_handle`]. Each tick
//!   walks the plans, checks each cert's `not_after - now < 30d`
//!   renewal window against the per-vhost cooldown
//!   ([`ProvisionerVhostState::next_attempt_after`]), fires an
//!   `order` for any plan inside the window, validates, promotes,
//!   and (after at least one successful renewal) rebuilds the
//!   per-listener resolvers and swaps them into the
//!   [`ListenerTls`] handles threaded through
//!   [`AcmeProvisioner::attach_tls_listeners`]. A renewal failure is
//!   journal-only + backoff advance — never a runtime error and
//!   never propagated out of the loop.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustls::pki_types::UnixTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedMutexGuard, RwLock, mpsc, watch};
use tracing::{info, warn};

use cosmix_config::acme_policy::{AcmeTosAcceptance, AcmeVhostPlan};
use cosmix_config::node::{TlsIdentityConfig, WebdAcmeChallenge, WebdAcmeProvider};
use cosmix_daemon::acme::{
    AcmeError, AcmeProvider, Http01Solver, WebdAcmeClient, verify_account_meta_local,
};
use cosmix_daemon::listen::ListenerTls;
use cosmix_daemon::tls::sni::SniCertResolver;
use cosmix_daemon::tls::{ChainEnvironment, validate_le_chain_for_environment};
use cosmix_props::{
    Actor, MergeMode, PropValue, RecordKey, Runtime, SetOpts, StoreError, Version, WriteOrigin,
};

use crate::VhostState;
use crate::tls_status::{
    AcmePlanSummary, AcmeStatusSnapshot, AcmeVhostStateSnapshot, ManualIdentitySnapshot,
    TlsStatusSnapshot,
};
use crate::vhost_directory::{VhostDirectory, from_namespace_rows};
use crate::vhosts_namespace::{NamespaceEvent, VhostRow, namespace_name, vhost_row_from_value};

/// C4b — shared per-FQDN lock map for serialising
/// `VhostRemoved` cleanup against the C5 `vhost.add` re-issue
/// path. The provisioner's `VhostRemoved` arm and the C5 verb
/// dispatcher acquire the same per-fqdn `Mutex<()>` via this
/// map, ensuring a `vhost.add` for a recently-deleted fqdn
/// blocks until the archive completes (and then re-issues
/// against an empty `live/`).
///
/// Wrapping is `Arc<TokioMutex<HashMap<...>>>` rather than
/// `DashMap` because (a) no other crate in the workspace
/// depends on `dashmap` and adding a dep for a single field
/// is unjustified; (b) lock acquisition is brief
/// (lookup-or-insert + clone the inner `Arc`, drop outer); (c)
/// contention is bounded by operator activity, not request
/// volume — at most one `VhostRemoved` plus one `vhost.add`
/// touching the same FQDN at the same time.
pub type FqdnLockMap = Arc<TokioMutex<HashMap<String, Arc<TokioMutex<()>>>>>;

/// C4b — error type returned by the two republish helpers
/// (`republish_tls_config` and `republish_vhost_directory`).
/// A typed error (rather than `anyhow::Result`) lets the
/// `issue_one` and `VhostRemoved` callers branch precisely on
/// the failure mode: a `Build` error is a substrate-invariant
/// violation (manual-PEM duplicates, ACME-identity collision),
/// while `NsList`/`NoNsRuntime`/`NoVhostsHandle` indicate the
/// arc-swap publish path is mis-wired and the namespace write
/// MUST be skipped (Codex rev 2 BLOCKER fix, lines 1589–1591).
///
/// Both helpers map all failure modes through this enum so
/// the spec's "namespace write skipped on publish failure"
/// invariant is preserved across both surfaces.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The ServerConfig builder rejected the candidate identity
    /// list. Usually a `(server_name, default)` collision between
    /// base identities and an ACME identity, or a chain that
    /// `SniCertResolver::from_config` failed to register.
    #[error("rebuilding rustls ServerConfig: {0}")]
    Build(anyhow::Error),

    /// Failed to read the `webd.vhosts` namespace via
    /// `Runtime::store().list(...)`. Recoverable: the next
    /// reconcile tick retries. Includes row-decode failures
    /// surfaced by `snapshot_rows` (codec drift between the
    /// substrate row schema and `VhostRow`).
    #[error("listing webd.vhosts namespace: {0}")]
    NsList(anyhow::Error),

    /// `from_namespace_rows` rejected the namespace snapshot.
    /// A substrate-invariant violation (config_bootstrap row
    /// absent from runtime map, etc.). The caller skips the
    /// namespace write so reconcile can retry once the operator
    /// repairs.
    #[error("building VhostDirectory: {0}")]
    Directory(anyhow::Error),

    /// The provisioner has no `vhosts_runtime` attached.
    /// Production wiring always attaches one when ACME plans
    /// exist; this surfaces a test wiring bug or a fresh
    /// build that bypassed `attach_ns_events`.
    #[error("no webd.vhosts runtime attached")]
    NoNsRuntime,

    /// The provisioner has no `node_vhosts` arc-swap attached.
    /// Same wiring shape as `NoNsRuntime` — production always
    /// attaches via `attach_node_vhosts`. Surfacing this as a
    /// typed error keeps the C4b ordering guarantee (Codex E.B
    /// caveat): a silent no-op publish would let the namespace
    /// write proceed against a stale arc-swap, which is
    /// exactly the invariant the publish-before-write order
    /// exists to prevent.
    #[error("no NodeState vhost arc-swap attached")]
    NoVhostsHandle,
}

/// C4b — ordering instrumentation slots, captured by tests to
/// pin the "arc-swap publishes BEFORE namespace write"
/// invariant. Production wiring leaves every slot `None`; the
/// `attach_publish_instrumentation` method (test-only callers,
/// but the method itself is plain code so the production path
/// runs the same `Option::is_some` branch the tests do — see
/// the C-decision below) sets all four `Some`. Spec line 1648
/// suggests `#[cfg(test)]` or feature-gating; we choose
/// `Option<Arc<AtomicU64>>` so tests exercise the production
/// code path with a single `is_some` branch per publish (zero
/// instruction-count cost outside the test harness).
#[derive(Debug, Default, Clone)]
pub struct PublishInstrumentation {
    /// Shared monotonic counter — incremented once at each
    /// captured publish/write event. The strict-`<` ordering
    /// in the V-O2 test relies on the counter being shared
    /// (so two events cannot share a value).
    pub seq: Arc<AtomicU64>,
    /// Captured at the moment `republish_tls_config`'s
    /// `ArcSwap::store` returns.
    pub tls_publish_seq: Arc<AtomicU64>,
    /// Captured at the moment `republish_vhost_directory`'s
    /// `ArcSwap::store` returns.
    pub vhost_directory_publish_seq: Arc<AtomicU64>,
    /// Captured at the moment the `props.set` await arm
    /// inside `writeback_issued_row` returns success.
    pub namespace_write_seq: Arc<AtomicU64>,
}

/// Default interval between [`AcmeProvisioner::run_forever`] sweeps
/// when the caller doesn't override it (production caller always
/// passes the default; tests pass a much shorter value). Six hours
/// matches the umbrella plan and the `staging_orphan_ttl` rationale
/// (see [`reconcile_on_startup`]) so leftover staging dirs and the
/// renewal cadence stay in step.
pub const DEFAULT_RENEWAL_TICK: Duration = Duration::from_secs(6 * 60 * 60);

/// Threshold for triggering renewal: when the leaf cert's
/// `not_after - now` falls below this, the next tick fires an
/// `order`. LE issues 90-day certs, so 30 days leaves three weeks
/// of slack between the renewal window opening and the cert
/// actually expiring — comfortably wider than the [`BACKOFF_SCHEDULE`]
/// ceiling (7d) so an operator-actionable failure has multiple
/// renewal attempts before the cert goes hard-expired.
pub const RENEWAL_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// On-disk record co-located with each issued chain
/// (`<acme-dir>/<fqdn>/meta.json`). Written atomically alongside the
/// PEM files by C5b's `promote_staged_to_live`. The struct is
/// `serde`-derived so the renewal loop can round-trip it without
/// pulling in a separate codec module.
///
/// Schema is versioned via an explicit `version: u8` discriminator
/// (see [`MetaV1::CURRENT_VERSION`]) and the struct name (`MetaV1`).
/// A future v2 lives in the same `meta.json` file with `version: 2`;
/// C5b's reader refuses to parse any version it does not know rather
/// than silently downgrading to "no live chain → fresh issuance",
/// which would trigger rate-limit churn against LE if an operator
/// rolled webd back over a v2 state directory. `deny_unknown_fields`
/// is set so a future writer that adds *fields* (not just versions)
/// also forces an explicit upgrade path rather than being accepted
/// with unknown semantics by an older reader.
///
/// `not_before` / `not_after` mirror the issued chain's leaf
/// certificate, parsed once at promotion time so the renewal
/// scheduler does not re-parse PEM on every tick.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MetaV1 {
    /// Schema discriminator. Always [`MetaV1::CURRENT_VERSION`]
    /// when this binary writes the file; C5b's reader rejects any
    /// other value with a hard error so a rollback over a future
    /// version surfaces as "stop and ask an operator" rather than
    /// "silently re-issue against LE".
    pub version: u8,
    /// ACME order URL from the directory that issued this chain.
    /// Used for audit/diagnostic surfacing only — the renewal loop
    /// always starts a fresh order rather than reusing this one.
    pub order_url: String,
    /// ACME account URL of the LE account that placed the order.
    pub account_url: String,
    /// Leaf certificate `notBefore`, in seconds since the UNIX epoch
    /// (UTC). Stored as an integer rather than RFC3339 so a future
    /// rebuilt agent (or `jq`) can compare it without a date parser.
    pub not_before_unix: i64,
    /// Leaf certificate `notAfter`, in seconds since the UNIX epoch
    /// (UTC). The renewal scheduler kicks at `not_after_unix - 30d`
    /// (Phase 2 §"Commit 5"). See C5b/C5d for the exact rule.
    pub not_after_unix: i64,
    /// When this chain was promoted to `live/` on this node, in
    /// seconds since the UNIX epoch (UTC).
    pub issued_at_unix: i64,
    /// Primary FQDN this chain covers. Redundant with the parent
    /// directory name; carried in-record so a misplaced `meta.json`
    /// (operator copy / rsync mistake) is detectable without a
    /// filesystem walk.
    pub fqdn: String,
    /// Additional SAN-coverage names. May be empty.
    pub aliases: Vec<String>,
    /// Which ACME provider issued the chain. Stored as the same
    /// `WebdAcmeProvider` enum the resolver hands us so a provider
    /// switch (staging → prod, or vice versa) forces a fresh
    /// issuance via the mismatch check in C5d's `reconcile`.
    pub provider: WebdAcmeProvider,
    /// The ACME directory URL the chain was issued against. Carried
    /// in addition to `provider` so a future LE directory change is
    /// auditable from `meta.json` alone without a co-located journal
    /// lookup.
    pub acme_directory_url: String,
}

impl MetaV1 {
    /// The schema version this binary writes and the only one C5b's
    /// reader currently accepts. Bumped together with a real
    /// migration plan whenever `MetaV1` changes incompatibly.
    pub const CURRENT_VERSION: u8 = 1;
}

/// Backoff intervals applied after consecutive issuance / renewal
/// failures for a single vhost. Index = `error_count - 1` clamped to
/// `BACKOFF_SCHEDULE.len() - 1`, so 1 failure → 1h, 2 → 6h, 3 → 24h,
/// 4 → 7d, 5+ → 7d. See [`next_attempt_after_failure`].
///
/// The schedule is deliberately conservative: LE production has a
/// 5/account/hour failed-validation cap and a 300/account/3h new-order
/// cap, and a misconfigured vhost can burn through both before an
/// operator notices. The 1h floor + 7d ceiling keeps a stuck vhost
/// inside both rate-budgets while still recovering within a workday
/// once the operator fixes the upstream cause.
pub const BACKOFF_SCHEDULE: [Duration; 5] = [
    Duration::from_secs(60 * 60),          //   1 hour
    Duration::from_secs(6 * 60 * 60),      //   6 hours
    Duration::from_secs(24 * 60 * 60),     //  24 hours
    Duration::from_secs(7 * 24 * 60 * 60), //   7 days
    Duration::from_secs(7 * 24 * 60 * 60), //   7 days (clamp)
];

/// Compute the next attempt time for a vhost that has just failed
/// for the `error_count`-th consecutive time.
///
/// `error_count` is 1-based: the caller has *already* incremented the
/// counter for the failure being recorded, so `error_count == 1`
/// after the first failure. `error_count == 0` is nonsensical (no
/// failure to schedule from) and saturates to the first-failure
/// interval rather than panicking — defensive because the only path
/// that constructs the counter is C5d's renewal loop, but the
/// arithmetic should not crash if a future caller miscounts.
pub fn next_attempt_after_failure(now: OffsetDateTime, error_count: u32) -> OffsetDateTime {
    let idx = error_count.saturating_sub(1) as usize;
    let idx = idx.min(BACKOFF_SCHEDULE.len() - 1);
    now + BACKOFF_SCHEDULE[idx]
}

/// Per-vhost provisioner state held in-memory by [`AcmeProvisioner`].
///
/// Persisted state lives in `MetaV1` on disk; this struct is the
/// *transient* slice the renewal loop consults to decide whether a
/// given vhost is currently in cooldown after a failure. On daemon
/// restart the default (zero errors, no cooldown) is reconstructed
/// from disk reconciliation — failures do not persist across
/// restarts by design (a stuck vhost gets one fresh attempt per
/// daemon boot, after which the backoff schedule takes over again).
#[derive(Debug, Default, Clone)]
pub struct ProvisionerVhostState {
    /// Most recent error message, for operator surfacing. `None`
    /// when the vhost has never failed in this process's lifetime
    /// or has succeeded since its last failure.
    pub last_error: Option<String>,
    /// Number of consecutive failures since the last success. Used
    /// as the index into [`BACKOFF_SCHEDULE`] via
    /// [`next_attempt_after_failure`]. Reset to 0 on success.
    pub last_error_count: u32,
    /// When this vhost is next eligible for an issuance attempt.
    /// `None` means "no cooldown — attempt on the next tick if
    /// reconciliation says it needs one".
    pub next_attempt_after: Option<OffsetDateTime>,
}

/// In-memory provisioner shell. C5a holds the configuration handed
/// in by `main()` plus the per-vhost transient state map; C5b adds
/// the disk-state methods, C5c the resolver swap handle, and C5d the
/// `startup_pass` / `run_forever` methods that actually drive the
/// state machine.
///
/// The struct is constructed in `main()` *after* `resolve_node_state`
/// produces the `AcmeVhostPlan` list and *before* the listener binds
/// (C5d): the startup pass must hard-fail listener bind on a
/// fresh-vhost order failure so an operator never sees a daemon that
/// "started cleanly" with a vhost stuck at 0 issued chains.
//
// No `Debug` derive: `WebdAcmeClient` (cached under `staging_client` /
// `prod_client` after C5d2) carries the LE account credential pair
// and intentionally does not implement `Debug`. The provisioner is
// owned by `main()` and the `run_forever` tokio task; no logging
// site needs a `{:?}` of the whole struct.
pub struct AcmeProvisioner {
    /// Filesystem root for ACME state. C5b lays out
    /// `<acme_dir>/<fqdn>/{live,staged,archive/}` per vhost.
    acme_dir: PathBuf,

    /// Per-vhost issuance plans produced by
    /// `cosmix_config::acme_policy::resolve_webd_acme`. Carried by
    /// value so the provisioner owns the canonical copy the renewal
    /// loop iterates; the resolver's output is not re-read.
    plans: Vec<AcmeVhostPlan>,

    /// Node-level ToS proof. `None` is legitimate (staging-only
    /// node, or no ACME at all); C5d's `startup_pass` rejects the
    /// fresh-order path for a *prod* plan when this is `None`.
    tos_acceptance: Option<AcmeTosAcceptance>,

    /// Shared challenge map handed to the HTTP-01 route built in
    /// P2-C4. C5d's `Http01Solver` impl writes `(token → keyauth)`
    /// here before the order's HTTP-01 challenge is triggered, and
    /// removes the entry on cleanup.
    challenges: Arc<RwLock<HashMap<String, String>>>,

    /// In-memory per-vhost cooldown / failure state. Keyed by FQDN
    /// (the same string carried in `AcmeVhostPlan::fqdn`). C5d1
    /// reads this in `startup_pass` only on the "carrying & servable
    /// — still in cooldown" branch; C5d2's `run_forever` tick records
    /// failures and advances backoff here.
    vhost_state: HashMap<String, ProvisionerVhostState>,

    /// Manual-PEM identities resolved by `resolve_node_state` (legacy
    /// pair + per-vhost `tls_cert` / `tls_key` rows). These are static
    /// across the daemon's lifetime; C5d2's `republish_tls_config`
    /// clones them as the floor of every rebuilt `ServerConfig` so
    /// SNI for manual-PEM hosts keeps resolving after an ACME-only
    /// swap.
    base_identities: Vec<TlsIdentityConfig>,

    /// ACME identities currently published — keyed by FQDN. Populated
    /// by [`AcmeProvisioner::startup_pass`] for both the carrying-
    /// servable and freshly-issued branches; mutated by
    /// [`AcmeProvisioner::run_forever`] on each successful renewal so
    /// the next [`AcmeProvisioner::republish_tls_config`] rebuilds
    /// from a coherent union.
    acme_identities: HashMap<String, TlsIdentityConfig>,

    /// Hot-swappable per-listener TLS handles (P2-C2), keyed by
    /// listener id. Empty is legitimate for tests (the renewal loop
    /// simply skips publish); production wiring attaches them via
    /// [`AcmeProvisioner::attach_tls_listeners`] after the initial
    /// resolvers are built so each renewal can swap a fresh resolver
    /// into the right listener. Each `ListenerTls` is a clone that
    /// shares its inner resolver slot with the `ListenerSet`'s accept
    /// loop, so a `swap` here is seen live by that listener.
    tls_listeners: HashMap<String, ListenerTls>,

    /// Maps every served vhost FQDN to its owning listener id (P2-C2)
    /// — the partition key `republish_tls_config_for_plans` uses to
    /// route each manual-PEM / ACME identity into the right listener's
    /// rebuilt resolver. Built once from the resolved listener set and
    /// attached alongside [`Self::tls_listeners`].
    fqdn_to_listener: HashMap<String, String>,

    /// Interval between [`AcmeProvisioner::run_forever`] sweeps.
    /// Production callers pass [`DEFAULT_RENEWAL_TICK`]; tests pass
    /// a much shorter value (milliseconds) to keep the suite fast.
    renewal_tick: Duration,

    /// External wake handle. `notify_one` here makes the next
    /// [`AcmeProvisioner::run_forever`] iteration tick immediately
    /// instead of waiting on the interval. C5d2 does not expose any
    /// external kicker; the handle exists for a follow-up commit
    /// (operator-driven force-renew via Bus) and for tests.
    notify: Arc<Notify>,

    /// Cached `WebdAcmeClient` for the LE staging directory. Loaded
    /// on first need (a fresh-issue or renewal targeting staging) and
    /// reused for every subsequent staging order so the account
    /// credential pair is read from disk + the directory is dialled
    /// once per daemon lifetime, not once per renewal.
    staging_client: Option<WebdAcmeClient>,

    /// Cached `WebdAcmeClient` for the LE production directory. Same
    /// lazy-load semantics as `staging_client`. The
    /// [`AcmeTosAcceptance`] is *borrowed* into
    /// `WebdAcmeClient::load_or_create_prod` (lib-daemon takes
    /// `&AcmeTosAcceptance` post-C5d2) so a transient load failure
    /// (LE directory dial, fresh-account POST, …) doesn't drop the
    /// proof and pin every subsequent retry into a misleading
    /// "no acme_tos_accepted" error.
    prod_client: Option<WebdAcmeClient>,

    /// Cumulative count of `tick_once` calls. Production callers
    /// don't read it; it exists so the C5d2 notify-wake test can
    /// observe that `notify_one` actually drove a sweep through the
    /// loop's `tokio::select!` (a spawn-then-abort test without an
    /// observable would pass even if the wake arm were broken). The
    /// per-tick atomic increment is the only production-path cost,
    /// once per 6h by default.
    tick_count: Arc<AtomicU64>,

    /// Status mirror sender, plumbed in from `main`. `None` is
    /// legitimate for tests (the publish call becomes a no-op).
    /// Production wiring always passes `Some` because the channel is
    /// constructed unconditionally — see
    /// [`crate::NodeState::tls_status_rx`] for the consumer side and
    /// [`crate::tls_status`] for the snapshot shape.
    tls_status_tx: Option<watch::Sender<TlsStatusSnapshot>>,

    /// C4a — receiver half of the `webd.vhosts` namespace event
    /// channel. Wired via [`AcmeProvisioner::attach_ns_events`] before
    /// the [`AcmeProvisioner::run_forever`] task spawn; `None` is
    /// legitimate for tests + the manual-PEM-only production path.
    ///
    /// The receiver is **taken** out of `self` at the top of
    /// `run_forever` (see the local-binding rationale in that method)
    /// so the `tokio::select!` arm can poll it without holding a `&mut
    /// self` borrow across the select. Construction leaves it `Some`;
    /// after `run_forever` enters its loop, this field is permanently
    /// `None` for the lifetime of the moved provisioner.
    ns_events: Option<mpsc::Receiver<NamespaceEvent>>,

    /// C4a — runtime handle for the `webd.vhosts` namespace. Threaded
    /// through `attach_ns_events` because every C4a code path that
    /// touches the receiver also needs the runtime: the event arm
    /// writes back `cert_blob_id` / `not_after` after a successful
    /// fresh-issuance, and the notify-driven snapshot reconcile arm
    /// lists the namespace to diff against `self.plans`. Carried as
    /// `Option<Arc<Runtime>>` so a test that only exercises the
    /// in-process select! shape can pass `None`.
    vhosts_runtime: Option<Arc<Runtime>>,

    /// C4b — publish target for the host-routing directory. The
    /// provisioner publishes a new `VhostDirectory` (via
    /// `republish_vhost_directory`) immediately before writing
    /// back the namespace row's `cert_blob_id` / `not_after`
    /// fields, AND on the `VhostRemoved` delete-arm cleanup
    /// path so the dispatch surface drops the entry before the
    /// archive runs.
    ///
    /// `Option` so the manual-PEM-only test path can skip the
    /// publish — production wiring always attaches via
    /// `attach_node_vhosts`. Absence on a production-shaped
    /// publish path is a typed `PublishError::NoVhostsHandle`,
    /// not a silent no-op (Codex 4b design-pass caveat).
    node_vhosts: Option<Arc<ArcSwap<VhostDirectory>>>,

    /// C4b — `resolve_node_state`-produced runtime map cloned
    /// into the provisioner at construction time. Threaded
    /// through to `from_namespace_rows` on every
    /// `republish_vhost_directory` call so `config_bootstrap`
    /// rows can recover their full `Arc<VhostState>` wiring
    /// (db, jmap_upstream, noded_ws, docs_dir, stats).
    ///
    /// Static across the provisioner's lifetime — the runtime
    /// map is computed once at startup from `node.conf.mix` and
    /// the manual-PEM identity set; runtime `vhost.add`s
    /// produce `bus_runtime` rows that synthesise their state
    /// inside `from_namespace_rows` without touching this
    /// map, so the map never needs to be refreshed.
    resolved_runtime_map: HashMap<String, Arc<VhostState>>,

    /// B1 fail-soft — vhost names `resolve_node_state` dropped because
    /// their per-vhost validation failed (bad cert / missing www_dir /
    /// unopenable CMS DB). Threaded into every
    /// `from_namespace_rows` call so a post-startup ACME republish
    /// tolerates the disabled vhost's surviving `config_bootstrap`
    /// namespace row (skip from the directory) instead of re-tripping
    /// the runtime-map invariant and failing the republish. Frozen at
    /// startup alongside `resolved_runtime_map`.
    disabled_hosts: HashSet<String>,

    /// C4b — per-FQDN coordination lock map, shared with the
    /// C5 `vhost.add` Bus verb dispatcher. The `VhostRemoved`
    /// cleanup arm acquires `key_locks[&fqdn]` before the
    /// pre-stale-event read; the C5 verb acquires the same
    /// lock before calling `Runtime::set_with_origin(...)`.
    /// This serialises a `vhost.add` for a recently-deleted
    /// fqdn against the in-flight archive, closing the
    /// rev-4-BLOCKER race window.
    ///
    /// `Arc<TokioMutex<HashMap<...>>>` so the C5 verb holds
    /// a clone of the same map identity; the inner per-fqdn
    /// `Arc<Mutex<()>>` is the actual coordination point.
    /// See [`FqdnLockMap`] for the rationale on not using
    /// DashMap.
    key_locks: FqdnLockMap,

    /// C5 — operator-driven force-renew queue. The
    /// `webd.acme.renew` Bus verb inserts a fqdn here, then
    /// calls `notify.notify_one()` to wake the renewal loop.
    /// `tick_once` drains the set at the top of the sweep
    /// and, for any plan whose fqdn appears in the drained
    /// snapshot, bypasses **both** timing gates (cooldown +
    /// renewal-window) for that tick only.
    ///
    /// Disabled-state and apex-policy gates are **not**
    /// bypassed — those represent operator policy, not
    /// scheduler timing, and a force-renew of a disabled
    /// vhost would be a footgun.
    ///
    /// Single-shot semantics: the set is drained, not
    /// inspected. A caller wanting to retry a failed
    /// force-renew re-invokes the Bus verb.
    force_renew_fqdns: Arc<TokioMutex<HashSet<String>>>,

    /// C4b — optional publish-ordering instrumentation. `None`
    /// in production. Tests attach a populated
    /// `PublishInstrumentation` via
    /// `attach_publish_instrumentation` to capture the
    /// sequence in which `republish_tls_config`,
    /// `republish_vhost_directory`, and the namespace row
    /// write complete. The V-O2 ordering test
    /// (`arc_swap_publish_precedes_namespace_write_observably`)
    /// asserts both `tls_publish_seq` and
    /// `vhost_directory_publish_seq` are strictly less than
    /// `namespace_write_seq` against the shared `seq`
    /// counter.
    publish_instrumentation: Option<PublishInstrumentation>,
}

// Manual `Debug` impl — `WebdAcmeClient` and `AcmeTosAcceptance` are
// intentionally non-Debug on the lib-daemon side (the former wraps
// account keys; the latter is a one-shot acceptance proof). Tests
// that `unwrap`/`expect` on `Result<AcmeProvisioner, _>` need the Ok
// variant to be Debug, so render those fields as placeholders rather
// than leaking their contents.
impl std::fmt::Debug for AcmeProvisioner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeProvisioner")
            .field("acme_dir", &self.acme_dir)
            .field("plans", &self.plans)
            .field(
                "tos_acceptance",
                &self.tos_acceptance.as_ref().map(|_| "<redacted>"),
            )
            .field("vhost_state", &self.vhost_state)
            .field("base_identities", &self.base_identities)
            .field("acme_identities", &self.acme_identities)
            .field(
                "tls_listeners",
                &self.tls_listeners.keys().collect::<Vec<_>>(),
            )
            .field("fqdn_to_listener", &self.fqdn_to_listener)
            .field("renewal_tick", &self.renewal_tick)
            .field(
                "staging_client",
                &self.staging_client.as_ref().map(|_| "<client>"),
            )
            .field(
                "prod_client",
                &self.prod_client.as_ref().map(|_| "<client>"),
            )
            .field(
                "node_vhosts",
                &self.node_vhosts.as_ref().map(|_| "<arc-swap>"),
            )
            .field("resolved_runtime_map_len", &self.resolved_runtime_map.len())
            .field(
                "publish_instrumentation",
                &self
                    .publish_instrumentation
                    .as_ref()
                    .map(|_| "<instrumented>"),
            )
            .finish()
    }
}

impl AcmeProvisioner {
    /// Construct an empty provisioner. C5a only builds the struct
    /// — `startup_pass` and `run_forever` arrive in C5d.
    ///
    /// Fails when two plan rows share an FQDN. The resolver in
    /// `cosmix_config::acme_policy::resolve_webd_acme` already
    /// rejects duplicates at config-load time; this is a
    /// belt-and-braces gate so the per-FQDN cooldown map and the
    /// per-FQDN on-disk state directory (C5b) cannot be silently
    /// shared between two plans should the upstream invariant ever
    /// regress.
    pub fn new(
        acme_dir: PathBuf,
        plans: Vec<AcmeVhostPlan>,
        tos_acceptance: Option<AcmeTosAcceptance>,
        challenges: Arc<RwLock<HashMap<String, String>>>,
        base_identities: Vec<TlsIdentityConfig>,
        renewal_tick: Duration,
        tls_status_tx: Option<watch::Sender<TlsStatusSnapshot>>,
    ) -> Result<Self> {
        // C5d1 is single-fqdn per plan: `WebdAcmeClient::order`
        // takes one identifier and the issued chain's SAN coverage
        // is validated against `[&plan.fqdn]` only. Allowing an
        // ACME row with aliases would register the alias under the
        // host-router map (so it'd reach a vhost) but the SNI
        // handshake for the alias would fall through the resolver
        // because no identity was emitted for it — silently routing
        // alias SNI through the non-strict default or `no_cert_for_sni`.
        // Reject it loudly until multi-SAN ordering lands.
        for plan in &plans {
            if !plan.aliases.is_empty() {
                return Err(anyhow!(
                    "AcmeProvisioner: vhost {:?} declares ACME with aliases {:?} — \
                     multi-SAN ACME issuance is not implemented in C5d1 \
                     (one-cert-per-fqdn only). Drop the aliases or hold the \
                     row on a manual-PEM cert covering them until the multi-SAN \
                     follow-up lands.",
                    plan.fqdn,
                    plan.aliases
                ));
            }
        }

        // Mixed-provider configs (e.g. one staging vhost + one prod
        // vhost) share `acme_dir` for both account credential pairs.
        // lib-daemon's `scan_for_foreign_provider`
        // (cosmix-lib-daemon/src/acme.rs) refuses to create a second
        // provider's account file when a different provider's
        // `account.<provider>.meta.json` is already present in the
        // same directory; that turns a mixed config into a hard
        // startup error on the second-provider plan with no remedy
        // short of separate state dirs. Until per-provider acme
        // subdirectories land, force operators to pick one provider
        // per webd instance up front so the failure shape is "your
        // config is mixed" rather than "the second plan refused to
        // load for an opaque reason".
        let mut providers = plans.iter().map(|p| p.provider);
        if let Some(first) = providers.next()
            && let Some(other) = providers.find(|p| *p != first)
        {
            return Err(anyhow!(
                "AcmeProvisioner: webd ACME plans mix providers ({:?} and {:?}) — \
                 lib-daemon's per-provider account-meta scan in a shared \
                 acme_dir refuses to bootstrap a second provider's account \
                 alongside the first. Pin all [[webd.vhost]] rows to a \
                 single provider, or wait for per-provider state-dir \
                 partitioning.",
                first,
                other
            ));
        }

        let mut vhost_state: HashMap<String, ProvisionerVhostState> =
            HashMap::with_capacity(plans.len());
        for plan in &plans {
            if vhost_state
                .insert(plan.fqdn.clone(), ProvisionerVhostState::default())
                .is_some()
            {
                return Err(anyhow!(
                    "AcmeProvisioner: duplicate FQDN {:?} in plans — \
                     the resolver should have rejected this; refusing \
                     to construct a provisioner that would share \
                     cooldown state and on-disk identity between two \
                     plan rows",
                    plan.fqdn
                ));
            }
        }
        Ok(Self {
            acme_dir,
            plans,
            tos_acceptance,
            challenges,
            vhost_state,
            base_identities,
            acme_identities: HashMap::new(),
            tls_listeners: HashMap::new(),
            fqdn_to_listener: HashMap::new(),
            renewal_tick,
            notify: Arc::new(Notify::new()),
            staging_client: None,
            prod_client: None,
            tick_count: Arc::new(AtomicU64::new(0)),
            tls_status_tx,
            ns_events: None,
            vhosts_runtime: None,
            node_vhosts: None,
            resolved_runtime_map: HashMap::new(),
            disabled_hosts: HashSet::new(),
            key_locks: Arc::new(TokioMutex::new(HashMap::new())),
            force_renew_fqdns: Arc::new(TokioMutex::new(HashSet::new())),
            publish_instrumentation: None,
        })
    }

    /// Build a fresh [`TlsStatusSnapshot`] from current provisioner
    /// state and publish it on the watch channel handed in at
    /// construction. No-op for tests that pass `None`.
    ///
    /// Called once from `main` after `startup_pass` completes, and
    /// from every state-mutating branch of `tick_once`. Publishing
    /// only at the top of each ~6h tick would leave `last_error` /
    /// `next_attempt` stale until the next sweep; operators want
    /// fresh failure detail, not yesterday's snapshot.
    pub fn publish_tls_status(&self) {
        let Some(tx) = &self.tls_status_tx else {
            return;
        };
        let plans: Vec<AcmePlanSummary> = self.plans.iter().map(AcmePlanSummary::from).collect();
        let mut vhost_state: std::collections::BTreeMap<String, AcmeVhostStateSnapshot> =
            std::collections::BTreeMap::new();
        // Iterate over `plans` (not `vhost_state`) so the wire shape
        // always carries one entry per configured plan — including
        // plans that have never failed (default state) — keeping the
        // dispatch output stable across boots and renewal sweeps.
        for plan in &self.plans {
            let st = self.vhost_state.get(&plan.fqdn);
            let next_rfc3339 = st.and_then(|s| s.next_attempt_after).map(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| t.to_string())
            });
            vhost_state.insert(
                plan.fqdn.clone(),
                AcmeVhostStateSnapshot {
                    issued: self.acme_identities.contains_key(&plan.fqdn),
                    last_error: st.and_then(|s| s.last_error.clone()),
                    last_error_count: st.map(|s| s.last_error_count).unwrap_or(0),
                    next_attempt_after_rfc3339: next_rfc3339,
                },
            );
        }
        let snap = TlsStatusSnapshot {
            manual_identities: self
                .base_identities
                .iter()
                .map(ManualIdentitySnapshot::from)
                .collect(),
            acme: Some(AcmeStatusSnapshot { plans, vhost_state }),
        };
        // `send_replace` ignores receiver count — a manual-PEM-only
        // node never reaches this method (no provisioner), so the
        // only reader is the Bus layer via `NodeState::tls_status_rx`.
        tx.send_replace(snap);
    }

    /// Attach the per-listener hot-swappable TLS handles + the
    /// FQDN→listener routing map (P2-C2). On each successful renewal
    /// [`AcmeProvisioner::run_forever`] rebuilds a `SniCertResolver`
    /// per affected listener and `swap`s it into the matching
    /// `ListenerTls`. Calling this *after* startup_pass — but *before*
    /// `run_forever` — is the expected production order (the initial
    /// resolvers are built from the combined manual + ACME identity
    /// union returned by startup_pass, so the handles can't be
    /// threaded earlier without a chicken-and-egg).
    pub fn attach_tls_listeners(
        &mut self,
        listeners: HashMap<String, ListenerTls>,
        fqdn_to_listener: HashMap<String, String>,
    ) {
        self.tls_listeners = listeners;
        self.fqdn_to_listener = fqdn_to_listener;
    }

    /// Attach the `webd.vhosts` namespace event receiver + runtime
    /// handle (C4a). Mirrors [`AcmeProvisioner::attach_tls_listeners`]'s
    /// post-construction wiring shape so `main` doesn't have to
    /// re-order the existing construction sequence — the receiver is
    /// produced by `register_vhosts_namespace` (called *after* the
    /// provisioner is built when an ACME plan exists) so a single
    /// constructor arg would force a major rewire.
    ///
    /// Called between `register_vhosts_namespace` + bootstrap-upsert
    /// and the `tokio::spawn(run_forever)` call. Production wiring
    /// always passes both arguments; the no-ACME-boot path never
    /// instantiates a provisioner and therefore never reaches this
    /// method — see the no-ACME limitation documented in `main.rs`
    /// near the receiver-park site for what that means in practice.
    pub fn attach_ns_events(&mut self, rx: mpsc::Receiver<NamespaceEvent>, runtime: Arc<Runtime>) {
        self.ns_events = Some(rx);
        self.vhosts_runtime = Some(runtime);
    }

    /// C4b — attach the `NodeState::vhosts` arc-swap publish target
    /// and the bootstrap-resolved runtime map. Wired between
    /// `attach_ns_events` and `tokio::spawn(run_forever)` so the
    /// first `tick_once` (and any incoming `VhostRemoved`) can
    /// publish a directory snapshot before mutating namespace state
    /// or archiving on-disk certs.
    ///
    /// `vhosts` is the same `Arc<ArcSwap<VhostDirectory>>` held in
    /// `NodeState::vhosts`; the provisioner's
    /// `republish_vhost_directory` `.store(...)`s a new snapshot
    /// without taking a write lock anywhere (arc-swap is the
    /// coordination mechanism).
    ///
    /// `resolved_runtime_map` is the `HashMap<String,
    /// Arc<VhostState>>` produced by `resolve_node_state` at
    /// startup — config-block vhosts' fully-wired
    /// `Arc<VhostState>` carriers. `from_namespace_rows` reuses
    /// these for `source = "config_bootstrap"` rows so CMS / JMAP
    /// / docs surfaces stay configured across runtime republishes.
    /// `bus_runtime` rows synthesise their `VhostState` from
    /// namespace fields and do not touch this map.
    ///
    /// Cloning the whole map at attach time (rather than holding
    /// an `Arc<HashMap<...>>`) is intentional: the runtime map is
    /// computed once and never mutated by runtime activity — the
    /// substrate's view of "what wiring config_bootstrap rows
    /// have" is frozen at startup.
    /// `disabled_hosts` (B1 fail-soft) is the set of vhost names
    /// `resolve_node_state` dropped on bad-cert / missing-www_dir /
    /// unopenable-CMS-DB; threaded through to `from_namespace_rows` so a
    /// republish tolerates their surviving `config_bootstrap` rows.
    pub fn attach_node_vhosts(
        &mut self,
        vhosts: Arc<ArcSwap<VhostDirectory>>,
        resolved_runtime_map: HashMap<String, Arc<VhostState>>,
        disabled_hosts: HashSet<String>,
    ) {
        self.node_vhosts = Some(vhosts);
        self.resolved_runtime_map = resolved_runtime_map;
        self.disabled_hosts = disabled_hosts;
    }

    /// C4b — attach a per-FQDN coordination lock map identity.
    /// The provisioner's `VhostRemoved` cleanup arm and the C5
    /// `vhost.add` Bus verb dispatcher MUST hold clones of the
    /// same map identity; main constructs the map once and hands
    /// one clone to each side.
    ///
    /// Called between construction and `tokio::spawn(run_forever)`.
    /// If omitted, the provisioner falls back to its own
    /// constructor-time `key_locks` instance — fine for tests
    /// that never invoke the C5 verb, fatal in production where
    /// the cap-split would let a `vhost.add` race the archive.
    /// Production wiring always calls this method.
    pub fn attach_key_locks(&mut self, key_locks: FqdnLockMap) {
        self.key_locks = key_locks;
    }

    /// C4b — cloneable handle to the per-FQDN lock map. The C5
    /// Bus verb dispatcher acquires the same lock identity via
    /// this handle. Wired into use in C5; until then carry the
    /// allow-dead-code so the workspace clippy gate stays clean.
    #[allow(dead_code)]
    pub fn key_locks_handle(&self) -> FqdnLockMap {
        Arc::clone(&self.key_locks)
    }

    /// C4b — attach publish-ordering instrumentation. Test-only
    /// caller surface (a follow-up V-O2 slice exercises the
    /// strict-ordering pin via this handle). Production callers
    /// do not invoke this; the `publish_instrumentation` field
    /// stays `None` and every publish site takes the
    /// no-instrumentation branch (single `Option::is_some`
    /// check, negligible overhead).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn attach_publish_instrumentation(&mut self, instr: PublishInstrumentation) {
        self.publish_instrumentation = Some(instr);
    }

    /// Cloneable handle to the renewal-tick wake channel. Calling
    /// `notify_one` on the returned handle from anywhere makes the
    /// next [`AcmeProvisioner::run_forever`] iteration run a sweep
    /// immediately instead of waiting on the interval. C5d2 ships
    /// the handle but no external kicker — present for tests and
    /// for a follow-up operator-force-renew commit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Queue a fqdn for force-renewal on the next `tick_once` sweep
    /// and wake the loop. Atomic: a caller cannot forget to notify
    /// after queueing, which would leave the entry stranded until
    /// the next scheduled tick (potentially hours away).
    ///
    /// The `tick_once` consumer drains the set at the top of each
    /// sweep — repeated calls with the same fqdn before a tick lands
    /// collapse to one force-renew attempt. Force-renew bypasses
    /// both the renewal-window and per-vhost cooldown gates for
    /// that fqdn for that tick only; if the issuance attempt
    /// fails, cooldown re-engages and a follow-up `notify_one`
    /// without a fresh `queue_force_renew` will not bypass it.
    ///
    /// No-op if `fqdn` is not present in the provisioner's plans
    /// — the bypass arm only triggers on a plan iteration match.
    /// The caller does NOT need to check membership; queueing an
    /// unknown fqdn is harmless and self-clears at tick time.
    ///
    /// Used by tests directly on `&self`; production callers reach
    /// the same semantic via the
    /// [`AcmeProvisioner::force_renew_handle`] + notify pair
    /// because the provisioner is moved into `tokio::spawn` before
    /// the Bus verbs come up — the verb cannot hold a `&self`
    /// reference.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn queue_force_renew(&self, fqdn: String) {
        self.force_renew_fqdns.lock().await.insert(fqdn);
        self.notify.notify_one();
    }

    /// Cloneable handle to the force-renew queue. The C5
    /// `webd.acme.renew` Bus verb dispatcher holds a clone of this
    /// Arc alongside [`AcmeProvisioner::notify_handle`] so it can
    /// drive `queue_force_renew` semantics from `NodeState` without
    /// keeping a borrow on the provisioner (which has moved into
    /// `tokio::spawn(run_forever)` by then).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn force_renew_handle(&self) -> Arc<TokioMutex<HashSet<String>>> {
        Arc::clone(&self.force_renew_fqdns)
    }

    /// Cloneable handle to the cumulative tick counter (see
    /// [`AcmeProvisioner::tick_count`] field docs). Test-only —
    /// production wiring has no need for this signal.
    #[cfg(test)]
    fn tick_count_handle(&self) -> Arc<AtomicU64> {
        self.tick_count.clone()
    }

    /// Pre-bind ACME pass (P2-C5d1).
    ///
    /// Reconciles any crash-interrupted promotion on disk, classifies
    /// each plan as **fresh** / **carrying-servable** / **carrying-
    /// unservable**, drives a fresh `WebdAcmeClient::order` through
    /// the five-step transaction for the fresh and carrying-unservable
    /// branches, validates every issued chain via
    /// `validate_le_chain_for_environment` *before* promotion, and
    /// returns the resulting [`TlsIdentityConfig`] list to be spliced
    /// into the rustls resolver.
    ///
    /// Caller contract: the HTTP-01 :80 listener MUST already be
    /// accepting connections backed by the shared `challenges` map
    /// (the same `Arc<RwLock<HashMap<...>>>` handed to
    /// [`AcmeProvisioner::new`]) before this method is awaited. The
    /// C4 acme-challenge route reads from that map; if :80 is not yet
    /// bound, every order's authorization will time out and a
    /// fresh-vhost order failure is a hard startup error per the
    /// plan.
    ///
    /// `now` and `now_unix` are both UTC, taken from the *same*
    /// clock read at the call site — the `OffsetDateTime` drives
    /// filesystem stamps + reconcile, the `UnixTime` drives chain
    /// validation. Passing them in (rather than reading the clock
    /// internally) keeps the method test-pinnable.
    ///
    /// `staging_orphan_ttl` matches the renewal tick (6h default);
    /// see [`reconcile_on_startup`] for the rationale.
    ///
    /// A fresh-order failure (or a carrying-unservable failure that
    /// the validator rejects after re-issuance) is propagated as a
    /// hard error: the caller MUST refuse to bind :443 on `Err`.
    pub async fn startup_pass(
        &mut self,
        now: OffsetDateTime,
        now_unix: UnixTime,
        staging_orphan_ttl: Duration,
    ) -> Result<Vec<TlsIdentityConfig>> {
        let mut identities: Vec<TlsIdentityConfig> = Vec::with_capacity(self.plans.len());

        // Clone the plan list so the borrow checker lets the loop
        // body see `&mut self` for the lazily-loaded client caches
        // in `self.staging_client` / `self.prod_client`.
        let plans = self.plans.clone();

        // Read-only account-meta verification: run lib-daemon's
        // field comparisons (`provider` / `directory_url` /
        // `tos_url`) against the recorded
        // `account.<provider>.meta.json` *without* loading
        // credentials or touching the network. Carrying-servable
        // boots stay offline-capable; operator drift (e.g. flipping
        // `acme_tos_accepted` to a newer URL while a still-valid
        // chain remains in `live/`) is surfaced at every startup
        // instead of lying dormant until renewal. The mixed-provider
        // gate in `new` guarantees `self.plans` has at most one
        // provider, so peeking the first plan is sufficient.
        if let Some(first) = plans.first() {
            let expected_tos = self.tos_acceptance.as_ref().map(|t| t.url());
            verify_account_meta_local(
                &self.acme_dir,
                webd_provider_to_lib(first.provider),
                expected_tos,
            )
            .context(
                "[webd-acme] account-meta verification at startup \
                 (read-only; no network) — operator drift on a still-valid \
                 chain must surface here, not at renewal time",
            )?;
        }
        for plan in &plans {
            let fqdn_dir = self.acme_dir.join(&plan.fqdn);
            std::fs::create_dir_all(&fqdn_dir).with_context(|| {
                format!(
                    "creating ACME state dir {:?} for vhost {:?}",
                    fqdn_dir, plan.fqdn
                )
            })?;
            let reconcile = reconcile_on_startup(&fqdn_dir, staging_orphan_ttl, now)
                .with_context(|| format!("reconciling ACME state for vhost {:?}", plan.fqdn))?;
            match &reconcile {
                ReconcileOutcome::CleanState => {}
                other => info!(
                    fqdn = %plan.fqdn,
                    outcome = ?other,
                    "ACME startup reconcile rolled forward / swept orphan staging"
                ),
            }

            let live_dir = fqdn_dir.join(LIVE_DIR);
            let live_chain = live_dir.join(FULLCHAIN_FILE);
            let live_key = live_dir.join(PRIVKEY_FILE);
            let live_meta = live_dir.join(META_FILE);

            // Classification: existence first, then servability for
            // carrying chains. See plan §"Commit 5 — startup_pass".
            let needs_issue = match read_meta_json(&live_meta) {
                Err(MetaReadError::NotFound { .. }) => true,
                Err(e) => {
                    return Err(anyhow!(
                        "[webd-acme] reading {} live meta.json for vhost {:?}: {} \
                         — corrupt / unreadable state must not be silently \
                         re-issued; rotate the state dir or roll back the binary",
                        plan.fqdn,
                        plan.fqdn,
                        e,
                    ));
                }
                Ok(meta) => {
                    // Existence check passed → classify servability.
                    // (i) chain present + readable;
                    // (ii) privkey.pem present + non-empty;
                    // (iii) validator OK against the configured
                    //     environment;
                    // (iv) `not_after > now`.
                    // Any of those failing means "carrying-unservable
                    // → collapse to the fresh path." A missing or
                    // truncated PEM file is recoverable disk damage,
                    // not a reason to refuse to bind :443; only meta
                    // corruption / unsupported-version / true I/O
                    // policy errors above propagate as hard errors.
                    let chain_pem = match std::fs::read(&live_chain) {
                        Ok(b) if !b.is_empty() => b,
                        Ok(_) => {
                            warn!(
                                fqdn = %plan.fqdn,
                                "ACME live fullchain.pem is empty — re-issuing"
                            );
                            Vec::new()
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            warn!(
                                fqdn = %plan.fqdn,
                                "ACME live fullchain.pem missing alongside meta.json — re-issuing"
                            );
                            Vec::new()
                        }
                        Err(e) => {
                            return Err(anyhow!(
                                "[webd-acme] reading {} for vhost {:?}: {} \
                                 — unrecoverable I/O on live chain; rotate \
                                 the state dir or restore from backup",
                                live_chain.display(),
                                plan.fqdn,
                                e,
                            ));
                        }
                    };
                    // privkey.pem must exist non-empty too; the
                    // rustls splice below references its path
                    // verbatim, so an absent/empty key would otherwise
                    // surface as a runtime handshake failure on the
                    // first connection.
                    let key_ok = match std::fs::metadata(&live_key) {
                        Ok(m) => m.len() > 0,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                        Err(e) => {
                            return Err(anyhow!(
                                "[webd-acme] stat {} for vhost {:?}: {} \
                                 — unrecoverable I/O on live key; rotate \
                                 the state dir or restore from backup",
                                live_key.display(),
                                plan.fqdn,
                                e,
                            ));
                        }
                    };
                    if !key_ok {
                        warn!(
                            fqdn = %plan.fqdn,
                            "ACME live privkey.pem missing or empty — re-issuing"
                        );
                    }
                    let env = chain_environment_for(plan.provider);
                    let chain_ok = !chain_pem.is_empty()
                        && validate_le_chain_for_environment(
                            &chain_pem,
                            &[&plan.fqdn],
                            now_unix,
                            env,
                        )
                        .is_ok();
                    let now_unix_secs = now.unix_timestamp();
                    let expired = meta.not_after_unix <= now_unix_secs;
                    let provider_drift = meta.provider != plan.provider;
                    match (chain_ok, expired, provider_drift, key_ok) {
                        (true, false, false, true) => {
                            // Carrying & servable. Splice the live
                            // PEMs into the identity set unchanged.
                            // (Aliases: see "Known gap" in the
                            // closeout — C5d1 issues per-fqdn only.)
                            let identity = TlsIdentityConfig {
                                server_name: plan.fqdn.clone(),
                                cert: live_chain.to_string_lossy().into_owned(),
                                key: live_key.to_string_lossy().into_owned(),
                                default: false,
                                no_sni_fallback: false,
                            };
                            identities.push(identity.clone());
                            // C5d2 needs the live identity for each
                            // FQDN so a future renewal can rebuild the
                            // ServerConfig from a coherent union of
                            // manual + ACME identities.
                            self.acme_identities.insert(plan.fqdn.clone(), identity);
                            if meta.not_after_unix - now_unix_secs < RENEWAL_WINDOW.as_secs() as i64
                            {
                                info!(
                                    fqdn = %plan.fqdn,
                                    not_after_unix = meta.not_after_unix,
                                    "ACME chain inside 30d window — \
                                     renewal will fire on the next \
                                     C5d2 tick"
                                );
                            }
                            false
                        }
                        _ => {
                            // Validator rejected, expired, provider
                            // drift, or PEM material missing/empty.
                            // Collapse to the fresh path. The plan
                            // calls this "carrying-unservable".
                            warn!(
                                fqdn = %plan.fqdn,
                                chain_ok,
                                key_ok,
                                expired,
                                provider_drift,
                                "ACME live chain is carrying-unservable \
                                 — re-issuing as fresh"
                            );
                            true
                        }
                    }
                }
            };

            if !needs_issue {
                continue;
            }

            // Fresh / carrying-unservable: drive the order through
            // the shared issuance helper (C5d2 extraction — same
            // path the renewal tick uses). A failure here is a
            // hard startup error per the plan.
            let identity = self
                .issue_one(plan, now, now_unix)
                .await
                .with_context(|| format!("ACME issuance for vhost {:?} (startup)", plan.fqdn))?;
            identities.push(identity.clone());
            self.acme_identities.insert(plan.fqdn.clone(), identity);
        }
        Ok(identities)
    }

    /// Shared issuance core for both [`AcmeProvisioner::startup_pass`]
    /// (fresh / carrying-unservable) and the renewal tick
    /// ([`AcmeProvisioner::run_forever`]). Drives the full
    /// five-step transaction:
    ///
    /// 1. Load the per-provider client (lazy, cached on `self`).
    /// 2. `client.order` against the configured FQDN, with HTTP-01
    ///    challenges routed through [`ProvisionerSolver`].
    /// 3. Validate the issued chain via
    ///    `validate_le_chain_for_environment` *before* promotion —
    ///    the LE-validator gauntlet is the entire point of having
    ///    the validator gate.
    /// 4. `stage_new_issuance` → `promote_staged_to_live` →
    ///    `prune_archive` (steps 1, 3, and 5 of the on-disk
    ///    transaction; step 2 is the validate above; step 4 — the
    ///    resolver swap — is the caller's responsibility).
    ///
    /// Returns the [`TlsIdentityConfig`] referring to the freshly-
    /// promoted `live/` PEM pair. Caller is responsible for
    /// splicing it into the rustls resolver (startup_pass appends
    /// to the returned vec; `run_forever` updates
    /// `self.acme_identities` and calls `republish_tls_config`).
    async fn issue_one(
        &mut self,
        plan: &AcmeVhostPlan,
        now: OffsetDateTime,
        now_unix: UnixTime,
    ) -> Result<TlsIdentityConfig> {
        let fqdn_dir = self.acme_dir.join(&plan.fqdn);
        std::fs::create_dir_all(&fqdn_dir).with_context(|| {
            format!(
                "creating ACME state dir {:?} for vhost {:?}",
                fqdn_dir, plan.fqdn
            )
        })?;
        let provider = webd_provider_to_lib(plan.provider);
        // Build the solver before the mutable `client_for` borrow so
        // the `self.challenges` clone and the cached client reference
        // don't overlap on `self`.
        let solver = ProvisionerSolver::new(self.challenges.clone());
        let client = self.client_for(plan).await?;
        info!(fqdn = %plan.fqdn, ?provider, "ACME order starting (HTTP-01)");
        let issued = client
            .order(&plan.fqdn, &solver)
            .await
            .with_context(|| format!("ACME order for vhost {:?} failed", plan.fqdn))?;

        // Step (2) of the on-disk transaction: validate the issued
        // chain *before* writing it. Refuse to promote a chain the
        // LE-validator gauntlet rejects.
        let env = chain_environment_for(plan.provider);
        validate_le_chain_for_environment(issued.cert_pem.as_bytes(), &[&plan.fqdn], now_unix, env)
            .with_context(|| {
                format!(
                    "validating LE-issued chain for vhost {:?} before promotion",
                    plan.fqdn
                )
            })?;

        // MetaV1: `order_url` / `account_url` left empty until
        // lib-daemon's `IssuedChain` surfaces them; the fields are
        // documented as audit-only and aren't read by the renewal
        // path.
        let meta = MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: String::new(),
            account_url: String::new(),
            not_before_unix: issued.not_before.unix_timestamp(),
            not_after_unix: issued.not_after.unix_timestamp(),
            issued_at_unix: now.unix_timestamp(),
            fqdn: plan.fqdn.clone(),
            aliases: Vec::new(),
            provider: plan.provider,
            acme_directory_url: provider.directory_url().to_string(),
        };
        let staged = stage_new_issuance(&fqdn_dir, &issued.cert_pem, &issued.key_pem, &meta, now)
            .with_context(|| format!("staging issued chain for vhost {:?}", plan.fqdn))?;
        promote_staged_to_live(&staged, now).with_context(|| {
            format!("promoting staged chain to live/ for vhost {:?}", plan.fqdn)
        })?;
        let removed = prune_archive(&fqdn_dir, 10)
            .with_context(|| format!("pruning archive for vhost {:?}", plan.fqdn))?;
        if removed > 0 {
            info!(fqdn = %plan.fqdn, removed, "pruned old archive entries");
        }

        let live_dir = fqdn_dir.join(LIVE_DIR);
        info!(
            fqdn = %plan.fqdn,
            not_after_unix = meta.not_after_unix,
            "ACME issuance promoted to live/"
        );
        Ok(TlsIdentityConfig {
            server_name: plan.fqdn.clone(),
            cert: live_dir.join(FULLCHAIN_FILE).to_string_lossy().into_owned(),
            key: live_dir.join(PRIVKEY_FILE).to_string_lossy().into_owned(),
            default: false,
            no_sni_fallback: false,
        })
    }

    /// Lazily load (and cache on `self`) the `WebdAcmeClient` for a
    /// given plan's provider. The first call per provider performs
    /// the LE-directory round-trip + reads the account credential
    /// pair from disk; subsequent calls hit the cache. Prod plans
    /// borrow `self.tos_acceptance` on every load attempt and retain
    /// the proof on the provisioner — the C5d2 retry-safety
    /// refactor: a transient prod account-load failure leaves the
    /// one-shot `AcmeTosAcceptance` mint in place so backoff
    /// retries can succeed without a daemon restart. The mint
    /// itself is still bare-private-constructed at resolver time;
    /// `client_for` only borrows it.
    async fn client_for(&mut self, plan: &AcmeVhostPlan) -> Result<&WebdAcmeClient> {
        match plan.provider {
            WebdAcmeProvider::LetsEncryptStaging => {
                if self.staging_client.is_none() {
                    self.staging_client = Some(
                        WebdAcmeClient::load_or_create_staging(&self.acme_dir, &plan.contact_email)
                            .await
                            .with_context(|| {
                                format!("loading LE staging account for vhost {:?}", plan.fqdn)
                            })?,
                    );
                }
                Ok(self.staging_client.as_ref().expect("just inserted"))
            }
            WebdAcmeProvider::LetsEncryptProd => {
                if self.prod_client.is_none() {
                    // Borrow the ToS proof rather than `.take()`-ing
                    // it: lib-daemon's `load_or_create_prod` takes
                    // `&AcmeTosAcceptance`, so a transient
                    // account-load failure leaves the proof in
                    // place for the next backoff retry. Taking the
                    // proof before the await would lose the
                    // one-shot mint on the first network blip and
                    // pin all later retries into a misleading "no
                    // acme_tos_accepted" path until daemon restart.
                    let tos = self.tos_acceptance.as_ref().ok_or_else(|| {
                        anyhow!(
                            "[webd-acme] vhost {:?} configured for LE prod \
                             but no `[webd] acme_tos_accepted` URL is set \
                             — the resolver should have rejected this earlier",
                            plan.fqdn
                        )
                    })?;
                    self.prod_client = Some(
                        WebdAcmeClient::load_or_create_prod(
                            &self.acme_dir,
                            &plan.contact_email,
                            tos,
                        )
                        .await
                        .with_context(|| {
                            format!("loading LE prod account for vhost {:?}", plan.fqdn)
                        })?,
                    );
                }
                Ok(self.prod_client.as_ref().expect("just inserted"))
            }
        }
    }

    /// Per-tick supervision loop (P2-C5d2). Spawned by `main()` as a
    /// tokio task after the initial per-listener resolvers are built
    /// and attached via
    /// [`AcmeProvisioner::attach_tls_listeners`]. Loops indefinitely;
    /// the returned `Result<()>` is only `Err` if the
    /// `acme_renewal_tick` interval cannot be constructed (a startup
    /// programmer error), never from a renewal failure.
    ///
    /// Each iteration awaits *either* the renewal-tick interval
    /// *or* an external `notify_one` on the shared
    /// [`AcmeProvisioner::notify_handle`], then runs
    /// [`AcmeProvisioner::tick_once`] which walks every plan, fires
    /// renewals whose live cert is inside the 30-day renewal window
    /// and whose per-vhost cooldown is past, and (after at least
    /// one success) republishes the resolver.
    ///
    /// Renewal failure is journal-only + backoff advance — never
    /// propagated out of the loop, never a runtime error. The
    /// existing live cert keeps serving until either the next
    /// successful renewal lands or the cert hard-expires (which an
    /// operator notices via the journal warn entries long before
    /// the 7d ceiling of the backoff schedule lets a stuck vhost
    /// reach `not_after`).
    pub async fn run_forever(mut self) -> Result<()> {
        let mut interval = tokio::time::interval(self.renewal_tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Deliberately *do not* consume the immediate-fire first tick.
        // `startup_pass` only fires a renewal for fresh / carrying-
        // unservable plans; a carrying-servable plan whose live cert
        // is already inside the 30-day renewal window is left alone
        // (info-logged only). Without an immediate first tick, a node
        // that boots with hours-to-minutes left on its cert would
        // wait the full `renewal_tick` (6h default) before any
        // renewal attempt — long enough for hard expiry. The
        // immediate tick is cheap for not-in-window plans (one
        // meta.json read per plan + a comparison), and correct for
        // in-window plans (issuance attempt that the loop would
        // otherwise defer).
        let notify = self.notify.clone();
        // C4a — take the receiver into a local before the loop so the
        // event arm's poll doesn't need a `&mut self` borrow across
        // the `select!`. If `attach_ns_events` was not called (test
        // path, or the no-ACME-but-somehow-spawned path), the arm
        // resolves to a `std::future::pending()` future that never
        // completes, leaving the existing interval + notify arms to
        // drive the loop unchanged.
        let mut ns_events = self.ns_events.take();
        info!(
            renewal_tick_secs = self.renewal_tick.as_secs(),
            plan_count = self.plans.len(),
            ns_events_attached = ns_events.is_some(),
            "ACME provisioner renewal loop started"
        );
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick_once().await;
                }
                _ = notify.notified() => {
                    // Additive snapshot reconcile (C4a): pick up any
                    // namespace rows whose `VhostSet` events were
                    // dropped (channel full) or whose writer bypassed
                    // the hook channel entirely (raw props.set with
                    // the hook channel disconnected, exercised by
                    // V-O1's `notify_snapshot_reconcile_picks_up_*`
                    // test). The inverse path ("plan exists, row
                    // absent — clean up") is C4b territory and is
                    // NOT exercised here. The tick_once that follows
                    // is the existing renewal sweep — running it on a
                    // notify wake keeps the operator-force-renew
                    // semantic (any external `notify_one` drives both
                    // a reconcile pass and a renewal sweep).
                    self.snapshot_reconcile_additive().await;
                    self.tick_once().await;
                }
                event = async {
                    match ns_events.as_mut() {
                        Some(rx) => rx.recv().await,
                        // No receiver attached — park forever; the
                        // other two arms drive the loop. This is the
                        // "test path with no ns_events wiring" case;
                        // production wiring always reaches the Some
                        // arm above.
                        None => std::future::pending::<Option<NamespaceEvent>>().await,
                    }
                } => {
                    match event {
                        Some(event) => {
                            self.handle_namespace_event(event).await;
                            // Deliberately skip tick_once after a
                            // single namespace event: the event arm
                            // is per-row and already runs issue_one
                            // inline for fresh-add. Folding a full
                            // sweep onto every event would amplify a
                            // burst of operator writes into a burst
                            // of meta.json reads, which is exactly
                            // what the 6h interval is meant to
                            // smooth out. The interval arm is the
                            // sweep driver; the event arm is the
                            // freshness driver.
                            continue;
                        }
                        None => {
                            // Sender side closed — namespace
                            // registration was torn down or the
                            // receiver was never wired through.
                            // Disable the arm by dropping the local
                            // `ns_events` and let the interval +
                            // notify arms keep the loop alive. We
                            // log at WARN once (subsequent
                            // iterations short-circuit through the
                            // `None` branch in `async`) so the
                            // operator notices a wiring break
                            // without spamming the journal.
                            warn!(
                                "ACME provisioner ns_events channel closed — \
                                 disabling event arm; loop continues on \
                                 interval + notify only (durable recovery via \
                                 snapshot_reconcile_additive on next notify)"
                            );
                            ns_events = None;
                        }
                    }
                }
            }
        }
    }

    // =====================================================================
    // C4a — namespace event ingestion + fresh-add issuance + additive
    // snapshot reconcile.
    // =====================================================================

    /// C4a dispatch for one [`NamespaceEvent`] delivered through
    /// `self.ns_events`.
    ///
    /// `VhostSet`: every ACME-configured + enabled row is run through
    /// [`Self::apply_vhost_row`], which unconditionally inserts/
    /// updates the plan, then either retries a stale writeback, or
    /// runs `issue_one` + writeback for genuinely-new rows. See
    /// `apply_vhost_row`'s docstring for the full coverage matrix.
    ///
    /// `VhostRemoved`: WARN-log + drop. Per the rev-6 plan §"Commit
    /// 4a" the cleanup arm (archive + arc-swap rebuild +
    /// `self.plans` mutation) is deferred to C4b. A reviewer or
    /// bisecter running C4a in isolation gets a loud signal per
    /// missed delete; C4b's startup orphan-scan recovers persistent
    /// leaks across a restart.
    async fn handle_namespace_event(&mut self, event: NamespaceEvent) {
        match event {
            NamespaceEvent::VhostSet { fqdn, row } => {
                if let Err(e) = self.apply_vhost_row(&fqdn, *row).await {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME provisioner failed to apply VhostSet — leaving \
                         in-memory state for the next snapshot_reconcile or \
                         interval tick to retry"
                    );
                }
            }
            NamespaceEvent::VhostRemoved { fqdn } => {
                if let Err(e) = self.handle_vhost_removed(&fqdn).await {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME provisioner VhostRemoved cleanup failed — \
                         leaving in-memory state for the next reconcile pass"
                    );
                }
            }
        }
    }

    /// C4b — `VhostRemoved` cleanup arm. Five-step protocol
    /// (rev-6 plan §"Commit 4b"):
    ///
    /// 1. Idempotent short-circuit: if `self.plans` has no entry for
    ///    `fqdn` AND `acme_identities` has none either, the row was
    ///    already cleaned up (duplicate event, or never owned by ACME).
    /// 2. Acquire the per-FQDN lock so a concurrent C5 `vhost.add`
    ///    for the same fqdn blocks until the archive completes.
    /// 3. Pre-stale-event guard: re-read the namespace to confirm
    ///    the row is genuinely gone. A `VhostRemoved` followed by a
    ///    `vhost.add` re-add within the same tick must NOT archive
    ///    the freshly-issued cert.
    /// 4. Publish-before-mutate: republish `republish_vhost_directory`
    ///    (the namespace LIST already excludes the deleted fqdn) and
    ///    `republish_tls_config_for_plans(plans_without_fqdn)` BEFORE
    ///    mutating provisioner state. This guarantees no in-flight
    ///    request can land on an ACME identity whose key is about
    ///    to disappear.
    /// 5. Mutate + archive: drop the entry from `self.plans`,
    ///    `acme_identities`, `vhost_state`; archive
    ///    `acme/<fqdn>/live/` → `acme/<fqdn>/archive/<ts>/live/`
    ///    so the bytes survive for forensics while the resolver
    ///    no longer serves them.
    async fn handle_vhost_removed(&mut self, fqdn: &str) -> Result<()> {
        // Step 1 — idempotent short-circuit (Codex E.1).
        let plan_present = self.plans.iter().any(|p| p.fqdn == fqdn);
        let identity_present = self.acme_identities.contains_key(fqdn);
        if !plan_present && !identity_present {
            info!(
                fqdn = %fqdn,
                "VhostRemoved short-circuit: no plan/identity for fqdn — \
                 nothing to clean up (duplicate event, or non-ACME row)"
            );
            return Ok(());
        }

        // Step 2 — per-FQDN lock. Holds across the whole critical
        // section so a concurrent `vhost.add` blocks until the
        // archive completes.
        let _guard = self.acquire_fqdn_lock(fqdn).await;

        // Step 3 — pre-stale-event guard. Re-read the namespace to
        // confirm the row is genuinely gone. A `VhostRemoved`
        // followed by `vhost.add` within the same tick lands the
        // row back; in that case we MUST NOT archive the cert.
        //
        // Codex C4b rev-1 MAJOR fix: classify the `get` error.
        // ONLY `StoreError::NotFound` means "row is gone"; any
        // other error (transient I/O, lock contention, …) must
        // NOT collapse into "treat as gone" because that would
        // archive a live cert on a flaky read. Log + bail so the
        // next reconcile pass retries.
        if let Some(runtime) = self.vhosts_runtime.as_ref() {
            let key = RecordKey::collection(namespace_name(), fqdn.to_string());
            match runtime.store().get(&key).await {
                Ok(_) => {
                    info!(
                        fqdn = %fqdn,
                        "VhostRemoved pre-stale guard: row is back in the \
                         namespace (vhost.add re-added during VhostRemoved \
                         critical section) — skipping cleanup"
                    );
                    return Ok(());
                }
                Err(StoreError::NotFound) => {
                    // Expected: the row IS gone. Proceed to publish.
                }
                Err(e) => {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "VhostRemoved pre-stale guard: namespace read failed \
                         (non-NotFound) — bailing rather than archiving a \
                         live cert on a flaky read; next reconcile pass retries"
                    );
                    return Ok(());
                }
            }
        }

        // Step 4 — publish-before-mutate. The namespace LIST
        // already excludes the deleted fqdn (the delete tx
        // committed before this event fired), so the VhostDirectory
        // republish reads a candidate set without it.
        //
        // Spec order: TLS first, then directory (see
        // `_doc/planned/webd-vhosts-phase3.md` § "Two distinct
        // archive helpers" + Codex C4b clarification). The TLS
        // republish must observe a plan set WITHOUT the removed
        // fqdn so the identity-emission filter excludes the
        // about-to-be-archived ACME identity; the actual
        // `self.plans` mutation happens in step 5.
        let plans_without: Vec<AcmeVhostPlan> = self
            .plans
            .iter()
            .filter(|p| p.fqdn != fqdn)
            .cloned()
            .collect();
        self.republish_tls_config_for_plans(&plans_without)?;
        self.republish_vhost_directory().await?;

        // Step 4b — POST-publish stale guard. A `vhost.add` that
        // landed AFTER the pre-stale guard but BEFORE the
        // publishes completed would put the row back; in that
        // case the publishes already excluded the fqdn (the
        // re-add lost the publish race), but archiving the live
        // cert + clearing in-memory state would still be wrong —
        // the next reconcile would re-issue from scratch and
        // throw away a perfectly good cert. Same NotFound-only
        // semantics as the pre-stale guard.
        //
        // Codex C4b rev-1 BLOCKER fix: spec requires a SECOND
        // `get` under the per-FQDN lock before archive.
        if let Some(runtime) = self.vhosts_runtime.as_ref() {
            let key = RecordKey::collection(namespace_name(), fqdn.to_string());
            match runtime.store().get(&key).await {
                Ok(_) => {
                    info!(
                        fqdn = %fqdn,
                        "VhostRemoved post-stale guard: row re-added \
                         between pre-guard and publishes (lost the publish \
                         race) — skipping archive; next reconcile will \
                         republish with the re-added row included"
                    );
                    return Ok(());
                }
                Err(StoreError::NotFound) => {
                    // Expected: still gone. Proceed to archive.
                }
                Err(e) => {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "VhostRemoved post-stale guard: namespace read failed \
                         (non-NotFound) — bailing rather than archiving a \
                         live cert on a flaky read; publishes already landed \
                         the row's exclusion, next reconcile pass retries"
                    );
                    return Ok(());
                }
            }
        }

        // Step 5 — mutate + archive. Drop in-memory state, then
        // archive the on-disk live cert.
        self.plans.retain(|p| p.fqdn != fqdn);
        self.acme_identities.remove(fqdn);
        // Codex E.5 — also drop the vhost_state entry so a future
        // re-add starts from a clean backoff slate.
        self.vhost_state.remove(fqdn);

        if let Err(e) = self.archive_live_dir(fqdn) {
            warn!(
                fqdn = %fqdn,
                error = %e,
                "VhostRemoved cleanup: archive_live_dir failed — in-memory \
                 state already cleared; orphan-scan on next startup will \
                 reclaim the directory"
            );
        }
        self.publish_tls_status();
        Ok(())
    }

    /// C4b — archive the live/ directory for a single fqdn under
    /// `acme/<fqdn>/archive/<ts>/live/`. Idempotent: a missing
    /// `live/` returns Ok (idempotent re-fire of the cleanup arm).
    ///
    /// Distinct from [`archive_orphan_acme_dirs`] which archives a
    /// whole `<fqdn>/` tree at startup. This helper preserves the
    /// `archive/`, `staging/` siblings so a future restore can
    /// inspect prior issuance history.
    fn archive_live_dir(&self, fqdn: &str) -> std::io::Result<()> {
        let fqdn_dir = self.acme_dir.join(fqdn);
        let live = fqdn_dir.join(LIVE_DIR);
        if !live.try_exists()? {
            return Ok(());
        }
        let archive_root = fqdn_dir.join(ARCHIVE_DIR);
        std::fs::create_dir_all(&archive_root)?;
        let now = OffsetDateTime::now_utc();
        let stamp = format_stamp(now);
        let archive_ts = unique_path_in(&archive_root, &stamp)?;
        std::fs::create_dir(&archive_ts)?;
        let dest = archive_ts.join(LIVE_DIR);
        std::fs::rename(&live, &dest)?;
        sync_dir(&fqdn_dir)?;
        sync_dir(&archive_root)?;
        sync_dir(&archive_ts)?;
        info!(
            fqdn = %fqdn,
            archive = %dest.display(),
            "VhostRemoved: archived live/ for removed fqdn"
        );
        Ok(())
    }

    /// C4a — apply one `VhostSet` row to the provisioner's in-memory
    /// state. Returns `Err` only on the namespace-writeback path
    /// (issuance failure is recorded against `self.vhost_state` and
    /// returned as `Ok` so the caller's WARN-log doesn't double-fire
    /// against an already-journalled backoff advance).
    ///
    /// Plan-registration gates (skipping the row entirely):
    /// 1. `event_fqdn != row.fqdn` → substrate-invariant violation
    ///    (the `before_set` rule pins these equal on every commit);
    ///    log + skip rather than risk writing back to the wrong key.
    /// 2. `enabled == false` → row is administratively dropped from
    ///    routing per the directory contract; no plan, no issuance.
    /// 3. No ACME columns set → manual-PEM or no-TLS vhost, not our
    ///    territory.
    ///
    /// **Plan registration is unconditional for enabled ACME rows
    /// with a valid triple** — even when the row is already covered.
    /// Splitting plan registration from issuance is the rev-2 Codex
    /// MAJOR fix: post-restart `bus_runtime` rows with a populated
    /// `cert_blob_id` must still land in `self.plans` so the renewal
    /// tick observes them. The spec at `_doc/planned/webd-vhosts-
    /// phase3.md` §"Commit 4a" pins this exact shape — "locate or
    /// insert the matching `AcmeVhostPlan`; if `acme_*` set and
    /// `cert_blob_id` is `None`, run `issue_one`".
    ///
    /// Issuance-vs-writeback-retry split (Codex C4a rev-3 MAJOR fix
    /// — the naive `is_covered` collapse masked the case where
    /// in-memory issuance succeeded but writeback failed, leaving
    /// the writeback un-retried for the process lifetime):
    /// 4a. `self.acme_identities.contains_key(fqdn)` AND
    ///     `row.cert_blob_id.is_none()` → in-memory identity present
    ///     but writeback never landed; **retry writeback** and return.
    /// 4b. `self.acme_identities.contains_key(fqdn)` AND
    ///     `row.cert_blob_id.is_some()` → fully covered; return.
    /// 4c. `row.cert_blob_id.is_some()` (acme_identities empty —
    ///     post-restart bus_runtime case) → covered for
    ///     renewal-scheduling purposes; the on-disk PEM survives
    ///     under `live/`. Loading it back into `acme_identities` so
    ///     the resolver can serve it is C4b's load-runtime-
    ///     identities arm. Return.
    /// 4d. Neither set → fresh-add path. Continue to cooldown gate
    ///     + `issue_one` + writeback.
    ///
    /// On a fresh-issue success: populate `self.acme_identities`,
    /// republish the TLS resolver, write back the daemon-owned
    /// columns via `Runtime::set_with_origin(.., WriteOrigin::
    /// backend())`. On failure: advance the per-vhost backoff (same
    /// shape as `tick_once`'s failure arm) and journal-warn — the
    /// existing resolver snapshot keeps serving (no manual-PEM
    /// identity affected since this was a fresh add).
    async fn apply_vhost_row(&mut self, fqdn: &str, row: VhostRow) -> Result<()> {
        // Codex MINOR: pin the substrate invariant. `before_set`
        // rule 2 enforces `row.fqdn == ctx.key.key` on every
        // committed write; a mismatched event is a substrate bug.
        // Skip rather than write back to the wrong key.
        if fqdn != row.fqdn {
            warn!(
                event_fqdn = %fqdn,
                row_fqdn = %row.fqdn,
                "ACME apply_vhost_row: event/row fqdn mismatch \
                 (before_set rule 2 violation) — skipping"
            );
            return Ok(());
        }
        if !row.enabled {
            return Ok(());
        }
        // B1 fail-soft: `resolve_node_state` disabled this vhost at
        // startup (bad cert / missing www_dir / unopenable CMS DB). Its
        // `config_bootstrap` namespace row survives (enabled=true) so it
        // resurrects on the next webd restart — but until then the
        // provisioner must NOT re-adopt a plan for it via the startup
        // reconcile (`startup_pass` → here) or the live event loop
        // (`run_forever` → here). Re-adopting would try to issue/serve a
        // vhost the resolver deemed unservable and diverge from the
        // disabled set `republish_vhost_directory` already honours. The
        // set is frozen at startup, matching the documented
        // "repair + restart" recovery contract.
        if self.disabled_hosts.contains(fqdn) {
            info!(
                fqdn = %fqdn,
                "ACME apply_vhost_row: fqdn is fail-soft-disabled \
                 (per-vhost validation failed at resolve); skipping plan \
                 adoption until repair + restart"
            );
            return Ok(());
        }
        let plan = match vhost_row_to_acme_plan(&row)? {
            Some(plan) => plan,
            None => return Ok(()),
        };
        // Locate or insert the plan in `self.plans` so future renewal
        // ticks observe it. If a same-FQDN plan already exists (e.g.
        // bootstrap inserted it then the operator flipped an ACME
        // field through props.set), overwrite — the namespace row is
        // the source of truth post-bootstrap. Done BEFORE the
        // coverage split per the Codex rev-2 MAJOR fix (plan
        // registration is unconditional for enabled ACME rows).
        if let Some(existing) = self.plans.iter_mut().find(|p| p.fqdn == plan.fqdn) {
            *existing = plan.clone();
        } else {
            self.plans.push(plan.clone());
            // The per-vhost cooldown map is keyed by FQDN; a fresh
            // plan starts with the default zero-failure state.
            self.vhost_state.entry(plan.fqdn.clone()).or_default();
        }
        // Codex C4a rev-3 MAJOR fix: split "covered for issuance"
        // from "needs daemon writeback". The naive `is_covered`
        // gate masked the case where `acme_identities` is populated
        // (in-process issuance succeeded) but `cert_blob_id` is
        // still absent (the first writeback failed with a transient
        // error or a `VersionMismatch`). Without this split, every
        // subsequent event/reconcile pass short-circuited and the
        // writeback never retried — the cert would remain visible
        // only for the current process lifetime.
        //
        // Order matters: the `acme_identities` branch is checked
        // first because it captures the "writeback retry needed"
        // case; the `cert_blob_id` branch is the post-restart
        // already-covered case (no retry needed, the daemon
        // writeback already landed in a prior run).
        if self.acme_identities.contains_key(fqdn) {
            // Two writeback-retry triggers:
            // 4a. `cert_blob_id` absent — first writeback never landed.
            // 4a'. `cert_blob_id` present BUT `not_after` lags the
            //      on-disk meta — a renewal-tick writeback failed
            //      (transient VersionMismatch, store error, …) and
            //      `tick_once` will not re-enter issuance until the
            //      already-extended live cert approaches the renewal
            //      window AGAIN. Codex C4b rev-1 BLOCKER fix.
            let needs_writeback = if row.cert_blob_id.is_none() {
                Some("cert_blob_id absent — prior writeback never landed")
            } else {
                match self.read_live_not_after_rfc3339(fqdn) {
                    Some(disk_not_after) => {
                        let row_not_after = row.not_after.as_deref();
                        if row_not_after != Some(disk_not_after.as_str()) {
                            Some(
                                "row.not_after disagrees with live meta.json — \
                                  renewal-tick writeback failed; retrying",
                            )
                        } else {
                            None
                        }
                    }
                    None => {
                        // Disk meta unreadable. Cannot decide;
                        // leave for next pass.
                        None
                    }
                }
            };
            if let Some(reason) = needs_writeback {
                info!(
                    fqdn = %fqdn,
                    reason,
                    "ACME apply_vhost_row: retrying writeback"
                );
                // C4b — Codex design-pass E.2: the writeback retry path
                // MUST run BOTH publishes before retrying the
                // namespace write. The prior failure may have left an
                // identity in `self.acme_identities` whose arc-swap
                // publish never happened, and `from_namespace_rows`
                // may need a fresh read against the post-bootstrap
                // namespace shape. Republish both arcs first; on
                // either Err, skip the writeback so the next tick
                // retries the whole sequence.
                if let Err(e) = self.republish_tls_config() {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME apply_vhost_row: TLS republish failed during \
                         writeback retry — leaving for next tick"
                    );
                    return Ok(());
                }
                if let Err(e) = self.republish_vhost_directory().await {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME apply_vhost_row: VhostDirectory republish failed \
                         during writeback retry — leaving for next tick"
                    );
                    return Ok(());
                }
                if let Err(e) = self.writeback_issued_row(fqdn).await {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME apply_vhost_row: writeback retry failed; \
                         the next event or snapshot reconcile will retry"
                    );
                }
            }
            return Ok(());
        }
        if row.cert_blob_id.is_some() {
            return Ok(());
        }
        let now = OffsetDateTime::now_utc();
        let now_unix_secs = now.unix_timestamp();
        let Ok(now_unix_secs_u64) = u64::try_from(now_unix_secs) else {
            warn!(
                fqdn = %fqdn,
                now_unix = now_unix_secs,
                "ACME apply_vhost_row observed pre-Unix-epoch clock — skipping \
                 fresh-issue attempt (the next notify or interval tick once \
                 NTP catches up will retry)"
            );
            return Ok(());
        };
        let now_unix = UnixTime::since_unix_epoch(Duration::from_secs(now_unix_secs_u64));
        // Cooldown gate, mirroring `tick_once`. A row that just landed
        // after a retry-during-cooldown should not burn another
        // attempt against the same backoff bucket.
        if let Some(state) = self.vhost_state.get(&plan.fqdn)
            && let Some(next) = state.next_attempt_after
            && next > now
        {
            info!(
                fqdn = %plan.fqdn,
                next_attempt_after = %next,
                "ACME apply_vhost_row: vhost in cooldown — deferring fresh-issue"
            );
            return Ok(());
        }
        info!(
            fqdn = %plan.fqdn,
            provider = ?plan.provider,
            "ACME apply_vhost_row: fresh-issue attempt for newly-observed row"
        );
        match self.issue_one(&plan, now, now_unix).await {
            Ok(identity) => {
                // Update `acme_identities` BEFORE the writeback +
                // republish so a duplicate VhostSet delivered during
                // the writeback await sees the in-memory identity
                // (the at-least-once delivery pin) and falls into
                // the coverage-split arm at the top of
                // `apply_vhost_row` rather than re-running
                // `issue_one`.
                self.acme_identities.insert(plan.fqdn.clone(), identity);
                let st = self.vhost_state.entry(plan.fqdn.clone()).or_default();
                st.last_error = None;
                st.last_error_count = 0;
                st.next_attempt_after = None;
                // C4b — Codex rev 2 BLOCKER: arc-swap publishes BEFORE
                // namespace writeback. If either republish fails, the
                // namespace write is skipped so renewal state stays
                // pre-publish; the coverage-split arm above retries
                // both republishes + writeback on the next event /
                // reconcile pass.
                if let Err(e) = self.republish_tls_config() {
                    warn!(
                        fqdn = %plan.fqdn,
                        error = %e,
                        "ACME apply_vhost_row: TLS republish failed \
                         post-issuance — writeback deferred until next tick"
                    );
                    self.publish_tls_status();
                    return Ok(());
                }
                if let Err(e) = self.republish_vhost_directory().await {
                    warn!(
                        fqdn = %plan.fqdn,
                        error = %e,
                        "ACME apply_vhost_row: VhostDirectory republish failed \
                         post-issuance — writeback deferred until next tick"
                    );
                    self.publish_tls_status();
                    return Ok(());
                }
                // Writeback. Failure here is not fatal: the
                // coverage split at the top of `apply_vhost_row`
                // detects "acme_identities populated + cert_blob_id
                // still None" on the next event/reconcile pass and
                // retries the writeback (now with both republishes
                // first) against the new version.
                let writeback_res = self.writeback_issued_row(&plan.fqdn).await;
                self.publish_tls_status();
                writeback_res
            }
            Err(e) => {
                let st = self.vhost_state.entry(plan.fqdn.clone()).or_default();
                st.last_error_count = st.last_error_count.saturating_add(1);
                let count = st.last_error_count;
                let next = next_attempt_after_failure(now, count);
                st.next_attempt_after = Some(next);
                st.last_error = Some(format!("{e:#}"));
                warn!(
                    fqdn = %plan.fqdn,
                    error = %e,
                    count,
                    next_attempt_after = %next,
                    "ACME apply_vhost_row: fresh-issue failed — backing off"
                );
                self.publish_tls_status();
                Ok(())
            }
        }
    }

    /// C4a — write the daemon-owned columns (`cert_blob_id`,
    /// `key_blob_id`, `not_after`) back to the namespace row that
    /// triggered a successful fresh-issuance.
    ///
    /// Uses [`MergeMode::Patch`] so the caller-writable columns
    /// (`enabled`, `www_dir`, `aliases`, `acme_*`, etc.) survive the
    /// write untouched. The `webd.vhosts` namespace pins
    /// `require_version = true`, so the writeback must read the
    /// current row's version + patch with `expected_version:
    /// Some(current.version)` — backend origin does **not** bypass
    /// the `require_version` gate (see `Runtime::set_with_origin`'s
    /// SPEC 12 §4.4 enforcement at runtime.rs:243-246). If the row
    /// was deleted between `issue_one` and the writeback (operator
    /// called `vhost.remove`), the read returns `NotFound` and we
    /// log + bail — C4b's removal arm cleans up the on-disk
    /// staging. If the version mismatched (concurrent operator
    /// patch landed), we log + bail; the next snapshot reconcile
    /// retries the writeback against the new version.
    ///
    /// Codex C4a review BLOCKER fix (rev 2): a prior draft used
    /// `expected_version: None` and would have been refused
    /// pre-hooks for every writeback. The test suite did not catch
    /// it because the V-O1 hermetic plans terminate at the
    /// `tos_acceptance: None` pre-network gate before reaching
    /// `issue_one`'s Ok arm.
    ///
    /// `cert_blob_id` / `key_blob_id` carry the on-disk live path as
    /// the marker value (no CAS-blob substrate yet — that's a later
    /// phase). The prefix `fs:` documents the indirection so a future
    /// CAS-backed scheme can differentiate via a different prefix
    /// without an additional schema column.
    /// Read `live/meta.json` for `fqdn` and return its
    /// `not_after_unix` formatted as RFC3339 — the same format the
    /// namespace's `not_after` column carries. Returns `None` when
    /// the file is missing/unreadable or the value is out of range
    /// (treated by callers as "cannot decide; defer to next pass"
    /// rather than as a hard error). Used by `apply_vhost_row`'s
    /// 4b branch to detect a stale `not_after` column after a
    /// renewal-tick writeback failed.
    fn read_live_not_after_rfc3339(&self, fqdn: &str) -> Option<String> {
        let live_meta = self.acme_dir.join(fqdn).join(LIVE_DIR).join(META_FILE);
        let meta = read_meta_json(&live_meta).ok()?;
        let secs_u64 = u64::try_from(meta.not_after_unix).ok()?;
        let dt = OffsetDateTime::from_unix_timestamp(secs_u64 as i64).ok()?;
        dt.format(&time::format_description::well_known::Rfc3339)
            .ok()
    }

    async fn writeback_issued_row(&self, fqdn: &str) -> Result<()> {
        let Some(runtime) = self.vhosts_runtime.as_ref() else {
            return Ok(());
        };
        let live_dir = self.acme_dir.join(fqdn).join(LIVE_DIR);
        let cert_marker = format!("fs:{}", live_dir.join(FULLCHAIN_FILE).to_string_lossy());
        let key_marker = format!("fs:{}", live_dir.join(PRIVKEY_FILE).to_string_lossy());
        // Read not_after from the just-written meta.json — `issue_one`
        // promoted it under live/ before returning Ok. Using the
        // file as the source of truth (rather than threading the
        // value out of issue_one) keeps the writeback's contract
        // symmetric with what startup_pass observes on the next
        // boot.
        let live_meta = live_dir.join(META_FILE);
        let not_after_rfc3339 = match read_meta_json(&live_meta) {
            Ok(meta) => {
                let secs = meta.not_after_unix;
                let secs_u64 = match u64::try_from(secs) {
                    Ok(v) => v,
                    Err(_) => {
                        warn!(
                            fqdn = %fqdn,
                            not_after_unix = secs,
                            "ACME writeback: meta not_after_unix is negative — \
                             skipping not_after column update"
                        );
                        return Ok(());
                    }
                };
                let dt = match OffsetDateTime::from_unix_timestamp(secs_u64 as i64) {
                    Ok(dt) => dt,
                    Err(e) => {
                        warn!(
                            fqdn = %fqdn,
                            not_after_unix = secs,
                            error = %e,
                            "ACME writeback: meta not_after_unix out of range — \
                             skipping not_after column update"
                        );
                        return Ok(());
                    }
                };
                dt.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| dt.to_string())
            }
            Err(e) => {
                warn!(
                    fqdn = %fqdn,
                    error = %e,
                    "ACME writeback: live meta.json unreadable post-issuance — \
                     writeback skipped (the cert is still on disk; \
                     in-memory acme_identities will keep the resolver \
                     serving for this process lifetime, and the coverage \
                     split in `apply_vhost_row` will retry the writeback \
                     on the next event)"
                );
                return Ok(());
            }
        };
        let key = RecordKey::collection(namespace_name(), fqdn.to_string());
        // OCC pin: read the current version BEFORE the patch.
        // `require_version: true` rejects writes without a version,
        // even on backend origin. NotFound here means the operator
        // raced a delete in between `issue_one` and the writeback;
        // C4b's removal arm owns staging cleanup, so we log and
        // bail rather than fail the issuance result.
        let current_version: Version = match runtime.store().get(&key).await {
            Ok(snap) => snap.value.version,
            Err(e) => {
                warn!(
                    fqdn = %fqdn,
                    error = %e,
                    "ACME writeback: row absent or unreadable post-issuance \
                     (likely a concurrent vhost.remove) — writeback skipped, \
                     in-memory acme_identities entry remains for the current \
                     process lifetime; the next process restart will see no \
                     cert_blob_id and re-attempt if the row is restored"
                );
                return Ok(());
            }
        };
        let mut patch = std::collections::BTreeMap::new();
        patch.insert("cert_blob_id".to_string(), PropValue::String(cert_marker));
        patch.insert("key_blob_id".to_string(), PropValue::String(key_marker));
        patch.insert(
            "not_after".to_string(),
            PropValue::String(not_after_rfc3339),
        );
        let body = PropValue::Object(patch);
        let ts_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(0);
        runtime
            .set_with_origin(
                key,
                body,
                SetOpts {
                    // Version-pinned per the method-level docs.
                    expected_version: Some(current_version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("acme_runtime_writeback".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
            .map_err(|e| anyhow!("webd.vhosts row writeback failed: {e}"))?;
        // C4b — capture the namespace-write moment for the V-O2
        // ordering pin. The fetch_add runs on the shared seq counter
        // so the captured value is strictly greater than the two
        // arc-swap publish slots captured earlier in the same
        // issuance critical section (both publishes always precede
        // this writeback by construction; see the
        // `republish_*` calls in `apply_vhost_row` / `tick_once`).
        if let Some(instr) = &self.publish_instrumentation {
            let seq = instr.seq.fetch_add(1, Ordering::SeqCst);
            instr.namespace_write_seq.store(seq, Ordering::SeqCst);
        }
        Ok(())
    }

    /// C4a — additive snapshot reconcile.
    ///
    /// Read the full `webd.vhosts` namespace via
    /// `Runtime::store().list`, then call `apply_vhost_row` for every
    /// row. `apply_vhost_row`'s coverage split (acme_identities-or-
    /// writeback-retry vs cert_blob_id-already-persisted) ensures
    /// only genuinely-new rows trigger issuance; the rest are
    /// either no-ops or writeback retries.
    ///
    /// This is the durability fix for dropped `VhostSet` events
    /// (channel full → try_send Full → WARN+drop), and also covers
    /// the raw `props.set` path where a caller bypasses the
    /// `vhost.add` ergonomic verb and writes directly through the
    /// property surface. The hook channel still fires in that case,
    /// but a test that wires the hook with a disconnected receiver
    /// (V-O1 `notify_snapshot_reconcile_picks_up_*`) demonstrates the
    /// reconcile arm is the load-bearing recovery path.
    ///
    /// **Additive only**: the inverse path ("plan exists in-memory
    /// but row absent from the namespace — clean up") is C4b
    /// territory. A row deletion that lands while C4a is the active
    /// commit leaves the plan in `self.plans` and the live cert
    /// untouched; C4b's startup orphan-scan recovers across a
    /// restart, and the runtime cleanup arm + ordering pin recover
    /// within the same process.
    async fn snapshot_reconcile_additive(&mut self) {
        let Some(runtime) = self.vhosts_runtime.clone() else {
            return;
        };
        let rows = match crate::vhosts_namespace::snapshot_rows(&runtime).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(
                    error = %e,
                    "ACME snapshot_reconcile_additive: namespace list failed — \
                     skipping pass (next notify or interval tick will retry)"
                );
                return;
            }
        };
        // C4b — additive pass.
        let mut row_fqdns: HashSet<String> = HashSet::with_capacity(rows.len());
        for row in rows {
            let fqdn = row.fqdn.clone();
            row_fqdns.insert(fqdn.clone());
            if let Err(e) = self.apply_vhost_row(&fqdn, row).await {
                warn!(
                    fqdn = %fqdn,
                    error = %e,
                    "ACME snapshot_reconcile_additive: apply_vhost_row failed — \
                     leaving for next reconcile / tick"
                );
            }
        }
        // C4b cleanup pass — detect "plan in-memory but row gone"
        // (the recovery path for a dropped `VhostRemoved` event;
        // bounded mpsc backpressure can in principle drop one if
        // the consumer is slow). Treat the missing row the same
        // shape as a fresh `VhostRemoved`: run the same handler so
        // the publish-before-mutate ordering, per-fqdn lock, and
        // archive land identically.
        let plan_fqdns: Vec<String> = self.plans.iter().map(|p| p.fqdn.clone()).collect();
        for fqdn in plan_fqdns {
            if row_fqdns.contains(&fqdn) {
                continue;
            }
            info!(
                fqdn = %fqdn,
                "snapshot_reconcile cleanup: plan in-memory but row absent — \
                 running VhostRemoved handler"
            );
            if let Err(e) = self.handle_vhost_removed(&fqdn).await {
                warn!(
                    fqdn = %fqdn,
                    error = %e,
                    "snapshot_reconcile cleanup: handle_vhost_removed failed — \
                     leaving for next reconcile"
                );
            }
        }
    }

    /// Single renewal-tick sweep. Walks every plan; for each one
    /// inside the 30-day renewal window whose cooldown is past,
    /// runs the same five-step transaction as `startup_pass`. On
    /// success: replaces the per-FQDN entry in `self.acme_identities`
    /// and (after the loop) rebuilds + publishes the resolver. On
    /// failure: advances per-vhost backoff, logs warn, leaves the
    /// previous live cert serving via the unchanged ArcSwap.
    ///
    /// Never returns `Err` — every failure is recorded against
    /// `vhost_state` and `republish_tls_config` is skipped if no
    /// renewal succeeded this tick. Pre-Unix-epoch wall-clock skips
    /// the whole sweep rather than crashing the loop (defensive: a
    /// system clock that wandered backward across boots is recoverable
    /// once NTP catches up; crashing the renewal loop is not).
    async fn tick_once(&mut self) {
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        let now = OffsetDateTime::now_utc();
        let now_unix_secs = now.unix_timestamp();
        let Ok(now_unix_secs_u64) = u64::try_from(now_unix_secs) else {
            warn!(
                now_unix = now_unix_secs,
                "ACME renewal tick observed pre-Unix-epoch clock — skipping sweep"
            );
            return;
        };
        let now_unix = UnixTime::since_unix_epoch(Duration::from_secs(now_unix_secs_u64));
        let plans = self.plans.clone();
        // Drain the operator-driven force-renew queue once per tick.
        // Drained entries bypass both the cooldown and renewal-window
        // gates below; entries queued mid-tick land in the next sweep.
        // Single-shot: if the issuance fails, cooldown re-engages and
        // the verb caller must re-queue to retry.
        let forced: HashSet<String> = {
            let mut guard = self.force_renew_fqdns.lock().await;
            std::mem::take(&mut *guard)
        };
        let mut any_renewed = false;
        // Codex C4b rev-1 BLOCKER fix: capture each successfully-
        // renewed fqdn so the `not_after` column can be written
        // back AFTER both arc-swap publishes succeed. Without this,
        // a successful renewal pushes the live cert's `not_after`
        // outside the 30d renewal window but the namespace row's
        // `not_after` column stays at the old (now-stale) expiry,
        // and the operator-facing `webd.tls.status` projection
        // disagrees with the on-disk truth until the next process
        // restart.
        let mut renewed_fqdns: Vec<String> = Vec::new();
        for plan in &plans {
            let force_this_fqdn = forced.contains(&plan.fqdn);
            // Cooldown gate: a vhost whose `next_attempt_after` is
            // still in the future skips this tick. Without this
            // check, a stuck vhost would burn an order attempt on
            // every interval regardless of the backoff schedule.
            //
            // Force-renew (operator-driven via `queue_force_renew`)
            // bypasses this gate for the queued fqdn — operators
            // overriding the schedule are accepting the LE issuance-
            // budget cost in exchange for explicit intent.
            if !force_this_fqdn
                && let Some(state) = self.vhost_state.get(&plan.fqdn)
                && let Some(next) = state.next_attempt_after
                && next > now
            {
                continue;
            }
            // Read live meta; classify renewal need by `not_after -
            // now`. NotFound / corruption is a runtime anomaly
            // (operator wiped state, disk failure, …) — log and skip
            // rather than burning an LE issuance budget on every tick
            // until restart.
            let live_meta = self
                .acme_dir
                .join(&plan.fqdn)
                .join(LIVE_DIR)
                .join(META_FILE);
            let meta = match read_meta_json(&live_meta) {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        fqdn = %plan.fqdn,
                        error = %e,
                        "ACME renewal tick: live meta unreadable — skipping \
                         (restart to recover via startup_pass)"
                    );
                    // Record the meta-read failure in `last_error`
                    // so `webd.tls.status` surfaces stale-state
                    // detail. `last_error_count` / `next_attempt_after`
                    // are deliberately left alone — those govern LE
                    // issuance backoff, and a local disk anomaly
                    // doesn't belong on that schedule (every tick
                    // would fail until operator intervention, no
                    // amount of waiting helps). A subsequent
                    // successful renewal clears `last_error` via the
                    // success branch's reset.
                    let st = self.vhost_state.entry(plan.fqdn.clone()).or_default();
                    st.last_error = Some(format!("live meta unreadable: {e:#}"));
                    self.publish_tls_status();
                    continue;
                }
            };
            let secs_to_expiry = meta.not_after_unix.saturating_sub(now_unix_secs);
            // Renewal-window gate: skip certs whose remaining lifetime
            // is still safely outside the 30d window. Force-renew
            // bypasses this for the queued fqdn — operators issuing
            // an early renewal know they're shortening the rotation
            // cadence by their explicit request.
            if !force_this_fqdn && secs_to_expiry > RENEWAL_WINDOW.as_secs() as i64 {
                continue;
            }
            info!(
                fqdn = %plan.fqdn,
                not_after_unix = meta.not_after_unix,
                secs_to_expiry,
                forced = force_this_fqdn,
                "ACME renewal tick: issuing"
            );
            match self.issue_one(plan, now, now_unix).await {
                Ok(identity) => {
                    self.acme_identities.insert(plan.fqdn.clone(), identity);
                    let st = self.vhost_state.entry(plan.fqdn.clone()).or_default();
                    st.last_error = None;
                    st.last_error_count = 0;
                    st.next_attempt_after = None;
                    any_renewed = true;
                    renewed_fqdns.push(plan.fqdn.clone());
                    // Publish AFTER mutating both `acme_identities`
                    // and `vhost_state` so the snapshot reflects the
                    // full success transition atomically.
                    self.publish_tls_status();
                }
                Err(e) => {
                    let st = self.vhost_state.entry(plan.fqdn.clone()).or_default();
                    st.last_error_count = st.last_error_count.saturating_add(1);
                    let count = st.last_error_count;
                    let next = next_attempt_after_failure(now, count);
                    st.next_attempt_after = Some(next);
                    st.last_error = Some(format!("{e:#}"));
                    warn!(
                        fqdn = %plan.fqdn,
                        error = %e,
                        count,
                        next_attempt_after = %next,
                        "ACME renewal failed — backing off"
                    );
                    // Publish on failure too — operators want fresh
                    // `last_error` / `next_attempt_after_rfc3339`
                    // detail. Without this, a stuck vhost would only
                    // surface its failure once a later success
                    // landed (which by definition might never).
                    self.publish_tls_status();
                }
            }
        }
        // Codex C4b rev-2 BLOCKER fix: convergence sweep for stale
        // `not_after`. The renewal-tick interval is the ONLY arm
        // that fires on a pure timer (snapshot_reconcile_additive
        // runs only on `notify.notified()`). Once a renewed cert's
        // `live/meta.json` carries a far-future expiry, the
        // renewal-window gate above causes the loop to skip the
        // row, so a prior writeback failure (transient
        // VersionMismatch, store I/O error) would leave `not_after`
        // stale until the cert approaches the renewal window AGAIN
        // — i.e. effectively forever for a non-pathological cert.
        //
        // Walk every plan with an in-memory identity, compare the
        // disk's `not_after` against the namespace row, and add
        // any laggards to the writeback target list. The
        // disk-vs-namespace comparison is the same convergence
        // signal `apply_vhost_row`'s 4a' branch uses.
        let mut lagging_fqdns: Vec<String> = Vec::new();
        if let Some(runtime) = self.vhosts_runtime.as_ref() {
            for plan in &plans {
                // Skip rows that we're already going to writeback
                // via the renewal path.
                if renewed_fqdns.iter().any(|r| r == &plan.fqdn) {
                    continue;
                }
                // Need an in-memory identity to writeback — the
                // 4a' branch's invariant. (Fresh rows without an
                // identity yet are issuance candidates, not
                // writeback candidates; the loop above already
                // handled them.)
                if !self.acme_identities.contains_key(&plan.fqdn) {
                    continue;
                }
                let Some(disk_not_after) = self.read_live_not_after_rfc3339(&plan.fqdn) else {
                    continue;
                };
                let key = RecordKey::collection(namespace_name(), plan.fqdn.clone());
                let row_not_after = match runtime.store().get(&key).await {
                    Ok(snap) => match vhost_row_from_value(&snap.value.value) {
                        Ok(row) => row.not_after,
                        Err(_) => continue,
                    },
                    Err(StoreError::NotFound) => continue,
                    Err(e) => {
                        warn!(
                            fqdn = %plan.fqdn,
                            error = %e,
                            "ACME renewal-tick convergence sweep: \
                             namespace read failed; deferring"
                        );
                        continue;
                    }
                };
                if row_not_after.as_deref() != Some(disk_not_after.as_str()) {
                    info!(
                        fqdn = %plan.fqdn,
                        row_not_after = ?row_not_after,
                        disk_not_after = %disk_not_after,
                        "ACME renewal-tick convergence sweep: not_after \
                         lag detected — queuing writeback"
                    );
                    lagging_fqdns.push(plan.fqdn.clone());
                }
            }
        }

        if any_renewed || !lagging_fqdns.is_empty() {
            // C4b E.3: both publishes must succeed before the
            // tick is considered "advanced" — the writeback
            // retry happens on the next tick via apply_vhost_row.
            // WARN-and-return preserves pre-publish state so the
            // retry has consistent inputs.
            if let Err(e) = self.republish_tls_config() {
                warn!(error = ?e, "republish_tls_config failed in renewal tick — skipping vhost-directory republish, next tick retries");
                return;
            }
            if let Err(e) = self.republish_vhost_directory().await {
                warn!(error = ?e, "republish_vhost_directory failed in renewal tick — next tick retries");
                return;
            }
            // Codex C4b rev-1 BLOCKER fix: writeback the freshly-
            // issued `not_after` (+ blob markers — harmless on
            // re-write, the markers point at the same `live/`
            // paths) for each successfully-renewed row. Runs
            // AFTER both publishes succeed so an arc-swap
            // republish failure does not leave a stale `not_after`
            // visible to operators. Writeback failures here log
            // and continue — the next tick's convergence sweep
            // will retry the laggards.
            for fqdn in renewed_fqdns.iter().chain(lagging_fqdns.iter()) {
                if let Err(e) = self.writeback_issued_row(fqdn).await {
                    warn!(
                        fqdn = %fqdn,
                        error = %e,
                        "ACME renewal-tick writeback failed — row's \
                         not_after stays at the prior expiry until the \
                         next convergence sweep retries"
                    );
                }
            }
        }
    }

    /// Rebuild each listener's [`SniCertResolver`] from the union of
    /// manual-PEM (`base_identities`) and current ACME
    /// (`acme_identities`) identities — **filtered through
    /// `self.plans`** so an ACME identity whose plan has been
    /// removed (C4b `VhostRemoved` arm) no longer serves on the
    /// TLS surface — partitioned by [`Self::fqdn_to_listener`] and
    /// swapped into the matching [`ListenerTls`]. No-op (Ok) if no
    /// listeners were attached (test paths).
    ///
    /// Returns [`PublishError::Build`] on builder failure
    /// (Codex rev 2 BLOCKER fix: the namespace write MUST skip
    /// on publish failure so renewal state stays pre-publish
    /// and the next tick retries). Caller responsibility: log
    /// at WARN and skip downstream namespace writes; the
    /// previous arc-swap snapshot keeps serving until the
    /// retry succeeds.
    fn republish_tls_config(&self) -> Result<(), PublishError> {
        self.republish_tls_config_for_plans(&self.plans)
    }

    /// C4b — the worker behind `republish_tls_config`, with the
    /// candidate plan set as an explicit parameter so the
    /// `VhostRemoved` arm can publish a snapshot WITHOUT the
    /// removed fqdn before mutating `self.plans`. For renewals
    /// and fresh-issues, the thin wrapper above passes
    /// `&self.plans` and the result is identical.
    ///
    /// **Plan-filtered ACME identity emission** (Codex E.6
    /// catch): identities are emitted only for fqdns whose
    /// plan is in the candidate set. A leftover identity in
    /// `self.acme_identities` whose plan has disappeared
    /// (because of an earlier delete the C4a-state observer
    /// dropped) is therefore not republished — the next
    /// reconcile-cleanup pass purges it from
    /// `acme_identities` to align with this gate.
    fn republish_tls_config_for_plans(&self, plans: &[AcmeVhostPlan]) -> Result<(), PublishError> {
        if self.tls_listeners.is_empty() {
            // Test paths without a TLS surface: no publish, no
            // sequence capture. Ok so the caller's `?` proceeds
            // (the listeners they'd protect don't exist).
            return Ok(());
        }
        // Build the live identity union (manual-PEM floor + ACME
        // identities for plans still present), then partition it by
        // owning listener so each listener's resolver carries only its
        // own vhosts' certs.
        let mut idents: Vec<TlsIdentityConfig> =
            Vec::with_capacity(self.base_identities.len() + plans.len());
        idents.extend(self.base_identities.iter().cloned());
        for plan in plans {
            if let Some(ident) = self.acme_identities.get(&plan.fqdn) {
                idents.push(ident.clone());
            }
        }
        // When there is exactly one TLS listener (the implicit
        // single-`wg` deployment, and any explicit one-listener
        // config), an identity whose host the startup
        // `fqdn_to_listener` snapshot doesn't know — e.g. a runtime
        // `vhost.add` issued after startup — has only one place it
        // could be served, so route it there. This preserves the
        // pre-P2 behaviour where the single resolver served every
        // ACME identity. With >1 listener a runtime add has no
        // unambiguous interface (config assigns hosts to listeners),
        // so it is skipped + warned rather than guessed onto the
        // wrong interface.
        let sole_listener: Option<&str> = if self.tls_listeners.len() == 1 {
            self.tls_listeners.keys().next().map(String::as_str)
        } else {
            None
        };
        let mut by_listener: HashMap<&str, Vec<TlsIdentityConfig>> = HashMap::new();
        for ident in &idents {
            let target = self
                .fqdn_to_listener
                .get(&ident.server_name)
                .map(String::as_str)
                .or(sole_listener);
            match target {
                Some(lid) => by_listener.entry(lid).or_default().push(ident.clone()),
                None => warn!(
                    server_name = %ident.server_name,
                    "republish: identity for a host with no listener mapping on a \
                     multi-listener node — skipped (a runtime vhost.add has no \
                     interface assignment until config names it)"
                ),
            }
        }
        // Rebuild every listener's resolver from its bucket (an empty
        // bucket swaps in an empty resolver = TLS-disabled for that
        // listener, which `ListenerTls::current` treats as "no cert").
        // Build all resolvers BEFORE swapping any so a single bad
        // bucket fails the whole republish and leaves every listener
        // on its prior snapshot (the retry-next-tick contract).
        let mut built: Vec<(&ListenerTls, Arc<SniCertResolver>)> =
            Vec::with_capacity(self.tls_listeners.len());
        for (lid, handle) in &self.tls_listeners {
            let bucket = by_listener.get(lid.as_str()).cloned().unwrap_or_default();
            let resolver =
                SniCertResolver::from_config(&bucket, /* strict_sni */ false).map_err(|e| {
                    warn!(
                        listener = %lid,
                        error = %e,
                        identity_count = bucket.len(),
                        "ACME resolver rebuild failed — keeping prior snapshot"
                    );
                    PublishError::Build(e)
                })?;
            built.push((handle, Arc::new(resolver)));
        }
        for (handle, resolver) in built {
            handle.swap(Some(resolver));
        }
        if let Some(instr) = &self.publish_instrumentation {
            let seq = instr.seq.fetch_add(1, Ordering::SeqCst);
            instr.tls_publish_seq.store(seq, Ordering::SeqCst);
        }
        info!(
            identity_count = idents.len(),
            plan_count = plans.len(),
            listener_count = self.tls_listeners.len(),
            "ACME resolver snapshots republished (per-listener)"
        );
        Ok(())
    }

    /// C4b — rebuild the [`VhostDirectory`] from a fresh
    /// `webd.vhosts` namespace snapshot and publish it via the
    /// attached `node_vhosts` arc-swap. The namespace IS the
    /// candidate (Codex 4b design-pass A): the
    /// `VhostRemoved`-arm caller delays the call until *after*
    /// the delete tx has committed AND the pre-publish stale-
    /// event guard has confirmed the row stays gone, so a
    /// re-read here is guaranteed to exclude the removed
    /// fqdn; pre-built candidate parameters would double-count
    /// the candidate snapshot without changing observable
    /// behaviour.
    ///
    /// Returns:
    /// - `PublishError::NoNsRuntime` if `attach_ns_events`
    ///   was skipped (test wiring bug).
    /// - `PublishError::NoVhostsHandle` if `attach_node_vhosts`
    ///   was skipped (test wiring bug). Surfaced as a typed
    ///   error rather than silently skipping so the C4b
    ///   ordering guarantee ("arc-swap publishes precede
    ///   namespace write") cannot be subverted by a missing
    ///   attachment in a production-shaped path.
    /// - `PublishError::NsList` / `PublishError::Decode` if
    ///   the namespace read or row decode fails. Recoverable:
    ///   the next reconcile or notify-wake retries.
    /// - `PublishError::Directory` if `from_namespace_rows`
    ///   rejects the candidate (substrate-invariant
    ///   violation — typically a config_bootstrap row that
    ///   never made it into the runtime map).
    async fn republish_vhost_directory(&self) -> Result<(), PublishError> {
        let runtime = self
            .vhosts_runtime
            .as_ref()
            .ok_or(PublishError::NoNsRuntime)?;
        let arc_swap = self
            .node_vhosts
            .as_ref()
            .ok_or(PublishError::NoVhostsHandle)?;
        let rows = crate::vhosts_namespace::snapshot_rows(runtime)
            .await
            .map_err(PublishError::NsList)?;
        let directory =
            from_namespace_rows(&rows, &self.resolved_runtime_map, &self.disabled_hosts)
                .map_err(PublishError::Directory)?;
        arc_swap.store(Arc::new(directory));
        if let Some(instr) = &self.publish_instrumentation {
            let seq = instr.seq.fetch_add(1, Ordering::SeqCst);
            instr
                .vhost_directory_publish_seq
                .store(seq, Ordering::SeqCst);
        }
        info!(
            row_count = rows.len(),
            "VhostDirectory snapshot republished"
        );
        Ok(())
    }

    /// C4b — acquire the per-FQDN coordination lock, returning
    /// an owned guard the caller holds for the duration of the
    /// critical section. Briefly takes the outer
    /// `Arc<TokioMutex<HashMap>>` to look-up-or-insert the
    /// inner `Arc<Mutex<()>>` entry, then drops the outer lock
    /// and awaits the inner guard. The C5 `vhost.add` verb
    /// acquires the same lock identity via
    /// `key_locks_handle().lock().await.entry(fqdn).or_insert_with(...)`
    /// so a re-add concurrent with an in-flight archive
    /// blocks rather than races.
    ///
    /// Returns an `OwnedMutexGuard<()>` so callers can `await`
    /// downstream futures while holding the lock — a borrow
    /// guard would constrain the call to a sync block.
    async fn acquire_fqdn_lock(&self, fqdn: &str) -> OwnedMutexGuard<()> {
        let inner: Arc<TokioMutex<()>> = {
            let mut outer = self.key_locks.lock().await;
            outer
                .entry(fqdn.to_string())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };
        inner.lock_owned().await
    }
}

/// Convert the lib-config `WebdAcmeProvider` (capital-E,
/// wire-form `letsencrypt_prod`) into the lib-daemon
/// [`AcmeProvider`] (lowercase-e). The two enums carry the
/// same set of variants by design; this helper is the single
/// translation site so a future variant added on either side
/// produces a non-exhaustive-match diagnostic here.
/// C4a — convert a `VhostRow` into an `AcmeVhostPlan` when the row
/// carries an ACME configuration. Returns `Ok(None)` for non-ACME
/// rows (manual-PEM or no-TLS) so the caller can short-circuit.
///
/// Validation mirrors the C2 `before_set` hook: ACME requires all of
/// `acme_provider`, `acme_challenge`, `acme_contact_email`. The hook
/// already enforces this on every committed write, so a row reaching
/// this function with a partial ACME triple is a substrate-invariant
/// violation and returns `Err` to surface the corruption rather than
/// silently classify the row as non-ACME.
///
/// Provider/challenge enum variants are parsed via serde's
/// `from_value` (the same path the schema enforces on write) so a
/// future variant added in `WebdAcmeProvider` / `WebdAcmeChallenge`
/// is picked up automatically by this conversion.
///
/// `aliases` is intentionally left empty in the produced plan even if
/// the row carries them: the C5d1-era one-cert-per-fqdn check in
/// `AcmeProvisioner::new` rejects any plan with aliases, and the C4a
/// fresh-issue path runs through the same `issue_one` that calls
/// `WebdAcmeClient::order(&plan.fqdn, ..)` with one identifier. A
/// future multi-SAN follow-up will lift both gates together; passing
/// aliases through here without lifting the issuance gate would
/// produce a plan that immediately rejects in `new`-equivalent code
/// downstream.
fn vhost_row_to_acme_plan(row: &VhostRow) -> Result<Option<AcmeVhostPlan>> {
    let provider_str = row.acme_provider.as_deref();
    let challenge_str = row.acme_challenge.as_deref();
    let contact_email = row.acme_contact_email.as_deref();
    match (provider_str, challenge_str, contact_email) {
        (None, None, None) => Ok(None),
        (Some(p), Some(c), Some(e)) => {
            let provider: WebdAcmeProvider = serde_json::from_value(serde_json::Value::String(
                p.to_string(),
            ))
            .map_err(|err| {
                anyhow!(
                    "vhost row fqdn={:?} has acme_provider={:?} that fails to \
                     deserialise into WebdAcmeProvider: {err}",
                    row.fqdn,
                    p
                )
            })?;
            let challenge: WebdAcmeChallenge = serde_json::from_value(serde_json::Value::String(
                c.to_string(),
            ))
            .map_err(|err| {
                anyhow!(
                    "vhost row fqdn={:?} has acme_challenge={:?} that fails to \
                     deserialise into WebdAcmeChallenge: {err}",
                    row.fqdn,
                    c
                )
            })?;
            Ok(Some(AcmeVhostPlan {
                vhost_index: 0,
                fqdn: row.fqdn.clone(),
                aliases: Vec::new(),
                provider,
                challenge,
                contact_email: e.to_string(),
            }))
        }
        _ => Err(anyhow!(
            "vhost row fqdn={:?} carries a partial ACME triple \
             (provider={:?}, challenge={:?}, contact_email={:?}) — \
             before_set should have rejected this on write; refusing to \
             classify as ACME or non-ACME",
            row.fqdn,
            provider_str,
            challenge_str,
            contact_email
        )),
    }
}

fn webd_provider_to_lib(p: WebdAcmeProvider) -> AcmeProvider {
    match p {
        WebdAcmeProvider::LetsEncryptProd => AcmeProvider::LetsencryptProd,
        WebdAcmeProvider::LetsEncryptStaging => AcmeProvider::LetsencryptStaging,
    }
}

/// Map a webd-side provider directly to the validator's
/// environment discriminator. Folds the
/// `WebdAcmeProvider → AcmeProvider → ChainEnvironment` chain
/// into one site so the validator gate cannot be miswired by
/// dropping the intermediate enum somewhere along the call.
fn chain_environment_for(p: WebdAcmeProvider) -> ChainEnvironment {
    webd_provider_to_lib(p).chain_environment()
}

/// [`Http01Solver`] adapter that bridges the lib-daemon ACME
/// order pipeline to the shared `acme_challenges` map the C4
/// `:80` route reads from. `provision` writes `(token →
/// key_authorization)`; `cleanup` removes by token; both are
/// idempotent. The C4 route validates incoming token shape
/// before reading, so the map only carries syntactically-valid
/// tokens.
///
/// The challenge map is `Arc<RwLock<HashMap>>`-shared between
/// the provisioner, `NodeState`, and the `:80` redirect router
/// — solver writes happen on the order task; reads happen on
/// the LE polling probe; the same `RwLock` arbitrates both.
#[derive(Debug, Clone)]
pub struct ProvisionerSolver {
    challenges: Arc<RwLock<HashMap<String, String>>>,
}

impl ProvisionerSolver {
    /// Bind the solver to the shared challenge map.
    pub fn new(challenges: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self { challenges }
    }
}

#[async_trait]
impl Http01Solver for ProvisionerSolver {
    async fn provision(
        &self,
        _domain: &str,
        token: &str,
        key_authorization: &str,
    ) -> Result<(), AcmeError> {
        let mut guard = self.challenges.write().await;
        guard.insert(token.to_string(), key_authorization.to_string());
        Ok(())
    }

    async fn cleanup(&self, _domain: &str, token: &str) -> Result<(), AcmeError> {
        let mut guard = self.challenges.write().await;
        guard.remove(token);
        Ok(())
    }
}

// ===========================================================================
// C5b: on-disk state machine. Pure functions on `&Path` — no provisioner
// state, no Bus, no async I/O. C5d's `startup_pass` and `run_forever` are
// the callers, holding the per-vhost `tokio::sync::Mutex` documented in
// `_doc/planned/webd-vhosts-phase2.md` § "Reader contract (load-bearing)".
// ===========================================================================

// Per-vhost on-disk layout (C5b's contract with C5d):
//
//   <fqdn_dir>/
//     live/                         # currently-served cert (read at startup)
//       fullchain.pem
//       privkey.pem
//       meta.json
//     staging/                      # in-flight issuance work
//       <timestamp>/
//         fullchain.pem
//         privkey.pem
//         meta.json
//     live.new/                     # transient — only mid-promotion
//     archive/                      # capped at 10 most-recent
//       <timestamp>/
//
// `<timestamp>` shape: `YYYY-MM-DDTHHMMSSZ` (RFC3339 strict, colons
// stripped) so directory names sort lexicographically by issuance time.

// Subdirectory names under `<fqdn_dir>`. Centralised so a rename or
// typo regression surfaces at one site instead of being smeared
// across every helper that touches the filesystem.
const LIVE_DIR: &str = "live";
const LIVE_NEW_DIR: &str = "live.new";
const STAGING_DIR: &str = "staging";
const ARCHIVE_DIR: &str = "archive";

/// File names under each leaf directory (`live/`, `live.new/`,
/// `staging/<ts>/`, `archive/<ts>/`).
const FULLCHAIN_FILE: &str = "fullchain.pem";
const PRIVKEY_FILE: &str = "privkey.pem";
const META_FILE: &str = "meta.json";

/// Errors from [`read_meta_json`]. C5d's policy:
///
/// * `NotFound` — no live chain on disk. Treat as fresh, schedule
///   first issuance.
/// * `Io` (any other I/O error) — hard fail. Operator must
///   investigate (permission, filesystem, ENOSPC).
/// * `Parse` — hard fail. Corrupt `meta.json` for an *acknowledged*
///   version is never silently treated as a fresh vhost — that would
///   burn LE issuance budget on each restart. Rotate the state
///   directory manually after diagnosing.
/// * `UnsupportedVersion` — hard fail. An older webd rolled back over
///   v2 state must refuse to load it rather than silently re-issuing.
#[derive(Debug, Error)]
pub enum MetaReadError {
    /// `meta.json` does not exist. This is the only `MetaReadError`
    /// that C5d treats as "no live chain — re-issue from scratch".
    /// All other variants are hard startup errors.
    #[error("meta.json at {path} not found")]
    NotFound {
        /// The path the read targeted.
        path: PathBuf,
    },
    /// Any I/O error other than `NotFound`. Hard fail in C5d.
    #[error("read meta.json at {path}: {source}")]
    Io {
        /// The path the read targeted.
        path: PathBuf,
        /// Underlying `io::Error`.
        #[source]
        source: std::io::Error,
    },
    /// JSON parse failed for a meta.json whose schema discriminator
    /// matched `MetaV1::CURRENT_VERSION`. Hard fail — never silently
    /// treated as a fresh vhost.
    #[error("parse meta.json at {path}: {source}")]
    Parse {
        /// The path the parse targeted.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// The schema discriminator is not `MetaV1::CURRENT_VERSION`.
    /// A future v2 webd ships a sibling reader; an older webd
    /// rolled back over v2 state must refuse to load it (silent
    /// downgrade to fresh issuance would burn LE rate-limit budget
    /// on every restart).
    #[error(
        "meta.json at {path} declares version {found} but this binary only \
         supports version {supported} — refuse to load (operator must roll \
         the binary forward or rotate the state directory)"
    )]
    UnsupportedVersion {
        /// The path the read targeted.
        path: PathBuf,
        /// The version found in the file.
        found: u8,
        /// The version this binary understands.
        supported: u8,
    },
}

/// Probe struct used for the two-phase read: extracts the `version`
/// discriminator from any `meta.json` payload *without* applying
/// `MetaV1`'s `deny_unknown_fields`. A future `version: 2` payload
/// that adds fields will deserialize cleanly here so the reader can
/// surface `UnsupportedVersion` instead of `Parse`.
#[derive(Deserialize)]
struct MetaVersionProbe {
    version: u8,
}

/// Read and validate a `MetaV1` from disk. Two-phase parse:
///
/// 1. Probe just the `version` field. If it isn't
///    `MetaV1::CURRENT_VERSION`, return `UnsupportedVersion` *before*
///    full `MetaV1` deserialization. This ensures a forward
///    `version: 2` payload (with new fields a v1 binary doesn't
///    understand) is classified as "unsupported version", not "parse
///    error".
/// 2. Full deserialize into `MetaV1`. With the version verified, any
///    failure here is a corrupt-state hard error (Parse).
///
/// `NotFound` is split out so C5d can branch cleanly: only `NotFound`
/// is "fresh vhost"; every other variant is a hard startup error.
pub fn read_meta_json(path: &Path) -> Result<MetaV1, MetaReadError> {
    let bytes = std::fs::read(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            MetaReadError::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            MetaReadError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    // Phase 1: version probe.
    let probe: MetaVersionProbe =
        serde_json::from_slice(&bytes).map_err(|source| MetaReadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if probe.version != MetaV1::CURRENT_VERSION {
        return Err(MetaReadError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: probe.version,
            supported: MetaV1::CURRENT_VERSION,
        });
    }
    // Phase 2: full deserialize with `deny_unknown_fields`. Any
    // failure here means version matched but the body is corrupt.
    let meta: MetaV1 = serde_json::from_slice(&bytes).map_err(|source| MetaReadError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(meta)
}

/// Atomic file write: tempfile in the same directory + `sync_all` on
/// the file + persist (atomic rename) + `sync_all` on the parent
/// directory. The parent-dir fsync is what makes the rename(2) entry
/// durable across power loss; without it, the file contents are on
/// disk but the directory entry pointing at them can be lost.
///
/// Filesystems that don't support directory fsync (FAT, some network
/// mounts) surface `EINVAL` here — propagated rather than swallowed,
/// because webd's `/var/lib/cosmix/webd/acme/` is assumed to live on
/// a real local filesystem and a surprise EINVAL is a deployment
/// configuration bug worth surfacing loudly.
pub fn write_atomic_pem(path: &Path, pem: &str) -> std::io::Result<()> {
    atomic_write_bytes(path, pem.as_bytes())
}

/// Atomic JSON write of a `MetaV1`. Same tempfile-then-rename
/// pattern as [`write_atomic_pem`], with `serde_json::to_vec_pretty`
/// so an operator running `jq` against the file sees structured
/// output without an extra pipe through `python -m json.tool`.
///
/// Refuses to write a `MetaV1` whose `version` field disagrees with
/// `MetaV1::CURRENT_VERSION`. The same binary should never persist an
/// unsupported version: a hand-constructed `MetaV1 { version: 2, .. }`
/// is a programmer error worth surfacing at the write call site, not
/// silently re-encoding on disk.
pub fn write_meta_json(path: &Path, meta: &MetaV1) -> std::io::Result<()> {
    if meta.version != MetaV1::CURRENT_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "write_meta_json: refusing to persist meta.version={} \
                 (this binary supports {})",
                meta.version,
                MetaV1::CURRENT_VERSION,
            ),
        ));
    }
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write_bytes(path, &bytes)
}

/// Common atomic-write core. Kept private; the two public helpers
/// above differ only in how they construct the byte payload.
fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic_write_bytes: target path has no parent",
        )
    })?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut f = tmp.as_file();
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    // Parent-dir fsync — see doc comment on `write_atomic_pem` for
    // the durability rationale.
    std::fs::File::open(parent).and_then(|f| f.sync_all())?;
    Ok(())
}

/// Format an `OffsetDateTime` into the `YYYY-MM-DDTHHMMSSZ` shape
/// the state machine uses for `staging/<ts>/` and
/// `archive/<ts>/` directory names. Colons are stripped to keep the
/// names filesystem-portable (Windows path semantics rejects `:` in
/// filenames; webd's substrate is Linux-only today but the cost of
/// keeping the names portable is one regex character).
///
/// The format-description is built via the `time` crate's
/// compile-time `format_description!` macro so a typo lands as a
/// compile error rather than a runtime panic the first time the
/// renewal loop fires.
fn format_stamp(now: OffsetDateTime) -> String {
    use time::UtcOffset;
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const FMT: &[FormatItem<'static>] =
        format_description!("[year]-[month]-[day]T[hour][minute][second]Z");
    // Normalize to UTC before formatting. The `Z` is a literal in the
    // format, so without this conversion a non-UTC `OffsetDateTime`
    // would write its *local* fields and then claim they were Zulu —
    // and `parse_stamp` (the inverse) decodes the result as UTC,
    // round-tripping a silently-shifted timestamp.
    let now_utc = now.to_offset(UtcOffset::UTC);
    // The format is total: every field is fixed-width and `Z` is a
    // literal, so the only way `format` can fail is an
    // `OffsetDateTime` whose year exceeds 9999, which is not a
    // condition reachable in this codebase.
    now_utc
        .format(&FMT)
        .expect("format_stamp: time format is total for supported year range")
}

/// Resolve a unique sibling path under `parent`, starting from
/// `base_name` and appending a zero-padded `-01`..`-99` suffix on
/// collision. Used by `stage_new_issuance` and
/// `promote_staged_to_live` to defend against the same-second
/// collision that would otherwise let two issuances share a
/// `staging/<ts>/` directory (clobbering each other's PEMs) or
/// break the archive rename mid-promotion.
///
/// The lookup is racy in the abstract — between `try_exists` and the
/// caller's `create_dir`/`rename`, another writer could materialise
/// the path. Acceptable because every caller holds the per-vhost
/// `tokio::sync::Mutex`, so the contract is "no concurrent writer
/// within this process" and (per the umbrella plan) "no second
/// provisioner across the host".
fn unique_path_in(parent: &Path, base_name: &str) -> std::io::Result<PathBuf> {
    let candidate = parent.join(base_name);
    if !candidate.try_exists()? {
        return Ok(candidate);
    }
    // Suffix space is intentionally bounded — under the per-vhost
    // lock invariant, more than a handful of same-second collisions
    // is impossible; a runaway loop here is a symptom of either a
    // broken lock or a corrupted state directory.
    //
    // The suffix is zero-padded to two digits (`-01`..`-99`) so the
    // file-name lex sort `prune_archive` relies on stays monotone:
    // without padding, `...Z-9` would sort newer than `...Z-10`, and
    // `prune_archive` could delete the wrong archive entry on the
    // first same-second collision past suffix 9.
    for n in 1..=99 {
        let candidate = parent.join(format!("{base_name}-{n:02}"));
        if !candidate.try_exists()? {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::other(format!(
        "unique_path_in: exhausted 99 suffixes for {base_name} \
         under {parent:?} — either the per-vhost lock is broken or \
         the state directory is corrupt"
    )))
}

/// Stage a new issuance's PEM + meta files under
/// `<fqdn_dir>/staging/<timestamp>/`. Returns the staging directory
/// path so the caller can hand it to [`promote_staged_to_live`]
/// after running the LE-validator gauntlet.
///
/// On same-second collision (two issuances within one wall-clock
/// second), the leaf is suffixed via [`unique_path_in`]. The leaf
/// itself is created with `std::fs::create_dir` (not `create_dir_all`)
/// so a stale `staging/<ts>/` directory cannot be silently reused;
/// the suffix path is the only legitimate way to reach a collision.
///
/// C5b ships *step 1 only* of the five-step transaction. Step 2's
/// chain validation (`validate_le_chain_for_environment`) lives in
/// the C5d provisioner where it has access to the resolved provider
/// for the vhost.
pub fn stage_new_issuance(
    fqdn_dir: &Path,
    fullchain_pem: &str,
    privkey_pem: &str,
    meta: &MetaV1,
    now: OffsetDateTime,
) -> std::io::Result<PathBuf> {
    let staging_root = fqdn_dir.join(STAGING_DIR);
    std::fs::create_dir_all(&staging_root)?;
    let stamp = format_stamp(now);
    let staged = unique_path_in(&staging_root, &stamp)?;
    // `create_dir` (not `create_dir_all`) — collision must be a hard
    // error rather than silent reuse. The suffix walk in
    // `unique_path_in` above produces a fresh non-existent path.
    std::fs::create_dir(&staged)?;
    sync_dir(&staging_root)?;
    write_atomic_pem(&staged.join(FULLCHAIN_FILE), fullchain_pem)?;
    write_atomic_pem(&staged.join(PRIVKEY_FILE), privkey_pem)?;
    write_meta_json(&staged.join(META_FILE), meta)?;
    Ok(staged)
}

/// Step (3) of the five-step transaction: promote a staged set to
/// live via three atomic renames. Caller MUST hold the per-vhost
/// `tokio::sync::Mutex` (C5d's responsibility) — no in-process reader
/// can interleave with the brief window where `<fqdn>/live/` is
/// absent.
///
/// Renames:
///   (3a) `staged_dir` → `<fqdn>/live.new/`
///   (3b) `<fqdn>/live/` → `<fqdn>/archive/<reconcile-ts>/` (only if
///        `live/` exists — first-issuance vhosts skip this step)
///   (3c) `<fqdn>/live.new/` → `<fqdn>/live/`
///
/// `<reconcile-ts>` is a fresh `format_stamp(now)`; the staged
/// directory's original timestamp is no longer referenced once
/// `staging/<orig-ts>/` is consumed by step (3a).
pub fn promote_staged_to_live(staged_dir: &Path, now: OffsetDateTime) -> std::io::Result<()> {
    // Strict shape check: `staged_dir` MUST be `<fqdn>/staging/<ts>`.
    // Without the `parent.file_name() == "staging"` gate, any
    // grandchild path (e.g. `<fqdn>/archive/<ts>`) would be eligible
    // for promotion and silently corrupt the live cert.
    let staging_root = staged_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "promote_staged_to_live: staged_dir has no parent",
        )
    })?;
    if staging_root.file_name().and_then(|n| n.to_str()) != Some(STAGING_DIR) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "promote_staged_to_live: staged_dir must live under \
                 '{STAGING_DIR}/', got parent {staging_root:?}"
            ),
        ));
    }
    let fqdn_dir = staging_root.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "promote_staged_to_live: staging/ has no <fqdn> parent",
        )
    })?;
    let live = fqdn_dir.join(LIVE_DIR);
    let live_new = fqdn_dir.join(LIVE_NEW_DIR);
    let archive_root = fqdn_dir.join(ARCHIVE_DIR);

    // Refuse to clobber a previous half-completed promotion. The
    // reconcile path is the only legitimate way `live.new/` can
    // exist when this function is called; if it survives into this
    // call, the caller skipped reconcile and we'd be silently
    // double-archiving the previous live cert.
    if live_new.try_exists()? {
        return Err(std::io::Error::other(
            "promote_staged_to_live: live.new/ already exists — \
             caller skipped reconcile_on_startup() or two writers \
             are racing (per-vhost lock is the contract)",
        ));
    }

    // Pre-flight the archive destination BEFORE step (3a). If
    // `archive/<reconcile-ts>/` would collide (same-second renewal),
    // resolve the suffix here so (3b) cannot strand us with
    // `live.new/` staged and the rename failing.
    let archive_dest = if live.try_exists()? {
        std::fs::create_dir_all(&archive_root)?;
        let stamp = format_stamp(now);
        Some(unique_path_in(&archive_root, &stamp)?)
    } else {
        None
    };

    // (3a) staged → live.new. The rename moves a directory entry
    // out of `<fqdn>/staging/` and into `<fqdn>/` — both source and
    // dest happen to share the same grandparent fsync (the umbrella
    // `<fqdn_dir>`), but the source parent (`staging/`) still needs
    // its own `sync_dir` to make the *removal* durable.
    std::fs::rename(staged_dir, &live_new)?;
    sync_dir(staging_root)?;
    sync_dir(fqdn_dir)?;

    // (3b) live → archive/<reconcile-ts> — skip on first issuance.
    if let Some(archived) = archive_dest {
        std::fs::rename(&live, &archived)?;
        sync_dir(fqdn_dir)?;
        sync_dir(&archive_root)?;
    }

    // (3c) live.new → live
    std::fs::rename(&live_new, &live)?;
    sync_dir(fqdn_dir)?;
    Ok(())
}

/// Step (5): keep only the `keep` most-recent
/// `<fqdn_dir>/archive/<ts>/` entries, by directory-name sort
/// (descending). Returns the number of entries removed.
///
/// Newer-first sort is correct because of `format_stamp`'s
/// fixed-width lexicographic-monotonic shape. Ties (same-second
/// issuance, vanishingly rare under LE issuance latency) fall to
/// arbitrary `std::fs::read_dir` order; the umbrella plan tolerates
/// that as "vanishingly rare so don't over-engineer."
pub fn prune_archive(fqdn_dir: &Path, keep: usize) -> std::io::Result<usize> {
    let archive_root = fqdn_dir.join(ARCHIVE_DIR);
    if !archive_root.try_exists()? {
        return Ok(0);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&archive_root)?
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().ok().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    // Sort newer-first by full path (which sorts by file name within
    // the same archive_root). Reverse lex-sort = descending by
    // timestamp.
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    let mut removed = 0;
    for stale in entries.into_iter().skip(keep) {
        std::fs::remove_dir_all(&stale)?;
        removed += 1;
    }
    if removed > 0 {
        sync_dir(&archive_root)?;
    }
    Ok(removed)
}

/// Side-effect bundle returned from a staging-dir sweep — separate
/// from `ReconcileOutcome` because the sweep runs in every branch
/// (roll-forward and no-roll-forward alike) and reports independently
/// of which dominant outcome the reconcile produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagingSweep {
    /// Staging directories whose name-parsed timestamp was older
    /// than `staging_orphan_ttl` (or whose name failed to parse —
    /// those are treated as ancient and discarded so that they
    /// cannot accumulate as private-key material).
    pub discarded: Vec<PathBuf>,
    /// Staging directories whose name-parsed timestamp was younger
    /// than the TTL and which therefore survived. Operator-readable
    /// diagnostic; the production single-provisioner invariant
    /// means a healthy startup sees this empty.
    pub retained: Vec<PathBuf>,
}

/// Outcome of [`reconcile_on_startup`]. Returned by value so C5d's
/// startup pass can log which case ran (operator-facing diagnostic;
/// a roll-forward should always show up in the boot log) without
/// re-walking the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// No `live.new/` and no `staging/<ts>/` entries at all. Either a
    /// fresh vhost or a clean shutdown.
    CleanState,
    /// Case (i): `live.new/` absent; only orphan staging present.
    /// The sweep is reported so the operator can see which dirs
    /// stuck around (or were discarded). When every staging dir is
    /// retained, this is the "young orphan defensively kept" case;
    /// when every staging dir is discarded, the previous variant
    /// (`CleanState`) would *not* fire — the `discarded` list is
    /// the signal that work happened.
    StagingSwept(StagingSweep),
    /// Case (ii): `live.new/` + intact `live/` — rolled forward.
    /// `live/` archived to `archive/<reconcile-ts>/`, then
    /// `live.new/` → `live/`. The staging sweep ran *after* the
    /// roll-forward as a defensive cleanup — its result is reported
    /// alongside.
    PromotionCompletedFromLiveNew {
        /// The path the old `live/` was archived to.
        archived_to: PathBuf,
        /// Staging dirs the post-roll-forward sweep touched.
        staging_sweep: StagingSweep,
    },
    /// Case (iii): `live.new/` with absent `live/` — last rename
    /// completed (`live.new/` → `live/`); existing
    /// `archive/<ts>/` left untouched. Same staging-sweep semantics
    /// as `PromotionCompletedFromLiveNew`.
    PromotionCompletedLastRename {
        /// Staging dirs the post-roll-forward sweep touched.
        staging_sweep: StagingSweep,
    },
}

/// Runs at startup, before any handshake or renewal-loop tick, to
/// resolve any in-progress promotion left by a previous crash. Four
/// reconcilable shapes per the plan; see
/// [`ReconcileOutcome`] for the variant taxonomy.
///
/// `staging_orphan_ttl` lets fresh-orphan `staging/<ts>/` dirs from
/// a race with a hypothetical second provisioner survive a restart;
/// the single-provisioner invariant means production startups will
/// always see orphans as older than the TTL, so in practice they
/// are discarded. The TTL is defensive paranoia, not load-bearing.
pub fn reconcile_on_startup(
    fqdn_dir: &Path,
    staging_orphan_ttl: std::time::Duration,
    now: OffsetDateTime,
) -> std::io::Result<ReconcileOutcome> {
    let live = fqdn_dir.join(LIVE_DIR);
    let live_new = fqdn_dir.join(LIVE_NEW_DIR);
    let staging_root = fqdn_dir.join(STAGING_DIR);

    // Roll-forward cases first: `live.new/` presence dominates
    // any orphan staging behaviour. The plan's case (ii) (live +
    // live.new) and case (iii) (live.new only) both demand a
    // complete-the-rename action. The staging sweep ALSO runs after
    // the roll-forward — leftover staging dirs (PEM private-key
    // material) must not survive indefinitely just because the
    // crash happened to leave `live.new/` present.
    if live_new.try_exists()? {
        let outcome = if live.try_exists()? {
            // (ii): rename live → archive/<reconcile-ts>/, then
            // live.new → live. Pre-flight the archive dest via
            // `unique_path_in` to defend against the same-second
            // collision (this reconcile-ts can match the existing
            // archive contents).
            let archive_root = fqdn_dir.join(ARCHIVE_DIR);
            std::fs::create_dir_all(&archive_root)?;
            let stamp = format_stamp(now);
            let archived = unique_path_in(&archive_root, &stamp)?;
            std::fs::rename(&live, &archived)?;
            sync_dir(fqdn_dir)?;
            sync_dir(&archive_root)?;
            std::fs::rename(&live_new, &live)?;
            sync_dir(fqdn_dir)?;
            let staging_sweep = sweep_staging(&staging_root, staging_orphan_ttl, now)?;
            ReconcileOutcome::PromotionCompletedFromLiveNew {
                archived_to: archived,
                staging_sweep,
            }
        } else {
            // (iii): live absent, live.new present. Just complete
            // the last rename; the archived old cert is already in
            // place from the previous step (3b) before the crash.
            std::fs::rename(&live_new, &live)?;
            sync_dir(fqdn_dir)?;
            let staging_sweep = sweep_staging(&staging_root, staging_orphan_ttl, now)?;
            ReconcileOutcome::PromotionCompletedLastRename { staging_sweep }
        };
        return Ok(outcome);
    }

    // No live.new/. Now check for orphan staging dirs (case i).
    let sweep = sweep_staging(&staging_root, staging_orphan_ttl, now)?;
    if sweep.discarded.is_empty() && sweep.retained.is_empty() {
        Ok(ReconcileOutcome::CleanState)
    } else {
        Ok(ReconcileOutcome::StagingSwept(sweep))
    }
}

/// Walk `staging_root` and classify each `<ts>/` entry against
/// `staging_orphan_ttl` *using the timestamp parsed from the
/// directory name*, not the filesystem mtime. Name-parsed age is
/// stable across rsync/container-restore (mtime would re-stamp the
/// dir to the restore time and treat a stale orphan as fresh) and is
/// what the production code wrote in the first place. Dirs whose
/// name fails to parse are discarded as ancient — anything that
/// wasn't placed by `stage_new_issuance` has no legitimate purpose
/// under `staging/`.
fn sweep_staging(
    staging_root: &Path,
    staging_orphan_ttl: std::time::Duration,
    now: OffsetDateTime,
) -> std::io::Result<StagingSweep> {
    if !staging_root.try_exists()? {
        return Ok(StagingSweep::default());
    }
    let mut sweep = StagingSweep::default();
    for entry in std::fs::read_dir(staging_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => {
                // Non-UTF8 leaf name — treat as ancient, discard.
                std::fs::remove_dir_all(&path)?;
                sweep.discarded.push(path);
                continue;
            }
        };
        // The dir's name is the format_stamp produced when the
        // staging dir was created. `unique_path_in` may have appended
        // `-N` on same-second collision; strip that suffix
        // (recognised as `Z-<digits>` at the end) before parsing.
        let stamp_str = strip_unique_suffix(&name);
        let age = match parse_stamp(stamp_str) {
            Some(parsed) => {
                // If `now` is before `parsed`, the dir is from the
                // future (clock skew) — treat as zero-age.
                if now > parsed {
                    let diff = now - parsed;
                    diff.try_into().unwrap_or(std::time::Duration::ZERO)
                } else {
                    std::time::Duration::ZERO
                }
            }
            None => {
                // Unparseable name — not from our state machine.
                // Treat as ancient and discard so leftover PEM
                // material cannot accumulate.
                std::fs::remove_dir_all(&path)?;
                sweep.discarded.push(path);
                continue;
            }
        };
        if age > staging_orphan_ttl {
            std::fs::remove_dir_all(&path)?;
            sweep.discarded.push(path);
        } else {
            sweep.retained.push(path);
        }
    }
    if !sweep.discarded.is_empty() {
        sync_dir(staging_root)?;
    }
    Ok(sweep)
}

/// Strip a trailing `-<digits>` suffix from a `format_stamp` output
/// that was uniquified by [`unique_path_in`]. The base stamp ends in
/// `Z`; the suffix is `-` followed by ASCII digits.
fn strip_unique_suffix(name: &str) -> &str {
    let Some(dash_idx) = name.rfind('-') else {
        return name;
    };
    let (head, tail) = name.split_at(dash_idx);
    // `tail` begins with '-'.
    let suffix_digits = &tail[1..];
    if !suffix_digits.is_empty()
        && suffix_digits.chars().all(|c| c.is_ascii_digit())
        && head.ends_with('Z')
    {
        head
    } else {
        name
    }
}

/// Inverse of [`format_stamp`] — parse `YYYY-MM-DDTHHMMSSZ` back
/// into an `OffsetDateTime` (UTC). Returns `None` for any string
/// that doesn't match the exact production shape (no leniency on
/// case, separator characters, or trailing data).
fn parse_stamp(s: &str) -> Option<OffsetDateTime> {
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const FMT: &[FormatItem<'static>] =
        format_description!("[year]-[month]-[day]T[hour][minute][second]Z");
    // `parse` is total over the shape; any deviation yields Err →
    // None. The trailing `Z` is consumed as a literal and the
    // parsed `PrimitiveDateTime` is treated as UTC.
    let pdt = time::PrimitiveDateTime::parse(s, &FMT).ok()?;
    Some(pdt.assume_utc())
}

/// Open a directory and call `sync_all` on it — the
/// rename(2)-durability primitive used by every helper above. Same
/// pattern as `cosmix-lib-daemon::acme::atomic_write_json` so the
/// two crates agree on what "atomic" means for ACME state.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|f| f.sync_all())
}

/// C4b — startup orphan scan. Walks `<acme_root>/` looking for
/// per-fqdn directories whose names are NOT in `known_fqdns`
/// (the namespace's current vhost set) and archives each such
/// directory by renaming it to
/// `<acme_root>/.orphan-archive/<fqdn>-<startup_ts>/`. The
/// `<fqdn>-<startup_ts>` form matches `_doc/planned/webd-vhosts-
/// phase3.md` § "Startup ordering" so a future restore can
/// locate by either fqdn or restart timestamp.
///
/// Distinct from [`AcmeProvisioner::archive_live_dir`] which
/// archives only `<fqdn>/live/` under `<fqdn>/archive/`. The
/// orphan scan archives the WHOLE `<fqdn>/` tree because the
/// fqdn itself is no longer recognised by the namespace — the
/// archive/staging siblings are orphaned along with the live
/// cert.
///
/// Idempotent + fail-soft: missing acme_root, missing files,
/// non-directory entries (e.g. `.orphan-archive/` itself once
/// created) are all skipped quietly. Returns the count of orphan
/// dirs archived for observability/testing.
///
/// `startup_ts` is the wall-clock at startup, used as the
/// archive-entry suffix so a fresh restart picks a new entry
/// rather than colliding with a prior one.
pub fn archive_orphan_acme_dirs(
    acme_root: &Path,
    known_fqdns: &HashSet<String>,
    startup_ts: OffsetDateTime,
) -> std::io::Result<usize> {
    if !acme_root.try_exists()? {
        return Ok(0);
    }
    let orphan_root = acme_root.join(".orphan-archive");
    // The orphan bucket is created lazily — only if at least one
    // orphan is found below. Avoids creating empty
    // `.orphan-archive/` dirs on every startup.
    let stamp = format_stamp(startup_ts);
    let mut archived = 0usize;
    for entry in std::fs::read_dir(acme_root)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name_os) = path.file_name() else {
            continue;
        };
        let Some(name) = name_os.to_str() else {
            continue;
        };
        // Skip the orphan bucket itself (dotfile-prefixed) and
        // any other dotfile or `_*` hidden namespace, plus any
        // non-directory entry.
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let meta = entry.file_type()?;
        if !meta.is_dir() {
            continue;
        }
        // FQDN match: substrate namespace key === directory name.
        if known_fqdns.contains(name) {
            continue;
        }
        // Orphan. Create the bucket lazily.
        std::fs::create_dir_all(&orphan_root)?;
        let entry_name = format!("{name}-{stamp}");
        let dest = orphan_root.join(&entry_name);
        let final_dest = if dest.try_exists()? {
            // Two startups in the same second; rare but possible
            // under test harness time-warps. Pick a fresh
            // sub-name rather than clobbering.
            unique_path_in(&orphan_root, &entry_name)?
        } else {
            dest
        };
        std::fs::rename(&path, &final_dest)?;
        sync_dir(acme_root)?;
        sync_dir(&orphan_root)?;
        archived += 1;
        info!(
            fqdn = %name,
            archive = %final_dest.display(),
            "Startup orphan-scan: archived unknown fqdn directory"
        );
    }
    Ok(archived)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Origin used as the `now` argument across the backoff tests so
    /// the asserted timestamps are stable regardless of wall-clock
    /// drift. The exact value is irrelevant; what matters is the
    /// delta against `BACKOFF_SCHEDULE`.
    fn fixed_now() -> OffsetDateTime {
        // 2026-01-01T00:00:00Z, comfortably inside the i64 range and
        // far enough from any LE certificate expiry edge to avoid
        // confusing a reader who skims the test output.
        OffsetDateTime::from_unix_timestamp(1_767_225_600).expect("fixed timestamp valid")
    }

    #[test]
    fn backoff_advances_on_failure() {
        let now = fixed_now();
        // 1..=6 failures → [1h, 6h, 24h, 7d, 7d, 7d]. The 5th+
        // failure clamps to the last schedule entry (7d) — verified
        // explicitly because the clamp is the property an operator
        // relies on when a vhost is stuck for weeks.
        let expected = [
            Duration::from_secs(60 * 60),
            Duration::from_secs(6 * 60 * 60),
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(7 * 24 * 60 * 60),
            Duration::from_secs(7 * 24 * 60 * 60),
            Duration::from_secs(7 * 24 * 60 * 60),
        ];
        for (i, dur) in expected.iter().enumerate() {
            let count = (i + 1) as u32;
            let got = next_attempt_after_failure(now, count);
            assert_eq!(
                got,
                now + *dur,
                "backoff for error_count={count} ({i}-indexed) did not match schedule entry"
            );
        }
    }

    #[test]
    fn backoff_zero_count_saturates_to_first_interval() {
        // Defensive: `error_count == 0` is nonsensical but must not
        // panic via subtraction underflow. The chosen behaviour is
        // "treat as the first failure" so a miscount produces a
        // conservative 1h cooldown rather than crashing the renewal
        // tick that called us.
        let now = fixed_now();
        let got = next_attempt_after_failure(now, 0);
        assert_eq!(got, now + BACKOFF_SCHEDULE[0]);
    }

    #[test]
    fn meta_v1_roundtrips_with_version_field() {
        // Pin the on-disk shape: `version: 1` is written, parsed,
        // and round-trips byte-for-byte through serde_json. Locks in
        // both the discriminator's presence and `MetaV1::CURRENT_VERSION`
        // so a future bump cannot land without updating this test.
        let m = MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: "https://acme.example/order/1".into(),
            account_url: "https://acme.example/account/1".into(),
            not_before_unix: 1_700_000_000,
            not_after_unix: 1_700_000_000 + 90 * 24 * 60 * 60,
            issued_at_unix: 1_700_000_000,
            fqdn: "h.example".into(),
            aliases: vec!["www.h.example".into()],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            acme_directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(
            json.contains("\"version\":1"),
            "MetaV1 JSON must carry the version discriminator: {json}"
        );
        let parsed: MetaV1 = serde_json::from_str(&json).expect("round-trip parse");
        assert_eq!(parsed, m);
        assert_eq!(parsed.version, 1);
    }

    #[test]
    fn meta_v1_rejects_unknown_fields() {
        // `deny_unknown_fields` is the C5a half of the forward-compat
        // contract: a future writer that adds a *field* without
        // bumping `version` is refused by an older reader rather
        // than silently accepted with unknown semantics. C5b adds
        // the matching `version != CURRENT_VERSION` reject.
        let bad = r#"{
            "version": 1,
            "order_url": "u", "account_url": "u",
            "not_before_unix": 0, "not_after_unix": 0, "issued_at_unix": 0,
            "fqdn": "h", "aliases": [],
            "provider": "letsencrypt_staging",
            "acme_directory_url": "u",
            "future_field": "surprise"
        }"#;
        let err = serde_json::from_str::<MetaV1>(bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("future_field") || msg.contains("unknown field"),
            "deny_unknown_fields error must name the offending field, got: {msg}"
        );
    }

    #[test]
    fn provisioner_new_rejects_duplicate_fqdns() {
        // The resolver already rejects duplicate FQDNs at config
        // load; this is the belt-and-braces gate inside `new` so
        // C5b's per-FQDN on-disk state directory and C5d's
        // cooldown map can't be silently shared between two plans
        // if the upstream invariant ever regresses.
        let plan = |idx, fqdn: &str| AcmeVhostPlan {
            vhost_index: idx,
            fqdn: fqdn.to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        let plans = vec![plan(0, "h.example"), plan(1, "h.example")];
        let challenges = Arc::new(RwLock::new(HashMap::new()));
        let err = AcmeProvisioner::new(
            PathBuf::from("/var/empty"),
            plans,
            None,
            challenges,
            Vec::new(),
            DEFAULT_RENEWAL_TICK,
            None,
        )
        .expect_err("duplicate FQDNs must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate FQDN") && msg.contains("h.example"),
            "expected duplicate-FQDN diagnostic, got: {msg}"
        );
    }

    #[test]
    fn provisioner_new_accepts_distinct_fqdns() {
        // Mirror of the duplicate test — the happy path constructs
        // cleanly and seeds one cooldown entry per plan.
        let plan = |idx, fqdn: &str| AcmeVhostPlan {
            vhost_index: idx,
            fqdn: fqdn.to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        let plans = vec![plan(0, "a.example"), plan(1, "b.example")];
        let challenges = Arc::new(RwLock::new(HashMap::new()));
        let p = AcmeProvisioner::new(
            PathBuf::from("/var/empty"),
            plans,
            None,
            challenges,
            Vec::new(),
            DEFAULT_RENEWAL_TICK,
            None,
        )
        .expect("distinct FQDNs construct");
        assert_eq!(p.vhost_state.len(), 2);
        assert!(p.vhost_state.contains_key("a.example"));
        assert!(p.vhost_state.contains_key("b.example"));
    }

    #[test]
    fn provisioner_new_rejects_acme_aliases_until_multisan() {
        // C5d1 is single-fqdn — ordering with aliases would
        // register the alias under the host-router map but
        // emit no resolver identity for it, silently routing
        // alias SNI through the non-strict default. Gate the
        // mismatch loudly until multi-SAN ordering lands.
        let plan = AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "h.example".into(),
            aliases: vec!["www.h.example".into()],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        let challenges = Arc::new(RwLock::new(HashMap::new()));
        let err = AcmeProvisioner::new(
            PathBuf::from("/var/empty"),
            vec![plan],
            None,
            challenges,
            Vec::new(),
            DEFAULT_RENEWAL_TICK,
            None,
        )
        .expect_err("ACME with aliases must be rejected in C5d1");
        let msg = format!("{err}");
        assert!(
            msg.contains("multi-SAN") && msg.contains("www.h.example"),
            "expected multi-SAN gap diagnostic naming the alias, got: {msg}"
        );
    }

    #[test]
    fn provisioner_new_rejects_mixed_providers() {
        // lib-daemon's `scan_for_foreign_provider` refuses to
        // bootstrap a second provider's account file alongside
        // the first in a shared `acme_dir`. Surface that as a
        // clean config-time rejection rather than a confusing
        // mid-startup_pass failure on the second plan.
        let plan = |fqdn: &str, provider| AcmeVhostPlan {
            vhost_index: 0,
            fqdn: fqdn.into(),
            aliases: vec![],
            provider,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        let plans = vec![
            plan("a.example", WebdAcmeProvider::LetsEncryptStaging),
            plan("b.example", WebdAcmeProvider::LetsEncryptProd),
        ];
        let challenges = Arc::new(RwLock::new(HashMap::new()));
        let err = AcmeProvisioner::new(
            PathBuf::from("/var/empty"),
            plans,
            None,
            challenges,
            Vec::new(),
            DEFAULT_RENEWAL_TICK,
            None,
        )
        .expect_err("mixed providers must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("mix providers"),
            "expected mixed-provider diagnostic, got: {msg}"
        );
    }

    #[test]
    fn backoff_resets_on_success() {
        // "Reset on success" is encoded in `ProvisionerVhostState`'s
        // default: a freshly-constructed entry (which is what the
        // C5d success path swaps in) carries zero errors and no
        // cooldown. This test pins that default so a future field
        // addition doesn't silently change the reset semantics.
        let s = ProvisionerVhostState::default();
        assert!(s.last_error.is_none());
        assert_eq!(s.last_error_count, 0);
        assert!(s.next_attempt_after.is_none());
    }

    // -----------------------------------------------------------------------
    // C5b tests — disk-state helpers. Bucket H per
    // `_doc/planned/webd-vhosts-phase2.md`.
    // -----------------------------------------------------------------------

    fn sample_meta(fqdn: &str) -> MetaV1 {
        MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: "https://acme.example/order/1".into(),
            account_url: "https://acme.example/account/1".into(),
            not_before_unix: 1_700_000_000,
            not_after_unix: 1_700_000_000 + 90 * 24 * 60 * 60,
            issued_at_unix: 1_700_000_000,
            fqdn: fqdn.to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            acme_directory_url: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
        }
    }

    /// Two timestamps far enough apart that `format_stamp` produces
    /// strictly-ordered directory names regardless of test-host time
    /// jitter.
    fn ts(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).expect("ts in range")
    }

    /// Helper: write a fully-formed `live/` directory with sample
    /// content so promotion / reconcile tests start from a realistic
    /// "carrying" state.
    fn install_live(fqdn_dir: &Path, marker: &str) {
        let live = fqdn_dir.join(LIVE_DIR);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join(FULLCHAIN_FILE), format!("FULLCHAIN-{marker}")).unwrap();
        std::fs::write(live.join(PRIVKEY_FILE), format!("KEY-{marker}")).unwrap();
        let meta = sample_meta("h.example");
        std::fs::write(
            live.join(META_FILE),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn write_atomic_pem_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fullchain.pem");
        write_atomic_pem(&target, "PEM-BODY\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "PEM-BODY\n");
    }

    #[test]
    fn meta_json_write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(META_FILE);
        let meta = sample_meta("h.example");
        write_meta_json(&target, &meta).unwrap();
        let got = read_meta_json(&target).unwrap();
        assert_eq!(got, meta);
    }

    #[test]
    fn read_meta_json_rejects_unsupported_version() {
        // Inject a hand-crafted JSON with version=2 directly (we
        // can't construct a MetaV1 with the wrong version via the
        // serde path — it's a u8 field with no default). The
        // reader must hard-fail rather than silently treat the
        // chain as absent.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(META_FILE);
        let bytes = r#"{
            "version": 2,
            "order_url": "u", "account_url": "u",
            "not_before_unix": 0, "not_after_unix": 0, "issued_at_unix": 0,
            "fqdn": "h", "aliases": [],
            "provider": "letsencrypt_staging",
            "acme_directory_url": "u"
        }"#;
        std::fs::write(&target, bytes).unwrap();
        match read_meta_json(&target) {
            Err(MetaReadError::UnsupportedVersion {
                found, supported, ..
            }) => {
                assert_eq!(found, 2);
                assert_eq!(supported, 1);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn stage_new_issuance_writes_three_files_under_timestamp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        let meta = sample_meta("h.example");
        let staged =
            stage_new_issuance(&fqdn_dir, "FULLCHAIN", "KEY", &meta, ts(1_767_225_600)).unwrap();
        assert!(staged.starts_with(fqdn_dir.join(STAGING_DIR)));
        assert!(staged.join(FULLCHAIN_FILE).exists());
        assert!(staged.join(PRIVKEY_FILE).exists());
        assert!(staged.join(META_FILE).exists());
        // The directory-name shape is the lex-sortable form per
        // `format_stamp`. 2026-01-01T000000Z (no colons).
        let name = staged.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("2026-") && name.ends_with('Z') && !name.contains(':'),
            "unexpected stamp shape: {name}"
        );
    }

    #[test]
    fn promote_first_issuance_lands_in_live_with_no_archive() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        let meta = sample_meta("h.example");
        let staged =
            stage_new_issuance(&fqdn_dir, "FULLCHAIN", "KEY", &meta, ts(1_767_225_600)).unwrap();
        promote_staged_to_live(&staged, ts(1_767_225_700)).unwrap();
        // live/ now holds the new cert; archive/ never created
        // because there was no prior live/ to displace.
        let live = fqdn_dir.join(LIVE_DIR);
        assert_eq!(
            std::fs::read_to_string(live.join(FULLCHAIN_FILE)).unwrap(),
            "FULLCHAIN"
        );
        assert!(!fqdn_dir.join(LIVE_NEW_DIR).exists());
        assert!(
            !fqdn_dir.join(ARCHIVE_DIR).exists() || {
                std::fs::read_dir(fqdn_dir.join(ARCHIVE_DIR))
                    .unwrap()
                    .count()
                    == 0
            }
        );
    }

    #[test]
    fn promote_with_existing_live_archives_old_cert() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        let meta = sample_meta("h.example");
        let staged =
            stage_new_issuance(&fqdn_dir, "NEW-CHAIN", "NEW-KEY", &meta, ts(1_767_225_600))
                .unwrap();
        promote_staged_to_live(&staged, ts(1_767_225_700)).unwrap();
        let live = fqdn_dir.join(LIVE_DIR);
        assert_eq!(
            std::fs::read_to_string(live.join(FULLCHAIN_FILE)).unwrap(),
            "NEW-CHAIN"
        );
        // Archive holds exactly one entry, and it carries the old
        // cert body.
        let archive_root = fqdn_dir.join(ARCHIVE_DIR);
        let entries: Vec<_> = std::fs::read_dir(&archive_root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "expected one archive entry");
        let archived = &entries[0];
        assert_eq!(
            std::fs::read_to_string(archived.join(FULLCHAIN_FILE)).unwrap(),
            "FULLCHAIN-OLD"
        );
    }

    #[test]
    fn promote_refuses_to_clobber_existing_live_new() {
        // If `live.new/` exists when `promote_staged_to_live` is
        // called, the caller skipped `reconcile_on_startup` and the
        // helper must refuse rather than silently double-archive
        // the previous live cert.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        std::fs::create_dir_all(fqdn_dir.join(LIVE_NEW_DIR)).unwrap();
        let meta = sample_meta("h.example");
        let staged = stage_new_issuance(&fqdn_dir, "NEW", "KEY", &meta, ts(1_767_225_600)).unwrap();
        let err = promote_staged_to_live(&staged, ts(1_767_225_700)).unwrap_err();
        assert!(
            err.to_string().contains("live.new/"),
            "expected refusal diagnostic, got: {err}"
        );
    }

    #[test]
    fn prune_archive_caps_at_keep_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        let archive_root = fqdn_dir.join(ARCHIVE_DIR);
        std::fs::create_dir_all(&archive_root).unwrap();
        // 14 deterministic entries with lex-sortable names:
        // 2026-01-01T000000Z .. 2026-01-14T000000Z.
        for day in 1..=14 {
            let name = format!("2026-01-{:02}T000000Z", day);
            std::fs::create_dir_all(archive_root.join(&name)).unwrap();
            // A marker file so we can confirm directory removal vs
            // just rename.
            std::fs::write(archive_root.join(&name).join("marker"), &name).unwrap();
        }
        let removed = prune_archive(&fqdn_dir, 10).unwrap();
        assert_eq!(removed, 4);
        let mut survivors: Vec<String> = std::fs::read_dir(&archive_root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        survivors.sort();
        assert_eq!(
            survivors,
            (5..=14)
                .map(|d| format!("2026-01-{d:02}T000000Z"))
                .collect::<Vec<_>>(),
            "newest-first prune must retain days 5..=14"
        );
    }

    #[test]
    fn prune_archive_with_no_archive_dir_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        // No archive/ subdir at all — common state on a fresh
        // first-issuance vhost.
        assert_eq!(prune_archive(&fqdn_dir, 10).unwrap(), 0);
    }

    #[test]
    fn reconcile_clean_state_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        let outcome =
            reconcile_on_startup(&fqdn_dir, Duration::from_secs(6 * 3600), ts(0)).unwrap();
        assert_eq!(outcome, ReconcileOutcome::CleanState);
        // live/ untouched
        assert_eq!(
            std::fs::read_to_string(fqdn_dir.join(LIVE_DIR).join(FULLCHAIN_FILE)).unwrap(),
            "FULLCHAIN-OLD"
        );
    }

    #[test]
    fn reconcile_case_ii_live_plus_live_new_rolls_forward() {
        // Plan case (ii): crash between 3a and 3b. live.new/ +
        // intact live/.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        let live_new = fqdn_dir.join(LIVE_NEW_DIR);
        std::fs::create_dir_all(&live_new).unwrap();
        std::fs::write(live_new.join(FULLCHAIN_FILE), "NEW-FULLCHAIN").unwrap();
        std::fs::write(live_new.join(PRIVKEY_FILE), "NEW-KEY").unwrap();
        std::fs::write(
            live_new.join(META_FILE),
            serde_json::to_vec_pretty(&sample_meta("h.example")).unwrap(),
        )
        .unwrap();

        let outcome =
            reconcile_on_startup(&fqdn_dir, Duration::from_secs(6 * 3600), ts(1_767_225_700))
                .unwrap();
        match outcome {
            ReconcileOutcome::PromotionCompletedFromLiveNew {
                archived_to,
                staging_sweep,
            } => {
                assert!(archived_to.starts_with(fqdn_dir.join(ARCHIVE_DIR)));
                assert_eq!(
                    std::fs::read_to_string(archived_to.join(FULLCHAIN_FILE)).unwrap(),
                    "FULLCHAIN-OLD"
                );
                // No staging dirs in this case — sweep must be empty.
                assert!(staging_sweep.discarded.is_empty());
                assert!(staging_sweep.retained.is_empty());
            }
            other => panic!("expected PromotionCompletedFromLiveNew, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(fqdn_dir.join(LIVE_DIR).join(FULLCHAIN_FILE)).unwrap(),
            "NEW-FULLCHAIN"
        );
        assert!(!fqdn_dir.join(LIVE_NEW_DIR).exists());
    }

    #[test]
    fn reconcile_case_iii_live_new_only_completes_last_rename() {
        // Plan case (iii): crash between 3b and 3c. live.new/
        // present, live/ absent, archive/ holds the old cert.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        let live_new = fqdn_dir.join(LIVE_NEW_DIR);
        std::fs::create_dir_all(&live_new).unwrap();
        std::fs::write(live_new.join(FULLCHAIN_FILE), "NEW-FULLCHAIN").unwrap();
        std::fs::write(live_new.join(PRIVKEY_FILE), "NEW-KEY").unwrap();
        std::fs::write(
            live_new.join(META_FILE),
            serde_json::to_vec_pretty(&sample_meta("h.example")).unwrap(),
        )
        .unwrap();
        let preexisting_archive = fqdn_dir.join(ARCHIVE_DIR).join("2026-01-01T000000Z");
        std::fs::create_dir_all(&preexisting_archive).unwrap();
        std::fs::write(preexisting_archive.join(FULLCHAIN_FILE), "OLD-ARCHIVED").unwrap();

        let outcome =
            reconcile_on_startup(&fqdn_dir, Duration::from_secs(6 * 3600), ts(1_767_225_700))
                .unwrap();
        match &outcome {
            ReconcileOutcome::PromotionCompletedLastRename { staging_sweep } => {
                assert!(staging_sweep.discarded.is_empty());
                assert!(staging_sweep.retained.is_empty());
            }
            other => panic!("expected PromotionCompletedLastRename, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(fqdn_dir.join(LIVE_DIR).join(FULLCHAIN_FILE)).unwrap(),
            "NEW-FULLCHAIN"
        );
        // Pre-existing archive entry left untouched.
        assert_eq!(
            std::fs::read_to_string(preexisting_archive.join(FULLCHAIN_FILE)).unwrap(),
            "OLD-ARCHIVED"
        );
        assert!(!fqdn_dir.join(LIVE_NEW_DIR).exists());
    }

    #[test]
    fn reconcile_case_i_young_orphan_staging_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        // Stage at `now=0` and reconcile at `now=0` so the parsed
        // age is zero seconds — definitively younger than the 6h TTL.
        let staging =
            stage_new_issuance(&fqdn_dir, "X", "Y", &sample_meta("h.example"), ts(0)).unwrap();
        let outcome = reconcile_on_startup(&fqdn_dir, Duration::from_secs(6 * 3600), ts(0))
            .expect("reconcile");
        match outcome {
            ReconcileOutcome::StagingSwept(sweep) => {
                assert!(
                    sweep.retained.contains(&staging),
                    "expected staging in retained, got sweep={sweep:?}"
                );
                assert!(sweep.discarded.is_empty());
            }
            other => panic!("expected StagingSwept(retained), got {other:?}"),
        }
        assert!(staging.exists(), "young orphan must survive reconcile");
    }

    #[test]
    fn reconcile_discards_old_orphan_staging_by_name_parsed_age() {
        // Defends MINOR 2 from Codex R2: age must be derived from
        // the directory NAME, not filesystem mtime. We stage at
        // `now=0` and reconcile at `now = 0 + 7h`, with a 6h TTL,
        // and assert the staging dir is discarded — independent of
        // the mtime, which the tempfs sets to wall-clock time.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        let staging =
            stage_new_issuance(&fqdn_dir, "X", "Y", &sample_meta("h.example"), ts(0)).unwrap();
        let outcome = reconcile_on_startup(
            &fqdn_dir,
            Duration::from_secs(6 * 3600),
            ts(7 * 3600), // 7h after the staging stamp → over TTL
        )
        .expect("reconcile");
        match outcome {
            ReconcileOutcome::StagingSwept(sweep) => {
                assert!(
                    sweep.discarded.contains(&staging),
                    "expected staging in discarded, got sweep={sweep:?}"
                );
                assert!(sweep.retained.is_empty());
            }
            other => panic!("expected StagingSwept(discarded), got {other:?}"),
        }
        assert!(!staging.exists(), "old orphan must be removed by reconcile");
    }

    #[test]
    fn reconcile_discards_unparseable_staging_dirs() {
        // A dir whose name isn't `format_stamp` output must be
        // treated as ancient — anything that wasn't placed by
        // `stage_new_issuance` has no legitimate purpose under
        // `staging/`, and leaving PEM material around forever is a
        // privacy regression.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        let staging_root = fqdn_dir.join(STAGING_DIR);
        std::fs::create_dir_all(&staging_root).unwrap();
        let bogus = staging_root.join("not-a-stamp");
        std::fs::create_dir(&bogus).unwrap();
        let outcome = reconcile_on_startup(&fqdn_dir, Duration::from_secs(6 * 3600), ts(0))
            .expect("reconcile");
        match outcome {
            ReconcileOutcome::StagingSwept(sweep) => {
                assert!(sweep.discarded.contains(&bogus));
                assert!(sweep.retained.is_empty());
            }
            other => panic!("expected StagingSwept(discarded), got {other:?}"),
        }
        assert!(!bogus.exists());
    }

    #[test]
    fn read_meta_json_notfound_is_split_from_io() {
        // Defends MAJOR 2 from Codex R2: NotFound is the ONLY
        // variant C5d will treat as "fresh vhost"; other I/O errors
        // must hard-fail. The path-with-no-parent case is the
        // simplest non-NotFound I/O reject we can synthesise.
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("nope.json");
        match read_meta_json(&absent) {
            Err(MetaReadError::NotFound { path }) => {
                assert_eq!(path, absent);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn read_meta_json_unknown_future_field_with_v2_is_unsupported_version() {
        // Defends MAJOR 1 from Codex R2: a v2 payload with extra
        // fields a v1 binary doesn't understand must classify as
        // UnsupportedVersion (clear operator signal) rather than
        // Parse (ambiguous between corrupt and forward-compat).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(META_FILE);
        let bytes = r#"{
            "version": 2,
            "order_url": "u", "account_url": "u",
            "not_before_unix": 0, "not_after_unix": 0, "issued_at_unix": 0,
            "fqdn": "h", "aliases": [],
            "provider": "letsencrypt_staging",
            "acme_directory_url": "u",
            "future_field_added_in_v2": "garbage"
        }"#;
        std::fs::write(&target, bytes).unwrap();
        match read_meta_json(&target) {
            Err(MetaReadError::UnsupportedVersion { found, .. }) => {
                assert_eq!(found, 2);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn write_meta_json_refuses_wrong_version() {
        // Defends NIT from Codex R2.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(META_FILE);
        let mut meta = sample_meta("h.example");
        meta.version = 2;
        let err = write_meta_json(&target, &meta).expect_err("write must reject v2");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn promote_refuses_non_staging_parent() {
        // Defends MAJOR 4 from Codex R2: a grandchild path that
        // isn't under `<fqdn>/staging/` must not be promoted.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        let archive_ts = fqdn_dir.join(ARCHIVE_DIR).join("2026-01-01T000000Z");
        std::fs::create_dir_all(&archive_ts).unwrap();
        let err = promote_staged_to_live(&archive_ts, ts(0)).expect_err("must reject");
        assert!(
            err.to_string().contains("staging"),
            "expected staging-shape diagnostic, got: {err}"
        );
    }

    #[test]
    fn stage_new_issuance_same_second_uses_suffix() {
        // Defends MAJOR 5 from Codex R2 (staging half). Two
        // same-second stagings must not silently share a directory
        // and clobber each other's PEMs.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        let meta = sample_meta("h.example");
        let first = stage_new_issuance(&fqdn_dir, "ONE-CHAIN", "ONE-KEY", &meta, ts(1_767_225_600))
            .unwrap();
        let second =
            stage_new_issuance(&fqdn_dir, "TWO-CHAIN", "TWO-KEY", &meta, ts(1_767_225_600))
                .unwrap();
        assert_ne!(first, second, "second staging must get a unique path");
        assert_eq!(
            std::fs::read_to_string(first.join(FULLCHAIN_FILE)).unwrap(),
            "ONE-CHAIN",
            "first staging contents must survive the second's creation"
        );
        assert_eq!(
            std::fs::read_to_string(second.join(FULLCHAIN_FILE)).unwrap(),
            "TWO-CHAIN"
        );
        // The second dir's name carries a zero-padded `-NN` suffix.
        let second_name = second.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            second_name.ends_with("-01") || second_name.ends_with("-02"),
            "expected -NN suffix, got {second_name}"
        );
    }

    #[test]
    fn promote_archive_dest_collision_resolves_via_suffix() {
        // Defends MAJOR 5 from Codex R2 (archive half). If the
        // archive timestamp would collide, the pre-flight resolves
        // a unique suffix BEFORE the staged→live.new rename so
        // promotion cannot strand mid-flight.
        let dir = tempfile::tempdir().unwrap();
        let fqdn_dir = dir.path().join("h.example");
        std::fs::create_dir_all(&fqdn_dir).unwrap();
        install_live(&fqdn_dir, "OLD");
        // Seed archive/ with the exact timestamp the promotion is
        // about to want.
        let collide_ts = ts(1_767_225_700);
        let preseed = fqdn_dir.join(ARCHIVE_DIR).join(format_stamp(collide_ts));
        std::fs::create_dir_all(&preseed).unwrap();
        std::fs::write(preseed.join(FULLCHAIN_FILE), "PRESEED").unwrap();

        let meta = sample_meta("h.example");
        let staged =
            stage_new_issuance(&fqdn_dir, "NEW-CHAIN", "NEW-KEY", &meta, ts(1_767_225_600))
                .unwrap();
        promote_staged_to_live(&staged, collide_ts).expect("promote must succeed");

        // Live carries the new cert.
        assert_eq!(
            std::fs::read_to_string(fqdn_dir.join(LIVE_DIR).join(FULLCHAIN_FILE)).unwrap(),
            "NEW-CHAIN"
        );
        // Pre-seed entry untouched; old live archived to the
        // suffixed sibling.
        assert_eq!(
            std::fs::read_to_string(preseed.join(FULLCHAIN_FILE)).unwrap(),
            "PRESEED"
        );
        let archive_root = fqdn_dir.join(ARCHIVE_DIR);
        let entries: Vec<_> = std::fs::read_dir(&archive_root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 2, "expected pre-seed + suffixed sibling");
    }

    #[test]
    fn strip_unique_suffix_round_trips() {
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z"),
            "2026-01-01T000000Z"
        );
        // Zero-padded production suffixes.
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z-01"),
            "2026-01-01T000000Z"
        );
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z-42"),
            "2026-01-01T000000Z"
        );
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z-99"),
            "2026-01-01T000000Z"
        );
        // Legacy unpadded form still strips, so an older state
        // directory (pre-zero-pad) keeps parsing.
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z-1"),
            "2026-01-01T000000Z"
        );
        // Suffix isn't a digit run — leave intact.
        assert_eq!(
            strip_unique_suffix("2026-01-01T000000Z-foo"),
            "2026-01-01T000000Z-foo"
        );
        // No `Z` before the dash — not our suffix pattern.
        assert_eq!(strip_unique_suffix("not-a-stamp"), "not-a-stamp");
    }

    #[test]
    fn unique_path_in_suffix_is_zero_padded() {
        // Defends MINOR R3 from Codex: pads must keep file_name()
        // lex sort monotone past 9 collisions.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let base = "2026-01-01T000000Z";
        std::fs::create_dir(parent.join(base)).unwrap();
        let first = unique_path_in(parent, base).unwrap();
        std::fs::create_dir(&first).unwrap();
        let first_name = first.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(first_name, format!("{base}-01"));
        // Force collisions up to -09 by pre-creating them, then
        // confirm the next allocation is -10 (the test that proves
        // sort-monotonicity over the 9→10 boundary).
        for n in 2..=9 {
            std::fs::create_dir(parent.join(format!("{base}-{n:02}"))).unwrap();
        }
        let tenth = unique_path_in(parent, base).unwrap();
        assert_eq!(
            tenth.file_name().unwrap().to_string_lossy(),
            format!("{base}-10")
        );
    }

    #[test]
    fn format_stamp_normalises_non_utc_offsets() {
        // Defends MINOR R3 from Codex: format_stamp normalizes to UTC
        // before emitting the literal `Z`, so a caller passing an
        // OffsetDateTime in +10:00 doesn't write its local fields and
        // mislabel them as Zulu. We construct an instant at unix=0
        // (Z midnight) but shifted into +10:00 and assert the
        // formatter still writes the UTC fields.
        let instant = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let shifted = instant.to_offset(time::UtcOffset::from_hms(10, 0, 0).unwrap());
        // The shifted value carries 1970-01-01 10:00:00 +10:00 in its
        // local-fields view, but represents the same instant as Z 0.
        assert_eq!(format_stamp(shifted), "1970-01-01T000000Z");
    }

    // -----------------------------------------------------------------------
    // C5d1 tests — Http01Solver adapter. Bucket H entry
    // `solver_publishes_then_cleans_up`.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn solver_publishes_then_cleans_up() {
        // The provisioner's Http01Solver bridges to the same
        // `acme_challenges` map the C4 :80 route reads from. Drive
        // provision -> cleanup directly and assert the map mutates
        // as expected (and that cleanup of an unknown token is a
        // no-op error-free idempotent, as the trait contract
        // requires for orders that fail before publish).
        let challenges: Arc<RwLock<HashMap<String, String>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let solver = ProvisionerSolver::new(challenges.clone());

        solver
            .provision(
                "h.example",
                "tok-22charsxxxxxxxxxxxxx",
                "tok-22charsxxxxxxxxxxxxx.thumbprint",
            )
            .await
            .expect("provision ok");
        {
            let g = challenges.read().await;
            assert_eq!(
                g.get("tok-22charsxxxxxxxxxxxxx").map(|s| s.as_str()),
                Some("tok-22charsxxxxxxxxxxxxx.thumbprint")
            );
        }

        solver
            .cleanup("h.example", "tok-22charsxxxxxxxxxxxxx")
            .await
            .expect("cleanup ok");
        {
            let g = challenges.read().await;
            assert!(
                g.get("tok-22charsxxxxxxxxxxxxx").is_none(),
                "cleanup must remove the token"
            );
        }

        // Idempotent cleanup of an unknown token (the order's
        // outer cleanup loop relies on this — see lib-daemon's
        // `order` for the guaranteed-cleanup contract).
        solver
            .cleanup("h.example", "never-provisioned")
            .await
            .expect("cleanup of unknown token is a no-op");
    }

    // -----------------------------------------------------------------------
    // C5d2 tests — `run_forever` / `tick_once` / `republish_tls_config`.
    //
    // These tests pin the no-network behaviours of the renewal loop:
    // the renewal-window gate, the cooldown gate, the unreadable-meta
    // skip, the notify wake path, and `republish_tls_config`'s safe
    // no-op without an attached handle. The success-path renewal
    // (which goes over the wire to LE) is exercised by Bucket E's
    // staging-only integration test, not here.
    // -----------------------------------------------------------------------

    fn _staging_plan(fqdn: &str) -> AcmeVhostPlan {
        AcmeVhostPlan {
            vhost_index: 0,
            fqdn: fqdn.to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptStaging,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        }
    }

    fn _new_provisioner(acme_dir: PathBuf, plans: Vec<AcmeVhostPlan>) -> AcmeProvisioner {
        AcmeProvisioner::new(
            acme_dir,
            plans,
            None,
            Arc::new(RwLock::new(HashMap::new())),
            Vec::new(),
            // Short renewal tick so a tokio-test `run_forever`
            // observation finishes within the suite budget. The
            // exact value is irrelevant; only `notify_one` is the
            // exercised wake path.
            Duration::from_millis(50),
            None,
        )
        .expect("construct provisioner")
    }

    fn _write_meta(acme_dir: &std::path::Path, fqdn: &str, not_after_unix: i64) {
        _write_meta_with_provider(
            acme_dir,
            fqdn,
            not_after_unix,
            WebdAcmeProvider::LetsEncryptStaging,
        );
    }

    fn _write_meta_with_provider(
        acme_dir: &std::path::Path,
        fqdn: &str,
        not_after_unix: i64,
        provider: WebdAcmeProvider,
    ) {
        let live = acme_dir.join(fqdn).join(LIVE_DIR);
        std::fs::create_dir_all(&live).unwrap();
        let meta = MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: String::new(),
            account_url: String::new(),
            not_before_unix: 0,
            not_after_unix,
            issued_at_unix: 0,
            fqdn: fqdn.to_string(),
            aliases: vec![],
            provider,
            acme_directory_url: String::new(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        std::fs::write(live.join(META_FILE), json).unwrap();
    }

    #[tokio::test]
    async fn tick_once_skips_outside_renewal_window() {
        // Cert valid for ~60 days → outside the 30d renewal window;
        // tick_once must NOT advance vhost_state at all (no issue
        // attempted, no failure recorded). The pin protects against a
        // future inversion of the `secs_to_expiry > RENEWAL_WINDOW`
        // check, which would burn an LE order on every tick.
        let tmp = tempfile::tempdir().unwrap();
        let plans = vec![_staging_plan("far.example")];
        let mut p = _new_provisioner(tmp.path().to_path_buf(), plans);
        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        _write_meta(tmp.path(), "far.example", now_unix + 60 * 24 * 60 * 60);
        p.tick_once().await;
        let st = p.vhost_state.get("far.example").expect("seeded by new()");
        assert!(
            st.last_error.is_none() && st.last_error_count == 0 && st.next_attempt_after.is_none(),
            "outside renewal window must be a true no-op: {st:?}"
        );
    }

    #[tokio::test]
    async fn tick_once_inside_renewal_window_attempts_issuance() {
        // Cert valid for ~5 days → INSIDE the 30d renewal window;
        // tick_once MUST attempt issuance on the very first call,
        // not wait for the next interval tick. This is the
        // immediate-first-tick contract (run_forever does NOT
        // consume the first interval tick) — a regression that
        // re-added the consume would leave a carrying-but-expiring
        // cert un-renewed for a full renewal_tick (~6h prod) at
        // boot, which for a sub-renewal-tick expiry could mean the
        // cert expires before the loop ever wakes.
        //
        // Hermetic by construction: use a *prod* plan with no
        // `tos_acceptance` mint set on the provisioner. `client_for`
        // fails with the "no acme_tos_accepted" error before any
        // network is touched — pre-network, deterministic, no
        // possibility of LE staging traffic in a CI runner.
        let tmp = tempfile::tempdir().unwrap();
        let prod_plan = AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "expiring.example".to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        // Switch the on-disk meta to match the prod plan provider so
        // the renewal-window gate doesn't trip a provider-mismatch
        // earlier than the client_for error we're targeting.
        let mut p = _new_provisioner(tmp.path().to_path_buf(), vec![prod_plan]);
        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        _write_meta_with_provider(
            tmp.path(),
            "expiring.example",
            now_unix + 5 * 24 * 60 * 60,
            WebdAcmeProvider::LetsEncryptProd,
        );
        // tos_acceptance is None on the test provisioner — client_for
        // for a prod plan returns Err("no acme_tos_accepted") before
        // any network call.
        p.tick_once().await;
        let st = p.vhost_state.get("expiring.example").unwrap();
        assert!(
            st.last_error.is_some() && st.last_error_count > 0 && st.next_attempt_after.is_some(),
            "expiring cert inside renewal window must trigger an \
             issuance attempt on the first tick (client_for then \
             fails locally with 'no acme_tos_accepted' — that's the \
             observable, fully hermetic): {st:?}"
        );
        let msg = st.last_error.as_ref().unwrap();
        assert!(
            msg.contains("acme_tos_accepted"),
            "expected pre-network tos-missing failure, got: {msg}"
        );
    }

    /// Force-renew BLOCKER fix: a cert outside the 30d renewal window
    /// AND a vhost still in cooldown — `tick_once` would normally skip
    /// both. When the fqdn is in `force_renew_fqdns`, both gates MUST
    /// be bypassed for THIS tick (the operator's `webd.acme.renew` is
    /// the explicit override). Observed via the hermetic "no
    /// acme_tos_accepted" failure path used in
    /// [`tick_once_inside_renewal_window_attempts_issuance`]: the
    /// failure is the proof that the gates were bypassed and issuance
    /// was actually attempted.
    #[tokio::test]
    async fn tick_once_force_renew_bypasses_window_and_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let prod_plan = AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "forced.example".to_string(),
            aliases: vec![],
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
            contact_email: "op@example".into(),
        };
        let mut p = _new_provisioner(tmp.path().to_path_buf(), vec![prod_plan]);
        // Cert valid for 60 days — comfortably OUTSIDE the 30d window
        // (would normally skip via the renewal-window gate).
        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        _write_meta_with_provider(
            tmp.path(),
            "forced.example",
            now_unix + 60 * 24 * 60 * 60,
            WebdAcmeProvider::LetsEncryptProd,
        );
        // AND arm cooldown 1h into the future — both gates closed.
        {
            let st = p.vhost_state.entry("forced.example".into()).or_default();
            st.last_error_count = 3;
            st.next_attempt_after = Some(OffsetDateTime::now_utc() + Duration::from_secs(3600));
        }
        // Plain tick_once: BOTH gates trip → no state change.
        p.tick_once().await;
        let st = p.vhost_state.get("forced.example").unwrap();
        assert_eq!(
            st.last_error_count, 3,
            "without force-renew, both gates must close → no advance"
        );
        let prior_next = st.next_attempt_after;
        // Queue force-renew + tick_once again.
        p.queue_force_renew("forced.example".into()).await;
        // The verb's notify_one wakes the loop too; here we drive tick
        // directly since we're testing the bypass arm, not the wake.
        p.tick_once().await;
        let st = p.vhost_state.get("forced.example").unwrap();
        // Bypass succeeded: issuance was attempted, the hermetic
        // "no acme_tos_accepted" failure recorded a new error count
        // AND a fresh next_attempt_after (post-failure backoff). If
        // either gate had still tripped, neither field would change.
        assert!(
            st.last_error_count > 3,
            "force-renew must bypass the timing gates and attempt \
             issuance (which fails locally with 'no acme_tos_accepted' \
             in this hermetic test): {st:?}"
        );
        assert_ne!(
            st.next_attempt_after, prior_next,
            "force-renew attempt must arm a fresh post-failure cooldown",
        );
        // Single-shot: the force-renew set must be drained.
        let snapshot: HashSet<String> = p.force_renew_fqdns.lock().await.clone();
        assert!(
            snapshot.is_empty(),
            "force-renew set must drain after tick: {snapshot:?}"
        );
    }

    #[tokio::test]
    async fn tick_once_respects_active_cooldown() {
        // A vhost in cooldown (`next_attempt_after` in the future)
        // must skip the issuance attempt regardless of expiry — the
        // cooldown gate is the only thing preventing a stuck vhost
        // from burning an LE order budget on every interval.
        let tmp = tempfile::tempdir().unwrap();
        let plans = vec![_staging_plan("cool.example")];
        let mut p = _new_provisioner(tmp.path().to_path_buf(), plans);
        // Cert expired 1d ago — would normally trigger issuance.
        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        _write_meta(tmp.path(), "cool.example", now_unix - 24 * 60 * 60);
        // Pre-load a cooldown 1h into the future.
        let st = p.vhost_state.entry("cool.example".into()).or_default();
        st.last_error_count = 3;
        st.next_attempt_after = Some(OffsetDateTime::now_utc() + Duration::from_secs(3600));
        let prior_count = st.last_error_count;
        p.tick_once().await;
        // The state must NOT have been mutated — no extra failure
        // attributed to a vhost that was correctly held off.
        let st = p.vhost_state.get("cool.example").unwrap();
        assert_eq!(
            st.last_error_count, prior_count,
            "cooldown must skip the attempt; no error increment expected"
        );
    }

    #[tokio::test]
    async fn tick_once_unreadable_meta_surfaces_anomaly_without_backoff() {
        // Live meta missing/unreadable is a runtime anomaly, not a
        // renewal failure — the backoff machinery (`last_error_count`
        // + `next_attempt_after`) must NOT advance, or a transient
        // local-disk hiccup would pin the vhost into the 7d cooldown
        // ceiling. BUT once the `webd.tls.status` Bus surface exists,
        // a silent skip leaves operators querying the mirror with a
        // stale "healthy" view of a cert that can no longer renew —
        // so `last_error` IS set so the anomaly is visible in the
        // status snapshot. A subsequent successful renewal clears it
        // via the success branch.
        let tmp = tempfile::tempdir().unwrap();
        let plans = vec![_staging_plan("nometa.example")];
        let mut p = _new_provisioner(tmp.path().to_path_buf(), plans);
        // Do NOT write a meta file — the live/ dir doesn't even exist.
        p.tick_once().await;
        let st = p.vhost_state.get("nometa.example").unwrap();
        assert_eq!(
            st.last_error_count, 0,
            "meta-unreadable must not advance backoff counter"
        );
        assert!(
            st.next_attempt_after.is_none(),
            "meta-unreadable must not arm cooldown"
        );
        assert!(
            st.last_error.is_some(),
            "meta-unreadable must surface as last_error for the Bus status mirror"
        );
    }

    #[tokio::test]
    async fn republish_tls_config_is_noop_without_handle() {
        // Test paths construct provisioners with no attached listeners
        // (no `attach_tls_listeners` call → `tls_listeners` empty).
        // `republish_tls_config` MUST tolerate that — the alternative
        // is panicking the renewal loop the first time it tries to
        // publish, which would surface as a `JoinError` in the
        // listener's `try_join!` and tear the daemon down.
        let tmp = tempfile::tempdir().unwrap();
        let p = _new_provisioner(tmp.path().to_path_buf(), Vec::new());
        // C4b: republish_tls_config now returns Result<(), PublishError>.
        // With no attached listeners the worker short-circuits to
        // Ok(()) so the next tick can still proceed.
        assert!(p.republish_tls_config().is_ok());
    }

    // -----------------------------------------------------------------------
    // V-O1 — C4a namespace-event ingestion + fresh issuance + additive
    // snapshot reconcile.
    //
    // Hermetic strategy: every test that wants to observe an issuance
    // attempt uses a *prod* plan / row with `tos_acceptance: None`,
    // because `client_for` fails locally with the
    // `"no acme_tos_accepted"` error before any network call. The
    // observable is `vhost_state[fqdn].last_error` — present means
    // `issue_one` was reached; absent means a coverage gate
    // short-circuited.
    // -----------------------------------------------------------------------

    /// Build a `VhostRow` for an ACME-prod-configured vhost. `enabled`
    /// defaults to true; `cert_blob_id` is None (uncovered); set to
    /// `Some` to flip the coverage gate.
    fn _acme_prod_row(fqdn: &str) -> VhostRow {
        VhostRow {
            fqdn: fqdn.to_string(),
            enabled: true,
            www_dir: "/var/www/test".to_string(),
            aliases: Vec::new(),
            source: Some("bus_runtime".to_string()),
            acme_provider: Some("letsencrypt_prod".to_string()),
            acme_challenge: Some("http01".to_string()),
            acme_contact_email: Some("op@example".to_string()),
            tls_cert_path: None,
            tls_key_path: None,
            cert_blob_id: None,
            key_blob_id: None,
            not_after: None,
            last_attempt: None,
            last_error_count: None,
            last_error: None,
        }
    }

    /// Build an ACME-aware provisioner for the V-O1 tests. Same
    /// hermetic shape as `_new_provisioner`, but with `tos_acceptance =
    /// None` so a prod-row issuance attempt fails inside `client_for`
    /// before any network call.
    fn _new_acme_provisioner(acme_dir: PathBuf) -> AcmeProvisioner {
        AcmeProvisioner::new(
            acme_dir,
            Vec::new(),
            None,
            Arc::new(RwLock::new(HashMap::new())),
            Vec::new(),
            Duration::from_secs(3600),
            None,
        )
        .expect("construct provisioner")
    }

    #[tokio::test]
    async fn vhost_set_event_for_new_acme_vhost_triggers_immediate_issuance() {
        // V-O1.1 — A `VhostSet` for a fresh ACME row (enabled, no
        // cert_blob_id, not in acme_identities) must drive the
        // fresh-issue path: plan inserted into self.plans,
        // issue_one called (and observed via the pre-network
        // "no acme_tos_accepted" failure).
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        let row = _acme_prod_row("fresh.example");
        let event = NamespaceEvent::VhostSet {
            fqdn: "fresh.example".to_string(),
            row: Box::new(row),
        };
        p.handle_namespace_event(event).await;
        assert!(
            p.plans.iter().any(|pl| pl.fqdn == "fresh.example"),
            "plan must be inserted on VhostSet for a new ACME row"
        );
        let st = p
            .vhost_state
            .get("fresh.example")
            .expect("vhost_state populated on issue attempt");
        assert!(
            st.last_error.is_some(),
            "issue_one must be reached and fail with the pre-network \
             tos-missing error: {st:?}"
        );
        assert_eq!(
            st.last_error_count, 1,
            "first failure must record error_count=1"
        );
        assert!(
            st.next_attempt_after.is_some(),
            "first failure must arm cooldown"
        );
    }

    #[tokio::test]
    async fn vhost_set_event_for_existing_vhost_with_cert_is_no_op() {
        // V-O1.2 — A `VhostSet` carrying a populated `cert_blob_id`
        // (set by a prior run, persisted across restart) must
        // short-circuit `issue_one` via the coverage split, BUT the
        // plan MUST still be registered in `self.plans` so the
        // renewal tick observes it. Codex C4a rev-2 MAJOR fix: plan
        // registration is unconditional for enabled ACME rows; the
        // coverage split only gates fresh-issuance.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        let mut row = _acme_prod_row("covered.example");
        row.cert_blob_id =
            Some("fs:/var/lib/cosmix/webd/acme/covered.example/live/fullchain.pem".into());
        let event = NamespaceEvent::VhostSet {
            fqdn: "covered.example".to_string(),
            row: Box::new(row),
        };
        p.handle_namespace_event(event).await;
        assert!(
            p.plans.iter().any(|pl| pl.fqdn == "covered.example"),
            "covered row MUST register a plan so the renewal tick \
             observes it (split plan-registration from issuance per \
             Codex C4a rev-2 MAJOR fix)"
        );
        // `vhost_state` is touched on plan insertion (default
        // initialised) but no failure should be recorded since
        // `issue_one` was not called.
        let st = p
            .vhost_state
            .get("covered.example")
            .expect("plan insertion default-initialises vhost_state");
        assert!(
            st.last_error.is_none() && st.last_error_count == 0,
            "covered row must NOT trigger an issue attempt: \
             vhost_state={st:?}"
        );
        assert!(
            !p.acme_identities.contains_key("covered.example"),
            "covered row must NOT populate acme_identities (the on-disk \
             PEM is loaded by C4b's load-runtime-identities arm, not by \
             apply_vhost_row)"
        );
    }

    #[tokio::test]
    async fn vhost_set_event_for_disabled_vhost_skips_issuance() {
        // V-O1.3 — `enabled=false` is the routing-layer drop signal;
        // it must also drop the fresh-issue path. The provisioner
        // must not burn an LE order on a row the directory will
        // skip dispatching to.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        let mut row = _acme_prod_row("disabled.example");
        row.enabled = false;
        let event = NamespaceEvent::VhostSet {
            fqdn: "disabled.example".to_string(),
            row: Box::new(row),
        };
        p.handle_namespace_event(event).await;
        assert!(
            p.plans.iter().all(|pl| pl.fqdn != "disabled.example"),
            "disabled row must NOT cause a plan insertion"
        );
        assert!(
            !p.vhost_state.contains_key("disabled.example"),
            "disabled row must NOT mutate vhost_state"
        );
    }

    /// Spin up an in-memory `webd.vhosts` namespace + provisioner with
    /// the runtime attached. Returns the (provisioner, runtime, rx)
    /// triple so the test can drive both sides — the `rx` lifeline is
    /// captured to keep the hook channel open; tests that drop it
    /// would re-introduce the `try_send` `Closed` shape, which is not
    /// what we're exercising in V-O1.4 / V-O1.5.
    fn _new_acme_provisioner_with_runtime(
        acme_dir: PathBuf,
    ) -> (
        AcmeProvisioner,
        Arc<Runtime>,
        mpsc::Receiver<NamespaceEvent>,
    ) {
        use crate::vhosts_namespace::register_vhosts_namespace;
        use cosmix_props::PropsRouter;
        use cosmix_props::sqlite::SqliteStore;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        let store = Arc::new(SqliteStore::new("webd", conn).expect("store"));
        let mut router = PropsRouter::new("webd");
        let (runtime, rx) =
            register_vhosts_namespace(&mut router, &store).expect("register webd.vhosts namespace");
        let mut p = _new_acme_provisioner(acme_dir);
        p.vhosts_runtime = Some(runtime.clone());
        // C4b — attach an empty arc-swap + a fresh lock map so the
        // republish-before-writeback path can publish a snapshot in
        // tests that use this helper. Without these the C4a-era
        // retry/reconcile tests would hit `PublishError::NoVhostsHandle`
        // and skip writeback.
        p.attach_node_vhosts(
            Arc::new(ArcSwap::from(Arc::new(VhostDirectory::empty()))),
            HashMap::new(),
            HashSet::new(),
        );
        p.attach_key_locks(Arc::new(TokioMutex::new(HashMap::new())));
        (p, runtime, rx)
    }

    /// Write a `VhostRow` directly into the namespace via
    /// `Runtime::set_with_origin(.., WriteOrigin::backend())`. Used by
    /// V-O1.4 / V-O1.5 to seed the namespace without going through the
    /// (not-yet-built) C5 ergonomic `vhost.add` verb. Uses the same
    /// backend-origin path the bootstrap upsert uses so the
    /// daemon-owned `source` column passes the `before_set` gate.
    async fn _write_row(runtime: &Runtime, row: &VhostRow) {
        use crate::vhosts_namespace::{namespace_name, vhost_row_to_value};
        let body = vhost_row_to_value(row).expect("row to value");
        let key = RecordKey::collection(namespace_name(), row.fqdn.clone());
        runtime
            .set_with_origin(
                key,
                body,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("webd-test"),
                    cause: Some("v_o1_test".into()),
                    ts_ms: 0,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("seed row via set_with_origin");
    }

    #[tokio::test]
    async fn channel_full_drops_event_but_snapshot_reconcile_recovers() {
        // V-O1.4 — The bounded mpsc channel + `try_send` failure
        // mode is the substrate's primary recovery surface. Seed N
        // rows directly into the namespace *without* draining the
        // hook channel between them — every after_set event lands
        // in the rx buffer (capped at PROVISIONER_EVENT_CHANNEL_CAPACITY).
        // Then run the additive reconcile and assert every row was
        // picked up.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        // Seed 8 rows (well under the 128-cap, but the reconcile arm
        // doesn't care about queue depth — it reads namespace state).
        for i in 0..8 {
            let fqdn = format!("burst{i}.example");
            let row = _acme_prod_row(&fqdn);
            _write_row(&runtime, &row).await;
        }
        // Run the additive reconcile WITHOUT draining the hook
        // channel — the recovery contract says the reconcile must
        // be sufficient on its own, regardless of how many events
        // were dropped.
        p.snapshot_reconcile_additive().await;
        for i in 0..8 {
            let fqdn = format!("burst{i}.example");
            assert!(
                p.plans.iter().any(|pl| pl.fqdn == fqdn),
                "snapshot_reconcile_additive must insert plan for {fqdn} \
                 even when no VhostSet event was consumed"
            );
            let st = p.vhost_state.get(&fqdn).expect("vhost_state populated");
            assert!(
                st.last_error.is_some(),
                "{fqdn}: issue_one must be reached via the reconcile arm"
            );
        }
    }

    #[tokio::test]
    async fn notify_snapshot_reconcile_picks_up_plan_added_via_props_set() {
        // V-O1.5 — A row written directly via Runtime::set_with_origin
        // (the path C5's ergonomic `vhost.add` verb will use, and the
        // path a raw `webd.props.set` from an external caller goes
        // through) must be picked up by `snapshot_reconcile_additive`
        // even if the after_set event was dropped. This is the
        // load-bearing durability test for the dropped-VhostSet hole.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        let row = _acme_prod_row("late.example");
        _write_row(&runtime, &row).await;
        p.snapshot_reconcile_additive().await;
        assert!(
            p.plans.iter().any(|pl| pl.fqdn == "late.example"),
            "snapshot_reconcile_additive must observe namespace rows \
             written via Runtime::set_with_origin"
        );
        let st = p
            .vhost_state
            .get("late.example")
            .expect("vhost_state populated on reconcile-driven issue attempt");
        assert!(
            st.last_error.is_some(),
            "issue_one must be reached via the reconcile arm: {st:?}"
        );
    }

    #[tokio::test]
    async fn apply_vhost_row_skips_failsoft_disabled_fqdn() {
        // B1 — a fail-soft-disabled fqdn (in `self.disabled_hosts`) must
        // NOT be re-adopted as a plan via the namespace event /
        // reconcile path, even though its `config_bootstrap` row is
        // enabled. Otherwise the provisioner would try to issue/serve a
        // vhost `resolve_node_state` deemed unservable, diverging from
        // the directory `republish_vhost_directory` already filters.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        p.disabled_hosts.insert("disabled.example".to_string());
        let row = _acme_prod_row("disabled.example");
        p.apply_vhost_row("disabled.example", row)
            .await
            .expect("apply on a disabled fqdn must be a no-op Ok");
        assert!(
            p.plans.iter().all(|pl| pl.fqdn != "disabled.example"),
            "disabled fqdn must NOT be adopted into self.plans"
        );
        assert!(
            !p.vhost_state.contains_key("disabled.example"),
            "disabled fqdn must NOT seed per-vhost cooldown state"
        );
    }

    #[tokio::test]
    async fn apply_vhost_row_is_idempotent_under_duplicate_delivery() {
        // V-O1.6 — Duplicate `VhostSet` delivery for the same fqdn
        // (e.g. the channel arm + the snapshot reconcile arm both
        // see the same row, or the substrate delivers the same
        // event twice across reconnect) must not multiply issuance
        // attempts. The first call records last_error_count=1 and
        // arms the cooldown; the second call hits the cooldown gate
        // before re-entering issue_one, leaving last_error_count
        // unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        let row = _acme_prod_row("dup.example");
        p.apply_vhost_row("dup.example", row.clone())
            .await
            .expect("first apply Ok");
        let count_after_first = p
            .vhost_state
            .get("dup.example")
            .expect("seeded")
            .last_error_count;
        assert_eq!(count_after_first, 1, "first call must record one failure");
        // Deliver again — same row, same fqdn. Cooldown gate must
        // skip the re-entry.
        p.apply_vhost_row("dup.example", row)
            .await
            .expect("second apply Ok");
        let count_after_second = p
            .vhost_state
            .get("dup.example")
            .expect("still present")
            .last_error_count;
        assert_eq!(
            count_after_second, 1,
            "duplicate delivery must not multiply issuance attempts under \
             the cooldown gate (count_after_first={count_after_first}, \
             count_after_second={count_after_second})"
        );
    }

    #[tokio::test]
    async fn vhost_removed_event_warn_logs_and_drops() {
        // V-O1.7 (extra) — The C4a state observer for VhostRemoved
        // must WARN-log and drop. Test the observable: the call
        // returns (no panic, no error), no plan is inserted, no
        // vhost_state side-effect.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_acme_provisioner(tmp.path().to_path_buf());
        let event = NamespaceEvent::VhostRemoved {
            fqdn: "removed.example".to_string(),
        };
        p.handle_namespace_event(event).await;
        assert!(
            p.plans.is_empty(),
            "VhostRemoved must not affect self.plans in C4a"
        );
        assert!(
            p.vhost_state.is_empty(),
            "VhostRemoved must not affect vhost_state in C4a"
        );
    }

    #[tokio::test]
    async fn apply_vhost_row_retries_writeback_when_identity_present_but_cert_absent() {
        // V-O1.8 (extra) — Codex C4a rev-3 MAJOR fix pin. When
        // `acme_identities` is populated (in-process issuance
        // succeeded) but `row.cert_blob_id` is still `None` (the
        // initial writeback failed or raced an operator patch), a
        // subsequent VhostSet / reconcile pass MUST retry the
        // writeback rather than short-circuit. Without this pin
        // the writeback would only happen for the current process
        // lifetime and the row would silently lose its
        // cert_blob_id stamp across restarts.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        // Stage a fake live cert dir + meta.json so the writeback's
        // not_after read succeeds (the writeback path needs a
        // readable meta.json to compute the not_after column).
        let live_dir = p.acme_dir.join("retry.example").join(LIVE_DIR);
        std::fs::create_dir_all(&live_dir).expect("mk live dir");
        std::fs::write(live_dir.join(FULLCHAIN_FILE), b"PEM-FULLCHAIN").expect("write fullchain");
        std::fs::write(live_dir.join(PRIVKEY_FILE), b"PEM-KEY").expect("write key");
        // Synthesise a real MetaV1 with a far-future not_after,
        // then write it through the crate's `write_meta_json` so
        // the schema check stays canonical.
        let meta = MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: "https://example.test/acme/order/test".into(),
            account_url: "https://example.test/acme/acct/test".into(),
            not_before_unix: 0,
            not_after_unix: 4_102_444_800_i64, // 2100-01-01
            issued_at_unix: 0,
            fqdn: "retry.example".to_string(),
            aliases: Vec::new(),
            provider: WebdAcmeProvider::LetsEncryptProd,
            acme_directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
        };
        write_meta_json(&live_dir.join(META_FILE), &meta).expect("write meta");
        // Seed the namespace with a row that has NO cert_blob_id.
        // Plant a fake identity in `acme_identities` to simulate
        // "in-memory issuance succeeded, writeback failed last
        // round".
        let row = _acme_prod_row("retry.example");
        _write_row(&runtime, &row).await;
        p.acme_identities.insert(
            "retry.example".to_string(),
            TlsIdentityConfig {
                server_name: "retry.example".to_string(),
                cert: live_dir.join(FULLCHAIN_FILE).to_string_lossy().into_owned(),
                key: live_dir.join(PRIVKEY_FILE).to_string_lossy().into_owned(),
                default: false,
                no_sni_fallback: false,
            },
        );
        // The second-arm event delivery must trigger writeback
        // retry. Use the snapshot reconcile path (equivalent shape).
        p.snapshot_reconcile_additive().await;
        // Confirm the writeback landed: re-read the row and assert
        // cert_blob_id was populated.
        let key = RecordKey::collection(
            crate::vhosts_namespace::namespace_name(),
            "retry.example".to_string(),
        );
        let snap = runtime
            .store()
            .get(&key)
            .await
            .expect("row must still exist post-reconcile");
        let body = match snap.value.value {
            PropValue::Object(o) => o,
            other => panic!("row body must be object, got {other:?}"),
        };
        let cert = body
            .get("cert_blob_id")
            .expect("writeback must populate cert_blob_id on retry");
        match cert {
            PropValue::String(s) => assert!(
                s.starts_with("fs:") && s.contains("retry.example"),
                "cert_blob_id must be the fs: marker for the live dir: {s}"
            ),
            other => panic!("cert_blob_id must be string, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_wakes_run_forever_iteration() {
        // The notify-handle wake path: with `renewal_tick` set far
        // beyond the test budget, after the loop's immediate-first
        // interval tick fires, the only way `tick_once` runs again
        // is via `notify_one`. Spawn `run_forever`, observe the
        // immediate tick land, kick the notify, then observe a
        // second `tick_once` to prove the wake path is wired.
        let tmp = tempfile::tempdir().unwrap();
        let mut p = _new_provisioner(tmp.path().to_path_buf(), Vec::new());
        // Override the millisecond tick set by `_new_provisioner` to
        // a value far beyond the test budget so `notify_one` is the
        // only source of additional `tick_once` calls after the
        // immediate-first interval fire.
        p.renewal_tick = Duration::from_secs(3600);
        let notify = p.notify_handle();
        let counter = p.tick_count_handle();
        let task = tokio::spawn(async move { p.run_forever().await });
        // Wait for the immediate-first interval tick (per MAJOR 1
        // fix: `run_forever` does NOT consume the first tick).
        let mut before = 0u64;
        for _ in 0..50 {
            before = counter.load(Ordering::Relaxed);
            if before >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            before >= 1,
            "immediate-first tick did not land within 500ms; got tick_count={before}"
        );
        // Now kick the wake path and observe a strictly-greater count.
        notify.notify_one();
        let mut after = before;
        for _ in 0..50 {
            after = counter.load(Ordering::Relaxed);
            if after > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        let _ = task.await;
        assert!(
            after > before,
            "notify_one did not gate a tick_once; before={before} after={after}"
        );
    }

    // -----------------------------------------------------------------------
    // V-O2 — C4b VhostRemoved cleanup + arc-swap ordering + archive helpers.
    //
    // The seven slots correspond 1:1 to spec lines 1407–1483 + 1614 +
    // archive_orphan/snapshot_reconcile follow-ups:
    //   * VhostRemoved success (archive lands, both arcs republish,
    //     in-memory state cleared).
    //   * VhostRemoved idempotent short-circuit (no plan / no identity).
    //   * snapshot_reconcile cleanup arm (plan in-memory but row gone).
    //   * archive_orphan_acme_dirs (3 shapes: clean, with-known, with-orphan).
    //   * republish failure does not advance writeback (NoVhostsHandle).
    //   * stale event short-circuits when row re-added.
    //   * archive_live_dir idempotent on missing live/.
    //
    // The 10th spec test (V-O2 publish-before-write ordering observed
    // via PublishInstrumentation) is gated to a later wiring slice
    // because the instrumentation needs both arcs attached AND a fake
    // post-issuance flow exercised end-to-end without an LE network
    // call. The seven below cover the observable invariants the
    // ordering test would catch.
    // -----------------------------------------------------------------------

    /// Helper: stage a fake live cert dir + valid meta.json under
    /// `<acme_dir>/<fqdn>/live/`. Mirrors the shape produced by a
    /// successful `promote_staged_to_live`.
    fn _stage_fake_live(acme_dir: &Path, fqdn: &str) {
        let live = acme_dir.join(fqdn).join(LIVE_DIR);
        std::fs::create_dir_all(&live).expect("mk live dir");
        std::fs::write(live.join(FULLCHAIN_FILE), b"PEM-FULLCHAIN").unwrap();
        std::fs::write(live.join(PRIVKEY_FILE), b"PEM-KEY").unwrap();
        let meta = MetaV1 {
            version: MetaV1::CURRENT_VERSION,
            order_url: "https://example.test/acme/order/test".into(),
            account_url: "https://example.test/acme/acct/test".into(),
            not_before_unix: 0,
            not_after_unix: 4_102_444_800_i64,
            issued_at_unix: 0,
            fqdn: fqdn.to_string(),
            aliases: Vec::new(),
            provider: WebdAcmeProvider::LetsEncryptProd,
            acme_directory_url: "https://acme-v02.api.letsencrypt.org/directory".into(),
        };
        write_meta_json(&live.join(META_FILE), &meta).unwrap();
    }

    #[tokio::test]
    async fn vhost_removed_archives_live_and_clears_in_memory_state() {
        // V-O2.1 — happy path. `VhostRemoved` arm archives the live
        // cert, drops `plans` / `acme_identities` / `vhost_state`,
        // and republishes both arc-swaps so the resolver no longer
        // serves the removed identity.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, _runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        _stage_fake_live(&p.acme_dir, "gone.example");
        // Seed plan + identity + vhost_state as if a prior fresh-issue
        // had landed.
        let plan = AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "gone.example".into(),
            aliases: Vec::new(),
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: WebdAcmeChallenge::Http01,
            contact_email: "ops@example.test".into(),
        };
        p.plans.push(plan);
        p.acme_identities.insert(
            "gone.example".into(),
            TlsIdentityConfig {
                server_name: "gone.example".into(),
                cert: "fs:/tmp/whatever".into(),
                key: "fs:/tmp/whatever".into(),
                default: false,
                no_sni_fallback: false,
            },
        );
        p.vhost_state.insert(
            "gone.example".into(),
            ProvisionerVhostState {
                last_error: Some("prior".into()),
                last_error_count: 1,
                next_attempt_after: None,
            },
        );
        // No namespace row written — so the pre-stale guard sees
        // "row genuinely gone" and the cleanup proceeds.
        p.handle_vhost_removed("gone.example")
            .await
            .expect("VhostRemoved cleanup must succeed");
        assert!(
            !p.plans.iter().any(|x| x.fqdn == "gone.example"),
            "plan must be dropped"
        );
        assert!(
            !p.acme_identities.contains_key("gone.example"),
            "acme_identities entry must be dropped"
        );
        assert!(
            !p.vhost_state.contains_key("gone.example"),
            "vhost_state entry must be dropped (Codex E.5)"
        );
        // live/ moved under archive/<ts>/live/.
        let live = p.acme_dir.join("gone.example").join(LIVE_DIR);
        assert!(!live.exists(), "live/ must be moved away");
        let archive_root = p.acme_dir.join("gone.example").join(ARCHIVE_DIR);
        assert!(
            archive_root.exists(),
            "archive/ must exist after VhostRemoved cleanup"
        );
        let mut found_archived = false;
        for entry in std::fs::read_dir(&archive_root).unwrap() {
            let ts_dir = entry.unwrap().path();
            if ts_dir.join(LIVE_DIR).exists() {
                found_archived = true;
                break;
            }
        }
        assert!(
            found_archived,
            "archive/<ts>/live/ must exist with the moved files"
        );
    }

    #[tokio::test]
    async fn vhost_removed_is_idempotent_when_plan_absent() {
        // V-O2.2 — idempotent short-circuit (Codex E.1). A second
        // `VhostRemoved` for a fqdn the provisioner does not track
        // is a no-op. Without the short-circuit the cleanup arm
        // would still take the per-fqdn lock and re-read the
        // namespace; the short-circuit avoids that overhead AND
        // makes the duplicate-event case observable as a true no-op.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, _runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        // No plan / no identity / no state. Cleanup must succeed
        // without panicking and without creating any archive dirs.
        p.handle_vhost_removed("unknown.example")
            .await
            .expect("idempotent short-circuit must Ok");
        assert!(
            !p.acme_dir.join("unknown.example").exists(),
            "no fqdn dir must be created by a short-circuited cleanup"
        );
    }

    #[tokio::test]
    async fn snapshot_reconcile_runs_cleanup_for_plan_with_missing_row() {
        // V-O2.3 — the cleanup arm of `snapshot_reconcile_additive`
        // catches a dropped `VhostRemoved` event. Plant a plan +
        // identity + a fake live cert in-memory but DO NOT write a
        // namespace row; reconcile must run the VhostRemoved
        // handler shape against it.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, _runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        _stage_fake_live(&p.acme_dir, "dropped.example");
        p.plans.push(AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "dropped.example".into(),
            aliases: Vec::new(),
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: WebdAcmeChallenge::Http01,
            contact_email: "ops@example.test".into(),
        });
        p.acme_identities.insert(
            "dropped.example".into(),
            TlsIdentityConfig {
                server_name: "dropped.example".into(),
                cert: "fs:/tmp".into(),
                key: "fs:/tmp".into(),
                default: false,
                no_sni_fallback: false,
            },
        );
        p.snapshot_reconcile_additive().await;
        assert!(
            !p.plans.iter().any(|x| x.fqdn == "dropped.example"),
            "snapshot_reconcile must run cleanup handler for plan-without-row"
        );
        assert!(
            !p.acme_identities.contains_key("dropped.example"),
            "identity must be dropped by cleanup arm"
        );
    }

    #[test]
    fn archive_orphan_acme_dirs_archives_unknown_fqdn() {
        // V-O2.4a — orphan-scan happy path: a `<acme_root>/<fqdn>/`
        // dir whose fqdn is NOT in known_fqdns gets moved under
        // `.orphan-archive/<fqdn>-<startup_ts>/` (spec form per
        // `_doc/planned/webd-vhosts-phase3.md`).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("orphan.example").join(LIVE_DIR)).unwrap();
        std::fs::write(
            root.join("orphan.example")
                .join(LIVE_DIR)
                .join(FULLCHAIN_FILE),
            b"X",
        )
        .unwrap();
        let mut known = HashSet::new();
        known.insert("kept.example".to_string());
        std::fs::create_dir_all(root.join("kept.example")).unwrap();
        let stamp_ts = OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
        let n =
            archive_orphan_acme_dirs(&root, &known, stamp_ts).expect("orphan scan must succeed");
        assert_eq!(n, 1, "exactly one orphan must be archived");
        assert!(
            !root.join("orphan.example").exists(),
            "orphan dir must be moved away from root"
        );
        assert!(
            root.join("kept.example").exists(),
            "known fqdn dir must stay in place"
        );
        let orphan_root = root.join(".orphan-archive");
        assert!(
            orphan_root.exists(),
            ".orphan-archive bucket must be created"
        );
        let stamp = format_stamp(stamp_ts);
        let archived = orphan_root.join(format!("orphan.example-{stamp}"));
        assert!(
            archived.exists(),
            "spec layout requires .orphan-archive/<fqdn>-<ts>/ entry: {}",
            archived.display()
        );
    }

    #[test]
    fn archive_orphan_acme_dirs_returns_zero_when_clean() {
        // V-O2.4b — no orphans → no .orphan-archive/ created, return 0.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("ok.example")).unwrap();
        let mut known = HashSet::new();
        known.insert("ok.example".to_string());
        let n = archive_orphan_acme_dirs(
            &root,
            &known,
            OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(),
        )
        .expect("orphan scan must succeed");
        assert_eq!(n, 0);
        assert!(
            !root.join(".orphan-archive").exists(),
            ".orphan-archive must not be created when there are no orphans"
        );
    }

    #[test]
    fn archive_orphan_acme_dirs_skips_dot_and_underscore_entries() {
        // V-O2.4c — `.orphan-archive/` itself (or any `.*` / `_*`
        // dir) must NOT be treated as an orphan candidate; otherwise
        // a second startup would recursively archive a prior orphan
        // bucket.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(
            root.join(".orphan-archive")
                .join("prior.example-2026-05-26T000000Z"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("_legacy")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        let known: HashSet<String> = HashSet::new();
        let n = archive_orphan_acme_dirs(
            &root,
            &known,
            OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(),
        )
        .expect("orphan scan must succeed");
        assert_eq!(n, 0, "_* and .* entries must be skipped");
        assert!(root.join(".orphan-archive").exists());
        assert!(root.join("_legacy").exists());
        assert!(root.join(".hidden").exists());
    }

    #[tokio::test]
    async fn vhost_removed_short_circuits_when_row_readded() {
        // V-O2.5 — pre-stale-event guard. The provisioner has a
        // plan + identity for fqdn X; a `VhostRemoved` event fires;
        // before the cleanup arm starts, the namespace row is
        // re-added (vhost.add re-issue path). The cleanup arm MUST
        // observe the row's return and skip the archive — otherwise
        // it would archive the freshly-issued cert.
        let tmp = tempfile::tempdir().unwrap();
        let (mut p, runtime, _rx) = _new_acme_provisioner_with_runtime(tmp.path().to_path_buf());
        _stage_fake_live(&p.acme_dir, "readded.example");
        p.plans.push(AcmeVhostPlan {
            vhost_index: 0,
            fqdn: "readded.example".into(),
            aliases: Vec::new(),
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: WebdAcmeChallenge::Http01,
            contact_email: "ops@example.test".into(),
        });
        p.acme_identities.insert(
            "readded.example".into(),
            TlsIdentityConfig {
                server_name: "readded.example".into(),
                cert: "fs:/tmp/c".into(),
                key: "fs:/tmp/k".into(),
                default: false,
                no_sni_fallback: false,
            },
        );
        // Write a row simulating the re-add landing before cleanup.
        _write_row(&runtime, &_acme_prod_row("readded.example")).await;
        p.handle_vhost_removed("readded.example")
            .await
            .expect("guarded cleanup must Ok-skip");
        // Plan + identity must remain — cleanup short-circuited.
        assert!(
            p.plans.iter().any(|x| x.fqdn == "readded.example"),
            "plan must survive when row is re-added before cleanup"
        );
        assert!(
            p.acme_identities.contains_key("readded.example"),
            "identity must survive when row is re-added before cleanup"
        );
        // live/ untouched.
        assert!(
            p.acme_dir.join("readded.example").join(LIVE_DIR).exists(),
            "live/ must NOT be archived when the row was re-added"
        );
    }

    #[tokio::test]
    async fn archive_live_dir_is_idempotent_on_missing_live() {
        // V-O2.6 — calling archive_live_dir for a fqdn whose live/
        // doesn't exist returns Ok without touching the filesystem.
        // Mirrors the duplicate `VhostRemoved` event shape after the
        // first cleanup already archived the bytes.
        let tmp = tempfile::tempdir().unwrap();
        let p = _new_provisioner(tmp.path().to_path_buf(), Vec::new());
        p.archive_live_dir("nonexistent.example")
            .expect("missing live/ must be Ok");
        assert!(
            !p.acme_dir.join("nonexistent.example").exists(),
            "no fqdn dir must be created"
        );
    }
}
