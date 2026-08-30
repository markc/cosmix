//! Renderer-neutral, reversible panel motion for
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1.
//!
//! Q-0 deliberately uses a constant-speed reference motion. Chrome may later
//! choose a spring curve, but reversal must retain the current visible fraction
//! rather than restart an animation from an endpoint.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

/// Interruptible motion between fully hidden (`0`) and fully visible (`1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelMotion {
    visible_fraction: f32,
    target: f32,
    velocity_per_second: f32,
    travel_time: Duration,
}

impl PanelMotion {
    pub fn new(travel_time: Duration) -> Result<Self, MotionError> {
        if travel_time.is_zero() {
            return Err(MotionError::ZeroTravelTime);
        }
        Ok(Self {
            visible_fraction: 0.0,
            target: 0.0,
            velocity_per_second: 0.0,
            travel_time,
        })
    }

    pub const fn visible_fraction(self) -> f32 {
        self.visible_fraction
    }

    pub const fn target(self) -> f32 {
        self.target
    }

    pub const fn velocity_per_second(self) -> f32 {
        self.velocity_per_second
    }

    pub const fn is_animating(self) -> bool {
        self.velocity_per_second != 0.0
    }

    pub fn reveal(&mut self) {
        self.set_target(1.0);
    }

    pub fn conceal(&mut self) {
        self.set_target(0.0);
    }

    pub fn advance(&mut self, elapsed: Duration) -> bool {
        if !self.is_animating() || elapsed.is_zero() {
            return false;
        }
        let previous = self.visible_fraction;
        let next = previous + self.velocity_per_second * elapsed.as_secs_f32();
        if (self.velocity_per_second > 0.0 && next >= self.target)
            || (self.velocity_per_second < 0.0 && next <= self.target)
        {
            self.visible_fraction = self.target;
            self.velocity_per_second = 0.0;
        } else {
            self.visible_fraction = next.clamp(0.0, 1.0);
        }
        self.visible_fraction != previous
    }

    fn set_target(&mut self, target: f32) {
        self.target = target;
        if self.visible_fraction == target {
            self.velocity_per_second = 0.0;
            return;
        }
        let speed = 1.0 / self.travel_time.as_secs_f32();
        self.velocity_per_second = if target > self.visible_fraction {
            speed
        } else {
            -speed
        };
    }
}

/// Invalid motion configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionError {
    ZeroTravelTime,
}

impl Display for MotionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("panel travel time must be greater than zero")
    }
}

impl Error for MotionError {}
