use std::sync::Arc;
use std::time::Duration;

use crate::core::{
    CornerEvent, Edge, LogicalSize, OutputKey, PanelEffect, PanelInput, PanelMode, PanelWake,
    ShellModel,
};

/// Geometry reported by a renderer/window-system host.
#[derive(Clone, Debug, PartialEq)]
pub struct HostGeometry {
    pub output: OutputKey,
    pub logical_size: LogicalSize,
}

/// Renderer-neutral control ingress.
#[cfg_attr(feature = "chrome", derive(bevy::prelude::Message))]
#[derive(Clone, Debug, PartialEq)]
pub struct ShellCommand {
    pub output: OutputKey,
    pub at: Duration,
    pub kind: ShellCommandKind,
}

/// Semantic shell actions; no transport is implied.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellCommandKind {
    Geometry(LogicalSize),
    Corner(CornerEvent),
    Panel { edge: Edge, input: PanelInput },
    Carousel { edge: Edge, input: CarouselInput },
}

/// One edge-attributed semantic transition from the current model update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellEffect {
    pub edge: Edge,
    pub effect: PanelEffect,
}

/// Carousel controls shared by pointer, keyboard, and future verb adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CarouselInput {
    Next,
    Previous,
    SelectId(String),
}

/// Layer-shell keyboard policy requested for a panel surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardInteractivity {
    None,
    OnDemand,
}

/// One edge's complete host/chrome presentation state.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelPresentation {
    pub edge: Edge,
    pub mode: PanelMode,
    pub mapped: bool,
    pub visible_fraction: f32,
    pub thickness_px: f32,
    pub exclusive_zone_px: f32,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub page_ids: Arc<[String]>,
    pub active_page_id: Option<String>,
}

/// Renderer-neutral dynamic content carried by the replayable frame.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellContentPresentation {
    pub bottom_clock_text: Option<String>,
}

/// Complete presentation snapshot reconciled by a [`crate::host::ShellHost`].
#[derive(Clone, Debug, PartialEq)]
pub struct ShellFrame {
    pub geometry: HostGeometry,
    pub panels: [PanelPresentation; 4],
    pub content: ShellContentPresentation,
    pub wake: WakePolicy,
    /// Earliest timer-driven model transition, retained even while animation
    /// also requests frame callbacks.
    pub wake_deadline: Option<Duration>,
}

impl ShellFrame {
    pub fn from_model(model: &ShellModel) -> Self {
        let panels = std::array::from_fn(|index| {
            let edge = Edge::ALL[index];
            let panel = model.panel(edge);
            PanelPresentation {
                edge,
                mode: panel.mode,
                mapped: panel.mapped,
                visible_fraction: panel.visible_fraction,
                thickness_px: panel.thickness_px,
                exclusive_zone_px: panel.exclusive_zone_px,
                keyboard_interactivity: if panel.mapped {
                    KeyboardInteractivity::OnDemand
                } else {
                    KeyboardInteractivity::None
                },
                page_ids: model.carousel(edge).shared_page_ids(),
                active_page_id: model.carousel(edge).active_id().map(str::to_owned),
            }
        });
        Self {
            geometry: HostGeometry {
                output: model.output().clone(),
                logical_size: model.geometry(),
            },
            panels,
            content: ShellContentPresentation::default(),
            wake: model.wake().into(),
            wake_deadline: model.next_deadline(),
        }
    }

    pub fn panel(&self, edge: Edge) -> &PanelPresentation {
        &self.panels[edge.index()]
    }
}

/// Host event-loop demand derived from current model and dynamic frame state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakePolicy {
    Idle,
    WakeAt(Duration),
    Animate,
}

impl From<PanelWake> for WakePolicy {
    fn from(value: PanelWake) -> Self {
        match value {
            PanelWake::Idle => Self::Idle,
            PanelWake::WakeAt(deadline) => Self::WakeAt(deadline),
            PanelWake::Animate => Self::Animate,
        }
    }
}
