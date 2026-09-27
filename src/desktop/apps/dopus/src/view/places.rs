//! The Places sidebar: a plain vertical strip (furniture policy — NOT a
//! carousel) of the core's Places list — Home, the filesystem root, then the
//! XDG user directories that exist ([`cosmix_dopus_core::places`], from
//! browser.rs:1267-1284). A click navigates the active pane; the active
//! pane's current directory highlights.

use iced::widget::{Scrollable, Space, button, column, container, image, row, text};
use iced::{Border, Element, Length, Padding};

use cosmix_dopus_core::{PaneId, PaneModel};

use crate::app::Msg;
use crate::icons::{self, Icon, Icons};
use crate::view::Look;

/// Sidebar width, logical px. The panes row starts here — the divider's
/// drag geometry depends on this constant (see `panes::Divider`).
pub const PLACES_W: f32 = 150.0;

/// An icon per place name. The core's names are fixed (`places`), so the
/// mapping is exhaustive over what it can produce; an unknown name falls
/// back to the folder icon.
fn place_icon(name: &str) -> Icon {
    match name {
        "Home" => Icon::House,
        "Filesystem" => Icon::HardDrive,
        "Desktop" => Icon::Grid,
        "Documents" => Icon::FileText,
        "Downloads" => Icon::Download,
        "Music" => Icon::Music,
        "Pictures" => Icon::FileImage,
        "Videos" => Icon::FileVideo,
        _ => Icon::Folder,
    }
}

/// The sidebar strip. `active` is the pane a click navigates; its current
/// directory (and only its) highlights when it matches a place.
pub fn sidebar<'a>(
    look: Look,
    icons: &'a Icons,
    tint: &'a str,
    active: PaneId,
    pane: &'a PaneModel,
    places: &'a [(&'static str, std::path::PathBuf)],
) -> Element<'a, Msg> {
    let mut list = column![
        button(
            text("Places")
                .font(look.ui_font)
                .size(look.px * 0.8)
                .color(look.tokens.muted_text)
        )
        .on_press(Msg::RefreshPlaces)
        .style(place_look(&look, false)),
        Space::new().height(Length::Fixed(4.0)),
    ]
    .padding(Padding {
        top: 8.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    })
    .spacing(2)
    .width(Length::Fill);
    for (name, path) in places {
        let selected = pane.path == *path;
        let label = super::elide::Label {
            text: (*name).to_owned(),
            font: look.ui_font,
            px: look.px * 0.9,
            color: if selected {
                look.tokens.primary_text
            } else {
                look.chrome.secondary_text
            },
        };
        let style = place_look(&look, selected);
        list = list.push(
            button(
                row![image_widget(icons, tint, place_icon(name)), label]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
            )
            .padding([4, 10])
            .width(Length::Fill)
            .on_press(Msg::Go(active, path.clone()))
            .style(style),
        );
    }
    container(Scrollable::new(list))
        .width(Length::Fixed(PLACES_W))
        .height(Length::Fill)
        .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
        .into()
}

/// A quiet sidebar entry; the selected place stays legible against the
/// secondary strip. Colours are `Copy` tokens (the ced `Look::flat` shape).
fn place_look(
    look: &Look,
    selected: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
    let (accent_bg, hover, text, muted, radius) = (
        look.tokens.primary,
        look.tokens.muted_surface,
        look.tokens.primary_text,
        look.tokens.muted_text,
        look.tokens.radius,
    );
    move |_theme, status| button::Style {
        background: match (selected, status) {
            (true, _) => Some(accent_bg.into()),
            (false, button::Status::Hovered | button::Status::Pressed) => Some(hover.into()),
            (false, _) => None,
        },
        text_color: if selected { text } else { muted },
        border: Border {
            radius: radius.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A cached icon handle as an iced image widget, 16 px (the header's shape;
/// `view::mod` shares this one — kept here so the sidebar is self-contained).
pub fn image_widget(icons: &Icons, tint: &str, icon: Icon) -> Element<'static, Msg> {
    match icons.get(icon, tint, icons::RASTER_PX) {
        Some(handle) => image(handle)
            .width(Length::Fixed(16.0))
            .height(Length::Fixed(16.0))
            .into(),
        None => container(Space::new())
            .width(Length::Fixed(16.0))
            .height(Length::Fixed(16.0))
            .into(),
    }
}
