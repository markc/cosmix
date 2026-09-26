//! Four-panel shell aggregation and clockwise corner mapping.
//!
//! This model deliberately consumes semantic [`CornerEvent`] values rather
//! than pointer samples. Q-0's detector and the future compositor topic source
//! are interchangeable producers; neither is a window host concern.

use std::collections::BTreeMap;
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
    /// Quoin's scene-only frame. Generic shell hosts can choose their policy.
    suppress_empty_edges: bool,
    thickness_set: [bool; 4],
    /// Authored page extents: while a listed page is its edge's active page,
    /// that edge is at least this thick. Never written into the remembered
    /// thickness, so the edge returns to it when another page is shown.
    page_minimums: [BTreeMap<String, f32>; 4],
    /// The panel whose surface holds the keyboard, as the host last reported.
    keyboard_focus: Option<Edge>,
    /// Whether the host has ever reported keyboard focus. A host without
    /// per-panel surfaces never does, and only then does Escape fall back to
    /// every mapped panel.
    focus_reported: bool,
    focus_directive: FocusDirective,
    /// When an ungranted cycle request gives up (see [`FOCUS_GRANT_TIMEOUT`]).
    focus_grant_deadline: Option<Duration>,
    /// The pending request came from a named activation that revealed a
    /// hidden edge: if it lapses ungranted, that reveal ends with it.
    focus_request_revealed: bool,
    last_update: Duration,
}

