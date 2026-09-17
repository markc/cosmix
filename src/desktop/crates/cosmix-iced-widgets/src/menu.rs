//! Application menus with modal iced overlays and app-owned action messages.
//!
//! F10 activates the menu bar. A context target opens on right click, or
//! Shift+F10 after clicking the target. Arrows, Home/End, Enter/Space and
//! Escape navigate. Accelerator strings are labels; the app owns shortcuts.

use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, text,
    widget::{Operation, Tree, tree},
};
use iced::{Border, Color, Element, Event, Length, Point, Rectangle, Size, Vector, keyboard};

/// An action, submenu, or separator. Disabled entries cannot be selected.
#[derive(Debug, Clone)]
pub struct Item<Message> {
    label: String,
    accelerator: String,
    enabled: bool,
    kind: Kind<Message>,
}

#[derive(Debug, Clone)]
enum Kind<Message> {
    Action(Message),
    Submenu(Vec<Item<Message>>),
    Separator,
}

impl<Message> Item<Message> {
    pub fn action(label: impl Into<String>, message: Message) -> Self {
        Self {
            label: label.into(),
            accelerator: String::new(),
            enabled: true,
            kind: Kind::Action(message),
        }
    }

    pub fn submenu(label: impl Into<String>, items: Vec<Self>) -> Self {
        Self {
            label: label.into(),
            accelerator: String::new(),
            enabled: true,
            kind: Kind::Submenu(items),
        }
    }

    pub fn separator() -> Self {
        Self {
            label: String::new(),
            accelerator: String::new(),
            enabled: false,
            kind: Kind::Separator,
        }
    }

    pub fn accelerator(mut self, label: impl Into<String>) -> Self {
        self.accelerator = label.into();
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    fn selectable(&self) -> bool {
        self.enabled && !matches!(self.kind, Kind::Separator)
    }

    fn children(&self) -> &[Self] {
        match &self.kind {
            Kind::Submenu(items) => items,
            _ => &[],
        }
    }
}

/// Renderer-independent colours and logical-pixel metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuStyle {
    pub background: Color,
    pub text: Color,
    pub disabled: Color,
    pub selected: Color,
    pub selected_text: Color,
    pub border: Color,
    pub radius: f32,
    pub text_size: f32,
    pub row_height: f32,
    pub padding: f32,
}

impl Default for MenuStyle {
    fn default() -> Self {
        Self {
            background: Color::from_rgb8(35, 37, 42),
            text: Color::WHITE,
            disabled: Color::from_rgb8(130, 132, 140),
            selected: Color::from_rgb8(51, 91, 145),
            selected_text: Color::WHITE,
            border: Color::from_rgb8(70, 74, 82),
            radius: 4.0,
            text_size: 14.0,
            row_height: 28.0,
            padding: 10.0,
        }
    }
}

/// A horizontal menu bar, or a context-menu wrapper around arbitrary content.
pub struct Menu<'a, Message, Theme, Renderer> {
    items: Vec<Item<Message>>,
    content: Option<Element<'a, Message, Theme, Renderer>>,
    style: MenuStyle,
}

impl<'a, Message, Theme, Renderer> Menu<'a, Message, Theme, Renderer> {
    pub fn bar(items: Vec<Item<Message>>) -> Self {
        Self {
            items,
            content: None,
            style: MenuStyle::default(),
        }
    }

    pub fn context(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        items: Vec<Item<Message>>,
    ) -> Self {
        Self {
            items,
            content: Some(content.into()),
            style: MenuStyle::default(),
        }
    }

