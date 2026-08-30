//! Reusable, typed drag-and-drop for CTK applications.
//!
//! Bevy picking deliberately supplies pointer gestures rather than a payload
//! or acceptance protocol. This module adds the missing threshold, one active
//! session, fail-closed target negotiation, a non-pickable UI ghost, and the
//! canonical delivery messages shared with the later OS bridge.
//!
//! # Application contract
//!
//! - Exactly one application acceptance resolver must answer each
//!   [`AcceptanceProposal`]. Duplicate answers fail closed and produce a
//!   warning.
//! - Every source-side click observer must call [`dnd_click_is_blocked`]
//!   before acting. CTK stops bubbling but cannot prevent another observer on
//!   the source from handling the same click.
//! - A [`DropTarget`] must have reachable picking geometry itself or be an
//!   ancestor of hovered picking geometry. Marker-only entities outside the
//!   hover ancestry cannot participate.
//! - A [`DragSource`] should be the entity Bevy actually picks. Decorative
//!   source descendants and every descendant created by [`GhostBuilder`] must
//!   carry [`Pickable::IGNORE`](bevy::picking::Pickable::IGNORE); the ghost
//!   root is ignored automatically, but picking policy is not inherited.
//! - A session whose render target cannot be traced back to a window cannot
//!   react to window cursor-leave, focus-loss, or close events. It remains
//!   bounded by gesture end, Escape, source loss, modal activation, or app
//!   exit; CTK warns once when this residual applies.
//!
//! Phase 1 deliberately differs from the original plan's negotiation loop:
//! [`AcceptanceProposal::selected_action`] is only the modifier-requested
//! action and changes only with modifiers. The negotiated action is a pure
//! function of that request and the matching [`DropAcceptance`]; it is
//! delivered in [`DndDrop`] and exposed in [`DndHighlightChanged`], but is
//! never fed back into a new proposal. This prevents legal resolver answers
//! from oscillating forever.

use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::path::PathBuf;
use std::sync::Arc;

use bevy::app::AppExit;
use bevy::camera::{NormalizedRenderTarget, RenderTarget};
use bevy::ecs::entity::ContainsEntity;
#[cfg(debug_assertions)]
use bevy::ecs::entity::EntityHashSet;
use bevy::ecs::hierarchy::ChildOf;
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::{ButtonInput, ButtonState};
use bevy::log::warn_once;
#[cfg(feature = "os-dnd")]
use bevy::picking::events::PointerState;
use bevy::picking::events::{
    Cancel as PointerCancel, Click, Drag, DragDrop, DragEnd, DragStart, Pointer, Release,
};
use bevy::picking::hover::HoverMap;
use bevy::picking::pointer::{PointerButton, PointerId};
#[cfg(feature = "os-dnd")]
use bevy::picking::pointer::{PointerLocation, PointerPress};
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::ui::widget::ViewportNode;
use bevy::ui::{px, ComputedUiTargetCamera, GlobalZIndex, UiScale};
use bevy::window::{CursorLeft, PrimaryWindow, WindowClosed, WindowFocused};

use crate::interaction::{InteractionPresentationSystems, ModalCoordinator};
use crate::modal_capture::{ensure_modal_capture_plugin, ModalCapture};

/// Logical distance a pointer must travel before an armed drag becomes active.
pub const DRAG_THRESHOLD_PX: f32 = 4.0;

/// DnD ghosts sit above CTK's highest current modal layer (`OVERWRITE_Z = 1110`).
pub const DND_GHOST_Z: i32 = 2_000;

/// Maximum stale-only post-release answers tolerated for one candidate.
///
/// Eight retries cover brief application state churn while keeping a broken
/// resolver bounded. Exceeding the cap cancels the drop instead of risking
/// delivery to a different ancestor.
const MAX_STALE_REPROPOSALS: u8 = 8;

/// An opaque identity for one internal or external transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransferId(pub u64);

/// Where a canonical drop originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DndOrigin {
    Internal(Entity),
    External(TransferId),
}

/// The deliberately small v1 payload family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragPayload {
    Paths(Vec<PathBuf>),
    Text(String),
    Entity(Entity),
}

impl DragPayload {
    fn summary(&self) -> PayloadSummary {
        match self {
            Self::Paths(paths) => PayloadSummary::Paths {
                count: Some(paths.len()),
            },
            Self::Text(text) => PayloadSummary::Text {
                bytes: Some(text.len()),
                chars: Some(text.chars().count()),
            },
            Self::Entity(entity) => PayloadSummary::Entity(*entity),
        }
    }
}

/// Cheap acceptance metadata derived without cloning payload contents.
///
/// Internal resolvers which need path identity can query the [`DragSource`]
/// named by [`DndOrigin::Internal`]. External acceptance happens before the
/// bridge fetches destination-pulled data, so only the offered kind may be
/// available at that stage. `None` means the metric is not yet known; it does
/// not mean zero and must not be treated as an empty payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadSummary {
    Paths {
        count: Option<usize>,
    },
    Text {
        bytes: Option<usize>,
        chars: Option<usize>,
    },
    Entity(Entity),
}

/// Action selected for a drop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DropAction {
    Copy,
    Move,
    #[default]
    Ask,
}

/// Set of actions accepted by a target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ActionMask(u8);

impl ActionMask {
    pub const NONE: Self = Self(0);
    pub const COPY: Self = Self(1 << 0);
    pub const MOVE: Self = Self(1 << 1);
    pub const ASK: Self = Self(1 << 2);
    pub const ALL: Self = Self(Self::COPY.0 | Self::MOVE.0 | Self::ASK.0);

    pub const fn contains(self, action: DropAction) -> bool {
        let bit = match action {
            DropAction::Copy => Self::COPY.0,
            DropAction::Move => Self::MOVE.0,
            DropAction::Ask => Self::ASK.0,
        };
        self.0 & bit != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn first(self) -> Option<DropAction> {
        [DropAction::Copy, DropAction::Move, DropAction::Ask]
            .into_iter()
            .find(|action| self.contains(*action))
    }
}

impl BitOr for ActionMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ActionMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Modifier snapshot used by acceptance and delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl Modifiers {
    fn from_keys(keys: &ButtonInput<KeyCode>) -> Self {
        Self {
            control: keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
            shift: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
            alt: keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]),
            super_key: keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]),
        }
    }

    fn requested_action(self) -> DropAction {
        if self.control {
            DropAction::Copy
        } else if self.shift {
            DropAction::Move
        } else {
            DropAction::Ask
        }
    }
}

/// Identity shared by every proposal for one drag session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProposalId(u64);

/// Monotonic acceptance-context version within one proposal identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalRevision(u64);

impl ProposalRevision {
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Reconstructs a revision from a value that crossed the transport
    /// boundary. Only the `os-dnd` glue needs this — an in-app proposal always
    /// mints its revisions here — so it is dead code with the feature off.
    #[cfg_attr(not(feature = "os-dnd"), expect(dead_code))]
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

/// Identity correlating a canonical drop with its eventual completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeliveryId(u64);

impl DeliveryId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Shared ownership of CTK's one DnD ingress lane.
///
/// Wayland offers have no local button grab, but while their hover is being
/// negotiated they must prevent a local source from arming a second session.
#[derive(Resource, Debug, Default)]
pub(crate) struct DndIngressGuard {
    pub(crate) wayland_active: bool,
}

/// CTK's per-frame acceptance request.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct AcceptanceProposal {
    pub proposal_id: ProposalId,
    pub revision: ProposalRevision,
    pub target: Entity,
    pub origin: DndOrigin,
    pub payload_summary: PayloadSummary,
    pub modifiers: Modifiers,
    pub position: Vec2,
    pub selected_action: DropAction,
}

/// An application's fail-closed response to [`AcceptanceProposal`].
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropAcceptance {
    pub proposal_id: ProposalId,
    pub revision: ProposalRevision,
    pub allowed_actions: ActionMask,
    pub preferred: DropAction,
}

/// Whether an `Ask` delivery needs a Wayland-protocol decision before the
/// application can start the chosen operation.
///
/// This records the delivery path, not the source application: an own-window
/// echo is Wayland-delivered even though its [`DndOrigin`] remains internal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropDecisionRequirement {
    None,
    Wayland,
}

/// Canonical delivery event consumed by applications.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct DndDrop {
    pub origin: DndOrigin,
    pub target: Entity,
    pub payload: DragPayload,
    pub action: DropAction,
    pub modifiers: Modifiers,
    pub delivery_id: DeliveryId,
    pub decision_requirement: DropDecisionRequirement,
}

/// The real operation's result, sent after a [`DndDrop`] resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropOutcome {
    Completed(DropAction),
    Failed,
}

/// Mandatory application response to a canonical delivery.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropComplete {
    pub delivery_id: DeliveryId,
    pub outcome: DropOutcome,
}

/// A delivery became terminal before the application completed it.
///
/// Delivery cancellation is deliberately separate from [`DropComplete`]:
/// adapters use this to withdraw deferred application work without advancing
/// an exactly-once transport completion latch.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DndDeliveryCancelled {
    pub delivery_id: DeliveryId,
}

/// Target highlight transition owned by CTK's acceptance result.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DndHighlightChanged {
    pub target: Entity,
    pub highlighted: bool,
    /// Negotiated action represented by this highlight transition.
    pub action: DropAction,
}

/// Why an internal session ended without delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DndCancelReason {
    Escape,
    PointerCancel,
    CursorLeft,
    WindowUnfocused,
    WindowClosed,
    AppExit,
    SourceDespawned,
    ModalOpened,
    DragEnded,
    Denied,
}

/// Observable cancellation, useful for status UI and the demo.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DndCancelled {
    pub source: Entity,
    pub reason: DndCancelReason,
}

/// Marker for an entity which may receive a drop after app acceptance.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct DropTarget;

/// CTK creates and positions the ghost root; this callback only populates it.
type GhostBuildFn = dyn Fn(Entity, &mut Commands) + Send + Sync + 'static;

// These bounds deliberately duplicate cosmix-wl-dnd's validation: this type
// remains available without the `os-dnd` feature, so it cannot import the
// transport. The gated conversion test pins both copies together.
const WAYLAND_SHM_SLOT_ALIGNMENT: usize = 64;
const MAX_WAYLAND_SHM_POOL_LEN: usize = i32::MAX as usize;
const MAX_WAYLAND_BUFFER_WIDTH: u32 = (i32::MAX as u32) / 4;
const MAX_WAYLAND_BUFFER_HEIGHT: u32 = i32::MAX as u32;

/// CPU-owned premultiplied RGBA8 pixels for a compositor drag icon.
///
/// The physical dimensions remain explicit because Wayland interprets them
/// together with the integer buffer scale. Keeping the complete raster behind
/// an [`Arc`] lets every entity using the same catalogue icon share one cached
/// allocation.
pub struct ExportIconRaster {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    buffer_scale: i32,
    logical_anchor: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportIconRasterError {
    ZeroWidth,
    ZeroHeight,
    PixelLengthOverflow,
    InvalidPixelLength { expected: usize, actual: usize },
    WidthTooLarge(u32),
    HeightTooLarge(u32),
    InvalidBufferScale(i32),
    WidthNotMultipleOfBufferScale { width: u32, buffer_scale: i32 },
    HeightNotMultipleOfBufferScale { height: u32, buffer_scale: i32 },
    ShmPoolTooLarge { required: usize },
}

impl ExportIconRaster {
    /// Constructs premultiplied RGBA8 pixels whose dimensions are safe to
    /// interpret at the declared Wayland buffer scale.
    ///
    /// Each colour channel must be no greater than that pixel's alpha channel.
    /// Debug builds assert this producer contract; release builds avoid a
    /// full-raster validation pass at the drag boundary.
    pub fn new(
        pixels: Vec<u8>,
        width: u32,
        height: u32,
        buffer_scale: i32,
    ) -> Result<Self, ExportIconRasterError> {
        if width == 0 {
            return Err(ExportIconRasterError::ZeroWidth);
        }
        if height == 0 {
            return Err(ExportIconRasterError::ZeroHeight);
        }
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ExportIconRasterError::PixelLengthOverflow)?;
        if width > MAX_WAYLAND_BUFFER_WIDTH {
            return Err(ExportIconRasterError::WidthTooLarge(width));
        }
        if height > MAX_WAYLAND_BUFFER_HEIGHT {
            return Err(ExportIconRasterError::HeightTooLarge(height));
        }
        if buffer_scale <= 0 {
            return Err(ExportIconRasterError::InvalidBufferScale(buffer_scale));
        }
        let scale = buffer_scale as u32;
        if !width.is_multiple_of(scale) {
            return Err(ExportIconRasterError::WidthNotMultipleOfBufferScale {
                width,
                buffer_scale,
            });
        }
        if !height.is_multiple_of(scale) {
            return Err(ExportIconRasterError::HeightNotMultipleOfBufferScale {
                height,
                buffer_scale,
            });
        }
        let pool_len =
            wayland_shm_slot_len(expected).ok_or(ExportIconRasterError::PixelLengthOverflow)?;
        if pool_len > MAX_WAYLAND_SHM_POOL_LEN {
            return Err(ExportIconRasterError::ShmPoolTooLarge { required: pool_len });
        }
        if pixels.len() != expected {
            return Err(ExportIconRasterError::InvalidPixelLength {
                expected,
                actual: pixels.len(),
            });
        }
        debug_assert!(
            pixels.chunks_exact(4).all(|pixel| {
                pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]
            }),
            "ExportIconRaster pixels must be premultiplied RGBA8"
        );
        Ok(Self {
            pixels,
            width,
            height,
            buffer_scale,
            logical_anchor: (width / scale / 2, height / scale / 2),
        })
    }

    #[cfg(feature = "icons")]
    pub(crate) fn with_logical_anchor(mut self, logical_anchor: (u32, u32)) -> Self {
        debug_assert!(logical_anchor.0 <= self.width / self.buffer_scale as u32);
        debug_assert!(logical_anchor.1 <= self.height / self.buffer_scale as u32);
        self.logical_anchor = logical_anchor;
        self
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn buffer_scale(&self) -> i32 {
        self.buffer_scale
    }

    /// Pointer anchor in Wayland surface-logical coordinates.
    pub fn logical_anchor(&self) -> (u32, u32) {
        self.logical_anchor
    }
}

