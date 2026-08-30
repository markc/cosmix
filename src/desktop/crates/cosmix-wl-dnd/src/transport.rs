use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use smithay_client_toolkit::data_device_manager::data_device::{
    DataDevice, DataDeviceData, DataDeviceHandler,
};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{DataSourceHandler, DragSource};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_data_device, delegate_pointer, delegate_registry, delegate_seat, delegate_shm,
    registry_handlers,
};
use wayland_backend::client::{ObjectId, WaylandError};
use wayland_backend::sys::client::Backend;
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction as WlDndAction;
use wayland_client::protocol::wl_data_offer::WlDataOffer;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use crate::icon::{OutgoingIcon, shm_slot_len, write_little_endian_argb8888};
use crate::mime::{MimeType, decode_payload};
use crate::queue::{BoundedEventQueue, EventClass, QueueConfig, QueueConfigError, QueueEvent};
use crate::receive::{ReceiveEffect, ReceiveError, ReceivePhase, ReceiveTransfer};
use crate::send::{
    MAX_PENDING_TERMINALS, NonceLookupError, NonceRegistry, OutgoingEvent, OutgoingPayload,
    OutgoingTerminalReason, OutgoingTransfer, SendConfig, SendConfigError, SendError,
    TransferNonce, URI_LIST_MIME, UTF8_TEXT_MIME,
};
use crate::types::{
    Acceptance, AcceptanceError, ActionMask, BridgeEvent, DataTransferId, DndAction, DndOrigin,
    DropComplete, DropDecision, MimeDescriptor, PayloadFailure, Position, ProposalRevision,
    SourceId, TargetId, TerminalDisposition, TerminalEvent, TerminalReason, TransportRevision,
};

const DEFAULT_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// How long an emitted `Ask` drop may sit unanswered.
///
/// This is a human clicking a confirm dialog, so it is deliberately short: the
/// whole wait holds a compositor offer and a payload FD open.
const DEFAULT_ASK_CONFIRMATION_DEADLINE: Duration = Duration::from_secs(30);

/// How long the resolved `Ask` may wait for both the compositor action
/// acknowledgement and the application's `DropComplete`.
///
/// Deliberately *not* shortened to match the confirmation wait: this window
/// contains the application's actual file operation, whose duration is
/// data-dependent — a multi-gigabyte move legitimately takes minutes, and
/// expiring it would abort a transfer that is making progress.
const DEFAULT_POST_DECISION_DEADLINE: Duration = Duration::from_secs(300);

/// How long a payload pipe may stay open without producing a byte.
const DEFAULT_PAYLOAD_INACTIVITY: Duration = Duration::from_secs(30);

/// Wall-clock interval a physical drop may wait for current acceptance.
const DEFAULT_DROP_FENCE_TIMEOUT: Duration = Duration::from_millis(500);

