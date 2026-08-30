//! Unified logging surface for every Cosmix binary.
//!
//! Scope: CLI flags (`LogOpts`), per-binary defaults (`LogDefaults`),
//! sinks (stderr / rolling file / journald), filter parsing, JSON +
//! human formats, the `--log-level none` → `EnvFilter::off()` path, a
//! live-reload handle (`LogReloadHandle`), and the full stats recorder
//! / JSONL sink subsystem.
//!
//! This is the **pure-core half**, living in the bus repo. The
//! cos-coupled SPEC-12 property surface (`register_log_namespace`,
//! `LogHandle::attach_props`, the stats namespace registration) moved
//! out of this crate to a cos extension crate; the future watcher
//! drives live filter swaps through the public `LogReloadHandle`.
//!
//! Two optional features layer on the core:
//! - `bus-handlers` — the `<svc>.stats.snapshot` Bus verb handler
//!   (`stats::handle_snapshot_bus`); pulls `cosmix-lib-client` (in bus).
//! - `prometheus` — the per-daemon `/metrics` endpoint.
//!
//! # Hard rules
//!
//! - **stdout is reserved for protocol output**, never logs. All sinks
//!   write to stderr / files / journald.
//! - **`--log-level none` is first-class.** It installs
//!   `EnvFilter::off()` behind a reload handle so a later filter swap
//!   (or `RUST_LOG`) can raise it without restarting the binary.

mod defaults;
mod handle;
mod init;
mod opts;
#[cfg(feature = "prometheus")]
mod prometheus_attach;
pub mod stats;

pub use defaults::{LogDefaults, RotationMode};
pub use handle::{LogError, LogHandle, LogReloadHandle};
pub use init::{default_log_dir, init};
pub use opts::{LogFormat, LogLevel, LogOpts, OnOff, StatsOpts, TriState};
