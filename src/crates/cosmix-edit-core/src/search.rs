//! Literal/regex search (plan §3.7).
//!
//! Literal = `regex::escape`; `RegexBuilder` with `multi_line(true)`,
//! `case_insensitive(!case)`, `size_limit(REGEX_SIZE_LIMIT)`. Per match: `text`
//! truncated to `MATCH_TEXT_MAX` (`text_truncated`); groups only when requested,
//! each truncated likewise; the whole match's encoded size capped at
//! `MATCH_ENCODED_MAX` — groups past it become `null` and `groups_truncated`
//! is set, so every page holds at least one match. The result stops at
//! `limit` or the caller's encoded budget with `truncated` and `next` (end of
//! the last returned match, or its start + 1 scalar for an empty match).
//! Bad regex → INVALID_ARGUMENT `bad_regex`.

use std::ops::Range;

use crate::pos::RangeSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindQuery {
    pub pattern: String,
    pub regex: bool,
    /// `true` = case-sensitive.
    pub case: bool,
    pub range: Option<RangeSpec>,
    pub groups: bool,
    pub limit: usize,
    /// Resume offset (a previous result's `next`).
    pub from: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub range: Range<usize>,
    pub text: String,
    pub text_truncated: bool,
    /// `None` unless groups were requested; `None` items = non-participating or past the budget.
    pub groups: Option<Vec<Option<String>>>,
    pub groups_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindResult {
    pub matches: Vec<Match>,
    pub truncated: bool,
    pub next: Option<usize>,
}
