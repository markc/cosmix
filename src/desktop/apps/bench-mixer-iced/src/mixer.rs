//! The mixer view: every channel strip plus the master, built from the shared
//! `cosmix-iced-widgets` controls and wrapped into rows (see
//! `cosmix_bench_feed::layout`).
//!
//! The strip skeleton is the Bevy arm's, control for control and gap for gap:
//! channel number, two-line name box, pan knob, meter beside fader taking the
//! spare height, then mute over solo. The master keeps the skeleton with the
//! number, knob and solo slots empty.
//!
//! Two parity notes, both consequences of the widget crate's surface rather
//! than choices made here (see the crate report):
//!
//! - `LevelMeter` takes one level and keeps its own time-based peak line. It
//!   has no input for the feed's `peak`, `hold` or `clipped`, so the hold
//!   marker and clip latch CTK draws are absent here. The stereo pair is two
//!   meters side by side inside the same [`METER_WIDTH`].
//! - `Fader` and `LevelMeter` map dB with `cosmix_iced_widgets::scale`, not
//!   the feed's CTK-matching `FADER_TAPER`, and neither takes a mapping. A
//!   given dB therefore sits at a slightly different height than in the Bevy
//!   arm. The quads drawn are the same in number and kind.

use cosmix_bench_feed::layout::{
    BUTTON_FONT, BUTTON_GAP, BUTTON_HEIGHT, BUTTON_MIN_WIDTH, FADER_METER_GAP, FADER_WIDTH,
    KNOB_SIZE, METER_WIDTH, NAME_BOX_HEIGHT, NAME_FONT, NUMBER_FONT, ROW_GAP, STRIP_GAP,
    STRIP_PADDING, STRIP_SECTION_GAP, STRIP_WIDTH, strip_rows,
};
use cosmix_bench_feed::{METER_CEIL_DB, METER_FLOOR_DB, MeterFrame, MixerFeed};
use cosmix_iced_widgets::{AudioStyle, Fader, Knob, LevelMeter, Toggle, Tokens};
use iced::widget::{Space, column, container, row, text};
use iced::{Center, Color, Element, Fill, Length};

use crate::app::{Bench, Message};
use crate::theme::strip_background;

/// Gap between the two meter lanes, inside [`METER_WIDTH`].
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

fn label(content: String, size: f32, colour: Color) -> Element<'static, Message> {
    text(content).size(size).color(colour).into()
}

/// The stereo meter pair, together as wide as CTK's single two-lane meter.
fn meter(frame: &MeterFrame, style: AudioStyle) -> Element<'static, Message> {
    let width = (METER_WIDTH - LANE_GAP) / 2.0;
    let lane = |index: usize| {
        LevelMeter::new(lane_db(frame.lanes[index].level))
            .width(width)
            .height(Fill)
            .style(style)
    };
    row![lane(0), lane(1)]
        .spacing(LANE_GAP)
        .height(Fill)
        .into()
}

fn strip<'a>(bench: &'a Bench, slot: usize, tokens: Tokens) -> Element<'a, Message> {
    let feed = bench.feed;
    let state = feed.strip(slot);
    let master = feed.is_master(slot);
    let style = tokens.audio_style();
    let inner_width = STRIP_WIDTH - 2.0 * STRIP_PADDING;

    let number = label(
        state.number.map_or_else(|| " ".to_owned(), |n| n.to_string()),
        NUMBER_FONT,
        tokens.muted_text,
    );
    // One word per line, centred, clipped to the fixed two-line box.
    let name = container(
        text(state.name.replace(' ', "\n"))
            .size(NAME_FONT)
            .align_x(Center)
            .color(tokens.card_text),
    )
    .width(inner_width)
    .height(NAME_BOX_HEIGHT)
    .center(Fill)
    .clip(true);

    let pan: Element<'a, Message> = if master {
        Space::new(KNOB_SIZE, KNOB_SIZE).into()
    } else {
        Knob::new(bench.pans[slot])
            .size(KNOB_SIZE)
            .on_change(move |pan| Message::Pan(slot, pan))
            .on_release(Message::Release)
            .style(style)
            .into()
    };

    let fader = Fader::new(bench.faders[slot])
        .width(FADER_WIDTH)
        .height(Fill)
        .on_change(move |db| Message::Fader(slot, db))
        .on_release(Message::Release)
        .style(style);
    let fader_row = row![meter(&bench.meters[slot], style), fader]
        .spacing(FADER_METER_GAP)
        .height(Fill);

    let mute = Toggle::new("M", bench.mutes[slot])
        .alert(true)
        .size(BUTTON_MIN_WIDTH, BUTTON_HEIGHT)
        .on_toggle(move |on| Message::Mute(slot, on))
        .style(style);
    let solo: Element<'a, Message> = if master {
        Space::new(BUTTON_MIN_WIDTH, BUTTON_HEIGHT).into()
    } else {
        Toggle::new("S", bench.solos[slot])
            .size(BUTTON_MIN_WIDTH, BUTTON_HEIGHT)
            .on_toggle(move |on| Message::Solo(slot, on))
            .style(style)
            .into()
    };
    let buttons = column![mute, solo].spacing(BUTTON_GAP).align_x(Center);

    let background = strip_background(tokens, master);
    container(
        column![number, name, pan, fader_row, buttons]
            .spacing(STRIP_SECTION_GAP)
            .align_x(Center)
            .height(Fill),
    )
    .width(STRIP_WIDTH)
    .height(Fill)
    .padding(STRIP_PADDING)
    .style(move |_| container::Style {
        background: Some(background.into()),
        ..container::Style::default()
    })
    .into()
}

/// The whole board: `strip_rows` slots per row, the master in the slot after
/// the last channel.
pub fn view<'a>(bench: &'a Bench, tokens: Tokens) -> Element<'a, Message> {
    let rows = strip_rows(bench.feed.strips()).into_iter().map(|slots| {
        row(slots.map(|slot| strip(bench, slot, tokens)))
            .spacing(STRIP_GAP)
            .height(Length::FillPortion(1))
            .into()
    });
    container(column(rows).spacing(ROW_GAP).width(Fill).height(Fill))
        .style(move |_| container::Style {
            background: Some(tokens.surface.into()),
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_bench_feed::{DEFAULT_STRIPS, meter_position};

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
    fn the_two_lanes_fill_the_shared_meter_width() {
        let width = (METER_WIDTH - LANE_GAP) / 2.0;
        assert!((2.0 * width + LANE_GAP - METER_WIDTH).abs() < 1e-6);
        assert!(width > 0.0);
    }

    #[test]
    fn the_board_wraps_into_the_same_rows_as_the_bevy_arm() {
        let rows = strip_rows(DEFAULT_STRIPS);
        assert_eq!(rows, vec![0..33, 33..65]);
        assert_eq!(rows.iter().map(|row| row.len()).sum::<usize>(), 65);
    }
}
