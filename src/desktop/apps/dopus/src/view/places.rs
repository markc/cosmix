//! The Places sidebar: a plain vertical strip (furniture policy — NOT a
//! carousel) of the core's Places list — Home, the filesystem root, then the
//! XDG user directories that exist ([`cosmix_dopus_core::places`], from
//! browser.rs:1267-1284). A click navigates the active pane; the active
//! pane's current directory highlights.

use iced::widget::{Scrollable, Space, button, column, container, image, row};
use iced::{Border, Element, Length, Padding};

use cosmix_dopus_core::{PaneId, PaneModel};

use crate::app::Msg;
use crate::icons::{self, Icon, Icons};
use crate::view::Look;

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
#[allow(clippy::too_many_arguments)] // shared measured geometry plus the existing pane inputs
pub fn sidebar<'a>(
    look: Look,
    first_row: super::FirstRow,
    icons: &'a Icons,
    tint: &'a str,
    active: PaneId,
    pane: &'a PaneModel,
    places: &'a [(&'static str, std::path::PathBuf)],
    actions: &[crate::verbs::ActionRow],
) -> Element<'a, Msg> {
    let mut list = column![]
        .padding(Padding {
            top: first_row.places_top,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .spacing(look.chrome.edge * 2.0)
        .width(Length::Fill);
    for (name, path) in places {
        let selected = pane.path == *path;
        let label = super::elide::Label {
            text: (*name).to_owned(),
            font: look.ui_font,
            px: look.sidebar_px(),
            color: if selected {
                look.tokens.selection_text
            } else {
                look.chrome.secondary_text
            },
        };
        let style = place_look(&look, selected);
        let entry = button(
            row![image_widget(look, icons, tint, place_icon(name)), label]
                .spacing(look.chrome.pad)
                .align_y(iced::Alignment::Center),
        )
        .padding([look.chrome.small, look.chrome.pad])
        .width(Length::Fill)
        .on_press(Msg::Go(active, path.clone()))
        .style(style);
        let label = if *name == "Home" {
            super::tips::action_label(actions, cosmix_actions::filemgr::NAV_HOME, name)
        } else {
            (*name).to_owned()
        };
        list = list.push(super::tips::tip(
            look,
            entry,
            format!(
                "{label}: {}",
                cosmix_dopus_core::sanitise_display_path(path)
            ),
        ));
    }
    container(Scrollable::new(list))
        .width(Length::Fill)
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
        look.tokens.selection,
        look.tokens.muted_surface,
        look.tokens.selection_text,
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
pub fn image_widget(look: Look, icons: &Icons, tint: &str, icon: Icon) -> Element<'static, Msg> {
    match icons.get(icon, tint, icons::RASTER_PX) {
        Some(handle) => image(handle)
            .width(Length::Fixed(look.chrome.icon))
            .height(Length::Fixed(look.chrome.icon))
            .into(),
        None => container(Space::new())
            .width(Length::Fixed(look.chrome.icon))
            .height(Length::Fixed(look.chrome.icon))
            .into(),
    }
}
