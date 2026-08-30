//! Verify a real subscriber install + double-init detection. Lives
//! in its own integration-test file (own process) because the global
//! dispatcher is install-once.

use cosmix_log::{LogDefaults, LogLevel, LogOpts, StatsOpts, TriState};

#[test]
fn init_installs_then_double_init_errors() {
    // Stderr-only, no file, no journald — minimal sink set so the
    // test doesn't write artefacts to ~/.local/log/cosmix.
    let opts = LogOpts {
        log_level: Some(LogLevel::Warn),
        log_stderr: Some(TriState::Always),
        log_color: Some(TriState::Never),
        ..Default::default()
    };
    let stats_opts = StatsOpts::default();
    let defaults = LogDefaults::gui("cosmix-test-install");
    let h =
        cosmix_log::init(&opts, &stats_opts, defaults).expect("first init installs the dispatcher");

    // tracing macros work without panicking.
    tracing::warn!("smoke");

    // Second init must fail with AlreadyInstalled.
    let opts2 = LogOpts {
        log_level: Some(LogLevel::Info),
        log_stderr: Some(TriState::Never),
        ..Default::default()
    };
    let err = cosmix_log::init(&opts2, &stats_opts, LogDefaults::default())
        .expect_err("second init returns AlreadyInstalled");
    assert!(
        matches!(err, cosmix_log::LogError::AlreadyInstalled(_)),
        "expected AlreadyInstalled, got {err:?}"
    );

    drop(h);
}
