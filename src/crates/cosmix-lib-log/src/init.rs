//! `init()` — the single entry point every Cosmix binary calls.
//!
//! Resolves the pre-Bus precedence ladder for the bootstrap
//! subscriber: env (`RUST_LOG`) > CLI (`LogOpts`) > defaults. A
//! later runtime "props rung" (the cos extension crate's watcher)
//! layers on top by swapping the live filter through the
//! [`crate::LogReloadHandle`] this crate exposes.

use std::io::IsTerminal;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::defaults::{LogDefaults, RotationMode};
use crate::handle::{LogError, LogHandle};
use crate::opts::{LogFormat, LogLevel, LogOpts, StatsOpts, TriState};
use crate::stats::{JsonlSink, StatsRecorderBuilder};

// Core copies of the plan §4.3 numeric bounds. Held literally here
// (not imported) because the canonical substrate exports live in the
// cos extension crate's property schema (source-of-truth), while
// `init::install_stats_recorder` here needs the same numeric bound
// check. Drift is unlikely (plan-defined invariants), but if the
// substrate floor/ceiling ever moves these constants must move with
// it.
//
// `INTERVAL_SECONDS_CEILING` is load-bearing for the Prometheus
// gauge idle-timeout (see `prometheus_attach::PROMETHEUS_GAUGE_IDLE_TIMEOUT`,
// which is derived from `stats::INTERVAL_SECONDS_CEILING`). Codex
// round-6 MAJOR: the props `before_set` chokepoint alone is not
// enough — `--stats-interval` and `LogDefaults::with_stats_interval`
// also feed the rollup loop, so the same ceiling must reject the
// out-of-band value at startup before the recorder commits to a
// cadence the exporter cannot survive.
const BYTE_BUDGET_FLOOR_MIB: u64 = 16;
const BYTE_BUDGET_CEILING_MIB: u64 = 1024;
// Re-exported as `stats::INTERVAL_SECONDS_CEILING` (the prometheus
// gauge idle-timeout derives from it). The canonical substrate copy
// used to live in the now-removed `stats::props`; this is the surviving
// source-of-truth for the bound.
pub(crate) const INTERVAL_SECONDS_CEILING: u64 = 3600;

/// `~/.local/log/cosmix/` — the canonical Cosmix log directory.
///
/// Falls back to `/tmp/cosmix-log` when `$HOME` is unset (matches
/// `cosmix-lib-daemon::log_dir`'s behaviour byte-for-byte so the P2
/// migration is behaviour-preserving).
pub fn default_log_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/log/cosmix"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/cosmix-log"))
}

