// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/document.rs; see vendor/msedit/README.md.
// Subset: the two traits and the `&[u8]` / `String` impls (the `PathBuf`
// impls are not vendored).

//! Abstractions over reading/writing arbitrary text containers.

use std::ops::Range;

use super::stdext::helpers::ReplaceRange as _;

/// An abstraction over reading from text containers.
pub trait ReadableDocument {
    /// Read some bytes starting at (including) the given absolute offset.
    ///
    /// # Warning
    ///
    /// * Be lenient on inputs:
    ///   * The given offset may be out of bounds and you MUST clamp it.
    ///   * You should not assume that offsets are at grapheme cluster boundaries.
    /// * Be strict on outputs:
    ///   * You MUST NOT break grapheme clusters across chunks.
    ///   * You MUST NOT return an empty slice unless the offset is at or beyond the end.
    ///
    /// Cosmix note: `GapBuffer` does NOT honour the grapheme promise (its chunk
    /// boundary is wherever the gap is). Nothing in cosmix-edit-core E0 relies on
    /// it; the E1 measurement code must add an adapter first (ced E0 plan §9.3).
    fn read_forward(&self, off: usize) -> &[u8];

    /// Read some bytes before (but not including) the given absolute offset.
    ///
    /// # Warning
    ///
    /// * Be lenient on inputs:
    ///   * The given offset may be out of bounds and you MUST clamp it.
    ///   * You should not assume that offsets are at grapheme cluster boundaries.
    /// * Be strict on outputs:
    ///   * You MUST NOT break grapheme clusters across chunks.
    ///   * You MUST NOT return an empty slice unless the offset is zero.
    fn read_backward(&self, off: usize) -> &[u8];
}

/// An abstraction over writing to text containers.
pub trait WriteableDocument: ReadableDocument {
    /// Replace the given range with the given bytes.
    ///
    /// # Warning
    ///
    /// * The given range may be out of bounds and you MUST clamp it.
    /// * The replacement may not be valid UTF8.
    fn replace(&mut self, range: Range<usize>, replacement: &[u8]);
}

impl ReadableDocument for &[u8] {
    fn read_forward(&self, off: usize) -> &[u8] {
        let s = *self;
        &s[off.min(s.len())..]
    }

    fn read_backward(&self, off: usize) -> &[u8] {
        let s = *self;
        &s[..off.min(s.len())]
    }
}

impl ReadableDocument for String {
    fn read_forward(&self, off: usize) -> &[u8] {
        let s = self.as_bytes();
        &s[off.min(s.len())..]
    }

    fn read_backward(&self, off: usize) -> &[u8] {
        let s = self.as_bytes();
        &s[..off.min(s.len())]
    }
}

impl WriteableDocument for String {
    fn replace(&mut self, range: Range<usize>, replacement: &[u8]) {
        // `replacement` is not guaranteed to be valid UTF-8, so we need to sanitize it.
        let utf8 = String::from_utf8_lossy(replacement);
        // SAFETY: `range` is guaranteed to be on codepoint boundaries.
        unsafe { self.as_mut_vec() }.replace_range(range, utf8.as_bytes());
    }
}
