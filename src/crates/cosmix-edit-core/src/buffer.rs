//! The buffer façade (plan §3.10): text + op log + anchors + selections.
//!
//! # Contract: transactions (plan §3.4, frozen)
//! All ops of one request refer to the same base text. Each resolves to a base
//! range `[s, e)` plus replacement (pure insert: `s == e`). Two ranges OVERLAP —
//! INVALID_ARGUMENT `overlap_in_txn` — when they share a byte
//! (`max(s) < min(e)`) or a pure insert lies strictly inside a non-empty range.
//! Two non-empty ranges with the same start always overlap.
//!
//! Application order: by `s` descending; at equal `s`, the (at most one)
//! non-empty range op first, then the pure inserts at `s` in REVERSE request
//! order. Invariant: when an op applies, every previously applied op lies at or
//! after the op's `e` in base coordinates, except a same-start range op, which
//! leaves `s` itself unchanged — so recorded sequential coordinates equal base
//! coordinates. Consequences: inserts at one offset read in request order;
//! inserts at a range's start read BEFORE its replacement; an insert at a
//! range's end reads after it. `changed` is computed after application.
//!
//! All validation (resolution, char boundaries, overlap, limits, byte budget)
//! happens before phase 1 of [`crate::text::Text`]; nothing mutates on refusal.
//!
//! # CAS
//! `Latest`: current text. `ExpectRev(r)`: `r != rev` → CONFLICT `stale_rev`.
//! `BaseRev(b)`: offsets only (`base_rev_needs_offsets`); the resolved set is
//! transformed through every logged edit `b+1..=rev` (`Priority::ThroughFirst`),
//! then char-boundary-validated against the current text (exact: a range that
//! overlaps no intervening edit sits in bytes unchanged since `b`). Overlap →
//! CONFLICT `overlap`; `b` older than retention → `history_trimmed`;
//! `b > rev` → INVALID_ARGUMENT `base_rev_in_future`.
//!
//! # Order of checks (the core's part of the refusal precedence)
//! arg shape (`bad_args`, `base_rev_needs_offsets`) → CAS (`stale_rev`,
//! `base_rev_in_future`, `history_trimmed`) → position resolution and
//! validation (incl. `overlap_in_txn`, rebase `overlap`, the post-edit
//! `cursor`) → limits (`limit`, `too_large`, `too_many_lines`, memory).
//!
//! # Constructors and `saved_rev`
//! [`Buffer::new`] and [`Buffer::from_bytes`] return `saved_rev() == None`
//! (dirty); editd calls [`Buffer::mark_saved`] once the load is bound to disk.

use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::anchor::{Anchor, AnchorSpec, Bias, NamedAnchor, Selection};
use crate::error::{CoreError, ErrorCode, reason};
use crate::history::{EntryKind, Group, LogEntry, OpLog};
use crate::limits::{
    FIND_DEFAULT_LIMIT, FIND_MAX_LIMIT, MATCH_ENCODED_MAX, MATCH_TEXT_MAX, MAX_ANCHORS, MAX_OPS_PER_TXN,
    MAX_REQUEST_TEXT_BYTES, MAX_SELECTION_ORIGINS, MAX_SELECTIONS_PER_ORIGIN, REGEX_SIZE_LIMIT,
};
use crate::origin::{Origin, OriginKind, Via};
use crate::ot::{Edit, Priority, RawEdit, post_ranges, post_ranges_raw, transform_range, transform_raw};
use crate::pos::{NamedPos, Point, PosSpec, RangeSpec};
use crate::search::{FindQuery, FindResult, Match};
use crate::text::{Text, count_newlines, oom};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eol {
    Lf,
    Crlf,
    Mixed,
    None,
}

/// What loading found and saving must restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub bom: bool,
    pub eol: Eol,
}