const MIN_ICON_COMPOSITOR_VERSION: u32 = 4;
const MAX_ICON_COMPOSITOR_VERSION: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgeConfig {
    pub queue: QueueConfig,
    pub max_payload_bytes: usize,
    pub ask_confirmation_deadline: Duration,
    pub post_decision_deadline: Duration,
    pub payload_inactivity: Duration,
    pub drop_fence_timeout: Duration,
    pub send: SendConfig,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            queue: QueueConfig::default(),
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            ask_confirmation_deadline: DEFAULT_ASK_CONFIRMATION_DEADLINE,
            post_decision_deadline: DEFAULT_POST_DECISION_DEADLINE,
            payload_inactivity: DEFAULT_PAYLOAD_INACTIVITY,
            drop_fence_timeout: DEFAULT_DROP_FENCE_TIMEOUT,
            send: SendConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeConfigError {
    Queue(QueueConfigError),
    ZeroMaxPayloadBytes,
    ZeroAskConfirmationDeadline,
    ZeroPostDecisionDeadline,
    ZeroPayloadInactivity,
    ZeroDropFenceTimeout,
    UnrepresentableAskConfirmationDeadline,
    UnrepresentablePostDecisionDeadline,
    UnrepresentablePayloadInactivity,
    UnrepresentableDropFenceTimeout,
    Send(SendConfigError),
}

impl BridgeConfig {
    pub fn validate(self, now: Instant) -> Result<Self, BridgeConfigError> {
        self.queue.validate().map_err(BridgeConfigError::Queue)?;
        if self.max_payload_bytes == 0 {
            return Err(BridgeConfigError::ZeroMaxPayloadBytes);
        }
        validate_duration(
            now,
            self.ask_confirmation_deadline,
            BridgeConfigError::ZeroAskConfirmationDeadline,
            BridgeConfigError::UnrepresentableAskConfirmationDeadline,
        )?;
        validate_duration(
            now,
            self.post_decision_deadline,
            BridgeConfigError::ZeroPostDecisionDeadline,
            BridgeConfigError::UnrepresentablePostDecisionDeadline,
        )?;
        validate_duration(
            now,
            self.payload_inactivity,
            BridgeConfigError::ZeroPayloadInactivity,
            BridgeConfigError::UnrepresentablePayloadInactivity,
        )?;
        validate_duration(
            now,
            self.drop_fence_timeout,
            BridgeConfigError::ZeroDropFenceTimeout,
            BridgeConfigError::UnrepresentableDropFenceTimeout,
        )?;
        self.send.validate(now).map_err(BridgeConfigError::Send)?;
        Ok(self)
    }
}

fn validate_duration(
    now: Instant,
    duration: Duration,
    zero: BridgeConfigError,
    unrepresentable: BridgeConfigError,
) -> Result<(), BridgeConfigError> {
    if duration.is_zero() {
        return Err(zero);
    }
    if now.checked_add(duration).is_none() {
        return Err(unrepresentable);
    }
    Ok(())
}

fn require_data_device_protocol_v3(version: u32) -> Result<(), InitError> {
    const REQUIRED: u32 = 3;
    if version < REQUIRED {
        return Err(InitError::DataDeviceProtocolTooOld {
            available: version,
            required: REQUIRED,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct DataDeviceProtocolV3;

impl TryFrom<u32> for DataDeviceProtocolV3 {
    type Error = InitError;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        require_data_device_protocol_v3(version)?;
        Ok(Self)
    }
}

fn create_data_devices<T>(_: DataDeviceProtocolV3, create: impl FnOnce() -> T) -> T {
    create()
}

#[derive(Clone, Copy)]
struct VacantSeat;

fn admit_unique_seat(already_present: bool) -> Option<VacantSeat> {
    (!already_present).then_some(VacantSeat)
}

fn create_seat_object<T>(_: VacantSeat, create: impl FnOnce() -> T) -> T {
    create()
}

/// Whether a held grab can be attributed to a caller's gesture.
///
/// Split out from [`WaylandBridge::grab_is_unambiguous`] because the state it
/// reads — live `wl_seat`/`wl_pointer` objects — cannot be constructed without
/// a compositor, so this is the only part of the rule a unit test can reach.
/// `== 1` is load-bearing in both directions: zero pointer-capable seats means
/// the grab cannot be honoured at all, and two or more means a toolkit's single
/// logical mouse cannot say which seat pressed.
const fn grab_is_attributable(has_grab: bool, pointer_capable_seats: usize) -> bool {
    has_grab && pointer_capable_seats == 1
}

/// The reason a terminated outgoing drag reports.
///
/// Split out of [`WaylandBridge::terminate_outgoing`] for the same reason as
/// [`grab_is_attributable`]: the call itself needs a live `wl_data_source`,
/// so this is the only part of the rule a unit test can reach.
///
/// `reached` is whatever caused this termination; `pointer_lost` is set iff
/// the drag's pointer has already stopped existing (see
/// [`TransportState::active_source_pointer_lost`]). When both are present the
/// loss wins — a `SourceCancelled` beating the lifecycle event to the drain,
/// or a deadline firing after the event was dropped, is a *consequence* of the
/// pointer going, and naming the consequence tells a consumer the opposite of
/// what happened. `Completed` is the exception: the drop was delivered, and a
/// pointer that vanished afterwards does not unmake that.
const fn outgoing_terminal_reason(
    reached: OutgoingTerminalReason,
    pointer_lost: Option<OutgoingTerminalReason>,
) -> OutgoingTerminalReason {
    match pointer_lost {
        Some(lost) if !matches!(reached, OutgoingTerminalReason::Completed) => lost,
        _ => reached,
    }
}

/// Whether both globals needed for an export icon are available.
///
/// Incoming DnD uses neither global, so icon support is deliberately
/// all-or-nothing and additive rather than another bridge-construction
/// requirement.
const fn export_icons_available(has_compositor: bool, has_shm: bool) -> bool {
    has_compositor && has_shm
}

fn globals_advertise_export_icons(globals: &GlobalList) -> bool {
    globals.contents().with_list(|list| {
        export_icons_available(
            list.iter().any(|global| {
                global.interface == WlCompositor::interface().name
                    && global.version >= MIN_ICON_COMPOSITOR_VERSION
            }),
            list.iter().any(|global| {
                global.interface == wl_shm::WlShm::interface().name && global.version >= 1
            }),
        )
    })
}

/// The offset request cannot be sent before wl_surface v5.
///
/// Split out because proxy versions need a live compositor, while the
/// compatibility rule does not. A v4 compositor still gets a visible icon;
/// only its top-left remains under the pointer.
const fn supported_icon_offset(surface_version: u32, offset: (i32, i32)) -> Option<(i32, i32)> {
    if surface_version >= 5 {
        Some(offset)
    } else {
        None
    }
}

fn seat_can_claim_grab(
    held: Option<&CallbackIdentity>,
    active: Option<&CallbackIdentity>,
    candidate: &CallbackIdentity,
) -> bool {
    held.is_none_or(|seat| seat == candidate) && active.is_none_or(|seat| seat == candidate)
}

fn cancellation_latch_capacity(queue: QueueConfig) -> usize {
    // Every queued Enter occupies one lifecycle slot, so this bound can retain
    // one cancellation for every Enter that can be queued simultaneously.
    queue.lifecycle_capacity
}

fn payload_worker_capacity(queue: QueueConfig) -> usize {
    // An admitted Enter is the only path to a payload worker. Reuse the
    // lifecycle admission budget so callback churn cannot retain more live
    // worker resources than one full batch of admitted transfer lifecycles.
    queue.lifecycle_capacity
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    NotWayland,
    InvalidConfig(BridgeConfigError),
    Backend(String),
    Surface(String),
    MissingDataDeviceManager(String),
    DataDeviceProtocolTooOld { available: u32, required: u32 },
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeError {
    Dispatch(String),
    Flush(String),
    Icon(String),
    NoActiveTransfer,
    /// A hover leave arrived before `drop`, so SCTK destroyed the offer.
    OfferLeft,
    /// The proxy itself was dead at the point a request was attempted.
    OfferProxyDead,
    StaleTransfer {
        active: DataTransferId,
        received: DataTransferId,
    },
    UnsupportedMime(String),
    /// The caller constructed an [`Acceptance`] the protocol forbids, or one
    /// claiming a transport revision the bridge has never delivered.
    InvalidAcceptance(AcceptanceError),
    Receive(ReceiveError),
    Send(SendError),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BridgeError {}

impl From<ReceiveError> for BridgeError {
    fn from(error: ReceiveError) -> Self {
        Self::Receive(error)
    }
}

impl From<SendError> for BridgeError {
    fn from(error: SendError) -> Self {
        Self::Send(error)
    }
}

struct WorkerResult {
    transfer_id: DataTransferId,
    payload: Result<crate::types::DragPayload, PayloadFailure>,
}

struct SendWorkerResult {
    transfer_id: DataTransferId,
    writer_id: u64,
    mime_type: String,
    result: Result<(), ()>,
}

struct SendCancel {
    flag: Arc<AtomicBool>,
    waker: UnixStream,
}

impl SendCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        wake(&self.waker);
    }
}

/// Delivers the one-shot cancellation byte, retrying an interrupted write.
///
/// A plain `write` that returns `EINTR` writes nothing, and the flag alone is
/// invisible to a worker parked in `poll(2)` — it would hold the payload fd
/// until its own deadline expired, leaving the peer blocked on an EOF that
/// never comes. `write_all` retries `Interrupted` for us.
fn wake(waker: &UnixStream) {
    let _ = (&mut &*waker).write_all(&[1]);
}

struct SendCancelWorker {
    flag: Arc<AtomicBool>,
    wake: UnixStream,
}

struct ActiveSource {
    source: DragSource,
    icon: Option<DragIconSurface>,
    transfer: OutgoingTransfer,
    seat: CallbackIdentity,
    writers: Vec<(u64, SendCancel)>,
    next_writer_id: u64,
}

trait DestroyIconSurface {
    fn destroy_icon_surface(&self);
}

impl DestroyIconSurface for WlSurface {
    fn destroy_icon_surface(&self) {
        self.destroy();
    }
}

struct DragIconSurface<S = WlSurface, B = Buffer, P = SlotPool>
where
    S: DestroyIconSurface,
{
    surface: S,
    buffer: B,
    _pool: P,
}

impl<S, B, P> Drop for DragIconSurface<S, B, P>
where
    S: DestroyIconSurface,
{
    fn drop(&mut self) {
        // Declaration order drops the buffer before the pool: SCTK marks an
        // attached buffer for destruction on wl_buffer.release, even when the
        // client-side pool is no longer retained.
        self.surface.destroy_icon_surface();
    }
}

fn destroy_drag_icon<T>(icon: &mut Option<T>) {
    drop(icon.take());
}

/// Bridge-side half of a payload worker's cancellation channel.
///
/// The flag carries *why* (a lock-free check the worker makes before every
/// read); the socket carries *now* — the worker blocks in `poll(2)` on the
/// payload FD and this socket at once, so an idle transfer costs zero wakeups
/// and a cancellation is observed within the same syscall instead of at the
/// next tick of a sleep loop.
struct PayloadCancel {
    flag: Arc<AtomicBool>,
    waker: UnixStream,
}

impl PayloadCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        // A byte is enough to make the socket readable; a full buffer (which
        // cannot happen for a single-byte protocol) would simply mean the
        // worker is already awake.
        wake(&self.waker);
    }
}

/// Worker-side half of the cancellation channel.
struct PayloadCancelWorker {
    flag: Arc<AtomicBool>,
    wake: UnixStream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RetainedOfferRequest {
    Accept(Option<String>),
    FinalActions {
        allowed: ActionMask,
        preferred: DndAction,
    },
}

/// Smallest proxy surface needed by the retained post-drop request path.
///
/// Keeping the request encoder generic lets tests model wayland-client's dead
/// proxy behaviour without substituting above this production helper.
trait RetainedOfferProxy {
    fn version(&self) -> u32;
    fn is_alive(&self) -> bool;
    fn accept(&self, serial: u32, mime: Option<String>);
    fn set_actions(&self, allowed: WlDndAction, preferred: WlDndAction);
}

impl RetainedOfferProxy for WlDataOffer {
    fn version(&self) -> u32 {
        Proxy::version(self)
    }

    fn is_alive(&self) -> bool {
        Proxy::is_alive(self)
    }

    fn accept(&self, serial: u32, mime: Option<String>) {
        self.accept(serial, mime);
    }

    fn set_actions(&self, allowed: WlDndAction, preferred: WlDndAction) {
        self.set_actions(allowed, preferred);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CallbackIdentity {
    Wayland(ObjectId),
    #[cfg(test)]
    Test(u64),
}

impl CallbackIdentity {
    fn wayland(proxy: &impl Proxy) -> Self {
        Self::Wayland(proxy.id())
    }

    #[cfg(test)]
    fn test(id: u64) -> Self {
        Self::Test(id)
    }
}

trait OfferBackend {
    fn offered_mimes(&self) -> Vec<(String, MimeType)>;
    fn source_actions(&self) -> ActionMask;
    fn device_identity(&self) -> CallbackIdentity;
    fn is_alive(&self) -> bool;
    fn accept_mime(&self, mime: Option<String>) -> bool;
    fn set_actions(&self, allowed: ActionMask, preferred: Option<DndAction>) -> bool;
    fn receive(
        &self,
        mime: String,
    ) -> std::io::Result<smithay_client_toolkit::data_device_manager::ReadPipe>;
    fn send_retained_request(&self, request: RetainedOfferRequest) -> bool;
    fn finish(&self) -> bool;
    fn destroy(&self);
    #[cfg(test)]
    fn test_id(&self) -> Option<u64> {
        None
    }
}

struct WaylandOfferBackend {
    offer: DragOffer,
    device: WlDataDevice,
}

impl OfferBackend for WaylandOfferBackend {
    fn offered_mimes(&self) -> Vec<(String, MimeType)> {
        self.offer.with_mime_types(|mimes| {
            mimes
                .iter()
                .filter_map(|raw| raw.parse::<MimeType>().ok().map(|mime| (raw.clone(), mime)))
                .collect()
        })
    }

    fn source_actions(&self) -> ActionMask {
        from_wayland_mask(self.offer.source_actions)
    }

    fn device_identity(&self) -> CallbackIdentity {
        CallbackIdentity::wayland(&self.device)
    }

    fn is_alive(&self) -> bool {
        Proxy::is_alive(self.offer.inner())
    }

    fn accept_mime(&self, mime: Option<String>) -> bool {
        if !Proxy::is_alive(self.offer.inner()) {
            return false;
        }
        self.offer.accept_mime_type(self.offer.serial, mime);
        true
    }

    fn set_actions(&self, allowed: ActionMask, preferred: Option<DndAction>) -> bool {
        if !Proxy::is_alive(self.offer.inner()) {
            return false;
        }
        self.offer.set_actions(
            to_wayland_mask(allowed),
            preferred.map_or_else(WlDndAction::empty, to_wayland_action),
        );
        true
    }

    fn receive(
        &self,
        mime: String,
    ) -> std::io::Result<smithay_client_toolkit::data_device_manager::ReadPipe> {
        self.offer.receive(mime)
    }

    fn send_retained_request(&self, request: RetainedOfferRequest) -> bool {
        send_retained_offer_request(&self.offer, request)
    }

    fn finish(&self) -> bool {
        if !Proxy::is_alive(self.offer.inner()) {
            return false;
        }
        self.offer.finish();
        true
    }

    fn destroy(&self) {
        self.offer.destroy();
    }
}

struct ActiveOffer {
    backend: Option<Box<dyn OfferBackend>>,
    /// A hover leave before `drop` destroys the SCTK offer and gates every
    /// request except the idempotent close path.
    offer_dead: bool,
    /// SCTK records every leave on `DragOffer::left`, even though it retains a
    /// dropped offer. This separate bit selects raw, protocol-valid requests
    /// that SCTK's wrappers would otherwise suppress.
    post_drop_left: bool,
    dropped: bool,
    offered_mimes: Vec<(String, MimeType)>,
    source_actions: ActionMask,
    origin: DndOrigin,
    transfer: ReceiveTransfer,
    fetch_cancel: Option<PayloadCancel>,
    fetched_mime: Option<MimeType>,
}

struct PendingCompletion {
    terminal: TerminalEvent,
    action: DndAction,
    deadline: Instant,
    expiry_reason: TerminalReason,
    finish_sent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlushStatus {
    Flushed,
    WouldBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionFlushProgress {
    Waiting,
    QueueFinish,
    Complete,
}

impl PendingCompletion {
    fn after_flush(&self, status: FlushStatus) -> CompletionFlushProgress {
        match (status, self.finish_sent) {
            (FlushStatus::WouldBlock, _) => CompletionFlushProgress::Waiting,
            (FlushStatus::Flushed, false) => CompletionFlushProgress::QueueFinish,
            (FlushStatus::Flushed, true) => CompletionFlushProgress::Complete,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferRequestKind {
    /// Hover-time MIME acceptance through SCTK. A refreshed acceptance after
    /// drop uses the retained-offer request below.
    Accept,
    /// Hover-time action negotiation. After `drop`, only the specialised final
    /// Ask request below is valid (wayland.xml `wl_data_offer.set_actions`).
    SetActions,
    /// Explicitly valid before and after `wl_data_device.drop`
    /// (wayland.xml `wl_data_offer.receive`).
    Receive,
    /// Required after a successful dropped transfer, after all receives.
    Finish,
    /// Valid to cancel at any point and the only request valid after `finish`.
    Destroy,
    /// A request on a retained dropped offer after pointer leave. SCTK 0.19.2
    /// suppresses both `accept` and `set_actions` when its `left` bit is set,
    /// even though the dropped offer remains live.
    RetainedPostDrop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OfferRequestGate {
    offer_dead: bool,
    dropped: bool,
}

impl OfferRequestGate {
    fn send<T>(
        self,
        kind: OfferRequestKind,
        request: impl FnOnce() -> T,
    ) -> Result<T, BridgeError> {
        if self.offer_dead && kind != OfferRequestKind::Destroy {
            return Err(BridgeError::OfferLeft);
        }
        if matches!(
            kind,
            OfferRequestKind::Finish | OfferRequestKind::RetainedPostDrop
        ) && !self.dropped
        {
            return Err(BridgeError::Receive(ReceiveError::InvalidTransition));
        }
        if self.dropped
            && matches!(
                kind,
                OfferRequestKind::Accept | OfferRequestKind::SetActions
            )
        {
            return Err(BridgeError::Receive(ReceiveError::InvalidTransition));
        }
        Ok(request())
    }
}

impl ActiveOffer {
    fn backend(&self, kind: OfferRequestKind) -> Result<&dyn OfferBackend, BridgeError> {
        let backend = self
            .backend
            .as_deref()
            .ok_or(BridgeError::NoActiveTransfer)?;
        OfferRequestGate {
            offer_dead: self.offer_dead,
            dropped: self.dropped,
        }
        .send(kind, || backend)?;
        if !matches!(
            kind,
            OfferRequestKind::Destroy | OfferRequestKind::RetainedPostDrop
        ) && !backend.is_alive()
        {
            return Err(BridgeError::OfferProxyDead);
        }
        Ok(backend)
    }

    fn accept_mime(&self, mime: Option<String>) -> Result<(), BridgeError> {
        self.backend(OfferRequestKind::Accept)?
            .accept_mime(mime)
            .then_some(())
            .ok_or(BridgeError::OfferProxyDead)
    }

    fn set_actions(
        &self,
        allowed: ActionMask,
        preferred: Option<DndAction>,
    ) -> Result<(), BridgeError> {
        self.backend(OfferRequestKind::SetActions)?
            .set_actions(allowed, preferred)
            .then_some(())
            .ok_or(BridgeError::OfferProxyDead)
    }

    fn receive(
        &self,
        mime: String,
    ) -> Result<std::io::Result<smithay_client_toolkit::data_device_manager::ReadPipe>, BridgeError>
    {
        Ok(self.backend(OfferRequestKind::Receive)?.receive(mime))
    }

    fn send_final_actions(
        &self,
        allowed: ActionMask,
        preferred: DndAction,
    ) -> Result<(), BridgeError> {
        if self.post_drop_left {
            self.backend(OfferRequestKind::RetainedPostDrop)?
                .send_retained_request(RetainedOfferRequest::FinalActions { allowed, preferred })
                .then_some(())
                .ok_or(BridgeError::OfferProxyDead)
        } else {
            self.set_actions(allowed, Some(preferred))
        }
    }

    fn send_post_drop_accept(&self, mime: Option<String>) -> Result<(), BridgeError> {
        self.backend(OfferRequestKind::RetainedPostDrop)?
            .send_retained_request(RetainedOfferRequest::Accept(mime))
            .then_some(())
            .ok_or(BridgeError::OfferProxyDead)
    }

    fn finish(&self) -> Result<(), BridgeError> {
        self.backend(OfferRequestKind::Finish)?
            .finish()
            .then_some(())
            .ok_or(BridgeError::OfferProxyDead)
    }

    fn destroy(&self) -> Result<(), BridgeError> {
        self.backend(OfferRequestKind::Destroy)?.destroy();
        Ok(())
    }
}

/// Wayland receive bridge scoped to the lifetime of one winit-owned window.
///
/// The bridge is deliberately `!Send`: the foreign display and reconstructed
/// surface are borrowed protocol identities owned by winit's event-loop
/// thread.
pub struct WaylandBridge {
    connection: Option<Connection>,
    event_queue: Option<EventQueue<TransportState>>,
    icon_globals: Option<GlobalList>,
    state: TransportState,
    active: Option<ActiveOffer>,
    outgoing: Option<ActiveSource>,
    nonce_registry: NonceRegistry,
    outgoing_events: VecDeque<OutgoingEvent>,
    /// Reserved terminal slots, one per outgoing transfer that has ended.
    ///
    /// This is a queue rather than a single slot because a transfer terminating
    /// clears `outgoing`, so the consumer can legitimately start the next drag
    /// before it drains. A single slot let the second terminal overwrite the
    /// first, and the consumer never learned the first transfer had ended.
    outgoing_terminals: VecDeque<OutgoingEvent>,
    /// The transfer `start_outgoing` is mid-way through starting, if any.
    ///
    /// Set once the source exists and cleared before the call returns. While
    /// set, terminating that id reserves no terminal — the caller learns of the
    /// failure from the `Err` instead, and has no id to match one against.
    unstarted_outgoing: Option<DataTransferId>,
    outgoing_event_capacity: usize,
    send_worker_tx: Sender<SendWorkerResult>,
    send_worker_rx: Receiver<SendWorkerResult>,
    app_queue: BoundedEventQueue<BridgeEvent>,
    worker_tx: Sender<WorkerResult>,
    worker_rx: Receiver<WorkerResult>,
    live_workers: BTreeSet<DataTransferId>,
    worker_capacity: usize,
    retired: BTreeSet<DataTransferId>,
    pending_completion: Option<PendingCompletion>,
    delivered_revision: TransportRevision,
    next_barrier_id: u64,
    connection_lost: Option<String>,
    config: BridgeConfig,
    #[cfg(test)]
    test_flushes: VecDeque<Result<FlushStatus, WaylandError>>,
    _not_send: PhantomData<Rc<()>>,
}

impl WaylandBridge {
    #[cfg(test)]
    fn for_frame_test(config: BridgeConfig) -> Self {
        let config = config.validate(Instant::now()).unwrap();
        let (worker_tx, worker_rx) = mpsc::channel();
        let (send_worker_tx, send_worker_rx) = mpsc::channel();
        Self {
            connection: None,
            event_queue: None,
            icon_globals: None,
            state: TransportState {
                registry_state: None,
                seat_state: None,
                data_device_manager: None,
                icon_compositor: None,
                icon_shm: None,
                surface: None,
                data_devices: Vec::new(),
                held_grab: None,
                active_source_seat: None,
                active_source_pointer_lost: None,
                protocol_queue: BoundedEventQueue::new(config.queue).unwrap(),
                device_transfers: KeyedCallbackCapture::default(),
                offer_transfers: KeyedCallbackCapture::default(),
                source_transfers: KeyedCallbackCapture::default(),
                pending_barriers: Vec::new(),
                callback_actions: Vec::new(),
                callback_source_actions: Vec::new(),
                replaced_before_enter: BTreeSet::new(),
                overflowed_transfers: BTreeSet::new(),
                cancelled_before_enter: VecDeque::new(),
                cancellation_capacity: cancellation_latch_capacity(config.queue),
                next_transfer_id: 1,
                transport_revision: 0,
            },
            active: None,
            outgoing: None,
            nonce_registry: NonceRegistry::default(),
            outgoing_events: VecDeque::new(),
            outgoing_terminals: VecDeque::new(),
            unstarted_outgoing: None,
            outgoing_event_capacity: config.queue.lifecycle_capacity,
            send_worker_tx,
            send_worker_rx,
            app_queue: BoundedEventQueue::new(config.queue).unwrap(),
            worker_tx,
            worker_rx,
            live_workers: BTreeSet::new(),
            worker_capacity: payload_worker_capacity(config.queue),
            retired: BTreeSet::new(),
            pending_completion: None,
            delivered_revision: TransportRevision(0),
            next_barrier_id: 1,
            connection_lost: None,
            config,
            test_flushes: VecDeque::new(),
            _not_send: PhantomData,
        }
    }

    /// Builds a guest event queue on winit's existing Wayland connection.
    ///
    /// # Safety
    ///
    /// Both handles must come from the same live window. The caller must drop
    /// or explicitly tear down this bridge before that window and its
    /// `wl_display`/`wl_surface` are destroyed. All methods must run on the
    /// window event-loop thread.
    pub unsafe fn from_raw_handles(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        config: BridgeConfig,
    ) -> Result<Self, InitError> {
        let config = config
            .validate(Instant::now())
            .map_err(InitError::InvalidConfig)?;
        let (display, surface) = match (display_handle, window_handle) {
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
                (display.display.as_ptr(), window.surface.as_ptr())
            }
            _ => return Err(InitError::NotWayland),
        };

        // SAFETY: upheld by the constructor's contract.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, event_queue) = registry_queue_init::<TransportState>(&connection)
            .map_err(|error| InitError::Backend(error.to_string()))?;
        let qh = event_queue.handle();

        // SAFETY: upheld by the constructor's same-connection contract.
        let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
            .map_err(|error| InitError::Surface(error.to_string()))?;
        let surface = WlSurface::from_id(&connection, surface_id)
            .map_err(|error| InitError::Surface(error.to_string()))?;

        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        let data_device_manager = DataDeviceManagerState::bind(&globals, &qh)
            .map_err(|error| InitError::MissingDataDeviceManager(error.to_string()))?;
        let supported_protocol =
            DataDeviceProtocolV3::try_from(data_device_manager.data_device_manager().version())?;
        let data_devices = create_data_devices(supported_protocol, || {
            seat_state
                .seats()
                .map(|seat| SeatObjects {
                    data_device: data_device_manager.get_data_device(&qh, &seat),
                    seat,
                    pointer: None,
                })
                .collect()
        });
        let (worker_tx, worker_rx) = mpsc::channel();
        let (send_worker_tx, send_worker_rx) = mpsc::channel();

        Ok(Self {
            connection: Some(connection),
            event_queue: Some(event_queue),
            icon_globals: Some(globals),
            state: TransportState {
                registry_state: Some(registry_state),
                seat_state: Some(seat_state),
                data_device_manager: Some(data_device_manager),
                icon_compositor: None,
                icon_shm: None,
                surface: Some(surface),
                data_devices,
                held_grab: None,
                active_source_seat: None,
                active_source_pointer_lost: None,
                protocol_queue: BoundedEventQueue::new(config.queue)
                    .expect("bridge config was validated"),
                device_transfers: KeyedCallbackCapture::default(),
                offer_transfers: KeyedCallbackCapture::default(),
                source_transfers: KeyedCallbackCapture::default(),
                pending_barriers: Vec::new(),
                callback_actions: Vec::new(),
                callback_source_actions: Vec::new(),
                replaced_before_enter: BTreeSet::new(),
                overflowed_transfers: BTreeSet::new(),
                cancelled_before_enter: VecDeque::new(),
                cancellation_capacity: cancellation_latch_capacity(config.queue),
                next_transfer_id: 1,
                transport_revision: 0,
            },
            active: None,
            outgoing: None,
            nonce_registry: NonceRegistry::default(),
            outgoing_events: VecDeque::new(),
            outgoing_terminals: VecDeque::new(),
            unstarted_outgoing: None,
            outgoing_event_capacity: config.queue.lifecycle_capacity,
            send_worker_tx,
            send_worker_rx,
            app_queue: BoundedEventQueue::new(config.queue).expect("bridge config was validated"),
            worker_tx,
            worker_rx,
            live_workers: BTreeSet::new(),
            worker_capacity: payload_worker_capacity(config.queue),
            retired: BTreeSet::new(),
            pending_completion: None,
            delivered_revision: TransportRevision(0),
            next_barrier_id: 1,
            connection_lost: None,
            config,
            #[cfg(test)]
            test_flushes: VecDeque::new(),
            _not_send: PhantomData,
        })
    }

    /// Whether a held grab exists and can be attributed to the caller's
    /// gesture without guessing.
    ///
    /// True requires exactly one pointer-capable seat: a caller above this
    /// crate sees one logical mouse, so with two seats it cannot tell whose
    /// press it is holding. Callers should consult this *before* moving their
    /// own state into an export — [`Self::start_outgoing`] enforces the same
    /// rule, but only after the caller has already committed.
    pub fn grab_is_unambiguous(&self) -> bool {
        grab_is_attributable(
            self.state.held_grab.is_some(),
            self.state
                .data_devices
                .iter()
                .filter(|objects| objects.pointer.is_some())
                .count(),
        )
    }

    /// Whether this bridge advertises everything needed for an export icon.
    ///
    /// This is a bind-free preflight over the retained global list. The first
    /// icon request performs and caches the actual bindings. Incoming DnD and
    /// iconless outgoing drags remain available when false.
    pub fn export_icons_available(&self) -> bool {
        self.icon_globals
            .as_ref()
            .is_some_and(globals_advertise_export_icons)
    }

    /// Starts the one explicit `Dragging -> Exporting` handoff.
    ///
    /// The payload has already moved into [`OutgoingPayload`]. The bridge uses
    /// the held left-button press captured by its own `wl_pointer`, registers
    /// the nonce and source correlation, creates a single-use source (whose
    /// constructor sends the sole `set_actions` request), then calls
    /// `start_drag` with icon `None`.
    ///
    /// # Error transaction
    ///
    /// Every returned error except [`BridgeError::Flush`] occurs before
    /// `wl_data_source.start_drag` is issued. A flush error can occur after the
    /// request, but wayland-backend defines every non-`WouldBlock` flush error
    /// as connection-fatal; this bridge synchronously marks the connection lost
    /// and tears the unreturned outgoing transfer down before returning it.
    /// `WouldBlock` is success and the queued request is retried by the pump.
    ///
    /// This call only marshals and flushes requests. It never reads or
    /// dispatches Wayland callbacks; those run later in [`Self::pump`].
    pub fn start_outgoing(
        &mut self,
        source: SourceId,
        payload: OutgoingPayload,
        actions: ActionMask,
        now: Instant,
    ) -> Result<DataTransferId, BridgeError> {
        self.start_outgoing_inner(source, payload, actions, None, now)
    }

    /// Starts an outgoing handoff with a compositor-owned raster drag icon.
    ///
    /// If this connection did not advertise both `wl_compositor` v4 and
    /// `wl_shm`, the transfer deliberately falls back to the same iconless path
    /// as [`Self::start_outgoing`]. Consumers can preflight that degradation
    /// through [`Self::export_icons_available`]. Its error and callback
    /// transaction is identical to [`Self::start_outgoing`].
    pub fn start_outgoing_with_icon(
        &mut self,
        source: SourceId,
        payload: OutgoingPayload,
        actions: ActionMask,
        icon: OutgoingIcon,
        now: Instant,
    ) -> Result<DataTransferId, BridgeError> {
        self.start_outgoing_inner(source, payload, actions, Some(icon), now)
    }

    fn start_outgoing_inner(
        &mut self,
        source: SourceId,
        payload: OutgoingPayload,
        actions: ActionMask,
        icon: Option<OutgoingIcon>,
        now: Instant,
    ) -> Result<DataTransferId, BridgeError> {
        if self.outgoing.is_some() || self.active.is_some() {
            return Err(BridgeError::Send(SendError::OutgoingAlreadyActive));
        }
        // Refusing to start is the honest failure. The alternative — evicting
        // the oldest reserved terminal — loses the only record that a previous
        // transfer ended, which is the one event a consumer cannot reconstruct.
        if self.outgoing_terminals.len() >= MAX_PENDING_TERMINALS {
            return Err(BridgeError::Send(SendError::UndrainedTerminals));
        }
        let grab = self
            .state
            .held_grab
            .as_ref()
            .map(|grab| {
                (
                    grab.seat.clone(),
                    grab.serial,
                    grab.origin.clone(),
                    CallbackIdentity::wayland(&grab.seat),
                )
            })
            .ok_or(BridgeError::Send(SendError::NoHeldGrab))?;
        if !self
            .state
            .data_devices
            .iter()
            .any(|objects| objects.seat == grab.0)
        {
            return Err(BridgeError::Send(SendError::NoHeldGrab));
        }
        // The caller's gesture and this grab must be the same physical press,
        // and nothing in the stack above can prove it: toolkits collapse every
        // seat into one logical mouse. With a second pointer-capable seat
        // present the grab belongs to whichever seat pressed first, so
        // escalating could run the drag on that seat while consuming the
        // other's gesture. See [`SendError::AmbiguousSeat`];
        // [`Self::grab_is_unambiguous`] lets a caller decline *before* it
        // commits its own state. Ordered after the grab checks so a plain
        // missing grab keeps reporting itself as one.
        if !self.grab_is_unambiguous() {
            return Err(BridgeError::Send(SendError::AmbiguousSeat));
        }
        let nonce =
            TransferNonce::random().map_err(|_| BridgeError::Send(SendError::RandomNonce))?;
        let transfer_id = self.state.allocate_transfer_id();
        let transfer = OutgoingTransfer::new(
            transfer_id,
            payload,
            nonce.clone(),
            actions,
            now,
            self.config.send,
        )?;
        let qh = self
            .event_queue
            .as_ref()
            .expect("production bridge owns an event queue")
            .handle();
        let manager = self
            .state
            .data_device_manager
            .as_ref()
            .expect("production transport owns a data-device manager");
        // SCTK's constructor offers every MIME and issues set_actions exactly
        // once. Do not call DragSource::set_actions: 0.19.2's wrapper sends the
        // request twice.
        let drag_source = manager.create_drag_and_drop_source(
            &qh,
            [
                URI_LIST_MIME.to_owned(),
                UTF8_TEXT_MIME.to_owned(),
                nonce.mime_type(),
            ],
            to_wayland_mask(actions),
        );
        if !Proxy::is_alive(drag_source.inner()) {
            return Err(BridgeError::Send(SendError::InvalidTransition));
        }
        let icon_offset = icon.as_ref().map(OutgoingIcon::offset);
        let mut drag_icon = self.create_drag_icon(icon)?;

        // Both correlations are live before start_drag. Dispatch cannot race
        // this thread, but the compositor may place an echo Enter in the same
        // batch as the first source callback.
        self.state
            .source_transfers
            .correlate(CallbackIdentity::wayland(drag_source.inner()), transfer_id);
        self.state.active_source_seat = Some(grab.3.clone());
        self.state.active_source_pointer_lost = None;
        {
            let data_device = &self
                .state
                .data_devices
                .iter()
                .find(|objects| objects.seat == grab.0)
                .expect("validated above")
                .data_device;
            if let Err(error) = self.nonce_registry.register_before_start(
                nonce.clone(),
                transfer_id,
                source,
                || {
                    let icon_surface = drag_icon.as_ref().map(|icon| &icon.surface);
                    drag_source.start_drag(data_device, &grab.2, icon_surface, grab.1);
                    if let Some(icon) = drag_icon.as_ref() {
                        if let Some((x, y)) = supported_icon_offset(
                            icon.surface.version(),
                            icon_offset.expect("prepared icon retains its offset"),
                        ) {
                            icon.surface.offset(x, y);
                        }
                        // start_drag must assign the permanent drag-icon role
                        // while the new surface is still roleless; a surface
                        // with any different role is a protocol error and can
                        // never be repurposed.
                        icon.surface.commit();
                    }
                },
            ) {
                destroy_drag_icon(&mut drag_icon);
                self.state.source_transfers.retire(transfer_id);
                self.state.active_source_seat = None;
                self.state.active_source_pointer_lost = None;
                return Err(error.into());
            }
        }
        self.outgoing = Some(ActiveSource {
            source: drag_source,
            icon: drag_icon,
            transfer,
            seat: grab.3,
            writers: Vec::new(),
            next_writer_id: 1,
        });
        // Marked unstarted until this call returns the id. Anything that
        // terminates the transfer before then — the flush below, or
        // `lose_connection` reached through it — must not reserve a terminal
        // naming an id the caller has never seen.
        self.unstarted_outgoing = Some(transfer_id);
        self.enqueue_outgoing(OutgoingEvent::StartRequested {
            transfer_id,
            source,
            nonce_mime: nonce.mime_type(),
        });
        let flushed = self.flush_or_fail();
        self.unstarted_outgoing = None;
        if let Err(error) = flushed {
            // `flush_or_fail` routes through `lose_connection`, which has
            // already torn the transfer down; the `unstarted_outgoing` mark is
            // what kept that teardown from reserving a terminal.
            self.terminate_outgoing(transfer_id, OutgoingTerminalReason::WaylandConnectionLost);
            return Err(error);
        }
        Ok(transfer_id)
    }

    fn create_drag_icon(
        &mut self,
        icon: Option<OutgoingIcon>,
    ) -> Result<Option<DragIconSurface>, BridgeError> {
        let Some(icon) = icon else {
            return Ok(None);
        };
        if !self.export_icons_available() {
            return Ok(None);
        }

        let qh = self
            .event_queue
            .as_ref()
            .expect("production bridge owns an event queue")
            .handle();
        let globals = self
            .icon_globals
            .as_ref()
            .expect("advertised icon globals came from a production registry");
        if self.state.icon_compositor.is_none() {
            self.state.icon_compositor = globals
                .bind(
                    &qh,
                    MIN_ICON_COMPOSITOR_VERSION..=MAX_ICON_COMPOSITOR_VERSION,
                    (),
                )
                .ok();
        }
        if self.state.icon_shm.is_none() {
            self.state.icon_shm = Shm::bind(globals, &qh).ok();
        }
        let (Some(compositor), Some(shm)) = (
            self.state.icon_compositor.as_ref(),
            self.state.icon_shm.as_ref(),
        ) else {
            return Ok(None);
        };

        let byte_len = icon.pixels.len();
        let pool_len =
            shm_slot_len(byte_len).expect("validated icon byte length has a rounded SHM slot");
        let width = icon.width as i32;
        let height = icon.height as i32;
        let stride = width * 4;
        let mut pool =
            SlotPool::new(pool_len, shm).map_err(|error| BridgeError::Icon(error.to_string()))?;
        let slot = pool
            .new_slot(byte_len)
            .map_err(|error| BridgeError::Icon(error.to_string()))?;
        write_little_endian_argb8888(&icon.pixels, &mut pool.raw_data_mut(&slot)[..byte_len]);
        let buffer = pool
            .create_buffer_in(&slot, width, height, stride, wl_shm::Format::Argb8888)
            .map_err(|error| BridgeError::Icon(error.to_string()))?;
        drop(slot);

        // This is intentionally a new roleless surface per transfer. A surface
        // that has ever held the drag-icon role can never be recycled.
        let surface = compositor.create_surface(&qh, ());
        surface.set_buffer_scale(icon.buffer_scale);
        let drag_icon = DragIconSurface {
            surface,
            buffer,
            _pool: pool,
        };
        drag_icon
            .buffer
            .attach_to(&drag_icon.surface)
            .map_err(|error| BridgeError::Icon(error.to_string()))?;
        drag_icon.surface.damage_buffer(0, 0, width, height);

        Ok(Some(drag_icon))
    }

    /// Origin to use when Phase 5b builds acceptance and the canonical drop.
    pub fn origin_for(&self, transfer_id: DataTransferId) -> DndOrigin {
        self.nonce_registry
            .correlation_for_incoming(transfer_id)
            .map_or(DndOrigin::External(transfer_id), |echo| {
                DndOrigin::Internal(echo.source)
            })
    }

    pub fn outgoing_phase(
        &self,
        transfer_id: DataTransferId,
    ) -> Result<crate::send::OutgoingPhase, BridgeError> {
        match self.outgoing.as_ref() {
            Some(active) if active.transfer.id() == transfer_id => Ok(active.transfer.phase()),
            Some(active) => Err(BridgeError::Send(SendError::StaleTransfer {
                active: active.transfer.id(),
                received: transfer_id,
            })),
            None => Err(BridgeError::Send(SendError::NoActiveTransfer)),
        }
    }

    /// Explicit cancellation seam for Escape/window teardown in Phase 5b.
    pub fn cancel_outgoing(
        &mut self,
        transfer_id: DataTransferId,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.ensure_outgoing(transfer_id)?;
        if let Some(active) = self.outgoing.as_mut() {
            active
                .transfer
                .cancel(OutgoingTerminalReason::CompositorCancelled, now);
        }
        let reason = self
            .outgoing
            .as_ref()
            .and_then(|active| active.transfer.terminal())
            .unwrap_or(OutgoingTerminalReason::CompositorCancelled);
        self.terminate_outgoing(transfer_id, reason);
        Ok(())
    }

    /// Drains source-side events after [`WaylandBridge::pump`].
    ///
    /// The reserved terminal is returned first, matching the receive queue's
    /// fail-closed release guarantee.
    pub fn drain_outgoing_events(&mut self) -> Vec<OutgoingEvent> {
        let mut events = Vec::new();
        events.extend(self.outgoing_terminals.drain(..));
        events.extend(self.outgoing_events.drain(..));
        events
    }

    /// Non-blocking frame pump. `dispatch_pending` never reads the socket.
    ///
    /// A lost display connection is reported as a terminal event first and only
    /// then as an error: the pump that observes the loss returns the reserved
    /// terminal, and every later pump returns [`BridgeError::Dispatch`] once the
    /// queue has drained. That ordering is what stops a compositor restart from
    /// stranding a live transfer's worker, offer and highlight.
    pub fn pump(&mut self, now: Instant) -> Result<Vec<BridgeEvent>, BridgeError> {
        if let Some(reason) = self.connection_lost.clone() {
            let pending = self.drain_app_frame();
            if !pending.is_empty() {
                return Ok(pending);
            }
            return Err(BridgeError::Dispatch(reason));
        }

        if let Err(error) = self
            .event_queue
            .as_mut()
            .expect("production bridge owns an event queue")
            .dispatch_pending(&mut self.state)
        {
            self.lose_connection(error.to_string());
            return Ok(self.drain_app_frame());
        }
        self.run_frame(now, |bridge| bridge.flush_connection())
    }

    /// Runs the connection-independent per-frame core over queues already
    /// populated by Wayland callbacks and payload workers.
    ///
    /// The flush operation is injected so ordering and completion visibility
    /// can be exercised without a live compositor connection.
    fn run_frame(
        &mut self,
        now: Instant,
        mut flush: impl FnMut(&mut Self) -> Result<FlushStatus, BridgeError>,
    ) -> Result<Vec<BridgeEvent>, BridgeError> {
        if let Some((id, reason)) = self.outgoing.as_mut().and_then(|active| {
            active
                .transfer
                .check_deadline(now)
                .map(|reason| (active.transfer.id(), reason))
        }) {
            self.terminate_outgoing(id, reason);
        }
        for id in std::mem::take(&mut self.state.overflowed_transfers) {
            if self.outgoing_id() == Some(id) {
                self.terminate_outgoing(id, OutgoingTerminalReason::QueueOverflow);
            } else if self.protocol_fact_id() == Some(id) {
                self.fail_active(id, TerminalReason::QueueOverflow, now);
            }
        }

        for event in self.state.protocol_queue.drain_frame() {
            self.process_protocol_event(event, now);
        }
        self.poll_workers(now);
        self.poll_send_workers(now);

        if let Some((id, reason)) = self.outgoing.as_mut().and_then(|active| {
            active
                .transfer
                .check_deadline(now)
                .map(|reason| (active.transfer.id(), reason))
        }) {
            self.terminate_outgoing(id, reason);
        }

        let completion_effects = self
            .active
            .as_mut()
            .and_then(|active| active.transfer.settle_completion(now).ok());
        if let Some(effects) = completion_effects {
            self.apply_effects(effects);
        }

        // After every protocol event of this frame, and before the consumer is
        // handed them, give any physical drop a resolution opportunity.
        let fence_effects = self
            .active
            .as_mut()
            .and_then(|active| active.transfer.resolve_drop_fence(now).ok());
        if let Some(effects) = fence_effects {
            self.apply_effects(effects);
        }

        let deadline_effects = self
            .active
            .as_mut()
            .and_then(|active| active.transfer.check_deadline(now).ok());
        if let Some(effects) = deadline_effects {
            self.apply_effects(effects);
        }

        if self.pending_completion.is_some() {
            if self.drive_pending_completion_with(now, &mut flush).is_err() {
                return Ok(self.drain_app_frame());
            }
        } else if flush(self).is_err() {
            return Ok(self.drain_app_frame());
        }
        Ok(self.drain_app_frame())
    }

    /// Drives a dead display connection through the ordinary terminal path.
    ///
    /// The terminal is left in the app queue rather than returned, so a failure
    /// observed inside a public method is still delivered by the next pump.
    fn lose_connection(&mut self, reason: String) {
        if self.connection_lost.is_none() {
            self.connection_lost = Some(reason);
        }
        if !self.exit_pending_completion(TerminalReason::WaylandConnectionLost)
            && let Some(id) = self.active.as_ref().map(|active| active.transfer.id())
        {
            self.fail_active(id, TerminalReason::WaylandConnectionLost, Instant::now());
        }
        if let Some(id) = self.outgoing_id() {
            self.terminate_outgoing_at(
                id,
                OutgoingTerminalReason::WaylandConnectionLost,
                Instant::now(),
            );
        }
    }

    fn raw_flush_connection(&mut self) -> Result<FlushStatus, WaylandError> {
        #[cfg(test)]
        if self.connection.is_none() {
            return self
                .test_flushes
                .pop_front()
                .unwrap_or(Ok(FlushStatus::Flushed));
        }
        classify_flush(
            self.connection
                .as_ref()
                .expect("production bridge owns a connection")
                .flush(),
        )
    }

    fn flush_connection(&mut self) -> Result<FlushStatus, BridgeError> {
        match self.raw_flush_connection() {
            Ok(status) => Ok(status),
            Err(error) => {
                let reason = error.to_string();
                self.lose_connection(reason.clone());
                Err(BridgeError::Flush(reason))
            }
        }
    }

    /// Flushes, failing the active transfer only if the connection has died.
    ///
    /// `WouldBlock` leaves requests in wayland-client's outgoing buffer for the
    /// next pump and is therefore a successful, transient result here.
    fn flush_or_fail(&mut self) -> Result<(), BridgeError> {
        self.flush_connection().map(|_| ())
    }

    /// Applies current target acceptance without fetching payload data.
    ///
    /// The acceptance is validated before any protocol request goes out. A
    /// caller cannot construct a `set_actions` the protocol forbids, and cannot
    /// claim currency with respect to a transport revision this bridge has
    /// never delivered — that claim is the drop fence's only evidence, so it is
    /// checked rather than trusted.
    pub fn accept(&mut self, acceptance: Acceptance) -> Result<(), BridgeError> {
        let id = match acceptance.context.origin {
            crate::types::DndOrigin::External(id) => id,
            crate::types::DndOrigin::Internal(source) => self
                .active
                .as_ref()
                .filter(|active| active.origin == DndOrigin::Internal(source))
                .map(|active| active.transfer.id())
                .ok_or(BridgeError::Receive(ReceiveError::OriginMismatch))?,
        };
        self.ensure_active(id)?;
        if self.active.as_ref().is_some_and(|active| active.offer_dead) {
            return Err(BridgeError::OfferLeft);
        }
        acceptance
            .validate()
            .map_err(BridgeError::InvalidAcceptance)?;
        let latest_delivered = self.delivered_revision;
        if acceptance.observed_transport_revision > latest_delivered {
            return Err(BridgeError::InvalidAcceptance(
                AcceptanceError::UnobservedTransportRevision {
                    observed: acceptance.observed_transport_revision,
                    latest_delivered,
                },
            ));
        }

        let actual_mime = {
            let active = self.active.as_ref().expect("checked above");
            choose_offered_mime(&active.offered_mimes, &acceptance.mime_type)
                .ok_or_else(|| BridgeError::UnsupportedMime(acceptance.mime_type.clone()))?
        };
        if self
            .active
            .as_ref()
            .and_then(|active| active.fetched_mime.as_ref())
            .is_some_and(|fetched| fetched != &actual_mime.1)
        {
            return Err(BridgeError::UnsupportedMime(acceptance.mime_type));
        }

        let request_result = (|| {
            let active = self.active.as_mut().expect("checked above");
            let post_drop = active.dropped;
            active.transfer.accept_for_origin(
                acceptance.context,
                acceptance.observed_transport_revision,
                active.origin,
            )?;
            if post_drop {
                active.send_post_drop_accept(Some(actual_mime.0.clone()))
            } else {
                active.accept_mime(Some(actual_mime.0.clone()))?;
                active.set_actions(acceptance.allowed_actions, Some(acceptance.preferred))
            }
        })();
        if matches!(request_result, Err(BridgeError::OfferProxyDead)) {
            self.fail_active(id, TerminalReason::OfferProxyDead, Instant::now());
        }
        request_result?;

        self.flush_or_fail()
    }

    /// Starts destination-pulled payload fetch for an accepted transfer.
    ///
    /// This explicit ID-correlated request is the lazy-fetch seam matching the
    /// upstream winit capability shape. It may be called before or after the
    /// compositor's physical drop callback.
    pub fn request_data(
        &mut self,
        id: DataTransferId,
        mime_type: &str,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.request_data_with_spawner(id, mime_type, now, |builder, worker| {
            builder.spawn(worker).map(|_| ())
        })
    }

    fn request_data_with_spawner(
        &mut self,
        id: DataTransferId,
        mime_type: &str,
        now: Instant,
        spawn: impl FnOnce(thread::Builder, Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.transfer.phase() != ReceivePhase::Offered)
        {
            return Err(BridgeError::Receive(ReceiveError::InvalidTransition));
        }
        let actual_mime = {
            let active = self.active.as_ref().expect("checked above");
            choose_offered_mime(&active.offered_mimes, mime_type)
                .ok_or_else(|| BridgeError::UnsupportedMime(mime_type.into()))?
        };
        self.start_fetch(id, actual_mime, now, spawn)?;
        self.flush_or_fail()
    }

    pub fn reject(&mut self, id: DataTransferId) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        let request_result = self
            .active
            .as_ref()
            .filter(|active| !active.dropped)
            .map_or(Ok(()), |active| {
                active.accept_mime(None)?;
                active.set_actions(ActionMask::NONE, None)
            });
        let reason = if matches!(request_result, Err(BridgeError::OfferProxyDead)) {
            TerminalReason::OfferProxyDead
        } else {
            TerminalReason::OfferRejected
        };
        self.fail_active(id, reason, Instant::now());
        request_result?;
        Ok(())
    }

    /// Clears hover acceptance without terminating the offer.
    ///
    /// Use `reject` only for an actual fail-closed terminal rejection.
    pub fn clear_acceptance(&mut self, id: DataTransferId) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        let result = (|| {
            let active = self.active.as_mut().expect("checked above");
            let negotiate_on_wire = !active.dropped;
            active.transfer.clear_acceptance()?;
            if negotiate_on_wire {
                active.accept_mime(None)?;
                active.set_actions(ActionMask::NONE, None)?;
            }
            Ok(())
        })();
        if matches!(result, Err(BridgeError::OfferProxyDead)) {
            self.fail_active(id, TerminalReason::OfferProxyDead, Instant::now());
        }
        result?;
        self.flush_or_fail()
    }

    pub fn invalidate_revision(
        &mut self,
        id: DataTransferId,
        revision: ProposalRevision,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        let effects = self
            .active
            .as_mut()
            .expect("checked above")
            .transfer
            .invalidate_revision(revision, now)?;
        self.apply_effects(effects);
        Ok(())
    }

    pub fn target_lost(
        &mut self,
        id: DataTransferId,
        target: TargetId,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        let effects = self
            .active
            .as_mut()
            .expect("checked above")
            .transfer
            .target_lost(target, now)?;
        self.apply_effects(effects);
        Ok(())
    }

    /// Applies the application's resolution of an `Ask` drop.
    ///
    /// The resolved action must be one the *source* advertised. The protocol
    /// makes a final preferred action outside `source_actions` an error, so it
    /// is rejected here rather than sent and left to the compositor.
    pub fn decide_drop(
        &mut self,
        id: DataTransferId,
        decision: DropDecision,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        if let Some(action) = match decision.decision {
            crate::types::DropDecisionKind::Copy => Some(DndAction::Copy),
            crate::types::DropDecisionKind::Move => Some(DndAction::Move),
            crate::types::DropDecisionKind::Dismissed => None,
        } {
            let source_actions = self
                .active
                .as_ref()
                .map(|active| active.source_actions)
                .unwrap_or(ActionMask::NONE);
            validate_final_action_offered(source_actions, action)
                .map_err(BridgeError::InvalidAcceptance)?;
        }

        let effects = self
            .active
            .as_mut()
            .expect("checked above")
            .transfer
            .drop_decision(decision, now, self.config.post_decision_deadline)?;

        for effect in effects {
            match effect {
                ReceiveEffect::SetActions { allowed, preferred } => {
                    let post_leave_path = self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.post_drop_left);
                    let request_result = self
                        .active
                        .as_ref()
                        .ok_or(BridgeError::NoActiveTransfer)?
                        .send_final_actions(allowed, preferred);
                    if request_result.is_ok() {
                        let barrier_id = self.next_barrier_id;
                        self.next_barrier_id = self.next_barrier_id.wrapping_add(1).max(1);
                        let barrier = AskBarrier {
                            transfer_id: id,
                            barrier_id,
                            requested_action: preferred,
                        };
                        self.state.pending_barriers.push(barrier);
                        self.queue_sync_barrier(barrier);
                        let effects = self
                            .active
                            .as_mut()
                            .expect("still active")
                            .transfer
                            .final_actions_sent(barrier_id, post_leave_path, now)?;
                        self.apply_effects(effects);
                    } else {
                        // Nothing went out, so no acknowledgement can arrive.
                        // Fail now instead of waiting out a deadline for an
                        // event that is structurally impossible.
                        self.fail_active(id, TerminalReason::OfferProxyDead, now);
                        request_result?;
                    }
                }
                other => self.apply_effects(vec![other]),
            }
        }
        self.flush_or_fail()
    }

    fn queue_sync_barrier(&self, barrier: AskBarrier) {
        if let (Some(connection), Some(event_queue)) = (&self.connection, &self.event_queue) {
            connection.display().sync(&event_queue.handle(), barrier);
        } else {
            #[cfg(not(test))]
            unreachable!("production bridge owns a connection and queue");
        }
    }

    pub fn complete_drop(
        &mut self,
        id: DataTransferId,
        complete: DropComplete,
        now: Instant,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        let effects = self
            .active
            .as_mut()
            .expect("checked above")
            .transfer
            .drop_complete(complete, now)?;
        self.apply_effects(effects);
        if self.pending_completion.is_some() {
            self.drive_pending_completion(now)?;
        }
        Ok(())
    }

    /// Shared failure seam used by receive today and Phase 5 send callbacks.
    pub fn fail_transfer(
        &mut self,
        id: DataTransferId,
        reason: TerminalReason,
    ) -> Result<(), BridgeError> {
        self.ensure_active(id)?;
        self.fail_active(id, reason, Instant::now());
        Ok(())
    }

    /// Explicit window-lifetime teardown. Returns the reserved terminal first.
    pub fn teardown(&mut self) -> Vec<BridgeEvent> {
        if !self.exit_pending_completion(TerminalReason::WindowTeardown)
            && let Some(id) = self.active.as_ref().map(|active| active.transfer.id())
        {
            self.fail_active(id, TerminalReason::WindowTeardown, Instant::now());
        }
        if let Some(id) = self.outgoing_id() {
            self.terminate_outgoing_at(id, OutgoingTerminalReason::WindowTeardown, Instant::now());
        }
        self.state.release_seat_objects();
        self.state.device_transfers.entries.clear();
        self.state.offer_transfers.entries.clear();
        self.state.source_transfers.entries.clear();
        self.state.pending_barriers.clear();
        self.state.callback_actions.clear();
        self.state.callback_source_actions.clear();
        self.state.replaced_before_enter.clear();
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
        self.drain_app_frame()
    }

    fn start_fetch(
        &mut self,
        id: DataTransferId,
        mime: (String, MimeType),
        now: Instant,
        spawn: impl FnOnce(thread::Builder, Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
    ) -> Result<(), BridgeError> {
        if self.live_workers.len() >= self.worker_capacity {
            self.fail_active(id, TerminalReason::PayloadWorkerCapacityExceeded, now);
            return Ok(());
        }
        if self
            .active
            .as_ref()
            .and_then(|active| active.fetched_mime.as_ref())
            .is_some_and(|fetched| fetched != &mime.1)
        {
            return Err(BridgeError::UnsupportedMime(mime.0));
        }
        let begin_result = (|| {
            let active = self.active.as_mut().expect("checked by accept");
            active.backend(OfferRequestKind::Receive)?;
            active
                .transfer
                .begin_fetch(now)
                .map_err(BridgeError::Receive)
        })();
        if matches!(begin_result, Err(BridgeError::OfferProxyDead)) {
            self.fail_active(id, TerminalReason::OfferProxyDead, now);
        }
        let effects = begin_result?;
        if !effects.is_empty() {
            self.apply_effects(effects);
            return Ok(());
        }
        let pipe = self
            .active
            .as_ref()
            .expect("deadline guard left the transfer active")
            .receive(mime.0.clone())?;
        let mut pipe = match pipe {
            Ok(pipe) => pipe,
            Err(error) => {
                let reason = if error.kind() == std::io::ErrorKind::NotConnected {
                    TerminalReason::OfferProxyDead
                } else {
                    TerminalReason::PipeFailure
                };
                self.fail_active(id, reason, now);
                return Ok(());
            }
        };
        let sender = self.worker_tx.clone();
        let max_bytes = self.config.max_payload_bytes;
        let inactivity = self.config.payload_inactivity;
        let (waker, wake) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(_) => {
                self.fail_active(id, TerminalReason::PipeFailure, now);
                return Ok(());
            }
        };
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = PayloadCancel {
            flag: Arc::clone(&flag),
            waker,
        };
        let worker_cancel = PayloadCancelWorker { flag, wake };
        let fetched_mime = mime.1.clone();
        let worker = Box::new(move || {
            let result = read_payload_cancellable(&mut pipe, max_bytes, &worker_cancel, inactivity)
                .map_err(|error| match error {
                    // A cancelled transfer is already terminal, so this value is
                    // only ever discarded by the retired-transfer filter.
                    PayloadReadError::Cancelled => PayloadFailure::Pipe,
                    PayloadReadError::Failure(failure) => failure,
                })
                .and_then(|bytes| {
                    decode_payload(&mime.1, &bytes).map_err(|_| PayloadFailure::Pipe)
                });
            let _ = sender.send(WorkerResult {
                transfer_id: id,
                payload: result,
            });
        });
        let builder = thread::Builder::new().name(format!("cosmix-wl-dnd-payload-{}", id.0));
        if spawn(builder, worker).is_err() {
            self.fail_active(id, TerminalReason::PipeFailure, now);
            return Ok(());
        }
        self.live_workers.insert(id);
        self.active
            .as_mut()
            .expect("fetch transfer remains active")
            .fetch_cancel = Some(cancel);
        self.active
            .as_mut()
            .expect("fetch transfer remains active")
            .fetched_mime = Some(fetched_mime);
        Ok(())
    }

    fn poll_workers(&mut self, now: Instant) {
        while let Ok(result) = self.worker_rx.try_recv() {
            self.live_workers.remove(&result.transfer_id);
            if self.retired.remove(&result.transfer_id) {
                continue;
            }
            if let Some(active) = self
                .active
                .as_mut()
                .filter(|active| active.transfer.id() == result.transfer_id)
            {
                active.fetch_cancel = None;
            }
            self.process_protocol_event(ProtocolEvent::Worker(result), now);
        }
    }

    fn process_protocol_event(&mut self, event: ProtocolEvent, now: Instant) {
        match event {
            ProtocolEvent::SeatRemoved(removed) => {
                if self
                    .outgoing
                    .as_ref()
                    .is_some_and(|active| active.seat == removed.seat)
                    && let Some(id) = self.outgoing_id()
                {
                    self.terminate_outgoing_at(id, OutgoingTerminalReason::SeatRemoved, now);
                }
                if let Some(active) = self.active.as_ref().filter(|active| {
                    active
                        .backend
                        .as_deref()
                        .is_some_and(|backend| backend.device_identity() == removed.device)
                }) {
                    let id = active.transfer.id();
                    self.fail_active(id, TerminalReason::OfferRejected, now);
                }
            }
            ProtocolEvent::PointerCapabilityLost { seat } => {
                // The seat survives, so its incoming transfers are untouched.
                // Only the drag started from the vanished pointer ends.
                if self
                    .outgoing
                    .as_ref()
                    .is_some_and(|active| active.seat == seat)
                    && let Some(id) = self.outgoing_id()
                {
                    self.terminate_outgoing_at(
                        id,
                        OutgoingTerminalReason::PointerCapabilityLost,
                        now,
                    );
                }
            }
            ProtocolEvent::OfferReplaced { transfer_id } => {
                self.offer_replaced(transfer_id);
            }
            ProtocolEvent::Enter {
                transfer_id,
                backend,
                owned_surface,
                position,
                transport_revision,
            } => self.enter(
                transfer_id,
                backend,
                owned_surface,
                position,
                transport_revision,
            ),
            ProtocolEvent::Leave { transfer_id } => {
                self.state.retire_pointer_leave(transfer_id);
                if self.consumer_active_id() != Some(transfer_id) {
                    return;
                }
                let active = self.active.as_mut().expect("device and id matched");
                if active.dropped {
                    active.post_drop_left = true;
                } else {
                    active.offer_dead = true;
                }
                let effects = active.transfer.leave(now).unwrap_or_default();
                self.apply_effects(effects);
            }
            ProtocolEvent::Drop {
                transfer_id,
                at_revision,
            } => {
                if self.consumer_active_id() != Some(transfer_id) {
                    return;
                }
                let active = self.active.as_mut().expect("device and id matched");
                active.dropped = true;
                let recorded =
                    active
                        .transfer
                        .physical_drop(at_revision, now, self.config.drop_fence_timeout);
                if recorded.is_err() {
                    self.fail_active(transfer_id, TerminalReason::OfferRejected, now);
                }
            }
            ProtocolEvent::Worker(result) => {
                if self.consumer_active_id() != Some(result.transfer_id) {
                    return;
                }
                let effects = self
                    .active
                    .as_mut()
                    .expect("id matched")
                    .transfer
                    .payload_ready(result.transfer_id, result.payload, now)
                    .unwrap_or_default();
                self.apply_effects(effects);
            }
            ProtocolEvent::SelectedAction {
                transfer_id,
                action,
                transport_revision,
            } => {
                if self.protocol_fact_id() != Some(transfer_id) {
                    return;
                }
                if let Some(pending) = self.pending_completion.as_ref() {
                    if action != Some(pending.action) {
                        self.reject_pending_completion(
                            transfer_id,
                            TerminalReason::FinalActionRejected,
                        );
                    }
                    return;
                }
                let effects = self
                    .active
                    .as_mut()
                    .expect("id matched")
                    .transfer
                    .compositor_action(action, now)
                    .unwrap_or_default();
                self.apply_effects(effects);
                if let Some(event) = ordinary_event_for_active(
                    self.consumer_active_id(),
                    transfer_id,
                    BridgeEvent::ActionChanged {
                        transfer_id,
                        action,
                        transport_revision,
                    },
                ) {
                    self.enqueue_app(event);
                }
            }
            ProtocolEvent::SourceActions {
                transfer_id,
                actions,
                transport_revision,
            } => {
                if self.protocol_fact_id() != Some(transfer_id) {
                    return;
                }
                if let Some(active) = self.active.as_mut() {
                    active.source_actions = actions;
                }
                if self
                    .pending_completion
                    .as_ref()
                    .is_some_and(|pending| !actions.contains(pending.action))
                {
                    self.reject_pending_completion(
                        transfer_id,
                        TerminalReason::FinalActionRejected,
                    );
                    return;
                }
                if self.consumer_active_id() == Some(transfer_id) {
                    self.enqueue_app(BridgeEvent::SourceActionsChanged {
                        transfer_id,
                        actions,
                        transport_revision,
                    });
                }
            }
            ProtocolEvent::Motion {
                transfer_id,
                position,
                transport_revision,
            } => {
                if self.consumer_active_id() == Some(transfer_id)
                    && self
                        .active
                        .as_ref()
                        .is_some_and(|active| !active.post_drop_left)
                {
                    self.enqueue_app(BridgeEvent::Motion {
                        transfer_id,
                        position,
                        transport_revision,
                    });
                }
            }
            ProtocolEvent::BarrierDone {
                transfer_id,
                barrier_id,
                requested_action,
                selected_action,
            } => {
                if self.consumer_active_id() != Some(transfer_id) {
                    return;
                }
                // Resolve only while draining the fully dispatched callback
                // batch. `SourceActions` is coalesced action work and drains
                // after lifecycle, so ActiveOffer may still hold the prior
                // mask here; the callback capture is the latest settled fact.
                let source_actions = self.state.callback_source_actions_for(transfer_id);
                let action = match selected_action {
                    Some(action) if action != DndAction::Ask && source_actions.contains(action) => {
                        Some(action)
                    }
                    _ if source_actions.contains(requested_action) => Some(requested_action),
                    _ => None,
                };
                let effects = self
                    .active
                    .as_mut()
                    .expect("id matched")
                    .transfer
                    .final_action_barrier_done(barrier_id, action, now)
                    .unwrap_or_default();
                self.apply_effects(effects);
            }
            ProtocolEvent::SourceAccepted { transfer_id } => {
                self.source_accepted(transfer_id, now);
            }
            ProtocolEvent::SourceSend {
                transfer_id,
                mime_type,
                pipe,
            } => {
                self.source_send(transfer_id, mime_type, pipe, now);
            }
            ProtocolEvent::SourceCancelled { transfer_id } => {
                if self.outgoing_id() == Some(transfer_id) {
                    if let Some(active) = self.outgoing.as_mut() {
                        active
                            .transfer
                            .cancel(OutgoingTerminalReason::CompositorCancelled, now);
                    }
                    let reason = self
                        .outgoing
                        .as_ref()
                        .and_then(|active| active.transfer.terminal())
                        .unwrap_or(OutgoingTerminalReason::CompositorCancelled);
                    self.terminate_outgoing(transfer_id, reason);
                }
            }
            ProtocolEvent::SourceDropped { transfer_id } => {
                self.source_dropped(transfer_id, now);
            }
            ProtocolEvent::SourceFinished { transfer_id } => {
                self.source_finished(transfer_id, now);
            }
            ProtocolEvent::SourceAction {
                transfer_id,
                action,
            } => {
                self.source_action(transfer_id, action, now);
            }
        }
    }

    fn enter(
        &mut self,
        transfer_id: DataTransferId,
        backend: Option<Box<dyn OfferBackend>>,
        owned_surface: bool,
        position: Position,
        transport_revision: TransportRevision,
    ) {
        if self.state.replaced_before_enter.remove(&transfer_id) {
            // SCTK destroyed this proxy before the replacement Enter handler;
            // dropping the captured wrapper must not send a second destroy.
            drop(backend);
            self.state.retire_transfer_terminal(transfer_id);
            return;
        }
        if take_cancelled_before_enter(&mut self.state.cancelled_before_enter, transfer_id) {
            if let Some(backend) = backend {
                reject_offer_backend(backend);
            }
            self.state.retire_transfer_terminal(transfer_id);
            return;
        }
        let Some(backend) = backend else {
            self.state.retire_transfer_terminal(transfer_id);
            return;
        };
        if !owned_surface {
            reject_offer_backend(backend);
            self.state.retire_transfer_terminal(transfer_id);
            return;
        }
        // There is one bridge consumer and therefore one active destination
        // transfer. The first seat wins; a concurrent seat's offer is rejected
        // rather than stealing or terminating the live transfer.
        if self.active.is_some() {
            reject_offer_backend(backend);
            self.state.retire_transfer_terminal(transfer_id);
            return;
        }
        let offered_mimes = backend.offered_mimes();
        let raw_mimes = offered_mimes
            .iter()
            .map(|(raw, _)| raw.clone())
            .collect::<Vec<_>>();
        let nonce = raw_mimes
            .iter()
            .find_map(|raw| TransferNonce::from_mime(raw));
        let origin = if self.outgoing.is_some() {
            match self.nonce_registry.attach_offered_echo(
                &raw_mimes,
                self.outgoing_id().expect("checked above"),
                transfer_id,
            ) {
                Ok(correlation) => DndOrigin::Internal(correlation.source),
                _ => {
                    reject_offer_backend(backend);
                    self.state.retire_transfer_terminal(transfer_id);
                    return;
                }
            }
        } else if let Some(nonce) = nonce {
            // No outgoing transfer, so this offer cannot be our own echo and
            // there is nothing to correlate it against.
            //
            // A tombstoned nonce is our own drag arriving after it ended, and a
            // still-live one is a second echo of a transfer whose incoming half
            // has not retired. Both stay rejected.
            //
            // Any other nonce belongs to a *different* process — a second
            // cosmix app is the case that matters, and it necessarily offers a
            // nonce we have never seen. Treating it as external grants an
            // attacker nothing: `External` is the less privileged origin, and
            // simply omitting the private MIME reaches it anyway. Only
            // `Internal` is worth spoofing, and that still demands a live
            // registry match on the branch above.
            match self.nonce_registry.lookup(&nonce) {
                Err(NonceLookupError::Unknown) => DndOrigin::External(transfer_id),
                _ => {
                    reject_offer_backend(backend);
                    self.state.retire_transfer_terminal(transfer_id);
                    return;
                }
            }
        } else {
            DndOrigin::External(transfer_id)
        };
        let descriptors = offered_mimes
            .iter()
            .map(|(raw, mime)| MimeDescriptor {
                essence: mime.essence(),
                raw: raw.clone(),
            })
            .collect();
        let source_actions = backend.source_actions();

        self.active = Some(ActiveOffer {
            backend: Some(backend),
            offer_dead: false,
            post_drop_left: false,
            dropped: false,
            offered_mimes,
            source_actions,
            origin,
            transfer: ReceiveTransfer::new(
                transfer_id,
                self.config.ask_confirmation_deadline,
                self.config.post_decision_deadline,
            ),
            fetch_cancel: None,
            fetched_mime: None,
        });
        self.enqueue_app(BridgeEvent::Entered {
            transfer_id,
            position,
            mime_types: descriptors,
            source_actions,
            transport_revision,
        });
    }

    fn apply_effects(&mut self, effects: Vec<ReceiveEffect>) {
        for effect in effects {
            match effect {
                ReceiveEffect::EmitDrop(drop) => {
                    self.enqueue_app(BridgeEvent::Drop(drop));
                }
                ReceiveEffect::SetActions { allowed, preferred } => {
                    if let Some(id) = self.consumer_active_id() {
                        let result = self
                            .active
                            .as_ref()
                            .expect("id came from active")
                            .set_actions(allowed, Some(preferred));
                        if let Err(error) = result {
                            let reason = if matches!(error, BridgeError::OfferProxyDead) {
                                TerminalReason::OfferProxyDead
                            } else {
                                TerminalReason::FinalActionRejected
                            };
                            self.fail_active(id, reason, Instant::now());
                        }
                    }
                }
                ReceiveEffect::HoverCleared {
                    transfer_id,
                    post_drop,
                } => self.enqueue_app(BridgeEvent::HoverLeft {
                    transfer_id,
                    post_drop,
                }),
                ReceiveEffect::FinishOffer => {}
                ReceiveEffect::DestroyOffer => {
                    self.close_offer(false);
                }
                ReceiveEffect::Terminal(event) => {
                    if event.disposition == TerminalDisposition::Finished {
                        let Some((deadline, expiry_reason)) = self
                            .active
                            .as_ref()
                            .and_then(|active| active.transfer.completion_flush_deadline())
                        else {
                            self.close_offer(false);
                            self.retire_active_with_terminal(TerminalEvent {
                                transfer_id: event.transfer_id,
                                disposition: TerminalDisposition::Rejected,
                                reason: TerminalReason::PostDecisionDeadlineExpired,
                            });
                            continue;
                        };
                        let Some(action) = self
                            .active
                            .as_ref()
                            .and_then(|active| active.transfer.completion_action())
                        else {
                            self.close_offer(false);
                            self.retire_active_with_terminal(TerminalEvent {
                                transfer_id: event.transfer_id,
                                disposition: TerminalDisposition::Rejected,
                                reason: TerminalReason::FinalActionRejected,
                            });
                            continue;
                        };
                        self.state
                            .protocol_queue
                            .discard_ordinary_for(event.transfer_id);
                        self.app_queue.discard_ordinary_for(event.transfer_id);
                        // Keep callback correlation until both finish flushes
                        // resolve. A same-device Enter in this window is an
                        // observable replacement of this still-retained offer.
                        self.pending_completion = Some(PendingCompletion {
                            terminal: event,
                            action,
                            deadline: deadline.at,
                            expiry_reason,
                            finish_sent: false,
                        });
                    } else {
                        self.retire_active_with_terminal(event);
                    }
                }
            }
        }
    }

    fn drive_pending_completion(&mut self, now: Instant) -> Result<(), BridgeError> {
        self.drive_pending_completion_with(now, &mut |bridge| bridge.flush_connection())
    }

    fn drive_pending_completion_with(
        &mut self,
        now: Instant,
        flush: &mut impl FnMut(&mut Self) -> Result<FlushStatus, BridgeError>,
    ) -> Result<(), BridgeError> {
        let Some(pending) = self.pending_completion.as_ref() else {
            return Ok(());
        };
        if now >= pending.deadline {
            let expiry_reason = pending.expiry_reason;
            self.exit_pending_completion(expiry_reason);
            return Ok(());
        }

        let status = flush(self)?;
        let progress = self
            .pending_completion
            .as_ref()
            .expect("completion still pending")
            .after_flush(status);
        match progress {
            CompletionFlushProgress::Waiting => return Ok(()),
            CompletionFlushProgress::QueueFinish => {
                if !self.close_offer(true) {
                    let id = self
                        .pending_completion
                        .take()
                        .expect("completion still pending")
                        .terminal
                        .transfer_id;
                    self.retire_active_with_terminal(TerminalEvent {
                        transfer_id: id,
                        disposition: TerminalDisposition::Rejected,
                        reason: TerminalReason::OfferProxyDead,
                    });
                    return Ok(());
                }
                // `close_offer(true)` has queued finish on the connection. That
                // commits success while the connection remains viable, even if
                // this flush would block; a fatal error latches connection loss
                // before the reserved completion is exited.
                self.pending_completion
                    .as_mut()
                    .expect("completion still pending")
                    .finish_sent = true;
                let status = flush(self)?;
                if self
                    .pending_completion
                    .as_ref()
                    .expect("completion still pending")
                    .after_flush(status)
                    == CompletionFlushProgress::Waiting
                {
                    return Ok(());
                }
            }
            CompletionFlushProgress::Complete => {}
        }
        let terminal = self
            .pending_completion
            .take()
            .expect("completion still pending")
            .terminal;
        self.retire_active_with_terminal(terminal);
        Ok(())
    }

    fn retire_active_with_terminal(&mut self, event: TerminalEvent) {
        self.app_queue.discard_ordinary_for(event.transfer_id);
        let _ = self.app_queue.enqueue(BridgeEvent::Terminal(event));
        if let Some(cancel) = self
            .active
            .as_mut()
            .and_then(|active| active.fetch_cancel.take())
        {
            cancel.cancel();
            self.retired.insert(event.transfer_id);
        }
        self.state.retire_transfer_terminal(event.transfer_id);
        let _ = self.nonce_registry.incoming_terminal(event.transfer_id);
        self.active = None;
    }

    fn close_offer(&mut self, finish: bool) -> bool {
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.backend.is_some())
        {
            // SCTK destroys an undropped offer before delivering Leave. The
            // proxy's liveness is that fact even if queued Leave work overflows
            // or dispatch fails before the bridge can drain it.
            active.offer_dead |= active
                .backend
                .as_deref()
                .is_some_and(|backend| !backend.is_alive());
            let finish_sent = !finish || active.finish().is_ok();
            // SCTK destroys an undropped offer before its Leave handler and
            // destroys a replaced offer before the next Enter handler.
            if !active.offer_dead {
                let _ = active.destroy();
            }
            active.backend = None;
            return finish_sent;
        }
        !finish
    }

    fn enqueue_app(&mut self, event: BridgeEvent) {
        let transfer_id = event.transfer_id();
        if self.app_queue.enqueue(event).is_err() {
            let Some(id) = transfer_id.or_else(|| self.consumer_active_id()) else {
                return;
            };
            self.fail_active(id, TerminalReason::QueueOverflow, Instant::now());
        }
    }

    fn fail_active(&mut self, id: DataTransferId, reason: TerminalReason, now: Instant) {
        if self.consumer_active_id() == Some(id) {
            self.state.protocol_queue.discard_ordinary_for(id);
            let effects = self
                .active
                .as_mut()
                .expect("id matched")
                .transfer
                .fail(id, reason, now)
                .unwrap_or_default();
            self.apply_effects(effects);
            return;
        }
        if self.protocol_fact_id() == Some(id) {
            self.reject_pending_completion(id, reason);
        }
    }

    fn reject_pending_completion(&mut self, id: DataTransferId, reason: TerminalReason) {
        if self.protocol_fact_id() != Some(id) || self.pending_completion.take().is_none() {
            return;
        }
        self.state.protocol_queue.discard_ordinary_for(id);
        self.close_offer(false);
        self.retire_active_with_terminal(TerminalEvent {
            transfer_id: id,
            disposition: TerminalDisposition::Rejected,
            reason,
        });
    }

    /// Exits a reserved completion through any non-success path.
    ///
    /// A queued finish commits the reserved `Completed` while the connection
    /// remains viable, because a later flush can still send it. Before finish is
    /// queued, or after a fatal connection loss makes that queue undrainable,
    /// synthesize the caller's rejection terminal.
    fn exit_pending_completion(&mut self, rejection_reason: TerminalReason) -> bool {
        let Some(pending) = self.pending_completion.take() else {
            return false;
        };
        // The loss latch can be stale when winit observes a disconnect before
        // this bridge pumps again, so decide queued-finish viability from a raw
        // flush now. This must not use `flush_connection`: its fatal path calls
        // `lose_connection`, which would re-enter this terminal-producing exit.
        let (finish_viable, fatal_probe) = if pending.finish_sent && self.connection_lost.is_none()
        {
            match self.raw_flush_connection() {
                Ok(FlushStatus::Flushed | FlushStatus::WouldBlock) => (true, false),
                Err(error) => {
                    self.connection_lost = Some(error.to_string());
                    (false, true)
                }
            }
        } else {
            (false, false)
        };
        let terminal = if finish_viable {
            pending.terminal
        } else {
            TerminalEvent {
                transfer_id: pending.terminal.transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: if fatal_probe {
                    TerminalReason::WaylandConnectionLost
                } else {
                    rejection_reason
                },
            }
        };
        self.close_offer(false);
        self.retire_active_with_terminal(terminal);
        true
    }

    fn offer_replaced(&mut self, id: DataTransferId) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.transfer.id() != id)
        {
            return;
        }
        self.active.as_mut().expect("id matched").offer_dead = true;
        self.state.replaced_before_enter.remove(&id);
        if self.consumer_active_id() == Some(id) {
            let effects = self
                .active
                .as_mut()
                .expect("id matched")
                .transfer
                .offer_replaced(id)
                .unwrap_or_default();
            self.apply_effects(effects);
            return;
        }

        // Call unconditionally and assert the result separately: `debug_assert!`
        // does not evaluate its argument in release builds, so wrapping the call
        // itself would silently skip the terminal in every shipped binary.
        let exited = self.exit_pending_completion(TerminalReason::OfferReplaced);
        debug_assert!(
            exited,
            "a non-consumer-active transfer must hold a pending completion"
        );
    }

    fn outgoing_id(&self) -> Option<DataTransferId> {
        self.outgoing.as_ref().map(|active| active.transfer.id())
    }

    fn ensure_outgoing(&self, id: DataTransferId) -> Result<(), BridgeError> {
        match self.outgoing_id() {
            Some(active) if active == id => Ok(()),
            Some(active) => Err(BridgeError::Send(SendError::StaleTransfer {
                active,
                received: id,
            })),
            None => Err(BridgeError::Send(SendError::NoActiveTransfer)),
        }
    }

    fn enqueue_outgoing(&mut self, event: OutgoingEvent) {
        if matches!(event, OutgoingEvent::ActionChanged { .. }) {
            self.outgoing_events
                .retain(|queued| !matches!(queued, OutgoingEvent::ActionChanged { .. }));
        }
        if self.outgoing_events.len() >= self.outgoing_event_capacity {
            if let Some(id) = self.outgoing_id() {
                self.terminate_outgoing(id, OutgoingTerminalReason::QueueOverflow);
            }
            return;
        }
        // A non-terminal event is only meaningful while its transfer is still
        // consumer-visible. Terminal transitions clear this queue first.
        if self.outgoing.is_some() {
            self.outgoing_events.push_back(event);
        }
    }

    fn terminate_outgoing(&mut self, transfer_id: DataTransferId, reason: OutgoingTerminalReason) {
        // Match before taking: `take().filter(..)` would drop the live transfer
        // on an id mismatch, destroying a session this call has no claim on.
        if self.outgoing_id() != Some(transfer_id) {
            return;
        }
        // A transfer `start_outgoing` has not yet returned an id for owes the
        // consumer nothing: it is being told by the synchronous `Err`, and a
        // terminal naming an unknown id would also burn one of the bounded
        // terminal slots. Keyed on the id rather than on the call path, so it
        // holds however the teardown is reached.
        let reserve = self.unstarted_outgoing != Some(transfer_id);
        let Some(mut active) = self.outgoing.take() else {
            return;
        };
        // Deliberate: the terminal is reserved as soon as the writers are
        // signalled, not once their fds have closed. The peer can therefore
        // still be blocked on EOF for the microseconds it takes each cancelled
        // worker to wake and drop its pipe. Waiting would mean joining writer
        // threads from the event-loop thread — a real hang risk in exchange for
        // a visibility ordering no consumer can act on, and the cancellation
        // socket already bounds how long a worker can hold the fd.
        for (_, cancel) in active.writers {
            cancel.cancel();
        }
        self.outgoing_events.clear();
        self.state.source_transfers.retire(transfer_id);
        // A drag whose pointer vanished ends *because* of that, whatever
        // reason reached this call first — a `SourceCancelled` that drained
        // ahead of the lifecycle event, or a deadline once the event was
        // dropped. `Completed` is the exception and keeps its name: the drop
        // was delivered, which is the more useful fact and is not contradicted
        // by the pointer going afterwards.
        let reason = outgoing_terminal_reason(reason, self.state.active_source_pointer_lost.take());
        if self.state.active_source_seat.as_ref() == Some(&active.seat) {
            self.state.active_source_seat = None;
        }
        self.nonce_registry.outgoing_terminal(transfer_id);
        if reserve {
            self.outgoing_terminals.push_back(OutgoingEvent::Terminal {
                transfer_id,
                reason,
            });
        }
        // The icon role is permanent, so its surface is destroyed at the
        // outgoing terminal rather than returned to any reuse pool. This is
        // deliberately later than dnd_drop_performed and any incoming echo
        // terminal: neither event says the source-side drag has ended.
        destroy_drag_icon(&mut active.icon);
        // Dropping the single owned DragSource destroys it exactly once.
        drop(active.source);
    }

    fn terminate_outgoing_at(
        &mut self,
        transfer_id: DataTransferId,
        fallback: OutgoingTerminalReason,
        now: Instant,
    ) {
        let reason = self
            .outgoing
            .as_mut()
            .and_then(|active| active.transfer.check_deadline(now))
            .unwrap_or(fallback);
        self.terminate_outgoing(transfer_id, reason);
    }

    fn source_accepted(&mut self, transfer_id: DataTransferId, now: Instant) {
        if self.outgoing_id() != Some(transfer_id) {
            return;
        }
        let result = self
            .outgoing
            .as_mut()
            .expect("id matched")
            .transfer
            .accepted(now);
        if result.is_err()
            && let Some(reason) = self
                .outgoing
                .as_ref()
                .and_then(|active| active.transfer.terminal())
        {
            self.terminate_outgoing(transfer_id, reason);
        }
    }

    fn source_action(
        &mut self,
        transfer_id: DataTransferId,
        action: Option<DndAction>,
        now: Instant,
    ) {
        if self.outgoing_id() != Some(transfer_id) {
            return;
        }
        let result = self
            .outgoing
            .as_mut()
            .expect("id matched")
            .transfer
            .action(now);
        if result.is_err() {
            if let Some(reason) = self
                .outgoing
                .as_ref()
                .and_then(|active| active.transfer.terminal())
            {
                self.terminate_outgoing(transfer_id, reason);
            }
            return;
        }
        self.enqueue_outgoing(OutgoingEvent::ActionChanged {
            transfer_id,
            action,
        });
    }

    fn source_dropped(&mut self, transfer_id: DataTransferId, now: Instant) {
        if self.outgoing_id() != Some(transfer_id) {
            return;
        }
        let result = self
            .outgoing
            .as_mut()
            .expect("id matched")
            .transfer
            .dropped(now);
        if result.is_err() {
            if let Some(reason) = self
                .outgoing
                .as_ref()
                .and_then(|active| active.transfer.terminal())
            {
                self.terminate_outgoing(transfer_id, reason);
            }
            return;
        }
        self.enqueue_outgoing(OutgoingEvent::DropPerformed { transfer_id });
    }

    fn source_finished(&mut self, transfer_id: DataTransferId, now: Instant) {
        if self.outgoing_id() != Some(transfer_id) {
            return;
        }
        let terminal = self
            .outgoing
            .as_mut()
            .expect("id matched")
            .transfer
            .finished(now);
        if let Some(reason) = terminal {
            self.terminate_outgoing(transfer_id, reason);
        }
    }

    fn source_send(
        &mut self,
        transfer_id: DataTransferId,
        mime_type: String,
        pipe: WritePipe,
        now: Instant,
    ) {
        if self.outgoing_id() != Some(transfer_id) {
            drop(pipe);
            return;
        }
        if self
            .outgoing
            .as_ref()
            .is_some_and(|active| active.writers.len() >= self.outgoing_event_capacity)
        {
            drop(pipe);
            self.terminate_outgoing(transfer_id, OutgoingTerminalReason::QueueOverflow);
            return;
        }
        let bytes = match self
            .outgoing
            .as_mut()
            .expect("id matched")
            .transfer
            .begin_send(&mime_type, now)
        {
            Ok(bytes) => bytes,
            Err(reason) => {
                drop(pipe);
                self.terminate_outgoing(transfer_id, reason);
                return;
            }
        };
        let writer_id = {
            let active = self.outgoing.as_mut().expect("id matched");
            let id = active.next_writer_id;
            active.next_writer_id = active.next_writer_id.wrapping_add(1).max(1);
            id
        };
        let Ok((bridge_wake, worker_wake)) = UnixStream::pair() else {
            drop(pipe);
            if let Some(active) = self.outgoing.as_mut() {
                active.transfer.writer_spawn_failed(now);
            }
            self.terminate_outgoing(transfer_id, OutgoingTerminalReason::WriterSpawnFailed);
            return;
        };
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = SendCancel {
            flag: Arc::clone(&flag),
            waker: bridge_wake,
        };
        let worker_cancel = SendCancelWorker {
            flag,
            wake: worker_wake,
        };
        self.outgoing
            .as_mut()
            .expect("id matched")
            .writers
            .push((writer_id, cancel));
        let sender = self.send_worker_tx.clone();
        let worker_mime = mime_type.clone();
        let inactivity = self.config.send.finish_deadline;
        let worker = move || {
            let result = write_payload_and_close(pipe, &bytes, &worker_cancel, inactivity);
            let _ = sender.send(SendWorkerResult {
                transfer_id,
                writer_id,
                mime_type: worker_mime,
                result,
            });
        };
        if thread::Builder::new()
            .name(format!("cosmix-dnd-send-{}", transfer_id.0))
            .spawn(worker)
            .is_err()
        {
            if let Some(active) = self.outgoing.as_mut() {
                active
                    .writers
                    .retain(|(candidate, _)| *candidate != writer_id);
                active.transfer.writer_spawn_failed(now);
            }
            self.terminate_outgoing(transfer_id, OutgoingTerminalReason::WriterSpawnFailed);
        }
    }

    fn poll_send_workers(&mut self, now: Instant) {
        while let Ok(result) = self.send_worker_rx.try_recv() {
            if self.outgoing_id() != Some(result.transfer_id) {
                continue;
            }
            if let Some(active) = self.outgoing.as_mut() {
                active
                    .writers
                    .retain(|(writer_id, _)| *writer_id != result.writer_id);
            }
            let success = result.result.is_ok();
            let terminal = self
                .outgoing
                .as_mut()
                .expect("id matched")
                .transfer
                .writer_finished(success, now);
            // `terminate_outgoing` clears the pending queue, so enqueueing
            // ahead of a terminal writes an event no consumer ever sees.
            if success && terminal.is_none() {
                self.enqueue_outgoing(OutgoingEvent::DataSent {
                    transfer_id: result.transfer_id,
                    mime_type: result.mime_type,
                });
            }
            if let Some(reason) = terminal {
                self.terminate_outgoing(result.transfer_id, reason);
            }
        }
    }

    fn ensure_active(&self, id: DataTransferId) -> Result<(), BridgeError> {
        match self.consumer_active_id() {
            Some(active) if active == id => Ok(()),
            Some(active) => Err(BridgeError::StaleTransfer {
                active,
                received: id,
            }),
            None => Err(BridgeError::NoActiveTransfer),
        }
    }

    /// The transfer still exposed to the consumer and its public methods.
    fn consumer_active_id(&self) -> Option<DataTransferId> {
        self.active
            .as_ref()
            .filter(|active| active.transfer.terminal_event().is_none())
            .map(|active| active.transfer.id())
    }

    /// The transfer whose compositor facts can still revise the outcome.
    fn protocol_fact_id(&self) -> Option<DataTransferId> {
        let active = self.active.as_ref()?;
        if active.transfer.terminal_event().is_none()
            || self.pending_completion.as_ref().is_some_and(|pending| {
                pending.terminal.transfer_id == active.transfer.id() && !pending.finish_sent
            })
        {
            Some(active.transfer.id())
        } else {
            None
        }
    }

    fn drain_app_frame(&mut self) -> Vec<BridgeEvent> {
        let events = self.app_queue.drain_frame();
        self.delivered_revision = newest_delivered_revision(self.delivered_revision, &events);
        events
    }
}

fn ordinary_event_for_active(
    active: Option<DataTransferId>,
    transfer_id: DataTransferId,
    event: BridgeEvent,
) -> Option<BridgeEvent> {
    (active == Some(transfer_id)).then_some(event)
}

fn classify_flush(result: Result<(), WaylandError>) -> Result<FlushStatus, WaylandError> {
    match result {
        Ok(()) => Ok(FlushStatus::Flushed),
        Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Ok(FlushStatus::WouldBlock)
        }
        Err(error) => Err(error),
    }
}

fn remember_cancelled_before_enter(
    cancelled: &mut VecDeque<DataTransferId>,
    transfer_id: DataTransferId,
    capacity: usize,
) -> Option<DataTransferId> {
    if cancelled.contains(&transfer_id) {
        return None;
    }
    let evicted = (cancelled.len() == capacity)
        .then(|| cancelled.pop_front())
        .flatten();
    cancelled.push_back(transfer_id);
    evicted
}

fn take_cancelled_before_enter(
    cancelled: &mut VecDeque<DataTransferId>,
    transfer_id: DataTransferId,
) -> bool {
    let Some(index) = cancelled
        .iter()
        .position(|candidate| *candidate == transfer_id)
    else {
        return false;
    };
    cancelled.remove(index);
    true
}

fn newest_delivered_revision(
    current: TransportRevision,
    events: &[BridgeEvent],
) -> TransportRevision {
    events.iter().fold(current, |latest, event| {
        let revision = match event {
            BridgeEvent::Entered {
                transport_revision, ..
            }
            | BridgeEvent::Motion {
                transport_revision, ..
            }
            | BridgeEvent::ActionChanged {
                transport_revision, ..
            }
            | BridgeEvent::SourceActionsChanged {
                transport_revision, ..
            } => *transport_revision,
            BridgeEvent::HoverLeft { .. } | BridgeEvent::Drop(_) | BridgeEvent::Terminal(_) => {
                latest
            }
        };
        latest.max(revision)
    })
}

fn validate_final_action_offered(
    source_actions: ActionMask,
    action: DndAction,
) -> Result<(), AcceptanceError> {
    if source_actions.contains(action) {
        Ok(())
    } else {
        Err(AcceptanceError::FinalActionNotOffered {
            action,
            source_actions,
        })
    }
}

fn latest_transfer_for_key<K: PartialEq>(
    entries: &[(K, DataTransferId)],
    key: &K,
) -> Option<DataTransferId> {
    entries
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, transfer_id)| *transfer_id)
}

impl Drop for WaylandBridge {
    fn drop(&mut self) {
        if let Some(cancel) = self
            .active
            .as_mut()
            .and_then(|active| active.fetch_cancel.take())
        {
            cancel.cancel();
        }
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.backend.is_some())
        {
            active.offer_dead |= active
                .backend
                .as_deref()
                .is_some_and(|backend| !backend.is_alive());
            if !active.offer_dead {
                let _ = active.destroy();
            }
            active.backend = None;
        }
        if let Some(mut active) = self.outgoing.take() {
            for (_, cancel) in active.writers {
                cancel.cancel();
            }
            self.state.source_transfers.retire(active.transfer.id());
            self.nonce_registry.outgoing_terminal(active.transfer.id());
            destroy_drag_icon(&mut active.icon);
        }
        self.state.release_seat_objects();
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
    }
}

