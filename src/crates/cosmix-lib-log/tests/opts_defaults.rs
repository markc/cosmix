//! Unit tests for the pure-data surface (no global dispatcher
//! touched). The `init()` path is exercised via a separate
//! integration test that owns a fresh process — see `init_smoke.rs`.

use clap::Parser;
use cosmix_log::{
    LogDefaults, LogFormat, LogLevel, LogOpts, OnOff, RotationMode, StatsOpts, TriState,
};

#[derive(Parser, Debug)]
struct Cmd {
    #[command(flatten)]
    log: LogOpts,
}

#[derive(Parser, Debug)]
struct StatsCmd {
    #[command(flatten)]
    stats: StatsOpts,
}

#[test]
fn log_opts_all_flags_parse() {
    let cmd = Cmd::try_parse_from([
        "test",
        "--log-level",
        "debug",
        "--log-filter",
        "cosmix_maild=debug,cosmix_bus=warn",
        "--log-format",
        "json",
        "--log-stderr",
        "never",
        "--log-file",
        "/tmp/x",
        "--log-journald",
        "true",
        "--log-color",
        "always",
    ])
    .unwrap();

    assert_eq!(cmd.log.log_level, Some(LogLevel::Debug));
    assert_eq!(
        cmd.log.log_filter.as_deref(),
        Some("cosmix_maild=debug,cosmix_bus=warn")
    );
    assert_eq!(cmd.log.log_format, Some(LogFormat::Json));
    assert_eq!(cmd.log.log_stderr, Some(TriState::Never));
    assert_eq!(cmd.log.log_file.as_deref(), Some("/tmp/x"));
    assert_eq!(cmd.log.log_journald, Some(true));
    assert_eq!(cmd.log.log_color, Some(TriState::Always));
}

#[test]
fn log_opts_no_flags_yields_all_none() {
    let cmd = Cmd::try_parse_from(["test"]).unwrap();
    assert_eq!(cmd.log.log_level, None);
    assert_eq!(cmd.log.log_filter, None);
    assert_eq!(cmd.log.log_format, None);
    assert_eq!(cmd.log.log_stderr, None);
    assert_eq!(cmd.log.log_file, None);
    assert_eq!(cmd.log.log_journald, None);
    assert_eq!(cmd.log.log_color, None);
}

#[test]
fn log_level_none_is_first_class_clap_value() {
    let cmd = Cmd::try_parse_from(["test", "--log-level", "none"]).unwrap();
    assert_eq!(cmd.log.log_level, Some(LogLevel::None));
}

#[test]
fn log_level_directives_match_envfilter_syntax() {
    assert_eq!(LogLevel::None.as_directive(), "off");
    assert_eq!(LogLevel::Error.as_directive(), "error");
    assert_eq!(LogLevel::Warn.as_directive(), "warn");
    assert_eq!(LogLevel::Info.as_directive(), "info");
    assert_eq!(LogLevel::Debug.as_directive(), "debug");
    assert_eq!(LogLevel::Trace.as_directive(), "trace");
}

#[test]
fn log_level_trace_is_first_class_clap_value() {
    let cmd = Cmd::try_parse_from(["test", "--log-level", "trace"]).unwrap();
    assert_eq!(cmd.log.log_level, Some(LogLevel::Trace));
}

#[test]
fn defaults_daemon_shape() {
    let d = LogDefaults::daemon("cosmix-maild");
    assert_eq!(d.identity, "cosmix-maild");
    assert_eq!(d.default_target, "cosmix_maild"); // hyphens → underscores
    assert_eq!(d.bus_service, None);
    assert_eq!(d.level, LogLevel::Info);
    assert_eq!(d.format, LogFormat::Json);
    // Journald-primary: no rolling file by default, journald on. An
    // operator opts into a file sink via with_log_file(...).
    assert!(d.log_file.is_none());
    assert_eq!(d.rotation, RotationMode::Daily);
    assert!(d.journald);
    // Stats surface: daemons opt in with a 256 MiB/day budget; the
    // Prometheus listener stays None so binaries make the bind explicit.
    assert!(d.stats);
    assert_eq!(d.stats_interval, 60);
    // No log_file directory to derive a JSONL path from, so stats_file
    // defaults to None (in-memory + Bus snapshot only). A daemon that
    // wants disk persistence sets it explicitly.
    assert!(d.stats_file.is_none());
    assert_eq!(d.stats_byte_budget, 256);
    assert!(d.stats_prometheus_listen.is_none());
}

