//! Modal dialogs for the core's file operations, in the shape of ced's
//! `chrome/dialogs`: a card centred over a scrim ([`frame`]), a
//! first-in-first-out [`ModalQueue`] (the core QUEUES concurrent modals —
//! `outstanding_reservations` is mint-ordered — and the oldest renders until
//! it is answered, then the next), and the two surfaces the core's
//! reservations need:
//!
//! - [`Dialog::Confirm`] — the core's (already sanitised) message text,
//!   Yes/No buttons with destructive emphasis on the confirming action,
//!   Enter = Yes, Escape = No (the key router captures both while a dialog
//!   is up and publishes [`Msg::DialogKey`]).
//! - [`Dialog::Prompt`] — a [`TextField`](cosmix_iced_widgets::TextField)
//!   seeded with the core's initial text (New Folder's constant, or the
//!   rename's REAL filename — never display-sanitised; the sanitisation law
//!   covers display only), live [`validate_filename`] feedback (error state
//!   on the field, the validator's message below it), Enter/OK = submit and
//!   Escape/Cancel = dismiss. The core re-validates at resolution — an
//!   invalid name is not a resolution, so both the OK button and the Enter
//!   path refuse to fire while the field is invalid.
//!
//! Scrim press, Cancel and Escape are all dismissals (fail-closed: a
//! dismissed confirm is `No`, a dismissed prompt is `prompt_text(token,
//! None)` — nothing runs). Zero colour literals: every colour is a token or
//! a mix of two.

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Vector};

use cosmix_dopus_core::{PromptKind, validate_filename};
use cosmix_iced_widgets::TextField;
use iced_tiny_skia::Renderer;

use crate::app::{DialogMsg, Msg};
use crate::view::Look;

/// The prompt field's focus id (the ced dialog `INPUT` shape).
pub const PROMPT_INPUT: &str = "dopus-dialog-prompt";

/// Dialogs that arrived while another was open. The core queues its
/// reservations; this mirrors that order on the view side so the OLDEST
/// always renders and an answered dialog is followed by the next
/// (ced's `chrome::dialogs::ModalQueue`, minus the staleness filter — a
/// reservation is only ever removed by the app answering it).
pub struct ModalQueue<T> {
    waiting: std::collections::VecDeque<T>,
}

impl<T> Default for ModalQueue<T> {
    fn default() -> Self {
        Self {
            waiting: std::collections::VecDeque::new(),
        }
    }
}

impl<T> ModalQueue<T> {
    /// Show `m` now if nothing is open, else queue it.
    pub fn offer(&mut self, current: &mut Option<T>, m: T) {
        match current {
            Some(_) => self.waiting.push_back(m),
            None => *current = Some(m),
        }
    }

    /// Once nothing is open, show the oldest queued dialog.
    pub fn next(&mut self, current: &mut Option<T>) {
        while current.is_none() {
            let Some(m) = self.waiting.pop_front() else {
                break;
            };
            *current = Some(m);
        }
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

#[cfg(test)]
mod queue_tests {
    use super::ModalQueue;

    #[test]
    fn a_dialog_never_replaces_an_open_one() {
        let mut q = ModalQueue::default();
        let mut open = None;
        q.offer(&mut open, 1);
        assert_eq!(open, Some(1), "nothing open: shown at once");
        q.offer(&mut open, 2);
        q.offer(&mut open, 3);
        assert_eq!(
            (open, q.len()),
            (Some(1), 2),
            "the open decision is kept; the rest wait"
        );
        q.next(&mut open);
        assert_eq!(open, Some(1), "nothing shows over an open dialog");
        open = None;
        q.next(&mut open);
        assert_eq!(open, Some(2), "oldest first");
        open = None;
        q.next(&mut open);
        assert_eq!(open, Some(3));
        open = None;
        q.next(&mut open);
        assert!(open.is_none() && q.is_empty());
    }
}

/// One pending core reservation, as the view draws it. `Prompt` carries the
/// field state (the seeded initial, then the user's text plus the live
/// [`validate_filename`] verdict) because the dialog owns it while it is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Confirm {
        token: u64,
        message: String,
    },
    Prompt {
        token: u64,
        kind: PromptKind,
        /// The field's current text: the core's initial (New Folder's
        /// constant, or the REAL filename for a rename), then what the user
        /// typed. Never sanitised — the field round-trips what is typed.
        text: String,
        /// The live validator verdict: the message shown under the field
        /// while the text is not a single valid file name.
        error: Option<String>,
    },
}

