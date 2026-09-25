//! Chrome (ced E1 plan §4.4): menu bar, tab strip, infobar, find/replace bar,
//! problems and output panels, status bar, dialogs.
//!
//! Every piece is a plain function from app state to an `Element<Msg>`, drawn
//! with [`Look`] — the resolved design tokens and the two typography roles —
//! so nothing here holds a colour literal or a font name. Fixed chrome
//! heights are constants so `ced.layout` can report the rectangles without
//! measuring widgets (the editor reports its own geometry).

pub mod dialogs;
pub mod find;
pub mod infobar;
pub mod menu;
pub mod output;
pub mod problems;
pub mod status;
pub mod tabs;
pub mod timer;

use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Element, Length, Padding};

use crate::theme::{Chrome, Theme};

/// The menu bar height (`MenuStyle::row_height`).
pub const MENU_H: f32 = 28.0;
/// The tab strip height.
pub const TABS_H: f32 = 32.0;
/// The status bar height.
pub const STATUS_H: f32 = 26.0;
/// The bottom panel (Problems / Output) height.
pub const PANEL_H: f32 = 180.0;

/// Everything a chrome view needs to draw: colours and fonts, all tokens.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub tokens: cosmix_iced_widgets::Tokens,
    pub chrome: Chrome,
    pub ui: iced::Font,
    pub ui_px: f32,
    pub mono: iced::Font,
    pub mono_px: f32,
}

impl Look {
    pub fn new(theme: &Theme, mono_px: f32) -> Self {
        Self {
            tokens: theme.tokens,
            chrome: theme.chrome,
            ui: theme.ui_font,
            ui_px: theme.ui.1,
            mono: theme.mono_font,
            mono_px,
        }
    }

    /// The chrome's small text size (status bar, badges).
    pub fn small_px(&self) -> f32 {
        (self.ui_px * 0.87).round()
    }

    /// Chrome text.
    pub fn text<'a>(&self, value: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
        text(value).font(self.ui).size(self.ui_px)
    }

    /// Small chrome text (status bar).
    pub fn small<'a>(&self, value: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
        text(value).font(self.ui).size(self.small_px())
    }

    /// Monospace text at the chrome size (paths, code in panels).
    pub fn code<'a>(&self, value: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
        text(value).font(self.mono).size(self.small_px())
    }

    /// A filled strip.
    pub fn strip(&self, background: Color, fg: Color) -> impl Fn(&iced::Theme) -> container::Style + 'static {
        move |_| container::Style {
            background: Some(Background::Color(background)),
            text_color: Some(fg),
            ..container::Style::default()
        }
    }

    /// A flat button: transparent until hovered, `muted_surface` then.
    pub fn flat(&self) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
        let t = self.tokens;
        move |_, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => Some(Background::Color(t.muted_surface)),
                _ => None,
            },
            text_color: if matches!(status, button::Status::Disabled) { t.muted_text } else { t.text },
            border: Border { radius: t.radius.into(), ..Border::default() },
            ..button::Style::default()
        }
    }

    /// The default (primary) action of a dialog or infobar.
    pub fn primary(&self) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
        let t = self.tokens;
        move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Disabled => t.muted_surface,
                _ => t.primary,
            })),
            text_color: t.primary_text,
            border: Border {
                radius: t.radius.into(),
                width: if matches!(status, button::Status::Hovered) { 1.0 } else { 0.0 },
                color: t.ring,
            },
            ..button::Style::default()
        }
    }

    /// A secondary (outlined) action.
    pub fn secondary(&self) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
        let t = self.tokens;
        move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => t.muted_surface,
                _ => t.card,
            })),
            text_color: if matches!(status, button::Status::Disabled) { t.muted_text } else { t.card_text },
            border: Border { radius: t.radius.into(), width: 1.0, color: t.border },
            ..button::Style::default()
        }
    }

    /// A destructive action (Discard, Don't Save).
    pub fn danger(&self) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
        let t = self.tokens;
        move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => t.destructive,
                _ => t.card,
            })),
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed => t.destructive_text,
                _ => t.destructive,
            },
            border: Border { radius: t.radius.into(), width: 1.0, color: t.destructive },
            ..button::Style::default()
        }
    }

    /// Text fields.
    pub fn input(&self) -> impl Fn(&iced::Theme, iced::widget::text_input::Status) -> iced::widget::text_input::Style + 'static {
        let t = self.tokens;
        move |_, status| t.text_input(status)
    }

    /// A button with chrome text and compact padding.
    pub fn button<'a, M: Clone + 'a>(&self, label: impl text::IntoFragment<'a>, on_press: Option<M>) -> iced::widget::Button<'a, M> {
        button(self.text(label)).padding(Padding::from([4, 12])).on_press_maybe(on_press)
    }

    /// A 1 px rule in the border colour.
    pub fn rule<'a, M: 'a>(&self) -> Element<'a, M> {
        let border = self.tokens.border;
        container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style { background: Some(Background::Color(border)), ..container::Style::default() })
            .into()
    }
}
