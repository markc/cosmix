//! Core state and host contracts for the Cosmix Quoin desktop shell.
//!
//! The default graph is deliberately renderer-, Bevy-, CTK-, ABP-, and
//! BUS-free. `chrome` adds the Bevy/CTK presentation adapter; `dev-host` adds
//! the normal-window tuning harness. Neither optional layer introduces a bus.

#![forbid(unsafe_code)]

pub mod core;
pub mod host;
pub mod runtime;

#[cfg(feature = "chrome-core")]
pub mod chrome;
#[cfg(feature = "dev-host")]
pub mod dev_host;
