//! CLI surface (`LogOpts`) and the small enum vocabulary it parses.

use clap::{Args, ValueEnum};

/// The six log levels — five levels plus `none`.
///
/// `none` is a peer of the other levels, not a derived state. It
/// becomes `EnvFilter::off()` behind a reload handle so a later filter
/// swap can raise it without restarting the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum LogLevel {
    None,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// EnvFilter directive string (`error`, `warn`, ..., or `off`).
    pub fn as_directive(self) -> &'static str {
        match self {
            LogLevel::None => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum LogFormat {
    Human,
    Json,
}

/// Tri-state selector for `auto | always | never` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum TriState {
    Auto,
    Always,
    Never,
}

/// CLI flags every Cosmix binary inherits via `#[command(flatten)]`.
///
/// Every field is `Option<T>` — `None` means "no CLI override given,"
/// and the binary's `LogDefaults` supplies the fallback. This keeps
/// the precedence ladder (props > env > CLI > defaults) parseable
/// without sentinel values.
#[derive(Debug, Clone, Default, Args)]
pub struct LogOpts {
    /// Overall log level (`none | error | warn | info | debug | trace`).
    #[arg(long, value_enum, global = true)]
    pub log_level: Option<LogLevel>,

    /// EnvFilter directive string (e.g. `cosmix_maild=debug,cosmix_bus=warn`).
    /// Merged with `--log-level` at parse time per `_doc/planned/cosmix-lib-log.md` §2.
    #[arg(long, global = true)]
    pub log_filter: Option<String>,

    /// Output format (`human` or `json`).
    #[arg(long, value_enum, global = true)]
    pub log_format: Option<LogFormat>,

    /// stderr sink selector (`auto | always | never`).
    #[arg(long, value_enum, global = true)]
    pub log_stderr: Option<TriState>,

    /// Rolling-file sink directory (overrides `LogDefaults.log_file`).
    /// Filename prefix is `LogDefaults.identity`; rotation is per
    /// `LogDefaults.rotation`. Empty string disables the file sink.
    #[arg(long, global = true)]
    pub log_file: Option<String>,

    /// journald sink toggle (`off | on`).
    #[arg(long, global = true)]
    pub log_journald: Option<bool>,

    /// ANSI colour mode (`auto | always | never`).
    #[arg(long, value_enum, global = true)]
    pub log_color: Option<TriState>,
}

/// Stats subsystem CLI flags — orthogonal to `LogOpts`.
///
/// Binaries that want stats `#[command(flatten)]` this struct
/// alongside `LogOpts`. Binaries that don't simply don't flatten it.
/// All fields are `Option<T>`: `None` means "no CLI override"; the
/// binary's `LogDefaults` stats fields supply the fallback.
///
/// See `_doc/planned/cosmix-lib-log-stats.md` §2 for the frozen CLI
/// surface.
#[derive(Debug, Clone, Default, Args)]
pub struct StatsOpts {
    /// Master toggle (`off | on`). Off = no recorder installed, zero
    /// overhead, `metrics::counter!` macros short-circuit through the
    /// facade's no-op recorder.
    #[arg(long, value_enum, global = true)]
    pub stats: Option<OnOff>,

    /// Roll-up cadence in seconds. `0` = flush only on exit
    /// (the one-shot Mix mode); otherwise must be in `1..=3600`
    /// (the substrate-wide ceiling, mirrored in
    /// `init::INTERVAL_SECONDS_CEILING` and the canonical
    /// `stats::INTERVAL_SECONDS_CEILING`). Refused at startup
    /// otherwise — the Prometheus gauge idle-timeout is derived
    /// from the same ceiling, so a higher value would let built-in
    /// gauges falsely expire between rollups. Default per
    /// `LogDefaults.stats_interval`.
    #[arg(long, global = true)]
    pub stats_interval: Option<u32>,

    /// JSONL output basename stem (the recorder appends a producer-
    /// class discriminator + `.open` while writing and renames to
    /// `.done` on `LogHandle::shutdown()` — see plan §4.1 for the
    /// producer table). Empty string disables disk persistence
    /// (in-memory + Bus only). Defaults to
    /// `<LogDefaults.log_file>/<identity>.stats.jsonl`.
    #[arg(long, global = true)]
    pub stats_file: Option<String>,

    /// Whether `LogHandle::Drop` triggers a final flush (`off | on`).
    /// Default `on`. Set `off` only for binaries where graceful
    /// shutdown is rare (broker-class daemons that exit on SIGKILL).
    #[arg(long, value_enum, global = true)]
    pub stats_on_exit: Option<OnOff>,

    /// Per-process daily JSONL byte budget in MiB (floor 16, ceiling
    /// 1024). Soft limit fires an hourly `warn`; ceiling pauses the
    /// disk-append path until UTC midnight without affecting in-memory
    /// counters. See plan §3.3.1.
    #[arg(long, global = true)]
    pub stats_byte_budget: Option<u32>,

    /// Prometheus exposition listener bind (e.g. `192.0.2.5:9100`).
    /// Empty string explicitly disables the endpoint; `None` falls
    /// through to `LogDefaults.stats_prometheus_listen` (typically
    /// WG-bound on daemons, `None` on GUIs and one-shot CLIs).
    /// See plan §8.1.
    #[arg(long, global = true)]
    pub stats_prometheus_listen: Option<String>,
}

/// Binary `off | on` selector. Distinct from `bool` so the clap
/// `value_enum` surface stays uniform with the rest of the stats
/// flags (operators flip `--stats off` / `--stats on` rather than
/// `--stats=true` / `--stats=false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum OnOff {
    Off,
    On,
}

impl OnOff {
    /// Convert to `bool` — `On` → `true`, `Off` → `false`.
    pub fn as_bool(self) -> bool {
        matches!(self, OnOff::On)
    }
}

impl From<bool> for OnOff {
    fn from(b: bool) -> Self {
        if b { OnOff::On } else { OnOff::Off }
    }
}
