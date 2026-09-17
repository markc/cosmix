//! Shared geometry. Both arms lay the surface out from these numbers so the
//! parity screenshots compare like with like. Sizes are logical pixels.
//!
//! The mixer is 64 compact strips plus the master, wrapped into as many rows
//! as the window width needs so every strip and meter is on screen — no
//! scrolled-off meters that one toolkit could skip drawing. One strip, top to
//! bottom: a one-line name label, the pan knob, the meter and fader side by
//! side (this block takes the spare height), then mute over solo. The master
//! keeps the same skeleton with the knob and solo slots empty.
//!
//! [`Layout::new`] resolves all of that into window-coordinate rectangles.
//! Both arms place their widgets from it, and the bake-off driver reads the
//! same rectangles (and [`Layout::fader_point`]) to aim injected input, so
//! nobody re-derives geometry from the constants.

use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Default window size: the nested harness's logical output (1105x622),
/// rounded down to a 16:9 size that fits inside it. Both arms take the same
/// size on the command line, so a run can measure at another size as long as
/// both arms use it.
pub const WINDOW_WIDTH: u32 = 1024;
pub const WINDOW_HEIGHT: u32 = 576;
/// Vertical gap between mixer rows.
pub const ROW_GAP: f32 = 4.0;
/// Horizontal gap between strips.
pub const STRIP_GAP: f32 = 1.0;

pub const STRIP_WIDTH: f32 = 44.0;
pub const STRIP_PADDING: f32 = 3.0;
/// Vertical gap between a strip's sections.
pub const STRIP_SECTION_GAP: f32 = 3.0;
pub const NAME_FONT: f32 = 9.0;
/// Height of the one-line name label.
pub const NAME_HEIGHT: f32 = 12.0;
pub const KNOB_SIZE: f32 = 26.0;
pub const FADER_WIDTH: f32 = 12.0;
pub const METER_WIDTH: f32 = 12.0;
/// Gap between the meter and the fader.
pub const FADER_METER_GAP: f32 = 4.0;
pub const BUTTON_MIN_WIDTH: f32 = 22.0;
pub const BUTTON_HEIGHT: f32 = 18.0;
pub const BUTTON_FONT: f32 = 10.0;
/// Gap between the mute and solo buttons.
pub const BUTTON_GAP: f32 = 3.0;

/// Rows padded above and below the song's key range in the roll.
pub const ROLL_KEY_MARGIN: u8 = 2;
/// Fewest key rows the roll shows.
pub const ROLL_MIN_KEY_SPAN: u8 = 24;
/// Narrowest a note is drawn, so short notes stay visible when zoomed out.
pub const ROLL_NOTE_MIN_WIDTH: f32 = 1.0;
/// Vertical inset of a note inside its key row, each side.
pub const ROLL_NOTE_INSET: f32 = 1.0;
/// Alpha of the black-key row shading over the roll background.
pub const ROLL_BLACK_KEY_ALPHA: f32 = 0.035;
/// Alpha of beat and measure gridlines.
pub const ROLL_BEAT_ALPHA: f32 = 0.07;
pub const ROLL_MEASURE_ALPHA: f32 = 0.16;

/// Shortest the fader (and the meter beside it) is ever drawn.
pub const MIN_FADER_HEIGHT: f32 = 40.0;
/// Travel inset at each end of a fader: the thumb centre never reaches the
/// widget's edge, so a pointer aiming at a travel position aims inside this
/// band. Matches CTK's 8px vertical fader padding; the iced arm builds its
/// fader to the same geometry.
pub const FADER_TRAVEL_INSET: f32 = 8.0;

/// A rectangle in logical pixels, origin top left. The mixer's rectangles are
/// in window coordinates; the roll's are in the roll canvas's own space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// The rectangle's centre.
    pub fn centre(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Whether `self` lies inside `outer` (a whole pixel of slack).
    pub fn inside(&self, outer: &Rect) -> bool {
        self.x >= outer.x - 1.0
            && self.y >= outer.y - 1.0
            && self.right() <= outer.right() + 1.0
            && self.bottom() <= outer.bottom() + 1.0
    }
}

/// One strip's widgets, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StripLayout {
    pub slot: usize,
    pub is_master: bool,
    /// The strip panel.
    pub rect: Rect,
    pub name: Rect,
    /// The pan knob; absent on the master (the space is kept).
    pub knob: Option<Rect>,
    pub meter: Rect,
    pub fader: Rect,
    pub mute: Rect,
    /// The solo toggle; absent on the master (the space is kept).
    pub solo: Option<Rect>,
}

/// The whole mixer board resolved for one window size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    /// Channel strips, not counting the master.
    pub strips: usize,
    pub strips_per_row: usize,
    pub rows: usize,
    pub strip_height: f32,
    pub fader_height: f32,
    /// One entry per slot: `0..strips` are channel strips, `strips` is master.
    pub slots: Vec<StripLayout>,
}