struct SeatObjects {
    seat: WlSeat,
    data_device: DataDevice,
    pointer: Option<WlPointer>,
}

struct HeldGrab {
    seat: WlSeat,
    pointer: WlPointer,
    serial: u32,
    origin: WlSurface,
    button: u32,
}

struct RemovedSeat {
    seat: CallbackIdentity,
    device: CallbackIdentity,
    transfer_ids: Vec<DataTransferId>,
}

#[derive(Clone, Copy, Debug)]
struct AskBarrier {
    transfer_id: DataTransferId,
    barrier_id: u64,
    requested_action: DndAction,
}

// Keeping the callback-captured offer backend inline is deliberate: Enter must
// own exactly the proxy observed by that callback. Processing-time lookup is
// structurally impossible, and the bounded queue holds at most a few dozen of
// these short-lived records.
#[allow(clippy::large_enum_variant)]
enum ProtocolEvent {
    SeatRemoved(RemovedSeat),
    /// A live seat lost its pointer capability. Its data device survives, so
    /// unlike [`Self::SeatRemoved`] this keys no incoming transfer — it exists
    /// solely to terminate an outgoing drag started from the vanished pointer.
    PointerCapabilityLost {
        seat: CallbackIdentity,
    },
    /// SCTK has already destroyed this proxy because the same data device
    /// entered another drag. This is a reserved terminal-latch event.
    OfferReplaced {
        transfer_id: DataTransferId,
    },
    Enter {
        transfer_id: DataTransferId,
        backend: Option<Box<dyn OfferBackend>>,
        owned_surface: bool,
        position: Position,
        transport_revision: TransportRevision,
    },
    Leave {
        transfer_id: DataTransferId,
    },
    Drop {
        transfer_id: DataTransferId,
        /// Revision current when the compositor's drop callback fired, so it
        /// includes any motion or action dispatched ahead of it in the same
        /// pump. Acceptance must cover this before the drop can be snapshotted.
        at_revision: TransportRevision,
    },
    Worker(WorkerResult),
    SelectedAction {
        transfer_id: DataTransferId,
        action: Option<DndAction>,
        transport_revision: TransportRevision,
    },
    SourceActions {
        transfer_id: DataTransferId,
        actions: ActionMask,
        transport_revision: TransportRevision,
    },
    Motion {
        transfer_id: DataTransferId,
        position: Position,
        transport_revision: TransportRevision,
    },
    BarrierDone {
        transfer_id: DataTransferId,
        barrier_id: u64,
        requested_action: DndAction,
        selected_action: Option<DndAction>,
    },
    SourceAccepted {
        transfer_id: DataTransferId,
    },
    SourceSend {
        transfer_id: DataTransferId,
        mime_type: String,
        pipe: WritePipe,
    },
    SourceCancelled {
        transfer_id: DataTransferId,
    },
    SourceDropped {
        transfer_id: DataTransferId,
    },
    SourceFinished {
        transfer_id: DataTransferId,
    },
    SourceAction {
        transfer_id: DataTransferId,
        action: Option<DndAction>,
    },
}

