//! Peak meter with a decaying peak-hold line.
use std::time::{Duration, Instant};

use iced_core::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced_core::{Element, Event, Length, Rectangle, Size, window};

use crate::AudioStyle;
use crate::audio_style::quad;
use crate::scale::{FLOOR_DB, db_to_position};

/// How long the peak line holds before falling.
pub const PEAK_HOLD: Duration = Duration::from_millis(1000);
/// Fall rate of the peak line after the hold, in dB per second.
pub const PEAK_FALL_DB_PER_SEC: f32 = 20.0;
/// Redraw interval while the peak line is falling (about 30 Hz).
pub const PEAK_FRAME: Duration = Duration::from_millis(33);
/// Zone boundaries, in dB.
const HIGH_DB: f32 = -12.0;
const CLIP_DB: f32 = -3.0;

/// A vertical level meter on the shared dB scale.
///
/// The app passes the current level each view (for example at its meter
/// rate). The widget keeps a peak line that holds for `PEAK_HOLD` and then
/// falls at `PEAK_FALL_DB_PER_SEC`. It schedules redraws only while that line
/// is above the level; a steady or silent meter schedules none.
pub struct LevelMeter {
    level_db: f32,
    width: f32,
    height: Length,
    style: AudioStyle,
}

impl LevelMeter {
    /// A meter showing `level_db` (`f32::NEG_INFINITY` for silence).
    pub fn new(level_db: f32) -> Self {
        Self {
            level_db,
            width: 8.0,
            height: Length::Fixed(160.0),
            style: AudioStyle::default(),
        }
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
}

/// Peak-hold state, advanced once per redraw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PeakHold {
    peak_db: f32,
    held_at: Option<Instant>,
    updated: Option<Instant>,
}

impl Default for PeakHold {
    fn default() -> Self {
        Self {
            peak_db: FLOOR_DB,
            held_at: None,
            updated: None,
        }
    }
}

fn floor(db: f32) -> f32 {
    if db.is_nan() {
        FLOOR_DB
    } else {
        db.max(FLOOR_DB)
    }
}

impl PeakHold {
    pub(crate) fn peak_db(&self) -> f32 {
        self.peak_db
    }

    /// Folds in `level_db` at `now`. Returns when the next redraw is needed,
    /// or `None` once the line rests on the level.
    pub(crate) fn advance(&mut self, level_db: f32, now: Instant) -> Option<Instant> {
        let level = floor(level_db);
        if let (Some(held_at), Some(updated)) = (self.held_at, self.updated) {
            let fall_from = updated.max(held_at + PEAK_HOLD);
            if now > fall_from {
                let fallen = (now - fall_from).as_secs_f32() * PEAK_FALL_DB_PER_SEC;
                self.peak_db = floor(self.peak_db - fallen);
            }
        }
        if level >= self.peak_db {
            self.peak_db = level;
            self.held_at = Some(now);
        }
        self.updated = Some(now);
        if self.peak_db <= level {
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

/// Filled segments for `level_db` in `bounds`, bottom up, as
/// (rectangle, zone) with zone 0 low, 1 high, 2 clip.
pub(crate) fn segments(bounds: Rectangle, level_db: f32) -> Vec<(Rectangle, usize)> {
    let top_of = |db: f32| bounds.y + (1.0 - db_to_position(db)) * bounds.height;
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

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer> for LevelMeter {
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
            && let Some(at) = tree
                .state
                .downcast_mut::<PeakHold>()
                .advance(self.level_db, *now)
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
        for (rect, zone) in segments(bounds, self.level_db) {
            let colour = [style.meter_low, style.meter_high, style.meter_clip][zone];
            quad(renderer, rect, colour, 0.0, None);
        }
        let peak = tree.state.downcast_ref::<PeakHold>().peak_db();
        if peak > FLOOR_DB {
            let y = bounds.y + (1.0 - db_to_position(peak)) * bounds.height;
            quad(
                renderer,
                Rectangle {
                    x: bounds.x,
                    y: y.min(bounds.y + bounds.height - 2.0),
                    width: bounds.width,
                    height: 2.0,
                },
                style.peak,
                0.0,
                None,
            );
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<LevelMeter>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(meter: LevelMeter) -> Self {
        Element::new(meter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_steady_levels_schedule_nothing() {
        let now = Instant::now();
        let mut hold = PeakHold::default();
        assert_eq!(hold.advance(f32::NEG_INFINITY, now), None);
        assert_eq!(hold.advance(-10.0, now), None);
        assert_eq!(hold.advance(-10.0, now + Duration::from_secs(5)), None);
        assert_eq!(hold.peak_db(), -10.0);
    }

    #[test]
    fn peak_holds_then_falls_to_the_level_and_stops() {
        let start = Instant::now();
        let mut hold = PeakHold::default();
        assert_eq!(hold.advance(-6.0, start), None);
        // The level drops: hold until the release time.
        let t = start + Duration::from_millis(100);
        assert_eq!(hold.advance(-30.0, t), Some(start + PEAK_HOLD));
        assert_eq!(hold.peak_db(), -6.0);
        // Half a second after release it has fallen 10 dB and keeps animating.
        let t = start + PEAK_HOLD + Duration::from_millis(500);
        assert_eq!(hold.advance(-30.0, t), Some(t + PEAK_FRAME));
        assert!((hold.peak_db() + 16.0).abs() < 1e-3);
        // A later step falls only by the time since the previous step.
        let t2 = t + Duration::from_millis(500);
        hold.advance(-30.0, t2);
        assert!((hold.peak_db() + 26.0).abs() < 1e-3);
        // It rests on the level and stops scheduling.
        let t3 = t2 + Duration::from_secs(2);
        assert_eq!(hold.advance(-30.0, t3), None);
        assert_eq!(hold.peak_db(), -30.0);
        // A new peak restarts the hold.
        let t4 = t3 + Duration::from_millis(10);
        assert_eq!(hold.advance(0.0, t4), None);
        assert_eq!(hold.advance(-40.0, t4), Some(t4 + PEAK_HOLD));
    }

    #[test]
    fn segments_follow_the_zones() {
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 100.0,
        };
        assert!(segments(bounds, f32::NEG_INFINITY).is_empty());
        let low = segments(bounds, -20.0);
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].1, 0);
        assert!((low[0].0.y - 65.0).abs() < 1e-3);
        let hot = segments(bounds, 6.0);
        assert_eq!(
            hot.iter().map(|(_, zone)| *zone).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(hot[2].0.y, 0.0);
        let total: f32 = hot.iter().map(|(rect, _)| rect.height).sum();
        assert!((total - 100.0).abs() < 1e-3);
    }
}
