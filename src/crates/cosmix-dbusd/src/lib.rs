//! cosmix-dbusd — the D-Bus boundary daemon for Cosmix.
//!
//! D-Bus is a foreign protocol (ADR 2026-09-18): no cosmix app or daemon
//! other than this one speaks it. Adapters are per domain — one adapter
//! owns one Bus service, one zbus connection and one tokio task, so a
//! fault drops exactly that adapter's names and nothing else. The
//! supervisor restarts a failed adapter with exponential backoff and
//! never lets one adapter take the daemon down.
//!
//! Core-first, like `cosmix-powerd`: the supervisor, backoff schedule,
//! adapter registry and config resolution in this crate's default build
//! have no Bus and no zbus dependency and are unit-testable alone; the
//! Bus citizen (`dbusd` control service, props, events) and the zbus
//! session-bus dial are behind the `cosmix` feature.

pub mod adapter;
pub mod backoff;
pub mod config;
pub mod state;
pub mod supervisor;

/// Scripted misbehaviour for the supervision tests. Compiled only for
/// tests, so it can never ride along in the release binary.
#[cfg(test)]
pub(crate) mod fault;

#[cfg(feature = "cosmix")]
pub mod citizen;
#[cfg(feature = "cosmix")]
pub mod props;
