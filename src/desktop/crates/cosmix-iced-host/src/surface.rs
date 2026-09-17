use crate::clipboard::{Adapter, Clipboard, NullClipboard};
use crate::damage::{DamageRect, PixelFormat, swap_red_blue};
use crate::diff::{self, union};
use crate::ime::{ImeRequest, PreeditOverlay};
use crate::{Program, Renderer, Theme};
use iced_core::event::Status;
use iced_core::mouse::{self, Cursor, Interaction};
use iced_core::renderer::Style;
use iced_core::theme::Base as _;
use iced_core::time::Instant;
use iced_core::widget::Operation;
use iced_core::widget::operation::Outcome;
use iced_core::{Color, Event, Font, InputMethod, Pixels, Point, Rectangle, Size, window};
use iced_graphics::Viewport;
use iced_runtime::user_interface::{self, UserInterface};

/// Frames of damage kept for buffer ages greater than one.
const HISTORY: usize = 8;

/// Initial state of a [`Surface`].
#[derive(Debug, Clone)]
pub struct Settings {
    pub physical_size: Size<u32>,
    pub scale_factor: f32,
    pub default_font: Font,
    pub default_text_size: Pixels,
    pub theme: Theme,
    /// Clear colour of damaged pixels. `None` uses the theme background;
    /// `Some(Color::TRANSPARENT)` leaves undrawn areas transparent.
    pub background: Option<Color>,
    /// Draw the IME preedit under the caret (iced_winit behaviour). Turn off
    /// when the input method or compositor draws its own.
    pub draw_preedit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            physical_size: Size::new(1, 1),
            scale_factor: 1.0,
            default_font: Font::DEFAULT,
            default_text_size: Pixels(16.0),
            theme: Theme::Light,
            background: None,
            draw_preedit: true,
        }
    }
}

/// When the host should draw next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Redraw {
    /// Nothing is animating; draw on the next input.
    #[default]
    Wait,
    /// Draw on the next frame callback / vblank.
    NextFrame,
    /// Arm a one-shot timer for this instant (caret blink, animations).
    At(Instant),
}

impl From<window::RedrawRequest> for Redraw {
    fn from(request: window::RedrawRequest) -> Self {
        match request {
            window::RedrawRequest::NextFrame => Self::NextFrame,
            window::RedrawRequest::At(at) => Self::At(at),
            window::RedrawRequest::Wait => Self::Wait,
        }
    }
}

/// State the host acts on, as of the last [`Surface::draw`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Requests {
    pub interaction: Interaction,
    pub ime: ImeRequest,
    pub redraw: Redraw,
}

/// Result of [`Surface::process`].
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// State changed since the last draw; call [`Surface::draw`].
    pub needs_redraw: bool,
    /// Messages delivered to [`Program::update`].
    pub messages: usize,
    pub interaction: Interaction,
    pub interaction_changed: bool,
    /// `NextFrame` when `needs_redraw`; otherwise the pending deadline (a
    /// focused field's caret blink) or `Wait`.
    pub redraw: Redraw,
    /// One status per processed event; `Ignored` events may be passed on
    /// (for example to a terminal grid under the chrome).
    pub statuses: Vec<Status>,
}

/// Result of [`Surface::draw`].
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Disjoint physical rectangles rewritten in the buffer. Empty means the
    /// buffer is unchanged and need not be committed.
    pub damage: Vec<DamageRect>,
    /// The whole buffer was rewritten.
    pub full: bool,
    pub requests: Requests,
    pub ime_changed: bool,
    pub interaction_changed: bool,
    pub messages: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawError {
    ZeroSize,
    /// `stride` must be a whole number of pixels and at least `width * 4`.
    UnsupportedStride {
        stride: u32,
        width: u32,
    },
    BufferTooSmall {
        needed: usize,
        got: usize,
    },
    /// tiny-skia refused a surface of this size (too large for its limits).
    UnsupportedSize {
        width: u32,
        height: u32,
    },
}

