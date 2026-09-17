//! One menu panel as a standalone widget, for a host that gives each panel
//! its own surface (xdg_popup), plus the panel geometry both modes share.
use iced_core::widget::{Tree, tree};
use iced_core::{
    Clipboard, Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Widget, layout,
    mouse, renderer, text, touch,
};

use super::{Item, Kind, MenuStyle, label, quad, text_width};

/// Minimum panel width in logical pixels.
pub const MIN_PANEL_WIDTH: f32 = 160.0;
/// Height of a separator row in logical pixels.
pub const SEPARATOR_HEIGHT: f32 = 8.0;

pub(crate) fn row_height<Message>(item: &Item<Message>, style: MenuStyle) -> f32 {
    if matches!(item.kind, Kind::Separator) {
        SEPARATOR_HEIGHT
    } else {
        style.row_height
    }
}

/// Natural panel width: the widest label plus accelerator (and submenu
/// arrow) with padding, at least `MIN_PANEL_WIDTH`.
pub(crate) fn panel_width<Message, Renderer: text::Renderer>(
    renderer: &Renderer,
    items: &[Item<Message>],
    style: MenuStyle,
) -> f32 {
    items
        .iter()
        .map(|item| {
            text_width(renderer, &item.label, style)
                + text_width(renderer, &item.accelerator, style)
                + if matches!(item.kind, Kind::Submenu(_)) {
                    text_width(renderer, "  ›", style)
                } else {
                    0.0
                }
                + style.padding * 3.0
        })
        .fold(MIN_PANEL_WIDTH, f32::max)
}

/// The size a panel showing `items` wants, in logical pixels. Use it to size a
/// popup surface.
pub fn panel_size<Message, Renderer: text::Renderer>(
    renderer: &Renderer,
    items: &[Item<Message>],
    style: MenuStyle,
) -> Size {
    Size::new(
        panel_width(renderer, items, style),
        items.iter().map(|item| row_height(item, style)).sum(),
    )
}

/// Bounds of row `index` in a panel `width` wide, relative to the panel's
/// top-left; `None` past the end. Use it as a submenu's anchor.
pub fn row_bounds<Message>(
    items: &[Item<Message>],
    index: usize,
    width: f32,
    style: MenuStyle,
) -> Option<Rectangle> {
    let item = items.get(index)?;
    let y = items[..index]
        .iter()
        .map(|item| row_height(item, style))
        .sum();
    Some(Rectangle::new(
        Point::new(0.0, y),
        Size::new(width, row_height(item, style)),
    ))
}

/// The row at panel-relative `y`, if any (separators and disabled rows
/// included; the navigator decides what they do).
pub fn row_at<Message>(items: &[Item<Message>], y: f32, style: MenuStyle) -> Option<usize> {
    if y < 0.0 {
        return None;
    }
    let mut top = 0.0;
    for (index, item) in items.iter().enumerate() {
        let bottom = top + row_height(item, style);
        if y < bottom {
            return Some(index);
        }
        top = bottom;
    }
    None
}

/// Draws a panel of `items` in `bounds`, highlighting `selected`.
pub(crate) fn draw_panel<Message, Renderer: text::Renderer>(
    renderer: &mut Renderer,
    bounds: Rectangle,
    items: &[Item<Message>],
    selected: Option<usize>,
    style: MenuStyle,
) {
    if bounds.height <= 0.0 {
        return;
    }
    quad(renderer, bounds, style.background, style);
    let mut y = bounds.y;
    for (index, item) in items.iter().enumerate() {
        let height = row_height(item, style);
        let row = Rectangle {
            y,
            height,
            ..bounds
        };
        y += height;
        if matches!(item.kind, Kind::Separator) {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: row.x + style.padding,
                        y: row.center_y(),
                        width: (row.width - style.padding * 2.0).max(0.0),
                        height: 1.0,
                    },
                    ..Default::default()
                },
                style.border,
            );
            continue;
        }
        let is_selected = selected == Some(index);
        if is_selected {
            quad(renderer, row, style.selected, style);
        }
        let color = if !item.selectable() {
            style.disabled
        } else if is_selected {
            style.selected_text
        } else {
            style.text
        };
        let text_bounds = Rectangle {
            x: row.x + style.padding,
            width: (row.width - style.padding * 2.0).max(0.0),
            ..row
        };
        label(renderer, &item.label, text_bounds, color, style, false);
        let trailing = if matches!(item.kind, Kind::Submenu(_)) {
            format!("{}  ›", item.accelerator)
        } else {
            item.accelerator.clone()
        };
        label(renderer, &trailing, text_bounds, color, style, true);
    }
}

/// One menu panel filling its own surface. It reports the pointer and presses
/// as row indices; feed them to `Navigator::hover` and `Navigator::click` for
/// this panel's level, and pass the resulting selection back as `selected`.
pub struct Panel<'a, Message> {
    items: &'a [Item<Message>],
    selected: Option<usize>,
    style: MenuStyle,
    on_hover: Option<Box<dyn Fn(Option<usize>) -> Message + 'a>>,
    on_press: Option<Box<dyn Fn(usize) -> Message + 'a>>,
}

impl<'a, Message> Panel<'a, Message> {
    /// A panel listing `items` with row `selected` highlighted.
    pub fn new(items: &'a [Item<Message>], selected: Option<usize>) -> Self {
        Self {
            items,
            selected,
            style: MenuStyle::default(),
            on_hover: None,
            on_press: None,
        }
    }

    /// Colours and metrics; see `Tokens::menu_style`.
    pub fn style(mut self, style: MenuStyle) -> Self {
        self.style = style;
        self
    }

