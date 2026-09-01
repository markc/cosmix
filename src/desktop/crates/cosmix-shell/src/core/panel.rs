//! Per-edge panel state machine from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1 and logical-pixel
//! thickness storage from §E2.
//!
//! Ordinary hide and Escape are intentional no-ops while pinned. Corner and
//! pointer containment are independent holds; concealment after either hold is
//! attributed to the event which armed the grace deadline.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use super::{MotionError, PanelMotion};

/// Stable semantic mode of one panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelMode {
    Hidden,
    Revealed,
    Pinned,
}

/// Inputs accepted by the pure panel state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelInput {
    Reveal,
    CornerEntered,
    CornerLeft,
    Hide,
    Escape,
    Pin,
    Unpin,
    PointerEntered,
    PointerLeft,
}

/// Why an actual reveal transition occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevealTrigger {
    Corner,
}

/// Why a grace deadline was armed before an actual conceal transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConcealReason {
    CornerLeft,
    Grace,
}

/// One observable semantic transition. An update carries at most one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelEffect {
    Reveal { trigger: RevealTrigger },
    Conceal { reason: ConcealReason },
    Pin { pinned: bool },
}

/// Per-panel timing and stored thickness.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelConfig {
    thickness_px: f32,
    grace: Duration,
    motion_time: Duration,
}

impl PanelConfig {
    pub fn new(
        thickness_px: f32,
        grace: Duration,
        motion_time: Duration,
    ) -> Result<Self, PanelConfigError> {
        if !thickness_px.is_finite() || thickness_px <= 0.0 {
            return Err(PanelConfigError::InvalidThickness(thickness_px));
        }
        if motion_time.is_zero() {
            return Err(PanelConfigError::Motion(MotionError::ZeroTravelTime));
        }
        Ok(Self {
            thickness_px,
            grace,
            motion_time,
        })
    }

    pub const fn thickness_px(self) -> f32 {
        self.thickness_px
    }

    pub const fn grace(self) -> Duration {
        self.grace
    }

    pub const fn motion_time(self) -> Duration {
        self.motion_time
    }
}

/// Public presentation snapshot for one panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelSnapshot {
    pub mode: PanelMode,
    pub visible_fraction: f32,
    pub target_fraction: f32,
    pub velocity_per_second: f32,
    pub thickness_px: f32,
    pub mapped: bool,
    pub exclusive_zone_px: f32,
    pub pointer_inside: bool,
    pub corner_inside: bool,
    pub hide_at: Option<Duration>,
    pub conceal_reason: Option<ConcealReason>,
}

/// Result of applying input or advancing real time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelUpdate {
    pub changed: bool,
    pub snapshot: PanelSnapshot,
    pub effect: Option<PanelEffect>,
}

/// The next host wake requirement for one panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelWake {
    Idle,
    WakeAt(Duration),
    Animate,
}

/// Pure semantic and motion state for one edge panel.
#[derive(Clone, Debug)]
pub struct PanelStateMachine {
    config: PanelConfig,
    mode: PanelMode,
    motion: PanelMotion,
    pointer_inside: bool,
    corner_inside: bool,
    hide_at: Option<Duration>,
    conceal_reason: Option<ConcealReason>,
    last_update: Duration,
}

impl PanelStateMachine {
    pub fn new(config: PanelConfig, start_at: Duration) -> Result<Self, PanelConfigError> {
        Ok(Self {
            config,
            mode: PanelMode::Hidden,
            motion: PanelMotion::new(config.motion_time).map_err(PanelConfigError::Motion)?,
            pointer_inside: false,
            corner_inside: false,
            hide_at: None,
            conceal_reason: None,
            last_update: start_at,
        })
    }

