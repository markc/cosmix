//! The editor widget and its messages (ced E1 plan §4.2). Stage S freezes the
//! message type, the palette, the layout report and the widget's constructor
//! signature (a compiling stub, so E1f's `app.rs` builds before E1e lands);
//! Stage E1e replaces the widget's body WITHOUT changing the signature (a
//! change needs the lead's sign-off — E1f composes it).

mod draw;
mod ime;
mod input;
pub mod layout;
pub mod lines;
pub mod widget;

use cosmix_edit_client::highlight::HlClass;
use cosmix_edit_client::model::{EditCommand, Scroll};

/// What the editor widget reports to the app.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorMsg {
    /// A keyboard- or mouse-derived editing / motion command.
    Command(EditCommand),
    /// The view scrolled.
    Scrolled(Scroll),
    Copy,
    Cut,
    /// Paste from the clipboard (`primary`: the middle-click selection).
    Paste { primary: bool },
    /// IME composition changed (preedit text; empty = cancelled).
    Preedit(String),
    /// IME committed text.
    ImeCommit(String),
    /// The widget gained / lost keyboard focus.
    Focus(bool),
    /// Geometry of the frame just laid out (feeds `ced.layout`).
    Layout(LayoutReport),
}

/// Engine geometry of the last frame, logical px (plan §4.8 `ced.layout`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LayoutReport {
    pub editor: [f32; 4],
    pub gutter_w: f32,
    pub line_height: f32,
    pub cell_w: f32,
    pub first_line: usize,
    pub visible_rows: usize,
    pub caret: [f32; 4],
}

/// Colours the widget draws with, built by `theme.rs` from cosmix-design
/// tokens (D17) — never literal colours in the widget.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub background: iced::Color,
    pub text: iced::Color,
    pub gutter_background: iced::Color,
    pub gutter_text: iced::Color,
    pub current_line: iced::Color,
    pub selection: iced::Color,
    pub caret: iced::Color,
    /// Origin colours: other `human:*` origins / `agent:*` origins.
    pub human_other: iced::Color,
    pub agent: iced::Color,
    pub error: iced::Color,
    pub warning: iced::Color,
    pub note: iced::Color,
    /// Indexed by `HlClass as usize`.
    pub highlight: [iced::Color; HL_CLASSES],
}

/// How the view is drawn beyond the text and colours: the Mono font at the
/// current zoom, the measurement settings and the View toggles. Built by the
/// app from the theme and `ced.conf.mix` every frame (E1f addition, additive
/// to the frozen `EditorWidget::new`).
#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    pub font: iced::Font,
    /// Text size, logical px (the Mono role, config `font_px`, zoom).
    pub px: f32,
    /// Line height as a multiple of `px`.
    pub line_height: f32,
    pub measure: cosmix_edit_core::view::MeasureCfg,
    pub whitespace: bool,
    pub line_numbers: bool,
    pub remote_carets: bool,
    /// The window has focus and no chrome field holds the keyboard.
    pub focused: bool,
}

impl Default for EditorView {
    fn default() -> Self {
        Self {
            font: iced::Font::MONOSPACE,
            px: 16.0,
            line_height: 1.3,
            measure: cosmix_edit_core::view::MeasureCfg { tab_size: 4, ambiguous_wide: false },
            whitespace: false,
            line_numbers: true,
            remote_carets: true,
            focused: true,
        }
    }
}

/// Number of [`HlClass`] variants.
pub const HL_CLASSES: usize = 17;

impl Palette {
    pub fn hl(&self, class: HlClass) -> iced::Color {
        self.highlight[class as usize]
    }
}

const _: () = assert!(HlClass::Invalid as usize + 1 == HL_CLASSES, "HL_CLASSES must match HlClass");
