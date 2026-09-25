//! Op log, undo lanes, coalescing and retention (plan §3.5).
//!
//! # Contract: undo is preflighted, never rolled back (frozen)
//! `undo(L)`:
//! 1. Peek (do not pop) lane L's top group.
//! 2. For each member rev, newest → oldest: take its inverse RangeSet
//!    ([`crate::ot::invert`]) and transform it through EVERY log entry after
//!    that member (including the group's own later members), so every item ends
//!    in current-rev coordinates. Overlap → CONFLICT `undo_conflict` (context
//!    `intervening_rev`, `intervening_origin`).
//! 3. Union the items into one RangeSet, newest member first. Two items
//!    overlapping under the §3.4 rule → `undo_conflict`. Equal-offset inserts
//!    keep list order.
//! 4. Verify against the CURRENT text that each item's target range holds
//!    exactly the text it removes; mismatch → `undo_conflict`.
//! 5. Apply the union as ONE ordinary two-phase transaction, recorded as one
//!    entry `Undo { of: first..=last }`; anchors/selections map once.
//! 6. Only then pop the group and push the new entry's rev on `redo[L]`.
//!
//! A failed undo/redo: refusal only — no entry, no rev change, no event, no
//! anchor/selection change, stacks unchanged. Redo is the same over the Undo
//! entry's inverse.
//!
//! # Lanes and coalescing
//! A new `Edit` in lane L pushes a group, or with `coalesce: true` appends to
//! the top group when that group's last rev is `rev - 1`, same lane,
//! single-edit txn, contiguous with the previous insert (or delete). It clears
//! `redo[L]`. Groups are therefore contiguous rev ranges. Global `"*"` picks the
//! lane whose top group has the highest rev.
//!
//! # Retention
//! A contiguous suffix: newest `LOG_MAX_ENTRIES` entries or `LOG_MAX_TEXT_BYTES`
//! of stored text, whichever binds. Undo reach is exactly the retained suffix;
//! groups referencing trimmed revs are dropped. A trimming apply reports
//! `history_trimmed_to`.
//!
//! # op_id
//! The core records and echoes `op_id` only. Deduplication belongs to editd's
//! buffer actor, keyed `(caller_key, origin, verb, op_id)` BEFORE lane
//! selection, successes only (plan §3.5).

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::origin::{Origin, Via};
use crate::ot::Edit;

/// Wire form (history entries and events): `"kind": "edit"|"undo"|"redo"|"reload"`
/// plus `"of": [first_rev, last_rev] | null`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Edit,
    /// `of` = the undone group's revs (contiguous by the coalescing rule).
    Undo { of: RangeInclusive<u64> },
    Redo { of: RangeInclusive<u64> },
    Reload,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub rev: u64,
    /// Who asked.
    pub origin: Origin,
    /// Whose undo stack it belongs to (for Undo/Redo: the lane undone).
    pub lane: Origin,
    pub kind: EntryKind,
    /// Application order, sequential coordinates.
    pub edits: Vec<Edit>,
    /// Text removed by each edit, same order.
    pub deleted: Vec<String>,
    pub via: Via,
    pub op_id: Option<String>,
    pub time_ms: u64,
}

/// Revs undone together; contiguous.
pub type Group = RangeInclusive<u64>;

#[derive(Debug, Clone, Default)]
pub struct Lane {
    pub undo: Vec<Group>,
    pub redo: Vec<Group>,
}

/// The retained suffix plus per-lane stacks.
#[derive(Debug, Default)]
pub struct OpLog {
    pub(crate) entries: std::collections::VecDeque<LogEntry>,
    #[allow(dead_code)] // Stage S stub; E0a's undo/redo read it.
    pub(crate) lanes: BTreeMap<Origin, Lane>,
    pub(crate) text_bytes: usize,
}

impl OpLog {
    pub fn oldest_rev(&self) -> u64 {
        todo!("E0a")
    }

    /// Up to `limit` entries with `rev > since_rev`, oldest first.
    pub fn since(&self, since_rev: u64, limit: usize) -> impl Iterator<Item = &LogEntry> {
        self.after(since_rev).take(limit)
    }

    /// Entries strictly after `rev`, in order (for transforms).
    pub fn after(&self, rev: u64) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter().filter(move |e| e.rev > rev)
    }
}
