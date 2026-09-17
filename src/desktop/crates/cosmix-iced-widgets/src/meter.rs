//! Peak meter with a decaying peak-hold line, plus host-supplied peak, hold
//! and clip state.
use std::time::{Duration, Instant};

use iced_core::widget::{Tree, tree};
use iced_core::{
    Clipboard, Element, Event, Layout, Length, Rectangle, Shell, Size, Widget, layout, mouse,
    renderer, window,
};

use crate::AudioStyle;
use crate::audio_style::quad;
use crate::scale::Taper;

/// How long the meter's own peak line holds before falling.
pub const PEAK_HOLD: Duration = Duration::from_millis(1000);
/// Fall rate of that line after the hold, in dB per second.
pub const PEAK_FALL_DB_PER_SEC: f32 = 20.0;
/// Redraw interval while it is falling (about 30 Hz).
pub const PEAK_FRAME: Duration = Duration::from_millis(33);
/// Zone boundaries, in dB. Their positions follow the taper in use.
pub const HIGH_DB: f32 = -12.0;
pub const CLIP_DB: f32 = -3.0;
/// Height of a marker line and of the clip latch block, in logical pixels.
const MARKER_HEIGHT: f32 = 2.0;
const CLIP_HEIGHT: f32 = 4.0;

/// A vertical level meter on the shared gain taper.
///
/// The app passes the current level each view (for example at its meter
/// rate). The widget keeps its own peak line that holds for `PEAK_HOLD` and
/// then falls at `PEAK_FALL_DB_PER_SEC`; it schedules redraws only while that
/// line is above the level, so a steady or silent meter schedules none. A
/// host that tracks its own peak, hold and clip state (as a mixer engine
/// does) passes them in and they are drawn as well.
pub struct LevelMeter<'a> {
    level_db: f32,
    peak_db: Option<f32>,
    hold_db: Option<f32>,
    clipped: bool,
    width: f32,
    height: Length,
    style: AudioStyle,
    taper: Taper<'a>,
}

impl<'a> LevelMeter<'a> {
    /// A meter showing `level_db` (`f32::NEG_INFINITY` for silence).
    pub fn new(level_db: f32) -> Self {
        Self {
            level_db,
            peak_db: None,
            hold_db: None,
            clipped: false,
            width: 8.0,
            height: Length::Fixed(160.0),
            style: AudioStyle::default(),
            taper: Taper::DEFAULT,
        }
    }

    /// The host's peak for this block, drawn as a line in the peak colour.
    pub fn peak(mut self, peak_db: Option<f32>) -> Self {
        self.peak_db = peak_db;
        self
    }

    /// The host's hold marker (a peak held for longer), drawn in the text
    /// colour so it reads apart from the peak line.
    pub fn hold(mut self, hold_db: Option<f32>) -> Self {
        self.hold_db = hold_db;
        self
    }

    /// The host's clip latch: fills a block at the top in the clip colour
    /// until the host clears it.
    pub fn clipped(mut self, clipped: bool) -> Self {
        self.clipped = clipped;
        self
    }

    /// Width in logical pixels (default 8).
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Height (default 160 px). Match the neighbouring `Fader`'s height and
    /// the scales line up.
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Colours; see `Tokens::audio_style`.
    pub fn style(mut self, style: AudioStyle) -> Self {
        self.style = style;
        self
    }

    /// The gain taper; give the neighbouring `Fader` the same one.
    pub fn taper(mut self, taper: Taper<'a>) -> Self {
        self.taper = taper;
        self
    }
}

/// Peak-hold state, advanced once per redraw.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct PeakHold {
    peak_db: Option<f32>,
    held_at: Option<Instant>,
    updated: Option<Instant>,
}

impl PeakHold {
    /// The line's gain, or `None` while it rests on the floor.
    pub(crate) fn peak_db(&self) -> Option<f32> {
        self.peak_db
    }

    /// Folds in `level_db` at `now`, on a scale whose floor is `floor_db`.
    /// Returns when the next redraw is needed, or `None` once the line rests
    /// on the level.
    pub(crate) fn advance(
        &mut self,
        level_db: f32,
        floor_db: f32,
        now: Instant,
    ) -> Option<Instant> {
        let floor = |db: f32| {
            if db.is_nan() {
                floor_db
            } else {
                db.max(floor_db)
            }
        };
        let level = floor(level_db);
        let mut peak = self.peak_db.unwrap_or(floor_db);
        if let (Some(held_at), Some(updated)) = (self.held_at, self.updated) {
            let fall_from = updated.max(held_at + PEAK_HOLD);
            if now > fall_from {
                let fallen = (now - fall_from).as_secs_f32() * PEAK_FALL_DB_PER_SEC;
                peak = floor(peak - fallen);
            }
        }
        if level >= peak {
            peak = level;
            self.held_at = Some(now);
        }
        self.updated = Some(now);
        self.peak_db = (peak > floor_db).then_some(peak);
        if peak <= level {
            return None;
        }
        let release = self.held_at.map_or(now, |at| at + PEAK_HOLD);
        Some(if now < release {
            release
        } else {
            now + PEAK_FRAME
        })
    }
}

