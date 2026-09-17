//! The mixer view: every channel strip plus the master, built from the shared
//! `cosmix-iced-widgets` controls and placed at the rectangles
//! `cosmix_bench_feed::layout::Layout` resolves — the same numbers the Bevy
//! arm places CTK's controls at, and the same ones the bake-off driver aims
//! its injected drags at.
//!
//! Two parity deltas here, both consequences of the widget crate's surface
//! rather than choices made in this arm (the full list ships as
//! `known-deltas.conf.mix`):
//!
//! - `LevelMeter` takes one level and keeps its own time-based peak line. It
//!   has no input for the feed's `peak`, `hold` or `clipped`, so the hold
//!   marker and clip latch CTK draws are absent. The stereo pair is two
//!   meters side by side inside the layout's meter rectangle.
//! - `Fader` and `LevelMeter` map dB with `cosmix_iced_widgets::scale`, not
//!   the feed's CTK-matching `FADER_TAPER`, and neither takes a mapping, so a
//!   given dB sits at a slightly different height than in the Bevy arm. The
//!   quads drawn are the same in number and kind. [`lane_db`] is the whole
//!   adapter: when the widgets take a mapping, it goes.

use cosmix_bench_feed::layout::{Layout, NAME_FONT, Rect, StripLayout};
use cosmix_bench_feed::{METER_CEIL_DB, METER_FLOOR_DB};
use cosmix_iced_widgets::{AudioStyle, Fader, Knob, LevelMeter, Toggle, Tokens};
use iced::widget::{container, text};
use iced::{Center, Color, Element, Fill};

use crate::app::{Bench, Message};
use crate::board::Board;
use crate::theme::strip_background;

/// Gap between the two meter lanes, inside the layout's meter rectangle.
const LANE_GAP: f32 = 1.0;

/// The dB a meter lane's normalised level stands for. `MeterLane::level` is
/// already a position on the feed's meter scale, so this inverts
/// `meter_position`; a silent lane lands on the floor and draws nothing.
pub fn lane_db(level: f32) -> f32 {
    if level <= 0.0 {
        f32::NEG_INFINITY
    } else {
        METER_FLOOR_DB + level.min(1.0) * (METER_CEIL_DB - METER_FLOOR_DB)
    }
}

/// The two lane rectangles inside the layout's meter block.
fn lanes(meter: Rect) -> [Rect; 2] {
    let w = (meter.w - LANE_GAP) / 2.0;
    [
        Rect { w, ..meter },
        Rect {
            x: meter.x + w + LANE_GAP,
            w,
            ..meter
        },
    ]
}

fn fill(colour: Color) -> Element<'static, Message> {
    container(iced::widget::Space::new())
        .width(Fill)
        .height(Fill)
        .style(move |_| container::Style {
            background: Some(colour.into()),
            ..container::Style::default()
        })
        .into()
}

/// One strip's widgets, appended to `board` in back-to-front order.
fn strip<'a>(
    mut board: Board<'a, Message, iced::Theme, iced::Renderer>,
    bench: &'a Bench,
    place: &StripLayout,
    tokens: Tokens,
    style: AudioStyle,
) -> Board<'a, Message, iced::Theme, iced::Renderer> {
    let slot = place.slot;
    board = board.push(place.rect, fill(strip_background(tokens, place.is_master)));

    // One line, centred, clipped: the feed's name already carries the channel
    // number ("Kick 12").
    board = board.push(
        place.name,
        container(
            text(&bench.names[slot])
                .size(NAME_FONT)
                .align_x(Center)
                .wrapping(text::Wrapping::None)
                .color(tokens.card_text),
        )
        .center(Fill)
        .clip(true),
    );

    if let Some(knob) = place.knob {
        board = board.push(
            knob,
            Knob::new(bench.pans[slot])
                .size(knob.w)
                .on_change(move |pan| Message::Pan(slot, pan))
                .on_release(Message::Release)
                .style(style),
        );
    }

    for (lane, rect) in lanes(place.meter).into_iter().enumerate() {
        let level = lane_db(bench.meters[slot].lanes[lane].level);
        board = board.push(
            rect,
            LevelMeter::new(level)
                .width(rect.w)
                .height(rect.h)
                .style(style),
        );
    }

    board = board.push(
        place.fader,
        Fader::new(bench.faders[slot])
            .width(place.fader.w)
            .height(place.fader.h)
            .on_change(move |db| Message::Fader(slot, db))
            .on_release(Message::Release)
            .style(style),
    );

    board = board.push(
        place.mute,
        Toggle::new("M", bench.mutes[slot])
            .alert(true)
            .size(place.mute.w, place.mute.h)
            .on_toggle(move |on| Message::Mute(slot, on))
            .style(style),
    );
    if let Some(solo) = place.solo {
        board = board.push(
            solo,
            Toggle::new("S", bench.solos[slot])
                .size(solo.w, solo.h)
                .on_toggle(move |on| Message::Solo(slot, on))
                .style(style),
        );
    }
    board
}

/// The whole board at the resolved layout.
pub fn view<'a>(bench: &'a Bench, layout: &Layout, tokens: Tokens) -> Element<'a, Message> {
    let style = tokens.audio_style();
    let mut board = Board::new(layout.width, layout.height)
        .push(layout.window(), fill(tokens.surface));
    for place in &layout.slots {
        board = strip(board, bench, place, tokens, style);
    }
    board.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_bench_feed::meter_position;

    #[test]
    fn lane_db_inverts_the_feeds_meter_scale() {
        for db in [-60.0, -40.0, -12.0, 0.0, 6.0] {
            let level = meter_position(db);
            if level > 0.0 {
                assert!((lane_db(level) - db).abs() < 1e-3, "{db} dB");
            }
        }
        assert_eq!(lane_db(0.0), f32::NEG_INFINITY);
        assert_eq!(lane_db(meter_position(-90.0)), f32::NEG_INFINITY);
        assert!((lane_db(1.0) - METER_CEIL_DB).abs() < 1e-3);
        // Out-of-range input is clamped, never extrapolated off the scale.
        assert!((lane_db(2.0) - METER_CEIL_DB).abs() < 1e-3);
    }

    #[test]
    fn the_two_lanes_fill_the_layouts_meter_rectangle() {
        let meter = Rect {
            x: 10.0,
            y: 20.0,
            w: 12.0,
            h: 90.0,
        };
        let [left, right] = lanes(meter);
        assert_eq!(left.x, meter.x);
        assert_eq!(right.right(), meter.right());
        assert!((right.x - left.right() - LANE_GAP).abs() < 1e-6);
        for lane in [left, right] {
            assert!(lane.w > 0.0);
            assert_eq!((lane.y, lane.h), (meter.y, meter.h));
            assert!(lane.inside(&meter), "a lane must stay in the meter block");
        }
    }

    #[test]
    fn every_widget_is_inside_its_strip_at_both_sizes() {
        for size in [(1024.0, 576.0), (1600.0, 900.0)] {
            let layout = Layout::new(size, 64);
            assert_eq!(layout.slots.len(), 65);
            for place in &layout.slots {
                assert!(place.rect.inside(&layout.window()), "{:?}", place.slot);
                for rect in [Some(place.name), place.knob, Some(place.mute), place.solo]
                    .into_iter()
                    .flatten()
                    .chain(lanes(place.meter))
                    .chain([place.fader])
                {
                    assert!(
                        rect.inside(&place.rect),
                        "slot {} rect {rect:?} escapes {:?} at {size:?}",
                        place.slot,
                        place.rect
                    );
                }
            }
            assert!(layout.master().knob.is_none() && layout.master().solo.is_none());
        }
    }
}
