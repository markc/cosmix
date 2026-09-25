//! Diagnostics for one buffer view — frontend lint results (ced E1 plan
//! §4.10). Stage S freezes the API; Stage E1e implements it.
//!
//! Contracts:
//! - Input is `mix lint --json -` output (`schema_version` 2:
//!   `{diagnostics:[{file, line, column, code, severity: error|warning|note,
//!   message, hint}]}`) run on CAPTURED bytes at a tagged gen; any other
//!   schema_version is refused.
//! - Results for another epoch, buffer, language or `cfg` are dropped.
//! - Covered-range invalidation: a diagnostic covers its whole source line at
//!   the tagged gen; if any delta since touched that range it is DROPPED, not
//!   mapped. Diagnostics on untouched lines map through the deltas.

use std::ops::Range;

use crate::highlight::ResultTag;
use crate::types::ViewDelta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// View byte range the squiggle covers.
    pub range: Range<usize>,
    /// 1-based line it was reported on (at the tagged gen).
    pub line: usize,
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagError {
    BadJson(String),
    UnsupportedSchema(u64),
    StaleTag,
}

#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    _private: (),
}

impl Diagnostics {
    /// Replace the set with a lint result for `tag`. `deltas_since` are the
    /// view deltas after `tag.gen`, in order (for covered-range invalidation).
    pub fn accept(
        &mut self,
        current: &ResultTag,
        tag: ResultTag,
        text_at_tag: &str,
        lint_json: &str,
        deltas_since: &[ViewDelta],
    ) -> Result<(), DiagError> {
        let _ = (current, tag, text_at_tag, lint_json, deltas_since);
        todo!("ced E1e")
    }

    /// Drop diagnostics whose covered line the delta touched; map the rest.
    pub fn apply_delta(&mut self, d: &ViewDelta) {
        let _ = d;
        todo!("ced E1e")
    }

    pub fn items(&self) -> &[Diagnostic] {
        todo!("ced E1e")
    }
}