#[test]
fn defaults_serve_shape() {
    let d = LogDefaults::serve("cosmix-mix");
    assert_eq!(d.identity, "cosmix-mix");
    assert_eq!(d.default_target, "cosmix_mix");
    assert_eq!(d.level, LogLevel::Info);
    assert_eq!(d.format, LogFormat::Human);
    assert!(d.log_file.is_none());
    assert!(d.journald);
    // Serve runtime is not a metrics producer: stats off, on-exit-only
    // interval (moot with stats off), no JSONL.
    assert!(!d.stats);
    assert_eq!(d.stats_interval, 0);
    assert!(d.stats_file.is_none());
    assert!(d.stats_prometheus_listen.is_none());
}

#[test]
fn defaults_gui_shape() {
    let d = LogDefaults::gui("cosmix-disp-skia");
    assert_eq!(d.identity, "cosmix-disp-skia");
    assert_eq!(d.default_target, "cosmix_disp_skia");
    assert_eq!(d.bus_service, None);
    assert_eq!(d.level, LogLevel::Warn);
    assert_eq!(d.format, LogFormat::Human);
    assert!(d.log_file.is_none());
    assert!(!d.journald);
    // GUI/one-shot class: stats off, floor budget, no disk persistence.
    assert!(!d.stats);
    assert_eq!(d.stats_byte_budget, 16);
    assert!(d.stats_file.is_none());
    assert!(d.stats_prometheus_listen.is_none());
}

#[test]
fn defaults_setters_chain() {
    let d = LogDefaults::daemon("cosmix-mcp")
        .with_bus_service("mcp")
        .with_rotation(RotationMode::Never)
        .with_journald(true);
    assert_eq!(d.bus_service.as_deref(), Some("mcp"));
    assert_eq!(d.rotation, RotationMode::Never);
    assert!(d.journald);
    // Setters preserve other daemon-class defaults.
    assert_eq!(d.level, LogLevel::Info);
    assert_eq!(d.format, LogFormat::Json);
}

#[test]
fn defaults_default_is_gui_class() {
    let d = LogDefaults::default();
    assert_eq!(d.level, LogLevel::Warn);
    assert_eq!(d.format, LogFormat::Human);
    assert!(d.log_file.is_none());
    assert!(!d.journald);
    assert_eq!(d.bus_service, None);
    assert!(!d.stats);
    // Default models one-shot / mix REPL: flush only on exit.
    assert_eq!(d.stats_interval, 0);
    assert_eq!(d.stats_byte_budget, 16);
    assert!(d.stats_file.is_none());
    assert!(d.stats_prometheus_listen.is_none());
}

#[test]
fn rotation_mode_default_is_daily() {
    assert_eq!(RotationMode::default(), RotationMode::Daily);
}

#[test]
fn with_filter_sets_default_filter() {
    let d = LogDefaults::daemon("cosmix-mcp").with_filter("info,cosmix_mcp=debug");
    assert_eq!(d.default_filter.as_deref(), Some("info,cosmix_mcp=debug"));
}

#[test]
fn stats_opts_all_flags_parse() {
    let cmd = StatsCmd::try_parse_from([
        "test",
        "--stats",
        "on",
        "--stats-interval",
        "30",
        "--stats-file",
        "/tmp/x.stats.jsonl",
        "--stats-on-exit",
        "off",
        "--stats-byte-budget",
        "128",
        "--stats-prometheus-listen",
        "192.0.2.5:9100",
    ])
    .unwrap();

    assert_eq!(cmd.stats.stats, Some(OnOff::On));
    assert_eq!(cmd.stats.stats_interval, Some(30));
    assert_eq!(cmd.stats.stats_file.as_deref(), Some("/tmp/x.stats.jsonl"));
    assert_eq!(cmd.stats.stats_on_exit, Some(OnOff::Off));
    assert_eq!(cmd.stats.stats_byte_budget, Some(128));
    assert_eq!(
        cmd.stats.stats_prometheus_listen.as_deref(),
        Some("192.0.2.5:9100")
    );
}