const fn wayland_shm_slot_len(byte_len: usize) -> Option<usize> {
    match byte_len.checked_add(WAYLAND_SHM_SLOT_ALIGNMENT - 1) {
        Some(len) => Some(len & !(WAYLAND_SHM_SLOT_ALIGNMENT - 1)),
        None => None,
    }
}

impl fmt::Debug for ExportIconRaster {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExportIconRaster")
            .field("pixel_bytes", &self.pixels.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("buffer_scale", &self.buffer_scale)
            .field("logical_anchor", &self.logical_anchor)
            .finish()
    }
}

impl fmt::Display for ExportIconRasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ExportIconRasterError {}

#[derive(Clone)]
pub struct GhostBuilder {
    build: Arc<GhostBuildFn>,
}

impl GhostBuilder {
    pub fn new(build: impl Fn(Entity, &mut Commands) + Send + Sync + 'static) -> Self {
        Self {
            build: Arc::new(build),
        }
    }

    pub fn empty() -> Self {
        Self::new(|_, _| {})
    }

    fn populate(&self, root: Entity, commands: &mut Commands) {
        (self.build)(root, commands);
    }
}

impl fmt::Debug for GhostBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GhostBuilder(..)")
    }
}

/// Typed payload and ghost content for a locally draggable entity.
#[derive(Component, Clone, Debug)]
pub struct DragSource {
    payload: DragPayload,
    ghost: GhostBuilder,
    export_icon: Option<Arc<ExportIconRaster>>,
    export_label: Option<String>,
}

impl DragSource {
    pub fn new(payload: DragPayload, ghost: GhostBuilder) -> Self {
        Self {
            payload,
            ghost,
            export_icon: None,
            export_label: None,
        }
    }

    /// Opts this source into a compositor icon once OS export consumes it.
    pub fn with_export_icon(mut self, icon: Arc<ExportIconRaster>) -> Self {
        self.export_icon = Some(icon);
        self
    }

    /// Adds the label composed with the square icon only when OS export begins.
    pub fn with_export_label(mut self, label: String) -> Self {
        self.export_label = Some(label);
        self
    }

    pub fn payload(&self) -> &DragPayload {
        &self.payload
    }

    pub fn export_icon(&self) -> Option<&Arc<ExportIconRaster>> {
        self.export_icon.as_ref()
    }

    pub fn export_label(&self) -> Option<&str> {
        self.export_label.as_deref()
    }
}

/// Present only while a threshold-crossed source must reject its `Click`.
///
/// Source click observers should use [`dnd_click_is_blocked`], matching CTK's
/// fader guard. CTK also stops the event bubbling beyond the source.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct DndClickSuppressed;

/// Returns whether a source click overlaps an active threshold-crossed drag
/// or the retained click/release pair from an Escape-cancelled drag.
pub fn dnd_click_is_blocked(source: Entity, session: &DragSession) -> bool {
    session
        .active
        .as_ref()
        .is_some_and(|active| active.source == source && active.phase == DragPhase::Dragging)
        || session
            .cancelled_click
            .as_ref()
            .is_some_and(|cancelled| cancelled.source == source)
}

/// Public states in the single-session lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    Armed,
    Dragging,
    Dropped,
    Cancelled,
    Exporting,
}

#[derive(Debug)]
struct ActiveDrag {
    phase: DragPhase,
    source: Entity,
    origin: DndOrigin,
    payload: DragPayload,
    payload_summary: PayloadSummary,
    ghost_builder: GhostBuilder,
    #[cfg(feature = "os-dnd")]
    export_icon: Option<Arc<ExportIconRaster>>,
    #[cfg(all(feature = "os-dnd", feature = "icons"))]
    export_label: Option<String>,
    ghost: Option<Entity>,
    pointer_id: PointerId,
    button: PointerButton,
    window: Option<Entity>,
    position: Vec2,
    target_chain: Vec<Entity>,
    candidate_index: usize,
    candidate: Option<Entity>,
    accepted_target: Option<Entity>,
    accepted_action: Option<DropAction>,
    modifiers: Modifiers,
    selected_action: DropAction,
    proposal_id: ProposalId,
    revision: ProposalRevision,
    drop_pending: bool,
    freeze_pending: bool,
    stale_reproposal_count: u8,
    duplicate_response_warned: bool,
}

#[derive(Debug)]
struct CancelledClickSuppression {
    source: Entity,
    pointer_id: PointerId,
    button: PointerButton,
    released: bool,
}

#[derive(Debug)]
struct ExportingDrag {
    source: Entity,
}

impl ActiveDrag {
    fn transition(&mut self, next: DragPhase) -> bool {
        let valid = matches!(
            (self.phase, next),
            (DragPhase::Armed, DragPhase::Dragging | DragPhase::Cancelled)
                | (
                    DragPhase::Dragging,
                    DragPhase::Dropped | DragPhase::Cancelled | DragPhase::Exporting
                )
        );
        if valid {
            self.phase = next;
        }
        valid
    }

    fn bump_revision(&mut self) {
        self.revision.0 = self
            .revision
            .0
            .checked_add(1)
            .expect("DnD proposal revision exhausted");
    }
}

/// At most one active DnD session.
#[derive(Resource, Debug)]
pub struct DragSession {
    active: Option<ActiveDrag>,
    exporting: Option<ExportingDrag>,
    cancelled_click: Option<CancelledClickSuppression>,
    next_proposal_id: u64,
    next_delivery_id: u64,
    #[cfg(feature = "os-dnd")]
    acceptance_invalidation_generation: u64,
}

#[cfg(test)]
#[derive(Resource, Default)]
struct DndTestDiagnostics {
    duplicate_warning_count: usize,
}

/// Ownership transferred from an internal drag into the Phase-5 OS bridge.
///
/// The boundary branch creates this exactly once after synchronous eligibility
/// and Bevy-state preflight. Payload ownership moves into this value; the CTK
/// session retains only its payload-free `Exporting` identity.
///
/// It carries exactly the two things the OS bridge consumes. The rest of the
/// retired drag — its origin, pointer, position, modifiers, in-app accepted
/// target — is deliberately *not* forwarded: the compositor owns the gesture
/// from here, so those values would be a stale second opinion about state the
/// bridge has better sources for.
#[cfg(feature = "os-dnd")]
#[derive(Debug)]
pub(crate) struct ExportHandoff {
    pub source: Entity,
    pub payload: DragPayload,
}

#[cfg(feature = "os-dnd")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExportCandidate {
    pub source: Entity,
    pub pointer_id: PointerId,
    pub button: PointerButton,
}

#[cfg(feature = "os-dnd")]
#[derive(Debug)]
pub(crate) struct ExportMaterial {
    pub paths: Vec<PathBuf>,
    pub icon: Option<Arc<ExportIconRaster>>,
    #[cfg(feature = "icons")]
    pub label: Option<String>,
}

impl Default for DragSession {
    fn default() -> Self {
        Self {
            active: None,
            exporting: None,
            cancelled_click: None,
            next_proposal_id: 1,
            next_delivery_id: 1,
            #[cfg(feature = "os-dnd")]
            acceptance_invalidation_generation: 0,
        }
    }
}

impl DragSession {
    pub fn phase(&self) -> Option<DragPhase> {
        self.active
            .as_ref()
            .map(|active| active.phase)
            .or_else(|| self.exporting.as_ref().map(|_| DragPhase::Exporting))
    }

    pub fn source(&self) -> Option<Entity> {
        self.active
            .as_ref()
            .map(|active| active.source)
            .or_else(|| self.exporting.as_ref().map(|exporting| exporting.source))
    }

    pub fn current_revision(&self) -> Option<ProposalRevision> {
        self.active.as_ref().map(|active| active.revision)
    }

    pub fn accepted_target(&self) -> Option<Entity> {
        self.active
            .as_ref()
            .and_then(|active| active.accepted_target)
    }

    /// Bumps acceptance freshness after live application state changes.
    ///
    /// App resolvers put the state-change system in [`AppResolve`] and call
    /// this before returning. A same-frame response for the old revision then
    /// cannot commit; CTK re-proposes the new revision next frame.
    pub fn invalidate_acceptance(&mut self) {
        #[cfg(feature = "os-dnd")]
        {
            self.acceptance_invalidation_generation = self
                .acceptance_invalidation_generation
                .checked_add(1)
                .expect("DnD acceptance invalidation generation exhausted");
        }
        if let Some(active) = self.active.as_mut() {
            if !active.drop_pending {
                active.candidate_index = 0;
                active.candidate = active.target_chain.first().copied();
            }
            active.bump_revision();
        }
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) const fn acceptance_invalidation_generation(&self) -> u64 {
        self.acceptance_invalidation_generation
    }

    /// Moves a dragging payload into the reserved OS-export state.
    ///
    /// The threshold handoff calls this only after the bridge has synchronously
    /// accepted a cloned payload. No ghost exists on that path. Source click
    /// suppression remains latched until the physical release because Bevy's
    /// drag state is cleared before the compositor takes ownership.
    ///
    /// Crate-private and feature-gated, deliberately and as a pair with
    /// [`Self::finish_export`]. Entering `Exporting` is only half a lifecycle:
    /// the state is left holding no payload and blocks every subsequent drag
    /// until it is retired, and `os_dnd` owns the only code that can retire it.
    /// A public entry point without a public exit is a way to wedge the session
    /// permanently — which was reachable with `os-dnd` *off*, where the exit did
    /// not exist at all.
    #[cfg(feature = "os-dnd")]
    pub(crate) fn begin_export(&mut self, commands: &mut Commands) -> Option<ExportHandoff> {
        if self.exporting.is_some() {
            return None;
        }
        self.active.as_ref()?.window?;
        let active = self.active.as_mut()?;
        if !active.transition(DragPhase::Exporting) {
            return None;
        }
        let active = self.active.take()?;
        self.cancelled_click = Some(CancelledClickSuppression {
            source: active.source,
            pointer_id: active.pointer_id,
            button: active.button,
            released: false,
        });
        self.exporting = Some(ExportingDrag {
            source: active.source,
        });
        // The source stays click-suppressed until Bevy observes the physical
        // release (or its level-triggered mouse backstop does). The outgoing
        // handoff clears Bevy's pointer state synchronously, so that later
        // release cannot synthesize a phantom in-app drop or click.
        cleanup_visuals(&active, commands, false);
        Some(ExportHandoff {
            source: active.source,
            payload: active.payload,
        })
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) fn export_candidate(&self, window: Entity) -> Option<ExportCandidate> {
        let active = self.active.as_ref()?;
        // Only a mouse drag can escalate. `cosmix-wl-dnd` starts the OS drag
        // from the seat's held *pointer* BTN_LEFT grab and nothing else, so a
        // touch or custom pointer has no grab to hand over: with no mouse
        // button down `start_outgoing` fails `NoHeldGrab` and the gesture is
        // consumed and lost, and with an unrelated mouse button down it
        // escalates under a grab the touch never owned — the compositor drag
        // would then track the mouse and end on *its* release. Declining keeps
        // such a drag in-app, which is the honest behaviour.
        if active.phase != DragPhase::Dragging
            || active.window != Some(window)
            || active.pointer_id != PointerId::Mouse
            || active.button != PointerButton::Primary
        {
            return None;
        }
        if !matches!(&active.payload, DragPayload::Paths(_)) {
            return None;
        }
        Some(ExportCandidate {
            source: active.source,
            pointer_id: active.pointer_id,
            button: active.button,
        })
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) fn export_material(&self, source: Entity) -> Option<ExportMaterial> {
        let active = self
            .active
            .as_ref()
            .filter(|active| active.phase == DragPhase::Dragging && active.source == source)?;
        let DragPayload::Paths(paths) = &active.payload else {
            return None;
        };
        Some(ExportMaterial {
            paths: paths.clone(),
            icon: active.export_icon.clone(),
            #[cfg(feature = "icons")]
            label: active.export_label.clone(),
        })
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) fn exporting_matches(&self, source: Entity) -> bool {
        self.exporting
            .as_ref()
            .is_some_and(|exporting| exporting.source == source)
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) fn exporting_source(&self) -> Option<Entity> {
        self.exporting.as_ref().map(|exporting| exporting.source)
    }

    #[cfg(feature = "os-dnd")]
    pub(crate) fn finish_export(&mut self, source: Entity) -> bool {
        if !self.exporting_matches(source) {
            return false;
        }
        self.exporting = None;
        true
    }

    /// Drops the click-suppression latch for a source whose window is being
    /// torn down.
    ///
    /// The latch normally survives until the physical release, backstopped by
    /// the level-triggered mouse state — but that resource is fed by winit
    /// messages, not polled, so a release delivered outside every surviving
    /// window never reaches it. Closing the exporting window mid-drag and
    /// releasing over the desktop therefore strands the latch and costs the
    /// next drag. Nothing can click a source whose window is gone, so clearing
    /// it here suppresses nothing real.
    #[cfg(feature = "os-dnd")]
    pub(crate) fn clear_click_suppression(&mut self, source: Entity, commands: &mut Commands) {
        if self
            .cancelled_click
            .as_ref()
            .is_none_or(|cancelled| cancelled.source != source)
        {
            return;
        }
        self.cancelled_click = None;
        if let Ok(mut source) = commands.get_entity(source) {
            source.remove::<DndClickSuppressed>();
        }
    }

    pub(crate) fn allocate_proposal_id(&mut self) -> ProposalId {
        let id = ProposalId(self.next_proposal_id);
        self.next_proposal_id = self
            .next_proposal_id
            .checked_add(1)
            .expect("DnD proposal id exhausted");
        id
    }

    pub(crate) fn allocate_delivery_id(&mut self) -> DeliveryId {
        let id = DeliveryId(self.next_delivery_id);
        self.next_delivery_id = self
            .next_delivery_id
            .checked_add(1)
            .expect("DnD delivery id exhausted");
        id
    }
}

