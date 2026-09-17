//! Shared geometry. Both arms lay the surface out from these numbers so the
//! parity screenshots compare like with like. Sizes are logical pixels.
//!
//! The mixer is 64 compact strips plus the master, wrapped into rows of
//! [`STRIPS_PER_ROW`] so every strip and meter is on screen at the fixed
//! window size (no scrolled-off meters that one toolkit could skip drawing).
//! One strip, top to bottom: channel number, a two-line name box, the pan
//! knob, the meter and fader side by side (this row takes the spare height),
//! then mute over solo. The master strip keeps the same skeleton with the
//! number, knob and solo slots left empty.

use std::ops::Range;

/// Fixed window size for every run.
pub const WINDOW_WIDTH: u32 = 1600;
pub const WINDOW_HEIGHT: u32 = 900;

/// Strip slots per mixer row (the master takes the slot after the last strip).
pub const STRIPS_PER_ROW: usize = 33;
/// Vertical gap between mixer rows.
pub const ROW_GAP: f32 = 4.0;
/// Horizontal gap between strips.
pub const STRIP_GAP: f32 = 1.0;

pub const STRIP_WIDTH: f32 = 44.0;
pub const STRIP_PADDING: f32 = 3.0;
/// Vertical gap between a strip's sections.
pub const STRIP_SECTION_GAP: f32 = 5.0;
pub const NUMBER_FONT: f32 = 10.0;
pub const NAME_FONT: f32 = 10.0;
/// Height of the two-line name box.
pub const NAME_BOX_HEIGHT: f32 = NAME_FONT * 2.0 + 4.0;
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

/// Whether MIDI key `key` is a black key.
pub fn is_black_key(key: u8) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

/// The strip slots in each mixer row. Slots `0..strips` are channel strips;
/// slot `strips` is the master.
pub fn strip_rows(strips: usize) -> Vec<Range<usize>> {
    let slots = strips + 1;
    (0..slots)
        .step_by(STRIPS_PER_ROW)
        .map(|start| start..(start + STRIPS_PER_ROW).min(slots))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_board_fits_the_window_in_two_rows() {
        let rows = strip_rows(crate::DEFAULT_STRIPS);
        assert_eq!(rows, vec![0..33, 33..65]);
        let widest = rows.iter().map(|row| row.len()).max().unwrap() as f32;
        assert!(widest * (STRIP_WIDTH + STRIP_GAP) <= WINDOW_WIDTH as f32);
    }

    #[test]
    fn rows_cover_every_slot_once() {
        for strips in [0, 1, 32, 33, 64, 100] {
            let slots: Vec<usize> = strip_rows(strips).into_iter().flatten().collect();
            assert_eq!(slots, (0..=strips).collect::<Vec<_>>());
        }
    }

    #[test]
    fn black_keys_follow_the_octave() {
        let black: Vec<u8> = (60..72).filter(|key| is_black_key(*key)).collect();
        assert_eq!(black, vec![61, 63, 66, 68, 70]);
    }
}
