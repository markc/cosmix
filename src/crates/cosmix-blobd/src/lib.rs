//! `cosmix-blobd` — the node blob store.
//!
//! Bytes by hash (mds's CAS), names by owner (pin rows in a blobd-owned
//! `blobd.sqlite`), movement by side-channel (the byte lane). References
//! — `{"blob":"b3:<64 hex>","size":N,"mime":…, "origin":"<node>"}` —
//! cross the Bus; bytes never do.
//!
//! Core-first: `core` is pure store logic with no Bus dependency and is
//! testable with the `cosmix` feature off. `citizen` (verbs, events,
//! props), `lane` (the HTTP byte lane) and `main` exist only under the
//! `cosmix` feature, following the `cosmix-powerd` shape.

pub mod core;

#[cfg(feature = "cosmix")]
pub mod citizen;
#[cfg(feature = "cosmix")]
pub mod fetch;
#[cfg(feature = "cosmix")]
pub mod lane;
#[cfg(feature = "cosmix")]
pub mod props;
