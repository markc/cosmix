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

/// Interactive thickness limits in logical pixels; 120 keeps side chrome usable.
pub const RESIZE_THICKNESS_RANGE: std::ops::RangeInclusive<f32> = 120.0..=500.0;

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
    /// Reveal when `Hidden`, hide otherwise; a pinned panel ignores it (the
    /// same law as [`PanelInput::Hide`]). The direction binds at Model time
    /// against the authoritative [`PanelMode`] — never against a caller's
    /// frame snapshot — so a mid-conceal panel (`mapped == true`, mode
    /// already `Hidden`) toggles back open, and two toggles applied in one
    /// drained batch net to identity rather than to a single toggle.
    Toggle,
    CornerEntered,
    CornerLeft,
    Hide,
    Escape,
    Pin,
    Unpin,
    PointerEntered,
    PointerLeft,
    ResizeStarted,
    ResizeCompleted,
    ResizeCancelled,
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
    ResizeCompleted,
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
    pub resize_active: bool,
    /// Last completed size, kept stable throughout an interactive gesture.
    pub settled_thickness_px: f32,
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
    resize_start: Option<f32>,
    corner_inside: bool,
    intro_until: Option<Duration>,
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
            resize_start: None,
            corner_inside: false,
            intro_until: None,
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
            PanelInput::ResizeStarted => {
                self.resize_start.get_or_insert(self.config.thickness_px);
                self.clear_deadline();
            }
            PanelInput::ResizeCompleted | PanelInput::ResizeCancelled => {
                if let Some(start) = self.resize_start.take() {
                    if input == PanelInput::ResizeCompleted {
                        effect = Some(PanelEffect::ResizeCompleted);
                    } else {
                        self.config.thickness_px = start;
                    }
                    if self.mode == PanelMode::Revealed
                        && !self.pointer_inside
                        && !self.corner_inside
                        && self.intro_until.is_none()
                    {
                        self.arm_deadline(at, ConcealReason::Grace);
                    }
                }
            }
            PanelInput::Reveal => {
                if self.mode != PanelMode::Pinned {
                    self.mode = PanelMode::Revealed;
                    self.clear_deadline();
                }
                self.motion.reveal();
            }
            PanelInput::Toggle => {
                if self.mode == PanelMode::Hidden {
                    self.mode = PanelMode::Revealed;
                    self.clear_deadline();
                    self.motion.reveal();
                } else if self.mode != PanelMode::Pinned && self.resize_start.is_none() {
                    // Mirrors Hide: a pinned panel ignores both directions.
                    self.mode = PanelMode::Hidden;
                    self.clear_deadline();
                    self.motion.conceal();
                }
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
                if self.mode == PanelMode::Revealed
                    && !self.pointer_inside
                    && self.intro_until.is_none()
                {
                    self.arm_deadline(at, ConcealReason::CornerLeft);
                }
            }
            PanelInput::Hide | PanelInput::Escape => {
                if self.mode != PanelMode::Pinned && self.resize_start.is_none() {
                    self.mode = PanelMode::Hidden;
                    self.clear_deadline();
                    self.motion.conceal();
                }
            }
            PanelInput::Pin => {
                self.intro_until = None;
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
                    if !self.pointer_inside && !self.corner_inside && self.intro_until.is_none() {
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
                if self.mode == PanelMode::Revealed
                    && !self.corner_inside
                    && self.intro_until.is_none()
                {
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

    /// A startup hold never claims real corner or pointer membership.
    pub fn start_intro(&mut self, duration: Duration) {
        if self.mode != PanelMode::Pinned {
            self.intro_until = Some(self.last_update + duration);
            self.mode = PanelMode::Revealed;
            self.clear_deadline();
            self.motion.reveal();
        }
    }

    /// Stored dimensions obey the same law as initial configuration.
    pub fn restore_thickness(&mut self, thickness_px: f32) -> Result<(), PanelConfigError> {
        self.config = PanelConfig::new(thickness_px, self.config.grace, self.config.motion_time)?;
        Ok(())
    }

    /// Runtime commands reject invalid input; pointer adapters clamp before ingress.
    pub fn resize_thickness(&mut self, thickness_px: f32) -> Result<(), PanelConfigError> {
        let config = PanelConfig::new(thickness_px, self.config.grace, self.config.motion_time)?;
        if !RESIZE_THICKNESS_RANGE.contains(&thickness_px) {
            return Err(PanelConfigError::InvalidThickness(thickness_px));
        }
        self.config = config;
        Ok(())
    }

    /// Membership of a retired output cannot hold its replacement open.
    pub(super) fn leave_output(&mut self) {
        let held = self.corner_inside || self.pointer_inside || self.resize_start.is_some();
        if let Some(start) = self.resize_start.take() {
            self.config.thickness_px = start;
        }
        self.corner_inside = false;
        self.pointer_inside = false;
        if held && self.mode == PanelMode::Revealed && self.intro_until.is_none() {
            self.arm_deadline(self.last_update, ConcealReason::Grace);
        }
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
            resize_active: self.resize_start.is_some(),
            settled_thickness_px: self.resize_start.unwrap_or(self.config.thickness_px),
            corner_inside: self.corner_inside,
            hide_at: self.hide_at,
            conceal_reason: self.conceal_reason,
        }
    }

    pub fn wake(&self) -> PanelWake {
        if self.motion.is_animating() {
            PanelWake::Animate
        } else if let Some(deadline) = self.next_deadline() {
            PanelWake::WakeAt(deadline)
        } else {
            PanelWake::Idle
        }
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        self.hide_at.into_iter().chain(self.intro_until).min()
    }

    fn advance_to(&mut self, at: Duration) -> Result<Option<PanelEffect>, PanelTimeError> {
        if at < self.last_update {
            return Err(PanelTimeError {
                previous: self.last_update,
                update: at,
            });
        }

        if let Some(deadline) = self.intro_until
            && deadline <= at
        {
            self.intro_until = None;
            if self.mode == PanelMode::Revealed && !self.pointer_inside && !self.corner_inside {
                self.arm_deadline(deadline, ConcealReason::Grace);
            }
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
        if self.resize_start.is_some() {
            return;
        }
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

#[cfg(test)]
mod intro_tests {
    use super::*;

    fn panel() -> PanelStateMachine {
        PanelStateMachine::new(
            PanelConfig::new(
                100.0,
                Duration::from_millis(800),
                Duration::from_millis(200),
            )
            .unwrap(),
            Duration::ZERO,
        )
        .unwrap()
    }

    #[test]
    fn runtime_resize_validates_range_and_keeps_pinned_zone_live() {
        let mut panel = panel();
        panel.apply(Duration::ZERO, PanelInput::Pin).unwrap();
        for thickness in [120.0, 250.0, 500.0] {
            panel.resize_thickness(thickness).unwrap();
            assert_eq!(panel.snapshot().thickness_px, thickness);
            assert_eq!(panel.snapshot().exclusive_zone_px, thickness);
        }
        for invalid in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            119.0,
            501.0,
        ] {
            assert!(panel.resize_thickness(invalid).is_err());
            assert_eq!(panel.snapshot().thickness_px, 500.0);
        }
    }

    #[test]
    fn resize_hold_survives_pointer_corner_intro_and_explicit_hide() {
        let mut panel = panel();
        panel.start_intro(Duration::from_secs(1));
        for input in [
            PanelInput::PointerEntered,
            PanelInput::CornerEntered,
            PanelInput::ResizeStarted,
            PanelInput::PointerLeft,
            PanelInput::CornerLeft,
            PanelInput::Hide,
            PanelInput::Escape,
            PanelInput::Toggle,
        ] {
            panel.apply(Duration::ZERO, input).unwrap();
        }
        panel.tick(Duration::from_secs(10)).unwrap();
        assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
        assert_eq!(panel.next_deadline(), None);
        assert_eq!(panel.wake(), PanelWake::Idle);
        assert_eq!(
            panel
                .apply(Duration::from_secs(10), PanelInput::ResizeCompleted)
                .unwrap()
                .effect,
            Some(PanelEffect::ResizeCompleted)
        );
        assert_eq!(
            panel
                .apply(Duration::from_secs(10), PanelInput::ResizeCompleted)
                .unwrap()
                .effect,
            None
        );
        panel.tick(Duration::from_secs(11)).unwrap();
        assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
    }

    #[test]
    fn resize_cancellation_and_output_retirement_restore_without_completion() {
        let mut panel = panel();
        for retire in [false, true] {
            panel
                .apply(Duration::ZERO, PanelInput::ResizeStarted)
                .unwrap();
            panel.resize_thickness(300.0).unwrap();
            assert_eq!(panel.snapshot().settled_thickness_px, 100.0);
            if retire {
                panel.leave_output();
            } else {
                assert_eq!(
                    panel
                        .apply(Duration::ZERO, PanelInput::ResizeCancelled)
                        .unwrap()
                        .effect,
                    None
                );
            }
            assert_eq!(panel.snapshot().thickness_px, 100.0);
            assert!(!panel.snapshot().resize_active);
            assert_eq!(
                panel
                    .apply(Duration::ZERO, PanelInput::ResizeCompleted)
                    .unwrap()
                    .effect,
                None
            );
        }
    }

    #[test]
    fn intro_expires_into_normal_grace_without_corner_membership() {
        let mut panel = panel();
        panel.start_intro(Duration::from_secs(2));
        assert!(!panel.snapshot().corner_inside);
        panel.tick(Duration::from_secs(1)).unwrap();
        assert_eq!(panel.wake(), PanelWake::WakeAt(Duration::from_secs(2)));
        panel.tick(Duration::from_secs(2)).unwrap();
        assert_eq!(panel.snapshot().hide_at, Some(Duration::from_millis(2800)));
        let update = panel.tick(Duration::from_secs(3)).unwrap();
        assert_eq!(update.snapshot.mode, PanelMode::Hidden);
        assert_eq!(
            update.effect,
            Some(PanelEffect::Conceal {
                reason: ConcealReason::Grace
            })
        );
        assert_eq!(panel.next_deadline(), None);
    }

    #[test]
    fn real_corner_enter_during_intro_survives_expiry() {
        let mut panel = panel();
        panel.start_intro(Duration::from_secs(2));
        panel
            .apply(Duration::from_secs(1), PanelInput::CornerEntered)
            .unwrap();
        panel.tick(Duration::from_secs(3)).unwrap();
        assert!(panel.snapshot().corner_inside);
        assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
        assert_eq!(panel.next_deadline(), None);
        panel
            .apply(Duration::from_secs(3), PanelInput::CornerLeft)
            .unwrap();
        assert_eq!(panel.snapshot().hide_at, Some(Duration::from_millis(3800)));
    }

    #[test]
    fn corner_and_pointer_leaves_cannot_end_intro_early() {
        let mut panel = panel();
        panel.start_intro(Duration::from_secs(2));
        for input in [
            PanelInput::CornerEntered,
            PanelInput::PointerEntered,
            PanelInput::CornerLeft,
            PanelInput::PointerLeft,
        ] {
            panel.apply(Duration::from_millis(100), input).unwrap();
        }
        panel.tick(Duration::from_millis(1900)).unwrap();
        assert_eq!(panel.snapshot().mode, PanelMode::Revealed);
        assert_eq!(panel.snapshot().hide_at, None);
    }

    #[test]
    fn intro_does_not_change_restored_pins() {
        let mut panel = panel();
        panel.apply(Duration::ZERO, PanelInput::Pin).unwrap();
        panel.start_intro(Duration::from_secs(2));
        panel.tick(Duration::from_secs(3)).unwrap();
        assert_eq!(panel.snapshot().mode, PanelMode::Pinned);
        assert_eq!(panel.next_deadline(), None);
    }
}
