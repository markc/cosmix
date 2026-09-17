//! Waveform overview with a cached body and an uncached playhead layer.
use std::sync::atomic::{AtomicU64, Ordering};

use iced::advanced::graphics::geometry::{self, Cache, Path};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};

use crate::AudioStyle;
use crate::audio_style::quad;

pub(crate) fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Pre-reduced audio: one (min, max) pair per bucket, in -1..=1.
///
/// Build it once per clip; the widget redraws its cached body only when a
/// different `WaveformPeaks` value (or a new size or style) is shown.
#[derive(Debug, Clone)]
pub struct WaveformPeaks {
    min: Vec<f32>,
    max: Vec<f32>,
    generation: u64,
}

impl WaveformPeaks {
    /// Reduces `samples` to one pair per `samples_per_bucket` samples.
    pub fn from_samples(samples: &[f32], samples_per_bucket: usize) -> Self {
        Self::from_min_max(samples.chunks(samples_per_bucket.max(1)).map(|chunk| {
            chunk
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
                    (lo.min(*s), hi.max(*s))
                })
        }))
    }

    /// Uses already-reduced (min, max) pairs.
    pub fn from_min_max(pairs: impl IntoIterator<Item = (f32, f32)>) -> Self {
        let (min, max) = pairs.into_iter().unzip();
        Self {
            min,
            max,
            generation: next_generation(),
        }
    }

    /// Number of buckets.
    pub fn len(&self) -> usize {
        self.min.len()
    }

    /// True when there are no buckets.
    pub fn is_empty(&self) -> bool {
        self.min.is_empty()
    }
}

/// The (min, max) covered by pixel `column` of `columns`, or `None` if empty.
pub(crate) fn column_extent(
    peaks: &WaveformPeaks,
    column: usize,
    columns: usize,
) -> Option<(f32, f32)> {
    let len = peaks.len();
    if len == 0 || columns == 0 || column >= columns {
        return None;
    }
    let start = (column * len / columns).min(len - 1);
    let end = ((column + 1) * len / columns).clamp(start + 1, len);
    let lo = peaks.min[start..end]
        .iter()
        .copied()
        .fold(f32::INFINITY, f32::min);
    let hi = peaks.max[start..end]
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    Some((lo.clamp(-1.0, 1.0), hi.clamp(-1.0, 1.0)))
}

/// The 0..=1 position of `point` across `bounds`, if inside.
pub(crate) fn seek_fraction(bounds: Rectangle, point: Point) -> Option<f32> {
    if bounds.contains(point) {
        Some(((point.x - bounds.x) / bounds.width.max(1.0)).clamp(0.0, 1.0))
    } else {
        None
    }
}

/// A waveform overview. The body is geometry cached until the peaks, size or
/// style change; the playhead is drawn on its own layer each frame, so moving
/// it never re-tessellates the body.
pub struct Waveform<'a, Message> {
    peaks: &'a WaveformPeaks,
    playhead: Option<f32>,
    on_seek: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    width: Length,
    height: Length,
    style: AudioStyle,
}

impl<'a, Message> Waveform<'a, Message> {
    /// Shows `peaks` across the widget's width.
    pub fn new(peaks: &'a WaveformPeaks) -> Self {
        Self {
            peaks,
            playhead: None,
            on_seek: None,
            width: Length::Fill,
            height: Length::Fixed(64.0),
            style: AudioStyle::default(),
        }
    }

    /// Playhead position as a 0..=1 fraction of the clip.
    pub fn playhead(mut self, fraction: Option<f32>) -> Self {
        self.playhead = fraction;
        self
    }

    /// A left press publishes the 0..=1 position under the pointer.
    pub fn on_seek(mut self, callback: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_seek = Some(Box::new(callback));
        self
    }