/// Install the global tracing subscriber per `opts` + `defaults`.
///
/// Returns a `LogHandle` the caller must hold for process lifetime.
/// Dropping it flushes pending file writes.
///
/// Precedence (highest wins): `RUST_LOG` env > `LogOpts.log_filter`
/// layered over `LogOpts.log_level` or `LogDefaults.level`. A runtime
/// rung layers on top via [`LogHandle::reload_handle`]: a consumer
/// (the cos extension watcher, or a Bus filter verb) swaps the live
/// filter through the returned [`crate::LogReloadHandle`].
///
/// # Stats install ordering (plan §3543-3559)
///
/// The substrate `StatsRecorder` is installed **before** the tracing
/// subscriber's `try_init()`. The cross-pillar
/// `cosmix_log_events_total` layer (an `EventCounterLayer` over the
/// `metrics` facade) drives `counter!()` macros on every admitted
/// event; if the recorder isn't already the global, those calls land
/// on the facade's no-op recorder and the count is lost forever.
/// Install order is `set_global_recorder` → publish to `SHARED` →
/// `try_init()` so the very first event the subscriber lets through
/// is counted by the real recorder.
pub fn init(
    opts: &LogOpts,
    stats_opts: &StatsOpts,
    defaults: LogDefaults,
) -> Result<LogHandle, LogError> {
    let effective_level = opts.log_level.unwrap_or(defaults.level);

    // Short-circuit (install no subscriber) only when the resolved
    // filter source produces *nothing* to install. The condition
    // mirrors `build_env_filter`'s priority ladder:
    //
    //   - `RUST_LOG` (non-empty) → real directive → not short-circuit.
    //     Empty `RUST_LOG=""` is treated as unset (matches
    //     `EnvFilter::try_from_default_env`'s own behaviour).
    //   - `--log-filter <s>` → real directive → not short-circuit.
    //   - `--log-level <l>` is the *explicit user choice*:
    //       `Some(None)` honours the user → short-circuit;
    //       `Some(other)` synthesises a directive → not short-circuit;
    //     CLI wins over `default_filter` per round 2 of review.
    //   - `--log-level` absent + `default_filter` set → install with
    //     the binary's hand-tuned baseline → not short-circuit.
    //   - `--log-level` absent + `default_filter` unset + defaults
    //     level is `None` → nothing to install → short-circuit.
    let env_log_set = rust_log_nonempty();
    let cli_filter_set = opts
        .log_filter
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let baseline_wants_subscriber = match opts.log_level {
        Some(LogLevel::None) => false,
        Some(_) => true,
        None => defaults.default_filter.is_some() || defaults.level != LogLevel::None,
    };
    // Resolve `stats_on` per the precedence ladder:
    //   CLI `--stats off|on` wins outright over the binary's
    //   `LogDefaults.stats` baseline; absent CLI flag falls through
    //   to the default. The substrate row (`<svc>.stats.enabled`)
    //   carries the same field but is **not** read here — the v0.1
    //   recorder is configured from CLI + defaults only and is fixed
    //   for the process lifetime; per plan §4.3, picking up substrate
    //   writes mid-run is deferred to SPEC-12 P3+. `attach_stats`
    //   observes substrate writes for audit visibility.
    let stats_on = stats_opts
        .stats
        .map(|s| s.as_bool())
        .unwrap_or(defaults.stats);

    // The `--log-level none + no env/CLI filter + level baseline ==
    // None` short-circuit decides whether we install a subscriber
    // at all. We compute it now so we can sequence validation:
    // **build_env_filter must run BEFORE install_stats_recorder on
    // the non-short-circuit path**, otherwise an invalid `RUST_LOG`
    // or `--log-filter` permanently strands the process-global
    // metrics recorder (the `metrics` crate has no uninstall path).
    // §3543-3559 only requires recorder install before `try_init()`,
    // which is still satisfied.
    let short_circuit_subscriber = !env_log_set && !cli_filter_set && !baseline_wants_subscriber;

    let filter_opt = if short_circuit_subscriber {
        debug_assert_eq!(effective_level, LogLevel::None);
        None
    } else {
        Some(build_env_filter(opts, &defaults, effective_level)?)
    };

    // Now-safe to install the recorder: every preceding step is
    // either infallible or has returned by here. The remaining
    // fallible step (`try_init` below) cleans up on error.
    //
    // Plan §3543-3559: recorder must be the process global BEFORE
    // `try_init()` so the cross-pillar EventCounterLayer counts the
    // very first admitted event into the real recorder.
    //
    // Standalone short-circuit: `--log-level none --stats on` still
    // installs the recorder so the binary's stats surface works
    // even with logging disabled.
    //
    // `Err(InvalidStats)` on bounds-check failure — no sink has been
    // opened at that point. `Ok(false)` on a lost install race —
    // the JSONL sink is **not** opened, so there is no orphan
    // `.open` file to flush.
    let stats_installed = if stats_on {
        install_stats_recorder(&defaults, stats_opts)?
    } else {
        false
    };

    // No short-circuit early-return any more: even when nothing in the
    // precedence ladder asks for a subscriber, we install
    // `EnvFilter::off()` behind the reload layer so a later filter swap
    // (via `LogReloadHandle::reload_filter`) can raise the filter at
    // runtime without restarting the binary. The `stats_installed` flag
    // is still threaded through so `LogHandle::shutdown()` drives the
    // final roll-up.

    // `filter_opt` is `Some` on every non-short-circuit path; when the
    // ladder produced nothing (`short_circuit_subscriber`), the
    // `EnvFilter::new("off")` below is the bootstrap baseline behind the
    // reload layer. `"off"` is a built-in directive so the parse cannot
    // fail in practice; the `expect` is documentation rather than a real
    // failure path.
    let filter = filter_opt.unwrap_or_else(|| {
        EnvFilter::try_new("off").expect("'off' is a valid EnvFilter directive")
    });
    let format = opts.log_format.unwrap_or(defaults.format);
    let mut guards = Vec::new();

    let ansi_on = match opts.log_color.unwrap_or(TriState::Auto) {
        TriState::Always => true,
        TriState::Never => false,
        TriState::Auto => std::io::stderr().is_terminal(),
    };

    // Resolve file sink: CLI `--log-file` overrides
    // `LogDefaults.log_file`; an explicit empty string disables it.
    let file_dir: Option<std::path::PathBuf> = match opts.log_file.as_deref() {
        Some("") => None,
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => defaults.log_file.clone(),
    };

    let journald_on = opts.log_journald.unwrap_or(defaults.journald);

    // Wrap the filter in `reload::Layer` so a `LogReloadHandle` can
    // swap it at runtime (e.g. the future cos props watcher, or a Bus
    // filter verb) without restarting the binary. This is core now.
    let (filter_layer, reload_handle) = {
        let (layer, handle) = tracing_subscriber::reload::Layer::new(filter);
        (layer, Some(handle))
    };

    let registry = tracing_subscriber::registry().with(filter_layer);

    // The layer-stack is fixed at install time. Each optional layer is
    // materialised as `Option<Layer>` — `tracing_subscriber::Layer for
    // Option<L>` makes a `None` layer a no-op at runtime.

    // Build the journald layer first so we can decide whether to also
    // emit on stderr. Under systemd the journald layer captures
    // everything natively; a stderr layer there would double every
    // entry (systemd also captures the unit's stderr into the journal).
    let journald_layer = build_journald_layer(journald_on, &defaults.identity);
    let journald_active = journald_layer.is_some();

    // stderr sink resolution. `Auto` is on only when journald did NOT
    // install — so journald-primary daemons don't get duplicated lines,
    // and a journald connect failure (layer = None) transparently falls
    // back to stderr. `Always`/`Never` are honoured verbatim.
    let stderr_on = match opts.log_stderr.unwrap_or(TriState::Auto) {
        TriState::Always => true,
        TriState::Never => false,
        TriState::Auto => !journald_active,
    };
    let stderr_layer = if stderr_on {
        Some(make_fmt_layer(
            format,
            ansi_on,
            std::io::stderr as fn() -> std::io::Stderr,
        ))
    } else {
        None
    };

    let file_layer = build_file_layer(format, file_dir, &defaults, &mut guards);

    // Stats event-counter layer (plan §3.3 — the cross-pillar bit).
    // Installed AFTER the registry-root `EnvFilter` so it counts
    // *admitted* events; events the filter rejects never reach any
    // layer. Gated on the resolved `stats_on` (computed above) and
    // on whether the recorder install actually succeeded — under
    // `--stats=off` or a lost recorder-install race the layer is
    // omitted entirely (plan §3.3 — "the layer is omitted entirely
    // … the level-filter short-circuit remains the only check").
    let stats_event_layer: Option<crate::stats::EventCounterLayer> = if stats_installed {
        Some(crate::stats::EventCounterLayer::new())
    } else {
        None
    };

    // `log`-crate records (from deps that emit via the `log` facade
    // rather than `tracing`) are bridged into tracing by `try_init()`
    // below: `tracing_subscriber`'s `tracing-log` feature is enabled,
    // so `SubscriberInitExt::try_init` installs a `tracing_log::LogTracer`
    // as the global `log` logger as part of init. We deliberately do
    // NOT call `LogTracer::init()` ourselves — doing so claims the
    // global `log` slot first and makes `try_init()` fail with a
    // `SetLoggerError`. (The dep is named explicitly in Cargo.toml to
    // keep the bridge an intentional, documented part of the surface.)

    // `try_init()` is the only step from here on that can fail. If it
    // does AFTER the recorder is installed (e.g. another global
    // dispatcher beat us), drive `shutdown_installed_recorder` so the
    // `.open` sink renames to `.done` rather than leaking past the
    // failed init.
    if let Err(e) = registry
        .with(stderr_layer)
        .with(file_layer)
        .with(journald_layer)
        .with(stats_event_layer)
        .try_init()
    {
        if stats_installed {
            crate::stats::shutdown_installed_recorder();
        }
        return Err(LogError::AlreadyInstalled(e.to_string()));
    }

    Ok(LogHandle {
        guards,
        reload: reload_handle,
        stats_installed,
        shutdown_done: false,
    })
}