/// Wire form: `OP := {"op":"insert","at":POS,"text":S} | {"op":"delete","range":RANGE}
/// | {"op":"replace","range":RANGE,"text":S}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum OpSpec {
    Insert { at: PosSpec, text: String },
    Delete { range: RangeSpec },
    Replace { range: RangeSpec, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cas {
    Latest,
    ExpectRev(u64),
    BaseRev(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnRequest {
    pub ops: Vec<OpSpec>,
    pub cas: Cas,
    pub coalesce: bool,
    /// Post-edit coordinates; replaces the editing origin's selections with one caret.
    pub cursor: Option<PosSpec>,
    /// Recorded and echoed only; dedup is editd's.
    pub op_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub rev: u64,
    pub base_rev: u64,
    pub kind: EntryKind,
    /// Application order (the event carries these).
    pub edits: Vec<Edit>,
    /// Inserted spans in the new text, offset order, computed after application.
    pub changed: Vec<Range<usize>>,
    pub rebased: bool,
    pub op_id: Option<String>,
    pub inserted_bytes: usize,
    pub deleted_bytes: usize,
    /// Set when this apply trimmed history: the new oldest retained rev.
    pub history_trimmed_to: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneSel {
    Own,
    All,
    Lane(Origin),
}

pub struct Buffer {
    text: Text,
    log: OpLog,
    rev: u64,
    saved_rev: Option<u64>,
    anchors: Vec<NamedAnchor>,
    selections: std::collections::BTreeMap<Origin, Vec<Selection>>,
}

/// One resolved op or undo item: base range, replacement, list index.
#[derive(Debug, Clone)]
struct Resolved {
    s: usize,
    e: usize,
    text: String,
    idx: usize,
}

/// Everything `commit_txn` needs besides the resolved items.
struct Commit<'a> {
    origin: &'a Origin,
    lane: Origin,
    kind: EntryKind,
    via: Via,
    now_ms: u64,
    op_id: Option<String>,
    coalesce: bool,
    cursor: Option<PosSpec>,
    rebased: bool,
}

/// One entry of the history an undo transforms through, as raw edits
/// (rewritten when an undo/redo pair inside the window cancels).
struct Red<'a> {
    entry: &'a LogEntry,
    edits: Vec<RawEdit>,
}

/// A member's inverse item during composition.
struct Composed<P> {
    range: Range<usize>,
    payload: P,
    member: u64,
}

/// Composes the inverses of a group's consecutive entries (oldest first; each
/// with its raw edits and one payload per edit) into one item list in reading
/// order, in the coordinates after the last member.
///
/// Older items are transformed through each newer member; the member's own
/// inverse items are then merged in. Two restored texts that land on one
/// offset are ordered by where the newer member's edit lay relative to the
/// older item when it applied (`transform_raw`'s side flag), which is their
/// original reading order — so a run of backspaces AND a run of forward
/// deletes both restore correctly. `Err((item_member, entry_rev))` on overlap.
fn compose_members<P>(members: Vec<(u64, Vec<RawEdit>, Vec<P>)>) -> Result<Vec<Composed<P>>, (u64, u64)> {
    let mut items: Vec<Composed<P>> = Vec::new();
    for (rev, edits, payloads) in members {
        let k = edits.len();
        // sides[item * k + i]: edit i's region lay left of (point) item.
        let mut sides = vec![false; items.len() * k];
        for (idx, it) in items.iter_mut().enumerate() {
            for (i, ed) in edits.iter().enumerate() {
                let (r, left) =
                    transform_raw(it.range.clone(), *ed, Priority::ThroughFirst).map_err(|_| (it.member, rev))?;
                it.range = r;
                sides[idx * k + i] = left;
            }
        }
        let mut payloads: Vec<Option<P>> = payloads.into_iter().map(Some).collect();
        // Reading order is reverse application order (see `ot::invert`).
        let new: Vec<(usize, Composed<P>)> = post_ranges_raw(&edits)
            .into_iter()
            .enumerate()
            .rev()
            .map(|(i, range)| {
                let payload = payloads[i].take().expect("one payload per edit");
                (i, Composed { range, payload, member: rev })
            })
            .collect();
        let mut merged = Vec::with_capacity(items.len() + new.len());
        let mut old = items.into_iter().enumerate().peekable();
        let mut new = new.into_iter().peekable();
        loop {
            let take_new = match (old.peek(), new.peek()) {
                (None, None) => break,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some((idx, o)), Some((i, n))) => {
                    if o.range.start != n.range.start {
                        n.range.start < o.range.start
                    } else {
                        match (o.range.is_empty(), n.range.is_empty()) {
                            (true, true) => sides[idx * k + i],
                            (true, false) => false,
                            (false, true) => true,
                            (false, false) => return Err((o.member, rev)),
                        }
                    }
                }
            };
            if take_new {
                merged.extend(new.next().map(|(_, n)| n));
            } else {
                merged.extend(old.next().map(|(_, o)| o));
            }
        }
        items = merged;
    }
    Ok(items)
}

/// Rewrites `rest` (the entries after `group`) as if `group` had never been
/// applied, for an undo/redo of `group` that cancels against it. Each entry's
/// edits are transformed through the group's inverse (the inverse's inserts
/// yield at a tie, `SelfFirst`, mirroring the `ThroughFirst` the undo used),
/// and the inverse is carried forward through the entry. `None` when any
/// entry touches the group's text: then the pair is not cancelled.
fn exclude<'a>(group: &[Red<'a>], rest: &[Red<'a>]) -> Option<Vec<Red<'a>>> {
    let members = group.iter().map(|r| (r.entry.rev, r.edits.clone(), r.edits.iter().map(|e| e.dd).collect())).collect();
    let mut inv: Vec<(Range<usize>, usize)> =
        compose_members(members).ok()?.into_iter().map(|c| (c.range, c.payload)).collect();
    let mut out = Vec::with_capacity(rest.len());
    for s in rest {
        let seq: Vec<RawEdit> = inv.iter().rev().map(|(r, len)| RawEdit { p: r.start, dd: r.len(), ii: *len }).collect();
        let mut edits = Vec::with_capacity(s.edits.len());
        for ed in &s.edits {
            let mut r = ed.p..ed.p + ed.dd;
            for iv in &seq {
                r = transform_raw(r, *iv, Priority::SelfFirst).ok()?.0;
            }
            edits.push(RawEdit { p: r.start, dd: ed.dd, ii: ed.ii });
        }
        for (r, _) in &mut inv {
            for ed in &s.edits {
                *r = transform_raw(r.clone(), *ed, Priority::ThroughFirst).ok()?.0;
            }
        }
        out.push(Red { entry: s.entry, edits });
    }
    Some(out)
}

/// An undo/redo item being composed: current range, text it restores, text
/// it expects to remove, and the member rev it came from.
struct UndoItem {
    range: Range<usize>,
    restore: String,
    expect: String,
    member: u64,
}

fn invalid(reason: &'static str, msg: impl Into<String>) -> CoreError {
    CoreError::new(ErrorCode::InvalidArgument, reason, msg)
}

fn limit(msg: impl Into<String>) -> CoreError {
    CoreError::new(ErrorCode::ResourceLimit, reason::LIMIT, msg)
}

const BOM: &[u8] = b"\xef\xbb\xbf";

/// Length of `s` as a JSON string literal (serde_json's escaping).
fn json_str_len(s: &str) -> usize {
    2 + s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

/// `s` cut to at most `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    (s[..cut].to_string(), true)
}

/// Fixed per-match encoded overhead: keys, two POINTs, flags.
const MATCH_OVERHEAD: usize = 192;

/// Whether `next` continues `prev` as one typing (or deleting) run.
fn contiguous(prev: &Edit, next: &Edit) -> bool {
    let pure_insert = |e: &Edit| e.delete == 0 && !e.insert.is_empty();
    let pure_delete = |e: &Edit| e.delete > 0 && e.insert.is_empty();
    if pure_insert(prev) && pure_insert(next) {
        return next.offset == prev.offset + prev.insert.len();
    }
    if pure_delete(prev) && pure_delete(next) {
        return next.offset + next.delete == prev.offset || next.offset == prev.offset;
    }
    false
}

/// §3.4 overlap rule over resolved base ranges.
fn check_overlap(items: &[Resolved]) -> Result<(), CoreError> {
    let mut order: Vec<&Resolved> = items.iter().collect();
    // Points before ranges at one start, so an insert at a range's start passes.
    order.sort_by_key(|r| (r.s, r.e > r.s));
    let mut max_e = 0;
    for r in order {
        if r.s < max_e {
            return Err(invalid(
                reason::OVERLAP_IN_TXN,
                format!("op at [{}, {}) overlaps another op in the same request", r.s, r.e),
            ));
        }
        if r.e > r.s {
            max_e = max_e.max(r.e);
        }
    }
    Ok(())
}

/// §3.4 application order: `s` descending; at equal `s` the non-empty range
/// first, then pure inserts in reverse list order.
fn canonical(items: &mut [Resolved]) {
    items.sort_by(|a, b| b.s.cmp(&a.s).then((b.e > b.s).cmp(&(a.e > a.s))).then(b.idx.cmp(&a.idx)));
}

/// The text after a canonical sequence, as segments of the current text and
/// inserted strings — enough to resolve a post-edit `cursor` before phase 1.
struct PostView<'a> {
    text: &'a Text,
    segs: Vec<(usize, Seg<'a>)>,
    len: usize,
}

enum Seg<'a> {
    Base(Range<usize>),
    Ins(&'a str),
}

impl Seg<'_> {
    fn len(&self) -> usize {
        match self {
            Seg::Base(r) => r.len(),
            Seg::Ins(s) => s.len(),
        }
    }
}

