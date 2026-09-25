use std::sync::Arc;
use std::time::Duration;

use crate::core::{
    CornerEvent, Edge, FocusDirective, LogicalSize, OutputKey, PanelEffect, PanelInput, PanelMode,
    PanelWake, ShellModel,
};

/// Geometry reported by a renderer/window-system host.
#[derive(Clone, Debug, PartialEq)]
pub struct HostGeometry {
    pub output: OutputKey,
    pub logical_size: LogicalSize,
}

/// Renderer-neutral control ingress.
#[cfg_attr(feature = "chrome-core", derive(bevy::prelude::Message))]
#[derive(Clone, Debug, PartialEq)]
pub struct ShellCommand {
    pub output: OutputKey,
    pub at: Duration,
    pub kind: ShellCommandKind,
}

/// Semantic shell actions; no transport is implied.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellCommandKind {
    Scene(super::SceneVerb),
    Quit,
    Geometry(LogicalSize),
    Resize {
        edge: Edge,
        thickness_px: f32,
    },
    /// A scripted, atomic resize that settles and persists in one command —
    /// the Bus/config path, distinct from the grip gesture's per-motion
    /// [`ShellCommandKind::Resize`] (which never persists on its own).
    ResizeCommit {
        edge: Edge,
        thickness_px: f32,
    },
    /// Resize with an application receipt, for request/reply transports.
    ResizeChecked {
        edge: Edge,
        thickness_px: f32,
        request_id: u64,
    },
    Corner(CornerEvent),
    /// The compositor's holder-plane capability changed: `true` hands
    /// transient reveal/conceal to its commands, `false` restores the local
    /// corner/pointer/grace rules. Process-wide, so it applies to whichever
    /// model is current regardless of `output`, and survives model replacement.
    HolderPlane(bool),
    Panel {
        edge: Edge,
        input: PanelInput,
    },
    Carousel {
        edge: Edge,
        input: CarouselInput,
    },
    /// Keyboard focus and the shell's own keys (shell doc §4.3, §5).
    Keyboard(KeyboardCommand),
    /// Register a sub-panel name on `edge` (panel doc §3). The dispatch
    /// reserved the registry seat transactionally before acking; the Model
    /// stage only fills the carousel slot, without revealing or selecting.
    /// `owner` is the broker-attested caller at dispatch, never a
    /// caller-supplied field.
    SubPanelRegister {
        edge: Edge,
        name: String,
        owner: String,
    },
    /// Remove a sub-panel by name (panel doc §3). The name is the address
    /// (§5), so `edge`, `owner` and `accepted_at` are the seat's own values
    /// resolved at dispatch — the registry applies the carousel's removal
    /// landing rule at the Model stage, atomically with the seat, and only
    /// while that exact registration (same owner AND same acceptance
    /// receipt) still stands: a name re-reserved by a later load after this
    /// seat was dropped is a replacement, not this removal's target.
    SubPanelRemove {
        edge: Edge,
        name: String,
        owner: String,
        accepted_at: u64,
    },
    /// Named activation (panel doc §6): show the registered sub-panel `name`
    /// on its edge. Addressed like [`Self::SubPanelRemove`] — `edge`, `owner`
    /// and `accepted_at` are the seat's own values resolved at dispatch, and
    /// the Model stage applies only while that exact registration stands.
    /// The carousel jumps to the page (a named change, never animated); a
    /// `Hidden` edge also gets a transient reveal, which the host holds with
    /// a compositor focus hold. A pinned or docked edge only switches pages:
    /// activation never changes a mode. With `focus` (the default) the
    /// panel then asks for the keyboard, exactly as a focus-cycle stop does;
    /// without it the activation only reveals or switches — the shape a
    /// notification that wants attention but not the keyboard uses.
    SubPanelActivate {
        edge: Edge,
        name: String,
        owner: String,
        accepted_at: u64,
        focus: bool,
    },
}

#[cfg_attr(feature = "chrome-core", derive(bevy::prelude::Message))]
#[derive(Clone, Debug)]
pub struct ShellResizeResult {
    pub request_id: u64,
    pub edge: Edge,
    pub requested: f32,
    pub max: f32,
    pub result: Result<(), ShellResizeError>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShellResizeError {
    OutputChanged,
    Configuration(crate::core::PanelConfigError),
}

/// One edge-attributed semantic transition from the current model update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellEffect {
    pub edge: Edge,
    pub effect: PanelEffect,
}

