//! Shim — SPEC 07 §3/§4 wire builders (`world.<svc>` + per-leaf
//! `<svc>.props.changed`) moved to `cosmix-lib-props-core` in C2
//! of the lib-props split
//! (`_doc/planned/2026-05-29-lib-props-split.md`). This file
//! preserves the `cosmix_props::publish::*` import surface for
//! consumers (cosmix-noded:312, cosmix-noded:375 — currently the
//! only two direct callers).
pub use cosmix_props_core::publish::*;
