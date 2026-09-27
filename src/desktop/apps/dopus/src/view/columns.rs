//! Sort header using exactly the same measured column geometry as FileList.
use super::{Look, elide::shape, rows::Columns};
use crate::app::{Msg, PaneOp};
use cosmix_dopus_core::{PaneId, SortColumn};
use iced::advanced::text::{self, Paragraph as _, Renderer as _};
use iced::advanced::{
    Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::{Element, Event, Length, Point, Rectangle, Size};
use iced_tiny_skia::Renderer;

type Para = <Renderer as text::Renderer>::Paragraph;
pub struct Header {
    pub look: Look,
    pub pane: PaneId,
    pub sort: SortColumn,
    pub ascending: bool,
}
impl Header {
    fn labels(&self) -> [Para; 3] {
        [
            ("Name", SortColumn::Name),
            ("Size", SortColumn::Size),
            ("Modified", SortColumn::Modified),
        ]
        .map(|(label, sort)| {
            let label = if sort == self.sort {
                format!("{label} {}", if self.ascending { "↑" } else { "↓" })
            } else {
                label.into()
            };
            shape(&label, self.look.ui_font, self.look.px)
        })
    }
}
impl Widget<Msg, iced::Theme, Renderer> for Header {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }
    fn state(&self) -> tree::State {
        tree::State::new(self.labels())
    }
    fn layout(&mut self, tree: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        let labels = self.labels();
        let height = labels[0].min_bounds().height + self.look.chrome.small * 2.0;
        *tree.state.downcast_mut::<[Para; 3]>() = labels;
        layout::Node::new(limits.resolve(Length::Fill, Length::Shrink, Size::new(0.0, height)))
    }
    fn update(
        &mut self,
        _: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _: &Renderer,
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) && bounds
            .intersection(viewport)
            .is_some_and(|clip| cursor.is_over(clip))
        {
            let x = cursor.position().unwrap_or_default().x - bounds.x;
            if let Some(i) = Columns::new(self.look)
                .cells(bounds.width)
                .iter()
                .position(|(start, width)| x >= *start && x < start + width)
            {
                shell.publish(Msg::Pane(
                    self.pane,
                    PaneOp::Sort([SortColumn::Name, SortColumn::Size, SortColumn::Modified][i]),
                ));
                shell.capture_event();
            }
        }
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _: &iced::Theme,
        _: &renderer::Style,
        layout: Layout<'_>,
        _: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        renderer.with_layer(clip, |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    ..Default::default()
                },
                self.look.chrome.secondary,
            );
            for (i, ((start, width), para)) in Columns::new(self.look)
                .cells(bounds.width)
                .into_iter()
                .zip(tree.state.downcast_ref::<[Para; 3]>())
                .enumerate()
            {
                let cell = Rectangle {
                    x: bounds.x + start,
                    width,
                    ..bounds
                };
                let x = if i == 0 {
                    cell.x
                } else {
                    cell.x + cell.width - para.min_bounds().width
                };
                renderer.with_layer(cell, |renderer| {
                    renderer.fill_paragraph(
                        para,
                        Point::new(x, bounds.y + self.look.chrome.small),
                        self.look.chrome.secondary_text,
                        cell,
                    )
                });
            }
        });
    }
}
impl<'a> From<Header> for Element<'a, Msg> {
    fn from(value: Header) -> Self {
        Element::new(value)
    }
}
