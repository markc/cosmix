//! Positions and ranges (plan §3.2). These types ARE the wire forms (§4.2).
//!
//! Canonical position = UTF-8 byte offset. `line` and `col` are 1-based; `col`
//! counts every Unicode scalar value from the line start **including `\r`**; the
//! line terminator is `\n` alone, so every char boundary has exactly one
//! `(line, col)` and the round trip is total. `line_count = newlines + 1`.
//! Max col on a line = scalar count excluding `\n`, plus 1.
//!
//! Resolution errors (INVALID_ARGUMENT): `line_out_of_range` (line outside
//! `1..=line_count`), `col_out_of_range`, `offset_out_of_range`,
//! `not_char_boundary`; `Lines(a, b)` requires `1 <= a <= b <= line_count`;
//! an unknown anchor is NOT_FOUND `unknown_anchor`.

use serde::{Deserialize, Serialize};

/// Every response position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub offset: usize,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedPos {
    Start,
    End,
}

/// `POS := <int offset> | {"line":L,"col":C?} | {"anchor":"name"} | "start" | "end"`.
/// Under `base_rev` only `Offset` is accepted (`base_rev_needs_offsets`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PosSpec {
    Offset(usize),
    Named(NamedPos),
    LineCol {
        line: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        col: Option<usize>,
    },
    Anchor {
        anchor: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllTag {
    All,
}

/// `RANGE := [s, e] | {"start":POS,"end":POS} | {"lines":[a,b]} | {"anchor":"name"} | "all"`.
/// Under `base_rev` only `Offsets` is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RangeSpec {
    Offsets([usize; 2]),
    Span { start: PosSpec, end: PosSpec },
    Lines { lines: [usize; 2] },
    Anchor { anchor: String },
    All(AllTag),
}

/// `SEL := {"anchor":POS,"head":POS} | RANGE` (explicit direction first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SelSpec {
    Directed { anchor: PosSpec, head: PosSpec },
    Range(RangeSpec),
}
