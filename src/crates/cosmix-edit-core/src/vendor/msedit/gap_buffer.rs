// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/edit/src/buffer/gap_buffer.rs; see vendor/msedit/README.md.
// Patched (ced E0 plan §2.2(3)): memory commit is fallible and all-or-nothing.
// Upstream deleted text before a silently-failing enlargement; here
// `allocate_gap` commits first and returns `Err` with the buffer untouched,
// `ensure_commit` lets a caller commit a whole transaction's peak up front,
// and `commit_calls` counts `virtual_commit` calls so phase 2 of a
// transaction can assert it made none.

use std::ops::Range;
use std::ptr::{self, NonNull};
use std::{io, slice};

use super::document::{ReadableDocument, WriteableDocument};
use super::helpers::*;
use super::stdext::helpers::{ReplaceRange as _, slice_copy_safe};
use super::stdext::sys_unix::{virtual_commit, virtual_release, virtual_reserve};

#[cfg(target_pointer_width = "32")]
const LARGE_CAPACITY: usize = 128 * MEBI;
#[cfg(target_pointer_width = "64")]
const LARGE_CAPACITY: usize = 4 * GIBI;
pub const LARGE_ALLOC_CHUNK: usize = 64 * KIBI;
pub const LARGE_GAP_CHUNK: usize = 4 * KIBI;

const SMALL_CAPACITY: usize = 128 * KIBI;
const SMALL_ALLOC_CHUNK: usize = 256;
const SMALL_GAP_CHUNK: usize = 16;

// TODO: Instead of having a specialization for small buffers here,
// tui.rs could also just keep a MRU set of large buffers around.
enum BackingBuffer {
    VirtualMemory(NonNull<u8>, usize),
    Vec(Vec<u8>),
}

