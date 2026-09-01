//! Pure corner engagement detection from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §D1.
//!
//! Q-0 can tune deadzone, dwell, and absolute-pointer velocity in a fullscreen
//! harness. The early "continued physical push" path is **synthetic-only in
//! Q-0**: winit raw device motion is not logical constrained motion, so a live
//! host must not feed it into [`PointerSample::attempted_motion`]. S-1 must
//! decide whether to extract this module into a neutral gesture crate; comp
//! must not acquire a dependency on `cosmix-shell`. Both dwell and synthetic
//! push engagement require a velocity-qualified interval; fast arrivals remain
//! candidates so a later stationary dwell can still engage.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use super::{Corner, LogicalPoint, LogicalSize, LogicalVector};

const MOTION_EPSILON_PX: f32 = 0.001;
const MIN_STATIONARY_CONFIRM: Duration = Duration::from_millis(1);

/// Tunable corner detector values, expressed in logical pixels and real time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerDetectorConfig {
    deadzone_px: f32,
    dwell: Duration,
    velocity_max_px_s: f32,
}

impl CornerDetectorConfig {
    pub fn new(
        deadzone_px: f32,
        dwell: Duration,
        velocity_max_px_s: f32,
    ) -> Result<Self, CornerDetectorError> {
        if !deadzone_px.is_finite() || deadzone_px <= 0.0 {
            return Err(CornerDetectorError::InvalidDeadzone(deadzone_px));
        }
        if !velocity_max_px_s.is_finite() || velocity_max_px_s <= 0.0 {
            return Err(CornerDetectorError::InvalidVelocityGate(velocity_max_px_s));
        }
        Ok(Self {
            deadzone_px,
            dwell,
            velocity_max_px_s,
        })
    }

    pub const fn deadzone_px(self) -> f32 {
        self.deadzone_px
    }

    pub const fn dwell(self) -> Duration {
        self.dwell
    }

    pub const fn velocity_max_px_s(self) -> f32 {
        self.velocity_max_px_s
    }
}

/// One absolute pointer observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerSample {
    pub at: Duration,
    pub position: LogicalPoint,
    pub output: LogicalSize,
    /// Logical motion discarded because the pointer was already constrained at
    /// the physical output corner. `Some` is a compositor-side fact, not raw
    /// device motion. Q-0 supplies it only from synthetic tests.
    pub attempted_motion: Option<LogicalVector>,
}

impl PointerSample {
    pub const fn new(at: Duration, position: LogicalPoint, output: LogicalSize) -> Self {
        Self {
            at,
            position,
            output,
            attempted_motion: None,
        }
    }

    pub const fn with_attempted_motion(mut self, motion: LogicalVector) -> Self {
        self.attempted_motion = Some(motion);
        self
    }
}

/// Why a corner became engaged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CornerTrigger {
    Dwell,
    SyntheticPush,
    /// Engagement reported by the compositor Bus service.
    Compositor,
}

/// Observable corner engagement transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CornerEvent {
    Entered {
        corner: Corner,
        dwell: Duration,
        trigger: CornerTrigger,
    },
    Left {
        corner: Corner,
    },
}

impl CornerEvent {
    pub const fn corner(self) -> Corner {
        match self {
            Self::Entered { corner, .. } | Self::Left { corner } => corner,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    corner: Corner,
    entered_at: Duration,
    velocity_eligible_since: Option<Duration>,
    last_motion_speed_px_s: Option<f32>,
}

/// Read-only detector state for the Q-0 tuning overlay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerDiagnostics {
    pub candidate: Option<Corner>,
    pub engaged: Option<Corner>,
    pub dwell_elapsed: Duration,
    pub dwell_deadline: Option<Duration>,
    pub last_motion_speed_px_s: Option<f32>,
}

/// Stateful detector for one output pointer stream.
#[derive(Clone, Debug)]
pub struct CornerDetector {
    config: CornerDetectorConfig,
    candidate: Option<Candidate>,
    engaged: Option<Corner>,
    last_sample: Option<PointerSample>,
}

impl CornerDetector {
    pub const fn new(config: CornerDetectorConfig) -> Self {
        Self {
            config,
            candidate: None,
            engaged: None,
            last_sample: None,
        }
    }

