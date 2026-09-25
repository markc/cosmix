//! Highlighting state for one buffer view (ced E1 plan §4.3). Stage S
//! freezes the API; Stage E1e implements it.
//!
//! Contracts:
//! - lsh languages: `cosmix_lsh::cache` checkpoints every 1024 lines over a
//!   `LineSource` built on `Text::chunk_at`; a cold seek far past the last
//!   checkpoint advances by at most [`SliceBudget`] per frame — `spans`
//!   returns `None` for a line not yet reached (drawn plain).
//! - Mix (`mix`, `scene`, `mix-data`; feature `mix`): whole-buffer relex off
//!   the UI thread through `cosmix_lib_mix::lexer::highlight`, requested
//!   150 ms after the last delta (the caller debounces and runs the worker);
//!   buffers over [`MIX_MAX_BYTES`] stay plain.
//! - Results carry a [`ResultTag`]. A result for another epoch, buffer,
//!   language or config is discarded. A result for an OLDER `gen` is used only
//!   for lines before the first line touched since that gen; lines from there
//!   on draw plain until a fresh result lands (a quote or comment delimiter
//!   must never leave stale tokens).

use std::path::Path;
use std::sync::Arc;

use cosmix_edit_core::text::Text;

use crate::types::ViewDelta;

/// Mix buffers larger than this are not highlighted.
pub const MIX_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Colour classes; ced maps each to a design token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlClass {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Type,
    Function,
    Variable,
    Operator,
    Punctuation,
    Meta,
    Inserted,
    Deleted,
    Heading,
    Link,
    Invalid,
}

/// Identity of an asynchronous result (highlight or lint).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResultTag {
    pub epoch: String,
    pub buffer: String,
    pub view_gen: u64,
    pub language: String,
    /// Hash of the settings the result depends on.
    pub cfg: u64,
}

/// Work allowed in one frame for cold lsh seeks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceBudget {
    pub max_lines: usize,
}

impl Default for SliceBudget {
    /// Roughly 2 ms of lsh work.
    fn default() -> Self {
        Self { max_lines: 2000 }
    }
}

pub struct Highlight {
    _private: (),
}

impl Highlight {
    /// Pick the highlighter from editd's `language` first, lsh
    /// `FILE_ASSOCIATIONS` for `path` second, else plain.
    pub fn for_language(editd_language: &str, path: Option<&Path>) -> Self {
        let _ = (editd_language, path);
        todo!("ced E1e")
    }

    /// Invalidate from the first line the delta touched.
    pub fn apply_delta(&mut self, text: &Text, d: &ViewDelta) {
        let _ = (text, d);
        todo!("ced E1e")
    }

    /// Spans of 1-based `line` (byte ranges in the text), or `None` while a
    /// cold seek has not reached it yet.
    pub fn spans(&mut self, text: &Text, line: usize, budget: &mut SliceBudget) -> Option<&[(std::ops::Range<usize>, HlClass)]> {
        let _ = (text, line, budget);
        todo!("ced E1e")
    }

    /// True while a cold seek is behind the visible range (the widget keeps
    /// requesting frames only while this holds).
    pub fn behind(&self) -> bool {
        todo!("ced E1e")
    }

    /// For Mix buffers: the relex to run now (tag + the text at that gen),
    /// once the 150 ms debounce has passed. `None` when nothing is due.
    pub fn mix_request(&mut self, text: &Text, tag: ResultTag) -> Option<(ResultTag, Arc<str>)> {
        let _ = (text, tag);
        todo!("ced E1e")
    }

    /// A relex result from the worker (see the tag rules in the module docs).
    #[cfg(feature = "mix")]
    pub fn mix_result(&mut self, tag: ResultTag, spans: Vec<(std::ops::Range<usize>, cosmix_mix::lexer::TokenClass)>) {
        let _ = (tag, spans);
        todo!("ced E1e")
    }
}
