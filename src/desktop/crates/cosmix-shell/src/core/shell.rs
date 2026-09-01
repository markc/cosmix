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
    PanelSnapshot, PanelStateMachine, PanelTimeError, PanelUpdate, PanelWake, seed_panel_thickness,
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

    /// Update current host geometry without changing stored panel thicknesses.
    pub fn set_geometry(&mut self, geometry: LogicalSize) {
        self.geometry = geometry;
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

    pub fn panel_input(
        &mut self,
        edge: Edge,
        at: Duration,
        input: PanelInput,
    ) -> Result<PanelUpdate, PanelTimeError> {
        self.ensure_monotonic(at)?;
        let update = self.panels[edge.index()].apply(at, input)?;
        self.last_update = at;
        Ok(update)
    }

    /// Apply compositor corner containment to the mapped clockwise edge.
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
