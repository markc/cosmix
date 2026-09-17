//! The raw demo: a hand-drawn text grid with a popup menu, IME preedit and
//! clipboard, on `cosmix-wl-app` alone.

use crate::font::Font;
use crate::paint::{Canvas, Rgb};
use crate::startup::Clock;
use cosmix_wl_app::{
    App, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, ButtonState, ContentPurpose, Ctx, CursorShape, Event,
    Frame, ImeEvent, ImeState, KeyState, Keysym, PointerKind, PopupSpec, Rect, Selection,
    SurfaceId, SurfaceInfo, WindowSpec,
};

/// Wake token that ends the run (sent by the `WL_DEMO_EXIT_AFTER` thread).
pub const WAKE_EXIT: u64 = 1;
/// The grid's input-method target (see `ImeState::target`).
pub const IME_GRID: u64 = 1;

const FONT_PX: f32 = 15.0;
const HEADER: u32 = 28;
const PAD: u32 = 8;
const MENU_ROW: u32 = 30;
const MENU_W: u32 = 220;

const BG: Rgb = Rgb(0x1d, 0x20, 0x26);
const BG_ALT: Rgb = Rgb(0x14, 0x2a, 0x22);
const FG: Rgb = Rgb(0xd8, 0xde, 0xe9);
const HEADER_BG: Rgb = Rgb(0x2e, 0x34, 0x40);
const ACCENT: Rgb = Rgb(0xeb, 0xcb, 0x8b);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Normal,
    Preedit,
    Cursor,
}

type Cell = (char, Style);

/// Lays `text` out in a `cols` x `rows` grid, wrapping and keeping the last
/// rows. Returns the cells and the cursor index (just after the text).
pub fn layout(text: &str, cols: usize, rows: usize) -> (Vec<char>, usize) {
    let mut cells = vec![' '; cols * rows];
    if cols == 0 || rows == 0 {
        return (cells, 0);
    }
    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(Vec::new());
            continue;
        }
        if lines.last().is_some_and(|l| l.len() == cols) {
            lines.push(Vec::new());
        }
        if let Some(l) = lines.last_mut() {
            l.push(ch);
        }
    }
    // The cursor sits after the last char; a full last line wraps it.
    if lines.last().is_some_and(|l| l.len() == cols) {
        lines.push(Vec::new());
    }
    let skip = lines.len().saturating_sub(rows);
    for (r, line) in lines[skip..].iter().enumerate() {
        for (c, ch) in line.iter().enumerate() {
            cells[r * cols + c] = *ch;
        }
    }
    let last = lines.len() - skip - 1;
    let cursor = last * cols + lines[lines.len() - 1].len();
    (cells, cursor)
}

