//! Piano roll for very large note sets.
//!
//! Notes are kept sorted by start, so only notes overlapping the view are
//! visited. The roll is drawn in fixed-width tiles of content; each tile's
//! geometry is cached per zoom level (pixels per beat and row height) and
//! scrolling only translates cached tiles. Grid, notes and playhead are
//! separate layers, so moving the playhead never touches note geometry.
use std::cell::RefCell;

use iced_core::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced_core::{Color, Element, Event, Length, Point, Rectangle, Size, Vector, keyboard};
use iced_graphics::geometry::{self, Cache, Path};

use crate::AudioStyle;
use crate::audio_style::quad;
use crate::waveform::next_generation;

/// Width of one cached tile, in logical pixels of content.
pub const TILE_WIDTH: f32 = 512.0;
/// Most tiles kept across zoom levels before the least recently used goes.
pub const MAX_TILES: usize = 64;
/// Zoom limits, in pixels per beat.
pub const MIN_PIXELS_PER_BEAT: f32 = 0.25;
pub const MAX_PIXELS_PER_BEAT: f32 = 2000.0;
const PITCHES: usize = 128;
const LINE_PIXELS: f32 = 40.0;
const VELOCITY_LEVELS: usize = 4;

/// One note. Times are in beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: u8,
}

/// An immutable, start-sorted note set. Build it once per song edit; the roll
/// redraws its tiles only when it is shown a different `RollNotes` value.
#[derive(Debug, Clone)]
pub struct RollNotes {
    notes: Vec<Note>,
    max_length: f32,
    end: f32,
    generation: u64,
}

impl RollNotes {
    /// Sorts `notes` by start. Notes with a pitch above 127, or a non-finite or
    /// non-positive length, are dropped.
    pub fn new(mut notes: Vec<Note>) -> Self {
        notes.retain(|note| {
            note.pitch < 128
                && note.start.is_finite()
                && note.length.is_finite()
                && note.length > 0.0
        });
        notes.sort_by(|a, b| a.start.total_cmp(&b.start));
        let max_length = notes.iter().map(|n| n.length).fold(0.0, f32::max);
        let end = notes.iter().map(|n| n.start + n.length).fold(0.0, f32::max);
        Self {
            notes,
            max_length,
            end,
            generation: next_generation(),
        }
    }

    /// The notes, sorted by start. Indices published by `on_note` refer here.
    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    /// Number of notes.
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    /// True when there are no notes.
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// The end of the last note, in beats.
    pub fn end_beat(&self) -> f32 {
        self.end
    }

    /// Notes overlapping `[from, to)` beats, with their indices. Cost is
    /// logarithmic in the note count plus the notes that start in
    /// `[from - longest note, to)`.
    pub fn visible(&self, from: f32, to: f32) -> impl Iterator<Item = (usize, &Note)> {
        let lo = self
            .notes
            .partition_point(|n| n.start < from - self.max_length);
        let hi = self.notes.partition_point(|n| n.start < to);
        self.notes[lo..hi.max(lo)]
            .iter()
            .enumerate()
            .map(move |(offset, note)| (lo + offset, note))
            .filter(move |(_, note)| note.start + note.length > from)
    }
}

/// The app-owned view: scroll position and zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollView {
    /// Beat at the left edge.
    pub scroll_beats: f32,
    /// Pixels scrolled down from pitch 127's row.
    pub scroll_y: f32,
    /// Horizontal zoom.
    pub pixels_per_beat: f32,
    /// Height of one pitch row.
    pub row_height: f32,
}

impl Default for RollView {
    fn default() -> Self {
        Self {
            scroll_beats: 0.0,
            scroll_y: 48.0 * 8.0,
            pixels_per_beat: 24.0,
            row_height: 8.0,
        }
    }
}

impl RollView {
    /// View-relative x of `beat`.
    pub fn x_of(&self, beat: f32) -> f32 {
        (beat - self.scroll_beats) * self.pixels_per_beat
    }

    /// Beat at view-relative `x`.
    pub fn beat_at(&self, x: f32) -> f32 {
        self.scroll_beats + x / self.pixels_per_beat
    }

    /// View-relative y of the top of `pitch`'s row.
    pub fn y_of(&self, pitch: u8) -> f32 {
        f32::from(127 - pitch.min(127)) * self.row_height - self.scroll_y
    }