    /// Width (default fill).
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Height (default 64 px).
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

struct WaveState<Renderer: geometry::Renderer> {
    body: Cache<Renderer>,
    generation: u64,
    style: Option<AudioStyle>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Waveform<'_, Message>
where
    Renderer: geometry::Renderer + 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WaveState<Renderer>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WaveState::<Renderer> {
            body: Cache::new(),
            generation: self.peaks.generation,
            style: Some(self.style),
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<WaveState<Renderer>>();
        if state.generation != self.peaks.generation || state.style != Some(self.style) {
            state.body.clear();
            state.generation = self.peaks.generation;
            state.style = Some(self.style);
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
        _tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && let Some(on_seek) = &self.on_seek
            && !shell.is_event_captured()
            && let Some(fraction) = cursor
                .position()
                .and_then(|point| seek_fraction(layout.bounds(), point))
        {
            shell.publish(on_seek(fraction));
            shell.capture_event();
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
        let state = tree.state.downcast_ref::<WaveState<Renderer>>();
        quad(renderer, bounds, style.background, 0.0, None);
        quad(
            renderer,
            Rectangle {
                y: bounds.center_y() - 0.5,
                height: 1.0,
                ..bounds
            },
            style.grid,
            0.0,
            None,
        );
        let peaks = self.peaks;
        let body = state.body.draw(renderer, bounds.size(), |frame| {
            let columns = frame.width().floor() as usize;
            let mid = frame.height() / 2.0;
            let path = Path::new(|builder| {
                for column in 0..columns {
                    if let Some((lo, hi)) = column_extent(peaks, column, columns) {
                        let top = mid - hi * mid;
                        let bottom = mid - lo * mid;
                        builder.rectangle(
                            Point::new(column as f32, top),
                            Size::new(1.0, (bottom - top).max(1.0)),
                        );
                    }
                }
            });
            frame.fill(&path, style.waveform);
        });
        renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(body);
        });
        if let Some(fraction) = self.playhead {
            let x = bounds.x + fraction.clamp(0.0, 1.0) * (bounds.width - 1.0).max(0.0);
            // Its own layer keeps the line above the body's mesh.
            renderer.with_layer(bounds, |renderer| {
                quad(
                    renderer,
                    Rectangle {
                        x,
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

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.on_seek.is_some() && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<Waveform<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: geometry::Renderer + 'static,
{
    fn from(waveform: Waveform<'a, Message>) -> Self {
        Element::new(waveform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_and_columns() {
        let samples: Vec<f32> = (0..8).map(|i| i as f32 / 4.0 - 1.0).collect();
        let peaks = WaveformPeaks::from_samples(&samples, 2);
        assert_eq!(peaks.len(), 4);
        assert_eq!(peaks.min, [-1.0, -0.5, 0.0, 0.5]);
        assert_eq!(peaks.max, [-0.75, -0.25, 0.25, 0.75]);
        // Fewer columns than buckets: each column spans two buckets.
        assert_eq!(column_extent(&peaks, 1, 2), Some((0.0, 0.75)));
        // More columns than buckets: columns repeat a bucket, never go empty.
        assert_eq!(column_extent(&peaks, 7, 8), Some((0.5, 0.75)));
        assert_eq!(column_extent(&peaks, 8, 8), None);
        assert_eq!(column_extent(&WaveformPeaks::from_min_max([]), 0, 8), None);
        // Out-of-range data is clamped.
        let loud = WaveformPeaks::from_min_max([(-3.0, 3.0)]);
        assert_eq!(column_extent(&loud, 0, 1), Some((-1.0, 1.0)));
    }

    #[test]
    fn generations_are_distinct_and_seek_hits_inside_only() {
        let a = WaveformPeaks::from_min_max([(0.0, 0.0)]);
        let b = a.clone();
        let c = WaveformPeaks::from_min_max([(0.0, 0.0)]);
        assert_eq!(a.generation, b.generation);
        assert_ne!(a.generation, c.generation);
        let bounds = Rectangle::new(Point::new(10.0, 10.0), Size::new(100.0, 20.0));
        assert_eq!(seek_fraction(bounds, Point::new(60.0, 15.0)), Some(0.5));
        assert_eq!(seek_fraction(bounds, Point::new(60.0, 40.0)), None);
    }
}
