// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/helpers.rs; see vendor/msedit/README.md.
// Subset: the size constants, `CoordType` and (ced E1, for unicode measurement)
// `Point` with its ordering; the rest of the upstream file serves the TUI and
// is not vendored.

use std::cmp::Ordering;

pub const KIBI: usize = 1024;
pub const MEBI: usize = 1024 * 1024;
pub const GIBI: usize = 1024 * 1024 * 1024;

/// A viewport coordinate type used throughout the application.
pub type CoordType = isize;

/// A 2D point. Uses [`CoordType`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Point {
    pub x: CoordType,
    pub y: CoordType,
}

impl Point {
    pub const MIN: Self = Self { x: CoordType::MIN, y: CoordType::MIN };
    pub const MAX: Self = Self { x: CoordType::MAX, y: CoordType::MAX };
}

impl PartialOrd<Self> for Point {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Point {
    fn cmp(&self, other: &Self) -> Ordering {
        self.y.cmp(&other.y).then(self.x.cmp(&other.x))
    }
}
