//! Absolute placement, so both arms put every widget on the same pixel.
//!
//! `cosmix_bench_feed::layout::Layout` resolves the whole board in window
//! coordinates and the bake-off driver aims its injected drags at those
//! rectangles. iced has no absolute-position container, and rebuilding the
//! geometry out of rows, columns and padding would drift from the shared
//! numbers by a rounding here and a gap there — exactly the kind of
//! difference a parity screenshot is meant to rule out. `Board` therefore
//! lays each child out inside its own rectangle and moves it there, and does
//! nothing else: one layer, no clipping, no overlap handling, so it adds
//! nothing to the drawing cost being measured.

use cosmix_bench_feed::layout::Rect;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};

pub struct Board<'a, Message, Theme, Renderer> {
    size: Size,
    places: Vec<Rect>,
    children: Vec<Element<'a, Message, Theme, Renderer>>,
}

impl<'a, Message, Theme, Renderer> Board<'a, Message, Theme, Renderer> {
    /// An empty board filling a `width` x `height` window.
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            size: Size::new(width, height),
            places: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Places `child` at `rect`, drawn over everything placed before it.
    pub fn push(
        mut self,
        rect: Rect,
        child: impl Into<Element<'a, Message, Theme, Renderer>>,
    ) -> Self {
        self.places.push(rect);
        self.children.push(child.into());
        self
    }
}

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for Board<'_, Message, Theme, Renderer>
{
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.size.width),
            Length::Fixed(self.size.height),
        )
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let nodes = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(&self.places)
            .map(|((child, tree), rect)| {
                let limits = layout::Limits::new(Size::ZERO, Size::new(rect.w, rect.h));
                child
                    .as_widget_mut()
                    .layout(tree, renderer, &limits)
                    .move_to(Point::new(rect.x, rect.y))
            })
            .collect();
        let size = limits.resolve(
            Length::Fixed(self.size.width),
            Length::Fixed(self.size.height),
            self.size,
        );
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, state), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(state, layout, renderer, operation);
            }
        });
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
        // Last placed is topmost, so events are offered in reverse.
        for ((child, tree), layout) in self
            .children
            .iter_mut()
            .rev()
            .zip(tree.children.iter_mut().rev())
            .zip(layout.children().rev())
        {
            child.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
            if shell.is_event_captured() {
                return;
            }
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
        self.children
            .iter()
            .rev()
            .zip(tree.children.iter().rev())
            .zip(layout.children().rev())
            .map(|((child, tree), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
            })
            .find(|interaction| *interaction != mouse::Interaction::None)
            .unwrap_or_default()
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
        for ((child, tree), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a>
    From<Board<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
{
    fn from(board: Board<'a, Message, Theme, Renderer>) -> Self {
        Element::new(board)
    }
}
