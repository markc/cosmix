//! The piano-roll viewport: which slice of the song is on screen, how input
//! moves it, where things land in pixels, and the scripted roll-mode path.
//!
//! The roll always shows the song's whole padded key range vertically;
//! horizontally it shows `span` ticks starting at `start`.

use std::f64::consts::TAU;

use crate::layout::{ROLL_NOTE_INSET, ROLL_NOTE_MIN_WIDTH};
use crate::song::{BenchNote, BenchSong};

/// Narrowest zoom, in measures.
pub const ROLL_MIN_SPAN_MEASURES: f64 = 1.0;
/// Widest zoom, in measures (also capped at the song length).
pub const ROLL_MAX_SPAN_MEASURES: f64 = 32.0;
/// Span a run opens at, in measures.
pub const ROLL_INITIAL_SPAN_MEASURES: f64 = 4.0;
/// Most notes an arm draws at once. The dense song has 512 notes per
/// measure, so the widest zoom shows about 16,400; the cap only matters for
/// denser songs, and both arms drop the same notes.
pub const ROLL_MAX_VISIBLE_NOTES: usize = 20_000;
/// Most gridlines drawn: past this many beats only measures are drawn, past
/// this many measures none are.
pub const ROLL_MAX_GRIDLINES: usize = 400;
/// Scripted zoom: one wide-and-back cycle.
pub const ROLL_SCRIPT_ZOOM_TICKS: u64 = 600;
/// Scripted scroll: one start-to-end sweep (then back).
pub const ROLL_SCRIPT_SWEEP_TICKS: u64 = 1800;
/// Zoom factor of one wheel line (zooming out; its inverse zooms in).
pub const ROLL_WHEEL_ZOOM: f64 = 1.25;
/// Fraction of the visible span one wheel line scrolls.
pub const ROLL_WHEEL_SCROLL: f64 = 0.1;

/// The visible slice of the song.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RollViewport {
    /// First visible tick.
    pub start: f64,
    /// Visible ticks.
    pub span: f64,
    pub key_lo: u8,
    pub key_hi: u8,
}

/// One vertical gridline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridLine {
    pub tick: u32,
    pub measure: bool,
}

/// A rectangle in the roll's own pixel space (origin top left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RollViewport {
    pub fn initial(song: &BenchSong) -> Self {
        Self {
            start: 0.0,
            span: f64::from(song.ticks_per_measure()) * ROLL_INITIAL_SPAN_MEASURES,
            key_lo: song.key_lo,
            key_hi: song.key_hi,
        }
        .clamped(song)
    }

    pub fn end(&self) -> f64 {
        self.start + self.span
    }

    /// Narrowest and widest span for `song`, ticks.
    pub fn span_limits(song: &BenchSong) -> (f64, f64) {
        let measure = f64::from(song.ticks_per_measure());
        let min = measure * ROLL_MIN_SPAN_MEASURES;
        let max = (measure * ROLL_MAX_SPAN_MEASURES)
            .min(f64::from(song.length_ticks))
            .max(min);
        (min, max)
    }

    /// Clamp the span to the zoom limits and the start into the song.
    pub fn clamped(mut self, song: &BenchSong) -> Self {
        let (min, max) = Self::span_limits(song);
        self.span = self.span.clamp(min, max);
        let last_start = (f64::from(song.length_ticks) - self.span).max(0.0);
        self.start = self.start.clamp(0.0, last_start);
        self
    }

    /// Scroll by `fraction` of the visible span (negative scrolls back).
    pub fn scrolled(mut self, song: &BenchSong, fraction: f64) -> Self {
        self.start += fraction * self.span;
        self.clamped(song)
    }

    /// Zoom by `factor` (above 1 shows more), keeping the tick under
    /// `anchor` (0 left edge, 1 right edge) in place where the limits allow.
    pub fn zoomed(mut self, song: &BenchSong, anchor: f64, factor: f64) -> Self {
        let anchor = anchor.clamp(0.0, 1.0);
        let pinned = self.start + anchor * self.span;
        let (min, max) = Self::span_limits(song);
        self.span = (self.span * factor).clamp(min, max);
        self.start = pinned - anchor * self.span;
        self.clamped(song)
    }

    pub fn key_rows(&self) -> u32 {
        u32::from(self.key_hi - self.key_lo) + 1
    }

    /// Pixels per key row in a roll `height` pixels tall.
    pub fn row_height(&self, height: f32) -> f32 {
        height / self.key_rows() as f32
    }

    /// X of `tick` in a roll `width` pixels wide.
    pub fn tick_x(&self, tick: f64, width: f32) -> f32 {
        ((tick - self.start) / self.span) as f32 * width
    }

    /// Top of `key`'s row in a roll `height` pixels tall.
    pub fn key_y(&self, key: u8, height: f32) -> f32 {
        f32::from(self.key_hi.saturating_sub(key)) * self.row_height(height)
    }

    /// Where `note` is drawn. Not clipped: a note starting before the view has
    /// a negative `x`, and the arm clips to the roll.
    pub fn note_rect(&self, note: &BenchNote, width: f32, height: f32) -> Rect {
        let x = self.tick_x(f64::from(note.start), width);
        let w = (self.tick_x(f64::from(note.end()), width) - x).max(ROLL_NOTE_MIN_WIDTH);
        let row = self.row_height(height);
        Rect {
            x,
            y: self.key_y(note.pitch, height) + ROLL_NOTE_INSET,
            w,
            h: (row - 2.0 * ROLL_NOTE_INSET).max(1.0),
        }
    }

    /// The black-key rows, top to bottom, as `(key, y, height)`.
    pub fn black_key_rows(&self, height: f32) -> impl Iterator<Item = (u8, f32, f32)> + '_ {
        let row = self.row_height(height);
        (self.key_lo..=self.key_hi)
            .rev()
            .filter(|key| crate::layout::is_black_key(*key))
            .map(move |key| (key, self.key_y(key, height), row))
    }

    /// Visible notes of `song` (indices into `song.notes`), capped at
    /// [`ROLL_MAX_VISIBLE_NOTES`]; returns how many were left out.
    pub fn visible_notes_into(&self, song: &BenchSong, out: &mut Vec<u32>) -> usize {
        song.visible_into(
            self.start,
            self.end(),
            self.key_lo,
            self.key_hi,
            ROLL_MAX_VISIBLE_NOTES,
            out,
        )
    }

    /// Gridlines strictly inside the view, left to right.
    pub fn gridlines_into(&self, song: &BenchSong, out: &mut Vec<GridLine>) {
        out.clear();
        let beat = f64::from(song.ticks_per_beat);
        let per_measure = song.beats_per_measure.max(1);
        let first = (self.start / beat).floor() as u64 + 1;
        let last = (self.end() / beat).ceil() as u64;
        let beats = last.saturating_sub(first) as usize;
        let measures_only = beats > ROLL_MAX_GRIDLINES;
        if measures_only && beats / per_measure as usize > ROLL_MAX_GRIDLINES {
            return;
        }
        for index in first..last {
            let measure = index % u64::from(per_measure) == 0;
            if measures_only && !measure {
                continue;
            }
            out.push(GridLine {
                tick: (index as f64 * beat) as u32,
                measure,
            });
        }
    }
}

