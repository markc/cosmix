//! The editor model: selection, preferred column, scroll, remote carets,
//! markers and the IME composition range, plus pure text commands that turn
//! into [`LocalEdit`]s (ced E1 plan §4.1). Shared by the ced widget, `ced
//! --headless` and the E2 scene widget. Stage S freezes the API; Stage E1e
//! implements it.
//!
//! Contracts:
//! - Every motion is grapheme-correct through `cosmix_edit_core::view`; Up and
//!   Down keep `preferred_cells`.
//! - [`EditorModel::apply_delta`] maps the own selection with `After` for
//!   `Local` deltas and `Before` for every other kind; remote carets, markers
//!   and diagnostics with `Before`; a `Resync` delta clamps to char boundaries.
//!   A delta that OVERLAPS the composition range cancels it (`composition =
//!   None`, and the widget re-enables its input method); a non-overlapping
//!   delta maps it.
//! - Multi-line `Tab`/`Outdent` produce ONE multi-item [`LocalEdit`] (one
//!   `edit.apply`, one undo group); `coalesce` only for single-grapheme typing
//!   and backspace/delete runs.
//! - `Newline` auto-indents (copies the line's leading whitespace) and uses the
//!   buffer's eol; `ToggleComment` uses the language's line-comment token
//!   (`--` mix/scene/mix-data, `//` rust/c/cpp/go/javascript, `#`
//!   shell/python/toml/yaml; `None` elsewhere → command yields nothing).
//! - Lines are 1-based, cells 0-based (like `view`).

use std::ops::Range;

use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::origin::Origin;
use cosmix_edit_core::text::Text;
use cosmix_edit_core::view::MeasureCfg;

use crate::types::{LocalEdit, ViewDelta};

/// Per-buffer editing settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditCfg {
    pub measure: MeasureCfg,
    /// Tab inserts spaces (to the next stop) instead of `\t`.
    pub insert_spaces: bool,
    /// `"\n"` or `"\r\n"` — the buffer's eol.
    pub eol: &'static str,
    /// The language's line-comment token, if it has one.
    pub line_comment: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scroll {
    /// 1-based first visible line.
    pub first_line: usize,
    /// Horizontal scroll in cells.
    pub x_cells: usize,
}

/// Lines another origin changed since the tab was last focused, as ranges
/// that follow the text (plan §4.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Markers {
    pub changed: Vec<(Range<usize>, Origin, u64 /* rev */)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorModel {
    pub sel: Selection,
    pub preferred_cells: Option<usize>,
    pub scroll: Scroll,
    pub overwrite: bool,
    /// Other origins' selections (display only).
    pub remote: Vec<(Origin, Vec<Selection>)>,
    pub markers: Markers,
    /// IME preedit anchor range, view coordinates.
    pub composition: Option<Range<usize>>,
}