impl fmt::Debug for ProtocolEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtocolEvent")
            .field("transfer_id", &self.transfer_id())
            .finish_non_exhaustive()
    }
}

impl QueueEvent for ProtocolEvent {
    type CoalesceKey = (DataTransferId, u8);

    fn class(&self) -> EventClass<Self::CoalesceKey> {
        match self {
            Self::SelectedAction { transfer_id, .. } => EventClass::Action((*transfer_id, 0)),
            Self::SourceActions { transfer_id, .. } => EventClass::Action((*transfer_id, 1)),
            Self::Motion { transfer_id, .. } => EventClass::Motion((*transfer_id, 0)),
            Self::SourceAction { transfer_id, .. } => EventClass::Action((*transfer_id, 2)),
            Self::SourceAccepted { transfer_id } => EventClass::Action((*transfer_id, 3)),
            Self::OfferReplaced { transfer_id } => EventClass::Terminal(*transfer_id),
            Self::SourceCancelled { transfer_id } => EventClass::Terminal(*transfer_id),
            Self::SeatRemoved(_)
            | Self::PointerCapabilityLost { .. }
            | Self::Enter { .. }
            | Self::Leave { .. }
            | Self::Drop { .. }
            | Self::Worker(_)
            | Self::BarrierDone { .. }
            | Self::SourceSend { .. }
            | Self::SourceDropped { .. }
            | Self::SourceFinished { .. } => EventClass::Lifecycle,
        }
    }

    fn transfer_id(&self) -> Option<DataTransferId> {
        match self {
            Self::Drop {
                transfer_id: id, ..
            }
            | Self::OfferReplaced {
                transfer_id: id, ..
            }
            | Self::Leave {
                transfer_id: id, ..
            }
            | Self::SelectedAction {
                transfer_id: id, ..
            }
            | Self::SourceActions {
                transfer_id: id, ..
            }
            | Self::Motion {
                transfer_id: id, ..
            }
            | Self::BarrierDone {
                transfer_id: id, ..
            }
            | Self::SourceAccepted {
                transfer_id: id, ..
            }
            | Self::SourceSend {
                transfer_id: id, ..
            }
            | Self::SourceCancelled {
                transfer_id: id, ..
            }
            | Self::SourceDropped {
                transfer_id: id, ..
            }
            | Self::SourceFinished {
                transfer_id: id, ..
            }
            | Self::SourceAction {
                transfer_id: id, ..
            } => Some(*id),
            Self::Enter { transfer_id, .. } => Some(*transfer_id),
            Self::Worker(result) => Some(result.transfer_id),
            Self::SeatRemoved(_) | Self::PointerCapabilityLost { .. } => None,
        }
    }
}

#[derive(Default)]
struct KeyedCallbackCapture {
    entries: Vec<(CallbackIdentity, DataTransferId)>,
}

impl KeyedCallbackCapture {
    fn correlate(&mut self, key: CallbackIdentity, transfer_id: DataTransferId) {
        self.entries.push((key, transfer_id));
    }

    fn capture<T>(
        &self,
        key: &CallbackIdentity,
        event: impl FnOnce(DataTransferId) -> T,
    ) -> Option<T> {
        self.transfer_for(key).map(event)
    }

    fn transfer_for(&self, key: &CallbackIdentity) -> Option<DataTransferId> {
        latest_transfer_for_key(&self.entries, key)
    }

    fn transfer_ids_for(&self, key: &CallbackIdentity) -> Vec<DataTransferId> {
        self.entries
            .iter()
            .filter(|(candidate, _)| candidate == key)
            .map(|(_, id)| *id)
            .collect()
    }

    fn replace(
        &mut self,
        key: CallbackIdentity,
        transfer_id: DataTransferId,
    ) -> Vec<DataTransferId> {
        let replaced = self.transfer_ids_for(&key);
        self.entries.retain(|(candidate, _)| candidate != &key);
        self.entries.push((key, transfer_id));
        replaced
    }

    fn retire(&mut self, transfer_id: DataTransferId) {
        self.entries.retain(|(_, id)| *id != transfer_id);
    }
}

struct TransportState {
    registry_state: Option<RegistryState>,
    seat_state: Option<SeatState>,
    data_device_manager: Option<DataDeviceManagerState>,
    icon_compositor: Option<WlCompositor>,
    icon_shm: Option<Shm>,
    surface: Option<WlSurface>,
    data_devices: Vec<SeatObjects>,
    held_grab: Option<HeldGrab>,
    active_source_seat: Option<CallbackIdentity>,
    /// Set the moment the live drag's pointer stops existing, and the reason
    /// that says which way it went.
    ///
    /// The queued `SeatRemoved` / `PointerCapabilityLost` event is what ends
    /// the drag promptly, but it cannot be what *names* the ending: it is
    /// lifecycle-class, so a `SourceCancelled` enqueued in the same dispatch
    /// drains first and terminates the drag under its own reason, and a
    /// saturated lifecycle queue can drop it outright. Either way the drag
    /// would end as `CompositorCancelled` or a deadline while the truth is
    /// that the pointer is gone — and a consumer holding
    /// button-release-gated state cannot tell. Recording the loss directly on
    /// the state is immune to both: it is not queued and cannot be masked.
    active_source_pointer_lost: Option<OutgoingTerminalReason>,
    protocol_queue: BoundedEventQueue<ProtocolEvent>,
    device_transfers: KeyedCallbackCapture,
    offer_transfers: KeyedCallbackCapture,
    source_transfers: KeyedCallbackCapture,
    pending_barriers: Vec<AskBarrier>,
    callback_actions: Vec<(DataTransferId, Option<DndAction>)>,
    /// Latest source mask captured by the callback adapter. This is updated at
    /// callback time so a following sync-done cannot consult a queued snapshot.
    callback_source_actions: Vec<(DataTransferId, ActionMask)>,
    /// Replacement facts for old Enter records that have not been admitted
    /// yet. The reserved terminal event drains before lifecycle, so the fact
    /// must survive until that old Enter itself is rejected.
    replaced_before_enter: BTreeSet<DataTransferId>,
    /// Direct cancellation records for identified events discarded by queue
    /// overflow. These are drained by the frame core; no replacement event is
    /// needed, so queue pressure cannot discard a cancellation.
    overflowed_transfers: BTreeSet<DataTransferId>,
    /// Overflow cancellation observed before its queued Enter is processed.
    /// Its capacity is derived from the lifecycle queue: every still-queued
    /// Enter can therefore retain one cancellation latch.
    cancelled_before_enter: VecDeque<DataTransferId>,
    cancellation_capacity: usize,
    next_transfer_id: u64,
    /// Monotonic across every motion, action, source-action and enter callback.
    transport_revision: u64,
}

impl TransportState {
    fn release_seat_objects(&mut self) {
        for objects in &mut self.data_devices {
            if let Some(pointer) = objects.pointer.take()
                && pointer.version() >= 3
            {
                pointer.release();
            }
        }
        self.held_grab = None;
        self.active_source_seat = None;
        self.active_source_pointer_lost = None;
        self.data_devices.clear();
    }

    fn allocate_transfer_id(&mut self) -> DataTransferId {
        let id = DataTransferId(self.next_transfer_id);
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
        id
    }

    fn capture_enter(
        &mut self,
        device: CallbackIdentity,
        offer: Option<CallbackIdentity>,
        backend: Option<Box<dyn OfferBackend>>,
        owned_surface: bool,
        position: Position,
    ) -> DataTransferId {
        let id = self.allocate_transfer_id();
        let source_actions = backend
            .as_deref()
            .map(OfferBackend::source_actions)
            .unwrap_or(ActionMask::NONE);

        // SCTK 0.19.2 data_device.rs:137 destroys this device's previous drag
        // offer before calling the client's Enter handler. Retire the device
        // key now, independently of whether the new Enter is later admitted.
        // The offer key has the longer lifetime: it survives post-drop Leave,
        // while the device key does not survive this replacement Enter.
        for replaced in self.device_transfers.replace(device, id) {
            self.replaced_before_enter.insert(replaced);
            self.enqueue(ProtocolEvent::OfferReplaced {
                transfer_id: replaced,
            });
        }
        if let Some(offer) = offer {
            self.offer_transfers.correlate(offer, id);
        }
        self.callback_source_actions.push((id, source_actions));
        let transport_revision = self.next_revision();
        self.enqueue(ProtocolEvent::Enter {
            transfer_id: id,
            backend,
            owned_surface,
            position,
            transport_revision,
        });
        id
    }

    fn capture_drop(&mut self, transfer_id: DataTransferId) {
        // Drop itself does not advance the fence: it demands acceptance
        // covering every revision already captured in this callback batch.
        self.enqueue(ProtocolEvent::Drop {
            transfer_id,
            at_revision: self.latest_revision(),
        });
    }

    fn enqueue(&mut self, event: ProtocolEvent) {
        let Err(error) = self.protocol_queue.enqueue(event) else {
            return;
        };
        let discarded = error.into_event();
        match discarded {
            ProtocolEvent::Enter {
                transfer_id,
                backend,
                ..
            } => {
                if let Some(backend) = backend {
                    reject_offer_backend(backend);
                }
                self.retire_transfer_terminal(transfer_id);
            }
            ProtocolEvent::SeatRemoved(removed) => {
                for id in removed.transfer_ids {
                    self.record_overflow(id);
                }
            }
            // Keys no transfer, so there is nothing to record: the outgoing
            // half it would have terminated lives on the bridge, not here.
            // Dropping it costs the same as dropping a `SeatRemoved` whose
            // outgoing termination is likewise only reachable bridge-side —
            // the drag then ends at its active deadline instead. Matching that
            // existing behaviour is deliberate.
            ProtocolEvent::PointerCapabilityLost { .. } => {}
            discarded => {
                let id = discarded
                    .transfer_id()
                    .expect("every non-seat protocol event is transfer-keyed");
                self.record_overflow(id);
            }
        }
    }

    fn record_overflow(&mut self, id: DataTransferId) {
        self.overflowed_transfers.insert(id);
        if self.protocol_queue.lifecycle_contains(|event| {
            matches!(
                event,
                ProtocolEvent::Enter { transfer_id, .. } if *transfer_id == id
            )
        }) && let Some(evicted) = remember_cancelled_before_enter(
            &mut self.cancelled_before_enter,
            id,
            self.cancellation_capacity,
        ) {
            self.terminate_queued_enter(evicted);
        }
        self.retire_transfer_terminal(id);
    }

    fn terminate_queued_enter(&mut self, id: DataTransferId) {
        self.overflowed_transfers.insert(id);
        if let Some(ProtocolEvent::Enter { backend, .. }) =
            self.protocol_queue.remove_lifecycle(|event| {
                matches!(
                    event,
                    ProtocolEvent::Enter { transfer_id, .. } if *transfer_id == id
                )
            })
            && let Some(backend) = backend
        {
            reject_offer_backend(backend);
        }
        self.retire_transfer_terminal(id);
    }

    /// Saturating rather than wrapping: the fence compares revisions with `>=`,
    /// so wrapping would silently make a new acceptance look stale.
    /// Reaching `u64::MAX` would require roughly 1.8e19 callbacks in one
    /// process lifetime, so saturation is the deliberate fail-safe.
    fn next_revision(&mut self) -> TransportRevision {
        self.transport_revision = self.transport_revision.saturating_add(1);
        TransportRevision(self.transport_revision)
    }

    fn latest_revision(&self) -> TransportRevision {
        TransportRevision(self.transport_revision)
    }

    fn record_seat_removal(&mut self, seat: &WlSeat) -> Option<RemovedSeat> {
        let index = self
            .data_devices
            .iter()
            .position(|objects| &objects.seat == seat)?;
        // Removing SeatObjects is part of recording the callback. If the
        // lifecycle queue is saturated, no queued event is needed to release
        // the data device.
        let objects = self.data_devices.remove(index);
        // Same reasoning as the capability-loss branch: the queued event is
        // what ends the drag promptly, this is what keeps the ending honest
        // when a terminal-class event beats it or the queue drops it.
        if self.active_source_seat.as_ref() == Some(&CallbackIdentity::wayland(&objects.seat)) {
            self.active_source_pointer_lost = Some(OutgoingTerminalReason::SeatRemoved);
        }
        if let Some(pointer) = objects.pointer {
            if pointer.version() >= 3 {
                pointer.release();
            }
            if self
                .held_grab
                .as_ref()
                .is_some_and(|grab| grab.seat == objects.seat)
            {
                self.held_grab = None;
            }
        }
        let seat = CallbackIdentity::wayland(&objects.seat);
        let device = CallbackIdentity::wayland(objects.data_device.inner());
        Some(self.record_device_removal(seat, device))
    }

    fn record_device_removal(
        &self,
        seat: CallbackIdentity,
        device: CallbackIdentity,
    ) -> RemovedSeat {
        let transfer_ids = self.device_transfers.transfer_ids_for(&device);
        RemovedSeat {
            seat,
            device,
            transfer_ids,
        }
    }

    fn callback_action_for(&self, transfer_id: DataTransferId) -> Option<DndAction> {
        self.callback_actions
            .iter()
            .rev()
            .find(|(id, _)| *id == transfer_id)
            .and_then(|(_, action)| *action)
    }

    fn callback_source_actions_for(&self, transfer_id: DataTransferId) -> ActionMask {
        self.callback_source_actions
            .iter()
            .rev()
            .find(|(id, _)| *id == transfer_id)
            .map_or(ActionMask::NONE, |(_, actions)| *actions)
    }

    fn retire_pointer_leave(&mut self, transfer_id: DataTransferId) {
        // A pointer leave invalidates queued positions, not the dropped offer.
        // The offer key survives a post-drop Leave. The device key is distinct:
        // it remains only until a replacement Enter for that device, where SCTK
        // destroys the old offer before this bridge sees the callback.
        self.protocol_queue.discard_motions_for(transfer_id);
    }

    fn retire_transfer_terminal(&mut self, transfer_id: DataTransferId) {
        self.device_transfers.retire(transfer_id);
        self.offer_transfers.retire(transfer_id);
        self.pending_barriers
            .retain(|barrier| barrier.transfer_id != transfer_id);
        self.callback_actions.retain(|(id, _)| *id != transfer_id);
        self.callback_source_actions
            .retain(|(id, _)| *id != transfer_id);
        self.replaced_before_enter.remove(&transfer_id);
    }

    fn capture_leave(&mut self, device: CallbackIdentity) {
        if let Some(event) = self
            .device_transfers
            .capture(&device, |transfer_id| ProtocolEvent::Leave { transfer_id })
        {
            self.enqueue(event);
        }
    }

    fn capture_drop_for_device(&mut self, device: CallbackIdentity) {
        if let Some(transfer_id) = self.device_transfers.transfer_for(&device) {
            self.capture_drop(transfer_id);
        }
    }

    fn capture_motion_for_device(&mut self, device: CallbackIdentity, position: Position) {
        if let Some(transfer_id) = self.device_transfers.transfer_for(&device) {
            let transport_revision = self.next_revision();
            self.enqueue(ProtocolEvent::Motion {
                transfer_id,
                position,
                transport_revision,
            });
        }
    }

    fn capture_source_actions_for_offer(&mut self, offer: CallbackIdentity, actions: ActionMask) {
        if let Some(transfer_id) = self.offer_transfers.transfer_for(&offer) {
            let transport_revision = self.next_revision();
            self.callback_source_actions
                .retain(|(candidate, _)| *candidate != transfer_id);
            self.callback_source_actions.push((transfer_id, actions));
            self.enqueue(ProtocolEvent::SourceActions {
                transfer_id,
                actions,
                transport_revision,
            });
        }
    }

    fn capture_selected_action_for_offer(
        &mut self,
        offer: CallbackIdentity,
        action: Option<DndAction>,
    ) {
        if let Some(transfer_id) = self.offer_transfers.transfer_for(&offer) {
            let transport_revision = self.next_revision();
            self.callback_actions
                .retain(|(candidate, _)| *candidate != transfer_id);
            self.callback_actions.push((transfer_id, action));
            self.enqueue(ProtocolEvent::SelectedAction {
                transfer_id,
                action,
                transport_revision,
            });
        }
    }

    fn capture_barrier_done(&mut self, barrier: &AskBarrier) {
        if !self.pending_barriers.iter().any(|candidate| {
            candidate.transfer_id == barrier.transfer_id
                && candidate.barrier_id == barrier.barrier_id
        }) {
            return;
        }
        let latest = self.callback_action_for(barrier.transfer_id);
        self.pending_barriers.retain(|candidate| {
            candidate.transfer_id != barrier.transfer_id
                || candidate.barrier_id != barrier.barrier_id
        });
        self.enqueue(ProtocolEvent::BarrierDone {
            transfer_id: barrier.transfer_id,
            barrier_id: barrier.barrier_id,
            requested_action: barrier.requested_action,
            selected_action: latest,
        });
    }
}

