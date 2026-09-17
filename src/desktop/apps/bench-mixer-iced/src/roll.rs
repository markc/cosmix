//! The roll view: the dense song on the shared `cosmix-iced-widgets`
//! piano-roll canvas.
//!
//! The widget owns the drawing strategy the bake-off asks about — only notes
//! overlapping the view are visited (binary search on start time), tile
//! geometry is cached per zoom level, scrolling translates cached tiles, and
//! grid, notes and playhead are separate layers. What this module owns is the
//! mapping from `cosmix_bench_feed`'s viewport, which is in song ticks and a
//! key range, onto the widget's view, which is in beats and pixel rows, so
//! both arms show the same slice of the same song.
//!
//! Two parity notes (see the crate report): the widget colours notes from one
//! `AudioStyle::note` with velocity as alpha, where the Bevy arm colours per
//! track through CTK's `channel_color`; and the playhead is left off, because
//! the Bevy arm draws none.

use cosmix_bench_feed::{BenchSong, RollViewport};
use cosmix_iced_widgets::piano_roll::{MAX_PIXELS_PER_BEAT, MIN_PIXELS_PER_BEAT};
use cosmix_iced_widgets::{Note, PianoRoll, RollNotes, RollView, Tokens};
use iced::widget::container;
use iced::{Element, Fill};

use crate::app::{Bench, Message};

/// The song as the widget's note set: ticks become beats, and the set is
/// built once per run (a new `RollNotes` value drops the widget's tiles).
pub fn notes(song: &BenchSong) -> RollNotes {
    let per_beat = song.ticks_per_beat.max(1) as f32;
    RollNotes::new(
        song.notes
            .iter()
            .map(|note| Note {
                start: note.start as f32 / per_beat,
                length: (note.length.max(1)) as f32 / per_beat,
                pitch: note.pitch,
                velocity: note.velocity,
            })
            .collect(),
    )
}

/// The widget view showing exactly `viewport` in a `width` x `height` canvas.
///
/// The widget's rows are absolute (row 0 is pitch 127), so the key range is
/// expressed as a scroll offset rather than a range: `scroll_y` puts
/// `viewport.key_hi`'s row at the top, and `row_height` makes the padded key
/// span fill the height, as `RollViewport::row_height` does for the Bevy arm.
pub fn roll_view(viewport: RollViewport, song: &BenchSong, width: f32, height: f32) -> RollView {
    let per_beat = f64::from(song.ticks_per_beat.max(1));
    let span_beats = (viewport.span / per_beat) as f32;
    let row_height = height.max(1.0) / viewport.key_rows() as f32;
    RollView {
        scroll_beats: (viewport.start / per_beat) as f32,
        scroll_y: f32::from(127u8.saturating_sub(viewport.key_hi)) * row_height,
        pixels_per_beat: (width.max(1.0) / span_beats.max(f32::MIN_POSITIVE))
            .clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT),
        row_height,
    }
}

pub fn view<'a>(bench: &'a Bench, tokens: Tokens) -> Element<'a, Message> {
    let style = tokens.audio_style();
    container(
        PianoRoll::new(&bench.notes, bench.roll)
            .width(Fill)
            .height(Fill)
            .on_view(Message::Roll)
            .style(style),
    )
    .width(Fill)
    .height(Fill)
    .style(move |_| container::Style {
        background: Some(tokens.surface.into()),
        ..container::Style::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_bench_feed::roll::roll_script;
    use cosmix_song::{Note as SongNote, Song, Track};

    /// A small multi-track song with the dense fixture's shape: 4/4 bars,
    /// several notes per beat, pitches walking per track.
    fn song() -> BenchSong {
        let mut song = Song::new("bench-roll");
        for track in 0..3usize {
            let mut t = Track::new(format!("T{track}"), track as u8);
            for step in 0..8 * 8u32 {
                let pitch = 40 + ((track as u32 * 5 + step) % 30) as u8;
                t.add_note(SongNote::new(pitch, 90, step * 240, 200));
            }
            song.add_track(t);
        }
        BenchSong::from_song(&song)
    }

    #[test]
    fn ticks_become_beats_and_every_note_survives() {
        let song = song();
        let notes = notes(&song);
        assert_eq!(notes.len(), song.notes.len());
        let per_beat = song.ticks_per_beat as f32;
        for (widget, feed) in notes.notes().iter().zip(&song.notes) {
            // Both lists are start-sorted, so they line up.
            assert!((widget.start - feed.start as f32 / per_beat).abs() < 1e-3);
            assert!((widget.length - feed.length as f32 / per_beat).abs() < 1e-3);
            assert_eq!(widget.pitch, feed.pitch);
        }
    }

    #[test]
    fn the_view_shows_exactly_the_feeds_slice() {
        let song = song();
        let (width, height) = (1024.0, 576.0);
        let viewport = RollViewport::initial(&song);
        let view = roll_view(viewport, &song, width, height);
        assert!(view.is_valid());
        let per_beat = f64::from(song.ticks_per_beat);
        // The left edge is the viewport start, and the right edge its end.
        assert!((f64::from(view.scroll_beats) - viewport.start / per_beat).abs() < 1e-3);
        let right = f64::from(view.beat_at(width));
        assert!(
            (right - viewport.end() / per_beat).abs() < 1e-3,
            "right edge {right} vs {}",
            viewport.end() / per_beat
        );
        // The top row is the highest key shown and the padded span fills the
        // height, matching RollViewport::key_y / row_height.
        assert!(view.y_of(viewport.key_hi).abs() < 1e-3);
        assert_eq!(view.pitch_at(0.0), Some(viewport.key_hi));
        assert_eq!(view.pitch_at(height - 0.5), Some(viewport.key_lo));
        assert!((view.row_height * viewport.key_rows() as f32 - height).abs() < 1e-3);
        assert!(
            (view.row_height - viewport.row_height(height)).abs() < 1e-3,
            "row height must match the feed's"
        );
    }

    #[test]
    fn every_scripted_viewport_maps_to_a_drawable_view() {
        let song = song();
        for tick in [0, 1, 300, 600, 1800, 2400, 3600] {
            let viewport = roll_script(&song, tick);
            let view = roll_view(viewport, &song, 1024.0, 576.0);
            assert!(view.is_valid(), "tick {tick}");
            assert!(view.pixels_per_beat >= MIN_PIXELS_PER_BEAT);
            assert!(view.pixels_per_beat <= MAX_PIXELS_PER_BEAT);
            assert!(view.scroll_beats >= 0.0);
        }
    }

    #[test]
    fn a_degenerate_canvas_still_yields_a_drawable_view() {
        let song = song();
        let view = roll_view(RollViewport::initial(&song), &song, 0.0, 0.0);
        assert!(
            view.is_valid(),
            "a zero-sized canvas must not produce a view the roll refuses to draw"
        );
    }
}
