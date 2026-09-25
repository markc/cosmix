//! Anchors and selections (plan §3.6).
//!
//! Mapping a point `a` through an edit `(p, dd, ii)` (frozen):
//!
//! | Case | Result |
//! |---|---|
//! | `dd == 0`, `p < a` | `a += ii` |
//! | `dd == 0`, `p == a` | `Before`: unchanged; `After`: `a += ii` |
//! | `a <= p`, `dd > 0` | unchanged; if `a == p` the insert part then applies the `p == a` rule |
//! | `p < a < p + dd` | `a = p`, then `Before`; named anchors get `collapsed_rev` |
//! | `a >= p + dd` | `a += ii - dd` |
//!
//! Named point anchors default `Before`. Range anchors are non-expanding (start
//! `After`, end `Before`, clamp `start <= end`; an empty range maps as a
//! `Before` point). The editing origin's selections map with `After`, every
//! other origin's with `Before`; a request `cursor` is in POST-edit coordinates
//! and replaces the editing origin's selections with one caret.

use serde::{Deserialize, Serialize};

use crate::ot::Edit;
use crate::pos::{PosSpec, RangeSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bias {
    #[default]
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub offset: usize,
    pub bias: Bias,
    /// Set when a delete strictly spanning the anchor collapsed it.
    pub collapsed_rev: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedAnchor {
    /// `^[A-Za-z0-9._-]{1,64}$`; buffer-global.
    pub name: String,
    pub start: Anchor,
    /// `Some` for a range anchor.
    pub end: Option<Anchor>,
}

/// Exactly one of `at` / `range`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnchorSpec {
    pub at: Option<PosSpec>,
    pub range: Option<RangeSpec>,
    pub bias: Option<Bias>,
}

/// Direction preserved; a caret when `anchor == head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

/// Map one point through one applied edit (table above). Returns the new
/// offset and whether it collapsed.
pub fn map_point(a: usize, bias: Bias, edit: &Edit) -> (usize, bool) {
    let _ = (a, bias, edit);
    todo!("E0a: §3.6 mapping table")
}

/// Whether `name` is a valid anchor name.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