/// Build and install the substrate `StatsRecorder` plus the
/// per-process JSONL sink. Called before `try_init()` so the
/// cross-pillar event-counter layer sees the real recorder from the
/// very first admitted event (plan §3543-3559).
///
/// Return shape:
/// - `Ok(true)` — recorder is the process global; `LogHandle::shutdown()`
///   will drive the final roll-up.
/// - `Ok(false)` — recorder install lost the race (another global
///   recorder is already installed); the event-counter layer becomes a
///   no-op on the facade. A stderr line is emitted. No JSONL sink is
///   opened on this path (see ordering note below) so there is no
///   orphan `.open` file to flush.
/// - `Err(InvalidStats)` — byte-budget fell outside the plan §2
///   [floor, ceiling] bounds. Refused at startup to mirror the
///   substrate `before_set` reject the same value would hit at L1
///   write-time. No sink is opened on this path either.
///
/// **Install-then-sink ordering.** `recorder.install()` runs *first*;
/// the JSONL sink is opened only after a successful install and
/// attached via `add_sink_to_installed`. The original layout (sink
/// added to the local recorder, then install) leaks an orphan
/// `.open` file on a lost install race because the local recorder is
/// dropped without a shutdown path. This ordering also means a sink
/// failure is **independent** of the recorder-install outcome — the
/// JSONL sink may be disabled while the recorder still serves Bus
/// and in-memory snapshot reads, and `Ok(true)` is returned in that
/// case.
fn install_stats_recorder(
    defaults: &LogDefaults,
    stats_opts: &StatsOpts,
) -> Result<bool, LogError> {
    // Cap precedence: CLI `--stats-byte-budget` over the binary's
    // `LogDefaults.stats_byte_budget`.
    let byte_budget_mib = stats_opts
        .stats_byte_budget
        .unwrap_or(defaults.stats_byte_budget);
    let budget_u64 = u64::from(byte_budget_mib);
    if !(BYTE_BUDGET_FLOOR_MIB..=BYTE_BUDGET_CEILING_MIB).contains(&budget_u64) {
        return Err(LogError::InvalidStats(format!(
            "byte_budget_mib={byte_budget_mib} outside [{BYTE_BUDGET_FLOOR_MIB}, {BYTE_BUDGET_CEILING_MIB}]",
        )));
    }

    // Interval precedence: CLI `--stats-interval` over the binary's
    // `LogDefaults.stats_interval`. `0` is the documented "on-exit
    // only" magic value (single end-of-process flush, see plan §4.3);
    // any positive value must fit within the ceiling that derives the
    // Prometheus gauge idle-timeout, otherwise a successful rollup at
    // a high-interval setting cannot refresh built-in gauges before
    // the exporter declares them idle. Mirrors the substrate
    // `before_set` reject so CLI/defaults and L1 props share one
    // chokepoint.
    let interval_seconds = stats_opts.stats_interval.unwrap_or(defaults.stats_interval);
    let interval_u64 = u64::from(interval_seconds);
    if interval_u64 > INTERVAL_SECONDS_CEILING {
        return Err(LogError::InvalidStats(format!(
            "stats_interval={interval_seconds} exceeds ceiling {INTERVAL_SECONDS_CEILING}",
        )));
    }

    let recorder = StatsRecorderBuilder::new(defaults.identity.clone()).build();

    // Install first — `install` consumes `recorder` and either makes
    // it the process global or fails (lost race). On failure, no
    // sink has been opened yet, so there is nothing to flush.
    if let Err(e) = recorder.install() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("cosmix-lib-log: stats recorder install failed: {e}\n").as_bytes(),
        );
        return Ok(false);
    }

    // JSONL sink path resolution. Per plan §2, `--stats-file <path>`
    // is the operator-overridable **basename**. We split the path
    // into `(parent_dir, sink_identity)`; the sink owns the rest of
    // the filename shape (`<sink_identity>.stats.<class>-<pid>-<ts>.jsonl.{open,done}`,
    // plan §4.1). The default `stats_file = <log_dir>/<identity>.stats.jsonl`
    // collapses correctly because we strip a trailing `.stats` from
    // the stem before passing it as the sink identity — otherwise
    // the default path would yield `<identity>.stats.stats.<class>-...jsonl`.
    let stats_file_opt: Option<std::path::PathBuf> = match stats_opts.stats_file.as_deref() {
        Some("") => None,
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => defaults.stats_file.clone(),
    };
    if let Some(stats_file) = stats_file_opt {
        let log_dir = stats_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let raw_stem = stats_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&defaults.identity);
        let sink_identity = raw_stem.strip_suffix(".stats").unwrap_or(raw_stem);
        // An empty stem (operator passed `/some/dir/`) is treated as
        // "no operator basename"; fall back to defaults.identity so
        // the sink filename still has the expected leading segment.
        let sink_identity = if sink_identity.is_empty() {
            defaults.identity.as_str()
        } else {
            sink_identity
        };
        match JsonlSink::daemon(&log_dir, sink_identity, byte_budget_mib) {
            Ok(sink) => {
                // `add_sink_to_installed` walks the global recorder
                // we just installed above. Returns `false` only when
                // no recorder is global — impossible here on the
                // Ok-arm of `recorder.install()` — but we log either
                // way so a future seam doesn't silently swallow a
                // sink registration.
                if !crate::stats::add_sink_to_installed(Arc::new(sink)) {
                    let _ = std::io::Write::write_all(
                        &mut std::io::stderr(),
                        b"cosmix-lib-log: stats JSONL sink not registered (recorder vanished?)\n",
                    );
                }
            }
            Err(e) => {
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("cosmix-lib-log: stats JSONL sink disabled: {e}\n").as_bytes(),
                );
            }
        }
    }

    Ok(true)
}

