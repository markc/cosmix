//! Edits, range sets and single-authority OT (plan §3.4, §3.5).
//!
//! # Transform (frozen)
//! A range `[s, e)` (pure insert: `s == e`) through an edit `(p, dd, ii)` applied
//! after it:
//! - `p + dd < s`, or `p + dd == s` with `dd > 0` → shift both ends by `ii - dd`;
//! - `p > e`, or `p == e` with `dd > 0` → unchanged;
//! - `dd == 0 && p == s == e` → `Priority`: `ThroughFirst` shifts by `ii`, `SelfFirst` unchanged;
//! - `dd == 0 && p == s < e` → shift by `ii`; `dd == 0 && p == e > s` → unchanged;
//! - otherwise → `Overlap`.
//!
//! # Tie priority is server order
//! Server rebasing a `base_rev` request through applied edits: `ThroughFirst`.
//! E1 client transforming an incoming server edit through its pending local
//! edits: `SelfFirst`; rebasing its pending edits through that server edit:
//! `ThroughFirst`.
//!
//! # Inverse (frozen)
//! The inverse of a log entry applies its edits' inverses in REVERSE application
//! order, each `(o, d, ins)` becoming `(o, ins.len(), deleted)` in the sequential
//! coordinates valid at that point, normalised into one `RangeSet` on the
//! entry's post-rev text. Example: `abcd` + `[X@3, Y@0]` → applied `X@3`, `Y@0`
//! → `YabcXd`; inverse = `{[0,1)→"", [4,5)→""}` at that rev → `abcd`.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::history::LogEntry;

/// One applied step, in the sequential coordinates valid when it applied.
/// Wire form `EDIT := {"offset":N,"delete":N,"insert":S}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub offset: usize,
    pub delete: usize,
    pub insert: String,
}

/// Non-overlapping `(range, replacement)` items, all in the coordinates of the
/// text at `rev`. Listed order matters only for equal-offset pure inserts,
/// which read in list order once applied (§3.4 application order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeSet {
    pub rev: u64,
    pub items: Vec<(Range<usize>, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The edit being transformed THROUGH was applied first (goes first at a tie).
    ThroughFirst,
    /// The range being transformed keeps its place at a tie.
    SelfFirst,
}

/// The transformed range touched text the other edit changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlap;

/// Transform one range through one later edit (rules in the module docs).
pub fn transform_range(r: Range<usize>, through: &Edit, prio: Priority) -> Result<Range<usize>, Overlap> {
    transform_range_side(r, through, prio).map(|(r, _)| r)
}

/// [`transform_range`] that also reports whether the edit's region lies to
/// the LEFT of the range (the range shifted). Undo composition uses it to
/// order restored texts that end up at one offset.
pub(crate) fn transform_range_side(
    r: Range<usize>,
    through: &Edit,
    prio: Priority,
) -> Result<(Range<usize>, bool), Overlap> {
    let (s, e) = (r.start, r.end);
    let (p, dd, ii) = (through.offset, through.delete, through.insert.len());
    let pe = p + dd;
    if pe < s || (pe == s && dd > 0) {
        Ok((s - dd + ii..e - dd + ii, true))
    } else if p > e || (p == e && dd > 0) {
        Ok((s..e, false))
    } else if dd == 0 && p == s && s == e {
        match prio {
            Priority::ThroughFirst => Ok((s + ii..e + ii, true)),
            Priority::SelfFirst => Ok((s..e, false)),
        }
    } else if dd == 0 && p == s && s < e {
        Ok((s + ii..e + ii, true))
    } else if dd == 0 && p == e && e > s {
        Ok((s..e, false))
    } else {
        Err(Overlap)
    }
}

/// Transform every item of `set` through `through` (applied in order after
/// `set.rev`). The result's `rev` is `set.rev + through.len()` only when the
/// caller passes whole log entries; the caller sets it (this keeps `set.rev`).
pub fn transform_set(set: &RangeSet, through: &[Edit], prio: Priority) -> Result<RangeSet, Overlap> {
    let mut items = Vec::with_capacity(set.items.len());
    for (r, text) in &set.items {
        let mut r = r.clone();
        for edit in through {
            r = transform_range(r, edit, prio)?;
        }
        items.push((r, text.clone()));
    }
    Ok(RangeSet { rev: set.rev, items })
}

/// Each edit's inserted span in the coordinates after the WHOLE sequence, in
/// sequence order. For a canonical sequence (every step at or before the
/// previous one) nothing overlaps; a later edit at the same point reads
/// before an earlier one, so ties shift (`ThroughFirst`).
pub(crate) fn post_ranges(edits: &[Edit]) -> Vec<Range<usize>> {
    edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut r = e.offset..e.offset + e.insert.len();
            for later in &edits[i + 1..] {
                match transform_range(r.clone(), later, Priority::ThroughFirst) {
                    Ok(t) => r = t,
                    Err(Overlap) => debug_assert!(false, "non-canonical edit sequence"),
                }
            }
            r
        })
        .collect()
}

/// The entry's inverse as a RangeSet on its post-rev text (module docs).
/// Items are in reading order: ascending, and at a tie in reverse
/// application order — the order the removed texts originally had.
/// Entries this crate logs are always canonical; a hand-built non-canonical
/// entry keeps untransformed ranges where a transform would overlap.
pub fn invert(entry: &LogEntry) -> RangeSet {
    let items = post_ranges(&entry.edits)
        .into_iter()
        .enumerate()
        .rev()
        .map(|(i, r)| (r, entry.deleted.get(i).cloned().unwrap_or_default()))
        .collect();
    RangeSet { rev: entry.rev, items }
}
