//! The iced demo: the raw grid under iced chrome, one buffer, one commit.
//! This file is the event-loop glue: routing, redraw scheduling, cursor.

use super::chrome::{self, Chrome};
use super::clipboard::{Fetch, Shared};
use super::damage::{Commit, merge};
use super::ime;
use super::keys::{self, Route};
use super::popups::{Bar, MenuPopups};
use crate::menus::{Entry, Metrics, Outcome};
use crate::raw::{IME_GRID, RawDemo};
use crate::startup::Clock;
use cosmix_iced_host::core::event::Status;
use cosmix_iced_host::core::mouse::Interaction;
use cosmix_iced_host::core::widget::operation::focusable;
use cosmix_iced_host::core::{Point, Size};
use cosmix_iced_host::{PixelFormat, Redraw, Settings, Surface, Update, input};
use cosmix_wl_app::{
    App, BTN_LEFT, ButtonState, Ctx, CursorShape, Event, Frame, PointerKind, Selection, SurfaceId,
    SurfaceInfo,
};

/// Timer token for the chrome's next redraw deadline (caret blink).
pub const TIMER_CHROME: u64 = 1;
/// The search field's input-method target.
pub const IME_FIELD: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ClearTab,
    Quit,
    Undo,
    Copy,
    Paste,
    SelectTab(usize),
    FocusSearch,
}

pub fn menu_bar() -> Bar<Action> {
    vec![
        (
            "File".into(),
            vec![
                Entry::action("Clear tab", Action::ClearTab).accelerator("Ctrl+Shift+L"),
                Entry::Separator,
                Entry::action("Quit", Action::Quit).accelerator("Ctrl+Shift+Q"),
            ],
        ),
        (
            "Edit".into(),
            vec![
                Entry::action("Undo", Action::Undo)
                    .accelerator("Ctrl+Z")
                    .disabled(),
                Entry::action("Copy", Action::Copy).accelerator("Ctrl+Shift+C"),
                Entry::action("Paste", Action::Paste).accelerator("Ctrl+Shift+V"),
            ],
        ),
        (
            "View".into(),
            vec![
                Entry::submenu(
                    "Tabs",
                    (0..3)
                        .map(|i| Entry::action(&format!("Tab {}", i + 1), Action::SelectTab(i)))
                        .collect(),
                ),
                Entry::action("Focus search", Action::FocusSearch).accelerator("Ctrl+F"),
            ],
        ),
    ]
}

fn cursor_shape(interaction: Interaction) -> CursorShape {
    match interaction {
        Interaction::Pointer => CursorShape::Pointer,
        Interaction::Text => CursorShape::Text,
        Interaction::NotAllowed => CursorShape::NotAllowed,
        Interaction::Grab => CursorShape::Grab,
        Interaction::Grabbing => CursorShape::Grabbing,
        Interaction::Crosshair => CursorShape::Crosshair,
        Interaction::Wait => CursorShape::Wait,
        Interaction::Progress => CursorShape::Progress,
        Interaction::Help => CursorShape::Help,
        Interaction::Move => CursorShape::Move,
        Interaction::ResizingHorizontally => CursorShape::EwResize,
        Interaction::ResizingVertically => CursorShape::NsResize,
        _ => CursorShape::Default,
    }
}

pub struct IcedDemo {
    raw: RawDemo,
    chrome: Surface<Chrome>,
    menus: MenuPopups<Action>,
    clipboard: Shared,
    metrics: Metrics,
    band: u32,
    pointer: Option<(f64, f64)>,
    field_ime: bool,
    tab_texts: Vec<String>,
    shown_tab: usize,
    cursor: Option<CursorShape>,
}

