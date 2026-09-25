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
    transform_raw(r, RawEdit::of(through), prio).map(|(r, _)| r)
}

/// An edit reduced to its shape: offset, deleted length, inserted length.
/// Transforms only ever need the lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawEdit {
    pub p: usize,
    pub dd: usize,
    pub ii: usize,
}

impl RawEdit {
    pub(crate) fn of(e: &Edit) -> Self {
        Self { p: e.offset, dd: e.delete, ii: e.insert.len() }
    }
}

/// [`transform_range`] on a [`RawEdit`], also reporting whether the edit's
/// region lies to the LEFT of the range (the range shifted). Undo
/// composition uses that to order restored texts that land on one offset.
pub(crate) fn transform_raw(r: Range<usize>, through: RawEdit, prio: Priority) -> Result<(Range<usize>, bool), Overlap> {
    let (s, e) = (r.start, r.end);
    let RawEdit { p, dd, ii } = through;
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
    post_ranges_raw(&edits.iter().map(RawEdit::of).collect::<Vec<_>>())
}

/// [`post_ranges`] on raw edits.
pub(crate) fn post_ranges_raw(edits: &[RawEdit]) -> Vec<Range<usize>> {
    edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut r = e.p..e.p + e.ii;
            for later in &edits[i + 1..] {
                match transform_raw(r.clone(), *later, Priority::ThroughFirst) {
                    Ok((t, _)) => r = t,
                    Err(Overlap) => debug_assert!(false, "non-canonical edit sequence"),
                }
            }
            r
        })
        .collect()
}

/// §3.4 overlap rule over base ranges `(start, end)`: two ranges overlap when
/// they share a byte (`max(s) < min(e)`) or a pure insert lies strictly inside
/// a non-empty range; two non-empty ranges with one start always overlap. An
/// insert at a range's start or end does not. Returns the first offender.
pub(crate) fn first_overlap(items: impl Iterator<Item = (usize, usize)>) -> Option<(usize, usize)> {
    let mut order: Vec<(usize, usize)> = items.collect();
    // Points before ranges at one start, so an insert at a range's start passes.
    order.sort_by_key(|&(s, e)| (s, e > s));
    let mut max_e = 0;
    for (s, e) in order {
        if s < max_e {
            return Some((s, e));
        }
        if e > s {
            max_e = max_e.max(e);
        }
    }
    None
}

/// §3.4 application order over `(start, end, request index)`: start
/// descending; at an equal start the (at most one) non-empty range first, then
/// pure inserts in REVERSE request order — which is what makes inserts at one
/// offset read in request order once applied.
pub(crate) fn canonical_cmp(a: (usize, usize, usize), b: (usize, usize, usize)) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then((b.1 > b.0).cmp(&(a.1 > a.0))).then(b.2.cmp(&a.2))
}

/// The server's application order for a base-coordinate transaction (ced E1
/// plan §1.3; the same code `Buffer::apply` uses, so client and server share
/// ONE implementation).
///
/// `items` are `(base range, replacement)` in REQUEST order (that order breaks
/// equal-offset ties). Returns, in application order, each step's ORIGINAL item
/// index and its sequential [`Edit`]. Because the order is canonical, every
/// step's sequential offsets equal its base offsets (E0 §3.4 invariant), so a
/// caller pairs each step with its item's deleted text by index.
///
/// `Err(Overlap)` when two items overlap under [`first_overlap`]'s rule or an
/// item's range is reversed.
pub fn txn_sequence(items: &[(Range<usize>, String)]) -> Result<Vec<(usize, Edit)>, Overlap> {
    if items.iter().any(|(r, _)| r.start > r.end) || first_overlap(items.iter().map(|(r, _)| (r.start, r.end))).is_some()
    {
        return Err(Overlap);
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        canonical_cmp((items[a].0.start, items[a].0.end, a), (items[b].0.start, items[b].0.end, b))
    });
    Ok(order
        .into_iter()
        .map(|i| {
            let (r, text) = &items[i];
            (i, Edit { offset: r.start, delete: r.end - r.start, insert: text.clone() })
        })
        .collect())
}

/// Transform a range through a whole base-coordinate TRANSACTION applied
/// after it (ced E1 plan §3.4, Stage S freeze note 3): `items` are
/// `(base range, inserted length)`, non-overlapping, all in the SAME
/// coordinates as `r`. Each item is judged against `r`'s original position
/// independently and the shifts are summed; any overlap is an `Overlap`.
///
/// This — not transforming through the transaction's sequential steps — is
/// what agrees with the server, which rebases the transaction's items as a
/// set. Counter-example for the sequential form: `r` = insert at 20 through
/// `[delete [10,20), insert "P"@10]`. Sequentially the delete moves `r` to 10
/// and it then ties with the insert at 10 (landing before "P"); as a set, `r`
/// sits after both items and lands after "P" — which is where the server puts
/// it.
pub fn transform_through_set(r: Range<usize>, items: &[(Range<usize>, usize)], prio: Priority) -> Result<Range<usize>, Overlap> {
    let (mut ds, mut de) = (0isize, 0isize);
    for (ir, ii) in items {
        let raw = RawEdit { p: ir.start, dd: ir.end - ir.start, ii: *ii };
        let (t, _) = transform_raw(r.clone(), raw, prio)?;
        ds += t.start as isize - r.start as isize;
        de += t.end as isize - r.end as isize;
    }
    let start = r.start as isize + ds;
    let end = r.end as isize + de;
    if start < 0 || end < start {
        return Err(Overlap);
    }
    Ok(start as usize..end as usize)
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
