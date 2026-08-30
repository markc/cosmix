//! Event-driven UPower battery and power state for Cosmix.
//!
//! Provenance: Quickshell's services were read for edge cases; no code was copied.

pub mod core;

#[cfg(feature = "cosmix")]
pub mod citizen;
#[cfg(feature = "cosmix")]
pub mod props;
#[cfg(feature = "cosmix")]
pub mod upower;