impl IcedDemo {
    pub fn new(clock: Clock) -> Self {
        let metrics = Metrics::default();
        let band = chrome::band_height(&metrics);
        let bar = menu_bar();
        let chrome_program = Chrome::new(bar.iter().map(|(t, _)| t.clone()).collect(), metrics);
        let style = chrome_program.style;
        let clipboard = Shared::default();
        let mut chrome = Surface::new(chrome_program, Settings::default());
        chrome.set_clipboard(Box::new(clipboard.clone()));
        let raw = RawDemo::embedded(clock, "wl-iced-demo", band);
        let first = raw.text.clone();
        Self {
            raw,
            chrome,
            menus: MenuPopups::new(bar, metrics, style),
            clipboard,
            metrics,
            band,
            pointer: None,
            field_ime: false,
            tab_texts: vec![first, String::new(), String::new()],
            shown_tab: 0,
            cursor: None,
        }
    }

    fn window_info(&self) -> Option<(SurfaceId, SurfaceInfo)> {
        self.raw.window().zip(self.raw.info())
    }

    fn resize_chrome(&mut self, info: &SurfaceInfo) {
        let band = info.scale.to_physical(self.band).min(info.physical.1);
        self.chrome.resize(
            Size::new(info.physical.0, band.max(1)),
            info.scale_factor() as f32,
        );
        self.chrome.invalidate();
    }

    fn schedule(&self, cx: &mut Ctx<'_>, redraw: Redraw) {
        match redraw {
            Redraw::NextFrame => {
                cx.cancel_timer(TIMER_CHROME);
                if let Some(w) = self.raw.window() {
                    cx.request_redraw(w);
                }
            }
            Redraw::At(at) => cx.set_timer(TIMER_CHROME, at),
            Redraw::Wait => cx.cancel_timer(TIMER_CHROME),
        }
    }

    fn set_cursor(&mut self, cx: &mut Ctx<'_>, shape: CursorShape) {
        if self.cursor != Some(shape) {
            self.cursor = Some(shape);
            cx.set_cursor(shape);
        }
    }

    fn in_band(&self, y: f64) -> bool {
        y < f64::from(self.band)
    }

    /// Run queued chrome events and act on the result.
    fn process(&mut self, cx: &mut Ctx<'_>) -> Update {
        let update = self.chrome.process();
        if update.needs_redraw {
            self.schedule(cx, Redraw::NextFrame);
        } else {
            self.schedule(cx, update.redraw);
        }
        if self.pointer.is_some_and(|(_, y)| self.in_band(y)) {
            self.set_cursor(cx, cursor_shape(update.interaction));
        }
        self.apply_tab(cx);
        let writes = std::mem::take(&mut self.clipboard.0.borrow_mut().writes);
        for (selection, text) in writes {
            cx.set_selection(selection, text);
        }
        update
    }

    fn apply_tab(&mut self, cx: &mut Ctx<'_>) {
        let active = self.chrome.program().active;
        if active != self.shown_tab && active < self.tab_texts.len() {
            self.tab_texts[self.shown_tab] = std::mem::take(&mut self.raw.text);
            self.raw.text = std::mem::take(&mut self.tab_texts[active]);
            self.shown_tab = active;
            self.raw.log(format_args!("tab {active}"));
            self.raw.invalidate(cx);
        }
    }

    fn after_menu_change(&mut self, cx: &mut Ctx<'_>) {
        if let Some((window, info)) = self.window_info() {
            self.menus.reconcile(cx, window, &info);
        }
        let root = self.menus.nav.root();
        if self.chrome.program().open_root != root {
            self.chrome.program_mut().open_root = root;
            self.schedule(cx, Redraw::NextFrame);
        }
    }

    fn outcome(&mut self, cx: &mut Ctx<'_>, outcome: Outcome<Action>) {
        if let Outcome::Activated(action) = outcome {
            self.raw.log(format_args!("menu action {action:?}"));
            self.run(cx, action);
        }
        self.after_menu_change(cx);
    }

