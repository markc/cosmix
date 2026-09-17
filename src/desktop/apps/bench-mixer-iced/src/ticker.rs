//! The feed clock, as a zero-sized widget.
//!
//! The arm must be reactive: no redraws at all when idle, and exactly one per
//! feed tick when something animates. iced has no timer subscription without
//! an async runtime (`iced::time::every` needs the `tokio` or `smol` feature,
//! whose threads would land in the wakeup measurement), so the clock is a
//! widget instead: on each `RedrawRequested` it asks the shell for the next
//! redraw at the next tick boundary, and publishes the tick when it changes.
//! That is the same mechanism `LevelMeter` uses for its peak line, and the
//! counterpart of the Bevy arm's `WinitSettings::reactive_low_power`.
//!
//! When nothing animates it schedules nothing, so the window is drawn once
//! and the process then sleeps until input arrives.

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
/// One further redraw is scheduled this long after the first, so a run whose
/// very first frame drew before fonts and layout settled is not left showing
/// it forever in a mode that never redraws again. One wake, not the Bevy
/// arm's second of continuous drawing.
pub const WARMUP: Duration = Duration::from_secs(1);

/// The instant tick `tick` starts, counted from `started`.
fn tick_start(started: Instant, tick: u64) -> Instant {
    started + Duration::from_secs_f64(tick as f64 / f64::from(TICK_HZ))
}

/// When the next redraw is wanted after a frame drawn at `elapsed` into the
/// run, and the tick that frame shows. `None` leaves the window asleep.
fn schedule(started: Instant, elapsed: Duration, animated: bool, warmed: bool) -> Option<Instant> {
    let animation = animated.then(|| tick_start(started, tick_at(elapsed) + 1) + TICK_SLACK);
    let warmup = (!warmed).then(|| started + WARMUP);
    [animation, warmup].into_iter().flatten().min()
}

/// Run clock, per widget tree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Clock {
    /// Set by the first frame; the run's time origin.
    started: Option<Instant>,
    warmed: bool,
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
        let warmed = clock.warmed;
        clock.warmed = warmed || elapsed >= WARMUP;
        if let Some(at) = schedule(started, elapsed, self.animated, warmed) {
            shell.request_redraw_at(at);
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
            schedule(started, Duration::from_secs(5), false, true),
            None,
            "an idle arm must let the window sleep"
        );
    }

    #[test]
    fn warmup_is_scheduled_once_even_when_idle() {
        let started = Instant::now();
        assert_eq!(
            schedule(started, Duration::ZERO, false, false),
            Some(started + WARMUP)
        );
        // ...and never again once it has passed.
        assert_eq!(schedule(started, WARMUP, false, true), None);
    }

    #[test]
    fn an_animated_run_wakes_just_after_each_tick_boundary() {
        let started = Instant::now();
        // 1.5 s in: tick 45 is showing, so the next wake carries tick 46.
        let elapsed = Duration::from_millis(1500);
        assert_eq!(tick_at(elapsed), 45);
        let at = schedule(started, elapsed, true, true).unwrap();
        assert_eq!(at, tick_start(started, 46) + TICK_SLACK);
        assert!(at > started + elapsed, "the wake must be in the future");
        // The slack puts the wake past the boundary, so the frame it draws
        // reads the new tick rather than the one that just ended.
        assert_eq!(tick_at(at.saturating_duration_since(started)), 46);
    }

    #[test]
    fn the_warmup_wake_wins_while_it_is_still_the_nearer_one() {
        let started = Instant::now();
        let early = schedule(started, Duration::ZERO, true, false).unwrap();
        assert_eq!(early, tick_start(started, 1) + TICK_SLACK);
        assert!(early < started + WARMUP);
        // Late in the warm-up window the tick wake is still the nearer one,
        // so scheduling never stalls waiting for warm-up.
        let late = schedule(started, Duration::from_millis(990), true, false).unwrap();
        assert!(late < started + WARMUP);
    }
}
