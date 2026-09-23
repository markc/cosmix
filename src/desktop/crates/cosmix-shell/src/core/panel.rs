//! Per-edge panel state machine from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1 and logical-pixel
//! thickness storage from §E2.
//!
//! Ordinary hide and Escape are intentional no-ops while pinned or docked. Corner and
//! pointer containment are independent holds; concealment after either hold is
//! attributed to the event which armed the grace deadline. A deliberate undock from
//! docked hides immediately unless a hold keeps the panel revealed: the grace delay
//! exists to forgive pointer overshoot and never applies to a deliberate action.
//! An interim local menu input suppresses concealment until the popup closes.
//!
//! Two drivers, one machine (shell design §4.3). Locally (the dev host, and any
//! compositor that does not report the holder plane) corner and pointer
//! membership reveal and the grace deadline conceals, as above. Once the host
//! reports the compositor's holder plane ([`PanelStateMachine::set_holder_plane`])
//! the machine is command-driven: the compositor owns the pointer/focus/popup
//! holders and the conceal delay, and says so with [`PanelInput::HolderReveal`] /
//! [`PanelInput::HolderConceal`]. Corner and pointer inputs then only record
//! membership, no grace deadline is ever armed, and a local hold (menu, resize,
//! intro, explicit show) only defers a conceal until it ends. Deliberate conceals
//! (hide, Escape, a hidden mode) latch against a still-holding compositor until
//! it next reports the holders released, so its verdict cannot undo them.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use super::{MotionError, PanelMotion};

/// Interactive thickness limits in logical pixels; 120 keeps side chrome usable.
pub const RESIZE_THICKNESS_RANGE: std::ops::RangeInclusive<f32> = 120.0..=500.0;

/// Stable semantic mode of one panel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PanelMode {
    #[default]
    Hidden,
    Pinned,
    Docked,
}

impl PanelMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Pinned => "pinned",
            Self::Docked => "docked",
        }
    }

    /// Parse the token [`PanelMode::as_str`] emits; anything else is refused.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hidden" => Some(Self::Hidden),
            "pinned" => Some(Self::Pinned),
            "docked" => Some(Self::Docked),
            _ => None,
        }
    }
}

/// Inputs accepted by the pure panel state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelInput {
    /// Interim corner-menu hold; replaced by compositor popup ownership later.
    MenuHold(bool),
    Reveal,
    /// Toggle transient visibility when `Hidden`; persistent panels ignore it (the
    /// same law as [`PanelInput::Hide`]). The direction binds at Model time
    /// against authoritative transient visibility — never against a caller's
    /// frame snapshot — so a mid-conceal panel (`mapped == true`, transient
    /// reveal already false) toggles back open, and two toggles applied in one
    /// drained batch net to identity rather than to a single toggle.
    Toggle,
    CornerEntered,
    CornerLeft,
    Hide,
    Escape,
    Pin,
    Unpin,
    /// Toggle pinning against the model's current mode, emitting the same
    /// effects as Pin/Unpin so persistence observes both directions.
    PinToggle,
    Dock,
    /// Leave `Docked`: while the pointer, corner or resize holds the panel it degrades
    /// to a transient reveal with normal grace; with no hold it hides
    /// immediately.
    Undock,
    DockToggle,
    /// Set a persistent mode. Hidden conceals immediately, even when a pointer
    /// or corner hold would keep an undocked panel transiently revealed.
    /// Repeating Hidden also dismisses a transient reveal, without emitting a
    /// persistence effect.
    SetMode(PanelMode),
    /// Compatibility release for legacy Bus unpin: releases either persistent
    /// mode into transient visibility with normal grace.
    Release,
    PointerEntered,
    PointerLeft,
    ResizeStarted,
    ResizeCompleted,
    ResizeCancelled,
    /// The compositor's holder plane: at least one holder (pointer, focus or
    /// popup) now holds this edge. Reveals a hidden panel unless a deliberate
    /// conceal latched it. Ignored while the holder plane is inactive.
    HolderReveal,
    /// The compositor's holder plane: the last holder released, after its
    /// conceal delay where one applies. Conceals a transient reveal at once
    /// unless a local hold still keeps it. Ignored while the plane is inactive.
    HolderConceal,
}

/// Why an actual reveal transition occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevealTrigger {
    Corner,
    /// The compositor reported a holder.
    Holders,
}