    fn run(&mut self, cx: &mut Ctx<'_>, action: Action) {
        match action {
            Action::ClearTab => {
                self.raw.text.clear();
                self.raw.invalidate(cx);
            }
            Action::Quit => cx.exit(),
            Action::Undo => {}
            Action::Copy => self.copy(cx),
            Action::Paste => cx.request_selection(Selection::Clipboard),
            Action::SelectTab(i) => {
                self.chrome.program_mut().active = i;
                self.apply_tab(cx);
                self.schedule(cx, Redraw::NextFrame);
            }
            Action::FocusSearch => {
                self.chrome
                    .operate(Box::new(focusable::focus(Chrome::search_id())));
                self.schedule(cx, Redraw::NextFrame);
            }
        }
    }

    fn copy(&mut self, cx: &mut Ctx<'_>) {
        let text = self.raw.text.clone();
        for selection in [Selection::Clipboard, Selection::Primary] {
            cx.set_selection(selection, text.clone());
            self.clipboard
                .0
                .borrow_mut()
                .set(selection, Some(text.clone()));
        }
    }

    fn key(&mut self, cx: &mut Ctx<'_>, key: cosmix_wl_app::KeyEvent) {
        match keys::route(&key, self.menus.nav.is_open()) {
            Route::Menu(nav) => {
                let outcome = self.menus.nav.key(&self.menus.bar, nav);
                self.outcome(cx, outcome);
            }
            Route::Swallow => {}
            Route::OpenBar => {
                self.menus.nav.open(&self.menus.bar, 0, true);
                self.after_menu_change(cx);
            }
            Route::Shortcut(action) => self.run(cx, action),
            Route::Chrome { then_grid } => {
                self.chrome.queue_event(keys::key_event(&key));
                let update = self.process(cx);
                if then_grid && update.statuses.first() != Some(&Status::Captured) {
                    self.raw.event(cx, Event::Key(key));
                }
            }
        }
    }

    fn window_pointer(&mut self, cx: &mut Ctx<'_>, p: cosmix_wl_app::PointerEvent) {
        let (x, y) = p.position;
        let point = Point::new(x as f32, y as f32);
        let open = self.menus.nav.is_open();
        match p.kind {
            PointerKind::Enter | PointerKind::Motion => {
                self.pointer = Some((x, y));
                self.chrome.cursor_moved(point);
                if !self.in_band(y) {
                    self.set_cursor(cx, CursorShape::Text);
                }
                if open
                    && let Some(root) = self.metrics.bar_hit(&self.menus.bar, x, y)
                    && self.menus.nav.hover_root(&self.menus.bar, root)
                {
                    self.after_menu_change(cx);
                }
            }
            PointerKind::Leave => {
                self.pointer = None;
                self.chrome.cursor_left();
            }
            PointerKind::Button {
                button,
                state: ButtonState::Pressed,
            } => {
                if button == BTN_LEFT
                    && let Some(root) = self.metrics.bar_hit(&self.menus.bar, x, y)
                {
                    if self.menus.nav.root() == Some(root) {
                        self.menus.close_all(cx);
                    } else {
                        self.menus.nav.open(&self.menus.bar, root, false);
                    }
                    self.after_menu_change(cx);
                    return;
                }
                if open {
                    self.menus.close_all(cx);
                    self.after_menu_change(cx);
                }
                self.chrome.queue_event(input::button_event(button, true));
            }
            PointerKind::Button { button, .. } => {
                self.chrome.queue_event(input::button_event(button, false));
            }
            PointerKind::Axis {
                horizontal,
                vertical,
                horizontal_120,
                vertical_120,
                ..
            } => {
                self.chrome
                    .queue_event(if horizontal_120 != 0 || vertical_120 != 0 {
                        input::wheel_value120(horizontal_120, vertical_120)
                    } else {
                        input::wheel_pixels(horizontal, vertical)
                    });
            }
        }
        let update = self.process(cx);
        let captured = update.statuses.contains(&Status::Captured);
        if !captured && !self.in_band(y) {
            self.raw.event(cx, Event::Pointer(p));
        }
    }