    /// Published when the row under the pointer changes (`None` when the
    /// pointer leaves the rows).
    pub fn on_hover(mut self, callback: impl Fn(Option<usize>) -> Message + 'a) -> Self {
        self.on_hover = Some(Box::new(callback));
        self
    }

    /// Published for a left press or touch on a row.
    pub fn on_press(mut self, callback: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_press = Some(Box::new(callback));
        self
    }
}

#[derive(Default)]
struct PanelState {
    // The last hover reported; `None` until the first report.
    hovered: Option<Option<usize>>,
}

impl<Message, Theme, Renderer: text::Renderer> Widget<Message, Theme, Renderer>
    for Panel<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PanelState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PanelState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = panel_size(renderer, self.items, self.style);
        layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, size))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<PanelState>();
        let row_under = |point: Option<Point>| {
            point
                .filter(|point| bounds.contains(*point))
                .and_then(|point| row_at(self.items, point.y - bounds.y, self.style))
        };
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft)
            | Event::Touch(touch::Event::FingerMoved { .. }) => {
                let point = match event {
                    Event::Touch(touch::Event::FingerMoved { position, .. }) => Some(*position),
                    _ => cursor.position(),
                };
                let row = row_under(point);
                if state.hovered != Some(row) {
                    state.hovered = Some(row);
                    if let Some(on_hover) = &self.on_hover {
                        shell.publish(on_hover(row));
                    }
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if shell.is_event_captured() {
                    return;
                }
                let point = match event {
                    Event::Touch(touch::Event::FingerPressed { position, .. }) => Some(*position),
                    _ => cursor.position(),
                };
                if let Some(row) = row_under(point) {
                    if let Some(on_press) = &self.on_press {
                        shell.publish(on_press(row));
                    }
                    shell.capture_event();
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        draw_panel(
            renderer,
            layout.bounds(),
            self.items,
            self.selected,
            self.style,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        match cursor
            .position_in(bounds)
            .and_then(|point| row_at(self.items, point.y, self.style))
        {
            Some(row) if self.items[row].selectable() => mouse::Interaction::Pointer,
            _ => mouse::Interaction::None,
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: text::Renderer + 'a> From<Panel<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(panel: Panel<'a, Message>) -> Self {
        Element::new(panel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item<u8>> {
        vec![
            Item::action("one", 1),
            Item::separator(),
            Item::submenu("more", vec![Item::action("two", 2)]),
        ]
    }

    #[test]
    fn rows_stack_with_separator_height() {
        let items = items();
        let style = MenuStyle::default();
        assert_eq!(
            row_bounds(&items, 0, 200.0, style),
            Some(Rectangle::new(Point::ORIGIN, Size::new(200.0, 28.0)))
        );
        assert_eq!(
            row_bounds(&items, 2, 200.0, style),
            Some(Rectangle::new(
                Point::new(0.0, 36.0),
                Size::new(200.0, 28.0)
            ))
        );
        assert_eq!(row_bounds(&items, 3, 200.0, style), None);
        assert_eq!(row_at(&items, 0.0, style), Some(0));
        assert_eq!(row_at(&items, 27.9, style), Some(0));
        assert_eq!(row_at(&items, 28.0, style), Some(1));
        assert_eq!(row_at(&items, 36.0, style), Some(2));
        assert_eq!(row_at(&items, 64.0, style), None);
        assert_eq!(row_at(&items, -1.0, style), None);
    }

    #[test]
    fn panel_size_has_a_minimum_width_and_sums_rows() {
        let size = panel_size(&(), &items(), MenuStyle::default());
        assert_eq!(size, Size::new(MIN_PANEL_WIDTH, 64.0));
        assert_eq!(
            panel_size::<u8, ()>(&(), &[], MenuStyle::default()).height,
            0.0
        );
    }

    #[test]
    fn panel_reports_hover_changes_once_and_presses() {
        let items = items();
        let mut panel = Panel::new(&items, None)
            .on_hover(|row| row.map_or(99, |row| row as u8))
            .on_press(|row| 100 + row as u8);
        let mut tree = Tree {
            tag: tree::Tag::of::<PanelState>(),
            state: tree::State::new(PanelState::default()),
            children: Vec::new(),
        };
        let node = layout::Node::new(Size::new(200.0, 64.0));
        let mut send = |event: Event, at: Option<Point>| {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            Widget::<u8, iced_core::Theme, ()>::update(
                &mut panel,
                &mut tree,
                &event,
                Layout::new(&node),
                at.map_or(mouse::Cursor::Unavailable, mouse::Cursor::Available),
                &(),
                &mut iced_core::clipboard::Null,
                &mut shell,
                &Rectangle::with_size(Size::INFINITE),
            );
            messages
        };
        let moved = |x, y| {
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(x, y),
            })
        };
        assert_eq!(send(moved(5.0, 40.0), Some(Point::new(5.0, 40.0))), [2]);
        assert!(send(moved(6.0, 41.0), Some(Point::new(6.0, 41.0))).is_empty());
        assert_eq!(send(moved(5.0, 5.0), Some(Point::new(5.0, 5.0))), [0]);
        assert_eq!(send(Event::Mouse(mouse::Event::CursorLeft), None), [99]);
        assert_eq!(
            send(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Some(Point::new(5.0, 30.0))
            ),
            [101]
        );
        assert!(
            send(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Some(Point::new(5.0, 300.0))
            )
            .is_empty()
        );
        assert_eq!(
            send(
                Event::Touch(touch::Event::FingerPressed {
                    id: touch::Finger(0),
                    position: Point::new(5.0, 40.0),
                }),
                None
            ),
            [102]
        );
    }
}
