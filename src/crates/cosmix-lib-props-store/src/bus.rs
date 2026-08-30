//! Shim — SPEC 07 §2 read wire moved to `cosmix-lib-props-core`
//! in C2 of the lib-props split
//! (`_doc/planned/2026-05-29-lib-props-split.md`). This file
//! preserves the `cosmix_props::bus::*` import surface for
//! consumers (cosmix-noded 1503/1638/216, cosmix-mail:78,
//! cosmix-mix serve_runtime:321, cosmix-indexd main:1503) and
//! keeps `pub mod mutation;` so `cosmix_props::bus::mutation::PropsRouter`
//! still resolves to the SPEC 12 §5–§8 mutation router that
//! stays in lib-props-store.

pub mod mutation;

pub use cosmix_props_core::bus::{PropsResponse, build_response, dispatch_props};
