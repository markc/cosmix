//! Grapheme-correct measurement and navigation over a [`Text`] — the frontend
//! facade over vendored msedit `unicode` + `navigation` (ced E1 plan §1.3).
//! Frozen in Stage S; bodies landed in Stage E1a.
//!
//! # Line and column convention (frozen)
//! Every public line number is **1-based**, like [`Text::line_start`] and the
//! `edit` wire. Visual columns (`cells`) are **0-based**.
//!
//! A line's *content* excludes its line ending: the `\n`, and the `\r` of a
//! `\r\n` (which is one cluster, UAX #29 GB3). A lone `\r` not followed by
//! `\n` is an ordinary control character and stays content.
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
//!   point*: the nearest earlier position that is certainly a boundary. A
//!   position is certainly a boundary when the scalar pair around it breaks
//!   in **every** state of the segmentation machine (msedit's join tables,
//!   the same ones the measurement uses). That covers after `\n` and around
//!   `Grapheme_Cluster_Break=Control|CR|LF` (GB4/GB5), and in ordinary text
//!   almost every position; the scan never passes the start of the line.
//!   The segmentation state (regional-indicator parity, ZWJ /
//!   Extended_Pictographic context, Extend runs) is carried across the gap;
//! - returns that cluster as one stitched owned chunk **whatever its length**
//!   (allocation only when a cluster actually straddles the gap, O(cluster)).
//!   Clusters are never split, so semantics are exact. (The renderer, not this
//!   module, caps drawing of a >4 KiB cluster at its first 256 bytes.)
//!
//! Chunks are also cut at a certain boundary roughly every 2 KiB, so a walk
//! reads (and the tests can count) only what it needs.
//!
//! # Cost
//! Measuring is a walk from the line start: [`visual_of`] and [`offset_at`]
//! are O(line prefix). A cell count beyond the widest the line could be
//! (End) answers the content end without reading the line. For long lines,
//! cache [`line_checkpoints`] and seek with [`visual_of_with`] /
//! [`offset_at_with`] (or [`clusters`] from a checkpoint): O(4 KiB).
//!
//! Ambiguous-width characters (UAX #11) measure 1 or 2 cells per call via
//! [`MeasureCfg::ambiguous_wide`]; there is no process-global setting.

use std::cell::OnceCell;
use std::ops::Range;

use crate::text::Text;
use crate::vendor::msedit::document::ReadableDocument;
use crate::vendor::msedit::helpers::{CoordType, Point};
use crate::vendor::msedit::navigation;
use crate::vendor::msedit::unicode::{
    ucd_grapheme_cluster_joins, ucd_grapheme_cluster_joins_done, ucd_grapheme_cluster_lookup,
    Cursor, MeasurementConfig,
};

#[cfg(test)]
mod tests;

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
    visual_of_with(text, cfg, offset, &[])
}

/// [`visual_of`], starting from the last of `checkpoints` (as returned by
/// [`line_checkpoints`] for the offset's line, and still current) at or
/// before the offset. Checkpoints of another line are ignored.
pub fn visual_of_with(
    text: &Text,
    cfg: &MeasureCfg,
    offset: usize,
    checkpoints: &[(usize, usize)],
) -> VisualPos {
    let doc = GraphemeDoc::new(text);
    let o = doc.floor(offset);
    let line = line_of(text, o);
    let content = doc.content(text, line);
    let o = o.min(content.end);
    let i = checkpoints.partition_point(|&(co, _)| co <= o);
    let (start, cells) = match i.checked_sub(1).map(|i| checkpoints[i]) {
        Some((co, cc)) if co >= content.start => (co, cc),
        _ => (content.start, 0),
    };
    let at = measurer(&doc, cfg, cursor_at(start, cells)).goto_offset(o);
    VisualPos { line, cells: at.column as usize }
}