impl std::fmt::Display for DrawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroSize => write!(f, "zero-sized buffer"),
            Self::UnsupportedStride { stride, width } => {
                write!(
                    f,
                    "stride {stride} is not whole pixels of at least 4 x width {width}"
                )
            }
            Self::BufferTooSmall { needed, got } => {
                write!(f, "buffer holds {got} bytes, {needed} needed")
            }
            Self::UnsupportedSize { width, height } => {
                write!(f, "tiny-skia refused a {width}x{height} surface")
            }
        }
    }
}

impl std::error::Error for DrawError {}

/// One iced program bound to one caller-owned buffer.
///
/// Not `Send`: the clipboard is a `Box<dyn Clipboard>` and iced's text
/// paragraphs are reference-counted without atomics. Keep a surface on the
/// thread that created it (a Bevy host holds it as a non-send resource).
pub struct Surface<P: Program> {
    program: P,
    renderer: Renderer,
    cache: user_interface::Cache,
    viewport: Viewport,
    theme: Theme,
    background: Option<Color>,
    clipboard: Box<dyn Clipboard>,
    events: Vec<Event>,
    cursor: Cursor,
    drawn_cursor: Cursor,
    drawn_at: Option<Instant>,
    clip_mask: tiny_skia::Mask,
    last_layers: Option<diff::Snapshot>,
    last_background: Color,
    /// Row stride and byte order of the buffer the last frame went into.
    last_buffer: Option<(u32, PixelFormat)>,
    /// Damage of recent frames, newest first, for buffer ages > 1.
    history: std::collections::VecDeque<Vec<DamageRect>>,
    invalid: bool,
    dirty: bool,
    requests: Requests,
    preedit: Option<PreeditOverlay>,
    draw_preedit: bool,
}

impl<P: Program> Surface<P> {
    /// A scale that is not finite and positive is replaced by 1.0.
    pub fn new(program: P, settings: Settings) -> Self {
        let size = non_zero(settings.physical_size);
        let scale = if valid_scale(settings.scale_factor) {
            settings.scale_factor
        } else {
            1.0
        };
        Self {
            program,
            renderer: Renderer::new(settings.default_font, settings.default_text_size),
            cache: user_interface::Cache::new(),
            viewport: Viewport::with_physical_size(size, scale),
            theme: settings.theme,
            background: settings.background,
            clipboard: Box::new(NullClipboard),
            events: Vec::new(),
            cursor: Cursor::Unavailable,
            drawn_cursor: Cursor::Unavailable,
            drawn_at: None,
            clip_mask: tiny_skia::Mask::new(size.width, size.height).expect("clip mask"),
            last_layers: None,
            last_buffer: None,
            history: std::collections::VecDeque::new(),
            last_background: Color::TRANSPARENT,
            invalid: true,
            dirty: true,
            requests: Requests::default(),
            preedit: None,
            draw_preedit: settings.draw_preedit,
        }
    }

    pub fn program(&self) -> &P {
        &self.program
    }

    /// Mutable access marks the surface dirty: the view is rebuilt on the
    /// next draw.
    pub fn program_mut(&mut self) -> &mut P {
        self.dirty = true;
        &mut self.program
    }

    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    pub fn physical_size(&self) -> Size<u32> {
        self.viewport.physical_size()
    }

    pub fn logical_size(&self) -> Size {
        self.viewport.logical_size()
    }

    pub fn scale_factor(&self) -> f32 {
        self.viewport.scale_factor()
    }

    /// Changes the buffer size or scale. Any change forces full damage. A
    /// scale that is not finite and positive keeps the current one.
    pub fn resize(&mut self, physical_size: Size<u32>, scale_factor: f32) {
        let physical_size = non_zero(physical_size);
        let scale_factor = if valid_scale(scale_factor) {
            scale_factor
        } else {
            self.viewport.scale_factor()
        };
        if physical_size == self.viewport.physical_size()
            && scale_factor == self.viewport.scale_factor()
        {
            return;
        }
        if physical_size != self.viewport.physical_size() {
            self.clip_mask =
                tiny_skia::Mask::new(physical_size.width, physical_size.height).expect("clip mask");
        }
        self.viewport = Viewport::with_physical_size(physical_size, scale_factor);
        self.invalidate();
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.invalidate();
    }