/// CTK computes proposal context here.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct DndPropose;

/// Applications place exactly one acceptance resolver in this set.
///
/// ```
/// # use bevy::prelude::*;
/// # use ctk::prelude::*;
/// # fn resolve(_: MessageReader<AcceptanceProposal>, _: MessageWriter<DropAcceptance>) {}
/// App::new().add_systems(Update, resolve.in_set(AppResolve));
/// ```
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppResolve;

/// CTK consumes only revision-matched acceptance here.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct DndCommit;

/// Installs CTK's in-app DnD core and canonical contract.
pub struct DndPlugin;

impl Plugin for DndPlugin {
    fn build(&self, app: &mut App) {
        ensure_modal_capture_plugin(app);
        app.init_resource::<DragSession>()
            .init_resource::<DndIngressGuard>()
            .init_resource::<HoverMap>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<UiScale>()
            .add_message::<AcceptanceProposal>()
            .add_message::<DropAcceptance>()
            .add_message::<DndDrop>()
            .add_message::<DropComplete>()
            .add_message::<DndDeliveryCancelled>()
            .add_message::<DndHighlightChanged>()
            .add_message::<DndCancelled>()
            .add_message::<KeyboardInput>()
            .add_message::<CursorLeft>()
            .add_message::<WindowFocused>()
            .add_message::<WindowClosed>()
            .add_message::<AppExit>()
            .add_observer(on_drag_start)
            .add_observer(on_drag)
            .add_observer(on_drag_drop)
            .add_observer(on_drag_end)
            .add_observer(on_pointer_cancel)
            .add_observer(suppress_source_click)
            .add_observer(release_cancelled_click_suppression)
            .configure_sets(
                Update,
                (DndPropose, AppResolve, DndCommit)
                    .chain()
                    .after(InteractionPresentationSystems),
            )
            .add_systems(
                Update,
                (
                    clear_released_click_suppression,
                    cancel_from_runtime,
                    update_proposal_context,
                    propose,
                )
                    .chain()
                    .in_set(DndPropose),
            )
            .add_systems(Update, commit.in_set(DndCommit));
        #[cfg(test)]
        app.init_resource::<DndTestDiagnostics>();
        #[cfg(debug_assertions)]
        app.add_systems(
            Update,
            validate_ghost_descendants
                .before(cancel_from_runtime)
                .in_set(DndPropose),
        );
    }
}

fn normalized_camera_target(
    camera: Entity,
    primary_window: Option<Entity>,
    camera_targets: &Query<&RenderTarget>,
) -> Option<NormalizedRenderTarget> {
    camera_targets.get(camera).ok()?.normalize(primary_window)
}

fn viewport_outer_target(
    target: &NormalizedRenderTarget,
    pointer_id: Option<PointerId>,
    primary_window: Option<Entity>,
    viewports: &Query<(&PointerId, &ViewportNode, &ComputedUiTargetCamera)>,
    camera_targets: &Query<&RenderTarget>,
) -> Option<NormalizedRenderTarget> {
    let mut outer_target = None;
    for (viewport_pointer, viewport, ui_camera) in viewports {
        if pointer_id.is_some_and(|expected| *viewport_pointer != expected) {
            continue;
        }
        let Some(inner_camera) = viewport.camera else {
            continue;
        };
        if normalized_camera_target(inner_camera, primary_window, camera_targets).as_ref()
            != Some(target)
        {
            continue;
        }
        let candidate = normalized_camera_target(ui_camera.get()?, primary_window, camera_targets)?;
        if outer_target
            .as_ref()
            .is_some_and(|existing| existing != &candidate)
        {
            return None;
        }
        outer_target = Some(candidate);
    }
    outer_target
}

fn pointer_window(
    target: &NormalizedRenderTarget,
    pointer_id: PointerId,
    primary_window: Option<Entity>,
    viewports: &Query<(&PointerId, &ViewportNode, &ComputedUiTargetCamera)>,
    camera_targets: &Query<&RenderTarget>,
) -> Option<Entity> {
    if let NormalizedRenderTarget::Window(window) = target {
        return Some(window.entity());
    }

    let mut current = viewport_outer_target(
        target,
        Some(pointer_id),
        primary_window,
        viewports,
        camera_targets,
    )
    .or_else(|| viewport_outer_target(target, None, primary_window, viewports, camera_targets))?;
    // Nested viewport chains are unusual but valid. Keep the walk bounded in
    // case malformed camera/view-target wiring forms a cycle.
    for _ in 0..32 {
        if let NormalizedRenderTarget::Window(window) = current {
            return Some(window.entity());
        }
        current = viewport_outer_target(&current, None, primary_window, viewports, camera_targets)?;
    }
    None
}

fn local_start_is_captured(
    capture: &ModalCapture,
    pending_file_requests: Option<&Messages<crate::file_requester::FileRequest>>,
) -> bool {
    capture.is_captured() || pending_file_requests.is_some_and(|requests| !requests.is_empty())
}

#[allow(clippy::too_many_arguments)] // Render-target resolution needs independent Bevy queries.
fn on_drag_start(
    drag: On<Pointer<DragStart>>,
    sources: Query<&DragSource>,
    viewports: Query<(&PointerId, &ViewportNode, &ComputedUiTargetCamera)>,
    camera_targets: Query<&RenderTarget>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    capture: Res<ModalCapture>,
    pending_file_requests: Option<Res<Messages<crate::file_requester::FileRequest>>>,
    ingress: Res<DndIngressGuard>,
    mut session: ResMut<DragSession>,
) {
    if drag.button != PointerButton::Primary
        || session.phase().is_some()
        || ingress.wayland_active
        || session.cancelled_click.is_some()
        || local_start_is_captured(&capture, pending_file_requests.as_deref())
    {
        return;
    }
    let Ok(source) = sources.get(drag.entity) else {
        return;
    };
    let proposal_id = session.allocate_proposal_id();
    let payload = source.payload.clone();
    let window = pointer_window(
        &drag.pointer_location.target,
        drag.pointer_id,
        primary_window.single().ok(),
        &viewports,
        &camera_targets,
    );
    if window.is_none() {
        warn_once!(
            pointer_id = ?drag.pointer_id,
            render_target = ?drag.pointer_location.target,
            "DnD session render target cannot be resolved to a window; window boundary, focus, \
             and close events cannot terminate this session"
        );
    }
    session.active = Some(ActiveDrag {
        phase: DragPhase::Armed,
        source: drag.entity,
        origin: DndOrigin::Internal(drag.entity),
        payload_summary: payload.summary(),
        payload,
        ghost_builder: source.ghost.clone(),
        #[cfg(feature = "os-dnd")]
        export_icon: source.export_icon.clone(),
        #[cfg(all(feature = "os-dnd", feature = "icons"))]
        export_label: source.export_label.clone(),
        ghost: None,
        pointer_id: drag.pointer_id,
        button: drag.button,
        window,
        position: drag.pointer_location.position,
        target_chain: Vec::new(),
        candidate_index: 0,
        candidate: None,
        accepted_target: None,
        accepted_action: None,
        modifiers: Modifiers::default(),
        selected_action: DropAction::Ask,
        proposal_id,
        revision: ProposalRevision(0),
        drop_pending: false,
        freeze_pending: false,
        stale_reproposal_count: 0,
        duplicate_response_warned: false,
    });
}

fn ghost_node(position: Vec2, ui_scale: f32) -> Node {
    let logical = position / ui_scale.max(f32::EPSILON);
    Node {
        position_type: PositionType::Absolute,
        left: px(logical.x + 12.0),
        top: px(logical.y + 12.0),
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::Center,
        column_gap: px(6),
        padding: UiRect::axes(px(8), px(5)),
        border_radius: BorderRadius::all(px(5)),
        ..default()
    }
}

fn spawn_drag_ghost(active: &mut ActiveDrag, commands: &mut Commands, ui_scale: f32) {
    debug_assert_eq!(active.phase, DragPhase::Dragging);
    debug_assert!(active.ghost.is_none());
    let ghost = commands
        .spawn((
            ghost_node(active.position, ui_scale),
            Pickable::IGNORE,
            GlobalZIndex(DND_GHOST_Z),
        ))
        .id();
    #[cfg(any(debug_assertions, test))]
    commands.entity(ghost).insert(DndGhost);
    active.ghost_builder.populate(ghost, commands);
    active.ghost = Some(ghost);
}

#[allow(clippy::too_many_arguments)]
fn on_drag(
    drag: On<Pointer<Drag>>,
    ui_scale: Res<UiScale>,
    #[cfg(feature = "os-dnd")] mut os_runtime: Option<NonSendMut<crate::os_dnd::OsDndRuntime>>,
    #[cfg(feature = "os-dnd")] mut pointer_state: Option<ResMut<PointerState>>,
    #[cfg(feature = "os-dnd")] mut pointers: Query<(
        &PointerId,
        &PointerLocation,
        &mut PointerPress,
    )>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
) {
    let threshold_window = {
        let Some(active) = session.active.as_mut().filter(|active| {
            active.source == drag.entity
                && active.pointer_id == drag.pointer_id
                && active.button == drag.button
        }) else {
            return;
        };
        active.position = drag.pointer_location.position;
        (active.phase == DragPhase::Armed
            && drag.distance.length() >= DRAG_THRESHOLD_PX * ui_scale.0
            && active.transition(DragPhase::Dragging))
        .then_some(active.window)
    };
    if let Some(_window) = threshold_window {
        // Every threshold-crossed drag blocks its source click exactly once.
        // Export success retains the marker behind the release latch; failure
        // proceeds to the ordinary ghost path with the same marker already set.
        commands.entity(drag.entity).insert(DndClickSuppressed);

        #[cfg(feature = "os-dnd")]
        if let (Some(runtime), Some(pointer_state), Some(window)) = (
            os_runtime.as_deref_mut(),
            pointer_state.as_deref_mut(),
            _window,
        ) {
            if crate::os_dnd::try_export_at_threshold(
                runtime,
                window,
                &mut session,
                &mut commands,
                pointer_state,
                &mut pointers,
            ) {
                return;
            }
        }

        let Some(active) = session.active.as_mut().filter(|active| {
            active.source == drag.entity
                && active.pointer_id == drag.pointer_id
                && active.button == drag.button
                && active.phase == DragPhase::Dragging
        }) else {
            return;
        };
        spawn_drag_ghost(active, &mut commands, ui_scale.0);
    }
    let Some(active) = session.active.as_ref().filter(|active| {
        active.source == drag.entity
            && active.pointer_id == drag.pointer_id
            && active.button == drag.button
    }) else {
        return;
    };
    if active.phase == DragPhase::Dragging {
        if let Some(ghost) = active.ghost {
            commands
                .entity(ghost)
                .insert(ghost_node(active.position, ui_scale.0));
        }
    }
}

#[cfg(any(debug_assertions, test))]
#[derive(Component)]
struct DndGhost;

#[cfg(debug_assertions)]
fn validate_ghost_descendants(
    ghosts: Query<Entity, With<DndGhost>>,
    children: Query<&Children>,
    pickables: Query<&Pickable>,
    mut warned: Local<EntityHashSet>,
) {
    for ghost in &ghosts {
        for descendant in children.iter_descendants(ghost) {
            if pickables.get(descendant) != Ok(&Pickable::IGNORE) && warned.insert(descendant) {
                warn!(
                    ?ghost,
                    ?descendant,
                    "DnD ghost descendant is pickable; every GhostBuilder descendant must carry \
                     Pickable::IGNORE"
                );
            }
        }
    }
}

fn on_drag_drop(
    drop: On<Pointer<DragDrop>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut session: ResMut<DragSession>,
) {
    let Some(active) = session.active.as_mut() else {
        return;
    };
    if active.phase == DragPhase::Dragging
        && active.source == drop.dropped
        && active.pointer_id == drop.pointer_id
        && active.button == drop.button
        && !active.drop_pending
    {
        latch_drop(active, drop.pointer_location.position, &keys);
        // Bevy emits DragDrop once for every `dragging_over` entity, so the
        // trigger target is authoritative but not unique. The first Update
        // instead freezes the complete release-frame HoverMap exactly once.
    }
}

fn on_drag_end(
    drag_end: On<Pointer<DragEnd>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
) {
    let Some(active) = session.active.as_mut().filter(|active| {
        active.source == drag_end.entity
            && active.pointer_id == drag_end.pointer_id
            && active.button == drag_end.button
    }) else {
        return;
    };
    if active.drop_pending {
        // Bevy orders DragDrop before DragEnd. The first event owns the
        // release snapshot, so DragEnd cannot latch the same release twice.
        return;
    }
    if active.phase == DragPhase::Dragging {
        // `dragging_over` can be empty when press, first movement and release
        // share one picking pass, even though the release HoverMap contains a
        // valid DropTarget ancestor. Freeze that map through the normal path.
        latch_drop(active, drag_end.pointer_location.position, &keys);
    } else {
        cancel_active(&mut session, DndCancelReason::DragEnded, &mut commands);
    }
}

fn latch_drop(active: &mut ActiveDrag, position: Vec2, keys: &ButtonInput<KeyCode>) {
    active.position = position;
    active.modifiers = Modifiers::from_keys(keys);
    active.selected_action = active.modifiers.requested_action();
    active.drop_pending = true;
    active.freeze_pending = true;
}

fn on_pointer_cancel(
    cancel: On<Pointer<PointerCancel>>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
) {
    let should_cancel = session
        .active
        .as_ref()
        .is_some_and(|active| active.pointer_id == cancel.pointer_id && !active.drop_pending);
    if should_cancel {
        cancel_active(&mut session, DndCancelReason::PointerCancel, &mut commands);
        return;
    }
    // A cancelled pointer never delivers the click the latch exists to
    // suppress, and never delivers the `Release` that would clear it.
    if session
        .cancelled_click
        .as_ref()
        .is_some_and(|cancelled| cancelled.pointer_id == cancel.pointer_id)
    {
        session
            .cancelled_click
            .as_mut()
            .expect("matching suppression checked above")
            .released = true;
    }
}

