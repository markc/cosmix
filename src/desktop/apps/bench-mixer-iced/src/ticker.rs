//! The feed clock, as a zero-sized widget.
//!
//! The arm must be reactive: no redraws at all when idle, and exactly one per
//! feed tick when something animates. iced has no timer subscription without
//! an async runtime (`iced::time::every` needs the `tokio` or `smol` feature,
//! whose threads would land in the wakeup measurement), so the clock is a
//! widget instead: on each `RedrawRequested` it asks the shell for the next
//! redraw and publishes the tick when it changes. That is the same mechanism
//! `LevelMeter` uses for its peak line, and the counterpart of the Bevy arm's
//! `WinitSettings` modes.
//!
//! The schedule matches the Bevy arm's exactly: continuous for [`WARMUP`],
//! then one wake per feed tick in an animated mode, and none at all in an
//! idle one — the window is drawn and the process sleeps until input arrives.

use std::time::{Duration, Instant};

use cosmix_bench_feed::{TICK_HZ, tick_at};
use iced_core::widget::{Tree, tree};
use iced_core::{
    Clipboard, Element, Event, Layout, Length, Rectangle, Shell, Size, Widget, layout, mouse,
    renderer, window,
};

/// Wake this long after a tick boundary, so the woken frame sees the new tick
/// (the Bevy arm's `TICK_SLACK`).
pub const TICK_SLACK: Duration = Duration::from_micros(500);
/// The run redraws continuously for this long before the reactive schedule
/// takes over, so fonts, layout and the first tile pass settle the same way
/// they do in the Bevy arm (whose `WARMUP` is the same second). Measurement
/// starts after it.
pub const WARMUP: Duration = Duration::from_secs(1);

/// What the next redraw should be after a frame drawn at `elapsed` into the
/// run. `Wait` leaves the window asleep until input arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Still warming up: draw again as soon as the compositor will take it.
    Frame,
    At(Instant),
    Wait,
}

/// The instant tick `tick` starts, counted from `started`.
fn tick_start(started: Instant, tick: u64) -> Instant {
    started + Duration::from_secs_f64(tick as f64 / f64::from(TICK_HZ))
}

/// When the next redraw is wanted after a frame drawn at `elapsed` into the
/// run. Continuous while warming up; then one wake per feed tick in an
/// animated mode, and nothing at all in an idle one.
fn schedule(started: Instant, elapsed: Duration, animated: bool) -> Next {
    if elapsed < WARMUP {
        return Next::Frame;
    }
    if animated {
        Next::At(tick_start(started, tick_at(elapsed) + 1) + TICK_SLACK)
    } else {
        Next::Wait
    }
}

/// Run clock, per widget tree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Clock {
    /// Set by the first frame; the run's time origin.
    started: Option<Instant>,
}

/// Publishes the feed tick and schedules the redraw that carries the next one.
pub struct Ticker<'a, Message> {
    current: u64,
    animated: bool,
    on_tick: Box<dyn Fn(u64) -> Message + 'a>,
}

impl<'a, Message> Ticker<'a, Message> {
    /// `current` is the tick the app is showing; `animated` is
    /// [`Mode::is_animated`](cosmix_bench_feed::Mode::is_animated).
    pub fn new(current: u64, animated: bool, on_tick: impl Fn(u64) -> Message + 'a) -> Self {
        Self {
            current,
            animated,
            on_tick: Box::new(on_tick),
        }
    }
}

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for Ticker<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Clock>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Clock::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(0.0), Length::Fixed(0.0))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, 0.0, 0.0)
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
        let Event::Window(window::Event::RedrawRequested(now)) = event else {
            return;
        };
        let clock = tree.state.downcast_mut::<Clock>();
        let started = *clock.started.get_or_insert(*now);
        let elapsed = now.saturating_duration_since(started);
        match schedule(started, elapsed, self.animated) {
            Next::Frame => shell.request_redraw(),
            Next::At(at) => shell.request_redraw_at(at),
            Next::Wait => {}
        }
        let tick = tick_at(elapsed);
        if tick != self.current {
            shell.publish((self.on_tick)(tick));
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<Ticker<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(ticker: Ticker<'a, Message>) -> Self {
        Element::new(ticker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_warmed_run_schedules_nothing() {
        let started = Instant::now();
        assert_eq!(
            schedule(started, Duration::from_secs(5), false),
            Next::Wait,
            "an idle arm must let the window sleep"
        );
    }

    #[test]
    fn warmup_is_continuous_in_every_mode_and_ends_on_time() {
        let started = Instant::now();
        for animated in [false, true] {
            for elapsed in [Duration::ZERO, Duration::from_millis(500), WARMUP - TICK_SLACK] {
                assert_eq!(
                    schedule(started, elapsed, animated),
                    Next::Frame,
                    "animated {animated} at {elapsed:?}"
                );
            }
        }
        // The instant it closes, the reactive schedule takes over.
        assert_eq!(schedule(started, WARMUP, false), Next::Wait);
        assert!(matches!(schedule(started, WARMUP, true), Next::At(_)));
    }

    #[test]
    fn an_animated_run_wakes_just_after_each_tick_boundary() {
        let started = Instant::now();
        // 1.5 s in: tick 45 is showing, so the next wake carries tick 46.
        let elapsed = Duration::from_millis(1500);
        assert_eq!(tick_at(elapsed), 45);
        let Next::At(at) = schedule(started, elapsed, true) else {
            panic!("an animated run past warm-up must schedule a wake");
        };
        assert_eq!(at, tick_start(started, 46) + TICK_SLACK);
        assert!(at > started + elapsed, "the wake must be in the future");
        // The slack puts the wake past the boundary, so the frame it draws
        // reads the new tick rather than the one that just ended.
        assert_eq!(tick_at(at.saturating_duration_since(started)), 46);
    }

    #[test]
    fn an_animated_run_wakes_once_per_tick_and_no_more() {
        let started = Instant::now();
        // Walk the wakes the way the widget does and count the ticks they
        // land on: one wake per tick, strictly increasing, no repeats.
        let mut elapsed = WARMUP;
        let mut ticks = Vec::new();
        for _ in 0..90 {
            let Next::At(at) = schedule(started, elapsed, true) else {
                panic!("expected a scheduled wake");
            };
            elapsed = at.saturating_duration_since(started);
            ticks.push(tick_at(elapsed));
        }
        assert!(ticks.windows(2).all(|w| w[1] == w[0] + 1), "{ticks:?}");
        // 90 ticks at 30 Hz is three seconds of run time, not more.
        let span = elapsed.saturating_sub(WARMUP).as_secs_f64();
        assert!((span - 3.0).abs() < 0.01, "90 wakes spanned {span} s");
    }
}
