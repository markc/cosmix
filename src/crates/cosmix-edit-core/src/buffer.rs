//! The buffer façade (plan §3.10): text + op log + anchors + selections.
//!
//! # Contract: transactions (plan §3.4, frozen)
//! All ops of one request refer to the same base text. Each resolves to a base
//! range `[s, e)` plus replacement (pure insert: `s == e`). Two ranges OVERLAP —
//! INVALID_ARGUMENT `overlap_in_txn` — when they share a byte
//! (`max(s) < min(e)`) or a pure insert lies strictly inside a non-empty range.
//! Two non-empty ranges with the same start always overlap.
//!
//! Application order: by `s` descending; at equal `s`, the (at most one)
//! non-empty range op first, then the pure inserts at `s` in REVERSE request
//! order. Invariant: when an op applies, every previously applied op lies at or
//! after the op's `e` in base coordinates, except a same-start range op, which
//! leaves `s` itself unchanged — so recorded sequential coordinates equal base
//! coordinates. Consequences: inserts at one offset read in request order;
//! inserts at a range's start read BEFORE its replacement; an insert at a
//! range's end reads after it. `changed` is computed after application.
//!
//! All validation (resolution, char boundaries, overlap, limits, byte budget)
//! happens before phase 1 of [`crate::text::Text`]; nothing mutates on refusal.
//!
//! # CAS
//! `Latest`: current text. `ExpectRev(r)`: `r != rev` → CONFLICT `stale_rev`.
//! `BaseRev(b)`: offsets only (`base_rev_needs_offsets`); the resolved set is
//! transformed through every logged edit `b+1..=rev` (`Priority::ThroughFirst`),
//! then char-boundary-validated against the current text (exact: a range that
//! overlaps no intervening edit sits in bytes unchanged since `b`). Overlap →
//! CONFLICT `overlap`; `b` older than retention → `history_trimmed`;
//! `b > rev` → INVALID_ARGUMENT `base_rev_in_future`.

use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::anchor::{AnchorSpec, NamedAnchor, Selection};
use crate::error::CoreError;
use crate::history::{EntryKind, LogEntry, OpLog};
use crate::origin::{Origin, Via};
use crate::ot::Edit;
use crate::pos::{Point, PosSpec, RangeSpec};
use crate::search::{FindQuery, FindResult};
use crate::text::Text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eol {
    Lf,
    Crlf,
    Mixed,
    None,
}

/// What loading found and saving must restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub bom: bool,
    pub eol: Eol,
}

