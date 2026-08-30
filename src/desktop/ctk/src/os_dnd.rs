//! Native desktop drag receive and outgoing-handoff glue for CTK.
//!
//! This module is feature-gated because it binds Wayland data devices and
//! brings the platform transport into a CTK consumer. Wayland offers use a
//! custom Bevy pointer for UI hit-testing and hover only. They never synthesize
//! a press, drag, release, source entity, or CTK ghost.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use bevy::app::AppExit;
use bevy::camera::{NormalizedRenderTarget, RenderTarget};
use bevy::ecs::hierarchy::ChildOf;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::ButtonState;
use bevy::log::{debug, error, warn};
use bevy::picking::events::PointerState;
use bevy::picking::hover::HoverMap;
use bevy::picking::pointer::{Location, PointerButton, PointerId, PointerLocation, PointerPress};
use bevy::picking::PickingSystems;
use bevy::prelude::*;
use bevy::ui::UiScale;
use bevy::window::{FileDragAndDrop, PrimaryWindow, RawHandleWrapper, WindowClosed, WindowRef};
use cosmix_wl_dnd as wl;
use uuid::Uuid;

use crate::dnd::{
    self, AcceptanceProposal, ActionMask, DeliveryId, DndCommit, DndDeliveryCancelled, DndDrop,
    DndHighlightChanged, DndIngressGuard, DndOrigin, DndPlugin, DndPropose, DragPayload,
    DragSession, DragSource, DropAcceptance, DropAction, DropComplete, DropDecisionRequirement,
    DropOutcome, DropTarget, ExportIconRaster, Modifiers, PayloadSummary, ProposalId,
    ProposalRevision, TransferId,
};
#[cfg(feature = "icons")]
use crate::icons::labelled_export_icon;
use crate::interaction::ModalCoordinator;

/// Application resolution for a Wayland `Ask` delivery.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropDecision {
    pub delivery_id: DeliveryId,
    pub decision: DropDecisionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropDecisionKind {
    Copy,
    Move,
    Dismissed,
}

/// Bridge-authoritative result of a Wayland `Ask` decision.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct DropDecisionResult {
    pub delivery_id: DeliveryId,
    pub status: DropDecisionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropDecisionStatus {
    Accepted,
    Rejected(String),
}

/// Native-X11 fallback delivery.
///
/// Bevy exposes paths but no XDND position, action, or offer lifecycle. The
/// application must therefore select an explicit position-less destination.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct PositionlessFileDrop {
    pub window: Entity,
    pub paths: Vec<PathBuf>,
}

/// Named coordinate conversion used by the Wayland ingress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OsDndPosition {
    /// Window-logical coordinates consumed by Bevy picking.
    pub bevy_logical: Vec2,
    /// CTK UI-coordinate equivalent after applying `UiScale`.
    pub ctk_ui_logical: Vec2,
}

/// Converts CTK's shared raster into the Wayland transport's validated owner.
///
/// The transport needs its own `Vec` for the SHM upload, so this is the sole
/// pixel copy and belongs at the future one-shot export handoff, never in a
/// per-frame drag system.
pub fn outgoing_icon_from_raster(
    raster: &ExportIconRaster,
    offset: (i32, i32),
) -> Result<wl::OutgoingIcon, wl::OutgoingIconError> {
    wl::OutgoingIcon::new(
        raster.pixels().to_vec(),
        raster.width(),
        raster.height(),
        raster.buffer_scale(),
        offset,
    )
}

/// Converts Wayland surface-logical coordinates into Bevy and CTK UI space.
///
/// `wl_data_device.motion` is already surface-logical, exactly like Bevy's
/// `CursorMoved` position. Bevy's UI backend applies the render-target scale
/// while hit-testing, so applying `UiScale` to the custom pointer itself would
/// double-scale it. The second value is the CTK style-space coordinate used
/// when UI-local placement is needed.
pub fn convert_surface_logical_position(
    position: wl::Position,
    ui_scale: f32,
) -> Option<OsDndPosition> {
    if !position.x.is_finite() || !position.y.is_finite() {
        return None;
    }
    let bevy_logical = Vec2::new(position.x as f32, position.y as f32);
    if !bevy_logical.is_finite() {
        return None;
    }
    let scale = ui_scale.max(f32::EPSILON);
    Some(OsDndPosition {
        bevy_logical,
        ctk_ui_logical: bevy_logical / scale,
    })
}

/// Installs native Wayland receive plus the honest native-X11 fallback.
pub struct OsDndPlugin;

impl Plugin for OsDndPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<DndPlugin>() {
            app.add_plugins(DndPlugin);
        }
        app.insert_non_send(OsDndRuntime::default())
            .add_message::<DropDecision>()
            .add_message::<DropDecisionResult>()
            .add_message::<PositionlessFileDrop>()
            .add_message::<FileDragAndDrop>()
            .add_message::<WindowClosed>()
            .add_message::<AppExit>()
            .add_systems(
                First,
                drive_platform_ingress.in_set(PickingSystems::PostInput),
            )
            .add_systems(
                Update,
                (
                    cancel_outgoing_from_runtime,
                    refresh_wayland_from_application_invalidation,
                    update_wayland_context,
                    propose_wayland,
                )
                    .chain()
                    .in_set(DndPropose),
            )
            .add_systems(
                Update,
                (apply_wayland_acceptance, deliver_wayland_drop)
                    .chain()
                    .after(DndCommit),
            )
            .add_systems(PostUpdate, forward_delivery_results);
    }
}

struct WindowBridge {
    bridge: wl::WaylandBridge,
    target: NormalizedRenderTarget,
    _handle_guard: RawHandleWrapper,
}

