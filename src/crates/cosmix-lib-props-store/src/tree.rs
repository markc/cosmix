//! Shim — `tree` moved to `cosmix-lib-props-core` in C1 of the
//! lib-props split (`_doc/planned/2026-05-29-lib-props-split.md`).
//! Re-exports preserve the `cosmix_props::tree::*` module-path
//! consumers use at 20+ call sites across the workspace.
pub use cosmix_props_core::tree::*;
