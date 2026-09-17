//! Application menus with modal iced overlays and app-owned action messages.
//!
//! F10 activates the menu bar. A context target opens on right click, or
//! Shift+F10 after clicking the target. Arrows, Home/End, Enter/Space and
//! Escape navigate. Accelerator strings are labels; the app owns shortcuts.

use iced::advanced::{
    Clipboard, Layout, Shell, Widget, input_method, layout, mouse, overlay, renderer, text,
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
    /// An entry that publishes `message` when activated, then closes the menu.
    pub fn action(label: impl Into<String>, message: Message) -> Self {
        Self {
            label: label.into(),
            accelerator: String::new(),
            enabled: true,
            kind: Kind::Action(message),
        }
    }

    /// An entry that opens a nested panel of `items`.
    pub fn submenu(label: impl Into<String>, items: Vec<Self>) -> Self {
        Self {
            label: label.into(),
            accelerator: String::new(),
            enabled: true,
            kind: Kind::Submenu(items),
        }
    }

    /// A horizontal rule; never selectable.
    pub fn separator() -> Self {
        Self {
            label: String::new(),
            accelerator: String::new(),
            enabled: false,
            kind: Kind::Separator,
        }
    }

    /// Right-aligned shortcut label, for display only. The app binds the key.
    pub fn accelerator(mut self, label: impl Into<String>) -> Self {
        self.accelerator = label.into();
        self
    }

    /// A disabled entry is drawn muted and skipped by pointer and keyboard.
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
    /// Bar and panel fill.
    pub background: Color,
    /// Enabled label and accelerator colour.
    pub text: Color,
    /// Disabled label colour.
    pub disabled: Color,
    /// Highlight fill of the selected or hovered row.
    pub selected: Color,
    /// Label colour on the highlight.
    pub selected_text: Color,
    /// Panel outline and separator colour.
    pub border: Color,
    /// Corner radius of panels and highlights.
    pub radius: f32,
    /// Label size in logical pixels.
    pub text_size: f32,
    /// Height of the bar and of each non-separator row.
    pub row_height: f32,
    /// Horizontal padding around labels.
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
    /// A full-width bar whose top-level `items` are usually submenus. A
    /// top-level action publishes directly. F10 activates the bar.
    pub fn bar(items: Vec<Item<Message>>) -> Self {
        Self {
            items,
            content: None,
            style: MenuStyle::default(),
        }
    }

    /// Wraps `content`; right-click on it, or Shift+F10 while it (or a
    /// focusable child) has focus, opens `items` as a popup.
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

    /// Replaces the default style; see `Tokens::menu_style`.
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
    // Set even by the inert closed overlay, before iced dispatches a batch.
    overlay_bounds: Option<Size>,
    translation: Vector,
}

#[derive(Default)]
struct ChildFocus {
    present: bool,
    focused: bool,
}

impl Operation for ChildFocus {
    fn focusable(
        &mut self,
        _id: Option<&iced::advanced::widget::Id>,
        _bounds: Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.present = true;
        self.focused |= state.is_focused();
    }

    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
}

