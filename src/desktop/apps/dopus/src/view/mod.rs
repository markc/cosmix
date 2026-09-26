//! Window composition: path header (navigation) · sort headers · the
//! [`rows::FileList`] · the status bar. Built-ins everywhere except the list;
//! every colour from the compiled tokens via [`Look`].

pub mod rows;
pub mod status;

use iced::widget::{button, column, container, image, row, text, Space};
use iced::{Border, Element, Length};
use iced_tiny_skia::Renderer;

use cosmix_actions::{filemgr, ActionId};
use cosmix_dopus_core::{PaneModel, VisibleRow};
use cosmix_iced_widgets::Tokens;

use crate::app::Msg;
use crate::icons::{self, Icons};
use crate::theme::Chrome;

/// What the view draws with: the compiled tokens plus the resolved fonts.
/// Passed by reference through every view fn (the ced `chrome::Look` shape).
pub struct Look<'a> {
    pub tokens: &'a Tokens,
    pub chrome: &'a Chrome,
    pub ui_font: iced::Font,
    pub mono_font: iced::Font,
    pub px: f32,
    pub mono_px: f32,
}

impl Look<'_> {
    /// A full-width strip (header, status bar) in the given token colours.
    pub fn strip(
        &self,
        background: iced::Color,
        text_color: iced::Color,
    ) -> impl Fn(&iced::Theme) -> container::Style + '_ {
        move |_| container::Style {
            background: Some(background.into()),
            text_color: Some(text_color),
            ..Default::default()
        }
    }
}

/// Header height, logical px.
pub const HEADER_H: f32 = 34.0;
/// Sort-header height.
pub const SORT_H: f32 = 26.0;
/// Status bar height.
pub const STATUS_H: f32 = 26.0;

/// The whole window: header strips, the listing, the status bar.
pub fn root<'a>(
    look: &'a Look<'a>,
    icons: &'a Icons,
    tint: &'a str,
    pane: &'a PaneModel,
    rows: &'a [VisibleRow],
    info: &'a str,
) -> Element<'a, Msg> {
    column![
        header(look, icons, tint, pane),
        sort_header(look, pane),
        rows::FileList::new(rows, pane.selected.as_deref(), &pane.expanded, icons, tint, look),
        status::bar(look, pane, info),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The navigation strip: back / forward / parent / home / refresh /
/// toggle-hidden icon buttons, then the sanitised path.
fn header<'a>(look: &'a Look<'a>, icons: &'a Icons, tint: &'a str, pane: &'a PaneModel) -> Element<'a, Msg> {
    let icon_button = |icon: icons::Icon, action: ActionId| {
        let look = button_look(look);
        button(
            image_widget(icons, tint, icon)
                .unwrap_or_else(|| Space::new(Length::Fixed(16.0), Length::Fixed(16.0))),
        )
        .padding(4)
        .on_press_maybe(
            availability(pane, action)
                .then_some(Msg::Actions(vec![action])),
        )
        .style(look)
    };
    container(
        row![
            icon_button(icons::Icon::ArrowLeft, filemgr::NAV_BACK),
            icon_button(icons::Icon::ArrowRight, filemgr::NAV_FORWARD),
            icon_button(icons::Icon::ArrowUp, filemgr::NAV_PARENT),
            icon_button(icons::Icon::House, filemgr::NAV_HOME),
            icon_button(icons::Icon::Refresh, filemgr::VIEW_REFRESH),
            icon_button(
                if pane.show_hidden { icons::Icon::EyeOff } else { icons::Icon::Eye },
                filemgr::VIEW_TOGGLE_HIDDEN
            ),
            text(cosmix_dopus_core::sanitise_display_path(&pane.path))
                .font(look.mono_font)
                .size(look.mono_px)
                .color(look.chrome.secondary_text),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(HEADER_H))
    .padding([0, 8])
    .align_y(iced::Alignment::Center)
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}

/// A directory can only be left when there is somewhere to go; the rest are
/// always offered (the core re-checks and status-lines the no-ops).
fn availability(pane: &PaneModel, action: ActionId) -> bool {
    if action == filemgr::NAV_PARENT {
        return pane.path.parent().is_some();
    }
    true
}

/// A ghost button style over the secondary strip: quiet until hovered.
fn button_look(
    look: &Look<'_>,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style + '_ {
    let (text, hover, radius) = (look.chrome.secondary_text, look.tokens.muted_surface, look.tokens.radius);
    move |_theme, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(hover.into()),
            _ => None,
        },
        text_color: Some(text),
        border: Border { radius: radius.into(), ..Default::default() },
        ..Default::default()
    }
}

/// A cached icon handle as an iced image widget, at the header's 16 px.
fn image_widget(
    icons: &Icons,
    tint: &str,
    icon: icons::Icon,
) -> Option<iced::widget::Image<iced::widget::image::Handle>> {
    let handle = icons.get(icon, tint, 32)?;
    Some(image(handle).width(Length::Fixed(16.0)).height(Length::Fixed(16.0)))
}

/// The sort headers: the three columns as buttons publishing the
/// `view.sort-*` actions; the active column shows its direction. The two
/// secondary columns are fixed-width, mirroring the row layout's right edge
/// ([`rows::SIZE_W`] / [`rows::MODIFIED_W`]).
fn sort_header<'a>(look: &'a Look<'a>, pane: &'a PaneModel) -> Element<'a, Msg> {
    let header_button = |label: &str, action: ActionId, column_sort| {
        let look = button_look(look);
        let active = pane.sort == column_sort;
        let label = if active {
            format!("{label} {}", if pane.ascending { "↑" } else { "↓" })
        } else {
            label.to_owned()
        };
        button(
            text(label)
                .font(look.ui_font)
                .size(look.px * 0.85)
                .color(if active { look.chrome.secondary_text } else { look.tokens.muted_text }),
        )
        .padding([2, 6])
        .on_press(Msg::Actions(vec![action]))
        .style(look)
    };
    container(
        row![
            header_button("Name", filemgr::VIEW_SORT_NAME, cosmix_dopus_core::SortColumn::Name),
            Space::new(Length::Fill, Length::Fixed(0.0)),
            header_button("Size", filemgr::VIEW_SORT_SIZE, cosmix_dopus_core::SortColumn::Size)
                .width(Length::Fixed(rows::SIZE_W)),
            Space::new(Length::Fixed(rows::GAP), Length::Fixed(0.0)),
            header_button("Modified", filemgr::VIEW_SORT_MODIFIED, cosmix_dopus_core::SortColumn::Modified)
                .width(Length::Fixed(rows::MODIFIED_W + 4.0)),
        ]
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(SORT_H))
    .padding([0, 8])
    .align_y(iced::Alignment::Center)
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}
