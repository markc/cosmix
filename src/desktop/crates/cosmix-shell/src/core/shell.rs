//! Four-panel shell aggregation and clockwise corner mapping.
//!
//! This model deliberately consumes semantic [`CornerEvent`] values rather
//! than pointer samples. Q-0's detector and the future compositor topic source
//! are interchangeable producers; neither is a window host concern.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use super::{
    Carousel, CarouselError, CornerEvent, Edge, FocusDirective, FocusStop, LogicalSize, OutputKey,
    PanelConfig, PanelConfigError, PanelInput, PanelMode, PanelSnapshot, PanelStateMachine,
    PanelTimeError, PanelUpdate, PanelWake, next_focus_stop, seed_panel_thickness,
};

/// Complete pure shell state for one output.
#[derive(Clone, Debug)]
pub struct ShellModel {
    output: OutputKey,
    geometry: LogicalSize,
    panels: [PanelStateMachine; 4],
    carousels: [Carousel; 4],
    thickness_set: [bool; 4],
    /// The panel whose surface holds the keyboard, as the host last reported.
    keyboard_focus: Option<Edge>,
    /// Whether the host has ever reported keyboard focus. A host without
    /// per-panel surfaces never does, and only then does Escape fall back to
    /// every mapped panel.
    focus_reported: bool,
    focus_directive: FocusDirective,
    /// When an ungranted cycle request gives up (see [`FOCUS_GRANT_TIMEOUT`]).
    focus_grant_deadline: Option<Duration>,
    last_update: Duration,
}

/// How long a focus-cycle target may ask for the keyboard without receiving
/// it. Comp grants an Exclusive layer only when it is actually shown; an
/// ungranted request must not linger and seize the keyboard later with no
/// user action.
pub const FOCUS_GRANT_TIMEOUT: Duration = Duration::from_millis(500);

impl ShellModel {
    pub fn new(
        output: OutputKey,
        geometry: LogicalSize,
        start_at: Duration,
        grace: Duration,
        motion_time: Duration,
    ) -> Result<Self, ShellError> {
        let build_panel = |edge| {
            let config =
                PanelConfig::new(seed_panel_thickness(edge, geometry), grace, motion_time)?;
            PanelStateMachine::new(config, start_at)
        };
        let panels = [
            build_panel(Edge::Left)?,
            build_panel(Edge::Bottom)?,
            build_panel(Edge::Right)?,
            build_panel(Edge::Top)?,
        ];
        Ok(Self {
            output,
            geometry,
            panels,
            carousels: std::array::from_fn(|_| Carousel::empty()),
            thickness_set: [false; 4],
            keyboard_focus: None,
            focus_reported: false,
            focus_directive: FocusDirective::Follow,
            focus_grant_deadline: None,
            last_update: start_at,
        })
    }

    pub fn output(&self) -> &OutputKey {
        &self.output
    }

    pub const fn geometry(&self) -> LogicalSize {
        self.geometry
    }

    pub const fn last_update(&self) -> Duration {
        self.last_update
    }

    /// Update current host geometry and fit panels within its exclusive budget.
    pub fn set_geometry(&mut self, geometry: LogicalSize) {
        self.geometry = geometry;
        self.fit_output_budget();
    }

    pub fn panel(&self, edge: Edge) -> PanelSnapshot {
        self.panels[edge.index()].snapshot()
    }

    pub fn carousel(&self, edge: Edge) -> &Carousel {
        &self.carousels[edge.index()]
    }

    pub fn carousel_mut(&mut self, edge: Edge) -> &mut Carousel {
        &mut self.carousels[edge.index()]
    }

    pub fn set_carousel(&mut self, edge: Edge, carousel: Carousel) {
        self.carousels[edge.index()] = carousel;
    }

