//! `LogHandle`, `LogReloadHandle`, and `LogError`.
//!
//! `LogHandle` owns the `tracing_appender::WorkerGuard`s, the live
//! filter reload handle, and any other per-init state. **Dropping it
//! flushes pending file writes.** Binaries hold it for process lifetime
//! — same contract as today's `cosmix-lib-daemon::init_tracing` return
//! value.

use thiserror::Error;

/// Errors `init` and the reload/prometheus paths may produce. Sink
/// failures (file open, journald connect) are *not* fatal — the library
/// logs them once to stderr (if available) and continues; only
/// configuration errors that make the subscriber un-installable surface
/// here.
#[derive(Debug, Error)]
pub enum LogError {
    /// `EnvFilter` directive failed to parse.
    #[error("invalid log filter directive: {0}")]
    InvalidFilter(String),

    /// `tracing_subscriber::registry().init()` failed — almost always
    /// because a global dispatcher is already installed (double-init).
    #[error("subscriber already installed (double init?): {0}")]
    AlreadyInstalled(String),

    /// A live filter swap via [`LogReloadHandle::reload_filter`] failed
    /// — the subscriber the reload layer belonged to was torn down.
    #[error("log filter reload failed (subscriber gone?): {0}")]
    Reload(String),

    /// A stats CLI flag or compiled default fell outside the bounds the
    /// plan declares hard-rejected at startup (plan §2 — currently the
    /// byte-budget [floor, ceiling] check). Surfaced as `Err` because
    /// the substrate `before_set` hook rejects the same out-of-band
    /// value at L1 write-time; bootstrap-time consistency demands the
    /// same posture.
    #[error("invalid stats configuration: {0}")]
    InvalidStats(String),
}

/// Internal handle to the reloadable `EnvFilter` layer. Always present
/// now (the reload layer is core). Consumers drive runtime filter swaps
/// through the public [`LogReloadHandle`] wrapper rather than touching
/// the `tracing_subscriber` reload type directly.
pub(crate) type ReloadHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

/// Process-lifetime handle returned by `init`.
///
/// Holds the appender `WorkerGuard`s and the live-filter `reload::Handle`.
/// `init` always installs a subscriber now — even `--log-level none`
/// installs `EnvFilter::off()` behind the reload layer — so `reload` is
/// `Some` on every successful `init`. Use [`LogHandle::reload_handle`]
/// to obtain a [`LogReloadHandle`] for runtime filter swaps.
pub struct LogHandle {
    // Carries the appender guards that must outlive every `tracing`
    // event.
    pub(crate) guards: Vec<tracing_appender::non_blocking::WorkerGuard>,
    /// Live filter reload handle. `Some` on every successful `init`
    /// (the `--log-level none` path installs `EnvFilter::off()` behind
    /// the reload layer rather than skipping subscriber install).
    pub(crate) reload: Option<ReloadHandle>,
    /// `true` when `init` succeeded in installing a process-global
    /// `StatsRecorder`. `shutdown()` consults this to decide whether
    /// to drive the final on-exit roll-up; if `false`, the recorder
    /// either was disabled (`--stats=off`) or lost the install race
    /// to another global recorder (degraded but non-fatal).
    pub(crate) stats_installed: bool,
    /// Idempotency flag for `shutdown()`. The Mix CLI calls
    /// `shutdown()` explicitly on every exit path (plan §4.4); the
    /// `Drop` impl also calls it as a best-effort fallback for
    /// `main()`-return exits. Either-call-is-fine semantics depend on
    /// this flag staying set across the first call.
    pub(crate) shutdown_done: bool,
}

