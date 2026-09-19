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
//! adapter registry and config resolution build without the `cosmix`
//! feature — no Bus, no zbus — and are unit-testable alone
//! (`cargo test -p cosmix-dbusd --no-default-features`); the Bus
//! citizen (`dbusd` control service, props, events) and the zbus
//! session-bus dial are behind the `cosmix` feature, which is part of
//! the default build.

// Fault containment contract: a panicking adapter run must UNWIND into
// its JoinHandle — with panic=abort every adapter panic would take the
// daemon down. Refuse any other strategy outright (not just abort:
// `not(panic = "unwind")` also catches whatever future strategies rustc
// grows). (Enforced here via cfg(panic), not in build.rs: CARGO_CFG_PANIC
// as seen by a build script is the build script's own strategy, which
// cargo always forces to unwind, so a build.rs check can never fire.)
#[cfg(not(panic = "unwind"))]
compile_error!(
    "cosmix-dbusd must be built with panic=unwind: its fault containment \
     relies on a panicking adapter run unwinding into its JoinHandle, not \
     aborting the process"
);

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
pub mod adapters;
#[cfg(feature = "cosmix")]
pub mod citizen;
#[cfg(feature = "cosmix")]
pub mod props;
