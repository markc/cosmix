//! Per-binary defaults — the data the `main` function knows that
//! `LogOpts` flags do not (binary identity, Bus service, default
//! sinks for the binary's class).
//!
//! The three names (`identity`, `default_target`, `bus_service`) are
//! deliberately distinct — see `_doc/planned/cosmix-lib-log.md` §6
//! for why conflating them was the catch from Codex round 3.

use crate::opts::{LogFormat, LogLevel};

/// File-rotation mode for the rolling-file sink.
///
/// `Daily` matches `cosmix-lib-daemon::init_tracing` byte-for-byte
/// (`<dir>/<identity>.YYYY-MM-DD.log`); `Never` matches
/// `cosmix-mcp`'s existing pattern (`<dir>/<identity>.log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationMode {
    #[default]
    Daily,
    Never,
}

/// Per-binary defaults the `main` function passes to `init`.
///
/// Default impl gives GUI-class defaults (warn/human, no file,
/// journald off, no Bus service) — suitable for one-shot CLIs and
/// the `mix` REPL out of the box.
#[derive(Debug, Clone)]
pub struct LogDefaults {
    /// File-log prefix and journald `syslog_identifier`. Matches argv[0]
    /// (e.g. `"cosmix-maild"`, `"cosmix-mcp"`).
    pub identity: String,

    /// Default EnvFilter target (the compiled crate name, with hyphens
    /// replaced by underscores — e.g. `"cosmix_maild"`).
    pub default_target: String,

    /// The `<svc>` in `<svc>.props.*` for binaries that opt into the
    /// L1 property surface. `None` is permanent for `mix` REPL and
    /// one-shot CLIs; daemons set this to their Bus service name.
    pub bus_service: Option<String>,

    pub level: LogLevel,
    pub format: LogFormat,
    /// `Some(dir)` enables the rolling-file sink under that directory;
    /// `None` disables it.
    pub log_file: Option<std::path::PathBuf>,
    pub rotation: RotationMode,
    pub journald: bool,
    /// Optional baseline EnvFilter directive. `None` means the
    /// directive is synthesised from `default_target` + `level`
    /// (`"{default_target}={level},cosmix_bus=info"`). `Some(s)` lets a
    /// binary preserve a hand-tuned directive across the migration —
    /// e.g. cosmix-mcp's existing `"info,cosmix_mcp=debug"`. The
    /// CLI's `--log-filter` always layers on top via comma append.
    pub default_filter: Option<String>,

    /// Whether the stats recorder is installed by default. Daemons
    /// set `true`; GUI and one-shot CLIs set `false`. See plan §5.
    pub stats: bool,

    /// Roll-up cadence in seconds. `0` = flush only on exit (the
    /// one-shot Mix mode). 60 for daemons; 0 for `mix -c`, `mix <file>`.
    pub stats_interval: u32,

    /// JSONL output basename stem. `None` disables disk persistence
    /// (in-memory + Bus only). The recorder appends a producer-class
    /// discriminator + `.open` while writing and renames to `.done`
    /// on shutdown — see plan §4.1 for the producer table. Defaults to
    /// `<log_file>/<identity>.stats.jsonl` when `log_file` is set.
    pub stats_file: Option<std::path::PathBuf>,

    /// Per-process daily JSONL byte budget in MiB. Floor 16, ceiling
    /// 1024. Default 256 for daemons, 16 for Mix. See plan §3.3.1.
    pub stats_byte_budget: u32,

    /// Prometheus exposition listener bind. `None` disables the
    /// endpoint. Default: WG-bound `<wg-ip>:9100` for daemons that
    /// opt in via per-binary `with_stats_prometheus_listen`. The base
    /// constructors leave this `None` so binaries make the bind
    /// address explicit (no surprise public binds). See plan §8.1
    /// and `feedback_wg_only_binding`.
    pub stats_prometheus_listen: Option<std::net::SocketAddr>,
}

impl Default for LogDefaults {
    fn default() -> Self {
        Self {
            identity: String::new(),
            default_target: String::new(),
            bus_service: None,
            level: LogLevel::Warn,
            format: LogFormat::Human,
            log_file: None,
            rotation: RotationMode::Daily,
            journald: false,
            default_filter: None,
            stats: false,
            // One-shot / mix REPL semantics: flush only on exit. Daemons
            // and long-running GUI binaries override via the dedicated
            // constructors (60s).
            stats_interval: 0,
            stats_file: None,
            stats_byte_budget: 16,
            stats_prometheus_listen: None,
        }
    }
}

impl LogDefaults {
    /// Daemon-class defaults: info/json, **journald-primary** (no
    /// rolling file), stats recorder on.
    ///
    /// The standing directive (2026-06-04) is that every daemon logs
    /// to journald and leans on systemd, rather than maintaining its
    /// own rolling log file under `~/.local/log/cosmix/`. So `journald`
    /// is `true` and `log_file` is `None` out of the box; an operator
    /// who still wants a file sink opts in via `with_log_file(...)`.
    /// With no `log_file` directory to derive a stats path from,
    /// `stats_file` defaults to `None` (in-memory + Bus snapshot only);
    /// a daemon that wants JSONL persistence sets it explicitly via
    /// `with_stats_file(...)`.
    pub fn daemon(identity: &str) -> Self {
        Self {
            identity: identity.to_string(),
            default_target: identity.replace('-', "_"),
            bus_service: None,
            level: LogLevel::Info,
            format: LogFormat::Json,
            log_file: None,
            rotation: RotationMode::Daily,
            journald: true,
            default_filter: None,
            stats: true,
            stats_interval: 60,
            stats_file: None,
            stats_byte_budget: 256,
            stats_prometheus_listen: None,
        }
    }