    pub const fn config(&self) -> CornerDetectorConfig {
        self.config
    }

    pub const fn engaged_corner(&self) -> Option<Corner> {
        self.engaged
    }

    /// Snapshot tuning state at `now` without changing detector semantics.
    pub fn diagnostics(&self, now: Duration) -> CornerDiagnostics {
        CornerDiagnostics {
            candidate: self.candidate.map(|candidate| candidate.corner),
            engaged: self.engaged,
            dwell_elapsed: self
                .candidate
                .and_then(|candidate| candidate.velocity_eligible_since)
                .map(|since| now.saturating_sub(since))
                .unwrap_or_default(),
            dwell_deadline: self.next_deadline(),
            last_motion_speed_px_s: self
                .candidate
                .and_then(|candidate| candidate.last_motion_speed_px_s),
        }
    }

    /// Exact real-time wake needed to complete a stationary dwell.
    pub fn next_deadline(&self) -> Option<Duration> {
        let candidate = self.candidate?;
        if let Some(since) = candidate.velocity_eligible_since {
            return Some(since + self.config.dwell);
        }
        self.last_sample.map(|sample| {
            sample.at
                + if self.config.dwell.is_zero() {
                    MIN_STATIONARY_CONFIRM
                } else {
                    self.config.dwell
                }
        })
    }

    /// Feed one monotonic sample and return zero or more engagement events.
    pub fn sample(
        &mut self,
        sample: PointerSample,
    ) -> Result<Vec<CornerEvent>, CornerDetectorError> {
        if !sample.output.contains(sample.position) {
            return Err(CornerDetectorError::PointerOutsideOutput {
                position: sample.position,
                output: sample.output,
            });
        }
        if let Some(last) = self.last_sample
            && sample.at < last.at
        {
            return Err(CornerDetectorError::NonMonotonicTime {
                previous: last.at,
                sample: sample.at,
            });
        }

        let mut events = Vec::with_capacity(2);
        if self
            .last_sample
            .is_some_and(|last| last.output != sample.output)
        {
            if let Some(corner) = self.engaged.take() {
                events.push(CornerEvent::Left { corner });
            }
            self.candidate = None;
            self.last_sample = None;
        }

        let speed = self.instantaneous_speed(sample);
        let corner = corner_at(sample.position, sample.output, self.config.deadzone_px);

        if let Some(engaged) = self.engaged {
            if corner == Some(engaged) {
                self.last_sample = Some(sample);
                return Ok(events);
            }
            self.engaged = None;
            events.push(CornerEvent::Left { corner: engaged });
        }

        let Some(corner) = corner else {
            self.candidate = None;
            self.last_sample = Some(sample);
            return Ok(events);
        };

        let continuing_candidate = self
            .candidate
            .is_some_and(|candidate| candidate.corner == corner);
        let mut became_velocity_eligible = false;
        if !continuing_candidate {
            let velocity_eligible_since = speed
                .map(|speed| speed <= self.config.velocity_max_px_s)
                .unwrap_or(false)
                .then_some(sample.at);
            self.candidate = Some(Candidate {
                corner,
                entered_at: sample.at,
                velocity_eligible_since,
                last_motion_speed_px_s: speed,
            });
        } else if let Some(speed) = speed
            && let Some(candidate) = &mut self.candidate
        {
            candidate.last_motion_speed_px_s = Some(speed);
            if speed <= self.config.velocity_max_px_s {
                if candidate.velocity_eligible_since.is_none() {
                    became_velocity_eligible = true;
                    candidate.velocity_eligible_since =
                        Some(self.last_sample.map(|last| last.at).unwrap_or(sample.at));
                }
            } else {
                candidate.velocity_eligible_since = None;
            }
        }

        let candidate = self.candidate.expect("candidate was just established");
        let dwell = sample.at.saturating_sub(candidate.entered_at);
        let velocity_dwell = candidate
            .velocity_eligible_since
            .map(|since| sample.at.saturating_sub(since));
        let trigger = if velocity_dwell.is_some_and(|dwell| dwell >= self.config.dwell) {
            Some(CornerTrigger::Dwell)
        } else if continuing_candidate
            && !became_velocity_eligible
            && candidate
                .last_motion_speed_px_s
                .is_some_and(|speed| speed <= self.config.velocity_max_px_s)
            && sample
                .attempted_motion
                .is_some_and(|motion| pushes_outward(corner, motion))
        {
            Some(CornerTrigger::SyntheticPush)
        } else {
            None
        };

        if let Some(trigger) = trigger {
            self.candidate = None;
            self.engaged = Some(corner);
            events.push(CornerEvent::Entered {
                corner,
                dwell,
                trigger,
            });
        }
        self.last_sample = Some(sample);
        Ok(events)
    }

