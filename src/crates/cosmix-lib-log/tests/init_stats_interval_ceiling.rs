//! Codex round-6 MAJOR regression: `init::install_stats_recorder`
//! must reject `--stats-interval` / `LogDefaults::stats_interval`
//! values above `INTERVAL_SECONDS_CEILING` (= 3600). Lives in its
//! own integration-test process because the global metrics recorder
//! is install-once.
//!
//! The two rejecting tests (`*_above_ceiling_is_rejected`) bail out
//! before `recorder.install()` runs, so they leave the global slot
//! free for siblings. The permissive test
//! (`zero_and_ceiling_values_pass_the_bound_check`) drives `stats=on`
//! and reaches the install — its first iteration takes the global
//! slot, and subsequent iterations within the same process tolerate
//! `AlreadyInstalled` (which only fires *after* the bound check;
//! seeing it still proves the interval value was admitted).

use cosmix_log::{LogDefaults, LogLevel, LogOpts, OnOff, StatsOpts, TriState};

fn quiet_opts() -> LogOpts {
    LogOpts {
        log_level: Some(LogLevel::Warn),
        log_stderr: Some(TriState::Never),
        log_color: Some(TriState::Never),
        ..Default::default()
    }
}

#[test]
fn cli_stats_interval_above_ceiling_is_rejected() {
    let opts = quiet_opts();
    let stats_opts = StatsOpts {
        stats: Some(OnOff::On),
        stats_interval: Some(3601),
        stats_file: Some(String::new()),
        ..Default::default()
    };
    let defaults = LogDefaults::gui("cosmix-test-interval-cli");
    let err = cosmix_log::init(&opts, &stats_opts, defaults)
        .expect_err("3601s interval must be refused before recorder install");
    match err {
        cosmix_log::LogError::InvalidStats(msg) => {
            assert!(
                msg.contains("stats_interval=3601") && msg.contains("ceiling 3600"),
                "expected interval-ceiling message, got: {msg}"
            );
        }
        other => panic!("expected InvalidStats, got {other:?}"),
    }
}

#[test]
fn defaults_stats_interval_above_ceiling_is_rejected() {
    let opts = quiet_opts();
    let stats_opts = StatsOpts {
        stats: Some(OnOff::On),
        stats_file: Some(String::new()),
        ..Default::default()
    };
    let defaults = LogDefaults::gui("cosmix-test-interval-default")
        .with_stats(true)
        .with_stats_interval(7200);
    let err = cosmix_log::init(&opts, &stats_opts, defaults)
        .expect_err("LogDefaults.stats_interval above ceiling must be refused");
    match err {
        cosmix_log::LogError::InvalidStats(msg) => {
            assert!(
                msg.contains("stats_interval=7200") && msg.contains("ceiling 3600"),
                "expected interval-ceiling message, got: {msg}"
            );
        }
        other => panic!("expected InvalidStats, got {other:?}"),
    }
}

#[test]
fn zero_and_ceiling_values_pass_the_bound_check() {
    // `0` is the documented "on-exit only" magic value; the ceiling
    // itself must also be admitted (closed interval). Drive `stats=on`
    // and `stats_file=""` so the call actually reaches the bound check
    // in `install_stats_recorder` (Codex round-7 MINOR: the prior
    // `stats=off` version short-circuited past the chokepoint). Each
    // arm tolerates `AlreadyInstalled` because the first successful
    // call here takes the global metrics-recorder + tracing-dispatcher
    // slots — that error only fires *after* the bound check, so its
    // presence still proves the interval value was admitted.
    let opts = quiet_opts();
    for value in [0u32, 1, 60, 3600] {
        let stats_opts = StatsOpts {
            stats: Some(OnOff::On),
            stats_interval: Some(value),
            stats_file: Some(String::new()),
            ..Default::default()
        };
        let defaults = LogDefaults::gui("cosmix-test-interval-permissive");
        match cosmix_log::init(&opts, &stats_opts, defaults) {
            Ok(_handle) => {}
            Err(cosmix_log::LogError::AlreadyInstalled(_)) => {}
            Err(other) => panic!("unexpected rejection for interval={value}: {other:?}"),
        }
    }
}
