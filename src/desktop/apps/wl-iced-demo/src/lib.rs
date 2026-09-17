//! Shared code for the test-B demo binaries: a text grid drawn by hand into
//! a `cosmix-wl-app` buffer, a popup menu, IME preedit and clipboard.

pub mod font;
pub mod paint;
pub mod raw;
pub mod startup;

/// True when `WL_DEMO_TRACE=1`.
pub fn trace_enabled() -> bool {
    std::env::var_os("WL_DEMO_TRACE").is_some_and(|v| v == "1")
}
