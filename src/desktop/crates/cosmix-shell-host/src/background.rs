//! Native layer-shell policy and callback pacing for animated backgrounds.
//!
//! Backgrounds never reserve workspace or acquire pointer/keyboard input.

use smithay_client_toolkit::{
    compositor::{CompositorState, Region},
    error::GlobalError,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, LayerSurface},
    },
};
use std::time::{Duration, Instant};

/// Apply to a role created with Layer::Background before its first commit.
pub fn configure_background(
    layer: &LayerSurface,
    compositor: &CompositorState,
) -> Result<(), GlobalError> {
    // An empty region is click-through; None would restore the default region.
    let empty = Region::new(compositor)?;
    // The creation request sets Background, including on layer-shell v1.
    // set_layer is only available since v2 and is unnecessary here.
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, 0);
    layer.set_margin(0, 0, 0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.wl_surface().set_input_region(Some(empty.wl_region()));
    layer.commit();
    Ok(())
}

/// A timer may enforce a maximum rate, but cannot bypass an outstanding
/// compositor callback. This lets an occluded surface stop consuming frames.
#[derive(Debug)]
pub struct BackgroundPacer {
    interval: Duration,
    due: Instant,
    generation: u64,
    pending: Option<u64>,
    paused: bool,
    resume_phase: bool,
}
impl BackgroundPacer {
    pub fn new(now: Instant, fps: u32) -> Result<Self, &'static str> {
        if !(1..=60).contains(&fps) {
            return Err("background fps must be between 1 and 60");
        }
        Ok(Self {
            interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            due: now,
            generation: 0,
            pending: None,
            paused: false,
            resume_phase: false,
        })
    }
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused && !paused {
            self.resume_phase = true;
        }
        self.paused = paused;
    }
    pub fn set_rate(&mut self, now: Instant, fps: u32) -> Result<(), &'static str> {
        if !(1..=60).contains(&fps) {
            return Err("background fps must be between 1 and 60");
        }
        self.interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        self.due = now + self.interval;
        self.resume_phase = false;
        Ok(())
    }
    /// Returns a fresh callback token when both rate and compositor permit it.
    pub fn begin_frame(&mut self, now: Instant) -> Option<u64> {
        if self.paused || self.pending.is_some() || now < self.due {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.pending = Some(self.generation);
        // Keep the timer phase across ordinary scheduler lateness. Rebasing
        // to "now" each frame accumulates jitter against the display cadence.
        // Skip expired slots in constant time; never replay missed frames.
        self.due = if self.resume_phase {
            self.resume_phase = false;
            now + self.interval
        } else {
            let remainder = now.duration_since(self.due).as_nanos() % self.interval.as_nanos();
            now + (self.interval - Duration::from_nanos(remainder as u64))
        };
        Some(self.generation)
    }
    pub fn frame_done(&mut self, generation: u64) -> bool {
        if self.pending != Some(generation) {
            return false;
        }
        self.pending = None;
        true
    }
    /// Only a renderer-confirmed absence of buffer submission permits retry
    /// without a compositor callback. Keep the existing rate deadline.
    pub(crate) fn frame_not_submitted(&mut self, generation: u64) -> bool {
        self.frame_done(generation)
    }
    pub fn deadline(&self) -> Option<Instant> {
        (!self.paused && self.pending.is_none() && self.generation != u64::MAX).then_some(self.due)
    }
    /// A recreated surface must not accept a callback from the previous role.
    pub fn recreate(&mut self, now: Instant) {
        self.generation = self.generation.saturating_add(1);
        self.pending = None;
        self.due = now;
        self.resume_phase = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduler_lateness_does_not_accumulate_into_the_next_deadline() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let interval = p.interval;
        for index in 0..120 {
            let target = now + interval * index;
            let actual = target + Duration::from_millis(2);
            let token = p.begin_frame(actual).unwrap();
            assert!(p.frame_done(token));
            assert_eq!(p.deadline(), Some(target + interval));
            assert!(p.begin_frame(actual).is_none());
        }
    }

    #[test]
    fn delayed_callbacks_skip_slots_and_pause_resumes_with_a_fresh_phase() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let token = p.begin_frame(now).unwrap();
        let resumed = now + Duration::from_secs(10);
        assert!(p.begin_frame(resumed).is_none());
        assert!(p.frame_done(token));
        let token = p.begin_frame(resumed).unwrap();
        assert!(p.frame_done(token));
        assert!(p.deadline().unwrap() > resumed);
        assert!(p.deadline().unwrap() <= resumed + p.interval);
        assert!(p.begin_frame(resumed).is_none());
        p.set_paused(true);
        p.set_paused(false);
        let later = resumed + Duration::from_secs(10);
        let token = p.begin_frame(later).unwrap();
        assert!(p.frame_done(token));
        assert_eq!(p.deadline(), Some(later + p.interval));
    }
    #[test]
    fn changing_rate_preserves_the_pending_callback_and_pause() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let token = p.begin_frame(now).unwrap();
        p.set_rate(now, 1).unwrap();
        assert!(p.begin_frame(now + Duration::from_secs(2)).is_none());
        assert!(p.frame_done(token));
        assert!(p.begin_frame(now + Duration::from_millis(500)).is_none());
        assert!(p.begin_frame(now + Duration::from_secs(1)).is_some());
        p.set_paused(true);
        assert!(p.set_rate(now, 0).is_err());
        p.set_rate(now, 60).unwrap();
        assert!(p.deadline().is_none());
    }

    #[test]
    fn timer_cannot_bypass_occlusion_callback() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let token = p.begin_frame(now).unwrap();
        assert!(p.begin_frame(now + Duration::from_secs(100)).is_none());
        assert!(p.deadline().is_none());
        assert!(p.frame_done(token));
        assert!(p.begin_frame(now + Duration::from_secs(100)).is_some());
    }
    #[test]
    fn limits_rate_and_rejects_previous_surface_callback() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let old = p.begin_frame(now).unwrap();
        p.frame_done(old);
        assert!(p.begin_frame(now + Duration::from_millis(5)).is_none());
        p.recreate(now);
        let new = p.begin_frame(now).unwrap();
        assert_ne!(old, new);
        assert!(!p.frame_done(old));
        assert!(p.frame_done(new));
    }
    #[test]
    fn pause_has_no_timer_and_exhaustion_cannot_reuse_a_token() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        p.set_paused(true);
        assert!(p.deadline().is_none());
        assert!(p.begin_frame(now).is_none());
        p.set_paused(false);
        assert!(p.begin_frame(now).is_some());
        p.generation = u64::MAX;
        p.pending = None;
        assert!(p.begin_frame(now + Duration::from_secs(1)).is_none());
        assert!(p.deadline().is_none());
    }
    #[test]
    fn failed_submission_retries_at_rate_and_retires_old_callback() {
        let now = Instant::now();
        let mut p = BackgroundPacer::new(now, 30).unwrap();
        let failed = p.begin_frame(now).unwrap();
        assert!(!p.frame_not_submitted(failed + 1));
        assert!(p.deadline().is_none());
        assert!(p.frame_not_submitted(failed));
        assert!(p.begin_frame(now).is_none());
        let successful = p.begin_frame(now + Duration::from_millis(34)).unwrap();
        assert!(!p.frame_done(failed));
        assert!(p.deadline().is_none());
        assert!(p.frame_done(successful));
    }
}