impl Drop for BackingBuffer {
    fn drop(&mut self) {
        unsafe {
            if let Self::VirtualMemory(ptr, reserve) = *self {
                virtual_release(ptr, reserve);
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only failure injection: while set, every commit attempt fails.
    static FAIL_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: make every subsequent memory commit on this thread fail.
#[cfg(test)]
pub fn inject_commit_failure(fail: bool) {
    FAIL_COMMIT.with(|f| f.set(fail));
}

fn out_of_reserve() -> io::Error {
    io::Error::new(io::ErrorKind::OutOfMemory, "gap buffer reserve exhausted")
}

/// Most people know how `Vec<T>` works: It has some spare capacity at the end,
/// so that pushing into it doesn't reallocate every single time. A gap buffer
/// is the same thing, but the spare capacity can be anywhere in the buffer.
/// This variant is optimized for large buffers and uses virtual memory.
pub struct GapBuffer {
    /// Pointer to the buffer.
    text: NonNull<u8>,
    /// Maximum size of the buffer, including gap.
    reserve: usize,
    /// Size of the buffer, including gap.
    commit: usize,
    /// Length of the stored text, NOT including gap.
    text_length: usize,
    /// Gap offset.
    gap_off: usize,
    /// Gap length.
    gap_len: usize,
    /// Increments every time the buffer is modified.
    generation: u32,
    /// Number of successful or attempted memory commits (cosmix addition).
    commit_calls: u64,
    /// If `Vec(..)`, the buffer is optimized for small amounts of text
    /// and uses the standard heap. Otherwise, it uses virtual memory.
    buffer: BackingBuffer,
}

impl GapBuffer {
    pub fn new(small: bool) -> io::Result<Self> {
        let reserve;
        let buffer;
        let text;

        if small {
            reserve = SMALL_CAPACITY;
            text = NonNull::dangling();
            buffer = BackingBuffer::Vec(Vec::new());
        } else {
            reserve = LARGE_CAPACITY;
            text = unsafe { virtual_reserve(reserve)? };
            buffer = BackingBuffer::VirtualMemory(text, reserve);
        }

        Ok(Self {
            text,
            reserve,
            commit: 0,
            text_length: 0,
            gap_off: 0,
            gap_len: 0,
            generation: 0,
            commit_calls: 0,
            buffer,
        })
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.text_length
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn set_generation(&mut self, generation: u32) {
        self.generation = generation;
    }

    /// Bytes of backing memory currently committed (text + gap).
    pub fn committed(&self) -> usize {
        self.commit
    }

    /// Number of memory-commit attempts made so far.
    pub fn commit_calls(&self) -> u64 {
        self.commit_calls
    }

    fn chunks(&self) -> (usize, usize) {
        if matches!(self.buffer, BackingBuffer::VirtualMemory(..)) {
            (LARGE_GAP_CHUNK, LARGE_ALLOC_CHUNK)
        } else {
            (SMALL_GAP_CHUNK, SMALL_ALLOC_CHUNK)
        }
    }

    /// The committed size a gap of at least `len` bytes needs when the text is
    /// `text_length` bytes long — exactly what `enlarge_gap` would require.
    pub fn commit_needed(text_length: usize, len: usize) -> Option<usize> {
        let gap_len_new = len.checked_add(2 * LARGE_GAP_CHUNK - 1)? & !(LARGE_GAP_CHUNK - 1);
        let bytes = text_length.checked_add(gap_len_new)?;
        Some(bytes.checked_add(LARGE_ALLOC_CHUNK - 1)? & !(LARGE_ALLOC_CHUNK - 1))
    }

    /// Commits backing memory up to `required_commit` bytes (never releases).
    /// Leaves the text untouched; on error nothing has changed.
    pub fn ensure_commit(&mut self, required_commit: usize) -> io::Result<()> {
        if required_commit <= self.commit {
            return Ok(());
        }
        let (_, alloc_chunk) = self.chunks();
        let bytes_new = required_commit
            .checked_add(alloc_chunk - 1)
            .ok_or_else(out_of_reserve)?
            & !(alloc_chunk - 1);
        if bytes_new > self.reserve {
            return Err(out_of_reserve());
        }
        self.commit_to(bytes_new)
    }

    fn commit_to(&mut self, bytes_new: usize) -> io::Result<()> {
        let bytes_old = self.commit;
        self.commit_calls += 1;
        #[cfg(test)]
        if FAIL_COMMIT.with(|f| f.get()) {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "injected commit failure"));
        }
        match &mut self.buffer {
            BackingBuffer::VirtualMemory(ptr, _) => unsafe {
                virtual_commit(ptr.add(bytes_old), bytes_new - bytes_old)?;
            },
            BackingBuffer::Vec(v) => {
                v.try_reserve_exact(bytes_new.saturating_sub(v.len()))
                    .map_err(|_| out_of_reserve())?;
                v.resize(bytes_new, 0);
                self.text = unsafe { NonNull::new_unchecked(v.as_mut_ptr()) };
            }
        }
        self.commit = bytes_new;
        Ok(())
    }

    /// Opens a gap of at least `len` bytes at `off`, deleting `delete` bytes
    /// after it. All-or-nothing: any memory the gap needs is committed BEFORE
    /// the gap moves or text is deleted, so an `Err` leaves the buffer exactly
    /// as it was (cosmix patch; upstream returned a short slice after deleting).
    pub fn allocate_gap(&mut self, off: usize, len: usize, delete: usize) -> io::Result<&mut [u8]> {
        // Sanitize parameters
        let off = off.min(self.text_length);
        let delete = delete.min(self.text_length - off);

        // Commit first, while nothing has been touched.
        let gap_after_delete = self.gap_len + delete;
        if len > gap_after_delete {
            let (gap_chunk, alloc_chunk) = self.chunks();
            let gap_len_new = (len + gap_chunk + gap_chunk - 1) & !(gap_chunk - 1);
            let bytes_new = self.text_length - delete + gap_len_new;
            if bytes_new > self.commit {
                let bytes_new = (bytes_new + alloc_chunk - 1) & !(alloc_chunk - 1);
                if bytes_new > self.reserve {
                    return Err(out_of_reserve());
                }
                self.commit_to(bytes_new)?;
            }
        }

        // Move the existing gap if it exists
        if off != self.gap_off {
            self.move_gap(off);
        }

        // Delete the text
        if delete > 0 {
            self.delete_text(delete);
        }

        // Enlarge the gap if needed (memory is already committed).
        if len > self.gap_len {
            self.enlarge_gap(len);
        }

        self.generation = self.generation.wrapping_add(1);
        Ok(unsafe { slice::from_raw_parts_mut(self.text.add(self.gap_off).as_ptr(), self.gap_len) })
    }

    fn move_gap(&mut self, off: usize) {
        if self.gap_len > 0 {
            //
            //                       v gap_off
            // left:  |ABCDEFGHIJKLMN   OPQRSTUVWXYZ|
            //        |ABCDEFGHI   JKLMNOPQRSTUVWXYZ|
            //                  ^ off
            //        move: JKLMN
            //
            //                       v gap_off
            // !left: |ABCDEFGHIJKLMN   OPQRSTUVWXYZ|
            //        |ABCDEFGHIJKLMNOPQRS   TUVWXYZ|
            //                            ^ off
            //        move: OPQRS
            //
            let left = off < self.gap_off;
            let move_src = if left { off } else { self.gap_off + self.gap_len };
            let move_dst = if left { off + self.gap_len } else { self.gap_off };
            let move_len = if left { self.gap_off - off } else { off - self.gap_off };

            unsafe { self.text.add(move_src).copy_to(self.text.add(move_dst), move_len) };

            if cfg!(debug_assertions) {
                // Fill the moved-out bytes with 0xCD to make debugging easier.
                unsafe { self.text.add(off).write_bytes(0xCD, self.gap_len) };
            }
        }

        self.gap_off = off;
    }

    fn delete_text(&mut self, delete: usize) {
        if cfg!(debug_assertions) {
            // Fill the deleted bytes with 0xCD to make debugging easier.
            unsafe { self.text.add(self.gap_off + self.gap_len).write_bytes(0xCD, delete) };
        }

        self.gap_len += delete;
        self.text_length -= delete;
    }

    /// Grows the gap to at least `len` bytes. The caller (`allocate_gap`) has
    /// already committed the memory this needs, so this only moves bytes.
    fn enlarge_gap(&mut self, len: usize) {
        let (gap_chunk, _) = self.chunks();

        let gap_len_old = self.gap_len;
        let gap_len_new = (len + gap_chunk + gap_chunk - 1) & !(gap_chunk - 1);
        debug_assert!(self.text_length + gap_len_new <= self.commit, "enlarge_gap without commit");

        let gap_beg = unsafe { self.text.add(self.gap_off) };
        unsafe {
            ptr::copy(
                gap_beg.add(gap_len_old).as_ptr(),
                gap_beg.add(gap_len_new).as_ptr(),
                self.text_length - self.gap_off,
            )
        };

        if cfg!(debug_assertions) {
            // Fill the moved-out bytes with 0xCD to make debugging easier.
            unsafe { gap_beg.add(gap_len_old).write_bytes(0xCD, gap_len_new - gap_len_old) };
        }

        self.gap_len = gap_len_new;
    }

    pub fn commit_gap(&mut self, len: usize) {
        assert!(len <= self.gap_len);
        self.text_length += len;
        self.gap_off += len;
        self.gap_len -= len;
    }

    /// All-or-nothing replace (cosmix patch: returns `Err` untouched on OOM).
    pub fn replace(&mut self, range: Range<usize>, src: &[u8]) -> io::Result<()> {
        let gap = self.allocate_gap(range.start, src.len(), range.end.saturating_sub(range.start))?;
        let len = slice_copy_safe(gap, src);
        self.commit_gap(len);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.gap_off = 0;
        self.gap_len += self.text_length;
        self.generation = self.generation.wrapping_add(1);
        self.text_length = 0;
    }

    pub fn extract_raw(&self, range: Range<usize>, out: &mut Vec<u8>, mut out_off: usize) {
        let end = range.end.min(self.text_length);
        let mut beg = range.start.min(end);
        out_off = out_off.min(out.len());

        if beg >= end {
            return;
        }

        out.reserve(end - beg);

        while beg < end {
            let chunk = self.read_forward(beg);
            let chunk = &chunk[..chunk.len().min(end - beg)];
            out.replace_range(out_off..out_off, chunk);
            beg += chunk.len();
            out_off += chunk.len();
        }
    }

    /// Copies the contents of the buffer into a string.
    pub fn copy_into(&self, dst: &mut dyn WriteableDocument) {
        let mut beg = 0;
        let mut off = 0;

        while {
            let chunk = self.read_forward(off);

            // The first write will be 0..usize::MAX and effectively clear() the destination.
            // Every subsequent write will be usize::MAX..usize::MAX and thus effectively append().
            dst.replace(beg..usize::MAX, chunk);
            beg = usize::MAX;

            off += chunk.len();
            off < self.text_length
        } {}
    }
}

impl ReadableDocument for GapBuffer {
    fn read_forward(&self, off: usize) -> &[u8] {
        let off = off.min(self.text_length);

        let (beg, len) = if off < self.gap_off {
            // Cursor is before the gap: We can read until the start of the gap.
            (off, self.gap_off - off)
        } else {
            // Cursor is after the gap: We can read until the end of the buffer.
            (off + self.gap_len, self.text_length - off)
        };

        unsafe { slice::from_raw_parts(self.text.add(beg).as_ptr(), len) }
    }

    fn read_backward(&self, off: usize) -> &[u8] {
        let off = off.min(self.text_length);

        let (beg, len) = if off <= self.gap_off {
            // Cursor is before the gap: We can read until the beginning of the buffer.
            (0, off)
        } else {
            // Cursor is after the gap: We can read until the end of the gap.
            (self.gap_off + self.gap_len, off - self.gap_off)
        };

        unsafe { slice::from_raw_parts(self.text.add(beg).as_ptr(), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(g: &GapBuffer) -> Vec<u8> {
        let mut out = Vec::new();
        g.extract_raw(0..g.len(), &mut out, 0);
        out
    }

    #[test]
    fn failed_commit_leaves_buffer_untouched() {
        let mut g = GapBuffer::new(false).unwrap();
        g.replace(0..0, b"hello world").unwrap();
        inject_commit_failure(true);
        let big = vec![b'x'; 256 * KIBI];
        let err = g.replace(0..5, &big);
        inject_commit_failure(false);
        assert!(err.is_err());
        assert_eq!(text(&g), b"hello world");
    }

    #[test]
    fn ensure_commit_then_replace_makes_no_further_commits() {
        let mut g = GapBuffer::new(false).unwrap();
        let need = GapBuffer::commit_needed(0, 200 * KIBI).unwrap();
        g.ensure_commit(need).unwrap();
        let before = g.commit_calls();
        g.replace(0..0, &vec![b'y'; 200 * KIBI]).unwrap();
        assert_eq!(g.commit_calls(), before);
    }
}