impl Default for EditorModel {
    fn default() -> Self {
        Self {
            sel: Selection { anchor: 0, head: 0 },
            preferred_cells: None,
            scroll: Scroll { first_line: 1, x_cells: 0 },
            overwrite: false,
            remote: Vec::new(),
            markers: Markers::default(),
            composition: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    /// Smart: first non-whitespace, then column 1.
    Home,
    End,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
    To(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCommand {
    Insert(String),
    Newline,
    Backspace,
    Delete,
    DeleteWordLeft,
    DeleteWordRight,
    Tab,
    Outdent,
    DuplicateLine,
    DeleteLine,
    MoveLineUp,
    MoveLineDown,
    ToggleComment,
    Move { to: Motion, extend: bool },
    SelectAll,
    /// Select the word at a view offset (double-click).
    SelectWord(usize),
    /// Select the line at a view offset (triple-click).
    SelectLine(usize),
    SetSelection(Selection),
}

impl EditCfg {
    /// Indentation unit for Tab / Outdent: `\t`, or `measure.tab_size`
    /// spaces when `insert_spaces` (the controller passes the language's
    /// indent width as the tab size of the editing cfg).
    fn indent_unit(&self) -> usize {
        self.measure.tab_size.clamp(1, 16) as usize
    }
}

/// The line-comment token of an editd language (plan §4.1), for
/// [`EditCfg::line_comment`].
pub fn line_comment_for(language: &str) -> Option<&'static str> {
    match language {
        "mix" | "scene" | "mix-data" => Some("--"),
        "rust" | "c" | "cpp" | "go" | "javascript" => Some("//"),
        "shell" | "python" | "toml" | "yaml" => Some("#"),
        _ => None,
    }
}

/// Most remote-origin change markers a view keeps (oldest dropped first).
pub const MAX_MARKERS: usize = 1024;

impl EditorModel {
    /// Map selection, remote carets, markers and composition through a delta.
    ///
    /// Markers: every edit of a delta whose origin is not ced's own UI lane
    /// (`human:ced`) — remote edits, undo/redo of another lane, and Bus-driven
    /// ced edits (`agent:ced.<caller>`) — adds its inserted span (or, for a
    /// pure delete, the empty span where it happened) to
    /// [`Markers::changed`]. Reloads and resyncs add none.
    pub fn apply_delta(&mut self, d: &ViewDelta) {
        use crate::types::{DeltaKind, UI_ORIGIN};
        use cosmix_edit_core::anchor::{Bias, map_point};

        if d.kind == DeltaKind::Resync {
            // No edits: offsets survive only as numbers; `clamp` fixes them
            // against the new text. A composition cannot survive a snapshot.
            self.composition = None;
            self.preferred_cells = None;
            return;
        }
        let own = if d.kind == DeltaKind::Local { Bias::After } else { Bias::Before };
        let mark = matches!(d.kind, DeltaKind::Local | DeltaKind::Remote | DeltaKind::Undo | DeltaKind::Redo)
            && d.origin.as_ref().is_some_and(|o| o.to_string() != UI_ORIGIN);
        for e in &d.edits {
            self.sel.anchor = map_point(self.sel.anchor, own, e).0;
            self.sel.head = map_point(self.sel.head, own, e).0;
            for (_, sels) in &mut self.remote {
                for s in sels {
                    s.anchor = map_point(s.anchor, Bias::Before, e).0;
                    s.head = map_point(s.head, Bias::Before, e).0;
                }
            }
            for (r, _, _) in &mut self.markers.changed {
                *r = map_range(r.clone(), e);
            }
            if let Some(c) = self.composition.clone() {
                self.composition = if overlaps(&c, e) { None } else { Some(map_range(c, e)) };
            }
            if mark && let Some(origin) = &d.origin {
                self.markers.changed.push((e.offset..e.offset + e.insert.len(), origin.clone(), d.rev));
            }
        }
        if self.markers.changed.len() > MAX_MARKERS {
            let excess = self.markers.changed.len() - MAX_MARKERS;
            self.markers.changed.drain(..excess);
        }
        if d.kind != DeltaKind::Local && !d.edits.is_empty() {
            // Another origin's edit can move text under a remembered column.
            self.preferred_cells = self.preferred_cells.filter(|_| d.kind == DeltaKind::Remote);
        }
    }

    /// Clamp every offset to `text` (length and char boundaries) — after a
    /// `Resync` delta, whose snapshot replaced the text wholesale. Additive to
    /// the Stage S API (E1e): `apply_delta` has no text to clamp against.
    pub fn clamp(&mut self, text: &Text) {
        self.sel.anchor = clamp_offset(text, self.sel.anchor);
        self.sel.head = clamp_offset(text, self.sel.head);
        for (_, sels) in &mut self.remote {
            for s in sels {
                s.anchor = clamp_offset(text, s.anchor);
                s.head = clamp_offset(text, s.head);
            }
        }
        for (r, _, _) in &mut self.markers.changed {
            let start = clamp_offset(text, r.start);
            *r = start..clamp_offset(text, r.end).max(start);
        }
        self.scroll.first_line = self.scroll.first_line.clamp(1, text.line_count().max(1));
    }

    /// View > Clear Change Markers, and 2 s after the tab is focused (§4.5).
    pub fn clear_markers(&mut self) {
        self.markers.changed.clear();
    }

    /// The widget reported an IME preedit (`active`: non-empty preedit). The
    /// composition anchors at the current selection, which a commit replaces.
    pub fn set_preedit(&mut self, active: bool) {
        self.composition = match (active, self.composition.take()) {
            (false, _) => None,
            (true, Some(c)) => Some(c),
            (true, None) => Some(sel_range(&self.sel)),
        };
    }

    /// Run a command against the current view text. Motions and selection
    /// commands only update the model and return `None`; editing commands
    /// return the edit to hand to `Mirror::local_edit` (the model's
    /// selection is updated from the mirror's `Local` delta, not here).
    ///
    /// The controller then sets `sel = LocalEdit::caret_after` for the view
    /// that issued the edit: mapping alone cannot express "select the moved
    /// lines" or "keep the indented block selected".
    pub fn command(&mut self, text: &Text, cfg: &EditCfg, c: EditCommand) -> Option<LocalEdit> {
        self.sel.anchor = clamp_offset(text, self.sel.anchor);
        self.sel.head = clamp_offset(text, self.sel.head);
        let keep_preferred = matches!(
            c,
            EditCommand::Move { to: Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_), .. }
        );
        if !keep_preferred {
            self.preferred_cells = None;
        }
        match c {
            EditCommand::Move { to, extend } => {
                self.motion(text, cfg, to, extend);
                None
            }
            EditCommand::SelectAll => {
                self.sel = Selection { anchor: 0, head: text.len() };
                None
            }
            EditCommand::SelectWord(o) => {
                let o = clamp_offset(text, o);
                let w = cosmix_edit_core::view::word_at(text, o);
                self.sel = Selection { anchor: w.start, head: w.end };
                None
            }
            EditCommand::SelectLine(o) => {
                let line = line_of(text, clamp_offset(text, o));
                let start = text.line_start(line).unwrap_or(0);
                let end = text.line_start(line + 1).unwrap_or(text.len());
                self.sel = Selection { anchor: start, head: end };
                None
            }
            EditCommand::SetSelection(s) => {
                self.sel = Selection { anchor: clamp_offset(text, s.anchor), head: clamp_offset(text, s.head) };
                None
            }
            EditCommand::Insert(s) => self.insert(text, s),
            EditCommand::Newline => {
                let r = sel_range(&self.sel);
                let line = line_of(text, r.start);
                let ls = text.line_start(line).unwrap_or(0);
                let indent_end = first_non_ws(text, line).min(r.start);
                let s = format!("{}{}", cfg.eol, slice(text, ls..indent_end));
                Some(replace(r, s, false))
            }
            EditCommand::Backspace => {
                let r = sel_range(&self.sel);
                if !r.is_empty() {
                    return Some(replace(r, String::new(), false));
                }
                if r.start == 0 {
                    return None;
                }
                let p = cosmix_edit_core::view::prev_grapheme(text, r.start);
                Some(replace(p..r.start, String::new(), true))
            }
            EditCommand::Delete => {
                let r = sel_range(&self.sel);
                if !r.is_empty() {
                    return Some(replace(r, String::new(), false));
                }
                if r.start >= text.len() {
                    return None;
                }
                let n = cosmix_edit_core::view::next_grapheme(text, r.start);
                Some(replace(r.start..n, String::new(), true))
            }
            EditCommand::DeleteWordLeft => {
                let r = sel_range(&self.sel);
                let r = if r.is_empty() { cosmix_edit_core::view::word_prev(text, r.start).min(r.start)..r.start } else { r };
                (!r.is_empty()).then(|| replace(r, String::new(), false))
            }
            EditCommand::DeleteWordRight => {
                let r = sel_range(&self.sel);
                let r = if r.is_empty() { r.start..cosmix_edit_core::view::word_next(text, r.start).max(r.start) } else { r };
                (!r.is_empty()).then(|| replace(r, String::new(), false))
            }
            EditCommand::Tab => self.tab(text, cfg),
            EditCommand::Outdent => self.outdent(text, cfg),
            EditCommand::DuplicateLine => self.duplicate(text, cfg),
            EditCommand::DeleteLine => self.delete_lines(text),
            EditCommand::MoveLineUp => self.move_lines(text, true),
            EditCommand::MoveLineDown => self.move_lines(text, false),
            EditCommand::ToggleComment => self.toggle_comment(text, cfg),
        }
    }

    fn motion(&mut self, text: &Text, cfg: &EditCfg, to: Motion, extend: bool) {
        use cosmix_edit_core::view;
        let r = sel_range(&self.sel);
        let head = self.sel.head;
        let collapse = !extend && !r.is_empty();
        let target = match to {
            Motion::Left if collapse => r.start,
            Motion::Right if collapse => r.end,
            Motion::Left => view::prev_grapheme(text, head),
            Motion::Right => view::next_grapheme(text, head),
            Motion::WordLeft => view::word_prev(text, head).min(head),
            Motion::WordRight => view::word_next(text, head).max(head),
            Motion::Up => self.vertical(text, cfg, -1),
            Motion::Down => self.vertical(text, cfg, 1),
            Motion::PageUp(n) => {
                let n = n.max(1);
                self.scroll.first_line = self.scroll.first_line.saturating_sub(n).max(1);
                self.vertical(text, cfg, -(n as isize))
            }
            Motion::PageDown(n) => {
                let n = n.max(1);
                self.scroll.first_line = (self.scroll.first_line + n).min(text.line_count().max(1));
                self.vertical(text, cfg, n as isize)
            }
            Motion::Home => {
                let line = line_of(text, head);
                let ls = text.line_start(line).unwrap_or(0);
                let fnw = first_non_ws(text, line);
                if head == fnw { ls } else { fnw }
            }
            Motion::End => content_end(text, line_of(text, head)),
            Motion::DocStart => 0,
            Motion::DocEnd => text.len(),
            Motion::To(o) => clamp_offset(text, o),
        };
        self.sel.head = target;
        if !extend {
            self.sel.anchor = target;
        }
    }

    /// Move the head `delta` lines, keeping `preferred_cells`.
    fn vertical(&mut self, text: &Text, cfg: &EditCfg, delta: isize) -> usize {
        use cosmix_edit_core::view;
        let pos = view::visual_of(text, &cfg.measure, self.sel.head);
        let cells = *self.preferred_cells.get_or_insert(pos.cells);
        let line = pos.line as isize + delta;
        if line < 1 {
            return 0;
        }
        if line as usize > text.line_count() {
            return text.len();
        }
        let o = view::offset_at(text, &cfg.measure, line as usize, cells, view::Round::Left);
        not_inside_crlf(text, o)
    }

    fn insert(&mut self, text: &Text, s: String) -> Option<LocalEdit> {
        let mut r = sel_range(&self.sel);
        if s.is_empty() && r.is_empty() {
            return None;
        }
        let one = is_one_char(&s) && s != "\n" && s != "\r";
        if self.overwrite && r.is_empty() && one && r.start < content_end(text, line_of(text, r.start)) {
            r = r.start..cosmix_edit_core::view::next_grapheme(text, r.start);
        }
        let coalesce = one && (r.is_empty() || self.overwrite);
        Some(replace(r, s, coalesce))
    }

    fn tab(&mut self, text: &Text, cfg: &EditCfg) -> Option<LocalEdit> {
        let r = sel_range(&self.sel);
        let (l1, l2) = sel_lines(text, &r);
        if l1 != l2 {
            let unit = if cfg.insert_spaces { " ".repeat(cfg.indent_unit()) } else { "\t".to_string() };
            let items: Vec<_> = (l1..=l2)
                .filter(|&l| text.line_range(l).is_some_and(|lr| !lr.is_empty()))
                .map(|l| {
                    let ls = text.line_start(l).unwrap_or(0);
                    (ls..ls, unit.clone())
                })
                .collect();
            return self.multi(items, false);
        }
        let unit = if cfg.insert_spaces {
            let cells = cosmix_edit_core::view::visual_of(text, &cfg.measure, r.start).cells;
            let w = cfg.indent_unit();
            " ".repeat(w - cells % w)
        } else {
            "\t".to_string()
        };
        let coalesce = r.is_empty() && unit == "\t";
        Some(replace(r, unit, coalesce))
    }

    fn outdent(&mut self, text: &Text, cfg: &EditCfg) -> Option<LocalEdit> {
        let r = sel_range(&self.sel);
        let (l1, l2) = sel_lines(text, &r);
        let unit = cfg.indent_unit();
        let items: Vec<_> = (l1..=l2)
            .filter_map(|l| {
                let lr = text.line_range(l)?;
                let head = slice(text, lr.start..(lr.start + unit).min(lr.end));
                let n = if head.starts_with('\t') { 1 } else { head.bytes().take_while(|&b| b == b' ').count() };
                (n > 0).then(|| (lr.start..lr.start + n, String::new()))
            })
            .collect();
        self.multi(items, false)
    }

    fn duplicate(&mut self, text: &Text, cfg: &EditCfg) -> Option<LocalEdit> {
        let r = sel_range(&self.sel);
        if !r.is_empty() {
            let copy = slice(text, r.clone());
            let edit = LocalEdit { items: vec![(r.end..r.end, copy)], coalesce: false, caret_after: self.sel };
            return Some(edit);
        }
        let line = line_of(text, r.start);
        let ls = text.line_start(line).unwrap_or(0);
        let ce = content_end(text, line);
        let s = format!("{}{}", cfg.eol, slice(text, ls..ce));
        let caret = r.start + s.len();
        Some(LocalEdit { items: vec![(ce..ce, s)], coalesce: false, caret_after: Selection { anchor: caret, head: caret } })
    }

    fn delete_lines(&mut self, text: &Text) -> Option<LocalEdit> {
        let r = sel_range(&self.sel);
        let (l1, l2) = sel_lines(text, &r);
        let start = text.line_start(l1).unwrap_or(0);
        let range = match text.line_start(l2 + 1) {
            Some(next) => start..next,
            None if l1 > 1 => content_end(text, l1 - 1)..text.len(),
            None => 0..text.len(),
        };
        if range.is_empty() {
            return None;
        }
        let caret = range.start;
        Some(LocalEdit { items: vec![(range, String::new())], coalesce: false, caret_after: Selection { anchor: caret, head: caret } })
    }

    fn move_lines(&mut self, text: &Text, up: bool) -> Option<LocalEdit> {
        let r = sel_range(&self.sel);
        let (l1, l2) = sel_lines(text, &r);
        let count = text.line_count();
        let (first, last) = if up {
            if l1 <= 1 {
                return None;
            }
            (l1 - 1, l2)
        } else {
            if l2 >= count {
                return None;
            }
            (l1, l2 + 1)
        };
        let region_start = text.line_start(first).unwrap_or(0);
        let region_end = text.line_start(last + 1).unwrap_or(text.len());
        // The single line swapped with the block, and the block itself.
        let pivot = if up { text.line_start(l1).unwrap_or(0) } else { text.line_start(l2 + 1).unwrap_or(text.len()) };
        let (a, b) = (slice(text, region_start..pivot), slice(text, pivot..region_end));
        // `a` always ends with its eol; `b` lacks one when it is the last line.
        let (new, shift) = if b.ends_with('\n') {
            (format!("{b}{a}"), b.len())
        } else {
            let term = eol_suffix(&a);
            (format!("{b}{term}{}", &a[..a.len() - term.len()]), b.len() + term.len())
        };
        // The block moves: up to the region start, or down past the line below
        // (and the eol it gained when that line was the last).
        let caret_after = if up {
            let back = pivot - region_start;
            Selection { anchor: self.sel.anchor - back, head: self.sel.head - back }
        } else {
            Selection { anchor: self.sel.anchor + shift, head: self.sel.head + shift }
        };
        Some(LocalEdit { items: vec![(region_start..region_end, new)], coalesce: false, caret_after })
    }

    fn toggle_comment(&mut self, text: &Text, cfg: &EditCfg) -> Option<LocalEdit> {
        let token = cfg.line_comment?;
        let r = sel_range(&self.sel);
        let (l1, l2) = sel_lines(text, &r);
        let lines: Vec<(usize, String)> = (l1..=l2)
            .filter_map(|l| {
                let ls = text.line_start(l)?;
                let s = slice(text, ls..content_end(text, l));
                (!s.trim().is_empty()).then_some((ls, s))
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        let ws = |s: &str| s.len() - s.trim_start_matches([' ', '\t']).len();
        let all = lines.iter().all(|(_, s)| s[ws(s)..].starts_with(token));
        let items: Vec<_> = if all {
            lines
                .iter()
                .map(|(ls, s)| {
                    let at = ls + ws(s);
                    let rest = &s[ws(s) + token.len()..];
                    let n = token.len() + usize::from(rest.starts_with(' '));
                    (at..at + n, String::new())
                })
                .collect()
        } else {
            let indent = lines.iter().map(|(_, s)| ws(s)).min().unwrap_or(0);
            lines.iter().map(|(ls, _)| (ls + indent..ls + indent, format!("{token} "))).collect()
        };
        self.multi(items, false)
    }

    /// A multi-item edit keeping the selection over the same text: its start
    /// maps `Before` (an indent inserted at the start stays inside), its end
    /// `After`; a caret maps `After`.
    fn multi(&self, items: Vec<(Range<usize>, String)>, coalesce: bool) -> Option<LocalEdit> {
        use cosmix_edit_core::anchor::{Bias, map_point};
        if items.is_empty() {
            return None;
        }
        let seq = cosmix_edit_core::ot::txn_sequence(&items).ok()?;
        let r = sel_range(&self.sel);
        let (mut s, mut e) = (r.start, r.end);
        let sb = if r.is_empty() { Bias::After } else { Bias::Before };
        for (_, edit) in &seq {
            s = map_point(s, sb, edit).0;
            e = map_point(e, Bias::After, edit).0;
        }
        let caret_after = if self.sel.anchor <= self.sel.head { Selection { anchor: s, head: e } } else { Selection { anchor: e, head: s } };
        Some(LocalEdit { items, coalesce, caret_after })
    }
}

fn replace(r: Range<usize>, s: String, coalesce: bool) -> LocalEdit {
    let caret = r.start + s.len();
    LocalEdit { items: vec![(r, s)], coalesce, caret_after: Selection { anchor: caret, head: caret } }
}

fn sel_range(s: &Selection) -> Range<usize> {
    s.anchor.min(s.head)..s.anchor.max(s.head)
}

/// Lines a selection covers; a non-empty selection ending at a line start
/// does not cover that line.
fn sel_lines(text: &Text, r: &Range<usize>) -> (usize, usize) {
    let l1 = line_of(text, r.start);
    let mut l2 = line_of(text, r.end);
    if l2 > l1 && text.line_start(l2) == Some(r.end) {
        l2 -= 1;
    }
    (l1, l2)
}

/// Map a range through one edit as a non-expanding range anchor.
fn map_range(r: Range<usize>, e: &cosmix_edit_core::ot::Edit) -> Range<usize> {
    use cosmix_edit_core::anchor::{Bias, map_point};
    if r.is_empty() {
        let p = map_point(r.start, Bias::Before, e).0;
        return p..p;
    }
    let s = map_point(r.start, Bias::After, e).0;
    let t = map_point(r.end, Bias::Before, e).0;
    s.min(t)..t
}

/// Whether an edit touches the inside of a composition range (a delete
/// reaching into it, or an insert strictly inside it). Edits that only abut
/// it map it instead.
fn overlaps(c: &Range<usize>, e: &cosmix_edit_core::ot::Edit) -> bool {
    let (p, end) = (e.offset, e.offset + e.delete);
    if e.delete > 0 {
        // An empty composition (a preedit at a caret) is hit only by a delete
        // spanning its point; a non-empty one by any delete reaching into it.
        return if c.is_empty() { p < c.start && end > c.start } else { p < c.end && end > c.start };
    }
    p > c.start && p < c.end
}

/// 1-based line containing `offset` (binary search over the line index).
pub fn line_of(text: &Text, offset: usize) -> usize {
    let (mut lo, mut hi) = (1, text.line_count().max(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if text.line_start(mid).is_some_and(|s| s <= offset) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

fn byte_at(text: &Text, o: usize) -> Option<u8> {
    text.chunk_at(o).first().copied()
}

/// `offset` clamped to the text and moved back to a char boundary.
pub fn clamp_offset(text: &Text, offset: usize) -> usize {
    let mut o = offset.min(text.len());
    while o > 0 && !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

/// The end of 1-based `line`'s content: before its `\n`, and before a `\r`
/// that precedes that `\n` (the caret never sits inside a CRLF).
pub fn content_end(text: &Text, line: usize) -> usize {
    let Some(r) = text.line_range(line) else { return text.len() };
    if r.end > r.start && byte_at(text, r.end - 1) == Some(b'\r') && byte_at(text, r.end) == Some(b'\n') { r.end - 1 } else { r.end }
}

fn not_inside_crlf(text: &Text, o: usize) -> usize {
    if o > 0 && byte_at(text, o - 1) == Some(b'\r') && byte_at(text, o) == Some(b'\n') { o - 1 } else { o }
}

/// Offset of the first non-blank byte of `line` (its content end when blank).
fn first_non_ws(text: &Text, line: usize) -> usize {
    let ls = text.line_start(line).unwrap_or(0);
    let ce = content_end(text, line);
    let mut o = ls;
    while o < ce && matches!(byte_at(text, o), Some(b' ' | b'\t')) {
        o += 1;
    }
    o
}

fn slice(text: &Text, r: Range<usize>) -> String {
    let mut s = String::with_capacity(r.len());
    text.read(r, &mut s);
    s
}

fn eol_suffix(s: &str) -> &'static str {
    if s.ends_with("\r\n") {
        "\r\n"
    } else if s.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn is_one_char(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some() && c.next().is_none()
}

#[cfg(test)]
mod tests;
