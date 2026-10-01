//! Between-pane gestures and the destination-anchored transfer decision.
//! The source and destination are snapshots; only an explicit choice starts work.
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use cosmix_dopus_core::{DropAction, PaneId, sanitise_display_path};
use iced::advanced::image::{self as aimage, Renderer as _};
use iced::advanced::text::Renderer as _;
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector, keyboard};
use iced_tiny_skia::Renderer;

use super::Look;
use crate::app::Msg;
use crate::icons::{self, Icons};

pub type Shared = Arc<Mutex<State>>;

#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub root: PathBuf,
    /// The complete receiving list, to keep the chooser over its pane.
    pub bounds: Rectangle,
    pub highlight: Rectangle,
}

#[derive(Debug, Clone)]
pub struct Gesture {
    pub pane: PaneId,
    pub source_root: PathBuf,
    pub source: PathBuf,
    pub is_dir: bool,
    pub pointer: Point,
    pub target: Option<Target>,
}

#[derive(Debug, Default)]
pub struct State {
    pub active: Option<Gesture>,
    pub pending: Option<Gesture>,
    /// Lists observe cancellation even when the root captures the event.
    pub cancel_epoch: u64,
}

pub fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl State {
    pub fn cancel(&mut self) {
        self.active = None;
        self.pending = None;
        self.cancel_epoch = self.cancel_epoch.wrapping_add(1);
    }

    fn release(&mut self) {
        self.pending = self.active.take().filter(|drag| drag.target.is_some());
    }
}

/// A root wrapper, so the preview can cross the divider and draw over both lists.
pub struct Layer<'a> {
    content: Element<'a, Msg>,
    shared: Shared,
    look: Look,
    icons: &'a Icons,
    tint: &'a str,
}

impl<'a> Layer<'a> {
    pub fn new(
        content: Element<'a, Msg>,
        shared: Shared,
        look: Look,
        icons: &'a Icons,
        tint: &'a str,
    ) -> Self {
        Self {
            content,
            shared,
            look,
            icons,
            tint,
        }
    }
}

/// Clamp the card to the receiving list and the current window viewport.
fn card_bounds(drag: &Gesture, viewport: Rectangle, look: Look) -> Option<Rectangle> {
    let target = drag.target.as_ref()?.bounds.intersection(&viewport)?;
    let width = (look.px * 20.0 + 2.0 * look.chrome.pad).min(target.width);
    let height = (look.px * 1.5 * 4.0 + 5.0 * look.chrome.small).min(target.height);
    Some(Rectangle {
        x: drag
            .pointer
            .x
            .clamp(target.x, target.x + target.width - width),
        y: drag
            .pointer
            .y
            .clamp(target.y, target.y + target.height - height),
        width,
        height,
    })
}

fn choice_at(point: Point, bounds: Rectangle) -> Option<Option<DropAction>> {
    if !bounds.contains(point) || point.y < bounds.y + bounds.height / 4.0 {
        return None;
    }
    match (((point.y - bounds.y) / bounds.height) * 4.0) as usize {
        1 => Some(Some(DropAction::Move)),
        2 => Some(Some(DropAction::Copy)),
        _ => Some(None),
    }
}

