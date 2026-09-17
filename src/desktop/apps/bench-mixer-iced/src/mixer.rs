//! The mixer view: every channel strip plus the master, built from the shared
//! `cosmix-iced-widgets` controls and placed at the rectangles
//! `cosmix_bench_feed::layout::Layout` resolves — the same numbers the Bevy
//! arm places CTK's controls at, and the same ones the bake-off driver aims
//! its injected drags at.
//!
//! Both arms draw the same heights: the widgets take the feed's own scales
//! (`cosmix-iced-widgets` slice 4 added `Taper`), so the fader uses
//! [`FADER_TAPER`] — which mirrors CTK's `default_fader_mapping` — and the
//! meters use the feed's linear meter scale, with `peak`, `hold` and
//! `clipped` handed over from the feed rather than re-derived from a timer.

use cosmix_bench_feed::layout::{Layout, NAME_FONT, Rect, StripLayout};
use cosmix_bench_feed::{FADER_TAPER, METER_CEIL_DB, METER_FLOOR_DB, MeterLane};
use cosmix_iced_widgets::scale::Taper;
use cosmix_iced_widgets::{AudioStyle, Fader, Knob, LevelMeter, Toggle, Tokens};
use iced::widget::{container, text};
use iced::{Center, Color, Element, Fill};

use crate::app::{Bench, Message};
use crate::board::Board;
use crate::theme::strip_background;

/// Gap between the two meter lanes, inside the layout's meter rectangle.
const LANE_GAP: f32 = 1.0;

/// The feed's meter scale as a taper: linear from the floor to the ceiling,
/// the scale `meter_position` is the position function of.
const METER_POINTS: [(f32, f32); 2] = [(0.0, METER_FLOOR_DB), (1.0, METER_CEIL_DB)];

/// The fader's gain scale, shared with the Bevy arm through the feed.
pub fn fader_taper() -> Taper<'static> {
    Taper::new(&FADER_TAPER)
}

/// The meter's scale, shared with the Bevy arm through the feed.
pub fn meter_taper() -> Taper<'static> {
    Taper::new(&METER_POINTS)
}

/// The dB a meter lane's normalised level stands for. The feed publishes
/// meter values as positions on its own scale, so this inverts
/// `meter_position`; a silent lane lands on the floor and draws nothing.
pub fn lane_db(level: f32) -> f32 {
    if level <= 0.0 {
        f32::NEG_INFINITY
    } else {
        METER_FLOOR_DB + level.min(1.0) * (METER_CEIL_DB - METER_FLOOR_DB)
    }
}

/// A marker the feed is not showing (a level at or below the floor) is drawn
/// by not drawing it, rather than as a line pinned to the bottom.
fn marker_db(position: f32) -> Option<f32> {
    (position > 0.0).then(|| lane_db(position))
}

/// One lane of the stereo pair, fed entirely from the feed's reading.
fn meter_lane(lane: MeterLane, rect: Rect, style: AudioStyle) -> LevelMeter<'static> {
    LevelMeter::new(lane_db(lane.level))
        .peak(marker_db(lane.peak))
        .hold(marker_db(lane.hold))
        .clipped(lane.clipped)
        .taper(meter_taper())
        .width(rect.w)
        .height(rect.h)
        .style(style)
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
        board = board.push(rect, meter_lane(bench.meters[slot].lanes[lane], rect, style));
    }

    board = board.push(
        place.fader,
        Fader::new(bench.faders[slot])
            .taper(fader_taper())
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
    use cosmix_bench_feed::{FADER_MAX_DB, FADER_MIN_DB, fader_position, meter_position};

    /// The load-bearing parity check: a dB value must sit at the same travel
    /// position in both arms. The Bevy arm pins CTK's mapping to the feed's
    /// `fader_position`; this pins the iced widget's taper to the same
    /// function, so the two boards draw the same thumb heights.
    #[test]
    fn the_fader_taper_is_the_feeds_taper() {
        let taper = fader_taper();
        assert_eq!(taper.points(), &FADER_TAPER);
        assert_eq!(taper.floor_db(), FADER_MIN_DB);
        assert_eq!(taper.max_db(), FADER_MAX_DB);
        for step in 0..=252 {
            let db = FADER_MIN_DB + step as f32 * 0.5;
            let widget = taper.position(db);
            let feed = fader_position(db);
            assert!((widget - feed).abs() < 1e-5, "{db} dB: {widget} vs {feed}");
        }
    }

    /// Same for the meters: a level the feed publishes as a position must be
    /// drawn at that position after the round trip through dB.
    #[test]
    fn the_meter_taper_is_the_feeds_meter_scale() {
        let taper = meter_taper();
        assert_eq!(taper.floor_db(), METER_FLOOR_DB);
        assert_eq!(taper.max_db(), METER_CEIL_DB);
        for step in 0..=100 {
            let level = step as f32 / 100.0;
            let drawn = taper.position(lane_db(level));
            assert!((drawn - level).abs() < 1e-5, "level {level} drawn {drawn}");
        }
        for db in [-60.0, -40.0, -12.0, -3.0, 0.0, 6.0] {
            let drawn = taper.position(db);
            assert!((drawn - meter_position(db)).abs() < 1e-5, "{db} dB");
        }
    }

    #[test]
    fn markers_below_the_floor_are_absent_rather_than_pinned_low() {
        assert_eq!(marker_db(0.0), None);
        assert_eq!(marker_db(meter_position(-90.0)), None);
        assert!(marker_db(0.5).is_some());
        // A silent lane draws no level, no peak, no hold and no clip.
        let silent = MeterLane::default();
        assert_eq!(lane_db(silent.level), f32::NEG_INFINITY);
        assert_eq!(marker_db(silent.peak), None);
        assert_eq!(marker_db(silent.hold), None);
        assert!(!silent.clipped);
    }

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
