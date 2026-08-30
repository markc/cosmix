//! Per-edge panel state machine from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1 and logical-pixel
//! thickness storage from §E2.
//!
//! Ordinary hide and Escape are intentional no-ops while pinned. Unpinning is
//! always explicit; if the pointer is outside when unpinned, the normal grace
//! deadline begins at that moment. A reveal received before panel containment
//! is observable gets the same fallback grace, preventing an orphaned panel.

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
    Hide,
    Escape,
    Pin,
    Unpin,
    PointerEntered,
    PointerLeft,
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
    pub hide_at: Option<Duration>,
}

/// Result of applying input or advancing real time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelUpdate {
    pub changed: bool,
    pub snapshot: PanelSnapshot,
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
    hide_at: Option<Duration>,
    last_update: Duration,
}

impl PanelStateMachine {
    pub fn new(config: PanelConfig, start_at: Duration) -> Result<Self, PanelConfigError> {
        Ok(Self {
            config,
            mode: PanelMode::Hidden,
            motion: PanelMotion::new(config.motion_time).map_err(PanelConfigError::Motion)?,
            pointer_inside: false,
            hide_at: None,
            last_update: start_at,
        })
    }

    pub fn apply(
        &mut self,
        at: Duration,
        input: PanelInput,
    ) -> Result<PanelUpdate, PanelTimeError> {
        let before = self.snapshot();
        self.advance_to(at)?;
        match input {
            PanelInput::Reveal => {
                if self.mode != PanelMode::Pinned {
                    self.mode = PanelMode::Revealed;
                    if self.pointer_inside {
                        self.hide_at = None;
                    } else if self.hide_at.is_none() {
                        self.hide_at = Some(at + self.config.grace);
                    }
                }
                self.motion.reveal();
            }
            PanelInput::Hide | PanelInput::Escape => {
                if self.mode != PanelMode::Pinned {
                    self.mode = PanelMode::Hidden;
                    self.hide_at = None;
                    self.motion.conceal();
                }
            }
            PanelInput::Pin => {
                self.mode = PanelMode::Pinned;
                self.hide_at = None;
                self.motion.reveal();
            }
            PanelInput::Unpin => {
                if self.mode == PanelMode::Pinned {
                    self.mode = PanelMode::Revealed;
                    self.hide_at = (!self.pointer_inside).then_some(at + self.config.grace);
                }
            }
            PanelInput::PointerEntered => {
                self.pointer_inside = true;
                self.hide_at = None;
                if self.mode == PanelMode::Hidden && self.motion.visible_fraction() > 0.0 {
                    self.mode = PanelMode::Revealed;
                    self.motion.reveal();
                }
            }
            PanelInput::PointerLeft => {
                self.pointer_inside = false;
                if self.mode == PanelMode::Revealed {
                    self.hide_at = Some(at + self.config.grace);
                }
            }
        }
        Ok(self.update_since(before))
    }

    pub fn tick(&mut self, at: Duration) -> Result<PanelUpdate, PanelTimeError> {
        let before = self.snapshot();
        self.advance_to(at)?;
        Ok(self.update_since(before))
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
            hide_at: self.hide_at,
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

    fn advance_to(&mut self, at: Duration) -> Result<(), PanelTimeError> {
        if at < self.last_update {
            return Err(PanelTimeError {
                previous: self.last_update,
                update: at,
            });
        }

        if let Some(deadline) = self.hide_at
            && deadline <= at
            && self.mode == PanelMode::Revealed
            && !self.pointer_inside
        {
            let before_deadline = deadline.saturating_sub(self.last_update);
            self.motion.advance(before_deadline);
            self.mode = PanelMode::Hidden;
            self.hide_at = None;
            self.motion.conceal();
            self.motion.advance(at.saturating_sub(deadline));
        } else {
            self.motion.advance(at.saturating_sub(self.last_update));
        }
        self.last_update = at;
        Ok(())
    }

    fn update_since(&self, before: PanelSnapshot) -> PanelUpdate {
        let snapshot = self.snapshot();
        PanelUpdate {
            changed: snapshot != before,
            snapshot,
        }
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
