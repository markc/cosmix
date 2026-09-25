// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/unicode/mod.rs; see vendor/msedit/README.md.
// Patched (ced E1a): re-exports the three grapheme-join lookups so `crate::view`
// can find cluster boundaries with exactly the tables the measurement code uses.

//! Everything related to Unicode lives here.

mod measurement;
mod tables;

pub use measurement::*;
pub(crate) use tables::{
    ucd_grapheme_cluster_joins, ucd_grapheme_cluster_joins_done, ucd_grapheme_cluster_lookup,
};