    pub fn style(mut self, style: MenuStyle) -> Self {
        self.style = style;
        self
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
struct State {
    open: bool,
    focused: bool,
    hovered: Option<usize>,
    root: usize,
    // One selected row per visible panel. None means pointer-opened, unselected.
    path: Vec<Option<usize>>,
    position: Point,
}

fn next<Message>(items: &[Item<Message>], selected: Option<usize>, forward: bool) -> Option<usize> {
    let len = items.len();
    (0..len)
        .map(|step| match selected {
            Some(index) if forward => (index + step + 1) % len,
            Some(index) => (index + len - (step + 1) % len) % len,
            None if forward => step,
            None => len - step - 1,
        })
        .find(|index| items[*index].selectable())
}

impl State {
    fn close(&mut self) {
        self.open = false;
        self.hovered = None;
        self.path.clear();
    }

    fn open(&mut self, items: &[Item<impl Clone>], bar: bool, root: usize, keyboard: bool) {
        self.root = root;
        self.open = true;
        let items = if bar {
            items.get(root).map(Item::children).unwrap_or(&[])
        } else {
            items
        };
        self.path = vec![if keyboard {
            next(items, None, true)
        } else {
            None
        }];
    }

    fn panel<'a, Message>(
        &self,
        items: &'a [Item<Message>],
        bar: bool,
        depth: usize,
    ) -> &'a [Item<Message>] {
        let mut items = if bar {
            items.get(self.root).map(Item::children).unwrap_or(&[])
        } else {
            items
        };
        for selected in self.path.iter().take(depth) {
            items = selected
                .and_then(|i| items.get(i))
                .map(Item::children)
                .unwrap_or(&[]);
        }
        items
    }

    fn activate<Message: Clone>(&mut self, items: &[Item<Message>], bar: bool) -> Option<Message> {
        let depth = self.path.len().checked_sub(1)?;
        let index = self.path[depth]?;
        let item = self.panel(items, bar, depth).get(index)?;
        if !item.selectable() {
            return None;
        }
        match &item.kind {
            Kind::Action(message) => {
                let message = message.clone();
                self.close();
                Some(message)
            }
            Kind::Submenu(children) => {
                self.path.push(next(children, None, true));
                None
            }
            Kind::Separator => None,
        }
    }

    fn key<Message: Clone>(
        &mut self,
        key: keyboard::key::Named,
        items: &[Item<Message>],
        bar: bool,
    ) -> Option<Message> {
        use keyboard::key::Named;
        let depth = self.path.len().checked_sub(1)?;
        match key {
            Named::ArrowDown | Named::ArrowUp | Named::Home | Named::End => {
                let selected = if matches!(key, Named::Home | Named::End) {
                    None
                } else {
                    self.path[depth]
                };
                self.path[depth] = next(
                    self.panel(items, bar, depth),
                    selected,
                    matches!(key, Named::ArrowDown | Named::Home),
                );
            }
            Named::Enter | Named::Space => {
                if bar
                    && depth == 0
                    && let Some(Item {
                        kind: Kind::Action(message),
                        enabled: true,
                        ..
                    }) = items.get(self.root)
                {
                    let message = message.clone();
                    self.close();
                    return Some(message);
                }
                return self.activate(items, bar);
            }
            Named::ArrowRight => {
                let children = self.path[depth]
                    .and_then(|i| self.panel(items, bar, depth).get(i))
                    .map(Item::children)
                    .unwrap_or(&[]);
                if !children.is_empty() {
                    self.path.push(next(children, None, true));
                } else if bar && let Some(root) = next(items, Some(self.root), true) {
                    self.open(items, true, root, true);
                }
            }
            Named::ArrowLeft if depth > 0 => {
                self.path.pop();
            }
            Named::ArrowLeft if bar => {
                if let Some(root) = next(items, Some(self.root), false) {
                    self.open(items, true, root, true);
                }
            }
            Named::Escape if depth > 0 => {
                self.path.pop();
            }
            Named::Escape | Named::Tab => self.close(),
            _ => {}
        }
        None
    }
}

fn row_height<Message>(item: &Item<Message>, style: MenuStyle) -> f32 {
    if matches!(item.kind, Kind::Separator) {
        8.0
    } else {
        style.row_height
    }
}

fn text_width<Renderer: text::Renderer>(renderer: &Renderer, value: &str, style: MenuStyle) -> f32 {
    use text::Paragraph;
    Renderer::Paragraph::with_text(text::Text {
        content: value,
        bounds: Size::INFINITE,
        size: style.text_size.into(),
        line_height: text::LineHeight::default(),
        font: renderer.default_font(),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
    })
    .min_width()
}

fn bar_rects<Message, Renderer: text::Renderer>(
    renderer: &Renderer,
    items: &[Item<Message>],
    bounds: Rectangle,
    style: MenuStyle,
) -> Vec<Rectangle> {
    let mut x = bounds.x;
    items
        .iter()
        .map(|item| {
            let width = text_width(renderer, &item.label, style) + style.padding * 2.0;
            let rect = Rectangle {
                x,
                y: bounds.y,
                width,
                height: bounds.height,
            };
            x += width;
            rect
        })
        .collect()
}

