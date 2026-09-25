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
    let _ = (r, through, prio);
    todo!("E0a: §3.4 transform rules")
}

/// Transform every item of `set` through `through` (applied in order after
/// `set.rev`). The result's `rev` is `set.rev + through.len()` only when the
/// caller passes whole log entries; the caller sets it.
pub fn transform_set(set: &RangeSet, through: &[Edit], prio: Priority) -> Result<RangeSet, Overlap> {
    let _ = (set, through, prio);
    todo!("E0a: item-wise transform_range")
}

/// The entry's inverse as a RangeSet on its post-rev text (module docs).
pub fn invert(entry: &LogEntry) -> RangeSet {
    let _ = entry;
    todo!("E0a: reverse-order inversion, normalised")
}