    /// End the current output pointer stream, emitting `Left` only for a corner
    /// which had actually emitted `Entered`.
    pub fn leave_output(&mut self, at: Duration) -> Result<Vec<CornerEvent>, CornerDetectorError> {
        if let Some(last) = self.last_sample
            && at < last.at
        {
            return Err(CornerDetectorError::NonMonotonicTime {
                previous: last.at,
                sample: at,
            });
        }
        self.candidate = None;
        self.last_sample = None;
        Ok(self
            .engaged
            .take()
            .map(|corner| vec![CornerEvent::Left { corner }])
            .unwrap_or_default())
    }

    fn instantaneous_speed(&self, sample: PointerSample) -> Option<f32> {
        let last = self.last_sample?;
        let elapsed = sample.at.checked_sub(last.at)?.as_secs_f32();
        if elapsed <= 0.0 {
            return None;
        }
        let movement = LogicalVector::new(
            sample.position.x - last.position.x,
            sample.position.y - last.position.y,
        )
        .length();
        Some(if movement > MOTION_EPSILON_PX {
            movement / elapsed
        } else {
            0.0
        })
    }
}

/// Invalid detector configuration or pointer stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CornerDetectorError {
    InvalidDeadzone(f32),
    InvalidVelocityGate(f32),
    PointerOutsideOutput {
        position: LogicalPoint,
        output: LogicalSize,
    },
    NonMonotonicTime {
        previous: Duration,
        sample: Duration,
    },
}

impl Display for CornerDetectorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDeadzone(value) => {
                write!(
                    formatter,
                    "corner deadzone must be finite and positive, got {value}"
                )
            }
            Self::InvalidVelocityGate(value) => write!(
                formatter,
                "corner velocity gate must be finite and positive, got {value}"
            ),
            Self::PointerOutsideOutput { position, output } => write!(
                formatter,
                "pointer ({}, {}) is outside logical output {}x{}",
                position.x,
                position.y,
                output.width(),
                output.height()
            ),
            Self::NonMonotonicTime { previous, sample } => write!(
                formatter,
                "pointer sample time {sample:?} precedes {previous:?}"
            ),
        }
    }
}

impl Error for CornerDetectorError {}

fn corner_at(point: LogicalPoint, output: LogicalSize, deadzone: f32) -> Option<Corner> {
    let left = point.x <= deadzone;
    let right = output.width() - point.x <= deadzone;
    let top = point.y <= deadzone;
    let bottom = output.height() - point.y <= deadzone;

    let candidates = [
        (left && top, Corner::TopLeft, point.x.hypot(point.y)),
        (
            left && bottom,
            Corner::BottomLeft,
            point.x.hypot(output.height() - point.y),
        ),
        (
            right && bottom,
            Corner::BottomRight,
            (output.width() - point.x).hypot(output.height() - point.y),
        ),
        (
            right && top,
            Corner::TopRight,
            (output.width() - point.x).hypot(point.y),
        ),
    ];
    candidates
        .into_iter()
        .filter(|(inside, _, _)| *inside)
        .min_by(|left, right| left.2.total_cmp(&right.2))
        .map(|(_, corner, _)| corner)
}

fn pushes_outward(corner: Corner, motion: LogicalVector) -> bool {
    match corner {
        Corner::TopLeft => motion.x < 0.0 || motion.y < 0.0,
        Corner::BottomLeft => motion.x < 0.0 || motion.y > 0.0,
        Corner::BottomRight => motion.x > 0.0 || motion.y > 0.0,
        Corner::TopRight => motion.x > 0.0 || motion.y < 0.0,
    }
}