#[derive(Default)]
pub(crate) struct OsDndRuntime {
    bridges: HashMap<Entity, WindowBridge>,
    unavailable_windows: HashSet<Entity>,
    native_x11: bool,
    active: Option<WaylandTransfer>,
    outgoing: Option<OutgoingTransfer>,
    next_transfer_id: u64,
    #[cfg(test)]
    threshold_test: Option<ThresholdTestBridge>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutgoingTransfer {
    window: Entity,
    bridge_id: wl::DataTransferId,
    source: Entity,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum ThresholdTestOutcome {
    Started(wl::DataTransferId),
    NoHeldGrab,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ThresholdTestCall {
    pub source: Entity,
    pub paths: Vec<PathBuf>,
    pub icon_offset: Option<(i32, i32)>,
}

#[cfg(test)]
struct ThresholdTestBridge {
    window: Entity,
    target: NormalizedRenderTarget,
    icons_available: bool,
    outcome: ThresholdTestOutcome,
    calls: Vec<ThresholdTestCall>,
}

#[cfg(test)]
impl OsDndRuntime {
    pub(crate) fn for_threshold_test(
        window: Entity,
        target: NormalizedRenderTarget,
        icons_available: bool,
        outcome: ThresholdTestOutcome,
    ) -> Self {
        Self {
            threshold_test: Some(ThresholdTestBridge {
                window,
                target,
                icons_available,
                outcome,
                calls: Vec::new(),
            }),
            ..default()
        }
    }

    pub(crate) fn threshold_test_calls(&self) -> &[ThresholdTestCall] {
        self.threshold_test
            .as_ref()
            .map_or(&[], |bridge| bridge.calls.as_slice())
    }

    pub(crate) const fn outgoing_for_test(&self) -> Option<(Entity, Entity)> {
        match self.outgoing {
            Some(outgoing) => Some((outgoing.window, outgoing.source)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct AppliedAcceptance {
    target: Entity,
    action: DropAction,
    modifiers: Modifiers,
    proposal_revision: ProposalRevision,
    transport_revision: wl::TransportRevision,
}

struct WaylandTransfer {
    window: Entity,
    bridge_id: wl::DataTransferId,
    origin: DndOrigin,
    pointer_id: PointerId,
    pointer_entity: Entity,
    target: NormalizedRenderTarget,
    mime_type: String,
    payload_summary: PayloadSummary,
    source_actions: wl::ActionMask,
    compositor_action: Option<DropAction>,
    position: OsDndPosition,
    modifiers: Modifiers,
    transport_revision: wl::TransportRevision,
    proposal_id: ProposalId,
    proposal_revision: u64,
    delivery_id: DeliveryId,
    target_chain: Vec<Entity>,
    candidate_index: usize,
    candidate: Option<Entity>,
    highlighted: Option<(Entity, DropAction)>,
    last_proposal: Option<AcceptanceProposal>,
    last_applied: Option<AppliedAcceptance>,
    data_requested: bool,
    post_drop_left: bool,
    pending_drop: Option<wl::DropEvent>,
    delivered: bool,
    /// The application has answered for the delivery, so a later terminal is a
    /// consequence of that answer rather than a cancellation of pending work.
    app_resolved: bool,
    refresh_pending: bool,
    acceptance_invalidation_generation: u64,
}

impl WaylandTransfer {
    fn revision(&self) -> ProposalRevision {
        ProposalRevision::from_raw(self.proposal_revision)
    }

    fn bump_revision(&mut self) {
        self.proposal_revision = self
            .proposal_revision
            .checked_add(1)
            .expect("Wayland DnD proposal revision exhausted");
    }

    fn selected_action(&self) -> DropAction {
        self.compositor_action
            .unwrap_or_else(|| requested_action(self.modifiers))
    }

    fn reset_candidates(&mut self, target_chain: Vec<Entity>) {
        self.target_chain = target_chain;
        self.candidate_index = 0;
        self.candidate = self.target_chain.first().copied();
        self.last_proposal = None;
        self.last_applied = None;
        self.bump_revision();
    }

    fn advance_candidate(&mut self) {
        self.candidate_index = self.candidate_index.saturating_add(1);
        self.candidate = self.target_chain.get(self.candidate_index).copied();
        self.last_proposal = None;
        self.last_applied = None;
        self.bump_revision();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncomingRoute {
    External,
    Internal(Entity),
}

/// An export is correlated **per window bridge**. Each bridge owns its own
/// nonce registry, so our own drag re-entering a *different* window of this
/// same process carries a nonce that bridge never issued and arrives as
/// `External` — which the live `outgoing` then rejects. That is deliberate and
/// it fails closed: a cross-window drop is declined, never delivered twice and
/// never delivered to the wrong session. Correlating own-process nonces across
/// bridges would be the alternative, and it is not worth its cost yet — CTK's
/// in-app drag has never crossed windows either (`export_candidate` requires
/// the session's own window, and hover routing is window-scoped), so this
/// declines a gesture that was never supported rather than regressing one.
/// Revisit together, if multi-window drag is ever wanted.
fn route_incoming_origin(
    origin: wl::DndOrigin,
    window: Entity,
    outgoing: Option<&OutgoingTransfer>,
    ctk_session_active: bool,
    exporting_source: Option<Entity>,
) -> Option<IncomingRoute> {
    match origin {
        wl::DndOrigin::External(_) if outgoing.is_none() && !ctk_session_active => {
            Some(IncomingRoute::External)
        }
        wl::DndOrigin::Internal(source) => {
            let source = Entity::from_bits(source.0);
            outgoing
                .filter(|outgoing| outgoing.window == window && outgoing.source == source)
                .filter(|_| exporting_source == Some(source))
                .map(|_| IncomingRoute::Internal(source))
        }
        // A foreign offer while exporting, a stale/spoofed internal origin, or
        // an external offer racing any live CTK session all fail closed.
        _ => None,
    }
}

fn reject_incoming(
    runtime: &mut OsDndRuntime,
    window: Entity,
    transfer_id: wl::DataTransferId,
    diagnosis: &'static str,
) {
    if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
        if let Err(error) = window_bridge.bridge.reject(transfer_id) {
            error!(?window, ?transfer_id, ?error, "{diagnosis}");
        }
    }
}

/// Attempts the threshold handoff while CTK still owns an intact dragging
/// payload. Only a successful protocol start commits the CTK session and
/// suppresses Bevy; every refusal leaves the ordinary in-app drag untouched.
pub(crate) fn try_export_at_threshold(
    runtime: &mut OsDndRuntime,
    window: Entity,
    session: &mut DragSession,
    commands: &mut Commands,
    pointer_state: &mut PointerState,
    pointers: &mut Query<(&PointerId, &PointerLocation, &mut PointerPress)>,
) -> bool {
    if runtime.outgoing.is_some() || runtime.active.is_some() {
        return false;
    }
    let Some(candidate) = session.export_candidate(window) else {
        return false;
    };
    // Eligibility pins both: the Wayland drag is started from the seat's held
    // pointer BTN_LEFT grab, so no other pointer or button can hand one over.
    debug_assert_eq!(candidate.button, PointerButton::Primary);
    debug_assert_eq!(candidate.pointer_id, PointerId::Mouse);

    #[cfg(test)]
    if runtime
        .threshold_test
        .as_ref()
        .is_some_and(|bridge| bridge.window == window)
    {
        let (target, icons_available) = {
            let bridge = runtime
                .threshold_test
                .as_ref()
                .expect("matching scripted threshold bridge checked above");
            (bridge.target.clone(), bridge.icons_available)
        };
        let started = start_threshold_export(
            window,
            &target,
            icons_available,
            candidate,
            session,
            commands,
            pointer_state,
            pointers,
            |source, payload, icon| {
                let bridge = runtime
                    .threshold_test
                    .as_mut()
                    .expect("scripted threshold bridge retained for one call");
                bridge.calls.push(ThresholdTestCall {
                    source: Entity::from_bits(source.0),
                    paths: payload.paths().to_vec(),
                    icon_offset: icon.as_ref().map(wl::OutgoingIcon::offset),
                });
                match bridge.outcome {
                    ThresholdTestOutcome::Started(id) => Ok(id),
                    ThresholdTestOutcome::NoHeldGrab => {
                        Err(wl::BridgeError::Send(wl::SendError::NoHeldGrab))
                    }
                }
            },
        );
        return record_threshold_export(runtime, window, started);
    }

    let Some(window_bridge) = runtime.bridges.get_mut(&window) else {
        return false;
    };
    // Bevy has one `PointerId::Mouse` however many Wayland seats exist, so on a
    // multi-seat session it cannot tell us whose press this drag belongs to
    // while the bridge holds exactly one seat's grab. Declining here keeps the
    // drag in-app; `start_outgoing` refuses the same case too.
    if !window_bridge.bridge.grab_is_unambiguous() {
        return false;
    }
    let target = window_bridge.target.clone();
    let icons_available = window_bridge.bridge.export_icons_available();
    let started = start_threshold_export(
        window,
        &target,
        icons_available,
        candidate,
        session,
        commands,
        pointer_state,
        pointers,
        |source, payload, icon| match icon {
            Some(icon) => window_bridge.bridge.start_outgoing_with_icon(
                source,
                payload,
                wl::ActionMask::ALL,
                icon,
                Instant::now(),
            ),
            None => window_bridge.bridge.start_outgoing(
                source,
                payload,
                wl::ActionMask::ALL,
                Instant::now(),
            ),
        },
    );
    record_threshold_export(runtime, window, started)
}

#[allow(clippy::too_many_arguments)]
fn start_threshold_export(
    window: Entity,
    bridge_target: &NormalizedRenderTarget,
    icons_available: bool,
    candidate: dnd::ExportCandidate,
    session: &mut DragSession,
    commands: &mut Commands,
    pointer_state: &mut PointerState,
    pointers: &mut Query<(&PointerId, &PointerLocation, &mut PointerPress)>,
    start: impl FnOnce(
        wl::SourceId,
        wl::OutgoingPayload,
        Option<wl::OutgoingIcon>,
    ) -> Result<wl::DataTransferId, wl::BridgeError>,
) -> Option<(wl::DataTransferId, Entity)> {
    let pointer_is_held_inside = pointers.iter_mut().any(|(id, location, press)| {
        *id == candidate.pointer_id
            && press.is_primary_pressed()
            && location
                .location()
                .is_some_and(|location| &location.target == bridge_target)
    });
    if !pointer_is_held_inside {
        return None;
    }

    // Structural eligibility is fully settled before the one filesystem
    // validation pass and icon copy. The original ActiveDrag remains intact.
    let material = session
        .export_material(candidate.source)
        .expect("threshold candidate still names the same active drag");
    let payload = match wl::OutgoingPayload::from_paths(material.paths) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(
                ?window,
                source = ?candidate.source,
                ?error,
                "declining OS DnD escalation because the path payload is not exportable"
            );
            return None;
        }
    };
    let icon = if icons_available {
        match material.icon.as_deref() {
            Some(raster) => {
                #[cfg(feature = "icons")]
                let labelled_raster = material.label.as_deref().and_then(|label| {
                    match labelled_export_icon(raster, label) {
                        Ok(labelled) => Some(labelled),
                        Err(error) => {
                            warn!(
                                ?window,
                                source = ?candidate.source,
                                %error,
                                "labelled export icon is unavailable; using the square icon"
                            );
                            None
                        }
                    }
                });
                #[cfg(feature = "icons")]
                let raster = labelled_raster.as_ref().unwrap_or(raster);
                let offset = icon_offset(raster);
                match outgoing_icon_from_raster(raster, offset) {
                    Ok(icon) => Some(icon),
                    Err(error) => {
                        error!(
                            ?window,
                            source = ?candidate.source,
                            ?error,
                            "declining OS DnD escalation because its validated icon diverged from \
                             the transport contract"
                        );
                        return None;
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    let bridge_id = match start(wl::SourceId(candidate.source.to_bits()), payload, icon) {
        Ok(bridge_id) => bridge_id,
        Err(error) => {
            warn!(
                ?window,
                source = ?candidate.source,
                ?error,
                "Wayland refused OS DnD escalation; continuing the in-app drag"
            );
            return None;
        }
    };

    // No Wayland callback can dispatch inside start/flush. With the protocol
    // start accepted, retire CTK's original payload and clear Bevy before this
    // observer returns; later DragDrop/DragEnd observers then see no active
    // in-app session.
    let handoff = session
        .begin_export(commands)
        .expect("successful threshold start retains its preflighted CTK session");
    debug_assert_eq!(handoff.source, candidate.source);
    drop(handoff.payload);
    suppress_bevy_drag(candidate.pointer_id, pointer_state, pointers);
    Some((bridge_id, candidate.source))
}

fn record_threshold_export(
    runtime: &mut OsDndRuntime,
    window: Entity,
    started: Option<(wl::DataTransferId, Entity)>,
) -> bool {
    let Some((bridge_id, source)) = started else {
        return false;
    };
    runtime.outgoing = Some(OutgoingTransfer {
        window,
        bridge_id,
        source,
    });
    true
}

fn icon_offset(raster: &ExportIconRaster) -> (i32, i32) {
    let (x, y) = raster.logical_anchor();
    (
        -i32::try_from(x).expect("validated export icon anchor fits i32"),
        -i32::try_from(y).expect("validated export icon anchor fits i32"),
    )
}

fn suppress_bevy_drag(
    pointer_id: PointerId,
    pointer_state: &mut PointerState,
    pointers: &mut Query<(&PointerId, &PointerLocation, &mut PointerPress)>,
) {
    pointer_state.clear(pointer_id);
    for (id, _, mut press) in pointers.iter_mut() {
        if *id == pointer_id {
            *press = PointerPress::default();
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_platform_ingress(
    mut commands: Commands,
    handles: Query<(Entity, &RawHandleWrapper), With<Window>>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    ui_scale: Res<UiScale>,
    mut closed: MessageReader<WindowClosed>,
    mut exits: MessageReader<AppExit>,
    mut file_drops: MessageReader<FileDragAndDrop>,
    mut positionless: MessageWriter<PositionlessFileDrop>,
    mut runtime: NonSendMut<OsDndRuntime>,
    mut session: ResMut<DragSession>,
    mut ingress: ResMut<DndIngressGuard>,
    mut highlights: MessageWriter<DndHighlightChanged>,
    mut delivery_cancellations: MessageWriter<DndDeliveryCancelled>,
) {
    let closing: Vec<_> = closed.read().map(|event| event.window).collect();
    let exiting = exits.read().next().is_some();
    if exiting {
        let windows: Vec<_> = runtime.bridges.keys().copied().collect();
        for window in windows {
            teardown_window(
                &mut runtime,
                window,
                &mut commands,
                &mut session,
                &mut ingress,
                &mut highlights,
                &mut delivery_cancellations,
            );
        }
        return;
    }
    for window in closing {
        teardown_window(
            &mut runtime,
            window,
            &mut commands,
            &mut session,
            &mut ingress,
            &mut highlights,
            &mut delivery_cancellations,
        );
    }

    let primary = primary_window.single().ok();
    for (window, handle) in &handles {
        if runtime.bridges.contains_key(&window) || runtime.unavailable_windows.contains(&window) {
            continue;
        }
        let Some(target) = RenderTarget::Window(WindowRef::Entity(window)).normalize(primary)
        else {
            continue;
        };
        // SAFETY: the raw handles come from this live Bevy window, this system
        // stays on the main thread, and WindowBridge retains the wrapper until
        // teardown drops the bridge first.
        let result = unsafe {
            wl::WaylandBridge::from_raw_handles(
                handle.get_display_handle(),
                handle.get_window_handle(),
                wl::BridgeConfig::default(),
            )
        };
        match result {
            Ok(bridge) => {
                runtime.bridges.insert(
                    window,
                    WindowBridge {
                        bridge,
                        target,
                        _handle_guard: handle.clone(),
                    },
                );
            }
            Err(wl::InitError::NotWayland) => {
                runtime.native_x11 = true;
                runtime.unavailable_windows.insert(window);
                debug!(
                    ?window,
                    "native X11 session: using position-less Bevy file drops"
                );
            }
            Err(error) => {
                runtime.unavailable_windows.insert(window);
                error!(?window, ?error, "OS DnD bridge initialisation failed");
            }
        }
    }

    let now = Instant::now();
    let mut batches = Vec::new();
    for (window, window_bridge) in &mut runtime.bridges {
        match window_bridge.bridge.pump(now) {
            Ok(events) => {
                let outgoing = window_bridge.bridge.drain_outgoing_events();
                batches.push((*window, events, outgoing));
            }
            Err(error) => {
                error!(?window, ?error, "OS DnD bridge pump failed");
            }
        }
    }
    for (window, events, outgoing_events) in batches {
        for event in events {
            handle_bridge_event(
                &mut runtime,
                window,
                event,
                ui_scale.0,
                &mut commands,
                &mut session,
                &mut ingress,
                &mut highlights,
                &mut delivery_cancellations,
            );
        }
        for event in outgoing_events {
            handle_outgoing_event(
                &mut runtime,
                window,
                event,
                &mut commands,
                &mut session,
                &mut ingress,
                &mut highlights,
                &mut delivery_cancellations,
            );
        }
    }

    if runtime.native_x11 {
        let mut by_window: HashMap<Entity, Vec<PathBuf>> = HashMap::new();
        for event in file_drops.read() {
            if let FileDragAndDrop::DroppedFile { window, path_buf } = event {
                by_window.entry(*window).or_default().push(path_buf.clone());
            }
        }
        for (window, paths) in by_window {
            // XDND position is not exposed by Bevy. The application must route
            // this batch to an explicit active destination; cursor guessing is
            // deliberately impossible at this boundary.
            positionless.write(PositionlessFileDrop { window, paths });
        }
    } else {
        file_drops.clear();
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_bridge_event(
    runtime: &mut OsDndRuntime,
    window: Entity,
    event: wl::BridgeEvent,
    ui_scale: f32,
    commands: &mut Commands,
    session: &mut DragSession,
    ingress: &mut DndIngressGuard,
    highlights: &mut MessageWriter<DndHighlightChanged>,
    delivery_cancellations: &mut MessageWriter<DndDeliveryCancelled>,
) {
    match event {
        wl::BridgeEvent::Entered {
            transfer_id,
            position,
            mime_types,
            source_actions,
            transport_revision,
        } => {
            let bridge_origin = runtime
                .bridges
                .get(&window)
                .expect("event came from a live bridge")
                .bridge
                .origin_for(transfer_id);
            let Some(route) = route_incoming_origin(
                bridge_origin,
                window,
                runtime.outgoing.as_ref(),
                session.phase().is_some(),
                session.exporting_source(),
            ) else {
                reject_incoming(
                    runtime,
                    window,
                    transfer_id,
                    "rejecting concurrent OS drop failed",
                );
                return;
            };
            let Some((mime_type, payload_summary)) = preferred_mime(&mime_types) else {
                reject_incoming(
                    runtime,
                    window,
                    transfer_id,
                    "rejecting unsupported OS drop failed",
                );
                return;
            };
            if runtime.active.is_some() {
                reject_incoming(
                    runtime,
                    window,
                    transfer_id,
                    "rejecting concurrent OS drop failed",
                );
                return;
            }
            let Some(converted) = convert_surface_logical_position(position, ui_scale) else {
                fail_bridge_transfer(
                    runtime,
                    window,
                    transfer_id,
                    wl::TerminalReason::OfferRejected,
                );
                return;
            };
            runtime.next_transfer_id = runtime.next_transfer_id.wrapping_add(1).max(1);
            let ctk_id = TransferId(runtime.next_transfer_id);
            let origin = match route {
                IncomingRoute::External => DndOrigin::External(ctk_id),
                IncomingRoute::Internal(source) => DndOrigin::Internal(source),
            };
            let pointer_id = PointerId::Custom(Uuid::from_u128(
                0xc05d_4d44_0000_0000_0000_0000_0000_0000 | ctk_id.0 as u128,
            ));
            let pointer_entity = commands
                .spawn((
                    pointer_id,
                    PointerLocation::new(Location {
                        target: runtime
                            .bridges
                            .get(&window)
                            .expect("event came from a live bridge")
                            .target
                            .clone(),
                        position: converted.bevy_logical,
                    }),
                ))
                .id();
            let target = runtime
                .bridges
                .get(&window)
                .expect("event came from a live bridge")
                .target
                .clone();
            runtime.active = Some(WaylandTransfer {
                window,
                bridge_id: transfer_id,
                origin,
                pointer_id,
                pointer_entity,
                target,
                mime_type,
                payload_summary,
                source_actions,
                compositor_action: None,
                position: converted,
                modifiers: Modifiers::default(),
                transport_revision,
                proposal_id: session.allocate_proposal_id(),
                proposal_revision: 0,
                delivery_id: session.allocate_delivery_id(),
                target_chain: Vec::new(),
                candidate_index: 0,
                candidate: None,
                highlighted: None,
                last_proposal: None,
                last_applied: None,
                data_requested: false,
                post_drop_left: false,
                pending_drop: None,
                delivered: false,
                app_resolved: false,
                refresh_pending: true,
                acceptance_invalidation_generation: session.acceptance_invalidation_generation(),
            });
            ingress.wayland_active = true;
        }
        wl::BridgeEvent::Motion {
            transfer_id,
            position,
            transport_revision,
        } => {
            let Some(active) = matching_active_mut(runtime, window, transfer_id) else {
                return;
            };
            let Some(converted) = convert_surface_logical_position(position, ui_scale) else {
                fail_bridge_transfer(
                    runtime,
                    window,
                    transfer_id,
                    wl::TerminalReason::OfferRejected,
                );
                return;
            };
            active.position = converted;
            active.transport_revision = transport_revision;
            active.refresh_pending = true;
            commands
                .entity(active.pointer_entity)
                .insert(PointerLocation::new(Location {
                    target: active.target.clone(),
                    position: converted.bevy_logical,
                }));
        }
        wl::BridgeEvent::ActionChanged {
            transfer_id,
            action,
            transport_revision,
        } => {
            let Some(active) = matching_active_mut(runtime, window, transfer_id) else {
                return;
            };
            active.compositor_action = action.map(from_wl_action);
            active.transport_revision = transport_revision;
            active.refresh_pending = true;
        }
        wl::BridgeEvent::SourceActionsChanged {
            transfer_id,
            actions,
            transport_revision,
        } => {
            let Some(active) = matching_active_mut(runtime, window, transfer_id) else {
                return;
            };
            active.source_actions = actions;
            active.transport_revision = transport_revision;
            active.refresh_pending = true;
        }
        wl::BridgeEvent::HoverLeft {
            transfer_id,
            post_drop,
        } => {
            if let Some(active) = matching_active_mut(runtime, window, transfer_id) {
                // KWin commonly batches motion → action → drop → leave. Keep
                // the last custom-pointer hit and continue accepting until 4a
                // returns a terminal; clearing here would deadlock the fence.
                active.post_drop_left |= post_drop;
            }
        }
        wl::BridgeEvent::Drop(drop) => {
            if let Some(active) = matching_active_mut(runtime, window, drop.transfer_id) {
                active.pending_drop = Some(drop);
            }
        }
        wl::BridgeEvent::Terminal(terminal) => {
            log_terminal(terminal);
            if runtime.active.as_ref().is_some_and(|active| {
                active.window == window && active.bridge_id == terminal.transfer_id
            }) {
                let cancellation = runtime
                    .active
                    .as_ref()
                    .and_then(|active| terminal_delivery_cancellation(active, window, &terminal));
                clear_wayland(runtime, commands, ingress, highlights);
                if let Some(cancellation) = cancellation {
                    delivery_cancellations.write(cancellation);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_outgoing_event(
    runtime: &mut OsDndRuntime,
    window: Entity,
    event: wl::OutgoingEvent,
    commands: &mut Commands,
    session: &mut DragSession,
    ingress: &mut DndIngressGuard,
    highlights: &mut MessageWriter<DndHighlightChanged>,
    delivery_cancellations: &mut MessageWriter<DndDeliveryCancelled>,
) {
    let wl::OutgoingEvent::Terminal {
        transfer_id,
        reason,
    } = event
    else {
        debug!(?window, ?event, "outgoing OS DnD event");
        return;
    };
    let Some(outgoing) = runtime
        .outgoing
        .filter(|outgoing| outgoing.window == window && outgoing.bridge_id == transfer_id)
    else {
        debug!(
            ?window,
            ?transfer_id,
            ?reason,
            "stale outgoing OS DnD terminal"
        );
        return;
    };

    if reason != wl::OutgoingTerminalReason::Completed
        && runtime.active.as_ref().is_some_and(|active| {
            active.window == window && active.origin == DndOrigin::Internal(outgoing.source)
        })
    {
        let incoming_id = runtime
            .active
            .as_ref()
            .expect("matching echo checked above")
            .bridge_id;
        reject_incoming(
            runtime,
            window,
            incoming_id,
            "rejecting own-window echo after outgoing cancellation failed",
        );
        let cancellation = runtime
            .active
            .as_ref()
            .and_then(|active| terminal_delivery_cancellation_for(active, window, incoming_id));
        clear_wayland(runtime, commands, ingress, highlights);
        if let Some(cancellation) = cancellation {
            delivery_cancellations.write(cancellation);
        }
    }

    runtime.outgoing = None;
    session.finish_export(outgoing.source);
    if terminal_strands_the_release(reason) {
        session.clear_click_suppression(outgoing.source, commands);
    }
    log_outgoing_terminal(transfer_id, reason);
}

/// Whether this terminal means the press that started the drag can never be
/// released.
///
/// The click-suppression latch is meant to outlive the drag: it exists to
/// swallow the click that the physical release still to come would otherwise
/// deliver to the source. Its backstop is the level-triggered mouse state,
/// which only moves when winit reports a button event — so a terminal that
/// destroys the pointer holding the press leaves both the release and the
/// backstop unreachable, and the latch costs the next drag.
///
/// Only seat and pointer loss qualify. Ordinary cancellation, deadlines and
/// completion all leave the pointer alive and the release still coming, so
/// they must keep the latch; clearing there would let the drag's own release
/// click through to the source. Window teardown strands it too, but that path
/// clears the latch in `teardown_window` alongside the rest of the window's
/// state.
const fn terminal_strands_the_release(reason: wl::OutgoingTerminalReason) -> bool {
    matches!(
        reason,
        wl::OutgoingTerminalReason::SeatRemoved | wl::OutgoingTerminalReason::PointerCapabilityLost
    )
}

fn terminal_delivery_cancellation_for(
    active: &WaylandTransfer,
    window: Entity,
    transfer_id: wl::DataTransferId,
) -> Option<DndDeliveryCancelled> {
    (active.delivered
        && !active.app_resolved
        && active.window == window
        && active.bridge_id == transfer_id)
        .then_some(DndDeliveryCancelled {
            delivery_id: active.delivery_id,
        })
}

#[allow(clippy::too_many_arguments)]
fn cancel_outgoing_from_runtime(
    mut keyboard: MessageReader<KeyboardInput>,
    sources: Query<(), With<DragSource>>,
    mut runtime: NonSendMut<OsDndRuntime>,
    mut session: ResMut<DragSession>,
    mut commands: Commands,
    mut ingress: ResMut<DndIngressGuard>,
    mut highlights: MessageWriter<DndHighlightChanged>,
    mut delivery_cancellations: MessageWriter<DndDeliveryCancelled>,
) {
    let escape = keyboard.read().any(|input| {
        input.key_code == KeyCode::Escape && input.state == ButtonState::Pressed && !input.repeat
    });
    let source_missing = runtime
        .outgoing
        .is_some_and(|outgoing| !sources.contains(outgoing.source));
    if !escape && !source_missing {
        return;
    }
    let Some(outgoing) = runtime.outgoing else {
        return;
    };

    if runtime.active.as_ref().is_some_and(|active| {
        active.window == outgoing.window && active.origin == DndOrigin::Internal(outgoing.source)
    }) {
        let incoming_id = runtime
            .active
            .as_ref()
            .expect("matching echo checked above")
            .bridge_id;
        reject_incoming(
            &mut runtime,
            outgoing.window,
            incoming_id,
            "rejecting own-window echo during explicit cancellation failed",
        );
        let cancellation = runtime.active.as_ref().and_then(|active| {
            terminal_delivery_cancellation_for(active, outgoing.window, incoming_id)
        });
        clear_wayland(&mut runtime, &mut commands, &mut ingress, &mut highlights);
        if let Some(cancellation) = cancellation {
            delivery_cancellations.write(cancellation);
        }
    }

    let cancel_result = runtime
        .bridges
        .get_mut(&outgoing.window)
        .ok_or(wl::BridgeError::Send(wl::SendError::NoActiveTransfer))
        .and_then(|window_bridge| {
            window_bridge
                .bridge
                .cancel_outgoing(outgoing.bridge_id, Instant::now())
        });
    let events = runtime
        .bridges
        .get_mut(&outgoing.window)
        .map(|window_bridge| window_bridge.bridge.drain_outgoing_events())
        .unwrap_or_default();
    for event in events {
        handle_outgoing_event(
            &mut runtime,
            outgoing.window,
            event,
            &mut commands,
            &mut session,
            &mut ingress,
            &mut highlights,
            &mut delivery_cancellations,
        );
    }
    if let Err(error) = cancel_result {
        error!(
            window = ?outgoing.window,
            source = ?outgoing.source,
            ?error,
            "cancelling outgoing OS DnD failed"
        );
        // Never strand CTK because a bridge disappeared between the input and
        // cancellation systems.
        runtime.outgoing = None;
        session.finish_export(outgoing.source);
    }
}

fn terminal_delivery_cancellation(
    active: &WaylandTransfer,
    window: Entity,
    terminal: &wl::TerminalEvent,
) -> Option<DndDeliveryCancelled> {
    // Cancellation means "the application is holding work for a delivery that
    // can no longer resolve". The test is therefore whether the application has
    // already answered — not the terminal reason. `Completed`, `AppDismissed`
    // and `AppOperationFailed` are all reasons an application resolution
    // produces, so keying off the reason alone would report a cancellation for
    // a delivery the application had already completed, and the public contract
    // would emit both for one delivery.
    (active.delivered
        && !active.app_resolved
        && active.window == window
        && active.bridge_id == terminal.transfer_id)
        .then_some(DndDeliveryCancelled {
            delivery_id: active.delivery_id,
        })
}

fn matching_active_mut(
    runtime: &mut OsDndRuntime,
    window: Entity,
    transfer_id: wl::DataTransferId,
) -> Option<&mut WaylandTransfer> {
    runtime
        .active
        .as_mut()
        .filter(|active| active.window == window && active.bridge_id == transfer_id)
}

fn preferred_mime(mime_types: &[wl::types::MimeDescriptor]) -> Option<(String, PayloadSummary)> {
    mime_types
        .iter()
        .find(|mime| mime.essence == "text/uri-list")
        .map(|mime| (mime.raw.clone(), PayloadSummary::Paths { count: None }))
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| mime.essence == "text/plain")
                .map(|mime| {
                    (
                        mime.raw.clone(),
                        PayloadSummary::Text {
                            bytes: None,
                            chars: None,
                        },
                    )
                })
        })
}

fn refresh_wayland_from_application_invalidation(
    session: Res<DragSession>,
    mut runtime: NonSendMut<OsDndRuntime>,
) {
    let Some(active) = runtime.active.as_mut().filter(|active| !active.delivered) else {
        return;
    };
    let invalidation_generation = session.acceptance_invalidation_generation();
    if active.acceptance_invalidation_generation != invalidation_generation {
        active.acceptance_invalidation_generation = invalidation_generation;
        active.refresh_pending = true;
    }
}

fn update_wayland_context(
    hover_map: Res<HoverMap>,
    keys: Res<ButtonInput<KeyCode>>,
    parents: Query<&ChildOf>,
    targets: Query<(), With<DropTarget>>,
    coordinator: Option<Res<ModalCoordinator>>,
    mut runtime: NonSendMut<OsDndRuntime>,
    mut highlights: MessageWriter<DndHighlightChanged>,
) {
    let Some(active) = runtime.active.as_mut().filter(|active| !active.delivered) else {
        return;
    };
    let modifiers = modifiers_from_keys(&keys);
    let modifiers_changed = modifiers != active.modifiers;
    if modifiers_changed {
        active.modifiers = modifiers;
        // The compositor action acknowledges the previous set_actions. A local
        // modifier edge selects the next preference before its acknowledgement.
        active.compositor_action = None;
    }
    let modal_root = coordinator
        .as_deref()
        .and_then(ModalCoordinator::active_root);
    let chain = dnd::target_chain(
        &hover_map,
        active.pointer_id,
        Entity::PLACEHOLDER,
        modal_root,
        &parents,
        &targets,
    );
    if active.refresh_pending || modifiers_changed || chain != active.target_chain {
        clear_highlight(active, &mut highlights);
        active.reset_candidates(chain);
        active.refresh_pending = false;
    }
}

fn propose_wayland(
    mut runtime: NonSendMut<OsDndRuntime>,
    mut proposals: MessageWriter<AcceptanceProposal>,
) {
    let Some(active) = runtime.active.as_mut().filter(|active| !active.delivered) else {
        return;
    };
    let Some(target) = active.candidate else {
        return;
    };
    let proposal = AcceptanceProposal {
        proposal_id: active.proposal_id,
        revision: active.revision(),
        target,
        origin: active.origin,
        payload_summary: active.payload_summary,
        modifiers: active.modifiers,
        position: active.position.bevy_logical,
        selected_action: active.selected_action(),
    };
    active.last_proposal = Some(proposal.clone());
    proposals.write(proposal);
}

fn apply_wayland_acceptance(
    mut responses: MessageReader<DropAcceptance>,
    mut runtime: NonSendMut<OsDndRuntime>,
    mut highlights: MessageWriter<DndHighlightChanged>,
) {
    let Some(active) = runtime.active.as_ref().filter(|active| !active.delivered) else {
        responses.clear();
        return;
    };
    let Some(proposal) = active.last_proposal.clone() else {
        responses.clear();
        return;
    };
    let current: Vec<_> = responses
        .read()
        .copied()
        .filter(|response| {
            response.proposal_id == proposal.proposal_id && response.revision == proposal.revision
        })
        .collect();
    if current.len() != 1 {
        if current.len() > 1 {
            error!(
                proposal_revision = proposal.revision.get(),
                response_count = current.len(),
                "Wayland DnD resolver returned duplicate acceptances"
            );
        }
        deny_wayland_candidate(&mut runtime, &mut highlights);
        return;
    }

    let response = restrict_to_source_actions(current[0], active.source_actions);
    let Some(action) = dnd::negotiated_action(proposal.selected_action, response) else {
        deny_wayland_candidate(&mut runtime, &mut highlights);
        return;
    };
    let Some(active) = runtime.active.as_mut() else {
        return;
    };
    let Some(target) = active.candidate else {
        return;
    };
    if active.highlighted != Some((target, action)) {
        clear_highlight(active, &mut highlights);
        active.highlighted = Some((target, action));
        highlights.write(DndHighlightChanged {
            target,
            highlighted: true,
            action,
        });
    }

    let acceptance = wl::Acceptance {
        mime_type: active.mime_type.clone(),
        allowed_actions: to_wl_mask(response.allowed_actions),
        preferred: to_wl_action(action),
        context: wl::AcceptedContext {
            target: wl::TargetId(target.to_bits()),
            action: to_wl_action(action),
            modifiers: to_wl_modifiers(active.modifiers),
            origin: to_wl_origin(active.origin, active.bridge_id),
            delivery_id: wl::DeliveryId(active.delivery_id.get()),
            revision: wl::ProposalRevision(active.revision().get()),
        },
        observed_transport_revision: active.transport_revision,
    };
    let window = active.window;
    let bridge_id = active.bridge_id;
    let should_request = !active.data_requested;
    let mime_type = active.mime_type.clone();
    let applied = AppliedAcceptance {
        target,
        action,
        modifiers: active.modifiers,
        proposal_revision: active.revision(),
        transport_revision: active.transport_revision,
    };

    let accept_result = runtime
        .bridges
        .get_mut(&window)
        .expect("active transfer owns a bridge")
        .bridge
        .accept(acceptance);
    if let Err(error) = accept_result {
        surface_acceptance_error(&mut runtime, window, bridge_id, error);
        return;
    }
    if should_request {
        let request_result = runtime
            .bridges
            .get_mut(&window)
            .expect("active transfer owns a bridge")
            .bridge
            .request_data(bridge_id, &mime_type, Instant::now());
        if let Err(error) = request_result {
            error!(?window, ?bridge_id, ?error, "OS DnD payload request failed");
            return;
        }
    }
    if let Some(active) = runtime.active.as_mut() {
        active.data_requested |= should_request;
        active.last_applied = Some(applied);
    }
}

fn deny_wayland_candidate(
    runtime: &mut OsDndRuntime,
    highlights: &mut MessageWriter<DndHighlightChanged>,
) {
    let Some(active) = runtime.active.as_mut() else {
        return;
    };
    clear_highlight(active, highlights);
    let window = active.window;
    let id = active.bridge_id;
    let may_clear = !active.post_drop_left;
    active.advance_candidate();
    if may_clear {
        if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
            if let Err(error) = window_bridge.bridge.clear_acceptance(id) {
                error!(
                    ?window,
                    ?id,
                    ?error,
                    "clearing denied OS DnD acceptance failed"
                );
            }
        }
    }
}

fn deliver_wayland_drop(
    targets: Query<(), With<DropTarget>>,
    mut runtime: NonSendMut<OsDndRuntime>,
    mut drops: MessageWriter<DndDrop>,
    mut highlights: MessageWriter<DndHighlightChanged>,
    mut commands: Commands,
) {
    let Some(active) = runtime.active.as_mut().filter(|active| !active.delivered) else {
        return;
    };
    let Some(drop) = active.pending_drop.take() else {
        return;
    };
    let target = Entity::from_bits(drop.target.0);
    let expected = active.last_applied;
    let current = expected.is_some_and(|accepted| {
        accepted.target == target
            && to_wl_action(accepted.action) == drop.action
            && to_wl_modifiers(accepted.modifiers) == drop.modifiers
            && accepted.proposal_revision.get() == drop.accepted_revision.0
            && accepted.transport_revision <= active.transport_revision
    });
    if !current {
        let window = active.window;
        let id = active.bridge_id;
        if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
            if let Err(error) =
                window_bridge
                    .bridge
                    .invalidate_revision(id, drop.accepted_revision, Instant::now())
            {
                error!(?window, ?id, ?error, "invalidating stale OS drop failed");
            }
        }
        return;
    }
    if !targets.contains(target) {
        let window = active.window;
        let id = active.bridge_id;
        if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
            if let Err(error) = window_bridge
                .bridge
                .target_lost(id, drop.target, Instant::now())
            {
                error!(?window, ?id, ?error, "retiring lost OS drop target failed");
            }
        }
        return;
    }
    if drop.delivery_id.0 != active.delivery_id.get()
        || drop.origin != to_wl_origin(active.origin, active.bridge_id)
    {
        error!(
            ?drop,
            expected_delivery = active.delivery_id.get(),
            "OS bridge returned a mismatched delivery"
        );
        let window = active.window;
        let bridge_id = active.bridge_id;
        fail_bridge_transfer(
            &mut runtime,
            window,
            bridge_id,
            wl::TerminalReason::ActionMismatch,
        );
        return;
    }
    let payload = match drop.payload {
        wl::DragPayload::Paths(paths) => DragPayload::Paths(paths),
        wl::DragPayload::Text(text) => DragPayload::Text(text),
    };
    drops.write(DndDrop {
        origin: active.origin,
        target,
        payload,
        action: from_wl_action(drop.action),
        modifiers: from_wl_modifiers(drop.modifiers),
        delivery_id: active.delivery_id,
        decision_requirement: DropDecisionRequirement::Wayland,
    });
    active.delivered = true;
    clear_highlight(active, &mut highlights);
    commands.entity(active.pointer_entity).try_despawn();
}

fn forward_delivery_results(
    mut decisions: MessageReader<DropDecision>,
    mut completions: MessageReader<DropComplete>,
    mut decision_results: MessageWriter<DropDecisionResult>,
    mut runtime: NonSendMut<OsDndRuntime>,
) {
    for decision in decisions.read() {
        let active = runtime
            .active
            .as_ref()
            .filter(|active| active.delivery_id == decision.delivery_id)
            .map(|active| (active.window, active.bridge_id));
        let bridge_decision = wl::DropDecision {
            delivery_id: wl::DeliveryId(decision.delivery_id.get()),
            decision: match decision.decision {
                DropDecisionKind::Copy => wl::DropDecisionKind::Copy,
                DropDecisionKind::Move => wl::DropDecisionKind::Move,
                DropDecisionKind::Dismissed => wl::DropDecisionKind::Dismissed,
            },
        };
        let result = active
            .ok_or_else(|| "delivery is no longer active".to_string())
            .and_then(|(window, bridge_id)| {
                let Some(window_bridge) = runtime.bridges.get_mut(&window) else {
                    return Err("delivery bridge is no longer active".to_string());
                };
                window_bridge
                    .bridge
                    .decide_drop(bridge_id, bridge_decision, Instant::now())
                    .map_err(|error| {
                        error!(?window, ?bridge_id, ?error, "OS Ask decision failed");
                        error.to_string()
                    })
            });
        decision_results.write(DropDecisionResult {
            delivery_id: decision.delivery_id,
            status: match result {
                Ok(()) => DropDecisionStatus::Accepted,
                Err(error) => {
                    if active.is_none() {
                        error!(
                            delivery_id = decision.delivery_id.get(),
                            %error,
                            "OS Ask decision failed"
                        );
                    }
                    DropDecisionStatus::Rejected(error)
                }
            },
        });
    }
    for complete in completions.read() {
        let Some(active) = runtime
            .active
            .as_ref()
            .filter(|active| active.delivery_id == complete.delivery_id)
        else {
            continue;
        };
        let window = active.window;
        let bridge_id = active.bridge_id;
        // The application has answered for this delivery. Record it before the
        // transport call, because the terminal that a resolution produces
        // (`AppDismissed`, `AppOperationFailed`) must not then be reported back
        // to the application as a cancellation.
        if let Some(active) = runtime.active.as_mut() {
            active.app_resolved = true;
        }
        let complete = wl::DropComplete {
            delivery_id: wl::DeliveryId(complete.delivery_id.get()),
            outcome: match complete.outcome {
                DropOutcome::Completed(action) => wl::DropOutcome::Completed(to_wl_action(action)),
                DropOutcome::Failed => wl::DropOutcome::Failed,
            },
        };
        if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
            if let Err(error) =
                window_bridge
                    .bridge
                    .complete_drop(bridge_id, complete, Instant::now())
            {
                // A dismissed Ask has already retired the bridge; the CTK
                // completion is still exactly-once, but there is no completion
                // latch left to notify.
                if !matches!(error, wl::BridgeError::NoActiveTransfer) {
                    error!(?window, ?bridge_id, ?error, "OS drop completion failed");
                }
            }
        }
    }
}

fn clear_highlight(
    active: &mut WaylandTransfer,
    highlights: &mut MessageWriter<DndHighlightChanged>,
) {
    if let Some((target, action)) = active.highlighted.take() {
        highlights.write(DndHighlightChanged {
            target,
            highlighted: false,
            action,
        });
    }
}

fn clear_wayland(
    runtime: &mut OsDndRuntime,
    commands: &mut Commands,
    ingress: &mut DndIngressGuard,
    highlights: &mut MessageWriter<DndHighlightChanged>,
) {
    let Some(mut active) = runtime.active.take() else {
        ingress.wayland_active = false;
        return;
    };
    clear_highlight(&mut active, highlights);
    commands.entity(active.pointer_entity).try_despawn();
    ingress.wayland_active = false;
}

fn teardown_window(
    runtime: &mut OsDndRuntime,
    window: Entity,
    commands: &mut Commands,
    session: &mut DragSession,
    ingress: &mut DndIngressGuard,
    highlights: &mut MessageWriter<DndHighlightChanged>,
    delivery_cancellations: &mut MessageWriter<DndDeliveryCancelled>,
) {
    if runtime
        .active
        .as_ref()
        .is_some_and(|active| active.window == window)
    {
        if let Some(delivery_id) = runtime
            .active
            .as_ref()
            .filter(|active| active.delivered && !active.app_resolved)
            .map(|active| active.delivery_id)
        {
            delivery_cancellations.write(DndDeliveryCancelled { delivery_id });
        }
        clear_wayland(runtime, commands, ingress, highlights);
    }
    if let Some(mut window_bridge) = runtime.bridges.remove(&window) {
        for event in window_bridge.bridge.teardown() {
            if let wl::BridgeEvent::Terminal(terminal) = event {
                log_terminal(terminal);
            }
        }
        for event in window_bridge.bridge.drain_outgoing_events() {
            if let wl::OutgoingEvent::Terminal {
                transfer_id,
                reason,
            } = event
            {
                log_outgoing_terminal(transfer_id, reason);
            }
        }
    }
    if let Some(outgoing) = runtime
        .outgoing
        .filter(|outgoing| outgoing.window == window)
    {
        runtime.outgoing = None;
        session.finish_export(outgoing.source);
        // The release that would clear this latch is delivered by winit only to
        // a surviving window, so a button-up over the desktop after this window
        // closed would never arrive and the next drag would be refused.
        session.clear_click_suppression(outgoing.source, commands);
    }
    runtime.unavailable_windows.remove(&window);
}

fn fail_bridge_transfer(
    runtime: &mut OsDndRuntime,
    window: Entity,
    transfer_id: wl::DataTransferId,
    reason: wl::TerminalReason,
) {
    if let Some(window_bridge) = runtime.bridges.get_mut(&window) {
        if let Err(error) = window_bridge.bridge.fail_transfer(transfer_id, reason) {
            error!(
                ?window,
                ?transfer_id,
                ?reason,
                ?error,
                "failing OS DnD transfer failed"
            );
        }
    }
}

fn surface_acceptance_error(
    runtime: &mut OsDndRuntime,
    window: Entity,
    transfer_id: wl::DataTransferId,
    bridge_error: wl::BridgeError,
) {
    match &bridge_error {
        wl::BridgeError::InvalidAcceptance(error) => {
            error!(
                ?window,
                ?transfer_id,
                ?error,
                "ctk produced an invalid OS DnD acceptance (programming error)"
            );
            fail_bridge_transfer(
                runtime,
                window,
                transfer_id,
                wl::TerminalReason::ActionMismatch,
            );
        }
        _ => error!(
            ?window,
            ?transfer_id,
            ?bridge_error,
            "applying OS DnD acceptance failed"
        ),
    }
}

fn modifiers_from_keys(keys: &ButtonInput<KeyCode>) -> Modifiers {
    Modifiers {
        control: keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
        shift: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        alt: keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]),
        super_key: keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]),
    }
}

fn requested_action(modifiers: Modifiers) -> DropAction {
    if modifiers.control {
        DropAction::Copy
    } else if modifiers.shift {
        DropAction::Move
    } else {
        DropAction::Ask
    }
}

fn restrict_to_source_actions(
    acceptance: DropAcceptance,
    source: wl::ActionMask,
) -> DropAcceptance {
    if source.is_empty() {
        return acceptance;
    }
    let mut allowed = ActionMask::NONE;
    for action in [DropAction::Copy, DropAction::Move, DropAction::Ask] {
        if acceptance.allowed_actions.contains(action) && source.contains(to_wl_action(action)) {
            allowed |= action_mask(action);
        }
    }
    DropAcceptance {
        allowed_actions: allowed,
        ..acceptance
    }
}

fn action_mask(action: DropAction) -> ActionMask {
    match action {
        DropAction::Copy => ActionMask::COPY,
        DropAction::Move => ActionMask::MOVE,
        DropAction::Ask => ActionMask::ASK,
    }
}

fn to_wl_mask(actions: ActionMask) -> wl::ActionMask {
    let mut mapped = wl::ActionMask::NONE;
    for action in [DropAction::Copy, DropAction::Move, DropAction::Ask] {
        if actions.contains(action) {
            mapped |= match action {
                DropAction::Copy => wl::ActionMask::COPY,
                DropAction::Move => wl::ActionMask::MOVE,
                DropAction::Ask => wl::ActionMask::ASK,
            };
        }
    }
    mapped
}

fn to_wl_action(action: DropAction) -> wl::DndAction {
    match action {
        DropAction::Copy => wl::DndAction::Copy,
        DropAction::Move => wl::DndAction::Move,
        DropAction::Ask => wl::DndAction::Ask,
    }
}

fn to_wl_origin(origin: DndOrigin, bridge_id: wl::DataTransferId) -> wl::DndOrigin {
    match origin {
        DndOrigin::Internal(source) => wl::DndOrigin::Internal(wl::SourceId(source.to_bits())),
        DndOrigin::External(_) => wl::DndOrigin::External(bridge_id),
    }
}

fn from_wl_action(action: wl::DndAction) -> DropAction {
    match action {
        wl::DndAction::Copy => DropAction::Copy,
        wl::DndAction::Move => DropAction::Move,
        wl::DndAction::Ask => DropAction::Ask,
    }
}

fn to_wl_modifiers(modifiers: Modifiers) -> wl::Modifiers {
    wl::Modifiers {
        control: modifiers.control,
        shift: modifiers.shift,
        alt: modifiers.alt,
        super_key: modifiers.super_key,
    }
}

fn from_wl_modifiers(modifiers: wl::Modifiers) -> Modifiers {
    Modifiers {
        control: modifiers.control,
        shift: modifiers.shift,
        alt: modifiers.alt,
        super_key: modifiers.super_key,
    }
}

fn log_terminal(terminal: wl::TerminalEvent) {
    let diagnosis = match terminal.reason {
        wl::TerminalReason::Completed => "application operation and compositor finish completed",
        wl::TerminalReason::OfferRejected => "destination rejected the offer",
        wl::TerminalReason::SourceCancelled => "source cancelled the drag",
        wl::TerminalReason::SourceFinished => "source ended the offer before completion",
        wl::TerminalReason::PipeFailure => "payload pipe failed",
        wl::TerminalReason::WindowTeardown => "destination window was torn down",
        wl::TerminalReason::LateWorkerResult => "payload worker returned after transfer retirement",
        wl::TerminalReason::LeaveBeforeDrop => "pointer left before a physical drop",
        wl::TerminalReason::TargetLost => "accepted CTK target disappeared",
        wl::TerminalReason::RevisionInvalidated => "accepted CTK revision became stale",
        wl::TerminalReason::DropFenceExpired => {
            "drop acceptance did not cover the recorded transport revision within 500 ms"
        }
        wl::TerminalReason::AppDismissed => "application dismissed Ask",
        wl::TerminalReason::AppOperationFailed => "application reported the file operation failed",
        wl::TerminalReason::ActionMismatch => "application and compositor actions disagreed",
        wl::TerminalReason::AskConfirmationDeadlineExpired => "Ask confirmation deadline expired",
        wl::TerminalReason::PayloadRequestDeadlineExpired => "payload request deadline expired",
        wl::TerminalReason::PostDecisionDeadlineExpired => {
            "post-decision completion deadline expired"
        }
        wl::TerminalReason::PostDropFinalActionDeadlineExpired => {
            "final compositor action deadline expired"
        }
        wl::TerminalReason::FinalActionRejected => "source withdrew or rejected the final action",
        wl::TerminalReason::PayloadTooLarge => "payload exceeded the configured byte cap",
        wl::TerminalReason::PayloadInactivityExpired => {
            "payload pipe made no progress before its inactivity deadline"
        }
        wl::TerminalReason::PayloadWorkerCapacityExceeded => {
            "all bounded payload reader slots were in flight"
        }
        wl::TerminalReason::WaylandConnectionLost => "Wayland display connection died",
        wl::TerminalReason::QueueOverflow => "bounded lifecycle queue overflowed",
        wl::TerminalReason::OfferReplaced => "the data device entered a replacement drag",
        wl::TerminalReason::OfferProxyDead => "the Wayland offer proxy was dead",
    };
    match terminal.disposition {
        wl::TerminalDisposition::Finished => {
            debug!(?terminal.transfer_id, ?terminal.reason, diagnosis, "OS DnD transfer terminal");
        }
        wl::TerminalDisposition::Rejected => {
            warn!(?terminal.transfer_id, ?terminal.reason, diagnosis, "OS DnD transfer rejected");
        }
    }
}

fn log_outgoing_terminal(transfer_id: wl::DataTransferId, reason: wl::OutgoingTerminalReason) {
    match reason {
        wl::OutgoingTerminalReason::Completed => {
            debug!(?transfer_id, ?reason, "outgoing OS DnD transfer completed");
        }
        _ => {
            warn!(?transfer_id, ?reason, "outgoing OS DnD transfer cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnd::ExportIconRasterError;
    use bevy::ecs::system::SystemState;
    use bevy::picking::backend::HitData;
    use bevy::picking::pointer::{PointerAction, PointerInput};

    #[test]
    fn export_raster_satisfies_the_wayland_icon_contract() {
        let raster = ExportIconRaster::new(vec![0; 80 * 80 * 4], 80, 80, 2).unwrap();
        let outgoing = outgoing_icon_from_raster(&raster, (-20, -20)).unwrap();

        assert_eq!(outgoing.pixels(), raster.pixels());
        assert_eq!(outgoing.pixels().len(), 80 * 80 * 4);
        assert_eq!((outgoing.width(), outgoing.height()), (80, 80));
        assert_eq!(outgoing.buffer_scale(), 2);
        assert_eq!(outgoing.offset(), (-20, -20));
    }

    /// Pins CTK's duplicated SHM bounds to the transport's own.
    ///
    /// Both constructors validate the pixel length *last*, so passing an empty
    /// buffer clears every geometry and pool check and reports the length it
    /// wanted. That is what makes this checkable without allocating the two
    /// gigabytes the limit describes — the drift this guards against is in the
    /// constants, not in the pixels.
    #[test]
    fn export_raster_shm_bounds_match_the_transport_exactly() {
        let largest_pool = (i32::MAX as usize) & !63;
        let largest_width = u32::try_from(largest_pool / 4).unwrap();

        assert_eq!(
            ExportIconRaster::new(Vec::new(), largest_width, 1, 1).unwrap_err(),
            ExportIconRasterError::InvalidPixelLength {
                expected: largest_pool,
                actual: 0,
            },
            "CTK must accept the geometry the transport accepts"
        );
        assert_eq!(
            wl::OutgoingIcon::new(Vec::new(), largest_width, 1, 1, (0, 0)).unwrap_err(),
            wl::OutgoingIconError::InvalidPixelLength {
                expected: largest_pool,
                actual: 0,
            },
        );

        let first_rejected = largest_width + 1;
        let over = wayland_shm_slot_len_for_test(first_rejected);
        assert_eq!(
            ExportIconRaster::new(Vec::new(), first_rejected, 1, 1).unwrap_err(),
            ExportIconRasterError::ShmPoolTooLarge { required: over },
            "CTK must reject the first geometry the transport rejects"
        );
        assert_eq!(
            wl::OutgoingIcon::new(Vec::new(), first_rejected, 1, 1, (0, 0)).unwrap_err(),
            wl::OutgoingIconError::ShmPoolTooLarge { required: over },
        );
    }

    fn wayland_shm_slot_len_for_test(width: u32) -> usize {
        ((width as usize * 4) + 63) & !63
    }

    /// `origin` is explicit because it is the axis this module must not
    /// conflate: a Wayland transfer carries an own-window echo just as readily
    /// as a foreign offer, and only the caller knows which it is modelling.
    fn wayland_transfer_fixture(
        session: &mut DragSession,
        window: Entity,
        target_entity: Entity,
        pointer_entity: Entity,
        pointer_id: PointerId,
        origin: DndOrigin,
    ) -> WaylandTransfer {
        WaylandTransfer {
            window,
            bridge_id: wl::DataTransferId(7),
            origin,
            pointer_id,
            pointer_entity,
            target: RenderTarget::Window(WindowRef::Entity(window))
                .normalize(None)
                .unwrap(),
            mime_type: "text/uri-list".into(),
            payload_summary: PayloadSummary::Paths { count: None },
            source_actions: wl::ActionMask::COPY | wl::ActionMask::ASK,
            compositor_action: None,
            position: OsDndPosition {
                bevy_logical: Vec2::new(20.0, 30.0),
                ctk_ui_logical: Vec2::new(20.0, 30.0),
            },
            modifiers: Modifiers::default(),
            transport_revision: wl::TransportRevision(3),
            proposal_id: session.allocate_proposal_id(),
            proposal_revision: 4,
            delivery_id: session.allocate_delivery_id(),
            target_chain: vec![target_entity],
            candidate_index: 1,
            candidate: None,
            highlighted: None,
            last_proposal: None,
            last_applied: None,
            data_requested: true,
            post_drop_left: false,
            pending_drop: None,
            delivered: false,
            app_resolved: false,
            refresh_pending: false,
            acceptance_invalidation_generation: session.acceptance_invalidation_generation(),
        }
    }

    /// The decision requirement follows the delivery path, so an own-window
    /// echo — internal origin, Wayland delivery — must still take the protocol
    /// handshake. Tagging it from the origin instead would start the file
    /// operation while the bridge is still waiting for the final non-`Ask`
    /// decision, and the source would then time out after the copy had already
    /// run. Asserting both origins is what makes that regression fail here.
    #[test]
    fn wayland_delivery_requires_the_wayland_decision_path() {
        for origin in [
            DndOrigin::External(TransferId(11)),
            DndOrigin::Internal(Entity::from_bits(2)),
        ] {
            assert_eq!(
                delivered_decision_requirement(origin),
                DropDecisionRequirement::Wayland,
                "Wayland delivery of {origin:?} must require the protocol decision"
            );
        }
    }

    fn delivered_decision_requirement(origin: DndOrigin) -> DropDecisionRequirement {
        let mut app = App::new();
        let window = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn(DropTarget).id();
        let pointer_entity = app.world_mut().spawn_empty().id();
        let pointer_id = PointerId::Custom(Uuid::from_u128(9));
        let mut session = DragSession::default();
        let mut active = wayland_transfer_fixture(
            &mut session,
            window,
            target,
            pointer_entity,
            pointer_id,
            origin,
        );
        active.last_applied = Some(AppliedAcceptance {
            target,
            action: DropAction::Ask,
            modifiers: Modifiers::default(),
            proposal_revision: active.revision(),
            transport_revision: active.transport_revision,
        });
        active.pending_drop = Some(wl::DropEvent {
            transfer_id: active.bridge_id,
            target: wl::TargetId(target.to_bits()),
            payload: wl::DragPayload::Paths(vec![PathBuf::from("/source")]),
            action: wl::DndAction::Ask,
            modifiers: to_wl_modifiers(active.modifiers),
            origin: to_wl_origin(active.origin, active.bridge_id),
            delivery_id: wl::DeliveryId(active.delivery_id.get()),
            accepted_revision: wl::ProposalRevision(active.revision().get()),
        });
        app.add_message::<DndDrop>()
            .add_message::<DndHighlightChanged>()
            .insert_non_send(OsDndRuntime {
                active: Some(active),
                ..default()
            })
            .add_systems(Update, deliver_wayland_drop);

        app.update();

        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        cursor.read(drops).last().unwrap().decision_requirement
    }

    fn response(allowed: ActionMask, preferred: DropAction) -> DropAcceptance {
        let proposal_id = DragSession::default().allocate_proposal_id();
        DropAcceptance {
            proposal_id,
            revision: ProposalRevision::from_raw(9),
            allowed_actions: allowed,
            preferred,
        }
    }

    #[test]
    fn own_window_echo_has_exactly_one_internal_delivery_route() {
        let window = Entity::from_bits(1);
        let source = Entity::from_bits(2);
        let outgoing = OutgoingTransfer {
            window,
            bridge_id: wl::DataTransferId(3),
            source,
        };
        let routes = [
            route_incoming_origin(
                wl::DndOrigin::Internal(wl::SourceId(source.to_bits())),
                window,
                Some(&outgoing),
                true,
                Some(source),
            ),
            route_incoming_origin(
                wl::DndOrigin::External(wl::DataTransferId(4)),
                window,
                Some(&outgoing),
                true,
                Some(source),
            ),
        ];

        assert_eq!(
            routes.into_iter().flatten().collect::<Vec<IncomingRoute>>(),
            vec![IncomingRoute::Internal(source)]
        );
    }

    /// Pins the documented cross-window precondition. Our own drag over a
    /// second window of this same process reaches that bridge as `External`
    /// (its nonce registry never issued the nonce), and the live export
    /// declines it. Both shapes are asserted so a future change that makes
    /// multi-window export work has to update this test deliberately.
    #[test]
    fn an_own_drag_over_a_second_window_is_declined_not_delivered() {
        let source = Entity::from_bits(2);
        let outgoing = OutgoingTransfer {
            window: Entity::from_bits(1),
            bridge_id: wl::DataTransferId(3),
            source,
        };
        let other_window = Entity::from_bits(7);

        for origin in [
            wl::DndOrigin::External(wl::DataTransferId(4)),
            wl::DndOrigin::Internal(wl::SourceId(source.to_bits())),
        ] {
            assert_eq!(
                route_incoming_origin(origin, other_window, Some(&outgoing), true, Some(source)),
                None
            );
        }
    }

    /// Exhaustive by construction: the `match` below has no wildcard, so a new
    /// terminal reason in `cosmix-wl-dnd` will not compile until someone
    /// decides which side it falls on. Both mistakes are real — keeping the
    /// latch when no release can arrive costs the next drag, and dropping it
    /// while the pointer lives lets the drag's own release click through to
    /// the source it was dragged from.
    #[test]
    fn only_seat_and_pointer_loss_strand_the_click_latch() {
        use wl::OutgoingTerminalReason as Reason;

        for reason in [
            Reason::Completed,
            Reason::CompositorCancelled,
            Reason::StartIgnored,
            Reason::ActiveDeadlineExpired,
            Reason::FinishDeadlineExpired,
            Reason::WriterSpawnFailed,
            Reason::WriterFailed,
            Reason::UnsupportedMime,
            Reason::SourceProxyDead,
            Reason::SeatRemoved,
            Reason::PointerCapabilityLost,
            Reason::WindowTeardown,
            Reason::WaylandConnectionLost,
            Reason::QueueOverflow,
        ] {
            let strands = match reason {
                // The pointer holding the press is gone with the seat or the
                // capability; neither the release nor the level-triggered
                // backstop can ever move again.
                Reason::SeatRemoved | Reason::PointerCapabilityLost => true,
                // `WindowTeardown` also strands it, but that path clears the
                // latch in `teardown_window` with the rest of the window's
                // state; every other reason leaves the pointer alive and the
                // release still coming.
                Reason::Completed
                | Reason::CompositorCancelled
                | Reason::StartIgnored
                | Reason::ActiveDeadlineExpired
                | Reason::FinishDeadlineExpired
                | Reason::WriterSpawnFailed
                | Reason::WriterFailed
                | Reason::UnsupportedMime
                | Reason::SourceProxyDead
                | Reason::WindowTeardown
                | Reason::WaylandConnectionLost
                | Reason::QueueOverflow => false,
            };
            assert_eq!(terminal_strands_the_release(reason), strands, "{reason:?}");
        }
    }

    #[test]
    fn wrong_source_echo_is_rejected_without_disturbing_the_live_export() {
        let window = Entity::from_bits(1);
        let source = Entity::from_bits(2);
        let outgoing = OutgoingTransfer {
            window,
            bridge_id: wl::DataTransferId(3),
            source,
        };

        assert_eq!(
            route_incoming_origin(
                wl::DndOrigin::Internal(wl::SourceId(Entity::from_bits(9).to_bits())),
                window,
                Some(&outgoing),
                true,
                Some(source),
            ),
            None
        );
        assert_eq!(outgoing.source, source);
        assert_eq!(outgoing.bridge_id, wl::DataTransferId(3));
    }

    #[test]
    #[allow(clippy::type_complexity)] // Mirrors the exact Bevy query used by the handoff.
    fn bevy_pointer_drag_state_is_cleared_at_handoff() {
        let mut app = App::new();
        let window = app.world_mut().spawn_empty().id();
        let source = app.world_mut().spawn_empty().id();
        let location = Location {
            target: RenderTarget::Window(WindowRef::Entity(window))
                .normalize(None)
                .unwrap(),
            position: Vec2::new(10.0, 20.0),
        };
        app.add_message::<PointerInput>();
        app.world_mut()
            .spawn((PointerId::Mouse, PointerLocation::new(location.clone())));
        app.world_mut().write_message(PointerInput::new(
            PointerId::Mouse,
            location.clone(),
            PointerAction::Press(PointerButton::Primary),
        ));
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();

        let mut pointer_state = PointerState::default();
        pointer_state
            .get_mut(PointerId::Mouse, PointerButton::Primary)
            .pressing
            .insert(
                source,
                (
                    location,
                    Instant::now(),
                    HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                ),
            );
        app.insert_resource(pointer_state);
        let mut state: SystemState<(
            ResMut<PointerState>,
            Query<(&PointerId, &PointerLocation, &mut PointerPress)>,
        )> = SystemState::new(app.world_mut());
        {
            let (mut pointer_state, mut pointers) = state.get_mut(app.world_mut()).unwrap();
            suppress_bevy_drag(PointerId::Mouse, &mut pointer_state, &mut pointers);
        }
        state.apply(app.world_mut());

        assert!(app
            .world()
            .resource::<PointerState>()
            .get(PointerId::Mouse, PointerButton::Primary)
            .is_some_and(|state| state.pressing.is_empty() && state.dragging.is_empty()));
        let mut query = app.world_mut().query::<(&PointerId, &PointerPress)>();
        let (_, press) = query.single(app.world()).unwrap();
        assert!(!press.is_any_pressed());
    }

    #[test]
    fn surface_logical_conversion_is_fractional_scale_safe() {
        let position = wl::Position { x: 150.0, y: 75.0 };
        for (scale, expected_ui) in [
            (1.25, Vec2::new(120.0, 60.0)),
            (1.5, Vec2::new(100.0, 50.0)),
            (2.0, Vec2::new(75.0, 37.5)),
        ] {
            let converted = convert_surface_logical_position(position, scale).unwrap();
            assert_eq!(converted.bevy_logical, Vec2::new(150.0, 75.0));
            assert_eq!(converted.ctk_ui_logical, expected_ui);
        }
    }

    #[test]
    fn non_finite_surface_coordinates_fail_closed() {
        assert!(convert_surface_logical_position(
            wl::Position {
                x: f64::NAN,
                y: 1.0,
            },
            1.0,
        )
        .is_none());
    }

    #[test]
    fn acceptance_mapping_intersects_source_mask_and_preserves_preference() {
        let mapped = restrict_to_source_actions(
            response(ActionMask::ALL, DropAction::Move),
            wl::ActionMask::COPY | wl::ActionMask::MOVE,
        );
        assert_eq!(mapped.allowed_actions, ActionMask::COPY | ActionMask::MOVE);
        assert_eq!(mapped.preferred, DropAction::Move);
        assert_eq!(
            to_wl_mask(mapped.allowed_actions),
            wl::ActionMask::COPY | wl::ActionMask::MOVE
        );
    }

    #[test]
    fn delivered_terminal_maps_to_delivery_cancellation_but_completion_does_not() {
        let mut session = DragSession::default();
        let window = Entity::from_bits(1);
        let target = Entity::from_bits(2);
        let pointer_entity = Entity::from_bits(3);
        let pointer_id = PointerId::Custom(Uuid::from_u128(4));
        let mut active = wayland_transfer_fixture(
            &mut session,
            window,
            target,
            pointer_entity,
            pointer_id,
            DndOrigin::External(TransferId(11)),
        );
        active.delivered = true;
        let terminal = wl::TerminalEvent {
            transfer_id: active.bridge_id,
            disposition: wl::TerminalDisposition::Rejected,
            reason: wl::TerminalReason::AskConfirmationDeadlineExpired,
        };

        assert_eq!(
            terminal_delivery_cancellation(&active, window, &terminal),
            Some(DndDeliveryCancelled {
                delivery_id: active.delivery_id,
            })
        );
        // An answered delivery is never cancelled, whichever terminal reason
        // that answer produced. `Completed`, `AppDismissed` and
        // `AppOperationFailed` are all downstream of an application
        // resolution, so keying off the reason alone would emit a cancellation
        // for a delivery the application had already completed and the public
        // contract would carry both for one delivery.
        active.app_resolved = true;
        for reason in [
            wl::TerminalReason::Completed,
            wl::TerminalReason::AppDismissed,
            wl::TerminalReason::AppOperationFailed,
            wl::TerminalReason::WindowTeardown,
        ] {
            assert_eq!(
                terminal_delivery_cancellation(
                    &active,
                    window,
                    &wl::TerminalEvent { reason, ..terminal },
                ),
                None,
                "an answered delivery must not be cancelled after {reason:?}"
            );
        }
    }

    #[test]
    fn application_invalidation_restarts_exhausted_wayland_candidate_without_bridge_event() {
        use bevy::picking::backend::HitData;

        let mut app = App::new();
        let window = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn(DropTarget).id();
        let pointer_entity = app.world_mut().spawn_empty().id();
        let pointer_id = PointerId::Custom(Uuid::from_u128(9));
        let mut session = DragSession::default();
        let active = wayland_transfer_fixture(
            &mut session,
            window,
            target,
            pointer_entity,
            pointer_id,
            DndOrigin::External(TransferId(11)),
        );
        let mut hover_map = HoverMap::default();
        hover_map
            .entry(pointer_id)
            .or_default()
            .insert(target, HitData::new(Entity::PLACEHOLDER, 0.0, None, None));
        app.insert_resource(session)
            .insert_resource(hover_map)
            .init_resource::<ButtonInput<KeyCode>>()
            .add_message::<AcceptanceProposal>()
            .add_message::<DndHighlightChanged>()
            .insert_non_send(OsDndRuntime {
                active: Some(active),
                ..default()
            })
            .add_systems(
                Update,
                (
                    refresh_wayland_from_application_invalidation,
                    update_wayland_context,
                    propose_wayland,
                )
                    .chain(),
            );

        app.update();
        assert!(app
            .world()
            .non_send::<OsDndRuntime>()
            .active
            .as_ref()
            .is_some_and(|active| active.candidate.is_none()));

        app.world_mut()
            .resource_mut::<DragSession>()
            .invalidate_acceptance();
        app.update();

        let runtime = app.world().non_send::<OsDndRuntime>();
        assert!(runtime
            .active
            .as_ref()
            .is_some_and(|active| active.candidate == Some(target)));
        let proposals = app.world().resource::<Messages<AcceptanceProposal>>();
        let mut cursor = proposals.get_cursor();
        assert_eq!(
            cursor
                .read(proposals)
                .map(|proposal| proposal.target)
                .collect::<Vec<_>>(),
            vec![target]
        );
    }

    #[test]
    fn batched_motion_action_drop_uses_new_target_action_and_revision() {
        let old_target = Entity::from_bits(1);
        let new_target = Entity::from_bits(2);
        let accepted = AppliedAcceptance {
            target: new_target,
            action: DropAction::Move,
            modifiers: Modifiers {
                shift: true,
                ..default()
            },
            proposal_revision: ProposalRevision::from_raw(12),
            transport_revision: wl::TransportRevision(44),
        };
        let drop = wl::DropEvent {
            transfer_id: wl::DataTransferId(3),
            target: wl::TargetId(new_target.to_bits()),
            payload: wl::DragPayload::Paths(vec![PathBuf::from("/new/item")]),
            action: wl::DndAction::Move,
            modifiers: wl::Modifiers {
                shift: true,
                ..default()
            },
            origin: wl::DndOrigin::External(wl::DataTransferId(3)),
            delivery_id: wl::DeliveryId(5),
            accepted_revision: wl::ProposalRevision(12),
        };

        assert_ne!(accepted.target, old_target);
        assert_eq!(accepted.target.to_bits(), drop.target.0);
        assert_eq!(to_wl_action(accepted.action), drop.action);
        assert_eq!(accepted.proposal_revision.get(), drop.accepted_revision.0);
        assert_eq!(accepted.transport_revision, wl::TransportRevision(44));
    }
}
