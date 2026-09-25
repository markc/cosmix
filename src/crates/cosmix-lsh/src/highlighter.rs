//! Line-by-line highlighting over a [`LineSource`], adapted from msedit
//! `crates/edit/src/lsh/highlighter.rs` (reference copy in
//! `vendor/edit-lsh/highlighter.rs`). Stage E1b implements the bodies.
//!
//! Contract (ced E1 plan §1.2(2)): keep upstream's multi-chunk line assembly
//! and `MAX_LINE_LEN`; newline scanning is plain byte scanning (no dependency on
//! `cosmix-edit-core`); spans are owned (`Vec<Span>`), so callers never see the
//! vendored arena types.

use lsh::runtime::Language;

use crate::LineSource;
use crate::defs::HighlightKind;

/// Lines at least this long are returned with no spans (upstream behaviour).
pub const MAX_LINE_LEN: usize = 32 * 1024;

/// `kind` applies from `start` (absolute byte offset in the source) up to the
/// next span's `start`, or to the end of the line (newline excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub kind: HighlightKind,
}

/// A restorable runtime position (see [`Highlighter::snapshot`]).
#[derive(Clone)]
pub struct HighlighterState {
    _private: (),
}

/// Highlights a source one line at a time from the top (or from a restored
/// [`HighlighterState`]).
pub struct Highlighter<'a> {
    _src: &'a dyn LineSource,
    _language: &'static Language,
}

impl<'a> Highlighter<'a> {
    pub fn new(src: &'a dyn LineSource, language: &'static Language) -> Self {
        let _ = (src, language);
        todo!("ced E1b")
    }

    /// 1-based number of the NEXT line [`Self::parse_next_line`] will parse.
    pub fn line(&self) -> usize {
        todo!("ced E1b")
    }

    /// Capture the runtime state at the current line boundary.
    pub fn snapshot(&self) -> HighlighterState {
        todo!("ced E1b")
    }

    /// Resume from a state captured on this same source (content up to that
    /// line unchanged).
    pub fn restore(&mut self, state: &HighlighterState) {
        let _ = state;
        todo!("ced E1b")
    }

    /// Parse the next line, replacing `out` with its spans (empty for an empty
    /// line or one of `MAX_LINE_LEN` bytes or more). Advances by one line.
    pub fn parse_next_line(&mut self, out: &mut Vec<Span>) {
        let _ = out;
        todo!("ced E1b")
    }
}
