// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/unicode/mod.rs; see cosmix-lsh/vendor/README.md.

//! Everything related to Unicode lives here.

mod sanitize;
mod utf8;

pub use sanitize::*;
pub use utf8::*;
