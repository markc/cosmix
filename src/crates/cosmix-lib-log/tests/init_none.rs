//! Verify the `--log-level none` path. This test lives in its own file
//! because `init` installs a process-global dispatcher; we must not
//! collide with `init_install.rs`.
//!
//! Post-move behaviour: `--log-level none` no longer short-circuits
//! subscriber installation. It installs `EnvFilter::off()` behind the
//! reload layer so a later `LogReloadHandle::reload_filter` (or the
//! future cos props watcher) can raise the filter without restarting.
//! So a subscriber IS installed, the handle hands out a reload handle,
//! and a second `init` collides on the global dispatcher.

use cosmix_log::{LogDefaults, LogError, LogLevel, LogOpts, StatsOpts};

#[test]
fn init_with_level_none_installs_off_subscriber() {
    let opts = LogOpts {
        log_level: Some(LogLevel::None),
        log_stderr: Some(cosmix_log::TriState::Never),
        ..Default::default()
    };
    // Disable stats so the path doesn't try to install the recorder +
    // open a JSONL sink (daemon defaults set `stats: true`).
    let stats_opts = StatsOpts {
        stats: Some(cosmix_log::OnOff::Off),
        ..Default::default()
    };
    let defaults = LogDefaults::daemon("cosmix-test-none");

    let handle =
        cosmix_log::init(&opts, &stats_opts, defaults).expect("init returns Ok on level=none");

    // A subscriber WAS installed behind the reload layer — the handle
    // hands out a live-reload handle even at `off()`.
    assert!(
        handle.reload_handle().is_some(),
        "level=none installs EnvFilter::off() behind a reload layer"
    );

    // The global dispatcher is now set, so a second init must collide.
    let opts2 = LogOpts {
        log_level: Some(LogLevel::Warn),
        log_stderr: Some(cosmix_log::TriState::Never),
        ..Default::default()
    };
    let defaults2 = LogDefaults::gui("cosmix-test-after-none");
    let err = cosmix_log::init(&opts2, &stats_opts, defaults2)
        .expect_err("second init collides on the installed global dispatcher");
    assert!(
        matches!(err, LogError::AlreadyInstalled(_)),
        "expected AlreadyInstalled, got {err:?}"
    );

    drop(handle);
}