    /// Pitch whose row contains view-relative `y`.
    pub fn pitch_at(&self, y: f32) -> Option<u8> {
        let row = ((y + self.scroll_y) / self.row_height).floor();
        if (0.0..PITCHES as f32).contains(&row) {
            Some(127 - row as u8)
        } else {
            None
        }
    }

    /// Zooms horizontally by `factor`, keeping the beat under view-relative
    /// `anchor_x` in place (unless that would scroll before beat 0).
    pub fn zoomed(&self, factor: f32, anchor_x: f32) -> Self {
        let anchor = self.beat_at(anchor_x);
        let pixels_per_beat =
            (self.pixels_per_beat * factor).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
        Self {
            pixels_per_beat,
            scroll_beats: (anchor - anchor_x / pixels_per_beat).max(0.0),
            ..*self
        }
    }

    /// Scrolls by pixels, clamped to the song (`end_beat`) and the 128 rows
    /// less the `viewport_height`.
    pub fn scrolled(&self, dx: f32, dy: f32, viewport_height: f32, end_beat: f32) -> Self {
        let max_y = (PITCHES as f32 * self.row_height - viewport_height).max(0.0);
        Self {
            scroll_beats: (self.scroll_beats + dx / self.pixels_per_beat)
                .clamp(0.0, end_beat.max(0.0)),
            scroll_y: (self.scroll_y + dy).clamp(0.0, max_y),
            ..*self
        }
    }

    /// Index of the topmost note under view-relative `point`.
    pub fn note_at(&self, notes: &RollNotes, point: Point) -> Option<usize> {
        let pitch = self.pitch_at(point.y)?;
        let beat = self.beat_at(point.x);
        notes
            .visible(beat, beat.next_up())
            .filter(|(_, note)| note.pitch == pitch)
            .map(|(index, _)| index)
            .last()
    }

    /// False for a zero, negative or non-finite zoom or row height, which the
    /// roll refuses to draw or scroll.
    pub fn is_valid(&self) -> bool {
        self.pixels_per_beat.is_finite()
            && self.pixels_per_beat > 0.0
            && self.row_height.is_finite()
            && self.row_height > 0.0
            && self.scroll_beats.is_finite()
            && self.scroll_y.is_finite()
    }

    fn key(&self) -> (u32, u32) {
        (self.pixels_per_beat.to_bits(), self.row_height.to_bits())
    }
}

/// Tile-local rectangles for tile `index`, grouped by velocity level. Notes
/// are clipped to the tile. Notes narrower than a pixel collapse onto one
/// pixel column, and each (column, pitch) is emitted once, so sub-pixel notes
/// in a zoomed-out dense song cost at most one rectangle per pixel per row.
pub(crate) fn tile_rects(
    notes: &RollNotes,
    pixels_per_beat: f32,
    row_height: f32,
    index: i64,
) -> [Vec<Rectangle>; VELOCITY_LEVELS] {
    let x0 = index as f32 * TILE_WIDTH;
    let columns = TILE_WIDTH as usize;
    let mut covered = vec![0u64; (columns * PITCHES).div_ceil(64)];
    let mut out: [Vec<Rectangle>; VELOCITY_LEVELS] = Default::default();
    let height = (row_height - 1.0).max(1.0);
    for (_, note) in notes.visible(x0 / pixels_per_beat, (x0 + TILE_WIDTH) / pixels_per_beat) {
        let start = (note.start * pixels_per_beat - x0).max(0.0);
        let end = ((note.start + note.length) * pixels_per_beat - x0).min(TILE_WIDTH);
        if end <= start {
            continue;
        }
        let y = f32::from(127 - note.pitch) * row_height;
        let level = usize::from(note.velocity.min(127)) * VELOCITY_LEVELS / 128;
        let rect = if end - start < 1.0 {
            let column = (start as usize).min(columns - 1);
            let bit = column * PITCHES + usize::from(note.pitch);
            if covered[bit / 64] & (1 << (bit % 64)) != 0 {
                continue;
            }
            covered[bit / 64] |= 1 << (bit % 64);
            Rectangle::new(Point::new(column as f32, y), Size::new(1.0, height))
        } else {
            // Leave a one-pixel gap between adjacent long notes.
            let width = if end - start > 3.0 {
                end - start - 1.0
            } else {
                end - start
            };
            Rectangle::new(Point::new(start, y), Size::new(width, height))
        };
        out[level].push(rect);
    }
    out
}

