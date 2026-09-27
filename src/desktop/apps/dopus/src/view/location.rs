//! Per-pane location bars. At rest: the pane path, display-sanitised via the
//! core's helper (the sanitisation law — display text never carries control
//! bytes). Editing: a [`TextField`](cosmix_iced_widgets::TextField) holding
//! the REAL path text (never sanitised — the editor round-trips what the
//! user typed) with Enter → navigate (leading `~` expanded) and Escape →
//! cancel.
//!
//! Focus: while an editor is up the app flips the key router's
//! `focus_editable`, so the chords fall through to the editor —
//! cosmix-actions' [`FocusContext`](cosmix_actions::FocusContext) contract.
//! Enter IS a keymap binding (it is file.open in the packaged keymap), but
//! every default is `allow_in_editable: false`, so the router suppresses it
//! while an editor holds focus and the keystroke reaches the field.
//! The field owns Enter submission; KeyRouter cancels editing on Escape.

use iced::widget::{button, container};
use iced::{Border, Element, Length};

use cosmix_dopus_core::{PaneId, PaneModel};
use cosmix_iced_widgets::TextField;

use crate::app::Msg;
use crate::view::Look;

/// The two editor focus ids (the ced dialog `INPUT` shape: static strings).
pub const LOCATION_LEFT: &str = "dopus-location-left";
pub const LOCATION_RIGHT: &str = "dopus-location-right";

/// The editor id for a pane (handed to `iced::widget::operation::focus`).
pub fn location_id(pane: PaneId) -> &'static str {
    match pane {
        PaneId::Left => LOCATION_LEFT,
        PaneId::Right => LOCATION_RIGHT,
    }
}

/// The bar above one pane's listing. `editing` is `Some(text)` when THIS
/// pane's bar is in edit mode (the real path text, not sanitised).
pub fn bar<'a>(
    look: Look,
    pane: &'a PaneModel,
    pane_id: PaneId,
    editing: Option<&'a str>,
) -> Element<'a, Msg> {
    match editing {
        Some(text) => editor(look, pane_id, text),
        None => display(look, pane, pane_id),
    }
}

/// At rest: the sanitised path as a whole-bar button (click → edit).
fn display(look: Look, pane: &PaneModel, pane_id: PaneId) -> Element<'static, Msg> {
    let path = cosmix_dopus_core::sanitise_display_path(&pane.path);
    container(
        button(super::elide::Label {
            text: path,
            font: look.mono_font,
            px: look.mono_px * 0.9,
            color: look.chrome.secondary_text,
        })
        .padding([
            look.chrome.edge * 2.0,
            look.chrome.small + look.chrome.edge * 2.0,
        ])
        .width(Length::Fill)
        .on_press(Msg::LocationEdit(pane_id))
        .style(bar_look(&look)),
    )
    .width(Length::Fill)
    .into()
}

/// Editing: the real path text in a token-styled field.
fn editor(look: Look, pane_id: PaneId, text: &str) -> Element<'_, Msg> {
    let field = TextField::new("path", text)
        .id(location_id(pane_id))
        .on_input(Msg::LocationInput)
        .on_submit(Msg::LocationSubmit(pane_id))
        .width(Length::Fill)
        .padding(iced::Padding::from([
            look.chrome.edge * 2.0,
            look.chrome.small + look.chrome.edge * 2.0,
        ]))
        .size(look.mono_px * 0.9)
        .style(field_look(&look));
    field.into()
}

/// The at-rest bar, styled as a button that reads like the editor it opens:
/// the same quiet `input` edge role ced uses for an unfocused text field.
fn bar_look(look: &Look) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
    let edge = look.chrome.edge;
    let (background, border, text_color, radius) = (
        look.tokens.input,
        look.tokens.input,
        look.chrome.secondary_text,
        look.tokens.radius,
    );
    move |_theme, _status| button::Style {
        background: Some(background.into()),
        text_color,
        border: Border {
            color: border,
            width: edge,
            radius: radius.into(),
        },
        ..Default::default()
    }
}

/// The field's style, from tokens only (`input` background and resting edge,
/// `ring` focus) — the [`Tokens::text_input`](cosmix_iced_widgets::Tokens)
/// shape; only focused editing uses the accent ring.
fn field_look(
    look: &Look,
) -> impl Fn(&iced::Theme, iced::widget::text_input::Status) -> iced::widget::text_input::Style + 'static
{
    let edge = look.chrome.edge;
    let (background, border, text_color, muted, ring, selection, radius) = (
        look.tokens.input,
        look.tokens.input,
        look.chrome.secondary_text,
        look.tokens.muted_text,
        look.tokens.ring,
        look.tokens.selection,
        look.tokens.radius,
    );
    move |_theme, status| iced::widget::text_input::Style {
        background: background.into(),
        border: Border {
            color: match status {
                iced::widget::text_input::Status::Focused { .. } => ring,
                _ => border,
            },
            width: edge,
            radius: radius.into(),
        },
        icon: muted,
        placeholder: muted,
        value: text_color,
        selection,
    }
}