    pub fn set_background(&mut self, background: Option<Color>) {
        self.background = background;
        self.dirty = true;
    }

    /// Forces the next draw to repaint the whole buffer (new or lost buffer,
    /// buffer contents not preserved).
    pub fn invalidate(&mut self) {
        self.invalid = true;
        self.dirty = true;
        // Older frames' damage describes contents that no longer exist.
        self.history.clear();
    }

    pub fn queue_event(&mut self, event: Event) {
        self.events.push(event);
    }

    /// Sets the pointer position in logical coordinates without queueing an
    /// event; `None` means the pointer is outside the surface.
    pub fn set_cursor(&mut self, position: Option<Point>) {
        self.cursor = position.map_or(Cursor::Unavailable, Cursor::Available);
    }

    /// Sets the pointer position and queues the matching mouse event.
    pub fn cursor_moved(&mut self, position: Point) {
        self.set_cursor(Some(position));
        self.queue_event(Event::Mouse(mouse::Event::CursorMoved { position }));
    }

    pub fn cursor_left(&mut self) {
        self.set_cursor(None);
        self.queue_event(Event::Mouse(mouse::Event::CursorLeft));
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Last state reported by [`Surface::draw`].
    pub fn requests(&self) -> &Requests {
        &self.requests
    }

    pub fn has_queued_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// Runs queued events through the widget tree and delivers the produced
    /// messages. All queued events go through one `UserInterface::update`.
    /// With nothing queued it builds nothing, but still reports pending work
    /// (a passed deadline, `program_mut`, `operate`, `invalidate`).
    pub fn process(&mut self) -> Update {
        self.process_at(Instant::now())
    }

    /// [`Surface::process`] at `now`.
    pub fn process_at(&mut self, now: Instant) -> Update {
        // A deadline that has passed is a pending draw. It must be read before
        // the tree is touched: the redraw pass below would report the next one.
        if matches!(self.requests.redraw, Redraw::At(at) if at <= now) {
            self.dirty = true;
        }
        if self.events.is_empty() {
            return Update {
                needs_redraw: self.dirty,
                messages: 0,
                interaction: self.requests.interaction,
                interaction_changed: false,
                redraw: self.pending_redraw(),
                statuses: Vec::new(),
            };
        }
        let events = std::mem::take(&mut self.events);
        let size = self.viewport.logical_size();
        let mut messages = Vec::new();
        let mut count = 0;

        // A rebuilt tree has no remembered widget status. Replaying the redraw
        // pass restores it, so hover and press changes in `events` request a
        // redraw as under iced_winit's long-lived interface. It replays the
        // last drawn frame (its time and cursor), so time-based widgets see no
        // elapsed time; a widget that counts redraw passes sees one more.
        let replay = Event::Window(window::Event::RedrawRequested(self.drawn_at.unwrap_or(now)));
        let mut cache = std::mem::take(&mut self.cache);
        let mut ui = UserInterface::build(self.program.view(), size, cache, &mut self.renderer);
        let (primed, _) = ui.update(
            std::slice::from_ref(&replay),
            self.drawn_cursor,
            &mut self.renderer,
            &mut Adapter(self.clipboard.as_mut()),
            &mut messages,
        );
        let mut request = match &primed {
            user_interface::State::Updated { redraw_request, .. } => *redraw_request,
            user_interface::State::Outdated => window::RedrawRequest::NextFrame,
        };
        if primed.has_layout_changed() || !messages.is_empty() {
            self.dirty = true;
        }
        if !messages.is_empty() {
            // As iced_winit: messages from the redraw pass are applied and the
            // tree rebuilt before input is handled.
            cache = ui.into_cache();
            count += messages.len();
            for message in messages.drain(..) {
                self.program.update(message);
            }
            ui = UserInterface::build(self.program.view(), size, cache, &mut self.renderer);
        }
        let (state, statuses) = ui.update(
            &events,
            self.cursor,
            &mut self.renderer,
            &mut Adapter(self.clipboard.as_mut()),
            &mut messages,
        );
        self.cache = ui.into_cache();

        let mut interaction = self.requests.interaction;
        match state {
            user_interface::State::Outdated => self.dirty = true,
            user_interface::State::Updated {
                mouse_interaction,
                redraw_request,
                has_layout_changed,
                ..
            } => {
                interaction = mouse_interaction;
                request = request.min(redraw_request);
                if has_layout_changed || redraw_request == window::RedrawRequest::NextFrame {
                    self.dirty = true;
                }
            }
        }
        if matches!(request, window::RedrawRequest::At(at) if at <= now) {
            self.dirty = true;
        }
        if !messages.is_empty() {
            self.dirty = true;
            count += messages.len();
            for message in messages {
                self.program.update(message);
            }
        }
        let interaction_changed = interaction != self.requests.interaction;
        self.requests.interaction = interaction;
        // A pending draw computes the deadline afresh (the change may have
        // blurred the field), so only an unchanged tree reports it here.
        if !self.dirty {
            self.requests.redraw = Redraw::from(request);
        }

        Update {
            needs_redraw: self.dirty,
            messages: count,
            interaction,
            interaction_changed,
            redraw: self.pending_redraw(),
            statuses,
        }
    }

    /// Whether state changed since the last draw.
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn pending_redraw(&self) -> Redraw {
        if self.dirty {
            Redraw::NextFrame
        } else {
            self.requests.redraw
        }
    }

    /// Applies a widget operation (focus, scroll, text selection) to the
    /// current tree, following chained operations.
    pub fn operate(&mut self, operation: Box<dyn Operation>) {
        let size = self.viewport.logical_size();
        let mut ui = UserInterface::build(
            self.program.view(),
            size,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let mut current = Some(operation);
        while let Some(mut operation) = current.take() {
            ui.operate(&self.renderer, operation.as_mut());
            if let Outcome::Chain(next) = operation.finish() {
                current = Some(next);
            }
        }
        self.cache = ui.into_cache();
        self.dirty = true;
    }

    /// Draws into `buffer` at the current time. See [`Surface::draw_at`].
    pub fn draw(
        &mut self,
        buffer: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
    ) -> Result<Frame, DrawError> {
        self.draw_at(buffer, width, height, stride, format, Instant::now())
    }

    /// [`Surface::draw_at`] into a buffer that is `age` frames old: 1 for
    /// the buffer the last frame was drawn into (the default), `n` for one
    /// holding the contents of `n` frames ago, 0 when the contents are
    /// unknown (a fresh buffer), which repaints everything.
    ///
    /// A client cycling through `wl_shm` buffers reports the age of the one
    /// it got; the damage of the frames in between is added, so those
    /// buffers catch up without a full repaint.
    pub fn draw_aged(
        &mut self,
        buffer: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        age: u32,
    ) -> Result<Frame, DrawError> {
        self.draw_inner(buffer, width, height, stride, format, Instant::now(), age)
    }

    /// Lays out and draws the program, diffs the result against the
    /// previous frame and rewrites only the damaged pixels.
    ///
    /// A `width`/`height` different from the current physical size resizes
    /// the surface (keeping the scale) and repaints everything. `now` drives
    /// caret blink and animations.
    pub fn draw_at(
        &mut self,
        buffer: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        now: Instant,
    ) -> Result<Frame, DrawError> {
        self.draw_inner(buffer, width, height, stride, format, now, 1)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_inner(
        &mut self,
        buffer: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        now: Instant,
        age: u32,
    ) -> Result<Frame, DrawError> {
        if width == 0 || height == 0 {
            return Err(DrawError::ZeroSize);
        }
        if !stride.is_multiple_of(4) || u64::from(stride) < u64::from(width) * 4 {
            return Err(DrawError::UnsupportedStride { stride, width });
        }
        // Rows may be padded (a texture wider than the surface); the padding
        // is never touched. Every row, the last included, is `stride` long.
        let row_pixels = stride / 4;
        let needed = stride as usize * height as usize;
        if buffer.len() < needed {
            return Err(DrawError::BufferTooSmall {
                needed,
                got: buffer.len(),
            });
        }
        let physical = Size::new(width, height);
        if physical != self.viewport.physical_size() {
            let scale = self.viewport.scale_factor();
            self.resize(physical, scale);
        }
        // Same size, different row padding or byte order: the contents are
        // not the previous frame's.
        if self
            .last_buffer
            .is_some_and(|last| last != (stride, format))
        {
            self.invalidate();
        }
        let older = match age {
            1 => Some(Vec::new()),
            n if n >= 2 && (n as usize - 1) <= self.history.len() => Some(
                self.history
                    .iter()
                    .take(n as usize - 1)
                    .flatten()
                    .copied()
                    .collect(),
            ),
            _ => None,
        };
        if older.is_none() {
            self.invalidate();
        }

        let scale = self.viewport.scale_factor();
        let size = self.viewport.logical_size();
        let redraw_event = [Event::Window(window::Event::RedrawRequested(now))];
        let mut messages = Vec::new();
        let mut delivered = 0;
        let mut cache = std::mem::take(&mut self.cache);
        let mut rounds = 0;

        // Mirrors iced_winit: messages produced by the redraw pass are applied
        // and the tree rebuilt, at most three passes.
        let (mut ui, state) = loop {
            let mut ui = UserInterface::build(self.program.view(), size, cache, &mut self.renderer);
            let (state, _) = ui.update(
                &redraw_event,
                self.cursor,
                &mut self.renderer,
                &mut Adapter(self.clipboard.as_mut()),
                &mut messages,
            );
            if (messages.is_empty() && !state.has_layout_changed()) || rounds >= 2 {
                break (ui, state);
            }
            rounds += 1;
            cache = ui.into_cache();
            delivered += messages.len();
            for message in messages.drain(..) {
                self.program.update(message);
            }
        };

        let base = self.theme.base();
        ui.draw(
            &mut self.renderer,
            &self.theme,
            &Style {
                text_color: base.text_color,
            },
            self.cursor,
        );
        self.cache = ui.into_cache();

        let previous = self.requests.clone();
        let mut next = Requests {
            interaction: previous.interaction,
            ime: ImeRequest::Disabled,
            redraw: Redraw::NextFrame,
        };
        let mut still_dirty = false;
        match &state {
            user_interface::State::Outdated => {
                // Nothing was laid out, so nothing reported an input method.
                // Dropping it here would disable text-input for a frame.
                next.ime = previous.ime.clone();
                still_dirty = true;
            }
            user_interface::State::Updated {
                mouse_interaction,
                redraw_request,
                input_method,
                ..
            } => {
                next.interaction = *mouse_interaction;
                next.redraw = (*redraw_request).into();
                next.ime = ImeRequest::from_iced(input_method, scale, physical);
                self.update_preedit(input_method, base.background_color);
            }
        }
        if self.draw_preedit
            && let Some(preedit) = &self.preedit
        {
            preedit.draw(&mut self.renderer, &self.theme, Rectangle::with_size(size));
        }

        if !messages.is_empty() {
            delivered += messages.len();
            for message in messages {
                self.program.update(message);
            }
            still_dirty = true;
        }
        if still_dirty {
            next.redraw = Redraw::NextFrame;
        }

        let background = self.background.unwrap_or(base.background_color);
        let full = self.invalid || self.last_layers.is_none() || self.last_background != background;
        let current = diff::Snapshot::new(self.renderer.layers());
        let damage = match self.last_layers.as_ref().filter(|_| !full) {
            None => {
                vec![DamageRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }]
            }
            Some(previous) => {
                let viewport = Rectangle::with_size(size);
                let mut logical = diff::damage(previous, &current);
                diff::expand_unclipped(&mut logical, previous, &current);
                let mut rects: Vec<DamageRect> = logical
                    .into_iter()
                    .filter_map(|rect| rect.intersection(&viewport))
                    .filter_map(|rect| DamageRect::from_logical(rect, scale, width, height))
                    .collect();
                rects.extend(older.into_iter().flatten());
                disjoint(diff::coalesce(rects))
            }
        };

        if !damage.is_empty() {
            let pixels = &mut buffer[..needed];
            if format == PixelFormat::Rgba8 {
                for rect in &damage {
                    swap_red_blue(pixels, row_pixels, *rect);
                }
            }
            if self.clip_mask.width() != row_pixels || self.clip_mask.height() != height {
                self.clip_mask = tiny_skia::Mask::new(row_pixels, height)
                    .ok_or(DrawError::UnsupportedSize { width, height })?;
            }
            let logical: Vec<Rectangle> =
                damage.iter().map(|rect| rect.to_logical(scale)).collect();
            {
                let mut pixmap = tiny_skia::PixmapMut::from_bytes(pixels, row_pixels, height)
                    .ok_or(DrawError::UnsupportedSize { width, height })?;
                self.renderer.draw(
                    &mut pixmap,
                    &mut self.clip_mask,
                    &self.viewport,
                    &logical,
                    background,
                );
            }
            if format == PixelFormat::Rgba8 {
                for rect in &damage {
                    swap_red_blue(pixels, row_pixels, *rect);
                }
            }
        }

        self.last_layers = Some(current);
        self.last_background = background;
        self.last_buffer = Some((stride, format));
        if self.history.len() == HISTORY {
            self.history.pop_back();
        }
        self.history.push_front(if full {
            vec![DamageRect {
                x: 0,
                y: 0,
                width,
                height,
            }]
        } else {
            damage.clone()
        });
        self.invalid = false;
        self.dirty = still_dirty;
        self.drawn_cursor = self.cursor;
        self.drawn_at = Some(now);

        let ime_changed = next.ime != previous.ime;
        let interaction_changed = next.interaction != previous.interaction;
        self.requests = next.clone();

        Ok(Frame {
            damage,
            full,
            requests: next,
            ime_changed,
            interaction_changed,
            messages: delivered,
        })
    }

    fn update_preedit(&mut self, input_method: &InputMethod, background: Color) {
        match input_method {
            InputMethod::Enabled {
                cursor,
                preedit: Some(preedit),
                ..
            } if !preedit.content.is_empty() => {
                let mut overlay = self.preedit.take().unwrap_or_else(PreeditOverlay::new);
                overlay.update(*cursor, preedit, background, &self.renderer);
                self.preedit = Some(overlay);
            }
            _ => self.preedit = None,
        }
    }
}

fn valid_scale(scale: f32) -> bool {
    scale.is_finite() && scale > 0.0
}

fn non_zero(size: Size<u32>) -> Size<u32> {
    Size::new(size.width.max(1), size.height.max(1))
}

/// Merges overlapping rectangles so every damaged pixel is rewritten (and,
/// for RGBA, byte-swapped) exactly once.
fn disjoint(mut rects: Vec<DamageRect>) -> Vec<DamageRect> {
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                if overlaps(&rects[i], &rects[j]) {
                    let other = rects.swap_remove(j);
                    rects[i] = union(&rects[i], &other);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
    rects
}

fn overlaps(a: &DamageRect, b: &DamageRect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> DamageRect {
        DamageRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn overlapping_damage_is_merged() {
        let rects = disjoint(vec![
            rect(0, 0, 10, 10),
            rect(20, 20, 5, 5),
            rect(5, 5, 10, 10),
        ]);
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&rect(0, 0, 15, 15)));
        assert!(rects.contains(&rect(20, 20, 5, 5)));
    }

    #[test]
    fn touching_damage_stays_separate() {
        assert_eq!(
            disjoint(vec![rect(0, 0, 10, 10), rect(10, 0, 10, 10)]).len(),
            2
        );
    }
}