// State-only events that content must see even while a menu is open. IME
// preedit and commit are input, so the open menu blocks them like key presses.
fn housekeeping(event: &Event) -> bool {
    matches!(
        event,
        Event::Window(_)
            | Event::InputMethod(input_method::Event::Opened | input_method::Event::Closed)
            | Event::Keyboard(keyboard::Event::ModifiersChanged(_))
    )
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
        let previously_captured = shell.is_event_captured();
        let state = tree.state.downcast_mut::<State>();
        if state.open && !housekeeping(event) {
            if previously_captured {
                return;
            }
            // iced obtains overlays before processing a batch. If this batch
            // opened the menu, subsequent events still arrive at the base
            // widget. Dispatch through the same popup path until iced rebuilds
            // the overlay. Events captured by an existing overlay never reach
            // the base widget (and the guard above also protects composition).
            let translation = state.translation;
            let bounds = state.overlay_bounds.unwrap_or(viewport.size());
            let mut popup: overlay::Element<'_, Message, Theme, Renderer> =
                overlay::Element::new(Box::new(Popup {
                    items: &self.items,
                    state,
                    bar: self.content.is_none(),
                    anchor: layout.bounds() + translation,
                    translation,
                    style: self.style,
                }));
            let node = popup.as_overlay_mut().layout(renderer, bounds);
            popup.as_overlay_mut().update(
                event,
                Layout::new(&node),
                cursor + translation,
                renderer,
                clipboard,
                shell,
            );
            drop(popup);
            if state.open && shell.is_event_captured() {
                // A capture in iced's base pass clears its stored overlay,
                // even for a key release that does not change our state.
                shell.invalidate_layout();
            }
            return;
        }
        let bar = self.content.is_none();
        if let Event::Touch(iced::touch::Event::FingerPressed { position, .. }) = event {
            state.focused =
                !previously_captured && layout.bounds().contains(*position - state.translation);
        } else if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
            state.focused = !previously_captured && cursor.is_over(layout.bounds());
        } else if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
            state.focused = false;
            if state.open {
                state.close();
                shell.request_redraw();
            }
        }
        // Like iced containers, always forward lifecycle and focus-changing
        // events, even when an earlier sibling captured the event. Children
        // get first refusal on context triggers, so nested menus prefer inner.
        if let Some(content) = &mut self.content {
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
            if matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::F10),
                    ..
                })
            ) {
                let mut focus = ChildFocus::default();
                content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout.children().next().unwrap(),
                    renderer,
                    &mut focus,
                );
                if focus.present {
                    state.focused = focus.focused;
                }
            }
        }
        if shell.is_event_captured() || state.open {
            return;
        }
        let touch_event;
        let (event, cursor) =
            if let Event::Touch(iced::touch::Event::FingerPressed { position, .. }) = event {
                touch_event = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
                (
                    &touch_event,
                    mouse::Cursor::Available(*position - state.translation),
                )
            } else {
                (event, cursor)
            };
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
        state.translation = translation;
        let open = state.open;
        let bar = self.content.is_none();
        let popup = overlay::Element::new(Box::new(Popup {
            items: &self.items,
            state,
            bar,
            anchor: layout.bounds() + translation,
            translation,
            style: self.style,
        }));
        let mut overlays = vec![popup];
        if !open
            && let Some(content) = &mut self.content
            && let Some(child) = content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout.children().next().unwrap(),
                renderer,
                viewport,
                translation,
            )
        {
            overlays.push(child);
        }
        // The closed popup is inert. Keeping it present lets iced finish its
        // complete event batch on dismissal and supplies true window bounds
        // for a menu opened during the subsequent base-widget pass.
        Some(overlay::Group::with_children(overlays).overlay())
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
        self.state.overlay_bounds = Some(bounds);
        if !self.state.open {
            return layout::Node::new(bounds);
        }
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
        if !self.state.open {
            return;
        }
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
        if !self.state.open || shell.is_event_captured() {
            return;
        }
        let touch_event;
        let (event, cursor) = match event {
            Event::Touch(iced::touch::Event::FingerPressed { position, .. }) => {
                touch_event = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
                (&touch_event, mouse::Cursor::Available(*position))
            }
            Event::Touch(iced::touch::Event::FingerMoved { position, .. }) => {
                touch_event = Event::Mouse(mouse::Event::CursorMoved {
                    position: *position,
                });
                (&touch_event, mouse::Cursor::Available(*position))
            }
            Event::Touch(
                iced::touch::Event::FingerLifted { position, .. }
                | iced::touch::Event::FingerLost { position, .. },
            ) => {
                touch_event = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
                (&touch_event, mouse::Cursor::Available(*position))
            }
            _ => (event, cursor),
        };
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
            // The menu is modal: no key press or IME text may reach the app or
            // `keyboard::listen` subscriptions behind it.
            Event::Keyboard(
                keyboard::Event::KeyPressed { .. } | keyboard::Event::KeyReleased { .. },
            )
            | Event::InputMethod(
                input_method::Event::Preedit(..) | input_method::Event::Commit(_),
            ) => shell.capture_event(),
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
            if self.state.open {
                shell.invalidate_layout();
            }
            shell.request_redraw();
        }
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if !self.state.open {
            return mouse::Interaction::None;
        }
        if self.hit(layout, cursor).is_some_and(|(depth, index)| {
            self.state.panel(self.items, self.bar, depth)[index].selectable()
        }) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::key::Named;

    #[cfg(debug_assertions)]
    #[derive(Default)]
    struct Recorder {
        quads: Vec<Rectangle>,
    }

    #[cfg(debug_assertions)]
    impl renderer::Renderer for Recorder {
        fn start_layer(&mut self, _: Rectangle) {}
        fn end_layer(&mut self) {}
        fn start_transformation(&mut self, _: iced::Transformation) {}
        fn end_transformation(&mut self) {}
        fn fill_quad(&mut self, quad: renderer::Quad, _: impl Into<iced::Background>) {
            self.quads.push(quad.bounds);
        }
        fn reset(&mut self, _: Rectangle) {
            self.quads.clear();
        }
        fn allocate_image(
            &mut self,
            handle: &iced::advanced::image::Handle,
            callback: impl FnOnce(
                Result<iced::advanced::image::Allocation, iced::advanced::image::Error>,
            ) + Send
            + 'static,
        ) {
            renderer::Renderer::allocate_image(&mut (), handle, callback);
        }
    }

    #[cfg(debug_assertions)]
    impl text::Renderer for Recorder {
        type Font = iced::Font;
        type Paragraph = ();
        type Editor = ();
        const ICON_FONT: iced::Font = iced::Font::DEFAULT;
        const CHECKMARK_ICON: char = 'x';
        const ARROW_DOWN_ICON: char = 'v';
        const SCROLL_UP_ICON: char = '^';
        const SCROLL_DOWN_ICON: char = 'v';
        const SCROLL_LEFT_ICON: char = '<';
        const SCROLL_RIGHT_ICON: char = '>';
        const ICED_LOGO: char = 'i';
        fn default_font(&self) -> iced::Font {
            iced::Font::DEFAULT
        }
        fn default_size(&self) -> iced::Pixels {
            iced::Pixels(14.0)
        }
        fn fill_paragraph(&mut self, _: &(), _: Point, _: Color, _: Rectangle) {}
        fn fill_editor(&mut self, _: &(), _: Point, _: Color, _: Rectangle) {}
        fn fill_text(&mut self, _: text::Text<String>, _: Point, _: Color, _: Rectangle) {}
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_open_and_release_batch_keeps_drawn_overlay_and_close_keeps_tail() {
        let mut renderer = Recorder::default();
        let menu: Menu<'_, u8, iced::Theme, Recorder> =
            Menu::bar(vec![Item::submenu("file", items())]);
        let mut ui = iced_runtime::UserInterface::build(
            menu,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut renderer,
        );
        let release = Event::Keyboard(keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(Named::F10),
            modified_key: keyboard::Key::Named(Named::F10),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        });
        let mut messages = vec![];
        let (_, statuses) = ui.update(
            &[key_event(Named::F10, keyboard::Modifiers::empty()), release],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses.len(), 2);
        ui.draw(
            &mut renderer,
            &iced::Theme::Dark,
            &renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        assert!(
            renderer
                .quads
                .iter()
                .any(|rect| rect.y == 28.0 && rect.width == 160.0),
            "popup must actually draw after captured key release"
        );
        let (_, statuses) = ui.update(
            &[
                key_event(Named::Escape, keyboard::Modifiers::empty()),
                Event::Keyboard(keyboard::Event::ModifiersChanged(
                    keyboard::Modifiers::empty(),
                )),
                Event::Window(iced::window::Event::Unfocused),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            statuses.len(),
            3,
            "iced must not drop events following menu dismissal"
        );
        assert_eq!(statuses[1], iced::event::Status::Ignored);
        ui.draw(
            &mut renderer,
            &iced::Theme::Dark,
            &renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        assert!(
            !renderer
                .quads
                .iter()
                .any(|rect| rect.y == 28.0 && rect.width == 160.0)
        );
    }

    #[cfg(debug_assertions)]
    fn character(value: &'static str) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(value.into()),
            modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some(value.into()),
            repeat: false,
        })
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_focused_child_opens_context_and_receives_modifiers_and_ime() {
        let id = iced::advanced::widget::Id::new("field");
        let field = iced::widget::text_input("", "")
            .id(id.clone())
            .on_input(|value| value);
        let menu: Menu<'_, String, iced::Theme, ()> =
            Menu::context(field, vec![Item::action("action", "action".to_owned())]);
        let mut ui = iced_runtime::UserInterface::build(
            menu,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut (),
        );
        ui.operate(
            &(),
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let mut messages = vec![];
        let (_, statuses) = ui.update(
            &[
                Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::CTRL)),
                key_event(Named::F10, keyboard::Modifiers::SHIFT),
            ],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            statuses[1],
            iced::event::Status::Captured,
            "keyboard-focused child enables context shortcut without click"
        );
        let (_, statuses) = ui.update(
            &[
                character("q"),
                Event::InputMethod(iced::advanced::input_method::Event::Preedit(
                    "界".into(),
                    Some(0..3),
                )),
                Event::InputMethod(iced::advanced::input_method::Event::Commit("界".into())),
            ],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            statuses,
            [iced::event::Status::Captured; 3],
            "an open menu blocks key presses and IME text from the app behind it"
        );
        assert!(messages.is_empty());
        let (_, statuses) = ui.update(
            &[
                Event::Keyboard(keyboard::Event::ModifiersChanged(
                    keyboard::Modifiers::empty(),
                )),
                Event::InputMethod(iced::advanced::input_method::Event::Closed),
                key_event(Named::Escape, keyboard::Modifiers::empty()),
                character("c"),
            ],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses.len(), 4);
        assert_eq!(statuses[0], iced::event::Status::Ignored);
        assert_eq!(
            messages,
            ["c"],
            "released Control must not turn typing into Copy, nor may Escape drop the tail"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_captured_sibling_click_unfocuses_context_child() {
        let id = iced::advanced::widget::Id::new("field");
        let menu: Menu<'_, String, iced::Theme, ()> = Menu::context(
            iced::widget::text_input("", "")
                .id(id.clone())
                .on_input(|value| value),
            vec![Item::action("action", "action".to_owned())],
        );
        let root = iced::widget::column![
            iced::widget::button("button").on_press("button".to_owned()),
            menu
        ];
        let mut ui = iced_runtime::UserInterface::build(
            root,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut (),
        );
        ui.operate(
            &(),
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let mut messages = vec![];
        ui.update(
            &[
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(Point::new(5.0, 5.0)),
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        let (_, statuses) = ui.update(
            &[
                key_event(Named::F10, keyboard::Modifiers::SHIFT),
                character("x"),
            ],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            statuses,
            [iced::event::Status::Ignored, iced::event::Status::Ignored]
        );
        assert_eq!(messages, ["button"]);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_nested_context_prefers_inner_and_touch_can_activate_it() {
        let inner = Menu::context(
            iced::widget::Space::new().width(200).height(100),
            vec![Item::action("inner", 1)],
        );
        let outer: Menu<'_, u8, iced::Theme, ()> =
            Menu::context(inner, vec![Item::action("outer", 2)]);
        let mut ui = iced_runtime::UserInterface::build(
            outer,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut (),
        );
        let mut messages = vec![];
        ui.update(
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Right,
            ))],
            mouse::Cursor::Available(Point::new(20.0, 20.0)),
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        ui.update(
            &[Event::Touch(iced::touch::Event::FingerPressed {
                id: iced::touch::Finger(0),
                position: Point::new(25.0, 25.0),
            })],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, [1]);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_same_batch_popup_hit_testing_uses_scrolled_window_coordinates() {
        let id = iced::advanced::widget::Id::new("scroll");
        let menu: Menu<'_, u8, iced::Theme, ()> =
            Menu::bar(vec![Item::submenu("file", vec![Item::action("run", 9)])]);
        let root = iced::widget::scrollable(iced::widget::column![
            iced::widget::Space::new().height(100),
            menu,
            iced::widget::Space::new().height(400)
        ])
        .id(id.clone())
        .width(200)
        .height(100);
        let mut ui = iced_runtime::UserInterface::build(
            root,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut (),
        );
        ui.operate(
            &(),
            &mut iced::advanced::widget::operation::scrollable::scroll_to::<()>(
                id,
                iced::advanced::widget::operation::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(80.0),
                },
            ),
        );
        let mut messages = vec![];
        let (_, statuses) = ui.update(
            &[
                key_event(Named::F10, keyboard::Modifiers::empty()),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(Point::new(10.0, 50.0)),
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses.len(), 2);
        assert_eq!(
            messages,
            [9],
            "bar at content y=100, scroll=80 has popup at window y=48"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    fn runtime_touch_opens_bar_and_dismisses_outside_without_hover_leak() {
        let menu: Menu<'_, u8, iced::Theme, ()> = Menu::bar(vec![Item::submenu("file", items())]);
        let mut ui = iced_runtime::UserInterface::build(
            menu,
            Size::new(400.0, 300.0),
            iced_runtime::user_interface::Cache::new(),
            &mut (),
        );
        let mut messages = vec![];
        ui.update(
            &[Event::Touch(iced::touch::Event::FingerPressed {
                id: iced::touch::Finger(0),
                position: Point::new(5.0, 5.0),
            })],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        let (state, _) = ui.update(
            &[],
            mouse::Cursor::Available(Point::new(5.0, 35.0)),
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert!(
            matches!(
                state,
                iced_runtime::user_interface::State::Updated {
                    mouse_interaction: mouse::Interaction::Idle,
                    ..
                }
            ),
            "disabled row must block hover through to underlying widgets"
        );
        let (_, statuses) = ui.update(
            &[
                Event::Touch(iced::touch::Event::FingerPressed {
                    id: iced::touch::Finger(1),
                    position: Point::new(390.0, 290.0),
                }),
                character("x"),
            ],
            mouse::Cursor::Unavailable,
            &mut (),
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            statuses,
            [iced::event::Status::Captured, iced::event::Status::Ignored]
        );
        assert!(messages.is_empty());
    }

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
    fn newly_opened_menu_handles_rest_of_base_event_batch() {
        let mut harness = Harness::new(Menu::bar(vec![Item::submenu("menu", items())]));
        let cursor = mouse::Cursor::Unavailable;
        harness.event(
            key_event(Named::F10, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        // No overlay recreation between these base events, as in iced's batch.
        let (messages, captured, _) = harness.event(
            key_event(Named::Enter, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        assert_eq!(messages, [1]);
        assert!(captured);
        assert!(!harness.tree.state.downcast_ref::<State>().open);

        harness.event(
            key_event(Named::F10, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        harness.event(
            key_event(Named::ArrowDown, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        harness.event(
            key_event(Named::Enter, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        let (messages, captured, _) = harness.event(
            key_event(Named::Enter, keyboard::Modifiers::empty()),
            cursor,
            false,
        );
        assert_eq!(messages, [2]);
        assert!(captured);
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