    /// Mix `--serve` runtime defaults: like `daemon()` but with the
    /// stats recorder **off** and human-format logging to journald.
    ///
    /// The serve runtime is a long-lived Bus handler loop that wants
    /// journald + `RUST_LOG`-style operator control, but no per-process
    /// stats recorder or JSONL sink (it is not a metrics producer in
    /// its own right). `stats_interval` is `0` (on-exit only — moot
    /// with `stats: false`), `log_file` is `None`, `journald` is `true`,
    /// level `info`, format `human`.
    pub fn serve(identity: &str) -> Self {
        Self {
            identity: identity.to_string(),
            default_target: identity.replace('-', "_"),
            bus_service: None,
            level: LogLevel::Info,
            format: LogFormat::Human,
            log_file: None,
            rotation: RotationMode::Daily,
            journald: true,
            default_filter: None,
            stats: false,
            stats_interval: 0,
            stats_file: None,
            stats_byte_budget: 16,
            stats_prometheus_listen: None,
        }
    }

    /// GUI-class defaults: warn/human, no file, journald off.
    ///
    /// Operators who want persistent GUI logs set `--log-file`
    /// explicitly — symmetric with the daemon path, no special-case.
    pub fn gui(identity: &str) -> Self {
        Self {
            identity: identity.to_string(),
            default_target: identity.replace('-', "_"),
            bus_service: None,
            level: LogLevel::Warn,
            format: LogFormat::Human,
            log_file: None,
            rotation: RotationMode::Daily,
            journald: false,
            default_filter: None,
            stats: false,
            stats_interval: 60,
            stats_file: None,
            stats_byte_budget: 16,
            stats_prometheus_listen: None,
        }
    }

    /// Set the Bus service name (the `<svc>` in `<svc>.props.*`).
    pub fn with_bus_service(mut self, svc: &str) -> Self {
        self.bus_service = Some(svc.to_string());
        self
    }

    /// Set the rolling-file sink directory.
    ///
    /// If `stats_file` is still at its constructor default — both
    /// parent directory equal to the previous `log_file` AND filename
    /// equal to `<identity>.stats.jsonl` — it is retargeted to the
    /// new directory, preserving the documented
    /// `<log_file>/<identity>.stats.jsonl` convention. An explicit
    /// prior `with_stats_file(...)` is honoured even when its file
    /// happens to live under the old log directory.
    pub fn with_log_file(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        let new_dir: std::path::PathBuf = dir.into();
        let default_name = format!("{}.stats.jsonl", self.identity);
        if let (Some(old_dir), Some(stats)) = (self.log_file.as_ref(), self.stats_file.as_ref())
            && stats.parent() == Some(old_dir.as_path())
            && stats.file_name().and_then(|s| s.to_str()) == Some(default_name.as_str())
        {
            self.stats_file = Some(new_dir.join(&default_name));
        }
        self.log_file = Some(new_dir);
        self
    }

    /// Set the file rotation mode.
    pub fn with_rotation(mut self, rotation: RotationMode) -> Self {
        self.rotation = rotation;
        self
    }

    /// Enable or disable the journald sink.
    pub fn with_journald(mut self, on: bool) -> Self {
        self.journald = on;
        self
    }

    /// Override the default level.
    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    /// Override the default format.
    pub fn with_format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Set a hand-tuned baseline EnvFilter directive (e.g.
    /// `"info,cosmix_mcp=debug"` for cosmix-mcp). Replaces the
    /// `default_target=level` synthesis; `--log-filter` still layers
    /// on top.
    pub fn with_filter(mut self, directive: &str) -> Self {
        self.default_filter = Some(directive.to_string());
        self
    }

    /// Enable or disable the stats recorder by default.
    pub fn with_stats(mut self, on: bool) -> Self {
        self.stats = on;
        self
    }

    /// Override the stats roll-up cadence in seconds. `0` = on-exit
    /// only; otherwise must be in `1..=3600` (ceiling enforced at
    /// startup in `init::install_stats_recorder` and mirrored on the
    /// substrate `stats.interval_seconds` write path). A value above
    /// the ceiling makes `init()` return `LogError::InvalidStats`.
    pub fn with_stats_interval(mut self, seconds: u32) -> Self {
        self.stats_interval = seconds;
        self
    }

    /// Override the stats JSONL output directory. Setting `""` is not
    /// the disable mechanism — pass `None` via direct field assignment
    /// or set `--stats-file=` on the CLI. This setter requires a real
    /// path.
    pub fn with_stats_file(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.stats_file = Some(dir.into());
        self
    }

    /// Override the per-process daily JSONL byte budget in MiB.
    /// Floor 16, ceiling 1024 (enforced at startup, not here).
    pub fn with_stats_byte_budget(mut self, mib: u32) -> Self {
        self.stats_byte_budget = mib;
        self
    }

    /// Set the Prometheus exposition listener bind. The substrate
    /// makes no assumption about which interface — daemons that opt
    /// in pass their WG IP explicitly. See `feedback_wg_only_binding`.
    pub fn with_stats_prometheus_listen(mut self, addr: std::net::SocketAddr) -> Self {
        self.stats_prometheus_listen = Some(addr);
        self
    }
}
