//! Checkpointed random access over a [`Highlighter`], adapted from msedit
//! `crates/edit/src/lsh/cache.rs` (reference copy in `vendor/edit-lsh/cache.rs`).
//! Stage E1b implements the bodies.
//!
//! Contract (ced E1 plan §1.2(3), §4.3): a checkpoint every [`INTERVAL`] lines,
//! pinned in every build profile; `parse_line` of line L restores the nearest
//! checkpoint at or before L and parses forward, recording checkpoints it
//! passes; `invalidate_from(L)` drops every checkpoint that could depend on
//! line L or later. The caller time-slices cold seeks (ced plan §4.3) by
//! calling [`Cache::advance`] with a line budget.

use crate::highlighter::{Highlighter, Span};

/// Lines between runtime checkpoints (upstream: 1024 in release, 16 in debug).
pub const INTERVAL: usize = 1024;

#[derive(Default)]
pub struct Cache {
    _private: (),
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop checkpoints at or after 1-based `line`'s interval.
    pub fn invalidate_from(&mut self, line: usize) {
        let _ = line;
        todo!("ced E1b")
    }

    /// Highest 1-based line the cache can reach without parsing (its last
    /// checkpoint), for time-slicing decisions.
    pub fn reach(&self) -> usize {
        todo!("ced E1b")
    }

    /// Parse up to `max_lines` further lines towards 1-based `target`, recording
    /// checkpoints; returns true once `target` is reachable from a checkpoint.
    pub fn advance(&mut self, h: &mut Highlighter<'_>, target: usize, max_lines: usize) -> bool {
        let _ = (h, target, max_lines);
        todo!("ced E1b")
    }

    /// Spans of 1-based `line` into `out`.
    pub fn parse_line(&mut self, h: &mut Highlighter<'_>, line: usize, out: &mut Vec<Span>) {
        let _ = (h, line, out);
        todo!("ced E1b")
    }
}