/// Build the effective `EnvFilter`.
///
/// `RUST_LOG` (if set) wins outright — preserves the one-shot debug
/// idiom. Otherwise the base directive is `default_filter` (if set,
/// preserves hand-tuned per-binary filters like cosmix-mcp's) or
/// synthesised from `default_target=<level>,cosmix_bus=info`.
/// `--log-filter` always layers on top via comma-append; EnvFilter
/// applies later directives last, so a `--log-filter
/// cosmix_maild::scoring=debug` over `cosmix_maild=info,cosmix_bus=info`
/// yields per-module debug without dropping the base.
fn build_env_filter(
    opts: &LogOpts,
    defaults: &LogDefaults,
    level: LogLevel,
) -> Result<EnvFilter, LogError> {
    if rust_log_nonempty() {
        return EnvFilter::try_from_default_env()
            .map_err(|e| LogError::InvalidFilter(e.to_string()));
    }

    // Base directive priority:
    //   1. Explicit CLI `--log-level` → synthesise from
    //      `default_target=<level>,cosmix_bus=info` (CLI wins over
    //      a binary's hand-tuned baseline; user requested a level).
    //   2. `LogDefaults.default_filter` → preserves hand-tuned
    //      per-binary baselines (cosmix-mcp's `info,cosmix_mcp=debug`)
    //      when the user did NOT pass `--log-level`.
    //   3. Synthesise from `default_target=<defaults.level>,cosmix_bus=info`
    //      → ordinary path for daemons/GUIs with no special baseline.
    let base = if opts.log_level.is_some() {
        synthesise_directive(&defaults.default_target, level)
    } else if let Some(custom) = &defaults.default_filter {
        custom.clone()
    } else {
        synthesise_directive(&defaults.default_target, level)
    };

    let directive = match &opts.log_filter {
        Some(extra) if !extra.is_empty() => format!("{base},{extra}"),
        _ => base,
    };

    EnvFilter::try_new(&directive).map_err(|e| LogError::InvalidFilter(e.to_string()))
}

