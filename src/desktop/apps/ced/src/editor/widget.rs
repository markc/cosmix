//! The editor widget (plan §4.2): a custom `iced::advanced::Widget` that
//! draws only the visible lines of a [`Text`] on a monospace cell grid —
//! grapheme clusters placed by `view::clusters`, ASCII runs in one text call,
//! every other cluster at its own cell — with selection, change tints,
//! squiggles, remote carets, the IME preedit, the gutter and overlay
//! scrollbars. It owns no buffer state: everything it changes goes out as an
//! [`EditorMsg`], and the controller applies it to the [`EditorModel`].
//!
//! Tree state holds only view mechanics: font metrics, the effective scroll
//! (ahead of the model by at most one message round trip), drag and click
//! state, the IME guard and per-text-version measurement caches.
//!
//! Every frame the widget reports its engine geometry
//! ([`EditorMsg::Layout`], feeding `ced.layout`) when it changed.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::highlight::Highlight;
use cosmix_edit_client::model::{EditCommand, EditorModel, Motion, Scroll, clamp_offset, line_of};
use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::text::Text;
use iced::advanced::layout::{self, Layout};
use iced::advanced::text::{self as atext, Paragraph};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, InputMethod, Shell, Widget, clipboard, input_method, mouse, renderer};
use iced::{Element, Event, Font, Length, Pixels, Point, Rectangle, Size, keyboard, window};

use super::layout::{self as geo, Geometry, Metrics};
use super::lines::{self, Checkpoints};
use super::{EditorMsg, EditorView, LayoutReport, Palette, draw, ime, input};

/// Change tints last this long (plan §4.5).
pub const TINT: Duration = Duration::from_secs(2);
/// Largest selection mirrored into the primary clipboard.
const PRIMARY_MAX: usize = 1024 * 1024;
/// Wheel notch = this many lines.
const WHEEL_LINES: f32 = 3.0;

pub struct EditorWidget;

impl EditorWidget {
    #[allow(clippy::new_ret_no_self)]
    pub fn new<'a>(
        text: &'a Text,
        model: &'a EditorModel,
        highlight: &'a Highlight,
        palette: &'a Palette,
        diagnostics: &'a Diagnostics,
    ) -> Element<'a, EditorMsg> {
        Self::with(text, model, highlight, palette, diagnostics, &EditorView::default())
    }

    pub fn with<'a>(
        text: &'a Text,
        model: &'a EditorModel,
        highlight: &'a Highlight,
        palette: &'a Palette,
        diagnostics: &'a Diagnostics,
        view: &EditorView,
    ) -> Element<'a, EditorMsg> {
        Element::new(Editor { text, model, highlight, palette, diagnostics, view: view.clone() })
    }
}

pub(super) struct Editor<'a> {
    pub(super) text: &'a Text,
    pub(super) model: &'a EditorModel,
    pub(super) highlight: &'a Highlight,
    pub(super) palette: &'a Palette,
    pub(super) diagnostics: &'a Diagnostics,
    pub(super) view: EditorView,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Selecting text from a click of this kind.
    Text,
    /// Selecting whole lines from the gutter, anchored at a line.
    Lines(usize),
    /// Dragging a scrollbar thumb, grabbed this far into it.
    VBar(f32),
    HBar(f32),
}

/// Measurement key: font, size and line-height bits.
type MetricsKey = (Font, u32, u32);

#[derive(Default)]
pub(super) struct State {
    metrics_key: Option<MetricsKey>,
    pub(super) metrics: Option<Metrics>,
    /// The effective scroll (drawn now; published to the model).
    pub(super) scroll: Scroll,
    /// Scrolls published and not yet echoed back by the model.
    echo: VecDeque<Scroll>,
    last_model_scroll: Option<Scroll>,
    pending_scroll: bool,
    seen_head: Option<usize>,
    seen_version: (u64, usize),
    caret_visible: bool,
    drag: Option<Drag>,
    drag_offset: Option<usize>,
    last_click: Option<mouse::Click>,
    mods: keyboard::Modifiers,
    wheel: (f32, f32),
    pub(super) ime: ime::Composition,
    was_focused: bool,
    primary_pending: bool,
    last_report: Option<LayoutReport>,
    pub(super) ck: RefCell<Checkpoints>,
    /// First time each marker rev was drawn (tints last [`TINT`] from then).
    pub(super) tint_seen: RefCell<HashMap<u64, Instant>>,
    /// Widest visible line (cells) at the last draw — the horizontal extent.
    pub(super) max_cells: Cell<usize>,
}