impl ProvidesRegistryState for TransportState {
    fn registry(&mut self) -> &mut RegistryState {
        self.registry_state
            .as_mut()
            .expect("production transport owns registry state")
    }

    registry_handlers![SeatState];
}

impl ShmHandler for TransportState {
    fn shm_state(&mut self) -> &mut Shm {
        self.icon_shm
            .as_mut()
            .expect("wl_shm events require the optional icon binding")
    }
}

impl SeatHandler for TransportState {
    fn seat_state(&mut self) -> &mut SeatState {
        self.seat_state
            .as_mut()
            .expect("production transport owns seat state")
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        // Seat creation owns a protocol object and is not per-drag lifecycle
        // work. Record it in this callback so drag queue pressure cannot leave
        // a hot-added seat without a data device.
        let already_present = self.data_devices.iter().any(|objects| objects.seat == seat);
        let Some(vacant) = admit_unique_seat(already_present) else {
            return;
        };
        let data_device = create_seat_object(vacant, || {
            self.data_device_manager
                .as_ref()
                .expect("production transport owns a data-device manager")
                .get_data_device(qh, &seat)
        });
        self.data_devices.push(SeatObjects {
            seat,
            data_device,
            pointer: None,
        });
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer {
            return;
        }
        let Some(index) = self
            .data_devices
            .iter()
            .position(|objects| objects.seat == seat)
        else {
            return;
        };
        if self.data_devices[index].pointer.is_some() {
            return;
        }
        let pointer = self.seat_state().get_pointer(qh, &seat).ok();
        self.data_devices[index].pointer = pointer;
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer {
            return;
        }
        if let Some(objects) = self
            .data_devices
            .iter_mut()
            .find(|objects| objects.seat == seat)
            && let Some(pointer) = objects.pointer.take()
        {
            if self
                .held_grab
                .as_ref()
                .is_some_and(|grab| grab.pointer == pointer)
            {
                self.held_grab = None;
            }
            if pointer.version() >= 3 {
                pointer.release();
            }
            // The pointer that holds a live drag's press is gone, and the
            // compositor is not obliged to cancel the source promptly. Ending
            // the drag here is deterministic, and it tells a caller that the
            // press can never be released — `SeatRemoved` already carries that
            // for the whole-seat case. Enqueued rather than terminated inline
            // because this is a callback on the state, not the bridge.
            let seat = CallbackIdentity::wayland(&seat);
            if self.active_source_seat.as_ref() == Some(&seat) {
                self.active_source_pointer_lost =
                    Some(OutgoingTerminalReason::PointerCapabilityLost);
                self.enqueue(ProtocolEvent::PointerCapabilityLost { seat });
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        if let Some(removed) = self.record_seat_removal(&seat) {
            self.enqueue(ProtocolEvent::SeatRemoved(removed));
        }
    }
}

impl PointerHandler for TransportState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let Some(seat) = self
            .data_devices
            .iter()
            .find(|objects| objects.pointer.as_ref() == Some(pointer))
            .map(|objects| objects.seat.clone())
        else {
            return;
        };
        let seat_key = CallbackIdentity::wayland(&seat);
        for event in events {
            match event.kind {
                PointerEventKind::Press {
                    button: BTN_LEFT,
                    serial,
                    ..
                } if self.surface.as_ref() == Some(&event.surface) => {
                    // The first held seat owns the gesture. A second seat cannot
                    // overwrite either its grab token or an active source.
                    let held_seat = self
                        .held_grab
                        .as_ref()
                        .map(|grab| CallbackIdentity::wayland(&grab.seat));
                    if !seat_can_claim_grab(
                        held_seat.as_ref(),
                        self.active_source_seat.as_ref(),
                        &seat_key,
                    ) {
                        continue;
                    }
                    self.held_grab = Some(HeldGrab {
                        seat: seat.clone(),
                        pointer: pointer.clone(),
                        serial,
                        origin: event.surface.clone(),
                        button: BTN_LEFT,
                    });
                }
                PointerEventKind::Release { button, .. }
                    if self
                        .held_grab
                        .as_ref()
                        .is_some_and(|grab| grab.pointer == *pointer && grab.button == button) =>
                {
                    self.held_grab = None;
                }
                _ => {}
            }
        }
    }
}

impl DataDeviceHandler for TransportState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let offer = device
            .data::<DataDeviceData>()
            .and_then(DataDeviceData::drag_offer);
        let offer_proxy = offer.as_ref().map(|offer| offer.inner().clone());
        let backend = offer.map(|offer| {
            Box::new(WaylandOfferBackend {
                offer,
                device: device.clone(),
            }) as Box<dyn OfferBackend>
        });
        let device = CallbackIdentity::wayland(device);
        self.capture_enter(
            device,
            offer_proxy.as_ref().map(CallbackIdentity::wayland),
            backend,
            self.surface.as_ref() == Some(surface),
            Position { x, y },
        );
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        self.capture_leave(CallbackIdentity::wayland(device));
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        self.capture_motion_for_device(CallbackIdentity::wayland(device), Position { x, y });
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        self.capture_drop_for_device(CallbackIdentity::wayland(device));
    }
}

impl DataOfferHandler for TransportState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: WlDndAction,
    ) {
        self.capture_source_actions_for_offer(
            CallbackIdentity::wayland(offer.inner()),
            from_wayland_mask(actions),
        );
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        action: WlDndAction,
    ) {
        self.capture_selected_action_for_offer(
            CallbackIdentity::wayland(offer.inner()),
            from_wayland_action(action),
        );
    }
}

impl DataSourceHandler for TransportState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        _: Option<String>,
    ) {
        if let Some(transfer_id) = self
            .source_transfers
            .transfer_for(&CallbackIdentity::wayland(source))
        {
            self.enqueue(ProtocolEvent::SourceAccepted { transfer_id });
        }
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime_type: String,
        pipe: WritePipe,
    ) {
        if let Some(transfer_id) = self
            .source_transfers
            .transfer_for(&CallbackIdentity::wayland(source))
        {
            self.enqueue(ProtocolEvent::SourceSend {
                transfer_id,
                mime_type,
                pipe,
            });
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let key = CallbackIdentity::wayland(source);
        if let Some(transfer_id) = self.source_transfers.transfer_for(&key) {
            self.enqueue(ProtocolEvent::SourceCancelled { transfer_id });
            self.source_transfers.retire(transfer_id);
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if let Some(transfer_id) = self
            .source_transfers
            .transfer_for(&CallbackIdentity::wayland(source))
        {
            // `dnd_drop_performed` is not terminal: this key deliberately
            // survives so later `send` and `dnd_finished` still correlate.
            self.enqueue(ProtocolEvent::SourceDropped { transfer_id });
        }
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let key = CallbackIdentity::wayland(source);
        if let Some(transfer_id) = self.source_transfers.transfer_for(&key) {
            self.enqueue(ProtocolEvent::SourceFinished { transfer_id });
            self.source_transfers.retire(transfer_id);
        }
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        action: WlDndAction,
    ) {
        if let Some(transfer_id) = self
            .source_transfers
            .transfer_for(&CallbackIdentity::wayland(source))
        {
            self.enqueue(ProtocolEvent::SourceAction {
                transfer_id,
                action: from_wayland_action(action),
            });
        }
    }
}

impl Dispatch<WlCallback, AskBarrier> for TransportState {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        barrier: &AskBarrier,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.capture_barrier_done(barrier);
        }
    }
}

delegate_registry!(TransportState);
delegate_seat!(TransportState);
delegate_pointer!(TransportState);
delegate_data_device!(TransportState);
delegate_shm!(TransportState);
delegate_noop!(TransportState: WlCompositor);
delegate_noop!(TransportState: ignore WlSurface);

#[derive(Debug, PartialEq, Eq)]
enum PayloadReadError {
    /// The bridge cancelled the fetch; the transfer is already terminal.
    Cancelled,
    Failure(PayloadFailure),
}

/// Reads one payload to completion, blocking in `poll(2)` between chunks.
///
/// Three properties this owes the rest of the crate:
///
/// * **No polling.** An idle source costs zero wakeups — the thread sits in
///   `poll` on the payload FD until there is something to read.
/// * **Immediate cancellation.** The cancellation socket is polled alongside
///   the payload, so a terminal transition wakes the worker in the same
///   syscall rather than at the next tick of a timer.
/// * **Bounded in time.** A source that opens the pipe, answers `receive` and
///   then never writes or closes cannot hold the transfer open: `inactivity`
///   measures time since the last byte, not total transfer duration, so a slow
///   but progressing multi-megabyte read is never cut off.
fn read_payload_cancellable(
    pipe: &mut smithay_client_toolkit::data_device_manager::ReadPipe,
    max_bytes: usize,
    cancel: &PayloadCancelWorker,
    inactivity: Duration,
) -> Result<Vec<u8>, PayloadReadError> {
    let payload_fd = pipe.as_raw_fd();
    let wake_fd = cancel.wake.as_raw_fd();
    // SAFETY: `payload_fd` is owned by `pipe` for this function's duration.
    let flags = unsafe { libc::fcntl(payload_fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(PayloadReadError::Failure(PayloadFailure::Pipe));
    }
    // SAFETY: the fd is valid and F_SETFL only changes its status flags.
    if unsafe { libc::fcntl(payload_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(PayloadReadError::Failure(PayloadFailure::Pipe));
    }

    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut last_progress = Instant::now();
    loop {
        if cancel.flag.load(Ordering::Acquire) {
            return Err(PayloadReadError::Cancelled);
        }
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                if bytes.len().saturating_add(read) > max_bytes {
                    return Err(PayloadReadError::Failure(PayloadFailure::TooLarge));
                }
                bytes.extend_from_slice(&chunk[..read]);
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let elapsed = last_progress.elapsed();
                if elapsed >= inactivity {
                    return Err(PayloadReadError::Failure(PayloadFailure::Inactive));
                }
                // `as_millis` truncates, so a sub-millisecond remainder would
                // become a zero-timeout poll. Floor it at 1ms: the poll timeout
                // is an approximation of the window, never the authority on it.
                // Expiry is decided in one place — the `elapsed >= inactivity`
                // check above — which is why a timed-out poll loops back rather
                // than rejecting here. A zero timeout would otherwise reject a
                // read whose configured window had not actually elapsed, and
                // the 1ms floor is what bounds that loop against spinning.
                // Asserted by construction rather than by a test: the only
                // observable difference is whether a source writing inside a
                // sub-millisecond window is seen, and no scheduler guarantees
                // timing at that scale. A test here would pass either way.
                let remaining = (inactivity - elapsed)
                    .as_millis()
                    .min(i32::MAX as u128)
                    .max(1) as i32;
                let mut fds = [
                    libc::pollfd {
                        fd: payload_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: wake_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: both fds are owned for this function's duration and
                // the array length matches the count passed.
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, remaining) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(PayloadReadError::Failure(PayloadFailure::Pipe));
                }
                if ready == 0 {
                    // A timed-out poll is not itself expiry: re-read, then let
                    // the single `elapsed >= inactivity` check above decide.
                    continue;
                }
                if fds[1].revents != 0 {
                    return Err(PayloadReadError::Cancelled);
                }
            }
            Err(_) => return Err(PayloadReadError::Failure(PayloadFailure::Pipe)),
        }
    }
    Ok(bytes)
}