/// The roll-mode viewport at `tick`: the span breathes between the zoom
/// limits every [`ROLL_SCRIPT_ZOOM_TICKS`] while the start sweeps the song
/// and back every `2 ×` [`ROLL_SCRIPT_SWEEP_TICKS`].
pub fn roll_script(song: &BenchSong, tick: u64) -> RollViewport {
    let (min, max) = RollViewport::span_limits(song);
    let zoom_phase = (tick % ROLL_SCRIPT_ZOOM_TICKS) as f64 / ROLL_SCRIPT_ZOOM_TICKS as f64;
    let zoom = (1.0 - (TAU * zoom_phase).cos()) / 2.0;
    let span = min * (max / min).powf(zoom);
    let sweep = (tick % (2 * ROLL_SCRIPT_SWEEP_TICKS)) as f64 / ROLL_SCRIPT_SWEEP_TICKS as f64;
    let sweep = if sweep > 1.0 { 2.0 - sweep } else { sweep };
    let start = sweep * (f64::from(song.length_ticks) - span).max(0.0);
    RollViewport {
        start,
        span,
        key_lo: song.key_lo,
        key_hi: song.key_hi,
    }
    .clamped(song)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::tests::small_song;

    fn song() -> BenchSong {
        BenchSong::from_song(&small_song(4, 64))
    }

    #[test]
    fn initial_view_opens_at_the_start() {
        let song = song();
        let view = RollViewport::initial(&song);
        assert_eq!(view.start, 0.0);
        assert_eq!(view.span, 4.0 * 1920.0);
        assert_eq!((view.key_lo, view.key_hi), (song.key_lo, song.key_hi));
    }

    #[test]
    fn zoom_and_scroll_stay_inside_the_song() {
        let song = song();
        let (min, max) = RollViewport::span_limits(&song);
        assert_eq!((min, max), (1920.0, 32.0 * 1920.0));
        let mut view = RollViewport::initial(&song);
        for _ in 0..50 {
            view = view.zoomed(&song, 0.3, ROLL_WHEEL_ZOOM);
        }
        assert_eq!(view.span, max);
        for _ in 0..500 {
            view = view.scrolled(&song, 0.5);
        }
        assert_eq!(view.end(), f64::from(song.length_ticks));
        for _ in 0..50 {
            view = view.zoomed(&song, 1.0, 1.0 / ROLL_WHEEL_ZOOM);
        }
        assert_eq!(view.span, min);
        assert!((view.end() - f64::from(song.length_ticks)).abs() < 1e-6);
        view = view.scrolled(&song, -1e9);
        assert_eq!(view.start, 0.0);
    }

    #[test]
    fn zoom_keeps_the_anchor_tick_in_place() {
        let song = song();
        let view = RollViewport {
            start: 20_000.0,
            ..RollViewport::initial(&song)
        };
        let anchor = 0.25;
        let pinned = view.start + anchor * view.span;
        let zoomed = view.zoomed(&song, anchor, 2.0);
        assert_eq!(zoomed.span, view.span * 2.0);
        assert!((zoomed.start + anchor * zoomed.span - pinned).abs() < 1e-9);
    }

    #[test]
    fn short_songs_cap_the_widest_zoom() {
        let song = BenchSong::from_song(&small_song(1, 3));
        assert_eq!(RollViewport::span_limits(&song), (1920.0, 3.0 * 1920.0));
        let script = roll_script(&song, ROLL_SCRIPT_ZOOM_TICKS / 2);
        assert_eq!(script.span, 3.0 * 1920.0);
        assert_eq!(script.start, 0.0);
    }

    #[test]
    fn note_geometry() {
        let song = song();
        let view = RollViewport {
            start: 1920.0,
            span: 1920.0,
            key_lo: 40,
            key_hi: 49,
        };
        let note = BenchNote {
            track: 0,
            start: 1920 + 480,
            length: 480,
            pitch: 47,
            velocity: 100,
        };
        let rect = view.note_rect(&note, 1000.0, 100.0);
        assert_eq!(rect, Rect { x: 250.0, y: 21.0, w: 250.0, h: 8.0 });
        let tiny = BenchNote { length: 1, ..note };
        assert_eq!(view.note_rect(&tiny, 1000.0, 100.0).w, ROLL_NOTE_MIN_WIDTH);
        let rows: Vec<u8> = view.black_key_rows(100.0).map(|(key, _, _)| key).collect();
        assert_eq!(rows, vec![49, 46, 44, 42]);
        let mut visible = Vec::new();
        assert_eq!(view.visible_notes_into(&song, &mut visible), 0);
        assert!(visible.iter().all(|i| {
            let n = song.notes[*i as usize];
            n.end() > 1920 && n.start < 3840 && (40..=49).contains(&n.pitch)
        }));
        assert!(!visible.is_empty());
    }

    #[test]
    fn gridlines_mark_measures_and_thin_out() {
        let song = song();
        let view = RollViewport {
            start: 1000.0,
            span: 1920.0,
            ..RollViewport::initial(&song)
        };
        let mut lines = Vec::new();
        view.gridlines_into(&song, &mut lines);
        let ticks: Vec<(u32, bool)> = lines.iter().map(|l| (l.tick, l.measure)).collect();
        assert_eq!(
            ticks,
            vec![(1440, false), (1920, true), (2400, false), (2880, false)]
        );
        // 32 measures = 128 beats: every beat still drawn.
        let wide = view.zoomed(&song, 0.0, 1e9);
        wide.gridlines_into(&song, &mut lines);
        assert_eq!(lines.len(), 128);
        let huge = RollViewport {
            start: 0.0,
            span: 1920.0 * 200.0,
            ..view
        };
        huge.gridlines_into(&song, &mut lines);
        assert!(lines.iter().all(|l| l.measure));
        assert_eq!(lines.len(), 199);
    }

    #[test]
    fn script_is_deterministic_and_bounded() {
        let song = song();
        let (min, max) = RollViewport::span_limits(&song);
        let first = roll_script(&song, 0);
        assert_eq!((first.start, first.span), (0.0, min));
        let widest = roll_script(&song, ROLL_SCRIPT_ZOOM_TICKS / 2);
        assert!((widest.span - max).abs() < 1e-6);
        let mut changed = 0;
        let mut last = first;
        for tick in 0..(4 * ROLL_SCRIPT_SWEEP_TICKS) {
            let view = roll_script(&song, tick);
            assert_eq!(view, roll_script(&song, tick));
            assert!(view.span >= min - 1e-9 && view.span <= max + 1e-9);
            assert!(view.start >= 0.0);
            assert!(view.end() <= f64::from(song.length_ticks) + 1e-6);
            changed += usize::from(view != last);
            last = view;
        }
        assert!(changed as u64 > 4 * ROLL_SCRIPT_SWEEP_TICKS - 10);
        // The sweep reaches the end of the song.
        let end = roll_script(&song, ROLL_SCRIPT_SWEEP_TICKS);
        assert!((end.end() - f64::from(song.length_ticks)).abs() < 1e-6);
    }
}