struct Menu {
    id: SurfaceId,
    rows: [(&'static str, Rgb); 3],
    hover: Option<usize>,
    drawn_hover: Option<usize>,
    info: Option<SurfaceInfo>,
}

pub struct RawDemo {
    trace: bool,
    clock: Clock,
    window: Option<SurfaceId>,
    info: Option<SurfaceInfo>,
    font: Option<Font>,
    cols: usize,
    rows: usize,
    /// The grid's text (the embedding app swaps it per tab).
    pub text: String,
    preedit: String,
    header: u32,
    embedded: bool,
    ime_blocked: bool,
    name: &'static str,
    drawn: Vec<Cell>,
    force_full: bool,
    alt_bg: bool,
    focused: bool,
    ime_rect: Option<Rect>,
    menus: Vec<Menu>,
    printed_startup: bool,
    frames: u64,
}

impl RawDemo {
    pub fn new(clock: Clock) -> Self {
        Self {
            trace: crate::trace_enabled(),
            clock,
            window: None,
            info: None,
            font: None,
            cols: 0,
            rows: 0,
            text: String::from("cosmix-wl-app raw demo. Type; right-click for a menu.\n"),
            preedit: String::new(),
            drawn: Vec::new(),
            force_full: true,
            alt_bg: false,
            focused: false,
            ime_rect: None,
            menus: Vec::new(),
            printed_startup: false,
            frames: 0,
            header: HEADER,
            embedded: false,
            ime_blocked: false,
            name: "wl-raw-demo",
        }
    }

    /// A grid under `header` logical pixels of someone else's chrome: no
    /// title band, no raw popup menu, and the embedder commits.
    pub fn embedded(clock: Clock, name: &'static str, header: u32) -> Self {
        Self {
            header,
            embedded: true,
            name,
            ..Self::new(clock)
        }
    }

    pub fn window(&self) -> Option<SurfaceId> {
        self.window
    }

    pub fn info(&self) -> Option<SurfaceInfo> {
        self.info
    }

    pub fn trace(&self) -> bool {
        self.trace
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Repaint the whole grid on the next draw.
    pub fn invalidate(&mut self, cx: &mut Ctx<'_>) {
        self.force_full = true;
        self.redraw(cx);
    }

    /// While blocked (a chrome text field owns the input method), the grid
    /// neither enables nor updates the IME.
    pub fn set_ime_blocked(&mut self, cx: &mut Ctx<'_>, blocked: bool) {
        if self.ime_blocked == blocked {
            return;
        }
        self.ime_blocked = blocked;
        self.ime_rect = None;
        if blocked {
            // Composition in the grid ends when the field takes the input
            // method; the runtime drops whatever was still in flight for it.
            if !self.preedit.is_empty() {
                self.preedit.clear();
                self.redraw(cx);
            }
        } else {
            self.sync_ime(cx);
        }
    }

    /// Count a frame committed by the embedder (for the startup line).
    pub fn note_frame(&mut self) {
        self.frames += 1;
        if !self.printed_startup {
            self.printed_startup = true;
            let now = std::time::Instant::now();
            eprintln!(
                "{}: startup_ms={:.1} (process start to first commit; main to commit {:.1} ms)",
                self.name,
                self.clock.startup_ms(now),
                self.clock.main_to_ms(now)
            );
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn log(&self, msg: std::fmt::Arguments<'_>) {
        if self.trace {
            eprintln!("{}: {msg}", self.name);
        }
    }

    fn bg(&self) -> Rgb {
        if self.alt_bg { BG_ALT } else { BG }
    }

    fn grid_origin(&self, info: &SurfaceInfo) -> (i32, i32) {
        let pad = info.scale.to_physical(PAD) as i32;
        (pad, info.scale.to_physical(self.header) as i32 + pad)
    }

    fn relayout(&mut self, info: SurfaceInfo) {
        let rescale = self.info.is_none_or(|old| old.scale != info.scale);
        if rescale || self.font.is_none() {
            match Font::load(FONT_PX, info.scale_factor()) {
                Ok(f) => self.font = Some(f),
                Err(e) => eprintln!("wl-raw-demo: font: {e}"),
            }
        }
        self.info = Some(info);
        if let Some(font) = &self.font {
            let (ox, oy) = self.grid_origin(&info);
            let w = info.physical.0 as i32 - 2 * ox;
            let h = info.physical.1 as i32 - oy - ox;
            self.cols = (w.max(0) as u32 / font.cell_w) as usize;
            self.rows = (h.max(0) as u32 / font.cell_h) as usize;
        }
        self.force_full = true;
    }

    fn wanted(&self) -> (Vec<Cell>, usize) {
        let (chars, cursor) = layout(&self.text, self.cols, self.rows);
        let mut cells: Vec<Cell> = chars.into_iter().map(|c| (c, Style::Normal)).collect();
        let mut at = cursor;
        for ch in self.preedit.chars() {
            if at >= cells.len() {
                break;
            }
            cells[at] = (ch, Style::Preedit);
            at += 1;
        }
        if at < cells.len() {
            cells[at].1 = Style::Cursor;
        }
        (cells, at)
    }

    pub fn insert(&mut self, cx: &mut Ctx<'_>, s: &str) {
        let clean: String = s
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        if clean.is_empty() {
            return;
        }
        self.text.push_str(&clean);
        self.redraw(cx);
    }

    fn redraw(&self, cx: &mut Ctx<'_>) {
        if let Some(w) = self.window {
            cx.request_redraw(w);
        }
    }

    fn sync_ime(&mut self, cx: &mut Ctx<'_>) {
        let (Some(window), Some(info), Some(font)) = (self.window, self.info, &self.font) else {
            return;
        };
        if self.ime_blocked {
            return;
        }
        if !self.focused {
            if self.ime_rect.take().is_some() {
                cx.set_ime(None);
            }
            return;
        }
        let (_, cursor) = self.wanted();
        let cols = self.cols.max(1);
        let (ox, oy) = self.grid_origin(&info);
        let px = ox + (cursor % cols) as i32 * font.cell_w as i32;
        let py = oy + (cursor / cols) as i32 * font.cell_h as i32;
        let s = info.scale_factor();
        let rect = Rect::new(
            (f64::from(px) / s) as i32,
            (f64::from(py) / s) as i32,
            1,
            (f64::from(font.cell_h) / s).ceil() as i32,
        );
        if self.ime_rect != Some(rect) {
            self.ime_rect = Some(rect);
            let mut state = ImeState::new(window, rect);
            state.target = IME_GRID;
            state.purpose = ContentPurpose::Terminal;
            cx.set_ime(Some(state));
        }
    }

    fn open_menu(&mut self, cx: &mut Ctx<'_>, parent: SurfaceId, spec: PopupSpec, sub: bool) {
        let rows = if sub {
            [
                ("Copy", Rgb(0x5e, 0x81, 0xac)),
                ("Paste", Rgb(0x81, 0xa1, 0xc1)),
                ("Close menu", Rgb(0x4c, 0x56, 0x6a)),
            ]
        } else {
            [
                ("Red-ish row", Rgb(0xbf, 0x61, 0x6a)),
                ("Toggle background", Rgb(0xa3, 0xbe, 0x8c)),
                ("More  >", Rgb(0xb4, 0x8e, 0xad)),
            ]
        };
        match cx.create_popup(parent, &spec) {
            Ok(id) => {
                self.log(format_args!(
                    "popup open {id:?} parent={parent:?} sub={sub}"
                ));
                self.menus.push(Menu {
                    id,
                    rows,
                    hover: None,
                    drawn_hover: None,
                    info: None,
                });
            }
            Err(e) => eprintln!("wl-raw-demo: popup: {e}"),
        }
    }

    fn close_menus(&mut self, cx: &mut Ctx<'_>, from: usize) {
        if let Some(menu) = self.menus.get(from) {
            cx.close_popup(menu.id);
            self.log(format_args!("popup close {:?}", menu.id));
        }
        self.menus.truncate(from);
    }

    fn menu_index(&self, id: SurfaceId) -> Option<usize> {
        self.menus.iter().position(|m| m.id == id)
    }

    fn activate(&mut self, cx: &mut Ctx<'_>, level: usize, row: usize) {
        let label = self.menus[level].rows[row].0;
        self.log(format_args!(
            "menu activate level={level} row={row} {label:?}"
        ));
        match (level, row) {
            (0, 0) => self.close_menus(cx, 0),
            (0, 1) => {
                self.alt_bg = !self.alt_bg;
                self.force_full = true;
                self.redraw(cx);
                self.close_menus(cx, 0);
            }
            (0, 2) => {
                if self.menus.len() > 1 {
                    return;
                }
                let parent = self.menus[0].id;
                let row_rect = Rect::new(0, (2 * MENU_ROW) as i32, MENU_W as i32, MENU_ROW as i32);
                self.open_menu(
                    cx,
                    parent,
                    PopupSpec::submenu(row_rect, (MENU_W, 3 * MENU_ROW)),
                    true,
                );
            }
            (_, 0) => {
                self.copy(cx);
                self.close_menus(cx, 0);
            }
            (_, 1) => {
                cx.request_selection(Selection::Clipboard);
                self.close_menus(cx, 0);
            }
            _ => self.close_menus(cx, 0),
        }
    }

    fn copy(&mut self, cx: &mut Ctx<'_>) {
        let ok = cx.set_selection(Selection::Clipboard, self.text.clone());
        cx.set_selection(Selection::Primary, self.text.clone());
        self.log(format_args!("copy {} bytes ok={ok}", self.text.len()));
    }

    fn set_hover(&mut self, cx: &mut Ctx<'_>, level: usize, hover: Option<usize>) {
        let menu = &mut self.menus[level];
        if menu.hover != hover {
            menu.hover = hover;
            cx.request_redraw(menu.id);
        }
    }

    fn draw_window(&mut self, frame: &mut Frame<'_>) {
        let Some((full, damage)) = self.paint_window(frame) else {
            return;
        };
        if full {
            frame.commit_full();
        } else if !damage.is_empty() {
            frame.commit_with_damage(&damage);
        }
        if full || !damage.is_empty() {
            self.note_frame();
            self.log(format_args!(
                "frame {} window full={full} damage_rects={}",
                self.frames,
                damage.len()
            ));
        }
    }

    /// Paint changed grid cells (everything when `full`) without committing.
    /// Returns whether the whole buffer was painted and the damaged cells.
    pub fn paint_window(&mut self, frame: &mut Frame<'_>) -> Option<(bool, Vec<Rect>)> {
        let info = frame.info();
        let full = frame.needs_full_redraw() || self.force_full;
        let bg = self.bg();
        let (ox, oy) = self.grid_origin(&info);
        let (cells, _) = self.wanted();
        let font = self.font.as_mut()?;
        if !full && cells.len() == self.drawn.len() && cells == self.drawn {
            // Nothing to paint: leave the buffer untouched.
            return Some((false, Vec::new()));
        }
        let (pixels, width, height, stride) = frame.buffer_mut();
        let mut canvas = Canvas {
            pixels,
            width,
            height,
            stride,
        };
        let (cw, ch) = (font.cell_w as i32, font.cell_h as i32);
        let mut damage = Vec::new();
        if full {
            canvas.fill(Rect::new(0, 0, width as i32, height as i32), bg);
            self.drawn.clear();
        }
        if full && !self.embedded {
            let header = info.scale.to_physical(HEADER) as i32;
            canvas.fill(Rect::new(0, 0, width as i32, header), HEADER_BG);
            let title = format!(
                "wl-raw-demo  {}x{} @ {:.2}  grid {}x{}",
                info.logical.0,
                info.logical.1,
                info.scale_factor(),
                self.cols,
                self.rows
            );
            canvas.text(font, ox, (header - ch) / 2, &title, ACCENT);
        }
        self.drawn.resize(cells.len(), (' ', Style::Normal));
        for (i, cell) in cells.iter().enumerate() {
            if !full && self.drawn[i] == *cell {
                continue;
            }
            let x = ox + (i % self.cols) as i32 * cw;
            let y = oy + (i / self.cols) as i32 * ch;
            let r = Rect::new(x, y, cw, ch);
            let (fg, cell_bg) = match cell.1 {
                Style::Normal | Style::Preedit => (FG, bg),
                Style::Cursor => (bg, FG),
            };
            canvas.fill(r, cell_bg);
            if cell.1 == Style::Preedit {
                canvas.fill(
                    Rect::new(x, y + ch - (ch / 12).max(1), cw, (ch / 12).max(1)),
                    ACCENT,
                );
            }
            if cell.0 != ' ' {
                canvas.glyph(
                    font,
                    x,
                    y,
                    cell.0,
                    if cell.1 == Style::Preedit { ACCENT } else { fg },
                );
            }
            self.drawn[i] = *cell;
            if !full {
                damage.push(r);
            }
        }
        self.force_full = false;
        Some((full, damage))
    }

    fn draw_menu(&mut self, level: usize, frame: &mut Frame<'_>) {
        let full = frame.needs_full_redraw();
        let info = frame.info();
        let menu = &mut self.menus[level];
        menu.info = Some(info);
        let Some(font) = self.font.as_mut() else {
            return;
        };
        let (pixels, width, height, stride) = frame.buffer_mut();
        let mut canvas = Canvas {
            pixels,
            width,
            height,
            stride,
        };
        let row_h = (height / 3) as i32;
        let mut damage = Vec::new();
        for (i, (label, colour)) in menu.rows.iter().enumerate() {
            let changed = menu.hover != menu.drawn_hover
                && (menu.hover == Some(i) || menu.drawn_hover == Some(i));
            if !full && !changed {
                continue;
            }
            let r = Rect::new(0, i as i32 * row_h, width as i32, row_h);
            let c = if menu.hover == Some(i) {
                Rgb(
                    colour.0.saturating_add(40),
                    colour.1.saturating_add(40),
                    colour.2.saturating_add(40),
                )
            } else {
                *colour
            };
            canvas.fill(r, c);
            let pad = info.scale.to_physical(PAD) as i32;
            canvas.text(
                font,
                pad,
                r.y + (row_h - font.cell_h as i32) / 2,
                label,
                Rgb(0x10, 0x10, 0x14),
            );
            damage.push(r);
        }
        menu.drawn_hover = menu.hover;
        if full {
            frame.commit_full();
        } else if !damage.is_empty() {
            frame.commit_with_damage(&damage);
        }
        if full || !damage.is_empty() {
            let id = menu.id;
            self.note_frame();
            self.log(format_args!(
                "frame {} popup {id:?} full={full} damage_rects={}",
                self.frames,
                damage.len()
            ));
        }
    }

    fn menu_row(&self, level: usize, y: f64) -> Option<usize> {
        let h = self.menus[level].info.map_or(3 * MENU_ROW, |i| i.logical.1);
        let row = (y / (f64::from(h) / 3.0)).floor();
        (0.0..3.0).contains(&row).then_some(row as usize)
    }
}

impl App for RawDemo {
    fn init(&mut self, cx: &mut Ctx<'_>) {
        let mut spec = WindowSpec::new(self.name, (800, 500));
        spec.app_id = format!("cosmix-{}", self.name);
        spec.min_size = Some((200, 120));
        self.window = Some(cx.create_window(spec));
        cx.set_cursor(CursorShape::Text);
    }

    fn event(&mut self, cx: &mut Ctx<'_>, event: Event) {
        match event {
            Event::Configure {
                surface,
                info,
                state,
                first,
            } if Some(surface) == self.window => {
                self.log(format_args!(
                    "configure first={first} logical={:?} physical={:?} scale={:.3} max={} ssd={}",
                    info.logical,
                    info.physical,
                    info.scale_factor(),
                    state.maximized,
                    state.server_decorations
                ));
                if first || self.info != Some(info) {
                    self.relayout(info);
                }
            }
            Event::ScaleChanged { surface, info } => {
                self.log(format_args!(
                    "scale {surface:?} {:.3} physical={:?}",
                    info.scale_factor(),
                    info.physical
                ));
                if Some(surface) == self.window {
                    self.relayout(info);
                    self.ime_rect = None;
                    self.sync_ime(cx);
                }
            }
            Event::PopupConfigure {
                surface,
                info,
                position,
                first,
                ..
            } => {
                self.log(format_args!(
                    "popup configure {surface:?} first={first} at={position:?} logical={:?} physical={:?}",
                    info.logical, info.physical
                ));
                if let Some(i) = self.menu_index(surface) {
                    self.menus[i].info = Some(info);
                }
            }
            Event::PopupDone { surface } => {
                self.log(format_args!("popup done {surface:?}"));
                if let Some(i) = self.menu_index(surface) {
                    self.menus.truncate(i);
                }
            }
            Event::CloseRequested { surface } => {
                self.log(format_args!("close requested {surface:?}"));
                cx.exit();
            }
            Event::KeyboardFocus { surface, focused } => {
                self.log(format_args!("keyboard focus {surface:?} {focused}"));
                if Some(surface) == self.window {
                    self.focused = focused;
                    self.sync_ime(cx);
                }
            }
            Event::Key(key) => {
                if key.state == KeyState::Released {
                    return;
                }
                self.log(format_args!(
                    "key {:?} sym=0x{:x} text={:?} mods={:?} surface={:?}",
                    key.state,
                    key.keysym.raw(),
                    key.text,
                    key.modifiers,
                    key.surface
                ));
                let sym = key.keysym;
                if let Some(level) = key.surface.and_then(|s| self.menu_index(s)) {
                    let hover = self.menus[level].hover;
                    if sym == Keysym::Escape {
                        self.close_menus(cx, level);
                    } else if sym == Keysym::Down {
                        self.set_hover(cx, level, Some(hover.map_or(0, |h| (h + 1) % 3)));
                    } else if sym == Keysym::Up {
                        self.set_hover(cx, level, Some(hover.map_or(2, |h| (h + 2) % 3)));
                    } else if (sym == Keysym::Return || sym == Keysym::Right)
                        && let Some(row) = hover
                    {
                        self.activate(cx, level, row);
                    } else if sym == Keysym::Left && level > 0 {
                        self.close_menus(cx, level);
                    }
                    return;
                }
                let m = key.modifiers;
                if m.ctrl && m.shift && (sym == Keysym::C || sym == Keysym::c) {
                    self.copy(cx);
                } else if m.ctrl && m.shift && (sym == Keysym::V || sym == Keysym::v) {
                    self.log(format_args!("paste requested"));
                    cx.request_selection(Selection::Clipboard);
                } else if sym == Keysym::Escape {
                    self.close_menus(cx, 0);
                } else if sym == Keysym::Return || sym == Keysym::KP_Enter {
                    self.insert(cx, "\n");
                } else if sym == Keysym::BackSpace {
                    if self.text.pop().is_some() {
                        self.redraw(cx);
                    }
                } else if !m.ctrl
                    && let Some(text) = &key.text
                {
                    let text = text.clone();
                    self.insert(cx, &text);
                }
                self.sync_ime(cx);
            }
            Event::Modifiers(_) => {}
            Event::Pointer(p) => {
                if let Some(level) = self.menu_index(p.surface) {
                    match p.kind {
                        PointerKind::Enter | PointerKind::Motion => {
                            let row = self.menu_row(level, p.position.1);
                            self.set_hover(cx, level, row);
                        }
                        PointerKind::Leave => self.set_hover(cx, level, None),
                        PointerKind::Button {
                            button: BTN_LEFT,
                            state: ButtonState::Pressed,
                        } => {
                            self.log(format_args!("menu press level={level} at={:?}", p.position));
                            if let Some(row) = self.menu_row(level, p.position.1) {
                                self.activate(cx, level, row);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                if Some(p.surface) != self.window {
                    return;
                }
                match p.kind {
                    PointerKind::Button {
                        button,
                        state: ButtonState::Pressed,
                    } => {
                        self.log(format_args!("button 0x{button:x} at={:?}", p.position));
                        if button == BTN_RIGHT && !self.embedded {
                            self.close_menus(cx, 0);
                            let (x, y) = (p.position.0 as i32, p.position.1 as i32);
                            if let Some(w) = self.window {
                                self.open_menu(
                                    cx,
                                    w,
                                    PopupSpec::menu_at(x, y, (MENU_W, 3 * MENU_ROW)),
                                    false,
                                );
                            }
                        } else if button == BTN_MIDDLE {
                            cx.request_selection(Selection::Primary);
                        }
                    }
                    PointerKind::Axis {
                        vertical,
                        vertical_120,
                        ..
                    } => {
                        self.log(format_args!("axis v={vertical:.1} v120={vertical_120}"));
                    }
                    PointerKind::Enter => self.log(format_args!("pointer enter")),
                    PointerKind::Leave => self.log(format_args!("pointer leave")),
                    _ => {}
                }
            }
            Event::Ime {
                surface,
                target,
                event,
            } => {
                self.log(format_args!("ime {surface:?} target={target} {event:?}"));
                if target != IME_GRID {
                    return;
                }
                match event {
                    ImeEvent::Focus { .. } => {}
                    ImeEvent::DeleteSurrounding { before, .. } => {
                        for _ in 0..before {
                            // Byte count; the demo text is mostly ASCII.
                            if self.text.pop().is_none() {
                                break;
                            }
                        }
                        self.redraw(cx);
                    }
                    ImeEvent::Commit(text) => self.insert(cx, &text),
                    ImeEvent::Preedit { text, .. } => {
                        self.preedit = text;
                        self.redraw(cx);
                    }
                }
                self.sync_ime(cx);
            }
            Event::SelectionText {
                selection,
                token,
                text,
                status,
            } => {
                self.log(format_args!(
                    "selection {selection:?} token={token} {status:?} {} bytes",
                    text.as_ref().map_or(0, |t| t.len())
                ));
                if let Some(text) = text {
                    self.insert(cx, &text);
                    self.sync_ime(cx);
                }
            }
            Event::SelectionLost { selection } => {
                self.log(format_args!("selection lost {selection:?}"));
            }
            Event::Wake(WAKE_EXIT) => {
                self.log(format_args!("exit timer"));
                cx.exit();
            }
            _ => {}
        }
    }

    fn draw(&mut self, _cx: &mut Ctx<'_>, frame: &mut Frame<'_>) {
        let id = frame.surface();
        if Some(id) == self.window {
            self.draw_window(frame);
        } else if let Some(level) = self.menu_index(id) {
            self.draw_menu(level, frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_wraps_and_scrolls() {
        let (cells, cursor) = layout("abcde", 3, 2);
        assert_eq!(cells, vec!['a', 'b', 'c', 'd', 'e', ' ']);
        assert_eq!(cursor, 5);
        let (cells, cursor) = layout("abc", 3, 2);
        assert_eq!(&cells[..3], &['a', 'b', 'c']);
        assert_eq!(cursor, 3);
        let (cells, cursor) = layout("a\nb\nc", 3, 2);
        assert_eq!(cells, vec!['b', ' ', ' ', 'c', ' ', ' ']);
        assert_eq!(cursor, 4);
        let (cells, cursor) = layout("abcdef", 3, 2);
        assert_eq!(cells, vec!['d', 'e', 'f', ' ', ' ', ' ']);
        assert_eq!(cursor, 3);
        assert_eq!(layout("x", 0, 0), (vec![], 0));
    }

    #[test]
    fn typing_changes_only_two_cells() {
        let mut demo = RawDemo::new(Clock::start());
        demo.text.clear();
        demo.cols = 10;
        demo.rows = 4;
        let (before, _) = demo.wanted();
        demo.text.push('x');
        let (after, _) = demo.wanted();
        let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        // The typed cell and the cursor that moved on.
        assert_eq!(changed, 2);
    }

    #[test]
    fn preedit_overlays_after_cursor() {
        let mut demo = RawDemo::new(Clock::start());
        demo.text = "ab".into();
        demo.preedit = "xy".into();
        demo.cols = 5;
        demo.rows = 1;
        let (cells, cursor) = demo.wanted();
        assert_eq!(cursor, 4);
        assert_eq!(cells[2], ('x', Style::Preedit));
        assert_eq!(cells[3], ('y', Style::Preedit));
        assert_eq!(cells[4].1, Style::Cursor);
    }
}