impl LogHandle {
    /// Final on-exit hook — drive a one-shot roll-up against the
    /// process-installed `StatsRecorder` (`period=0` marks the final
    /// line for downstream readers per plan §4.1), then flush every
    /// installed sink so the `.open → .done` rename closes the
    /// durability barrier. Idempotent; safe to call multiple times.
    ///
    /// **You must call this before `std::process::exit`.** Rust
    /// destructors are skipped by `process::exit`, so relying on
    /// `LogHandle::Drop` to flush stats re-introduces the Mix
    /// empty-stats bug class for every binary that exits non-locally
    /// (plan §4.4). The `Drop` impl is best-effort for `main()`-return
    /// exits only.
    ///
    /// After `shutdown()` returns the recorder remains installed (the
    /// `metrics` global cannot be uninstalled), but every sink has
    /// finalised — subsequent `record_period` calls return errors and
    /// no further bytes reach disk. The `WorkerGuard`s held by this
    /// handle flush when the handle itself drops (which happens after
    /// this method returns).
    pub fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        if self.stats_installed {
            crate::stats::shutdown_installed_recorder();
        }
    }
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        // Best-effort fallback for `main()`-return exits. Binaries
        // that exit via `process::exit` MUST call `shutdown()`
        // explicitly — `drop` is not reached on that path.
        self.shutdown();
    }
}

impl LogHandle {
    /// The live-reload handle, if a subscriber was installed.
    ///
    /// `init` always installs a subscriber (the `--log-level none` path
    /// installs `EnvFilter::off()` behind the reload layer), so this
    /// returns `Some` on every successful `init`. The future cos
    /// `cosmix-lib-log-props` watcher and any Bus-driven filter verb
    /// drive runtime level swaps through the returned
    /// [`LogReloadHandle`].
    pub fn reload_handle(&self) -> Option<LogReloadHandle> {
        self.reload.clone().map(|inner| LogReloadHandle { inner })
    }
}

#[cfg(feature = "prometheus")]
impl LogHandle {
    /// Build the Prometheus exporter, attach it as a redaction-first
    /// child of the installed `StatsRecorder`, and spawn the
    /// `/metrics` HTTP listener on the caller's Tokio runtime
    /// (plan §8.1).
    ///
    /// `addr` is the WG-only bind from `LogDefaults.stats_prometheus_listen`
    /// (typically `Some("172.16.x.y:9100".parse().unwrap())` per
    /// `feedback_wg_only_binding`). The CLI surface accepts
    /// `--stats-prometheus-listen=""` to disable the endpoint;
    /// daemons should *not* call `attach_prometheus` in that case.
    ///
    /// Must be called *after* `init()` and with a Tokio runtime in
    /// scope — `PrometheusBuilder::build()` spawns its upkeep task
    /// with `tokio::spawn` regardless of whether the listener
    /// future itself runs. The recommended order (plan §8.1) is:
    /// `init()` → runtime up → `attach_prometheus`.
    ///
    /// Errors:
    /// - [`LogError::InvalidStats`] if `--stats=off` (no recorder is
    ///   the global), if a `PrometheusChild` has already been
    ///   attached, or if `PrometheusBuilder::build()` fails (bind
    ///   error, missing config).
    ///
    /// Consumes `self` and returns it back.
    pub async fn attach_prometheus(self, addr: std::net::SocketAddr) -> Result<Self, LogError> {
        crate::prometheus_attach::attach(addr).await?;
        Ok(self)
    }
}

/// Opaque handle to the live-reloadable `EnvFilter` layer. Hands the
/// caller (e.g. the cos `cosmix-lib-log-props` watcher, or a future
/// Mix Bus verb) the ability to swap the active filter at runtime
/// without exposing the `tracing_subscriber` reload type.
#[derive(Clone)]
pub struct LogReloadHandle {
    inner: ReloadHandle,
}

impl LogReloadHandle {
    /// Swap the active `EnvFilter`. Returns `Err` if the subscriber was
    /// torn down.
    pub fn reload_filter(&self, filter: tracing_subscriber::EnvFilter) -> Result<(), LogError> {
        self.inner
            .reload(filter)
            .map_err(|e| LogError::Reload(e.to_string()))
    }
}

impl std::fmt::Debug for LogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogHandle")
            .field("guards", &self.guards.len())
            .field("reload", &self.reload.is_some())
            .finish()
    }
}
