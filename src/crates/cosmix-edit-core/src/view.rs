//! Grapheme-correct measurement and navigation over a [`Text`] — the frontend
//! facade over vendored msedit `unicode` + `navigation` (ced E1 plan §1.3).
//! Frozen in Stage S; bodies land in Stage E1a.
//!
//! # Line and column convention (frozen)
//! Every public line number is **1-based**, like [`Text::line_start`] and the
//! `edit` wire. Visual columns (`cells`) are **0-based**.
//!
//! # The grapheme-safe adapter (frozen contract, plan §1.3(3))
//! msedit's measurement code requires that no chunk handed to it ends inside a
//! grapheme cluster (`vendor/msedit/document.rs:23`,
//! `vendor/msedit/unicode/measurement.rs`), which a gap buffer cut at an
//! arbitrary gap position does not guarantee — and E0 allows edits inside
//! clusters, so the gap can sit mid-cluster. Every function here therefore
//! reads through a private `GraphemeDoc<'a>` adapter over [`Text`] that:
//! - returns chunks that never end inside a cluster;
//! - at the gap, finds the straddling cluster by segmenting from a *restart
//!   point* (the start of the line containing the gap, or — when that is more
//!   than 1 KiB back — the nearest earlier position that is certainly a
//!   boundary: after `\n`, or between two scalars of
//!   `Grapheme_Cluster_Break=Control|LF|CR`), carrying the segmentation state
//!   (regional-indicator parity, ZWJ / Extended_Pictographic context, Extend
//!   runs) across the gap;
//! - returns that cluster as one stitched owned chunk **whatever its length**
//!   (allocation only when a cluster actually straddles the gap, O(cluster)).
//!   Clusters are never split, so semantics are exact. (The renderer, not this
//!   module, caps drawing of a >4 KiB cluster at its first 256 bytes.)
//!
//! Ambiguous-width characters (UAX #11) measure 1 or 2 cells per call via
//! [`MeasureCfg::ambiguous_wide`]; there is no process-global setting.

use std::ops::Range;

use crate::text::Text;

/// Measurement settings, per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasureCfg {
    /// Tab stop width in cells, `1..=16` (values outside are clamped).
    pub tab_size: u8,
    /// UAX #11 ambiguous-width characters measure 2 cells when true, else 1.
    pub ambiguous_wide: bool,
}

impl Default for MeasureCfg {
    fn default() -> Self {
        Self { tab_size: 4, ambiguous_wide: false }
    }
}

/// A position on the cell grid: 1-based `line`, 0-based `cells` from the
/// line start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VisualPos {
    pub line: usize,
    pub cells: usize,
}

/// How [`offset_at`] resolves a cell inside a wide cluster or tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// The cluster boundary at or before the cell.
    Left,
    /// The cluster boundary at or after the cell.
    Right,
    /// Whichever boundary is nearer (ties go left) — mouse hit-testing.
    Nearest,
}

/// One grapheme cluster as the renderer draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    /// Byte range of the cluster in the text.
    pub range: Range<usize>,
    /// Cells it occupies: 0 (e.g. a lone combining mark at line start is still
    /// one cluster; zero-width controls), 1 or 2; a tab reports its expanded
    /// width to the next stop.
    pub cells: u8,
    pub is_tab: bool,
    /// Every byte is ASCII (the renderer draws ASCII runs as one text call).
    pub ascii: bool,
}

/// The visual position of `offset` (clamped to the text; an offset inside a
/// cluster measures as that cluster's start).
pub fn visual_of(text: &Text, cfg: &MeasureCfg, offset: usize) -> VisualPos {
    let _ = (text, cfg, offset);
    todo!("ced E1a")
}

/// The byte offset of the cluster boundary at `cells` on 1-based `line`
/// (clamped to the line's end, `\n` excluded), resolved per `round`.
pub fn offset_at(text: &Text, cfg: &MeasureCfg, line: usize, cells: usize, round: Round) -> usize {
    let _ = (text, cfg, line, cells, round);
    todo!("ced E1a")
}

/// The next grapheme-cluster boundary after `offset` (`\r\n` is one cluster,
/// UAX #29 GB3); the text length at the end.
pub fn next_grapheme(text: &Text, offset: usize) -> usize {
    let _ = (text, offset);
    todo!("ced E1a")
}

/// The previous grapheme-cluster boundary before `offset`; 0 at the start.
pub fn prev_grapheme(text: &Text, offset: usize) -> usize {
    let _ = (text, offset);
    todo!("ced E1a")
}

/// The next word boundary (vendored msedit `word_forward`).
pub fn word_next(text: &Text, offset: usize) -> usize {
    let _ = (text, offset);
    todo!("ced E1a")
}

/// The previous word boundary (vendored msedit `word_backward`).
pub fn word_prev(text: &Text, offset: usize) -> usize {
    let _ = (text, offset);
    todo!("ced E1a")
}

/// The word (or whitespace / separator run) containing `offset` —
/// double-click selection.
pub fn word_at(text: &Text, offset: usize) -> Range<usize> {
    let _ = (text, offset);
    todo!("ced E1a")
}

/// The clusters of `range` (which must lie within one line, `\n` excluded),
/// the first starting at visual column `start_cells` (so tabs expand
/// correctly mid-line).
pub fn clusters<'a>(
    text: &'a Text,
    cfg: &MeasureCfg,
    range: Range<usize>,
    start_cells: usize,
) -> impl Iterator<Item = Cluster> + 'a {
    let _ = (text, cfg, range, start_cells);
    std::iter::from_fn(|| -> Option<Cluster> { todo!("ced E1a") })
}

/// `(offset, cells)` checkpoints every 4 KiB of 1-based `line` (the first is
/// the line start at 0 cells), so seeking into a very long line costs
/// O(4 KiB), not O(line).
pub fn line_checkpoints(text: &Text, cfg: &MeasureCfg, line: usize) -> Vec<(usize, usize)> {
    let _ = (text, cfg, line);
    todo!("ced E1a")
}
