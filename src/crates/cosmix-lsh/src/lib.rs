//! Syntax highlighting for the Cosmix editor (ced): msedit's `lsh` — a small
//! compiler from `definitions/*.lsh` to bytecode plus a line-oriented runtime
//! — vendored from microsoft/edit@826b4c0 (MIT; `vendor/README.md`), with a
//! chunk-source adapter so any text store can be highlighted.
//!
//! Contracts (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §1.2,
//! frozen in Stage S; bodies land in Stage E1b):
//! - [`defs`] is **pre-generated and committed** (`examples/gen.rs` writes it,
//!   `tests/defs_fresh.rs` proves it is current). No build script.
//! - [`highlighter::Highlighter`] reads a [`LineSource`] line by line; lines
//!   longer than [`highlighter::MAX_LINE_LEN`] (32 KiB) are left unhighlighted
//!   (upstream behaviour).
//! - [`cache::Cache`] keeps a runtime checkpoint every [`cache::INTERVAL`]
//!   (1024, pinned in every profile — upstream used 16 under
//!   `debug_assertions`) so random seeks cost at most one interval.
//! - **Line numbers are 1-based** everywhere in this API, like
//!   `cosmix-edit-core` and the `edit` wire.
//! - No mutable statics anywhere in this crate or its vendored dependencies.

pub mod cache;
pub mod defs;
pub mod highlighter;

/// The vendored runtime types (`Language`, `Highlight`).
pub use lsh::runtime;

/// A text store the highlighter can read from: returns the contiguous bytes
/// starting at `offset` up to some chunk boundary (empty at/after the end).
/// A line may span several chunks; the highlighter assembles it. Unlike
/// `cosmix-edit-core`'s measurement adapter, chunks may split grapheme
/// clusters — lsh works on bytes.
pub trait LineSource {
    fn read_forward(&self, offset: usize) -> &[u8];
}

impl LineSource for &[u8] {
    fn read_forward(&self, offset: usize) -> &[u8] {
        self.get(offset..).unwrap_or(&[])
    }
}

impl LineSource for str {
    fn read_forward(&self, offset: usize) -> &[u8] {
        self.as_bytes().get(offset..).unwrap_or(&[])
    }
}
