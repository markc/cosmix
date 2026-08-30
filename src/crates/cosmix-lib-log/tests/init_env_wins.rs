//! Verify that `RUST_LOG=...` overrides a `--log-level none` short-
//! circuit — i.e. env-var precedence beats the level field, so a
//! binary whose defaults are `none` still honours one-shot
//! `RUST_LOG=debug` debugging. Own integration-test file (own
//! process) because we set RUST_LOG before `init` and because
//! `init` installs a process-global dispatcher.

use cosmix_log::{LogDefaults, LogLevel, LogOpts, StatsOpts, TriState};

#[test]
fn rust_log_env_overrides_level_none_shortcircuit() {
    // SAFETY: this test owns its process (separate integration-test
    // binary), so mutating env vars is safe.
    unsafe {
        std::env::set_var("RUST_LOG", "warn");
    }

    let opts = LogOpts {
        log_level: Some(LogLevel::None),
        log_stderr: Some(TriState::Never),
        ..Default::default()
    };
    let stats_opts = StatsOpts::default();
    let defaults = LogDefaults::gui("cosmix-test-env-wins");

    let h = cosmix_log::init(&opts, &stats_opts, defaults)
        .expect("init must install a subscriber even when level=none, because RUST_LOG is set");

    // A second init must now fail with AlreadyInstalled — the
    // discriminator proving we actually installed a global
    // dispatcher rather than short-circuiting.
    let opts2 = LogOpts {
        log_level: Some(LogLevel::Warn),
        log_stderr: Some(TriState::Never),
        ..Default::default()
    };
    let err = cosmix_log::init(&opts2, &stats_opts, LogDefaults::default())
        .expect_err("second init must fail — proves the first installed the dispatcher");
    assert!(
        matches!(err, cosmix_log::LogError::AlreadyInstalled(_)),
        "expected AlreadyInstalled, got {err:?}"
    );

    drop(h);
    unsafe {
        std::env::remove_var("RUST_LOG");
    }
}