impl Layout {
    /// Resolve the board for a `size` window with `strips` channel strips.
    pub fn new(size: (f32, f32), strips: usize) -> Self {
        let (width, height) = size;
        let per_row = strips_per_row(width);
        let rows = strip_rows(strips, width);
        let row_count = rows.len().max(1);
        let strip_height =
            ((height - ROW_GAP * (row_count - 1) as f32) / row_count as f32).max(0.0);
        // The fader block absorbs whatever the fixed sections leave.
        let fixed = 2.0 * STRIP_PADDING
            + NAME_HEIGHT
            + KNOB_SIZE
            + 2.0 * BUTTON_HEIGHT
            + BUTTON_GAP
            + 3.0 * STRIP_SECTION_GAP;
        let fader_height = (strip_height - fixed).max(MIN_FADER_HEIGHT);
        // Centre the board horizontally; a partial last row starts at the same
        // left edge as the full rows above it.
        let board_width = per_row as f32 * (STRIP_WIDTH + STRIP_GAP) - STRIP_GAP;
        let left = ((width - board_width) / 2.0).max(0.0);

        let inner = STRIP_WIDTH - 2.0 * STRIP_PADDING;
        let block_width = METER_WIDTH + FADER_METER_GAP + FADER_WIDTH;
        let slots = rows
            .iter()
            .enumerate()
            .flat_map(|(row, range)| {
                range.clone().enumerate().map(move |(column, slot)| {
                    let rect = Rect {
                        x: left + column as f32 * (STRIP_WIDTH + STRIP_GAP),
                        y: row as f32 * (strip_height + ROW_GAP),
                        w: STRIP_WIDTH,
                        h: strip_height,
                    };
                    let centred = |w: f32, y: f32, h: f32| Rect {
                        x: rect.x + STRIP_PADDING + (inner - w) / 2.0,
                        y,
                        w,
                        h,
                    };
                    let is_master = slot == strips;
                    let mut y = rect.y + STRIP_PADDING;
                    let name = centred(inner, y, NAME_HEIGHT);
                    y += NAME_HEIGHT + STRIP_SECTION_GAP;
                    let knob = centred(KNOB_SIZE, y, KNOB_SIZE);
                    y += KNOB_SIZE + STRIP_SECTION_GAP;
                    let block_left = rect.x + STRIP_PADDING + (inner - block_width) / 2.0;
                    let meter = Rect {
                        x: block_left,
                        y,
                        w: METER_WIDTH,
                        h: fader_height,
                    };
                    let fader = Rect {
                        x: block_left + METER_WIDTH + FADER_METER_GAP,
                        y,
                        w: FADER_WIDTH,
                        h: fader_height,
                    };
                    y += fader_height + STRIP_SECTION_GAP;
                    let mute = centred(BUTTON_MIN_WIDTH, y, BUTTON_HEIGHT);
                    y += BUTTON_HEIGHT + BUTTON_GAP;
                    let solo = centred(BUTTON_MIN_WIDTH, y, BUTTON_HEIGHT);
                    StripLayout {
                        slot,
                        is_master,
                        rect,
                        name,
                        knob: (!is_master).then_some(knob),
                        meter,
                        fader,
                        mute,
                        solo: (!is_master).then_some(solo),
                    }
                })
            })
            .collect();

        Self {
            width,
            height,
            strips,
            strips_per_row: per_row,
            rows: row_count,
            strip_height,
            fader_height,
            slots,
        }
    }

    /// The window rectangle.
    pub fn window(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: self.width,
            h: self.height,
        }
    }

    pub fn slot(&self, slot: usize) -> &StripLayout {
        &self.slots[slot]
    }

    pub fn master(&self) -> &StripLayout {
        &self.slots[self.strips]
    }

    /// The point a pointer aims at to put `slot`'s fader at travel
    /// `position` (0 bottom, 1 top) — the drag target for the bake-off driver
    /// and the same geometry both arms draw.
    pub fn fader_point(&self, slot: usize, position: f32) -> (f32, f32) {
        let fader = self.slots[slot].fader;
        let travel = (fader.h - 2.0 * FADER_TRAVEL_INSET).max(0.0);
        let x = fader.x + fader.w / 2.0;
        let y = fader.bottom() - FADER_TRAVEL_INSET - position.clamp(0.0, 1.0) * travel;
        (x, y)
    }
}

/// Whether MIDI key `key` is a black key.
pub fn is_black_key(key: u8) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

/// Strip slots that fit one row of a `width`-pixel board.
pub fn strips_per_row(width: f32) -> usize {
    ((width / (STRIP_WIDTH + STRIP_GAP)).floor() as usize).max(1)
}