fn velocity_colour(base: Color, level: usize) -> Color {
    Color {
        a: base.a * (0.4 + 0.6 * (level + 1) as f32 / VELOCITY_LEVELS as f32),
        ..base
    }
}

struct Tile<Renderer: geometry::Renderer> {
    zoom: (u32, u32),
    index: i64,
    used: u64,
    cache: Cache<Renderer>,
}

/// Tile caches keyed by (zoom key, tile index), least recently used first out.
pub(crate) struct TileSet<Renderer: geometry::Renderer> {
    tick: u64,
    tiles: Vec<Tile<Renderer>>,
}

impl<Renderer: geometry::Renderer> TileSet<Renderer> {
    fn new() -> Self {
        Self {
            tick: 0,
            tiles: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.tiles.clear();
    }

    /// Returns the cache for a tile and whether it already existed.
    fn get(&mut self, key: (u32, u32), index: i64) -> (&Cache<Renderer>, bool) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(position) = self
            .tiles
            .iter()
            .position(|tile| tile.zoom == key && tile.index == index)
        {
            self.tiles[position].used = tick;
            return (&self.tiles[position].cache, true);
        }
        if self.tiles.len() >= MAX_TILES
            && let Some(oldest) = self
                .tiles
                .iter()
                .enumerate()
                .min_by_key(|(_, tile)| tile.used)
                .map(|(position, _)| position)
        {
            self.tiles.swap_remove(oldest);
        }
        self.tiles.push(Tile {
            zoom: key,
            index,
            used: tick,
            cache: Cache::new(),
        });
        (&self.tiles.last().expect("just pushed").cache, false)
    }
}

struct RollState<Renderer: geometry::Renderer> {
    tiles: RefCell<TileSet<Renderer>>,
    generation: u64,
    style: AudioStyle,
    modifiers: keyboard::Modifiers,
}

/// A controlled, scrollable and zoomable piano roll.
///
/// Wheel scrolls vertically, Shift+wheel horizontally, Ctrl+wheel zooms
/// around the pointer. Each change publishes the new `RollView`; store it
/// and pass it back. A left press on a note publishes its index.
pub struct PianoRoll<'a, Message> {
    notes: &'a RollNotes,
    view: RollView,
    playhead: Option<f32>,
    on_view: Option<Box<dyn Fn(RollView) -> Message + 'a>>,
    on_note: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    width: Length,
    height: Length,
    style: AudioStyle,
}

impl<'a, Message> PianoRoll<'a, Message> {
    /// Shows `notes` through `view`.
    pub fn new(notes: &'a RollNotes, view: RollView) -> Self {
        Self {
            notes,
            view,
            playhead: None,
            on_view: None,
            on_note: None,
            width: Length::Fill,
            height: Length::Fill,
            style: AudioStyle::default(),
        }
    }

    /// Playhead position in beats.
    pub fn playhead(mut self, beat: Option<f32>) -> Self {
        self.playhead = beat;
        self
    }

    /// Enables scrolling and zooming.
    pub fn on_view(mut self, callback: impl Fn(RollView) -> Message + 'a) -> Self {
        self.on_view = Some(Box::new(callback));
        self
    }

    /// Enables note picking; publishes an index into `RollNotes::notes`.
    pub fn on_note(mut self, callback: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_note = Some(Box::new(callback));
        self
    }

    /// Width (default fill).
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Height (default fill).
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Colours; see `Tokens::audio_style`.
    pub fn style(mut self, style: AudioStyle) -> Self {
        self.style = style;
        self
    }
}

fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// Pixel spacing and beat step of visible bar lines: whole bars (4 beats),
/// doubled until lines are at least 6 px apart.
fn bar_step(pixels_per_beat: f32) -> f32 {
    let mut beats = 4.0;
    while beats * pixels_per_beat < 6.0 && beats < 1e6 {
        beats *= 2.0;
    }
    beats
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for PianoRoll<'_, Message>
where
    Renderer: geometry::Renderer + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RollState<Renderer>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(RollState::<Renderer> {
            tiles: RefCell::new(TileSet::new()),
            generation: self.notes.generation,
            style: self.style,
            modifiers: keyboard::Modifiers::empty(),
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<RollState<Renderer>>();
        if state.generation != self.notes.generation || state.style != self.style {
            state.tiles.get_mut().clear();
            state.generation = self.notes.generation;
            state.style = self.style;
        }
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
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
        let state = tree.state.downcast_mut::<RollState<Renderer>>();
        let bounds = layout.bounds();
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(on_view) = &self.on_view else {
                    return;
                };
                let Some(point) = cursor.position_in(bounds) else {
                    return;
                };
                if shell.is_event_captured() || !self.view.is_valid() {
                    return;
                }
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (x * LINE_PIXELS, y * LINE_PIXELS),
                    mouse::ScrollDelta::Pixels { x, y } => (x, y),
                };
                let next = if state.modifiers.control() {
                    self.view.zoomed(1.2f32.powf(dy / LINE_PIXELS), point.x)
                } else if state.modifiers.shift() {
                    self.view
                        .scrolled(-(dx + dy), 0.0, bounds.height, self.notes.end_beat())
                } else {
                    self.view
                        .scrolled(-dx, -dy, bounds.height, self.notes.end_beat())
                };
                if next != self.view {
                    self.view = next;
                    shell.publish(on_view(next));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(on_note) = &self.on_note
                    && !shell.is_event_captured()
                    && let Some(point) = cursor.position_in(bounds)
                {
                    if let Some(index) = self.view.note_at(self.notes, point) {
                        shell.publish(on_note(index));
                    }
                    shell.capture_event();
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if bounds.width < 1.0 || bounds.height < 1.0 {
            return;
        }
        let style = self.style;
        let view = self.view;
        if !view.is_valid() {
            quad(renderer, bounds, style.background, 0.0, None);
            return;
        }
        let state = tree.state.downcast_ref::<RollState<Renderer>>();

        // Layer 1: grid, visible rows and bar lines only.
        renderer.with_layer(bounds, |renderer| {
            quad(renderer, bounds, style.background, 0.0, None);
            let first_row = (view.scroll_y / view.row_height).floor().max(0.0) as usize;
            let last_row = (((view.scroll_y + bounds.height) / view.row_height).ceil() as usize)
                .min(PITCHES - 1);
            for row in first_row..=last_row {
                let pitch = 127 - row as u8;
                if is_black_key(pitch) {
                    quad(
                        renderer,
                        Rectangle {
                            y: bounds.y + view.y_of(pitch),
                            height: view.row_height,
                            ..bounds
                        },
                        style.lane,
                        0.0,
                        None,
                    );
                }
            }
            let step = bar_step(view.pixels_per_beat);
            let mut beat = (view.scroll_beats / step).ceil() * step;
            while view.x_of(beat) < bounds.width {
                quad(
                    renderer,
                    Rectangle {
                        x: bounds.x + view.x_of(beat).floor(),
                        width: 1.0,
                        ..bounds
                    },
                    style.grid,
                    0.0,
                    None,
                );
                beat += step;
            }
        });

        // Layer 2: cached note tiles, translated for the scroll position.
        let scroll_x = view.scroll_beats * view.pixels_per_beat;
        let first = (scroll_x / TILE_WIDTH).floor() as i64;
        let last = ((scroll_x + bounds.width) / TILE_WIDTH).floor() as i64;
        let tile_size = Size::new(TILE_WIDTH, PITCHES as f32 * view.row_height);
        let mut tiles = state.tiles.borrow_mut();
        renderer.with_layer(bounds, |renderer| {
            for index in first..=last {
                let (cache, _) = tiles.get(view.key(), index);
                let geometry = cache.draw(renderer, tile_size, |frame| {
                    let levels =
                        tile_rects(self.notes, view.pixels_per_beat, view.row_height, index);
                    for (level, rects) in levels.iter().enumerate() {
                        if rects.is_empty() {
                            continue;
                        }
                        let path = Path::new(|builder| {
                            for rect in rects {
                                builder.rectangle(rect.position(), rect.size());
                            }
                        });
                        frame.fill(&path, velocity_colour(style.note, level));
                    }
                });
                let offset = Vector::new(
                    bounds.x + index as f32 * TILE_WIDTH - scroll_x,
                    bounds.y - view.scroll_y,
                );
                renderer.with_translation(offset, |renderer| {
                    renderer.draw_geometry(geometry);
                });
            }
        });

        // Layer 3: the playhead.
        if let Some(beat) = self.playhead {
            let x = view.x_of(beat);
            if (0.0..bounds.width).contains(&x) {
                renderer.with_layer(bounds, |renderer| {
                    quad(
                        renderer,
                        Rectangle {
                            x: bounds.x + x.floor(),
                            width: 1.0,
                            ..bounds
                        },
                        style.playhead,
                        0.0,
                        None,
                    );
                });
            }
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        match cursor.position_in(layout.bounds()) {
            Some(point)
                if self.on_note.is_some() && self.view.note_at(self.notes, point).is_some() =>
            {
                mouse::Interaction::Pointer
            }
            _ => mouse::Interaction::None,
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<PianoRoll<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: geometry::Renderer + 'static,
{
    fn from(roll: PianoRoll<'a, Message>) -> Self {
        Element::new(roll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f32, length: f32, pitch: u8) -> Note {
        Note {
            start,
            length,
            pitch,
            velocity: 100,
        }
    }

    /// Deterministic dense song: `count` notes over 1024 beats.
    fn dense(count: usize) -> RollNotes {
        let mut seed = 0x2545_f491_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        RollNotes::new(
            (0..count)
                .map(|_| Note {
                    start: (next() % 1_024_000) as f32 / 1000.0,
                    length: 0.05 + (next() % 4000) as f32 / 1000.0,
                    pitch: (next() % 128) as u8,
                    velocity: (next() % 128) as u8,
                })
                .collect(),
        )
    }

    #[test]
    fn notes_are_sorted_filtered_and_measured() {
        let notes = RollNotes::new(vec![
            note(4.0, 1.0, 60),
            note(0.0, 8.0, 61),
            note(1.0, 0.0, 62),
            note(f32::NAN, 1.0, 63),
            note(2.0, 1.0, 200),
        ]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes.notes()[0].pitch, 61);
        assert_eq!(notes.end_beat(), 8.0);
    }

    #[test]
    fn visible_query_matches_brute_force_on_a_dense_song() {
        let notes = dense(131_072);
        for (from, to) in [(0.0, 4.0), (500.0, 501.5), (1020.0, 2000.0), (-10.0, 0.01)] {
            let fast: Vec<usize> = notes.visible(from, to).map(|(i, _)| i).collect();
            let slow: Vec<usize> = notes
                .notes()
                .iter()
                .enumerate()
                .filter(|(_, n)| n.start < to && n.start + n.length > from)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(fast, slow, "{from}..{to}");
        }
        // A long early note is still found late in its span.
        let long = RollNotes::new(vec![note(0.0, 100.0, 1), note(50.0, 1.0, 2)]);
        let found: Vec<u8> = long.visible(90.0, 91.0).map(|(_, n)| n.pitch).collect();
        assert_eq!(found, [1]);
    }

    #[test]
    fn view_mapping_round_trips_and_zoom_keeps_the_anchor() {
        let view = RollView {
            scroll_beats: 10.0,
            scroll_y: 80.0,
            pixels_per_beat: 20.0,
            row_height: 8.0,
        };
        assert_eq!(view.x_of(12.0), 40.0);
        assert_eq!(view.beat_at(40.0), 12.0);
        assert_eq!(view.y_of(117), 0.0);
        assert_eq!(view.pitch_at(0.0), Some(117));
        assert_eq!(view.pitch_at(7.9), Some(117));
        assert_eq!(view.pitch_at(8.0), Some(116));
        assert_eq!(view.pitch_at(-81.0), None);
        assert_eq!(view.pitch_at(128.0 * 8.0 - 80.0), None);

        let zoomed = view.zoomed(2.0, 100.0);
        assert_eq!(zoomed.pixels_per_beat, 40.0);
        assert!((zoomed.beat_at(100.0) - view.beat_at(100.0)).abs() < 1e-4);
        assert_eq!(view.zoomed(1e9, 0.0).pixels_per_beat, MAX_PIXELS_PER_BEAT);
        assert_eq!(view.zoomed(0.5, 1000.0).scroll_beats, 0.0);
    }

    #[test]
    fn scrolling_is_clamped_to_song_and_rows() {
        let view = RollView::default();
        let end = view.scrolled(1e9, 1e9, 200.0, 64.0);
        assert_eq!(end.scroll_beats, 64.0);
        assert_eq!(end.scroll_y, 128.0 * 8.0 - 200.0);
        let start = view.scrolled(-1e9, -1e9, 200.0, 64.0);
        assert_eq!((start.scroll_beats, start.scroll_y), (0.0, 0.0));
        // A viewport taller than the rows cannot scroll vertically.
        assert_eq!(view.scrolled(0.0, 50.0, 5000.0, 64.0).scroll_y, 0.0);
    }

    #[test]
    fn note_hit_testing_prefers_the_topmost_and_respects_rows() {
        let notes = RollNotes::new(vec![
            note(0.0, 4.0, 60),
            note(1.0, 1.0, 60),
            note(1.0, 1.0, 61),
        ]);
        let view = RollView {
            scroll_beats: 0.0,
            scroll_y: 0.0,
            pixels_per_beat: 10.0,
            row_height: 10.0,
        };
        let row_60 = view.y_of(60) + 5.0;
        // Stable sort: the later-listed note at the same start is drawn on top.
        assert_eq!(view.note_at(&notes, Point::new(15.0, row_60)), Some(1));
        assert_eq!(view.note_at(&notes, Point::new(35.0, row_60)), Some(0));
        assert_eq!(view.note_at(&notes, Point::new(45.0, row_60)), None);
        assert_eq!(
            view.note_at(&notes, Point::new(15.0, view.y_of(61) + 5.0)),
            Some(2)
        );
        assert_eq!(notes.notes()[1].pitch, 60);
    }

    #[test]
    fn tiles_clip_notes_and_collapse_dense_columns() {
        // A note spanning two tiles appears in both, clipped at the seam.
        let notes = RollNotes::new(vec![note(0.0, 20.0, 0)]);
        let left = tile_rects(&notes, 40.0, 10.0, 0);
        let right = tile_rects(&notes, 40.0, 10.0, 1);
        let all = |levels: &[Vec<Rectangle>; VELOCITY_LEVELS]| {
            levels.iter().flatten().copied().collect::<Vec<_>>()
        };
        assert_eq!(all(&left).len(), 1);
        assert_eq!(all(&left)[0].x, 0.0);
        assert_eq!(all(&left)[0].x + all(&left)[0].width, TILE_WIDTH - 1.0);
        assert_eq!(all(&right)[0].x, 0.0);
        assert_eq!(all(&right)[0].y, 1270.0);
        assert!(all(&tile_rects(&notes, 40.0, 10.0, 2)).is_empty());

        // 1000 short notes on one pitch within one pixel become one rect.
        let dense_column = RollNotes::new(
            (0..1000)
                .map(|i| note(i as f32 * 0.0001, 0.00005, 5))
                .collect(),
        );
        assert_eq!(all(&tile_rects(&dense_column, 1.0, 8.0, 0)).len(), 1);

        // Zoomed right out, a whole dense song costs at most a pixel per row.
        let song = dense(131_072);
        let count = all(&tile_rects(&song, MIN_PIXELS_PER_BEAT, 8.0, 0)).len();
        assert!(count <= TILE_WIDTH as usize * PITCHES, "{count}");
        assert!(count > 0);
    }

    #[test]
    fn tile_set_reuses_per_zoom_and_evicts_least_recent() {
        let mut tiles = TileSet::<()>::new();
        let zoom_a = (1, 1);
        let zoom_b = (2, 1);
        assert!(!tiles.get(zoom_a, 0).1);
        assert!(tiles.get(zoom_a, 0).1);
        assert!(!tiles.get(zoom_b, 0).1);
        for index in 1..MAX_TILES as i64 - 1 {
            tiles.get(zoom_b, index);
        }
        assert_eq!(tiles.tiles.len(), MAX_TILES);
        // zoom_a/0 was used before every zoom_b tile but zoom_b/0.
        tiles.get(zoom_a, 0);
        tiles.get(zoom_b, 999);
        assert_eq!(tiles.tiles.len(), MAX_TILES);
        assert!(tiles.get(zoom_a, 0).1);
        assert!(
            !tiles.get(zoom_b, 0).1,
            "least recently used tile was evicted"
        );
    }

    #[test]
    fn bar_lines_thin_out_when_zoomed_out() {
        assert_eq!(bar_step(24.0), 4.0);
        assert_eq!(bar_step(1.0), 8.0);
        assert_eq!(bar_step(0.25), 32.0);
        assert!(is_black_key(61) && !is_black_key(60));
        assert!(bar_step(0.0) >= 1e6);
        assert!(
            !RollView {
                pixels_per_beat: 0.0,
                ..RollView::default()
            }
            .is_valid()
        );
        assert!(RollView::default().is_valid());
    }
}
