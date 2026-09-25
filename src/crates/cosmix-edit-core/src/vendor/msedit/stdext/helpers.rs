// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-edit-core from microsoft/edit@826b4c0 crates/stdext/src/helpers.rs; see vendor/msedit/README.md.
// Subset: `slice_copy_safe`, `ReplaceRange` + its `Vec` impl and the private
// `vec_replace_impl` it calls. Bodies unchanged.

use std::ops::{Bound, Range, RangeBounds};
use std::ptr;

/// [`<[T]>::copy_from_slice`] panics if the two slices have different lengths.
/// This one just returns the copied amount.
pub fn slice_copy_safe<T: Copy>(dst: &mut [T], src: &[T]) -> usize {
    let len = src.len().min(dst.len());
    unsafe { ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), len) };
    len
}

/// [`Vec::splice`] results in really bad assembly.
/// This doesn't. Don't use [`Vec::splice`].
pub trait ReplaceRange<T: Copy> {
    fn replace_range<R: RangeBounds<usize>>(&mut self, range: R, src: &[T]);
}

impl<T: Copy> ReplaceRange<T> for Vec<T> {
    fn replace_range<R: RangeBounds<usize>>(&mut self, range: R, src: &[T]) {
        let start = match range.start_bound() {
            Bound::Included(&start) => start,
            Bound::Excluded(start) => start + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(end) => end + 1,
            Bound::Excluded(&end) => end,
            Bound::Unbounded => usize::MAX,
        };
        vec_replace_impl(self, start..end, src);
    }
}

fn vec_replace_impl<T: Copy>(dst: &mut Vec<T>, range: Range<usize>, src: &[T]) {
    unsafe {
        let dst_len = dst.len();
        let src_len = src.len();
        let off = range.start.min(dst_len);
        let del_len = range.end.saturating_sub(off).min(dst_len - off);

        if del_len == 0 && src_len == 0 {
            return; // nothing to do
        }

        let tail_len = dst_len - off - del_len;
        let new_len = dst_len - del_len + src_len;

        if src_len > del_len {
            dst.reserve(src_len - del_len);
        }

        // NOTE: drop_in_place() is not needed here, because T is constrained to Copy.

        // SAFETY: as_mut_ptr() must called after reserve() to ensure that the pointer is valid.
        let ptr = dst.as_mut_ptr().add(off);

        // Shift the tail.
        if tail_len > 0 && src_len != del_len {
            ptr::copy(ptr.add(del_len), ptr.add(src_len), tail_len);
        }

        // Copy in the replacement.
        ptr::copy_nonoverlapping(src.as_ptr(), ptr, src_len);
        dst.set_len(new_len);
    }
}
