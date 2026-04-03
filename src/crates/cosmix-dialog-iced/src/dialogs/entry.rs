//! Single-line text entry dialog.

use iced::widget::{button, column, container, row, text, text_input, Space};
use iced::{Element, Length};

use crate::theme;
use crate::Message;

pub fn view<'a>(prompt: &'a str, value: &'a str, placeholder: &'a str) -> Element<'a, Message> {
    let label = text(prompt).size(15).color(theme::FG_PRIMARY);

    let input = text_input(placeholder, value)
        .on_input(Message::InputChanged)
        .on_submit(Message::Submit)
        .size(14)
        .padding(8);

    let cancel_btn = button(text("Cancel").size(14).center())
        .on_press(Message::Cancel)
        .padding([8, 20])
        .height(34)
        .style(theme::btn_secondary);

    let ok_btn = button(text("OK").size(14).center())
        .on_press(Message::Submit)
        .padding([8, 24])
        .height(34)
        .style(theme::btn_primary);

    let footer = container(
        row![Space::new().width(Length::Fill), cancel_btn, ok_btn].spacing(8),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(theme::dialog_footer);

    container(
        column![
            container(column![label, input].spacing(8))
                .padding([16, 20])
                .width(Length::Fill)
                .height(Length::Fill)
                .center_y(Length::Fill),
            footer,
        ],
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::dialog_frame)
    .into()
}
