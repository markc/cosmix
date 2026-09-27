//! Shared status and panel toggles. Directory summaries belong to each pane.

use iced::widget::{button, container, row, text};
use iced::{Element, Length};

use crate::app::Msg;
use crate::view::Look;

/// The status bar strip.
pub fn bar<'a>(
    look: Look,
    info: &'a str,
    places_open: bool,
    properties_open: bool,
    actions: &[crate::verbs::ActionRow],
) -> Element<'a, Msg> {
    container(row![
        button(
            text(format!(
                "{} {}",
                if places_open { "●" } else { "○" },
                super::tips::action_label(actions, cosmix_actions::view::TOGGLE_PLACES, "Places"),
            ))
            .font(look.ui_font)
            .size(look.small_px)
        )
        .padding(look.chrome.small)
        .style(super::button_look(&look))
        .on_press(Msg::Actions(vec![cosmix_actions::view::TOGGLE_PLACES])),
        button(
            text(format!(
                "{} {}",
                if properties_open { "●" } else { "○" },
                super::tips::action_label(
                    actions,
                    cosmix_actions::view::TOGGLE_PROPERTIES,
                    "Properties"
                ),
            ))
            .font(look.ui_font)
            .size(look.small_px)
        )
        .padding(look.chrome.small)
        .style(super::button_look(&look))
        .on_press(Msg::Actions(vec![cosmix_actions::view::TOGGLE_PROPERTIES])),
        super::elide::Label {
            text: info.into(),
            font: look.ui_font,
            px: look.small_px,
            color: look.chrome.secondary_text
        },
    ])
    .width(Length::Fill)
    .padding([0.0, look.chrome.pad])
    .align_y(iced::Alignment::Center)
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}