/// The byte offset of the cluster boundary at `cells` on 1-based `line`
/// (clamped to the line's end, `\n` excluded), resolved per `round`.
pub fn offset_at(text: &Text, cfg: &MeasureCfg, line: usize, cells: usize, round: Round) -> usize {
    offset_at_with(text, cfg, line, cells, round, &[])
}

/// [`offset_at`], starting from the last of `checkpoints` (as returned by
/// [`line_checkpoints`] for `line`, and still current) at or before `cells`.
/// Checkpoints of another line are ignored.
pub fn offset_at_with(
    text: &Text,
    cfg: &MeasureCfg,
    line: usize,
    cells: usize,
    round: Round,
    checkpoints: &[(usize, usize)],
) -> usize {
    let doc = GraphemeDoc::new(text);
    let line = line.clamp(1, text.line_count().max(1));
    let content = doc.content(text, line);
    // No cluster is wider than a tab stop or two cells, and each is at least
    // one byte: past this the target is off the end of the line.
    let widest = usize::from(tab_size(cfg)).max(2);
    if cells >= (content.end - content.start).saturating_mul(widest) {
        return content.end;
    }
    let i = checkpoints.partition_point(|&(_, cc)| cc <= cells);
    let (start, start_cells) = match i.checked_sub(1).map(|i| checkpoints[i]) {
        Some((co, cc)) if content.contains(&co) => (co, cc),
        _ => (content.start, 0),
    };
    let mut m = measurer(&doc, cfg, cursor_at(start, start_cells));
    let left = m.goto_visual(Point { x: cells as CoordType, y: 0 });
    let left_cells = left.column as usize;
    if round == Round::Left || left_cells == cells || left.offset >= content.end {
        return left.offset;
    }
    // `left` is the start of the cluster that covers `cells`.
    let right = m.goto_offset(left.offset + 1);
    let right_cells = right.column as usize;
    if round == Round::Right || right_cells - cells < cells - left_cells {
        right.offset
    } else {
        left.offset
    }
}

/// The next grapheme-cluster boundary after `offset` (`\r\n` is one cluster,
/// UAX #29 GB3); the text length at the end.
pub fn next_grapheme(text: &Text, offset: usize) -> usize {
    let doc = GraphemeDoc::new(text);
    if offset >= doc.len() {
        return doc.len();
    }
    doc.segment_around(offset).1
}

/// The previous grapheme-cluster boundary before `offset`; 0 at the start.
pub fn prev_grapheme(text: &Text, offset: usize) -> usize {
    let doc = GraphemeDoc::new(text);
    match offset.min(doc.len()) {
        0 => 0,
        o => doc.floor(o - 1),
    }
}

/// The next word boundary (vendored msedit `word_forward`).
pub fn word_next(text: &Text, offset: usize) -> usize {
    let doc = GraphemeDoc::new(text);
    let o = doc.floor(offset);
    doc.ceil(navigation::word_forward(&doc, o))
}

/// The previous word boundary (vendored msedit `word_backward`).
pub fn word_prev(text: &Text, offset: usize) -> usize {
    let doc = GraphemeDoc::new(text);
    let o = doc.floor(offset);
    doc.floor(navigation::word_backward(&doc, o))
}

/// The word (or whitespace / separator run) containing `offset` —
/// double-click selection.
pub fn word_at(text: &Text, offset: usize) -> Range<usize> {
    let doc = GraphemeDoc::new(text);
    let o = doc.floor(offset);
    let r = navigation::word_select(&doc, o);
    doc.floor(r.start)..doc.ceil(r.end)
}