impl<'a> PostView<'a> {
    fn new(text: &'a Text, seq: &'a [Edit]) -> Self {
        let mut segs = Vec::new();
        let mut pos = 0;
        let mut post = 0;
        let mut push = |seg: Seg<'a>, post: &mut usize| {
            let n = seg.len();
            if n > 0 {
                segs.push((*post, seg));
                *post += n;
            }
        };
        for e in seq.iter().rev() {
            push(Seg::Base(pos..e.offset), &mut post);
            push(Seg::Ins(&e.insert), &mut post);
            pos = e.offset + e.delete;
        }
        push(Seg::Base(pos..text.len()), &mut post);
        Self { text, segs, len: post }
    }

    fn is_char_boundary(&self, o: usize) -> bool {
        if o >= self.len {
            return o == self.len;
        }
        let i = self.segs.partition_point(|(start, _)| *start <= o) - 1;
        let (start, seg) = &self.segs[i];
        match seg {
            Seg::Base(r) => self.text.is_char_boundary(r.start + (o - start)),
            Seg::Ins(s) => s.is_char_boundary(o - start),
        }
    }

    /// Post offset of the start of 1-based `line`.
    fn line_start(&self, line: usize) -> Option<usize> {
        if line == 1 {
            return Some(0);
        }
        let mut want = line - 1;
        for (start, seg) in &self.segs {
            match seg {
                Seg::Base(r) => {
                    let starts = self.text.starts_in(r.start, r.end);
                    if starts.len() >= want {
                        return Some(start + (starts[want - 1] as usize - r.start));
                    }
                    want -= starts.len();
                }
                Seg::Ins(s) => {
                    for (j, b) in s.bytes().enumerate() {
                        if b == b'\n' {
                            want -= 1;
                            if want == 0 {
                                return Some(start + j + 1);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Offset `cols` scalars after `from`, not crossing a `\n`.
    fn after_cols(&self, from: usize, cols: usize) -> Option<usize> {
        if cols == 0 {
            return Some(from);
        }
        let mut remaining = cols;
        let mut off = from;
        let mut step = |c: char| {
            if c == '\n' {
                return false;
            }
            off += c.len_utf8();
            remaining -= 1;
            remaining > 0
        };
        'segs: for (start, seg) in &self.segs {
            let end = start + seg.len();
            if end <= from {
                continue;
            }
            let local = from.saturating_sub(*start);
            match seg {
                Seg::Base(r) => {
                    let mut go = true;
                    self.text.for_each_str(r.start + local..r.end, |s| {
                        for c in s.chars() {
                            if !step(c) {
                                go = false;
                                return false;
                            }
                        }
                        true
                    });
                    if !go {
                        break 'segs;
                    }
                }
                Seg::Ins(s) => {
                    for c in s[local..].chars() {
                        if !step(c) {
                            break 'segs;
                        }
                    }
                }
            }
        }
        (remaining == 0).then_some(off)
    }
}

impl Buffer {
    /// Empty buffer at rev 0.
    pub fn new() -> Result<Self, CoreError> {
        Ok(Self::with_text(Text::new()?))
    }

    fn with_text(text: Text) -> Self {
        Self {
            text,
            log: OpLog::default(),
            rev: 0,
            saved_rev: None,
            anchors: Vec::new(),
            selections: Default::default(),
        }
    }

    /// BOM strip, UTF-8 check (`not_utf8`), eol scan, size/line limits.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, FileMeta), CoreError> {
        let (bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let s = std::str::from_utf8(body).map_err(|e| {
            let at = e.valid_up_to() + if bom { BOM.len() } else { 0 };
            invalid(reason::NOT_UTF8, format!("file is not valid UTF-8 (first bad byte at {at})")).with("offset", at)
        })?;
        let text = Text::from_text(s)?;
        let lf = count_newlines(s);
        let crlf = s.matches("\r\n").count();
        let eol = match (lf, crlf) {
            (0, _) => Eol::None,
            (lf, crlf) if lf == crlf => Eol::Crlf,
            (_, 0) => Eol::Lf,
            _ => Eol::Mixed,
        };
        Ok((Self::with_text(text), FileMeta { bom, eol }))
    }

    /// BOM restored, text verbatim.
    pub fn to_bytes(&self, meta: &FileMeta) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len() + BOM.len());
        if meta.bom {
            out.extend_from_slice(BOM);
        }
        out.extend_from_slice(self.text.to_string_lossless().as_bytes());
        out
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.text.line_count()
    }

    pub fn point(&self, offset: usize) -> Point {
        self.text.point(offset)
    }

    fn check_offset(&self, o: usize) -> Result<usize, CoreError> {
        if o > self.len() {
            return Err(invalid(reason::OFFSET_OUT_OF_RANGE, format!("offset {o} is past the end ({})", self.len()))
                .with("offset", o)
                .with("bytes", self.len()));
        }
        if !self.text.is_char_boundary(o) {
            return Err(invalid(reason::NOT_CHAR_BOUNDARY, format!("offset {o} is inside a UTF-8 sequence")).with("offset", o));
        }
        Ok(o)
    }

    fn check_line(&self, line: usize) -> Result<(), CoreError> {
        if line == 0 || line > self.line_count() {
            return Err(invalid(
                reason::LINE_OUT_OF_RANGE,
                format!("line {line} is outside 1..={}", self.line_count()),
            )
            .with("line", line)
            .with("lines", self.line_count()));
        }
        Ok(())
    }

    fn unknown_anchor(name: &str) -> CoreError {
        CoreError::new(ErrorCode::NotFound, reason::UNKNOWN_ANCHOR, format!("no anchor named {name:?}")).with("name", name)
    }

    pub fn resolve_pos(&self, p: &PosSpec) -> Result<usize, CoreError> {
        match p {
            PosSpec::Offset(o) => self.check_offset(*o),
            PosSpec::Named(NamedPos::Start) => Ok(0),
            PosSpec::Named(NamedPos::End) => Ok(self.len()),
            PosSpec::LineCol { line, col } => {
                self.check_line(*line)?;
                let col = col.unwrap_or(1);
                self.text.line_col_offset(*line, col).ok_or_else(|| {
                    invalid(reason::COL_OUT_OF_RANGE, format!("col {col} is past the end of line {line}"))
                        .with("line", *line)
                        .with("col", col)
                })
            }
            PosSpec::Anchor { anchor } => {
                self.anchor(anchor).map(|a| a.start.offset).ok_or_else(|| Self::unknown_anchor(anchor))
            }
        }
    }

    pub fn resolve_range(&self, r: &RangeSpec) -> Result<Range<usize>, CoreError> {
        let ordered = |s: usize, e: usize| {
            if s > e {
                Err(invalid(reason::BAD_ARGS, format!("range start {s} is after its end {e}")))
            } else {
                Ok(s..e)
            }
        };
        match r {
            RangeSpec::Offsets([s, e]) => ordered(self.check_offset(*s)?, self.check_offset(*e)?),
            RangeSpec::Span { start, end } => ordered(self.resolve_pos(start)?, self.resolve_pos(end)?),
            RangeSpec::Lines { lines: [a, b] } => {
                self.check_line(*a)?;
                self.check_line(*b)?;
                if a > b {
                    return Err(invalid(reason::LINE_OUT_OF_RANGE, format!("lines [{a}, {b}] are reversed"))
                        .with("line", *a));
                }
                let s = self.text.line_start(*a).unwrap_or(0);
                let e = self.text.line_start(b + 1).unwrap_or(self.len());
                Ok(s..e)
            }
            RangeSpec::Anchor { anchor } => {
                let a = self.anchor(anchor).ok_or_else(|| Self::unknown_anchor(anchor))?;
                Ok(a.start.offset..a.end.map_or(a.start.offset, |e| e.offset))
            }
            RangeSpec::All(_) => Ok(0..self.len()),
        }
    }

    /// Chunked copy; never moves the gap.
    pub fn read(&self, r: Range<usize>, out: &mut String) {
        self.text.read(r, out)
    }

    /// Frozen copy for paged reads (editd leases its bytes first).
    pub fn snapshot(&self) -> Arc<str> {
        Arc::from(self.text.to_string_lossless())
    }

    fn conflict(&self, reason: &'static str, msg: impl Into<String>) -> CoreError {
        CoreError::new(ErrorCode::Conflict, reason, msg).with("rev", self.rev)
    }

    /// Validates and resolves a request to base ranges in the CURRENT text
    /// (rebased when `base_rev` is older). Pure.
    fn resolve_txn(&self, req: &TxnRequest) -> Result<Vec<Resolved>, CoreError> {
        if req.ops.is_empty() {
            return Err(invalid(reason::BAD_ARGS, "a transaction needs at least one op"));
        }
        if let Cas::BaseRev(_) = req.cas {
            let offsets_only = req.ops.iter().all(|op| match op {
                OpSpec::Insert { at, .. } => matches!(at, PosSpec::Offset(_)),
                OpSpec::Delete { range } | OpSpec::Replace { range, .. } => matches!(range, RangeSpec::Offsets(_)),
            });
            if !offsets_only {
                return Err(invalid(reason::BASE_REV_NEEDS_OFFSETS, "base_rev requests take byte offsets only"));
            }
        }
        let rebase_from = match req.cas {
            Cas::Latest => None,
            Cas::ExpectRev(r) => {
                if r != self.rev {
                    return Err(self.conflict(
                        reason::STALE_REV,
                        format!("expect_rev {r} is stale: buffer is at rev {}", self.rev),
                    ));
                }
                None
            }
            Cas::BaseRev(b) => {
                if b > self.rev {
                    return Err(invalid(
                        reason::BASE_REV_IN_FUTURE,
                        format!("base_rev {b} is newer than the buffer (rev {})", self.rev),
                    )
                    .with("rev", self.rev));
                }
                if b < self.oldest_rev() {
                    return Err(self
                        .conflict(
                            reason::HISTORY_TRIMMED,
                            format!(
                                "base_rev {b} is older than the retained history (oldest rev {})",
                                self.oldest_rev()
                            ),
                        )
                        .with("oldest_rev", self.oldest_rev()));
                }
                (b < self.rev).then_some(b)
            }
        };

        let mut items = Vec::with_capacity(req.ops.len());
        match rebase_from {
            None => {
                for (idx, op) in req.ops.iter().enumerate() {
                    let (s, e, text) = match op {
                        OpSpec::Insert { at, text } => {
                            let p = self.resolve_pos(at)?;
                            (p, p, text.as_str())
                        }
                        OpSpec::Delete { range } => {
                            let r = self.resolve_range(range)?;
                            (r.start, r.end, "")
                        }
                        OpSpec::Replace { range, text } => {
                            let r = self.resolve_range(range)?;
                            (r.start, r.end, text.as_str())
                        }
                    };
                    items.push(Resolved { s, e, text: text.to_string(), idx });
                }
                check_overlap(&items)?;
            }
            Some(b) => {
                // Length of the text at rev `b`, from the net size of every later entry.
                let mut hist_len = self.len() as i128;
                for entry in self.log.after(b) {
                    for e in &entry.edits {
                        hist_len += e.delete as i128 - e.insert.len() as i128;
                    }
                }
                for (idx, op) in req.ops.iter().enumerate() {
                    let (s, e, text) = match op {
                        OpSpec::Insert { at: PosSpec::Offset(p), text } => (*p, *p, text.clone()),
                        OpSpec::Delete { range: RangeSpec::Offsets([s, e]) } => (*s, *e, String::new()),
                        OpSpec::Replace { range: RangeSpec::Offsets([s, e]), text } => (*s, *e, text.clone()),
                        _ => unreachable!("offsets checked above"),
                    };
                    if e as i128 > hist_len || s as i128 > hist_len {
                        return Err(invalid(
                            reason::OFFSET_OUT_OF_RANGE,
                            format!("offset {} is past the end of rev {b} ({hist_len} bytes)", s.max(e)),
                        )
                        .with("offset", s.max(e)));
                    }
                    if s > e {
                        return Err(invalid(reason::BAD_ARGS, format!("range start {s} is after its end {e}")));
                    }
                    items.push(Resolved { s, e, text, idx });
                }
                check_overlap(&items)?;
                for item in &mut items {
                    let mut r = item.s..item.e;
                    for entry in self.log.after(b) {
                        for edit in &entry.edits {
                            r = transform_range(r, edit, Priority::ThroughFirst).map_err(|_| {
                                self.conflict(
                                    reason::OVERLAP,
                                    format!(
                                        "base_rev {b}: range [{}, {}) overlaps rev {} by {}",
                                        item.s, item.e, entry.rev, entry.origin
                                    ),
                                )
                                .with("intervening_rev", entry.rev)
                                .with("intervening_origin", entry.origin.to_string())
                            })?;
                        }
                    }
                    self.check_offset(r.start)?;
                    self.check_offset(r.end)?;
                    item.s = r.start;
                    item.e = r.end;
                }
            }
        }

        if req.ops.len() > MAX_OPS_PER_TXN {
            return Err(limit(format!("{} ops exceed the {MAX_OPS_PER_TXN}-op limit", req.ops.len()))
                .with("limit", MAX_OPS_PER_TXN));
        }
        let inserted: usize = items.iter().map(|r| r.text.len()).sum();
        if inserted > MAX_REQUEST_TEXT_BYTES {
            return Err(CoreError::new(
                ErrorCode::ResourceLimit,
                reason::TOO_LARGE,
                format!("{inserted} inserted bytes exceed the {MAX_REQUEST_TEXT_BYTES}-byte request limit"),
            )
            .with("limit", MAX_REQUEST_TEXT_BYTES));
        }
        Ok(items)
    }

    /// Resolves a `cursor` in the coordinates after `seq` (canonical).
    fn resolve_post(&self, p: &PosSpec, seq: &[Edit], final_lines: usize) -> Result<usize, CoreError> {
        let view = PostView::new(&self.text, seq);
        match p {
            PosSpec::Offset(o) => {
                if *o > view.len {
                    return Err(invalid(
                        reason::OFFSET_OUT_OF_RANGE,
                        format!("cursor {o} is past the end of the edited text ({})", view.len),
                    )
                    .with("offset", *o));
                }
                if !view.is_char_boundary(*o) {
                    return Err(invalid(reason::NOT_CHAR_BOUNDARY, format!("cursor {o} is inside a UTF-8 sequence"))
                        .with("offset", *o));
                }
                Ok(*o)
            }
            PosSpec::Named(NamedPos::Start) => Ok(0),
            PosSpec::Named(NamedPos::End) => Ok(view.len),
            PosSpec::LineCol { line, col } => {
                if *line == 0 || *line > final_lines {
                    return Err(invalid(
                        reason::LINE_OUT_OF_RANGE,
                        format!("cursor line {line} is outside 1..={final_lines}"),
                    )
                    .with("line", *line));
                }
                let col = col.unwrap_or(1);
                let start = view.line_start(*line).unwrap_or(view.len);
                view.after_cols(start, col.saturating_sub(1)).filter(|_| col >= 1).ok_or_else(|| {
                    invalid(reason::COL_OUT_OF_RANGE, format!("cursor col {col} is past the end of line {line}"))
                        .with("line", *line)
                        .with("col", col)
                })
            }
            PosSpec::Anchor { anchor } => {
                let mut a = self.anchor(anchor).cloned().ok_or_else(|| Self::unknown_anchor(anchor))?;
                a.map(seq, self.rev + 1);
                Ok(a.start.offset)
            }
        }
    }

    /// Canonicalises, runs both phases and records the entry. Everything that
    /// can refuse happens before `Text::commit`.
    fn commit_txn(&mut self, mut items: Vec<Resolved>, c: Commit<'_>) -> Result<Applied, CoreError> {
        canonical(&mut items);
        let mut deleted = Vec::with_capacity(items.len());
        let mut seq = Vec::with_capacity(items.len());
        for r in items {
            let mut d = String::new();
            self.text.read(r.s..r.e, &mut d);
            deleted.push(d);
            seq.push(Edit { offset: r.s, delete: r.e - r.s, insert: r.text });
        }

        // Validation that needs the post-edit shape, then limits.
        let sim = self.text.simulate(&seq)?;
        let cursor = match &c.cursor {
            Some(p) => {
                let at = self.resolve_post(p, &seq, sim.final_lines)?;
                if !self.selections.contains_key(c.origin) && self.selections.len() >= MAX_SELECTION_ORIGINS {
                    return Err(limit(format!("{MAX_SELECTION_ORIGINS} origins already hold selections"))
                        .with("limit", MAX_SELECTION_ORIGINS));
                }
                Some(at)
            }
            None => None,
        };

        // Phase 1: memory for text, line index, log and lane stacks.
        let prepared = self.text.prepare(seq.clone())?;
        self.log.entries.try_reserve(1).map_err(|_| oom("op log"))?;
        {
            let lane = self.log.lanes.entry(c.lane.clone()).or_default();
            lane.undo.try_reserve(1).map_err(|_| oom("undo stack"))?;
            lane.redo.try_reserve(1).map_err(|_| oom("redo stack"))?;
        }

        // Phase 2: nothing below can fail.
        let base_rev = self.rev;
        let new_rev = base_rev + 1;
        self.text.commit(prepared);
        for a in &mut self.anchors {
            a.map(&seq, new_rev);
        }
        for (o, sels) in &mut self.selections {
            let bias = if o == c.origin { Bias::After } else { Bias::Before };
            for s in sels {
                s.map(&seq, bias);
            }
        }
        if let Some(at) = cursor {
            self.selections.insert(c.origin.clone(), vec![Selection { anchor: at, head: at }]);
        }

        let mut changed: Vec<Range<usize>> =
            post_ranges(&seq).into_iter().filter(|r| !r.is_empty()).collect();
        changed.sort_by_key(|r| r.start);
        let inserted_bytes = seq.iter().map(|e| e.insert.len()).sum();
        let deleted_bytes = seq.iter().map(|e| e.delete).sum();

        let coalesced = c.coalesce
            && matches!(c.kind, EntryKind::Edit)
            && seq.len() == 1
            && self.log.lanes.get(&c.lane).and_then(|l| l.undo.last()).is_some_and(|g| *g.end() == base_rev)
            && self.log.entries.back().is_some_and(|prev| {
                prev.rev == base_rev
                    && prev.kind == EntryKind::Edit
                    && prev.lane == c.lane
                    && prev.edits.len() == 1
                    && contiguous(&prev.edits[0], &seq[0])
            });
        let lane = self.log.lanes.entry(c.lane.clone()).or_default();
        match &c.kind {
            EntryKind::Edit | EntryKind::Reload => {
                lane.redo.clear();
                match lane.undo.last_mut() {
                    Some(g) if coalesced => *g = *g.start()..=new_rev,
                    _ => lane.undo.push(new_rev..=new_rev),
                }
            }
            EntryKind::Undo { .. } => {
                lane.undo.pop();
                lane.redo.push(new_rev..=new_rev);
            }
            EntryKind::Redo { .. } => {
                lane.redo.pop();
                lane.undo.push(new_rev..=new_rev);
            }
        }

        let entry = LogEntry {
            rev: new_rev,
            origin: c.origin.clone(),
            lane: c.lane,
            kind: c.kind.clone(),
            edits: seq.clone(),
            deleted,
            via: c.via,
            op_id: c.op_id.clone(),
            time_ms: c.now_ms,
        };
        let history_trimmed_to = self.log.push(entry);
        self.rev = new_rev;
        Ok(Applied {
            rev: new_rev,
            base_rev,
            kind: c.kind,
            edits: seq,
            changed,
            rebased: c.rebased,
            op_id: c.op_id,
            inserted_bytes,
            deleted_bytes,
            history_trimmed_to,
        })
    }

    /// One transaction (module docs). On `Err` nothing changed.
    pub fn apply(&mut self, req: TxnRequest, origin: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        let items = self.resolve_txn(&req)?;
        let rebased = matches!(req.cas, Cas::BaseRev(b) if b < self.rev);
        self.commit_txn(
            items,
            Commit {
                origin,
                lane: origin.clone(),
                kind: EntryKind::Edit,
                via,
                now_ms,
                op_id: req.op_id,
                coalesce: req.coalesce,
                cursor: req.cursor,
                rebased,
            },
        )
    }

    /// Byte-budget preview for editd's lease: the peak growth (text + log text)
    /// `apply` would need. Pure; same validation as `apply`.
    pub fn apply_cost(&self, req: &TxnRequest) -> Result<usize, CoreError> {
        let items = self.resolve_txn(req)?;
        self.cost_of(items)
    }

    fn pick_lane(&self, sel: &LaneSel, caller: &Origin, redo: bool) -> Result<Origin, CoreError> {
        let stack = |o: &Origin| {
            self.log.lanes.get(o).and_then(|l| if redo { l.redo.last() } else { l.undo.last() }).cloned()
        };
        let lane = match sel {
            LaneSel::Own => caller.clone(),
            LaneSel::Lane(o) => o.clone(),
            LaneSel::All => self
                .log
                .lanes
                .keys()
                .filter_map(|o| stack(o).map(|g| (*g.end(), o)))
                .max_by_key(|(end, _)| *end)
                .map(|(_, o)| o.clone())
                .unwrap_or_else(|| caller.clone()),
        };
        if stack(&lane).is_none() {
            let (r, what) = if redo { (reason::NOTHING_TO_REDO, "redo") } else { (reason::NOTHING_TO_UNDO, "undo") };
            let lane_name = match sel {
                LaneSel::All => "*".to_string(),
                _ => lane.to_string(),
            };
            return Err(CoreError::new(ErrorCode::NotFound, r, format!("nothing to {what} in lane {lane_name}"))
                .with("lane", lane_name)
                .with("rev", self.rev));
        }
        Ok(lane)
    }

    fn undo_conflict(&self, lane: &Origin, member: u64, entry: Option<&LogEntry>) -> CoreError {
        let mut err = match entry {
            Some(e) => self
                .conflict(
                    reason::UNDO_CONFLICT,
                    format!("undoing rev {member} conflicts with rev {} by {}", e.rev, e.origin),
                )
                .with("intervening_rev", e.rev)
                .with("intervening_origin", e.origin.to_string()),
            None => self.conflict(
                reason::UNDO_CONFLICT,
                format!("undoing rev {member} conflicts with later edits to the same text"),
            ),
        };
        err = err.with("lane", lane.to_string());
        err
    }

    fn entry(&self, rev: u64) -> Option<&LogEntry> {
        self.log.after(rev.saturating_sub(1)).next().filter(|e| e.rev == rev)
    }

    /// The history an undo of a group ending at `last` transforms through:
    /// every later entry, except that an undo/redo whose target entries are
    /// still in the list CANCELS against them. The entries between the pair
    /// are rewritten to exclude the cancelled edits ([`exclude`]). Without
    /// this, "edit, fix the edit, undo the fix, undo the edit" would refuse:
    /// the fix overlaps the edit's text even though it was itself undone.
    /// A pair that cannot be excluded cleanly stays in the list (the undo may
    /// then refuse — conservative, never wrong).
    fn reduced_after(&self, last: u64) -> Vec<Red<'_>> {
        let mut red: Vec<Red<'_>> = Vec::new();
        for entry in self.log.after(last) {
            if let EntryKind::Undo { of } | EntryKind::Redo { of } = &entry.kind {
                let (a, b) = (*of.start(), *of.end());
                let n = usize::try_from(b.saturating_sub(a).saturating_add(1)).unwrap_or(usize::MAX);
                if let Some(pos) = red.iter().position(|r| r.entry.rev == a) {
                    let whole = red
                        .get(pos..pos.saturating_add(n))
                        .is_some_and(|g| g.iter().zip(a..=b).all(|(r, rev)| r.entry.rev == rev));
                    if whole && let Some(rest) = exclude(&red[pos..pos + n], &red[pos + n..]) {
                        red.truncate(pos);
                        red.extend(rest);
                        continue;
                    }
                }
            }
            red.push(Red { entry, edits: entry.edits.iter().map(RawEdit::of).collect() });
        }
        red
    }

    /// The preflight of `history` steps 1-4: the group's inverse in current
    /// coordinates, in reading order, verified against the current text.
    /// Members compose with [`compose_members`]; the result is transformed
    /// through [`Self::reduced_after`].
    fn compose_inverse(&self, group: &Group, lane: &Origin) -> Result<Vec<UndoItem>, CoreError> {
        let first = *group.start();
        let last = *group.end();
        if first <= self.oldest_rev() {
            return Err(self
                .conflict(reason::HISTORY_TRIMMED, format!("rev {first} is older than the retained history"))
                .with("oldest_rev", self.oldest_rev()));
        }
        let members = self
            .log
            .after(first - 1)
            .take_while(|e| e.rev <= last)
            .map(|e| {
                // A group is contiguous log entries of ONE lane: coalescing
                // only extends a group whose last entry is the log's newest.
                debug_assert_eq!(&e.lane, lane, "group {first}..={last} holds rev {} of another lane", e.rev);
                let payloads = e
                    .edits
                    .iter()
                    .enumerate()
                    .map(|(i, ed)| (e.deleted.get(i).cloned().unwrap_or_default(), ed.insert.clone()))
                    .collect();
                (e.rev, e.edits.iter().map(RawEdit::of).collect(), payloads)
            })
            .collect();
        let composed = compose_members(members)
            .map_err(|(member, rev)| self.undo_conflict(lane, member, self.entry(rev)))?;
        let mut items: Vec<UndoItem> = composed
            .into_iter()
            .map(|c| UndoItem { range: c.range, restore: c.payload.0, expect: c.payload.1, member: c.member })
            .collect();
        for r in self.reduced_after(last) {
            for it in &mut items {
                for ed in &r.edits {
                    it.range = transform_raw(it.range.clone(), *ed, Priority::ThroughFirst)
                        .map_err(|_| self.undo_conflict(lane, it.member, Some(r.entry)))?
                        .0;
                }
            }
        }

        // Step 3: the union must itself be overlap-free.
        let mut max_e = 0;
        for it in &items {
            if it.range.start < max_e {
                return Err(self.undo_conflict(lane, it.member, None));
            }
            if !it.range.is_empty() {
                max_e = it.range.end;
            }
        }
        // Step 4: every item must still find exactly the text it removes.
        let mut buf = String::new();
        for it in &items {
            buf.clear();
            self.text.read(it.range.clone(), &mut buf);
            if buf != it.expect {
                return Err(self.undo_conflict(lane, it.member, None));
            }
        }
        Ok(items)
    }

    /// The lane, group and inverse items an undo (or redo) would commit. Pure.
    fn undo_plan(&self, sel: &LaneSel, caller: &Origin, redo: bool) -> Result<(Origin, Group, Vec<Resolved>), CoreError> {
        let lane = self.pick_lane(sel, caller, redo)?;
        let group = {
            let l = &self.log.lanes[&lane];
            let top = if redo { l.redo.last() } else { l.undo.last() };
            top.cloned().expect("pick_lane checked")
        };
        let items = self.compose_inverse(&group, &lane)?;
        let resolved = items
            .into_iter()
            .enumerate()
            .map(|(idx, it)| Resolved { s: it.range.start, e: it.range.end, text: it.restore, idx })
            .collect();
        Ok((lane, group, resolved))
    }

    /// Peak growth (text + log text) committing `items` would need.
    fn cost_of(&self, mut items: Vec<Resolved>) -> Result<usize, CoreError> {
        canonical(&mut items);
        let seq: Vec<Edit> =
            items.into_iter().map(|r| Edit { offset: r.s, delete: r.e - r.s, insert: r.text }).collect();
        let sim = self.text.simulate(&seq)?;
        let log_text: usize = seq.iter().map(|e| e.insert.len() + e.delete).sum();
        Ok(sim.peak_len.saturating_sub(self.len()) + log_text)
    }

    fn undo_redo(
        &mut self,
        sel: LaneSel,
        caller: &Origin,
        via: Via,
        now_ms: u64,
        redo: bool,
        op_id: Option<String>,
    ) -> Result<Applied, CoreError> {
        let (lane, group, resolved) = self.undo_plan(&sel, caller, redo)?;
        let kind = if redo { EntryKind::Redo { of: group } } else { EntryKind::Undo { of: group } };
        self.commit_txn(
            resolved,
            Commit { origin: caller, lane, kind, via, now_ms, op_id, coalesce: false, cursor: None, rebased: false },
        )
    }

    /// Preflighted undo (see `history` module docs). On `Err` nothing changed.
    pub fn undo(&mut self, lane: LaneSel, caller: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        self.undo_redo(lane, caller, via, now_ms, false, None)
    }

    pub fn redo(&mut self, lane: LaneSel, caller: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        self.undo_redo(lane, caller, via, now_ms, true, None)
    }

    /// [`undo`](Self::undo) / [`redo`](Self::redo) recording the request's
    /// `op_id` on the new entry (echoed in `Applied`, history and events).
    pub fn undo_redo_op(
        &mut self,
        lane: LaneSel,
        caller: &Origin,
        via: Via,
        now_ms: u64,
        redo: bool,
        op_id: Option<String>,
    ) -> Result<Applied, CoreError> {
        self.undo_redo(lane, caller, via, now_ms, redo, op_id)
    }

    /// Byte-budget preview of an undo (or redo), like [`apply_cost`](Self::apply_cost):
    /// the peak growth of text + log text it would need. Pure; the same
    /// refusals as the undo itself (nothing to undo, conflict, limits).
    pub fn undo_cost(&self, lane: &LaneSel, caller: &Origin, redo: bool) -> Result<usize, CoreError> {
        let (_, _, resolved) = self.undo_plan(lane, caller, redo)?;
        self.cost_of(resolved)
    }

    /// Common prefix/suffix kept; one `Reload` entry in lane `tool:disk`.
    /// `None` when identical.
    pub fn reload_minimal(&mut self, new_text: &str, via: Via, now_ms: u64) -> Result<Option<Applied>, CoreError> {
        let item = {
            let cur = self.text.contiguous();
            let mut pre = cur.bytes().zip(new_text.bytes()).take_while(|(a, b)| a == b).count();
            while !cur.is_char_boundary(pre) || !new_text.is_char_boundary(pre) {
                pre -= 1;
            }
            if pre == cur.len() && pre == new_text.len() {
                return Ok(None);
            }
            let room = cur.len().min(new_text.len()) - pre;
            let mut suf = cur.bytes().rev().zip(new_text.bytes().rev()).take(room).take_while(|(a, b)| a == b).count();
            while !cur.is_char_boundary(cur.len() - suf) || !new_text.is_char_boundary(new_text.len() - suf) {
                suf -= 1;
            }
            Resolved { s: pre, e: cur.len() - suf, text: new_text[pre..new_text.len() - suf].to_string(), idx: 0 }
        };
        let disk = Origin::new(OriginKind::Tool, "disk");
        self.commit_txn(
            vec![item],
            Commit {
                origin: &disk,
                lane: disk.clone(),
                kind: EntryKind::Reload,
                via,
                now_ms,
                op_id: None,
                coalesce: false,
                cursor: None,
                rebased: false,
            },
        )
        .map(Some)
    }

    pub fn set_selections(&mut self, origin: &Origin, sels: Vec<Selection>) -> Result<(), CoreError> {
        if sels.len() > MAX_SELECTIONS_PER_ORIGIN {
            return Err(limit(format!("{} selections exceed the {MAX_SELECTIONS_PER_ORIGIN} per origin", sels.len()))
                .with("limit", MAX_SELECTIONS_PER_ORIGIN));
        }
        for s in &sels {
            self.check_offset(s.anchor)?;
            self.check_offset(s.head)?;
        }
        if sels.is_empty() {
            self.selections.remove(origin);
            return Ok(());
        }
        if !self.selections.contains_key(origin) && self.selections.len() >= MAX_SELECTION_ORIGINS {
            return Err(limit(format!("{MAX_SELECTION_ORIGINS} origins already hold selections"))
                .with("limit", MAX_SELECTION_ORIGINS));
        }
        self.selections.insert(origin.clone(), sels);
        Ok(())
    }

    pub fn selections(&self) -> impl Iterator<Item = (&Origin, &[Selection])> {
        self.selections.iter().map(|(o, s)| (o, s.as_slice()))
    }

    pub fn anchor_set(&mut self, name: &str, spec: AnchorSpec) -> Result<(), CoreError> {
        if !crate::anchor::is_valid_name(name) {
            return Err(invalid(reason::BAD_NAME, format!("anchor name {name:?} must match ^[A-Za-z0-9._-]{{1,64}}$")));
        }
        let anchor = match (&spec.at, &spec.range) {
            (Some(at), None) => {
                let o = self.resolve_pos(at)?;
                NamedAnchor {
                    name: name.to_string(),
                    start: Anchor { offset: o, bias: spec.bias.unwrap_or_default(), collapsed_rev: None },
                    end: None,
                }
            }
            (None, Some(range)) => {
                let r = self.resolve_range(range)?;
                NamedAnchor {
                    name: name.to_string(),
                    start: Anchor { offset: r.start, bias: Bias::After, collapsed_rev: None },
                    end: Some(Anchor { offset: r.end, bias: Bias::Before, collapsed_rev: None }),
                }
            }
            _ => return Err(invalid(reason::BAD_ARGS, "anchor.set takes exactly one of `at` or `range`")),
        };
        if let Some(existing) = self.anchors.iter_mut().find(|a| a.name == name) {
            *existing = anchor;
            return Ok(());
        }
        if self.anchors.len() >= MAX_ANCHORS {
            return Err(limit(format!("{MAX_ANCHORS} anchors already set")).with("limit", MAX_ANCHORS));
        }
        self.anchors.push(anchor);
        Ok(())
    }

    pub fn anchor(&self, name: &str) -> Option<&NamedAnchor> {
        self.anchors.iter().find(|a| a.name == name)
    }

    pub fn anchors(&self) -> impl Iterator<Item = &NamedAnchor> {
        self.anchors.iter()
    }

    pub fn anchor_clear(&mut self, name: &str) -> bool {
        let before = self.anchors.len();
        self.anchors.retain(|a| a.name != name);
        self.anchors.len() != before
    }

    /// `budget_bytes` = the encoded-reply budget left for matches.
    ///
    /// A resumed search (`from` = a previous `next`) may report an empty match
    /// at exactly `from`, which the uninterrupted search would have skipped as
    /// adjacent to the previous match.
    pub fn find(&mut self, q: &FindQuery, budget_bytes: usize) -> Result<FindResult, CoreError> {
        let range = match &q.range {
            Some(r) => self.resolve_range(r)?,
            None => 0..self.len(),
        };
        let mut pos = range.start;
        if let Some(f) = q.from {
            pos = pos.max(self.check_offset(f)?);
        }
        let max = if q.limit == 0 { FIND_DEFAULT_LIMIT } else { q.limit.min(FIND_MAX_LIMIT) };
        let pattern = if q.regex { q.pattern.clone() } else { regex::escape(&q.pattern) };
        let re = regex::RegexBuilder::new(&pattern)
            .multi_line(true)
            .case_insensitive(!q.case)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| invalid(reason::BAD_REGEX, format!("bad pattern: {e}")))?;

        let end = range.end;
        let text = self.text.contiguous();
        let hay = &text[..end];
        let next_char = |i: usize| i + hay[i..].chars().next().map_or(1, char::len_utf8);
        let mut matches: Vec<Match> = Vec::new();
        let mut used = 0usize;
        let mut truncated = false;
        let mut last_end: Option<usize> = None;
        while pos <= end {
            let (m, caps) = if q.groups {
                match re.captures_at(hay, pos) {
                    Some(c) => (c.get(0).expect("group 0 always participates"), Some(c)),
                    None => break,
                }
            } else {
                match re.find_at(hay, pos) {
                    Some(m) => (m, None),
                    None => break,
                }
            };
            if m.is_empty() && last_end == Some(m.start()) {
                pos = next_char(m.start());
                continue;
            }
            if matches.len() == max {
                truncated = true;
                break;
            }
            let (text, text_truncated) = truncate(m.as_str(), MATCH_TEXT_MAX);
            let mut size = MATCH_OVERHEAD + json_str_len(&text);
            let mut groups_truncated = false;
            let groups = caps.map(|c| {
                let mut out = Vec::with_capacity(c.len().saturating_sub(1));
                for g in c.iter().skip(1) {
                    let g = g.map(|g| truncate(g.as_str(), MATCH_TEXT_MAX).0);
                    let g_size = 1 + g.as_deref().map_or(4, json_str_len);
                    if size + g_size > MATCH_ENCODED_MAX {
                        // Omit this group and every later one: a `null` per
                        // omitted slot would itself grow past the cap.
                        groups_truncated = true;
                        break;
                    }
                    size += g_size;
                    out.push(g);
                }
                out
            });
            if !matches.is_empty() && used + size > budget_bytes {
                truncated = true;
                break;
            }
            used += size;
            let r = m.range();
            pos = if r.is_empty() { next_char(r.start) } else { r.end };
            last_end = Some(r.end);
            matches.push(Match { range: r, text, text_truncated, groups, groups_truncated });
        }
        let next = if truncated {
            matches.last().map(|m| if m.range.is_empty() { next_char(m.range.start) } else { m.range.end })
        } else {
            None
        };
        Ok(FindResult { matches, truncated, next })
    }

    /// Up to `limit` entries with `rev > since_rev`, oldest first.
    pub fn history(&self, since_rev: u64, limit: usize) -> impl Iterator<Item = &LogEntry> {
        self.log.since(since_rev, limit)
    }

    pub fn oldest_rev(&self) -> u64 {
        self.log.oldest_rev()
    }

    /// Bytes of retained log text (counted in editd's aggregate budget).
    pub fn log_text_bytes(&self) -> usize {
        self.log.text_bytes
    }

    pub fn mark_saved(&mut self) {
        self.saved_rev = Some(self.rev);
    }

    pub fn saved_rev(&self) -> Option<u64> {
        self.saved_rev
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_rev != Some(self.rev)
    }

    /// Test access to the text.
    #[cfg(test)]
    pub(crate) fn text(&self) -> &Text {
        &self.text
    }

    /// Test: every piece of state a refusal must leave untouched.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "text={:?}\nlines={:?}\nrev={} saved={:?}\nlog={:?}\nbase={} bytes={}\nlanes={:?}\nanchors={:?}\nsels={:?}",
            self.text.to_string_lossless(),
            self.text.line_index(),
            self.rev,
            self.saved_rev,
            self.log.entries,
            self.log.base,
            self.log.text_bytes,
            self.log.lanes,
            self.anchors,
            self.selections,
        )
    }

    /// Undo/redo stacks of one lane (tests and editd diagnostics).
    pub fn lane_stacks(&self, lane: &Origin) -> (Vec<Group>, Vec<Group>) {
        self.log.lanes.get(lane).map(|l| (l.undo.clone(), l.redo.clone())).unwrap_or_default()
    }
}