impl Widget<Msg, iced::Theme, Renderer> for Layer<'_> {
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
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        {
            let mut state = lock(&self.shared);
            let engaged = state.active.is_some() || state.pending.is_some();
            if engaged
                && matches!(
                    event,
                    Event::Window(iced::window::Event::Unfocused | iced::window::Event::Resized(_))
                        | Event::Mouse(mouse::Event::CursorLeft)
                        | Event::Keyboard(keyboard::Event::KeyPressed {
                            key: keyboard::Key::Named(keyboard::key::Named::Escape),
                            ..
                        })
                )
            {
                state.cancel();
                shell.request_redraw();
                shell.capture_event();
                return;
            }
            if let Some(pending) = state.pending.as_ref() {
                if let Event::Mouse(mouse::Event::ButtonPressed(button)) = event {
                    let choice = if *button == mouse::Button::Left {
                        cursor
                            .position()
                            .and_then(|point| {
                                card_bounds(pending, *viewport, self.look)
                                    .and_then(|bounds| choice_at(point, bounds))
                            })
                            .flatten()
                    } else {
                        None
                    };
                    if let Some(action) = choice {
                        let pending = state.pending.take().expect("pending choice");
                        let target = pending.target.expect("validated drop target");
                        shell.publish(Msg::DropTransfer(
                            pending.pane,
                            pending.source,
                            target.path,
                            action,
                        ));
                    } else {
                        state.cancel();
                    }
                    shell.request_redraw();
                }
                // No underlying action, click or shortcut may mutate the snapshots.
                if matches!(event, Event::Mouse(_) | Event::Keyboard(_)) {
                    if matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. })) {
                        shell.request_redraw();
                    }
                    shell.capture_event();
                    return;
                }
            }
            if state.active.is_some() && matches!(event, Event::Keyboard(_)) {
                shell.capture_event();
                return;
            }
            if matches!(
                event,
                Event::Mouse(
                    mouse::Event::CursorMoved { .. }
                        | mouse::Event::ButtonReleased(mouse::Button::Left)
                )
            ) && let Some(active) = state.active.as_mut()
            {
                if let Some(position) = cursor.position() {
                    active.pointer = position;
                }
                active.target = None;
            }
        }
        // Lists resolve actual row and pane geometry before the release is finalised.
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        let mut state = lock(&self.shared);
        if state.active.is_some() {
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            ) {
                state.release();
            }
            if matches!(event, Event::Mouse(_)) {
                shell.capture_event();
                shell.request_redraw();
            }
        }
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
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        use iced::advanced::Renderer as _;
        let state = lock(&self.shared);
        let t = self.look.tokens;
        if let Some(active) = &state.active {
            let label = active
                .source
                .file_name()
                .map(|name| cosmix_dopus_core::sanitise_display_text(&name.to_string_lossy()))
                .unwrap_or_default();
            let icon = self.look.chrome.icon;
            let pad = self.look.chrome.pad;
            let bounds = Rectangle {
                x: active.pointer.x + icon,
                y: active.pointer.y + icon,
                width: (self.look.px * 20.0).min(viewport.width),
                height: self.look.px * 1.5 + 2.0 * pad,
            };
            renderer.with_layer(*viewport, |renderer| {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds,
                        border: iced::Border {
                            color: t.border,
                            width: self.look.chrome.edge,
                            radius: t.radius.into(),
                        },
                        ..Default::default()
                    },
                    t.popover,
                );
                if let Some(handle) = self.icons.get(
                    icons::file_icon(&active.source, active.is_dir, false),
                    self.tint,
                    icons::RASTER_PX,
                ) {
                    renderer.draw_image(
                        aimage::Image::new(handle),
                        Rectangle {
                            x: bounds.x + pad,
                            y: bounds.center_y() - icon / 2.0,
                            width: icon,
                            height: icon,
                        },
                        *viewport,
                    );
                }
                draw_text(
                    renderer,
                    &label,
                    Point::new(
                        bounds.x + pad + icon + self.look.chrome.small,
                        bounds.y + pad,
                    ),
                    Rectangle {
                        x: bounds.x + pad + icon + self.look.chrome.small,
                        width: (bounds.width - 2.0 * pad - icon - self.look.chrome.small).max(0.0),
                        ..bounds
                    },
                    self.look,
                );
            });
        }
        if let Some(pending) = &state.pending
            && let Some(bounds) = card_bounds(pending, *viewport, self.look)
        {
            renderer.with_layer(bounds, |renderer| {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds,
                        border: iced::Border {
                            color: t.border,
                            width: self.look.chrome.edge,
                            radius: t.radius.into(),
                        },
                        ..Default::default()
                    },
                    t.popover,
                );
                let title = pending
                    .target
                    .as_ref()
                    .map(|target| sanitise_display_path(&target.path))
                    .unwrap_or_default();
                for (index, label) in [title.as_str(), "Move here", "Copy here", "Cancel"]
                    .iter()
                    .enumerate()
                {
                    let row = Rectangle {
                        y: bounds.y + bounds.height * index as f32 / 4.0,
                        height: bounds.height / 4.0,
                        ..bounds
                    };
                    if index > 0 && cursor.is_over(row) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: row,
                                ..Default::default()
                            },
                            t.muted_surface,
                        );
                    }
                    draw_text(
                        renderer,
                        label,
                        Point::new(row.x + self.look.chrome.pad, row.y + self.look.chrome.small),
                        row,
                        self.look,
                    );
                }
            });
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
        let state = lock(&self.shared);
        if state.active.is_some() {
            mouse::Interaction::Grabbing
        } else if let Some(pending) = &state.pending {
            if cursor.position().is_some_and(|point| {
                card_bounds(pending, *viewport, self.look)
                    .and_then(|bounds| choice_at(point, bounds))
                    .is_some()
            }) {
                mouse::Interaction::Pointer
            } else {
                mouse::Interaction::Idle
            }
        } else {
            self.content
                .as_widget()
                .mouse_interaction(tree, layout, cursor, viewport, renderer)
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Msg, iced::Theme, Renderer>> {
        let engaged = {
            let state = lock(&self.shared);
            state.active.is_some() || state.pending.is_some()
        };
        if engaged {
            None
        } else {
            self.content
                .as_widget_mut()
                .overlay(tree, layout, renderer, viewport, translation)
        }
    }
}

