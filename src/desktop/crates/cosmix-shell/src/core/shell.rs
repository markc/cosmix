//! Four-panel shell aggregation and clockwise corner mapping.
//!
//! This model deliberately consumes semantic [`CornerEvent`] values rather
//! than pointer samples. Q-0's detector and the future compositor topic source
//! are interchangeable producers; neither is a window host concern.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use super::{
    Carousel, CornerEvent, Edge, LogicalSize, OutputKey, PanelConfig, PanelConfigError, PanelInput,
    PanelMode, PanelSnapshot, PanelStateMachine, PanelTimeError, PanelUpdate, PanelWake,
    seed_panel_thickness,
};

/// Complete pure shell state for one output.
#[derive(Clone, Debug)]
pub struct ShellModel {
    output: OutputKey,
    geometry: LogicalSize,
    panels: [PanelStateMachine; 4],
    carousels: [Carousel; 4],
    last_update: Duration,
}

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

    /// Restored thickness has the same validation as a newly constructed panel.
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
        self.panels[edge.index()].restore_thickness(thickness)
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
            return self.panels[edge.index()].restore_thickness(thickness);
        }
        self.panels[edge.index()].resize_thickness(thickness)
    }

    /// Cold-start discovery is independent of compositor corner membership.
    pub fn start_intro(&mut self, duration: Duration) {
        for panel in &mut self.panels {
            panel.start_intro(duration);
        }
    }

    /// Output migration preserves live panel state, including stored sizes and pages.
    pub fn carry_live_state(&mut self, outgoing: &Self) {
        self.panels = outgoing.panels.clone();
        for panel in &mut self.panels {
            panel.leave_output();
        }
        self.carousels = outgoing.carousels.clone();
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
        if matches!(
            input,
            PanelInput::Dock | PanelInput::DockToggle | PanelInput::SetMode(PanelMode::Docked)
        ) {
            let _ = self.restore_thickness(edge, self.panel(edge).thickness_px);
        }
        let update = self.panels[edge.index()].apply(at, input)?;
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

    pub fn tick(&mut self, at: Duration) -> Result<[PanelUpdate; 4], PanelTimeError> {
        self.ensure_monotonic(at)?;
        let [left, bottom, right, top] = &mut self.panels;
        let updates = [
            left.tick(at)?,
            bottom.tick(at)?,
            right.tick(at)?,
            top.tick(at)?,
        ];
        self.last_update = at;
        Ok(updates)
    }

    pub fn wake(&self) -> PanelWake {
        let mut earliest = None;
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