fn quad<Renderer: renderer::Renderer>(
    renderer: &mut Renderer,
    bounds: Rectangle,
    background: Color,
    style: MenuStyle,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border {
                color: style.border,
                width: 1.0,
                radius: style.radius.into(),
            },
            ..Default::default()
        },
        background,
    );
}

fn label<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    value: &str,
    bounds: Rectangle,
    color: Color,
    style: MenuStyle,
    right: bool,
) {
    renderer.fill_text(
        text::Text {
            content: value.to_owned(),
            bounds: bounds.size(),
            size: style.text_size.into(),
            line_height: text::LineHeight::default(),
            font: renderer.default_font(),
            align_x: if right {
                text::Alignment::Right
            } else {
                text::Alignment::Left
            },
            align_y: iced::alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
        },
        Point::new(
            if right {
                bounds.x + bounds.width
            } else {
                bounds.x
            },
            bounds.center_y(),
        ),
        color,
        bounds,
    );
}

impl<Message: Clone, Theme, Renderer: text::Renderer> Widget<Message, Theme, Renderer>
    for Menu<'_, Message, Theme, Renderer>
{
    fn size(&self) -> Size<Length> {
        self.content
            .as_ref()
            .map(|content| content.as_widget().size())
            .unwrap_or(Size::new(
                Length::Fill,
                Length::Fixed(self.style.row_height),
            ))
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        self.content.iter().map(Tree::new).collect()
    }
    fn diff(&self, tree: &mut Tree) {
        if let Some(content) = &self.content {
            tree.diff_children(std::slice::from_ref(content));
        } else {
            tree.children.clear();
        }
        // Rebuilt/dynamic menu models must never retain an invalid navigation path.
        let state = tree.state.downcast_mut::<State>();
        let bar = self.content.is_none();
        if state.open
            && ((bar
                && self
                    .items
                    .get(state.root)
                    .is_none_or(|item| !item.selectable()))
                || state.path.iter().enumerate().any(|(depth, selected)| {
                    selected.is_some_and(|index| {
                        state
                            .panel(&self.items, bar, depth)
                            .get(index)
                            .is_none_or(|item| !item.selectable())
                    })
                }))
        {
            state.close();
        }
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        if let Some(content) = &mut self.content {
            let node = content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);
            layout::Node::with_children(node.size(), vec![node])
        } else {
            layout::Node::new(limits.resolve(
                Length::Fill,
                Length::Fixed(self.style.row_height),
                Size::ZERO,
            ))
        }
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
        if let Some(content) = &self.content {
            content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout.children().next().unwrap(),
                cursor,
                viewport,
            );
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        quad(renderer, layout.bounds(), self.style.background, self.style);
        for (index, (item, rect)) in self
            .items
            .iter()
            .zip(bar_rects(
                renderer,
                &self.items,
                layout.bounds(),
                self.style,
            ))
            .enumerate()
        {
            let selected = (state.open && state.root == index)
                || (!state.open && state.hovered == Some(index));
            if selected {
                quad(renderer, rect, self.style.selected, self.style);
            }
            let color = if !item.selectable() {
                self.style.disabled
            } else if selected {
                self.style.selected_text
            } else {
                self.style.text
            };
            label(
                renderer,
                &item.label,
                Rectangle {
                    x: rect.x + self.style.padding,
                    width: (rect.width - self.style.padding * 2.0).max(0.0),
                    ..rect
                },
                color,
                self.style,
                false,
            );
        }
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Some(content) = &mut self.content {
            content.as_widget_mut().operate(
                &mut tree.children[0],
                layout.children().next().unwrap(),
                renderer,
                operation,
            );
        }
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
        let state = tree.state.downcast_mut::<State>();
        if state.open {
            return;
        }
        let bar = self.content.is_none();
        let mut opened = false;
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) if bar => {
                let hovered = bar_rects(renderer, &self.items, layout.bounds(), self.style)
                    .iter()
                    .enumerate()
                    .find(|(index, rect)| self.items[*index].selectable() && cursor.is_over(**rect))
                    .map(|(index, _)| index);
                if state.hovered != hovered {
                    state.hovered = hovered;
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                state.focused = cursor.is_over(layout.bounds());
                if state.focused && bar && *button == mouse::Button::Left {
                    if let Some(index) =
                        bar_rects(renderer, &self.items, layout.bounds(), self.style)
                            .iter()
                            .position(|rect| cursor.is_over(*rect))
                        && self.items[index].selectable()
                    {
                        if let Kind::Action(message) = &self.items[index].kind {
                            shell.publish(message.clone());
                        } else {
                            state.open(&self.items, true, index, false);
                        }
                        opened = true;
                    }
                } else if state.focused && !bar && *button == mouse::Button::Right {
                    state.position = cursor.position().unwrap_or(layout.bounds().position());
                    state.open(&self.items, false, 0, false);
                    opened = true;
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::F10),
                modifiers,
                ..
            }) if (bar && !modifiers.shift()) || (!bar && state.focused && modifiers.shift()) => {
                if let Some(index) = next(&self.items, None, true) {
                    state.position = layout.bounds().position();
                    state.open(&self.items, bar, index, true);
                    opened = true;
                }
            }
            _ => {}
        }
        if opened {
            shell.capture_event();
            shell.invalidate_layout();
            shell.request_redraw();
        } else if let Some(content) = &mut self.content {
            content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                layout.children().next().unwrap(),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
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
        if let Some(content) = &self.content {
            content.as_widget().mouse_interaction(
                &tree.children[0],
                layout.children().next().unwrap(),
                cursor,
                viewport,
                renderer,
            )
        } else if bar_rects(renderer, &self.items, layout.bounds(), self.style)
            .iter()
            .zip(&self.items)
            .any(|(rect, item)| item.selectable() && cursor.is_over(*rect))
        {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
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
        let state = tree.state.downcast_mut::<State>();
        if state.open {
            Some(overlay::Element::new(Box::new(Popup {
                items: &self.items,
                state,
                bar: self.content.is_none(),
                anchor: layout.bounds() + translation,
                translation,
                style: self.style,
            })))
        } else if let Some(content) = &mut self.content {
            content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout.children().next().unwrap(),
                renderer,
                viewport,
                translation,
            )
        } else {
            None
        }
    }
}

