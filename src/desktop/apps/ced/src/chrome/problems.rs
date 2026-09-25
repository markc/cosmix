//! The Problems panel (ced E1 plan §4.10): the active tab's lint
//! diagnostics, one row each — severity, `line:col`, code, message, hint.
//! Clicking a row moves the caret to it.

use cosmix_edit_client::diag::{Diagnostic, Severity};
use iced::widget::{button, column, container, row, scrollable};
use iced::{Alignment, Element, Length, Padding};

use super::{Look, PANEL_H};
use crate::app::Msg;

/// The severity word and whether it draws in the alarm colour.
pub fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

/// `items` are the tab's diagnostics; `col_of(offset)` gives the editd col.
pub fn view<'a>(look: Look, items: &'a [Diagnostic], col_of: impl Fn(usize) -> usize, note: Option<&'a str>) -> Element<'a, Msg> {
    let t = look.tokens;
    let mut list = column![].spacing(1);
    if items.is_empty() {
        list = list.push(
            container(look.small(note.unwrap_or("No problems.")).color(t.muted_text)).padding(Padding::from([6, 12])),
        );
    }
    for d in items {
        let colour = match d.severity {
            Severity::Error => t.destructive,
            Severity::Warning => look.chrome.warning,
            Severity::Note => t.muted_text,
        };
        let mut line = row![
            look.small(severity_label(d.severity)).color(colour).width(Length::Fixed(64.0)),
            look.code(format!("{}:{}", d.line, col_of(d.range.start))).color(t.muted_text).width(Length::Fixed(72.0)),
            look.code(d.code.as_str()).color(t.muted_text).width(Length::Fixed(120.0)),
            look.small(d.message.as_str()),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(hint) = &d.hint {
            line = line.push(look.small(format!("— {hint}")).color(t.muted_text));
        }
        list = list.push(
            button(line).width(Length::Fill).padding(Padding::from([3, 12])).style(look.flat()).on_press(Msg::GotoOffset(d.range.start)),
        );
    }
    panel(look, "Problems", scrollable(list).height(Length::Fill).into())
}

/// A titled bottom panel.
pub fn panel<'a>(look: Look, title: &'a str, body: Element<'a, Msg>) -> Element<'a, Msg> {
    let t = look.tokens;
    let header = row![
        look.small(title).color(look.chrome.secondary_text),
        iced::widget::space().width(Length::Fill),
        button(look.text("×").color(t.muted_text)).padding(Padding::from([0, 6])).style(look.flat()).on_press(Msg::ClosePanel),
    ]
    .align_y(Alignment::Center);
    column![
        look.rule(),
        container(header).padding(Padding::from([2, 12])).width(Length::Fill).style(look.strip(look.chrome.secondary, look.chrome.secondary_text)),
        container(body).width(Length::Fill).height(Length::Fill).style(look.strip(t.surface, t.text)),
    ]
    .height(Length::Fixed(PANEL_H))
    .into()
}