/// Keyboard ingress that is about focus rather than one edge's mode; the
/// per-edge pin/dock/hide bindings use the precise mode verbs instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardCommand {
    /// Which panel surface now holds the keyboard (`None`: none of them).
    FocusObserved(Option<Edge>),
    /// Move focus to the next visible pinned or docked panel, then back to
    /// the application.
    CycleFocus,
    /// Escape reached a focused panel.
    Escape,
}

/// Carousel controls shared by pointer, keyboard, and future verb adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CarouselInput {
    Next,
    Previous,
    SelectId(String),
}

/// How one edge's active carousel page changed in the current model update
/// (panel doc §5, §8). Only sequential chevron paging carries
/// [`PageChange::Sequential`], so chrome can slide it; every other switch —
/// dots, `page.set`/activate verbs, selection restores, removal landings —
/// is a direct jump and never animates. The marker lives for the single
/// update that applied the change; [`ShellFrame::from_model`] leaves it
/// `None`, so a frame that did not run a carousel command never looks
/// sequential.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PageChange {
    #[default]
    None,
    Sequential {
        /// `Next` pages forward, `Previous` back; sets the slide direction.
        forward: bool,
    },
    Named,
}

/// Layer-shell keyboard policy requested for a panel surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardInteractivity {
    None,
    OnDemand,
    /// Only while the focus cycle or a named activation is moving the
    /// keyboard into this panel: a client cannot focus its own layer on
    /// demand, so they ask for the grab, and drop it to on-demand once comp
    /// grants it (so a click elsewhere can still take focus away).
    Exclusive,
}

/// One edge's complete host/chrome presentation state.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelPresentation {
    pub edge: Edge,
    pub max_thickness_px: f32,
    pub mode: PanelMode,
    pub transient_revealed: bool,
    pub mapped: bool,
    pub visible_fraction: f32,
    pub thickness_px: f32,
    pub resize_active: bool,
    pub settled_thickness_px: f32,
    pub exclusive_zone_px: f32,
    pub keyboard_interactivity: KeyboardInteractivity,
    /// The shell asks for the keyboard in this panel (a focus-cycle stop or
    /// a named activation), until the request ends.
    pub keyboard_requested: bool,
    /// The host reports this panel's surface holds the keyboard.
    pub keyboard_focused: bool,
    pub page_ids: Arc<[String]>,
    pub active_page_id: Option<String>,
    /// Marker for the change that produced `active_page_id` this update, if
    /// any; drives carousel motion (see [`PageChange`]).
    pub page_change: PageChange,
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
    /// Frame-only Quoin refuses commands that would expose an empty edge.
    pub empty_edges_suppressed: bool,
    /// Deferred saved selections, kept separate from the pages currently shown.
    pub pending_page_restores: [Option<String>; 4],
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
                max_thickness_px: model.max_thickness(edge),
                mode: panel.mode,
                transient_revealed: panel.transient_revealed,
                mapped: panel.mapped,
                visible_fraction: panel.visible_fraction,
                thickness_px: panel.thickness_px,
                resize_active: panel.resize_active,
                settled_thickness_px: panel.settled_thickness_px,
                exclusive_zone_px: panel.exclusive_zone_px,
                keyboard_interactivity: match model.focus_directive() {
                    _ if !panel.mapped => KeyboardInteractivity::None,
                    // Refusing focus on every panel hands it back to the
                    // application until the host reports it has left.
                    FocusDirective::Release => KeyboardInteractivity::None,
                    // Exclusive only until comp grants the request: once
                    // the panel holds the keyboard the grab has done its job,
                    // and on-demand lets a click elsewhere take focus away.
                    FocusDirective::Panel(target)
                        if target == edge && model.keyboard_focus() != Some(edge) =>
                    {
                        KeyboardInteractivity::Exclusive
                    }
                    _ => KeyboardInteractivity::OnDemand,
                },
                keyboard_requested: model.focus_directive() == FocusDirective::Panel(edge),
                keyboard_focused: model.keyboard_focus() == Some(edge),
                page_ids: model.carousel(edge).shared_page_ids(),
                active_page_id: model.carousel(edge).active_id().map(str::to_owned),
                page_change: PageChange::None,
            }
        });
        Self {
            empty_edges_suppressed: model.empty_edges_suppressed(),
            pending_page_restores: std::array::from_fn(|index| {
                model
                    .carousel(Edge::ALL[index])
                    .pending_restore()
                    .map(str::to_owned)
            }),
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