/// Why a grace deadline was armed before an actual conceal transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConcealReason {
    CornerLeft,
    Grace,
    /// Command-driven: nothing holds the panel any more — the compositor
    /// released its holders and no local hold remains. Never a grace deadline.
    Holders,
}

/// One observable semantic transition. An update carries at most one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelEffect {
    ResizeCompleted,
    Reveal { trigger: RevealTrigger },
    Conceal { reason: ConcealReason },
    ModeChanged { mode: PanelMode },
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
    /// Runtime visibility intent only; never persisted. Only true in Hidden.
    pub transient_revealed: bool,
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
    menu_hold: bool,
    config: PanelConfig,
    mode: PanelMode,
    transient_revealed: bool,
    motion: PanelMotion,
    pointer_inside: bool,
    resize_start: Option<f32>,
    corner_inside: bool,
    intro_until: Option<Duration>,
    hide_at: Option<Duration>,
    conceal_reason: Option<ConcealReason>,
    last_update: Duration,
    /// The compositor reports the holder plane: reveal/conceal are its commands.
    holder_plane: bool,
    /// The compositor's last verdict: some holder holds this edge.
    comp_held: bool,
    /// An explicit transient reveal (Reveal, Toggle): kept until a deliberate
    /// conceal or until the compositor's holders take it and release it.
    shown: bool,
    /// A deliberate conceal the compositor's holders have not yet released:
    /// its reveals are ignored until it reports the holders released.
    latched: bool,
}