fn draw_text(renderer: &mut Renderer, content: &str, position: Point, clip: Rectangle, look: Look) {
    renderer.fill_text(
        iced::advanced::text::Text {
            content: content.to_owned(),
            bounds: clip.size(),
            size: iced::Pixels(look.px),
            line_height: iced::advanced::text::LineHeight::Absolute(iced::Pixels(look.px * 1.4)),
            font: look.ui_font,
            align_x: iced::advanced::text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: iced::advanced::text::Shaping::Advanced,
            wrapping: iced::advanced::text::Wrapping::None,
        },
        position,
        look.tokens.popover_text,
        clip,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn look() -> Look {
        let theme = crate::theme::resolve_selection(
            &crate::theme::Selection {
                scheme: Default::default(),
                mode: Default::default(),
                design_source: None,
            },
            Vec::new(),
        );
        Look {
            sidebar_px: theme.sidebar_px,
            small_px: theme.small_px,
            tokens: theme.tokens,
            chrome: theme.chrome,
            ui_font: theme.ui_font,
            mono_font: theme.mono_font,
            px: theme.ui_px(),
            mono_px: theme.mono.1,
        }
    }

    fn send(
        layer: &mut Layer<'_>,
        tree: &mut Tree,
        renderer: &Renderer,
        event: Event,
        point: Point,
    ) -> Vec<Msg> {
        let bounds = Rectangle::with_size(Size::new(600.0, 300.0));
        let node = layer.layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let mut messages = Vec::new();
        layer.update(
            tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages
    }

    fn single_list<'a>(
        rows: &'a [cosmix_dopus_core::VisibleRow],
        root: &'a std::path::Path,
        icons: &'a Icons,
        expanded: &'a std::collections::HashSet<PathBuf>,
        shared: Shared,
        look: Look,
    ) -> Layer<'a> {
        let columns = crate::view::rows::Columns {
            name_min: 50.0,
            size: 30.0,
            modified: 40.0,
            gap: 4.0,
            pad: 4.0,
        };
        let content = Element::new(crate::view::rows::FileList::new(
            rows,
            None,
            root,
            expanded,
            icons,
            "",
            look,
            &[],
            columns,
            PaneId::Left,
            shared.clone(),
            false,
        ))
        .map(|msg| Msg::PaneRows(PaneId::Left, msg));
        Layer::new(content, shared, look, icons, "")
    }

    #[test]
    fn armed_press_cancels_if_async_listing_replaces_the_row_index() {
        let dir = tempfile::tempdir().unwrap();
        let entry = |name: &str| cosmix_dopus_core::VisibleRow {
            entry: cosmix_dopus_core::FileEntry {
                path: dir.path().join(name),
                name: name.into(),
                is_dir: false,
                size: Some(1),
                modified: None,
                child_count: None,
            },
            depth: 0,
        };
        let original = vec![entry("pressed"), entry("replacement")];
        let reordered = vec![entry("replacement"), entry("pressed")];
        let look = look();
        let icons = Icons::new();
        let expanded = Default::default();
        let shared: Shared = Default::default();
        let renderer = Renderer::new(look.ui_font, iced::Pixels(look.px));
        let mut layer = single_list(
            &original,
            dir.path(),
            &icons,
            &expanded,
            shared.clone(),
            look,
        );
        let mut tree = Tree::new(&layer as &dyn Widget<Msg, iced::Theme, Renderer>);
        send(
            &mut layer,
            &mut tree,
            &renderer,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Point::new(100.0, 10.0),
        );
        // The app rebuilt the widget after an async relist/sort, retaining its Tree.
        let mut replacement = single_list(
            &reordered,
            dir.path(),
            &icons,
            &expanded,
            shared.clone(),
            look,
        );
        replacement.diff(&mut tree);
        let moved = Point::new(120.0, 10.0);
        assert!(
            send(
                &mut replacement,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::CursorMoved { position: moved }),
                moved
            )
            .is_empty()
        );
        assert!(
            lock(&shared).active.is_none(),
            "replacement row cannot become the drag source"
        );
        assert!(
            send(
                &mut replacement,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                moved
            )
            .is_empty()
        );
    }

    #[test]
    fn leaving_before_threshold_or_root_cancellation_cannot_start_a_phantom_drag() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![cosmix_dopus_core::VisibleRow {
            entry: cosmix_dopus_core::FileEntry {
                path: dir.path().join("pressed"),
                name: "pressed".into(),
                is_dir: false,
                size: Some(1),
                modified: None,
                child_count: None,
            },
            depth: 0,
        }];
        let look = look();
        let icons = Icons::new();
        let expanded = Default::default();
        let shared: Shared = Default::default();
        let renderer = Renderer::new(look.ui_font, iced::Pixels(look.px));
        let mut layer = single_list(&rows, dir.path(), &icons, &expanded, shared.clone(), look);
        let mut tree = Tree::new(&layer as &dyn Widget<Msg, iced::Theme, Renderer>);
        for cancel_at_root in [false, true] {
            send(
                &mut layer,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Point::new(100.0, 10.0),
            );
            if cancel_at_root {
                lock(&shared).cancel();
            } else {
                send(
                    &mut layer,
                    &mut tree,
                    &renderer,
                    Event::Mouse(mouse::Event::CursorLeft),
                    Point::new(100.0, 10.0),
                );
            }
            // Release happened outside the window and was never delivered. The
            // pointer returns with no button held, beyond the old threshold.
            let reenter = Point::new(140.0, 10.0);
            assert!(
                send(
                    &mut layer,
                    &mut tree,
                    &renderer,
                    Event::Mouse(mouse::Event::CursorMoved { position: reenter }),
                    reenter
                )
                .is_empty()
            );
            assert!(lock(&shared).active.is_none());
        }
    }

    #[test]
    fn actual_list_events_preserve_clicks_and_drag_in_both_directions() {
        use crate::view::rows::{Columns, FileList, RowsMsg};
        use cosmix_dopus_core::{FileEntry, VisibleRow};
        let dir = tempfile::tempdir().unwrap();
        let roots = [dir.path().join("left"), dir.path().join("right")];
        for root in &roots {
            std::fs::create_dir(root).unwrap();
            std::fs::write(root.join("item"), b"contents").unwrap();
        }
        for source_pane in [PaneId::Left, PaneId::Right] {
            let look = look();
            let shared: Shared = Default::default();
            let icons = Icons::new();
            let rows: [Vec<VisibleRow>; 2] = std::array::from_fn(|index| {
                vec![VisibleRow {
                    entry: FileEntry {
                        path: roots[index].join("item"),
                        name: "item".into(),
                        is_dir: false,
                        size: Some(8),
                        modified: None,
                        child_count: None,
                    },
                    depth: 0,
                }]
            });
            let columns = Columns {
                name_min: 50.0,
                size: 30.0,
                modified: 40.0,
                gap: 4.0,
                pad: 4.0,
            };
            let expanded = std::collections::HashSet::new();
            let list = |pane: PaneId| -> Element<'_, Msg> {
                Element::new(FileList::new(
                    &rows[pane.index()],
                    None,
                    &roots[pane.index()],
                    &expanded,
                    &icons,
                    "",
                    look,
                    &[],
                    columns,
                    pane,
                    shared.clone(),
                    false,
                ))
                .map(move |msg| Msg::PaneRows(pane, msg))
            };
            let content = iced::widget::row![list(PaneId::Left), list(PaneId::Right)]
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
            let mut layer = Layer::new(content, shared.clone(), look, &icons, "");
            let mut tree = Tree::new(&layer as &dyn Widget<Msg, iced::Theme, Renderer>);
            let renderer = Renderer::new(look.ui_font, iced::Pixels(look.px));
            let start = Point::new(
                if source_pane == PaneId::Left {
                    100.0
                } else {
                    400.0
                },
                10.0,
            );
            let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
            let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
            send(&mut layer, &mut tree, &renderer, press.clone(), start);
            let near = Point::new(start.x + 2.0, start.y);
            send(
                &mut layer,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::CursorMoved { position: near }),
                near,
            );
            let clicked = send(&mut layer, &mut tree, &renderer, release.clone(), near);
            assert!(clicked.iter().any(|message| matches!(message, Msg::PaneRows(pane, RowsMsg::Select(path)) if *pane == source_pane && *path == roots[source_pane.index()].join("item"))));
            assert!(lock(&shared).active.is_none());
            send(&mut layer, &mut tree, &renderer, press.clone(), start);
            let threshold = Point::new(start.x + 10.0, start.y);
            send(
                &mut layer,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::CursorMoved {
                    position: threshold,
                }),
                threshold,
            );
            assert!(lock(&shared).active.is_some());
            let destination = Point::new(
                if source_pane == PaneId::Left {
                    500.0
                } else {
                    200.0
                },
                80.0,
            );
            send(
                &mut layer,
                &mut tree,
                &renderer,
                Event::Mouse(mouse::Event::CursorMoved {
                    position: destination,
                }),
                destination,
            );
            send(&mut layer, &mut tree, &renderer, release, destination);
            assert!(!roots[source_pane.other().index()].join("copied").exists());
            let card = card_bounds(
                lock(&shared)
                    .pending
                    .as_ref()
                    .expect("opposite pane accepts drop"),
                Rectangle::with_size(Size::new(600.0, 300.0)),
                look,
            )
            .unwrap();
            let choose_copy = Point::new(card.center_x(), card.y + card.height * 0.625);
            let messages = send(&mut layer, &mut tree, &renderer, press, choose_copy);
            assert!(
                matches!(messages.as_slice(), [Msg::DropTransfer(pane, source, target, DropAction::Copy)]
                if *pane == source_pane && *source == roots[source_pane.index()].join("item") && *target == roots[source_pane.other().index()])
            );
            assert!(lock(&shared).pending.is_none());
        }
    }

    #[test]
    fn chooser_stays_over_target_even_in_narrow_viewport() {
        let viewport = Rectangle {
            x: 30.0,
            y: 40.0,
            width: 180.0,
            height: 140.0,
        };
        let target = Rectangle {
            x: 140.0,
            y: 50.0,
            width: 70.0,
            height: 120.0,
        };
        let gesture = Gesture {
            pane: PaneId::Left,
            source_root: "/source".into(),
            source: "/source/file".into(),
            is_dir: false,
            pointer: Point::new(205.0, 165.0),
            target: Some(Target {
                path: "/target".into(),
                root: "/target".into(),
                bounds: target,
                highlight: target,
            }),
        };
        let card = card_bounds(&gesture, viewport, look()).unwrap();
        assert_eq!(card.intersection(&target), Some(card));
        assert_eq!(card.intersection(&viewport), Some(card));
    }

    #[test]
    fn selected_directory_draws_drop_outline_above_its_selection() {
        use crate::view::rows::{Columns, FileList};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("folder");
        let rows = vec![cosmix_dopus_core::VisibleRow {
            entry: cosmix_dopus_core::FileEntry {
                path: target.clone(),
                name: "folder".into(),
                is_dir: true,
                size: None,
                modified: None,
                child_count: None,
            },
            depth: 0,
        }];
        let look = look();
        let icons = Icons::new();
        let expanded = Default::default();
        let shared: Shared = Default::default();
        let columns = Columns {
            name_min: 50.0,
            size: 30.0,
            modified: 40.0,
            gap: 4.0,
            pad: 4.0,
        };
        let mut list = FileList::new(
            &rows,
            Some(&target),
            dir.path(),
            &expanded,
            &icons,
            "",
            look,
            &[],
            columns,
            PaneId::Left,
            shared.clone(),
            false,
        );
        let mut tree =
            Tree::new(&list as &dyn Widget<crate::view::rows::RowsMsg, iced::Theme, Renderer>);
        let mut renderer = Renderer::new(look.ui_font, iced::Pixels(look.px));
        let viewport = Rectangle::with_size(Size::new(600.0, 300.0));
        let node = list.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut messages = Vec::new();
        list.update(
            &mut tree,
            &Event::Window(iced::window::Event::Focused),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        lock(&shared).active = Some(Gesture {
            pane: PaneId::Right,
            source_root: "/source".into(),
            source: "/source/item".into(),
            is_dir: false,
            pointer: Point::new(100.0, 10.0),
            target: Some(Target {
                path: target,
                root: dir.path().to_path_buf(),
                bounds: viewport,
                highlight: Rectangle {
                    height: 40.0,
                    ..viewport
                },
            }),
        });
        list.draw(
            &tree,
            &mut renderer,
            &iced::Theme::default(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        let quads: Vec<_> = renderer
            .layers()
            .iter()
            .flat_map(|layer| layer.quads.iter())
            .collect();
        let selection = quads
            .iter()
            .position(|(_, background)| {
                *background == iced::Background::Color(look.tokens.selection)
            })
            .expect("selected row background");
        let outline = quads
            .iter()
            .position(|(quad, _)| quad.border.width > 0.0 && quad.border.color == look.tokens.ring)
            .expect("drop target outline");
        assert!(
            outline > selection,
            "opaque selection cannot cover the drop outline"
        );
        assert_eq!(
            quads[outline].1,
            iced::Background::Color(iced::Color::TRANSPARENT)
        );
    }
    #[test]
    fn dropping_without_a_target_cancels_and_valid_drop_pins_both_paths() {
        let gesture = Gesture {
            pane: PaneId::Left,
            source_root: "/source".into(),
            source: "/source/file".into(),
            is_dir: false,
            pointer: Point::new(60.0, 60.0),
            target: None,
        };
        let mut state = State {
            active: Some(gesture.clone()),
            pending: None,
            ..Default::default()
        };
        state.release();
        assert!(state.pending.is_none());
        let mut gesture = gesture;
        gesture.target = Some(Target {
            path: "/target".into(),
            root: "/target".into(),
            bounds: Rectangle::with_size(Size::new(200.0, 200.0)),
            highlight: Rectangle::default(),
        });
        state.active = Some(gesture);
        state.release();
        assert!(state.active.is_none());
        let pending = state.pending.as_ref().unwrap();
        assert_eq!(pending.source, PathBuf::from("/source/file"));
        assert_eq!(
            pending.target.as_ref().unwrap().path,
            PathBuf::from("/target")
        );
        state.cancel();
        assert!(state.pending.is_none());
    }
    #[test]
    fn chooser_distinguishes_move_copy_cancel_and_outside() {
        let bounds = Rectangle {
            x: 300.0,
            y: 100.0,
            width: 200.0,
            height: 160.0,
        };
        assert_eq!(
            choice_at(Point::new(320.0, 160.0), bounds),
            Some(Some(DropAction::Move))
        );
        assert_eq!(
            choice_at(Point::new(320.0, 200.0), bounds),
            Some(Some(DropAction::Copy))
        );
        assert_eq!(choice_at(Point::new(320.0, 240.0), bounds), Some(None));
        assert_eq!(choice_at(Point::new(200.0, 160.0), bounds), None);
    }

    #[test]
    fn pending_widget_cancels_on_focus_loss_escape_and_outside_press() {
        let look = look();
        let icons = Icons::new();
        let renderer = Renderer::new(look.ui_font, iced::Pixels(look.px));
        let target = Rectangle {
            x: 300.0,
            y: 0.0,
            width: 300.0,
            height: 300.0,
        };
        let events = [
            Event::Window(iced::window::Event::Unfocused),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
        ];
        for event in events {
            let shared: Shared = Default::default();
            lock(&shared).pending = Some(Gesture {
                pane: PaneId::Left,
                source_root: "/source".into(),
                source: "/source/file".into(),
                is_dir: false,
                pointer: Point::new(400.0, 80.0),
                target: Some(Target {
                    path: "/target".into(),
                    root: "/target".into(),
                    bounds: target,
                    highlight: target,
                }),
            });
            let mut layer = Layer::new(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
                shared.clone(),
                look,
                &icons,
                "",
            );
            let mut tree = Tree::new(&layer as &dyn Widget<Msg, iced::Theme, Renderer>);
            assert!(
                send(
                    &mut layer,
                    &mut tree,
                    &renderer,
                    event,
                    Point::new(10.0, 10.0)
                )
                .is_empty()
            );
            assert!(lock(&shared).pending.is_none());
        }
    }
}