    /// Reconcile an edge's declared order, with new names starting empty.
    ///
    /// This is the config-driven construction path: registering attaches
    /// content to the declared names afterwards — a declared name fills its
    /// slot in order, an undeclared name appends to the tail. Re-declaring
    /// preserves registrations, selection and memory by name. Live names no
    /// longer declared become tail entries in their previous relative order.
    pub fn declare_carousel(
        &mut self,
        edge: Edge,
        page_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(), CarouselError> {
        self.carousels[edge.index()].redeclare(page_ids)
    }

    /// Whether this edge has a restored, resized or scene-seeded thickness.
    pub fn has_remembered_thickness(&self, edge: Edge) -> bool {
        self.thickness_set[edge.index()]
    }

    /// Restore an edge preference, preventing later scenes from replacing it.
    pub fn restore_thickness(
        &mut self,
        edge: Edge,
        thickness: f32,
    ) -> Result<(), PanelConfigError> {
        let thickness = if thickness.is_finite() && thickness > 0.0 {
            thickness.min(self.max_thickness(edge))
        } else {
            thickness
        };
        self.panels[edge.index()].restore_thickness(thickness)?;
        self.thickness_set[edge.index()] = true;
        Ok(())
    }

    /// Maximum thickness that leaves space for the opposite panel and work area.
    pub fn max_thickness(&self, edge: Edge) -> f32 {
        let (opposite, extent) = match edge {
            Edge::Left => (Edge::Right, self.geometry.width()),
            Edge::Right => (Edge::Left, self.geometry.width()),
            Edge::Top => (Edge::Bottom, self.geometry.height()),
            Edge::Bottom => (Edge::Top, self.geometry.height()),
        };
        // Leave a positive extent for the opposing surface and the work area.
        // A zero-sized layer configure means "client chooses", not a valid
        // empty viewport, and can otherwise disconnect opposing panels.
        let minimum = 1.0_f32.min(extent / 4.0);
        (extent - self.panel(opposite).exclusive_zone_px.max(minimum) - minimum).max(minimum)
    }

    fn fit_output_budget(&mut self) {
        let thickness_set = self.thickness_set;
        for (a, b, extent) in [
            (Edge::Left, Edge::Right, self.geometry.width()),
            (Edge::Top, Edge::Bottom, self.geometry.height()),
        ] {
            let total = self.panel(a).exclusive_zone_px + self.panel(b).exclusive_zone_px;
            if total > (extent - 1.0).max(2.0) {
                let ratio = (extent - 1.0).max(2.0) / total;
                for edge in [a, b] {
                    let size = (self.panel(edge).thickness_px * ratio).max(1.0);
                    let _ = self.panels[edge.index()].restore_thickness(size);
                }
            }
            for edge in [a, b] {
                let _ = self.restore_thickness(edge, self.panel(edge).thickness_px);
            }
        }
        self.thickness_set = thickness_set;
    }

    pub fn resize_thickness(&mut self, edge: Edge, thickness: f32) -> Result<(), PanelConfigError> {
        let max = self.max_thickness(edge);
        if thickness > max {
            return Err(PanelConfigError::ThicknessBudget {
                edge,
                requested: thickness,
                max,
            });
        }
        if max < *super::RESIZE_THICKNESS_RANGE.start() && thickness == max {
            return self.restore_thickness(edge, thickness);
        }
        self.panels[edge.index()].resize_thickness(thickness)?;
        self.thickness_set[edge.index()] = true;
        Ok(())
    }

    /// Cold-start discovery is independent of compositor corner membership.
    pub fn start_intro(&mut self, duration: Duration) {
        for (panel, carousel) in self.panels.iter_mut().zip(&mut self.carousels) {
            let before = panel.snapshot();
            panel.start_intro(duration);
            if before.mode == PanelMode::Hidden && !before.transient_revealed {
                carousel.restore_selection();
            }
        }
    }

    /// Output migration preserves live panel state, including stored sizes and pages.
    pub fn carry_live_state(&mut self, outgoing: &Self) {
        self.panels = outgoing.panels.clone();
        for panel in &mut self.panels {
            panel.leave_output();
        }
        self.carousels = outgoing.carousels.clone();
        self.thickness_set = outgoing.thickness_set;
        self.last_update = outgoing.last_update;
        self.fit_output_budget();
    }

    pub fn panel_input(
        &mut self,
        edge: Edge,
        at: Duration,
        input: PanelInput,
    ) -> Result<PanelUpdate, PanelTimeError> {
        self.ensure_monotonic(at)?;
        // Only Dock clamps into the opposing-edge thickness budget here: Docked
        // is the only mode that claims an exclusive zone, so it is the only one
        // that competes for it. Pin/PinToggle deliberately do NOT clamp -- a
        // Pinned overlay claims no zone and may legitimately overhang an
        // opposing Docked panel (the same way a transient reveal already does).
        // fit_output_budget() still clamps on every geometry change regardless
        // of mode, so this only affects the initial thickness on entry.
        if matches!(
            input,
            PanelInput::Dock | PanelInput::DockToggle | PanelInput::SetMode(PanelMode::Docked)
        ) {
            let remembered = self.thickness_set[edge.index()];
            let _ = self.restore_thickness(edge, self.panel(edge).thickness_px);
            self.thickness_set[edge.index()] = remembered;
        }
        let panel = &mut self.panels[edge.index()];
        let before = panel.snapshot();
        // Resolve expired grace/intro timers before deciding whether this input
        // reveals a hidden edge, including when no frame tick ran in between.
        let advanced = panel.tick(at)?;
        let hidden =
            advanced.snapshot.mode == PanelMode::Hidden && !advanced.snapshot.transient_revealed;
        let mut update = panel.apply(at, input)?;
        update.changed = update.snapshot != before;
        update.effect = update.effect.or(advanced.effect);
        if hidden
            && (update.snapshot.mode != PanelMode::Hidden || update.snapshot.transient_revealed)
        {
            self.carousels[edge.index()].restore_selection();
        }
        self.last_update = at;
        Ok(update)
    }

    /// Persistent mode ingress for future input adapters; no corner wiring implied.
    pub fn set_mode(
        &mut self,
        edge: Edge,
        at: Duration,
        mode: PanelMode,
    ) -> Result<PanelUpdate, PanelTimeError> {
        self.panel_input(edge, at, PanelInput::SetMode(mode))
    }

    /// Apply compositor corner containment or clicks to the mapped clockwise edge.
    pub fn corner_event(
        &mut self,
        at: Duration,
        event: CornerEvent,
    ) -> Result<Option<PanelUpdate>, PanelTimeError> {
        match event {
            CornerEvent::Entered { corner, .. } => self
                .panel_input(corner.summoned_edge(), at, PanelInput::CornerEntered)
                .map(Some),
            CornerEvent::Left { corner } => self
                .panel_input(corner.summoned_edge(), at, PanelInput::CornerLeft)
                .map(Some),
            CornerEvent::Clicked { corner } => self
                .panel_input(corner.summoned_edge(), at, PanelInput::DockToggle)
                .map(Some),
        }
    }

    /// The panel whose surface holds the keyboard, as the host last reported.
    pub const fn keyboard_focus(&self) -> Option<Edge> {
        self.keyboard_focus
    }

    pub const fn focus_directive(&self) -> FocusDirective {
        self.focus_directive
    }

    /// Host report of which panel surface now holds the keyboard. A
    /// [`FocusDirective::Panel`] ends once focus has landed there and then
    /// left; a [`FocusDirective::Release`] ends once no panel holds it.
    pub fn keyboard_focus_observed(&mut self, edge: Option<Edge>) {
        let previous = std::mem::replace(&mut self.keyboard_focus, edge);
        self.focus_reported = true;
        if matches!(self.focus_directive, FocusDirective::Panel(target) if edge == Some(target)) {
            self.focus_grant_deadline = None;
        }
        self.focus_directive = match self.focus_directive {
            FocusDirective::Panel(target) if previous == Some(target) && edge != Some(target) => {
                FocusDirective::Follow
            }
            FocusDirective::Release if edge.is_none() => FocusDirective::Follow,
            directive => directive,
        };
    }

    /// The "cycle focus through shell panels" binding (shell doc §5): the
    /// visible pinned and docked panels on this output in [`Edge::ALL`]
    /// order, then back to the application. Never changes a mode. A stop
    /// that has not received the keyboard by `at` + [`FOCUS_GRANT_TIMEOUT`]
    /// stops asking for it.
    pub fn cycle_keyboard_focus(&mut self, at: Duration) -> FocusStop {
        let current = match self.focus_directive {
            FocusDirective::Panel(edge) => Some(edge),
            _ => self.keyboard_focus,
        };
        let stops: Vec<Edge> = Edge::ALL
            .into_iter()
            .filter(|&edge| {
                let panel = self.panel(edge);
                panel.mode != PanelMode::Hidden && panel.mapped
            })
            .collect();
        let stop = next_focus_stop(&stops, current);
        self.focus_directive = match stop {
            FocusStop::Panel(edge) => FocusDirective::Panel(edge),
            FocusStop::Application => self.release_directive(),
        };
        self.focus_grant_deadline = match stop {
            FocusStop::Panel(edge) if self.keyboard_focus != Some(edge) => {
                Some(at + FOCUS_GRANT_TIMEOUT)
            }
            _ => None,
        };
        stop
    }

    /// Escape from a focused panel (shell doc §4.3). A transient reveal hides
    /// (latching while the pointer is still inside); a pinned or docked panel
    /// changes nothing. Either way keyboard focus is given back. Only a host
    /// that has never reported focus (one without per-panel surfaces) sends
    /// the Escape to every mapped panel, as before focus was tracked; once
    /// focus is reported, "no panel holds it" addresses no panel.
    pub fn escape(&mut self, at: Duration) -> Result<Vec<(Edge, PanelUpdate)>, PanelTimeError> {
        let focused = self.keyboard_focus.or(match self.focus_directive {
            FocusDirective::Panel(edge) => Some(edge),
            _ => None,
        });
        let targets: Vec<Edge> = match focused {
            Some(edge) => vec![edge],
            None if self.focus_reported => Vec::new(),
            None => Edge::ALL
                .into_iter()
                .filter(|&edge| self.panel(edge).mapped)
                .collect(),
        };
        let mut updates = Vec::with_capacity(targets.len());
        for edge in targets {
            updates.push((edge, self.panel_input(edge, at, PanelInput::Escape)?));
        }
        self.focus_directive = self.release_directive();
        self.focus_grant_deadline = None;
        Ok(updates)
    }

    /// Only a panel that holds the keyboard has anything to give back; a
    /// release nobody observes ending would leave every panel refusing focus.
    fn release_directive(&self) -> FocusDirective {
        if self.keyboard_focus.is_some() {
            FocusDirective::Release
        } else {
            FocusDirective::Follow
        }
    }

    pub fn tick(&mut self, at: Duration) -> Result<[PanelUpdate; 4], PanelTimeError> {
        self.ensure_monotonic(at)?;
        let [left, bottom, right, top] = &mut self.panels;
        let updates = [
            left.tick(at)?,
            bottom.tick(at)?,
            right.tick(at)?,
            top.tick(at)?,
        ];
        // A focus target that has finished unmapping has no surface to hold
        // the keyboard, and one comp has not granted it in time is not being
        // shown; either way a later reveal must never inherit the grab.
        if let FocusDirective::Panel(edge) = self.focus_directive
            && (!self.panel(edge).mapped
                || self.focus_grant_deadline.is_some_and(|deadline| deadline <= at))
        {
            self.focus_directive = FocusDirective::Follow;
            self.focus_grant_deadline = None;
        }
        self.last_update = at;
        Ok(updates)
    }

    pub fn wake(&self) -> PanelWake {
        let mut earliest = self.focus_grant_deadline;
        for panel in &self.panels {
            match panel.wake() {
                PanelWake::Animate => return PanelWake::Animate,
                PanelWake::WakeAt(deadline) => {
                    earliest =
                        Some(earliest.map_or(deadline, |current: Duration| current.min(deadline)));
                }
                PanelWake::Idle => {}
            }
        }
        earliest.map_or(PanelWake::Idle, PanelWake::WakeAt)
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        self.panels
            .iter()
            .filter_map(PanelStateMachine::next_deadline)
            .chain(self.focus_grant_deadline)
            .min()
    }

    fn ensure_monotonic(&self, at: Duration) -> Result<(), PanelTimeError> {
        if at < self.last_update {
            return Err(PanelTimeError {
                previous: self.last_update,
                update: at,
            });
        }
        Ok(())
    }
}

/// Shell construction failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShellError {
    Panel(PanelConfigError),
}

impl From<PanelConfigError> for ShellError {
    fn from(value: PanelConfigError) -> Self {
        Self::Panel(value)
    }
}

impl Display for ShellError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panel(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for ShellError {}
