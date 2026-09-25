//! Text storage: vendored gap buffer + line index, with two-phase application
//! (plan §3.1).
//!
//! # Contract: two-phase apply (frozen)
//! 1. [`Text::prepare`] mutates nothing. Given the exact application sequence
//!    `e_1 … e_k` (each in the coordinates valid when it applies — which, by the
//!    §3.4 ordering invariant, equal the base coordinates), it simulates the
//!    sequence with checked arithmetic (overflow → RESOURCE_LIMIT):
//!    - `len_i = len_{i-1} - delete_i + insert_i.len()`;
//!    - `need_i = GapBuffer::commit_needed(len_{i-1} - delete_i, insert_i.len())`
//!      (exactly what the gap buffer would commit at step i);
//!    - `lines_i = lines_{i-1} - newlines(deleted span_i) + newlines(insert_i)`,
//!      the deleted span's newlines read from the unmodified line index;
//!    - `required_commit = max need_i`, `peak_len = max len_i`, `peak_lines = max lines_i`.
//!
//!    It checks `peak_len <= MAX_BUFFER_BYTES`, `peak_lines <= MAX_LINES`, then
//!    `ensure_commit(required_commit)`,
//!    `starts.try_reserve(peak_lines.saturating_sub(starts.len()))` and a
//!    pre-reserved scratch vector for the largest insert's line starts. Any
//!    failure → RESOURCE_LIMIT with the text untouched.
//! 2. [`Text::commit`] applies the sequence and cannot fail or allocate: every
//!    gap-buffer `replace` finds its memory committed (asserted via
//!    `commit_calls` in debug builds), and the line index is spliced in place
//!    (`copy_within` into reserved capacity; no `Vec::splice`, no temporaries).
//!
//! Callers (the buffer façade) reserve their own log/lane vectors in phase 1 too.
//! Heap OOM for ordinary allocations aborts the process (E0 is volatile).
//!
//! `prepare` also refuses (INTERNAL) a sequence that is not in the §3.4
//! canonical shape — every step lying at or before the previous step's offset
//! (`offset_i + delete_i <= offset_{i-1}`) and on char boundaries — because
//! the simulation reads deleted newlines from the unmodified index, which is
//! only exact for that shape.
//!
//! Reads never move the gap (`read` copies chunks); only `contiguous` does,
//! for regex search.

use std::ops::Range;

use crate::error::{CoreError, ErrorCode, reason};
use crate::limits::{MAX_BUFFER_BYTES, MAX_LINES};
use crate::ot::Edit;
use crate::pos::Point;
use crate::vendor::msedit::document::ReadableDocument;
use crate::vendor::msedit::gap_buffer::GapBuffer;
use crate::vendor::msedit::simd::lines_fwd;

/// Invariant: the bytes are valid UTF-8.
pub struct Text {
    gap: GapBuffer,
    lines: LineIndex,
}

/// `starts[0] == 0`; one entry per line; `u32` because `MAX_BUFFER_BYTES < 4 GiB`.
struct LineIndex {
    starts: Vec<u32>,
    /// Phase-1 reserved scratch for the new line starts of one insert.
    scratch: Vec<u32>,
}

/// Proof that phase 1 succeeded for exactly this sequence; consumed by `commit`.
pub struct Prepared {
    pub(crate) sequence: Vec<Edit>,
    pub peak_len: usize,
    pub peak_lines: usize,
    pub final_len: usize,
    pub final_lines: usize,
}

/// Phase-1 simulation result (pure).
pub(crate) struct Simulation {
    pub required_commit: usize,
    pub peak_len: usize,
    pub peak_lines: usize,
    pub final_len: usize,
    pub final_lines: usize,
    pub max_insert_lines: usize,
}

pub(crate) fn oom(what: &str) -> CoreError {
    CoreError::new(ErrorCode::ResourceLimit, reason::OUT_OF_MEMORY, format!("out of memory: {what}"))
}

fn too_large(len: usize) -> CoreError {
    CoreError::new(
        ErrorCode::ResourceLimit,
        reason::TOO_LARGE,
        format!("text would reach {len} bytes (limit {MAX_BUFFER_BYTES})"),
    )
    .with("limit", MAX_BUFFER_BYTES)
}

