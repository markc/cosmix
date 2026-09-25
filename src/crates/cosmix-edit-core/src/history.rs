//! Op log, undo lanes, coalescing and retention (plan §3.5).
//!
//! # Contract: undo is preflighted, never rolled back (frozen)
//! `undo(L)`:
//! 1. Peek (do not pop) lane L's top group.
//! 2. For each member rev, newest → oldest: take its inverse RangeSet
//!    ([`crate::ot::invert`]) and transform it through EVERY log entry after
//!    that member (including the group's own later members), so every item ends
//!    in current-rev coordinates. Overlap → CONFLICT `undo_conflict` (context
//!    `intervening_rev`, `intervening_origin`). (E0a sharpening: an undo/redo
//!    entry in that window whose target entries are also in it cancels
//!    against them, the entries between being rewritten to exclude them —
//!    otherwise "edit, fix it, undo the fix, undo the edit" refuses, because
//!    the fix overlaps the edit's text although it was itself undone. See
//!    `Buffer::reduced_after`.)
//! 3. Union the items into one RangeSet in READING order. Two items
//!    overlapping under the §3.4 rule → `undo_conflict`. Equal-offset inserts
//!    keep list order. (E0a sharpening: the plan said "newest member first",
//!    which restores a run of backspaces correctly but REVERSES a run of
//!    forward deletes — both coalesce by the rule below. The composition in
//!    `Buffer::compose_inverse` orders a tie by where the newer member's edit
//!    lay relative to the older item when it applied, which is the original
//!    order in both cases.)
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
    pub(crate) lanes: BTreeMap<Origin, Lane>,
    pub(crate) text_bytes: usize,
    /// The rev just before the first retained entry: the oldest rev a
    /// `base_rev` may name, and the lower bound of `history`.
    pub(crate) base: u64,
}

/// Stored text of one entry (inserted + deleted bytes).
pub(crate) fn entry_text_bytes(e: &LogEntry) -> usize {
    e.edits.iter().map(|x| x.insert.len()).sum::<usize>() + e.deleted.iter().map(String::len).sum::<usize>()
}

impl OpLog {
    /// The oldest rev whose successors are all retained: a `base_rev` or an
    /// undo reaching further back is `history_trimmed`.
    pub fn oldest_rev(&self) -> u64 {
        self.base
    }

    /// Appends `entry` (its space was reserved in phase 1) and trims to the
    /// retention limits. Returns the new `oldest_rev` if anything was trimmed.
    pub(crate) fn push(&mut self, entry: LogEntry) -> Option<u64> {
        self.text_bytes += entry_text_bytes(&entry);
        self.entries.push_back(entry);
        let mut trimmed = false;
        while self.entries.len() > crate::limits::LOG_MAX_ENTRIES
            || self.text_bytes > crate::limits::LOG_MAX_TEXT_BYTES
        {
            let Some(old) = self.entries.pop_front() else { break };
            self.text_bytes -= entry_text_bytes(&old);
            self.base = old.rev;
            trimmed = true;
        }
        if !trimmed {
            return None;
        }
        // Undo reach is exactly the retained suffix.
        let base = self.base;
        for lane in self.lanes.values_mut() {
            lane.undo.retain(|g| *g.start() > base);
            lane.redo.retain(|g| *g.start() > base);
        }
        self.lanes.retain(|_, l| !l.undo.is_empty() || !l.redo.is_empty());
        Some(base)
    }

    /// Up to `limit` entries with `rev > since_rev`, oldest first.
    pub fn since(&self, since_rev: u64, limit: usize) -> impl Iterator<Item = &LogEntry> {
        self.after(since_rev).take(limit)
    }

    /// Entries strictly after `rev`, in order (for transforms).
    pub fn after(&self, rev: u64) -> impl Iterator<Item = &LogEntry> {
        // Revs are contiguous, so the first entry after `rev` is found by index.
        let skip = match self.entries.front() {
            Some(first) => usize::try_from(rev.saturating_add(1).saturating_sub(first.rev)).unwrap_or(usize::MAX),
            None => 0,
        };
        self.entries.range(skip.min(self.entries.len())..)
    }
}
