//! Yes / No / Cancel question dialog.

use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Element, Length};

use crate::theme;
use crate::Message;

pub fn view<'a>(
    msg_text: &'a str,
    yes_label: &'a str,
    no_label: &'a str,
    show_cancel: bool,
) -> Element<'a, Message> {
    let icon = text("?").size(28).color(theme::INFO);
    let label = text(msg_text).size(15).color(theme::FG_PRIMARY);

    let body = row![icon, label]
        .spacing(12)
        .align_y(Alignment::Center);

    let yes_btn = button(text(yes_label).size(14).center())
        .on_press(Message::Submit)
        .padding([8, 20])
        .height(34)
        .style(theme::btn_primary);

    let no_btn = button(text(no_label).size(14).center())
        .on_press(Message::Cancel)
        .padding([8, 20])
        .height(34)
        .style(theme::btn_secondary);

    let mut buttons = row![].spacing(8);

    if show_cancel {
        let cancel_btn = button(text("Cancel").size(14).center())
            .on_press(Message::Dismiss)
            .padding([6, 20])
            .style(theme::btn_secondary);
        buttons = buttons.push(cancel_btn);
    }

    buttons = buttons
        .push(Space::new().width(Length::Fill))
        .push(no_btn)
        .push(yes_btn);

    let footer = container(buttons)
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