impl<'a> Editor<'a> {
    pub(super) fn line_count(&self) -> usize {
        self.text.line_count().max(1)
    }

    fn geometry(&self, st: &State, bounds: Rectangle) -> Option<Geometry> {
        Some(Geometry::new(bounds, st.metrics?, self.line_count(), self.view.line_numbers))
    }

    fn line_h(&self) -> f32 {
        self.view.px * self.view.line_height
    }

    /// Measure the cell width once per font / size change.
    fn ensure_metrics<R: atext::Renderer<Font = Font>>(&self, st: &mut State) {
        let key = (self.view.font, self.view.px.to_bits(), self.view.line_height.to_bits());
        if st.metrics_key == Some(key) {
            return;
        }
        let sample = "0123456789".repeat(4);
        let p = R::Paragraph::with_text(atext::Text {
            content: sample.as_str(),
            bounds: Size::INFINITE,
            size: Pixels(self.view.px),
            line_height: atext::LineHeight::Absolute(Pixels(self.line_h())),
            font: self.view.font,
            align_x: atext::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: atext::Shaping::Basic,
            wrapping: atext::Wrapping::None,
        });
        let width = p.min_bounds().width;
        let cell_w = if width > 0.0 { width / sample.len() as f32 } else { self.view.px * 0.6 };
        st.metrics_key = Some(key);
        st.metrics = Some(Metrics { cell_w, line_h: self.line_h() });
    }

    /// Adopt an external scroll, follow the caret after a motion or an edit
    /// at a visible caret, clamp.
    fn sync_scroll(&self, st: &mut State, size: Size) {
        let Some(metrics) = st.metrics else { return };
        let g = Geometry::new(Rectangle::with_size(size), metrics, self.line_count(), self.view.line_numbers);
        if st.last_model_scroll != Some(self.model.scroll) {
            if let Some(i) = st.echo.iter().position(|s| *s == self.model.scroll) {
                st.echo.drain(..=i);
            } else {
                st.scroll = self.model.scroll;
                st.echo.clear();
            }
            st.last_model_scroll = Some(self.model.scroll);
        }
        let head = clamp_offset(self.text, self.model.sel.head);
        let version = (self.text.commit_calls(), self.text.len());
        let moved = st.seen_head != Some(head);
        let edited = st.seen_version != version;
        if moved || edited {
            // Follow a motion always; follow an edit only when the caret was
            // on screen (an agent editing elsewhere must not yank the view).
            if (moved && !edited) || st.caret_visible || st.seen_head.is_none() {
                let (line, cells) = lines::cells_of(self.text, &self.view.measure, &mut st.ck.borrow_mut(), head);
                let next = geo::follow(st.scroll, line, cells, g.full_rows(), g.cols(), self.line_count());
                if next != st.scroll {
                    st.scroll = next;
                    st.pending_scroll = true;
                }
            }
            st.seen_head = Some(head);
            st.seen_version = version;
        }
        let clamped = geo::clamp_scroll(st.scroll, self.line_count());
        if clamped != st.scroll {
            st.scroll = clamped;
            st.pending_scroll = true;
        }
        let line = line_of(self.text, head);
        st.caret_visible = line >= st.scroll.first_line && line < st.scroll.first_line + g.full_rows();
    }

    fn set_scroll(&self, st: &mut State, scroll: Scroll, shell: &mut Shell<'_, EditorMsg>) {
        let scroll = geo::clamp_scroll(scroll, self.line_count());
        if scroll != st.scroll {
            st.scroll = scroll;
            st.echo.push_back(scroll);
            shell.publish(EditorMsg::Scrolled(scroll));
            shell.request_redraw();
        }
        st.pending_scroll = false;
    }

    /// The view offset under `p` (lines past the end → the text end).
    fn offset_at(&self, st: &State, g: &Geometry, p: Point) -> usize {
        let (line, cells) = g.hit(p, st.scroll);
        if line > self.text.line_count() {
            return self.text.len();
        }
        lines::offset_at(self.text, &self.view.measure, &mut st.ck.borrow_mut(), line, cells)
    }