#[test]
fn stats_opts_no_flags_yields_all_none() {
    let cmd = StatsCmd::try_parse_from(["test"]).unwrap();
    assert_eq!(cmd.stats.stats, None);
    assert_eq!(cmd.stats.stats_interval, None);
    assert_eq!(cmd.stats.stats_file, None);
    assert_eq!(cmd.stats.stats_on_exit, None);
    assert_eq!(cmd.stats.stats_byte_budget, None);
    assert_eq!(cmd.stats.stats_prometheus_listen, None);
}

#[test]
fn on_off_round_trips_bool() {
    assert!(OnOff::On.as_bool());
    assert!(!OnOff::Off.as_bool());
    assert_eq!(OnOff::from(true), OnOff::On);
    assert_eq!(OnOff::from(false), OnOff::Off);
}

#[test]
fn with_log_file_retargets_default_stats_file() {
    // When log_file is set AND stats_file holds the default
    // <log_file>/<identity>.stats.jsonl basename, a subsequent
    // with_log_file must carry the default stats_file along — otherwise
    // the documented convention silently breaks. (daemon() no longer
    // pre-sets a file sink, so the precondition is built explicitly.)
    let d = LogDefaults::daemon("cosmix-maild")
        .with_log_file("/old/log")
        .with_stats_file("/old/log/cosmix-maild.stats.jsonl")
        .with_log_file("/var/log/cosmix");
    let stats = d.stats_file.as_ref().expect("stats_file is retargeted");
    assert_eq!(
        stats,
        std::path::Path::new("/var/log/cosmix/cosmix-maild.stats.jsonl")
    );
    assert_eq!(
        d.log_file.as_deref(),
        Some(std::path::Path::new("/var/log/cosmix"))
    );
}

#[test]
fn with_log_file_preserves_explicit_stats_file() {
    // Explicit `with_stats_file` overrides (non-default filename) must
    // NOT be clobbered by a subsequent `with_log_file` call, regardless
    // of order.
    let d1 = LogDefaults::daemon("cosmix-maild")
        .with_stats_file("/srv/metrics/maild.jsonl")
        .with_log_file("/var/log/cosmix");
    assert_eq!(
        d1.stats_file.as_deref(),
        Some(std::path::Path::new("/srv/metrics/maild.jsonl"))
    );

    let d2 = LogDefaults::daemon("cosmix-maild")
        .with_log_file("/var/log/cosmix")
        .with_stats_file("/srv/metrics/maild.jsonl");
    assert_eq!(
        d2.stats_file.as_deref(),
        Some(std::path::Path::new("/srv/metrics/maild.jsonl"))
    );
}

#[test]
fn with_log_file_preserves_same_dir_explicit_override() {
    // Edge case: caller explicitly sets stats_file to a non-default
    // filename UNDER the current log directory. A subsequent
    // with_log_file move must NOT silently retarget it, because the
    // filename ≠ <identity>.stats.jsonl signals an explicit override.
    let old_dir = std::path::PathBuf::from("/old/log");
    let explicit = old_dir.join("custom-metrics.jsonl");
    let d = LogDefaults::daemon("cosmix-maild")
        .with_log_file(&old_dir)
        .with_stats_file(&explicit)
        .with_log_file("/var/log/cosmix");
    assert_eq!(d.stats_file.as_deref(), Some(explicit.as_path()));
}

#[test]
fn defaults_stats_setters_chain() {
    let listen: std::net::SocketAddr = "192.0.2.5:9100".parse().unwrap();
    let d = LogDefaults::gui("cosmix-mix")
        .with_stats(true)
        .with_stats_interval(0)
        .with_stats_file("/tmp/stats")
        .with_stats_byte_budget(64)
        .with_stats_prometheus_listen(listen);
    assert!(d.stats);
    assert_eq!(d.stats_interval, 0);
    assert_eq!(
        d.stats_file.as_deref(),
        Some(std::path::Path::new("/tmp/stats"))
    );
    assert_eq!(d.stats_byte_budget, 64);
    assert_eq!(d.stats_prometheus_listen, Some(listen));
}