fn suppress_source_click(
    mut click: On<Pointer<Click>>,
    suppressed: Query<(), With<DndClickSuppressed>>,
) {
    if suppressed.contains(click.entity) {
        click.propagate(false);
    }
}

fn release_cancelled_click_suppression(
    release: On<Pointer<Release>>,
    mut session: ResMut<DragSession>,
) {
    let Some(cancelled) = session.cancelled_click.as_mut() else {
        return;
    };
    if cancelled.pointer_id != release.pointer_id || cancelled.button != release.button {
        return;
    }
    // Bevy emits every Click before its paired Release, but a release can be
    // dispatched for several hovered entities. Defer removal to Update so the
    // complete release batch remains suppressed regardless of hash-map order.
    cancelled.released = true;
}

fn clear_released_click_suppression(
    mut session: ResMut<DragSession>,
    mouse_buttons: Option<Res<ButtonInput<MouseButton>>>,
    mut commands: Commands,
) {
    let Some(cancelled) = session.cancelled_click.as_ref() else {
        return;
    };
    // `Pointer<Release>` is the ordinary clear, but it is not guaranteed to
    // arrive at all: bevy_picking emits `Cancel` only to entities the pointer
    // is hovering, and promises that no further event follows a `Cancel` for
    // that pointer (bevy_picking-0.19.0 events.rs:638 and the emission site at
    // :1200). A pointer lost while over no target therefore produces neither
    // event. The physical button is level-triggered and cannot be missed, so
    // it backstops the latch — without it a lost pointer strands
    // `cancelled_click`, and `on_drag_start` refuses every subsequent drag for
    // the life of the process.
    if cancelled.released || !suppression_button_held(cancelled, mouse_buttons.as_deref()) {
        let source = session
            .cancelled_click
            .take()
            .expect("cleared suppression checked above")
            .source;
        if let Ok(mut source) = commands.get_entity(source) {
            source.remove::<DndClickSuppressed>();
        }
    }
}

/// Whether the button whose click is being suppressed is still physically
/// held. Only the mouse has an authoritative level signal here; other pointer
/// kinds keep the edge-triggered `Release`/`Cancel` paths, which is why
/// `on_pointer_cancel` clears the latch as well. A consumer that never added
/// Bevy's `InputPlugin` has no mouse to lose, so an absent resource keeps the
/// edge behaviour rather than clearing the latch out from under a live press.
fn suppression_button_held(
    cancelled: &CancelledClickSuppression,
    mouse_buttons: Option<&ButtonInput<MouseButton>>,
) -> bool {
    let Some(mouse_buttons) = mouse_buttons else {
        return true;
    };
    if cancelled.pointer_id != PointerId::Mouse {
        return true;
    }
    mouse_buttons.pressed(match cancelled.button {
        PointerButton::Primary => MouseButton::Left,
        PointerButton::Secondary => MouseButton::Right,
        PointerButton::Middle => MouseButton::Middle,
    })
}

#[allow(clippy::too_many_arguments)] // Each independent terminal input has its own Bevy reader.
fn cancel_from_runtime(
    mut keyboard: MessageReader<KeyboardInput>,
    mut cursor_left: MessageReader<CursorLeft>,
    mut focused: MessageReader<WindowFocused>,
    mut closed: MessageReader<WindowClosed>,
    mut exits: MessageReader<AppExit>,
    sources: Query<(), With<DragSource>>,
    coordinator: Option<Res<ModalCoordinator>>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
) {
    let escape = keyboard.read().any(|input| {
        input.key_code == KeyCode::Escape && input.state == ButtonState::Pressed && !input.repeat
    });
    let left_windows: Vec<_> = cursor_left.read().map(|event| event.window).collect();
    let cursor_left_matches = session.active.as_ref().is_some_and(|active| {
        active
            .window
            .is_some_and(|active_window| left_windows.contains(&active_window))
    });
    let lost_focus = focused
        .read()
        .any(|event| !event.focused && session_window_matches(&session, event.window));
    let window_closed = closed
        .read()
        .any(|event| session_window_matches(&session, event.window));
    let app_exit = exits.read().next().is_some();
    let source_missing = session
        .active
        .as_ref()
        .is_some_and(|active| !sources.contains(active.source));
    let modal_opened = coordinator
        .as_deref()
        .is_some_and(ModalCoordinator::is_active);

    // A whole-app exit, destruction of the session's known window, loss of
    // the source, or modal activation invalidates routing even after release.
    // Modal activation is hard because delivering to a background ancestor
    // through a newly opened modal would cross the capture boundary.
    if app_exit {
        cancel_active(&mut session, DndCancelReason::AppExit, &mut commands);
        return;
    }
    if window_closed {
        cancel_active(&mut session, DndCancelReason::WindowClosed, &mut commands);
        return;
    }
    if source_missing {
        cancel_active(
            &mut session,
            DndCancelReason::SourceDespawned,
            &mut commands,
        );
        return;
    }
    if modal_opened {
        cancel_active(&mut session, DndCancelReason::ModalOpened, &mut commands);
        return;
    }

    // DragDrop is observed in PreUpdate before this system runs. Once it has
    // latched `drop_pending`, the gesture has completed. Escape, cursor-leave,
    // and focus-loss are soft gesture inputs: after button-up they must
    // neither export nor cancel the valid pending drop.
    if session
        .active
        .as_ref()
        .is_some_and(|active| active.drop_pending)
    {
        return;
    }

    if escape {
        cancel_active(&mut session, DndCancelReason::Escape, &mut commands);
        return;
    }
    if cursor_left_matches {
        cancel_active(&mut session, DndCancelReason::CursorLeft, &mut commands);
        return;
    }
    if lost_focus {
        cancel_active(
            &mut session,
            DndCancelReason::WindowUnfocused,
            &mut commands,
        );
    }
}

fn session_window_matches(session: &DragSession, window: Entity) -> bool {
    session
        .active
        .as_ref()
        .is_some_and(|active| active.window == Some(window))
}

fn cancel_active(session: &mut DragSession, reason: DndCancelReason, commands: &mut Commands) {
    let Some(mut active) = session.active.take() else {
        return;
    };
    let retain_click_suppression =
        reason == DndCancelReason::Escape && active.phase == DragPhase::Dragging;
    if !active.transition(DragPhase::Cancelled) {
        return;
    }
    if retain_click_suppression {
        session.cancelled_click = Some(CancelledClickSuppression {
            source: active.source,
            pointer_id: active.pointer_id,
            button: active.button,
            released: false,
        });
    }
    cleanup_visuals(&active, commands, !retain_click_suppression);
    commands.write_message(DndCancelled {
        source: active.source,
        reason,
    });
}

fn cleanup_visuals(active: &ActiveDrag, commands: &mut Commands, remove_click_suppression: bool) {
    if let Some(ghost) = active.ghost {
        commands.entity(ghost).try_despawn();
    }
    if remove_click_suppression {
        if let Ok(mut source) = commands.get_entity(active.source) {
            source.remove::<DndClickSuppressed>();
        }
    }
    debug_assert_eq!(
        active.accepted_target.is_some(),
        active.accepted_action.is_some(),
        "DnD highlight target/action invariant broken"
    );
    if let (Some(target), Some(action)) = (active.accepted_target, active.accepted_action) {
        commands.write_message(DndHighlightChanged {
            target,
            highlighted: false,
            action,
        });
    }
}

fn clear_highlight(active: &mut ActiveDrag, commands: &mut Commands) {
    debug_assert_eq!(
        active.accepted_target.is_some(),
        active.accepted_action.is_some(),
        "DnD highlight target/action invariant broken"
    );
    if let (Some(target), Some(action)) =
        (active.accepted_target.take(), active.accepted_action.take())
    {
        commands.write_message(DndHighlightChanged {
            target,
            highlighted: false,
            action,
        });
    }
}

fn update_proposal_context(
    hover_map: Res<HoverMap>,
    keys: Res<ButtonInput<KeyCode>>,
    parents: Query<&ChildOf>,
    targets: Query<(), With<DropTarget>>,
    coordinator: Option<Res<ModalCoordinator>>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
) {
    let modal_root = coordinator
        .as_deref()
        .and_then(ModalCoordinator::active_root);
    let Some(active) = session
        .active
        .as_mut()
        .filter(|active| active.phase == DragPhase::Dragging)
    else {
        return;
    };

    if active.drop_pending {
        if active.freeze_pending {
            // HoverMap was refreshed immediately before DragDrop in Bevy's
            // PreUpdate picking chain. Freeze that release-frame chain once;
            // from here candidate_index can only advance and the chain can
            // never be reset or extended.
            clear_highlight(active, &mut commands);
            active.target_chain = target_chain(
                &hover_map,
                active.pointer_id,
                active.source,
                modal_root,
                &parents,
                &targets,
            );
            active.candidate_index = 0;
            active.candidate = active.target_chain.first().copied();
            active.freeze_pending = false;
            active.stale_reproposal_count = 0;
            active.bump_revision();
        }
        return;
    }

    let modifiers = Modifiers::from_keys(&keys);
    let requested_action = modifiers.requested_action();
    let target_chain = target_chain(
        &hover_map,
        active.pointer_id,
        active.source,
        modal_root,
        &parents,
        &targets,
    );
    if active.target_chain != target_chain || active.modifiers != modifiers {
        clear_highlight(active, &mut commands);
        active.target_chain = target_chain;
        active.candidate_index = 0;
        active.candidate = active.target_chain.first().copied();
        active.stale_reproposal_count = 0;
        active.modifiers = modifiers;
        active.selected_action = requested_action;
        active.bump_revision();
    }
}

pub(crate) fn target_chain(
    hover_map: &HoverMap,
    pointer_id: PointerId,
    source: Entity,
    modal_root: Option<Entity>,
    parents: &Query<&ChildOf>,
    targets: &Query<(), With<DropTarget>>,
) -> Vec<Entity> {
    let Some(hovered) = hover_map.get(&pointer_id) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for hovered in hovered.keys() {
        candidates.extend(targets_in_chain(
            *hovered, source, modal_root, parents, targets,
        ));
    }
    candidates.sort_unstable_by(|(left, left_depth), (right, right_depth)| {
        right_depth
            .cmp(left_depth)
            .then_with(|| left.index().cmp(&right.index()))
    });
    candidates.dedup_by_key(|(target, _)| *target);
    candidates.into_iter().map(|(target, _)| target).collect()
}

fn targets_in_chain(
    hovered: Entity,
    source: Entity,
    modal_root: Option<Entity>,
    parents: &Query<&ChildOf>,
    targets: &Query<(), With<DropTarget>>,
) -> Vec<(Entity, usize)> {
    let mut current = hovered;
    let mut offset = 0;
    let mut candidates = Vec::new();
    let mut found_modal = modal_root.is_none();
    loop {
        if current != source && targets.contains(current) {
            candidates.push((current, offset));
        }
        if Some(current) == modal_root {
            found_modal = true;
            break;
        }
        let Ok(parent) = parents.get(current) else {
            break;
        };
        current = parent.parent();
        offset += 1;
    }
    if !found_modal {
        return Vec::new();
    }
    // Convert distance-from-hover into hierarchy depth so nested targets sort
    // ahead of accepting ancestors, including across non-blocking hover roots.
    let top_depth = hierarchy_depth(hovered, parents);
    candidates
        .into_iter()
        .map(|(target, target_offset)| (target, top_depth.saturating_sub(target_offset)))
        .collect()
}

fn hierarchy_depth(mut entity: Entity, parents: &Query<&ChildOf>) -> usize {
    let mut depth = 0;
    while let Ok(parent) = parents.get(entity) {
        entity = parent.parent();
        depth += 1;
    }
    depth
}

fn advance_candidate(active: &mut ActiveDrag) {
    active.candidate_index = active.candidate_index.saturating_add(1);
    active.candidate = active.target_chain.get(active.candidate_index).copied();
    active.stale_reproposal_count = 0;
    active.bump_revision();
}

fn deny_current_candidate(session: &mut DragSession, commands: &mut Commands, drop_pending: bool) {
    let exhausted = {
        let active = session
            .active
            .as_mut()
            .expect("DnD session disappeared while denying a candidate");
        clear_highlight(active, commands);
        advance_candidate(active);
        active.candidate.is_none()
    };
    if drop_pending && exhausted {
        cancel_active(session, DndCancelReason::Denied, commands);
    }
}

fn propose(session: Res<DragSession>, mut proposals: MessageWriter<AcceptanceProposal>) {
    let Some(active) = session
        .active
        .as_ref()
        .filter(|active| active.phase == DragPhase::Dragging)
    else {
        return;
    };
    let Some(target) = active.candidate else {
        return;
    };
    proposals.write(AcceptanceProposal {
        proposal_id: active.proposal_id,
        revision: active.revision,
        target,
        origin: active.origin,
        payload_summary: active.payload_summary,
        modifiers: active.modifiers,
        position: active.position,
        selected_action: active.selected_action,
    });
}

pub(crate) fn negotiated_action(
    selected: DropAction,
    acceptance: DropAcceptance,
) -> Option<DropAction> {
    if acceptance.allowed_actions.is_empty() {
        None
    } else if acceptance.allowed_actions.contains(selected) {
        Some(selected)
    } else if acceptance.allowed_actions.contains(acceptance.preferred) {
        Some(acceptance.preferred)
    } else {
        acceptance.allowed_actions.first()
    }
}

