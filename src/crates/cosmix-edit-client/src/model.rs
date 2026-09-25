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

impl EditorModel {
    /// Map selection, remote carets, markers and composition through a delta.
    pub fn apply_delta(&mut self, d: &ViewDelta) {
        let _ = d;
        todo!("ced E1e")
    }

    /// Run a command against the current view text. Motions and selection
    /// commands only update the model and return `None`; editing commands
    /// return the edit to hand to `Mirror::local_edit` (the model's
    /// selection is updated from the mirror's `Local` delta, not here).
    pub fn command(&mut self, text: &Text, cfg: &EditCfg, c: EditCommand) -> Option<LocalEdit> {
        let _ = (text, cfg, c);
        todo!("ced E1e")
    }
}