/// The clusters of `range` (which must lie within one line, `\n` excluded),
/// the first starting at visual column `start_cells` (so tabs expand
/// correctly mid-line).
///
/// `range.start` must be a cluster boundary (a line start or a checkpoint).
/// A cluster that starts before `range.end` is yielded whole; the walk stops
/// at a line ending.
pub fn clusters<'a>(
    text: &'a Text,
    cfg: &MeasureCfg,
    range: Range<usize>,
    start_cells: usize,
) -> impl Iterator<Item = Cluster> + 'a {
    let doc = GraphemeDoc::new(text);
    let cfg = *cfg;
    let end = range.end.min(doc.len());
    let mut at = cursor_at(range.start.min(end), start_cells);
    std::iter::from_fn(move || {
        if at.offset >= end {
            return None;
        }
        let next = measurer(&doc, &cfg, at).goto_offset(at.offset + 1);
        if next.offset <= at.offset || next.logical_pos.y != at.logical_pos.y {
            // A line ending: the caller's range ran past the content.
            at.offset = end;
            return None;
        }
        let range = at.offset..next.offset;
        let cluster = Cluster {
            cells: (next.column - at.column) as u8,
            is_tab: range.len() == 1 && doc.byte(range.start) == b'\t',
            ascii: range.clone().all(|i| doc.byte(i).is_ascii()),
            range,
        };
        at = next;
        Some(cluster)
    })
}

/// `(offset, cells)` checkpoints every 4 KiB of 1-based `line` (the first is
/// the line start at 0 cells), so seeking into a very long line costs
/// O(4 KiB), not O(line).
///
/// Checkpoint k (k ≥ 1) is the first cluster boundary at or after
/// `line start + k·4 KiB`; one inside the line ending is not reported.
pub fn line_checkpoints(text: &Text, cfg: &MeasureCfg, line: usize) -> Vec<(usize, usize)> {
    let doc = GraphemeDoc::new(text);
    let line = line.clamp(1, text.line_count().max(1));
    let content = doc.content(text, line);
    let mut out = vec![(content.start, 0)];
    let mut m = measurer(&doc, cfg, cursor_at(content.start, 0));
    let mut target = content.start + CHECKPOINT_EVERY;
    while target < content.end {
        let at = m.goto_offset(target);
        if at.offset >= content.end {
            break;
        }
        if out.last().is_none_or(|&(o, _)| at.offset > o) {
            out.push((at.offset, at.column as usize));
        }
        target += CHECKPOINT_EVERY;
    }
    out
}

const CHECKPOINT_EVERY: usize = 4 * 1024;

/// Soft chunk size: chunks end at the first certain boundary at or after it.
const CHUNK: usize = 2 * 1024;

fn tab_size(cfg: &MeasureCfg) -> u8 {
    cfg.tab_size.clamp(1, 16)
}

fn measurer<'d>(doc: &'d GraphemeDoc<'_>, cfg: &MeasureCfg, at: Cursor) -> MeasurementConfig<'d> {
    MeasurementConfig::new(doc)
        .with_tab_size(CoordType::from(tab_size(cfg)))
        .with_ambiguous_width(if cfg.ambiguous_wide { 2 } else { 1 })
        .with_cursor(at)
}

/// A cursor at a cluster boundary `cells` into its line. Its line is `y = 0`,
/// so crossing a line ending shows as `logical_pos.y == 1`.
fn cursor_at(offset: usize, cells: usize) -> Cursor {
    let x = cells as CoordType;
    Cursor {
        offset,
        logical_pos: Point { x: 0, y: 0 },
        visual_pos: Point { x, y: 0 },
        column: x,
        wrap_opp: false,
    }
}