impl Dialog {
    /// A prompt as the core raised it: seeded with the initial text and
    /// validated once so the field starts in a known state.
    pub fn prompt(token: u64, kind: PromptKind, initial: String) -> Self {
        let mut dialog = Dialog::Prompt {
            token,
            kind,
            text: initial,
            error: None,
        };
        dialog.revalidate();
        dialog
    }

    /// The field's text changed: keep it and re-run the validator.
    pub fn input(&mut self, text: String) {
        if let Dialog::Prompt { text: field, .. } = self {
            *field = text;
        }
        self.revalidate();
    }

    /// Live `validate_filename` feedback (app-contract law 6): the field
    /// error state plus the validator's message.
    fn revalidate(&mut self) {
        if let Dialog::Prompt { text, error, .. } = self {
            *error = validate_filename(text).err();
        }
    }

    /// The dialog card over the scrim; a scrim press dismisses.
    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg, iced::Theme, Renderer> {
        match self {
            Dialog::Confirm { message, .. } => frame(
                look,
                "Confirm",
                text(message.as_str())
                    .font(look.ui_font)
                    .size(look.px)
                    .into(),
                vec![
                    dialog_button(
                        look,
                        "Yes",
                        Msg::Dialog(DialogMsg::Answer(true)),
                        Kind::Danger,
                        true,
                    ),
                    dialog_button(
                        look,
                        "No",
                        Msg::Dialog(DialogMsg::Answer(false)),
                        Kind::Quiet,
                        true,
                    ),
                ],
                look.chrome.icon * 28.0,
            ),
            Dialog::Prompt {
                kind,
                text: value,
                error,
                ..
            } => {
                let title = match kind {
                    PromptKind::NewFolder => "New folder",
                    PromptKind::Rename => "Rename",
                };
                let mut body = column![
                    TextField::new(title, value)
                        .id(PROMPT_INPUT)
                        .on_input(|text| Msg::Dialog(DialogMsg::Input(text)))
                        .width(Length::Fill)
                        .padding(iced::Padding::from([look.chrome.small, look.chrome.pad]))
                        .size(look.px)
                        .style(field_look(look, error.is_some())),
                ]
                .spacing(look.chrome.small + 2.0 * look.chrome.edge);
                if let Some(message) = error {
                    body = body.push(
                        text(message.as_str())
                            .font(look.ui_font)
                            .size(look.px * 0.85)
                            .color(look.chrome.warning),
                    );
                }
                frame(
                    look,
                    title,
                    body.into(),
                    vec![
                        dialog_button(
                            look,
                            "OK",
                            Msg::Dialog(DialogMsg::Submit),
                            Kind::Primary,
                            error.is_none(),
                        ),
                        dialog_button(
                            look,
                            "Cancel",
                            Msg::Dialog(DialogMsg::Dismiss),
                            Kind::Quiet,
                            true,
                        ),
                    ],
                    look.chrome.icon * 28.0,
                )
            }
        }
    }
}

/// Confirm and prompt cards share one width.

/// Button weight: the primary submit, the destructive confirming action of a
/// delete confirm, or a quiet dismissal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Primary,
    Danger,
    Quiet,
}

/// A dialog button; `enabled = false` greys it and takes its press away
/// (an invalid prompt cannot submit).
fn dialog_button<'a>(
    look: Look,
    label: &'static str,
    msg: Msg,
    kind: Kind,
    enabled: bool,
) -> Element<'a, Msg> {
    let t = look.tokens;
    let (background, text_color, border) = match kind {
        Kind::Primary => (Some(t.primary), t.primary_text, t.primary),
        Kind::Danger => (Some(t.destructive), t.destructive_text, t.destructive),
        Kind::Quiet => (None, t.popover_text, t.border),
    };
    // A hovered/pressed accent darkens towards its own text token — a mix of
    // two tokens, never a literal.
    let hover = match kind {
        Kind::Primary => darker(t.primary, t.primary_text),
        Kind::Danger => darker(t.destructive, t.destructive_text),
        Kind::Quiet => t.muted_surface,
    };
    let quiet = kind == Kind::Quiet;
    let disabled = !enabled;
    button(
        text(label)
            .font(look.ui_font)
            .size(look.px * 0.9)
            .color(if disabled { t.muted_text } else { text_color }),
    )
    .padding([look.chrome.small, look.chrome.gap])
    .on_press_maybe(enabled.then_some(msg))
    .style(move |_theme, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed if !disabled => Some(hover.into()),
            _ => background.map(Background::Color),
        },
        text_color: if disabled { t.muted_text } else { text_color },
        border: Border {
            color: border,
            width: if quiet { look.chrome.edge } else { 0.0 },
            radius: look.tokens.radius.into(),
        },
        ..Default::default()
    })
    .into()
}