/// What a marker line means, so a test can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Marker {
    /// The meter's own falling peak line.
    Falling,
    /// The host's peak for this block.
    Peak,
    /// The host's hold marker.
    Hold,
    /// The host's clip latch.
    Clip,
}

/// Filled segments for `level_db` in `bounds`, bottom up, as
/// (rectangle, zone) with zone 0 low, 1 high, 2 clip.
pub(crate) fn segments(
    bounds: Rectangle,
    taper: Taper<'_>,
    level_db: f32,
) -> Vec<(Rectangle, usize)> {
    let top_of = |db: f32| bounds.y + (1.0 - taper.position(db)) * bounds.height;
    let level_y = top_of(level_db);
    let bottom = bounds.y + bounds.height;
    let mut out = Vec::new();
    for (zone, (from, to)) in [
        (bottom, top_of(HIGH_DB)),
        (top_of(HIGH_DB), top_of(CLIP_DB)),
        (top_of(CLIP_DB), bounds.y),
    ]
    .into_iter()
    .enumerate()
    {
        let top = to.max(level_y);
        if top < from {
            out.push((
                Rectangle {
                    x: bounds.x,
                    y: top,
                    width: bounds.width,
                    height: from - top,
                },
                zone,
            ));
        }
    }
    out
}

/// Marker rectangles, bottom-most first, for the lines drawn over the bar.
pub(crate) fn markers(
    bounds: Rectangle,
    taper: Taper<'_>,
    falling_db: Option<f32>,
    peak_db: Option<f32>,
    hold_db: Option<f32>,
    clipped: bool,
) -> Vec<(Rectangle, Marker)> {
    let line = |db: f32, kind| {
        let y = bounds.y + (1.0 - taper.position(db)) * bounds.height;
        (
            Rectangle {
                x: bounds.x,
                y: y.min(bounds.y + bounds.height - MARKER_HEIGHT)
                    .max(bounds.y),
                width: bounds.width,
                height: MARKER_HEIGHT,
            },
            kind,
        )
    };
    let mut out = Vec::new();
    // A marker at or below the floor is not a reading, so it is not drawn.
    for (db, kind) in [
        (falling_db, Marker::Falling),
        (peak_db, Marker::Peak),
        (hold_db, Marker::Hold),
    ] {
        if let Some(db) = db.filter(|db| *db > taper.floor_db()) {
            out.push(line(db, kind));
        }
    }
    if clipped {
        out.push((
            Rectangle {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width,
                height: CLIP_HEIGHT.min(bounds.height),
            },
            Marker::Clip,
        ));
    }
    out
}

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for LevelMeter<'_>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PeakHold>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PeakHold::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.width), self.height)
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
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event
            && let Some(at) = tree.state.downcast_mut::<PeakHold>().advance(
                self.level_db,
                self.taper.floor_db(),
                *now,
            )
        {
            shell.request_redraw_at(at);
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
        let style = self.style;
        quad(renderer, bounds, style.track, style.radius.min(2.0), None);
        for (rect, zone) in segments(bounds, self.taper, self.level_db) {
            let colour = [style.meter_low, style.meter_high, style.meter_clip][zone];
            quad(renderer, rect, colour, 0.0, None);
        }
        let falling = tree.state.downcast_ref::<PeakHold>().peak_db();
        for (rect, kind) in markers(
            bounds,
            self.taper,
            falling,
            self.peak_db,
            self.hold_db,
            self.clipped,
        ) {
            let colour = match kind {
                Marker::Falling | Marker::Peak => style.peak,
                Marker::Hold => style.text,
                Marker::Clip => style.meter_clip,
            };
            quad(renderer, rect, colour, 0.0, None);
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<LevelMeter<'a>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(meter: LevelMeter<'a>) -> Self {
        Element::new(meter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::FLOOR_DB;

    fn bounds() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 100.0,
        }
    }

    #[test]
    fn silence_and_steady_levels_schedule_nothing() {
        let now = Instant::now();
        let mut hold = PeakHold::default();
        assert_eq!(hold.advance(f32::NEG_INFINITY, FLOOR_DB, now), None);
        assert_eq!(hold.peak_db(), None);
        assert_eq!(hold.advance(-10.0, FLOOR_DB, now), None);
        assert_eq!(
            hold.advance(-10.0, FLOOR_DB, now + Duration::from_secs(5)),
            None
        );
        assert_eq!(hold.peak_db(), Some(-10.0));
    }

    #[test]
    fn peak_holds_then_falls_to_the_level_and_stops() {
        let start = Instant::now();
        let mut hold = PeakHold::default();
        assert_eq!(hold.advance(-6.0, FLOOR_DB, start), None);
        let t = start + Duration::from_millis(100);
        assert_eq!(hold.advance(-30.0, FLOOR_DB, t), Some(start + PEAK_HOLD));
        assert_eq!(hold.peak_db(), Some(-6.0));
        let t = start + PEAK_HOLD + Duration::from_millis(500);
        assert_eq!(hold.advance(-30.0, FLOOR_DB, t), Some(t + PEAK_FRAME));
        assert!((hold.peak_db().unwrap() + 16.0).abs() < 1e-3);
        let t2 = t + Duration::from_millis(500);
        hold.advance(-30.0, FLOOR_DB, t2);
        assert!((hold.peak_db().unwrap() + 26.0).abs() < 1e-3);
        let t3 = t2 + Duration::from_secs(2);
        assert_eq!(hold.advance(-30.0, FLOOR_DB, t3), None);
        assert_eq!(hold.peak_db(), Some(-30.0));
        let t4 = t3 + Duration::from_millis(10);
        assert_eq!(hold.advance(0.0, FLOOR_DB, t4), None);
        assert_eq!(hold.advance(-40.0, FLOOR_DB, t4), Some(t4 + PEAK_HOLD));
    }

    #[test]
    fn a_host_floor_replaces_the_scale_floor() {
        let now = Instant::now();
        let mut hold = PeakHold::default();
        // On a -80 dB scale, -70 dB is a reading, not silence.
        assert_eq!(hold.advance(-70.0, -80.0, now), None);
        assert_eq!(hold.peak_db(), Some(-70.0));
        // On the default -60 dB scale it is the floor, so no line is drawn.
        let mut hold = PeakHold::default();
        assert_eq!(hold.advance(-70.0, FLOOR_DB, now), None);
        assert_eq!(hold.peak_db(), None);
    }

    #[test]
    fn segments_follow_the_zones_and_the_taper() {
        let taper = Taper::DEFAULT;
        assert!(segments(bounds(), taper, f32::NEG_INFINITY).is_empty());
        let low = segments(bounds(), taper, -20.0);
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].1, 0);
        assert!((low[0].0.y - 65.0).abs() < 1e-3);
        let hot = segments(bounds(), taper, 6.0);
        assert_eq!(
            hot.iter().map(|(_, zone)| *zone).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(hot[2].0.y, 0.0);
        let total: f32 = hot.iter().map(|(rect, _)| rect.height).sum();
        assert!((total - 100.0).abs() < 1e-3);
        // A host taper moves the zone boundaries with it.
        let points = [(0.0, -80.0), (1.0, 0.0)];
        let linear = Taper::new(&points);
        // -20 dB is three quarters up this scale, against a third on the default.
        let low = segments(bounds(), linear, -20.0);
        assert!((low[0].0.y - 25.0).abs() < 1e-3, "{:?}", low[0].0);
        assert!((low[0].0.height - 75.0).abs() < 1e-3);
    }

    #[test]
    fn markers_draw_only_real_readings() {
        let taper = Taper::DEFAULT;
        assert!(markers(bounds(), taper, None, None, None, false).is_empty());
        // Below the floor is not a reading.
        assert!(
            markers(bounds(), taper, Some(-60.0), Some(-90.0), None, false).is_empty(),
            "floor readings must not draw"
        );
        let all = markers(bounds(), taper, Some(-20.0), Some(-6.0), Some(0.0), true);
        assert_eq!(
            all.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
            [Marker::Falling, Marker::Peak, Marker::Hold, Marker::Clip]
        );
        // Higher gain sits higher up the meter; the clip latch caps it.
        assert!(all[0].0.y > all[1].0.y && all[1].0.y > all[2].0.y);
        assert_eq!(all[3].0.y, 0.0);
        assert_eq!(all[3].0.height, CLIP_HEIGHT);
        // A marker at the very top stays inside the bar.
        let top = markers(bounds(), taper, None, Some(12.0), None, false);
        assert_eq!(top[0].0.y, 0.0);
        // A host taper moves them too.
        let points = [(0.0, -80.0), (1.0, 0.0)];
        let linear = Taper::new(&points);
        let moved = markers(bounds(), linear, None, Some(-40.0), None, false);
        assert!((moved[0].0.y - 50.0).abs() < 1e-3);
    }
}
