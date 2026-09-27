//! Window composition: the Places sidebar · left pane · divider · right
//! pane, over the status bar — [`places`] · [`panes::pane_column`] (pane
//! header, [`location`] bar, sort header, [`rows::FileList`]) ·
//! [`panes::Divider`] · [`status::bar`] — with a [`dialogs`] modal card
//! stacked over it all while a core reservation is unanswered. Built-ins
//! everywhere except the list and the divider; every colour from the
//! compiled tokens via [`Look`].

pub mod dialogs;
pub mod columns;
pub mod elide;
pub mod location;
pub mod panes;
pub mod places;
pub mod rows;
pub mod status;

use iced::widget::{column, container, row};
use iced::{Element, Length};

use cosmix_dopus_core::{PaneId, PaneModel, VisibleRow};

use crate::app::Msg;
use crate::icons::Icons;
use crate::theme::Chrome;

/// What the view draws with: the compiled tokens plus the resolved fonts.
/// Passed by value through every view fn (the ced `chrome::Look` shape —
/// everything is `Copy`, so styling closures capture copies and stay
/// `'static` instead of borrowing a local `Look`).
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub tokens: cosmix_iced_widgets::Tokens,
    pub chrome: Chrome,
    pub ui_font: iced::Font,
    pub mono_font: iced::Font,
    pub px: f32,
    pub mono_px: f32,
}

impl Look {
    /// A full-width strip (headers, status bar) in the given token colours.
    pub fn strip(
        &self,
        background: iced::Color,
        text_color: iced::Color,
    ) -> impl Fn(&iced::Theme) -> container::Style + 'static {
        move |_| container::Style {
            background: Some(background.into()),
            text_color: Some(text_color),
            ..Default::default()
        }
    }
}

/// Pane header height (nav strip + location bar), logical px.
pub const HEADER_H: f32 = 34.0;
/// Sort-header height.
pub const SORT_H: f32 = 26.0;
/// Status bar height.
pub const STATUS_H: f32 = 26.0;

/// The whole window: sidebar · left pane · divider · right pane, then the
/// status bar. `split_ratio` (the core's live value) quantises the pane
/// Fill portions; `editing` is `(pane, real path text)` while a location
/// bar is being edited; the listed `rows` are the app's per-pane snapshots;
/// `dialog` is the outstanding core reservation rendered as a modal card
/// over a scrim (nothing else on this surface while it is up).
// The window's whole projection in one call (ced's editor/draw.rs precedent
// for the allow).
#[allow(clippy::too_many_arguments)]
pub fn root<'a>(
    look: Look,
    icons: &'a Icons,
    tint: &'a str,
    active: PaneId,
    split_ratio: f32,
    left: &'a PaneModel,
    right: &'a PaneModel,
    left_rows: &'a [VisibleRow],
    right_rows: &'a [VisibleRow],
    editing: Option<(PaneId, &'a str)>,
    info: &'a str,
    dialog: Option<&'a dialogs::Dialog>,
    places: &'a [(&'static str, std::path::PathBuf)],
) -> Element<'a, Msg> {
    let (left_edit, right_edit) = match editing {
        Some((PaneId::Left, text)) => (Some(text), None),
        Some((PaneId::Right, text)) => (None, Some(text)),
        None => (None, None),
    };
    let (active_pane, _active_rows) = match active {
        PaneId::Left => (left, left_rows),
        PaneId::Right => (right, right_rows),
    };
    // The ratio quantised to whole Fill portions out of 100 (the drag clamp
    // already keeps it in 0.1–0.9, so both sides get at least 10).
    let left_portion = (split_ratio.clamp(panes::SPLIT_MIN, panes::SPLIT_MAX) * 100.0).round() as u16;
    let content = column![
        row![
            places::sidebar(look, icons, tint, active, active_pane, places),
            panes::pane_column(
                look, icons, tint, PaneId::Left, left, left_rows, left_portion, active == PaneId::Left, left_edit,
            ),
            panes::Divider::new(&look),
            panes::pane_column(
                look, icons, tint, PaneId::Right, right, right_rows, 100 - left_portion, active == PaneId::Right,
                right_edit,
            ),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(iced::Alignment::Start),
        status::bar(look, active_pane, info),
    ]
    .width(Length::Fill)
    .height(Length::Fill);
    match dialog {
        // The modal card is stacked OVER the window; the scrim takes every
        // click not on the card, and the router's modal scope takes every
        // chord plus Enter/Escape.
        Some(dialog) => iced::widget::stack![content, dialogs::Dialog::view(dialog, look)].into(),
        None => content.into(),
    }
}

/// A ghost button style over the secondary strip: quiet until hovered. The
/// colours are `Copy` tokens, so the closure captures values and is
/// `'static` (the ced `chrome::Look::flat` shape).
pub fn button_look(look: &Look) -> impl Fn(&iced::Theme, iced::widget::button::Status) -> iced::widget::button::Style + 'static {
    let (text, hover, radius) = (look.chrome.secondary_text, look.tokens.muted_surface, look.tokens.radius);
    move |_theme, status| iced::widget::button::Style {
        background: match status {
            iced::widget::button::Status::Hovered | iced::widget::button::Status::Pressed => Some(hover.into()),
            _ => None,
        },
        text_color: text,
        border: iced::Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    }
}

/// A cached icon handle as an iced image widget, at the header's 16 px; a
/// blank 16 px filler while the rasterisation is still in flight.
pub fn image_widget(icons: &Icons, tint: &str, icon: crate::icons::Icon) -> Element<'static, Msg> {
    places::image_widget(icons, tint, icon)
}
