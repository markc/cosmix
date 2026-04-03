//! Progress bar dialog.

use iced::widget::{button, column, container, progress_bar, row, text, Space};
use iced::{Element, Length};

use crate::theme;
use crate::Message;

pub fn view<'a>(msg_text: &'a str, fraction: f32, pulsate: bool) -> Element<'a, Message> {
    let label = text(msg_text).size(15).color(theme::FG_PRIMARY);

    let bar = if pulsate {
        progress_bar(0.0..=1.0, (fraction * 3.0) % 1.0)
    } else {
        progress_bar(0.0..=1.0, fraction)
    };

    let pct_text = if pulsate {
        text("Working...").size(12).color(theme::FG_SECONDARY)
    } else {
        text(format!("{:.0}%", fraction * 100.0)).size(12).color(theme::FG_SECONDARY)
    };

    let cancel_btn = button(text("Cancel").size(14).center())
        .on_press(Message::Cancel)
        .padding([8, 20])
        .height(34)
        .style(theme::btn_secondary);

    let footer = container(
        row![Space::new().width(Length::Fill), cancel_btn],
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::dialog_footer);

    container(
        column![
            container(column![label, bar, pct_text].spacing(8))
                .padding([16, 20])
                .width(Length::Fill)
                .height(Length::Fill),
            footer,
        ],
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::dialog_frame)
    .into()
}
