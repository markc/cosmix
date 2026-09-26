//! The status bar: the core's info line (what is selected) on the left; the
//! pane's own status and the directory summary on the right. The relative
//! modified times inside the info line are formatted by the core's helpers
//! against the app's clock — the app re-renders on its 200 ms tick
//! (app contract, law 1: the app's own clock, never a cached string that
//! ages silently).

use iced::widget::{container, row, text, Space};
use iced::{Element, Length};

use cosmix_dopus_core::PaneModel;

use crate::app::Msg;
use crate::view::Look;

/// The status bar strip.
pub fn bar<'a>(look: Look, pane: &'a PaneModel, info: &'a str) -> Element<'a, Msg> {
    let summary = cosmix_dopus_core::pane_summary(&pane.root);
    container(
        row![
            text(info).font(look.ui_font).size(look.px * 0.85).color(look.chrome.secondary_text),
            container(Space::new()).width(Length::Fill),
            text(summary).font(look.mono_font).size(look.mono_px * 0.85).color(look.tokens.muted_text),
        ],
    )
    .width(Length::Fill)
    .height(Length::Fixed(crate::view::STATUS_H))
    .padding([0, 8])
    .align_y(iced::Alignment::Center)
    .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
    .into()
}