fn synthesise_directive(default_target: &str, level: LogLevel) -> String {
    if default_target.is_empty() {
        level.as_directive().to_string()
    } else {
        format!(
            "{}={},cosmix_bus=info",
            default_target,
            level.as_directive()
        )
    }
}

fn rust_log_nonempty() -> bool {
    std::env::var_os("RUST_LOG")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

fn build_file_layer<S>(
    format: LogFormat,
    file_dir: Option<std::path::PathBuf>,
    defaults: &LogDefaults,
    guards: &mut Vec<tracing_appender::non_blocking::WorkerGuard>,
) -> Option<Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let dir = file_dir?;
    // Soft-fail: a mkdir-or-appender error logs to stderr and disables
    // the file layer. Matches cosmix-mcp's existing pattern; never
    // aborts startup over a logging sink.
    let dir_ok = std::fs::create_dir_all(&dir).map_err(|e| e.to_string());
    let appender_result = dir_ok.and_then(|()| {
        let rotation = match defaults.rotation {
            RotationMode::Daily => tracing_appender::rolling::Rotation::DAILY,
            RotationMode::Never => tracing_appender::rolling::Rotation::NEVER,
        };
        // Filename prefix per-mode matches the byte-for-byte targets:
        //   Daily  → `<identity>` (`rolling::daily(dir, identity)`
        //            produces `<identity>.YYYY-MM-DD`).
        //   Never  → `<identity>.log` (matches cosmix-mcp).
        let prefix = match defaults.rotation {
            RotationMode::Daily => defaults.identity.clone(),
            RotationMode::Never => format!("{}.log", defaults.identity),
        };
        tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(rotation)
            .filename_prefix(prefix)
            .build(&dir)
            .map_err(|e| e.to_string())
    });
    match appender_result {
        Ok(appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            guards.push(guard);
            Some(make_fmt_layer(format, false, non_blocking))
        }
        Err(e) => {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("cosmix-lib-log: file sink disabled: {e}\n").as_bytes(),
            );
            None
        }
    }
}

fn build_journald_layer<S>(
    on: bool,
    identity: &str,
) -> Option<Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if !on {
        return None;
    }
    match tracing_journald::layer() {
        Ok(layer) => Some(Box::new(layer.with_syslog_identifier(identity.to_string()))),
        Err(e) => {
            // Soft-fail per plan §5.5: a journald init error disables
            // the layer but never aborts startup. We can't log it
            // through tracing (no subscriber yet), so straight to
            // stderr.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("cosmix-lib-log: journald sink disabled: {e}\n").as_bytes(),
            );
            None
        }
    }
}

fn make_fmt_layer<S, W>(
    format: LogFormat,
    ansi: bool,
    writer: W,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_ansi(false),
        ),
        LogFormat::Human => Box::new(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(ansi),
        ),
    }
}
