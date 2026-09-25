//! Line-by-line highlighting over a [`LineSource`], adapted from msedit
//! `crates/edit/src/lsh/highlighter.rs` (reference copy in
//! `vendor/edit-lsh/highlighter.rs`).
//!
//! Contract (ced E1 plan §1.2(2)): keep upstream's multi-chunk line assembly
//! and `MAX_LINE_LEN`; newline scanning is plain byte scanning (no dependency on
//! `cosmix-edit-core`); spans are owned (`Vec<Span>`), so callers never see the
//! vendored arena types.
//!
//! Adaptations from upstream, beyond the `LineSource` swap:
//! - A line of `MAX_LINE_LEN` bytes or more is scanned to its real end even
//!   when it spans several chunks. Upstream stopped assembling at the cap and
//!   left the read offset mid-line, so the tail was parsed as the next line and
//!   every later line number was off by one.
//! - The runtime's past-the-end sentinel span is dropped: a [`Span`] runs to
//!   the next span or the end of the line, so the sentinel carries nothing.

use lsh::runtime::{Language, Runtime, RuntimeState};
use stdext::arena::{Arena, scratch_arena};
use stdext::collections::BVec;

use crate::LineSource;
use crate::defs::{ASSEMBLY, CHARSETS, HighlightKind, STRINGS};

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
    /// Byte offset of the start of the next line.
    offset: usize,
    /// 0-based index of the next line.
    line0: usize,
    state: RuntimeState,
}

impl HighlighterState {
    /// 1-based number of the line this state resumes at.
    pub fn line(&self) -> usize {
        self.line0 + 1
    }

    pub(crate) fn line0(&self) -> usize {
        self.line0
    }
}

/// Highlights a source one line at a time from the top (or from a restored
/// [`HighlighterState`]).
pub struct Highlighter<'a> {
    src: &'a dyn LineSource,
    language: &'static Language,
    offset: usize,
    line0: usize,
    runtime: Runtime<'static, 'static, 'static>,
}

impl<'a> Highlighter<'a> {
    pub fn new(src: &'a dyn LineSource, language: &'static Language) -> Self {
        Self {
            src,
            language,
            offset: 0,
            line0: 0,
            runtime: Runtime::new(&ASSEMBLY, &STRINGS, &CHARSETS, language.entrypoint),
        }
    }

    /// The language this highlighter runs.
    pub fn language(&self) -> &'static Language {
        self.language
    }

    /// 1-based number of the NEXT line [`Self::parse_next_line`] will parse.
    pub fn line(&self) -> usize {
        self.line0 + 1
    }

    pub(crate) fn line0(&self) -> usize {
        self.line0
    }

    /// Back to line 1 with a fresh runtime.
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.src, self.language);
    }

    /// Capture the runtime state at the current line boundary.
    pub fn snapshot(&self) -> HighlighterState {
        HighlighterState { offset: self.offset, line0: self.line0, state: self.runtime.snapshot() }
    }

    /// Resume from a state captured on this same source (content up to that
    /// line unchanged).
    pub fn restore(&mut self, state: &HighlighterState) {
        self.offset = state.offset;
        self.line0 = state.line0;
        self.runtime.restore(&state.state);
    }

    /// Parse the next line, replacing `out` with its spans (empty for an empty
    /// line or one of `MAX_LINE_LEN` bytes or more). Advances by one line.
    pub fn parse_next_line(&mut self, out: &mut Vec<Span>) {
        out.clear();
        let scratch = scratch_arena(None);
        let (line_off, line) = self.read_next_line(&scratch);

        // As upstream: an empty read (past the end) and an over-long line are
        // not run, so the runtime state carries over them unchanged.
        if line.is_empty() || line.len() >= MAX_LINE_LEN {
            return;
        }

        let line = strip_newline(line);
        let res = self.runtime.parse_next_line::<HighlightKind>(&scratch, line);
        out.extend(
            res.iter()
                .filter(|h| h.start < line.len())
                .map(|h| Span { start: line_off + h.start, kind: h.kind }),
        );
    }

    /// Read the next line (newline included) into `arena` unless one chunk
    /// already holds all of it. Returns its absolute start offset. The bytes
    /// are capped at `MAX_LINE_LEN`, but the read offset always moves to the
    /// start of the following line.
    fn read_next_line<'s>(&mut self, arena: &'s Arena) -> (usize, &'s [u8])
    where
        'a: 's,
    {
        self.line0 += 1;

        let line_beg = self.offset;
        let mut chunk = self.src.read_forward(self.offset);
        if chunk.is_empty() {
            return (line_beg, chunk);
        }

        let (off, found) = line_end(chunk);
        self.offset += off;
        if found {
            return (line_beg, &chunk[..off]);
        }
        let next = self.src.read_forward(self.offset);
        if next.is_empty() {
            return (line_beg, &chunk[..off]);
        }

        let mut line_buf: BVec<'s, u8> = BVec::empty();
        let end = off.min(MAX_LINE_LEN);
        line_buf.extend_from_slice(arena, &chunk[..end]);
        chunk = next;

        loop {
            let (off, found) = line_end(chunk);
            self.offset += off;
            let room = MAX_LINE_LEN.saturating_sub(line_buf.len());
            if room > 0 {
                line_buf.extend_from_slice(arena, &chunk[..off.min(room)]);
            }
            if found {
                break;
            }
            chunk = self.src.read_forward(self.offset);
            if chunk.is_empty() {
                break;
            }
        }

        (line_beg, line_buf.leak())
    }
}

/// `(bytes up to and including the first '\n', whether one was found)`.
fn line_end(chunk: &[u8]) -> (usize, bool) {
    match chunk.iter().position(|&b| b == b'\n') {
        Some(i) => (i + 1, true),
        None => (chunk.len(), false),
    }
}

/// The line without its trailing `\n` or `\r\n`.
fn strip_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}
