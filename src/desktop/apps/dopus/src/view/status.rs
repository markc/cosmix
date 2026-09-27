//! The status bar: the core's info line (what is selected) on the left; the
//! pane's own status and the directory summary on the right. Panel buttons
//! show open/closed state; timestamps use the core's absolute formatter.

use iced::widget::{button, container, row, text};
use iced::{Element, Length};

use cosmix_dopus_core::PaneModel;

use crate::app::Msg;
use crate::view::Look;

/// The status bar strip.
pub fn bar<'a>(
    look: Look,
    pane: &'a PaneModel,
    info: &'a str,
    places_open: bool,
    properties_open: bool,
) -> Element<'a, Msg> {
    let summary = cosmix_dopus_core::pane_summary(&pane.root);
    container(row![
        button(
            text(format!(
                "{} Places (F13)",
                if places_open { "●" } else { "○" }
            ))
            .font(look.ui_font)
            .size(look.small_px)
        )
        .padding(look.chrome.small)
        .style(super::button_look(&look))
        .on_press(Msg::Actions(vec![cosmix_actions::view::TOGGLE_PLACES])),
        button(
            text(format!(
                "{} Properties (F14)",
                if properties_open { "●" } else { "○" }
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
            px: look.px * 0.85,
            color: look.chrome.secondary_text
        },
        text(summary)
            .font(look.mono_font)
            .size(look.mono_px * 0.85)
            .color(look.tokens.muted_text),
    ])
    .width(Length::Fill)
    .padding([0.0, look.chrome.pad])
    .align_y(iced::Alignment::Center)
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}