    fn popup_pointer(&mut self, cx: &mut Ctx<'_>, level: usize, p: cosmix_wl_app::PointerEvent) {
        let Some(entries) = self.menus.entries(level) else {
            return;
        };
        let row = self.metrics.row_hit(entries, p.position.0, p.position.1);
        match p.kind {
            PointerKind::Enter | PointerKind::Motion => {
                self.set_cursor(cx, CursorShape::Default);
                if self.menus.nav.hover(&self.menus.bar, level, row) {
                    self.after_menu_change(cx);
                }
            }
            PointerKind::Button {
                button: BTN_LEFT,
                state: ButtonState::Pressed,
            } => {
                if let Some(row) = row {
                    let outcome = self.menus.nav.click(&self.menus.bar, level, row);
                    self.outcome(cx, outcome);
                }
            }
            _ => {}
        }
    }

    fn update_ime(&mut self, cx: &mut Ctx<'_>) {
        let Some(window) = self.raw.window() else {
            return;
        };
        match ime::to_wl(&self.chrome.requests().ime, window, (0, 0)) {
            Some(mut state) => {
                state.target = IME_FIELD;
                self.field_ime = true;
                self.raw.set_ime_blocked(cx, true);
                cx.set_ime(Some(state));
            }
            None => {
                if self.field_ime {
                    self.field_ime = false;
                    cx.set_ime(None);
                }
                self.raw.set_ime_blocked(cx, false);
            }
        }
    }

    fn draw_window(&mut self, cx: &mut Ctx<'_>, frame: &mut Frame<'_>) {
        if frame.needs_full_redraw() {
            self.chrome.invalidate();
        }
        let Some((grid_full, grid)) = self.raw.paint_window(frame) else {
            return;
        };
        if grid_full {
            self.chrome.invalidate();
        }
        let info = frame.info();
        let (pixels, width, height, stride) = frame.buffer_mut();
        let band = info.scale.to_physical(self.band).clamp(1, height);
        let band_bytes = (stride * band) as usize;
        let drawn = match self.chrome.draw(
            &mut pixels[..band_bytes],
            width,
            band,
            stride,
            PixelFormat::Argb8888,
        ) {
            Ok(drawn) => drawn,
            Err(e) => {
                eprintln!("wl-iced-demo: chrome draw: {e}");
                return;
            }
        };
        let chrome_rects = drawn.damage.clone();
        match merge((width, height), grid_full, &grid, &chrome_rects) {
            Commit::Nothing => {}
            Commit::Full => frame.commit_full(),
            Commit::Rects(rects) => frame.commit_with_damage(&rects),
        }
        let committed = grid_full || !grid.is_empty() || !chrome_rects.is_empty();
        if !committed {
            // Neither the grid nor iced wrote a pixel.
            frame.keep_contents();
        }
        if committed {
            self.raw.note_frame();
            if self.raw.trace() {
                self.raw.log(format_args!(
                    "frame {} window grid_full={grid_full} grid={:?} chrome_full={} chrome={:?}",
                    self.raw.frames(),
                    grid.iter()
                        .map(|r| (r.x, r.y, r.width, r.height))
                        .collect::<Vec<_>>(),
                    drawn.full,
                    chrome_rects
                        .iter()
                        .map(|r| (r.x, r.y, r.width, r.height))
                        .collect::<Vec<_>>()
                ));
            }
        }
        self.schedule(cx, drawn.requests.redraw);
        if drawn.ime_changed {
            self.update_ime(cx);
        }
        if drawn.interaction_changed && self.pointer.is_some_and(|(_, y)| self.in_band(y)) {
            self.set_cursor(cx, cursor_shape(drawn.requests.interaction));
        }
    }
}

impl App for IcedDemo {
    fn init(&mut self, cx: &mut Ctx<'_>) {
        self.raw.init(cx);
    }