/// Wire form: `OP := {"op":"insert","at":POS,"text":S} | {"op":"delete","range":RANGE}
/// | {"op":"replace","range":RANGE,"text":S}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum OpSpec {
    Insert { at: PosSpec, text: String },
    Delete { range: RangeSpec },
    Replace { range: RangeSpec, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cas {
    Latest,
    ExpectRev(u64),
    BaseRev(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnRequest {
    pub ops: Vec<OpSpec>,
    pub cas: Cas,
    pub coalesce: bool,
    /// Post-edit coordinates; replaces the editing origin's selections with one caret.
    pub cursor: Option<PosSpec>,
    /// Recorded and echoed only; dedup is editd's.
    pub op_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub rev: u64,
    pub base_rev: u64,
    pub kind: EntryKind,
    /// Application order (the event carries these).
    pub edits: Vec<Edit>,
    /// Inserted spans in the new text, offset order, computed after application.
    pub changed: Vec<Range<usize>>,
    pub rebased: bool,
    pub op_id: Option<String>,
    pub inserted_bytes: usize,
    pub deleted_bytes: usize,
    /// Set when this apply trimmed history: the new oldest retained rev.
    pub history_trimmed_to: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneSel {
    Own,
    All,
    Lane(Origin),
}

pub struct Buffer {
    text: Text,
    log: OpLog,
    rev: u64,
    saved_rev: Option<u64>,
    anchors: Vec<NamedAnchor>,
    selections: std::collections::BTreeMap<Origin, Vec<Selection>>,
}

impl Buffer {
    /// Empty buffer at rev 0.
    pub fn new() -> Result<Self, CoreError> {
        todo!("E0a")
    }

    /// BOM strip, UTF-8 check (`not_utf8`), eol scan, size/line limits.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, FileMeta), CoreError> {
        let _ = bytes;
        todo!("E0a")
    }

    /// BOM restored, text verbatim.
    pub fn to_bytes(&self, meta: &FileMeta) -> Vec<u8> {
        let _ = meta;
        todo!("E0a")
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.text.line_count()
    }

    pub fn point(&self, offset: usize) -> Point {
        self.text.point(offset)
    }

    pub fn resolve_pos(&self, p: &PosSpec) -> Result<usize, CoreError> {
        let _ = p;
        todo!("E0a")
    }

    pub fn resolve_range(&self, r: &RangeSpec) -> Result<Range<usize>, CoreError> {
        let _ = r;
        todo!("E0a")
    }

    /// Chunked copy; never moves the gap.
    pub fn read(&self, r: Range<usize>, out: &mut String) {
        self.text.read(r, out)
    }

    /// Frozen copy for paged reads (editd leases its bytes first).
    pub fn snapshot(&self) -> Arc<str> {
        todo!("E0a")
    }

    /// One transaction (module docs). On `Err` nothing changed.
    pub fn apply(&mut self, req: TxnRequest, origin: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        let _ = (req, origin, via, now_ms);
        todo!("E0a")
    }

    /// Byte-budget preview for editd's lease: the peak growth (text + log text)
    /// `apply` would need. Pure; same validation as `apply`.
    pub fn apply_cost(&self, req: &TxnRequest) -> Result<usize, CoreError> {
        let _ = req;
        todo!("E0a")
    }

    /// Preflighted undo (see `history` module docs). On `Err` nothing changed.
    pub fn undo(&mut self, lane: LaneSel, caller: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        let _ = (lane, caller, via, now_ms);
        todo!("E0a")
    }

    pub fn redo(&mut self, lane: LaneSel, caller: &Origin, via: Via, now_ms: u64) -> Result<Applied, CoreError> {
        let _ = (lane, caller, via, now_ms);
        todo!("E0a")
    }

    /// Common prefix/suffix kept; one `Reload` entry in lane `tool:disk`.
    /// `None` when identical.
    pub fn reload_minimal(&mut self, new_text: &str, via: Via, now_ms: u64) -> Result<Option<Applied>, CoreError> {
        let _ = (new_text, via, now_ms);
        todo!("E0a")
    }

    pub fn set_selections(&mut self, origin: &Origin, sels: Vec<Selection>) -> Result<(), CoreError> {
        let _ = (origin, sels);
        todo!("E0a")
    }

    pub fn selections(&self) -> impl Iterator<Item = (&Origin, &[Selection])> {
        self.selections.iter().map(|(o, s)| (o, s.as_slice()))
    }

    pub fn anchor_set(&mut self, name: &str, spec: AnchorSpec) -> Result<(), CoreError> {
        let _ = (name, spec);
        todo!("E0a")
    }

    pub fn anchor(&self, name: &str) -> Option<&NamedAnchor> {
        self.anchors.iter().find(|a| a.name == name)
    }

    pub fn anchors(&self) -> impl Iterator<Item = &NamedAnchor> {
        self.anchors.iter()
    }

    pub fn anchor_clear(&mut self, name: &str) -> bool {
        let before = self.anchors.len();
        self.anchors.retain(|a| a.name != name);
        self.anchors.len() != before
    }

    /// `budget_bytes` = the encoded-reply budget left for matches.
    pub fn find(&mut self, q: &FindQuery, budget_bytes: usize) -> Result<FindResult, CoreError> {
        let _ = (q, budget_bytes);
        todo!("E0a")
    }

    /// Up to `limit` entries with `rev > since_rev`, oldest first.
    pub fn history(&self, since_rev: u64, limit: usize) -> impl Iterator<Item = &LogEntry> {
        self.log.since(since_rev, limit)
    }

    pub fn oldest_rev(&self) -> u64 {
        self.log.oldest_rev()
    }

    /// Bytes of retained log text (counted in editd's aggregate budget).
    pub fn log_text_bytes(&self) -> usize {
        self.log.text_bytes
    }

    pub fn mark_saved(&mut self) {
        self.saved_rev = Some(self.rev);
    }

    pub fn saved_rev(&self) -> Option<u64> {
        self.saved_rev
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_rev != Some(self.rev)
    }
}