    fn line_start(&self, line: usize) -> usize {
        self.text.line_start(line).unwrap_or(self.text.len())
    }

    /// The caret's rectangle (at the composition start while composing).
    pub(super) fn caret_rect(&self, st: &State, g: &Geometry) -> Rectangle {
        let at = self.model.composition.as_ref().map_or(self.model.sel.head, |c| c.start);
        let (line, cells) = lines::cells_of(self.text, &self.view.measure, &mut st.ck.borrow_mut(), clamp_offset(self.text, at));
        g.caret_rect(line, cells, st.scroll)
    }

    fn on_redraw(&self, st: &mut State, g: &Geometry, cursor: mouse::Cursor, now: Instant, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, EditorMsg>) {
        if st.pending_scroll {
            let s = st.scroll;
            st.echo.push_back(s);
            st.pending_scroll = false;
            shell.publish(EditorMsg::Scrolled(s));
        }

        // Focus loss or a remote delta over the composition cancels it.
        if st.was_focused && !self.view.focused && st.ime.active() {
            st.ime.cancel();
            shell.publish(EditorMsg::Preedit(String::new()));
        }
        st.was_focused = self.view.focused;
        if st.ime.active() {
            if self.model.composition.is_some() {
                st.ime.anchored = true;
            } else if st.ime.anchored {
                st.ime.cancel();
            }
        }
        let caret = self.caret_rect(st, g);
        if self.view.focused && st.ime.enabled() {
            shell.request_input_method(&InputMethod::Enabled {
                cursor: caret,
                purpose: input_method::Purpose::Normal,
                preedit: None::<input_method::Preedit<&str>>,
            });
        } else {
            shell.request_input_method(&InputMethod::<&str>::Disabled);
        }

        let report = LayoutReport {
            editor: geo::rect4(g.bounds),
            gutter_w: g.gutter_w,
            line_height: g.metrics.line_h,
            cell_w: g.metrics.cell_w,
            first_line: st.scroll.first_line,
            visible_rows: g.full_rows(),
            caret: geo::rect4(caret),
        };
        if st.last_report != Some(report) {
            st.last_report = Some(report);
            shell.publish(EditorMsg::Layout(report));
        }

        if std::mem::take(&mut st.primary_pending) {
            let (a, b) = (self.model.sel.anchor.min(self.model.sel.head), self.model.sel.anchor.max(self.model.sel.head));
            if a < b && b - a <= PRIMARY_MAX && b <= self.text.len() {
                let mut s = String::with_capacity(b - a);
                self.text.read(a..b, &mut s);
                clipboard.write(clipboard::Kind::Primary, s);
            }
        }

        // Drag auto-scroll: one step per frame while the pointer is outside.
        if let (Some(Drag::Text), Some(p)) = (st.drag, cursor.position()) {
            let t = g.text_rect();
            let step = |d: f32| 1 + (d / g.metrics.line_h / 2.0) as usize;
            let mut s = st.scroll;
            if p.y < t.y {
                s.first_line = s.first_line.saturating_sub(step(t.y - p.y)).max(1);
            } else if p.y > t.y + t.height {
                s.first_line += step(p.y - t.y - t.height);
            }
            if p.x < t.x {
                s.x_cells = s.x_cells.saturating_sub(1);
            } else if p.x > t.x + t.width {
                s.x_cells += 1;
            }
            if s != st.scroll {
                self.set_scroll(st, s, shell);
                self.drag_to(st, g, p, shell);
                shell.request_redraw();
            }
        }

        // Tints: one redraw when the oldest live one expires.
        let seen = st.tint_seen.borrow();
        if let Some(expiry) = seen.values().map(|t| *t + TINT).filter(|e| *e > now).min() {
            shell.request_redraw_at(expiry);
        }
    }