/// The prompt field's style: `input` background, `border` at rest, `ring`
/// focused — and `warning` instead of `ring` while the validator objects.
fn field_look(
    look: Look,
    invalid: bool,
) -> impl Fn(&iced::Theme, iced::widget::text_input::Status) -> iced::widget::text_input::Style + 'static
{
    let (background, border, text_color, muted, ring, warning, selection, radius) = (
        look.tokens.input,
        look.tokens.border,
        look.tokens.popover_text,
        look.tokens.muted_text,
        look.tokens.ring,
        look.chrome.warning,
        look.tokens.selection,
        look.tokens.radius,
    );
    move |_theme, status| iced::widget::text_input::Style {
        background: background.into(),
        border: Border {
            color: match status {
                iced::widget::text_input::Status::Focused { .. } if invalid => warning,
                iced::widget::text_input::Status::Focused { .. } => ring,
                _ if invalid => warning,
                _ => border,
            },
            width: look.chrome.edge,
            radius: radius.into(),
        },
        icon: muted,
        placeholder: muted,
        value: text_color,
        selection,
    }
}

/// The dialog card: a title, a body and a right-aligned button row, centred
/// over a scrim (ced's `chrome::dialogs::frame`, over the dopus `Look`). A
/// press on the scrim dismisses ([`DialogMsg::Dismiss`]).
pub fn frame<'a>(
    look: Look,
    title: &'static str,
    body: Element<'a, Msg, iced::Theme, Renderer>,
    buttons: Vec<Element<'a, Msg, iced::Theme, Renderer>>,
    width: f32,
) -> Element<'a, Msg, iced::Theme, Renderer> {
    let t = look.tokens;
    let mut actions = row![iced::widget::space().width(Length::Fill)]
        .spacing(look.chrome.pad)
        .align_y(Alignment::Center);
    for b in buttons {
        actions = actions.push(b);
    }
    let card = container(
        column![
            text(title)
                .font(look.ui_font)
                .size(look.px * 1.15)
                .color(t.popover_text),
            body,
            actions,
        ]
        .spacing(look.chrome.gap),
    )
    .padding(Padding::from([
        look.chrome.icon,
        look.chrome.icon + look.chrome.small,
    ]))
    .width(Length::Fixed(width))
    .style(move |_| container::Style {
        background: Some(Background::Color(t.popover)),
        text_color: Some(t.popover_text),
        border: Border {
            color: t.border,
            width: look.chrome.edge,
            radius: (t.radius * 1.5).into(),
        },
        shadow: Shadow {
            color: Color {
                a: 0.35,
                ..darker(t.surface, t.text)
            },
            offset: Vector::new(0.0, look.chrome.small + 2.0 * look.chrome.edge),
            blur_radius: look.chrome.gap * 2.0,
        },
        ..container::Style::default()
    });
    let scrim = Color {
        a: 0.55,
        ..darker(t.surface, t.text)
    };
    iced::widget::opaque(
        iced::widget::mouse_area(
            container(iced::widget::opaque(card))
                .center(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(scrim)),
                    ..container::Style::default()
                }),
        )
        .on_press(Msg::Dialog(DialogMsg::Dismiss)),
    )
}

/// The darker of two tokens: the base of shadows and the scrim, so they
/// darken in light and dark mode alike without a colour literal (ced's
/// `chrome::dialogs::darker`).
fn darker(a: Color, b: Color) -> Color {
    let lum = |c: Color| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    if lum(a) <= lum(b) { a } else { b }
}
