//! Checkpointed random access over a [`Highlighter`], adapted from msedit
//! `crates/edit/src/lsh/cache.rs` (reference copy in `vendor/edit-lsh/cache.rs`).
//!
//! Contract (ced E1 plan §1.2(3), §4.3): a checkpoint every [`INTERVAL`] lines,
//! pinned in every build profile; `parse_line` of line L restores the nearest
//! checkpoint at or before L and parses forward, recording checkpoints it
//! passes; `invalidate_from(L)` drops every checkpoint that could depend on
//! line L or later. The caller time-slices cold seeks (ced plan §4.3) by
//! calling [`Cache::advance`] with a line budget.
//!
//! Beyond upstream, the cache keeps a **frontier**: the state at the furthest
//! line it has parsed to. A time-sliced seek resumes there, so a budget smaller
//! than [`INTERVAL`] still makes progress from one frame to the next, and
//! consecutive `parse_line` calls on fresh highlighters (one per frame, since a
//! highlighter borrows the text) resume where the last one stopped.
//!
//! Invariants: checkpoint `k` is the state at the start of 0-based line
//! `k * INTERVAL`, with no gaps; the frontier never lies past the next
//! checkpoint to be recorded (`checkpoints.len() * INTERVAL`), so parsing on
//! from it always records that checkpoint. The highlighter handed in may be
//! fresh or anywhere: its state was derived from the source it borrows, so it
//! is a valid starting point whenever it is at or before the requested line.

use crate::highlighter::{Highlighter, HighlighterState, Span};

/// Lines between runtime checkpoints (upstream: 1024 in release, 16 in debug).
pub const INTERVAL: usize = 1024;

#[derive(Default)]
pub struct Cache {
    checkpoints: Vec<HighlighterState>,
    frontier: Option<HighlighterState>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop checkpoints at or after 1-based `line`'s interval.
    ///
    /// Precisely: every cached state that starts after `line` goes, because it
    /// was derived by parsing `line`. A state that starts AT `line` depends only
    /// on the lines before it and stays.
    pub fn invalidate_from(&mut self, line: usize) {
        let l0 = line.saturating_sub(1);
        self.checkpoints.truncate(l0 / INTERVAL + 1);
        if self.frontier.as_ref().is_some_and(|f| f.line0() > l0) {
            self.frontier = None;
        }
    }

    /// Highest 1-based line the cache can reach without parsing (its last
    /// checkpoint, or the frontier when that is further), for time-slicing
    /// decisions.
    pub fn reach(&self) -> usize {
        let ckpt = self.checkpoints.last().map_or(0, HighlighterState::line0);
        let front = self.frontier.as_ref().map_or(0, HighlighterState::line0);
        ckpt.max(front) + 1
    }

    /// Parse up to `max_lines` further lines towards 1-based `target`, recording
    /// checkpoints; returns true once `target` is reachable from a checkpoint.
    pub fn advance(&mut self, h: &mut Highlighter<'_>, target: usize, max_lines: usize) -> bool {
        let goal = target.saturating_sub(1) / INTERVAL;
        if self.checkpoints.len() <= goal {
            self.seek(h, goal * INTERVAL, max_lines);
        }
        self.checkpoints.len() > goal
    }

    /// Spans of 1-based `line` into `out`.
    pub fn parse_line(&mut self, h: &mut Highlighter<'_>, line: usize, out: &mut Vec<Span>) {
        self.seek(h, line.saturating_sub(1), usize::MAX);
        h.parse_next_line(out);
        self.record(h);
        self.push_frontier(h);
    }

    /// Position `h` at the start of 0-based `l0` from the best known state at
    /// or before it, parsing at most `budget` lines on the way. Records every
    /// checkpoint passed and moves the frontier to where `h` stops.
    fn seek(&mut self, h: &mut Highlighter<'_>, l0: usize, budget: usize) {
        let here = Some(h.line0()).filter(|&at| at <= l0);
        let front = self.frontier.as_ref().map(HighlighterState::line0).filter(|&f| f <= l0);
        let ckpt = self.checkpoints.len().checked_sub(1).map(|last| (l0 / INTERVAL).min(last));
        let ckpt_line = ckpt.map(|k| self.checkpoints[k].line0());

        // The furthest valid start wins; `h` itself on a tie (no restore).
        if here < front.max(ckpt_line) || here.is_none() {
            if front.is_some() && front >= ckpt_line {
                let state = self.frontier.clone().expect("front is Some");
                h.restore(&state);
            } else if let Some(k) = ckpt {
                let state = self.checkpoints[k].clone();
                h.restore(&state);
            } else {
                h.reset();
            }
        }

        self.record(h);
        let mut discard = Vec::new();
        let mut left = budget;
        while h.line0() < l0 && left > 0 {
            h.parse_next_line(&mut discard);
            left -= 1;
            self.record(h);
        }
        self.push_frontier(h);
    }

    /// Record `h`'s position if it is the next checkpoint due.
    fn record(&mut self, h: &Highlighter<'_>) {
        if h.line0() == self.checkpoints.len() * INTERVAL {
            self.checkpoints.push(h.snapshot());
        }
    }

    /// Move the frontier to `h` if that is further and keeps the invariant.
    fn push_frontier(&mut self, h: &Highlighter<'_>) {
        let at = h.line0();
        let further = self.frontier.as_ref().is_none_or(|f| f.line0() < at);
        if further && at <= self.checkpoints.len() * INTERVAL {
            self.frontier = Some(h.snapshot());
        }
    }
}
