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
//! [`Capture`] is still load-bearing for Escape — no keymap entry owns it —
//! and turns Enter into a message before the field sees it (no new keymap
//! ids — a `location.focus` id plus a Ctrl+L chord would need a
//! cosmix-actions keymap addition: reported, not invented locally).

use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, renderer};
use iced::widget::{button, container};
use iced::{Border, Element, Event, Length, Rectangle, Size};

use cosmix_dopus_core::{PaneId, PaneModel};
use cosmix_iced_widgets::TextField;
use iced_tiny_skia::Renderer;

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
pub fn bar<'a>(look: Look, pane: &'a PaneModel, pane_id: PaneId, editing: Option<&'a str>) -> Element<'a, Msg> {
    match editing {
        Some(text) => editor(look, pane_id, text),
        None => display(look, pane, pane_id),
    }
}

/// At rest: the sanitised path as a whole-bar button (click → edit).
fn display(look: Look, pane: &PaneModel, pane_id: PaneId) -> Element<'static, Msg> {
    let path = cosmix_dopus_core::sanitise_display_path(&pane.path);
    container(
        button(
            iced::widget::text(path)
                .font(look.mono_font)
                .size(look.mono_px * 0.9)
                .color(look.chrome.secondary_text),
        )
        .padding([2, 6])
        .width(Length::Fill)
        .on_press(Msg::LocationEdit(pane_id))
        .style(bar_look(&look)),
    )
    .width(Length::Fill)
    .into()
}

/// Editing: the real path text in a token-styled field, wrapped in
/// [`Capture`] for Enter/Escape.
fn editor(look: Look, pane_id: PaneId, text: &str) -> Element<'_, Msg> {
    let field = TextField::new("path", text)
        .id(location_id(pane_id))
        .on_input(Msg::LocationInput)
        .width(Length::Fill)
        .padding(iced::Padding::from([2, 6]))
        .size(look.mono_px * 0.9)
        .style(field_look(&look));
    Capture { content: field.into(), pane_id }.into()
}

/// The at-rest bar, styled as a button that reads like the editor it opens:
/// the same `input` background and `border`, hovered.
fn bar_look(look: &Look) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'static {
    let (background, border, text_color, ring, radius) = (
        look.tokens.input,
        look.tokens.border,
        look.chrome.secondary_text,
        look.tokens.ring,
        look.tokens.radius,
    );
    move |_theme, status| button::Style {
        background: Some(background.into()),
        text_color,
        border: Border {
            color: match status {
                button::Status::Hovered | button::Status::Pressed => ring,
                _ => border,
            },
            width: 1.0,
            radius: radius.into(),
        },
        ..Default::default()
    }
}

/// The field's style, from tokens only (`input` background, `border`,
/// `ring` focus) — the [`Tokens::text_input`](cosmix_iced_widgets::Tokens)
/// shape, at rest using the quieter `border` instead of `input` so the bar
/// reads as a button until clicked.
fn field_look(
    look: &Look,
) -> impl Fn(&iced::Theme, iced::widget::text_input::Status) -> iced::widget::text_input::Style + 'static {
    let (background, border, text_color, muted, ring, selection, radius) = (
        look.tokens.input,
        look.tokens.border,
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
            width: 1.0,
            radius: radius.into(),
        },
        icon: muted,
        placeholder: muted,
        value: text_color,
        selection,
    }
}

/// Enter/Escape capture around an editing field (the `keys::KeyRouter`
/// shape, cut down): intercepts the two keys the keymap does not own,
/// publishes them, and forwards everything else to the field.
pub struct Capture<'a> {
    content: Element<'a, Msg, iced::Theme, Renderer>,
    pane_id: PaneId,
}

impl Widget<Msg, iced::Theme, Renderer> for Capture<'_> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        // The editor only exists while it is being edited, so these are
        // unconditional: Enter submits, Escape cancels.
        if let Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
            match key {
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
                    if !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
                {
                    shell.publish(Msg::LocationSubmit(self.pane_id));
                    shell.capture_event();
                    return;
                }
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
                    shell.publish(Msg::LocationCancel);
                    shell.capture_event();
                    return;
                }
                _ => {}
            }
        }
        self.content
            .as_widget_mut()
            .update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<iced::advanced::overlay::Element<'b, Msg, iced::Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a> From<Capture<'a>> for Element<'a, Msg, iced::Theme, Renderer> {
    fn from(capture: Capture<'a>) -> Self {
        Element::new(capture)
    }
}