    fn drag_to(&self, st: &mut State, g: &Geometry, p: Point, shell: &mut Shell<'_, EditorMsg>) {
        match st.drag {
            Some(Drag::Text) => {
                let o = self.offset_at(st, g, p);
                if st.drag_offset != Some(o) {
                    st.drag_offset = Some(o);
                    shell.publish(EditorMsg::Command(EditCommand::Move { to: Motion::To(o), extend: true }));
                }
            }
            Some(Drag::Lines(anchor)) => {
                let (line, _) = g.hit(p, st.scroll);
                let line = line.min(self.line_count());
                let sel = if line >= anchor {
                    Selection { anchor: self.line_start(anchor), head: self.line_start(line + 1) }
                } else {
                    Selection { anchor: self.line_start(anchor + 1), head: self.line_start(line) }
                };
                if st.drag_offset != Some(sel.head) {
                    st.drag_offset = Some(sel.head);
                    shell.publish(EditorMsg::Command(EditCommand::SetSelection(sel)));
                }
            }
            Some(Drag::VBar(grab)) => {
                let track = g.vbar_track();
                let rows = g.full_rows();
                let first = geo::thumb_to_first(track.height, self.line_count() + rows - 1, rows, p.y - track.y - grab);
                self.set_scroll(st, Scroll { first_line: first + 1, ..st.scroll }, shell);
            }
            Some(Drag::HBar(grab)) => {
                let track = g.hbar_track();
                let cols = g.cols();
                let total = st.max_cells.get().max(st.scroll.x_cells + cols);
                let x = geo::thumb_to_first(track.width, total, cols, p.x - track.x - grab);
                self.set_scroll(st, Scroll { x_cells: x, ..st.scroll }, shell);
            }
            None => {}
        }
    }

    /// Press on a scrollbar: grab the thumb, or page towards the click.
    fn press_bar(&self, st: &mut State, g: &Geometry, p: Point, shell: &mut Shell<'_, EditorMsg>) -> bool {
        let rows = g.full_rows();
        let v = g.vbar_track();
        if v.contains(p)
            && let Some((off, len)) = geo::thumb(v.height, self.line_count() + rows - 1, rows, st.scroll.first_line - 1)
        {
            let top = v.y + off;
            if p.y >= top && p.y <= top + len {
                st.drag = Some(Drag::VBar(p.y - top));
            } else {
                let first = if p.y < top { st.scroll.first_line.saturating_sub(rows) } else { st.scroll.first_line + rows };
                self.set_scroll(st, Scroll { first_line: first.max(1), ..st.scroll }, shell);
            }
            return true;
        }
        let h = g.hbar_track();
        let cols = g.cols();
        let total = st.max_cells.get().max(st.scroll.x_cells + cols);
        if h.contains(p)
            && let Some((off, len)) = geo::thumb(h.width, total, cols, st.scroll.x_cells)
        {
            let left = h.x + off;
            if p.x >= left && p.x <= left + len {
                st.drag = Some(Drag::HBar(p.x - left));
            } else {
                let x = if p.x < left { st.scroll.x_cells.saturating_sub(cols) } else { st.scroll.x_cells + cols };
                self.set_scroll(st, Scroll { x_cells: x, ..st.scroll }, shell);
            }
            return true;
        }
        false
    }
}

