//! Info / Warning / Error message dialog.

use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Element, Length};

use crate::theme;
use crate::Message;

pub fn view<'a>(msg_text: &'a str, level: &'a str) -> Element<'a, Message> {
    let icon_text = match level {
        "warning" => "⚠",
        "error" => "✕",
        _ => "ℹ",
    };
    let icon_color = match level {
        "warning" => theme::WARNING,
        "error" => theme::DANGER,
        _ => theme::INFO,
    };

    let icon = text(icon_text).size(28).color(icon_color);
    let label = text(msg_text).size(15).color(theme::FG_PRIMARY);

    let body = row![icon, label]
        .spacing(12)
        .align_y(Alignment::Center);

    let ok_btn = button(text("OK").size(14).center())
        .on_press(Message::Submit)
        .padding([8, 24])
        .height(34)
        .style(theme::btn_primary);

    let footer = container(
        row![Space::new().width(Length::Fill), ok_btn],
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::dialog_footer);

    container(
        column![
            container(body).padding([16, 20]).width(Length::Fill).height(Length::Fill),
            footer,
        ],
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::dialog_frame)
    .into()
}