/// Writes one lazy source payload without blocking the Wayland event thread.
///
/// The worker polls the destination pipe and a cancellation socket together.
/// Dropping `pipe` is intentionally outside this helper: the caller closes it
/// before publishing the result, making EOF part of the observable success
/// condition rather than eventual cleanup.
fn write_payload_cancellable(
    pipe: &mut WritePipe,
    bytes: &[u8],
    cancel: &SendCancelWorker,
    inactivity: Duration,
) -> Result<(), ()> {
    let payload_fd = pipe.as_raw_fd();
    let wake_fd = cancel.wake.as_raw_fd();
    // SAFETY: `payload_fd` is owned by `pipe` for this function's duration.
    let flags = unsafe { libc::fcntl(payload_fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(());
    }
    // SAFETY: the fd is valid and F_SETFL only changes status flags.
    if unsafe { libc::fcntl(payload_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(());
    }

    let mut written = 0;
    let mut last_progress = Instant::now();
    while written < bytes.len() {
        if cancel.flag.load(Ordering::Acquire) {
            return Err(());
        }
        match pipe.write(&bytes[written..]) {
            Ok(0) => return Err(()),
            Ok(count) => {
                written += count;
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let elapsed = last_progress.elapsed();
                if elapsed >= inactivity {
                    return Err(());
                }
                let remaining = (inactivity - elapsed)
                    .as_millis()
                    .min(i32::MAX as u128)
                    .max(1) as i32;
                let mut fds = [
                    libc::pollfd {
                        fd: payload_fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: wake_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: both fds remain owned for this call and the array
                // length matches the count.
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, remaining) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(());
                }
                if ready == 0 {
                    continue;
                }
                if fds[1].revents != 0 {
                    return Err(());
                }
                if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(());
                }
            }
            Err(_) => return Err(()),
        }
    }
    pipe.flush().map_err(|_| ())
}

fn write_payload_and_close(
    mut pipe: WritePipe,
    bytes: &[u8],
    cancel: &SendCancelWorker,
    inactivity: Duration,
) -> Result<(), ()> {
    let result = write_payload_cancellable(&mut pipe, bytes, cancel, inactivity);
    // EOF is the source-side commit. Close before returning, so a worker cannot
    // publish either success or failure while another process still waits.
    drop(pipe);
    result
}

/// Sends a request on the live dropped offer after `wl_data_device.leave`.
///
/// This is not a protocol deviation. The `drop` contract requires the
/// destination to keep using the offer for `receive`, final acceptance, a final
/// non-Ask `set_actions`, and `finish`. SCTK 0.19.2's
/// `DataDeviceOffer::leave` preserves a dropped offer (`data_offer.rs:315`),
/// but both `DragOffer::accept_mime_type` and `DragOffer::set_actions` suppress
/// their request when `left` is true (`data_offer.rs:126` and `:105`). Keeping
/// both bypasses here gives them the same protocol-version and point-of-request
/// `Proxy::is_alive` check.
fn send_retained_offer_request(offer: &DragOffer, request: RetainedOfferRequest) -> bool {
    send_retained_proxy_request(offer.inner(), offer.serial, request)
}

fn send_retained_proxy_request(
    proxy: &impl RetainedOfferProxy,
    serial: u32,
    request: RetainedOfferRequest,
) -> bool {
    if proxy.version() < 3 || !proxy.is_alive() {
        return false;
    }
    match request {
        RetainedOfferRequest::Accept(mime) => proxy.accept(serial, mime),
        RetainedOfferRequest::FinalActions { allowed, preferred } => {
            proxy.set_actions(to_wayland_mask(allowed), to_wayland_action(preferred));
        }
    }
    true
}

fn reject_offer_backend(backend: Box<dyn OfferBackend>) {
    let gate = OfferRequestGate::default();
    reject_offer_requests(|kind| {
        let _ = gate.send(kind, || match kind {
            OfferRequestKind::Accept => {
                let _ = backend.accept_mime(None);
            }
            OfferRequestKind::Destroy => backend.destroy(),
            _ => unreachable!("unowned rejection only accepts and destroys"),
        });
    });
}

fn reject_offer_requests(mut send: impl FnMut(OfferRequestKind)) {
    send(OfferRequestKind::Accept);
    send(OfferRequestKind::Destroy);
}

fn choose_offered_mime(
    offered: &[(String, MimeType)],
    requested: &str,
) -> Option<(String, MimeType)> {
    let requested = requested.parse::<MimeType>().ok()?;
    offered
        .iter()
        .find(|(_, mime)| *mime == requested || (mime.is_utf8_text() && requested.is_utf8_text()))
        .cloned()
}

fn to_wayland_action(action: DndAction) -> WlDndAction {
    match action {
        DndAction::Copy => WlDndAction::Copy,
        DndAction::Move => WlDndAction::Move,
        DndAction::Ask => WlDndAction::Ask,
    }
}

fn to_wayland_mask(mask: ActionMask) -> WlDndAction {
    let mut actions = WlDndAction::empty();
    if mask.contains(DndAction::Copy) {
        actions |= WlDndAction::Copy;
    }
    if mask.contains(DndAction::Move) {
        actions |= WlDndAction::Move;
    }
    if mask.contains(DndAction::Ask) {
        actions |= WlDndAction::Ask;
    }
    actions
}

fn from_wayland_action(action: WlDndAction) -> Option<DndAction> {
    match action {
        WlDndAction::Copy => Some(DndAction::Copy),
        WlDndAction::Move => Some(DndAction::Move),
        WlDndAction::Ask => Some(DndAction::Ask),
        _ => None,
    }
}

fn from_wayland_mask(mask: WlDndAction) -> ActionMask {
    let mut actions = ActionMask::NONE;
    if mask.contains(WlDndAction::Copy) {
        actions |= ActionMask::COPY;
    }
    if mask.contains(WlDndAction::Move) {
        actions |= ActionMask::MOVE;
    }
    if mask.contains(WlDndAction::Ask) {
        actions |= ActionMask::ASK;
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// Wall-clock bound for spins that wait on a real payload worker thread.
    ///
    /// These waits must be bounded by TIME, never by a fixed iteration count. A
    /// spin of N `yield_now`s is load-dependent: in release under the parallel
    /// suite, 1_000 yields elapse in microseconds and the worker thread may not
    /// have been scheduled even once, so the bound expires before the work it
    /// is waiting for can possibly happen. That made the release suite fail
    /// roughly two runs in three while debug stayed green — a gate that fails
    /// randomly hides the next real regression in its own noise.
    const TEST_WORKER_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestWireRequest {
        Accept(u64, u32, Option<String>),
        Actions(u64, ActionMask, Option<DndAction>),
        Receive(u64, String),
        Finish(u64),
        Destroy(u64),
    }

    #[derive(Default)]
    struct TestOfferLog {
        wire: Vec<TestWireRequest>,
        accepted: Vec<(u64, Option<String>)>,
        actions: Vec<(u64, ActionMask, Option<DndAction>)>,
        received: Vec<(u64, String)>,
        final_actions: Vec<(u64, ActionMask, DndAction)>,
        destroyed: Vec<u64>,
        finished: Vec<u64>,
    }

    struct TestProxyState {
        alive: Cell<bool>,
        version: Cell<u32>,
        serial: u32,
        left: Cell<bool>,
        dropped: Cell<bool>,
        x: Cell<f64>,
        y: Cell<f64>,
        time: Cell<Option<u32>>,
        source_actions: Cell<ActionMask>,
        offered_mimes: RefCell<Vec<String>>,
        receive_failure: Cell<Option<std::io::ErrorKind>>,
        hold_receive_open: Cell<bool>,
        open_writers: RefCell<Vec<UnixStream>>,
    }

    #[derive(Clone)]
    struct TestOfferProxy {
        id: u64,
        state: Rc<TestProxyState>,
        log: Rc<RefCell<TestOfferLog>>,
    }

    impl TestOfferProxy {
        fn new(id: u64, source_actions: ActionMask, log: &Rc<RefCell<TestOfferLog>>) -> Self {
            Self {
                id,
                state: Rc::new(TestProxyState {
                    alive: Cell::new(true),
                    version: Cell::new(3),
                    serial: id as u32,
                    left: Cell::new(false),
                    dropped: Cell::new(false),
                    x: Cell::new(0.0),
                    y: Cell::new(0.0),
                    time: Cell::new(None),
                    source_actions: Cell::new(source_actions),
                    offered_mimes: RefCell::new(vec!["text/plain".into()]),
                    receive_failure: Cell::new(None),
                    hold_receive_open: Cell::new(false),
                    open_writers: RefCell::new(Vec::new()),
                }),
                log: Rc::clone(log),
            }
        }

        fn accept(&self, serial: u32, mime: Option<String>) -> bool {
            // SCTK 0.19.2 data_offer.rs:126-129 suppresses accept after leave
            // and forwards the caller's serial unchanged.
            if self.state.left.get() {
                return true;
            }
            if !self.state.alive.get() {
                return false;
            }
            self.raw_accept(serial, mime);
            true
        }

        fn raw_accept(&self, serial: u32, mime: Option<String>) {
            // SCTK 0.19.2 data_offer.rs:126-129 forwards this exact serial/MIME
            // pair when its wrapper guard admits the request.
            let mut log = self.log.borrow_mut();
            log.wire
                .push(TestWireRequest::Accept(self.id, serial, mime.clone()));
            log.accepted.push((self.id, mime));
        }

        fn actions(&self, allowed: ActionMask, preferred: Option<DndAction>) -> bool {
            // SCTK 0.19.2 data_offer.rs:105-108 sends only on v3+ before leave.
            if self.state.version.get() < 3 || self.state.left.get() {
                return true;
            }
            if !self.state.alive.get() {
                return false;
            }
            self.raw_actions(allowed, preferred);
            true
        }

        fn raw_actions(&self, allowed: ActionMask, preferred: Option<DndAction>) {
            // SCTK 0.19.2 data_offer.rs:105-108 forwards one set_actions request
            // with the allowed and preferred values after its wrapper guards.
            let mut log = self.log.borrow_mut();
            log.wire
                .push(TestWireRequest::Actions(self.id, allowed, preferred));
            log.actions.push((self.id, allowed, preferred));
        }

        fn final_actions(&self, allowed: ActionMask, preferred: DndAction) {
            // SCTK 0.19.2 data_offer.rs:105-108 identifies this as the same
            // wl_data_offer.set_actions request; only its left guard is bypassed.
            if !self.state.alive.get() {
                return;
            }
            self.raw_actions(allowed, Some(preferred));
            self.log
                .borrow_mut()
                .final_actions
                .push((self.id, allowed, preferred));
        }

        fn finish(&self) -> bool {
            // SCTK 0.19.2 data_offer.rs:89-92 gates finish on version, not its
            // local left/dropped/alive flags.
            if self.state.version.get() < 3 {
                return false;
            }
            let mut log = self.log.borrow_mut();
            log.wire.push(TestWireRequest::Finish(self.id));
            log.finished.push(self.id);
            true
        }

        fn destroy(&self) {
            // SCTK 0.19.2 data_offer.rs:132-135 forwards every destroy call;
            // protocol misuse is not made locally idempotent by this model.
            self.state.alive.set(false);
            let mut log = self.log.borrow_mut();
            log.wire.push(TestWireRequest::Destroy(self.id));
            log.destroyed.push(self.id);
        }

        fn receive(
            &self,
            mime: String,
        ) -> std::io::Result<smithay_client_toolkit::data_device_manager::ReadPipe> {
            // SCTK 0.19.2 data_offer.rs:114-120 rejects a left offer unless the
            // data-device Drop callback marked it dropped first.
            if self.state.left.get() && !self.state.dropped.get() {
                return Err(std::io::Error::other("offer has left"));
            }
            if !self.state.alive.get() {
                return Err(std::io::Error::from(std::io::ErrorKind::NotConnected));
            }
            // SCTK 0.19.2 data_offer.rs:438-445 can fail while creating the
            // receive pipe, before any wl_data_offer.receive request is sent.
            if let Some(kind) = self.state.receive_failure.get() {
                return Err(std::io::Error::from(kind));
            }
            let (reader, mut writer) = UnixStream::pair()?;
            let mut log = self.log.borrow_mut();
            log.wire
                .push(TestWireRequest::Receive(self.id, mime.clone()));
            log.received.push((self.id, mime));
            drop(log);
            if self.state.hold_receive_open.get() {
                self.state.open_writers.borrow_mut().push(writer);
            } else {
                writer.write_all(b"payload")?;
            }
            Ok(read_pipe(reader))
        }
    }

    impl RetainedOfferProxy for TestOfferProxy {
        fn version(&self) -> u32 {
            self.state.version.get()
        }

        fn is_alive(&self) -> bool {
            self.state.alive.get()
        }

        fn accept(&self, serial: u32, mime: Option<String>) {
            self.raw_accept(serial, mime);
        }

        fn set_actions(&self, allowed: WlDndAction, preferred: WlDndAction) {
            self.final_actions(
                from_wayland_mask(allowed),
                from_wayland_action(preferred).expect("test final action is non-null"),
            );
        }
    }

    struct TestOfferBackend {
        device_id: u64,
        proxy: TestOfferProxy,
    }

    impl OfferBackend for TestOfferBackend {
        fn offered_mimes(&self) -> Vec<(String, MimeType)> {
            // SCTK 0.19.2 data_offer.rs:95-100 exposes the offer's recorded MIME
            // list rather than a fixed fixture MIME.
            self.proxy
                .state
                .offered_mimes
                .borrow()
                .iter()
                .filter_map(|raw| raw.parse().ok().map(|parsed| (raw.clone(), parsed)))
                .collect()
        }

        fn source_actions(&self) -> ActionMask {
            self.proxy.state.source_actions.get()
        }

        fn device_identity(&self) -> CallbackIdentity {
            CallbackIdentity::test(self.device_id)
        }

        fn is_alive(&self) -> bool {
            self.proxy.state.alive.get()
        }

        fn accept_mime(&self, mime: Option<String>) -> bool {
            self.proxy.accept(self.proxy.state.serial, mime)
        }

        fn set_actions(&self, allowed: ActionMask, preferred: Option<DndAction>) -> bool {
            self.proxy.actions(allowed, preferred)
        }

        fn receive(
            &self,
            mime: String,
        ) -> std::io::Result<smithay_client_toolkit::data_device_manager::ReadPipe> {
            self.proxy.receive(mime)
        }

        fn send_retained_request(&self, request: RetainedOfferRequest) -> bool {
            send_retained_proxy_request(&self.proxy, self.proxy.state.serial, request)
        }

        fn finish(&self) -> bool {
            self.proxy.finish()
        }

        fn destroy(&self) {
            self.proxy.destroy();
        }

        fn test_id(&self) -> Option<u64> {
            Some(self.proxy.id)
        }
    }

    /// Behavioural model of SCTK 0.19.2's drag-offer adapter only.
    ///
    /// Every SCTK bump must re-read this model against
    /// `data_device_manager/{data_device,data_offer}.rs`; it is intentionally
    /// not a general Wayland compositor fake.
    struct TestProxyLayer {
        log: Rc<RefCell<TestOfferLog>>,
        current: RefCell<Vec<(u64, TestOfferProxy)>>,
        motion_time: Cell<u32>,
    }

    impl TestProxyLayer {
        fn new(log: &Rc<RefCell<TestOfferLog>>) -> Self {
            Self {
                log: Rc::clone(log),
                current: RefCell::new(Vec::new()),
                motion_time: Cell::new(0),
            }
        }

        fn enter(
            &self,
            state: &mut TransportState,
            device_id: u64,
            offer_id: u64,
            owned_surface: bool,
            source_actions: ActionMask,
        ) -> (DataTransferId, TestOfferProxy) {
            // SCTK 0.19.2 data_device.rs:137-151 destroys the previous drag offer
            // unconditionally before invoking DataDeviceHandler::enter.
            let mut current = self.current.borrow_mut();
            if let Some(index) = current
                .iter()
                .position(|(candidate, _)| *candidate == device_id)
            {
                current.remove(index).1.destroy();
            }
            let proxy = TestOfferProxy::new(offer_id, source_actions, &self.log);
            current.push((device_id, proxy.clone()));
            drop(current);
            let id = state.capture_enter(
                CallbackIdentity::test(device_id),
                Some(CallbackIdentity::test(offer_id)),
                Some(Box::new(TestOfferBackend {
                    device_id,
                    proxy: proxy.clone(),
                })),
                owned_surface,
                Position { x: 0.0, y: 0.0 },
            );
            (id, proxy)
        }

        fn enter_without_offer(
            &self,
            state: &mut TransportState,
            device_id: u64,
            owned_surface: bool,
        ) -> DataTransferId {
            // SCTK 0.19.2 data_device.rs:137-155 calls the handler even when
            // Enter carries no wl_data_offer, after destroying any prior offer.
            let mut current = self.current.borrow_mut();
            if let Some(index) = current
                .iter()
                .position(|(candidate, _)| *candidate == device_id)
            {
                current.remove(index).1.destroy();
            }
            drop(current);
            state.capture_enter(
                CallbackIdentity::test(device_id),
                None,
                None,
                owned_surface,
                Position { x: 0.0, y: 0.0 },
            )
        }

        fn source_actions(
            &self,
            state: &mut TransportState,
            proxy: &TestOfferProxy,
            actions: ActionMask,
        ) {
            // SCTK 0.19.2 data_offer.rs:378-387 stores the mask before calling
            // DataOfferHandler::source_actions.
            proxy.state.source_actions.set(actions);
            state.capture_source_actions_for_offer(CallbackIdentity::test(proxy.id), actions);
        }

        fn selected_action(
            &self,
            state: &mut TransportState,
            proxy: &TestOfferProxy,
            action: Option<DndAction>,
        ) {
            // SCTK 0.19.2 data_offer.rs:393-400 stores the selected action before
            // calling DataOfferHandler::selected_action.
            state.capture_selected_action_for_offer(CallbackIdentity::test(proxy.id), action);
        }

        fn drop(&self, state: &mut TransportState, device_id: u64) {
            // SCTK 0.19.2 data_device.rs:181-197 marks the offer dropped before
            // invoking DataDeviceHandler::drop_performed.
            if let Some((_, proxy)) = self
                .current
                .borrow()
                .iter()
                .find(|(candidate, _)| *candidate == device_id)
            {
                proxy.state.dropped.set(true);
            }
            state.capture_drop_for_device(CallbackIdentity::test(device_id));
        }

        fn motion(&self, state: &mut TransportState, device_id: u64, position: Position) {
            // SCTK 0.19.2 data_device.rs:169-179 and data_offer.rs:295-302 update
            // coordinates/time before invoking DataDeviceHandler::motion.
            let time = self.motion_time.get().saturating_add(1);
            self.motion_time.set(time);
            if let Some((_, proxy)) = self
                .current
                .borrow()
                .iter()
                .find(|(candidate, _)| *candidate == device_id)
            {
                proxy.state.x.set(position.x);
                proxy.state.y.set(position.y);
                proxy.state.time.set(Some(time));
            }
            state.capture_motion_for_device(CallbackIdentity::test(device_id), position);
        }

        fn leave(&self, state: &mut TransportState, device_id: u64) {
            // SCTK 0.19.2 data_device.rs:157-167 and data_offer.rs:315-324 set
            // left, destroy/remove an undropped offer, and retain a dropped one
            // before invoking DataDeviceHandler::leave.
            let mut current = self.current.borrow_mut();
            if let Some(index) = current
                .iter()
                .position(|(candidate, _)| *candidate == device_id)
            {
                let proxy = current[index].1.clone();
                proxy.state.left.set(true);
                if !proxy.state.dropped.get() {
                    proxy.destroy();
                    current.remove(index);
                }
            }
            drop(current);
            state.capture_leave(CallbackIdentity::test(device_id));
        }
    }

    fn captured_enter(
        transfer_id: DataTransferId,
        offer_id: u64,
        owned_surface: bool,
        log: &Rc<RefCell<TestOfferLog>>,
    ) -> ProtocolEvent {
        let proxy = TestOfferProxy::new(offer_id, ActionMask::COPY | ActionMask::MOVE, log);
        ProtocolEvent::Enter {
            transfer_id,
            backend: Some(Box::new(TestOfferBackend {
                device_id: offer_id,
                proxy,
            })),
            owned_surface,
            position: Position { x: 0.0, y: 0.0 },
            transport_revision: TransportRevision(transfer_id.0),
        }
    }

    fn capture_test_enter(
        state: &mut TransportState,
        offer_id: u64,
        owned_surface: bool,
        log: &Rc<RefCell<TestOfferLog>>,
    ) -> DataTransferId {
        capture_test_enter_with_actions(
            state,
            offer_id,
            owned_surface,
            ActionMask::COPY | ActionMask::MOVE,
            log,
        )
    }

    fn capture_test_enter_with_actions(
        state: &mut TransportState,
        offer_id: u64,
        owned_surface: bool,
        source_actions: ActionMask,
        log: &Rc<RefCell<TestOfferLog>>,
    ) -> DataTransferId {
        TestProxyLayer::new(log)
            .enter(state, offer_id, offer_id, owned_surface, source_actions)
            .0
    }

    fn acceptance(id: DataTransferId, target: u64, action: DndAction, observed: u64) -> Acceptance {
        Acceptance {
            mime_type: "text/plain".into(),
            allowed_actions: match action {
                DndAction::Copy => ActionMask::COPY,
                DndAction::Move => ActionMask::MOVE,
                DndAction::Ask => ActionMask::ASK,
            },
            preferred: action,
            context: crate::types::AcceptedContext {
                target: TargetId(target),
                action,
                modifiers: crate::types::Modifiers::default(),
                origin: crate::types::DndOrigin::External(id),
                delivery_id: crate::types::DeliveryId(9),
                revision: ProposalRevision(observed),
            },
            observed_transport_revision: TransportRevision(observed),
        }
    }

    fn active_offer_id(bridge: &WaylandBridge) -> Option<(DataTransferId, u64)> {
        let active = bridge.active.as_ref()?;
        Some((active.transfer.id(), active.backend.as_deref()?.test_id()?))
    }

    #[test]
    fn text_plain_forms_match_both_ways() {
        let offered_plain = vec![(
            "text/plain".into(),
            "text/plain".parse::<MimeType>().unwrap(),
        )];
        assert_eq!(
            choose_offered_mime(&offered_plain, "text/plain;charset=utf-8")
                .unwrap()
                .0,
            "text/plain"
        );

        let offered_utf8 = vec![(
            "text/plain;charset=utf-8".into(),
            "text/plain;charset=utf-8".parse::<MimeType>().unwrap(),
        )];
        assert_eq!(
            choose_offered_mime(&offered_utf8, "text/plain").unwrap().0,
            "text/plain;charset=utf-8"
        );
    }

    #[test]
    fn test_proxy_layer_mirrors_sctk_leave_drop_and_request_guards() {
        // SCTK 0.19.2 data_device.rs:157-197 and data_offer.rs:105-134.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();
        assert!(log.borrow().wire.contains(&TestWireRequest::Accept(
            101,
            101,
            Some("text/plain".into())
        )));

        layer.leave(&mut bridge.state, 11);
        assert!(proxy.state.left.get());
        assert!(!proxy.state.dropped.get());
        assert!(!proxy.state.alive.get());
        assert_eq!(log.borrow().destroyed, vec![101]);
        assert!(proxy.accept(999, Some("text/plain".into())));
        assert!(proxy.actions(ActionMask::COPY, Some(DndAction::Copy)));
        assert_eq!(log.borrow().destroyed, vec![101]);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert_eq!(
            log.borrow().destroyed,
            vec![101],
            "bridge cleanup does not double-destroy SCTK's left offer"
        );

        let dropped_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let dropped_layer = TestProxyLayer::new(&dropped_log);
        let mut dropped_bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (_, dropped_proxy) =
            dropped_layer.enter(&mut dropped_bridge.state, 22, 202, true, ActionMask::COPY);
        dropped_layer.drop(&mut dropped_bridge.state, 22);
        dropped_layer.leave(&mut dropped_bridge.state, 22);
        assert!(dropped_proxy.state.dropped.get());
        assert!(dropped_proxy.state.left.get());
        assert!(dropped_proxy.state.alive.get());
        assert!(dropped_proxy.receive("text/plain".into()).is_ok());
        assert!(dropped_log.borrow().destroyed.is_empty());
    }

    /// A second pointer-capable seat makes the held grab unattributable: Bevy
    /// reports one logical mouse whichever seat pressed, so escalating could
    /// run the drag on the seat that grabbed first while consuming the other
    /// seat's gesture. The refusal itself needs live `wl_seat` objects, so this
    /// pins the rule at the only layer a unit test can reach.
    /// The queued lifecycle event is not enough on its own: a `SourceCancelled`
    /// enqueued in the same dispatch is terminal-class and drains first, and a
    /// saturated lifecycle queue drops the event outright. Either way the drag
    /// ends under a reason that says the pointer is still there. A consumer
    /// holding button-release-gated state acts on that reason, so the recorded
    /// loss has to outrank it.
    #[test]
    fn a_lost_pointer_outranks_the_reason_that_reached_the_termination() {
        use OutgoingTerminalReason as Reason;

        for masking in [
            Reason::CompositorCancelled,
            Reason::ActiveDeadlineExpired,
            Reason::FinishDeadlineExpired,
            Reason::SourceProxyDead,
        ] {
            assert_eq!(
                outgoing_terminal_reason(masking, Some(Reason::SeatRemoved)),
                Reason::SeatRemoved
            );
            assert_eq!(
                outgoing_terminal_reason(masking, Some(Reason::PointerCapabilityLost)),
                Reason::PointerCapabilityLost
            );
            // With no loss recorded the reason is passed through untouched.
            assert_eq!(outgoing_terminal_reason(masking, None), masking);
        }
        // A delivered drop keeps its name: losing the pointer afterwards does
        // not unmake the drop, and `Completed` is the more useful fact.
        assert_eq!(
            outgoing_terminal_reason(Reason::Completed, Some(Reason::SeatRemoved)),
            Reason::Completed
        );
    }

    #[test]
    fn only_a_single_pointer_capable_seat_makes_a_grab_attributable() {
        assert!(grab_is_attributable(true, 1));
        assert!(!grab_is_attributable(true, 0));
        assert!(!grab_is_attributable(true, 2));
        assert!(!grab_is_attributable(false, 1));
    }

    #[test]
    fn icon_offset_requires_wl_surface_v5() {
        assert_eq!(supported_icon_offset(4, (12, 12)), None);
        assert_eq!(supported_icon_offset(5, (12, 12)), Some((12, 12)));
        assert_eq!(supported_icon_offset(6, (-3, 7)), Some((-3, 7)));
    }

    #[test]
    fn icon_globals_are_an_additive_all_or_nothing_capability() {
        assert!(export_icons_available(true, true));
        assert!(!export_icons_available(true, false));
        assert!(!export_icons_available(false, true));
        assert!(!export_icons_available(false, false));
    }

    #[test]
    fn missing_icon_globals_leave_incoming_dnd_live() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        assert!(!bridge.export_icons_available());
        assert!(bridge.state.icon_compositor.is_none());
        assert!(bridge.state.icon_shm.is_none());

        let icon = OutgoingIcon::new(vec![0; 4], 1, 1, 1, (0, 0)).unwrap();
        assert!(bridge.create_drag_icon(Some(icon)).unwrap().is_none());
        assert!(bridge.state.icon_compositor.is_none());
        assert!(bridge.state.icon_shm.is_none());

        let (id, _) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Entered { transfer_id, .. }] if *transfer_id == id
        ));
    }

    #[derive(Clone)]
    struct TestIconSurface(Rc<Cell<usize>>);

    impl DestroyIconSurface for TestIconSurface {
        fn destroy_icon_surface(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Every production exit either calls `destroy_drag_icon` directly or, if
    /// construction unwinds before ownership is installed, reaches the same
    /// `DragIconSurface` Drop. The concrete proxy cannot be built without a
    /// compositor, so this exercises that shared owner rather than faking one.
    #[test]
    fn drag_icon_surface_is_destroyed_exactly_once_on_every_exit_shape() {
        enum Exit {
            StartFailure,
            OutgoingTerminal,
            WindowTeardown,
            BridgeDrop,
        }

        for exit in [
            Exit::StartFailure,
            Exit::OutgoingTerminal,
            Exit::WindowTeardown,
            Exit::BridgeDrop,
        ] {
            let destroys = Rc::new(Cell::new(0));
            let mut icon = Some(DragIconSurface {
                surface: TestIconSurface(Rc::clone(&destroys)),
                buffer: (),
                _pool: (),
            });
            match exit {
                Exit::StartFailure | Exit::OutgoingTerminal | Exit::WindowTeardown => {
                    destroy_drag_icon(&mut icon);
                    // Re-entrant terminal observations are harmless.
                    destroy_drag_icon(&mut icon);
                }
                Exit::BridgeDrop => drop(icon.take()),
            }
            drop(icon);
            assert_eq!(destroys.get(), 1);
        }
    }

    /// A nonce this process has never issued belongs to a *different* cosmix
    /// process — a second filemgr is the case that matters. Rejecting it broke
    /// filemgr-to-filemgr entirely while buying nothing: the sender could reach
    /// the same `External` origin by simply not offering the private MIME.
    #[test]
    fn a_foreign_processes_nonce_enters_as_an_ordinary_external_offer() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        *proxy.state.offered_mimes.borrow_mut() = vec![
            "text/uri-list".into(),
            // Well-formed, and emphatically not ours: nothing is registered.
            format!(
                "{}ffffffffffffffffffffffffffffffff",
                crate::send::NONCE_MIME_PREFIX
            ),
        ];
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Entered { transfer_id, .. }] if *transfer_id == id
        ));
        assert_eq!(
            bridge.active.as_ref().map(|active| active.origin),
            Some(DndOrigin::External(id))
        );
    }

    /// The other half of the same branch: our *own* retired nonce coming back is
    /// a late echo of a drag that has ended, and still fails closed.
    #[test]
    fn a_tombstoned_nonce_is_still_rejected_without_an_outgoing_transfer() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let nonce = TransferNonce::random().unwrap();
        bridge
            .nonce_registry
            .register(nonce.clone(), DataTransferId(9), SourceId(1))
            .unwrap();
        // No echo was ever attached, so the outgoing terminal retires the entry
        // immediately and leaves a tombstone behind.
        bridge.nonce_registry.outgoing_terminal(DataTransferId(9));
        assert!(matches!(
            bridge.nonce_registry.lookup(&nonce),
            Err(NonceLookupError::Tombstoned)
        ));

        let (_, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        *proxy.state.offered_mimes.borrow_mut() = vec!["text/uri-list".into(), nonce.mime_type()];
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(
            events.is_empty(),
            "a late echo must not surface as an entry"
        );
        assert!(bridge.active.is_none());
    }

    #[test]
    fn test_proxy_layer_models_receive_failures_motion_mimes_and_no_offer_enter() {
        // SCTK 0.19.2 data_device.rs:137-179 and data_offer.rs:95-120,295-302,438-445.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        *proxy.state.offered_mimes.borrow_mut() = vec!["text/uri-list".into()];
        layer.motion(&mut bridge.state, 11, Position { x: 4.0, y: 9.0 });
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                BridgeEvent::Entered { mime_types, .. },
                BridgeEvent::Motion {
                    position: Position { x: 4.0, y: 9.0 },
                    ..
                }
            ] if mime_types[0].raw == "text/uri-list"
        ));
        assert_eq!((proxy.state.x.get(), proxy.state.y.get()), (4.0, 9.0));
        assert_eq!(proxy.state.time.get(), Some(1));

        proxy
            .state
            .receive_failure
            .set(Some(std::io::ErrorKind::OutOfMemory));
        assert_eq!(
            proxy.receive("text/uri-list".into()).unwrap_err().kind(),
            std::io::ErrorKind::OutOfMemory
        );
        assert!(log.borrow().received.is_empty());
        bridge.reject(id).unwrap();
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        let no_offer = layer.enter_without_offer(&mut bridge.state, 22, true);
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty()
        );
        assert!(bridge.active.is_none());
        assert_eq!(
            bridge
                .state
                .device_transfers
                .transfer_for(&CallbackIdentity::test(22)),
            None,
            "an offer-less Enter is retired when drained"
        );
        assert_eq!(no_offer, DataTransferId(2));
    }

    #[test]
    fn test_proxy_layer_finish_and_destroy_match_sctk_request_gates() {
        // SCTK 0.19.2 data_offer.rs:89-92,132-135.
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let proxy = TestOfferProxy::new(101, ActionMask::COPY, &log);
        proxy.state.version.set(2);
        assert!(!proxy.finish());
        assert!(log.borrow().finished.is_empty());
        proxy.state.version.set(3);
        proxy.state.alive.set(false);
        assert!(proxy.finish(), "finish is version-gated, not flag-gated");
        proxy.destroy();
        proxy.destroy();
        assert_eq!(log.borrow().destroyed, vec![101, 101]);
    }

    #[test]
    fn a_final_action_absent_from_the_source_mask_is_not_offered() {
        // wayland.xml wl_data_offer.set_actions: a final Ask resolution absent
        // from wl_data_offer.source_actions raises a protocol error.
        let source_actions = ActionMask::COPY | ActionMask::ASK;
        assert_eq!(
            validate_final_action_offered(source_actions, DndAction::Move),
            Err(AcceptanceError::FinalActionNotOffered {
                action: DndAction::Move,
                source_actions,
            })
        );
        assert_eq!(
            validate_final_action_offered(source_actions, DndAction::Copy),
            Ok(())
        );
    }

    fn cancel_pair() -> (PayloadCancel, PayloadCancelWorker) {
        let (waker, wake) = UnixStream::pair().unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        (
            PayloadCancel {
                flag: Arc::clone(&flag),
                waker,
            },
            PayloadCancelWorker { flag, wake },
        )
    }

    fn read_pipe(reader: UnixStream) -> smithay_client_toolkit::data_device_manager::ReadPipe {
        smithay_client_toolkit::data_device_manager::ReadPipe::from(OwnedFd::from(reader))
    }

    #[test]
    fn terminal_cancellation_stops_an_open_payload_pipe() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig {
            payload_inactivity: Duration::from_secs(3600),
            ..BridgeConfig::default()
        });
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        proxy.state.hold_receive_open.set(true);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        assert_eq!(bridge.live_workers, BTreeSet::from([id]));

        bridge.reject(id).unwrap();
        let terminal = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            terminal.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::OfferRejected,
                ..
            })] if *transfer_id == id
        ));

        let spin_started = Instant::now();
        while !bridge.live_workers.is_empty() {
            assert!(
                spin_started.elapsed() < TEST_WORKER_TIMEOUT,
                "cancelled payload worker did not retire within {TEST_WORKER_TIMEOUT:?}"
            );
            thread::yield_now();
            assert!(
                bridge
                    .run_frame(now, |_| Ok(FlushStatus::Flushed))
                    .unwrap()
                    .is_empty(),
                "cancelled worker completion is retired, not re-terminalised"
            );
        }
        assert!(
            bridge.live_workers.is_empty(),
            "terminal cancellation wakes the worker"
        );
        assert!(
            bridge.retired.is_empty(),
            "the worker result reaps its retired latch"
        );
    }

    /// The flag alone would be enough for a sleep loop; this proves the socket
    /// wakes a worker that is genuinely blocked in `poll` with no data pending.
    #[test]
    fn cancellation_wakes_a_worker_already_waiting_in_poll() {
        let (reader, writer) = UnixStream::pair().unwrap();
        let mut pipe = read_pipe(reader);
        let (bridge_side, worker_side) = cancel_pair();

        let handle = thread::spawn(move || {
            // A long inactivity window, so only cancellation can end this read.
            read_payload_cancellable(&mut pipe, 1024, &worker_side, Duration::from_secs(3600))
        });

        // Give the worker time to reach `poll` before cancelling.
        thread::sleep(Duration::from_millis(50));
        bridge_side.cancel();

        assert_eq!(handle.join().unwrap(), Err(PayloadReadError::Cancelled));
        drop(writer);
    }

    #[test]
    fn a_source_that_never_closes_expires_on_inactivity() {
        // Held open and never written to, exactly like a source that answers
        // `receive` and then stalls.
        let (reader, _writer) = UnixStream::pair().unwrap();
        let mut pipe = read_pipe(reader);
        let (_bridge_side, worker_side) = cancel_pair();

        assert_eq!(
            read_payload_cancellable(&mut pipe, 1024, &worker_side, Duration::from_millis(80)),
            Err(PayloadReadError::Failure(PayloadFailure::Inactive))
        );
    }

    #[test]
    fn inactivity_measures_time_since_the_last_byte_not_total_duration() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut pipe = read_pipe(reader);
        let (_bridge_side, worker_side) = cancel_pair();

        let handle = thread::spawn(move || {
            read_payload_cancellable(&mut pipe, 1024, &worker_side, Duration::from_millis(150))
        });
        // Three writes spanning well past one inactivity window, none of them
        // more than one window apart.
        for _ in 0..3 {
            thread::sleep(Duration::from_millis(70));
            writer.write_all(b"ab").unwrap();
        }
        drop(writer);

        assert_eq!(handle.join().unwrap(), Ok(b"ababab".to_vec()));
    }

    #[test]
    fn the_payload_cap_is_inclusive_and_the_next_byte_is_rejected() {
        let cap = 4096;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut pipe = read_pipe(reader);
        let (_bridge, worker) = cancel_pair();
        let handle = thread::spawn(move || {
            read_payload_cancellable(&mut pipe, cap, &worker, Duration::from_secs(30))
        });
        writer.write_all(&vec![b'x'; cap]).unwrap();
        drop(writer);
        assert_eq!(handle.join().unwrap().map(|bytes| bytes.len()), Ok(cap));

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut pipe = read_pipe(reader);
        let (_bridge, worker) = cancel_pair();
        let handle = thread::spawn(move || {
            read_payload_cancellable(&mut pipe, cap, &worker, Duration::from_secs(30))
        });
        writer.write_all(&vec![b'x'; cap + 1]).unwrap();
        drop(writer);
        assert_eq!(
            handle.join().unwrap(),
            Err(PayloadReadError::Failure(PayloadFailure::TooLarge))
        );
    }

    #[test]
    fn payload_failures_map_onto_distinct_terminal_reasons() {
        assert_eq!(
            PayloadFailure::TooLarge.reason(),
            TerminalReason::PayloadTooLarge
        );
        assert_eq!(
            PayloadFailure::Inactive.reason(),
            TerminalReason::PayloadInactivityExpired
        );
        assert_eq!(PayloadFailure::Pipe.reason(), TerminalReason::PipeFailure);
    }

    #[test]
    fn a_drop_classifies_as_lifecycle_and_carries_its_fence_revision() {
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        for _ in 0..12 {
            bridge.state.next_revision();
        }
        bridge.state.capture_drop(DataTransferId(7));

        let events = bridge.state.protocol_queue.drain_frame();
        assert!(matches!(
            events.as_slice(),
            [ProtocolEvent::Drop {
                transfer_id: DataTransferId(7),
                at_revision: TransportRevision(12),
            }]
        ));
        assert!(matches!(events[0].class(), EventClass::Lifecycle));
    }

    #[test]
    fn enter_leave_enter_batch_keeps_the_second_captured_offer_live() {
        // SCTK 0.19.2 data_device.rs:137-167 destroys/leaves the first offer
        // before exposing a later Enter captured in the same dispatch batch.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (first, _) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        layer.leave(&mut bridge.state, 11);
        let (second, _) = layer.enter(&mut bridge.state, 22, 202, true, ActionMask::COPY);
        let consumer_events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert_eq!(active_offer_id(&bridge), Some((second, 202)));
        assert_eq!(log.borrow().destroyed, vec![101]);
        assert!(matches!(
            consumer_events.as_slice(),
            [
                BridgeEvent::Terminal(TerminalEvent {
                    transfer_id,
                    reason: TerminalReason::LeaveBeforeDrop,
                    ..
                }),
                BridgeEvent::Entered {
                    transfer_id: entered,
                    ..
                }
            ] if *transfer_id == first && *entered == second
        ));
    }

    #[test]
    fn foreign_then_owned_enter_batch_rejects_only_the_foreign_offer() {
        // SCTK 0.19.2 data_device.rs:137-155 delivers every Enter before the
        // bridge applies its wl_surface ownership filter.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let _foreign = capture_test_enter(&mut bridge.state, 101, false, &log);
        let owned = capture_test_enter(&mut bridge.state, 202, true, &log);
        let consumer_events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert_eq!(active_offer_id(&bridge), Some((owned, 202)));
        assert_eq!(log.borrow().destroyed, vec![101]);
        assert_eq!(log.borrow().accepted, vec![(101, None)]);
        assert!(matches!(
            consumer_events.as_slice(),
            [BridgeEvent::Entered { transfer_id, .. }] if *transfer_id == owned
        ));
    }

    #[test]
    fn selected_action_is_drained_before_a_pending_worker_result() {
        let now = Instant::now();
        let id = DataTransferId(1);
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        bridge
            .state
            .protocol_queue
            .enqueue(captured_enter(id, 101, true, &log))
            .unwrap();
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        let transfer = &mut bridge.active.as_mut().unwrap().transfer;
        transfer
            .accept(
                crate::types::AcceptedContext {
                    target: TargetId(1),
                    action: DndAction::Copy,
                    modifiers: crate::types::Modifiers::default(),
                    origin: crate::types::DndOrigin::External(id),
                    delivery_id: crate::types::DeliveryId(9),
                    revision: ProposalRevision(1),
                },
                TransportRevision(1),
            )
            .unwrap();
        assert!(transfer.begin_fetch(now).unwrap().is_empty());
        assert!(
            transfer
                .compositor_action(Some(DndAction::Copy), now)
                .unwrap()
                .is_empty()
        );
        bridge
            .state
            .protocol_queue
            .enqueue(ProtocolEvent::Drop {
                transfer_id: id,
                at_revision: TransportRevision(1),
            })
            .unwrap();
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty(),
            "the previous frame snapshots the drop while payload I/O is pending"
        );

        bridge
            .state
            .protocol_queue
            .enqueue(ProtocolEvent::SelectedAction {
                transfer_id: id,
                action: Some(DndAction::Move),
                transport_revision: TransportRevision(2),
            })
            .unwrap();
        bridge
            .worker_tx
            .send(WorkerResult {
                transfer_id: id,
                payload: Ok(crate::types::DragPayload::Paths(vec![PathBuf::from(
                    "/tmp/a",
                )])),
            })
            .unwrap();

        let consumer_events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let drop = consumer_events
            .iter()
            .find_map(|event| match event {
                BridgeEvent::Drop(drop) => Some(drop),
                _ => None,
            })
            .expect("worker completion emits the consumer drop");
        assert_eq!(drop.action, DndAction::Move);
    }

    #[test]
    fn zero_and_unrepresentable_bridge_deadlines_are_rejected_at_construction() {
        let now = Instant::now();
        for (config, expected) in [
            (
                BridgeConfig {
                    ask_confirmation_deadline: Duration::ZERO,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::ZeroAskConfirmationDeadline,
            ),
            (
                BridgeConfig {
                    post_decision_deadline: Duration::ZERO,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::ZeroPostDecisionDeadline,
            ),
            (
                BridgeConfig {
                    payload_inactivity: Duration::ZERO,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::ZeroPayloadInactivity,
            ),
            (
                BridgeConfig {
                    drop_fence_timeout: Duration::ZERO,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::ZeroDropFenceTimeout,
            ),
            (
                BridgeConfig {
                    ask_confirmation_deadline: Duration::MAX,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::UnrepresentableAskConfirmationDeadline,
            ),
            (
                BridgeConfig {
                    post_decision_deadline: Duration::MAX,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::UnrepresentablePostDecisionDeadline,
            ),
            (
                BridgeConfig {
                    payload_inactivity: Duration::MAX,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::UnrepresentablePayloadInactivity,
            ),
            (
                BridgeConfig {
                    drop_fence_timeout: Duration::MAX,
                    ..BridgeConfig::default()
                },
                BridgeConfigError::UnrepresentableDropFenceTimeout,
            ),
        ] {
            assert_eq!(config.validate(now), Err(expected));
        }
    }

    #[test]
    fn data_device_protocol_v3_is_required_before_devices_are_created() {
        // Wayland wl_data_device_manager v3 is where action negotiation and
        // finish become available. The constructor can reach device creation
        // only through the same validated token exercised here.
        // This does not cover SCTK's concrete `from_raw_handles` constructor:
        // that needs an in-process wayland-server stub, out of scope for 0.1.0.
        let creations = Cell::new(0);
        assert_eq!(
            DataDeviceProtocolV3::try_from(2).map(|_| ()),
            Err(InitError::DataDeviceProtocolTooOld {
                available: 2,
                required: 3,
            })
        );
        assert_eq!(creations.get(), 0);
        let supported = DataDeviceProtocolV3::try_from(3).unwrap();
        assert_eq!(
            create_data_devices(supported, || {
                creations.set(creations.get() + 1);
                7
            }),
            7
        );
        assert_eq!(creations.get(), 1);
    }

    /// Motion and action callbacks coalesce per frame, so the fence must be
    /// satisfied by the *surviving* event's revision — the newest one.
    #[test]
    fn coalescing_keeps_the_newest_revision_for_each_callback_class() {
        let mut queue = BoundedEventQueue::<ProtocolEvent>::new(QueueConfig::default()).unwrap();
        let id = DataTransferId(1);
        for revision in 1..=5 {
            let _ = queue.enqueue(ProtocolEvent::Motion {
                transfer_id: id,
                position: Position {
                    x: revision as f64,
                    y: 0.0,
                },
                transport_revision: TransportRevision(revision),
            });
        }
        let frame = queue.drain_frame();
        assert_eq!(frame.len(), 1);
        match &frame[0] {
            ProtocolEvent::Motion {
                transport_revision, ..
            } => assert_eq!(*transport_revision, TransportRevision(5)),
            other => panic!("expected motion, got {:?}", other.transfer_id()),
        }
    }

    #[test]
    fn transport_revisions_are_monotonic_across_callback_kinds() {
        // Bridge design: every callback kind that can stale a drop fence shares
        // one monotonic transport revision sequence.
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = capture_test_enter(&mut bridge.state, 101, true, &log);
        let key = CallbackIdentity::test(101);
        bridge
            .state
            .capture_motion_for_device(key.clone(), Position { x: 1.0, y: 2.0 });
        bridge
            .state
            .capture_selected_action_for_offer(key.clone(), Some(DndAction::Copy));
        bridge
            .state
            .capture_source_actions_for_offer(key, ActionMask::COPY);
        let mut revisions = bridge
            .state
            .protocol_queue
            .drain_frame()
            .into_iter()
            .filter_map(|event| match event {
                ProtocolEvent::Enter {
                    transfer_id,
                    transport_revision,
                    ..
                }
                | ProtocolEvent::Motion {
                    transfer_id,
                    transport_revision,
                    ..
                }
                | ProtocolEvent::SelectedAction {
                    transfer_id,
                    transport_revision,
                    ..
                }
                | ProtocolEvent::SourceActions {
                    transfer_id,
                    transport_revision,
                    ..
                } if transfer_id == id => Some(transport_revision),
                _ => None,
            })
            .collect::<Vec<_>>();
        revisions.sort();
        assert_eq!(
            revisions,
            vec![
                TransportRevision(1),
                TransportRevision(2),
                TransportRevision(3),
                TransportRevision(4)
            ]
        );

        bridge.state.transport_revision = u64::MAX;
        assert_eq!(
            bridge.state.next_revision(),
            TransportRevision(u64::MAX),
            "revisions saturate rather than wrapping behind delivered state"
        );
    }

    fn enter_test_offer(
        bridge: &mut WaylandBridge,
        log: &Rc<RefCell<TestOfferLog>>,
        now: Instant,
    ) -> DataTransferId {
        let id = capture_test_enter(&mut bridge.state, 101, true, log);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Entered {
                transfer_id,
                transport_revision: TransportRevision(1),
                ..
            }] if *transfer_id == id
        ));
        id
    }

    fn wait_until_payload_ready(bridge: &mut WaylandBridge, now: Instant) {
        let spin_started = Instant::now();
        loop {
            if bridge
                .active
                .as_ref()
                .is_some_and(|active| active.transfer.phase() == ReceivePhase::Ready)
            {
                return;
            }
            assert!(
                spin_started.elapsed() < TEST_WORKER_TIMEOUT,
                "payload worker did not complete within {TEST_WORKER_TIMEOUT:?}"
            );
            thread::yield_now();
            bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        }
    }

    fn reserve_copy_completion_before_finish(
        bridge: &mut WaylandBridge,
        layer: &TestProxyLayer,
        now: Instant,
    ) -> (DataTransferId, TestOfferProxy) {
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::ASK,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let transfer = &mut bridge.active.as_mut().unwrap().transfer;
        transfer
            .accept(
                crate::types::AcceptedContext {
                    target: TargetId(1),
                    action: DndAction::Copy,
                    modifiers: crate::types::Modifiers::default(),
                    origin: crate::types::DndOrigin::External(id),
                    delivery_id: crate::types::DeliveryId(9),
                    revision: ProposalRevision(1),
                },
                TransportRevision(1),
            )
            .unwrap();
        assert!(transfer.begin_fetch(now).unwrap().is_empty());
        assert!(
            transfer
                .compositor_action(Some(DndAction::Copy), now)
                .unwrap()
                .is_empty()
        );
        layer.drop(&mut bridge.state, 11);
        bridge
            .worker_tx
            .send(WorkerResult {
                transfer_id: id,
                payload: Ok(crate::types::DragPayload::Paths(vec![PathBuf::from(
                    "/tmp/a",
                )])),
            })
            .unwrap();
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .iter()
                .any(|event| matches!(event, BridgeEvent::Drop(_)))
        );

        bridge.test_flushes.push_back(Ok(FlushStatus::WouldBlock));
        bridge
            .complete_drop(
                id,
                DropComplete {
                    delivery_id: crate::types::DeliveryId(9),
                    outcome: crate::types::DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert!(bridge.pending_completion.is_some());
        assert!(
            bridge
                .pending_completion
                .as_ref()
                .is_some_and(|pending| !pending.finish_sent)
        );
        (id, proxy)
    }

    fn reserved_copy_completion(
        now: Instant,
        finish_sent: bool,
    ) -> (WaylandBridge, DataTransferId) {
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, _) = reserve_copy_completion_before_finish(&mut bridge, &layer, now);
        if finish_sent {
            let mut flushes = [FlushStatus::Flushed, FlushStatus::WouldBlock].into_iter();
            assert!(
                bridge
                    .run_frame(now, |_| Ok(flushes.next().expect("two completion flushes")))
                    .unwrap()
                    .is_empty()
            );
            assert!(
                bridge
                    .pending_completion
                    .as_ref()
                    .is_some_and(|pending| pending.finish_sent)
            );
        }
        (bridge, id)
    }

    fn assert_pending_exit(
        events: &[BridgeEvent],
        id: DataTransferId,
        finish_committed: bool,
        rejection_reason: TerminalReason,
    ) {
        let expected = if finish_committed {
            TerminalEvent {
                transfer_id: id,
                disposition: TerminalDisposition::Finished,
                reason: TerminalReason::Completed,
            }
        } else {
            TerminalEvent {
                transfer_id: id,
                disposition: TerminalDisposition::Rejected,
                reason: rejection_reason,
            }
        };
        assert_eq!(events, &[BridgeEvent::Terminal(expected)]);
    }

    fn queue_fatal_test_flush(bridge: &mut WaylandBridge) {
        bridge
            .test_flushes
            .push_back(Err(WaylandError::Io(std::io::Error::from(
                std::io::ErrorKind::BrokenPipe,
            ))));
    }

    fn prepare_post_drop_ask(
        bridge: &mut WaylandBridge,
        layer: &TestProxyLayer,
        now: Instant,
    ) -> (DataTransferId, TestOfferProxy, crate::types::DropEvent) {
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::ASK,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 1)).unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        wait_until_payload_ready(bridge, now);

        layer.selected_action(&mut bridge.state, &proxy, Some(DndAction::Ask));
        layer.drop(&mut bridge.state, 11);
        layer.leave(&mut bridge.state, 11);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 2)).unwrap();
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let drop = events
            .into_iter()
            .find_map(|event| match event {
                BridgeEvent::Drop(drop) => Some(drop),
                _ => None,
            })
            .expect("current Ask acceptance resolves the drop fence");
        (id, proxy, drop)
    }

    #[test]
    fn a_post_drop_leave_keeps_accept_live_and_delivers_the_current_target() {
        // Wayland requires an accepted MIME before wl_data_offer.finish; SCTK
        // 0.19.2 data_offer.rs:126 suppresses accept after its `left` latch.
        // wayland.xml wl_data_device.drop says copy/move honours the last
        // action; only an Ask result sends one final set_actions.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::MOVE,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);
        bridge.clear_acceptance(id).unwrap();
        assert_eq!(log.borrow().accepted.last(), Some(&(101, None)));

        layer.motion(&mut bridge.state, 11, Position { x: 20.0, y: 5.0 });
        layer.selected_action(&mut bridge.state, &proxy, Some(DndAction::Move));
        layer.drop(&mut bridge.state, 11);
        layer.leave(&mut bridge.state, 11);
        let batch = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(batch.iter().any(|event| matches!(
            event,
            BridgeEvent::HoverLeft {
                transfer_id,
                post_drop: true
            } if *transfer_id == id
        )));
        assert!(
            !batch
                .iter()
                .any(|event| matches!(event, BridgeEvent::Drop(_)))
        );

        bridge
            .accept(acceptance(id, 2, DndAction::Move, 3))
            .unwrap();
        assert_eq!(
            log.borrow().accepted.last(),
            Some(&(101, Some("text/plain".into()))),
            "the current target's MIME is accepted on the retained offer"
        );
        assert_eq!(
            log.borrow().actions.len(),
            2,
            "ordinary post-drop set_actions stays refused"
        );

        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let drop = events
            .iter()
            .find_map(|event| match event {
                BridgeEvent::Drop(drop) => Some(drop),
                _ => None,
            })
            .expect("current acceptance resolves the drop fence");
        assert_eq!(drop.target, TargetId(2));
        assert_eq!(drop.action, DndAction::Move);

        bridge
            .complete_drop(
                id,
                DropComplete {
                    delivery_id: drop.delivery_id,
                    outcome: crate::types::DropOutcome::Completed(DndAction::Move),
                },
                now,
            )
            .unwrap();
        let wire = &log.borrow().wire;
        let accepted = wire
            .iter()
            .rposition(|request| {
                matches!(
                    request,
                    TestWireRequest::Accept(101, _, Some(mime)) if mime == "text/plain"
                )
            })
            .expect("post-drop accept reached the wire");
        assert_eq!(
            wire[accepted],
            TestWireRequest::Accept(101, 101, Some("text/plain".into())),
            "the retained accept carries the real Enter serial"
        );
        let finished = wire
            .iter()
            .position(|request| matches!(request, TestWireRequest::Finish(101)))
            .expect("successful completion sends finish");
        assert!(accepted < finished, "finish is ordered behind final accept");

        // Defence in depth: a dead proxy must fail in the shared raw helper,
        // not report local success after wayland-client silently drops it.
        let dead_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let dead_layer = TestProxyLayer::new(&dead_log);
        let mut dead_bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (dead_id, dead_proxy) = dead_layer.enter(
            &mut dead_bridge.state,
            11,
            202,
            true,
            ActionMask::COPY | ActionMask::MOVE,
        );
        dead_bridge
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        dead_bridge
            .accept(acceptance(dead_id, 1, DndAction::Copy, 1))
            .unwrap();
        dead_layer.selected_action(&mut dead_bridge.state, &dead_proxy, Some(DndAction::Move));
        dead_layer.drop(&mut dead_bridge.state, 11);
        dead_layer.leave(&mut dead_bridge.state, 11);
        dead_bridge
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        dead_proxy.state.alive.set(false);
        assert_eq!(
            dead_bridge.accept(acceptance(dead_id, 2, DndAction::Move, 2)),
            Err(BridgeError::OfferProxyDead)
        );
        assert!(matches!(
            dead_bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::OfferProxyDead,
            })] if *transfer_id == dead_id
        ));
        assert_eq!(
            dead_log
                .borrow()
                .accepted
                .iter()
                .filter(|(offer, mime)| *offer == 202 && mime.is_some())
                .count(),
            1,
            "the dead retained proxy receives no second accept"
        );
    }

    #[test]
    fn a_post_drop_leave_keeps_payload_receive_live() {
        // SCTK 0.19.2 DragOffer::receive permits a post-drop left offer, while
        // the test proxy makes the same call fail once the proxy is dead.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::MOVE,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();

        layer.motion(&mut bridge.state, 11, Position { x: 20.0, y: 5.0 });
        layer.selected_action(&mut bridge.state, &proxy, Some(DndAction::Move));
        layer.drop(&mut bridge.state, 11);
        layer.leave(&mut bridge.state, 11);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(id, 2, DndAction::Move, 3))
            .unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();

        assert_eq!(log.borrow().received, vec![(101, "text/plain".into())]);

        let dead_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let dead_layer = TestProxyLayer::new(&dead_log);
        let mut dead_bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (dead_id, dead_proxy) =
            dead_layer.enter(&mut dead_bridge.state, 11, 202, true, ActionMask::COPY);
        dead_bridge
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        dead_bridge
            .accept(acceptance(dead_id, 1, DndAction::Copy, 1))
            .unwrap();
        dead_layer.selected_action(&mut dead_bridge.state, &dead_proxy, Some(DndAction::Copy));
        dead_layer.drop(&mut dead_bridge.state, 11);
        dead_layer.leave(&mut dead_bridge.state, 11);
        dead_bridge
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        dead_proxy.state.alive.set(false);
        assert_eq!(
            dead_bridge.request_data(dead_id, "text/plain", now),
            Err(BridgeError::OfferProxyDead)
        );
        assert!(matches!(
            dead_bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::OfferProxyDead,
                ..
            })] if *transfer_id == dead_id
        ));
        assert!(dead_log.borrow().received.is_empty());
    }

    #[test]
    fn a_post_drop_ask_keeps_callback_correlation_until_completion() {
        // Wayland drop keeps the offer live after pointer leave; offer.action
        // and wl_callback.done must remain correlated until terminal cleanup.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::ASK,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 1)).unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);

        let device_key = CallbackIdentity::test(11);
        let offer_key = CallbackIdentity::test(101);
        layer.selected_action(&mut bridge.state, &proxy, Some(DndAction::Ask));
        layer.drop(&mut bridge.state, 11);
        layer.leave(&mut bridge.state, 11);
        assert_eq!(
            bridge.state.device_transfers.transfer_for(&device_key),
            Some(id)
        );
        let first = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(first.iter().any(|event| matches!(
            event,
            BridgeEvent::HoverLeft {
                transfer_id,
                post_drop: true
            } if *transfer_id == id
        )));
        bridge.accept(acceptance(id, 1, DndAction::Ask, 2)).unwrap();
        let dropped = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let drop = dropped
            .iter()
            .find_map(|event| match event {
                BridgeEvent::Drop(drop) => Some(drop.clone()),
                _ => None,
            })
            .expect("current Ask acceptance resolves the drop fence");

        bridge
            .decide_drop(
                id,
                DropDecision {
                    delivery_id: drop.delivery_id,
                    decision: crate::types::DropDecisionKind::Copy,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            bridge.state.offer_transfers.transfer_for(&offer_key),
            Some(id),
            "post-drop leave preserves the offer callback key"
        );
        layer.selected_action(&mut bridge.state, &proxy, Some(DndAction::Copy));
        let barrier = *bridge
            .state
            .pending_barriers
            .first()
            .expect("final set_actions installs its sync barrier");
        bridge.state.capture_barrier_done(&barrier);
        let acknowledged = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            acknowledged.as_slice(),
            [BridgeEvent::ActionChanged {
                transfer_id,
                action: Some(DndAction::Copy),
                ..
            }] if *transfer_id == id
        ));
        bridge
            .complete_drop(
                id,
                DropComplete {
                    delivery_id: drop.delivery_id,
                    outcome: crate::types::DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        let completed = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            completed.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id: terminal_id,
                disposition: TerminalDisposition::Finished,
                reason: TerminalReason::Completed,
            })] if *terminal_id == id
        ));
        assert_eq!(log.borrow().finished, vec![101]);
    }

    #[test]
    fn a_final_preferred_action_is_authoritative_when_the_action_does_not_change() {
        // wl_data_offer.set_actions says callbacks may be emitted only when the
        // selected action changes. The requested action is therefore the
        // fallback only while the source's callback-time mask still offers it.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, _, drop) = prepare_post_drop_ask(&mut bridge, &layer, now);
        bridge
            .decide_drop(
                id,
                DropDecision {
                    delivery_id: drop.delivery_id,
                    decision: crate::types::DropDecisionKind::Copy,
                },
                now,
            )
            .unwrap();
        let barrier = *bridge.state.pending_barriers.first().unwrap();
        bridge.state.capture_barrier_done(&barrier);
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty(),
            "no repeated action callback is required while Copy remains offered"
        );
        bridge
            .complete_drop(
                id,
                DropComplete {
                    delivery_id: drop.delivery_id,
                    outcome: crate::types::DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                disposition: TerminalDisposition::Finished,
                reason: TerminalReason::Completed,
                ..
            })]
        ));

        let withdrawn_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let withdrawn_layer = TestProxyLayer::new(&withdrawn_log);
        let mut withdrawn = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (withdrawn_id, proxy, drop) =
            prepare_post_drop_ask(&mut withdrawn, &withdrawn_layer, now);
        withdrawn
            .decide_drop(
                withdrawn_id,
                DropDecision {
                    delivery_id: drop.delivery_id,
                    decision: crate::types::DropDecisionKind::Copy,
                },
                now,
            )
            .unwrap();
        withdrawn_layer.selected_action(&mut withdrawn.state, &proxy, Some(DndAction::Copy));
        let barrier = *withdrawn.state.pending_barriers.first().unwrap();
        withdrawn.state.capture_barrier_done(&barrier);
        // The callback after sync-done is in the same dispatch_pending batch;
        // settle-time facts must withdraw both the cached and fallback action.
        withdrawn_layer.source_actions(&mut withdrawn.state, &proxy, ActionMask::ASK);
        assert!(matches!(
            withdrawn
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::FinalActionRejected,
                ..
            })] if *transfer_id == withdrawn_id
        ));
    }

    #[test]
    fn a_none_action_after_the_final_request_fails_fast() {
        // Bridge design decision: sync done converts a missing unchanged
        // action into None when the callback-time source mask has withdrawn
        // the requested result; ReceiveTransfer then terminates immediately.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy, drop) = prepare_post_drop_ask(&mut bridge, &layer, now);
        bridge
            .decide_drop(
                id,
                DropDecision {
                    delivery_id: drop.delivery_id,
                    decision: crate::types::DropDecisionKind::Copy,
                },
                now,
            )
            .unwrap();
        layer.source_actions(&mut bridge.state, &proxy, ActionMask::ASK);
        let barrier = *bridge.state.pending_barriers.first().unwrap();
        bridge.state.capture_barrier_done(&barrier);

        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::FinalActionRejected,
            })] if *transfer_id == id
        ));
        assert!(log.borrow().finished.is_empty());
    }

    #[test]
    fn an_unacknowledged_post_drop_final_action_names_that_path() {
        // Bridge design decision: only a final request sent through SCTK's
        // post-leave bypass owns PostDropFinalActionDeadlineExpired.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = capture_test_enter_with_actions(
            &mut bridge.state,
            101,
            true,
            ActionMask::COPY | ActionMask::ASK,
            &log,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 1)).unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);

        let key = CallbackIdentity::test(101);
        bridge
            .state
            .capture_selected_action_for_offer(key.clone(), Some(DndAction::Ask));
        bridge.state.capture_drop_for_device(key.clone());
        bridge.state.capture_leave(key);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 2)).unwrap();
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let drop = events
            .iter()
            .find_map(|event| match event {
                BridgeEvent::Drop(drop) => Some(drop),
                _ => None,
            })
            .expect("current Ask acceptance emits the drop");
        bridge
            .decide_drop(
                id,
                DropDecision {
                    delivery_id: drop.delivery_id,
                    decision: crate::types::DropDecisionKind::Copy,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            log.borrow().final_actions,
            vec![(101, ActionMask::COPY, DndAction::Copy)]
        );

        let expired = bridge
            .run_frame(now + DEFAULT_POST_DECISION_DEADLINE, |_| {
                Ok(FlushStatus::Flushed)
            })
            .unwrap();
        assert!(matches!(
            expired.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::PostDropFinalActionDeadlineExpired,
                ..
            })] if *transfer_id == id
        ));
    }

    #[test]
    fn a_payload_request_after_its_deadline_cannot_start_fetch_or_revive_the_drop() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = enter_test_offer(&mut bridge, &log, now);
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();

        bridge.state.enqueue(ProtocolEvent::SelectedAction {
            transfer_id: id,
            action: Some(DndAction::Copy),
            transport_revision: TransportRevision(1),
        });
        bridge.state.capture_drop(id);
        bridge
            .state
            .enqueue(ProtocolEvent::Leave { transfer_id: id });
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        bridge
            .request_data(id, "text/plain", now + DEFAULT_ASK_CONFIRMATION_DEADLINE)
            .unwrap();
        assert!(log.borrow().received.is_empty());
        let events = bridge
            .run_frame(now + DEFAULT_ASK_CONFIRMATION_DEADLINE, |_| {
                Ok(FlushStatus::Flushed)
            })
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::PayloadRequestDeadlineExpired,
                ..
            })] if *transfer_id == id
        ));
        assert!(log.borrow().finished.is_empty());
        assert!(log.borrow().final_actions.is_empty());
    }

    #[test]
    fn a_connection_loss_terminates_exactly_once_from_any_state() {
        // wayland-client reports a fatal display error for every live protocol
        // object; bridge cleanup must cancel real receive I/O and emit one latch.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        proxy.state.hold_receive_open.set(true);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();
        bridge.request_data(id, "text/plain", now).unwrap();
        assert_eq!(bridge.live_workers, BTreeSet::from([id]));

        bridge.lose_connection("display lost".into());
        bridge.lose_connection("second report".into());
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::WaylandConnectionLost,
                ..
            })] if *transfer_id == id
        ));
        assert_eq!(log.borrow().destroyed, vec![101]);
        assert!(bridge.active.is_none());
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty()
        );
        let spin_started = Instant::now();
        while !bridge.live_workers.is_empty() {
            assert!(
                spin_started.elapsed() < TEST_WORKER_TIMEOUT,
                "payload worker did not retire within {TEST_WORKER_TIMEOUT:?}"
            );
            thread::yield_now();
            assert!(
                bridge
                    .run_frame(now, |_| Ok(FlushStatus::Flushed))
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(bridge.live_workers.is_empty());

        // ...from Offered, before a payload worker exists.
        let offered_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut offered = WaylandBridge::for_frame_test(BridgeConfig::default());
        let offered_id = enter_test_offer(&mut offered, &offered_log, now);
        offered.lose_connection("display lost while offered".into());
        offered.lose_connection("duplicate".into());
        assert!(matches!(
            offered
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::WaylandConnectionLost,
                ..
            })] if *transfer_id == offered_id
        ));

        // ...while an Ask is waiting for the application's decision.
        let ask_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let ask_layer = TestProxyLayer::new(&ask_log);
        let mut ask = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (ask_id, _, _) = prepare_post_drop_ask(&mut ask, &ask_layer, now);
        ask.lose_connection("display lost during Ask".into());
        ask.lose_connection("duplicate".into());
        assert!(matches!(
            ask.run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::WaylandConnectionLost,
                ..
            })] if *transfer_id == ask_id
        ));

        // ...while a successful completion is hidden behind its finish flush.
        let pending_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut pending = WaylandBridge::for_frame_test(BridgeConfig::default());
        let pending_id = enter_test_offer(&mut pending, &pending_log, now);
        pending.pending_completion = Some(PendingCompletion {
            terminal: TerminalEvent {
                transfer_id: pending_id,
                disposition: TerminalDisposition::Finished,
                reason: TerminalReason::Completed,
            },
            action: DndAction::Copy,
            deadline: now + DEFAULT_POST_DECISION_DEADLINE,
            expiry_reason: TerminalReason::PostDecisionDeadlineExpired,
            finish_sent: false,
        });
        pending.lose_connection("display lost during completion".into());
        pending.lose_connection("duplicate".into());
        let events = pending
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::WaylandConnectionLost,
            })] if *transfer_id == pending_id
        ));
        assert_eq!(pending_log.borrow().destroyed, vec![101]);
    }

    #[test]
    fn cancelled_drag_never_sends_finish_or_post_leave_final_actions() {
        // SCTK 0.19.2 data_offer.rs:315 destroys a left offer that was not
        // dropped; finish and the Ask-only final actions are post-drop paths.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(
            &mut bridge.state,
            11,
            101,
            true,
            ActionMask::COPY | ActionMask::ASK,
        );
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(id, 1, DndAction::Ask, 1)).unwrap();
        layer.leave(&mut bridge.state, 11);
        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::LeaveBeforeDrop,
                ..
            })] if *transfer_id == id
        ));
        assert!(!proxy.state.alive.get());
        assert!(!log.borrow().wire.iter().any(|request| matches!(
            request,
            TestWireRequest::Finish(_)
                | TestWireRequest::Actions(_, _, Some(DndAction::Copy | DndAction::Move))
        )));
    }

    #[test]
    fn a_leave_overflow_after_sctk_destroy_does_not_destroy_the_offer_twice() {
        // SCTK 0.19.2 data_offer.rs:315 destroys an undropped offer before the
        // Leave callback; data_offer.rs:132-135 does not deduplicate destroy.
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(config);
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.state.enqueue(ProtocolEvent::Drop {
            transfer_id: DataTransferId(999),
            at_revision: TransportRevision(1),
        });

        layer.leave(&mut bridge.state, 11);
        assert!(!proxy.state.alive.get());
        assert_eq!(log.borrow().destroyed, vec![101]);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::QueueOverflow,
                ..
            })] if *transfer_id == id
        ));
        assert_eq!(
            log.borrow().destroyed,
            vec![101],
            "cleanup observes SCTK's destruction before draining Leave"
        );
    }

    #[test]
    fn dropping_the_bridge_before_an_sctk_leave_drains_does_not_destroy_twice() {
        // SCTK 0.19.2 data_offer.rs:315 destroys before Leave is delivered, and
        // data_offer.rs:132-135 forwards a second destroy unless liveness gates it.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (_, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        layer.leave(&mut bridge.state, 11);
        assert!(!proxy.state.alive.get());
        drop(bridge);

        assert_eq!(log.borrow().destroyed, vec![101]);
    }

    #[test]
    fn would_block_flush_is_transient_and_every_other_io_error_is_fatal() {
        assert!(matches!(
            classify_flush(Err(WaylandError::Io(std::io::Error::from(
                std::io::ErrorKind::WouldBlock
            )))),
            Ok(FlushStatus::WouldBlock)
        ));
        assert!(
            classify_flush(Err(WaylandError::Io(std::io::Error::from(
                std::io::ErrorKind::BrokenPipe
            ))))
            .is_err()
        );
    }

    #[test]
    fn completed_stays_hidden_and_live_until_finish_flush_succeeds() {
        // wayland-client Connection::flush may return WouldBlock with requests
        // queued; wayland.xml wl_data_offer.finish is the success commit.
        let now = Instant::now();
        let id = DataTransferId(1);
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        bridge
            .state
            .protocol_queue
            .enqueue(captured_enter(id, 101, true, &log))
            .unwrap();
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let transfer = &mut bridge.active.as_mut().unwrap().transfer;
        transfer
            .accept(
                crate::types::AcceptedContext {
                    target: TargetId(1),
                    action: DndAction::Copy,
                    modifiers: crate::types::Modifiers::default(),
                    origin: crate::types::DndOrigin::External(id),
                    delivery_id: crate::types::DeliveryId(9),
                    revision: ProposalRevision(1),
                },
                TransportRevision(1),
            )
            .unwrap();
        assert!(transfer.begin_fetch(now).unwrap().is_empty());
        assert!(
            transfer
                .compositor_action(Some(DndAction::Copy), now)
                .unwrap()
                .is_empty()
        );
        bridge
            .state
            .protocol_queue
            .enqueue(ProtocolEvent::Drop {
                transfer_id: id,
                at_revision: TransportRevision(1),
            })
            .unwrap();
        bridge
            .worker_tx
            .send(WorkerResult {
                transfer_id: id,
                payload: Ok(crate::types::DragPayload::Paths(vec![PathBuf::from(
                    "/tmp/a",
                )])),
            })
            .unwrap();
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .iter()
                .any(|event| matches!(event, BridgeEvent::Drop(_)))
        );

        bridge.test_flushes.push_back(Ok(FlushStatus::WouldBlock));
        bridge
            .complete_drop(
                id,
                DropComplete {
                    delivery_id: crate::types::DeliveryId(9),
                    outcome: crate::types::DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert!(bridge.pending_completion.is_some());
        assert!(
            bridge.test_flushes.is_empty(),
            "public complete_drop must drive its first completion flush"
        );

        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::WouldBlock))
                .unwrap()
                .is_empty()
        );
        assert!(bridge.active.is_some());
        assert!(log.borrow().finished.is_empty());

        let mut flushes = [FlushStatus::Flushed, FlushStatus::WouldBlock].into_iter();
        assert!(
            bridge
                .run_frame(now, |_| Ok(flushes.next().unwrap()))
                .unwrap()
                .is_empty()
        );
        assert!(bridge.active.is_some());
        assert_eq!(log.borrow().finished, vec![101]);

        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(bridge.active.is_none());
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Finished,
                reason: TerminalReason::Completed,
            })] if *transfer_id == id
        ));
    }

    #[test]
    fn pending_completion_exit_requires_finish_queueing_and_a_viable_connection() {
        // wayland-backend 0.3.15 sys/client_impl/mod.rs:297-303,386-391: only fatal flush errors poison later flushes.
        for finish_sent in [false, true] {
            for connection_lost in [false, true] {
                let now = Instant::now();
                let (mut bridge, id) = reserved_copy_completion(now, finish_sent);
                if connection_lost {
                    bridge.connection_lost = Some("fatal display error".into());
                } else if finish_sent {
                    // A live connection with buffered output remains viable.
                    bridge.test_flushes.push_back(Ok(FlushStatus::WouldBlock));
                }
                let rejection_reason = if connection_lost {
                    TerminalReason::WaylandConnectionLost
                } else {
                    TerminalReason::OfferRejected
                };

                assert!(bridge.exit_pending_completion(rejection_reason));
                assert_pending_exit(
                    &bridge.drain_app_frame(),
                    id,
                    finish_sent && !connection_lost,
                    rejection_reason,
                );
            }
        }
    }

    #[test]
    fn pending_completion_deadline_preserves_queued_finish_on_a_live_connection() {
        // wayland-client 0.31.14 conn.rs:120-125: a later flush can deliver pending requests.
        for finish_sent in [false, true] {
            let now = Instant::now();
            let (mut bridge, id) = reserved_copy_completion(now, finish_sent);

            let events = bridge
                .run_frame(now + DEFAULT_POST_DECISION_DEADLINE, |_| {
                    Ok(FlushStatus::Flushed)
                })
                .unwrap();
            assert_pending_exit(
                &events,
                id,
                finish_sent,
                TerminalReason::PostDecisionDeadlineExpired,
            );
        }
    }

    #[test]
    fn pending_completion_deadline_fatal_probe_rejects_queued_finish() {
        let now = Instant::now();
        let (mut bridge, id) = reserved_copy_completion(now, true);
        queue_fatal_test_flush(&mut bridge);

        let events = bridge
            .run_frame(now + DEFAULT_POST_DECISION_DEADLINE, |_| {
                Ok(FlushStatus::Flushed)
            })
            .unwrap();
        assert_pending_exit(&events, id, false, TerminalReason::WaylandConnectionLost);
        assert!(bridge.connection_lost.is_some());
    }

    #[test]
    fn pending_completion_connection_loss_rejects_even_after_finish_queueing() {
        // wayland-backend 0.3.15 sys/client_impl/mod.rs:297-303,386-391: fatal errors poison flush.
        for finish_sent in [false, true] {
            let now = Instant::now();
            let (mut bridge, id) = reserved_copy_completion(now, finish_sent);

            bridge.lose_connection("display lost during completion".into());
            assert_pending_exit(
                &bridge.drain_app_frame(),
                id,
                false,
                TerminalReason::WaylandConnectionLost,
            );
        }
    }

    #[test]
    fn pending_completion_fatal_flush_uses_connection_loss_exit_once() {
        let now = Instant::now();
        let (mut bridge, id) = reserved_copy_completion(now, true);
        queue_fatal_test_flush(&mut bridge);

        assert!(matches!(
            bridge.flush_connection(),
            Err(BridgeError::Flush(_))
        ));
        let events = bridge.drain_app_frame();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BridgeEvent::Terminal(_)))
                .count(),
            1,
            "a fatal ordinary flush must produce exactly one terminal"
        );
        assert_pending_exit(&events, id, false, TerminalReason::WaylandConnectionLost);
    }

    #[test]
    fn fatal_flush_is_connection_terminal_before_the_error_returns() {
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        queue_fatal_test_flush(&mut bridge);

        assert!(matches!(
            bridge.flush_connection(),
            Err(BridgeError::Flush(_))
        ));
        assert!(bridge.connection_lost.is_some());
        assert!(bridge.outgoing.is_none());
        assert!(bridge.drain_outgoing_events().is_empty());
    }

    #[test]
    fn would_block_flush_is_success_and_keeps_the_connection_live() {
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        bridge.test_flushes.push_back(Ok(FlushStatus::WouldBlock));

        assert_eq!(bridge.flush_connection().unwrap(), FlushStatus::WouldBlock);
        assert!(bridge.connection_lost.is_none());
    }

    #[test]
    fn pending_completion_teardown_preserves_queued_finish_on_a_live_connection() {
        // wayland-client 0.31.14 conn.rs:120-125: a later flush can deliver pending requests.
        for finish_sent in [false, true] {
            let now = Instant::now();
            let (mut bridge, id) = reserved_copy_completion(now, finish_sent);

            let events = bridge.teardown();
            assert_pending_exit(&events, id, finish_sent, TerminalReason::WindowTeardown);
        }
    }

    #[test]
    fn pending_completion_teardown_fatal_probe_rejects_once_without_reentry() {
        let now = Instant::now();
        let (mut bridge, id) = reserved_copy_completion(now, true);
        queue_fatal_test_flush(&mut bridge);

        let events = bridge.teardown();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BridgeEvent::Terminal(_)))
                .count(),
            1,
            "the raw fatal probe must not re-enter the terminal path"
        );
        assert_pending_exit(&events, id, false, TerminalReason::WaylandConnectionLost);
        assert!(bridge.connection_lost.is_some());
    }

    #[test]
    fn pending_completion_replacement_preserves_queued_finish_on_a_live_connection() {
        // wayland-client 0.31.14 conn.rs:120-125: a later flush can deliver pending requests.
        for finish_sent in [false, true] {
            let now = Instant::now();
            let (mut bridge, id) = reserved_copy_completion(now, finish_sent);

            bridge.offer_replaced(id);
            assert_pending_exit(
                &bridge.drain_app_frame(),
                id,
                finish_sent,
                TerminalReason::OfferReplaced,
            );
        }
    }

    #[test]
    fn pending_completion_replacement_fatal_probe_rejects_queued_finish() {
        let now = Instant::now();
        let (mut bridge, id) = reserved_copy_completion(now, true);
        queue_fatal_test_flush(&mut bridge);

        bridge.offer_replaced(id);
        let events = bridge.drain_app_frame();
        assert_pending_exit(&events, id, false, TerminalReason::WaylandConnectionLost);
        assert!(bridge.connection_lost.is_some());
    }

    #[test]
    fn a_source_actions_withdrawal_between_completion_reservation_and_finish_rejects_without_finish()
     {
        // wayland.xml wl_data_offer.source_actions/set_actions requires the
        // selected final action to remain inside the source's advertised mask.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = reserve_copy_completion_before_finish(&mut bridge, &layer, now);

        layer.source_actions(&mut bridge.state, &proxy, ActionMask::ASK);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::FinalActionRejected,
            })] if *transfer_id == id
        ));
        assert!(
            !log.borrow()
                .wire
                .iter()
                .any(|request| matches!(request, TestWireRequest::Finish(101))),
            "the withdrawal landed before the finish commit point"
        );
    }

    #[test]
    fn a_selected_action_change_before_finish_invalidates_the_reserved_completion() {
        // wayland.xml wl_data_offer.action makes each compositor-selected action
        // the current protocol fact until finish commits the transfer.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = reserve_copy_completion_before_finish(&mut bridge, &layer, now);

        layer.selected_action(&mut bridge.state, &proxy, None);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::FinalActionRejected,
            })] if *transfer_id == id
        ));
        assert!(
            !log.borrow()
                .wire
                .iter()
                .any(|request| matches!(request, TestWireRequest::Finish(101)))
        );
    }

    #[test]
    fn an_overflowed_fact_before_finish_rejects_the_reserved_completion_without_finish() {
        // wayland.xml wl_data_offer.action/source_actions can revise the result
        // until finish; losing either callback therefore cannot preserve success.
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                action_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(config);
        let (id, proxy) = reserve_copy_completion_before_finish(&mut bridge, &layer, now);
        bridge.state.enqueue(ProtocolEvent::SourceActions {
            transfer_id: DataTransferId(999),
            actions: ActionMask::COPY,
            transport_revision: TransportRevision(2),
        });

        layer.source_actions(&mut bridge.state, &proxy, ActionMask::ASK);
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::QueueOverflow,
            })] if *transfer_id == id
        ));
        assert!(
            !log.borrow()
                .wire
                .iter()
                .any(|request| matches!(request, TestWireRequest::Finish(101)))
        );
    }

    #[test]
    fn cancellation_latch_capacity_is_derived_from_lifecycle_capacity() {
        // Queue design invariant: every simultaneously queued Enter has one
        // non-evictable overflow cancellation latch.
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 2,
                action_capacity: 2,
                motion_capacity: 1,
                motion_drain_budget: 1,
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(config);
        let active = capture_test_enter(&mut bridge.state, 100, true, &log);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let queued_one = capture_test_enter(&mut bridge.state, 101, true, &log);
        let queued_two = capture_test_enter(&mut bridge.state, 102, true, &log);

        bridge.state.capture_leave(CallbackIdentity::test(101));
        bridge.state.capture_leave(CallbackIdentity::test(102));
        assert_eq!(
            bridge.state.cancelled_before_enter,
            VecDeque::from([queued_one, queued_two])
        );
        assert_eq!(
            bridge.state.cancellation_capacity,
            config.queue.lifecycle_capacity
        );

        bridge.state.enqueue(ProtocolEvent::SelectedAction {
            transfer_id: queued_one,
            action: Some(DndAction::Copy),
            transport_revision: TransportRevision(4),
        });
        bridge.state.enqueue(ProtocolEvent::SourceActions {
            transfer_id: queued_two,
            actions: ActionMask::COPY,
            transport_revision: TransportRevision(5),
        });
        bridge.state.enqueue(ProtocolEvent::SelectedAction {
            transfer_id: active,
            action: Some(DndAction::Move),
            transport_revision: TransportRevision(6),
        });
        assert_eq!(
            bridge.state.cancelled_before_enter,
            VecDeque::from([queued_one, queued_two]),
            "overflow on an already-active transfer needs no queued-Enter latch"
        );

        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BridgeEvent::Entered { .. })),
            "neither cancelled queued offer is admitted"
        );
        let mut destroyed = log.borrow().destroyed.clone();
        destroyed.sort_unstable();
        assert_eq!(destroyed, vec![100, 101, 102]);
    }

    #[test]
    fn a_seat_removal_overflow_cancels_its_queued_enter() {
        // Design decision: a discarded SeatRemoved records every correlated
        // transfer at callback time through the same fail-closed overflow path
        // as direct events.
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(config);
        let id = capture_test_enter(&mut bridge.state, 101, true, &log);

        let removed = bridge
            .state
            .record_device_removal(CallbackIdentity::test(100), CallbackIdentity::test(101));
        assert_eq!(removed.transfer_ids, vec![id]);
        bridge.state.enqueue(ProtocolEvent::SeatRemoved(removed));
        assert_eq!(bridge.state.cancelled_before_enter, VecDeque::from([id]));
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty()
        );
        assert!(bridge.active.is_none());
        assert_eq!(log.borrow().accepted, vec![(101, None)]);
        assert_eq!(log.borrow().destroyed, vec![101]);
    }

    /// A capability loss leaves the seat and its data device in place, so
    /// unlike `SeatRemoved` it keys no incoming transfer and must not disturb
    /// one. The outgoing half it does end needs a live `wl_data_source`, which
    /// no unit test can build; that routing is the same `outgoing.seat`
    /// comparison `SeatRemoved` already uses.
    #[test]
    fn a_pointer_capability_loss_spares_the_seats_incoming_transfer() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = capture_test_enter(&mut bridge.state, 101, true, &log);

        bridge.state.enqueue(ProtocolEvent::PointerCapabilityLost {
            seat: CallbackIdentity::test(100),
        });
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        assert!(
            events
                .iter()
                .any(|event| matches!(event, BridgeEvent::Entered { .. })),
            "the incoming transfer is admitted as usual"
        );
        assert_eq!(
            bridge.active.as_ref().map(|active| active.transfer.id()),
            Some(id)
        );
        assert!(
            bridge.drain_outgoing_events().is_empty(),
            "there is no outgoing half to terminate"
        );
    }

    #[test]
    fn a_seat_addition_bypasses_full_drag_lifecycle_queue() {
        // Design decision: hot-added seat objects are created in the seat
        // callback and never enter the per-drag lifecycle queue.
        // This does not invoke SCTK's concrete `SeatHandler::new_seat`: that
        // needs an in-process wayland-server stub, out of scope for 0.1.0.
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(config);
        capture_test_enter(&mut bridge.state, 101, true, &log);
        let creations = Cell::new(0);
        let vacant = admit_unique_seat(false).expect("new seat is admitted");
        let device = create_seat_object(vacant, || {
            creations.set(creations.get() + 1);
            77
        });

        assert_eq!(device, 77);
        assert_eq!(creations.get(), 1);
        assert!(admit_unique_seat(true).is_none());
        assert_eq!(
            bridge.state.protocol_queue.stats().lifecycle,
            1,
            "the full drag queue is untouched by seat creation"
        );
    }

    #[test]
    fn every_overflow_discard_keeps_cancellation_until_its_queued_enter() {
        let now = Instant::now();
        let run_case = |target: ProtocolEvent, filler: Option<ProtocolEvent>| {
            let config = BridgeConfig {
                queue: QueueConfig {
                    lifecycle_capacity: 1,
                    action_capacity: 1,
                    motion_capacity: 1,
                    motion_drain_budget: 1,
                },
                ..BridgeConfig::default()
            };
            let log = Rc::new(RefCell::new(TestOfferLog::default()));
            let mut bridge = WaylandBridge::for_frame_test(config);
            let id = capture_test_enter(&mut bridge.state, 101, true, &log);
            assert_eq!(id, DataTransferId(1));
            if let Some(filler) = filler {
                bridge.state.enqueue(filler);
            }
            bridge.state.enqueue(target);

            assert_eq!(bridge.state.cancelled_before_enter, VecDeque::from([id]));
            assert!(
                bridge
                    .run_frame(now, |_| Ok(FlushStatus::Flushed))
                    .unwrap()
                    .is_empty()
            );
            assert!(bridge.active.is_none());
            assert!(bridge.state.cancelled_before_enter.is_empty());
            assert_eq!(log.borrow().destroyed, vec![101]);
        };

        for lifecycle in [
            ProtocolEvent::Leave {
                transfer_id: DataTransferId(1),
            },
            ProtocolEvent::Drop {
                transfer_id: DataTransferId(1),
                at_revision: TransportRevision(1),
            },
            ProtocolEvent::Worker(WorkerResult {
                transfer_id: DataTransferId(1),
                payload: Err(PayloadFailure::Pipe),
            }),
            ProtocolEvent::BarrierDone {
                transfer_id: DataTransferId(1),
                barrier_id: 7,
                requested_action: DndAction::Copy,
                selected_action: Some(DndAction::Copy),
            },
        ] {
            run_case(lifecycle, None);
        }
        run_case(
            ProtocolEvent::SelectedAction {
                transfer_id: DataTransferId(1),
                action: Some(DndAction::Copy),
                transport_revision: TransportRevision(2),
            },
            Some(ProtocolEvent::SelectedAction {
                transfer_id: DataTransferId(99),
                action: Some(DndAction::Move),
                transport_revision: TransportRevision(1),
            }),
        );
        run_case(
            ProtocolEvent::SourceActions {
                transfer_id: DataTransferId(1),
                actions: ActionMask::COPY,
                transport_revision: TransportRevision(2),
            },
            Some(ProtocolEvent::SourceActions {
                transfer_id: DataTransferId(99),
                actions: ActionMask::MOVE,
                transport_revision: TransportRevision(1),
            }),
        );
        run_case(
            ProtocolEvent::Motion {
                transfer_id: DataTransferId(1),
                position: Position { x: 2.0, y: 0.0 },
                transport_revision: TransportRevision(2),
            },
            Some(ProtocolEvent::Motion {
                transfer_id: DataTransferId(99),
                position: Position { x: 1.0, y: 0.0 },
                transport_revision: TransportRevision(1),
            }),
        );
    }

    #[test]
    fn an_unowned_offer_is_rejected_then_destroyed() {
        // Bridge design decision: an Enter for another surface is rejected
        // with a null accept before the offer is destroyed.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (_, proxy) = layer.enter(&mut bridge.state, 11, 101, false, ActionMask::COPY);
        assert_eq!(
            bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap(),
            Vec::<BridgeEvent>::new()
        );
        assert_eq!(
            log.borrow().wire,
            vec![
                TestWireRequest::Accept(101, 101, None),
                TestWireRequest::Destroy(101)
            ]
        );
        assert!(!proxy.state.alive.get());
        assert!(bridge.active.is_none());
    }

    #[test]
    fn a_worker_spawn_failure_maps_to_pipe_failure() {
        // Bridge design decision: failure to create the payload reader thread
        // is a transfer-level PipeFailure, reached through request_data.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = enter_test_offer(&mut bridge, &log, now);
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 1))
            .unwrap();
        bridge
            .request_data_with_spawner(id, "text/plain", now, |_, _| {
                Err(std::io::Error::other("thread limit"))
            })
            .unwrap();

        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::PipeFailure,
                ..
            })] if *transfer_id == id
        ));
        assert_eq!(log.borrow().received, vec![(101, "text/plain".into())]);
        assert!(log.borrow().finished.is_empty());
    }

    #[test]
    fn rejecting_a_dead_offer_reports_the_request_failure() {
        // wayland-client Proxy::is_alive is checked at the point of request;
        // a failed null accept is not an application rejection.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (id, proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        proxy.state.alive.set(false);

        assert_eq!(bridge.reject(id), Err(BridgeError::OfferProxyDead));
        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::OfferProxyDead,
                ..
            })] if *transfer_id == id
        ));
    }

    #[test]
    fn a_payload_request_over_the_derived_worker_bound_fails_closed() {
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 2,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(config);
        let mut parked_workers: Vec<Box<dyn FnOnce() + Send + 'static>> = Vec::new();

        for sequence in 0..2 {
            let device = 20 + sequence;
            let offer = 200 + sequence;
            let (id, _) = layer.enter(&mut bridge.state, device, offer, true, ActionMask::COPY);
            bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
            bridge
                .accept(acceptance(id, 1, DndAction::Copy, id.0))
                .unwrap();
            bridge
                .request_data_with_spawner(id, "text/plain", now, |_, worker| {
                    parked_workers.push(worker);
                    Ok(())
                })
                .unwrap();
            bridge.reject(id).unwrap();
            bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        }
        assert_eq!(bridge.worker_capacity, config.queue.lifecycle_capacity);
        assert_eq!(bridge.live_workers.len(), 2);

        let (overflow, _) = layer.enter(&mut bridge.state, 99, 999, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(overflow, 1, DndAction::Copy, overflow.0))
            .unwrap();
        bridge.request_data(overflow, "text/plain", now).unwrap();
        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::PayloadWorkerCapacityExceeded,
                ..
            })] if *transfer_id == overflow
        ));
        assert_eq!(bridge.live_workers.len(), 2);
        drop(parked_workers);
    }

    #[test]
    fn a_successful_payload_releases_the_worker_capacity_for_the_next_drag() {
        let now = Instant::now();
        let config = BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        };
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(config);

        let (first, _) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge
            .accept(acceptance(first, 1, DndAction::Copy, 1))
            .unwrap();
        bridge.request_data(first, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);
        assert!(
            bridge.live_workers.is_empty(),
            "a successful worker returns its capacity"
        );
        bridge.reject(first).unwrap();
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();

        let (second, _) = layer.enter(&mut bridge.state, 12, 202, true, ActionMask::COPY);
        let entered = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        let revision = entered
            .iter()
            .find_map(|event| match event {
                BridgeEvent::Entered {
                    transport_revision, ..
                } => Some(transport_revision.0),
                _ => None,
            })
            .expect("second drag entered");
        bridge
            .accept(acceptance(second, 1, DndAction::Copy, revision))
            .unwrap();
        bridge.request_data(second, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);
        assert!(
            bridge
                .active
                .as_ref()
                .is_some_and(|active| active.transfer.phase() == ReceivePhase::Ready),
            "the next successful payload is admitted under the released cap"
        );
    }

    #[test]
    fn a_terminal_transition_suppresses_the_following_action_event() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = enter_test_offer(&mut bridge, &log, now);
        bridge
            .state
            .enqueue(ProtocolEvent::Leave { transfer_id: id });
        bridge.state.enqueue(ProtocolEvent::SelectedAction {
            transfer_id: id,
            action: None,
            transport_revision: TransportRevision(2),
        });

        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::LeaveBeforeDrop,
                ..
            })] if *transfer_id == id
        ));
        assert!(log.borrow().finished.is_empty());
        assert!(log.borrow().final_actions.is_empty());
    }

    #[test]
    fn callback_transfer_lookup_is_keyed_per_device() {
        // SCTK 0.19.2 data_device.rs:137 destroys A before calling Enter B on
        // the same device. The bridge must retire A's device key even when B
        // is foreign and rejected, so B's later Drop cannot resurrect A.
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let layer = TestProxyLayer::new(&log);
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let (a, a_proxy) = layer.enter(&mut bridge.state, 11, 101, true, ActionMask::COPY);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(a, 1, DndAction::Copy, 1)).unwrap();
        bridge.request_data(a, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut bridge, now);
        layer.selected_action(&mut bridge.state, &a_proxy, Some(DndAction::Copy));
        layer.drop(&mut bridge.state, 11);
        layer.leave(&mut bridge.state, 11);
        bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        bridge.accept(acceptance(a, 1, DndAction::Copy, 2)).unwrap();
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .iter()
                .any(|event| matches!(event, BridgeEvent::Drop(_)))
        );

        assert_eq!(
            bridge
                .state
                .offer_transfers
                .transfer_for(&CallbackIdentity::test(101)),
            Some(a),
            "the dropped offer key survives post-drop Leave"
        );
        let (b, _) = layer.enter(&mut bridge.state, 11, 202, false, ActionMask::COPY);
        assert_eq!(
            bridge
                .state
                .device_transfers
                .transfer_for(&CallbackIdentity::test(11)),
            Some(b),
            "replacement Enter retires A's device key at callback time"
        );
        assert_eq!(
            bridge
                .state
                .offer_transfers
                .transfer_for(&CallbackIdentity::test(101)),
            Some(a),
            "A's offer key remains until the reserved terminal is driven"
        );

        assert!(matches!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                disposition: TerminalDisposition::Rejected,
                reason: TerminalReason::OfferReplaced,
            })] if *transfer_id == a
        ));
        assert!(!a_proxy.state.alive.get());
        assert_eq!(log.borrow().accepted.last(), Some(&(202, None)));

        layer.drop(&mut bridge.state, 11);
        assert!(
            bridge
                .run_frame(now, |_| Ok(FlushStatus::Flushed))
                .unwrap()
                .is_empty(),
            "B's post-rejection Drop has no device correlation to A or B"
        );

        // The same correlation must survive a completed transfer whose finish
        // flush is pending, even when the replacement Enter itself overflows.
        let pending_log = Rc::new(RefCell::new(TestOfferLog::default()));
        let pending_layer = TestProxyLayer::new(&pending_log);
        let mut pending = WaylandBridge::for_frame_test(BridgeConfig {
            queue: QueueConfig {
                lifecycle_capacity: 1,
                ..QueueConfig::default()
            },
            ..BridgeConfig::default()
        });
        let (old, old_proxy) =
            pending_layer.enter(&mut pending.state, 44, 301, true, ActionMask::COPY);
        pending
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        pending
            .accept(acceptance(old, 1, DndAction::Copy, 1))
            .unwrap();
        pending.request_data(old, "text/plain", now).unwrap();
        wait_until_payload_ready(&mut pending, now);
        pending_layer.selected_action(&mut pending.state, &old_proxy, Some(DndAction::Copy));
        pending_layer.drop(&mut pending.state, 44);
        pending
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        pending
            .accept(acceptance(old, 1, DndAction::Copy, 2))
            .unwrap();
        let dropped = pending
            .run_frame(now, |_| Ok(FlushStatus::Flushed))
            .unwrap();
        assert!(
            dropped
                .iter()
                .any(|event| matches!(event, BridgeEvent::Drop(_)))
        );
        pending.test_flushes.push_back(Ok(FlushStatus::WouldBlock));
        pending
            .complete_drop(
                old,
                DropComplete {
                    delivery_id: crate::types::DeliveryId(9),
                    outcome: crate::types::DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert!(pending.pending_completion.is_some());
        assert_eq!(
            pending
                .state
                .device_transfers
                .transfer_for(&CallbackIdentity::test(44)),
            Some(old),
            "pending completion retains its device correlation"
        );

        pending.state.enqueue(ProtocolEvent::Worker(WorkerResult {
            transfer_id: DataTransferId(999),
            payload: Err(PayloadFailure::Pipe),
        }));
        let (replacement, _) =
            pending_layer.enter(&mut pending.state, 44, 302, false, ActionMask::COPY);
        assert_eq!(replacement, DataTransferId(2));
        assert!(matches!(
            pending
                .run_frame(now, |_| Ok(FlushStatus::WouldBlock))
                .unwrap()
                .as_slice(),
            [BridgeEvent::Terminal(TerminalEvent {
                transfer_id,
                reason: TerminalReason::OfferReplaced,
                ..
            })] if *transfer_id == old
        ));
        assert!(pending.pending_completion.is_none());
        assert!(!old_proxy.state.alive.get());
        assert_eq!(pending_log.borrow().accepted.last(), Some(&(302, None)));
    }

    #[test]
    fn only_revisions_returned_to_the_consumer_raise_the_delivery_ceiling() {
        let now = Instant::now();
        let log = Rc::new(RefCell::new(TestOfferLog::default()));
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let id = enter_test_offer(&mut bridge, &log, now);
        bridge.state.enqueue(ProtocolEvent::Motion {
            transfer_id: id,
            position: Position { x: 1.0, y: 2.0 },
            transport_revision: TransportRevision(7),
        });

        assert_eq!(
            bridge.accept(acceptance(id, 1, DndAction::Copy, 7)),
            Err(BridgeError::InvalidAcceptance(
                AcceptanceError::UnobservedTransportRevision {
                    observed: TransportRevision(7),
                    latest_delivered: TransportRevision(1),
                }
            ))
        );
        let events = bridge.run_frame(now, |_| Ok(FlushStatus::Flushed)).unwrap();
        assert!(matches!(
            events.as_slice(),
            [BridgeEvent::Motion {
                transport_revision: TransportRevision(7),
                ..
            }]
        ));
        bridge
            .accept(acceptance(id, 1, DndAction::Copy, 7))
            .unwrap();
    }

    fn send_cancel_pair(cancelled: bool) -> (SendCancel, SendCancelWorker) {
        let (bridge_wake, worker_wake) = UnixStream::pair().unwrap();
        let flag = Arc::new(AtomicBool::new(cancelled));
        (
            SendCancel {
                flag: Arc::clone(&flag),
                waker: bridge_wake,
            },
            SendCancelWorker {
                flag,
                wake: worker_wake,
            },
        )
    }

    #[test]
    fn lazy_send_closes_the_fd_before_success_is_observable() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        let owned: OwnedFd = writer.into();
        let pipe = WritePipe::from(owned);
        let raw_fd = pipe.as_raw_fd();
        let (_bridge_cancel, worker_cancel) = send_cancel_pair(false);

        assert_eq!(
            write_payload_and_close(pipe, b"payload", &worker_cancel, Duration::from_secs(1)),
            Ok(())
        );
        assert_eq!(
            // SAFETY: querying a numeric fd does not take ownership.
            unsafe { libc::fcntl(raw_fd, libc::F_GETFD) },
            -1,
            "worker must close before returning success"
        );
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"payload");
    }

    #[test]
    fn lazy_send_closes_the_fd_on_the_cancelled_error_path() {
        let (writer, _reader) = UnixStream::pair().unwrap();
        let owned: OwnedFd = writer.into();
        let pipe = WritePipe::from(owned);
        let raw_fd = pipe.as_raw_fd();
        let (_bridge_cancel, worker_cancel) = send_cancel_pair(true);

        assert_eq!(
            write_payload_and_close(pipe, b"payload", &worker_cancel, Duration::from_secs(1)),
            Err(())
        );
        assert_eq!(
            // SAFETY: querying a numeric fd does not take ownership.
            unsafe { libc::fcntl(raw_fd, libc::F_GETFD) },
            -1,
            "worker must close before returning failure"
        );
    }

    #[test]
    fn second_seat_cannot_replace_the_first_seats_grab_or_active_source() {
        let first = CallbackIdentity::test(1);
        let second = CallbackIdentity::test(2);
        assert!(seat_can_claim_grab(None, None, &first));
        assert!(!seat_can_claim_grab(Some(&first), None, &second));
        assert!(!seat_can_claim_grab(None, Some(&first), &second));
        assert!(seat_can_claim_grab(Some(&first), Some(&first), &first));
    }

    #[test]
    fn echo_origin_lookup_preserves_the_internal_source_identity() {
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        let nonce =
            TransferNonce::from_mime("application/x-cosmix-dnd-0123456789abcdef0123456789abcdef")
                .unwrap();
        bridge
            .nonce_registry
            .register(nonce.clone(), DataTransferId(1), SourceId(77))
            .unwrap();
        bridge
            .nonce_registry
            .attach_echo(&nonce, DataTransferId(2))
            .unwrap();
        assert_eq!(
            bridge.origin_for(DataTransferId(2)),
            DndOrigin::Internal(SourceId(77))
        );
        assert_eq!(
            bridge.origin_for(DataTransferId(3)),
            DndOrigin::External(DataTransferId(3))
        );
    }

    #[test]
    fn every_undrained_terminal_survives_and_bounds_the_next_start() {
        let mut bridge = WaylandBridge::for_frame_test(BridgeConfig::default());
        for index in 0..MAX_PENDING_TERMINALS {
            bridge
                .outgoing_terminals
                .push_back(OutgoingEvent::Terminal {
                    transfer_id: DataTransferId(index as u64 + 1),
                    reason: OutgoingTerminalReason::CompositorCancelled,
                });
        }
        // At the cap the honest answer is a refusal; dropping a terminal would
        // leave a consumer permanently unaware that a transfer ended.
        let payload = OutgoingPayload::from_paths(vec![PathBuf::from("/")]).unwrap();
        assert!(matches!(
            bridge.start_outgoing(
                SourceId(1),
                payload.clone(),
                ActionMask::COPY,
                Instant::now()
            ),
            Err(BridgeError::Send(SendError::UndrainedTerminals))
        ));

        bridge.outgoing_events.push_back(OutgoingEvent::DataSent {
            transfer_id: DataTransferId(99),
            mime_type: URI_LIST_MIME.into(),
        });
        let drained = bridge.drain_outgoing_events();
        let terminals: Vec<u64> = drained
            .iter()
            .filter_map(|event| match event {
                OutgoingEvent::Terminal { transfer_id, .. } => Some(transfer_id.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            terminals,
            (1..=MAX_PENDING_TERMINALS as u64).collect::<Vec<_>>()
        );
        assert!(matches!(
            drained.last(),
            Some(OutgoingEvent::DataSent { .. })
        ));

        // Drained, so the cap no longer bites: the next refusal is the ordinary
        // missing-grab one.
        assert!(matches!(
            bridge.start_outgoing(SourceId(1), payload, ActionMask::COPY, Instant::now()),
            Err(BridgeError::Send(SendError::NoHeldGrab))
        ));
        // A refused start must not leave the unstarted mark latched: it is keyed
        // by id, so a stale one would silently swallow a later transfer's
        // terminal — the very failure the mark exists to prevent.
        assert_eq!(bridge.unstarted_outgoing, None);
    }

    #[test]
    fn source_send_drains_before_dnd_finished_in_the_same_callback_batch() {
        let config = QueueConfig::default();
        let mut queue = BoundedEventQueue::new(config).unwrap();
        let (writer, _reader) = UnixStream::pair().unwrap();
        let owned: OwnedFd = writer.into();
        queue
            .enqueue(ProtocolEvent::SourceSend {
                transfer_id: DataTransferId(1),
                mime_type: URI_LIST_MIME.into(),
                pipe: WritePipe::from(owned),
            })
            .unwrap();
        queue
            .enqueue(ProtocolEvent::SourceFinished {
                transfer_id: DataTransferId(1),
            })
            .unwrap();
        let drained = queue.drain_frame();
        assert!(matches!(
            drained.as_slice(),
            [
                ProtocolEvent::SourceSend { .. },
                ProtocolEvent::SourceFinished { .. }
            ]
        ));
    }
}
