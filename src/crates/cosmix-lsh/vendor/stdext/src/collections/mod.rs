// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/collections/mod.rs; see cosmix-lsh/vendor/README.md.

mod string;
mod vec;

pub use string::{BString, BStringFormatter};
pub use vec::BVec;
