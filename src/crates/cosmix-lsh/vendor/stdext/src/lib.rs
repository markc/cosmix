// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/lib.rs; see cosmix-lsh/vendor/README.md.

//! Arena allocators. Small and fast.

#![cfg_attr(
    target_arch = "loongarch64",
    feature(stdarch_loongarch),
    allow(clippy::incompatible_msrv)
)]

pub mod alloc;
pub mod arena;
pub mod collections;
pub mod float;
pub mod glob;
mod helpers;
mod maybe_owned;
pub mod simd;
pub mod sys;
pub mod unicode;

pub use helpers::*;
pub use maybe_owned::*;