fn too_many_lines(lines: usize) -> CoreError {
    CoreError::new(
        ErrorCode::ResourceLimit,
        reason::TOO_MANY_LINES,
        format!("text would reach {lines} lines (limit {MAX_LINES})"),
    )
    .with("limit", MAX_LINES)
}

fn overflow() -> CoreError {
    CoreError::new(ErrorCode::ResourceLimit, reason::TOO_LARGE, "size arithmetic overflowed")
}

fn internal(msg: impl Into<String>) -> CoreError {
    CoreError { code: ErrorCode::Internal, reason: None, message: msg.into(), context: Default::default() }
}

/// Byte length of the UTF-8 sequence starting with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xc0 == 0x80
}

pub(crate) fn count_newlines(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

impl Text {
    /// Empty text. Fails only if the address-space reservation fails (RESOURCE_LIMIT).
    pub fn new() -> Result<Self, CoreError> {
        let gap = GapBuffer::new(false).map_err(|e| oom(&format!("address-space reservation: {e}")))?;
        Ok(Self { gap, lines: LineIndex { starts: vec![0], scratch: Vec::new() } })
    }

    /// Text from validated UTF-8 (size and line limits checked).
    pub fn from_text(text: &str) -> Result<Self, CoreError> {
        if text.len() > MAX_BUFFER_BYTES {
            return Err(too_large(text.len()));
        }
        let bytes = text.as_bytes();
        let mut starts: Vec<u32> = vec![0];
        let mut off = 0;
        while off < bytes.len() {
            let (next, found) = lines_fwd(bytes, off, 0, 1);
            if found == 0 {
                break;
            }
            if starts.len() >= MAX_LINES {
                return Err(too_many_lines(starts.len() + 1));
            }
            starts.try_reserve(1).map_err(|_| oom("line index"))?;
            starts.push(next as u32);
            off = next;
        }
        let mut t = Self::new()?;
        t.gap.replace(0..0, bytes).map_err(|e| oom(&format!("text: {e}")))?;
        t.lines.starts = starts;
        Ok(t)
    }

    pub fn len(&self) -> usize {
        self.gap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn line_count(&self) -> usize {
        self.lines.starts.len()
    }

    /// Byte offset of the start of 1-based `line`.
    pub fn line_start(&self, line: usize) -> Option<usize> {
        line.checked_sub(1).and_then(|i| self.lines.starts.get(i)).map(|&s| s as usize)
    }

    /// Byte offset of the end of 1-based `line`, excluding its `\n`.
    pub(crate) fn line_end(&self, line: usize) -> Option<usize> {
        self.line_start(line)?;
        Some(match self.lines.starts.get(line) {
            Some(&next) => next as usize - 1,
            None => self.len(),
        })
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len() {
            return None;
        }
        self.gap.read_forward(offset).first().copied()
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        match self.byte_at(offset) {
            Some(b) => !is_continuation(b),
            None => offset == self.len(),
        }
    }

    /// 0-based index of the line containing `offset`.
    fn line_index_of(&self, offset: usize) -> usize {
        self.lines.starts.partition_point(|&s| s as usize <= offset) - 1
    }

    pub fn point(&self, offset: usize) -> Point {
        let offset = offset.min(self.len());
        let li = self.line_index_of(offset);
        let start = self.lines.starts[li] as usize;
        let mut col = 1;
        self.for_each_str(start..offset, |s| {
            col += s.chars().count();
            true
        });
        Point { offset, line: li + 1, col }
    }

    /// Newlines in the byte range `[a, b)` (from the line index).
    pub(crate) fn newlines_in(&self, a: usize, b: usize) -> usize {
        let starts = &self.lines.starts;
        starts.partition_point(|&s| s as usize <= b) - starts.partition_point(|&s| s as usize <= a)
    }

    /// Line starts (as byte offsets) in `(a, b]`, in order.
    pub(crate) fn starts_in(&self, a: usize, b: usize) -> &[u32] {
        let starts = &self.lines.starts;
        let lo = starts.partition_point(|&s| s as usize <= a);
        let hi = starts.partition_point(|&s| s as usize <= b);
        &starts[lo..hi]
    }

    /// Calls `f` with consecutive `&str` pieces covering `range` (whose ends
    /// must be char boundaries), stopping early when `f` returns false. A
    /// scalar split by the gap is reassembled, so pieces are always whole.
    pub(crate) fn for_each_str(&self, range: Range<usize>, mut f: impl FnMut(&str) -> bool) {
        let end = range.end.min(self.len());
        let mut off = range.start.min(end);
        let mut carry = [0u8; 4];
        let mut carry_len = 0;
        while off < end {
            let chunk = self.gap.read_forward(off);
            let mut rest = &chunk[..chunk.len().min(end - off)];
            off += rest.len();
            if carry_len > 0 {
                let want = utf8_len(carry[0]);
                let take = (want - carry_len).min(rest.len());
                carry[carry_len..carry_len + take].copy_from_slice(&rest[..take]);
                carry_len += take;
                rest = &rest[take..];
                if carry_len < want {
                    continue;
                }
                if let Ok(s) = std::str::from_utf8(&carry[..carry_len])
                    && !f(s)
                {
                    return;
                }
            }
            let (valid, tail) = match std::str::from_utf8(rest) {
                Ok(s) => (s, &[][..]),
                Err(e) => {
                    let (v, t) = rest.split_at(e.valid_up_to());
                    (std::str::from_utf8(v).unwrap_or_default(), t)
                }
            };
            if !valid.is_empty() && !f(valid) {
                return;
            }
            let n = tail.len().min(4);
            carry[..n].copy_from_slice(&tail[..n]);
            carry_len = n;
        }
    }

    /// Appends `range` to `out` by chunked copy; never moves the gap.
    pub fn read(&self, range: Range<usize>, out: &mut String) {
        self.for_each_str(range, |s| {
            out.push_str(s);
            true
        });
    }

    /// Moves the gap to the end and returns the whole text (regex search only).
    pub fn contiguous(&mut self) -> &str {
        let len = self.gap.len();
        // A zero-length gap request never commits memory, so it cannot fail.
        let _ = self.gap.allocate_gap(len, 0, 0);
        let bytes = self.gap.read_forward(0);
        debug_assert_eq!(bytes.len(), len);
        std::str::from_utf8(bytes).unwrap_or_default()
    }

    /// The phase-1 simulation alone: validates the sequence's shape and
    /// computes its peaks. Pure.
    pub(crate) fn simulate(&self, sequence: &[Edit]) -> Result<Simulation, CoreError> {
        let mut len = self.len();
        let mut lines = self.line_count();
        let mut sim = Simulation {
            required_commit: 0,
            peak_len: len,
            peak_lines: lines,
            final_len: len,
            final_lines: lines,
            max_insert_lines: 0,
        };
        let mut prev_offset = usize::MAX;
        for e in sequence {
            let end = e.offset.checked_add(e.delete).ok_or_else(overflow)?;
            if end > prev_offset || end > self.len() {
                return Err(internal(format!(
                    "edit sequence is not canonical at offset {} (delete {})",
                    e.offset, e.delete
                )));
            }
            if !self.is_char_boundary(e.offset) || !self.is_char_boundary(end) {
                return Err(internal(format!("edit [{}, {end}) is not on char boundaries", e.offset)));
            }
            prev_offset = e.offset;
            let after_delete = len.checked_sub(e.delete).ok_or_else(overflow)?;
            if !e.insert.is_empty() {
                let need = GapBuffer::commit_needed(after_delete, e.insert.len()).ok_or_else(overflow)?;
                sim.required_commit = sim.required_commit.max(need);
            }
            len = after_delete.checked_add(e.insert.len()).ok_or_else(overflow)?;
            let ins_lines = count_newlines(&e.insert);
            sim.max_insert_lines = sim.max_insert_lines.max(ins_lines);
            lines = lines
                .checked_sub(self.newlines_in(e.offset, end))
                .and_then(|l| l.checked_add(ins_lines))
                .ok_or_else(overflow)?;
            sim.peak_len = sim.peak_len.max(len);
            sim.peak_lines = sim.peak_lines.max(lines);
        }
        sim.final_len = len;
        sim.final_lines = lines;
        if sim.peak_len > MAX_BUFFER_BYTES {
            return Err(too_large(sim.peak_len));
        }
        if sim.peak_lines > MAX_LINES {
            return Err(too_many_lines(sim.peak_lines));
        }
        Ok(sim)
    }

    /// Phase 1 (see module docs). Mutates nothing observable.
    pub fn prepare(&mut self, sequence: Vec<Edit>) -> Result<Prepared, CoreError> {
        let sim = self.simulate(&sequence)?;
        self.gap
            .ensure_commit(sim.required_commit)
            .map_err(|e| oom(&format!("text commit of {} bytes: {e}", sim.required_commit)))?;
        let starts = &mut self.lines.starts;
        starts
            .try_reserve(sim.peak_lines.saturating_sub(starts.len()))
            .map_err(|_| oom("line index"))?;
        let scratch = &mut self.lines.scratch;
        scratch.clear();
        scratch.try_reserve(sim.max_insert_lines).map_err(|_| oom("line scratch"))?;
        Ok(Prepared {
            sequence,
            peak_len: sim.peak_len,
            peak_lines: sim.peak_lines,
            final_len: sim.final_len,
            final_lines: sim.final_lines,
        })
    }

    /// Phase 2 (see module docs). Cannot fail.
    pub fn commit(&mut self, prepared: Prepared) {
        let commits_before = self.gap.commit_calls();
        for e in &prepared.sequence {
            let end = e.offset + e.delete;
            let replaced = self.gap.replace(e.offset..end, e.insert.as_bytes());
            debug_assert!(replaced.is_ok(), "phase 1 committed the memory");
            self.splice_lines(e.offset, end, &e.insert);
        }
        debug_assert_eq!(self.gap.commit_calls(), commits_before, "phase 2 committed memory");
        debug_assert_eq!(self.len(), prepared.final_len);
        debug_assert_eq!(self.line_count(), prepared.final_lines);
    }

    /// Splices the line index for one applied edit, in place: capacity was
    /// reserved in phase 1, so nothing here allocates.
    fn splice_lines(&mut self, offset: usize, end: usize, insert: &str) {
        let LineIndex { starts, scratch } = &mut self.lines;
        scratch.clear();
        for (i, b) in insert.bytes().enumerate() {
            if b == b'\n' {
                debug_assert!(scratch.len() < scratch.capacity());
                scratch.push((offset + i + 1) as u32);
            }
        }
        let lo = starts.partition_point(|&s| s as usize <= offset);
        let hi = starts.partition_point(|&s| s as usize <= end);
        let old_len = starts.len();
        let removed = hi - lo;
        let added = scratch.len();
        let new_len = old_len - removed + added;
        debug_assert!(new_len <= starts.capacity());
        if added > removed {
            starts.resize(new_len, 0);
            starts.copy_within(hi..old_len, lo + added);
        } else {
            starts.copy_within(hi..old_len, lo + added);
            starts.truncate(new_len);
        }
        starts[lo..lo + added].copy_from_slice(scratch);
        let delete = end - offset;
        let ins = insert.len();
        for s in &mut starts[lo + added..] {
            *s = (*s as usize - delete + ins) as u32;
        }
    }

    /// Test/debug: memory-commit attempts so far (phase 2 must add none).
    pub fn commit_calls(&self) -> u64 {
        self.gap.commit_calls()
    }

    #[cfg(test)]
    pub(crate) fn line_index(&self) -> &[u32] {
        &self.lines.starts
    }

    #[cfg(test)]
    pub(crate) fn line_index_capacity(&self) -> usize {
        self.lines.starts.capacity()
    }

    /// Whole text as a `String` (tests and snapshots).
    pub(crate) fn to_string_lossless(&self) -> String {
        let mut out = String::with_capacity(self.len());
        self.read(0..self.len(), &mut out);
        out
    }

    /// Byte offset `col - 1` scalars after the start of `line` (1-based),
    /// not crossing the line's `\n`.
    pub(crate) fn line_col_offset(&self, line: usize, col: usize) -> Option<usize> {
        let start = self.line_start(line)?;
        let end = self.line_end(line)?;
        if col == 0 {
            return None;
        }
        let mut remaining = col - 1;
        let mut off = start;
        if remaining == 0 {
            return Some(start);
        }
        self.for_each_str(start..end, |s| {
            for (i, _) in s.char_indices() {
                if remaining == 0 {
                    off += i;
                    return false;
                }
                remaining -= 1;
            }
            off += s.len();
            remaining > 0
        });
        (remaining == 0).then_some(off)
    }
}
