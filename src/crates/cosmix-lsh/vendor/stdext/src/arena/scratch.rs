// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/arena/scratch.rs; see cosmix-lsh/vendor/README.md.

use std::io;
#[cfg(debug_assertions)]
use std::marker::PhantomData;
use std::ops::Deref;

#[cfg(debug_assertions)]
use super::debug;
use super::{Arena, release};
use crate::helpers::*;

/// Borrows an [`Arena`] for temporary allocations.
///
/// See [`scratch_arena`].
#[cfg(debug_assertions)]
pub struct ScratchArena<'a> {
    arena: debug::Arena,
    offset: usize,
    _phantom: PhantomData<&'a ()>,
}

#[cfg(not(debug_assertions))]
pub struct ScratchArena<'a> {
    arena: &'a Arena,
    offset: usize,
}

#[cfg(debug_assertions)]
impl<'a> ScratchArena<'a> {
    fn new(arena: &'a release::Arena) -> Self {
        let offset = arena.offset();
        ScratchArena { arena: Arena::delegated(arena), _phantom: PhantomData, offset }
    }
}

#[cfg(not(debug_assertions))]
impl<'a> ScratchArena<'a> {
    fn new(arena: &'a release::Arena) -> Self {
        let offset = arena.offset();
        ScratchArena { arena, offset }
    }
}

impl Drop for ScratchArena<'_> {
    fn drop(&mut self) {
        unsafe { self.arena.reset(self.offset) };
    }
}

#[cfg(debug_assertions)]
impl Deref for ScratchArena<'_> {
    type Target = debug::Arena;

    fn deref(&self) -> &Self::Target {
        &self.arena
    }
}

#[cfg(not(debug_assertions))]
impl Deref for ScratchArena<'_> {
    type Target = Arena;

    fn deref(&self) -> &Self::Target {
        self.arena
    }
}

// cosmix patch (ced E1 plan §1.2(1)): upstream's `mod single_threaded` kept the
// scratch arenas in a mutable static (behind the `single-threaded` feature). It
// is deleted; only the thread-local variant below exists.

mod multi_threaded {
    use std::cell::Cell;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    thread_local! {
        static S_SCRATCH: [Cell<release::Arena>; 2] =
            const { [Cell::new(release::Arena::empty()), Cell::new(release::Arena::empty())] };
    }

    static INIT_SIZE: AtomicUsize = AtomicUsize::new(128 * MEBI);

    /// Sets the default scratch arena size.
    #[allow(dead_code)]
    pub fn init(capacity: usize) -> io::Result<()> {
        if capacity != 0 {
            INIT_SIZE.store(capacity, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Need an arena for temporary allocations? [`scratch_arena`] got you covered.
    /// It returns an [`Arena`] that resets when it goes out of scope.
    ///
    /// # Safety
    ///
    /// If your function takes an [`Arena`] argument, you **MUST** pass it to `scratch_arena` as `Some(&arena)`.
    #[allow(dead_code)]
    pub fn scratch_arena(conflict: Option<&Arena>) -> ScratchArena<'static> {
        #[cfg(debug_assertions)]
        let conflict = conflict.map(|a| a.delegate_target_unchecked());

        #[cold]
        fn init(s: &[Cell<release::Arena>; 2]) {
            let capacity = INIT_SIZE.load(Ordering::Relaxed);
            for s in s {
                s.set(release::Arena::new(capacity).unwrap());
            }
        }

        S_SCRATCH.with(|arenas| {
            let index = ptr::eq(opt_ptr(conflict), arenas[0].as_ptr()) as usize;
            let arena = unsafe { &*arenas[index].as_ptr() };
            if arena.is_empty() {
                init(arenas);
            }
            ScratchArena::new(arena)
        })
    }
}

pub use multi_threaded::*;