    fn event(&mut self, cx: &mut Ctx<'_>, event: Event) {
        match event {
            ev @ Event::Configure {
                surface,
                info,
                first,
                ..
            } if Some(surface) == self.raw.window() => {
                self.raw.event(cx, ev);
                self.resize_chrome(&info);
                // Measurement hook: with no input injection, focus the
                // search field (and tell iced the window is focused) so the
                // caret-blink idle cost can be measured.
                if first && std::env::var_os("WL_DEMO_FOCUS_SEARCH").is_some_and(|v| v == "1") {
                    self.chrome.queue_event(input::focus_event(true));
                    self.process(cx);
                    self.run(cx, Action::FocusSearch);
                }
            }
            ev @ Event::ScaleChanged { surface, info } => {
                if Some(surface) == self.raw.window() {
                    self.raw.event(cx, ev);
                    self.resize_chrome(&info);
                } else {
                    self.menus.configure(surface, &info);
                }
            }
            Event::PopupConfigure {
                surface,
                info,
                position,
                ..
            } => {
                self.raw.log(format_args!(
                    "panel configure {surface:?} at={position:?} logical={:?} physical={:?}",
                    info.logical, info.physical
                ));
                self.menus.configure(surface, &info);
            }
            Event::PopupDone { surface } => {
                self.raw.log(format_args!("panel done {surface:?}"));
                self.menus.dismissed(surface);
                self.after_menu_change(cx);
            }
            ev @ Event::KeyboardFocus { surface, focused }
                if Some(surface) == self.raw.window() =>
            {
                self.chrome.queue_event(input::focus_event(focused));
                self.process(cx);
                self.raw.event(cx, ev);
            }
            Event::Key(key) => {
                self.raw.log(format_args!(
                    "key {:?} sym=0x{:x} text={:?} surface={:?}",
                    key.state,
                    key.keysym.raw(),
                    key.text,
                    key.surface
                ));
                self.key(cx, key);
            }
            Event::Modifiers(m) => {
                self.chrome.queue_event(keys::modifiers_event(m));
                self.process(cx);
            }
            Event::Pointer(p) => {
                if Some(p.surface) == self.raw.window() {
                    self.window_pointer(cx, p);
                } else if let Some(level) = self.menus.level_of(p.surface) {
                    self.popup_pointer(cx, level, p);
                }
            }
            // Input-method results go to the owner they were meant for.
            Event::Ime {
                target: IME_FIELD,
                event,
                ..
            } => {
                self.raw.log(format_args!("ime -> chrome {event:?}"));
                if let Some(e) = ime::to_iced(&event) {
                    self.chrome.queue_event(e);
                    self.process(cx);
                }
            }
            ev @ Event::Ime {
                target: IME_GRID, ..
            } => self.raw.event(cx, ev),
            Event::Ime { target, .. } => {
                self.raw
                    .log(format_args!("ime for unknown target {target}"));
            }
            Event::SelectionChanged { selection } | Event::SelectionLost { selection } => {
                let token = cx.request_selection(selection);
                self.clipboard.0.borrow_mut().start_fetch(selection, token);
            }
            ev @ Event::SelectionText { .. } => {
                let Event::SelectionText {
                    selection,
                    token,
                    ref text,
                    status,
                } = ev
                else {
                    return;
                };
                let fetch = self
                    .clipboard
                    .0
                    .borrow_mut()
                    .finish_fetch(token, text.clone(), status);
                if fetch == Fetch::NotAFetch {
                    if text.is_some() {
                        self.clipboard.0.borrow_mut().set(selection, text.clone());
                    }
                    self.raw.event(cx, ev);
                }
            }
            Event::Timer(TIMER_CHROME) => {
                if let Some(w) = self.raw.window() {
                    cx.request_redraw(w);
                }
            }
            other => self.raw.event(cx, other),
        }
    }

    fn draw(&mut self, cx: &mut Ctx<'_>, frame: &mut Frame<'_>) {
        if Some(frame.surface()) == self.raw.window() {
            self.draw_window(cx, frame);
        } else if self.menus.draw(frame) {
            self.raw.note_frame();
        }
    }
}
