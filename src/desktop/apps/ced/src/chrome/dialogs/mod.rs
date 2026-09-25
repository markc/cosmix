//! Modal dialogs (ced E1 plan §4.4, D11), drawn over the window with a
//! scrim. Every async completion a dialog starts carries the `Intent` that
//! opened it (D5): a dialog opened by a Bus caller's action saves under that
//! caller's lane, one opened from the window under `human:ced`.

pub mod about;
pub mod confirm;
pub mod conflict;
pub mod file;
pub mod goto;
pub mod keys;
pub mod recovered;

use iced::widget::{column, container, row};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Vector};

use super::Look;
use crate::app::Msg;

/// The open dialog.
#[derive(Debug, Clone)]
pub enum Modal {
    File(file::FileDialog),
    Confirm(confirm::Confirm),
    Goto(goto::Goto),
    About,
    Keys,
    Conflict(conflict::ConflictView),
    Recovered(recovered::Recovered),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DialogMsg {
    File(file::FileMsg),
    Confirm(confirm::Choice),
    Goto(goto::GotoMsg),
    Recovered(recovered::RecoveredMsg),
    /// Close without acting (Escape, ×, Cancel).
    Close,
}

impl Modal {
    /// The widget to focus when the dialog opens.
    pub fn focus_id(&self) -> Option<&'static str> {
        match self {
            Modal::File(_) => Some(file::PATH_INPUT),
            Modal::Goto(_) => Some(goto::INPUT),
            _ => None,
        }
    }

    pub fn view<'a>(&'a self, look: Look, ctx: &DialogCtx<'a>) -> Element<'a, Msg> {
        match self {
            Modal::File(d) => d.view(look),
            Modal::Confirm(d) => d.view(look),
            Modal::Goto(d) => d.view(look),
            Modal::About => about::view(look, ctx),
            Modal::Keys => keys::view(look, ctx.macros),
            Modal::Conflict(d) => d.view(look),
            Modal::Recovered(d) => d.view(look),
        }
    }
}

/// Read-only context some dialogs show.
#[derive(Debug, Clone, Default)]
pub struct DialogCtx<'a> {
    pub edit_version: Option<&'a str>,
    pub edit_epoch: Option<&'a str>,
    pub volatile: Option<bool>,
    pub theme: String,
    pub mono: String,
    pub ui: String,
    pub config_path: Option<String>,
    pub macros: &'a [crate::macros::MacroDef],
}

/// The dialog card: a title, a body and a right-aligned button row, centred
/// over a scrim.
pub fn frame<'a>(look: Look, title: &'a str, body: Element<'a, Msg>, buttons: Vec<Element<'a, Msg>>, width: f32) -> Element<'a, Msg> {
    let t = look.tokens;
    let mut actions = row![iced::widget::space().width(Length::Fill)].spacing(8).align_y(Alignment::Center);
    for b in buttons {
        actions = actions.push(b);
    }
    let card = container(
        column![look.text(title).size(look.ui_px * 1.15).color(t.popover_text), body, actions].spacing(14),
    )
    .padding(Padding::from([18, 20]))
    .width(Length::Fixed(width))
    .style(move |_| container::Style {
        background: Some(Background::Color(t.popover)),
        text_color: Some(t.popover_text),
        border: Border { color: t.border, width: 1.0, radius: (t.radius * 1.5).into() },
        shadow: Shadow { color: Color { a: 0.35, ..darker(t.surface, t.text) }, offset: Vector::new(0.0, 6.0), blur_radius: 24.0 },
        ..container::Style::default()
    });
    let scrim = Color { a: 0.55, ..darker(t.surface, t.text) };
    iced::widget::opaque(
        iced::widget::mouse_area(
            container(iced::widget::opaque(card)).center(Length::Fill).style(move |_| container::Style {
                background: Some(Background::Color(scrim)),
                ..container::Style::default()
            }),
        )
        .on_press(Msg::Dialog(DialogMsg::Close)),
    )
}

/// The darker of two tokens: the base of shadows and the scrim, so they
/// darken in light and dark mode alike without a colour literal.
fn darker(a: Color, b: Color) -> Color {
    let lum = |c: Color| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    if lum(a) <= lum(b) { a } else { b }
}
