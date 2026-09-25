// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/sys/mod.rs; see cosmix-lsh/vendor/README.md.

//! Platform abstractions.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use std::fs::canonicalize;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;
