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

use cosmix_edit_core::anchor::{Bias, map_point};
use cosmix_edit_core::ot::Edit;

use crate::highlight::ResultTag;
use crate::types::{DeltaKind, ViewDelta};

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
    items: Vec<Diagnostic>,
    /// Per item: the whole source line it covers (incl. its `\n`), view coords.
    covered: Vec<Range<usize>>,
}

#[derive(serde::Deserialize)]
struct Report {
    schema_version: u64,
    #[serde(default)]
    diagnostics: Vec<RawDiag>,
}

#[derive(serde::Deserialize)]
struct RawDiag {
    #[serde(default)]
    code: String,
    severity: String,
    line: Option<usize>,
    column: Option<usize>,
    #[serde(default)]
    message: String,
    hint: Option<String>,
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
        let same = current.epoch == tag.epoch && current.buffer == tag.buffer && current.language == tag.language && current.cfg == tag.cfg;
        if !same || tag.view_gen > current.view_gen {
            return Err(DiagError::StaleTag);
        }
        let report: Report = serde_json::from_str(lint_json).map_err(|e| DiagError::BadJson(e.to_string()))?;
        if report.schema_version != 2 {
            return Err(DiagError::UnsupportedSchema(report.schema_version));
        }
        let mut next = Diagnostics::default();
        for raw in report.diagnostics {
            let severity = match raw.severity.as_str() {
                "error" => Severity::Error,
                "warning" => Severity::Warning,
                _ => Severity::Note,
            };
            let line = raw.line.unwrap_or(1).max(1);
            let Some((covered, range)) = locate(text_at_tag, line, raw.column) else { continue };
            next.items.push(Diagnostic { range, line, severity, code: raw.code, message: raw.message, hint: raw.hint });
            next.covered.push(covered);
        }
        for d in deltas_since {
            next.apply_delta(d);
        }
        *self = next;
        Ok(())
    }

    /// Drop diagnostics whose covered line the delta touched; map the rest.
    pub fn apply_delta(&mut self, d: &ViewDelta) {
        if d.kind == DeltaKind::Resync {
            self.items.clear();
            self.covered.clear();
            return;
        }
        for e in &d.edits {
            let items = std::mem::take(&mut self.items);
            let covered = std::mem::take(&mut self.covered);
            for (mut item, cov) in items.into_iter().zip(covered) {
                if touches(&cov, e) {
                    continue;
                }
                item.range = map(item.range, e);
                self.items.push(item);
                self.covered.push(map(cov, e));
            }
        }
    }

    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }
}

/// The covered line (with its `\n`) and the squiggle range for a 1-based line
/// and 1-based scalar column in `text`. Without a column the squiggle is the
/// line's content past its indentation; with one it runs over the word there
/// (at least one scalar).
fn locate(text: &str, line: usize, column: Option<usize>) -> Option<(Range<usize>, Range<usize>)> {
    let start = if line == 1 { 0 } else { text.match_indices('\n').nth(line - 2).map(|(i, _)| i + 1)? };
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let covered = start..(end + 1).min(text.len());
    let content = text[start..end].trim_end_matches('\r');
    let squiggle = match column.filter(|&c| c >= 1) {
        None => {
            let indent = content.len() - content.trim_start().len();
            start + indent..start + content.len()
        }
        Some(col) => {
            let at = content.char_indices().nth(col - 1).map_or(content.len(), |(i, _)| i);
            let word = content[at..].char_indices().find(|&(_, c)| !(c.is_alphanumeric() || "_$.".contains(c))).map_or(content.len() - at, |(i, _)| i);
            let len = if word > 0 { word } else { content[at..].chars().next().map_or(0, char::len_utf8) };
            start + at..start + at + len
        }
    };
    // An empty line (or a column past its end) still gets a visible mark.
    let squiggle = if squiggle.is_empty() { squiggle.start..squiggle.start } else { squiggle };
    Some((covered, squiggle))
}

/// Whether an edit changes the covered line: any delete reaching into it
/// (deleting the `\n` before it only joins it to the previous line, so a
/// delete ENDING at its start does not count), or an insert inside it or at
/// its start (unless the insert is whole lines, ending in `\n`).
fn touches(cov: &Range<usize>, e: &Edit) -> bool {
    let p = e.offset;
    let delete_hits = e.delete > 0 && p < cov.end && p + e.delete > cov.start;
    let insert_hits = !e.insert.is_empty() && ((p > cov.start && p < cov.end) || (p == cov.start && !e.insert.ends_with('\n')));
    delete_hits || insert_hits
}

/// A non-expanding range anchor: whole lines inserted at its start push it
/// down (start `After`), text inserted at its end stays outside (end `Before`).
fn map(r: Range<usize>, e: &Edit) -> Range<usize> {
    let s = map_point(r.start, Bias::After, e).0;
    let t = map_point(r.end, Bias::Before, e).0.max(s);
    s..t
}

#[cfg(test)]
mod tests;