/// How long a focus-cycle target may ask for the keyboard without receiving
/// it. Comp grants an Exclusive layer only when it is actually shown and no
/// session lock is active; an ungranted request must not linger and seize
/// the keyboard later (on unlock, or once shown) with no user action. A
/// granted request that a lock then takes the keyboard from ends through the
/// ordinary landed-then-left rule of `keyboard_focus_observed`.
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
            suppress_empty_edges: false,
            thickness_set: [false; 4],
            page_minimums: std::array::from_fn(|_| BTreeMap::new()),
            keyboard_focus: None,
            focus_reported: false,
            focus_directive: FocusDirective::Follow,
            focus_grant_deadline: None,
            focus_request_revealed: false,
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

    /// The edge as presented. While the active page declares an authored
    /// extent, a remembered (restored, resized) thickness is widened to it;
    /// an edge with nothing remembered presents the extent itself, which is
    /// then its seed as well as its floor. The extent is capped by the edge's
    /// resize range and by the output budget: two docked opposite edges
    /// share what their remembered zones leave, in proportion to how much
    /// each widens, so their zones never exceed the output. A remembered
    /// `settled_thickness_px` never records a widening; an unremembered one
    /// reports the presented seed, so the first save persists what was shown.
    pub fn panel(&self, edge: Edge) -> PanelSnapshot {
        let mut panel = self.remembered_panel(edge);
        let Some(wanted) = self.wanted_thickness(edge, &panel) else {
            return panel;
        };
        let (opposite, extent) = self.opposite_extent(edge);
        let other = self.remembered_panel(opposite);
        let docked = panel.exclusive_zone_px > 0.0;
        let other_extra = if other.exclusive_zone_px > 0.0 {
            self.wanted_thickness(opposite, &other)
                .map_or(0.0, |other_wanted| other_wanted - other.thickness_px)
        } else {
            0.0
        };
        let own_extra = if docked { wanted - panel.thickness_px } else { 0.0 };
        let [own, opp] = fit_docked_pair(
            (extent - 1.0).max(2.0),
            [panel.exclusive_zone_px, other.exclusive_zone_px],
            [own_extra, other_extra],
        );
        let remembered = self.thickness_set[edge.index()];
        let thickness = if docked {
            panel.thickness_px + own
        } else {
            // An overlay fits beside the opposite edge's presented zone.
            let fitted = wanted.min(self.max_thickness_for_zone(edge, other.exclusive_zone_px + opp));
            if remembered { fitted.max(panel.thickness_px) } else { fitted }
        };
        panel.thickness_px = thickness;
        if docked {
            panel.exclusive_zone_px = thickness;
        }
        if !remembered && !panel.resize_active {
            panel.settled_thickness_px = thickness;
        }
        panel
    }

    /// The smallest size a resize of `edge` may leave while its active page
    /// declares an authored extent: that extent within the resize range and
    /// the output budget. [`Self::resize_thickness`] raises any smaller input
    /// to it; steppers read it to report the size they will actually save.
    pub fn resize_floor(&self, edge: Edge) -> Option<f32> {
        let range = super::resize_thickness_range(edge);
        let minimum = self.active_page_minimum(edge)?;
        Some(minimum.clamp(*range.start(), *range.end()).min(self.max_thickness(edge)))
    }

    /// The thickness `edge`'s active page asks for: its authored extent within
    /// the resize range, over the remembered thickness when one exists. `None`
    /// without an authored extent.
    fn wanted_thickness(&self, edge: Edge, remembered: &PanelSnapshot) -> Option<f32> {
        let range = super::resize_thickness_range(edge);
        let minimum = self
            .active_page_minimum(edge)?
            .clamp(*range.start(), *range.end());
        Some(if self.thickness_set[edge.index()] {
            remembered.thickness_px.max(minimum)
        } else {
            minimum
        })
    }

    /// The edge without any page minimum: what a resize or restore changed.
    fn remembered_panel(&self, edge: Edge) -> PanelSnapshot {
        let mut panel = self.panels[edge.index()].snapshot();
        if self.edge_is_empty(edge) {
            // Keep the saved mode and dimensions, but never present or reserve
            // an empty edge. A later registration resumes those preferences.
            panel.transient_revealed = false;
            panel.intro_revealed = false;
            panel.mapped = false;
            panel.visible_fraction = 0.0;
            panel.target_fraction = 0.0;
            panel.velocity_per_second = 0.0;
            panel.exclusive_zone_px = 0.0;
            panel.hide_at = None;
        }
        panel
    }

    pub fn suppress_empty_edges(&mut self, enabled: bool) {
        self.suppress_empty_edges = enabled;
    }

    pub fn empty_edges_suppressed(&self) -> bool {
        self.suppress_empty_edges
    }

    fn edge_is_empty(&self, edge: Edge) -> bool {
        self.suppress_empty_edges && self.carousel(edge).page_ids().is_empty()
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
            // Remembered values budget against remembered values: an opposite
            // page's minimum must not shrink what this edge remembers.
            let (opposite, _) = self.opposite_extent(edge);
            thickness.min(self.max_thickness_against(edge, self.remembered_panel(opposite)))
        } else {
            thickness
        };
        self.panels[edge.index()].restore_thickness(thickness)?;
        self.thickness_set[edge.index()] = true;
        Ok(())
    }

    /// Maximum thickness that leaves space for the opposite panel and work area.
    pub fn max_thickness(&self, edge: Edge) -> f32 {
        let (opposite, _) = self.opposite_extent(edge);
        self.max_thickness_against(edge, self.panel(opposite))
    }

    fn opposite_extent(&self, edge: Edge) -> (Edge, f32) {
        match edge {
            Edge::Left => (Edge::Right, self.geometry.width()),
            Edge::Right => (Edge::Left, self.geometry.width()),
            Edge::Top => (Edge::Bottom, self.geometry.height()),
            Edge::Bottom => (Edge::Top, self.geometry.height()),
        }
    }

    fn max_thickness_against(&self, edge: Edge, opposite: PanelSnapshot) -> f32 {
        self.max_thickness_for_zone(edge, opposite.exclusive_zone_px)
    }

    fn max_thickness_for_zone(&self, edge: Edge, opposite_zone: f32) -> f32 {
        let (_, extent) = self.opposite_extent(edge);
        // Leave a positive extent for the opposing surface and the work area.
        // A zero-sized layer configure means "client chooses", not a valid
        // empty viewport, and can otherwise disconnect opposing panels.
        let minimum = 1.0_f32.min(extent / 4.0);
        (extent - opposite_zone.max(minimum) - minimum).max(minimum)
    }

    /// Record `page`'s authored extent on `edge` (`None` clears it). While
    /// that page is the edge's active page the edge is at least this thick;
    /// the remembered thickness is untouched, so showing another page returns
    /// the edge to it. Returns whether anything changed.
    pub fn set_page_minimum_thickness(
        &mut self,
        edge: Edge,
        page: &str,
        minimum: Option<f32>,
    ) -> bool {
        let minimums = &mut self.page_minimums[edge.index()];
        match minimum.filter(|minimum| minimum.is_finite() && *minimum > 0.0) {
            Some(minimum) => minimums.insert(page.to_owned(), minimum) != Some(minimum),
            None => minimums.remove(page).is_some(),
        }
    }

    /// Take the outgoing model's authored page extents. They describe mounted
    /// pages rather than an output, so every replacement keeps them.
    pub fn adopt_page_minimums(&mut self, outgoing: &Self) {
        self.page_minimums = outgoing.page_minimums.clone();
    }

    /// The active page's authored extent on `edge`, if it declared one.
    pub fn active_page_minimum(&self, edge: Edge) -> Option<f32> {
        let page = self.carousel(edge).active_id()?;
        self.page_minimums[edge.index()].get(page).copied()
    }

    fn fit_output_budget(&mut self) {
        let thickness_set = self.thickness_set;
        for (a, b, extent) in [
            (Edge::Left, Edge::Right, self.geometry.width()),
            (Edge::Top, Edge::Bottom, self.geometry.height()),
        ] {
            let total = self.remembered_panel(a).exclusive_zone_px
                + self.remembered_panel(b).exclusive_zone_px;
            if total > (extent - 1.0).max(2.0) {
                let ratio = (extent - 1.0).max(2.0) / total;
                for edge in [a, b] {
                    let size = (self.remembered_panel(edge).thickness_px * ratio).max(1.0);
                    let _ = self.panels[edge.index()].restore_thickness(size);
                }
            }
            for edge in [a, b] {
                let _ = self.restore_thickness(edge, self.remembered_panel(edge).thickness_px);
            }
        }
        self.thickness_set = thickness_set;
    }

    pub fn resize_thickness(&mut self, edge: Edge, thickness: f32) -> Result<(), PanelConfigError> {
        let max = self.max_thickness(edge);
        // Below the shown page's authored extent a resize would be invisible
        // and still saved: clamp it, so what is shown is what is saved.
        let range = super::resize_thickness_range(edge);
        let thickness = match self.resize_floor(edge) {
            Some(floor) if thickness.is_finite() => thickness.max(floor),
            _ => thickness,
        };
        if thickness > max {
            return Err(PanelConfigError::ThicknessBudget {
                edge,
                requested: thickness,
                max,
            });
        }
        if max < *range.start() && thickness == max {
            return self.restore_thickness(edge, thickness);
        }
        self.panels[edge.index()].resize_thickness(thickness, range)?;
        self.thickness_set[edge.index()] = true;
        Ok(())
    }

    /// Cold-start discovery is independent of compositor corner membership.
    pub fn start_intro(&mut self, duration: Duration) {
        for (panel, carousel) in self.panels.iter_mut().zip(&mut self.carousels) {
            if self.suppress_empty_edges && carousel.page_ids().is_empty() {
                continue;
            }
            let before = panel.snapshot();
            panel.start_intro(duration);
            if before.mode == PanelMode::Hidden && !before.transient_revealed {
                carousel.restore_selection();
            }
        }
    }

    /// Hand transient reveal/conceal to the compositor's holder plane, or take
    /// it back (see [`PanelStateMachine::set_holder_plane`]). The grace given to
    /// [`ShellModel::new`] only applies while the plane is inactive: the dev
    /// host and a compositor that does not report the plane.
    /// `at` is when the capability changed; a fall back to local rules gives
    /// an unheld reveal its full grace from then. Like any input it advances
    /// the model to `at` first, so later inputs cannot be timed before it.
    pub fn set_holder_plane(
        &mut self,
        available: bool,
        at: Duration,
    ) -> Result<[PanelUpdate; 4], PanelTimeError> {
        self.ensure_monotonic(at)?;
        let [left, bottom, right, top] = &mut self.panels;
        let updates = [
            left.set_holder_plane(available, at)?,
            bottom.set_holder_plane(available, at)?,
            right.set_holder_plane(available, at)?,
            top.set_holder_plane(available, at)?,
        ];
        self.last_update = at;
        Ok(updates)
    }

    /// Whether the compositor's holder plane drives transient visibility.
    pub fn holder_plane(&self) -> bool {
        self.panels[0].holder_plane()
    }

    /// Output migration preserves live panel state, including stored sizes and pages.
    pub fn carry_live_state(&mut self, outgoing: &Self) {
        self.panels = outgoing.panels.clone();
        for panel in &mut self.panels {
            panel.leave_output();
        }
        self.carousels = outgoing.carousels.clone();
        self.thickness_set = outgoing.thickness_set;
        self.page_minimums = outgoing.page_minimums.clone();
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
        if self.edge_is_empty(edge) && input.requires_content() {
            self.last_update = at;
            return Ok(PanelUpdate {
                changed: false,
                snapshot: self.panel(edge),
                effect: None,
            });
        }
        self.apply_panel_input(edge, at, input)
    }

    /// Restore a persistent preference even before its scene registers. Empty
    /// edges remain unmapped and reserve no space; interactive input cannot
    /// use this restoration path.
    pub fn restore_mode(
        &mut self,
        edge: Edge,
        at: Duration,
        mode: PanelMode,
    ) -> Result<PanelUpdate, PanelTimeError> {
        self.ensure_monotonic(at)?;
        self.apply_panel_input(edge, at, PanelInput::SetMode(mode))
    }

    fn apply_panel_input(
        &mut self,
        edge: Edge,
        at: Duration,
        input: PanelInput,
    ) -> Result<PanelUpdate, PanelTimeError> {
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
            let _ = self.restore_thickness(edge, self.remembered_panel(edge).thickness_px);
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
        if self.edge_is_empty(edge) {
            update.snapshot = self.panel(edge);
        }
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
        match stop {
            FocusStop::Panel(edge) => self.request_keyboard_focus(edge, at),
            FocusStop::Application => {
                self.focus_directive = self.release_directive();
                self.focus_grant_deadline = None;
            }
        }
        stop
    }

    /// Ask for the keyboard in `edge`'s panel: the focus cycle's stops and
    /// named activation (panel doc §6, whose hidden edge is revealed first)
    /// both come here. The request lapses unless comp grants it by `at` +
    /// [`FOCUS_GRANT_TIMEOUT`], and ends once focus has landed there and
    /// then left, on Escape, or at the next cycle stop. An unmapped panel has
    /// no surface to focus and is not asked for. Never changes a mode.
    pub fn request_keyboard_focus(&mut self, edge: Edge, at: Duration) {
        if !self.panel(edge).mapped {
            return;
        }
        self.focus_directive = FocusDirective::Panel(edge);
        self.focus_grant_deadline =
            (self.keyboard_focus != Some(edge)).then_some(at + FOCUS_GRANT_TIMEOUT);
        self.focus_request_revealed = false;
    }

    /// A named activation's request ([`Self::request_keyboard_focus`]).
    /// `revealed`: the activation revealed a hidden edge for it. Should comp
    /// never grant the keyboard (a session lock, a higher exclusive layer),
    /// that reveal ends when the request lapses — an open panel without the
    /// keyboard is one Escape cannot reach, since Escape goes to the
    /// application.
    pub fn request_activation_focus(&mut self, edge: Edge, at: Duration, revealed: bool) {
        self.request_keyboard_focus(edge, at);
        self.focus_request_revealed = revealed && self.focus_grant_deadline.is_some();
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
        let mut updates = [
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
            let lapsed = self.focus_grant_deadline.is_some_and(|deadline| deadline <= at);
            self.focus_directive = FocusDirective::Follow;
            self.focus_grant_deadline = None;
            // An activation's reveal that never got the keyboard ends too.
            if lapsed && std::mem::take(&mut self.focus_request_revealed) {
                let panel = self.panel(edge);
                if panel.mode == PanelMode::Hidden && panel.transient_revealed {
                    let update = self.panels[edge.index()].apply(at, PanelInput::Hide)?;
                    let ticked = updates[edge.index()];
                    updates[edge.index()] = PanelUpdate {
                        changed: ticked.changed || update.changed,
                        snapshot: update.snapshot,
                        effect: update.effect.or(ticked.effect),
                    };
                }
            }
        }
        self.last_update = at;
        for edge in Edge::ALL {
            if self.edge_is_empty(edge) {
                updates[edge.index()] = PanelUpdate {
                    changed: false,
                    snapshot: self.panel(edge),
                    effect: None,
                };
            }
        }
        Ok(updates)
    }

    pub fn wake(&self) -> PanelWake {
        let mut earliest = self.focus_grant_deadline;
        for edge in Edge::ALL {
            if self.edge_is_empty(edge) {
                continue;
            }
            let panel = &self.panels[edge.index()];
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
        Edge::ALL
            .into_iter()
            .filter(|&edge| !self.edge_is_empty(edge))
            .filter_map(|edge| self.panels[edge.index()].next_deadline())
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

/// Share a docked opposite pair's widening within `budget`. `zones` are the
/// remembered exclusive zones (already fitted by `fit_output_budget`), and
/// `extras` how far each edge's presented thickness departs from them (zero
/// for an edge that is not docked). A narrowing always applies and frees
/// room; the widenings share what is left, in proportion. Returns each
/// edge's applied departure.
fn fit_docked_pair(budget: f32, zones: [f32; 2], extras: [f32; 2]) -> [f32; 2] {
    let room = budget - zones[0] - zones[1] - extras[0].min(0.0) - extras[1].min(0.0);
    let growth = extras[0].max(0.0) + extras[1].max(0.0);
    let scale = if growth > room { room.max(0.0) / growth } else { 1.0 };
    extras.map(|extra| extra.min(0.0) + extra.max(0.0) * scale)
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

#[cfg(test)]
mod empty_edge_tests {
    use super::*;
    use crate::core::{Corner, CornerTrigger};

    fn model() -> ShellModel {
        let mut model = ShellModel::new(
            OutputKey::new("test-output").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        model.suppress_empty_edges(true);
        model
    }

    #[test]
    fn empty_hotspots_and_intro_do_nothing_until_registration() {
        let mut model = model();
        model.set_carousel(Edge::Left, Carousel::declared(["scene-tools"]).unwrap());
        model.start_intro(Duration::from_secs(2));
        let event = CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: Duration::ZERO,
            trigger: CornerTrigger::Compositor,
        };
        let update = model.corner_event(Duration::ZERO, event).unwrap().unwrap();
        assert!(!update.changed);
        assert!(update.effect.is_none());
        for edge in Edge::ALL {
            for input in [
                PanelInput::Pin,
                PanelInput::Dock,
                PanelInput::Reveal,
                PanelInput::HolderReveal,
                PanelInput::SetMode(PanelMode::Docked),
            ] {
                assert!(
                    !model
                        .panel_input(edge, Duration::ZERO, input)
                        .unwrap()
                        .changed
                );
            }
            assert!(!model.panel(edge).mapped);
            assert_eq!(model.panel(edge).exclusive_zone_px, 0.0);
        }
        assert_eq!(model.wake(), PanelWake::Idle);
        assert_eq!(model.next_deadline(), None);
        model
            .carousel_mut(Edge::Left)
            .register("scene-tools")
            .unwrap();
        model.corner_event(Duration::ZERO, event).unwrap();
        assert!(model.panel(Edge::Left).transient_revealed);
        assert_eq!(model.carousel(Edge::Left).active_id(), Some("scene-tools"));
    }

    #[test]
    fn saved_dock_reserves_nothing_until_content_registers_or_after_removal() {
        let mut model = model();
        model
            .restore_mode(Edge::Bottom, Duration::ZERO, PanelMode::Docked)
            .unwrap();
        model.tick(Duration::from_secs(1)).unwrap();
        assert_eq!(model.panel(Edge::Bottom).mode, PanelMode::Docked);
        assert!(!model.panel(Edge::Bottom).mapped);
        assert_eq!(model.panel(Edge::Bottom).exclusive_zone_px, 0.0);
        assert_eq!(model.wake(), PanelWake::Idle);
        model
            .carousel_mut(Edge::Bottom)
            .register("scene-panel")
            .unwrap();
        assert!(model.panel(Edge::Bottom).mapped);
        assert!(model.panel(Edge::Bottom).exclusive_zone_px > 0.0);
        model
            .carousel_mut(Edge::Bottom)
            .remove("scene-panel")
            .unwrap();
        assert!(!model.panel(Edge::Bottom).mapped);
        assert_eq!(model.panel(Edge::Bottom).exclusive_zone_px, 0.0);
    }
}

#[cfg(test)]
mod page_minimum_tests {
    use super::*;

    fn model() -> ShellModel {
        let mut model = ShellModel::new(
            OutputKey::new("test-output").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        model
            .set_carousel(Edge::Left, Carousel::new(["launcher", "notes"]).unwrap());
        model.restore_thickness(Edge::Left, 422.0).unwrap();
        model
    }

    #[test]
    fn active_page_minimum_widens_the_saved_thickness_while_shown() {
        let mut model = model();
        assert!(model.set_page_minimum_thickness(Edge::Left, "launcher", Some(440.0)));
        assert!(!model.set_page_minimum_thickness(Edge::Left, "launcher", Some(440.0)));
        model
            .restore_mode(Edge::Left, Duration::ZERO, PanelMode::Docked)
            .unwrap();
        let panel = model.panel(Edge::Left);
        assert_eq!(panel.thickness_px, 440.0);
        assert_eq!(panel.exclusive_zone_px, 440.0);
        assert_eq!(panel.settled_thickness_px, 422.0);
        // Geometry fitting and dock entry work on the remembered value.
        model.set_geometry(LogicalSize::new(1200.0, 900.0).unwrap());
        model.set_mode(Edge::Left, Duration::ZERO, PanelMode::Pinned).unwrap();
        model.set_mode(Edge::Left, Duration::ZERO, PanelMode::Docked).unwrap();
        assert_eq!(model.panel(Edge::Left).settled_thickness_px, 422.0);
        assert_eq!(model.panel(Edge::Left).thickness_px, 440.0);
        model.carousel_mut(Edge::Left).select_id("notes");
        assert_eq!(model.panel(Edge::Left).thickness_px, 422.0);
        assert_eq!(model.panel(Edge::Left).exclusive_zone_px, 422.0);
        model.carousel_mut(Edge::Left).select_id("launcher");
        assert_eq!(model.panel(Edge::Left).thickness_px, 440.0);
        // Clearing the request, or a smaller one, leaves the saved width.
        model.set_page_minimum_thickness(Edge::Left, "launcher", Some(100.0));
        assert_eq!(model.panel(Edge::Left).thickness_px, 422.0);
        assert!(model.set_page_minimum_thickness(Edge::Left, "launcher", None));
        assert_eq!(model.panel(Edge::Left).thickness_px, 422.0);
    }

    #[test]
    fn page_minimum_is_capped_by_the_resize_range() {
        let mut model = model();
        model.set_page_minimum_thickness(Edge::Left, "launcher", Some(50_000.0));
        let panel = model.panel(Edge::Left);
        assert_eq!(panel.thickness_px, *crate::core::resize_thickness_range(Edge::Left).end());
        assert_eq!(panel.settled_thickness_px, 422.0);
    }

    /// Review M1: two docked opposite edges both widened by their pages share
    /// the room their remembered zones leave; the zones never exceed the
    /// output (800 px: remembered 300 + 300, pages 440 and 480, which would
    /// need 920).
    #[test]
    fn widened_opposite_docked_edges_fit_the_output_together() {
        let mut model = ShellModel::new(
            OutputKey::new("test-output").unwrap(),
            LogicalSize::new(800.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        for (edge, page, minimum) in [(Edge::Left, "wide", 440.0), (Edge::Right, "wider", 480.0)] {
            model.set_carousel(edge, Carousel::new([page]).unwrap());
            model.restore_thickness(edge, 300.0).unwrap();
            model.restore_mode(edge, Duration::ZERO, PanelMode::Docked).unwrap();
            model.set_page_minimum_thickness(edge, page, Some(minimum));
        }
        let (left, right) = (model.panel(Edge::Left), model.panel(Edge::Right));
        let zones = left.exclusive_zone_px + right.exclusive_zone_px;
        assert!(zones <= 799.0 + 1e-3, "zones {zones} overflow 800");
        // Shared in proportion to each widening (140 and 180 of 199 spare).
        assert!(left.thickness_px > 300.0 && left.thickness_px < 440.0, "{}", left.thickness_px);
        assert!(right.thickness_px > 300.0 && right.thickness_px < 480.0, "{}", right.thickness_px);
        let ratio = (left.thickness_px - 300.0) / (right.thickness_px - 300.0);
        assert!((ratio - 140.0 / 180.0).abs() < 1e-3, "{ratio}");
        assert_eq!((left.settled_thickness_px, right.settled_thickness_px), (300.0, 300.0));
        // One widened edge beside a plain docked one gets all the spare room.
        model.set_page_minimum_thickness(Edge::Right, "wider", None);
        assert_eq!(model.panel(Edge::Left).thickness_px, 440.0);
        assert_eq!(model.panel(Edge::Right).thickness_px, 300.0);
    }

    /// Review M2: an edge with nothing remembered presents its page's extent
    /// itself (seed and floor), even below the output-derived default, and
    /// reports it as settled so the first save persists what was shown.
    #[test]
    fn a_fresh_edge_presents_the_authored_extent() {
        let mut model = ShellModel::new(
            OutputKey::new("test-output").unwrap(),
            LogicalSize::new(1920.0, 1080.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        model.set_carousel(Edge::Bottom, Carousel::new(["scene-panel"]).unwrap());
        model.restore_mode(Edge::Bottom, Duration::ZERO, PanelMode::Docked).unwrap();
        assert!(model.panel(Edge::Bottom).thickness_px > 100.0, "default is taller");
        model.set_page_minimum_thickness(Edge::Bottom, "scene-panel", Some(52.0));
        let bottom = model.panel(Edge::Bottom);
        assert_eq!(
            (bottom.thickness_px, bottom.exclusive_zone_px, bottom.settled_thickness_px),
            (52.0, 52.0, 52.0)
        );
        // Once a size is remembered, the extent is only a floor over it.
        model.restore_thickness(Edge::Bottom, 70.0).unwrap();
        assert_eq!(model.panel(Edge::Bottom).thickness_px, 70.0);
    }

    /// Review m1: a resize below the shown page's extent is clamped to it, so
    /// the value saved is the value shown.
    #[test]
    fn a_resize_below_the_shown_extent_saves_the_extent() {
        let mut model = model();
        model.set_page_minimum_thickness(Edge::Left, "launcher", Some(440.0));
        model.resize_thickness(Edge::Left, 300.0).unwrap();
        let panel = model.panel(Edge::Left);
        assert_eq!((panel.thickness_px, panel.settled_thickness_px), (440.0, 440.0));
        // A page without an extent resizes freely.
        model.carousel_mut(Edge::Left).select_id("notes");
        model.resize_thickness(Edge::Left, 300.0).unwrap();
        assert_eq!(model.panel(Edge::Left).settled_thickness_px, 300.0);
    }
}