impl<'a, Message: Clone + 'a, Theme: 'a, Renderer: text::Renderer + 'a>
    From<Menu<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
{
    fn from(menu: Menu<'a, Message, Theme, Renderer>) -> Self {
        Self::new(menu)
    }
}

struct Popup<'a, Message> {
    items: &'a [Item<Message>],
    state: &'a mut State,
    bar: bool,
    anchor: Rectangle,
    translation: Vector,
    style: MenuStyle,
}

impl<Message> Popup<'_, Message> {
    fn hit(&self, layout: Layout<'_>, cursor: mouse::Cursor) -> Option<(usize, usize)> {
        let panels: Vec<_> = layout.children().collect();
        for (depth, panel) in panels.iter().enumerate().rev() {
            if let Some(position) = cursor.position_over(panel.bounds()) {
                let mut y = panel.bounds().y;
                for (index, item) in self
                    .state
                    .panel(self.items, self.bar, depth)
                    .iter()
                    .enumerate()
                {
                    let height = row_height(item, self.style);
                    if position.y >= y && position.y < y + height {
                        return Some((depth, index));
                    }
                    y += height;
                }
            }
        }
        None
    }
}

impl<Message: Clone, Theme, Renderer: text::Renderer> overlay::Overlay<Message, Theme, Renderer>
    for Popup<'_, Message>
{
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let mut panels = Vec::new();
        let mut position = if self.bar {
            bar_rects(renderer, self.items, self.anchor, self.style)
                .get(self.state.root)
                .map(|rect| Point::new(rect.x, rect.y + rect.height))
                .unwrap_or(self.anchor.position())
        } else {
            self.state.position + self.translation
        };
        for depth in 0..self.state.path.len() {
            let items = self.state.panel(self.items, self.bar, depth);
            let width = items
                .iter()
                .map(|item| {
                    text_width(renderer, &item.label, self.style)
                        + text_width(renderer, &item.accelerator, self.style)
                        + if matches!(item.kind, Kind::Submenu(_)) {
                            text_width(renderer, "  ›", self.style)
                        } else {
                            0.0
                        }
                        + self.style.padding * 3.0
                })
                .fold(160.0, f32::max)
                .min(bounds.width);
            let height = items
                .iter()
                .map(|item| row_height(item, self.style))
                .sum::<f32>();
            if position.x + width > bounds.width && depth > 0 {
                let previous: &layout::Node = &panels[depth - 1];
                position.x = previous.bounds().x - width;
            }
            position.x = position.x.clamp(0.0, (bounds.width - width).max(0.0));
            position.y = position.y.clamp(0.0, (bounds.height - height).max(0.0));
            panels.push(layout::Node::new(Size::new(width, height)).move_to(position));
            let selected = self.state.path[depth].unwrap_or(0);
            let offset = items
                .iter()
                .take(selected)
                .map(|item| row_height(item, self.style))
                .sum::<f32>();
            position = Point::new(position.x + width, position.y + offset);
        }
        // Full-window bounds make dismissal modal, including outside clicks.
        layout::Node::with_children(bounds, panels)
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
    ) {
        for (depth, panel) in layout.children().enumerate() {
            let bounds = panel.bounds();
            quad(renderer, bounds, self.style.background, self.style);
            let mut y = bounds.y;
            for (index, item) in self
                .state
                .panel(self.items, self.bar, depth)
                .iter()
                .enumerate()
            {
                let height = row_height(item, self.style);
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
                                x: row.x + self.style.padding,
                                y: row.center_y(),
                                width: (row.width - self.style.padding * 2.0).max(0.0),
                                height: 1.0,
                            },
                            ..Default::default()
                        },
                        self.style.border,
                    );
                    continue;
                }
                let selected = self.state.path[depth] == Some(index);
                if selected {
                    quad(renderer, row, self.style.selected, self.style);
                }
                let color = if !item.selectable() {
                    self.style.disabled
                } else if selected {
                    self.style.selected_text
                } else {
                    self.style.text
                };
                let text_bounds = Rectangle {
                    x: row.x + self.style.padding,
                    width: (row.width - self.style.padding * 2.0).max(0.0),
                    ..row
                };
                label(renderer, &item.label, text_bounds, color, self.style, false);
                let trailing = if matches!(item.kind, Kind::Submenu(_)) {
                    format!("{}  ›", item.accelerator)
                } else {
                    item.accelerator.clone()
                };
                label(renderer, &trailing, text_bounds, color, self.style, true);
            }
        }
    }
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        let before = self.state.clone();
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                ..
            }) => {
                if let Some(message) = self.state.key(*key, self.items, self.bar) {
                    shell.publish(message);
                }
                shell.capture_event();
            }
            Event::Keyboard(_) | Event::InputMethod(_) | Event::Touch(_) => shell.capture_event(),
            Event::Mouse(mouse::Event::CursorMoved { .. })
            | Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let clicked = matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)));
                if let Some((depth, index)) = self.hit(layout, cursor) {
                    let item = &self.state.panel(self.items, self.bar, depth)[index];
                    if item.selectable() {
                        let changed = self.state.path[depth] != Some(index);
                        if changed || clicked {
                            self.state.path.truncate(depth + 1);
                            self.state.path[depth] = Some(index);
                            if clicked {
                                if let Some(message) = self.state.activate(self.items, self.bar) {
                                    shell.publish(message);
                                }
                            } else if matches!(item.kind, Kind::Submenu(_)) {
                                self.state.path.push(None);
                            }
                        }
                    } else {
                        self.state.path.truncate(depth + 1);
                        self.state.path[depth] = None;
                    }
                } else if self.bar
                    && let Some(index) = bar_rects(renderer, self.items, self.anchor, self.style)
                        .iter()
                        .position(|rect| cursor.is_over(*rect))
                {
                    if clicked
                        && self.items[index].selectable()
                        && let Kind::Action(message) = &self.items[index].kind
                    {
                        shell.publish(message.clone());
                        self.state.close();
                    } else if self.items[index].selectable() && self.state.root != index {
                        self.state.open(self.items, true, index, false);
                    } else if clicked {
                        self.state.close();
                    }
                } else if clicked {
                    self.state.close();
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(_)) => {
                self.state.close();
                shell.capture_event();
            }
            Event::Mouse(_) => shell.capture_event(),
            Event::Window(iced::window::Event::Unfocused) => self.state.close(),
            _ => {}
        }
        if *self.state != before {
            shell.invalidate_layout();
            shell.request_redraw();
        }
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.hit(layout, cursor).is_some_and(|(depth, index)| {
            self.state.panel(self.items, self.bar, depth)[index].selectable()
        }) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::key::Named;

    #[cfg(debug_assertions)]
    struct Harness {
        menu: Menu<'static, u8, iced::Theme, ()>,
        tree: Tree,
        node: layout::Node,
    }

    #[cfg(debug_assertions)]
    impl Harness {
        fn new(mut menu: Menu<'static, u8, iced::Theme, ()>) -> Self {
            let mut tree = Tree::new(&menu as &dyn Widget<u8, iced::Theme, ()>);
            let node = menu.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 300.0)),
            );
            Self { menu, tree, node }
        }

        fn event(
            &mut self,
            event: Event,
            cursor: mouse::Cursor,
            popup: bool,
        ) -> (Vec<u8>, bool, iced::window::RedrawRequest) {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
            if popup {
                let mut overlay = self
                    .menu
                    .overlay(
                        &mut self.tree,
                        Layout::new(&self.node),
                        &(),
                        &viewport,
                        Vector::ZERO,
                    )
                    .expect("menu is open");
                let node = overlay.as_overlay_mut().layout(&(), viewport.size());
                overlay.as_overlay_mut().update(
                    &event,
                    Layout::new(&node),
                    cursor,
                    &(),
                    &mut iced::advanced::clipboard::Null,
                    &mut shell,
                );
            } else {
                self.menu.update(
                    &mut self.tree,
                    &event,
                    Layout::new(&self.node),
                    cursor,
                    &(),
                    &mut iced::advanced::clipboard::Null,
                    &mut shell,
                    &viewport,
                );
            }
            let captured = shell.is_event_captured();
            let redraw = shell.redraw_request();
            (messages, captured, redraw)
        }
    }

    #[cfg(debug_assertions)]
    fn key_event(key: Named, modifiers: keyboard::Modifiers) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    #[cfg(debug_assertions)]
    fn widget_hover_redraws_only_when_highlight_changes() {
        let mut harness = Harness::new(Menu::bar(vec![Item::submenu("menu", items())]));
        let position = Point::new(5.0, 5.0);
        let event = Event::Mouse(mouse::Event::CursorMoved { position });
        let cursor = mouse::Cursor::Available(position);
        assert_eq!(
            harness.event(event.clone(), cursor, false).2,
            iced::window::RedrawRequest::NextFrame
        );
        assert_eq!(
            harness.event(event, cursor, false).2,
            iced::window::RedrawRequest::Wait
        );
        assert_eq!(
            harness
                .event(
                    Event::Mouse(mouse::Event::CursorLeft),
                    mouse::Cursor::Unavailable,
                    false
                )
                .2,
            iced::window::RedrawRequest::NextFrame
        );
        assert_eq!(harness.tree.state.downcast_ref::<State>().hovered, None);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn widget_bar_keyboard_and_overlay_pointer_emit_actions() {
        let mut harness = Harness::new(Menu::bar(vec![Item::submenu("menu", items())]));
        let (_, captured, _) = harness.event(
            key_event(Named::F10, keyboard::Modifiers::empty()),
            mouse::Cursor::Unavailable,
            false,
        );
        assert!(captured);
        assert_eq!(harness.tree.state.downcast_ref::<State>().path, [Some(2)]);
        // Popup starts below the 28px bar: disabled (28), separator (8), action (28).
        let (messages, captured, _) = harness.event(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(Point::new(10.0, 70.0)),
            true,
        );
        assert!(captured);
        assert_eq!(messages, [1]);
        assert!(!harness.tree.state.downcast_ref::<State>().open);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn context_overlay_skips_disabled_and_captures_outside_dismissal() {
        let mut harness = Harness::new(Menu::context(
            iced::widget::Space::new().width(200).height(100),
            items(),
        ));
        let cursor = mouse::Cursor::Available(Point::new(20.0, 20.0));
        let (_, captured, _) = harness.event(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
            cursor,
            false,
        );
        assert!(captured);
        let (messages, captured, _) = harness.event(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            cursor,
            true,
        );
        assert!(captured);
        assert!(messages.is_empty());
        assert!(harness.tree.state.downcast_ref::<State>().open);
        harness.event(
            key_event(Named::ArrowDown, keyboard::Modifiers::empty()),
            cursor,
            true,
        );
        assert_eq!(harness.tree.state.downcast_ref::<State>().path, [Some(2)]);
        let (messages, captured, _) = harness.event(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(Point::new(390.0, 290.0)),
            true,
        );
        assert!(messages.is_empty());
        assert!(captured);
        assert!(!harness.tree.state.downcast_ref::<State>().open);
        // Context keyboard activation uses the focus retained by its target.
        harness.event(
            key_event(Named::F10, keyboard::Modifiers::SHIFT),
            cursor,
            false,
        );
        assert!(harness.tree.state.downcast_ref::<State>().open);
    }

    fn items() -> Vec<Item<u8>> {
        vec![
            Item::action("disabled", 0).enabled(false),
            Item::separator(),
            Item::action("one", 1),
            Item::submenu("more", vec![Item::separator(), Item::action("two", 2)]),
        ]
    }

    #[test]
    fn navigation_skips_disabled_and_separators_and_wraps() {
        let items = items();
        let mut state = State::default();
        state.open(&items, false, 0, true);
        assert_eq!(state.path, [Some(2)]);
        state.key(Named::ArrowUp, &items, false);
        assert_eq!(state.path, [Some(3)]);
        state.key(Named::ArrowDown, &items, false);
        assert_eq!(state.path, [Some(2)]);
        state.key(Named::End, &items, false);
        assert_eq!(state.path, [Some(3)]);
        state.key(Named::Home, &items, false);
        assert_eq!(state.path, [Some(2)]);
    }

    #[test]
    fn bar_action_roots_activate_from_keyboard() {
        let items = vec![Item::action("run", 7)];
        let mut state = State::default();
        state.open(&items, true, 0, true);
        assert_eq!(state.key(Named::Enter, &items, true), Some(7));
        assert!(!state.open);
    }

    #[test]
    fn nested_navigation_and_activation_emit_only_action() {
        let items = items();
        let mut state = State::default();
        state.open(&items, false, 0, true);
        state.key(Named::End, &items, false);
        assert_eq!(state.key(Named::ArrowRight, &items, false), None);
        assert_eq!(state.path, [Some(3), Some(1)]);
        state.key(Named::ArrowLeft, &items, false);
        assert_eq!(state.path, [Some(3)]);
        state.key(Named::Enter, &items, false);
        assert_eq!(state.key(Named::Enter, &items, false), Some(2));
        assert!(!state.open);
        assert!(state.path.is_empty());
    }

    #[test]
    fn empty_or_fully_disabled_menus_are_safe() {
        for items in [
            vec![],
            vec![
                Item::action("disabled", 1).enabled(false),
                Item::separator(),
            ],
        ] {
            let mut state = State::default();
            state.open(&items, false, 0, true);
            for key in [
                Named::Home,
                Named::End,
                Named::ArrowDown,
                Named::ArrowUp,
                Named::ArrowRight,
                Named::Enter,
            ] {
                assert_eq!(state.key(key, &items, false), None);
            }
            assert_eq!(state.path, [None]);
            state.key(Named::Escape, &items, false);
            assert!(!state.open);
        }
    }

    #[test]
    fn bar_arrows_switch_roots_and_escape_unwinds() {
        let menus = vec![
            Item::submenu("first", items()),
            Item::submenu("disabled", items()).enabled(false),
            Item::submenu("last", items()),
        ];
        let mut state = State::default();
        state.open(&menus, true, 0, true);
        state.key(Named::ArrowLeft, &menus, true);
        assert_eq!(state.root, 2);
        state.key(Named::End, &menus, true);
        state.key(Named::ArrowRight, &menus, true);
        assert_eq!(state.path.len(), 2);
        state.key(Named::Escape, &menus, true);
        assert_eq!(state.path.len(), 1);
        assert!(state.open);
        state.key(Named::Escape, &menus, true);
        assert!(!state.open);
    }
}