    pub fn apply(
        &mut self,
        at: Duration,
        input: PanelInput,
    ) -> Result<PanelUpdate, PanelTimeError> {
        let before = self.snapshot();
        let mut effect = self.advance_to(at)?;
        match input {
            PanelInput::Reveal => {
                if self.mode != PanelMode::Pinned {
                    self.mode = PanelMode::Revealed;
                    self.clear_deadline();
                }
                self.motion.reveal();
            }
            PanelInput::CornerEntered => {
                if self.corner_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.corner_inside = true;
                self.clear_deadline();
                if self.mode != PanelMode::Pinned {
                    if self.mode == PanelMode::Hidden {
                        effect = Some(PanelEffect::Reveal {
                            trigger: RevealTrigger::Corner,
                        });
                    }
                    self.mode = PanelMode::Revealed;
                    self.motion.reveal();
                }
            }
            PanelInput::CornerLeft => {
                if !self.corner_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.corner_inside = false;
                if self.mode == PanelMode::Revealed && !self.pointer_inside {
                    self.arm_deadline(at, ConcealReason::CornerLeft);
                }
            }
            PanelInput::Hide | PanelInput::Escape => {
                if self.mode != PanelMode::Pinned {
                    self.mode = PanelMode::Hidden;
                    self.clear_deadline();
                    self.motion.conceal();
                }
            }
            PanelInput::Pin => {
                if self.mode != PanelMode::Pinned {
                    effect = Some(PanelEffect::Pin { pinned: true });
                }
                self.mode = PanelMode::Pinned;
                self.clear_deadline();
                self.motion.reveal();
            }
            PanelInput::Unpin => {
                if self.mode == PanelMode::Pinned {
                    self.mode = PanelMode::Revealed;
                    effect = effect.or(Some(PanelEffect::Pin { pinned: false }));
                    if !self.pointer_inside && !self.corner_inside {
                        self.arm_deadline(at, ConcealReason::Grace);
                    }
                }
            }
            PanelInput::PointerEntered => {
                if self.pointer_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.pointer_inside = true;
                self.clear_deadline();
                if self.mode == PanelMode::Hidden && self.motion.visible_fraction() > 0.0 {
                    self.mode = PanelMode::Revealed;
                    self.motion.reveal();
                }
            }
            PanelInput::PointerLeft => {
                if !self.pointer_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.pointer_inside = false;
                if self.mode == PanelMode::Revealed && !self.corner_inside {
                    self.arm_deadline(at, ConcealReason::Grace);
                }
            }
        }
        Ok(self.update_since(before, effect))
    }

    pub fn tick(&mut self, at: Duration) -> Result<PanelUpdate, PanelTimeError> {
        let before = self.snapshot();
        let effect = self.advance_to(at)?;
        Ok(self.update_since(before, effect))
    }

    pub fn snapshot(&self) -> PanelSnapshot {
        let visible_fraction = self.motion.visible_fraction();
        PanelSnapshot {
            mode: self.mode,
            visible_fraction,
            target_fraction: self.motion.target(),
            velocity_per_second: self.motion.velocity_per_second(),
            thickness_px: self.config.thickness_px,
            mapped: self.mode != PanelMode::Hidden || visible_fraction > 0.0,
            exclusive_zone_px: if self.mode == PanelMode::Pinned {
                self.config.thickness_px
            } else {
                0.0
            },
            pointer_inside: self.pointer_inside,
            corner_inside: self.corner_inside,
            hide_at: self.hide_at,
            conceal_reason: self.conceal_reason,
        }
    }

    pub fn wake(&self) -> PanelWake {
        if self.motion.is_animating() {
            PanelWake::Animate
        } else if let Some(deadline) = self.hide_at {
            PanelWake::WakeAt(deadline)
        } else {
            PanelWake::Idle
        }
    }

    fn advance_to(&mut self, at: Duration) -> Result<Option<PanelEffect>, PanelTimeError> {
        if at < self.last_update {
            return Err(PanelTimeError {
                previous: self.last_update,
                update: at,
            });
        }

        let mut effect = None;
        if let Some(deadline) = self.hide_at
            && deadline <= at
            && self.mode == PanelMode::Revealed
            && !self.pointer_inside
            && !self.corner_inside
        {
            let before_deadline = deadline.saturating_sub(self.last_update);
            self.motion.advance(before_deadline);
            self.mode = PanelMode::Hidden;
            effect = self
                .conceal_reason
                .map(|reason| PanelEffect::Conceal { reason });
            self.hide_at = None;
            self.conceal_reason = None;
            self.motion.conceal();
            self.motion.advance(at.saturating_sub(deadline));
        } else {
            self.motion.advance(at.saturating_sub(self.last_update));
        }
        self.last_update = at;
        Ok(effect)
    }

    fn update_since(&self, before: PanelSnapshot, effect: Option<PanelEffect>) -> PanelUpdate {
        let snapshot = self.snapshot();
        PanelUpdate {
            changed: snapshot != before,
            snapshot,
            effect,
        }
    }

    fn clear_deadline(&mut self) {
        self.hide_at = None;
        self.conceal_reason = None;
    }

    fn arm_deadline(&mut self, at: Duration, reason: ConcealReason) {
        self.hide_at = Some(at + self.config.grace);
        self.conceal_reason = Some(reason);
    }
}

/// Invalid panel configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanelConfigError {
    InvalidThickness(f32),
    Motion(MotionError),
}

impl Display for PanelConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidThickness(value) => {
                write!(
                    formatter,
                    "panel thickness must be finite and positive, got {value}"
                )
            }
            Self::Motion(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for PanelConfigError {}

/// A panel update used a timestamp older than its preceding update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelTimeError {
    pub previous: Duration,
    pub update: Duration,
}

impl Display for PanelTimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "panel update time {:?} precedes {:?}",
            self.update, self.previous
        )
    }
}

impl Error for PanelTimeError {}
