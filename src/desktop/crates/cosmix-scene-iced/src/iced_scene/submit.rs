//! Enter-to-submit around a `TextField`, which exposes no `on_submit`.

use std::collections::HashSet;

use cosmix_iced_host::core::keyboard::{self, key::Named};
use cosmix_iced_host::core::layout::{self, Layout};
use cosmix_iced_host::core::widget::operation::Focusable;
use cosmix_iced_host::core::widget::tree::{self, Tree};
use cosmix_iced_host::core::widget::{Id, Operation, Widget};
use cosmix_iced_host::core::{
    Clipboard, Event, Length, Rectangle, Shell, Size, Vector, mouse, overlay, renderer,
};
use cosmix_iced_host::{Element, Renderer, Theme};

pub(crate) fn on_submit<'a, Message: Clone + 'a>(
    content: Element<'a, Message>,
    message: Message,
    id: Id,
) -> Element<'a, Message> {
    Element::new(Submit {
        content,
        message,
        id,
    })
}

struct Submit<'a, Message> {
    content: Element<'a, Message>,
    message: Message,
    id: Id,
}

/// Collects the ids of focused widgets.
#[derive(Default)]
pub(crate) struct FocusProbe(pub HashSet<Id>);

impl Operation for FocusProbe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if let Some(id) = id
            && state.is_focused()
        {
            self.0.insert(id.clone());
        }
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Submit<'_, Message> {
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

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let enter = matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Enter),
                ..
            })
        );
        let submit = enter && {
            let mut probe = FocusProbe::default();
            self.content
                .as_widget_mut()
                .operate(tree, layout, renderer, &mut probe);
            probe.0.contains(&self.id)
        };
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        if submit {
            shell.publish(self.message.clone());
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}