impl<'a, Theme, R> Widget<EditorMsg, Theme, R> for Editor<'a>
where
    R: atext::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State { caret_visible: true, ..State::default() })
    }

    fn size(&self) -> Size<Length> {
        Size { width: Length::Fill, height: Length::Fill }
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &R, limits: &layout::Limits) -> layout::Node {
        let size = limits.width(Length::Fill).height(Length::Fill).max();
        let st = tree.state.downcast_mut::<State>();
        self.ensure_metrics::<R>(st);
        self.sync_scroll(st, size);
        layout::Node::new(size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, EditorMsg>,
        _viewport: &Rectangle,
    ) {
        let st = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let Some(g) = self.geometry(st, bounds) else { return };
        match event {
            Event::Window(window::Event::RedrawRequested(now)) => self.on_redraw(st, &g, cursor, *now, clipboard, shell),
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => st.mods = *m,
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. }) => {
                st.mods = *modifiers;
                if !self.view.focused || st.ime.active() || shell.is_event_captured() {
                    return;
                }
                if let Some(cmd) = input::command(key, *modifiers, text.as_deref(), g.full_rows()) {
                    shell.publish(EditorMsg::Command(cmd));
                    shell.capture_event();
                }
            }
            Event::InputMethod(ev) if self.view.focused => match ev {
                input_method::Event::Opened => st.ime.opened(),
                input_method::Event::Closed => {
                    let had = st.ime.active();
                    st.ime.closed();
                    if had {
                        shell.publish(EditorMsg::Preedit(String::new()));
                    }
                    shell.request_redraw();
                }
                input_method::Event::Preedit(s, _) => {
                    if st.ime.preedit(s) {
                        shell.publish(EditorMsg::Preedit(s.clone()));
                    }
                    shell.request_redraw();
                }
                input_method::Event::Commit(s) => {
                    if st.ime.commit() && !s.is_empty() {
                        shell.publish(EditorMsg::ImeCommit(s.clone()));
                    }
                    shell.capture_event();
                }
            },
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = cursor.position_over(bounds) else { return };
                shell.capture_event();
                if !self.view.focused {
                    shell.publish(EditorMsg::Focus(true));
                }
                if self.press_bar(st, &g, p, shell) {
                    return;
                }
                if p.x < g.text_rect().x {
                    let (line, _) = g.hit(p, st.scroll);
                    let line = line.min(self.line_count());
                    st.drag = Some(Drag::Lines(line));
                    st.drag_offset = None;
                    self.drag_to(st, &g, p, shell);
                    return;
                }
                let o = self.offset_at(st, &g, p);
                let click = mouse::Click::new(p, mouse::Button::Left, st.last_click);
                st.last_click = Some(click);
                let cmd = match click.kind() {
                    mouse::click::Kind::Single => EditCommand::Move { to: Motion::To(o), extend: st.mods.shift() },
                    mouse::click::Kind::Double => EditCommand::SelectWord(o),
                    mouse::click::Kind::Triple => EditCommand::SelectLine(o),
                };
                shell.publish(EditorMsg::Command(cmd));
                st.drag = matches!(click.kind(), mouse::click::Kind::Single).then_some(Drag::Text);
                st.drag_offset = Some(o);
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if st.drag.is_some() {
                    self.drag_to(st, &g, *position, shell);
                    let t = g.text_rect();
                    if matches!(st.drag, Some(Drag::Text)) && !t.contains(*position) {
                        shell.request_redraw();
                    }
                } else if cursor.is_over(g.gutter_rect()) {
                    // Hover tooltips on the origin strip.
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if matches!(st.drag.take(), Some(Drag::Text | Drag::Lines(_))) {
                    st.primary_pending = true;
                    shell.request_redraw();
                }
                st.drag_offset = None;
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)) => {
                let Some(p) = cursor.position_over(g.text_rect()) else { return };
                let o = self.offset_at(st, &g, p);
                shell.publish(EditorMsg::Command(EditCommand::Move { to: Motion::To(o), extend: false }));
                shell.publish(EditorMsg::Paste { primary: true });
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if !cursor.is_over(bounds) {
                    return;
                }
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (x * WHEEL_LINES, y * WHEEL_LINES),
                    mouse::ScrollDelta::Pixels { x, y } => (x / g.metrics.cell_w, y / g.metrics.line_h),
                };
                // Shift turns a vertical wheel horizontal.
                let (dx, dy) = if st.mods.shift() && dx == 0.0 { (dy, 0.0) } else { (dx, dy) };
                st.wheel.0 -= dx;
                st.wheel.1 -= dy;
                let (cx, cy) = (st.wheel.0.trunc(), st.wheel.1.trunc());
                st.wheel.0 -= cx;
                st.wheel.1 -= cy;
                let mut s = st.scroll;
                s.first_line = (s.first_line as i64 + cy as i64).max(1) as usize;
                s.x_cells = (s.x_cells as i64 + cx as i64).max(0) as usize;
                self.set_scroll(st, s, shell);
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let st = tree.state.downcast_ref::<State>();
        let Some(g) = self.geometry(st, layout.bounds()) else { return };
        draw::draw(self, st, &g, renderer, cursor);
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, _viewport: &Rectangle, _renderer: &R) -> mouse::Interaction {
        let st = tree.state.downcast_ref::<State>();
        let Some(g) = self.geometry(st, layout.bounds()) else { return mouse::Interaction::None };
        match st.drag {
            Some(Drag::Text) => return mouse::Interaction::Text,
            Some(Drag::VBar(_) | Drag::HBar(_)) => return mouse::Interaction::Grabbing,
            _ => {}
        }
        let Some(p) = cursor.position_over(g.bounds) else { return mouse::Interaction::None };
        if g.vbar_track().contains(p) || g.hbar_track().contains(p) || p.x < g.text_rect().x {
            mouse::Interaction::Idle
        } else {
            mouse::Interaction::Text
        }
    }
}
