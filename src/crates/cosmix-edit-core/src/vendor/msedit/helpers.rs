// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/helpers.rs; see vendor/msedit/README.md.
// Subset: the size constants and `CoordType` only (the rest of the upstream
// file serves the TUI and is not vendored).

pub const KIBI: usize = 1024;
pub const MEBI: usize = 1024 * 1024;
pub const GIBI: usize = 1024 * 1024 * 1024;

/// A viewport coordinate type used throughout the application.
pub type CoordType = isize;