fn commit(
    mut acceptances: MessageReader<DropAcceptance>,
    sources: Query<(), With<DragSource>>,
    targets: Query<(), With<DropTarget>>,
    mut session: ResMut<DragSession>,
    #[cfg(test)] mut diagnostics: ResMut<DndTestDiagnostics>,
    mut drops: MessageWriter<DndDrop>,
    mut commands: Commands,
) {
    let Some(active) = session
        .active
        .as_mut()
        .filter(|active| active.phase == DragPhase::Dragging)
    else {
        acceptances.clear();
        return;
    };
    let responses: Vec<_> = acceptances
        .read()
        .copied()
        .filter(|response| response.proposal_id == active.proposal_id)
        .collect();
    let current: Vec<_> = responses
        .iter()
        .copied()
        .filter(|response| response.revision == active.revision)
        .collect();
    let drop_pending = active.drop_pending;

    if current.len() != 1 {
        if responses.len() > 1 && !active.duplicate_response_warned {
            active.duplicate_response_warned = true;
            #[cfg(test)]
            {
                diagnostics.duplicate_warning_count += 1;
            }
            warn!(
                proposal_id = active.proposal_id.0,
                revision = active.revision.0,
                response_count = responses.len(),
                "multiple DnD acceptance responses violate the exactly-one-resolver contract; \
                 failing this candidate closed"
            );
        }
        if drop_pending {
            let sole_stale = responses.len() == 1 && current.is_empty();
            if sole_stale {
                if active.stale_reproposal_count >= MAX_STALE_REPROPOSALS {
                    // Repeated invalidation makes this destination unstable.
                    // Cancel the whole drop: falling through to an ancestor
                    // could silently deliver somewhere the user did not aim.
                    cancel_active(&mut session, DndCancelReason::Denied, &mut commands);
                } else {
                    active.stale_reproposal_count += 1;
                    // invalidate_acceptance already moved the revision. Keep
                    // this candidate and re-propose it at that current
                    // revision next frame.
                }
                return;
            }
            // Release freezes a finite chain. A denial, silence, or duplicate
            // response consumes exactly one candidate, so a resolver that
            // never answers still terminates when the chain is exhausted.
            deny_current_candidate(&mut session, &mut commands, true);
        } else if responses.is_empty() || current.len() > 1 {
            deny_current_candidate(&mut session, &mut commands, false);
        }
        // Before release, a sole stale response proves the revision changed
        // after resolution. Defer and re-propose the current revision.
        return;
    }

    let acceptance = current[0];
    let Some(action) = negotiated_action(active.selected_action, acceptance) else {
        deny_current_candidate(&mut session, &mut commands, drop_pending);
        return;
    };

    let Some(target) = active.candidate else {
        if drop_pending {
            cancel_active(&mut session, DndCancelReason::Denied, &mut commands);
        }
        return;
    };
    if active.accepted_target != Some(target) || active.accepted_action != Some(action) {
        clear_highlight(active, &mut commands);
        active.accepted_target = Some(target);
        active.accepted_action = Some(action);
        commands.write_message(DndHighlightChanged {
            target,
            highlighted: true,
            action,
        });
    }
    if !drop_pending {
        return;
    }

    if !sources.contains(active.source) {
        cancel_active(
            &mut session,
            DndCancelReason::SourceDespawned,
            &mut commands,
        );
        return;
    }
    if !targets.contains(target) {
        // AppResolve may despawn the candidate or remove DropTarget. The
        // release chain itself remains frozen; consume only this invalid
        // candidate and continue to the next ancestor.
        deny_current_candidate(&mut session, &mut commands, true);
        return;
    }
    if !active.transition(DragPhase::Dropped) {
        return;
    }
    let delivery_id = session.allocate_delivery_id();
    let active = session
        .active
        .take()
        .expect("active DnD session disappeared during commit");
    cleanup_visuals(&active, &mut commands, true);
    drops.write(DndDrop {
        origin: active.origin,
        target,
        payload: active.payload,
        action,
        modifiers: active.modifiers,
        delivery_id,
        decision_requirement: DropDecisionRequirement::None,
    });
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "os-dnd")]
    use std::time::Instant;

    use bevy::camera::NormalizedRenderTarget;
    use bevy::ecs::system::SystemState;
    use bevy::input::keyboard::{Key, NativeKey};
    use bevy::picking::backend::HitData;
    #[cfg(feature = "os-dnd")]
    use bevy::picking::events::DragEntry;
    use bevy::picking::events::{
        pointer_events, DragEnter, DragLeave, DragOver, Enter, Leave, Move, Out, Over,
        PointerState, Press, Release, Scroll,
    };
    use bevy::picking::hover::PreviousHoverMap;
    use bevy::picking::pointer::{
        update_pointer_map, Location, PointerAction, PointerInput, PointerLocation, PointerMap,
        PointerPress,
    };
    use bevy::picking::PickingSettings;
    use bevy::window::WindowRef;

    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(DndPlugin);
        app
    }

    fn source(app: &mut App) -> Entity {
        app.world_mut()
            .spawn(DragSource::new(
                DragPayload::Text("payload".into()),
                GhostBuilder::empty(),
            ))
            .id()
    }

    #[cfg(feature = "os-dnd")]
    fn file_source(app: &mut App, path: PathBuf, with_icon: bool) -> Entity {
        file_source_with_label(app, path, with_icon, None)
    }

    #[cfg(feature = "os-dnd")]
    fn file_source_with_label(
        app: &mut App,
        path: PathBuf,
        with_icon: bool,
        label: Option<String>,
    ) -> Entity {
        let source = DragSource::new(
            DragPayload::Paths(vec![path]),
            GhostBuilder::new(|root, commands| {
                commands.entity(root).insert(Name::new("fallback ghost"));
            }),
        );
        let source = match label {
            Some(label) => source.with_export_label(label),
            None => source,
        };
        let source = if with_icon {
            source.with_export_icon(Arc::new(
                ExportIconRaster::new(vec![0; 80 * 80 * 4], 80, 80, 2).unwrap(),
            ))
        } else {
            source
        };
        app.world_mut().spawn(source).id()
    }

    #[test]
    fn export_raster_rejects_a_shm_pool_one_pixel_past_the_limit() {
        let width = ((i32::MAX as usize) & !(WAYLAND_SHM_SLOT_ALIGNMENT - 1)) / 4 + 1;
        assert_eq!(
            ExportIconRaster::new(Vec::new(), width as u32, 1, 1).unwrap_err(),
            ExportIconRasterError::ShmPoolTooLarge {
                required: 2_147_483_648,
            }
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ExportIconRaster pixels must be premultiplied RGBA8")]
    fn export_raster_debug_asserts_premultiplied_rgba() {
        let _ = ExportIconRaster::new(vec![255, 0, 0, 128], 1, 1, 1);
    }

    #[test]
    fn drag_source_export_icon_is_additive_and_arc_backed() {
        let plain = DragSource::new(DragPayload::Text("payload".into()), GhostBuilder::empty());
        assert_eq!(plain.payload(), &DragPayload::Text("payload".into()));
        assert!(plain.export_icon().is_none());
        assert!(plain.export_label().is_none());

        let raster = Arc::new(ExportIconRaster::new(vec![0; 4 * 4 * 4], 4, 4, 1).unwrap());
        let source = DragSource::new(DragPayload::Text("payload".into()), GhostBuilder::empty())
            .with_export_icon(Arc::clone(&raster))
            .with_export_label("name.txt".into());
        assert!(Arc::ptr_eq(source.export_icon().unwrap(), &raster));
        assert_eq!(source.export_label(), Some("name.txt"));
        assert_eq!(raster.logical_anchor(), (2, 2));
    }

    fn location(window: Entity, position: Vec2) -> Location {
        Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position,
        }
    }

    fn hit() -> HitData {
        HitData::new(Entity::PLACEHOLDER, 0.0, None, None)
    }

    fn arm(app: &mut App, source: Entity, window: Entity) {
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            DragStart {
                button: PointerButton::Primary,
                hit: hit(),
            },
            source,
        ));
    }

    fn drag(app: &mut App, source: Entity, window: Entity, distance: Vec2) {
        drag_with_button(app, source, window, distance, PointerButton::Primary);
    }

    fn drag_with_button(
        app: &mut App,
        source: Entity,
        window: Entity,
        distance: Vec2,
        button: PointerButton,
    ) {
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, distance),
            Drag {
                button,
                distance,
                delta: distance,
            },
            source,
        ));
    }

    fn release_over(
        app: &mut App,
        source: Entity,
        target: Entity,
        window: Entity,
        button: PointerButton,
    ) {
        let position = Vec2::new(10.0, 0.0);
        // This is Bevy 0.19's real release ordering from `pointer_events`.
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, position),
            DragDrop {
                button,
                dropped: source,
                hit: hit(),
            },
            target,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, position),
            DragEnd {
                button,
                distance: position,
            },
            source,
        ));
    }

    fn hover(app: &mut App, entity: Entity) {
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(entity, hit());
    }

    fn enable_pointer_input_pipeline(app: &mut App, window: Entity) {
        app.init_resource::<PreviousHoverMap>()
            .init_resource::<PickingSettings>()
            .init_resource::<PointerState>()
            .init_resource::<PointerMap>()
            .add_message::<PointerInput>()
            .add_message::<Pointer<PointerCancel>>()
            .add_message::<Pointer<Click>>()
            .add_message::<Pointer<Press>>()
            .add_message::<Pointer<DragDrop>>()
            .add_message::<Pointer<DragEnd>>()
            .add_message::<Pointer<DragEnter>>()
            .add_message::<Pointer<Drag>>()
            .add_message::<Pointer<DragLeave>>()
            .add_message::<Pointer<DragOver>>()
            .add_message::<Pointer<DragStart>>()
            .add_message::<Pointer<Scroll>>()
            .add_message::<Pointer<Move>>()
            .add_message::<Pointer<Out>>()
            .add_message::<Pointer<Over>>()
            .add_message::<Pointer<Leave>>()
            .add_message::<Pointer<Enter>>()
            .add_message::<Pointer<Release>>()
            .add_systems(PreUpdate, (PointerInput::receive, pointer_events).chain());
        app.world_mut().spawn((
            PointerId::Mouse,
            PointerLocation::new(location(window, Vec2::ZERO)),
            PointerPress::default(),
        ));
        app.world_mut()
            .run_system_cached(update_pointer_map)
            .unwrap();
    }

    #[cfg(feature = "os-dnd")]
    fn hold_primary_pointer(app: &mut App, source: Entity, window: Entity) {
        let location = location(window, Vec2::ZERO);
        app.world_mut().write_message(PointerInput::new(
            PointerId::Mouse,
            location.clone(),
            PointerAction::Press(PointerButton::Primary),
        ));
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        let mut pointer_state = app.world_mut().resource_mut::<PointerState>();
        let state = pointer_state.get_mut(PointerId::Mouse, PointerButton::Primary);
        state
            .pressing
            .insert(source, (location, Instant::now(), hit()));
        state.dragging.insert(
            source,
            DragEntry {
                start_pos: Vec2::ZERO,
                latest_pos: Vec2::new(DRAG_THRESHOLD_PX, 0.0),
            },
        );
    }

    fn accept_all(
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::ALL,
                preferred: DropAction::Copy,
            });
        }
    }

    #[derive(Component)]
    struct DenyAfterRelease;

    #[derive(Component)]
    struct DenyAlways;

    fn deny_marked(
        denied: Query<(), With<DenyAlways>>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: if denied.contains(proposal.target) {
                    ActionMask::NONE
                } else {
                    ActionMask::ASK
                },
                preferred: DropAction::Ask,
            });
        }
    }

    fn deny_marked_after_release(
        session: Res<DragSession>,
        denied: Query<(), With<DenyAfterRelease>>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        let released = session
            .active
            .as_ref()
            .is_some_and(|active| active.drop_pending);
        for proposal in proposals.read() {
            let allowed_actions = if released && denied.contains(proposal.target) {
                ActionMask::NONE
            } else if released {
                ActionMask::ALL
            } else {
                ActionMask::ASK
            };
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions,
                preferred: DropAction::Ask,
            });
        }
    }

    fn accept_copy(
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::COPY,
                preferred: DropAction::Copy,
            });
        }
    }

    fn second_accept_all(
        proposals: MessageReader<AcceptanceProposal>,
        acceptances: MessageWriter<DropAcceptance>,
    ) {
        accept_all(proposals, acceptances);
    }

    #[derive(Component)]
    struct RemoveTargetOnResolve;

    fn accept_and_remove_marked_target(
        marked: Query<(), With<RemoveTargetOnResolve>>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
        mut commands: Commands,
    ) {
        for proposal in proposals.read() {
            if marked.contains(proposal.target) {
                commands.entity(proposal.target).remove::<DropTarget>();
            }
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::ALL,
                preferred: DropAction::Ask,
            });
        }
    }

    fn oscillating_resolver(
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            let (allowed_actions, preferred) = match proposal.selected_action {
                DropAction::Ask => (ActionMask::COPY, DropAction::Copy),
                DropAction::Copy => (ActionMask::ASK, DropAction::Ask),
                DropAction::Move => (ActionMask::MOVE, DropAction::Move),
            };
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions,
                preferred,
            });
        }
    }

    fn remove_source_after_release(
        session: Res<DragSession>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
        mut commands: Commands,
    ) {
        let released = session
            .active
            .as_ref()
            .is_some_and(|active| active.drop_pending);
        for proposal in proposals.read() {
            if released {
                let DndOrigin::Internal(source) = proposal.origin else {
                    continue;
                };
                commands.entity(source).remove::<DragSource>();
            }
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::ASK,
                preferred: DropAction::Ask,
            });
        }
    }

    fn invalidate_once_then_accept(
        mut invalidated: Local<bool>,
        mut session: ResMut<DragSession>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            if !*invalidated {
                *invalidated = true;
                session.invalidate_acceptance();
            }
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::ASK,
                preferred: DropAction::Ask,
            });
        }
    }

    fn invalidate_then_accept_every_frame(
        mut session: ResMut<DragSession>,
        mut proposals: MessageReader<AcceptanceProposal>,
        mut acceptances: MessageWriter<DropAcceptance>,
    ) {
        for proposal in proposals.read() {
            session.invalidate_acceptance();
            acceptances.write(DropAcceptance {
                proposal_id: proposal.proposal_id,
                revision: proposal.revision,
                allowed_actions: ActionMask::ASK,
                preferred: DropAction::Ask,
            });
        }
    }

    fn begin_dragging(app: &mut App) -> (Entity, Entity, Entity) {
        let window = app.world_mut().spawn_empty().id();
        let source = source(app);
        arm(app, source, window);
        drag(app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();
        let ghost = app
            .world_mut()
            .query_filtered::<Entity, With<DndGhost>>()
            .single(app.world())
            .unwrap();
        (window, source, ghost)
    }

    fn assert_cancelled(app: &mut App, source: Entity, ghost: Entity) {
        app.update();
        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert!(app.world().get_entity(ghost).is_err());
        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
    }

    #[derive(Resource, Default)]
    struct UnsuppressedSourceClicks(usize);

    fn count_unsuppressed_source_clicks(
        click: On<Pointer<Click>>,
        sources: Query<(), With<DragSource>>,
        session: Res<DragSession>,
        mut clicks: ResMut<UnsuppressedSourceClicks>,
    ) {
        if sources.contains(click.entity) && !dnd_click_is_blocked(click.entity, &session) {
            clicks.0 += 1;
        }
    }

    #[test]
    fn drag_start_arms_and_distance_crosses_scaled_threshold() {
        let mut app = app();
        app.world_mut().resource_mut::<UiScale>().0 = 2.0;
        let window = app.world_mut().spawn_empty().id();
        let source = source(&mut app);

        arm(&mut app, source, window);
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Armed)
        );
        drag(&mut app, source, window, Vec2::new(7.9, 0.0));
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Armed)
        );
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<DndGhost>>()
                .iter(app.world())
                .count(),
            0
        );

        drag(&mut app, source, window, Vec2::new(8.0, 0.0));
        app.world_mut().flush();
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Dragging)
        );
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<DndGhost>>()
                .iter(app.world())
                .count(),
            1
        );
    }

    #[cfg(feature = "os-dnd")]
    #[test]
    fn export_starts_at_threshold_while_pointer_is_still_inside_and_never_spawns_a_ghost() {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let path = std::env::current_dir().unwrap();
        let source = file_source(&mut app, path.clone(), true);
        hold_primary_pointer(&mut app, source, window);
        let target = location(window, Vec2::ZERO).target;
        app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
            window,
            target,
            true,
            crate::os_dnd::ThresholdTestOutcome::Started(cosmix_wl_dnd::DataTransferId(41)),
        ));
        let mut drops = app.world().resource::<Messages<DndDrop>>().get_cursor();

        arm(&mut app, source, window);
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Armed)
        );
        // No CursorLeft is sent: this Drag event is still targeted at the
        // source window and must itself perform the compositor handoff.
        drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();

        let session = app.world().resource::<DragSession>();
        assert_eq!(session.phase(), Some(DragPhase::Exporting));
        assert!(session.active.is_none());
        assert_eq!(session.exporting_source(), Some(source));
        assert!(session.cancelled_click.as_ref().is_some_and(|cancelled| {
            cancelled.source == source
                && cancelled.pointer_id == PointerId::Mouse
                && cancelled.button == PointerButton::Primary
                && !cancelled.released
        }));
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<DndGhost>>()
                .iter(app.world())
                .count(),
            0,
            "an exported drag must not create even a transient Bevy ghost"
        );

        let runtime = app.world().non_send::<crate::os_dnd::OsDndRuntime>();
        assert_eq!(runtime.outgoing_for_test(), Some((window, source)));
        assert_eq!(
            runtime.threshold_test_calls(),
            &[crate::os_dnd::ThresholdTestCall {
                source,
                paths: vec![path],
                icon_offset: Some((-20, -20)),
            }]
        );
        let pointer_state = app.world().resource::<PointerState>();
        assert!(pointer_state
            .get(PointerId::Mouse, PointerButton::Primary)
            .is_some_and(|state| state.pressing.is_empty() && state.dragging.is_empty()));
        let mut pointers = app.world_mut().query::<(&PointerId, &PointerPress)>();
        let (_, press) = pointers.single(app.world()).unwrap();
        assert!(!press.is_any_pressed());

        let target = app.world_mut().spawn(DropTarget).id();
        release_over(&mut app, source, target, window, PointerButton::Primary);
        let messages = app.world().resource::<Messages<DndDrop>>();
        assert_eq!(drops.read(messages).count(), 0);
    }

    #[cfg(feature = "os-dnd")]
    #[test]
    fn threshold_export_uses_the_iconless_start_without_both_source_and_bridge_capability() {
        for (with_icon, icons_available) in [(false, true), (true, false)] {
            let mut app = app();
            let window = app.world_mut().spawn_empty().id();
            enable_pointer_input_pipeline(&mut app, window);
            let source = file_source(&mut app, std::env::current_dir().unwrap(), with_icon);
            hold_primary_pointer(&mut app, source, window);
            app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
                window,
                location(window, Vec2::ZERO).target,
                icons_available,
                crate::os_dnd::ThresholdTestOutcome::Started(cosmix_wl_dnd::DataTransferId(42)),
            ));

            arm(&mut app, source, window);
            drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));

            let runtime = app.world().non_send::<crate::os_dnd::OsDndRuntime>();
            assert_eq!(runtime.threshold_test_calls().len(), 1);
            assert_eq!(runtime.threshold_test_calls()[0].icon_offset, None);
        }
    }

    #[cfg(all(feature = "os-dnd", feature = "icons"))]
    #[test]
    fn labelled_composition_uses_the_icon_centre_anchor() {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let source = file_source_with_label(
            &mut app,
            std::env::current_dir().unwrap(),
            true,
            Some("source.txt".into()),
        );
        hold_primary_pointer(&mut app, source, window);
        app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
            window,
            location(window, Vec2::ZERO).target,
            true,
            crate::os_dnd::ThresholdTestOutcome::Started(cosmix_wl_dnd::DataTransferId(42)),
        ));

        arm(&mut app, source, window);
        drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();

        let runtime = app.world().non_send::<crate::os_dnd::OsDndRuntime>();
        assert_eq!(runtime.threshold_test_calls().len(), 1);
        assert_eq!(
            runtime.threshold_test_calls()[0].icon_offset,
            Some((-24, -24))
        );
    }

    #[cfg(all(feature = "os-dnd", feature = "icons"))]
    #[test]
    fn labelled_composition_failure_falls_back_to_the_square_icon() {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let source = file_source_with_label(
            &mut app,
            std::env::current_dir().unwrap(),
            true,
            Some(String::new()),
        );
        hold_primary_pointer(&mut app, source, window);
        app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
            window,
            location(window, Vec2::ZERO).target,
            true,
            crate::os_dnd::ThresholdTestOutcome::Started(cosmix_wl_dnd::DataTransferId(43)),
        ));

        arm(&mut app, source, window);
        drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();

        let runtime = app.world().non_send::<crate::os_dnd::OsDndRuntime>();
        assert_eq!(runtime.threshold_test_calls().len(), 1);
        assert_eq!(
            runtime.threshold_test_calls()[0].icon_offset,
            Some((-20, -20)),
            "an empty label must fail composition without cancelling the square-icon export"
        );
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Exporting)
        );
    }

    #[cfg(feature = "os-dnd")]
    #[derive(Debug, PartialEq, Eq)]
    struct FallbackSnapshot {
        phase: Option<DragPhase>,
        payload: DragPayload,
        ghost_count: usize,
        source_suppressed: bool,
        click_blocked: bool,
        export_latch: bool,
        pointer_press_held: bool,
        pointer_state_pressing: bool,
        pointer_state_dragging: bool,
    }

    #[cfg(feature = "os-dnd")]
    fn fallback_snapshot(app: &mut App, source: Entity) -> FallbackSnapshot {
        let session = app.world().resource::<DragSession>();
        let payload = session
            .active
            .as_ref()
            .expect("fallback retains its active drag")
            .payload
            .clone();
        let phase = session.phase();
        let click_blocked = dnd_click_is_blocked(source, session);
        let export_latch = session.cancelled_click.is_some();
        let state = app
            .world()
            .resource::<PointerState>()
            .get(PointerId::Mouse, PointerButton::Primary)
            .expect("held pointer state exists");
        let pointer_state_pressing = state.pressing.contains_key(&source);
        let pointer_state_dragging = state.dragging.contains_key(&source);
        let mut pointers = app.world_mut().query::<(&PointerId, &PointerPress)>();
        let (_, press) = pointers.single(app.world()).unwrap();
        let pointer_press_held = press.is_primary_pressed();
        let ghost_count = app
            .world_mut()
            .query_filtered::<Entity, With<DndGhost>>()
            .iter(app.world())
            .count();
        FallbackSnapshot {
            phase,
            payload,
            ghost_count,
            source_suppressed: app.world().entity(source).contains::<DndClickSuppressed>(),
            click_blocked,
            export_latch,
            pointer_press_held,
            pointer_state_pressing,
            pointer_state_dragging,
        }
    }

    #[cfg(feature = "os-dnd")]
    fn threshold_fallback_fixture(scripted_failure: bool) -> (App, Entity, Entity) {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let source = file_source(&mut app, std::env::current_dir().unwrap(), false);
        hold_primary_pointer(&mut app, source, window);
        if scripted_failure {
            app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
                window,
                location(window, Vec2::ZERO).target,
                true,
                crate::os_dnd::ThresholdTestOutcome::NoHeldGrab,
            ));
        }
        arm(&mut app, source, window);
        drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();
        (app, source, window)
    }

    #[cfg(feature = "os-dnd")]
    #[test]
    fn refused_escalation_is_identical_to_a_never_exportable_drag_and_suppression_clears() {
        let (mut refused, refused_source, refused_window) = threshold_fallback_fixture(true);
        let (mut unavailable, unavailable_source, unavailable_window) =
            threshold_fallback_fixture(false);

        let refused_snapshot = fallback_snapshot(&mut refused, refused_source);
        let unavailable_snapshot = fallback_snapshot(&mut unavailable, unavailable_source);
        assert_eq!(refused_snapshot, unavailable_snapshot);
        assert_eq!(
            refused_snapshot,
            FallbackSnapshot {
                phase: Some(DragPhase::Dragging),
                payload: DragPayload::Paths(vec![std::env::current_dir().unwrap()]),
                ghost_count: 1,
                source_suppressed: true,
                click_blocked: true,
                export_latch: false,
                pointer_press_held: true,
                pointer_state_pressing: true,
                pointer_state_dragging: true,
            }
        );
        assert_eq!(
            refused
                .world()
                .non_send::<crate::os_dnd::OsDndRuntime>()
                .threshold_test_calls()
                .len(),
            1
        );

        for (app, source, window) in [
            (&mut refused, refused_source, refused_window),
            (&mut unavailable, unavailable_source, unavailable_window),
        ] {
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location(window, Vec2::new(DRAG_THRESHOLD_PX, 0.0)),
                PointerCancel { hit: hit() },
                source,
            ));
            app.world_mut().flush();
            assert_eq!(app.world().resource::<DragSession>().phase(), None);
            assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
            assert!(!dnd_click_is_blocked(
                source,
                app.world().resource::<DragSession>()
            ));
        }
    }

    #[cfg(feature = "os-dnd")]
    #[test]
    fn invalid_export_payload_falls_back_before_the_bridge_call() {
        let mut app = app();
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let path = PathBuf::from("/definitely/not/a/real/cosmix-dnd-path");
        let source = file_source(&mut app, path.clone(), false);
        hold_primary_pointer(&mut app, source, window);
        app.insert_non_send(crate::os_dnd::OsDndRuntime::for_threshold_test(
            window,
            location(window, Vec2::ZERO).target,
            true,
            crate::os_dnd::ThresholdTestOutcome::Started(cosmix_wl_dnd::DataTransferId(43)),
        ));

        arm(&mut app, source, window);
        drag(&mut app, source, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();

        assert_eq!(
            app.world()
                .non_send::<crate::os_dnd::OsDndRuntime>()
                .threshold_test_calls(),
            &[]
        );
        assert_eq!(
            fallback_snapshot(&mut app, source),
            FallbackSnapshot {
                phase: Some(DragPhase::Dragging),
                payload: DragPayload::Paths(vec![path]),
                ghost_count: 1,
                source_suppressed: true,
                click_blocked: true,
                export_latch: false,
                pointer_press_held: true,
                pointer_state_pressing: true,
                pointer_state_dragging: true,
            }
        );
    }

    #[test]
    fn secondary_drag_events_do_not_mutate_active_primary_drag() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        let original_position = app
            .world()
            .resource::<DragSession>()
            .active
            .as_ref()
            .unwrap()
            .position;
        let target = app.world_mut().spawn(DropTarget).id();

        drag_with_button(
            &mut app,
            source,
            window,
            Vec2::new(40.0, 0.0),
            PointerButton::Secondary,
        );
        release_over(&mut app, source, target, window, PointerButton::Secondary);

        let active = app
            .world()
            .resource::<DragSession>()
            .active
            .as_ref()
            .unwrap();
        assert_eq!(active.phase, DragPhase::Dragging);
        assert_eq!(active.position, original_position);
        assert!(!active.drop_pending);
        assert!(app.world().get_entity(ghost).is_ok());
    }

    #[test]
    fn escape_cancellation_suppresses_the_retained_click_until_release() {
        let mut app = app();
        app.init_resource::<UnsuppressedSourceClicks>()
            .add_observer(count_unsuppressed_source_clicks);
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: Key::Unidentified(NativeKey::Unidentified),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert!(app.world().get_entity(ghost).is_err());
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));

        // Bevy 0.19 dispatches Click before Release for one physical button-up.
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            Click {
                button: PointerButton::Primary,
                hit: hit(),
                duration: std::time::Duration::ZERO,
                count: 1,
            },
            source,
        ));
        assert_eq!(app.world().resource::<UnsuppressedSourceClicks>().0, 0);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            Release {
                button: PointerButton::Primary,
                hit: hit(),
            },
            source,
        ));
        assert!(dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
        app.update();

        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(!dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
    }

    /// The latch blocks `on_drag_start`, so anything that strands it kills DnD
    /// for the life of the process. bevy_picking guarantees no event follows a
    /// `Cancel` for that pointer (events.rs:638) and emits `Cancel` only to
    /// hovered entities (:1200), so a pointer lost over no target delivers
    /// neither `Release` nor `Cancel`. Only the physical button clears this.
    #[test]
    fn a_lost_pointer_never_strands_the_click_suppression_latch() {
        let mut app = app();
        app.init_resource::<ButtonInput<MouseButton>>();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        let (window, dragged, _ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: Key::Escape,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();

        // Still held: the latch must survive, or the click it exists to
        // suppress gets through.
        assert!(dnd_click_is_blocked(
            dragged,
            app.world().resource::<DragSession>()
        ));

        // The button comes up with neither a `Release` nor a `Cancel` event.
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .release(MouseButton::Left);
        app.update();

        assert!(!app.world().entity(dragged).contains::<DndClickSuppressed>());
        assert!(!dnd_click_is_blocked(
            dragged,
            app.world().resource::<DragSession>()
        ));

        // And DnD is live again rather than permanently refusing to arm.
        let next = source(&mut app);
        arm(&mut app, next, window);
        drag(&mut app, next, window, Vec2::new(DRAG_THRESHOLD_PX, 0.0));
        app.world_mut().flush();
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Dragging)
        );
    }

    #[test]
    fn pointer_cancel_clears_a_retained_click_suppression() {
        let mut app = app();
        let (window, source, _ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: Key::Escape,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
        assert!(dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            PointerCancel { hit: hit() },
            source,
        ));
        app.update();

        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(!dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
    }

    #[test]
    fn pointer_cancel_cancels_and_cleans() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            PointerCancel { hit: hit() },
            source,
        ));
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn pointer_cancel_from_another_pointer_does_not_cancel() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().trigger(Pointer::new(
            PointerId::Touch(7),
            location(window, Vec2::ZERO),
            PointerCancel { hit: hit() },
            source,
        ));

        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Dragging)
        );
        assert!(app.world().get_entity(ghost).is_ok());
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
    }

    #[test]
    fn cursor_left_cancels_ineligible_session_and_cleans() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(CursorLeft { window });
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn cursor_left_after_release_does_not_destroy_latched_drop() {
        let mut app = app();
        app.add_systems(Update, accept_all.in_set(AppResolve));
        let (window, source, ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, target);
        app.update();

        release_over(&mut app, source, target, window, PointerButton::Primary);
        app.world_mut().write_message(CursorLeft { window });
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert!(app.world().get_entity(ghost).is_err());
        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, target);
        assert_eq!(drop.action, DropAction::Ask);
    }

    #[test]
    fn release_uses_release_frame_hover_map() {
        let mut app = app();
        app.add_systems(Update, accept_all.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let previous_target = app.world_mut().spawn(DropTarget).id();
        let release_target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, previous_target);
        app.update();

        // Bevy refreshes HoverMap immediately before emitting DragDrop in
        // PreUpdate. Model that real ordering without an intervening Update.
        {
            let mut hover = app.world_mut().resource_mut::<HoverMap>();
            let hovered = hover.get_mut(&PointerId::Mouse).unwrap();
            hovered.clear();
            hovered.insert(release_target, hit());
        }
        release_over(
            &mut app,
            source,
            release_target,
            window,
            PointerButton::Primary,
        );
        app.update();

        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, release_target);
    }

    #[test]
    fn drag_drop_snapshot_is_not_relatched_by_following_drag_end() {
        let mut app = app();
        let (window, source, _ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        let drop_position = Vec2::new(10.0, 2.0);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, drop_position),
            DragDrop {
                button: PointerButton::Primary,
                dropped: source,
                hit: hit(),
            },
            target,
        ));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::ShiftLeft);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ControlLeft);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::new(99.0, 88.0)),
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(99.0, 88.0),
            },
            source,
        ));

        let active = app
            .world()
            .resource::<DragSession>()
            .active
            .as_ref()
            .unwrap();
        assert!(active.drop_pending);
        assert!(active.freeze_pending);
        assert_eq!(active.position, drop_position);
        assert_eq!(
            active.modifiers,
            Modifiers {
                shift: true,
                ..default()
            }
        );
        assert_eq!(active.selected_action, DropAction::Move);
    }

    #[test]
    fn same_pass_release_without_drag_drop_lands_on_ancestor_target() {
        let mut app = app();
        app.add_systems(Update, accept_all.in_set(AppResolve));
        let window = app.world_mut().spawn_empty().id();
        enable_pointer_input_pipeline(&mut app, window);
        let pane = app.world_mut().spawn(DropTarget).id();
        let source = source(&mut app);
        app.world_mut().entity_mut(pane).add_child(source);
        hover(&mut app, source);

        let release_position = Vec2::new(DRAG_THRESHOLD_PX + 2.0, 3.0);
        app.world_mut().write_message(PointerInput::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            PointerAction::Press(PointerButton::Primary),
        ));
        app.world_mut().write_message(PointerInput::new(
            PointerId::Mouse,
            location(window, release_position),
            PointerAction::Move {
                delta: release_position,
            },
        ));
        app.world_mut().write_message(PointerInput::new(
            PointerId::Mouse,
            location(window, release_position),
            PointerAction::Release(PointerButton::Primary),
        ));

        app.update();

        assert_eq!(
            app.world().resource::<Messages<Pointer<DragDrop>>>().len(),
            0
        );
        assert_eq!(
            app.world().resource::<Messages<Pointer<DragEnd>>>().len(),
            1
        );
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, pane);
    }

    #[test]
    fn focus_loss_cancels_and_cleans() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(WindowFocused {
            window,
            focused: false,
        });
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn unknown_session_window_ignores_unrelated_window_events() {
        let mut app = app();
        let (_window, source, ghost) = begin_dragging(&mut app);
        app.world_mut()
            .resource_mut::<DragSession>()
            .active
            .as_mut()
            .unwrap()
            .window = None;
        let unrelated = app.world_mut().spawn_empty().id();
        app.world_mut()
            .write_message(CursorLeft { window: unrelated });
        app.world_mut().write_message(WindowFocused {
            window: unrelated,
            focused: false,
        });
        app.world_mut()
            .write_message(WindowClosed { window: unrelated });
        app.update();

        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Dragging)
        );
        assert!(app.world().get_entity(ghost).is_ok());
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
    }

    #[test]
    fn window_close_cancels_and_cleans() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(WindowClosed { window });
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn app_exit_cancels_and_cleans() {
        let mut app = app();
        let (_window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().write_message(AppExit::Success);
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn source_despawn_cancels_and_cleans() {
        let mut app = app();
        let (_window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().despawn(source);
        app.update();
        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert!(app.world().get_entity(ghost).is_err());
    }

    #[test]
    fn source_removed_during_app_resolve_cancels_before_delivery() {
        let mut app = app();
        app.add_systems(Update, remove_source_after_release.in_set(AppResolve));
        let (window, source, ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, target);
        app.update();
        release_over(&mut app, source, target, window, PointerButton::Primary);
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert!(app.world().get_entity(ghost).is_err());
        assert!(!app.world().entity(source).contains::<DragSource>());
        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        assert_eq!(cursor.read(drops).count(), 0);
        let cancellations = app.world().resource::<Messages<DndCancelled>>();
        let mut cursor = cancellations.get_cursor();
        assert_eq!(
            cursor.read(cancellations).last().unwrap().reason,
            DndCancelReason::SourceDespawned
        );
    }

    #[test]
    fn opening_modal_mid_drag_cancels_and_exposes_root() {
        let mut app = app();
        app.world_mut().init_resource::<ModalCoordinator>();
        let (_window, source, ghost) = begin_dragging(&mut app);
        let root = app.world_mut().spawn_empty().id();
        let focus = app.world_mut().spawn_empty().id();
        crate::interaction::activate_modal(
            &mut app.world_mut().resource_mut::<ModalCoordinator>(),
            crate::interaction::ModalPresenter::Interaction,
            crate::interaction::InteractionId::next(),
            root,
            focus,
            None,
        );
        assert_eq!(
            app.world().resource::<ModalCoordinator>().active_root(),
            Some(root)
        );
        assert_cancelled(&mut app, source, ghost);
    }

    #[test]
    fn drag_end_without_hover_freezes_then_cancels_and_cleans() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::new(10.0, 0.0)),
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(10.0, 0.0),
            },
            source,
        ));
        assert_cancelled(&mut app, source, ghost);
        let cancellations = app.world().resource::<Messages<DndCancelled>>();
        let mut cursor = cancellations.get_cursor();
        assert_eq!(
            cursor.read(cancellations).last().unwrap().reason,
            DndCancelReason::Denied
        );
    }

    #[test]
    fn click_suppression_exists_only_while_threshold_crossed() {
        let mut app = app();
        let (window, source, ghost) = begin_dragging(&mut app);
        assert!(dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
        app.world_mut().write_message(CursorLeft { window });
        assert_cancelled(&mut app, source, ghost);
        assert!(!dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
    }

    #[test]
    #[allow(clippy::type_complexity)] // This unit test needs both hierarchy query views together.
    fn nearest_target_prefers_deepest_and_respects_modal_subtree() {
        let mut app = app();
        let outer = app.world_mut().spawn(DropTarget).id();
        let modal = app.world_mut().spawn_empty().id();
        let inner = app.world_mut().spawn(DropTarget).id();
        let leaf = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(outer).add_child(modal);
        app.world_mut().entity_mut(modal).add_child(inner);
        app.world_mut().entity_mut(inner).add_child(leaf);
        let unrelated = app.world_mut().spawn(DropTarget).id();
        let source = app.world_mut().spawn_empty().id();
        let mut hover = HoverMap::default();
        hover
            .entry(PointerId::Mouse)
            .or_default()
            .insert(leaf, hit());
        hover
            .entry(PointerId::Mouse)
            .or_default()
            .insert(unrelated, hit());
        let mut state: SystemState<(Query<&ChildOf>, Query<(), With<DropTarget>>)> =
            SystemState::new(app.world_mut());
        let (parents, targets) = state.get(app.world()).unwrap();

        assert_eq!(
            target_chain(
                &hover,
                PointerId::Mouse,
                source,
                Some(modal),
                &parents,
                &targets,
            ),
            vec![inner]
        );
        assert_eq!(
            targets_in_chain(leaf, source, None, &parents, &targets)
                .into_iter()
                .map(|value| value.0)
                .collect::<Vec<_>>(),
            vec![inner, outer]
        );
    }

    #[test]
    fn accepted_target_emits_highlight_on_then_off_when_nearest_changes() {
        let mut app = app();
        let source = source(&mut app);
        let first = app.world_mut().spawn(DropTarget).id();
        let second = app.world_mut().spawn(DropTarget).id();
        let proposal = proposal_for(&mut app, source, first, false);
        let mut highlights = app
            .world()
            .resource::<Messages<DndHighlightChanged>>()
            .get_cursor();
        app.world_mut().write_message(DropAcceptance {
            proposal_id: proposal.proposal_id,
            revision: proposal.revision,
            allowed_actions: ActionMask::ASK,
            preferred: DropAction::Ask,
        });
        app.update();
        assert_eq!(
            app.world().resource::<DragSession>().accepted_target(),
            Some(first)
        );

        {
            let mut hover = app.world_mut().resource_mut::<HoverMap>();
            hover.get_mut(&PointerId::Mouse).unwrap().clear();
            hover
                .get_mut(&PointerId::Mouse)
                .unwrap()
                .insert(second, hit());
        }
        app.update();
        assert_eq!(
            app.world().resource::<DragSession>().accepted_target(),
            None
        );

        let messages = app.world().resource::<Messages<DndHighlightChanged>>();
        assert_eq!(
            highlights.read(messages).copied().collect::<Vec<_>>(),
            vec![
                DndHighlightChanged {
                    target: first,
                    highlighted: true,
                    action: DropAction::Ask,
                },
                DndHighlightChanged {
                    target: first,
                    highlighted: false,
                    action: DropAction::Ask,
                },
            ]
        );
    }

    fn proposal_for(
        app: &mut App,
        source: Entity,
        target: Entity,
        drop_pending: bool,
    ) -> AcceptanceProposal {
        // This constructor intentionally isolates response/highlight mechanics.
        // Tests using it do not prove DragStart/Drag/DragDrop observer ordering,
        // release-frame HoverMap capture, or button/pointer correlation.
        let mut session = app.world_mut().resource_mut::<DragSession>();
        let id = session.allocate_proposal_id();
        session.active = Some(ActiveDrag {
            phase: DragPhase::Dragging,
            source,
            origin: DndOrigin::Internal(source),
            payload: DragPayload::Text("payload".into()),
            payload_summary: PayloadSummary::Text {
                bytes: Some(7),
                chars: Some(7),
            },
            ghost_builder: GhostBuilder::empty(),
            #[cfg(feature = "os-dnd")]
            export_icon: None,
            #[cfg(all(feature = "os-dnd", feature = "icons"))]
            export_label: None,
            ghost: None,
            pointer_id: PointerId::Mouse,
            button: PointerButton::Primary,
            window: None,
            position: Vec2::new(10.0, 20.0),
            target_chain: vec![target],
            candidate_index: 0,
            candidate: Some(target),
            accepted_target: None,
            accepted_action: None,
            modifiers: Modifiers::default(),
            selected_action: DropAction::Ask,
            proposal_id: id,
            revision: ProposalRevision(3),
            drop_pending,
            freeze_pending: false,
            stale_reproposal_count: 0,
            duplicate_response_warned: false,
        });
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(target, hit());
        AcceptanceProposal {
            proposal_id: id,
            revision: ProposalRevision(3),
            target,
            origin: DndOrigin::Internal(source),
            payload_summary: PayloadSummary::Text {
                bytes: Some(7),
                chars: Some(7),
            },
            modifiers: Modifiers::default(),
            position: Vec2::new(10.0, 20.0),
            selected_action: DropAction::Ask,
        }
    }

    #[test]
    fn matching_revision_commits_slim_drop() {
        let mut app = app();
        let source = source(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        let proposal = proposal_for(&mut app, source, target, true);
        app.world_mut().write_message(DropAcceptance {
            proposal_id: proposal.proposal_id,
            revision: proposal.revision,
            allowed_actions: ActionMask::ASK,
            preferred: DropAction::Ask,
        });
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.origin, DndOrigin::Internal(source));
        assert_eq!(drop.target, target);
        assert_eq!(drop.payload, DragPayload::Text("payload".into()));
        assert_eq!(drop.action, DropAction::Ask);
        assert_eq!(drop.decision_requirement, DropDecisionRequirement::None);
    }

    #[test]
    fn denied_nested_target_falls_back_to_accepting_ancestor() {
        let mut app = app();
        app.add_systems(Update, deny_marked.in_set(AppResolve));
        let (_window, _source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn((DropTarget, DenyAlways)).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);

        app.update();
        {
            let session = app.world().resource::<DragSession>();
            let active = session.active.as_ref().unwrap();
            assert_eq!(active.candidate, Some(outer));
            assert_eq!(active.target_chain, vec![inner, outer]);
        }

        app.update();
        assert_eq!(
            app.world().resource::<DragSession>().accepted_target(),
            Some(outer)
        );
    }

    #[test]
    fn released_nested_denial_walks_frozen_chain_to_accepting_ancestor() {
        let mut app = app();
        app.add_systems(Update, deny_marked_after_release.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn((DropTarget, DenyAfterRelease)).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);

        // Establish the deepest candidate while the pointer is still held.
        app.update();
        assert_eq!(
            app.world().resource::<DragSession>().accepted_target(),
            Some(inner)
        );
        release_over(&mut app, source, inner, window, PointerButton::Primary);

        app.update();
        {
            let session = app.world().resource::<DragSession>();
            let active = session.active.as_ref().unwrap();
            assert!(active.drop_pending);
            assert!(!active.freeze_pending);
            assert_eq!(active.target_chain, vec![inner, outer]);
            assert_eq!(active.candidate, Some(outer));
        }
        let unrelated = app.world_mut().spawn(DropTarget).id();
        {
            let mut hover = app.world_mut().resource_mut::<HoverMap>();
            let hovered = hover.get_mut(&PointerId::Mouse).unwrap();
            hovered.clear();
            hovered.insert(unrelated, hit());
        }
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, outer);
    }

    #[test]
    fn modal_opened_during_latched_ancestor_walk_cancels_hard() {
        let mut app = app();
        app.world_mut().init_resource::<ModalCoordinator>();
        app.add_systems(Update, deny_marked_after_release.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn((DropTarget, DenyAfterRelease)).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);
        release_over(&mut app, source, inner, window, PointerButton::Primary);
        app.update();
        assert_eq!(
            app.world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap()
                .candidate,
            Some(outer)
        );

        let root = app.world_mut().spawn_empty().id();
        let focus = app.world_mut().spawn_empty().id();
        crate::interaction::activate_modal(
            &mut app.world_mut().resource_mut::<ModalCoordinator>(),
            crate::interaction::ModalPresenter::Interaction,
            crate::interaction::InteractionId::next(),
            root,
            focus,
            None,
        );
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let cancellations = app.world().resource::<Messages<DndCancelled>>();
        let mut cursor = cancellations.get_cursor();
        assert_eq!(
            cursor.read(cancellations).last().unwrap().reason,
            DndCancelReason::ModalOpened
        );
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        assert_eq!(cursor.read(drops).count(), 0);
    }

    #[test]
    fn drop_modifiers_remain_frozen_during_ancestor_walk() {
        let mut app = app();
        app.add_systems(Update, deny_marked_after_release.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn((DropTarget, DenyAfterRelease)).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        release_over(&mut app, source, inner, window, PointerButton::Primary);

        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::ShiftLeft);
        app.update();

        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, outer);
        assert_eq!(drop.action, DropAction::Move);
        assert_eq!(
            drop.modifiers,
            Modifiers {
                shift: true,
                ..default()
            }
        );
    }

    #[test]
    fn invalidated_target_advances_frozen_chain_before_delivery() {
        let mut app = app();
        app.add_systems(Update, accept_and_remove_marked_target.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app
            .world_mut()
            .spawn((DropTarget, RemoveTargetOnResolve))
            .id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);
        release_over(&mut app, source, inner, window, PointerButton::Primary);

        app.update();
        {
            let active = app
                .world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap();
            assert_eq!(active.candidate, Some(outer));
            assert_eq!(active.target_chain, vec![inner, outer]);
        }
        assert!(!app.world().entity(inner).contains::<DropTarget>());
        app.update();

        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, outer);
    }

    #[test]
    fn duplicate_resolvers_fail_closed_with_one_warning_per_session() {
        let mut app = app();
        app.add_systems(Update, (accept_all, second_accept_all).in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn(DropTarget).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);
        release_over(&mut app, source, inner, window, PointerButton::Primary);

        app.update();
        assert_eq!(
            app.world()
                .resource::<DndTestDiagnostics>()
                .duplicate_warning_count,
            1
        );
        assert_eq!(
            app.world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap()
                .candidate,
            Some(outer)
        );
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        assert_eq!(
            app.world()
                .resource::<DndTestDiagnostics>()
                .duplicate_warning_count,
            1
        );
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        assert_eq!(cursor.read(drops).count(), 0);
    }

    #[test]
    fn stale_response_after_invalidation_reproposes_original_candidate() {
        let mut app = app();
        app.add_systems(Update, invalidate_once_then_accept.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let ancestor = app.world_mut().spawn(DropTarget).id();
        let original = app.world_mut().spawn(DropTarget).id();
        app.world_mut().entity_mut(ancestor).add_child(original);
        hover(&mut app, original);
        release_over(&mut app, source, original, window, PointerButton::Primary);

        app.update();
        {
            let active = app
                .world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap();
            assert_eq!(active.candidate, Some(original));
            assert_eq!(active.stale_reproposal_count, 1);
        }
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.target, original);
    }

    #[test]
    fn repeated_stale_responses_cancel_at_cap_without_ancestor_fallback() {
        let mut app = app();
        app.add_systems(
            Update,
            invalidate_then_accept_every_frame.in_set(AppResolve),
        );
        let (window, source, _ghost) = begin_dragging(&mut app);
        let ancestor = app.world_mut().spawn(DropTarget).id();
        let original = app.world_mut().spawn(DropTarget).id();
        app.world_mut().entity_mut(ancestor).add_child(original);
        hover(&mut app, original);
        release_over(&mut app, source, original, window, PointerButton::Primary);

        for expected_count in 1..=MAX_STALE_REPROPOSALS {
            app.update();
            let active = app
                .world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap();
            assert_eq!(active.candidate, Some(original));
            assert_eq!(active.stale_reproposal_count, expected_count);
        }
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        assert_eq!(cursor.read(drops).count(), 0);
        let cancellations = app.world().resource::<Messages<DndCancelled>>();
        let mut cursor = cancellations.get_cursor();
        assert_eq!(
            cursor.read(cancellations).last().unwrap().reason,
            DndCancelReason::Denied
        );
    }

    #[test]
    fn silent_resolver_walk_is_bounded_by_frozen_chain_exhaustion() {
        let mut app = app();
        let (window, source, _ghost) = begin_dragging(&mut app);
        let outer = app.world_mut().spawn(DropTarget).id();
        let inner = app.world_mut().spawn(DropTarget).id();
        app.world_mut().entity_mut(outer).add_child(inner);
        hover(&mut app, inner);
        release_over(&mut app, source, inner, window, PointerButton::Primary);

        app.update();
        {
            let active = app
                .world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap();
            assert_eq!(active.target_chain, vec![inner, outer]);
            assert_eq!(active.candidate, Some(outer));
        }
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        assert_eq!(cursor.read(drops).count(), 0);
    }

    #[test]
    fn negotiated_action_commits_without_changing_proposal_revision() {
        let mut app = app();
        app.add_systems(Update, accept_copy.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, target);
        release_over(&mut app, source, target, window, PointerButton::Primary);
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let proposals = app.world().resource::<Messages<AcceptanceProposal>>();
        let mut proposal_cursor = proposals.get_cursor();
        let proposal = proposal_cursor.read(proposals).last().unwrap();
        assert_eq!(proposal.selected_action, DropAction::Ask);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.action, DropAction::Copy);
    }

    #[test]
    fn oscillating_resolver_cannot_feed_negotiation_back_into_proposal() {
        let mut app = app();
        app.add_systems(Update, oscillating_resolver.in_set(AppResolve));
        let (window, source, _ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, target);
        app.update();

        {
            let active = app
                .world()
                .resource::<DragSession>()
                .active
                .as_ref()
                .unwrap();
            assert_eq!(active.selected_action, DropAction::Ask);
            assert_eq!(active.accepted_action, Some(DropAction::Copy));
        }
        release_over(&mut app, source, target, window, PointerButton::Primary);
        app.update();

        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        let drop = cursor.read(drops).last().unwrap();
        assert_eq!(drop.action, DropAction::Copy);
    }

    /// Window teardown mid-export must drop the click latch. The mouse backstop
    /// that normally clears it is fed by winit messages rather than polled, so
    /// a release delivered over the desktop after the exporting window closed
    /// never arrives — leaving the latch set and `on_drag_start` refusing the
    /// next drag for a source whose window is already gone.
    #[cfg(feature = "os-dnd")]
    #[test]
    fn teardown_clears_a_latch_whose_release_can_no_longer_be_delivered() {
        let mut app = app();
        let (_window, source, _ghost) = begin_dragging(&mut app);
        let mut state: SystemState<(ResMut<DragSession>, Commands)> =
            SystemState::new(app.world_mut());
        {
            let (mut session, mut commands) = state.get_mut(app.world_mut()).unwrap();
            session.begin_export(&mut commands).unwrap();
        }
        state.apply(app.world_mut());
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());

        {
            let (mut session, mut commands) = state.get_mut(app.world_mut()).unwrap();
            assert!(session.finish_export(source));
            // An unrelated source must not have its latch taken.
            session.clear_click_suppression(Entity::from_bits(u64::MAX), &mut commands);
            assert!(dnd_click_is_blocked(source, &session));
            session.clear_click_suppression(source, &mut commands);
        }
        state.apply(app.world_mut());
        app.update();

        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(!dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
    }

    /// Escalation is a *mouse* capability. `cosmix-wl-dnd` starts the OS drag
    /// from the seat's held pointer BTN_LEFT grab, so a touch drag has no grab
    /// to hand over: it would either fail `NoHeldGrab` — consuming and losing
    /// the gesture — or, with an unrelated mouse button down, ride a grab it
    /// never owned and end on the mouse's release.
    #[cfg(feature = "os-dnd")]
    #[test]
    fn only_a_primary_mouse_drag_is_export_eligible() {
        let mut app = app();
        let (window, _source, _ghost) = begin_dragging(&mut app);
        let mut session = app.world_mut().resource_mut::<DragSession>();
        // Structural eligibility deliberately does no filesystem work. The
        // transport payload constructor is the sole real-path validator.
        session.active.as_mut().unwrap().payload =
            DragPayload::Paths(vec![PathBuf::from("/definitely/not/a/real/path")]);
        assert!(session.export_candidate(window).is_some());

        for pointer_id in [PointerId::Touch(1), PointerId::Touch(2)] {
            session.active.as_mut().unwrap().pointer_id = pointer_id;
            assert_eq!(session.export_candidate(window), None);
        }
        session.active.as_mut().unwrap().pointer_id = PointerId::Mouse;
        assert!(session.export_candidate(window).is_some());
    }

    // Gated with the lifecycle it exercises: `begin_export`/`finish_export` do
    // not exist without `os-dnd`, and an ungated test here broke `cargo test -p
    // ctk` while `cargo build -p ctk` stayed green — the lib compiles, the test
    // target does not.
    #[cfg(feature = "os-dnd")]
    #[test]
    fn export_transition_moves_payload_and_failure_cleanup_retains_click_latch() {
        let mut app = app();
        app.add_systems(Update, accept_all.in_set(AppResolve));
        let (window, source, ghost) = begin_dragging(&mut app);
        let target = app.world_mut().spawn(DropTarget).id();
        hover(&mut app, target);
        let mut highlights = app
            .world()
            .resource::<Messages<DndHighlightChanged>>()
            .get_cursor();
        app.update();

        let mut state: SystemState<(ResMut<DragSession>, Commands)> =
            SystemState::new(app.world_mut());
        let handoff = {
            let (mut session, mut commands) = state.get_mut(app.world_mut()).unwrap();
            session.begin_export(&mut commands).unwrap()
        };
        state.apply(app.world_mut());

        assert_eq!(handoff.source, source);
        assert_eq!(handoff.payload, DragPayload::Text("payload".into()));
        assert_eq!(
            app.world().resource::<DragSession>().phase(),
            Some(DragPhase::Exporting)
        );
        assert!(app.world().resource::<DragSession>().active.is_none());
        assert!(app.world().get_entity(ghost).is_err());
        assert!(app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
        let messages = app.world().resource::<Messages<DndHighlightChanged>>();
        assert_eq!(
            highlights.read(messages).copied().collect::<Vec<_>>(),
            vec![
                DndHighlightChanged {
                    target,
                    highlighted: true,
                    action: DropAction::Ask,
                },
                DndHighlightChanged {
                    target,
                    highlighted: false,
                    action: DropAction::Ask,
                },
            ]
        );

        // Models the outgoing transfer becoming terminal after a successful
        // start. The source remains protected until the physical release that
        // would otherwise become a click.
        assert!(app
            .world_mut()
            .resource_mut::<DragSession>()
            .finish_export(source));
        assert_eq!(app.world().resource::<DragSession>().phase(), None);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(window, Vec2::ZERO),
            Release {
                button: PointerButton::Primary,
                hit: hit(),
            },
            source,
        ));
        app.update();
        assert!(!app.world().entity(source).contains::<DndClickSuppressed>());
        assert!(!dnd_click_is_blocked(
            source,
            app.world().resource::<DragSession>()
        ));
    }
}
