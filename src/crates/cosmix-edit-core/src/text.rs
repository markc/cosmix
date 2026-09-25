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
//! Reads never move the gap (`read` copies chunks); only `contiguous` does,
//! for regex search.

use std::ops::Range;

use crate::error::CoreError;
use crate::ot::Edit;
use crate::pos::Point;
use crate::vendor::msedit::gap_buffer::GapBuffer;

/// Invariant: the bytes are valid UTF-8.
pub struct Text {
    gap: GapBuffer,
    lines: LineIndex,
}

/// `starts[0] == 0`; one entry per line; `u32` because `MAX_BUFFER_BYTES < 4 GiB`.
#[allow(dead_code)] // Stage S stub; E0a uses every field.
struct LineIndex {
    starts: Vec<u32>,
    /// Phase-1 reserved scratch for the new line starts of one insert.
    scratch: Vec<u32>,
}

/// Proof that phase 1 succeeded for exactly this sequence; consumed by `commit`.
pub struct Prepared {
    #[allow(dead_code)] // Stage S stub; E0a consumes it in `commit`.
    pub(crate) sequence: Vec<Edit>,
    pub peak_len: usize,
    pub peak_lines: usize,
    pub final_len: usize,
    pub final_lines: usize,
}

impl Text {
    /// Empty text. Fails only if the address-space reservation fails (RESOURCE_LIMIT).
    pub fn new() -> Result<Self, CoreError> {
        todo!("E0a")
    }

    /// Text from validated UTF-8 (size and line limits checked).
    pub fn from_text(text: &str) -> Result<Self, CoreError> {
        let _ = text;
        todo!("E0a")
    }

    pub fn len(&self) -> usize {
        todo!("E0a")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn line_count(&self) -> usize {
        todo!("E0a")
    }

    /// Byte offset of the start of 1-based `line`.
    pub fn line_start(&self, line: usize) -> Option<usize> {
        let _ = line;
        todo!("E0a")
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        let _ = offset;
        todo!("E0a")
    }

    pub fn point(&self, offset: usize) -> Point {
        let _ = offset;
        todo!("E0a")
    }

    /// Appends `range` to `out` by chunked copy; never moves the gap.
    pub fn read(&self, range: Range<usize>, out: &mut String) {
        let _ = (range, out);
        todo!("E0a")
    }

    /// Moves the gap to the end and returns the whole text (regex search only).
    pub fn contiguous(&mut self) -> &str {
        todo!("E0a")
    }

    /// Phase 1 (see module docs). Mutates nothing.
    pub fn prepare(&mut self, sequence: Vec<Edit>) -> Result<Prepared, CoreError> {
        let _ = sequence;
        todo!("E0a")
    }

    /// Phase 2 (see module docs). Cannot fail.
    pub fn commit(&mut self, prepared: Prepared) {
        let _ = prepared;
        todo!("E0a")
    }

    /// Test/debug: memory-commit attempts so far (phase 2 must add none).
    pub fn commit_calls(&self) -> u64 {
        self.gap.commit_calls()
    }

    #[allow(dead_code)]
    fn line_index(&self) -> &[u32] {
        &self.lines.starts
    }
}