/// The strip slots in each mixer row of a `width`-pixel board. Slots
/// `0..strips` are channel strips; slot `strips` is the master. Every slot is
/// on screen: the board wraps into as many rows as it needs.
pub fn strip_rows(strips: usize, width: f32) -> Vec<Range<usize>> {
    let slots = strips + 1;
    let per_row = strips_per_row(width);
    (0..slots)
        .step_by(per_row)
        .map(|start| start..(start + per_row).min(slots))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_board_wraps_into_rows_that_fit() {
        let width = WINDOW_WIDTH as f32;
        let rows = strip_rows(crate::DEFAULT_STRIPS, width);
        assert_eq!(rows, vec![0..22, 22..44, 44..65]);
        let widest = rows.iter().map(|row| row.len()).max().unwrap() as f32;
        assert!(widest * (STRIP_WIDTH + STRIP_GAP) <= width);
        // A wider board uses fewer, longer rows.
        assert_eq!(
            strip_rows(crate::DEFAULT_STRIPS, 1600.0),
            vec![0..35, 35..65]
        );
    }

    #[test]
    fn rows_cover_every_slot_once() {
        for width in [80.0, 1024.0, 1600.0] {
            for strips in [0, 1, 32, 33, 64, 100] {
                let slots: Vec<usize> = strip_rows(strips, width).into_iter().flatten().collect();
                assert_eq!(slots, (0..=strips).collect::<Vec<_>>(), "width {width}");
            }
        }
        // A board too narrow for even one strip still shows one per row.
        assert_eq!(strips_per_row(10.0), 1);
    }

    #[test]
    fn default_board_fits_the_default_window() {
        let layout = Layout::new((WINDOW_WIDTH as f32, WINDOW_HEIGHT as f32), 64);
        assert_eq!((layout.rows, layout.strips_per_row), (3, 22));
        assert_eq!(layout.slots.len(), 65);
        let window = layout.window();
        for strip in &layout.slots {
            assert!(
                strip.rect.inside(&window),
                "strip {} off screen",
                strip.slot
            );
            for rect in [
                Some(strip.name),
                strip.knob,
                Some(strip.meter),
                Some(strip.fader),
                Some(strip.mute),
                strip.solo,
            ]
            .into_iter()
            .flatten()
            {
                assert!(
                    rect.inside(&strip.rect),
                    "slot {} widget {rect:?}",
                    strip.slot
                );
            }
        }
        assert!(layout.fader_height >= MIN_FADER_HEIGHT);
        // The master is the last slot and drops its knob and solo.
        let master = layout.master();
        assert!(master.is_master && master.knob.is_none() && master.solo.is_none());
        assert_eq!(master.slot, 64);
    }

    #[test]
    fn wide_window_reproduces_the_two_row_board() {
        let layout = Layout::new((1600.0, 900.0), 64);
        assert_eq!((layout.rows, layout.strips_per_row), (2, 35));
        assert_eq!(layout.strip_height, (900.0 - ROW_GAP) / 2.0);
        // The old board's fader filled the strip's spare height.
        assert!(layout.fader_height > 300.0);
        for strip in &layout.slots {
            assert!(strip.rect.inside(&layout.window()));
        }
    }

    #[test]
    fn fader_point_walks_the_travel_inside_the_fader() {
        let layout = Layout::new((WINDOW_WIDTH as f32, WINDOW_HEIGHT as f32), 64);
        let fader = layout.slot(7).fader;
        let (x_bottom, y_bottom) = layout.fader_point(7, 0.0);
        let (x_top, y_top) = layout.fader_point(7, 1.0);
        assert_eq!(x_bottom, fader.centre().0);
        assert_eq!(x_top, x_bottom);
        assert_eq!(y_bottom, fader.bottom() - FADER_TRAVEL_INSET);
        assert_eq!(y_top, fader.y + FADER_TRAVEL_INSET);
        let mid = layout.fader_point(7, 0.5).1;
        assert!(y_top < mid && mid < y_bottom);
        // Out-of-range positions clamp to the ends.
        assert_eq!(layout.fader_point(7, 5.0).1, y_top);
        assert_eq!(layout.fader_point(7, -5.0).1, y_bottom);
    }

    #[test]
    fn small_windows_keep_a_usable_fader() {
        let layout = Layout::new((400.0, 300.0), 64);
        assert_eq!(layout.fader_height, MIN_FADER_HEIGHT);
        assert_eq!(layout.slots.len(), 65);
        // A strip may now be taller than its row; the rows still start where
        // the row pitch says, which is what both arms draw.
        assert!(layout.rows > 3);
    }

    #[test]
    fn black_keys_follow_the_octave() {
        let black: Vec<u8> = (60..72).filter(|key| is_black_key(*key)).collect();
        assert_eq!(black, vec![61, 63, 66, 68, 70]);
    }
}
