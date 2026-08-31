//! Event-driven MPRIS2 media-player state and controls for Cosmix.
//!
//! Provenance: Quickshell's MPRIS service was read for edge cases; nothing was
//! copied.

pub mod core;

#[cfg(feature = "cosmix")]
pub mod citizen;
#[cfg(feature = "cosmix")]
pub mod mpris;
#[cfg(feature = "cosmix")]
pub mod props;
