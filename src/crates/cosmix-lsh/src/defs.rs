//! Compiled lsh definitions: bytecode, strings, charsets, the language table
//! and file associations.
//!
//! **Stage S placeholder.** Stage E1b replaces this whole file with the output
//! of `examples/gen.rs` (the generator msedit runs from its build script,
//! `crates/edit/build/main.rs`), committed, and proven current by
//! `tests/defs_fresh.rs`. The item names below are the frozen consumer surface;
//! their contents (and `HighlightKind`'s variants) come from the generator.

use lsh::runtime::Language;

/// One highlight class per lsh `yield` kind (generated; placeholder variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Other = 0,
}

impl TryFrom<u32> for HighlightKind {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Other),
            _ => Err(()),
        }
    }
}

/// Every compiled language (`id`, display `name`, bytecode `entrypoint`).
pub static LANGUAGES: &[Language] = &[];
/// `(path glob, language)` in priority order (upstream `#[path = …]` attributes).
pub static FILE_ASSOCIATIONS: &[(&str, &Language)] = &[];
/// The bytecode.
pub static ASSEMBLY: [u8; 0] = [];
/// Interned strings referenced by the bytecode.
pub static STRINGS: [&str; 0] = [];
/// Transposed 256-bit charsets referenced by the bytecode.
pub static CHARSETS: [[u16; 16]; 0] = [];