/// The 1-based line containing `offset` (an offset at a `\n` belongs to the
/// line it ends).
fn line_of(text: &Text, offset: usize) -> usize {
    let (mut lo, mut hi) = (1, text.line_count().max(1));
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if text.line_start(mid).is_some_and(|s| s <= offset) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[cfg(test)]
thread_local! {
    /// Bytes handed out or decoded by `GraphemeDoc` on this thread (tests
    /// bound how much of a long line a seek reads).
    static TOUCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn touch(n: usize) {
    #[cfg(test)]
    TOUCHED.with(|t| t.set(t.get() + n));
    #[cfg(not(test))]
    let _ = n;
}

/// The cluster that straddles the gap, stitched; `start == end` (and no
/// bytes) when the gap sits on a boundary.
struct Straddle {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

/// The grapheme-safe [`ReadableDocument`] over a [`Text`] (module docs).
struct GraphemeDoc<'a> {
    /// Text before the gap, `[0, gap)`.
    before: &'a [u8],
    /// Text after the gap, `[gap, len)`.
    after: &'a [u8],
    straddle: OnceCell<Straddle>,
}

impl<'a> GraphemeDoc<'a> {
    fn new(text: &'a Text) -> Self {
        let before = text.chunk_at(0);
        let after: &[u8] = if before.len() < text.len() { text.chunk_at(before.len()) } else { &[] };
        debug_assert_eq!(before.len() + after.len(), text.len());
        Self { before, after, straddle: OnceCell::new() }
    }

    fn gap(&self) -> usize {
        self.before.len()
    }

    fn len(&self) -> usize {
        self.before.len() + self.after.len()
    }

    fn byte(&self, i: usize) -> u8 {
        match i.checked_sub(self.gap()) {
            None => self.before[i],
            Some(j) => self.after[j],
        }
    }

    /// `[r)` within one side of the gap.
    fn slice(&self, r: Range<usize>) -> &[u8] {
        let g = self.gap();
        if r.end <= g {
            &self.before[r]
        } else {
            debug_assert!(r.start >= g);
            &self.after[r.start - g..r.end - g]
        }
    }

    /// Content of 1-based `line` (in range): its line ending excluded.
    fn content(&self, text: &Text, line: usize) -> Range<usize> {
        let r = text.line_range(line).unwrap_or(0..0);
        let crlf = r.end < self.len() && r.end > r.start && self.byte(r.end - 1) == b'\r';
        r.start..if crlf { r.end - 1 } else { r.end }
    }

    fn is_char_boundary(&self, p: usize) -> bool {
        p == 0 || p >= self.len() || !is_continuation(self.byte(p))
    }

    /// The scalar starting at char boundary `p < len`, and its byte length.
    fn char_at(&self, p: usize) -> (char, usize) {
        let want = utf8_len(self.byte(p)).min(self.len() - p);
        let mut buf = [0u8; 4];
        for (k, b) in buf.iter_mut().enumerate().take(want) {
            *b = self.byte(p + k);
        }
        touch(want);
        match std::str::from_utf8(&buf[..want]).ok().and_then(|s| s.chars().next()) {
            Some(c) => (c, want),
            // Unreachable for a `Text` (valid UTF-8); decode like `Utf8Chars` would.
            None => (char::REPLACEMENT_CHARACTER, 1),
        }
    }

    /// The char boundary before `p > 0`.
    fn prev_char_boundary(&self, p: usize) -> usize {
        let mut q = p - 1;
        while q > 0 && p - q < 4 && is_continuation(self.byte(q)) {
            q -= 1;
        }
        q
    }

    /// Whether `p` is a cluster boundary whatever precedes the scalar before
    /// it: the pair around `p` breaks in every segmentation state.
    fn certain(&self, p: usize) -> bool {
        if p == 0 || p >= self.len() {
            return true;
        }
        if !self.is_char_boundary(p) {
            return false;
        }
        let (lead, _) = self.char_at(self.prev_char_boundary(p));
        let (trail, _) = self.char_at(p);
        let (l, t) = (ucd_grapheme_cluster_lookup(lead), ucd_grapheme_cluster_lookup(trail));
        ucd_grapheme_cluster_joins_done(ucd_grapheme_cluster_joins(0, l, t))
            && ucd_grapheme_cluster_joins_done(ucd_grapheme_cluster_joins(1, l, t))
    }

    /// The nearest certain boundary at or before `p`.
    fn restart(&self, p: usize) -> usize {
        let mut q = p.min(self.len());
        while !self.certain(q) {
            q = self.prev_char_boundary(q);
        }
        q
    }

    /// `(last boundary ≤ o, first boundary > o)` for `o < len`, by segmenting
    /// forward from a restart point with msedit's join tables.
    fn segment_around(&self, o: usize) -> (usize, usize) {
        let len = self.len();
        debug_assert!(o < len);
        let mut floor = self.restart(o);
        let (mut lead, n) = self.char_at(floor);
        let mut pos = floor + n;
        let mut state = 0;
        while pos < len {
            let (trail, n) = self.char_at(pos);
            let s = ucd_grapheme_cluster_joins(
                state,
                ucd_grapheme_cluster_lookup(lead),
                ucd_grapheme_cluster_lookup(trail),
            );
            if ucd_grapheme_cluster_joins_done(s) {
                if pos > o {
                    return (floor, pos);
                }
                floor = pos;
                state = 0;
            } else {
                state = s;
            }
            lead = trail;
            pos += n;
        }
        (floor, len)
    }

    /// The start of the cluster containing `o` (`o` itself on a boundary).
    fn floor(&self, o: usize) -> usize {
        let o = o.min(self.len());
        if self.certain(o) { o } else { self.segment_around(o).0 }
    }

    /// The end of the cluster containing `o` (`o` itself on a boundary).
    fn ceil(&self, o: usize) -> usize {
        let o = o.min(self.len());
        if self.certain(o) {
            return o;
        }
        match self.segment_around(o) {
            (f, _) if f == o => o,
            (_, next) => next,
        }
    }

    fn straddle(&self) -> &Straddle {
        self.straddle.get_or_init(|| {
            let g = self.gap();
            if self.certain(g) {
                return Straddle { start: g, end: g, bytes: Vec::new() };
            }
            let (start, end) = self.segment_around(g);
            if start == g {
                return Straddle { start: g, end: g, bytes: Vec::new() };
            }
            let mut bytes = Vec::with_capacity(end - start);
            bytes.extend_from_slice(&self.before[start..]);
            bytes.extend_from_slice(&self.after[..end - g]);
            touch(bytes.len());
            Straddle { start, end, bytes }
        })
    }

    /// The first certain boundary in `[off + CHUNK, limit)`, if any.
    fn cut_forward(&self, off: usize, limit: usize) -> Option<usize> {
        let mut p = off + CHUNK;
        while p < limit {
            if self.certain(p) {
                return Some(p);
            }
            p += if self.is_char_boundary(p) { utf8_len(self.byte(p)) } else { 1 };
        }
        None
    }

    /// The last certain boundary in `(limit, off - CHUNK]`, if any.
    fn cut_backward(&self, off: usize, limit: usize) -> Option<usize> {
        let mut p = off - CHUNK;
        while p > limit {
            if self.certain(p) {
                return Some(p);
            }
            p -= 1;
        }
        None
    }
}

impl ReadableDocument for GraphemeDoc<'_> {
    fn read_forward(&self, off: usize) -> &[u8] {
        let len = self.len();
        let off = off.min(len);
        if off == len {
            return &[];
        }
        let st = self.straddle();
        let natural_end = if off < st.start {
            st.start
        } else if off < st.end {
            let chunk = &st.bytes[off - st.start..];
            touch(chunk.len());
            return chunk;
        } else {
            len
        };
        let end = if natural_end - off > CHUNK {
            self.cut_forward(off, natural_end).unwrap_or(natural_end)
        } else {
            natural_end
        };
        touch(end - off);
        self.slice(off..end)
    }

    fn read_backward(&self, off: usize) -> &[u8] {
        let off = off.min(self.len());
        if off == 0 {
            return &[];
        }
        let st = self.straddle();
        let natural_start = if off <= st.start {
            0
        } else if off <= st.end {
            let chunk = &st.bytes[..off - st.start];
            touch(chunk.len());
            return chunk;
        } else {
            st.end
        };
        let start = if off - natural_start > CHUNK {
            self.cut_backward(off, natural_start).unwrap_or(natural_start)
        } else {
            natural_start
        };
        touch(off - start);
        self.slice(start..off)
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Byte length of the UTF-8 sequence starting with `lead` (1 for a stray byte).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}