impl PanelStateMachine {
    pub fn new(config: PanelConfig, start_at: Duration) -> Result<Self, PanelConfigError> {
        Ok(Self {
            holder_plane: false,
            comp_held: false,
            shown: false,
            latched: false,
            menu_hold: false,
            config,
            mode: PanelMode::Hidden,
            transient_revealed: false,
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
        // Acquire before advancing a grace deadline at the same timestamp.
        if input == PanelInput::MenuHold(true) && at >= self.last_update {
            self.menu_hold = true;
            self.clear_deadline();
        }
        let mut effect = self.advance_to(at)?;
        let plane = self.holder_plane;
        match input {
            PanelInput::MenuHold(open) => {
                self.menu_hold = open;
                self.clear_deadline();
                if !open {
                    if plane {
                        effect = self.settle_holders().or(effect);
                    } else if self.transient_revealed
                        && !self.pointer_inside
                        && !self.corner_inside
                        && self.resize_start.is_none()
                        && self.intro_until.is_none()
                    {
                        self.transient_revealed = false;
                        self.shown = false;
                        self.motion.conceal();
                    }
                }
            }
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
                    if plane {
                        // The completion effect is the one persistence needs.
                        let _ = self.settle_holders();
                    } else if self.transient_revealed
                        && !self.pointer_inside
                        && !self.corner_inside
                        && self.intro_until.is_none()
                    {
                        self.arm_deadline(at, ConcealReason::Grace);
                    }
                }
            }
            PanelInput::Reveal => {
                if self.mode == PanelMode::Hidden {
                    self.transient_revealed = true;
                    self.shown = true;
                    self.latched = false;
                    self.clear_deadline();
                }
                self.motion.reveal();
            }
            PanelInput::Toggle => {
                if self.mode == PanelMode::Hidden && !self.transient_revealed {
                    self.transient_revealed = true;
                    self.shown = true;
                    self.latched = false;
                    self.clear_deadline();
                    self.motion.reveal();
                } else if self.mode == PanelMode::Hidden
                    && self.resize_start.is_none()
                    && !self.menu_hold
                {
                    // Mirrors Hide: persistent panels ignore both directions.
                    self.conceal_now();
                }
            }
            // Command-driven: membership is the compositor's to judge; the
            // local flags stay current for the undock hold check and for a
            // fall back to local behaviour.
            PanelInput::CornerEntered if plane => self.corner_inside = true,
            PanelInput::CornerLeft if plane => self.corner_inside = false,
            PanelInput::PointerEntered if plane => self.pointer_inside = true,
            PanelInput::PointerLeft if plane => self.pointer_inside = false,
            PanelInput::HolderReveal if plane => {
                self.comp_held = true;
                if self.mode == PanelMode::Hidden && !self.latched {
                    if !self.transient_revealed {
                        effect = Some(PanelEffect::Reveal {
                            trigger: RevealTrigger::Holders,
                        });
                    }
                    self.transient_revealed = true;
                    self.clear_deadline();
                    self.motion.reveal();
                }
            }
            PanelInput::HolderConceal if plane => {
                // A release after a hold ends an explicit show too: the
                // pointer came and went. A repeated verdict does not.
                if std::mem::replace(&mut self.comp_held, false) {
                    self.shown = false;
                }
                self.latched = false;
                effect = self.settle_holders().or(effect);
            }
            PanelInput::HolderReveal | PanelInput::HolderConceal => {}
            PanelInput::CornerEntered => {
                if self.corner_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.corner_inside = true;
                self.clear_deadline();
                if self.mode == PanelMode::Hidden {
                    if !self.transient_revealed {
                        effect = Some(PanelEffect::Reveal {
                            trigger: RevealTrigger::Corner,
                        });
                    }
                    self.transient_revealed = true;
                    self.motion.reveal();
                }
            }
            PanelInput::CornerLeft => {
                if !self.corner_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.corner_inside = false;
                if self.transient_revealed && !self.pointer_inside && self.intro_until.is_none() {
                    self.arm_deadline(at, ConcealReason::CornerLeft);
                }
            }
            PanelInput::Hide | PanelInput::Escape => {
                // Command-driven, no latch is needed: a compositor still
                // holding the edge sends nothing until its holders release.
                if self.mode == PanelMode::Hidden && self.resize_start.is_none() && !self.menu_hold
                {
                    self.conceal_now();
                }
            }
            PanelInput::Unpin | PanelInput::PinToggle if self.mode == PanelMode::Pinned => {
                effect = self.release(at).or(effect);
            }
            PanelInput::Undock | PanelInput::DockToggle if self.mode == PanelMode::Docked => {
                // A deliberate undock hides at once when nothing holds the
                // panel; the grace delay only ever forgives pointer overshoot,
                // never a deliberate action. A held undock keeps its transient
                // reveal.
                if self.pointer_inside
                    || self.corner_inside
                    || self.resize_start.is_some()
                    || self.menu_hold
                {
                    effect = self.release(at).or(effect);
                } else {
                    effect = self.change_mode(PanelMode::Hidden).or(effect);
                }
            }
            PanelInput::Release => effect = self.release(at).or(effect),
            PanelInput::Pin | PanelInput::PinToggle => {
                effect = self.change_mode(PanelMode::Pinned).or(effect);
            }
            PanelInput::Dock | PanelInput::DockToggle => {
                effect = self.change_mode(PanelMode::Docked).or(effect);
            }
            PanelInput::SetMode(mode) => effect = self.change_mode(mode).or(effect),
            PanelInput::Unpin | PanelInput::Undock => {}
            PanelInput::PointerEntered => {
                if self.pointer_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.pointer_inside = true;
                self.clear_deadline();
                if self.mode == PanelMode::Hidden && self.motion.visible_fraction() > 0.0 {
                    self.transient_revealed = true;
                    self.motion.reveal();
                }
            }
            PanelInput::PointerLeft => {
                if !self.pointer_inside {
                    return Ok(self.update_since(before, effect));
                }
                self.pointer_inside = false;
                if self.transient_revealed && !self.corner_inside && self.intro_until.is_none() {
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
        if self.mode == PanelMode::Hidden {
            self.intro_until = Some(self.last_update + duration);
            self.transient_revealed = true;
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
        let held = self.corner_inside
            || self.pointer_inside
            || self.resize_start.is_some()
            || self.menu_hold;
        self.menu_hold = false;
        if let Some(start) = self.resize_start.take() {
            self.config.thickness_px = start;
        }
        self.corner_inside = false;
        self.pointer_inside = false;
        if self.holder_plane {
            // The compositor tracks its holders by output name, not by this
            // model's membership; only the local holds just dropped matter.
            let _ = self.settle_holders();
        } else if held && self.transient_revealed && self.intro_until.is_none() {
            self.arm_deadline(self.last_update, ConcealReason::Grace);
        }
    }

    /// Switch between local and command-driven reveal/conceal (see the module
    /// docs). The host calls this whenever the compositor's holder-plane
    /// capability changes, and must default to `false` on any doubt.
    ///
    /// Going command-driven drops the local grace deadline: a transient reveal
    /// the local hints still hold waits for the compositor's verdict (it
    /// re-states one for every hidden mode report the host replays), and one
    /// they no longer hold conceals at once. Going local resumes the normal
    /// rules from the current membership, arming grace for an unheld reveal.
    pub fn set_holder_plane(&mut self, available: bool) {
        if self.holder_plane == available {
            return;
        }
        self.holder_plane = available;
        self.comp_held = false;
        self.latched = false;
        self.clear_deadline();
        if self.mode != PanelMode::Hidden || !self.transient_revealed {
            return;
        }
        let hinted = self.pointer_inside || self.corner_inside;
        if available {
            if !hinted {
                let _ = self.settle_holders();
            }
        } else if !hinted && !self.shown && self.intro_until.is_none() {
            self.arm_deadline(self.last_update, ConcealReason::Grace);
        }
    }

    pub const fn holder_plane(&self) -> bool {
        self.holder_plane
    }

    pub fn snapshot(&self) -> PanelSnapshot {
        let visible_fraction = self.motion.visible_fraction();
        PanelSnapshot {
            mode: self.mode,
            transient_revealed: self.transient_revealed,
            visible_fraction,
            target_fraction: self.motion.target(),
            velocity_per_second: self.motion.velocity_per_second(),
            thickness_px: self.config.thickness_px,
            // Keep the surface mapped until the outgoing animation finishes.
            mapped: self.mode != PanelMode::Hidden
                || self.transient_revealed
                || visible_fraction > 0.0,
            exclusive_zone_px: if self.mode == PanelMode::Docked {
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

        let mut intro_ended = false;
        if let Some(deadline) = self.intro_until
            && deadline <= at
        {
            self.intro_until = None;
            if self.holder_plane {
                intro_ended = true;
            } else if self.transient_revealed && !self.pointer_inside && !self.corner_inside {
                self.arm_deadline(deadline, ConcealReason::Grace);
            }
        }
        let mut effect = None;
        if let Some(deadline) = self.hide_at
            && deadline <= at
            && self.transient_revealed
            && !self.pointer_inside
            && !self.corner_inside
            && !self.menu_hold
        {
            let before_deadline = deadline.saturating_sub(self.last_update);
            self.motion.advance(before_deadline);
            self.transient_revealed = false;
            self.shown = false;
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
        if intro_ended {
            effect = self.settle_holders().or(effect);
        }
        Ok(effect)
    }

    fn change_mode(&mut self, mode: PanelMode) -> Option<PanelEffect> {
        let changed = self.mode != mode;
        // A deliberate hide from a persistent mode is reported to the
        // compositor, whose verdict on that report may still name a holder
        // (the corner menu, the pointer on the hotspot): it must not reopen
        // the panel. Its next release clears the latch.
        if changed {
            self.latched = self.holder_plane && mode == PanelMode::Hidden;
        }
        // Persistent panels have no holders, so there is no verdict to keep.
        self.comp_held = false;
        self.mode = mode;
        self.transient_revealed = false;
        self.shown = false;
        self.intro_until = None;
        self.clear_deadline();
        if mode == PanelMode::Hidden {
            self.motion.conceal();
        } else {
            self.motion.reveal();
        }
        changed.then_some(PanelEffect::ModeChanged { mode })
    }

    fn release(&mut self, at: Duration) -> Option<PanelEffect> {
        if self.mode == PanelMode::Hidden {
            return None;
        }
        self.mode = PanelMode::Hidden;
        self.transient_revealed = true;
        self.shown = false;
        self.latched = false;
        self.motion.reveal();
        self.clear_deadline();
        if self.holder_plane {
            // The hidden mode report draws the compositor's verdict; until it
            // lands the reveal counts as held, so no local hold ending first
            // can conceal a panel the pointer still holds.
            self.comp_held = true;
        } else if !self.pointer_inside && !self.corner_inside && self.intro_until.is_none() {
            self.arm_deadline(at, ConcealReason::Grace);
        }
        Some(PanelEffect::ModeChanged {
            mode: PanelMode::Hidden,
        })
    }

    /// Command-driven conceal: a transient reveal with neither a compositor
    /// holder nor a local hold (menu, resize, intro, explicit show) ends now.
    fn settle_holders(&mut self) -> Option<PanelEffect> {
        let local_hold = self.menu_hold
            || self.resize_start.is_some()
            || self.intro_until.is_some()
            || self.shown;
        if !self.holder_plane
            || self.mode != PanelMode::Hidden
            || !self.transient_revealed
            || self.comp_held
            || local_hold
        {
            return None;
        }
        self.conceal_now();
        Some(PanelEffect::Conceal {
            reason: ConcealReason::Holders,
        })
    }

    fn conceal_now(&mut self) {
        self.transient_revealed = false;
        self.shown = false;
        self.clear_deadline();
        self.motion.conceal();
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
        // Command-driven, the conceal delay is the compositor's alone.
        if self.resize_start.is_some() || self.menu_hold || self.holder_plane {
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
    ThicknessBudget {
        edge: super::Edge,
        requested: f32,
        max: f32,
    },
    Motion(MotionError),
}

impl Display for PanelConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThicknessBudget {
                edge,
                requested,
                max,
            } => {
                write!(
                    formatter,
                    "panel {edge:?} thickness {requested} exceeds output budget {max}"
                )
            }
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

    #[test]
    fn open_menu_holds_reveal_until_closed() {
        for persistent in [None, Some(PanelMode::Pinned), Some(PanelMode::Docked)] {
            let mut panel = panel();
            for input in [
                PanelInput::CornerEntered,
                PanelInput::MenuHold(true),
                PanelInput::CornerLeft,
                PanelInput::PointerEntered,
                PanelInput::PointerLeft,
                PanelInput::Hide,
                PanelInput::Escape,
                PanelInput::Toggle,
            ] {
                panel.apply(Duration::ZERO, input).unwrap();
            }
            panel.tick(Duration::from_secs(10)).unwrap();
            assert!(panel.snapshot().transient_revealed);
            assert_eq!(panel.next_deadline(), None);
            if let Some(mode) = persistent {
                let update = panel
                    .apply(Duration::from_secs(10), PanelInput::SetMode(mode))
                    .unwrap();
                assert_eq!(update.effect, Some(PanelEffect::ModeChanged { mode }));
            }
            panel
                .apply(Duration::from_secs(10), PanelInput::MenuHold(false))
                .unwrap();
            assert_eq!(
                panel.snapshot().mode,
                persistent.unwrap_or(PanelMode::Hidden)
            );
            assert_eq!(
                panel.snapshot().target_fraction,
                if persistent.is_some() { 1.0 } else { 0.0 }
            );
            assert!(!panel.snapshot().transient_revealed);
            assert_eq!(panel.next_deadline(), None);
        }
    }

    #[test]
    fn menu_hold_does_not_reveal_hidden_panel_and_preserves_other_holds() {
        let mut panel = panel();
        panel
            .apply(Duration::ZERO, PanelInput::MenuHold(true))
            .unwrap();
        assert!(!panel.snapshot().mapped);
        panel
            .apply(Duration::ZERO, PanelInput::CornerEntered)
            .unwrap();
        panel
            .apply(Duration::ZERO, PanelInput::MenuHold(false))
            .unwrap();
        assert!(panel.snapshot().transient_revealed);
        panel.apply(Duration::ZERO, PanelInput::CornerLeft).unwrap();
        panel.tick(Duration::from_secs(1)).unwrap();
        assert!(!panel.snapshot().transient_revealed);
    }

    #[test]
    fn menu_acquisition_at_grace_deadline_prevents_conceal() {
        let mut panel = panel();
        panel
            .apply(Duration::ZERO, PanelInput::CornerEntered)
            .unwrap();
        panel.apply(Duration::ZERO, PanelInput::CornerLeft).unwrap();
        let update = panel
            .apply(Duration::from_millis(800), PanelInput::MenuHold(true))
            .unwrap();
        assert_eq!(update.effect, None);
        assert!(update.snapshot.transient_revealed);
        assert_eq!(update.snapshot.target_fraction, 1.0);
        panel.tick(Duration::from_secs(10)).unwrap();
        assert!(panel.snapshot().transient_revealed);
    }

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
    fn deliberate_undock_hides_immediately_only_when_unheld() {
        let mut panel = panel();
        for held in [false, true] {
            if held {
                panel
                    .apply(Duration::ZERO, PanelInput::CornerEntered)
                    .unwrap();
            }
            let pinned = panel.apply(Duration::ZERO, PanelInput::DockToggle).unwrap();
            assert_eq!(panel.snapshot().mode, PanelMode::Docked);
            assert_eq!(panel.snapshot().hide_at, None);
            assert_eq!(
                pinned.effect,
                Some(PanelEffect::ModeChanged {
                    mode: PanelMode::Docked
                })
            );
            let unpinned = panel.apply(Duration::ZERO, PanelInput::DockToggle).unwrap();
            assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
            assert_eq!(panel.snapshot().transient_revealed, held);
            assert_eq!(
                unpinned.effect,
                Some(PanelEffect::ModeChanged {
                    mode: PanelMode::Hidden
                })
            );
            // Held keeps the transient reveal open; unheld hides at once —
            // no grace deadline, conceal motion already targeted at hidden.
            assert_eq!(panel.snapshot().hide_at, None);
            if !held {
                assert_eq!(panel.snapshot().conceal_reason, None);
                assert_eq!(panel.snapshot().target_fraction, 0.0);
            }
        }
    }

    #[test]
    fn runtime_resize_validates_range_and_keeps_docked_zone_live() {
        let mut panel = panel();
        panel.apply(Duration::ZERO, PanelInput::Dock).unwrap();
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
        assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
        assert!(panel.snapshot().transient_revealed);
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

    /// The capable half of the pair above: command-driven, the intro's end
    /// arms no grace. The compositor's verdict alone decides.
    #[test]
    fn intro_expires_into_holder_verdict_when_command_driven() {
        for comp_holds in [false, true] {
            let mut panel = commanded();
            panel.start_intro(Duration::from_secs(2));
            if comp_holds {
                let update = panel
                    .apply(Duration::from_millis(500), PanelInput::HolderReveal)
                    .unwrap();
                assert_eq!(update.effect, None, "already revealed by the intro");
            }
            panel.tick(Duration::from_secs(1)).unwrap();
            assert_eq!(panel.wake(), PanelWake::WakeAt(Duration::from_secs(2)));
            let update = panel.tick(Duration::from_secs(2)).unwrap();
            assert_eq!(update.snapshot.hide_at, None, "no local grace");
            assert_eq!(update.snapshot.transient_revealed, comp_holds);
            if comp_holds {
                assert_eq!(update.effect, None);
                let update = panel
                    .apply(Duration::from_secs(3), PanelInput::HolderConceal)
                    .unwrap();
                assert!(!update.snapshot.transient_revealed);
                assert_eq!(update.effect, Some(holders_conceal()));
            } else {
                assert_eq!(update.snapshot.target_fraction, 0.0);
                assert_eq!(update.effect, Some(holders_conceal()));
            }
            assert_eq!(panel.next_deadline(), None);
        }
    }

    fn commanded() -> PanelStateMachine {
        let mut panel = panel();
        panel.set_holder_plane(true);
        panel
    }

    fn holders_conceal() -> PanelEffect {
        PanelEffect::Conceal {
            reason: ConcealReason::Holders,
        }
    }

    #[test]
    fn command_driven_membership_neither_reveals_nor_arms_grace() {
        let mut panel = commanded();
        for input in [PanelInput::CornerEntered, PanelInput::PointerEntered] {
            let update = panel.apply(Duration::ZERO, input).unwrap();
            assert_eq!(update.effect, None);
            assert!(!update.snapshot.transient_revealed, "{input:?} reveals only locally");
        }
        assert!(panel.snapshot().corner_inside && panel.snapshot().pointer_inside);
        let update = panel
            .apply(Duration::from_millis(100), PanelInput::HolderReveal)
            .unwrap();
        assert_eq!(
            update.effect,
            Some(PanelEffect::Reveal {
                trigger: RevealTrigger::Holders
            })
        );
        for input in [PanelInput::CornerLeft, PanelInput::PointerLeft] {
            panel.apply(Duration::from_millis(200), input).unwrap();
        }
        assert_eq!(panel.snapshot().hide_at, None, "the conceal delay is comp's");
        panel.tick(Duration::from_secs(10)).unwrap();
        assert!(panel.snapshot().transient_revealed);
        let update = panel
            .apply(Duration::from_secs(10), PanelInput::HolderConceal)
            .unwrap();
        assert_eq!(update.effect, Some(holders_conceal()));
        assert_eq!(update.snapshot.target_fraction, 0.0, "at once: comp served the delay");
    }

    #[test]
    fn local_holds_defer_holder_conceal_until_they_end() {
        let at = Duration::ZERO;
        // The corner menu.
        let mut panel = commanded();
        panel.apply(at, PanelInput::HolderReveal).unwrap();
        panel.apply(at, PanelInput::MenuHold(true)).unwrap();
        panel.apply(at, PanelInput::HolderConceal).unwrap();
        assert!(panel.snapshot().transient_revealed, "the open menu holds");
        let update = panel.apply(at, PanelInput::MenuHold(false)).unwrap();
        assert_eq!(update.effect, Some(holders_conceal()));
        // A resize: completion is the effect persistence needs.
        let mut panel = commanded();
        panel.apply(at, PanelInput::HolderReveal).unwrap();
        panel.apply(at, PanelInput::ResizeStarted).unwrap();
        panel.apply(at, PanelInput::HolderConceal).unwrap();
        assert!(panel.snapshot().transient_revealed, "the resize holds");
        let update = panel.apply(at, PanelInput::ResizeCompleted).unwrap();
        assert_eq!(update.effect, Some(PanelEffect::ResizeCompleted));
        assert!(!update.snapshot.transient_revealed);
        // An explicit show survives a repeated verdict, and ends when the
        // pointer has come and gone (a hold, then its release).
        let mut panel = commanded();
        panel.apply(at, PanelInput::Reveal).unwrap();
        panel.apply(at, PanelInput::HolderConceal).unwrap();
        assert!(panel.snapshot().transient_revealed, "a restated verdict is not a release");
        panel.apply(at, PanelInput::HolderReveal).unwrap();
        let update = panel.apply(at, PanelInput::HolderConceal).unwrap();
        assert_eq!(update.effect, Some(holders_conceal()));
    }

    #[test]
    fn deliberate_hide_is_not_undone_by_the_holder_verdict() {
        let at = Duration::ZERO;
        // Hide from the menu of a pinned panel: the hidden report's verdict
        // still names the menu, and must not reopen the panel.
        let mut panel = commanded();
        panel.apply(at, PanelInput::Pin).unwrap();
        panel.apply(at, PanelInput::SetMode(PanelMode::Hidden)).unwrap();
        panel.apply(at, PanelInput::HolderReveal).unwrap();
        assert!(!panel.snapshot().transient_revealed, "latched");
        panel.apply(at, PanelInput::HolderConceal).unwrap();
        let update = panel.apply(at, PanelInput::HolderReveal).unwrap();
        assert!(update.snapshot.transient_revealed, "the next hold reveals again");
        // A held undock is not deliberate concealment: it keeps its reveal,
        // counted as held, until the verdict on its hidden report arrives.
        let mut panel = commanded();
        panel.apply(at, PanelInput::Dock).unwrap();
        panel.apply(at, PanelInput::CornerEntered).unwrap();
        panel.apply(at, PanelInput::DockToggle).unwrap();
        assert!(panel.snapshot().transient_revealed);
        panel.apply(at, PanelInput::MenuHold(true)).unwrap();
        panel.apply(at, PanelInput::MenuHold(false)).unwrap();
        assert!(panel.snapshot().transient_revealed, "the verdict is still to come");
        let update = panel.apply(at, PanelInput::HolderConceal).unwrap();
        assert_eq!(update.effect, Some(holders_conceal()));
        // An unheld undock hides at once, as locally.
        let mut panel = commanded();
        panel.apply(at, PanelInput::Dock).unwrap();
        panel.apply(at, PanelInput::DockToggle).unwrap();
        assert!(!panel.snapshot().transient_revealed);
        assert_eq!(panel.snapshot().target_fraction, 0.0);
    }

    #[test]
    fn holder_plane_switch_defaults_to_local_rules() {
        // Commands are ignored while the plane is inactive.
        let mut panel = panel();
        for input in [PanelInput::HolderReveal, PanelInput::HolderConceal] {
            let update = panel.apply(Duration::ZERO, input).unwrap();
            assert!(!update.changed && update.effect.is_none(), "{input:?}");
        }
        // Going command-driven drops local grace; an unheld reveal ends now.
        panel.apply(Duration::ZERO, PanelInput::CornerEntered).unwrap();
        panel.apply(Duration::ZERO, PanelInput::CornerLeft).unwrap();
        assert_eq!(panel.snapshot().hide_at, Some(Duration::from_millis(800)));
        panel.set_holder_plane(true);
        assert_eq!(panel.snapshot().hide_at, None);
        assert!(!panel.snapshot().transient_revealed);
        // A reveal the local hints still hold waits for comp's verdict.
        let mut panel = panel_with_corner_held();
        panel.set_holder_plane(true);
        panel.tick(Duration::from_secs(10)).unwrap();
        assert!(panel.snapshot().transient_revealed);
        panel
            .apply(Duration::from_secs(10), PanelInput::HolderConceal)
            .unwrap();
        assert!(!panel.snapshot().transient_revealed);
        // Falling back to local rules re-arms grace for an unheld reveal.
        let mut panel = commanded();
        panel.apply(Duration::ZERO, PanelInput::HolderReveal).unwrap();
        panel.set_holder_plane(false);
        assert_eq!(panel.snapshot().hide_at, Some(Duration::from_millis(800)));
        let update = panel.tick(Duration::from_millis(800)).unwrap();
        assert_eq!(
            update.effect,
            Some(PanelEffect::Conceal {
                reason: ConcealReason::Grace
            })
        );
        // A held one keeps waiting for its membership to end, as before.
        let mut panel = commanded();
        panel.apply(Duration::ZERO, PanelInput::PointerEntered).unwrap();
        panel.apply(Duration::ZERO, PanelInput::HolderReveal).unwrap();
        panel.set_holder_plane(false);
        assert_eq!(panel.snapshot().hide_at, None);
        panel
            .apply(Duration::from_secs(1), PanelInput::PointerLeft)
            .unwrap();
        assert_eq!(panel.snapshot().hide_at, Some(Duration::from_millis(1800)));
    }

    fn panel_with_corner_held() -> PanelStateMachine {
        let mut panel = panel();
        panel
            .apply(Duration::ZERO, PanelInput::CornerEntered)
            .unwrap();
        panel
    }

    /// Acceptance gate (shell design §4.3): hover, focus and Escape never
    /// change the persistent mode, whatever holds the panel and whichever
    /// driver runs it. Focus reaches the model as the compositor's holder
    /// commands; hover as corner/pointer membership.
    #[test]
    fn hover_focus_and_escape_never_change_mode() {
        use PanelInput::*;
        let holders: [&[PanelInput]; 6] = [
            &[],
            &[CornerEntered],
            &[PointerEntered],
            &[HolderReveal],
            &[MenuHold(true)],
            &[CornerEntered, PointerEntered, HolderReveal, MenuHold(true)],
        ];
        let gestures = [
            CornerEntered,
            PointerEntered,
            HolderReveal,
            Escape,
            CornerLeft,
            PointerLeft,
            HolderConceal,
            Escape,
            MenuHold(true),
            MenuHold(false),
            Escape,
        ];
        for plane in [false, true] {
            for mode in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
                for held in holders {
                    let mut panel = panel();
                    panel.set_holder_plane(plane);
                    panel.apply(Duration::ZERO, PanelInput::SetMode(mode)).unwrap();
                    for input in held {
                        panel.apply(Duration::ZERO, *input).unwrap();
                    }
                    for (step, input) in gestures.into_iter().enumerate() {
                        let at = Duration::from_millis(100 * step as u64);
                        let update = panel.apply(at, input).unwrap();
                        assert!(
                            !matches!(update.effect, Some(PanelEffect::ModeChanged { .. })),
                            "plane={plane} mode={mode:?} held={held:?}: {input:?} changed mode"
                        );
                        assert_eq!(update.snapshot.mode, mode);
                    }
                    let update = panel.tick(Duration::from_secs(10)).unwrap();
                    assert_eq!(update.snapshot.mode, mode, "plane={plane} held={held:?}");
                    if mode != PanelMode::Hidden {
                        assert_eq!(update.snapshot.target_fraction, 1.0, "still shown");
                        assert_eq!(
                            update.snapshot.exclusive_zone_px,
                            if mode == PanelMode::Docked { 100.0 } else { 0.0 },
                            "reservation untouched"
                        );
                    }
                }
            }
        }
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
        assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
        assert!(panel.snapshot().transient_revealed);
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
        assert_eq!(panel.snapshot().mode, PanelMode::Hidden);
        assert!(panel.snapshot().transient_revealed);
        assert_eq!(panel.snapshot().hide_at, None);
    }

    #[test]
    fn intro_does_not_change_restored_docks() {
        let mut panel = panel();
        panel.apply(Duration::ZERO, PanelInput::Dock).unwrap();
        panel.start_intro(Duration::from_secs(2));
        panel.tick(Duration::from_secs(3)).unwrap();
        assert_eq!(panel.snapshot().mode, PanelMode::Docked);
        assert_eq!(panel.next_deadline(), None);
    }
}
