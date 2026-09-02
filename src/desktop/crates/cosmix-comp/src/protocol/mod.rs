//! Smithay protocol state and the narrow protocol-to-ECS bridge.

#[cfg(feature = "bus")]
mod corner;
#[cfg(feature = "bus")]
pub(crate) mod port_observation;
#[cfg(feature = "bus")]
pub(crate) mod port_snapshot;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet},
    env,
    error::Error,
    fs, mem,
    os::fd::{AsFd, BorrowedFd},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(test)]
use std::sync::atomic::AtomicU8;
#[cfg(all(test, feature = "bus"))]
use std::sync::atomic::AtomicU64;

#[cfg(test)]
use smithay::reexports::wayland_server::backend::GlobalId;

/// Re-exported for the live input adapter's anchor, and gated exactly as that
/// adapter is. Two names, not the module: the event path stays private to
/// `input`, and what leaves is only the factory type the backend must construct
/// and the shape it must construct it as.
#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) use self::input::{BoxedLibinputFactory, InputSourceFactory};
pub(crate) use self::release_use::DmabufUseId;
// Re-exported because the startup report travels out to whoever started the
// runtime, and a report whose cases cannot be named where it is read is the
// `Option` it replaced wearing a longer type. Only the enum: the identity and
// the refusal reason ride inside it and are read through their fields and
// `Debug`, so exporting their names too would widen the surface for nothing.
pub(crate) use self::explicit_sync::ExplicitSyncPreparation;
use self::{
    acquire_gate::{AcquireGateEngine, GateId, LinuxAcquireGatePlatform},
    explicit_sync::{ImportDeviceDecision, prepare_linux_import_device},
    release_use::{
        AbandonedUse, AddRendererOwnerDecision, BeginUseDecision, MAX_GLOBAL_DMABUF_USES,
        ReleaseUseAbandonReason, ReleaseUseEngine, ReleaseUseFailure, ReleaseUsePlatform,
        RetiredUse, RetirementUpdate, TerminalUse,
    },
};
use crate::{
    backend::{
        BackendData, BackendKind, CaptureSourceId, KmsBackendData, SeatRegion, WinitBackendData,
        kms::{KmsRenderCommand, KmsRenderReply},
    },
    bindings::{BindingAction, BindingProfile, BindingState, KeyDisposition},
    capture::{
        CaptureCancellation, CaptureDestination, CaptureDmabufComplete, CaptureDmabufDestination,
        CaptureDmabufFailed, CaptureFormat, CaptureId, CapturePixels, CapturePresented,
        CaptureRegion, CaptureRequest, CaptureReservationLease,
    },
    decoration::DecorationStartup,
};

#[cfg(feature = "bus")]
use crate::port::{
    PORT_QUEUE_CAPACITY, PortCommand, PortControl, PortProtocolWiring, PortRequest, PortStarter,
    PortWorker,
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use crate::backend::kms::KmsTopologyLifecycleEvent;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
use crate::capture::kms_capture_source_is_current;
use cosmix_deco::{
    CaptionButton, ChromeLayout, ChromePart, DecoExtents, DecoTheme, ResizeEdge as DecoResizeEdge,
    vec2,
};
use cosmix_wgpu_dmabuf::{
    DmabufBufferId, DmabufCapabilities, DmabufDescriptor, DmabufPlane, ReleaseCallback,
    RetirementBatchId, RetirementRequestError, RetirementRequestSender, RetirementSequence,
    RetirementWorker, RetirementWorkerError, RetirementWorkerReport, ValidateDmabuf,
    WaitForSubmittedWork, spawn_retirement_worker,
};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::{
    backend::allocator::{Buffer as _, Format, dmabuf::Dmabuf},
    backend::input::{Axis, AxisRelativeDirection, AxisSource, ButtonState, KeyState, TouchSlot},
    delegate_data_device, delegate_dmabuf, delegate_foreign_toplevel_list,
    delegate_fractional_scale, delegate_idle_notify, delegate_output, delegate_seat,
    delegate_session_lock, delegate_shm, delegate_viewporter,
    desktop::{
        LayerMap, LayerSurface as DesktopLayerSurface, PopupKeyboardGrab, PopupKind, PopupManager,
        PopupPointerGrab, find_popup_root_surface, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle, Keycode},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData, Focus, MotionEvent,
            PointerHandle,
        },
        touch::{
            DownEvent as TouchDownEvent, MotionEvent as TouchMotionEvent, UpEvent as TouchUpEvent,
        },
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, Mode as PollMode, PostAction,
            channel::{self, Event as ChannelEvent, Sender as CommandSender},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::{
            Error as SessionLockError, ExtSessionLockV1,
        },
        wayland_protocols::xdg::{
            decoration::zv1::server::{
                zxdg_decoration_manager_v1,
                zxdg_toplevel_decoration_v1::{self, Mode as DecorationMode},
            },
            shell::server::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base},
        },
        wayland_server::{
            Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource as _,
            WEnum,
            backend::{ClientData, ClientId, DisconnectReason, ObjectId, protocol::ProtocolError},
            protocol::{
                wl_buffer, wl_callback, wl_compositor, wl_output as wl_output_protocol, wl_region,
                wl_seat, wl_shm, wl_subcompositor, wl_subsurface,
                wl_surface::{self, WlSurface},
            },
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            self, BufferAssignment, Cacheable, CompositorClientState, CompositorHandler,
            CompositorState, Damage, RectangleKind, RegionUserData, SubsurfaceCachedState,
            SubsurfaceUserData, SurfaceAttributes, SurfaceUserData, TraversalAction,
            with_surface_tree_downward, with_surface_tree_upward,
        },
        dmabuf::{
            DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
            get_dmabuf,
        },
        drm_syncobj::{DrmSyncPoint, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        foreign_toplevel_list::{
            ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
        },
        fractional_scale::{self, FractionalScaleHandler, FractionalScaleManagerState},
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        output::{OutputHandler, OutputManagerState},
        seat::CURSOR_IMAGE_ROLE,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        session_lock::{
            LockSurface, LockSurfaceConfigure, SessionLockHandler, SessionLockManagerState,
            SessionLocker,
        },
        shell::wlr_layer::{
            Anchor, ExclusiveZone, KeyboardInteractivity, Layer as WlrLayer,
            LayerSurface as WlrLayerSurface, LayerSurfaceConfigure, LayerSurfaceData,
            WlrLayerShellGlobalData, WlrLayerShellHandler, WlrLayerShellState,
            WlrLayerSurfaceUserData,
        },
        shell::xdg::{
            Configure, PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface,
            XdgPopupSurfaceData, XdgPositionerUserData, XdgShellHandler, XdgShellState,
            XdgShellSurfaceUserData, XdgSurfaceUserData, XdgToplevelSurfaceData, XdgWmBaseUserData,
            decoration::{
                XdgDecorationHandler, XdgDecorationManagerGlobalData, XdgDecorationState,
            },
        },
        shm::{ShmHandler, ShmState, with_buffer_contents, with_buffer_contents_mut},
        socket::ListeningSocketSource,
        viewporter::{ViewportCachedState, ViewporterState, ensure_viewport_valid},
    },
};

const DEFAULT_TOPLEVEL_WIDTH: i32 = 640;
const DEFAULT_TOPLEVEL_HEIGHT: i32 = 420;
const DEFAULT_TOPLEVEL_OUTPUT_SHARE: f32 = 0.72;
const CASCADE_ORIGIN: f32 = 36.0;
const MAX_LAYER_GEOMETRY_VALUE: i64 = 1 << 24;
const CASCADE_STEP: f32 = 48.0;
const OUTPUT_MARGIN: f32 = 24.0;
const MAX_SURFACE_DIMENSION: u32 = 8192;
const MAX_SURFACE_BYTES: usize = 64 * 1024 * 1024;
// Live protocol objects and topology records consume memory without attaching
// a buffer, so their own generous hard caps are enforced at request ingress.
// Real clients remain far below these limits; refusal uses wl_display.no_memory
// because exhaustion, rather than protocol-invalid syntax, is the condition.
const MAX_CLIENT_SURFACES: usize = 4096;
pub(crate) const MAX_GLOBAL_SURFACES: usize = 16_384;
/// Compositor-owned title retention and render-work bound. Real desktop titles
/// are normally tens to low hundreds of characters; 1,024 Unicode scalar values
/// leaves ample room while bounding UTF-8 storage to 4 KiB and grapheme indexing
/// to at most 1,025 boundaries.
const MAX_TITLE_SCALARS: usize = 1_024;

fn capped_toplevel_title(title: &str) -> Arc<str> {
    let end = title
        .char_indices()
        .nth(MAX_TITLE_SCALARS)
        .map_or(title.len(), |(index, _)| index);
    Arc::from(&title[..end])
}
const MAX_SUBSURFACE_DEPTH: usize = 256;
// SHM limits count persistent converted backing. Renderer events share that
// backing by Arc; Bevy's required owned Image copy is made on the render thread.
// A browser may use hundreds of small surfaces; 256 MiB per client and 512 MiB
// globally still allow four/eight maximum-sized 64 MiB surfaces.
const MAX_CLIENT_SHM_BYTES: usize = 256 * 1024 * 1024;
const MAX_GLOBAL_SHM_BYTES: usize = 512 * 1024 * 1024;
const MAX_PENDING_EVENT_BYTES: usize = 512 * 1024 * 1024;
// The surface roster is a singleton whose worst case is bounded by the global
// surface cap alone, so its space is reserved out of the pending-event budget
// rather than competing for it. It exists to converge the renderer *after* an
// event was rejected, so an admission decision able to reject it in turn would
// defeat the only thing it is for.
// Must match what `protocol_event_retained_bytes` reports for a full roster —
// the vector's storage *and* the event itself — or the reservation is short by
// one `ProtocolEvent` and the claim is not the worst case.
const MAX_SURFACE_ROSTER_BYTES: usize =
    mem::size_of::<ProtocolEvent>() + MAX_GLOBAL_SURFACES * mem::size_of::<SurfaceId>();
const MAX_PENDING_SURFACE_EVENT_BYTES: usize = MAX_PENDING_EVENT_BYTES - MAX_SURFACE_ROSTER_BYTES;
const MAX_CLIENT_RETAINED_DMABUFS: usize = 256;
const MAX_GLOBAL_RETAINED_DMABUFS: usize = 1024;
const MAX_DMABUF_CACHE_IDENTITIES: usize = 64;
const MAX_PENDING_DMABUF_INVALIDATIONS: usize = 256;
const MAX_DAMAGE_RECTS: usize = 256;
const PRIMARY_POINTER_BUTTON: u32 = 0x110;
const TITLEBAR_DOUBLE_CLICK_MILLIS: u32 = 400;
const TITLEBAR_DOUBLE_CLICK_SLOP: f64 = 5.0;
const DMABUF_VALIDATION_QUEUE_CAPACITY: usize = 64;
const ECS_ACTION_QUEUE_CAPACITY: usize = 8;
const DIRTY_SURFACE_RECOVERY_BATCH: usize = 16;
const MAX_COMMITTED_INPUT_REGION_RECTS: usize = 256;
pub(crate) const MAX_CAPTURE_FRAMES: usize = 32;
pub(crate) const MAX_CLIENT_CAPTURE_REQUESTS: usize = 4;
pub(crate) const MAX_IN_FLIGHT_CAPTURES: usize = 8;
pub(crate) const MAX_CLIENT_CAPTURE_MANAGERS: usize = 8;
pub(crate) const MAX_GLOBAL_CAPTURE_MANAGERS: usize = 64;
pub(crate) const SCREENCOPY_MANAGER_ERROR_IMPLEMENTATION_LIMIT: u32 = 0;
pub(crate) const MAX_CLIENT_CAPTURE_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_GLOBAL_CAPTURE_BYTES: usize = 256 * 1024 * 1024;
const CAPTURE_SHM_BYTES_PER_TURN: usize = 256 * 1024;
/// Absolute lifetime of one admitted screencopy request. This is a request
/// deadline, not a periodic maintenance timer: a stuck Bevy/GPU completion
/// fails the one client operation. Once Bevy has extracted a screenshot, its
/// reservation remains charged until Bevy reports completion.
pub(crate) const CAPTURE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SurfaceId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum StackBand {
    Background,
    Bottom,
    #[default]
    Normal,
    Top,
    Overlay,
    Lock,
}

impl StackBand {
    const COUNT: usize = 6;

    const fn index(self) -> usize {
        match self {
            Self::Background => 0,
            Self::Bottom => 1,
            Self::Normal => 2,
            Self::Top => 3,
            Self::Overlay => 4,
            Self::Lock => 5,
        }
    }

    const fn for_layer(layer: WlrLayer) -> Self {
        match layer {
            WlrLayer::Background => Self::Background,
            WlrLayer::Bottom => Self::Bottom,
            WlrLayer::Top => Self::Top,
            WlrLayer::Overlay => Self::Overlay,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SurfaceStackKey {
    pub(crate) band: StackBand,
    pub(crate) sequence: u64,
    pub(crate) tree_index: u32,
}

impl SurfaceStackKey {
    const fn root(band: StackBand, sequence: u64) -> Self {
        Self {
            band,
            sequence,
            tree_index: 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn normal(sequence: u64) -> Self {
        Self::root(StackBand::Normal, sequence)
    }
}

/// Latest confined pointer coordinate shared with the renderer.
///
/// The four fields are one mutex-protected value so a renderer can never
/// combine one motion's coordinate with another motion's visibility or
/// revision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CursorPositionSnapshot {
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// False only while the nested host pointer is outside the compositor
    /// window. The retained cursor asset remains live, but capture must not
    /// overlay it until a later ordered motion re-enters the output.
    pub(crate) on_output: bool,
    pub(crate) revision: u64,
}

impl Default for CursorPositionSnapshot {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            on_output: true,
            revision: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceLayout {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) z: SurfaceStackKey,
    pub(crate) source: Option<TextureSourceRect>,
    pub(crate) parent: Option<SurfaceId>,
    pub(crate) transform: SurfaceTransform,
    /// Effective visibility, including every ancestor's mapping state.
    pub(crate) visible: bool,
    pub(crate) toplevel: Option<ToplevelSceneState>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SurfaceSceneSnapshot {
    pub(crate) layout: SurfaceLayout,
    pub(crate) kind: SceneSurfaceKind,
    pub(crate) title: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneSurfaceKind {
    Toplevel,
    Subsurface,
    Popup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneDecorationMode {
    Unbound,
    ClientSide,
    ServerSide,
}

#[derive(Clone, Copy, Debug, Default)]
struct SceneCommitCachedState {
    window_geometry_changed: bool,
    acknowledged_decoration: Option<SceneDecorationMode>,
    acknowledged_window_state: Option<WindowStateSnapshot>,
    decoration_reverts: bool,
    refresh_ancestor_window_geometry: bool,
}

impl Cacheable for SceneCommitCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        mem::take(self)
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

fn update_pending_scene_commit_state(
    surface: &WlSurface,
    update: impl FnOnce(&mut SceneCommitCachedState),
) {
    compositor::with_states(surface, |states| {
        update(
            states
                .cached_state
                .get::<SceneCommitCachedState>()
                .pending(),
        );
    });
}

fn current_scene_commit_state(surface: &WlSurface) -> SceneCommitCachedState {
    compositor::with_states(surface, |states| {
        if !states.cached_state.has::<SceneCommitCachedState>() {
            return SceneCommitCachedState::default();
        }
        *states
            .cached_state
            .get::<SceneCommitCachedState>()
            .current()
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ToplevelSceneState {
    pub(crate) decoration: SceneDecorationMode,
    pub(crate) focused: bool,
    pub(crate) committed_maximized: bool,
    pub(crate) window_geometry: SceneWindowGeometry,
    pub(crate) chrome_pointer: ChromePointerSceneState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChromePointerSceneState {
    pub(crate) hovered_button: Option<CaptionButton>,
    pub(crate) cluster_hovered: bool,
    pub(crate) pressed_button: Option<CaptionButton>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SceneWindowGeometry {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LogicalOutputRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl From<(u32, u32)> for LogicalOutputRect {
    fn from((width, height): (u32, u32)) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NormalRestore {
    window_origin: (f32, f32),
    client_size: (i32, i32),
    output: LogicalOutputRect,
    server_side: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WindowStateSnapshot {
    maximized: bool,
    window_origin: (f32, f32),
    client_size: (i32, i32),
    normal_restore: Option<NormalRestore>,
}

#[derive(Clone, Copy, Debug)]
struct ConfigureWindowStateSnapshot {
    serial: Serial,
    state: WindowStateSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceTransform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextureSourceRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Debug)]
pub(crate) struct ShmFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) opaque: bool,
    pub(crate) rgba: Arc<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct DmabufFrame {
    pub(crate) buffer_id: DmabufBufferId,
    pub(crate) cacheable: bool,
    pub(crate) token: u64,
    pub(crate) descriptor: DmabufDescriptor,
    pub(crate) use_id: Option<DmabufUseId>,
}

#[derive(Debug)]
pub(crate) enum SurfaceFrame {
    Shm(ShmFrame),
    Dmabuf(DmabufFrame),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CursorPresentation {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) source: Option<TextureSourceRect>,
    pub(crate) transform: SurfaceTransform,
}

/// Renderer-facing cursor image state.
#[derive(Debug)]
pub(crate) enum CursorImage {
    Default,
    Hidden,
    Chrome(ChromeCursorIcon),
    Surface {
        id: ObjectId,
        hotspot: (i32, i32),
        presentation: CursorPresentation,
        frame: Option<SurfaceFrame>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChromeCursorIcon {
    Move,
    NResize,
    NeResize,
    EResize,
    SeResize,
    SResize,
    SwResize,
    WResize,
    NwResize,
}

#[derive(Debug)]
pub(crate) enum ProtocolEvent {
    /// Compositor-owned session-lock cover state. An active event installs an
    /// opaque blank before any lock-surface content and names the security
    /// epoch a renderer must report only after presenting it.
    SecurityScene {
        active: bool,
        presentation_epoch: Option<u64>,
        presentations: Vec<SecurityPresentationTarget>,
    },
    OutputResized {
        width: u32,
        height: u32,
    },
    SurfaceUpserted {
        id: SurfaceId,
        scene: SurfaceSceneSnapshot,
        frame: SurfaceFrame,
    },
    SurfaceRelayout {
        id: SurfaceId,
        scene: SurfaceSceneSnapshot,
    },
    SurfaceUnmapped {
        id: SurfaceId,
    },
    SurfaceDestroyed {
        id: SurfaceId,
    },
    /// The authoritative set of surfaces the renderer should be showing.
    ///
    /// Applying it means: remove every renderer-side Wayland surface whose id
    /// is absent from `mapped`, and leave the listed ones untouched. It is a
    /// singleton, and `PendingProtocolEvents::take` emits it ahead of every
    /// per-surface event in the same batch — a stale upsert applied after the
    /// roster would recreate exactly the entity the roster just removed.
    ///
    /// This is the recovery route for a *rejected* removal. `dirty_surfaces`
    /// cannot carry one: it feeds [`WaylandState::latest_surface_upsert`],
    /// which answers `Gone` for a surface that no longer exists, so re-marking
    /// a lost tombstone only drops the mark again. Membership, unlike a
    /// per-surface delta, is idempotent and needs no record of the departed.
    SurfaceRoster {
        mapped: Vec<SurfaceId>,
    },
    CursorUpdated {
        image: CursorImage,
    },
    /// A protocol `wl_buffer` was destroyed. Cached GPU backing for this
    /// identity must not survive to alias a later resource.
    DmabufBufferDestroyed {
        buffer_id: DmabufBufferId,
    },
    /// A bounded epoch tombstone subsuming every individual DMA-BUF cache
    /// invalidation queued before it.
    DmabufCacheInvalidated,
    /// Ordered, lossless capture work. `PendingProtocolEvents::take` emits
    /// these after every scene/topology mutation retained in the same batch.
    CaptureRequested(CaptureRequest),
    CaptureDamageWatch(crate::capture::CaptureDamageWatch),
    /// Retain only these current KMS source generations in the main-world
    /// damage journal. Unchanged outputs keep their revision history.
    CaptureKmsSourcesRetired {
        current: BTreeMap<crate::backend::kms::OutputKey, u64>,
    },
    RuntimeFailed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
pub(crate) enum SecurityPresentationScene {
    Lock,
    Blank,
    Client,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SecurityPresentationTarget {
    pub(crate) output: String,
    pub(crate) scene: SecurityPresentationScene,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EcsAction {
    ExitNestedCompositor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostButtonState {
    Pressed,
    Released,
}

/// One seat operation, in the compositor's own terms.
///
/// Both input transports produce this type and nothing else: the nested winit
/// backend batches a `Vec<HostInput>` into a Bevy frame command, and the
/// bare-metal backend routes one at a time straight off the protocol calloop
/// (`protocol::input`). Only the transports differ — every seat policy decision
/// lives behind [`WaylandState::handle_host_input`], so a bare-metal-only
/// pointer or keyboard path cannot exist without deleting a variant here.
///
/// Timestamps are carried rather than sampled at dispatch: libinput reports the
/// time the device produced the event, which is not the time the compositor got
/// round to it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HostInput {
    /// Absolute pointer position in compositor coordinates.
    PointerMotionAbsolute {
        x: f64,
        y: f64,
        time: u32,
    },
    /// Accelerated relative motion. The handler adds it to the current cursor
    /// position and clamps to the output; a transport never tracks the cursor
    /// itself, or the two would drift apart.
    ///
    /// Only a relative device produces this, so only the bare-metal transport
    /// constructs it — the nested one is handed absolute host coordinates.
    PointerMotion {
        dx: f64,
        dy: f64,
        time: u32,
    },
    /// The host pointer left the nested compositor window. There is no client
    /// coordinate to deliver, but compositor-owned pointer observations must
    /// tear down immediately because no later motion sample is guaranteed.
    PointerLeave,
    PointerButton {
        button: u32,
        state: HostButtonState,
        time: u32,
    },
    /// One scroll event, carrying each axis only if the device reported it.
    ///
    /// The two axes are separate `Option`s rather than a pair of numbers
    /// because libinput reports them independently: an ordinary vertical wheel
    /// event has no horizontal axis at all
    /// (`vendor/smithay/src/backend/libinput/mod.rs:170-219` gates both `amount`
    /// and `amount_v120` on `has_axis`). Collapsing an unreported axis to zero
    /// makes it indistinguishable from a device reporting that the axis has
    /// stopped, which is a real event with real consequences — see
    /// [`WaylandState::pointer_axis`].
    PointerAxis {
        horizontal: Option<HostAxis>,
        vertical: Option<HostAxis>,
        source: AxisSource,
        relative_direction: (AxisRelativeDirection, AxisRelativeDirection),
        time: u32,
    },
    Key {
        keycode: Keycode,
        state: HostButtonState,
        time: u32,
    },
    KeyboardFocusLost,
    /// A device reporting a touch capability was attached.
    ///
    /// Unlike the keyboard and the pointer, the touch capability is *not*
    /// created with the compositor. `wl_seat.capabilities` is a promise about
    /// hardware, and a client that sees the touch bit is entitled to bind
    /// `wl_touch` and wait forever. So the capability tracks device presence,
    /// which means arrival and departure have to be seat commands rather than
    /// something the conversion can drop.
    TouchDeviceAdded,
    /// A device reporting a touch capability was detached.
    ///
    /// The counterpart of [`HostInput::TouchDeviceAdded`]. Unlike the keyboard's
    /// unreconciled held-key gap, this one is closed: withdrawing the capability
    /// cancels any live touch session first, so a client is never left holding
    /// contacts that can no longer be released.
    TouchDeviceRemoved,
    /// A new contact, positioned in compositor coordinates.
    ///
    /// Absolute like [`HostInput::PointerMotionAbsolute`] and confined the same
    /// way, but it does **not** move the cursor: a touchscreen and a pointer are
    /// two devices, and warping the visible cursor to a fingertip would be a
    /// fiction the pointer's own clients would then see.
    TouchDown {
        slot: TouchSlot,
        x: f64,
        y: f64,
        time: u32,
    },
    TouchMotion {
        slot: TouchSlot,
        x: f64,
        y: f64,
        time: u32,
    },
    /// A contact was lifted. Carries no position, because a device does not
    /// report one — `wl_touch.up` has no coordinates and neither does Smithay's
    /// `UpEvent`.
    TouchUp {
        slot: TouchSlot,
        time: u32,
    },
    /// The end of one set of simultaneous touch changes.
    ///
    /// Carries neither slot nor timestamp: `wl_touch.frame` has no arguments and
    /// is about the batch, not about any one contact.
    TouchFrame,
    /// The compositor has taken the touch stream over; every contact ends now.
    ///
    /// Not per-slot: `wl_touch.cancel` ends the whole touch session for a
    /// client. Also the hook rung F submits on session pause rather than
    /// reaching around [`WaylandState::handle_host_input`] — a paused VT and a
    /// device-initiated cancel want exactly the same seat policy, and having two
    /// ways to spell it is how they drift apart.
    TouchCancel,
    OutputResized {
        width: u32,
        height: u32,
    },
    OutputScaleChanged {
        scale: f64,
    },
}

/// One axis of a scroll event, exactly as the device reported it.
///
/// Held behind an `Option` at the use site so three states stay distinct:
/// absent (the device said nothing about this axis), a non-zero amount (a
/// scroll), and a reported zero (the sequence on this axis has ended). The
/// first and the third are not interchangeable — inventing a zero for an
/// unreported axis sends a `wl_pointer.axis_stop` the hardware never produced,
/// and discarding a reported zero withholds the only event that ends a client's
/// kinetic scrolling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HostAxis {
    /// Continuous amount in the units `wl_pointer.axis` carries.
    pub(crate) amount: f64,
    /// Discrete motion in 120ths of a detent. Only wheel-like sources promise
    /// it, so `None` here means "this device does not do detents", not zero.
    pub(crate) v120: Option<i32>,
}

impl HostInput {
    /// The compositor's only evdev-to-XKB keycode conversion.
    ///
    /// The two differ by a constant 8, and Smithay's libinput backend has
    /// already applied it by the time an event reaches us
    /// (`KeyboardKeyEvent::key_code` in `backend/libinput/mod.rs` returns
    /// `key() + 8`). The bare-metal path must therefore not convert again,
    /// while the nested winit path — which receives raw evdev codes from the
    /// host — must. Both would compile and both would look right; the symptom
    /// is every key arriving as a different key. Keeping the offset in one
    /// constructor is what stops the two transports disagreeing, so a caller
    /// that already holds a [`Keycode`] must build the variant directly rather
    /// than round-tripping through here.
    pub(crate) fn key_from_evdev(evdev_code: u32, state: HostButtonState, time: u32) -> Self {
        Self::Key {
            keycode: Keycode::new(evdev_code + 8),
            state,
            time,
        }
    }
}

enum ProtocolCommand {
    Frame {
        inputs: Vec<HostInput>,
    },
    ReleaseDmabuf {
        token: u64,
    },
    KmsRenderReply {
        reply: KmsRenderReply,
    },
    SecurityPresented {
        presentation_epoch: u64,
        evidence: SecurityPresentationEvidence,
    },
    CapturePixels(CapturePixels),
    CaptureDmabufComplete(CaptureDmabufComplete),
    CaptureDmabufFailed(CaptureDmabufFailed),
    CapturePresented(CapturePresented),
    CaptureDamageEligible {
        id: CaptureId,
        generation: u64,
        security_epoch: u64,
        revision: u64,
        damage: Vec<CaptureRegion>,
    },
    CaptureFailed {
        id: CaptureId,
        generation: u64,
        security_epoch: u64,
    },
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    KmsTopologyLifecycle {
        event: KmsTopologyLifecycleEvent,
        acknowledgement: SyncSender<Result<(), String>>,
    },
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    QuerySessionLockActive {
        acknowledgement: SyncSender<bool>,
    },
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    FlushEvents {
        acknowledgement: SyncSender<Result<EventFlushOutcome, String>>,
    },
    #[cfg(test)]
    Barrier {
        acknowledgement: SyncSender<()>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SecurityPresentationEvidence {
    Nested {
        output: String,
    },
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    Kms {
        generation: u64,
        output: crate::backend::kms::OutputKey,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolShutdownCause {
    Orderly,
    RuntimeFailure,
}

impl ProtocolShutdownCause {
    fn release_use_abandon_reason(self) -> ReleaseUseAbandonReason {
        match self {
            Self::Orderly => ReleaseUseAbandonReason::OrderlyShutdown,
            Self::RuntimeFailure => ReleaseUseAbandonReason::DispatchFailure,
        }
    }
}

/// Whether a completed protocol run represents an internal failure that its
/// owner must hear about.
fn protocol_exit_failed(
    run: &Result<(), String>,
    shutdown_cause: Option<ProtocolShutdownCause>,
) -> bool {
    run.is_err() || matches!(shutdown_cause, Some(ProtocolShutdownCause::RuntimeFailure))
}

struct DmabufValidationRequest {
    descriptor: DmabufDescriptor,
    notifier: ImportNotifier,
    format: Format,
}

fn assert_dmabuf_validation_off_protocol_thread() {
    let current = thread::current();
    assert_ne!(
        current.name(),
        Some("cosmix-wayland"),
        "Vulkan DMA-BUF validation must never execute on the protocol thread"
    );
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            thread_name = current.name().unwrap_or("<unnamed>"),
            thread_id = ?current.id(),
            "Vulkan DMA-BUF validation is isolated from the protocol thread"
        );
    }
}

fn spawn_dmabuf_validation_worker(
    mut validator: Box<dyn ValidateDmabuf>,
    wake: channel::Sender<()>,
) -> Result<SyncSender<DmabufValidationRequest>, String> {
    let (sender, receiver) =
        mpsc::sync_channel::<DmabufValidationRequest>(DMABUF_VALIDATION_QUEUE_CAPACITY);
    thread::Builder::new()
        .name("cosmix-dmabuf-validate".into())
        .spawn(move || {
            // Worker-local rather than shared with the protocol thread: a
            // handler that consulted it could only skip work the worker is
            // about to refuse anyway, so the flag would be doing nothing an
            // offline test could ever see.
            let mut poisoned = false;
            while let Ok(request) = receiver.recv() {
                assert_dmabuf_validation_off_protocol_thread();
                let DmabufValidationRequest {
                    descriptor,
                    notifier,
                    format,
                } = request;
                // A poisoned probe is never called again, but the loop keeps
                // running: `ImportNotifier`'s destructor only logs, so a worker
                // that returned here would leave every queued client waiting on
                // an event that can no longer arrive. Draining and refusing is
                // what tells them.
                if poisoned {
                    notifier.failed();
                } else {
                    match catch_unwind(AssertUnwindSafe(|| validator.validate(descriptor))) {
                        Err(_) => {
                            // An ordinary `Err` means this buffer is unusable; a
                            // panic means the probe itself is. Its state after an
                            // unwind is not something this compositor can reason
                            // about, so it is retired permanently rather than
                            // called again on the next client's descriptor.
                            poisoned = true;
                            tracing::error!(
                                ?format,
                                "DMA-BUF validation probe panicked; refusing every further import"
                            );
                            notifier.failed();
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                ?format,
                                %error,
                                "Vulkan test import rejected DMA-BUF parameters"
                            );
                            notifier.failed();
                        }
                        Ok(Ok(())) => {
                            if let Err(error) = notifier.successful::<WaylandState>() {
                                tracing::debug!(
                                    %error,
                                    "DMA-BUF client destroyed params during import"
                                );
                            }
                        }
                    }
                }
                // Both `failed` and `successful` only *queue* the event onto the
                // client's connection; the protocol thread flushes solely from
                // inside a dispatch cycle, and a client that sent the falliable
                // `create` and is now waiting for its answer — the whole point
                // of that request — sends nothing more to wake it. Without this
                // the outcome sits unflushed until unrelated traffic happens
                // along. The callback is empty because waking is the entire job.
                //
                // A closed channel means the protocol thread is already gone, so
                // there is nothing left to wake.
                let _ = wake.send(());
            }
        })
        .map_err(|error| format!("failed to start DMA-BUF validation worker: {error}"))?;
    Ok(sender)
}

const PROTOCOL_EVENT_BATCH_CAPACITY: usize = 2;
const PROTOCOL_THREAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct ProtocolThreadCompletion(SyncSender<()>);

impl Drop for ProtocolThreadCompletion {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Publish an unexpected protocol-thread exit after every value owned by that
/// thread has been destroyed.
///
/// The callback is a generic boundary rather than a KMS revocation sender:
/// this module does not own the live-session vocabulary. Declared before the
/// server and dropped after it, so a notification means the thread has stopped
/// serving and has finished destroying its protocol state, including a panic
/// path. An orderly shutdown disarms it only after that destruction.
struct ProtocolThreadFailure(Option<Box<dyn FnOnce() + Send>>);

impl ProtocolThreadFailure {
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for ProtocolThreadFailure {
    fn drop(&mut self) {
        if let Some(notify) = self.0.take() {
            notify();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolThreadJoinOutcome {
    Joined,
    Panicked,
    TimedOut,
}

fn join_protocol_thread_after_completion(
    thread: JoinHandle<()>,
    completion: &Receiver<()>,
    timeout: Duration,
) -> ProtocolThreadJoinOutcome {
    match completion.recv_timeout(timeout) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
            Ok(()) => ProtocolThreadJoinOutcome::Joined,
            Err(_) => ProtocolThreadJoinOutcome::Panicked,
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            drop(thread);
            ProtocolThreadJoinOutcome::TimedOut
        }
    }
}

/// Main-world handle for the independently driven Wayland protocol thread.
///
/// Smithay itself remains single-threaded: its display, calloop event loop and
/// all protocol state are created and used only by `cosmix-wayland`. Channels
/// are the sole ownership boundary with Bevy.
#[derive(bevy::prelude::Resource)]
pub(crate) struct WaylandRuntime {
    commands: CommandSender<ProtocolCommand>,
    client_scene_feed: Option<ClientSceneFeed>,
    ecs_actions: Mutex<Receiver<EcsAction>>,
    kms_render_commands: Arc<Mutex<Receiver<KmsRenderCommand>>>,
    thread: Option<JoinHandle<()>>,
    thread_completion: Option<Mutex<Receiver<()>>>,
    #[cfg(feature = "bus")]
    port_starter: Option<PortStarter>,
    #[cfg(feature = "bus")]
    port_worker: Option<PortWorker>,
    /// What the protocol thread reported about explicit sync as it came up.
    ///
    /// `None` exactly when no protocol thread was started, which is only the
    /// two `#[cfg(test)]` constructors below that fabricate a runtime around
    /// bare channels — the same condition `thread: None` records. Every runtime
    /// that started a thread carries `Some`, because the readiness reply the
    /// thread sends *is* this report and construction does not return without
    /// one.
    explicit_sync_startup: Option<ExplicitSyncStartupReport>,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    input_lifecycle: Option<input::InputLifecycleClient>,
    #[cfg(test)]
    _test_channels: Option<TestRuntimeChannels>,
}

/// The renderer-side half of the protocol event channel.
///
/// This is deliberately narrower than [`WaylandRuntime`]: a scene App can
/// receive client pixels and return DMA-BUF ownership without gaining frame,
/// input, topology, or shutdown control over the protocol thread.
#[derive(bevy::prelude::Resource)]
pub(crate) struct ClientSceneFeed {
    events: Mutex<Receiver<Vec<ProtocolEvent>>>,
    commands: CommandSender<ProtocolCommand>,
    cursor_position: Arc<Mutex<CursorPositionSnapshot>>,
    #[cfg(test)]
    _test_command_source: Option<Mutex<channel::Channel<ProtocolCommand>>>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
static_assertions::assert_not_impl_any!(ClientSceneFeed: Clone, Copy);

impl ClientSceneFeed {
    fn new(
        events: Receiver<Vec<ProtocolEvent>>,
        commands: CommandSender<ProtocolCommand>,
        cursor_position: Arc<Mutex<CursorPositionSnapshot>>,
    ) -> Self {
        Self {
            events: Mutex::new(events),
            commands,
            cursor_position,
            #[cfg(test)]
            _test_command_source: None,
        }
    }

    pub(crate) fn drain_events(&self) -> Result<Vec<ProtocolEvent>, String> {
        let events = self
            .events
            .lock()
            .map_err(|_| "Wayland protocol event receiver was poisoned".to_string())?;
        let mut drained = Vec::new();
        loop {
            match events.try_recv() {
                Ok(mut batch) => drained.append(&mut batch),
                Err(TryRecvError::Empty) => return Ok(drained),
                Err(TryRecvError::Disconnected) => {
                    return if drained.is_empty() {
                        Err("Wayland protocol thread disconnected".into())
                    } else {
                        Ok(drained)
                    };
                }
            }
        }
    }

    pub(crate) fn dmabuf_release_callback(&self, token: u64) -> ReleaseCallback {
        let commands = self.commands.clone();
        Box::new(move || {
            if commands
                .send(ProtocolCommand::ReleaseDmabuf { token })
                .is_err()
            {
                tracing::debug!(token, "protocol thread gone before DMA-BUF release");
            }
        })
    }

    pub(crate) fn capture_completion_reporter(&self) -> CaptureCompletionReporter {
        CaptureCompletionReporter {
            commands: self.commands.clone(),
        }
    }

    pub(crate) fn cursor_position(&self) -> CursorPositionSnapshot {
        *self
            .cursor_position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> (SyncSender<Vec<ProtocolEvent>>, Self) {
        let (commands, command_source) = channel::channel();
        let (sender, events) = mpsc::sync_channel(PROTOCOL_EVENT_BATCH_CAPACITY);
        let cursor_position = Arc::new(Mutex::new(CursorPositionSnapshot::default()));
        (
            sender,
            Self {
                events: Mutex::new(events),
                commands,
                cursor_position,
                _test_command_source: Some(Mutex::new(command_source)),
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn set_cursor_position_for_test(&self, snapshot: CursorPositionSnapshot) {
        *self
            .cursor_position
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
    }

    #[cfg(test)]
    pub(crate) fn released_dmabuf_tokens_for_test(&self) -> Vec<u64> {
        let commands = self
            ._test_command_source
            .as_ref()
            .expect("test scene feed retains its command source")
            .lock()
            .expect("test scene command source mutex poisoned");
        let mut tokens = Vec::new();
        loop {
            match commands.try_recv() {
                Ok(ProtocolCommand::ReleaseDmabuf { token }) => tokens.push(token),
                Ok(_) => panic!("scene feed emitted an unrelated command"),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return tokens,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn capture_outcomes_for_test(&self) -> Vec<CaptureTestOutcome> {
        let commands = self
            ._test_command_source
            .as_ref()
            .expect("test scene feed retains its command source")
            .lock()
            .expect("test scene command source mutex poisoned");
        let mut outcomes = Vec::new();
        loop {
            match commands.try_recv() {
                Ok(ProtocolCommand::CapturePixels(pixels)) => {
                    outcomes.push(CaptureTestOutcome::Pixels(pixels.id));
                }
                Ok(ProtocolCommand::CaptureDmabufComplete(completion)) => {
                    outcomes.push(CaptureTestOutcome::Pixels(completion.id));
                }
                Ok(ProtocolCommand::CaptureDmabufFailed(failure)) => {
                    outcomes.push(CaptureTestOutcome::Failed(failure.id));
                }
                Ok(ProtocolCommand::CapturePresented(presented)) => {
                    outcomes.push(CaptureTestOutcome::Presented {
                        id: presented.id,
                        seconds: presented.seconds,
                        nanoseconds: presented.nanoseconds,
                    });
                }
                Ok(ProtocolCommand::CaptureFailed { id, .. }) => {
                    outcomes.push(CaptureTestOutcome::Failed(id));
                }
                Ok(_) => panic!("scene feed emitted an unrelated command"),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                    return outcomes;
                }
            }
        }
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CaptureTestOutcome {
    Pixels(CaptureId),
    Presented {
        id: CaptureId,
        seconds: u64,
        nanoseconds: u32,
    },
    Failed(CaptureId),
}

/// Coordinator-side capability for advancing client frame callbacks.
///
/// Unlike [`WaylandRuntime`], this cloneable seam can send only an empty frame
/// boundary. The live coordinator owns it; render Apps cannot inject input or
/// gain topology and shutdown authority through it.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone)]
pub(crate) struct ClientFrameClock {
    commands: CommandSender<ProtocolCommand>,
}

/// Renderer-to-protocol acknowledgement for a security presentation epoch.
///
/// Nested mode reports only after its swapchain image was presented and the
/// submitted GPU work completed through this interface.
/// The KMS displayed-frame wiring intentionally lands here in session-lock
/// slice 2; enqueuing or submitting a frame is not sufficient evidence.
#[derive(Clone)]
pub(crate) struct SecurityPresentationReporter {
    commands: CommandSender<ProtocolCommand>,
}

impl SecurityPresentationReporter {
    pub(crate) fn presented(
        &self,
        presentation_epoch: u64,
        output: impl Into<String>,
    ) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::SecurityPresented {
                presentation_epoch,
                evidence: SecurityPresentationEvidence::Nested {
                    output: output.into(),
                },
            })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn kms_presented(
        &self,
        presentation_epoch: u64,
        generation: u64,
        output: crate::backend::kms::OutputKey,
    ) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::SecurityPresented {
                presentation_epoch,
                evidence: SecurityPresentationEvidence::Kms { generation, output },
            })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }
}

/// Narrow cloneable return path for capture map/presentation completion.
/// Wayland resources remain owned by the protocol thread.
#[derive(Clone, bevy::prelude::Resource)]
pub(crate) struct CaptureCompletionReporter {
    commands: CommandSender<ProtocolCommand>,
}

impl CaptureCompletionReporter {
    pub(crate) fn damage_eligible(
        &self,
        id: CaptureId,
        generation: u64,
        security_epoch: u64,
        revision: u64,
        damage: Vec<CaptureRegion>,
    ) {
        if self
            .commands
            .send(ProtocolCommand::CaptureDamageEligible {
                id,
                generation,
                security_epoch,
                revision,
                damage,
            })
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture damage eligibility");
        }
    }

    pub(crate) fn pixels(&self, pixels: CapturePixels) {
        if self
            .commands
            .send(ProtocolCommand::CapturePixels(pixels))
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture pixels");
        }
    }

    pub(crate) fn dmabuf_complete(&self, completion: CaptureDmabufComplete) {
        if self
            .commands
            .send(ProtocolCommand::CaptureDmabufComplete(completion))
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture DMA-BUF completion");
        }
    }

    pub(crate) fn dmabuf_failed(&self, failure: CaptureDmabufFailed) {
        if self
            .commands
            .send(ProtocolCommand::CaptureDmabufFailed(failure))
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture DMA-BUF failure");
        }
    }

    pub(crate) fn presented(&self, presented: CapturePresented) {
        if self
            .commands
            .send(ProtocolCommand::CapturePresented(presented))
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture presentation");
        }
    }

    pub(crate) fn failed(&self, id: CaptureId, generation: u64, security_epoch: u64) {
        if self
            .commands
            .send(ProtocolCommand::CaptureFailed {
                id,
                generation,
                security_epoch,
            })
            .is_err()
        {
            tracing::debug!("protocol thread gone before capture failure");
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl ClientFrameClock {
    pub(crate) fn pulse(&self) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::Frame { inputs: Vec::new() })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, ClientFramePulseProbe) {
        let (commands, source) = channel::channel();
        (Self { commands }, ClientFramePulseProbe { source })
    }
}

#[cfg(test)]
pub(crate) struct ClientFramePulseProbe {
    source: channel::Channel<ProtocolCommand>,
}

#[cfg(test)]
impl ClientFramePulseProbe {
    pub(crate) fn drain(&self) -> usize {
        let mut pulses = 0;
        loop {
            match self.source.try_recv() {
                Ok(ProtocolCommand::Frame { inputs }) => {
                    assert!(
                        inputs.is_empty(),
                        "client clock sends empty frame boundaries"
                    );
                    pulses += 1;
                }
                Ok(_) => panic!("client frame clock emitted an unrelated command"),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return pulses,
            }
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone)]
pub(crate) struct KmsTopologyClient {
    commands: CommandSender<ProtocolCommand>,
    render_commands: Arc<Mutex<Receiver<KmsRenderCommand>>>,
    input_lifecycle: Option<input::InputLifecycleClient>,
}

/// Whether a protocol flush left renderer state compacted in the protocol
/// outbox because the bounded scene channel was still full.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventFlushOutcome {
    Complete,
    Pending,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl KmsTopologyClient {
    pub(crate) fn submit_render_reply(&self, reply: KmsRenderReply) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::KmsRenderReply { reply })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }

    pub(crate) fn submit_lifecycle(
        &self,
        event: KmsTopologyLifecycleEvent,
        timeout: Duration,
    ) -> Result<(), String> {
        let (acknowledgement, reply) = mpsc::sync_channel(1);
        self.commands
            .send(ProtocolCommand::KmsTopologyLifecycle {
                event,
                acknowledgement,
            })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())?;
        reply
            .recv_timeout(timeout)
            .map_err(|error| format!("Wayland topology acknowledgement failed: {error}"))?
    }

    pub(crate) fn drain_render_commands(&self) -> Result<Vec<KmsRenderCommand>, String> {
        drain_kms_render_commands(&self.render_commands)
    }

    /// Ask the protocol thread which lock lifecycle owns presentation now.
    ///
    /// Resume must make this decision before it offers retained scanout to the
    /// renderer. Reading a cached coordinator-side copy would create exactly
    /// the pause race this query exists to close.
    pub(crate) fn session_lock_active(&self, timeout: Duration) -> Result<bool, String> {
        let (acknowledgement, reply) = mpsc::sync_channel(1);
        self.commands
            .send(ProtocolCommand::QuerySessionLockActive { acknowledgement })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())?;
        reply
            .recv_timeout(timeout)
            .map_err(|error| format!("Wayland session-lock query failed: {error}"))
    }

    /// Wake the protocol loop and wait until its compacted renderer outbox has
    /// been offered to the scene channel.
    ///
    /// This is deliberately not a frame pulse: resume needs the newest client
    /// state before its first rendered update without completing any client's
    /// frame callback early.
    pub(crate) fn flush_events(&self, timeout: Duration) -> Result<EventFlushOutcome, String> {
        let (acknowledgement, reply) = mpsc::sync_channel(1);
        self.commands
            .send(ProtocolCommand::FlushEvents { acknowledgement })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())?;
        reply
            .recv_timeout(timeout)
            .map_err(|error| format!("Wayland event flush acknowledgement failed: {error}"))?
    }

    pub(crate) fn reconcile_and_suspend_input(&self, timeout: Duration) -> Result<(), String> {
        self.input_lifecycle
            .as_ref()
            .ok_or_else(|| "Wayland runtime has no input lifecycle source".to_string())?
            .reconcile_and_suspend(timeout)
    }

    pub(crate) fn resume_input(&self, timeout: Duration) -> Result<(), String> {
        self.input_lifecycle
            .as_ref()
            .ok_or_else(|| "Wayland runtime has no input lifecycle source".to_string())?
            .resume(timeout)
    }
}

#[cfg(test)]
struct TestRuntimeChannels {
    _command_source: Mutex<channel::Channel<ProtocolCommand>>,
    _event_sender: SyncSender<Vec<ProtocolEvent>>,
    _ecs_action_sender: SyncSender<EcsAction>,
    _kms_render_command_sender: Option<Sender<KmsRenderCommand>>,
}

struct WaylandInputWiring {
    source: Option<Box<dyn input::InputSourceRegistration>>,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    lifecycle: Option<input::InputLifecycleClient>,
    binding_profile: BindingProfile,
    vt_switch_requested: Option<Box<dyn Fn(u8) + Send>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) struct LiveInputWiring {
    source: Box<dyn input::InputSourceRegistration>,
    lifecycle: input::InputLifecycleClient,
    vt_switch_requested: Box<dyn Fn(u8) + Send>,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveInputWiring {
    pub(crate) fn new<V>(
        source: input::InputSourceFactory<input::BoxedLibinputFactory>,
        vt_switch_requested: V,
    ) -> Self
    where
        V: Fn(u8) + Send + 'static,
    {
        let (source, lifecycle) = input::lifecycle_input_source(source.0);
        Self {
            source: Box::new(source),
            lifecycle,
            vt_switch_requested: Box::new(vt_switch_requested),
        }
    }
}

impl WaylandRuntime {
    #[allow(clippy::too_many_arguments)] // explicit test/runtime wiring stays visible at construction
    pub(crate) fn new(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        dmabuf_capabilities: Option<DmabufCapabilities>,
        dmabuf_validator: Option<Box<dyn ValidateDmabuf>>,
        retirement_adapter: Option<Box<dyn WaitForSubmittedWork>>,
        capture_advertisements: crate::capture::CaptureAdvertisementRegistry,
        policy: WaylandRuntimePolicy,
    ) -> Result<Self, Box<dyn Error>> {
        Self::with_input_source(
            socket_name,
            backend_kind,
            output_size,
            WaylandGpuWiring {
                dmabuf_capabilities,
                dmabuf_validator,
                retirement_adapter,
                capture_advertisements,
            },
            policy,
            None,
            None,
        )
    }

    #[cfg(feature = "bus")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_production(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        dmabuf_capabilities: Option<DmabufCapabilities>,
        dmabuf_validator: Option<Box<dyn ValidateDmabuf>>,
        retirement_adapter: Option<Box<dyn WaitForSubmittedWork>>,
        capture_advertisements: crate::capture::CaptureAdvertisementRegistry,
        policy: WaylandRuntimePolicy,
        service: String,
    ) -> Result<Self, Box<dyn Error>> {
        let (port, starter) = crate::port::prepare(service, "nested", &policy.decoration)?;
        Self::with_input_source_port(
            socket_name,
            backend_kind,
            output_size,
            WaylandGpuWiring {
                dmabuf_capabilities,
                dmabuf_validator,
                retirement_adapter,
                capture_advertisements,
            },
            policy,
            None,
            None,
            Some(port),
            Some(starter),
        )
    }

    /// Start the protocol thread with one mandatory bare-metal input source.
    ///
    /// The source factory crosses the thread boundary, not the source it builds:
    /// libinput is not `Send` and must be constructed on the protocol thread.
    /// `protocol_failed` is disarmed for an explicit startup refusal and runs
    /// after the server has been destroyed when dispatch later stops
    /// unexpectedly; its concrete failure vocabulary remains with the live
    /// backend that supplies it.
    #[cfg(all(feature = "kms-live", not(test)))]
    #[cfg_attr(feature = "bus", allow(dead_code))]
    pub(crate) fn new_with_input_source<N>(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input: LiveInputWiring,
        protocol_failed: N,
    ) -> Result<Self, Box<dyn Error>>
    where
        N: FnOnce() + Send + 'static,
    {
        Self::with_input_source_and_bindings(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            policy,
            WaylandInputWiring {
                source: Some(input.source),
                lifecycle: Some(input.lifecycle),
                binding_profile: BindingProfile::KmsLive,
                vt_switch_requested: Some(input.vt_switch_requested),
            },
            Some(Box::new(protocol_failed)),
        )
    }

    #[cfg(all(feature = "bus", feature = "kms-live", not(test)))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_input_source_production<N>(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input: LiveInputWiring,
        protocol_failed: N,
        service: String,
    ) -> Result<Self, Box<dyn Error>>
    where
        N: FnOnce() + Send + 'static,
    {
        let (port, starter) = crate::port::prepare(service, "kms", &policy.decoration)?;
        Self::with_input_source_and_bindings_port(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            policy,
            WaylandInputWiring {
                source: Some(input.source),
                lifecycle: Some(input.lifecycle),
                binding_profile: BindingProfile::KmsLive,
                vt_switch_requested: Some(input.vt_switch_requested),
            },
            Some(Box::new(protocol_failed)),
            Some(port),
            Some(starter),
        )
    }

    /// Start the protocol thread with a bare-metal input source registered on it.
    ///
    /// Separate from [`WaylandRuntime::new`] so ordinary nested call sites do
    /// not carry the binding-profile and VT-route arguments. The factory
    /// constructs the backend on *this* thread and reaches the libseat session
    /// by message rather than by owning it. Keeping registration inside
    /// construction puts the source on the protocol event loop before readiness
    /// can be acknowledged.
    fn with_input_source(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input_source: Option<Box<dyn input::InputSourceRegistration>>,
        protocol_failed: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::with_input_source_and_bindings(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            policy,
            WaylandInputWiring {
                source: input_source,
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                lifecycle: None,
                binding_profile: BindingProfile::Nested,
                vt_switch_requested: None,
            },
            protocol_failed,
        )
    }

    #[cfg(feature = "bus")]
    #[allow(clippy::too_many_arguments)]
    fn with_input_source_port(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input_source: Option<Box<dyn input::InputSourceRegistration>>,
        protocol_failed: Option<Box<dyn FnOnce() + Send>>,
        port: Option<PortProtocolWiring>,
        port_starter: Option<PortStarter>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::with_input_source_and_bindings_port(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            policy,
            WaylandInputWiring {
                source: input_source,
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                lifecycle: None,
                binding_profile: BindingProfile::Nested,
                vt_switch_requested: None,
            },
            protocol_failed,
            port,
            port_starter,
        )
    }

    fn with_input_source_and_bindings(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input: WaylandInputWiring,
        protocol_failed: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::with_input_source_and_bindings_port(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            policy,
            input,
            protocol_failed,
            #[cfg(feature = "bus")]
            None,
            #[cfg(feature = "bus")]
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_input_source_and_bindings_port(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        policy: WaylandRuntimePolicy,
        input: WaylandInputWiring,
        protocol_failed: Option<Box<dyn FnOnce() + Send>>,
        #[cfg(feature = "bus")] port: Option<PortProtocolWiring>,
        #[cfg(feature = "bus")] port_starter: Option<PortStarter>,
    ) -> Result<Self, Box<dyn Error>> {
        let WaylandInputWiring {
            source: input_source,
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
                lifecycle: input_lifecycle,
            binding_profile,
            vt_switch_requested,
        } = input;
        let WaylandRuntimePolicy {
            keybindings_enabled,
            explicit_sync_exposure_mode,
            decoration,
        } = policy;
        let (commands, command_source) = channel::channel();
        let (event_sender, events) = mpsc::sync_channel(PROTOCOL_EVENT_BATCH_CAPACITY);
        let (ecs_action_sender, ecs_actions) = mpsc::sync_channel(ECS_ACTION_QUEUE_CAPACITY);
        let (kms_render_command_sender, kms_render_commands) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let cursor_position = Arc::new(Mutex::new(CursorPositionSnapshot::default()));
        let protocol_cursor_position = Arc::clone(&cursor_position);
        let socket_name = socket_name.to_owned();

        let thread = thread::Builder::new()
            .name("cosmix-wayland".into())
            .spawn(move || {
                // Declared first so it is dropped last, after every server
                // value, on normal return and panic alike.
                let _completion = ProtocolThreadCompletion(completion_sender);
                // Declared before the server so an unwinding panic drops the
                // server first and publishes the failure afterwards.
                let mut failure = ProtocolThreadFailure(protocol_failed);
                let bootstrap = ProtocolServerBootstrap {
                    command_source,
                    event_sender: event_sender.clone(),
                    ecs_action_sender,
                    kms_render_command_sender,
                    keybindings_enabled,
                    binding_profile,
                    vt_switch_requested,
                    explicit_sync_exposure_mode,
                    decoration,
                    input_source,
                    cursor_position: protocol_cursor_position,
                };
                #[cfg(feature = "bus")]
                let result = match port {
                    Some(port) => ProtocolServer::new_production(
                        &socket_name,
                        backend_kind,
                        output_size,
                        gpu,
                        bootstrap,
                        port,
                    ),
                    None => {
                        ProtocolServer::new(&socket_name, backend_kind, output_size, gpu, bootstrap)
                    }
                };
                #[cfg(not(feature = "bus"))]
                let result =
                    ProtocolServer::new(&socket_name, backend_kind, output_size, gpu, bootstrap);
                let mut server = match result {
                    Ok((server, explicit_sync)) => {
                        let _ = ready_sender.send(Ok(explicit_sync));
                        server
                    }
                    Err(error) => {
                        // Startup has its own synchronous error reply; publishing
                        // a second asynchronous runtime failure would describe
                        // the same event twice.
                        failure.disarm();
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };

                let run = server.run();
                let runtime_failed = protocol_exit_failed(&run, server.state.shutdown_cause);
                if let Err(error) = run {
                    tracing::error!(%error, "Wayland protocol thread stopped");
                    let _ = event_sender.try_send(vec![ProtocolEvent::RuntimeFailed(error)]);
                }
                // Destruction can itself panic. Keep the failure armed until the
                // server is gone so that path is reported as unexpected too.
                drop(server);
                if !runtime_failed {
                    failure.disarm();
                }
            })?;

        match ready_receiver.recv() {
            Ok(Ok(explicit_sync_startup)) => Ok(Self {
                client_scene_feed: Some(ClientSceneFeed::new(
                    events,
                    commands.clone(),
                    cursor_position,
                )),
                commands,
                ecs_actions: Mutex::new(ecs_actions),
                kms_render_commands: Arc::new(Mutex::new(kms_render_commands)),
                thread: Some(thread),
                thread_completion: Some(Mutex::new(completion_receiver)),
                #[cfg(feature = "bus")]
                port_starter,
                #[cfg(feature = "bus")]
                port_worker: None,
                explicit_sync_startup: Some(explicit_sync_startup),
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                input_lifecycle,
                #[cfg(test)]
                _test_channels: None,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error.into())
            }
            Err(error) => {
                let _ = thread.join();
                Err(format!("Wayland protocol thread exited during startup: {error}").into())
            }
        }
    }

    /// What the protocol thread decided about explicit sync while starting.
    ///
    /// `None` only for a runtime that never started one; see the field. The
    /// report describes startup and does not move afterwards, so a caller
    /// asking whether the global is advertised *now* must ask the state that
    /// tracks withdrawal, not this.
    pub(crate) fn explicit_sync_startup(&self) -> Option<&ExplicitSyncStartupReport> {
        self.explicit_sync_startup.as_ref()
    }

    #[cfg(feature = "bus")]
    pub(crate) fn start_port(&mut self) -> Result<(), String> {
        if self.port_worker.is_some() {
            return Ok(());
        }
        let starter = self
            .port_starter
            .take()
            .ok_or_else(|| "compositor Bus port was not prepared".to_string())?;
        self.port_worker = Some(starter.start()?);
        Ok(())
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn kms_topology_client(&self) -> KmsTopologyClient {
        KmsTopologyClient {
            commands: self.commands.clone(),
            render_commands: Arc::clone(&self.kms_render_commands),
            input_lifecycle: self.input_lifecycle.clone(),
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn client_frame_clock(&self) -> ClientFrameClock {
        ClientFrameClock {
            commands: self.commands.clone(),
        }
    }

    pub(crate) fn security_presentation_reporter(&self) -> SecurityPresentationReporter {
        SecurityPresentationReporter {
            commands: self.commands.clone(),
        }
    }

    pub(crate) fn capture_completion_reporter(&self) -> CaptureCompletionReporter {
        CaptureCompletionReporter {
            commands: self.commands.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_ecs_action(action: EcsAction) -> Self {
        let (commands, command_source) = channel::channel();
        let (event_sender, events) = mpsc::sync_channel(1);
        let (ecs_action_sender, ecs_actions) = mpsc::sync_channel(1);
        let (kms_render_command_sender, kms_render_commands) = mpsc::channel();
        let cursor_position = Arc::new(Mutex::new(CursorPositionSnapshot::default()));
        ecs_action_sender
            .try_send(action)
            .expect("test ECS action channel has capacity");
        Self {
            client_scene_feed: Some(ClientSceneFeed::new(
                events,
                commands.clone(),
                cursor_position,
            )),
            commands,
            ecs_actions: Mutex::new(ecs_actions),
            kms_render_commands: Arc::new(Mutex::new(kms_render_commands)),
            thread: None,
            thread_completion: None,
            explicit_sync_startup: None,
            #[cfg(feature = "bus")]
            port_starter: None,
            #[cfg(feature = "bus")]
            port_worker: None,
            input_lifecycle: None,
            _test_channels: Some(TestRuntimeChannels {
                _command_source: Mutex::new(command_source),
                _event_sender: event_sender,
                _ecs_action_sender: ecs_action_sender,
                _kms_render_command_sender: Some(kms_render_command_sender),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_runtime_failure_and_disconnected_kms(error: &str) -> Self {
        let (commands, command_source) = channel::channel();
        let (event_sender, events) = mpsc::sync_channel(1);
        let (ecs_action_sender, ecs_actions) = mpsc::sync_channel(1);
        let (kms_render_command_sender, kms_render_commands) = mpsc::channel();
        let cursor_position = Arc::new(Mutex::new(CursorPositionSnapshot::default()));
        event_sender
            .try_send(vec![ProtocolEvent::RuntimeFailed(error.into())])
            .expect("test protocol event channel has capacity");
        drop(kms_render_command_sender);
        Self {
            client_scene_feed: Some(ClientSceneFeed::new(
                events,
                commands.clone(),
                cursor_position,
            )),
            commands,
            ecs_actions: Mutex::new(ecs_actions),
            kms_render_commands: Arc::new(Mutex::new(kms_render_commands)),
            thread: None,
            thread_completion: None,
            explicit_sync_startup: None,
            #[cfg(feature = "bus")]
            port_starter: None,
            #[cfg(feature = "bus")]
            port_worker: None,
            input_lifecycle: None,
            _test_channels: Some(TestRuntimeChannels {
                _command_source: Mutex::new(command_source),
                _event_sender: event_sender,
                _ecs_action_sender: ecs_action_sender,
                _kms_render_command_sender: None,
            }),
        }
    }

    pub(crate) fn take_client_scene_feed(&mut self) -> Result<ClientSceneFeed, String> {
        self.client_scene_feed
            .take()
            .ok_or_else(|| "Wayland client scene feed was already taken".to_string())
    }

    #[cfg(test)]
    pub(crate) fn drain_events(&self) -> Result<Vec<ProtocolEvent>, String> {
        self.client_scene_feed
            .as_ref()
            .ok_or_else(|| "Wayland client scene feed was already taken".to_string())?
            .drain_events()
    }

    pub(crate) fn drain_ecs_actions(&self) -> Result<Vec<EcsAction>, String> {
        let actions = self
            .ecs_actions
            .lock()
            .map_err(|_| "ECS action receiver was poisoned".to_string())?;
        let mut drained = Vec::new();
        loop {
            match actions.try_recv() {
                Ok(action) => drained.push(action),
                Err(TryRecvError::Empty) => return Ok(drained),
                Err(TryRecvError::Disconnected) => {
                    return if drained.is_empty() {
                        Err("Wayland protocol ECS action channel disconnected".into())
                    } else {
                        Ok(drained)
                    };
                }
            }
        }
    }

    pub(crate) fn drain_kms_render_commands(&self) -> Result<Vec<KmsRenderCommand>, String> {
        drain_kms_render_commands(&self.kms_render_commands)
    }

    pub(crate) fn finish_frame(&self, inputs: Vec<HostInput>) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::Frame { inputs })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }

    pub(crate) fn submit_kms_render_reply(&self, reply: KmsRenderReply) -> Result<(), String> {
        self.commands
            .send(ProtocolCommand::KmsRenderReply { reply })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn submit_kms_topology_lifecycle(
        &self,
        event: KmsTopologyLifecycleEvent,
        timeout: Duration,
    ) -> Result<(), String> {
        let (acknowledgement, reply) = mpsc::sync_channel(1);
        self.commands
            .send(ProtocolCommand::KmsTopologyLifecycle {
                event,
                acknowledgement,
            })
            .map_err(|_| "Wayland protocol thread disconnected".to_string())?;
        reply
            .recv_timeout(timeout)
            .map_err(|error| format!("Wayland topology acknowledgement failed: {error}"))?
    }

    #[cfg(test)]
    fn wait_until_protocol_thread_processed(&self, awaited: &str) {
        let (acknowledgement, receiver) = mpsc::sync_channel(1);
        self.commands
            .send(ProtocolCommand::Barrier { acknowledgement })
            .unwrap_or_else(|_| panic!("protocol thread disconnected while awaiting {awaited}"));
        receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|error| {
                panic!("timed out after 30 seconds awaiting {awaited}: {error}")
            });
    }
}

fn drain_kms_render_commands(
    receiver: &Mutex<Receiver<KmsRenderCommand>>,
) -> Result<Vec<KmsRenderCommand>, String> {
    let commands = receiver
        .lock()
        .map_err(|_| "KMS render command receiver was poisoned".to_string())?;
    let mut drained = Vec::new();
    loop {
        match commands.try_recv() {
            Ok(command) => drained.push(command),
            Err(TryRecvError::Empty) => return Ok(drained),
            Err(TryRecvError::Disconnected) => {
                return if drained.is_empty() {
                    Err("Wayland protocol KMS render command channel disconnected".into())
                } else {
                    Ok(drained)
                };
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WaylandRuntimePolicy {
    pub(crate) keybindings_enabled: bool,
    pub(crate) explicit_sync_exposure_mode: ExplicitSyncExposureMode,
    pub(crate) decoration: DecorationStartup,
}

/// The GPU-side wiring the protocol thread is handed at construction.
///
/// Grouped because all three travel together and are consumed together: the
/// capabilities describe the device, the validator gates imports onto it, and
/// the retirement adapter is how the thread learns work on it has finished.
/// Separately they are three of the parameters `WaylandRuntime::new` forwards
/// verbatim; together they leave room for one that is genuinely about something
/// else. Deliberately **not** folded into [`WaylandRuntimePolicy`], which
/// describes decisions rather than resources.
pub(crate) struct WaylandGpuWiring {
    pub(crate) dmabuf_capabilities: Option<DmabufCapabilities>,
    pub(crate) dmabuf_validator: Option<Box<dyn ValidateDmabuf>>,
    pub(crate) retirement_adapter: Option<Box<dyn WaitForSubmittedWork>>,
    pub(crate) capture_advertisements: crate::capture::CaptureAdvertisementRegistry,
}

impl Drop for WaylandRuntime {
    fn drop(&mut self) {
        #[cfg(feature = "bus")]
        let mut port = self.port_worker.take();
        #[cfg(feature = "bus")]
        self.port_starter.take();
        #[cfg(feature = "bus")]
        if let Some(port) = port.as_mut() {
            port.begin_shutdown();
        }
        let _ = self.commands.send(ProtocolCommand::Shutdown);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let Some(completion) = self.thread_completion.take() else {
            tracing::error!("Wayland protocol thread detached without a completion channel");
            drop(thread);
            return;
        };
        let completion = completion
            .into_inner()
            .expect("protocol completion receiver is never shared");
        match join_protocol_thread_after_completion(
            thread,
            &completion,
            PROTOCOL_THREAD_SHUTDOWN_TIMEOUT,
        ) {
            ProtocolThreadJoinOutcome::Joined => {}
            ProtocolThreadJoinOutcome::Panicked => {
                tracing::error!("Wayland protocol thread panicked during shutdown");
            }
            ProtocolThreadJoinOutcome::TimedOut => {
                tracing::error!(
                    timeout_seconds = PROTOCOL_THREAD_SHUTDOWN_TIMEOUT.as_secs(),
                    "Wayland protocol thread did not stop in time and was detached"
                );
                // A detached protocol thread is a test failure, not a log line:
                // an otherwise-passing test would report success while the
                // thread it was exercising is wedged. Never panic while already
                // unwinding — a second panic out of `Drop` aborts the process
                // and destroys the original failure's report.
                #[cfg(test)]
                if !std::thread::panicking() {
                    panic!(
                        "Wayland protocol thread did not stop within \
                         {PROTOCOL_THREAD_SHUTDOWN_TIMEOUT:?} and was detached"
                    );
                }
            }
        }
        #[cfg(feature = "bus")]
        if let Some(port) = port {
            port.finish();
        }
    }
}

struct ProtocolServer {
    event_loop: EventLoop<'static, WaylandState>,
    display: Display<WaylandState>,
    state: WaylandState,
    prepared_import_device: Option<smithay::backend::drm::DrmDeviceFd>,
    retirement_worker: RetirementWorker,
    event_sender: SyncSender<Vec<ProtocolEvent>>,
    pending_events: PendingProtocolEvents,
    dirty_surfaces: DirtySurfaces,
    dirty_cursor: bool,
}

struct ProtocolServerBootstrap {
    command_source: channel::Channel<ProtocolCommand>,
    event_sender: SyncSender<Vec<ProtocolEvent>>,
    ecs_action_sender: SyncSender<EcsAction>,
    kms_render_command_sender: Sender<KmsRenderCommand>,
    keybindings_enabled: bool,
    binding_profile: BindingProfile,
    vt_switch_requested: Option<Box<dyn Fn(u8) + Send>>,
    explicit_sync_exposure_mode: ExplicitSyncExposureMode,
    decoration: DecorationStartup,
    /// The bare-metal input source, if one was supplied.
    ///
    /// It is carried in the bootstrap because it must be moved onto the
    /// protocol thread and registered there — a source registered from the
    /// calling thread would put its callback on the wrong event loop. Nested
    /// runtimes supply `None`; the live KMS runtime makes the source mandatory
    /// at its public construction seam.
    input_source: Option<Box<dyn input::InputSourceRegistration>>,
    cursor_position: Arc<Mutex<CursorPositionSnapshot>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExplicitSyncExposureMode {
    Disabled,
    Production,
}

impl ExplicitSyncExposureMode {
    const fn prepares_import_device(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

enum ExplicitSyncGlobal {
    Live(DrmSyncobjState),
    #[cfg(test)]
    Probe(GlobalId),
}

use explicit_sync_activation::{
    ExplicitSyncActivation, create_drm_syncobj_state, decide_explicit_sync_activation,
    take_drm_syncobj_global,
};
// The dormancy tripwire itself is consulted only through
// `decide_explicit_sync_activation`; the tests assert on it directly.
#[cfg(test)]
use explicit_sync_activation::should_advertise_explicit_sync_global;
// Named only where startup reports are built by hand. Production reads both
// through the report — the identity by its fields, the reason by `Debug` — so
// neither name is needed outside tests.
#[cfg(test)]
use self::explicit_sync::{
    ImportDeviceUnavailable, PreparedImportDevice, PreparedImportDeviceIdentity,
};

/// Seals the activation token behind a private field so the rest of this module
/// cannot forge one, and keeps the sole `DrmSyncobjState::new` call site inside
/// the seal. Creating that state advertises the `linux-drm-syncobj-v1` global as
/// an unconditional side effect, so the only construction path is
/// [`create_drm_syncobj_state`], which demands an
/// [`ExplicitSyncActivation`] that only [`decide_explicit_sync_activation`] can
/// mint after consulting [`should_advertise_explicit_sync_global`]. Permanent
/// faults take the state through [`take_drm_syncobj_global`], the sole
/// `into_global` call site, before disabling the advertised global.
///
/// Rust cannot stop a future module from importing Smithay's constructor and
/// bypassing all of this, so the seal is backed by
/// `only_the_sealed_module_constructs_the_drm_syncobj_state`, which scans the
/// crate's sources and fails if a second call site appears.
mod explicit_sync_activation {
    use super::{
        DisplayHandle, DrmSyncobjState, ExplicitSyncExposureMode, ExplicitSyncGlobal, WaylandState,
    };
    use smithay::backend::drm::DrmDeviceFd;
    use smithay::reexports::wayland_server::backend::GlobalId;

    #[derive(Debug)]
    pub(super) struct ExplicitSyncActivation {
        _private: (),
    }

    pub(super) const fn should_advertise_explicit_sync_global(
        exposure_mode: ExplicitSyncExposureMode,
        prepared_import_device: bool,
    ) -> bool {
        let exposure_allowed = match exposure_mode {
            ExplicitSyncExposureMode::Disabled => false,
            ExplicitSyncExposureMode::Production => true,
        };
        exposure_allowed && prepared_import_device
    }

    pub(super) fn decide_explicit_sync_activation(
        exposure_mode: ExplicitSyncExposureMode,
        prepared_import_device: bool,
    ) -> Option<ExplicitSyncActivation> {
        should_advertise_explicit_sync_global(exposure_mode, prepared_import_device)
            .then_some(ExplicitSyncActivation { _private: () })
    }

    pub(super) fn create_drm_syncobj_state(
        display_handle: &DisplayHandle,
        import_device: DrmDeviceFd,
        _activation: ExplicitSyncActivation,
    ) -> DrmSyncobjState {
        DrmSyncobjState::new::<WaylandState>(display_handle, import_device)
    }

    pub(super) fn take_drm_syncobj_global(
        slot: &mut Option<ExplicitSyncGlobal>,
    ) -> Option<GlobalId> {
        match slot.take()? {
            ExplicitSyncGlobal::Live(state) => Some(state.into_global()),
            #[cfg(test)]
            ExplicitSyncGlobal::Probe(global) => Some(global),
        }
    }
}

/// What the protocol thread found about explicit sync while starting, and what
/// it did about it.
///
/// The two halves are reported together because neither alone answers "is the
/// global there, and if not, why". `global_advertised` is the observable
/// outcome; `preparation` is the reason it came out that way. They are related
/// by `should_advertise_explicit_sync_global` applied to the exposure mode and
/// whether preparation yielded a device — a relationship a caller can check,
/// which is why both are carried rather than one being derived from the other
/// here.
///
/// Startup only, like [`ExplicitSyncPreparation`]: a later permanent fault
/// withdraws the global and leaves this report describing what startup found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExplicitSyncStartupReport {
    pub(crate) preparation: ExplicitSyncPreparation,
    pub(crate) global_advertised: bool,
}

/// What a startup report amounts to for whoever started the runtime.
///
/// The report says what happened; this says what it means. Separated because
/// the meaning is a *relationship* between the two halves of the report, and a
/// relationship stated once can be checked, whereas the same reasoning spread
/// across log call sites cannot be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExplicitSyncStartupVerdict {
    /// The exposure mode withheld explicit sync and nothing was advertised.
    /// This is the configured outcome, not a fault.
    DisabledAsConfigured,
    /// A device was prepared and `linux-drm-syncobj-v1` went live.
    Advertised,
    /// Preparation was asked for and refused, so no global was advertised.
    /// Clients that would have used explicit sync fall back to implicit.
    Degraded,
    /// The two halves of the report disagree.
    ///
    /// Advertising follows from preparation and the exposure mode alone
    /// (`should_advertise_explicit_sync_global`), so a prepared device with no
    /// global, or a global with nothing prepared, means the construction path
    /// stopped following the rule it is supposed to follow. Nothing produces
    /// this today; it exists so that if something starts to, the run says so
    /// rather than presenting the outcome as ordinary.
    Inconsistent,
}

/// Judge a startup report against the rule that decides advertising.
///
/// Deliberately re-derives nothing from the exposure mode: the mode is already
/// folded into both halves of the report, and asking for it again would let a
/// caller supply a different one from the one the thread actually used.
pub(crate) fn judge_explicit_sync_startup(
    report: &ExplicitSyncStartupReport,
) -> ExplicitSyncStartupVerdict {
    let prepared = matches!(report.preparation, ExplicitSyncPreparation::Prepared(_));
    match (prepared, report.global_advertised) {
        (true, true) => ExplicitSyncStartupVerdict::Advertised,
        (false, false) => match report.preparation {
            ExplicitSyncPreparation::SkippedByPolicy => {
                ExplicitSyncStartupVerdict::DisabledAsConfigured
            }
            _ => ExplicitSyncStartupVerdict::Degraded,
        },
        _ => ExplicitSyncStartupVerdict::Inconsistent,
    }
}

/// Prepare the explicit-sync import device if the exposure mode calls for one,
/// and say what happened either way.
///
/// The preparation is taken as a closure so this decision can be exercised
/// against a source that opens nothing. That matters for the case the
/// production code cannot demonstrate about itself: a mode that skips
/// preparation must not *call* preparation, and only a fake that records
/// whether it was invoked can witness the difference between not calling it and
/// calling it and discarding the answer.
///
/// Logging is left to the caller, which has the adapter name this does not.
fn prepare_explicit_sync_import_device<Device>(
    exposure_mode: ExplicitSyncExposureMode,
    prepare: impl FnOnce() -> ImportDeviceDecision<Device>,
) -> (Option<Device>, ExplicitSyncPreparation) {
    if !exposure_mode.prepares_import_device() {
        return (None, ExplicitSyncPreparation::SkippedByPolicy);
    }
    match prepare() {
        ImportDeviceDecision::Prepared(prepared) => {
            let (device, identity) = prepared.split();
            (Some(device), ExplicitSyncPreparation::Prepared(identity))
        }
        ImportDeviceDecision::Unavailable(reason) => {
            (None, ExplicitSyncPreparation::Unavailable(reason))
        }
    }
}

fn construct_explicit_sync_state<Device, State>(
    exposure_mode: ExplicitSyncExposureMode,
    prepared_import_device: Option<Device>,
    factory: impl FnOnce(ExplicitSyncActivation, Device) -> State,
) -> Option<State> {
    let activation =
        decide_explicit_sync_activation(exposure_mode, prepared_import_device.is_some())?;
    let import_device = prepared_import_device?;
    Some(factory(activation, import_device))
}

impl ProtocolServer {
    /// Build the server, and hand back what it decided about explicit sync
    /// alongside it.
    ///
    /// The report is returned rather than stored because its one consumer is on
    /// the other side of the readiness channel: it is the answer the coordinator
    /// needs before the first client connects, and keeping it here would leave
    /// that coordinator with the `Option` and the log line it had before.
    fn new(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        bootstrap: ProtocolServerBootstrap,
    ) -> Result<(Self, ExplicitSyncStartupReport), String> {
        Self::new_with_port(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            bootstrap,
            #[cfg(feature = "bus")]
            None,
        )
    }

    #[cfg(feature = "bus")]
    fn new_production(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        bootstrap: ProtocolServerBootstrap,
        port: PortProtocolWiring,
    ) -> Result<(Self, ExplicitSyncStartupReport), String> {
        Self::new_with_port(
            socket_name,
            backend_kind,
            output_size,
            gpu,
            bootstrap,
            Some(port),
        )
    }

    fn new_with_port(
        socket_name: &str,
        backend_kind: BackendKind,
        output_size: (u32, u32),
        gpu: WaylandGpuWiring,
        bootstrap: ProtocolServerBootstrap,
        #[cfg(feature = "bus")] port: Option<PortProtocolWiring>,
    ) -> Result<(Self, ExplicitSyncStartupReport), String> {
        let WaylandGpuWiring {
            dmabuf_capabilities,
            dmabuf_validator,
            retirement_adapter,
            capture_advertisements,
        } = gpu;
        let ProtocolServerBootstrap {
            command_source,
            event_sender,
            ecs_action_sender,
            kms_render_command_sender,
            keybindings_enabled,
            binding_profile,
            vt_switch_requested,
            explicit_sync_exposure_mode,
            decoration,
            input_source,
            cursor_position,
        } = bootstrap;
        #[cfg(feature = "bus")]
        let (port_source, port_context, observation_producer, observation_event_seq) = match port {
            Some(port) => (
                Some(port.source),
                Some(port.context.clone()),
                port.observation_producer,
                port.context.event_seq.clone(),
            ),
            None => {
                let lost_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let event_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let (producer, receiver) = port_observation::outbox(lost_count);
                drop(receiver);
                (None, None, producer, event_seq)
            }
        };
        let display = Display::<WaylandState>::new().map_err(|error| error.to_string())?;
        let display_handle = display.handle();
        let backend_name = match backend_kind {
            BackendKind::Winit => "winit",
            BackendKind::Kms => "KMS",
        };
        let dmabuf_capabilities = dmabuf_capabilities
            .ok_or_else(|| format!("{backend_name} backend requires DMA-BUF capabilities"))?;
        let retirement_adapter = retirement_adapter
            .ok_or_else(|| format!("{backend_name} backend requires a GPU retirement adapter"))?;
        let (prepared_import_device, explicit_sync_preparation) =
            prepare_explicit_sync_import_device(explicit_sync_exposure_mode, || {
                prepare_linux_import_device(&dmabuf_capabilities.drm_adapter)
            });
        match &explicit_sync_preparation {
            ExplicitSyncPreparation::SkippedByPolicy => {
                tracing::debug!("explicit-sync import-device preparation disabled");
            }
            ExplicitSyncPreparation::Prepared(identity) => {
                tracing::info!(
                    expected_render_dev_t = identity.expected_render_dev_t,
                    observed_render_dev_t = identity.observed_render_dev_t,
                    resolved_path = %identity.resolved_path.display(),
                    observed_node_type = ?identity.observed_node_type,
                    "prepared explicit-sync DRM import device"
                );
            }
            ExplicitSyncPreparation::Unavailable(reason) => {
                tracing::warn!(
                    ?reason,
                    adapter = dmabuf_capabilities.adapter_name,
                    "explicit sync unavailable; protocol global remains absent"
                );
            }
        }
        let drm_syncobj_state = construct_explicit_sync_state(
            explicit_sync_exposure_mode,
            prepared_import_device.clone(),
            |activation, import_device| {
                create_drm_syncobj_state(&display_handle, import_device, activation)
            },
        )
        .map(ExplicitSyncGlobal::Live);
        let explicit_sync_global_advertised = drm_syncobj_state.is_some();

        // CompositorState owns wl_subcompositor and Smithay's synchronized
        // subsurface transaction semantics. Each applied child transaction is
        // mirrored into its own ECS surface entity below.
        let compositor_state = CompositorState::new::<WaylandState>(&display_handle);
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<WaylandState>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<WaylandState>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<WaylandState>(&display_handle);
        let session_lock_state =
            SessionLockManagerState::new::<WaylandState, _>(&display_handle, |_| true);
        // Visible only to clients carrying XWaylandClientData; native clients
        // cannot bind it (Smithay's can_view filter).
        #[cfg(feature = "xwayland")]
        let xwayland_shell_state = smithay::wayland::xwayland_shell::XWaylandShellState::new::<
            WaylandState,
        >(&display_handle);
        display_handle.create_global::<WaylandState, ZwlrScreencopyManagerV1, _>(3, ());
        let xdg_decoration_state = XdgDecorationState::new::<WaylandState>(&display_handle);
        let fractional_scale_state =
            FractionalScaleManagerState::new::<WaylandState>(&display_handle);
        let viewporter_state = ViewporterState::new::<WaylandState>(&display_handle);
        let shm_state = ShmState::new::<WaylandState>(&display_handle, Vec::new());
        let supported_dmabuf_formats = dmabuf_capabilities
            .formats
            .iter()
            .filter_map(|format| {
                Some(Format {
                    code: format.fourcc.try_into().ok()?,
                    modifier: format.modifier.into(),
                })
            })
            .collect::<Vec<_>>();
        let feedback = DmabufFeedbackBuilder::new(
            dmabuf_capabilities.main_device as _,
            supported_dmabuf_formats.iter().copied(),
        )
        .build()
        .map_err(|error| format!("failed to build DMA-BUF feedback: {error}"))?;
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global = dmabuf_state
            .create_global_with_default_feedback::<WaylandState>(&display_handle, &feedback);

        let data_device_state = DataDeviceState::new::<WaylandState>(&display_handle);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "cosmix");
        let keyboard = seat
            .add_keyboard(Default::default(), 500, 30)
            .map_err(|error| error.to_string())?;
        let pointer = seat.add_pointer();
        let diagnostic_sender = spawn_shm_diagnostic_worker();

        let backend = match backend_kind {
            BackendKind::Winit => {
                let output = Output::new(
                    "cosmix-nested-0".into(),
                    PhysicalProperties {
                        size: (254, 169).into(),
                        subpixel: Subpixel::Unknown,
                        make: "CosMix".into(),
                        model: "Nested Bevy Output".into(),
                    },
                );
                output.create_global::<WaylandState>(&display_handle);
                let mode = output_mode(output_size);
                output.change_current_state(
                    Some(mode),
                    Some(Transform::Normal),
                    Some(Scale::Integer(1)),
                    Some((0, 0).into()),
                );
                output.set_preferred(mode);
                BackendData::Winit(WinitBackendData {
                    output,
                    output_mode: mode,
                    output_size,
                    output_scale: 1.0,
                })
            }
            BackendKind::Kms => BackendData::Kms(KmsBackendData::new(output_size)),
        };

        let event_loop = EventLoop::try_new().map_err(|error| error.to_string())?;
        let mut foreign_toplevel_nonce = [0_u8; 16];
        getrandom::fill(&mut foreign_toplevel_nonce)
            .map_err(|error| format!("failed to seed foreign-toplevel identifiers: {error}"))?;
        let idle_notifier_state = IdleNotifierState::new(&display_handle, event_loop.handle());
        let foreign_toplevel_list_state =
            ForeignToplevelListState::new::<WaylandState>(&display_handle);
        let (retirement_report_sender, retirement_report_source) = channel::channel();
        let (retirement_request_sender, retirement_worker) =
            spawn_retirement_worker(retirement_adapter, MAX_GLOBAL_DMABUF_USES, move |report| {
                retirement_report_sender.send(report).is_ok()
            })
            .map_err(|error| format!("failed to spawn GPU retirement worker: {error}"))?;
        let (client_disconnect_sender, client_disconnect_source) = channel::channel();
        let (capture_release_sender, capture_release_source) = channel::channel();
        // Spawned here rather than beside the other workers above because the
        // worker needs a handle onto this event loop: its outcomes are queued
        // from off-thread and only a dispatch cycle flushes them.
        let dmabuf_validation = match dmabuf_validator {
            Some(validator) => {
                let (wake_sender, wake_source) = channel::channel::<()>();
                event_loop
                    .handle()
                    .insert_source(wake_source, |_event, (), _state| {})
                    .map_err(|error| error.to_string())?;
                Some(spawn_dmabuf_validation_worker(validator, wake_sender)?)
            }
            None => None,
        };
        let acquire_gates = AcquireGateEngine::new(LinuxAcquireGatePlatform::new(
            event_loop.handle(),
            display_handle.clone(),
        ));
        #[cfg(test)]
        let release_use_test_probe = ReleaseUseTestProbe::default();
        let release_uses = ReleaseUseEngine::new(LinuxReleaseUsePlatform::new(
            display_handle.clone(),
            retirement_request_sender,
            #[cfg(test)]
            release_use_test_probe.clone(),
        ));
        let state = WaylandState {
            acquire_gates,
            release_uses,
            client_disconnect_sender,
            display_handle,
            compositor_state,
            output_manager_state,
            xdg_shell_state,
            layer_shell_state,
            session_lock_state,
            lock_lifecycle: LockLifecycle::Unlocked,
            lock_surfaces_by_output: HashMap::new(),
            kms_session_lock_gate: KmsSessionLockGate::default(),
            #[cfg(test)]
            session_unlock_callbacks: 0,
            next_lock_generation: 0,
            next_security_presentation_epoch: 0,
            next_capture_manager_id: 0,
            next_capture_id: 0,
            capture_managers: HashMap::new(),
            capture_frames: HashMap::new(),
            capture_frames_by_resource: HashMap::new(),
            capture_reservations: HashMap::new(),
            capture_release_sender,
            capture_loop_handle: event_loop.handle(),
            saved_cursor_selection: None,
            idle_notifier_state,
            foreign_toplevel_list_state,
            xdg_decoration_state,
            fractional_scale_state,
            viewporter_state,
            #[cfg(feature = "xwayland")]
            xwayland_shell_state,
            #[cfg(feature = "xwayland")]
            xwayland: xwayland::XwaylandRuntime::new(socket_name.to_owned()),
            shm_state,
            dmabuf_state,
            drm_syncobj_state,
            dmabuf_global,
            supported_dmabuf_formats,
            capture_advertisements,
            dmabuf_validation,
            data_device_state,
            seat_state,
            seat,
            keyboard,
            input_ingress: input::InputIngressState::default(),
            touch_devices: 0,
            bindings: BindingState::for_profile(binding_profile, keybindings_enabled),
            decoration,
            ecs_action_sender,
            kms_render_command_sender,
            vt_switch_requested,
            pointer,
            popup_manager: PopupManager::default(),
            backend,
            cursor_position: (0.0, 0.0),
            cursor_position_snapshot: cursor_position,
            cursor_selection: CursorSelection::Default,
            chrome_cursor_override: None,
            cursor_surfaces: HashMap::new(),
            chrome_hover: None,
            chrome_pressed: None,
            chrome_pointer_grab: None,
            titlebar_click_candidate: None,
            suppressed_chrome_buttons: HashSet::new(),
            #[cfg(test)]
            chrome_geometry_retarget_count: 0,
            #[cfg(test)]
            committed_window_state_transitions: Vec::new(),
            pointer_hit_test_transaction_applying: false,
            pointer_hit_test_dirty: false,
            pointer_hit_test_batch_depth: 0,
            pointer_grab_teardown_deferred: false,
            pointer_focus_local_position: None,
            #[cfg(test)]
            pointer_hit_test_reconciliations: 0,
            interactive_pointer: None,
            exclusive_keyboard_focus: None,
            minimized_toplevels: Vec::new(),
            surfaces: HashMap::new(),
            foreign_toplevels: HashMap::new(),
            foreign_toplevel_identifiers: HashMap::new(),
            foreign_toplevel_nonce,
            buffer_history_surfaces: HashSet::new(),
            attach_history_surfaces: HashSet::new(),
            committed_surfaces: HashSet::new(),
            surface_objects: HashMap::new(),
            xdg_surface_objects: HashMap::new(),
            dispatching_xdg_surface: None,
            pending_parentless_popups: HashMap::new(),
            committed_surface_stacks: HashMap::new(),
            warned_unsupported_surfaces: HashSet::new(),
            #[cfg(feature = "bus")]
            port_context,
            #[cfg(feature = "bus")]
            pending_port_requests: Vec::with_capacity(PORT_QUEUE_CAPACITY),
            #[cfg(feature = "bus")]
            pending_port_controls: Vec::with_capacity(PORT_QUEUE_CAPACITY),
            #[cfg(feature = "bus")]
            observations: port_observation::ObservationState::new(
                observation_producer,
                observation_event_seq,
                event_loop.handle(),
            ),
            events: Vec::new(),
            event_flush_acknowledgements: Vec::new(),
            pending_full_upserts: HashSet::new(),
            pending_cursor_update: false,
            next_surface_id: 1,
            next_layout_index: 0,
            next_stack_sequences: [0; StackBand::COUNT],
            next_buffer_token: 1,
            next_dmabuf_buffer_id: 1,
            dmabuf_buffer_ids: HashMap::new(),
            retained_buffers: RetentionTable::default(),
            budgeted_dmabuf_tokens: HashSet::new(),
            surface_count: 0,
            subsurface_topology: HashMap::new(),
            damage_requests_since_apply: HashMap::new(),
            last_keyboard_action: None,
            shm_bytes: 0,
            diagnostic_sender,
            shutdown_cause: None,
            explicit_sync_global_advertised,
            #[cfg(test)]
            explicit_sync_global_withdrawals: 0,
            #[cfg(test)]
            acquire_gate_pre_commit_count: 0,
            #[cfg(test)]
            acquire_gate_client_destroyed_count: 0,
            #[cfg(test)]
            acquire_gate_surface_destroyed_count: 0,
            #[cfg(test)]
            acquire_gate_destroy_observed_surface_count: None,
            #[cfg(test)]
            committed_release_point_override: None,
            #[cfg(test)]
            release_use_test_probe,
            #[cfg(test)]
            release_use_client_missing_count: 0,
            #[cfg(test)]
            release_use_record_missing_count: 0,
            #[cfg(test)]
            release_use_force_client_missing: false,
            #[cfg(test)]
            release_use_remove_record_after_prepare: false,
            #[cfg(test)]
            effective_window_geometry_calls: 0,
        };

        #[cfg(feature = "bus")]
        let mut state = state;
        #[cfg(feature = "bus")]
        state.refresh_corner_regions();

        // Spawn the first XWayland generation once the event loop exists.
        // Not under `cfg(test)`: the in-process protocol tests must never
        // launch a real Xwayland; the live gate owns that proof.
        #[cfg(all(feature = "xwayland", not(test)))]
        let mut state = state;
        #[cfg(all(feature = "xwayland", not(test)))]
        state.start_xwayland();

        event_loop
            .handle()
            .insert_source(client_disconnect_source, |event, (), state| {
                if let ChannelEvent::Msg(client_id) = event {
                    state.handle_client_disconnect(&client_id);
                }
            })
            .map_err(|error| error.to_string())?;

        event_loop
            .handle()
            .insert_source(capture_release_source, |event, (), state| {
                if let ChannelEvent::Msg(id) = event {
                    state.release_capture_reservation(id);
                }
            })
            .map_err(|error| error.to_string())?;

        event_loop
            .handle()
            .insert_source(retirement_report_source, |event, (), state| match event {
                ChannelEvent::Msg(report) => state.handle_retirement_report(report),
                ChannelEvent::Closed => state.handle_retirement_worker_closed(),
            })
            .map_err(|error| error.to_string())?;

        let listening_socket =
            ListeningSocketSource::with_name(socket_name).map_err(|error| error.to_string())?;
        event_loop
            .handle()
            .insert_source(
                listening_socket,
                |client_stream, (), state: &mut WaylandState| {
                    if let Err(error) = state.display_handle.insert_client(
                        client_stream,
                        Arc::new(WaylandClientState::new(
                            state.client_disconnect_sender.clone(),
                        )),
                    ) {
                        tracing::error!(%error, "failed to register Wayland client");
                    }
                },
            )
            .map_err(|error| error.to_string())?;

        // Registered before the command source, and the order is the point: the
        // command source is the frame-bounded path, and input must not be
        // behind it in any sense. Registration order does not decide calloop
        // dispatch order, so this is not load-bearing for latency — it is
        // load-bearing for reading, since an input source appended after the
        // frame plumbing invites the assumption that it is part of it.
        if let Some(source) = input_source {
            source
                .register(&event_loop.handle())
                .map_err(|error| format!("failed to register input source: {error}"))?;
        }

        #[cfg(feature = "bus")]
        if let Some(source) = port_source {
            event_loop
                .handle()
                .insert_source(source, |event, (), state| match event {
                    ChannelEvent::Msg(PortCommand::Snapshot(request)) => {
                        if state.pending_port_requests.len() < PORT_QUEUE_CAPACITY {
                            state.pending_port_requests.push(request);
                        }
                    }
                    ChannelEvent::Msg(PortCommand::Watch(request)) => {
                        if state.pending_port_controls.len() < PORT_QUEUE_CAPACITY {
                            state
                                .pending_port_controls
                                .push(PortControl::Watch(request));
                        }
                    }
                    ChannelEvent::Msg(PortCommand::Set(request)) => {
                        if state.pending_port_controls.len() < PORT_QUEUE_CAPACITY {
                            state.pending_port_controls.push(PortControl::Set(request));
                        }
                    }
                    ChannelEvent::Msg(PortCommand::WatchState { active, order }) => {
                        if state.pending_port_controls.len() < PORT_QUEUE_CAPACITY {
                            state
                                .pending_port_controls
                                .push(PortControl::WatchState { active, order });
                        } else if let Some(context) = state.port_context.as_ref() {
                            if active {
                                context
                                    .pending_active_order
                                    .fetch_max(order, Ordering::AcqRel);
                            } else {
                                context
                                    .pending_idle_order
                                    .fetch_max(order, Ordering::AcqRel);
                            }
                        }
                    }
                    ChannelEvent::Closed => {}
                })
                .map_err(|error| error.to_string())?;
        }

        event_loop
            .handle()
            .insert_source(command_source, |event, (), state| match event {
                ChannelEvent::Msg(ProtocolCommand::Frame { inputs }) => {
                    state.handle_frame(inputs);
                }
                ChannelEvent::Msg(ProtocolCommand::ReleaseDmabuf { token }) => {
                    state.release_buffer_token(token);
                }
                ChannelEvent::Msg(ProtocolCommand::KmsRenderReply { reply }) => {
                    #[cfg(any(all(feature = "kms-live", not(test)), test))]
                    let ready = match &reply {
                        KmsRenderReply::OutputReady { generation, key } => {
                            Some((*generation, key.clone()))
                        }
                        _ => None,
                    };
                    match state.backend.apply_kms_render_reply(reply) {
                        Ok(commands) if !commands.is_empty() => {
                            for command in commands {
                                if state.kms_render_command_sender.send(command).is_err() {
                                    state.events.push(ProtocolEvent::RuntimeFailed(
                                        "KMS render command receiver disconnected".into(),
                                    ));
                                    state.request_shutdown(ProtocolShutdownCause::RuntimeFailure);
                                    break;
                                }
                            }
                        }
                        Ok(_) => {
                            #[cfg(any(all(feature = "kms-live", not(test)), test))]
                            if let Some((generation, key)) = ready
                                && state.backend.kms_output_is_ready(generation, &key)
                            {
                                state.kms_output_ready(generation, &key);
                            }
                        }
                        Err(error) => {
                            state
                                .events
                                .push(ProtocolEvent::RuntimeFailed(error.to_string()));
                            state.request_shutdown(ProtocolShutdownCause::RuntimeFailure);
                        }
                    }
                }
                ChannelEvent::Msg(ProtocolCommand::SecurityPresented {
                    presentation_epoch,
                    evidence,
                }) => {
                    state.acknowledge_security_presentation(presentation_epoch, evidence);
                }
                ChannelEvent::Msg(ProtocolCommand::CapturePixels(pixels)) => {
                    state.capture_pixels_ready(pixels);
                }
                ChannelEvent::Msg(ProtocolCommand::CaptureDmabufComplete(completion)) => {
                    state.capture_dmabuf_ready(completion);
                }
                ChannelEvent::Msg(ProtocolCommand::CaptureDmabufFailed(failure)) => {
                    state.capture_dmabuf_failed(failure);
                }
                ChannelEvent::Msg(ProtocolCommand::CapturePresented(presented)) => {
                    state.capture_presented(presented);
                }
                ChannelEvent::Msg(ProtocolCommand::CaptureDamageEligible {
                    id,
                    generation,
                    security_epoch,
                    revision,
                    damage,
                }) => {
                    if state.capture_frames.get(&id).is_some_and(|frame| {
                        frame.generation == generation
                            && frame.security_epoch == security_epoch
                            && frame.submitted
                            && !frame.terminal
                            && !frame.job_pending
                    }) {
                        state.admit_capture(id, revision, damage);
                    }
                }
                ChannelEvent::Msg(ProtocolCommand::CaptureFailed {
                    id,
                    generation,
                    security_epoch,
                }) => {
                    if state.capture_frames.get(&id).is_some_and(|frame| {
                        frame.generation == generation && frame.security_epoch == security_epoch
                    }) {
                        state.fail_capture(id);
                    }
                }
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                ChannelEvent::Msg(ProtocolCommand::KmsTopologyLifecycle {
                    event,
                    acknowledgement,
                }) => {
                    #[cfg(feature = "bus")]
                    state.mark_output_topology_before_change();
                    let pause = matches!(&event, KmsTopologyLifecycleEvent::Pause);
                    let resumed = matches!(&event, KmsTopologyLifecycleEvent::Resume(_));
                    let previous_kms_outputs = state.backend.kms_registered_outputs();
                    if pause {
                        let kms_captures = state
                            .capture_frames
                            .iter()
                            .filter_map(|(id, frame)| {
                                matches!(frame.source_id, CaptureSourceId::Kms { .. })
                                    .then_some(*id)
                            })
                            .collect::<Vec<_>>();
                        for id in kms_captures {
                            state.fail_capture(id);
                        }
                        state.kms_authority_lost();
                    }
                    let previous_scale = state.backend.output_scale();
                    let previous_output = state.logical_output_rect();
                    let previous_usable = state.usable_output_rect();
                    let result = state
                        .backend
                        .apply_kms_topology_lifecycle(event)
                        .map_err(|error| error.to_string())
                        .and_then(|commands| {
                            let presentation_generations = if pause {
                                BTreeMap::new()
                            } else {
                                state.backend.kms_presentation_generations()
                            };
                            state.retain_current_kms_capture_baselines(&presentation_generations);
                            state.events.push(ProtocolEvent::CaptureKmsSourcesRetired {
                                current: presentation_generations.clone(),
                            });
                            if !pause {
                                state.begin_pointer_hit_test_batch();
                                let mapped_surfaces = state
                                    .surfaces
                                    .values()
                                    .filter(|record| {
                                        record.layout.visible
                                            && !matches!(record.role, SurfaceRole::Layer(_))
                                    })
                                    .map(|record| record.role.wl_surface().clone())
                                    .collect::<Vec<_>>();
                                state.backend.reconcile_kms_client_output::<WaylandState>(
                                    &state.display_handle,
                                    &mapped_surfaces,
                                );
                                let current_kms_outputs = state.backend.kms_registered_outputs();
                                state.retire_replaced_kms_lock_surfaces(
                                    &previous_kms_outputs,
                                    &current_kms_outputs,
                                );
                                let replaced_captures = state
                                    .capture_frames
                                    .iter()
                                    .filter_map(|(id, frame)| {
                                        (!kms_capture_source_is_current(
                                            &frame.source_id,
                                            &presentation_generations,
                                        ))
                                        .then_some(*id)
                                    })
                                    .collect::<Vec<_>>();
                                for id in replaced_captures {
                                    state.fail_capture(id);
                                }
                                state.kms_begin_preparing(presentation_generations, resumed);
                                state.reconcile_layer_output_bindings();
                                let scale = state.backend.output_scale();
                                if scale != previous_scale {
                                    state.publish_surface_preferred_scale(scale);
                                }
                                state.reconcile_output_after_topology_change_if_needed(
                                    previous_output,
                                    previous_usable,
                                );
                                state.end_pointer_hit_test_batch();
                            }
                            for command in commands {
                                state.kms_render_command_sender.send(command).map_err(|_| {
                                    "KMS render command receiver disconnected".to_string()
                                })?;
                            }
                            Ok(())
                        });
                    if let Err(error) = &result {
                        state
                            .events
                            .push(ProtocolEvent::RuntimeFailed(error.clone()));
                        state.request_shutdown(ProtocolShutdownCause::RuntimeFailure);
                    }
                    let _ = acknowledgement.send(result);
                }
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                ChannelEvent::Msg(ProtocolCommand::QuerySessionLockActive { acknowledgement }) => {
                    let _ = acknowledgement.send(state.session_lock_active());
                }
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                ChannelEvent::Msg(ProtocolCommand::FlushEvents { acknowledgement }) => {
                    // Acknowledge only after `dispatch_cycle` has offered the
                    // outbox to the renderer, not here while publication is
                    // still pending at the end of this dispatch.
                    state.event_flush_acknowledgements.push(acknowledgement);
                }
                #[cfg(test)]
                ChannelEvent::Msg(ProtocolCommand::Barrier { acknowledgement }) => {
                    let _ = acknowledgement.send(());
                }
                ChannelEvent::Msg(ProtocolCommand::Shutdown) => {
                    state.request_shutdown(ProtocolShutdownCause::Orderly);
                }
                ChannelEvent::Closed => {
                    state.request_shutdown(ProtocolShutdownCause::RuntimeFailure);
                }
            })
            .map_err(|error| error.to_string())?;

        let display_poll_fd = display
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| format!("failed to clone Wayland display poll fd: {error}"))?;
        event_loop
            .handle()
            .insert_source(
                Generic::new(display_poll_fd, Interest::READ, PollMode::Level),
                |_, _, _| Ok(PostAction::Continue),
            )
            .map_err(|error| error.to_string())?;

        // `explicit_sync_global_advertised` is logged here rather than only on
        // the dispatch-start `debug!` so an operator at default level can always
        // tell whether `linux-drm-syncobj-v1` went live, in every run and
        // regardless of whether import-device preparation succeeded.
        tracing::info!(
            socket = socket_name,
            adapter = dmabuf_capabilities.adapter_name,
            main_device = dmabuf_capabilities.main_device,
            dmabuf_formats = dmabuf_capabilities.formats.len(),
            explicit_sync_import_device_prepared = prepared_import_device.is_some(),
            explicit_sync_global_advertised,
            "Wayland socket ready"
        );
        Ok((
            Self {
                event_loop,
                display,
                state,
                prepared_import_device,
                retirement_worker,
                event_sender,
                pending_events: PendingProtocolEvents::default(),
                dirty_surfaces: DirtySurfaces::default(),
                dirty_cursor: false,
            },
            ExplicitSyncStartupReport {
                preparation: explicit_sync_preparation,
                global_advertised: explicit_sync_global_advertised,
            },
        ))
    }

    fn run(&mut self) -> Result<(), String> {
        tracing::debug!(
            explicit_sync_import_device_prepared = self.prepared_import_device.is_some(),
            explicit_sync_global_advertised = self.state.explicit_sync_global_advertised,
            "starting Wayland protocol dispatch"
        );
        let result = (|| {
            while self.state.shutdown_cause.is_none() {
                self.dispatch_cycle(None)?;
            }
            Ok(())
        })();
        // Orderly XWayland teardown: descriptor removed first, launches
        // refused, X records destroyed, source/XWM dropped — before the rest
        // of protocol teardown proceeds.
        #[cfg(feature = "xwayland")]
        self.state.shutdown_xwayland();
        let reason = match &result {
            Ok(()) => self
                .state
                .shutdown_cause
                .expect("successful protocol run records its shutdown cause")
                .release_use_abandon_reason(),
            Err(_) => ReleaseUseAbandonReason::DispatchFailure,
        };
        let abandoned = self.state.release_uses.abandon_all(reason);
        if abandoned > 0 {
            tracing::warn!(abandoned, ?reason, "abandoned DMA-BUF release uses");
        }
        result
    }

    fn dispatch_cycle(&mut self, timeout: Option<Duration>) -> Result<(), String> {
        self.event_loop
            .dispatch(timeout, &mut self.state)
            .map_err(|error| format!("calloop dispatch failed: {error}"))?;
        self.display
            .dispatch_clients(&mut self.state)
            .map_err(|error| format!("Wayland client dispatch failed: {error}"))?;
        self.state.reconcile_subsurface_roles();
        self.state.backend.maintain_after_protocol_dispatch();
        self.state.popup_manager.cleanup();
        #[cfg(feature = "bus")]
        port_observation::service_observations(&mut self.state);
        #[cfg(feature = "bus")]
        port_snapshot::service_requests(&mut self.state);
        self.display
            .flush_clients()
            .map_err(|error| format!("Wayland client flush failed: {error}"))?;
        let published = self.publish_events();
        let flush_result = match &published {
            Ok(EventPublication::Complete) => Ok(EventFlushOutcome::Complete),
            Ok(EventPublication::Pending) => Ok(EventFlushOutcome::Pending),
            Ok(EventPublication::Disconnected) => {
                Err("renderer event receiver disconnected".into())
            }
            Err(error) => Err(error.clone()),
        };
        for acknowledgement in mem::take(&mut self.state.event_flush_acknowledgements) {
            let _ = acknowledgement.send(flush_result.clone());
        }
        published.map(|_| ())
    }

    fn publish_events(&mut self) -> Result<EventPublication, String> {
        // Before `state.events`, so that a surface which *also* committed a
        // fresh buffer this dispatch has its complete upsert queued on the
        // normal path — `queue_renderer_event` then clears the dirty mark, and
        // the retained frame is never republished on top of the newer one.
        self.dirty_surfaces
            .extend(mem::take(&mut self.state.pending_full_upserts));
        self.dirty_cursor |= mem::take(&mut self.state.pending_cursor_update);

        for event in mem::take(&mut self.state.events) {
            self.queue_renderer_event(event);
        }

        if self.dirty_cursor
            && let Some(event) = self.state.latest_cursor_update()
        {
            self.queue_renderer_event(event);
        }

        // Taking the batch is what spends these surfaces' turns; see the
        // method. The outcomes below only decide who stays on the worklist.
        let recovery_ids = self.dirty_surfaces.take_recovery_batch();
        for id in recovery_ids {
            match self.state.latest_surface_upsert(id) {
                LatestSurfaceUpsert::Ready(event) => {
                    self.queue_renderer_event(*event);
                }
                LatestSurfaceUpsert::Gone => {
                    self.dirty_surfaces.remove(&id);
                }
                LatestSurfaceUpsert::Retry => {}
            }
        }
        if self.pending_events.is_empty() {
            return Ok(EventPublication::Complete);
        }

        match self.event_sender.try_send(self.pending_events.take()) {
            Ok(()) => Ok(EventPublication::Complete),
            Err(TrySendError::Full(events)) => {
                self.pending_events = PendingProtocolEvents::from_events(events).map_err(|_| {
                    "renderer returned an event batch larger than its accepted budget".to_string()
                })?;
                Ok(EventPublication::Pending)
            }
            // The renderer owns the receiver and may deliberately drop it
            // before the coordinator stops this thread. That says only that
            // there is no scene consumer left; protocol-worker death is
            // reported independently by the thread's failure guard.
            Err(TrySendError::Disconnected(_)) => Ok(EventPublication::Disconnected),
        }
    }

    fn queue_renderer_event(&mut self, event: ProtocolEvent) {
        self.queue_renderer_event_with_limit(event, MAX_PENDING_SURFACE_EVENT_BYTES);
    }

    #[cfg(test)]
    fn queue_renderer_event_with_test_limit(&mut self, event: ProtocolEvent, limit: usize) {
        self.queue_renderer_event_with_limit(event, limit);
    }

    fn queue_renderer_event_with_limit(&mut self, event: ProtocolEvent, limit: usize) {
        if matches!(&event, ProtocolEvent::SecurityScene { active: true, .. }) {
            // Lock entry is a hard publication boundary. A normal-surface
            // upsert may already be retained from an earlier full channel;
            // purge it before the empty/lock-only roster is installed.
            let retained = self.state.mapped_surface_ids();
            let retired = self.pending_events.retain_surface_events(&retained);
            for token in retired {
                self.state.release_buffer_token(token);
            }
            self.dirty_surfaces.retain(|id| retained.contains(id));
        }
        let surface_id = protocol_event_surface_id(&event);
        let complete_state = matches!(event, ProtocolEvent::SurfaceUpserted { .. });
        let cursor_state = matches!(event, ProtocolEvent::CursorUpdated { .. });
        // Which recovery route a rejected event needs is a property of the
        // *surface*, not of the event's variant. `dirty_surfaces` re-derives an
        // upsert from the compositor's records, and `latest_surface_upsert`
        // answers `Gone` for anything that is not presentable — so a mark set
        // for a surface in that state is dropped again on the next pass and the
        // renderer keeps its entity forever. Only membership can remove it.
        //
        // Classifying by variant was a proxy for this, and an incomplete one.
        // A surface can go non-presentable *after* an event about it is
        // produced and before the outbox is drained — a client that commits a
        // buffer and then destroys the role object in the same dispatch leaves
        // a complete, correctly-produced upsert queued for a record that is
        // already dormant. `push_surface_upsert` cannot catch that; it decides
        // at production time. Rejecting such an upsert took the recoverable
        // branch, the mark was dropped as `Gone`, and no roster was installed —
        // the renderer went on drawing a surface whose role the client had
        // destroyed. So ask the predicate the roster and recovery already share.
        let recoverable = surface_id.is_none_or(|id| self.state.surface_is_recoverable(id));
        if !recoverable && let Some(id) = surface_id {
            self.dirty_surfaces.remove(&id);
        }
        match self.pending_events.push_with_limit(event, limit) {
            Ok(result) => {
                for token in result.retired_tokens {
                    self.state.release_buffer_token(token);
                }
                self.dirty_surfaces.extend(result.evicted_surfaces);
                if complete_state && let Some(id) = surface_id {
                    self.dirty_surfaces.remove(&id);
                }
                if cursor_state {
                    self.dirty_cursor = false;
                }
            }
            Err(event) => {
                let event = *event;
                if let ProtocolEvent::CaptureRequested(request) = &event {
                    self.state.fail_capture(request.id);
                    return;
                }
                if let Some(token) = protocol_event_dmabuf_token(&event) {
                    self.state.release_buffer_token(token);
                }
                if !recoverable {
                    // This surface has no per-surface recovery route, for the
                    // reason spelled out where `recoverable` is computed.
                    // Converge on membership instead: the roster states which
                    // surfaces should exist, which is a superset of what this
                    // one event was going to say, and it subsumes every other
                    // queued event for a departed surface rather than joining
                    // the queue behind them.
                    let mapped = self.state.mapped_surface_ids();
                    let compaction = self.pending_events.install_surface_roster(&mapped);
                    for token in compaction.retired_tokens {
                        self.state.release_buffer_token(token);
                    }
                    self.dirty_surfaces.retain(|id| mapped.contains(id));
                    // A roster removes; it never creates. A surface whose stale
                    // tombstone the roster just discarded may have no renderer
                    // entity at all, so it needs an upsert, and only
                    // `dirty_surfaces` can produce one.
                    self.dirty_surfaces.extend(compaction.resurrected);
                    tracing::warn!(
                        surface_id = surface_id.map(|id| id.0),
                        mapped = mapped.len(),
                        "an event for a surface the renderer must stop holding \
                         did not fit the bounded outbox; converging the \
                         renderer on the surface roster instead"
                    );
                } else {
                    if let Some(id) = surface_id {
                        self.dirty_surfaces.insert(id);
                    }
                    if cursor_state {
                        self.dirty_cursor = true;
                    }
                    tracing::warn!(
                        bytes = protocol_event_retained_bytes(&event),
                        "renderer update deferred because it cannot fit the bounded outbox"
                    );
                }
            }
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn retain_current_kms_damage_baselines(
    baselines: &mut HashMap<CaptureSourceId, u64>,
    generations: &BTreeMap<crate::backend::kms::OutputKey, u64>,
) {
    baselines.retain(|source, _| kms_capture_source_is_current(source, generations));
}

impl Drop for ProtocolServer {
    fn drop(&mut self) {
        // `state` still owns the sole request sender while this destructor is
        // running. Close it unconditionally before any fallible cleanup so an
        // idle worker's blocking receive can finish without a timer.
        self.state.release_uses.stop_retirement_worker();
        // A worker already inside the bounded GPU wait cannot be joined here:
        // doing so would make protocol teardown wait on the renderer. Its only
        // report endpoint belongs to the protocol event loop, so detaching is
        // safe after closing the sole request sender above.
        self.retirement_worker.detach();
        if std::thread::panicking() {
            let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.abandon_release_uses_from_drop();
            }));
            if cleanup.is_err() {
                tracing::error!(
                    "release-use cleanup also panicked during protocol-server unwinding"
                );
            }
        } else {
            self.abandon_release_uses_from_drop();
        }
    }
}

impl ProtocolServer {
    fn abandon_release_uses_from_drop(&mut self) {
        let abandoned = self
            .state
            .release_uses
            .abandon_all(ReleaseUseAbandonReason::ServerDrop);
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                reason = ?ReleaseUseAbandonReason::ServerDrop,
                "abandoned DMA-BUF release uses from protocol-server drop backstop"
            );
        }
    }
}

/// Position in the recovery worklist. Strictly increasing, so comparing two
/// tickets answers which of the two surfaces has been waiting longer.
type DirtyTicket = u64;

/// Surfaces whose renderer state is behind the compositor's, and the order in
/// which recovery serves them.
///
/// Recovery is rate-limited to `DIRTY_SURFACE_RECOVERY_BATCH` surfaces per
/// dispatch, so *which* surfaces a batch names decides whether a surface is
/// ever served at all. Selecting them by surface id does not: ids come from a
/// strictly monotonic counter that is never recycled, so every surface a client
/// creates from now on sorts above every surface already waiting. A rotating
/// id cursor never wraps past a client that keeps creating surfaces, and the
/// surfaces below the cursor are starved for the life of the compositor — the
/// renderer goes on showing stale content for a real window, and the trigger is
/// ordinary client traffic. Id order is the one order a client controls.
///
/// So order by waiting time instead, which no client can rewrite. Each surface
/// takes a ticket when it *becomes* dirty and keeps it until it is served;
/// selection takes the oldest tickets. Two rules make that a bound rather than
/// a heuristic:
///
/// - Marking a surface that is already dirty leaves its ticket alone. If
///   re-marking moved a surface to the tail, a client whose traffic evicts
///   *another* client's surface from the outbox could hold that surface at the
///   tail forever, which is the same starvation wearing a different hat.
/// - Being attempted moves a surface to the tail, whatever the outcome. See
///   the call in `publish_events`.
///
/// Together those give the property this type is for: while a surface waits, it
/// is never overtaken by one marked after it, so a surface with `n` ahead of it
/// is *attempted* within `ceil((n + 1) / DIRTY_SURFACE_RECOVERY_BATCH)`
/// dispatches, however much newer work arrives meanwhile. "While it waits" is
/// the whole of it: being attempted hands the surface a tail ticket, so anything
/// marked during its wait is now ahead of it, and the bound restarts from that
/// surface's new place. That is the second rule working, not an exception to the
/// first — a turn spent is a turn spent.
///
/// Two things that bound deliberately do not follow from it, because claiming
/// either would overstate what this ordering can promise:
///
/// - It bounds attempts, not successes. `latest_surface_upsert` may answer
///   `Retry` for as long as the condition lasts, and a recovered upsert may be
///   rejected by the outbox again; the surface keeps its turn each time round,
///   but nothing here makes the underlying condition clear.
/// - It is relative, not absolute. `MAX_GLOBAL_SURFACES` caps the surfaces a
///   client may have *live*, and does not cap this worklist: a mark *can*
///   outlive its surface, and the eviction site takes whatever ids the outbox
///   had to drop. Three sites retire a mark early — `Gone` in `publish_events`,
///   the non-recoverable branch in `queue_renderer_event_with_limit`, and roster
///   compaction's `retain` — but none of them is a bound, because none is
///   guaranteed to run for any particular mark. So `n` has no ceiling worth
///   quoting, and the guarantee is that waiting is what decides the order —
///   not that the wait is shorter than some number of dispatches.
#[derive(Default)]
struct DirtySurfaces {
    tickets: HashMap<SurfaceId, DirtyTicket>,
    next_ticket: DirtyTicket,
}

impl DirtySurfaces {
    /// Mark `id` dirty, if it is not already. A surface that is already
    /// waiting keeps the ticket it has, for the reason on the type.
    fn insert(&mut self, id: SurfaceId) {
        if self.tickets.contains_key(&id) {
            return;
        }
        let ticket = self.allocate_ticket();
        self.tickets.insert(id, ticket);
    }

    fn remove(&mut self, id: &SurfaceId) {
        self.tickets.remove(id);
    }

    fn retain(&mut self, mut keep: impl FnMut(&SurfaceId) -> bool) {
        self.tickets.retain(|id, _| keep(id));
    }

    /// The surfaces recovery should attempt this dispatch — the oldest
    /// tickets, oldest first — moved to the tail of the worklist as they are
    /// handed over.
    ///
    /// Selecting and requeueing are one operation because they must not be
    /// separable. An attempt spends a surface's turn whether or not it
    /// succeeds: `latest_surface_upsert` can answer `Retry` indefinitely, and
    /// a `Ready` upsert can be rejected by the outbox and marked again. Either
    /// one, keeping its old place, would win selection again on the very next
    /// dispatch and hold the slot against work that has waited longer. Handing
    /// the caller a batch to requeue afterwards would leave that starvation
    /// one forgotten line away.
    ///
    /// Surfaces the caller's outcomes then remove from the worklist are simply
    /// gone; this reorders what is waiting, it never marks anything dirty.
    fn take_recovery_batch(&mut self) -> Vec<SurfaceId> {
        let batch = self.oldest_waiting();
        for id in &batch {
            // Cannot be vacant — `oldest_waiting` read these out of the map.
            let ticket = self.allocate_ticket();
            self.tickets.insert(*id, ticket);
        }
        batch
    }

    fn oldest_waiting(&self) -> Vec<SurfaceId> {
        // Nothing is waiting on almost every dispatch, and this runs on all of
        // them: the heap below would allocate its capacity before discovering
        // there was nothing to put in it.
        if self.tickets.is_empty() {
            return Vec::new();
        }
        // A heap bounded to the batch size, rather than sorting the worklist:
        // this runs every dispatch and the worklist can hold every surface in
        // the compositor, so the cost is O(worklist * log batch), not
        // O(worklist * log worklist). The heap holds the *largest* tickets
        // seen so far at its root, and drops that root once it is over size,
        // so what survives is the batch's worth of smallest tickets.
        let mut oldest = BinaryHeap::with_capacity(DIRTY_SURFACE_RECOVERY_BATCH + 1);
        for (id, ticket) in &self.tickets {
            oldest.push((*ticket, id.0));
            if oldest.len() > DIRTY_SURFACE_RECOVERY_BATCH {
                oldest.pop();
            }
        }
        let mut batch = oldest.into_vec();
        batch.sort_unstable();
        batch
            .into_iter()
            .map(|(_, id)| SurfaceId(id))
            .collect::<Vec<_>>()
    }

    fn allocate_ticket(&mut self) -> DirtyTicket {
        if self.next_ticket == DirtyTicket::MAX {
            self.rebase_tickets();
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        ticket
    }

    /// Renumber the worklist from zero, keeping every surface's place, so that
    /// ticket allocation never has to saturate.
    ///
    /// Saturating instead would hand every later surface the same ticket, which
    /// collapses the waiting order this type exists to keep and puts the
    /// starvation straight back. Unreachable in practice at one ticket per
    /// mark, but the alternative is a silent failure rather than a loud one.
    fn rebase_tickets(&mut self) {
        let mut ordered = self
            .tickets
            .iter()
            .map(|(id, ticket)| (*ticket, *id))
            .collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|(ticket, _)| *ticket);
        for (place, (_, id)) in ordered.into_iter().enumerate() {
            self.tickets.insert(id, place as DirtyTicket);
        }
        self.next_ticket = self.tickets.len() as DirtyTicket;
    }
}

#[cfg(test)]
impl DirtySurfaces {
    fn contains(&self, id: &SurfaceId) -> bool {
        self.tickets.contains_key(id)
    }

    fn clear(&mut self) {
        self.tickets.clear();
    }

    fn len(&self) -> usize {
        self.tickets.len()
    }

    fn ticket_of(&self, id: &SurfaceId) -> Option<DirtyTicket> {
        self.tickets.get(id).copied()
    }

    /// Drive the worklist to the brink of ticket exhaustion, so that the next
    /// allocation has to rebase. Reaching it by marking surfaces would take
    /// longer than the hardware lasts.
    fn exhaust_tickets_for_test(&mut self) {
        self.next_ticket = DirtyTicket::MAX;
    }
}

impl Extend<SurfaceId> for DirtySurfaces {
    fn extend<T: IntoIterator<Item = SurfaceId>>(&mut self, ids: T) {
        for id in ids {
            self.insert(id);
        }
    }
}

#[derive(Default)]
struct PendingProtocolEvents {
    security: Option<ProtocolEvent>,
    roster: Option<ProtocolEvent>,
    output: Option<ProtocolEvent>,
    cursor: Option<ProtocolEvent>,
    surfaces: HashMap<SurfaceId, ProtocolEvent>,
    dmabuf_invalidations: HashSet<DmabufBufferId>,
    invalidate_all_dmabufs: bool,
    current_kms_capture_sources: Option<BTreeMap<crate::backend::kms::OutputKey, u64>>,
    captures: Vec<ProtocolEvent>,
    runtime_failures: Vec<ProtocolEvent>,
    bytes: usize,
    pressure_warned: bool,
}

#[derive(Default)]
struct PendingPush {
    retired_tokens: Vec<u64>,
    evicted_surfaces: Vec<SurfaceId>,
}

/// What installing a surface roster discarded, and what the caller owes as a
/// result.
#[derive(Default)]
struct RosterCompaction {
    /// Renderer DMA-BUF tokens carried by discarded upserts. The caller must
    /// release each one or the buffer is never returned to its client.
    retired_tokens: Vec<u64>,
    /// Surfaces the roster lists whose queued tombstone was discarded as stale.
    /// The renderer may hold no entity for them, and the roster does not create
    /// one, so the caller must mark them for upsert recovery.
    resurrected: Vec<SurfaceId>,
}

impl PendingProtocolEvents {
    fn from_events(events: Vec<ProtocolEvent>) -> Result<Self, Box<ProtocolEvent>> {
        let mut pending = Self::default();
        for event in events {
            pending.push(event)?;
        }
        Ok(pending)
    }

    fn is_empty(&self) -> bool {
        self.security.is_none()
            && self.roster.is_none()
            && self.output.is_none()
            && self.cursor.is_none()
            && self.surfaces.is_empty()
            && self.dmabuf_invalidations.is_empty()
            && !self.invalidate_all_dmabufs
            && self.current_kms_capture_sources.is_none()
            && self.captures.is_empty()
            && self.runtime_failures.is_empty()
    }

    /// Drop queued surface deltas that the renderer must no longer observe.
    /// Returns renderer-owned DMA-BUF tokens carried by discarded upserts.
    fn retain_surface_events(&mut self, retained: &HashSet<SurfaceId>) -> Vec<u64> {
        let mut retired_tokens = Vec::new();
        let mut bytes = self.bytes;
        self.surfaces.retain(|id, event| {
            if retained.contains(id) {
                return true;
            }
            bytes = bytes.saturating_sub(protocol_event_retained_bytes(event));
            if let Some(token) = protocol_event_dmabuf_token(event) {
                retired_tokens.push(token);
            }
            false
        });
        self.bytes = bytes;
        retired_tokens
    }

    fn push(&mut self, event: ProtocolEvent) -> Result<PendingPush, Box<ProtocolEvent>> {
        self.push_with_limit(event, MAX_PENDING_SURFACE_EVENT_BYTES)
    }

    /// Replace the pending roster with `mapped`, and compact the queued
    /// per-surface events against it.
    ///
    /// The roster is a statement about *now*, and everything already queued was
    /// queued before it. Two kinds of queued event therefore contradict it and
    /// are discarded:
    ///
    /// - anything at all for a surface the roster does **not** list — the
    ///   renderer is about to remove that entity, so a queued upsert would
    ///   recreate it and a queued tombstone is redundant;
    /// - a **tombstone** for a surface the roster **does** list. A `SurfaceId`
    ///   outlives an unmap, so a surface can be unmapped, queued as a tombstone,
    ///   and mapped again before the batch goes out. Emitting the roster and
    ///   then that tombstone would remove an entity the roster had just called
    ///   live, and nothing behind it in the batch says otherwise. Its id is
    ///   returned as resurrected so the caller can mark it for upsert recovery.
    ///
    /// A mapped surface's queued upsert or relayout is newer state for a
    /// surface that still exists, so it is kept.
    ///
    /// Discarded bytes come back, which is what makes this a recovery route
    /// rather than one more thing competing for the outbox: the case that makes
    /// a removal unadmittable is a saturated outbox, and under churn its
    /// contents are dominated by tombstones the roster subsumes.
    ///
    /// Returns the renderer DMA-BUF tokens carried by discarded upserts. The
    /// caller owns releasing them; this type has no access to
    /// [`WaylandState::release_buffer_token`].
    ///
    /// That token return is defensive rather than currently reachable, and the
    /// reason is worth writing down because a sizing change would make it live.
    /// Write `E` for a tombstone's charge, `C` for the total bytes of every
    /// eviction candidate, `A` for whatever is already queued under the incoming
    /// id, `B` for the bytes currently charged and `L` for the limit.
    /// `push_with_limit` rejects a lifecycle event exactly when taking every
    /// candidate still leaves no room — `B - A - C + E > L`, i.e.
    /// `E > A + C + (L - B)`. A queued DMA-BUF upsert is charged exactly `E` and
    /// contributes that `E` to either `A` or `C`; since the production limit is
    /// constant and admission keeps `B <= L`, the inequality cannot then hold.
    /// A DMA-BUF upsert therefore always rescues the tombstone rather than being
    /// discarded here. Charge DMA-BUF upserts anything less than a whole
    /// `ProtocolEvent` and that stops being true, at which point dropping the
    /// token here would withhold a `wl_buffer.release` for the lifetime of the
    /// compositor.
    ///
    /// Note what that argument does *not* say: rejection does not imply the
    /// outbox held only absent tombstones. It may equally have been retaining an
    /// `OutputResized`, or upserts whose combined charge is under `E`. "The
    /// outbox comes back empty" is a property of a fully-saturated churn
    /// fixture, not an invariant of this function.
    fn install_surface_roster(&mut self, mapped: &HashSet<SurfaceId>) -> RosterCompaction {
        let mut compaction = RosterCompaction::default();
        let mut bytes = self.bytes;
        self.surfaces.retain(|id, queued| {
            let listed = mapped.contains(id);
            if listed
                && !matches!(
                    queued,
                    ProtocolEvent::SurfaceUnmapped { .. } | ProtocolEvent::SurfaceDestroyed { .. }
                )
            {
                return true;
            }
            bytes = bytes.saturating_sub(protocol_event_retained_bytes(queued));
            if let Some(token) = protocol_event_dmabuf_token(queued) {
                compaction.retired_tokens.push(token);
            }
            if listed {
                compaction.resurrected.push(*id);
            }
            false
        });
        self.bytes = bytes;
        let mut mapped = mapped.iter().copied().collect::<Vec<_>>();
        mapped.sort_unstable_by_key(|id| id.0);
        self.roster = Some(ProtocolEvent::SurfaceRoster { mapped });
        compaction
    }

    fn push_with_limit(
        &mut self,
        event: ProtocolEvent,
        limit: usize,
    ) -> Result<PendingPush, Box<ProtocolEvent>> {
        // The roster is admitted unconditionally and charged nothing. Its
        // worst case is reserved out of the budget once, in
        // `MAX_PENDING_SURFACE_EVENT_BYTES`, because it is a singleton bounded
        // by `MAX_GLOBAL_SURFACES` — and because rejecting the event whose job
        // is to recover from a rejection would be circular. This arm is also
        // what round-trips a roster back in through `from_events` when a full
        // channel hands the batch back.
        match event {
            ProtocolEvent::SecurityScene { .. } => {
                self.security = Some(event);
                return Ok(PendingPush::default());
            }
            ProtocolEvent::SurfaceRoster { .. } => {
                self.roster = Some(event);
                return Ok(PendingPush::default());
            }
            ProtocolEvent::DmabufBufferDestroyed { buffer_id } => {
                self.push_dmabuf_invalidation(buffer_id);
                return Ok(PendingPush::default());
            }
            ProtocolEvent::DmabufCacheInvalidated => {
                self.invalidate_all_dmabufs = true;
                self.dmabuf_invalidations.clear();
                return Ok(PendingPush::default());
            }
            ProtocolEvent::CaptureKmsSourcesRetired { current } => {
                self.current_kms_capture_sources = Some(current);
                return Ok(PendingPush::default());
            }
            ProtocolEvent::CaptureRequested(_) | ProtocolEvent::CaptureDamageWatch(_)
                if self.captures.len() < MAX_CAPTURE_FRAMES =>
            {
                self.captures.push(event);
                return Ok(PendingPush::default());
            }
            ProtocolEvent::CaptureRequested(_) | ProtocolEvent::CaptureDamageWatch(_) => {
                return Err(Box::new(event));
            }
            _ => {}
        }
        let old_bytes = match &event {
            ProtocolEvent::SecurityScene { .. } => 0,
            ProtocolEvent::OutputResized { .. } => self
                .output
                .as_ref()
                .map_or(0, protocol_event_retained_bytes),
            ProtocolEvent::CursorUpdated { .. } => self
                .cursor
                .as_ref()
                .map_or(0, protocol_event_retained_bytes),
            ProtocolEvent::SurfaceUpserted { id, .. }
            | ProtocolEvent::SurfaceRelayout { id, .. }
            | ProtocolEvent::SurfaceUnmapped { id }
            | ProtocolEvent::SurfaceDestroyed { id } => self
                .surfaces
                .get(id)
                .map_or(0, protocol_event_retained_bytes),
            // Charged nothing, in both directions, so that deleting the fast
            // path above degrades to "admitted and stored without a charge"
            // rather than to a roster that can be rejected or evicted against.
            ProtocolEvent::SurfaceRoster { .. }
            | ProtocolEvent::DmabufBufferDestroyed { .. }
            | ProtocolEvent::DmabufCacheInvalidated
            | ProtocolEvent::CaptureKmsSourcesRetired { .. }
            | ProtocolEvent::CaptureRequested(_)
            | ProtocolEvent::CaptureDamageWatch(_)
            | ProtocolEvent::RuntimeFailed(_) => 0,
        };
        let new_bytes = match &event {
            ProtocolEvent::SecurityScene { .. }
            | ProtocolEvent::SurfaceRoster { .. }
            | ProtocolEvent::DmabufBufferDestroyed { .. }
            | ProtocolEvent::DmabufCacheInvalidated
            | ProtocolEvent::CaptureKmsSourcesRetired { .. }
            | ProtocolEvent::CaptureRequested(_)
            | ProtocolEvent::CaptureDamageWatch(_) => 0,
            ProtocolEvent::SurfaceRelayout { id, scene }
                if matches!(
                    self.surfaces.get(id),
                    Some(ProtocolEvent::SurfaceUpserted { .. })
                ) =>
            {
                self.surfaces
                    .get(id)
                    .and_then(|queued| match queued {
                        ProtocolEvent::SurfaceUpserted { scene, .. } => Some(scene),
                        _ => None,
                    })
                    .map_or(old_bytes, |queued_scene| {
                        old_bytes
                            .saturating_sub(queued_scene.title.as_deref().map_or(0, str::len))
                            .saturating_add(scene.title.as_deref().map_or(0, str::len))
                    })
            }
            ProtocolEvent::SurfaceRelayout { id, .. }
                if matches!(
                    self.surfaces.get(id),
                    Some(
                        ProtocolEvent::SurfaceUnmapped { .. }
                            | ProtocolEvent::SurfaceDestroyed { .. }
                    )
                ) =>
            {
                old_bytes
            }
            ProtocolEvent::RuntimeFailed(_) if !self.runtime_failures.is_empty() => 0,
            _ => protocol_event_retained_bytes(&event),
        };
        if new_bytes > limit {
            return Err(Box::new(event));
        }

        let mut result = PendingPush::default();
        let incoming_id = protocol_event_surface_id(&event);
        // Eviction is staged, not applied, until admission is certain.
        //
        // The caller learns about displaced state only through the `Ok` arm: it
        // releases `retired_tokens` and re-dirties `evicted_surfaces` from
        // there. An `Err` says "this one event did not fit" and carries nothing
        // else, so anything already removed on the way to that `Err` would be
        // gone with no owner and no recovery route — a displaced DMA-BUF's
        // renderer token would never reach `release_buffer_token`, and its
        // `wl_buffer.release` and explicit-sync retirement would be withheld
        // for the lifetime of the compositor. Removing only after the final
        // admission check succeeds, and rolling back exactly when it does not,
        // makes that unreachable by construction rather than by argument about
        // which limits can be hit.
        let bytes_before = self.bytes;
        let mut evicted: Vec<(SurfaceId, ProtocolEvent)> = Vec::new();
        if bounded_replacement_bytes(self.bytes, old_bytes, new_bytes, limit).is_none() {
            let mut candidates = self
                .surfaces
                .iter()
                .filter_map(|(id, queued)| {
                    (*id != incoming_id.unwrap_or(SurfaceId(0))
                        && matches!(
                            queued,
                            ProtocolEvent::SurfaceUpserted { .. }
                                | ProtocolEvent::SurfaceRelayout { .. }
                        ))
                    .then_some((*id, protocol_event_retained_bytes(queued)))
                })
                .collect::<Vec<_>>();
            candidates.sort_unstable_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
            for (id, bytes) in candidates {
                let Some(removed) = self.surfaces.remove(&id) else {
                    continue;
                };
                self.bytes = self.bytes.saturating_sub(bytes);
                evicted.push((id, removed));
                if bounded_replacement_bytes(self.bytes, old_bytes, new_bytes, limit).is_some() {
                    break;
                }
            }
        }
        let Some(prospective) = bounded_replacement_bytes(self.bytes, old_bytes, new_bytes, limit)
        else {
            for (id, restored) in evicted {
                self.surfaces.insert(id, restored);
            }
            self.bytes = bytes_before;
            return Err(Box::new(event));
        };
        for (id, removed) in evicted {
            if let Some(token) = protocol_event_dmabuf_token(&removed) {
                result.retired_tokens.push(token);
            }
            result.evicted_surfaces.push(id);
            if !self.pressure_warned {
                self.pressure_warned = true;
                tracing::warn!(
                    surface_id = id.0,
                    bytes = protocol_event_retained_bytes(&removed),
                    "dropped stale renderer frame to preserve protocol liveness"
                );
            }
        }

        match event {
            ProtocolEvent::SecurityScene { .. } => {
                self.security = Some(event);
            }
            ProtocolEvent::OutputResized { .. } => {
                self.output = Some(event);
            }
            ProtocolEvent::CursorUpdated { .. } => {
                if let Some(previous) = self.cursor.replace(event)
                    && let Some(token) = protocol_event_dmabuf_token(&previous)
                {
                    result.retired_tokens.push(token);
                }
            }
            ProtocolEvent::SurfaceRelayout { id, scene } => {
                if let Some(ProtocolEvent::SurfaceUpserted {
                    scene: queued_scene,
                    ..
                }) = self.surfaces.get_mut(&id)
                {
                    *queued_scene = scene;
                } else if !matches!(
                    self.surfaces.get(&id),
                    Some(
                        ProtocolEvent::SurfaceUnmapped { .. }
                            | ProtocolEvent::SurfaceDestroyed { .. }
                    )
                ) {
                    self.surfaces
                        .insert(id, ProtocolEvent::SurfaceRelayout { id, scene });
                }
            }
            ProtocolEvent::SurfaceUpserted { id, .. }
            | ProtocolEvent::SurfaceUnmapped { id }
            | ProtocolEvent::SurfaceDestroyed { id } => {
                if let Some(previous) = self.surfaces.insert(id, event)
                    && let Some(token) = protocol_event_dmabuf_token(&previous)
                {
                    result.retired_tokens.push(token);
                }
            }
            // Unreached while the fast path above stands, and deliberately the
            // same store it performs: a roster that lost its fast path must
            // still be kept, not dropped on the floor.
            ProtocolEvent::SurfaceRoster { .. } => {
                self.roster = Some(event);
            }
            ProtocolEvent::DmabufBufferDestroyed { buffer_id } => {
                self.push_dmabuf_invalidation(buffer_id);
            }
            ProtocolEvent::DmabufCacheInvalidated => {
                self.invalidate_all_dmabufs = true;
                self.dmabuf_invalidations.clear();
            }
            ProtocolEvent::CaptureKmsSourcesRetired { current } => {
                self.current_kms_capture_sources = Some(current);
            }
            ProtocolEvent::CaptureRequested(_) | ProtocolEvent::CaptureDamageWatch(_) => {
                self.captures.push(event);
            }
            ProtocolEvent::RuntimeFailed(_) => {
                if self.runtime_failures.is_empty() {
                    self.runtime_failures.push(event);
                }
            }
        }
        self.bytes = prospective;
        Ok(result)
    }

    fn take(&mut self) -> Vec<ProtocolEvent> {
        self.bytes = 0;
        self.pressure_warned = false;
        let mut events = Vec::with_capacity(
            usize::from(self.security.is_some())
                + usize::from(self.roster.is_some())
                + usize::from(self.output.is_some())
                + usize::from(self.cursor.is_some())
                + self.surfaces.len()
                + self.dmabuf_invalidations.len()
                + usize::from(self.invalidate_all_dmabufs)
                + usize::from(self.current_kms_capture_sources.is_some())
                + self.captures.len()
                + self.runtime_failures.len(),
        );
        // Security state goes first so the opaque blank is installed before
        // ordinary surface membership is withdrawn in the same batch.
        if let Some(security) = self.security.take() {
            events.push(security);
        }
        // The roster goes ahead of every per-surface event in the
        // batch. Reversed, a queued upsert for a surface the roster removes
        // would recreate the entity immediately after it was dropped, which is
        // the very defect the roster exists to close.
        if let Some(roster) = self.roster.take() {
            events.push(roster);
        }
        if let Some(output) = self.output.take() {
            events.push(output);
        }
        if let Some(cursor) = self.cursor.take() {
            events.push(cursor);
        }
        events.extend(self.surfaces.drain().map(|(_, event)| event));
        events.extend(
            self.dmabuf_invalidations
                .drain()
                .map(|buffer_id| ProtocolEvent::DmabufBufferDestroyed { buffer_id }),
        );
        if mem::take(&mut self.invalidate_all_dmabufs) {
            events.push(ProtocolEvent::DmabufCacheInvalidated);
        }
        if let Some(current) = self.current_kms_capture_sources.take() {
            events.push(ProtocolEvent::CaptureKmsSourcesRetired { current });
        }
        events.append(&mut self.captures);
        events.append(&mut self.runtime_failures);
        events
    }

    fn push_dmabuf_invalidation(&mut self, buffer_id: DmabufBufferId) {
        if self.invalidate_all_dmabufs {
            return;
        }
        self.dmabuf_invalidations.insert(buffer_id);
        if self.dmabuf_invalidations.len() > MAX_PENDING_DMABUF_INVALIDATIONS {
            self.dmabuf_invalidations.clear();
            self.invalidate_all_dmabufs = true;
        }
    }
}

fn bounded_replacement_bytes(
    current: usize,
    replaced: usize,
    replacement: usize,
    limit: usize,
) -> Option<usize> {
    let prospective = current.saturating_sub(replaced).saturating_add(replacement);
    (prospective <= limit).then_some(prospective)
}

fn protocol_event_surface_id(event: &ProtocolEvent) -> Option<SurfaceId> {
    match event {
        ProtocolEvent::SurfaceUpserted { id, .. }
        | ProtocolEvent::SurfaceRelayout { id, .. }
        | ProtocolEvent::SurfaceUnmapped { id }
        | ProtocolEvent::SurfaceDestroyed { id } => Some(*id),
        ProtocolEvent::SecurityScene { .. }
        | ProtocolEvent::OutputResized { .. }
        | ProtocolEvent::CursorUpdated { .. }
        | ProtocolEvent::DmabufBufferDestroyed { .. }
        | ProtocolEvent::DmabufCacheInvalidated
        | ProtocolEvent::CaptureKmsSourcesRetired { .. }
        | ProtocolEvent::CaptureRequested(_)
        | ProtocolEvent::CaptureDamageWatch(_)
        | ProtocolEvent::SurfaceRoster { .. }
        | ProtocolEvent::RuntimeFailed(_) => None,
    }
}

fn protocol_event_dmabuf_token(event: &ProtocolEvent) -> Option<u64> {
    match event {
        ProtocolEvent::SurfaceUpserted {
            frame: SurfaceFrame::Dmabuf(frame),
            ..
        } => Some(frame.token),
        ProtocolEvent::CursorUpdated {
            image:
                CursorImage::Surface {
                    frame: Some(SurfaceFrame::Dmabuf(frame)),
                    ..
                },
        } => Some(frame.token),
        _ => None,
    }
}

/// Whether the renderer should be holding an entity for this surface.
///
/// The one predicate behind three decisions that must never disagree: which ids
/// a roster lists, whether dirty recovery can produce an upsert, and whether a
/// commit may publish one. An id one of them admits and another refuses either
/// survives a roster it should not, or is removed and immediately restored.
fn surface_is_presentable(record: &SurfaceRecord) -> bool {
    record.mapped && !matches!(record.role, SurfaceRole::Dormant(_))
}

/// A committed buffer may map the surface unless it belongs to an X11 window
/// that is not yet association+map eligible: the backing is retained for the
/// map grant to republish, but nothing is presented early.
fn commit_may_map_surface(record: &SurfaceRecord) -> bool {
    #[cfg(feature = "xwayland")]
    if let SurfaceRole::X11(role) = &record.role {
        return role.phase.eligible();
    }
    #[cfg(not(feature = "xwayland"))]
    let _ = record;
    true
}

fn protocol_event_retained_bytes(event: &ProtocolEvent) -> usize {
    match event {
        ProtocolEvent::SurfaceUpserted {
            scene,
            frame: SurfaceFrame::Shm(frame),
            ..
        } => frame
            .rgba
            .len()
            .saturating_add(scene.title.as_deref().map_or(0, str::len)),
        ProtocolEvent::CursorUpdated {
            image:
                CursorImage::Surface {
                    frame: Some(SurfaceFrame::Shm(frame)),
                    ..
                },
        } => frame.rgba.len(),
        ProtocolEvent::SurfaceUpserted { scene, .. }
        | ProtocolEvent::SurfaceRelayout { scene, .. } => mem::size_of::<ProtocolEvent>()
            .saturating_add(scene.title.as_deref().map_or(0, str::len)),
        ProtocolEvent::SecurityScene { .. }
        | ProtocolEvent::OutputResized { .. }
        | ProtocolEvent::CursorUpdated { .. }
        | ProtocolEvent::SurfaceUnmapped { .. }
        | ProtocolEvent::SurfaceDestroyed { .. }
        | ProtocolEvent::DmabufBufferDestroyed { .. }
        | ProtocolEvent::DmabufCacheInvalidated
        | ProtocolEvent::CaptureKmsSourcesRetired { .. }
        | ProtocolEvent::CaptureRequested(_)
        | ProtocolEvent::CaptureDamageWatch(_) => mem::size_of::<ProtocolEvent>(),
        // Reported honestly for the record, but never charged: the roster's
        // worst case is reserved out of the budget once, and it is admitted
        // ahead of the accounting entirely.
        ProtocolEvent::SurfaceRoster { mapped } => mem::size_of::<ProtocolEvent>()
            .saturating_add(mapped.len().saturating_mul(mem::size_of::<SurfaceId>())),
        ProtocolEvent::RuntimeFailed(message) => {
            mem::size_of::<ProtocolEvent>().saturating_add(message.len())
        }
    }
}

fn configure_sequence_is_acked(required: Option<Serial>, acknowledged: Option<Serial>) -> bool {
    required
        .zip(acknowledged)
        .is_some_and(|(required, acknowledged)| acknowledged >= required)
}

fn effectively_visible(
    own_buffer: bool,
    ancestor_visible: bool,
    association_committed: bool,
) -> bool {
    own_buffer && ancestor_visible && association_committed
}

fn scene_decoration_mode(mode: DecorationMode) -> SceneDecorationMode {
    match mode {
        DecorationMode::ClientSide => SceneDecorationMode::ClientSide,
        DecorationMode::ServerSide => SceneDecorationMode::ServerSide,
        _ => SceneDecorationMode::Unbound,
    }
}

fn sync_toplevel_scene_state(record: &mut SurfaceRecord) {
    record.layout.toplevel = match (&record.role, record.committed_window_geometry) {
        // Both managed-toplevel roles publish the same scene state; the
        // renderer selects SSD chrome purely on `decoration == ServerSide`
        // and never learns which protocol the window arrived through.
        (role, Some(window_geometry)) if role.managed_toplevel() => Some(ToplevelSceneState {
            decoration: record.committed_decoration,
            focused: record.focused,
            committed_maximized: record.committed_maximized,
            window_geometry,
            chrome_pointer: record.chrome_pointer,
        }),
        _ => None,
    };
}

struct SurfaceRecord {
    id: SurfaceId,
    role: SurfaceRole,
    mapped: bool,
    layout: SurfaceLayout,
    title: Option<Arc<str>>,
    app_id: Option<Arc<str>>,
    /// Global origin of the xdg_surface window geometry. `layout` starts at
    /// the wl_surface/buffer origin, which can precede it for CSD shadows.
    window_origin: (f32, f32),
    configured_size: (i32, i32),
    commit_count: u64,
    shm_backing: Option<ShmBacking>,
    dmabuf_backing: Option<DmabufBacking>,
    buffer_dimensions: Option<(u32, u32)>,
    /// Serial that must be acknowledged before the current initial/remap
    /// sequence may attach content.
    required_configure: Option<Serial>,
    last_acked_configure: Option<Serial>,
    last_acked_size: Option<(i32, i32)>,
    decoration_object_bound: bool,
    committed_decoration: SceneDecorationMode,
    requested_maximized: bool,
    committed_maximized: bool,
    normal_restore: Option<NormalRestore>,
    pending_window_state: Option<WindowStateSnapshot>,
    configured_window_states: Vec<ConfigureWindowStateSnapshot>,
    minimized: bool,
    focused: bool,
    chrome_pointer: ChromePointerSceneState,
    committed_window_geometry: Option<SceneWindowGeometry>,
    committed_window_geometry_explicit: bool,
    pending_popup_reposition: Option<PendingPopupReposition>,
    /// Adding a subsurface is double-buffered on its parent.
    parent_association_committed: bool,
    committed_input_region: Option<CommittedInputRegion>,
    pixel_probe_logged: bool,
    logged_diagnostics: HashSet<SurfaceDiagnostic>,
}

impl SurfaceRecord {
    fn scene_snapshot(&self) -> SurfaceSceneSnapshot {
        SurfaceSceneSnapshot {
            layout: self.layout,
            kind: self.role.scene_kind(),
            title: self.title.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SurfaceDiagnostic {
    DmabufCommitted,
    ShmCommitted,
    InvalidSize,
    InvalidViewport,
    DmabufDescription,
    BufferImport,
    InputRegionBoundingBox,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CommittedInputRegion {
    Empty,
    Operations(Vec<CommittedInputRegionOperation>),
    BoundingBox(CommittedInputRect),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommittedInputRegionOperation {
    add: bool,
    rect: CommittedInputRect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommittedInputRect {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl CommittedInputRect {
    fn from_smithay(rect: Rectangle<i32, Logical>) -> Self {
        let left = i64::from(rect.loc.x);
        let top = i64::from(rect.loc.y);
        Self {
            left,
            top,
            right: left + i64::from(rect.size.w),
            bottom: top + i64::from(rect.size.h),
        }
    }

    fn contains(self, point: (i32, i32)) -> bool {
        let (x, y) = (i64::from(point.0), i64::from(point.1));
        x >= self.left && y >= self.top && x < self.right && y < self.bottom
    }

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

impl CommittedInputRegion {
    fn from_surface_attributes(attributes: &SurfaceAttributes) -> (Option<Self>, Option<usize>) {
        let Some(region) = attributes.input_region.as_ref() else {
            return (None, None);
        };
        if region.rects.is_empty() {
            return (Some(Self::Empty), None);
        }
        if region.rects.len() <= MAX_COMMITTED_INPUT_REGION_RECTS {
            let operations = region
                .rects
                .iter()
                .map(|(kind, rect)| CommittedInputRegionOperation {
                    add: matches!(kind, RectangleKind::Add),
                    rect: CommittedInputRect::from_smithay(*rect),
                })
                .collect();
            return (Some(Self::Operations(operations)), None);
        }
        let bounds = region
            .rects
            .iter()
            .filter(|(kind, _)| matches!(kind, RectangleKind::Add))
            .map(|(_, rect)| CommittedInputRect::from_smithay(*rect))
            .reduce(CommittedInputRect::union);
        (
            Some(bounds.map_or(Self::Empty, Self::BoundingBox)),
            Some(region.rects.len()),
        )
    }

    fn contains(&self, point: (i32, i32)) -> bool {
        match self {
            Self::Empty => false,
            Self::BoundingBox(rect) => rect.contains(point),
            Self::Operations(operations) => operations.iter().fold(false, |contains, operation| {
                if operation.rect.contains(point) {
                    operation.add
                } else {
                    contains
                }
            }),
        }
    }
}

#[derive(Debug)]
struct ShmBacking {
    width: u32,
    height: u32,
    format: wl_shm::Format,
    rgba: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct DmabufBacking {
    buffer: wl_buffer::WlBuffer,
    buffer_id: DmabufBufferId,
    descriptor: DmabufDescriptor,
    retention_token: u64,
    use_id: Option<DmabufUseId>,
}

enum LatestSurfaceUpsert {
    Ready(Box<ProtocolEvent>),
    Gone,
    Retry,
}

#[derive(Clone, Default)]
struct SubsurfaceTopology {
    parent: Option<ObjectId>,
    children: HashSet<ObjectId>,
    depth: usize,
    subtree_height: usize,
}

struct ResolvedPopupGeometry {
    geometry: Rectangle<i32, Logical>,
    layout: SurfaceLayout,
    window_origin: (f32, f32),
}

struct PendingPopupReposition {
    serial: Serial,
    layout: SurfaceLayout,
    window_origin: (f32, f32),
    configured_size: (i32, i32),
}

enum SurfaceRole {
    Toplevel(ToplevelSurface),
    Popup(PopupSurface),
    Layer(LayerRole),
    LockSurface(LockSurfaceRole),
    Subsurface {
        surface: WlSurface,
        parent: WlSurface,
    },
    Dormant(WlSurface),
    /// A normal managed X11 toplevel, created at XWayland association time.
    /// Override-redirect windows never get this role (X-1 refusal). Boxed:
    /// `X11Surface` carries its whole atom table inline, and every
    /// `SurfaceRecord` would otherwise pay for it.
    #[cfg(feature = "xwayland")]
    X11(Box<X11ToplevelRole>),
}

#[cfg(feature = "xwayland")]
struct X11ToplevelRole {
    /// The associated Wayland surface XWayland commits buffers through.
    wl_surface: WlSurface,
    surface: smithay::xwayland::X11Surface,
    xid: smithay::xwayland::xwm::X11Window,
    /// XWayland generation this window belongs to; teardown is
    /// generation-guarded.
    #[allow(dead_code)]
    generation: u64,
    /// Association/map eligibility state machine (the presentation gate).
    phase: xwayland::X11WindowPhase,
    /// Last granted global content rectangle (X geometry authority; the
    /// committed buffer remains the presentation authority).
    granted_geometry: Rectangle<i32, Logical>,
    fullscreen: bool,
}

struct LockSurfaceRole {
    surface: LockSurface,
    output: Output,
    lock_generation: u64,
}

struct LayerRole {
    surface: DesktopLayerSurface,
    output: LayerOutputBinding,
    initial_layer: WlrLayer,
    committed_layer: WlrLayer,
    committed_keyboard_interactivity: KeyboardInteractivity,
}

enum LayerOutputBinding {
    Default(Output),
    Explicit(Output),
    Closed,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
enum LayerOutputTransition {
    Keep,
    Migrate(Output),
    Close,
}

impl LayerOutputBinding {
    fn output(&self) -> Option<&Output> {
        match self {
            Self::Default(output) | Self::Explicit(output) => Some(output),
            Self::Closed => None,
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn transition(
        &self,
        default_output: Option<&Output>,
        output_is_registered: impl FnOnce(&Output) -> bool,
    ) -> LayerOutputTransition {
        let Some(output) = self.output() else {
            return LayerOutputTransition::Keep;
        };
        if output_is_registered(output) {
            return LayerOutputTransition::Keep;
        }
        match self {
            Self::Explicit(_) => LayerOutputTransition::Close,
            Self::Default(_) => default_output
                .cloned()
                .map_or(LayerOutputTransition::Close, LayerOutputTransition::Migrate),
            Self::Closed => LayerOutputTransition::Keep,
        }
    }
}

enum ConfigureTarget {
    Toplevel(ToplevelSurface),
    Popup(PopupSurface),
    Layer(DesktopLayerSurface),
    Lock(LockSurface),
}

enum LockLifecycle {
    Unlocked,
    Locking {
        owner: ClientId,
        lock_resource: ExtSessionLockV1,
        locker: SessionLocker,
        generation: u64,
        presentation_epoch: u64,
        pending_outputs: HashSet<String>,
        pending_kms_outputs: BTreeMap<crate::backend::kms::OutputKey, u64>,
    },
    Locked {
        owner: ClientId,
        lock_resource: ExtSessionLockV1,
        generation: u64,
    },
    OrphanedLocked {
        generation: u64,
    },
}

impl LockLifecycle {
    fn is_active(&self) -> bool {
        !matches!(self, Self::Unlocked)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
enum KmsSecurityBarrierPurpose {
    LockResume,
    UnlockRestore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KmsSecurityBarrier {
    purpose: KmsSecurityBarrierPurpose,
    presentation_epoch: u64,
    pending_outputs: BTreeMap<crate::backend::kms::OutputKey, u64>,
}

impl KmsSecurityBarrier {
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn acknowledge(
        &mut self,
        presentation_epoch: u64,
        generation: u64,
        output: &crate::backend::kms::OutputKey,
    ) -> bool {
        if self.presentation_epoch != presentation_epoch
            || self.pending_outputs.get(output) != Some(&generation)
        {
            return false;
        }
        self.pending_outputs.remove(output);
        self.pending_outputs.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
enum KmsPresentationPhase {
    Unavailable,
    Preparing {
        outputs: BTreeMap<crate::backend::kms::OutputKey, u64>,
        ready: BTreeSet<crate::backend::kms::OutputKey>,
    },
    Ready {
        outputs: BTreeMap<crate::backend::kms::OutputKey, u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KmsSessionLockGate {
    phase: KmsPresentationPhase,
    resume_barrier: Option<KmsSecurityBarrier>,
    deferred_unlock: bool,
    unlock_barrier: Option<KmsSecurityBarrier>,
    input_hold_logged: bool,
    suppressed_keys: HashSet<Keycode>,
    suppressed_buttons: HashSet<u32>,
    physical_touch_slots: HashSet<TouchSlot>,
    suppressed_touch_slots: HashSet<TouchSlot>,
}

impl Default for KmsSessionLockGate {
    fn default() -> Self {
        Self {
            phase: KmsPresentationPhase::Unavailable,
            resume_barrier: None,
            deferred_unlock: false,
            unlock_barrier: None,
            input_hold_logged: false,
            suppressed_keys: HashSet::new(),
            suppressed_buttons: HashSet::new(),
            physical_touch_slots: HashSet::new(),
            suppressed_touch_slots: HashSet::new(),
        }
    }
}

#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
impl KmsSessionLockGate {
    fn authority_lost(&mut self) {
        if self.unlock_barrier.take().is_some() {
            self.deferred_unlock = true;
        }
        self.phase = KmsPresentationPhase::Unavailable;
        self.resume_barrier = None;
        self.input_hold_logged = false;
        // Touch-device reconciliation reaches the seat through synthetic
        // device removal with `user_activity=false`, so it deliberately
        // bypasses `observe_physical_touch`. Authority loss is the physical
        // boundary: neither the source map nor its quarantine may survive it.
        self.physical_touch_slots.clear();
        self.suppressed_touch_slots.clear();
    }

    fn begin_preparing(&mut self, outputs: BTreeMap<crate::backend::kms::OutputKey, u64>) {
        self.phase = KmsPresentationPhase::Preparing {
            outputs,
            ready: BTreeSet::new(),
        };
        self.input_hold_logged = false;
    }

    fn output_ready(&mut self, generation: u64, output: &crate::backend::kms::OutputKey) -> bool {
        let KmsPresentationPhase::Preparing { outputs, ready } = &mut self.phase else {
            return false;
        };
        if outputs.get(output) != Some(&generation) {
            return false;
        }
        ready.insert(output.clone());
        if ready.len() != outputs.len() || outputs.is_empty() {
            return false;
        }
        let outputs = outputs.clone();
        self.phase = KmsPresentationPhase::Ready { outputs };
        true
    }

    fn ready_outputs(&self) -> Option<&BTreeMap<crate::backend::kms::OutputKey, u64>> {
        match &self.phase {
            KmsPresentationPhase::Ready { outputs } if !outputs.is_empty() => Some(outputs),
            KmsPresentationPhase::Unavailable
            | KmsPresentationPhase::Preparing { .. }
            | KmsPresentationPhase::Ready { .. } => None,
        }
    }

    fn client_delivery_blocked(&self, lock_active: bool, locking: bool) -> bool {
        (lock_active
            && (!matches!(self.phase, KmsPresentationPhase::Ready { .. })
                || locking
                || self.resume_barrier.is_some()))
            || self.deferred_unlock
            || self.unlock_barrier.is_some()
    }

    fn normal_scene_restricted(&self) -> bool {
        self.deferred_unlock || self.unlock_barrier.is_some()
    }

    fn observe_physical_touch(&mut self, input: &HostInput) {
        match input {
            HostInput::TouchDown { slot, .. } => {
                self.physical_touch_slots.insert(*slot);
            }
            HostInput::TouchUp { slot, .. } => {
                self.physical_touch_slots.remove(slot);
            }
            HostInput::TouchCancel => self.physical_touch_slots.clear(),
            _ => {}
        }
    }

    fn quarantine_current_input(
        &mut self,
        keys: impl IntoIterator<Item = Keycode>,
        buttons: impl IntoIterator<Item = u32>,
    ) {
        self.suppressed_keys.extend(keys);
        self.suppressed_buttons.extend(buttons);
        self.suppressed_touch_slots
            .extend(self.physical_touch_slots.iter().copied());
    }

    fn observe_blocked_input(&mut self, input: &HostInput) {
        match input {
            HostInput::Key {
                keycode,
                state: HostButtonState::Pressed,
                ..
            } => {
                self.suppressed_keys.insert(*keycode);
            }
            HostInput::Key {
                keycode,
                state: HostButtonState::Released,
                ..
            } => {
                self.suppressed_keys.remove(keycode);
            }
            HostInput::PointerButton {
                button,
                state: HostButtonState::Pressed,
                ..
            } => {
                self.suppressed_buttons.insert(*button);
            }
            HostInput::PointerButton {
                button,
                state: HostButtonState::Released,
                ..
            } => {
                self.suppressed_buttons.remove(button);
            }
            HostInput::TouchDown { slot, .. } => {
                self.suppressed_touch_slots.insert(*slot);
            }
            HostInput::TouchUp { slot, .. } => {
                self.suppressed_touch_slots.remove(slot);
            }
            HostInput::TouchCancel => self.suppressed_touch_slots.clear(),
            _ => {}
        }
    }

    fn suppress_quarantined_input(&mut self, input: &HostInput) -> bool {
        match input {
            HostInput::Key {
                keycode,
                state: HostButtonState::Released,
                ..
            } if self.suppressed_keys.remove(keycode) => true,
            HostInput::Key { keycode, .. } if self.suppressed_keys.contains(keycode) => true,
            HostInput::PointerButton {
                button,
                state: HostButtonState::Released,
                ..
            } if self.suppressed_buttons.remove(button) => true,
            HostInput::PointerButton { button, .. } if self.suppressed_buttons.contains(button) => {
                true
            }
            HostInput::TouchDown { slot, .. } if !self.suppressed_touch_slots.is_empty() => {
                self.suppressed_touch_slots.insert(*slot);
                true
            }
            HostInput::TouchMotion { .. } | HostInput::TouchFrame
                if !self.suppressed_touch_slots.is_empty() =>
            {
                true
            }
            HostInput::TouchUp { slot, .. }
                if self.suppressed_touch_slots.remove(slot)
                    || !self.suppressed_touch_slots.is_empty() =>
            {
                true
            }
            HostInput::TouchCancel if !self.suppressed_touch_slots.is_empty() => {
                self.suppressed_touch_slots.clear();
                true
            }
            _ => false,
        }
    }
}

enum XdgConfigureRequest {
    Initial,
    Toplevel { force: bool },
    PopupReposition { token: u32 },
    Lock,
}

impl SurfaceRole {
    fn scene_kind(&self) -> SceneSurfaceKind {
        match self {
            Self::Toplevel(_) => SceneSurfaceKind::Toplevel,
            #[cfg(feature = "xwayland")]
            Self::X11(_) => SceneSurfaceKind::Toplevel,
            Self::Subsurface { .. } => SceneSurfaceKind::Subsurface,
            Self::Popup(_) => SceneSurfaceKind::Popup,
            Self::Layer(_) => SceneSurfaceKind::Subsurface,
            Self::LockSurface(_) => SceneSurfaceKind::Subsurface,
            // Dormant records are excluded by `surface_is_presentable`, so
            // this value can never cross the protocol-to-scene boundary.
            Self::Dormant(_) => SceneSurfaceKind::Subsurface,
        }
    }

    fn wl_surface(&self) -> &WlSurface {
        match self {
            Self::Toplevel(surface) => surface.wl_surface(),
            Self::Popup(surface) => surface.wl_surface(),
            Self::Layer(role) => role.surface.wl_surface(),
            Self::LockSurface(role) => role.surface.wl_surface(),
            Self::Subsurface { surface, .. } => surface,
            Self::Dormant(surface) => surface,
            #[cfg(feature = "xwayland")]
            Self::X11(role) => &role.wl_surface,
        }
    }

    fn toplevel(&self) -> Option<&ToplevelSurface> {
        match self {
            Self::Toplevel(surface) => Some(surface),
            Self::Popup(_)
            | Self::Layer(_)
            | Self::LockSurface(_)
            | Self::Subsurface { .. }
            | Self::Dormant(_) => None,
            // Deliberately None: this accessor answers "which xdg toplevel",
            // and X11 windows have none. Managed-toplevel policy uses
            // `managed_toplevel()` instead.
            #[cfg(feature = "xwayland")]
            Self::X11(_) => None,
        }
    }

    /// Whether this role is a managed toplevel window — xdg or X11 — for
    /// chrome, focus, stacking and foreign-toplevel policy. Every policy
    /// site that used to test `SurfaceRole::Toplevel` alone must use this
    /// unless it genuinely needs the xdg configure machinery.
    fn managed_toplevel(&self) -> bool {
        match self {
            Self::Toplevel(_) => true,
            #[cfg(feature = "xwayland")]
            Self::X11(_) => true,
            _ => false,
        }
    }

    #[cfg(feature = "xwayland")]
    fn x11(&self) -> Option<&X11ToplevelRole> {
        match self {
            Self::X11(role) => Some(role),
            _ => None,
        }
    }

    fn parent_surface(&self) -> Option<&WlSurface> {
        match self {
            Self::Subsurface { parent, .. } => Some(parent),
            Self::Toplevel(_)
            | Self::Popup(_)
            | Self::Layer(_)
            | Self::LockSurface(_)
            | Self::Dormant(_) => None,
            #[cfg(feature = "xwayland")]
            Self::X11(_) => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Toplevel(_) => "toplevel",
            Self::Popup(_) => "popup",
            Self::Layer(_) => "layer",
            Self::LockSurface(_) => "lock",
            Self::Subsurface { .. } => "subsurface",
            Self::Dormant(_) => "dormant",
            #[cfg(feature = "xwayland")]
            Self::X11(_) => "x11-toplevel",
        }
    }
}

fn set_xdg_configured(surface: &WlSurface, configured: bool) {
    compositor::with_states(surface, |states| {
        if let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() {
            let mut data = data.lock().expect("xdg toplevel state lock");
            data.configured = configured;
            if !configured {
                data.configure_serial = None;
                data.current_serial = None;
            }
        } else if let Some(data) = states.data_map.get::<XdgPopupSurfaceData>() {
            let mut data = data.lock().expect("xdg popup state lock");
            data.configured = configured;
            if !configured {
                data.configure_serial = None;
                data.current_serial = None;
            }
        }
    });
}

fn child_surface_ids(
    surfaces: &HashMap<ObjectId, SurfaceRecord>,
) -> HashMap<SurfaceId, Vec<SurfaceId>> {
    let mut children: HashMap<SurfaceId, Vec<SurfaceId>> = HashMap::new();
    for record in surfaces.values() {
        if let Some(parent) = record.layout.parent {
            children.entry(parent).or_default().push(record.id);
        }
    }
    children
}

fn record_root_id(
    surfaces: &HashMap<ObjectId, SurfaceRecord>,
    surface_objects: &HashMap<SurfaceId, ObjectId>,
    surface: ObjectId,
) -> Option<SurfaceId> {
    let mut current = surfaces.get(&surface)?.id;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        let parent = surface_objects
            .get(&current)
            .and_then(|object| surfaces.get(object))
            .and_then(|record| record.layout.parent);
        match parent {
            Some(parent) => current = parent,
            None => return Some(current),
        }
    }
    None
}

#[derive(Clone)]
enum InteractivePointer {
    Move {
        surface: WlSurface,
        start_pointer: (f64, f64),
        start_origin: (f32, f32),
    },
    Resize {
        surface: WlSurface,
        edges: xdg_toplevel::ResizeEdge,
        start_pointer: (f64, f64),
        start_origin: (f32, f32),
        start_size: (i32, i32),
    },
}

#[derive(Clone)]
enum PointerTarget {
    Client {
        surface: WlSurface,
        origin: Point<f64, Logical>,
    },
    Chrome {
        object: ObjectId,
        part: ChromePart,
        button_cluster_hovered: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChromePointerGrabKind {
    Move,
    Resize(DecoResizeEdge),
    Button(CaptionButton),
}

#[derive(Clone)]
struct ChromePointerGrab {
    surface: WlSurface,
    button: u32,
    kind: ChromePointerGrabKind,
    start_pointer: (f64, f64),
    dragged: bool,
}

#[derive(Clone)]
struct TitlebarClickCandidate {
    surface: WlSurface,
    position: (f64, f64),
    time: u32,
}

struct RetainedBuffer<V> {
    value: V,
    count: usize,
}

struct RetentionTable<K, V> {
    tokens: HashMap<u64, K>,
    buffers: HashMap<K, RetainedBuffer<V>>,
}

impl<K, V> Default for RetentionTable<K, V> {
    fn default() -> Self {
        Self {
            tokens: HashMap::new(),
            buffers: HashMap::new(),
        }
    }
}

impl<K, V> RetentionTable<K, V>
where
    K: Clone + Eq + std::hash::Hash,
{
    fn retain(&mut self, token: u64, key: K, value: V) {
        self.tokens.insert(token, key.clone());
        match self.buffers.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().count = entry.get().count.saturating_add(1);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RetainedBuffer { value, count: 1 });
            }
        }
    }

    fn release(&mut self, token: u64) -> Option<V> {
        let key = self.tokens.remove(&token)?;
        let entry = self.buffers.get_mut(&key)?;
        entry.count = entry.count.saturating_sub(1);
        if entry.count == 0 {
            self.buffers.remove(&key).map(|entry| entry.value)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CursorSelection {
    Default,
    Hidden,
    Surface(ObjectId),
}

fn chrome_resize_cursor(edge: DecoResizeEdge) -> ChromeCursorIcon {
    match edge {
        DecoResizeEdge::Top => ChromeCursorIcon::NResize,
        DecoResizeEdge::TopRight => ChromeCursorIcon::NeResize,
        DecoResizeEdge::Right => ChromeCursorIcon::EResize,
        DecoResizeEdge::BottomRight => ChromeCursorIcon::SeResize,
        DecoResizeEdge::Bottom => ChromeCursorIcon::SResize,
        DecoResizeEdge::BottomLeft => ChromeCursorIcon::SwResize,
        DecoResizeEdge::Left => ChromeCursorIcon::WResize,
        DecoResizeEdge::TopLeft => ChromeCursorIcon::NwResize,
    }
}

#[derive(Debug)]
struct CursorSurfaceRecord {
    surface: WlSurface,
    hotspot: (i32, i32),
    shm_backing: Option<ShmBacking>,
    dmabuf_backing: Option<DmabufBacking>,
    buffer_dimensions: Option<(u32, u32)>,
    presentation: Option<CursorPresentation>,
}

struct CursorCommit<'a> {
    buffer: Option<&'a BufferAssignment>,
    damage: &'a [Damage],
    force_full_damage: bool,
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
    buffer_delta: Option<(i32, i32)>,
}

struct SurfaceBufferCommit {
    damage: Vec<Damage>,
    force_full_damage: bool,
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
    window_geometry_changed: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreencopyManagerData {
    id: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreencopyFrameData {
    id: CaptureId,
}

#[derive(Clone, Debug)]
struct CaptureManagerRecord {
    client_id: ClientId,
    live_frames: usize,
    resource_alive: bool,
    damage_baselines: HashMap<CaptureSourceId, u64>,
}

enum CaptureFrameDestination {
    Shm(wl_buffer::WlBuffer),
    Dmabuf {
        buffer: wl_buffer::WlBuffer,
        descriptor: Arc<DmabufDescriptor>,
        retention_token: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureDmabufMetadataMatch {
    Matches,
    InvalidBuffer,
    UnsupportedModifier,
}

impl CaptureFrameDestination {
    fn buffer(&self) -> &wl_buffer::WlBuffer {
        match self {
            Self::Shm(buffer) | Self::Dmabuf { buffer, .. } => buffer,
        }
    }

    fn take_retention_token(&mut self) -> Option<u64> {
        match self {
            Self::Shm(_) => None,
            Self::Dmabuf {
                retention_token, ..
            } => retention_token.take(),
        }
    }
}

struct CaptureFrameRecord {
    resource: ZwlrScreencopyFrameV1,
    client_id: ClientId,
    manager_id: u64,
    source_id: CaptureSourceId,
    output_name: String,
    generation: u64,
    security_epoch: u64,
    region: CaptureRegion,
    logical_rect: (i32, i32, u32, u32),
    source_storage_extent: (u32, u32),
    displayed_physical_extent: (u32, u32),
    scale120: u32,
    transform: smithay::utils::Transform,
    dmabuf_advertisement: Option<crate::backend::CaptureDmabufAdvertisement>,
    format: CaptureFormat,
    stride: u32,
    overlay_cursor: bool,
    submitted: bool,
    with_damage: bool,
    terminal: bool,
    resource_alive: bool,
    job_pending: bool,
    destination: Option<CaptureFrameDestination>,
    pixels: Option<CapturePixels>,
    dmabuf_completion: Option<CaptureDmabufComplete>,
    presentation: Option<CapturePresented>,
    next_write_row: u32,
    write_scheduled: bool,
    reserved_bytes: usize,
    deadline: Option<Instant>,
    cancellation: Option<CaptureCancellation>,
    damage_baseline: Option<u64>,
    damage_revision: u64,
    damage: Vec<CaptureRegion>,
}

impl CaptureFrameRecord {
    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.submitted && !self.terminal
    }
}

struct CaptureReservationRecord {
    client_id: ClientId,
    bytes: usize,
}

struct WaylandState {
    acquire_gates: AcquireGateEngine<LinuxAcquireGatePlatform>,
    release_uses: ReleaseUseEngine<LinuxReleaseUsePlatform>,
    client_disconnect_sender: channel::Sender<ClientId>,
    display_handle: DisplayHandle,
    compositor_state: CompositorState,
    #[allow(dead_code)]
    output_manager_state: OutputManagerState,
    xdg_shell_state: XdgShellState,
    layer_shell_state: WlrLayerShellState,
    session_lock_state: SessionLockManagerState,
    lock_lifecycle: LockLifecycle,
    lock_surfaces_by_output: HashMap<String, ObjectId>,
    kms_session_lock_gate: KmsSessionLockGate,
    #[cfg(test)]
    session_unlock_callbacks: usize,
    next_lock_generation: u64,
    next_security_presentation_epoch: u64,
    next_capture_manager_id: u64,
    next_capture_id: u64,
    capture_managers: HashMap<u64, CaptureManagerRecord>,
    capture_frames: HashMap<CaptureId, CaptureFrameRecord>,
    capture_frames_by_resource: HashMap<ObjectId, CaptureId>,
    capture_reservations: HashMap<CaptureId, CaptureReservationRecord>,
    capture_release_sender: channel::Sender<CaptureId>,
    capture_loop_handle: LoopHandle<'static, WaylandState>,
    saved_cursor_selection: Option<CursorSelection>,
    idle_notifier_state: IdleNotifierState<Self>,
    foreign_toplevel_list_state: ForeignToplevelListState,
    #[allow(dead_code)]
    xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)]
    fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    viewporter_state: ViewporterState,
    #[cfg(feature = "xwayland")]
    xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    #[cfg(feature = "xwayland")]
    xwayland: xwayland::XwaylandRuntime,
    shm_state: ShmState,
    dmabuf_state: DmabufState,
    drm_syncobj_state: Option<ExplicitSyncGlobal>,
    #[allow(dead_code)]
    dmabuf_global: DmabufGlobal,
    supported_dmabuf_formats: Vec<Format>,
    capture_advertisements: crate::capture::CaptureAdvertisementRegistry,
    /// The protocol thread's end of the DMA-BUF validation queue. `None` when
    /// the compositor runs without a probe, in which case metadata validation is
    /// the whole of the check.
    dmabuf_validation: Option<SyncSender<DmabufValidationRequest>>,
    data_device_state: DataDeviceState,
    seat_state: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: KeyboardHandle<Self>,
    input_ingress: input::InputIngressState,
    /// How many attached devices report a touch capability.
    ///
    /// The seat's touch capability is derived from this rather than cached
    /// beside it: `Seat::get_touch()` is the one authority on whether touch is
    /// currently advertised, and a second copy of that answer could disagree
    /// with what clients were actually told.
    touch_devices: usize,
    bindings: BindingState,
    decoration: DecorationStartup,
    ecs_action_sender: SyncSender<EcsAction>,
    kms_render_command_sender: Sender<KmsRenderCommand>,
    vt_switch_requested: Option<Box<dyn Fn(u8) + Send>>,
    pointer: PointerHandle<Self>,
    popup_manager: PopupManager,
    backend: BackendData,
    cursor_position: (f64, f64),
    cursor_position_snapshot: Arc<Mutex<CursorPositionSnapshot>>,
    cursor_selection: CursorSelection,
    chrome_cursor_override: Option<ChromeCursorIcon>,
    cursor_surfaces: HashMap<ObjectId, CursorSurfaceRecord>,
    chrome_hover: Option<(ObjectId, Option<CaptionButton>)>,
    chrome_pressed: Option<(ObjectId, CaptionButton, u32)>,
    chrome_pointer_grab: Option<ChromePointerGrab>,
    titlebar_click_candidate: Option<TitlebarClickCandidate>,
    suppressed_chrome_buttons: HashSet<u32>,
    #[cfg(test)]
    chrome_geometry_retarget_count: usize,
    #[cfg(test)]
    committed_window_state_transitions: Vec<bool>,
    /// An applied Smithay transaction changed scene state that participates in
    /// pointer hit testing. Reconcile only from `transaction_applied`, after
    /// every surface in the transaction has made its state current.
    pointer_hit_test_transaction_applying: bool,
    pointer_hit_test_dirty: bool,
    pointer_hit_test_batch_depth: u32,
    pointer_grab_teardown_deferred: bool,
    pointer_focus_local_position: Option<(ObjectId, (f64, f64))>,
    #[cfg(test)]
    pointer_hit_test_reconciliations: usize,
    interactive_pointer: Option<InteractivePointer>,
    exclusive_keyboard_focus: Option<ObjectId>,
    minimized_toplevels: Vec<ObjectId>,
    surfaces: HashMap<ObjectId, SurfaceRecord>,
    foreign_toplevels: HashMap<SurfaceId, ForeignToplevelHandle>,
    foreign_toplevel_identifiers: HashMap<SurfaceId, String>,
    foreign_toplevel_nonce: [u8; 16],
    /// Surfaces that have ever attached a non-null buffer. Layer-shell's
    /// AlreadyConstructed rule uses this narrower history.
    buffer_history_surfaces: HashSet<ObjectId>,
    /// Every surface that has received wl_surface.attach, including NULL.
    /// ext-session-lock rejects any prior attach history.
    attach_history_surfaces: HashSet<ObjectId>,
    /// Every wl_surface that has ever committed, including an empty commit.
    /// ext-session-lock's AlreadyConstructed rule includes both commit and
    /// attach history, while layer-shell only needs the latter.
    committed_surfaces: HashSet<ObjectId>,
    surface_objects: HashMap<SurfaceId, ObjectId>,
    xdg_surface_objects: HashMap<ObjectId, ObjectId>,
    dispatching_xdg_surface: Option<ObjectId>,
    pending_parentless_popups: HashMap<ObjectId, PositionerState>,
    committed_surface_stacks: HashMap<ObjectId, Vec<ObjectId>>,
    warned_unsupported_surfaces: HashSet<ObjectId>,
    #[cfg(feature = "bus")]
    port_context: Option<Arc<port_snapshot::SnapshotContext>>,
    #[cfg(feature = "bus")]
    pending_port_requests: Vec<PortRequest>,
    #[cfg(feature = "bus")]
    pending_port_controls: Vec<PortControl>,
    #[cfg(feature = "bus")]
    observations: port_observation::ObservationState,
    events: Vec<ProtocolEvent>,
    /// Flush pokes handled in this dispatch. Their callers may proceed only
    /// after `publish_events` has offered the compacted outbox.
    event_flush_acknowledgements: Vec<SyncSender<Result<EventFlushOutcome, String>>>,
    /// Surfaces that became presentable again from content they already had,
    /// rather than from a fresh commit — so nothing on the normal path will
    /// publish an upsert for them.
    ///
    /// Deliberately not published from here as a `SurfaceUpserted`.
    /// `latest_surface_upsert` can answer `Retry` when a DMA-BUF cannot be
    /// retained right now, and this is the one producer with no later commit
    /// to try again on. `publish_events` therefore hands these to
    /// `dirty_surfaces`, whose whole job is retrying until the state is
    /// published, and which also drops the mark if the surface goes away
    /// first.
    pending_full_upserts: HashSet<SurfaceId>,
    /// The selected cursor changed without an immediately publishable renderer
    /// owner. `ProtocolServer` promotes this into `dirty_cursor`, whose retry
    /// path can retain the current DMA-BUF once pressure clears.
    pending_cursor_update: bool,
    next_surface_id: u64,
    next_layout_index: u32,
    next_stack_sequences: [u64; StackBand::COUNT],
    next_buffer_token: u64,
    next_dmabuf_buffer_id: u64,
    dmabuf_buffer_ids: HashMap<ObjectId, DmabufBufferId>,
    retained_buffers: RetentionTable<ObjectId, wl_buffer::WlBuffer>,
    budgeted_dmabuf_tokens: HashSet<u64>,
    surface_count: usize,
    subsurface_topology: HashMap<ObjectId, SubsurfaceTopology>,
    damage_requests_since_apply: HashMap<ObjectId, usize>,
    last_keyboard_action: Option<(Serial, WlSurface)>,
    shm_bytes: usize,
    diagnostic_sender: SyncSender<ShmDiagnostic>,
    shutdown_cause: Option<ProtocolShutdownCause>,
    explicit_sync_global_advertised: bool,
    #[cfg(test)]
    explicit_sync_global_withdrawals: usize,
    #[cfg(test)]
    acquire_gate_pre_commit_count: usize,
    #[cfg(test)]
    acquire_gate_client_destroyed_count: usize,
    #[cfg(test)]
    acquire_gate_surface_destroyed_count: usize,
    #[cfg(test)]
    acquire_gate_destroy_observed_surface_count: Option<usize>,
    #[cfg(test)]
    committed_release_point_override: Option<TestCommittedPoint>,
    #[cfg(test)]
    release_use_test_probe: ReleaseUseTestProbe,
    #[cfg(test)]
    release_use_client_missing_count: usize,
    #[cfg(test)]
    release_use_record_missing_count: usize,
    #[cfg(test)]
    release_use_force_client_missing: bool,
    #[cfg(test)]
    release_use_remove_record_after_prepare: bool,
    #[cfg(test)]
    effective_window_geometry_calls: usize,
}

fn capture_physical_region(
    source: &crate::backend::CaptureSourceSnapshot,
    requested: Option<(i32, i32, i32, i32)>,
) -> Option<CaptureRegion> {
    let (_, _, logical_width, logical_height) = source.logical_rect;
    let (x, y, width, height) = requested.unwrap_or((
        0,
        0,
        i32::try_from(logical_width).ok()?,
        i32::try_from(logical_height).ok()?,
    ));
    if width <= 0 || height <= 0 {
        return None;
    }
    let right = i64::from(x).checked_add(i64::from(width))?;
    let bottom = i64::from(y).checked_add(i64::from(height))?;
    let left = i64::from(x).max(0).min(i64::from(logical_width));
    let top = i64::from(y).max(0).min(i64::from(logical_height));
    let right = right.max(0).min(i64::from(logical_width));
    let bottom = bottom.max(0).min(i64::from(logical_height));
    if left >= right || top >= bottom {
        return None;
    }
    let project = |edge: i64| -> Option<u32> {
        let numerator = i128::from(edge).checked_mul(i128::from(source.scale120))?;
        let rounded = numerator.checked_add(60)?.checked_div(120)?;
        u32::try_from(rounded).ok()
    };
    let physical_left = project(left)?;
    let physical_top = project(top)?;
    let physical_right = project(right)?;
    let physical_bottom = project(bottom)?;
    let (x, y, width, height) = (
        physical_left,
        physical_top,
        physical_right.checked_sub(physical_left)?,
        physical_bottom.checked_sub(physical_top)?,
    );
    if width == 0 || height == 0 {
        return None;
    }
    let (displayed_width, displayed_height) = source.displayed_physical_extent;
    let right = x.checked_add(width)?;
    let bottom = y.checked_add(height)?;
    if right > displayed_width || bottom > displayed_height {
        return None;
    }
    Some(CaptureRegion {
        x,
        y,
        width,
        height,
    })
}

fn capture_dmabuf_is_eligible(
    source: &crate::backend::CaptureSourceSnapshot,
    logical_region: Option<(i32, i32, i32, i32)>,
    region: CaptureRegion,
) -> bool {
    logical_region.is_none()
        && region
            == (CaptureRegion {
                x: 0,
                y: 0,
                width: source.displayed_physical_extent.0,
                height: source.displayed_physical_extent.1,
            })
        && source.transform == smithay::utils::Transform::Normal
        && source.source_storage_extent == source.displayed_physical_extent
}

fn capture_reservation_bytes(output_size: (u32, u32), region: CaptureRegion) -> Option<usize> {
    let region_bytes = usize::try_from(region.width)
        .ok()?
        .checked_mul(4)?
        .checked_mul(region.height as usize)?;
    let source_row_bytes = usize::try_from(output_size.0).ok()?.checked_mul(4)?;
    let source_bytes = source_row_bytes.checked_mul(output_size.1 as usize)?;
    let padded_row_bytes = source_row_bytes.checked_add(255)? & !255;
    let staging_bytes = padded_row_bytes.checked_mul(output_size.1 as usize)?;
    source_bytes
        .checked_mul(2)?
        .checked_add(staging_bytes)?
        .checked_add(region_bytes)
}

fn capture_dmabuf_reservation_bytes(extent: (u32, u32)) -> Option<usize> {
    usize::try_from(extent.0)
        .ok()?
        .checked_mul(4)?
        .checked_mul(extent.1 as usize)
}

fn validate_capture_buffer_data(
    mapping_length: usize,
    data: smithay::wayland::shm::BufferData,
    format: CaptureFormat,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<(), ()> {
    let expected_format = match format {
        CaptureFormat::Argb8888 => wl_shm::Format::Argb8888,
        CaptureFormat::Xrgb8888 => wl_shm::Format::Xrgb8888,
    };
    if data.format != expected_format
        || data.width != i32::try_from(width).map_err(|_| ())?
        || data.height != i32::try_from(height).map_err(|_| ())?
        || data.stride != i32::try_from(stride).map_err(|_| ())?
        || data.offset < 0
    {
        return Err(());
    }
    let offset = usize::try_from(data.offset).map_err(|_| ())?;
    let stride = usize::try_from(data.stride).map_err(|_| ())?;
    let row_bytes = usize::try_from(width)
        .map_err(|_| ())?
        .checked_mul(4)
        .ok_or(())?;
    let end = offset
        .checked_add(
            usize::try_from(height.saturating_sub(1))
                .map_err(|_| ())?
                .checked_mul(stride)
                .ok_or(())?,
        )
        .and_then(|last_row| last_row.checked_add(row_bytes))
        .ok_or(())?;
    (end <= mapping_length).then_some(()).ok_or(())
}

fn validate_screencopy_shm_buffer(
    buffer: &wl_buffer::WlBuffer,
    format: CaptureFormat,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<(), ()> {
    with_buffer_contents_mut(buffer, |_base, length, data| {
        validate_capture_buffer_data(length, data, format, width, height, stride)
    })
    .map_err(|_| ())?
}

fn schedule_capture_write(handle: LoopHandle<'static, WaylandState>, id: CaptureId) {
    let next = handle.clone();
    handle.insert_idle(move |state| {
        if state.write_capture_chunk(id) {
            schedule_capture_write(next, id);
        }
    });
}

fn schedule_capture_deadline(
    handle: LoopHandle<'static, WaylandState>,
    id: CaptureId,
    deadline: Instant,
) {
    handle
        .insert_source(Timer::from_deadline(deadline), move |fired, (), state| {
            if state
                .capture_frames
                .get(&id)
                .is_some_and(|record| !record.terminal && record.deadline == Some(fired))
            {
                state.fail_capture(id);
            }
            TimeoutAction::Drop
        })
        .expect("insert per-request screencopy deadline");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventPublication {
    Complete,
    Pending,
    Disconnected,
}

fn terminate_resource_exhausting_client(
    display_handle: &DisplayHandle,
    client: &Client,
    message: impl Into<String>,
) {
    let message = message.into();
    tracing::warn!(%message, "disconnecting resource-exhausting client");
    client.kill(
        display_handle,
        ProtocolError {
            // wl_display.error.no_memory is core error code 1.
            code: 1,
            object_id: 1,
            object_interface: "wl_display".to_string(),
            message,
        },
    );
}

struct LinuxReleaseUsePlatform {
    display_handle: DisplayHandle,
    retired_uses:
        BTreeMap<RetirementSequence, RetiredUse<ClientId, ObjectId, CommittedReleasePoint>>,
    retirement_requests: Option<RetirementRequestSender>,
    next_retirement_sequence: Option<u64>,
    next_expected_batch_id: u64,
    accepting_retirements: bool,
    explicit_sync_health: ExplicitSyncHealth,
    #[cfg(test)]
    test_probe: ReleaseUseTestProbe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExplicitSyncHealth {
    Healthy,
    Faulted(ExplicitSyncFault),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExplicitSyncFault {
    RequestQueueFull,
    WorkerUnavailable,
    RetirementSequenceExhausted,
    Worker(RetirementWorkerError),
    InvalidReport {
        expected_batch_id: RetirementBatchId,
        batch_id: RetirementBatchId,
        high_water: RetirementSequence,
    },
    PointSignalFailed(String),
}

impl LinuxReleaseUsePlatform {
    fn new(
        display_handle: DisplayHandle,
        retirement_requests: RetirementRequestSender,
        #[cfg(test)] test_probe: ReleaseUseTestProbe,
    ) -> Self {
        Self {
            display_handle,
            retired_uses: BTreeMap::new(),
            retirement_requests: Some(retirement_requests),
            next_retirement_sequence: Some(1),
            next_expected_batch_id: 1,
            accepting_retirements: true,
            explicit_sync_health: ExplicitSyncHealth::Healthy,
            #[cfg(test)]
            test_probe,
        }
    }

    fn allocate_retirement_sequence(&mut self) -> Option<RetirementSequence> {
        let sequence = self.next_retirement_sequence?;
        self.next_retirement_sequence = sequence.checked_add(1);
        Some(RetirementSequence(sequence))
    }

    fn transition_to_fault(
        &mut self,
        fault: ExplicitSyncFault,
        completed: Vec<TerminalUse<ClientId>>,
    ) -> RetirementUpdate<ClientId> {
        if matches!(self.explicit_sync_health, ExplicitSyncHealth::Faulted(_)) {
            return RetirementUpdate::Awaiting;
        }
        tracing::error!(?fault, "explicit-sync retirement permanently faulted");
        #[cfg(test)]
        self.test_probe.observations().faults.push(fault.clone());
        self.explicit_sync_health = ExplicitSyncHealth::Faulted(fault);
        self.accepting_retirements = false;
        self.retirement_requests.take();
        RetirementUpdate::Faulted(completed)
    }

    fn pending_batch(&self, report: &RetirementWorkerReport) -> Option<Vec<RetirementSequence>> {
        if report.batch_id != RetirementBatchId(self.next_expected_batch_id)
            || !self.retired_uses.contains_key(&report.high_water)
        {
            return None;
        }
        let sequences = self
            .retired_uses
            .range(..=report.high_water)
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>();
        (!sequences.is_empty()).then_some(sequences)
    }
}

enum CommittedReleasePoint {
    Linux(DrmSyncPoint),
    #[cfg(test)]
    Fake(TestCommittedPoint),
}

impl CommittedReleasePoint {
    fn signal(&self) -> std::io::Result<()> {
        match self {
            Self::Linux(point) => point.signal(),
            #[cfg(test)]
            Self::Fake(point) => point.signal(),
        }
    }

    #[cfg(test)]
    fn mark_retirement_seam(&self) -> Option<u64> {
        match self {
            Self::Linux(_) => None,
            Self::Fake(point) => {
                point.mark_retirement_seam();
                Some(point.id())
            }
        }
    }

    #[cfg(test)]
    fn mark_abandoned(&self) -> Option<u64> {
        match self {
            Self::Linux(_) => None,
            Self::Fake(point) => {
                point.mark_abandoned();
                Some(point.id())
            }
        }
    }

    #[cfg(test)]
    fn test_id(&self) -> Option<u64> {
        match self {
            Self::Linux(_) => None,
            Self::Fake(point) => Some(point.id()),
        }
    }
}

#[cfg(test)]
const TEST_POINT_PENDING: u8 = 0;
#[cfg(test)]
const TEST_POINT_SIGNALLED: u8 = 1;
#[cfg(test)]
const TEST_POINT_ABANDONED: u8 = 2;

#[cfg(test)]
struct TestCommittedPointState {
    id: u64,
    disposition: AtomicU8,
    reached_retirement_seam: AtomicBool,
    signal_count: AtomicUsize,
    signal_should_fail: AtomicBool,
}

#[cfg(test)]
struct TestCommittedPoint {
    state: Arc<TestCommittedPointState>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestCommittedPointHandle {
    state: Arc<TestCommittedPointState>,
}

#[cfg(test)]
impl TestCommittedPoint {
    fn pair(id: u64) -> (Self, TestCommittedPointHandle) {
        let state = Arc::new(TestCommittedPointState {
            id,
            disposition: AtomicU8::new(TEST_POINT_PENDING),
            reached_retirement_seam: AtomicBool::new(false),
            signal_count: AtomicUsize::new(0),
            signal_should_fail: AtomicBool::new(false),
        });
        (
            Self {
                state: Arc::clone(&state),
            },
            TestCommittedPointHandle { state },
        )
    }

    fn id(&self) -> u64 {
        self.state.id
    }

    fn signal(&self) -> std::io::Result<()> {
        self.state.signal_count.fetch_add(1, Ordering::SeqCst);
        if self.state.signal_should_fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other(
                "injected release-point signal failure",
            ));
        }
        self.state
            .disposition
            .compare_exchange(
                TEST_POINT_PENDING,
                TEST_POINT_SIGNALLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| std::io::Error::other("test release point disposed more than once"))?;
        Ok(())
    }

    fn mark_retirement_seam(&self) {
        assert!(
            !self
                .state
                .reached_retirement_seam
                .swap(true, Ordering::SeqCst),
            "test release point reaches the retirement seam exactly once"
        );
        assert_eq!(
            self.state.disposition.load(Ordering::SeqCst),
            TEST_POINT_PENDING,
            "reaching the retirement seam is not a terminal point disposition"
        );
    }

    fn mark_abandoned(&self) {
        let result = self.state.disposition.compare_exchange(
            TEST_POINT_PENDING,
            TEST_POINT_ABANDONED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        if !std::thread::panicking() {
            result.expect("test release point is abandoned exactly once");
        }
    }
}

#[cfg(test)]
impl Drop for TestCommittedPoint {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert_ne!(
                self.state.disposition.load(Ordering::SeqCst),
                TEST_POINT_PENDING,
                "taken release point was dropped without signalling or abandonment"
            );
        }
    }
}

#[cfg(test)]
impl TestCommittedPointHandle {
    fn signal_count(&self) -> usize {
        self.state.signal_count.load(Ordering::SeqCst)
    }

    fn fail_signal(&self) {
        self.state.signal_should_fail.store(true, Ordering::SeqCst);
    }

    fn reached_retirement_seam(&self) -> bool {
        self.state.reached_retirement_seam.load(Ordering::SeqCst)
    }

    fn was_abandoned(&self) -> bool {
        self.state.disposition.load(Ordering::SeqCst) == TEST_POINT_ABANDONED
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.state.disposition.load(Ordering::SeqCst),
            TEST_POINT_SIGNALLED | TEST_POINT_ABANDONED
        )
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestReleaseUseRetirement {
    use_id: DmabufUseId,
    client: ClientId,
    buffer: ObjectId,
    point_id: u64,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestReleaseUseAbandonment {
    use_id: DmabufUseId,
    client: ClientId,
    buffer: ObjectId,
    point_id: u64,
    reason: ReleaseUseAbandonReason,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestReleaseUseRejection {
    client: ClientId,
    point_id: u64,
    failure: ReleaseUseFailure,
}

#[cfg(test)]
#[derive(Default)]
struct ReleaseUseTestObservations {
    retirements: Vec<TestReleaseUseRetirement>,
    rejections: Vec<TestReleaseUseRejection>,
    abandonments: Vec<TestReleaseUseAbandonment>,
    faults: Vec<ExplicitSyncFault>,
}

#[cfg(test)]
#[derive(Clone, Default)]
struct ReleaseUseTestProbe(Arc<Mutex<ReleaseUseTestObservations>>);

#[cfg(test)]
impl ReleaseUseTestProbe {
    fn observations(&self) -> std::sync::MutexGuard<'_, ReleaseUseTestObservations> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ReleaseUsePlatform for LinuxReleaseUsePlatform {
    type ClientKey = ClientId;
    type Client = Client;
    type BufferKey = ObjectId;
    type Point = CommittedReleasePoint;
    type RetirementEvent = RetirementWorkerReport;

    fn explicit_sync_healthy(&self) -> bool {
        matches!(self.explicit_sync_health, ExplicitSyncHealth::Healthy)
            && self.accepting_retirements
    }

    fn retire_use(
        &mut self,
        retired: RetiredUse<Self::ClientKey, Self::BufferKey, Self::Point>,
    ) -> RetirementUpdate<Self::ClientKey> {
        tracing::debug!(
            use_id = retired.id.0,
            client = ?retired.client,
            buffer = ?retired.buffer,
            "DMA-BUF use entered GPU retirement"
        );
        #[cfg(test)]
        if let Some(point_id) = retired.release_point.mark_retirement_seam() {
            self.test_probe
                .observations()
                .retirements
                .push(TestReleaseUseRetirement {
                    use_id: retired.id,
                    client: retired.client.clone(),
                    buffer: retired.buffer.clone(),
                    point_id,
                });
        }
        let terminal = TerminalUse {
            id: retired.id,
            client: retired.client.clone(),
        };
        let Some(sequence) = self.allocate_retirement_sequence() else {
            self.abandon_use(
                AbandonedUse {
                    id: retired.id,
                    client: retired.client,
                    buffer: retired.buffer,
                    release_point: retired.release_point,
                },
                ReleaseUseAbandonReason::ExplicitSyncFault,
            );
            return self.transition_to_fault(
                ExplicitSyncFault::RetirementSequenceExhausted,
                vec![terminal],
            );
        };
        assert!(
            self.retired_uses.insert(sequence, retired).is_none(),
            "retirement sequence must be unique"
        );
        let request = self
            .retirement_requests
            .as_ref()
            .ok_or(RetirementRequestError::Disconnected)
            .and_then(|sender| sender.try_send(sequence));
        match request {
            Ok(()) => RetirementUpdate::Awaiting,
            Err(RetirementRequestError::Full) => {
                self.transition_to_fault(ExplicitSyncFault::RequestQueueFull, Vec::new())
            }
            Err(RetirementRequestError::Disconnected) => {
                self.transition_to_fault(ExplicitSyncFault::WorkerUnavailable, Vec::new())
            }
        }
    }

    fn handle_retirement_event(
        &mut self,
        report: Self::RetirementEvent,
    ) -> RetirementUpdate<Self::ClientKey> {
        if matches!(self.explicit_sync_health, ExplicitSyncHealth::Faulted(_)) {
            return RetirementUpdate::Awaiting;
        }
        let expected_batch_id = RetirementBatchId(self.next_expected_batch_id);
        let Some(sequences) = self.pending_batch(&report) else {
            let fault = ExplicitSyncFault::InvalidReport {
                expected_batch_id,
                batch_id: report.batch_id,
                high_water: report.high_water,
            };
            return self.transition_to_fault(fault, Vec::new());
        };
        if let Err(error) = report.result {
            return self.transition_to_fault(ExplicitSyncFault::Worker(error), Vec::new());
        }
        let Some(next_expected_batch_id) = self.next_expected_batch_id.checked_add(1) else {
            return self.transition_to_fault(
                ExplicitSyncFault::InvalidReport {
                    expected_batch_id,
                    batch_id: report.batch_id,
                    high_water: report.high_water,
                },
                Vec::new(),
            );
        };
        self.next_expected_batch_id = next_expected_batch_id;

        let mut completed = Vec::with_capacity(sequences.len());
        for sequence in sequences {
            let retired = self
                .retired_uses
                .remove(&sequence)
                .expect("validated retirement report names a pending use");
            let terminal = TerminalUse {
                id: retired.id,
                client: retired.client.clone(),
            };
            if let Err(error) = retired.release_point.signal() {
                let message = error.to_string();
                self.abandon_use(
                    AbandonedUse {
                        id: retired.id,
                        client: retired.client,
                        buffer: retired.buffer,
                        release_point: retired.release_point,
                    },
                    ReleaseUseAbandonReason::ExplicitSyncFault,
                );
                completed.push(terminal);
                return self
                    .transition_to_fault(ExplicitSyncFault::PointSignalFailed(message), completed);
            }
            drop(retired.release_point);
            completed.push(terminal);
        }
        RetirementUpdate::Completed(completed)
    }

    fn retirement_worker_closed(&mut self) -> RetirementUpdate<Self::ClientKey> {
        if !self.accepting_retirements
            || matches!(self.explicit_sync_health, ExplicitSyncHealth::Faulted(_))
        {
            RetirementUpdate::Awaiting
        } else {
            self.transition_to_fault(ExplicitSyncFault::WorkerUnavailable, Vec::new())
        }
    }

    fn stop_retirement_worker(&mut self) {
        self.retirement_requests.take();
    }

    fn disconnect_explicit_sync_client(&mut self, client: &Self::ClientKey) {
        self.display_handle.backend_handle().kill_client(
            client.clone(),
            DisconnectReason::ProtocolError(ProtocolError {
                code: 1,
                object_id: 1,
                object_interface: "wl_display".into(),
                message: "explicit-sync retirement permanently faulted".into(),
            }),
        );
    }

    fn abandon_use(
        &mut self,
        abandoned: AbandonedUse<Self::ClientKey, Self::BufferKey, Self::Point>,
        reason: ReleaseUseAbandonReason,
    ) {
        tracing::debug!(
            use_id = abandoned.id.0,
            client = ?abandoned.client,
            buffer = ?abandoned.buffer,
            ?reason,
            "abandoning unsignalled DMA-BUF release point"
        );
        #[cfg(test)]
        if let Some(point_id) = abandoned.release_point.mark_abandoned() {
            self.test_probe
                .observations()
                .abandonments
                .push(TestReleaseUseAbandonment {
                    use_id: abandoned.id,
                    client: abandoned.client.clone(),
                    buffer: abandoned.buffer.clone(),
                    point_id,
                    reason,
                });
        }
        drop(abandoned.release_point);
    }

    fn abandon_retired_uses(
        &mut self,
        reason: ReleaseUseAbandonReason,
    ) -> Vec<TerminalUse<Self::ClientKey>> {
        self.accepting_retirements = false;
        self.retirement_requests.take();
        let retired = mem::take(&mut self.retired_uses);
        let mut terminal = Vec::with_capacity(retired.len());
        for (_, retired) in retired {
            terminal.push(TerminalUse {
                id: retired.id,
                client: retired.client.clone(),
            });
            self.abandon_use(
                AbandonedUse {
                    id: retired.id,
                    client: retired.client,
                    buffer: retired.buffer,
                    release_point: retired.release_point,
                },
                reason,
            );
        }
        terminal
    }

    fn reject_use(
        &mut self,
        client: &Self::Client,
        release_point: Self::Point,
        failure: &ReleaseUseFailure,
    ) {
        #[cfg(test)]
        let point_id = release_point.test_id();
        if let Err(error) = release_point.signal() {
            tracing::warn!(%error, "failed to signal rejected DMA-BUF release point");
        }
        #[cfg(test)]
        if let Some(point_id) = point_id {
            self.test_probe
                .observations()
                .rejections
                .push(TestReleaseUseRejection {
                    client: client.id(),
                    point_id,
                    failure: *failure,
                });
        }
        terminate_resource_exhausting_client(
            &self.display_handle,
            client,
            format!("explicit-sync release use unavailable: {failure}"),
        );
    }
}

fn committed_syncobj_state(
    cached: &mut compositor::CachedState<DrmSyncobjCachedState>,
) -> &mut DrmSyncobjCachedState {
    cached.current()
}

impl WaylandState {
    fn create_screencopy_manager(&mut self, client_id: ClientId) -> Option<u64> {
        let client_live = self
            .capture_managers
            .values()
            .filter(|manager| manager.client_id == client_id)
            .count();
        let global_live = self.capture_managers.len();
        if client_live >= MAX_CLIENT_CAPTURE_MANAGERS || global_live >= MAX_GLOBAL_CAPTURE_MANAGERS
        {
            return None;
        }
        self.next_capture_manager_id = self.next_capture_manager_id.wrapping_add(1).max(1);
        let id = self.next_capture_manager_id;
        self.capture_managers.insert(
            id,
            CaptureManagerRecord {
                client_id,
                live_frames: 0,
                resource_alive: true,
                damage_baselines: HashMap::new(),
            },
        );
        Some(id)
    }

    fn destroy_screencopy_manager(&mut self, id: u64) {
        let remove = self.capture_managers.get_mut(&id).is_some_and(|manager| {
            manager.resource_alive = false;
            manager.live_frames == 0
        });
        if remove {
            self.capture_managers.remove(&id);
        }
    }

    fn allocate_capture_id(&mut self) -> CaptureId {
        self.next_capture_id = self.next_capture_id.wrapping_add(1).max(1);
        CaptureId(self.next_capture_id)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the manager request plus new frame identity
    fn create_screencopy_frame(
        &mut self,
        id: CaptureId,
        manager_id: u64,
        client: &Client,
        resource: ZwlrScreencopyFrameV1,
        output: &wl_output_protocol::WlOutput,
        overlay_cursor: i32,
        logical_region: Option<(i32, i32, i32, i32)>,
    ) {
        let client_live_frames = self
            .capture_frames
            .values()
            .filter(|frame| frame.client_id == client.id() && frame.resource_alive)
            .count();
        if self.capture_frames.len() >= MAX_CAPTURE_FRAMES
            || client_live_frames >= MAX_CLIENT_CAPTURE_REQUESTS
        {
            resource.failed();
            return;
        }
        let Some(mut source) = self.backend.capture_source_for_output(output) else {
            resource.failed();
            return;
        };
        let Some(region) = capture_physical_region(&source, logical_region) else {
            resource.failed();
            return;
        };
        let Some(stride) = region.width.checked_mul(4) else {
            resource.failed();
            return;
        };
        let dmabuf_eligible = capture_dmabuf_is_eligible(&source, logical_region, region);
        source.dmabuf = dmabuf_eligible
            .then(|| {
                self.capture_advertisements
                    .advertisement(&source.source_id, source.source_storage_extent)
            })
            .flatten();
        resource.buffer(
            wl_shm::Format::Xrgb8888,
            region.width,
            region.height,
            stride,
        );
        if resource.version() >= 3 {
            if let Some(advertisement) = &source.dmabuf {
                resource.linux_dmabuf(
                    advertisement.fourcc,
                    advertisement.width,
                    advertisement.height,
                );
            }
            resource.buffer_done();
        }
        let record = CaptureFrameRecord {
            resource: resource.clone(),
            client_id: client.id(),
            manager_id,
            source_id: source.source_id,
            output_name: source.output_name,
            generation: source.generation,
            security_epoch: self.next_security_presentation_epoch,
            region,
            logical_rect: source.logical_rect,
            source_storage_extent: source.source_storage_extent,
            displayed_physical_extent: source.displayed_physical_extent,
            scale120: source.scale120,
            transform: source.transform,
            dmabuf_advertisement: source.dmabuf,
            format: CaptureFormat::Xrgb8888,
            stride,
            overlay_cursor: overlay_cursor != 0,
            submitted: false,
            with_damage: false,
            terminal: false,
            resource_alive: true,
            job_pending: false,
            destination: None,
            pixels: None,
            dmabuf_completion: None,
            presentation: None,
            next_write_row: 0,
            write_scheduled: false,
            reserved_bytes: 0,
            deadline: None,
            cancellation: None,
            damage_baseline: None,
            damage_revision: 0,
            damage: Vec::new(),
        };
        self.capture_frames_by_resource.insert(resource.id(), id);
        self.capture_frames.insert(id, record);
        if let Some(manager) = self.capture_managers.get_mut(&manager_id) {
            manager.live_frames = manager.live_frames.saturating_add(1);
        }
    }

    fn submit_screencopy(
        &mut self,
        id: CaptureId,
        frame: &ZwlrScreencopyFrameV1,
        buffer: wl_buffer::WlBuffer,
        with_damage: bool,
    ) {
        let Some(existing) = self.capture_frames.get(&id) else {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "screencopy frame is no longer available",
            );
            return;
        };
        if existing.submitted {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "screencopy frame has already been used",
            );
            return;
        }
        // One-shot before validation: an invalid first buffer cannot be
        // followed by a valid second request on the same frame object.
        let current_epoch = self.next_security_presentation_epoch;
        let damage_baseline = self.capture_frames.get(&id).and_then(|record| {
            self.capture_managers
                .get(&record.manager_id)
                .and_then(|manager| manager.damage_baselines.get(&record.source_id).copied())
        });
        let (format, region, stride, dmabuf_advertisement) = {
            let record = self
                .capture_frames
                .get_mut(&id)
                .expect("capture checked immediately above");
            record.submitted = true;
            // Creation advertises immutable geometry; submission selects the
            // security epoch whose displayed scene this request may observe.
            record.security_epoch = current_epoch;
            record.damage_baseline = damage_baseline;
            (
                record.format,
                record.region,
                record.stride,
                record.dmabuf_advertisement.clone(),
            )
        };
        let destination = match get_dmabuf(&buffer) {
            Ok(dmabuf) => {
                let Some(advertisement) = dmabuf_advertisement else {
                    if let Some(record) = self.capture_frames.get_mut(&id) {
                        record.terminal = true;
                    }
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                        "DMA-BUF was not advertised for this screencopy frame",
                    );
                    return;
                };
                let metadata_matches = Self::capture_dmabuf_metadata_matches(
                    &advertisement,
                    dmabuf.num_planes(),
                    u32::try_from(dmabuf.size().w).ok(),
                    u32::try_from(dmabuf.size().h).ok(),
                    dmabuf.format().code as u32,
                    u64::from(dmabuf.format().modifier),
                );
                match metadata_matches {
                    CaptureDmabufMetadataMatch::Matches => {}
                    CaptureDmabufMetadataMatch::InvalidBuffer => {
                        if let Some(record) = self.capture_frames.get_mut(&id) {
                            record.terminal = true;
                        }
                        frame.post_error(
                            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                            "DMA-BUF does not match the immutable screencopy advertisement",
                        );
                        return;
                    }
                    CaptureDmabufMetadataMatch::UnsupportedModifier => {
                        self.fail_capture(id);
                        return;
                    }
                }
                let descriptor = match describe_dmabuf(dmabuf) {
                    Ok(descriptor) => Arc::new(descriptor),
                    Err(error) => {
                        tracing::warn!(capture_id = id.0, %error, "failed to retain screencopy DMA-BUF descriptor");
                        self.fail_capture(id);
                        return;
                    }
                };
                let Some(retention_token) = self.try_retain_capture_dmabuf(buffer.clone()) else {
                    self.fail_capture(id);
                    return;
                };
                CaptureFrameDestination::Dmabuf {
                    buffer,
                    descriptor,
                    retention_token: Some(retention_token),
                }
            }
            Err(_) => {
                if validate_screencopy_shm_buffer(
                    &buffer,
                    format,
                    region.width,
                    region.height,
                    stride,
                )
                .is_err()
                {
                    if let Some(record) = self.capture_frames.get_mut(&id) {
                        record.terminal = true;
                    }
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                        "buffer does not match the advertised screencopy shm layout",
                    );
                    return;
                }
                CaptureFrameDestination::Shm(buffer)
            }
        };
        let cancellation = CaptureCancellation::default();
        let deadline = Instant::now()
            .checked_add(CAPTURE_REQUEST_TIMEOUT)
            .unwrap_or_else(Instant::now);
        let wait = {
            let record = self
                .capture_frames
                .get_mut(&id)
                .expect("validated capture remains live");
            record.destination = Some(destination);
            record.with_damage = with_damage;
            record.cancellation = Some(cancellation.clone());
            record.deadline = Some(deadline);
            (with_damage && record.damage_baseline.is_some()).then(|| {
                crate::capture::CaptureDamageWatch {
                    id,
                    source_id: record.source_id.clone(),
                    generation: record.generation,
                    security_epoch: record.security_epoch,
                    region: record.region,
                    logical_rect: record.logical_rect,
                    source_storage_extent: record.source_storage_extent,
                    displayed_physical_extent: record.displayed_physical_extent,
                    scale120: record.scale120,
                    transform: record.transform,
                    overlay_cursor: record.overlay_cursor,
                    baseline: record
                        .damage_baseline
                        .expect("damage waiter has a baseline"),
                    cancellation: cancellation.clone(),
                    deadline,
                }
            })
        };
        schedule_capture_deadline(self.capture_loop_handle.clone(), id, deadline);
        if let Some(wait) = wait {
            self.events.push(ProtocolEvent::CaptureDamageWatch(wait));
        } else {
            self.admit_capture(id, 0, Vec::new());
        }
    }

    fn capture_dmabuf_metadata_matches(
        advertisement: &crate::backend::CaptureDmabufAdvertisement,
        planes: usize,
        width: Option<u32>,
        height: Option<u32>,
        fourcc: u32,
        modifier: u64,
    ) -> CaptureDmabufMetadataMatch {
        if planes != 1
            || width != Some(advertisement.width)
            || height != Some(advertisement.height)
            || fourcc != advertisement.fourcc
        {
            CaptureDmabufMetadataMatch::InvalidBuffer
        } else if advertisement
            .allowed_modifiers
            .binary_search(&modifier)
            .is_err()
        {
            CaptureDmabufMetadataMatch::UnsupportedModifier
        } else {
            CaptureDmabufMetadataMatch::Matches
        }
    }

    fn admit_capture(&mut self, id: CaptureId, damage_revision: u64, damage: Vec<CaptureRegion>) {
        let Some(record) = self.capture_frames.get(&id) else {
            return;
        };
        if record.terminal
            || record.job_pending
            || record.deadline.is_none_or(|at| at <= Instant::now())
        {
            self.fail_capture(id);
            return;
        }
        let client_id = record.client_id.clone();
        let source_storage_extent = record.source_storage_extent;
        let region = record.region;
        // Account for the capture-owned staging and packed conversion result
        // only after a damage waiter becomes eligible.
        let dmabuf_destination = matches!(
            record.destination,
            Some(CaptureFrameDestination::Dmabuf { .. })
        );
        let reserved_bytes = if dmabuf_destination {
            capture_dmabuf_reservation_bytes(record.displayed_physical_extent)
        } else {
            capture_reservation_bytes(source_storage_extent, region)
        };
        let Some(reserved_bytes) = reserved_bytes else {
            self.fail_capture(id);
            return;
        };
        let client_in_flight = self
            .capture_reservations
            .values()
            .filter(|candidate| candidate.client_id == client_id)
            .count();
        let global_in_flight = self.capture_reservations.len();
        let client_bytes = self
            .capture_reservations
            .values()
            .filter(|candidate| candidate.client_id == client_id)
            .try_fold(reserved_bytes, |total, candidate| {
                total.checked_add(candidate.bytes)
            });
        let global_bytes = self
            .capture_reservations
            .values()
            .try_fold(reserved_bytes, |total, candidate| {
                total.checked_add(candidate.bytes)
            });
        if client_in_flight >= MAX_CLIENT_CAPTURE_REQUESTS
            || global_in_flight >= MAX_IN_FLIGHT_CAPTURES
            || client_bytes.is_none_or(|bytes| bytes > MAX_CLIENT_CAPTURE_BYTES)
            || global_bytes.is_none_or(|bytes| bytes > MAX_GLOBAL_CAPTURE_BYTES)
        {
            self.fail_capture(id);
            return;
        }
        let cancellation = record
            .cancellation
            .clone()
            .expect("submitted capture owns cancellation state");
        let reservation = CaptureReservationLease::new(id, self.capture_release_sender.clone());
        let request = {
            let record = self
                .capture_frames
                .get_mut(&id)
                .expect("admitted capture remains live");
            record.damage_revision = damage_revision;
            record.damage = damage;
            record.reserved_bytes = reserved_bytes;
            record.job_pending = true;
            let deadline = record.deadline.expect("submitted capture owns a deadline");
            let destination = match record
                .destination
                .as_mut()
                .expect("submitted capture owns its destination")
            {
                CaptureFrameDestination::Shm(_) => CaptureDestination::Shm,
                CaptureFrameDestination::Dmabuf {
                    descriptor,
                    retention_token,
                    ..
                } => CaptureDestination::Dmabuf(CaptureDmabufDestination {
                    descriptor: Arc::clone(descriptor),
                    retention_token: retention_token
                        .take()
                        .expect("DMA-BUF capture token moves to the render request once"),
                }),
            };
            CaptureRequest {
                id,
                source_id: record.source_id.clone(),
                output_name: record.output_name.clone(),
                generation: record.generation,
                security_epoch: record.security_epoch,
                region: record.region,
                logical_rect: record.logical_rect,
                source_storage_extent: record.source_storage_extent,
                displayed_physical_extent: record.displayed_physical_extent,
                scale120: record.scale120,
                transform: record.transform,
                format: record.format,
                destination,
                overlay_cursor: record.overlay_cursor,
                cursor: None,
                with_damage: record.with_damage,
                damage_baseline: record.damage_baseline,
                damage_revision: record.damage_revision,
                damage: record.damage.clone(),
                cancellation,
                reservation,
                deadline,
            }
        };
        self.capture_reservations.insert(
            id,
            CaptureReservationRecord {
                client_id,
                bytes: reserved_bytes,
            },
        );
        self.events.push(ProtocolEvent::CaptureRequested(request));
    }

    fn destroy_screencopy_frame(&mut self, id: CaptureId) {
        let Some(record) = self.capture_frames.get_mut(&id) else {
            return;
        };
        record.resource_alive = false;
        record.terminal = true;
        record.job_pending = false;
        if let Some(cancellation) = &record.cancellation {
            cancellation.cancel();
        }
        let retention_token = record
            .destination
            .as_mut()
            .and_then(CaptureFrameDestination::take_retention_token);
        let completion_token = record
            .dmabuf_completion
            .take()
            .map(|completion| completion.retention_token);
        record.destination = None;
        record.pixels = None;
        record.presentation = None;
        let resource_id = record.resource.id();
        let manager_id = record.manager_id;
        if let Some(token) = retention_token {
            self.release_buffer_token(token);
        }
        if let Some(token) = completion_token {
            self.release_buffer_token(token);
        }
        self.capture_frames_by_resource.remove(&resource_id);
        let remove_manager = self
            .capture_managers
            .get_mut(&manager_id)
            .is_some_and(|manager| {
                manager.live_frames = manager.live_frames.saturating_sub(1);
                !manager.resource_alive && manager.live_frames == 0
            });
        if remove_manager {
            self.capture_managers.remove(&manager_id);
        }
        self.capture_frames.remove(&id);
    }

    fn fail_capture(&mut self, id: CaptureId) {
        let Some(record) = self.capture_frames.get_mut(&id) else {
            return;
        };
        if record.terminal {
            return;
        }
        record.terminal = true;
        record.job_pending = false;
        if let Some(cancellation) = &record.cancellation {
            cancellation.cancel();
        }
        let retention_token = record
            .destination
            .as_mut()
            .and_then(CaptureFrameDestination::take_retention_token);
        let completion_token = record
            .dmabuf_completion
            .take()
            .map(|completion| completion.retention_token);
        record.destination = None;
        record.pixels = None;
        record.presentation = None;
        if record.resource.is_alive() {
            record.resource.failed();
        }
        if let Some(token) = retention_token {
            self.release_buffer_token(token);
        }
        if let Some(token) = completion_token {
            self.release_buffer_token(token);
        }
    }

    fn release_capture_reservation(&mut self, id: CaptureId) {
        self.capture_reservations.remove(&id);
        if let Some(record) = self.capture_frames.get_mut(&id) {
            record.reserved_bytes = 0;
        }
    }

    fn capture_pixels_ready(&mut self, pixels: CapturePixels) {
        let Some(record) = self.capture_frames.get_mut(&pixels.id) else {
            return;
        };
        record.job_pending = false;
        if record.terminal
            || record.source_id != pixels.source_id
            || record.generation != pixels.generation
            || record.security_epoch != pixels.security_epoch
            || record.region.width != pixels.width
            || record.region.height != pixels.height
            || record.format != pixels.format
        {
            return;
        }
        let resource_id = record.resource.id();
        record.pixels = Some(pixels);
        self.maybe_schedule_capture_write(resource_id);
    }

    fn capture_dmabuf_ready(&mut self, completion: CaptureDmabufComplete) {
        let id = completion.id;
        let accepted = self.capture_frames.get_mut(&id).is_some_and(|record| {
            record.job_pending = false;
            if record.terminal
                || record.source_id != completion.source_id
                || record.generation != completion.generation
                || record.security_epoch != completion.security_epoch
                || !matches!(
                    record.destination,
                    Some(CaptureFrameDestination::Dmabuf { .. })
                )
            {
                return false;
            }
            record.dmabuf_completion = Some(completion.clone());
            true
        });
        if accepted {
            let resource_id = self
                .capture_frames
                .get(&id)
                .expect("accepted DMA-BUF completion retains its frame")
                .resource
                .id();
            self.maybe_schedule_capture_write(resource_id);
        } else {
            self.release_buffer_token(completion.retention_token);
        }
    }

    fn capture_dmabuf_failed(&mut self, failure: CaptureDmabufFailed) {
        let matching = self.capture_frames.get(&failure.id).is_some_and(|record| {
            record.generation == failure.generation
                && record.security_epoch == failure.security_epoch
        });
        if matching {
            self.fail_capture(failure.id);
        }
        self.release_buffer_token(failure.retention_token);
    }

    fn capture_presented(&mut self, presented: CapturePresented) {
        let Some(record) = self.capture_frames.get_mut(&presented.id) else {
            return;
        };
        if record.terminal
            || record.source_id != presented.source_id
            || record.generation != presented.generation
            || record.security_epoch != presented.security_epoch
            || presented.nanoseconds > 999_999_999
        {
            return;
        }
        let resource_id = record.resource.id();
        record.presentation = Some(presented);
        self.maybe_schedule_capture_write(resource_id);
    }

    fn maybe_schedule_capture_write(&mut self, resource_id: ObjectId) {
        let Some(id) = self.capture_frames_by_resource.get(&resource_id).copied() else {
            return;
        };
        if self.maybe_publish_capture_dmabuf(id) {
            return;
        }
        let ready = self.capture_frames.get_mut(&id).is_some_and(|record| {
            if record.write_scheduled
                || record.terminal
                || record.pixels.is_none()
                || record.presentation.is_none()
            {
                return false;
            }
            let pixels = record.pixels.as_ref().expect("checked above");
            let presented = record.presentation.as_ref().expect("checked above");
            if pixels.frame_token != presented.frame_token {
                return false;
            }
            record.write_scheduled = true;
            true
        });
        if ready {
            schedule_capture_write(self.capture_loop_handle.clone(), id);
        }
    }

    fn maybe_publish_capture_dmabuf(&mut self, id: CaptureId) -> bool {
        let ready = self.capture_frames.get(&id).is_some_and(|record| {
            !record.terminal
                && record.dmabuf_completion.is_some()
                && record.presentation.is_some()
                && matches!(
                    record.destination,
                    Some(CaptureFrameDestination::Dmabuf { .. })
                )
        });
        if !ready {
            return false;
        }
        let invalid_token = self.capture_frames.get(&id).and_then(|record| {
            let completion = record.dmabuf_completion.as_ref()?;
            let presentation = record.presentation.as_ref()?;
            (completion.frame_token != presentation.frame_token
                || presentation.nanoseconds > 999_999_999
                || completion.damage.iter().any(|damage| {
                    damage.width == 0
                        || damage.height == 0
                        || damage.x.saturating_add(damage.width) > record.region.width
                        || damage.y.saturating_add(damage.height) > record.region.height
                }))
            .then_some(completion.retention_token)
        });
        if invalid_token.is_some() {
            self.fail_capture(id);
            return true;
        }
        let (retention_token, manager_id, source_id, damage_revision) = {
            let record = self
                .capture_frames
                .get_mut(&id)
                .expect("DMA-BUF terminal checked immediately above");
            let completion = record
                .dmabuf_completion
                .take()
                .expect("DMA-BUF terminal owns completion");
            let presentation = record
                .presentation
                .take()
                .expect("DMA-BUF terminal owns presentation");
            debug_assert_eq!(completion.frame_token, presentation.frame_token);
            if record.with_damage {
                for damage in &completion.damage {
                    record
                        .resource
                        .damage(damage.x, damage.y, damage.width, damage.height);
                }
            }
            record
                .resource
                .flags(zwlr_screencopy_frame_v1::Flags::empty());
            record.resource.ready(
                (presentation.seconds >> 32) as u32,
                presentation.seconds as u32,
                presentation.nanoseconds,
            );
            record.terminal = true;
            record.job_pending = false;
            record.destination = None;
            (
                completion.retention_token,
                record.manager_id,
                record.source_id.clone(),
                completion.damage_revision,
            )
        };
        if let Some(manager) = self.capture_managers.get_mut(&manager_id) {
            manager
                .damage_baselines
                .entry(source_id)
                .and_modify(|revision| *revision = (*revision).max(damage_revision))
                .or_insert(damage_revision);
        }
        self.release_buffer_token(retention_token);
        true
    }

    /// Copy at most `CAPTURE_SHM_BYTES_PER_TURN`, then re-arm as a calloop idle
    /// source. No one client can monopolise the shared protocol dispatch thread
    /// with a full-output memcpy, and ready is emitted only by the final chunk.
    fn write_capture_chunk(&mut self, id: CaptureId) -> bool {
        let Some((buffer, pixels, presentation, format, region, stride, start_row)) =
            self.capture_frames.get_mut(&id).and_then(|record| {
                record.write_scheduled = false;
                (!record.terminal).then(|| {
                    Some((
                        match record.destination.as_ref()? {
                            CaptureFrameDestination::Shm(buffer) => buffer.clone(),
                            CaptureFrameDestination::Dmabuf { .. } => return None,
                        },
                        record.pixels.clone()?,
                        record.presentation.clone()?,
                        record.format,
                        record.region,
                        record.stride,
                        record.next_write_row,
                    ))
                })?
            })
        else {
            return false;
        };
        if !buffer.is_alive() || pixels.frame_token != presentation.frame_token {
            self.fail_capture(id);
            return false;
        }
        let row_bytes = stride as usize;
        let rows_per_turn = (CAPTURE_SHM_BYTES_PER_TURN / row_bytes).max(1) as u32;
        let end_row = start_row.saturating_add(rows_per_turn).min(region.height);
        let result = with_buffer_contents_mut(&buffer, |base, length, data| {
            validate_capture_buffer_data(
                length,
                data,
                format,
                region.width,
                region.height,
                stride,
            )?;
            let offset = usize::try_from(data.offset).map_err(|_| ())?;
            let stride = usize::try_from(data.stride).map_err(|_| ())?;
            for row in start_row..end_row {
                let row = row as usize;
                let source = pixels
                    .packed_bgra
                    .get(row * row_bytes..(row + 1) * row_bytes)
                    .ok_or(())?;
                let destination_offset = offset
                    .checked_add(row.checked_mul(stride).ok_or(())?)
                    .ok_or(())?;
                // SAFETY: the mapping and complete destination extent were
                // checked above; no reference into client-mutated shm escapes
                // this callback.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source.as_ptr(),
                        base.add(destination_offset),
                        row_bytes,
                    );
                }
            }
            Ok::<(), ()>(())
        });
        if !matches!(result, Ok(Ok(()))) {
            self.fail_capture(id);
            return false;
        }
        let record = self
            .capture_frames
            .get_mut(&id)
            .expect("capture survives its checked shm write");
        record.next_write_row = end_row;
        if end_row < record.region.height {
            record.write_scheduled = true;
            return true;
        }
        if record.with_damage {
            for damage in &pixels.damage {
                if damage.width != 0
                    && damage.height != 0
                    && damage.x.saturating_add(damage.width) <= record.region.width
                    && damage.y.saturating_add(damage.height) <= record.region.height
                {
                    record
                        .resource
                        .damage(damage.x, damage.y, damage.width, damage.height);
                }
            }
        }
        let flags = if pixels.y_invert {
            zwlr_screencopy_frame_v1::Flags::YInvert
        } else {
            zwlr_screencopy_frame_v1::Flags::empty()
        };
        record.resource.flags(flags);
        record.resource.ready(
            (presentation.seconds >> 32) as u32,
            presentation.seconds as u32,
            presentation.nanoseconds,
        );
        if let Some(manager) = self.capture_managers.get_mut(&record.manager_id) {
            manager
                .damage_baselines
                .entry(record.source_id.clone())
                .and_modify(|revision| *revision = (*revision).max(pixels.damage_revision))
                .or_insert(pixels.damage_revision);
        }
        record.terminal = true;
        record.job_pending = false;
        record.destination = None;
        record.pixels = None;
        record.presentation = None;
        false
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn retain_current_kms_capture_baselines(
        &mut self,
        generations: &BTreeMap<crate::backend::kms::OutputKey, u64>,
    ) {
        for manager in self.capture_managers.values_mut() {
            retain_current_kms_damage_baselines(&mut manager.damage_baselines, generations);
        }
    }

    fn fail_stale_capture_epochs(&mut self, current_epoch: u64) {
        let stale = self
            .capture_frames
            .iter()
            .filter_map(|(id, frame)| {
                (!frame.terminal && frame.submitted && frame.security_epoch != current_epoch)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in stale {
            self.fail_capture(id);
        }
    }

    fn session_lock_active(&self) -> bool {
        self.lock_lifecycle.is_active()
            || (matches!(self.backend, BackendData::Kms(_))
                && self.kms_session_lock_gate.normal_scene_restricted())
    }

    fn session_lock_acceptance_output(&self) -> Option<Output> {
        if matches!(self.backend, BackendData::Kms(_))
            && self.kms_session_lock_gate.ready_outputs().is_none()
        {
            return None;
        }
        self.backend
            .default_output()
            .filter(|output| output.current_mode().is_some())
    }

    fn begin_session_lock(&mut self, locker: SessionLocker) {
        let Some(output) = self.session_lock_acceptance_output() else {
            return;
        };
        if self.session_lock_active() {
            return;
        }
        let Some(owner) = locker.ext_session_lock().client().map(|client| client.id()) else {
            return;
        };
        let lock_resource = locker.ext_session_lock().clone();
        self.next_lock_generation = self.next_lock_generation.saturating_add(1);
        self.next_security_presentation_epoch =
            self.next_security_presentation_epoch.saturating_add(1);
        self.fail_stale_capture_epochs(self.next_security_presentation_epoch);
        let generation = self.next_lock_generation;
        let presentation_epoch = self.next_security_presentation_epoch;
        let mut pending_outputs = HashSet::new();
        pending_outputs.insert(output.name());
        let pending_kms_outputs = self
            .kms_session_lock_gate
            .ready_outputs()
            .cloned()
            .unwrap_or_default();

        // Reconcile releases while ordinary focus and binding state still have
        // their pre-lock meaning, then make every later policy query fail closed.
        self.release_pressed_keys();
        #[cfg(feature = "bus")]
        self.mark_session_observation_dirty();
        self.lock_lifecycle = LockLifecycle::Locking {
            owner,
            lock_resource,
            locker,
            generation,
            presentation_epoch,
            pending_outputs,
            pending_kms_outputs,
        };
        self.teardown_input_for_session_lock();
        self.close_all_foreign_toplevels();
        self.events.push(ProtocolEvent::SecurityScene {
            active: true,
            presentation_epoch: Some(presentation_epoch),
            presentations: vec![SecurityPresentationTarget {
                output: output.name(),
                scene: SecurityPresentationScene::Blank,
            }],
        });
        self.events.push(ProtocolEvent::SurfaceRoster {
            mapped: self.mapped_surface_ids().into_iter().collect(),
        });
        tracing::info!(
            generation,
            presentation_epoch,
            "session lock entering Locking"
        );
    }

    fn teardown_input_for_session_lock(&mut self) {
        #[cfg(feature = "bus")]
        self.reset_corner_detector();
        let popup_parents = self
            .surfaces
            .values()
            .filter(|record| {
                matches!(
                    record.role,
                    SurfaceRole::Toplevel(_) | SurfaceRole::Layer(_)
                )
            })
            .map(|record| record.role.wl_surface().clone())
            .collect::<Vec<_>>();
        for parent in popup_parents {
            self.dismiss_popup_descendants(&parent);
        }
        if self.keyboard.is_grabbed() {
            self.keyboard.clone().unset_grab(self);
        }
        if self.pointer.is_grabbed() {
            self.pointer.clone().unset_grab_without_focus_restore(
                self,
                SERIAL_COUNTER.next_serial(),
                monotonic_millis(),
            );
        }
        self.cancel_touch();
        self.cancel_chrome_pointer_grab(false);
        self.finish_interactive_pointer(false);
        self.interactive_pointer = None;
        self.exclusive_keyboard_focus = None;
        self.last_keyboard_action = None;
        self.keyboard
            .clone()
            .set_focus(self, None, SERIAL_COUNTER.next_serial());
        let (x, y) = self.cursor_position;
        self.pointer.clone().motion(
            self,
            None,
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: monotonic_millis(),
            },
        );
        self.pointer.clone().frame(self);
        self.pointer_focus_local_position = None;
        if self.saved_cursor_selection.is_none() {
            self.saved_cursor_selection = Some(self.cursor_selection.clone());
        }
        self.cursor_selection = CursorSelection::Hidden;
        self.chrome_cursor_override = None;
        self.publish_current_cursor();
    }

    fn acknowledge_security_presentation(
        &mut self,
        presentation_epoch: u64,
        evidence: SecurityPresentationEvidence,
    ) {
        match evidence {
            SecurityPresentationEvidence::Nested { output } => {
                self.acknowledge_nested_security_presentation(presentation_epoch, &output);
            }
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
            SecurityPresentationEvidence::Kms { generation, output } => {
                self.acknowledge_kms_security_presentation(presentation_epoch, generation, &output);
            }
        }
    }

    fn acknowledge_nested_security_presentation(&mut self, presentation_epoch: u64, output: &str) {
        #[cfg(feature = "bus")]
        self.mark_session_observation_dirty();
        let lifecycle = mem::replace(&mut self.lock_lifecycle, LockLifecycle::Unlocked);
        match lifecycle {
            LockLifecycle::Locking {
                owner,
                lock_resource,
                locker,
                generation,
                presentation_epoch: expected,
                mut pending_outputs,
                pending_kms_outputs,
            } if expected == presentation_epoch => {
                pending_outputs.remove(output);
                if pending_outputs.is_empty() {
                    locker.lock();
                    self.lock_lifecycle = LockLifecycle::Locked {
                        owner,
                        lock_resource,
                        generation,
                    };
                    tracing::info!(
                        generation,
                        presentation_epoch,
                        "session lock entered Locked"
                    );
                } else {
                    self.lock_lifecycle = LockLifecycle::Locking {
                        owner,
                        lock_resource,
                        locker,
                        generation,
                        presentation_epoch: expected,
                        pending_outputs,
                        pending_kms_outputs,
                    };
                }
            }
            other => self.lock_lifecycle = other,
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn acknowledge_kms_security_presentation(
        &mut self,
        presentation_epoch: u64,
        generation: u64,
        output: &crate::backend::kms::OutputKey,
    ) {
        #[cfg(feature = "bus")]
        self.mark_session_observation_dirty();
        let lifecycle = mem::replace(&mut self.lock_lifecycle, LockLifecycle::Unlocked);
        match lifecycle {
            LockLifecycle::Locking {
                owner,
                lock_resource,
                locker,
                generation: lock_generation,
                presentation_epoch: expected_epoch,
                pending_outputs,
                mut pending_kms_outputs,
            } if expected_epoch == presentation_epoch
                && pending_kms_outputs.get(output) == Some(&generation) =>
            {
                pending_kms_outputs.remove(output);
                tracing::info!(
                    presentation_epoch,
                    generation,
                    output = output.connector_name,
                    phase = "initial-lock",
                    "session-lock-kms-epoch-displayed"
                );
                tracing::info!(
                    presentation_epoch,
                    generation,
                    "session-lock-kms-initial-epoch-displayed"
                );
                if pending_kms_outputs.is_empty() {
                    locker.lock();
                    self.lock_lifecycle = LockLifecycle::Locked {
                        owner,
                        lock_resource,
                        generation: lock_generation,
                    };
                    tracing::info!(
                        generation = lock_generation,
                        presentation_epoch,
                        "session lock entered Locked"
                    );
                } else {
                    self.lock_lifecycle = LockLifecycle::Locking {
                        owner,
                        lock_resource,
                        locker,
                        generation: lock_generation,
                        presentation_epoch: expected_epoch,
                        pending_outputs,
                        pending_kms_outputs,
                    };
                }
                return;
            }
            other => self.lock_lifecycle = other,
        }

        let resume_complete = self
            .kms_session_lock_gate
            .resume_barrier
            .as_mut()
            .is_some_and(|barrier| barrier.acknowledge(presentation_epoch, generation, output));
        if resume_complete {
            self.kms_session_lock_gate.resume_barrier = None;
            self.kms_session_lock_gate.input_hold_logged = false;
            tracing::info!(
                presentation_epoch,
                generation,
                output = output.connector_name,
                phase = "resume-lock",
                "session-lock-kms-epoch-displayed"
            );
            tracing::info!(
                presentation_epoch,
                generation,
                "session-lock-kms-resume-epoch-displayed"
            );
            tracing::info!("session-lock-kms-locked-exposure-enabled");
            return;
        }

        let unlock_complete = self
            .kms_session_lock_gate
            .unlock_barrier
            .as_mut()
            .is_some_and(|barrier| barrier.acknowledge(presentation_epoch, generation, output));
        if unlock_complete {
            self.kms_session_lock_gate.unlock_barrier = None;
            self.kms_session_lock_gate.input_hold_logged = false;
            tracing::info!(
                presentation_epoch,
                generation,
                output = output.connector_name,
                phase = "resume-unlock",
                "session-lock-kms-epoch-displayed"
            );
            tracing::info!(
                presentation_epoch,
                generation,
                "session-lock-kms-unlock-epoch-displayed"
            );
            self.restore_unlocked_focus_and_input();
            tracing::info!("session-lock-kms-normal-exposure-restored");
        }
    }

    fn close_all_foreign_toplevels(&mut self) {
        for (_, handle) in self.foreign_toplevels.drain() {
            self.foreign_toplevel_list_state.remove_toplevel(&handle);
        }
    }

    fn deactivate_all_lock_surfaces(&mut self) {
        let surfaces = self
            .lock_surfaces_by_output
            .drain()
            .filter_map(|(_, object)| {
                self.surfaces
                    .get(&object)
                    .map(|record| record.role.wl_surface().clone())
            })
            .collect::<Vec<_>>();
        for surface in surfaces {
            self.deactivate_surface_role(&surface);
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_input_before_kms_unlock(&mut self) {
        let keys = self.keyboard.pressed_keys();
        let buttons = self.pointer.current_pressed();
        self.kms_session_lock_gate
            .quarantine_current_input(keys.iter().copied(), buttons.iter().copied());
        let held = input::SeatHeldState {
            keys: keys.clone(),
            buttons: buttons.clone(),
        };
        let releases = self.input_ingress.release_session_boundary(&held);
        for input in releases {
            self.handle_host_input_with_activity(input, false);
        }
        self.release_pressed_keys();
        let time = monotonic_millis();
        for button in self.pointer.current_pressed() {
            self.pointer_button(button, HostButtonState::Released, time);
        }
        self.teardown_input_for_session_lock();
        tracing::info!("session-lock-kms-lock-input-reconciled");
    }

    fn prepare_unlock_scene_transition(&mut self) {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        if matches!(self.backend, BackendData::Kms(_)) {
            self.reconcile_input_before_kms_unlock();
            // Establish the policy gate before changing the protocol lifecycle.
            // Surface deactivation re-hit-tests focus synchronously.
            self.kms_session_lock_gate.deferred_unlock = true;
        }
    }

    fn publish_unlocked_scene(
        &mut self,
        presentation_epoch: Option<u64>,
        presentation_outputs: Vec<String>,
    ) {
        self.events.push(ProtocolEvent::SecurityScene {
            active: false,
            presentation_epoch,
            presentations: presentation_outputs
                .into_iter()
                .map(|output| SecurityPresentationTarget {
                    output,
                    scene: SecurityPresentationScene::Client,
                })
                .collect(),
        });
        self.events.push(ProtocolEvent::SurfaceRoster {
            mapped: self.mapped_surface_ids().into_iter().collect(),
        });
        self.pending_full_upserts.extend(self.mapped_surface_ids());
    }

    fn restore_unlocked_focus_and_input(&mut self) {
        if self.session_lock_active() {
            return;
        }
        if let Some(selection) = self.saved_cursor_selection.take() {
            self.cursor_selection = selection;
            self.publish_current_cursor();
        }
        let toplevels = self
            .surfaces
            .values()
            .filter(|record| record.mapped && matches!(record.role, SurfaceRole::Toplevel(_)))
            .map(|record| record.role.wl_surface().clone())
            .collect::<Vec<_>>();
        for surface in toplevels {
            self.sync_foreign_toplevel(&surface);
        }
        self.arbitrate_keyboard_focus(None, true, false);
        self.retarget_pointer_after_visibility_change();
    }

    fn finish_unlock_scene_transition(&mut self) {
        if matches!(self.backend, BackendData::Kms(_)) {
            let Some(outputs) = self.kms_session_lock_gate.ready_outputs().cloned() else {
                self.kms_session_lock_gate.deferred_unlock = true;
                tracing::info!("session-lock-kms-unlock-deferred-authority-lost");
                return;
            };
            self.next_security_presentation_epoch =
                self.next_security_presentation_epoch.saturating_add(1);
            self.fail_stale_capture_epochs(self.next_security_presentation_epoch);
            let presentation_epoch = self.next_security_presentation_epoch;
            self.kms_session_lock_gate.deferred_unlock = false;
            self.kms_session_lock_gate.resume_barrier = None;
            self.kms_session_lock_gate.unlock_barrier = Some(KmsSecurityBarrier {
                purpose: KmsSecurityBarrierPurpose::UnlockRestore,
                presentation_epoch,
                pending_outputs: outputs.clone(),
            });
            let presentation_outputs = outputs
                .keys()
                .map(|output| output.connector_name.clone())
                .collect();
            self.publish_unlocked_scene(Some(presentation_epoch), presentation_outputs);
            tracing::info!(
                presentation_epoch,
                outputs = outputs.len(),
                "session-lock-kms-unlock-presentation-armed"
            );
            return;
        }

        self.next_security_presentation_epoch =
            self.next_security_presentation_epoch.saturating_add(1);
        self.fail_stale_capture_epochs(self.next_security_presentation_epoch);
        let presentation_epoch = self.next_security_presentation_epoch;
        let outputs = self
            .backend
            .default_output()
            .map(|output| vec![output.name()])
            .unwrap_or_default();
        self.publish_unlocked_scene(Some(presentation_epoch), outputs);
        self.restore_unlocked_focus_and_input();
    }

    fn leave_session_lock(&mut self) {
        if !matches!(self.lock_lifecycle, LockLifecycle::Locked { .. }) {
            return;
        }
        #[cfg(feature = "bus")]
        self.mark_session_observation_dirty();
        #[cfg(test)]
        {
            self.session_unlock_callbacks = self.session_unlock_callbacks.saturating_add(1);
        }
        self.prepare_unlock_scene_transition();
        self.lock_lifecycle = LockLifecycle::Unlocked;
        self.deactivate_all_lock_surfaces();
        self.finish_unlock_scene_transition();
        tracing::info!("session lock returned to Unlocked");
    }

    fn abort_locking_after_owner_death(&mut self, lock_resource: &ExtSessionLockV1) {
        #[cfg(feature = "bus")]
        self.mark_session_observation_dirty();
        self.session_lock_state.abort_lock_outputs(lock_resource);
        self.prepare_unlock_scene_transition();
        self.lock_lifecycle = LockLifecycle::Unlocked;
        self.deactivate_all_lock_surfaces();
        self.finish_unlock_scene_transition();
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn kms_authority_lost(&mut self) {
        self.reconcile_all_input_authority_loss();
        self.kms_session_lock_gate.authority_lost();
        tracing::info!(
            lock_state = match self.lock_lifecycle {
                LockLifecycle::Unlocked => "unlocked",
                LockLifecycle::Locking { .. } => "locking",
                LockLifecycle::Locked { .. } => "locked",
                LockLifecycle::OrphanedLocked { .. } => "orphaned-locked",
            },
            "session-lock-kms-authority-lost"
        );
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn retire_replaced_kms_lock_surfaces(
        &mut self,
        previous: &[(crate::backend::kms::OutputKey, Output)],
        current: &[(crate::backend::kms::OutputKey, Output)],
    ) {
        let current_keys = current.iter().map(|(key, _)| key).collect::<HashSet<_>>();
        let retired_outputs = previous
            .iter()
            .filter(|(key, _)| !current_keys.contains(key))
            .map(|(_, output)| output.clone())
            .collect::<Vec<_>>();
        for retired_output in retired_outputs {
            let lock_surface = self.surfaces.values().find_map(|record| {
                let SurfaceRole::LockSurface(role) = &record.role else {
                    return None;
                };
                (role.output == retired_output)
                    .then_some((role.surface.clone(), role.surface.wl_surface().clone()))
            });
            let Some((lock_surface, wl_surface)) = lock_surface else {
                continue;
            };
            self.session_lock_state.retire_lock_surface(&lock_surface);
            if self
                .lock_surfaces_by_output
                .get(&retired_output.name())
                .is_some_and(|object| object == &wl_surface.id())
            {
                self.lock_surfaces_by_output.remove(&retired_output.name());
            }
            self.deactivate_surface_role(&wl_surface);
            tracing::info!(
                output = retired_output.name(),
                "session-lock-kms-output-lock-surface-retired"
            );
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn kms_begin_preparing(
        &mut self,
        outputs: BTreeMap<crate::backend::kms::OutputKey, u64>,
        resumed: bool,
    ) {
        self.kms_session_lock_gate.begin_preparing(outputs.clone());
        if !self.lock_lifecycle.is_active() {
            return;
        }

        self.next_security_presentation_epoch =
            self.next_security_presentation_epoch.saturating_add(1);
        self.fail_stale_capture_epochs(self.next_security_presentation_epoch);
        let presentation_epoch = self.next_security_presentation_epoch;
        let presentations = outputs
            .keys()
            .map(|output| SecurityPresentationTarget {
                output: output.connector_name.clone(),
                scene: self.kms_lock_scene_for_output(&output.connector_name),
            })
            .collect::<Vec<_>>();
        let presentation_outputs = presentations
            .iter()
            .map(|presentation| presentation.output.clone())
            .collect::<Vec<_>>();
        match &mut self.lock_lifecycle {
            LockLifecycle::Locking {
                presentation_epoch: epoch,
                pending_outputs,
                pending_kms_outputs,
                ..
            } => {
                *epoch = presentation_epoch;
                *pending_outputs = presentation_outputs.iter().cloned().collect();
                *pending_kms_outputs = outputs.clone();
            }
            LockLifecycle::Locked { .. } | LockLifecycle::OrphanedLocked { .. } => {
                self.kms_session_lock_gate.resume_barrier = Some(KmsSecurityBarrier {
                    purpose: KmsSecurityBarrierPurpose::LockResume,
                    presentation_epoch,
                    pending_outputs: outputs.clone(),
                });
            }
            LockLifecycle::Unlocked => unreachable!("active lock checked above"),
        }
        self.events.push(ProtocolEvent::SecurityScene {
            active: true,
            presentation_epoch: Some(presentation_epoch),
            presentations,
        });
        self.events.push(ProtocolEvent::SurfaceRoster {
            mapped: self.mapped_surface_ids().into_iter().collect(),
        });
        tracing::info!(
            presentation_epoch,
            outputs = outputs.len(),
            resumed,
            "session-lock-kms-resume-blank-first"
        );
        tracing::info!(presentation_epoch, "session-lock-kms-normal-exposure-held");
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn kms_lock_scene_for_output(&self, output_name: &str) -> SecurityPresentationScene {
        let has_mapped_lock_surface = self
            .lock_surfaces_by_output
            .get(output_name)
            .and_then(|object| self.surfaces.get(object))
            .is_some_and(|record| {
                record.mapped && matches!(record.role, SurfaceRole::LockSurface(_))
            });
        if has_mapped_lock_surface {
            SecurityPresentationScene::Lock
        } else {
            SecurityPresentationScene::Blank
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn kms_output_ready(&mut self, generation: u64, output: &crate::backend::kms::OutputKey) {
        if !self.kms_session_lock_gate.output_ready(generation, output) {
            return;
        }
        tracing::info!(generation, "session-lock-kms-output-set-ready");
        if self.kms_session_lock_gate.deferred_unlock {
            self.finish_unlock_scene_transition();
        }
    }

    fn handle_client_disconnect(&mut self, client_id: &ClientId) {
        self.destroy_client_acquire_gates(client_id);
        let frames = self
            .capture_frames
            .iter()
            .filter_map(|(id, frame)| (&frame.client_id == client_id).then_some(*id))
            .collect::<Vec<_>>();
        for id in frames {
            self.destroy_screencopy_frame(id);
        }
        self.capture_managers
            .retain(|_, manager| &manager.client_id != client_id);
        #[cfg(feature = "bus")]
        if matches!(
            &self.lock_lifecycle,
            LockLifecycle::Locking { owner, .. } | LockLifecycle::Locked { owner, .. }
                if owner == client_id
        ) {
            self.mark_session_observation_dirty();
        }
        let lifecycle = mem::replace(&mut self.lock_lifecycle, LockLifecycle::Unlocked);
        match lifecycle {
            LockLifecycle::Locking {
                owner,
                lock_resource,
                ..
            } if &owner == client_id => {
                self.abort_locking_after_owner_death(&lock_resource);
                tracing::warn!("session-lock owner died before Locked; lock aborted");
            }
            LockLifecycle::Locked {
                owner, generation, ..
            } if &owner == client_id => {
                self.lock_lifecycle = LockLifecycle::OrphanedLocked { generation };
                self.deactivate_all_lock_surfaces();
                self.events
                    .push(ProtocolEvent::SurfaceRoster { mapped: Vec::new() });
                tracing::error!(
                    generation,
                    "session-lock owner died; remaining orphan-locked"
                );
            }
            other => self.lock_lifecycle = other,
        }
    }

    /// Record why the protocol run is ending, with runtime failure dominating.
    ///
    /// The calloop channel drains every queued command in a single dispatch, so
    /// a runtime failure and an orderly `Shutdown` can both be handled before
    /// `run()` next checks the flag. Last-write-wins would then label a lost KMS
    /// channel as an orderly shutdown, and every release point abandoned on the
    /// way out would be recorded under the wrong reason. A failure that has
    /// already happened is not undone by someone subsequently asking us to stop.
    fn request_shutdown(&mut self, cause: ProtocolShutdownCause) {
        if matches!(
            self.shutdown_cause,
            Some(ProtocolShutdownCause::RuntimeFailure)
        ) {
            return;
        }
        self.shutdown_cause = Some(cause);
    }

    fn withdraw_explicit_sync_global(&mut self, reason: &'static str) {
        let global = take_drm_syncobj_global(&mut self.drm_syncobj_state);
        self.explicit_sync_global_advertised = false;
        let Some(global) = global else {
            return;
        };

        #[cfg(test)]
        {
            self.explicit_sync_global_withdrawals += 1;
        }
        self.display_handle.disable_global::<WaylandState>(global);
        tracing::warn!(
            reason,
            explicit_sync_global_advertised = false,
            "withdrew explicit-sync protocol global after permanent fault"
        );
    }

    fn handle_retirement_report(&mut self, report: RetirementWorkerReport) {
        if self.release_uses.handle_retirement_event(report) {
            self.withdraw_explicit_sync_global("retirement report fault");
        }
    }

    fn handle_retirement_worker_closed(&mut self) {
        if self.release_uses.retirement_worker_closed() {
            self.withdraw_explicit_sync_global("retirement worker closed");
        }
    }

    fn handle_frame(&mut self, inputs: Vec<HostInput>) {
        for input in inputs {
            self.handle_host_input(input);
        }

        if matches!(self.backend, BackendData::Kms(_))
            && self.kms_session_lock_gate.client_delivery_blocked(
                self.session_lock_active(),
                matches!(self.lock_lifecycle, LockLifecycle::Locking { .. }),
            )
        {
            return;
        }

        let frame_time = monotonic_millis();
        let mut delivered = self
            .surfaces
            .values()
            .filter(|record| {
                !matches!(record.role, SurfaceRole::Dormant(_))
                    && self.surface_is_session_presentable(record)
                    && record.role.parent_surface().is_none()
                    && !self.surface_belongs_to_minimized_toplevel(record.role.wl_surface())
            })
            .map(|record| send_frames_surface_tree(record.role.wl_surface(), frame_time))
            .sum::<usize>();
        if let CursorSelection::Surface(id) = &self.cursor_selection
            && let Some(record) = self.cursor_surfaces.get(id)
            && record.presentation.is_some()
        {
            delivered += send_frames_surface_tree(&record.surface, frame_time);
        }
        if delivered > 0 {
            tracing::trace!(delivered, "completed Wayland frame callbacks");
        }
    }

    fn surface_belongs_to_minimized_toplevel(&self, surface: &WlSurface) -> bool {
        let root = canonical_root_surface(&self.popup_manager, surface);
        self.surfaces
            .get(&root.id())
            .is_some_and(|record| record.minimized)
    }

    /// The single seat-policy entry point, shared by both input transports.
    ///
    /// Nothing may reach the pointer, keyboard or output methods except through
    /// here. `protocol::input` converts bare-metal events into `HostInput` and
    /// calls this directly off the calloop; the nested backend calls it from
    /// `handle_frame`. See [`HostInput`].
    pub(crate) fn handle_host_input(&mut self, input: HostInput) {
        self.handle_host_input_with_activity(input, true);
    }

    fn handle_host_input_with_activity(&mut self, input: HostInput, user_activity: bool) {
        let exposure_sensitive = matches!(
            &input,
            HostInput::PointerMotionAbsolute { .. }
                | HostInput::PointerMotion { .. }
                | HostInput::PointerButton { .. }
                | HostInput::PointerAxis { .. }
                | HostInput::Key { .. }
                | HostInput::TouchDown { .. }
                | HostInput::TouchMotion { .. }
                | HostInput::TouchUp { .. }
                | HostInput::TouchFrame
                | HostInput::TouchCancel
        );
        if user_activity && exposure_sensitive {
            self.notify_idle_activity();
        }
        if user_activity && matches!(self.backend, BackendData::Kms(_)) {
            self.kms_session_lock_gate.observe_physical_touch(&input);
        }
        let kms_delivery_blocked = user_activity
            && exposure_sensitive
            && matches!(self.backend, BackendData::Kms(_))
            && self.kms_session_lock_gate.client_delivery_blocked(
                self.session_lock_active(),
                matches!(self.lock_lifecycle, LockLifecycle::Locking { .. }),
            );
        if kms_delivery_blocked
            && let HostInput::Key {
                keycode,
                state,
                time,
            } = input
        {
            self.kms_session_lock_gate.observe_blocked_input(&input);
            if !self.kms_session_lock_gate.input_hold_logged {
                self.kms_session_lock_gate.input_hold_logged = true;
                tracing::info!("session-lock-kms-input-held");
            }
            // Update XKB and evaluate the compositor-only Ctrl-Alt-Fn escape
            // route before the client-presentation gate swallows the key.
            // Every non-VT disposition is intercepted below and cannot reach
            // a client.
            self.keyboard_keycode_presentation_gated(keycode, state, time);
            return;
        }
        if kms_delivery_blocked {
            self.kms_session_lock_gate.observe_blocked_input(&input);
            if !self.kms_session_lock_gate.input_hold_logged {
                self.kms_session_lock_gate.input_hold_logged = true;
                tracing::info!("session-lock-kms-input-held");
            }
            return;
        }
        if user_activity
            && matches!(self.backend, BackendData::Kms(_))
            && self
                .kms_session_lock_gate
                .suppress_quarantined_input(&input)
        {
            if let HostInput::Key {
                keycode,
                state: HostButtonState::Released,
                time,
            } = input
            {
                // Quarantine is a client-delivery boundary, not an XKB
                // boundary. A press observed while the presentation gate was
                // closed already advanced Smithay's pressed/modifier state,
                // so its release must take the same intercepted path even if
                // the display barrier opened in between.
                self.keyboard_keycode_presentation_gated(keycode, HostButtonState::Released, time);
            }
            tracing::debug!("suppressed input carried across the KMS lock boundary");
            return;
        }
        match input {
            HostInput::PointerMotionAbsolute { x, y, time } => self.pointer_moved(x, y, time),
            HostInput::PointerMotion { dx, dy, time } => self.pointer_motion(dx, dy, time),
            HostInput::PointerLeave => {
                #[cfg(feature = "bus")]
                self.reset_corner_detector();
                let mut snapshot = self
                    .cursor_position_snapshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if snapshot.on_output {
                    snapshot.on_output = false;
                    snapshot.revision = snapshot.revision.saturating_add(1);
                }
            }
            HostInput::PointerButton {
                button,
                state,
                time,
            } => {
                self.pointer_button(button, state, time);
            }
            HostInput::PointerAxis {
                horizontal,
                vertical,
                source,
                relative_direction,
                time,
            } => self.pointer_axis(horizontal, vertical, source, relative_direction, time),
            HostInput::Key {
                keycode,
                state,
                time,
            } => self.keyboard_keycode(keycode, state, time),
            HostInput::KeyboardFocusLost => {
                self.cancel_chrome_pointer_grab(true);
                self.update_chrome_hover(None);
                self.set_chrome_cursor_override(None);
                self.release_pressed_keys();
            }
            HostInput::TouchDeviceAdded => self.add_touch_device(),
            HostInput::TouchDeviceRemoved => self.remove_touch_device(),
            HostInput::TouchDown { slot, x, y, time } => self.touch_down(slot, x, y, time),
            HostInput::TouchMotion { slot, x, y, time } => self.touch_motion(slot, x, y, time),
            HostInput::TouchUp { slot, time } => self.touch_up(slot, time),
            HostInput::TouchFrame => self.touch_frame(),
            HostInput::TouchCancel => self.cancel_touch(),
            HostInput::OutputResized { width, height } => {
                self.resize_output(width, height);
            }
            HostInput::OutputScaleChanged { scale } => {
                self.change_output_scale(scale);
            }
        }
    }

    fn notify_idle_activity(&mut self) {
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);
    }

    fn with_client_state<T>(
        surface: &WlSurface,
        function: impl FnOnce(&WaylandClientState) -> T,
    ) -> Option<T> {
        let client = surface.client()?;
        let state = client.get_data::<WaylandClientState>()?;
        Some(function(state))
    }

    fn sync_foreign_toplevel(&mut self, surface: &WlSurface) {
        if self.session_lock_active() {
            return;
        }
        let Some((id, commit_count, title, app_id)) =
            self.surfaces.get(&surface.id()).and_then(|record| {
                (record.mapped && record.role.managed_toplevel()).then(|| {
                    (
                        record.id,
                        record.commit_count,
                        record.title.as_deref().unwrap_or_default().to_owned(),
                        record.app_id.as_deref().unwrap_or_default().to_owned(),
                    )
                })
            })
        else {
            return;
        };
        if let Some(handle) = self.foreign_toplevels.get(&id) {
            let title_changed = handle.title() != title;
            let app_id_changed = handle.app_id() != app_id;
            if title_changed {
                handle.send_title(&title);
            }
            if app_id_changed {
                handle.send_app_id(&app_id);
            }
            if title_changed || app_id_changed {
                handle.send_done();
            }
            return;
        }

        // Exactly 32 printable ASCII bytes. XOR is injective for each half
        // within one compositor instance, while the getrandom nonce prevents
        // deterministic SurfaceId/commit counters being reused after restart.
        let instance_high = u64::from_ne_bytes(
            self.foreign_toplevel_nonce[..8]
                .try_into()
                .expect("nonce high half"),
        );
        let instance_low = u64::from_ne_bytes(
            self.foreign_toplevel_nonce[8..]
                .try_into()
                .expect("nonce low half"),
        );
        let identifier = self
            .foreign_toplevel_identifiers
            .entry(id)
            .or_insert_with(|| {
                format!(
                    "{:016x}{:016x}",
                    instance_high ^ id.0,
                    instance_low ^ commit_count
                )
            })
            .clone();
        let handle = self
            .foreign_toplevel_list_state
            .new_toplevel_with_identifier::<Self>(identifier, title, app_id);
        self.foreign_toplevels.insert(id, handle);
    }

    fn close_foreign_toplevel(&mut self, surface: &WlSurface) {
        let Some(id) = self.surfaces.get(&surface.id()).map(|record| record.id) else {
            return;
        };
        self.foreign_toplevel_identifiers.remove(&id);
        if let Some(handle) = self.foreign_toplevels.remove(&id) {
            self.foreign_toplevel_list_state.remove_toplevel(&handle);
        }
    }

    fn prepare_acquire_gate(&mut self, surface: &WlSurface) {
        #[cfg(test)]
        {
            self.acquire_gate_pre_commit_count =
                self.acquire_gate_pre_commit_count.saturating_add(1);
        }
        let Some(client) = surface.client() else {
            return;
        };
        let acquire_point = compositor::with_states(surface, |states| {
            let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
            cached.pending().acquire_point.clone()
        });
        let _ = self.acquire_gates.prepare_commit(
            client.id(),
            &client,
            surface.id(),
            surface.clone(),
            acquire_point,
        );
    }

    fn acquire_gate_source_ready(&mut self, gate_id: GateId) {
        if let Some(client_id) = self.acquire_gates.source_ready(gate_id) {
            self.wake_acquire_gate_client(client_id);
        }
    }

    fn destroy_client_acquire_gates(&mut self, client_id: &ClientId) {
        #[cfg(test)]
        {
            self.acquire_gate_client_destroyed_count =
                self.acquire_gate_client_destroyed_count.saturating_add(1);
        }
        let _ = self.acquire_gates.client_destroyed(client_id);
    }

    fn destroy_surface_acquire_gates(&mut self, surface: &WlSurface) {
        #[cfg(test)]
        {
            self.acquire_gate_surface_destroyed_count =
                self.acquire_gate_surface_destroyed_count.saturating_add(1);
            // Ordering witness: this runs last in `destroyed`, after
            // `destroy_surface_record` has refunded this surface's shm bytes and
            // dmabuf token and after `surface_count` was decremented. A gate
            // released earlier can wake a fused sibling into budgets that still
            // count the dying surface. `surface_count` is used rather than a
            // `surfaces` lookup because only role-bearing surfaces have a record,
            // which would make the check vacuous for a bare `wl_surface`.
            self.acquire_gate_destroy_observed_surface_count = Some(self.surface_count);
        }
        for client_id in self.acquire_gates.surface_destroyed(&surface.id()) {
            self.wake_acquire_gate_client(client_id);
        }
    }

    fn wake_acquire_gate_client(&mut self, client_id: ClientId) {
        let Ok(client_data) = self
            .display_handle
            .backend_handle()
            .get_client_data(client_id)
        else {
            return;
        };
        let display_handle = self.display_handle.clone();
        if let Some(client_state) = client_data.downcast_ref::<WaylandClientState>() {
            client_state
                .compositor_state
                .blocker_cleared(self, &display_handle);
            return;
        }
        #[cfg(feature = "xwayland")]
        if let Some(client_state) =
            client_data.downcast_ref::<smithay::xwayland::XWaylandClientData>()
        {
            client_state
                .compositor_state
                .blocker_cleared(self, &display_handle);
            return;
        }
        unreachable!("clients carry WaylandClientState or XWaylandClientData")
    }

    fn prepare_scene_commit(&self, surface: &WlSurface) {
        let refresh_ancestor_window_geometry = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Subsurface { .. }))
            && !compositor::is_sync_subsurface(surface);
        if refresh_ancestor_window_geometry {
            update_pending_scene_commit_state(surface, |state| {
                state.refresh_ancestor_window_geometry = true;
            });
        }
    }

    fn reject_resource_limit(&self, surface: &WlSurface, message: impl Into<String>) {
        let Some(client) = surface.client() else {
            return;
        };
        self.reject_client_resource_limit(&client, message);
    }

    fn reject_client_resource_limit(&self, client: &Client, message: impl Into<String>) {
        terminate_resource_exhausting_client(&self.display_handle, client, message);
    }

    fn proposed_subsurface_depth(&self, surface: &WlSurface, parent: &WlSurface) -> Option<usize> {
        let parent_depth = self
            .subsurface_topology
            .get(&parent.id())
            .map_or(0, |node| node.depth);
        let subtree_height = self
            .subsurface_topology
            .get(&surface.id())
            .map_or(0, |node| node.subtree_height);
        proposed_subsurface_depth(parent_depth, subtree_height)
    }

    fn set_subsurface_depths(&mut self, root: ObjectId, depth: usize) {
        let mut stack = vec![(root, depth)];
        while let Some((object, object_depth)) = stack.pop() {
            let children = {
                let Some(node) = self.subsurface_topology.get_mut(&object) else {
                    continue;
                };
                node.depth = object_depth;
                node.children.iter().cloned().collect::<Vec<_>>()
            };
            stack.extend(
                children
                    .into_iter()
                    .map(|child| (child, object_depth.saturating_add(1))),
            );
        }
    }

    fn recompute_subsurface_heights(&mut self, mut object: Option<ObjectId>) {
        let mut visited = HashSet::new();
        while let Some(current) = object {
            if !visited.insert(current.clone()) {
                break;
            }
            let (parent, children) = match self.subsurface_topology.get(&current) {
                Some(node) => (
                    node.parent.clone(),
                    node.children.iter().cloned().collect::<Vec<_>>(),
                ),
                None => break,
            };
            let height = children
                .iter()
                .filter_map(|child| self.subsurface_topology.get(child))
                .map(|child| child.subtree_height.saturating_add(1))
                .max()
                .unwrap_or(0);
            if let Some(node) = self.subsurface_topology.get_mut(&current) {
                node.subtree_height = height;
            }
            object = parent;
        }
    }

    fn detach_subsurface_topology(&mut self, surface: &WlSurface) {
        let object = surface.id();
        let old_parent = self
            .subsurface_topology
            .get_mut(&object)
            .and_then(|node| node.parent.take());
        if let Some(parent) = &old_parent
            && let Some(parent_node) = self.subsurface_topology.get_mut(parent)
        {
            parent_node.children.remove(&object);
        }
        self.set_subsurface_depths(object, 0);
        self.recompute_subsurface_heights(old_parent);
    }

    fn attach_subsurface_topology(&mut self, surface: &WlSurface, parent: &WlSurface) {
        self.detach_subsurface_topology(surface);
        let object = surface.id();
        let parent_object = parent.id();
        let depth = self
            .subsurface_topology
            .get(&parent_object)
            .map_or(1, |node| node.depth.saturating_add(1));
        self.subsurface_topology
            .entry(object.clone())
            .or_default()
            .parent = Some(parent_object.clone());
        self.subsurface_topology
            .entry(parent_object.clone())
            .or_default()
            .children
            .insert(object.clone());
        self.set_subsurface_depths(object, depth);
        self.recompute_subsurface_heights(Some(parent_object));
    }

    fn remove_subsurface_topology(&mut self, surface: &WlSurface) {
        self.detach_subsurface_topology(surface);
        let object = surface.id();
        let children = self
            .subsurface_topology
            .remove(&object)
            .map(|node| node.children)
            .unwrap_or_default();
        for child in children {
            if let Some(node) = self.subsurface_topology.get_mut(&child) {
                node.parent = None;
            }
            self.set_subsurface_depths(child, 0);
        }
    }

    fn update_layer_map<R>(
        &mut self,
        output: &Output,
        update: impl FnOnce(&mut LayerMap) -> R,
    ) -> R {
        #[cfg(feature = "bus")]
        self.mark_output_before_change(output, "layer.arrange");
        let output_origin = output.current_location();
        let (existing, geometry_before) = {
            let layer_map = layer_map_for_output(output);
            let existing = layer_map.layers().cloned().collect::<Vec<_>>();
            let geometry = existing
                .iter()
                .filter_map(|layer| {
                    layer_map
                        .layer_geometry(layer)
                        .map(|geometry| (layer.wl_surface().id(), geometry))
                })
                .collect::<HashMap<_, _>>();
            (existing, geometry)
        };
        let configured = existing
            .iter()
            .filter(|layer| {
                compositor::with_states(layer.wl_surface(), |states| {
                    let mut attributes = states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .expect("desktop layer owns protocol attributes")
                        .lock()
                        .expect("layer attributes lock");
                    if attributes.initial_configure_sent {
                        attributes.initial_configure_sent = false;
                        true
                    } else {
                        false
                    }
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let (result, arranged) = {
            let mut layer_map = layer_map_for_output(output);
            let result = update(&mut layer_map);
            let arranged = layer_map
                .layers()
                .filter_map(|layer| {
                    let geometry = layer_map.layer_geometry(layer)?;
                    Some((layer.clone(), geometry))
                })
                .collect::<Vec<_>>();
            (result, arranged)
        };
        let geometry_after = arranged
            .iter()
            .map(|(layer, geometry)| (layer.wl_surface().id(), *geometry))
            .collect::<HashMap<_, _>>();
        let geometry_changed = geometry_before != geometry_after;

        for layer in &configured {
            compositor::with_states(layer.wl_surface(), |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .expect("desktop layer owns protocol attributes")
                    .lock()
                    .expect("layer attributes lock")
                    .initial_configure_sent = true;
            });
        }

        for (layer, _) in &arranged {
            if configured.contains(layer)
                && let Some(serial) = layer.layer_surface().send_pending_configure()
            {
                self.record_layer_required_configure(
                    layer.wl_surface(),
                    serial,
                    "layer-map arrange",
                );
            }
        }

        let mut moved_roots = Vec::new();
        #[cfg(feature = "bus")]
        let mut observed_layers = Vec::new();
        for (layer, geometry) in arranged {
            let Some(record) = self.surfaces.get_mut(&layer.wl_surface().id()) else {
                continue;
            };
            if !matches!(record.role, SurfaceRole::Layer(_)) {
                continue;
            }
            #[cfg(feature = "bus")]
            observed_layers.push(record.id);
            let location = (
                (output_origin.x + geometry.loc.x) as f32,
                (output_origin.y + geometry.loc.y) as f32,
            );
            if (record.layout.x, record.layout.y) == location {
                continue;
            }
            let delta = (location.0 - record.layout.x, location.1 - record.layout.y);
            record.layout.x = location.0;
            record.layout.y = location.1;
            record.window_origin = location;
            moved_roots.push((record.id, delta));
            if record.mapped {
                self.events.push(ProtocolEvent::SurfaceRelayout {
                    id: record.id,
                    scene: record.scene_snapshot(),
                });
            }
        }
        for (root, delta) in moved_roots {
            self.shift_surface_descendants(root, delta);
        }
        #[cfg(feature = "bus")]
        for id in observed_layers {
            self.mark_surface_dirty(id, "layer.arrange");
        }
        if geometry_changed {
            self.invalidate_pointer_hit_test_geometry();
        }
        result
    }

    fn record_layer_required_configure(
        &mut self,
        surface: &WlSurface,
        serial: Serial,
        source: &'static str,
    ) {
        let protocol_state = compositor::with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<LayerSurfaceData>()
                .expect("desktop layer owns protocol attributes")
                .lock()
                .expect("layer attributes lock");
            (
                attributes.initial_configure_sent,
                attributes.configured,
                attributes.configure_serial,
            )
        });
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        if !matches!(record.role, SurfaceRole::Layer(_)) {
            return;
        }
        record.required_configure = Some(serial);
        debug_assert!(protocol_state.0);
        tracing::debug!(
            surface_id = record.id.0,
            surface = ?surface.id(),
            ?serial,
            source,
            smithay_initial_configure_sent = protocol_state.0,
            smithay_configured = protocol_state.1,
            smithay_acked = ?protocol_state.2,
            "sent layer configure and updated common gate"
        );
    }

    fn arrange_layer_output(&mut self, output: &Output) {
        if let Err((surface, error)) = self.validate_layer_output_arrangement(output, None) {
            self.post_invalid_layer_state(&surface, error);
            return;
        }
        self.update_layer_map(output, |layer_map| {
            layer_map.arrange();
        });
    }

    fn map_layer_on_output(&mut self, output: &Output, layer: &DesktopLayerSurface) -> bool {
        if let Err((surface, error)) = self.validate_layer_output_arrangement(output, Some(layer)) {
            self.post_invalid_layer_state(&surface, error);
            return false;
        }
        let configured = compositor::with_states(layer.wl_surface(), |states| {
            let mut attributes = states
                .data_map
                .get::<LayerSurfaceData>()
                .expect("desktop layer owns protocol attributes")
                .lock()
                .expect("layer attributes lock");
            let configured = attributes.initial_configure_sent;
            attributes.initial_configure_sent = false;
            configured
        });
        let result = self.update_layer_map(output, |layer_map| {
            if layer_map.layer_geometry(layer).is_none() {
                layer_map.map_layer(layer).is_ok()
            } else {
                layer_map.arrange();
                true
            }
        });
        if configured {
            compositor::with_states(layer.wl_surface(), |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .expect("desktop layer owns protocol attributes")
                    .lock()
                    .expect("layer attributes lock")
                    .initial_configure_sent = true;
            });
            if result && let Some(serial) = layer.layer_surface().send_pending_configure() {
                self.record_layer_required_configure(
                    layer.wl_surface(),
                    serial,
                    "mapped-layer arrange",
                );
            }
        }
        result
    }

    fn ensure_layer_mapped_and_arranged(&mut self, surface: &WlSurface) {
        let usable_before = self.usable_output_rect();
        let role = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::Layer(role) = &record.role else {
                return None;
            };
            Some((role.surface.clone(), role.output.output()?.clone()))
        });
        let Some((layer, output)) = role else {
            return;
        };
        let _ = self.map_layer_on_output(&output, &layer);
        if self.usable_output_rect() != usable_before {
            self.reconfigure_window_states_for_output();
        }
    }

    fn unmap_layer_from_output(&mut self, surface: &WlSurface) {
        let usable_before = self.usable_output_rect();
        let role = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::Layer(role) = &record.role else {
                return None;
            };
            Some((role.surface.clone(), role.output.output()?.clone()))
        });
        if let Some((layer, output)) = role {
            self.update_layer_map(&output, |layer_map| layer_map.unmap_layer(&layer));
        }
        if self.usable_output_rect() != usable_before {
            self.reconfigure_window_states_for_output();
        }
    }

    fn arrange_all_layer_outputs(&mut self) {
        let mut outputs = Vec::new();
        for record in self.surfaces.values() {
            if let SurfaceRole::Layer(role) = &record.role
                && let Some(output) = role.output.output()
                && !outputs.contains(output)
            {
                outputs.push(output.clone());
            }
        }
        for output in outputs {
            self.arrange_layer_output(&output);
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_layer_output_bindings(&mut self) {
        let default_output = self.backend.default_output();
        let transitions = self
            .surfaces
            .iter()
            .filter_map(|(object, record)| {
                let SurfaceRole::Layer(role) = &record.role else {
                    return None;
                };
                let transition = role.output.transition(default_output.as_ref(), |output| {
                    self.backend.output_is_registered(output)
                });
                let in_layer_map = role.output.output().is_some_and(|output| {
                    layer_map_for_output(output)
                        .layer_geometry(&role.surface)
                        .is_some()
                });
                (!matches!(transition, LayerOutputTransition::Keep)).then(|| {
                    (
                        object.clone(),
                        role.surface.clone(),
                        role.output.output().cloned(),
                        transition,
                        record.mapped,
                        in_layer_map,
                    )
                })
            })
            .collect::<Vec<_>>();

        let mut closed_surfaces = Vec::new();
        for (object, layer, old_output, transition, was_mapped, in_layer_map) in transitions {
            if let Some(old_output) = old_output {
                self.update_layer_map(&old_output, |layer_map| layer_map.unmap_layer(&layer));
            }
            match transition {
                LayerOutputTransition::Migrate(output) => {
                    if let Some(record) = self.surfaces.get_mut(&object)
                        && let SurfaceRole::Layer(role) = &mut record.role
                    {
                        role.output = LayerOutputBinding::Default(output.clone());
                    }
                    let mapped = !in_layer_map || self.map_layer_on_output(&output, &layer);
                    if !mapped {
                        if let Some(record) = self.surfaces.get_mut(&object)
                            && let SurfaceRole::Layer(role) = &mut record.role
                        {
                            role.output = LayerOutputBinding::Closed;
                        }
                        layer.layer_surface().send_close();
                        closed_surfaces.push(layer.wl_surface().clone());
                    }
                }
                LayerOutputTransition::Close => {
                    #[cfg(feature = "bus")]
                    self.mark_surface_unmapped(layer.wl_surface());
                    let unmapped = self.surfaces.get_mut(&object).and_then(|record| {
                        let SurfaceRole::Layer(role) = &mut record.role else {
                            return None;
                        };
                        role.output = LayerOutputBinding::Closed;
                        record.mapped = false;
                        was_mapped.then_some(record.id)
                    });
                    layer.layer_surface().send_close();
                    closed_surfaces.push(layer.wl_surface().clone());
                    if let Some(id) = unmapped {
                        self.events.push(ProtocolEvent::SurfaceUnmapped { id });
                    }
                }
                LayerOutputTransition::Keep => unreachable!("filtered above"),
            }
        }
        if !closed_surfaces.is_empty() {
            self.recompute_effective_visibility();
            for surface in closed_surfaces {
                self.clear_focus_for_surface(&surface);
            }
        }
    }

    fn layer_output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        self.surfaces
            .get(&surface.id())
            .and_then(|record| match &record.role {
                SurfaceRole::Layer(role) => role.output.output().cloned(),
                _ => None,
            })
    }

    fn layer_role_is_closed(&self, surface: &WlSurface) -> bool {
        self.surfaces.get(&surface.id()).is_some_and(|record| {
            matches!(
                record.role,
                SurfaceRole::Layer(LayerRole {
                    output: LayerOutputBinding::Closed,
                    ..
                })
            )
        })
    }

    fn sync_layer_stack_band(&mut self, surface: &WlSurface) {
        let Some(band) = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::Layer(role) = &record.role else {
                return None;
            };
            Some(StackBand::for_layer(role.surface.cached_state().layer))
        }) else {
            return;
        };
        if self.surfaces[&surface.id()].layout.z.band == band {
            return;
        }
        self.restack_role_tree(surface, band, "layer.arrange");
    }

    fn restack_role_tree(&mut self, surface: &WlSurface, band: StackBand, _cause: &'static str) {
        let Some(root_id) = record_root_id(&self.surfaces, &self.surface_objects, surface.id())
        else {
            return;
        };
        let objects = self
            .surfaces
            .keys()
            .filter(|object| {
                record_root_id(&self.surfaces, &self.surface_objects, (*object).clone())
                    == Some(root_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut groups = objects
            .iter()
            .map(|object| {
                let record = &self.surfaces[object];
                (record.layout.z, object.clone())
            })
            .collect::<Vec<_>>();
        groups.sort_unstable_by_key(|(key, _)| *key);
        let mut next_keys = HashMap::new();
        for (key, _) in &groups {
            next_keys
                .entry((key.band, key.sequence))
                .or_insert_with(|| self.allocate_stack_key(band));
        }
        #[cfg(feature = "bus")]
        let mut observed = Vec::new();
        for (old_key, object) in groups {
            let new_root = next_keys[&(old_key.band, old_key.sequence)];
            if let Some(record) = self.surfaces.get_mut(&object) {
                record.layout.z = SurfaceStackKey {
                    tree_index: old_key.tree_index,
                    ..new_root
                };
                if record.mapped {
                    self.events.push(ProtocolEvent::SurfaceRelayout {
                        id: record.id,
                        scene: record.scene_snapshot(),
                    });
                }
                #[cfg(feature = "bus")]
                observed.push(record.id);
            }
        }
        #[cfg(feature = "bus")]
        for id in observed {
            self.mark_surface_dirty(id, _cause);
        }
        #[cfg(feature = "bus")]
        self.mark_stack_dirty(_cause);
        self.refresh_committed_surface_stack(surface);
        self.invalidate_pointer_hit_test();
        // The comp scene is authoritative for cross-protocol ordering; every
        // restack is mirrored into the XWM stacking state.
        #[cfg(feature = "xwayland")]
        self.sync_xwm_stacking();
    }

    fn validate_layer_surface_state(&self, surface: &WlSurface) -> Result<(), String> {
        let (layer, output) = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| {
                let SurfaceRole::Layer(role) = &record.role else {
                    return None;
                };
                Some((role.surface.clone(), role.output.output()?.clone()))
            })
            .ok_or_else(|| "layer surface has no live output".to_string())?;
        self.validate_layer_output_arrangement(&output, Some(&layer))
            .map_err(|(_, error)| error)
    }

    fn validate_layer_output_arrangement(
        &self,
        output: &Output,
        include: Option<&DesktopLayerSurface>,
    ) -> Result<(), (WlSurface, String)> {
        #[derive(Clone, Copy)]
        struct CheckedRect {
            x: i64,
            y: i64,
            width: i64,
            height: i64,
        }

        fn checked_add(value: i64, delta: i64, field: &str) -> Result<i64, String> {
            value
                .checked_add(delta)
                .ok_or_else(|| format!("layer {field} arithmetic overflow"))
        }

        fn checked_sub(value: i64, delta: i64, field: &str) -> Result<i64, String> {
            value
                .checked_sub(delta)
                .ok_or_else(|| format!("layer {field} arithmetic overflow"))
        }

        fn validate_rect(rect: CheckedRect, label: &str) -> Result<(), String> {
            if rect.width < 0 || rect.height < 0 {
                return Err(format!("{label} has a negative arrangement area"));
            }
            let right = checked_add(rect.x, rect.width, "right edge")?;
            let bottom = checked_add(rect.y, rect.height, "bottom edge")?;
            if [rect.x, rect.y, rect.width, rect.height, right, bottom]
                .into_iter()
                .any(|value| value.abs() > MAX_LAYER_GEOMETRY_VALUE)
            {
                return Err(format!(
                    "{label} exceeds the layer geometry bound {MAX_LAYER_GEOMETRY_VALUE}"
                ));
            }
            Ok(())
        }

        fn validate_remaining_zone(rect: CheckedRect) -> Result<(), String> {
            if rect.width < 0 || rect.height < 0 {
                return Err("remaining layer zone has a negative arrangement area".into());
            }
            let right = checked_add(rect.x, rect.width, "right edge")?;
            let bottom = checked_add(rect.y, rect.height, "bottom edge")?;
            if [rect.x, rect.y, rect.width, rect.height, right, bottom]
                .into_iter()
                .any(|value| value.abs() > MAX_LAYER_GEOMETRY_VALUE)
            {
                return Err(format!(
                    "remaining layer zone exceeds the layer geometry bound {MAX_LAYER_GEOMETRY_VALUE}"
                ));
            }
            Ok(())
        }

        let mut layers = layer_map_for_output(output)
            .layers()
            .cloned()
            .collect::<Vec<_>>();
        if let Some(include) = include
            && !layers.contains(include)
        {
            layers.push(include.clone());
        }
        let Some(first_surface) = layers.first().map(|layer| layer.wl_surface().clone()) else {
            return Ok(());
        };
        let mode = output.current_mode().ok_or_else(|| {
            (
                first_surface.clone(),
                "layer output has no current mode".to_string(),
            )
        })?;
        let logical = mode
            .size
            .to_f64()
            .to_logical(output.current_scale().fractional_scale())
            .to_i32_round::<i32>();
        let logical = output.current_transform().transform_size(logical);
        let output_rect = CheckedRect {
            x: 0,
            y: 0,
            width: i64::from(logical.w),
            height: i64::from(logical.h),
        };
        validate_rect(output_rect, "layer output").map_err(|error| (first_surface, error))?;

        let mut zone = output_rect;
        let mut total_exclusive_zone = 0_i64;
        for layer in layers {
            let surface = layer.wl_surface().clone();
            let state = layer.cached_state();
            let result = (|| {
                let values = [
                    ("width", i64::from(state.size.w)),
                    ("height", i64::from(state.size.h)),
                    ("top margin", i64::from(state.margin.top)),
                    ("right margin", i64::from(state.margin.right)),
                    ("bottom margin", i64::from(state.margin.bottom)),
                    ("left margin", i64::from(state.margin.left)),
                ];
                if let Some((name, value)) = values
                    .into_iter()
                    .find(|(_, value)| value.abs() > MAX_LAYER_GEOMETRY_VALUE)
                {
                    return Err(format!(
                        "{name} {value} exceeds the layer geometry bound {MAX_LAYER_GEOMETRY_VALUE}"
                    ));
                }
                if state.size.w < 0 || state.size.h < 0 {
                    return Err("layer requested a negative size".into());
                }

                if let ExclusiveZone::Exclusive(amount) = state.exclusive_zone {
                    let amount = i64::from(amount);
                    if amount > MAX_LAYER_GEOMETRY_VALUE {
                        return Err(format!(
                            "exclusive zone {amount} exceeds the layer geometry bound {MAX_LAYER_GEOMETRY_VALUE}"
                        ));
                    }
                    total_exclusive_zone =
                        checked_add(total_exclusive_zone, amount, "cumulative exclusive zone")?;
                    if total_exclusive_zone > MAX_LAYER_GEOMETRY_VALUE {
                        return Err(format!(
                            "cumulative exclusive zone {total_exclusive_zone} exceeds the per-output bound {MAX_LAYER_GEOMETRY_VALUE}"
                        ));
                    }
                }

                let mut source = match state.exclusive_zone {
                    ExclusiveZone::Exclusive(_) | ExclusiveZone::Neutral => zone,
                    ExclusiveZone::DontCare => output_rect,
                };
                if state.anchor.contains(Anchor::LEFT) {
                    source.width =
                        checked_sub(source.width, i64::from(state.margin.left), "source width")?;
                }
                if state.anchor.contains(Anchor::RIGHT) {
                    source.width =
                        checked_sub(source.width, i64::from(state.margin.right), "source width")?;
                }
                if state.anchor.contains(Anchor::TOP) {
                    source.height =
                        checked_sub(source.height, i64::from(state.margin.top), "source height")?;
                }
                if state.anchor.contains(Anchor::BOTTOM) {
                    source.height = checked_sub(
                        source.height,
                        i64::from(state.margin.bottom),
                        "source height",
                    )?;
                }
                validate_rect(source, "layer source")?;

                let mut width = i64::from(state.size.w).min(source.width);
                let mut height = i64::from(state.size.h).min(source.height);
                if width == 0 {
                    width = source.width / 2;
                }
                if height == 0 {
                    height = source.height / 2;
                }
                if state.anchor.anchored_horizontally() {
                    width = source.width;
                }
                if state.anchor.anchored_vertically() {
                    height = source.height;
                }
                let x = if state.anchor.contains(Anchor::LEFT) {
                    checked_add(source.x, i64::from(state.margin.left), "location x")?
                } else if state.anchor.contains(Anchor::RIGHT) {
                    checked_add(
                        source.x,
                        checked_sub(source.width, width, "right anchored width")?,
                        "location x",
                    )?
                } else {
                    checked_add(
                        source.x,
                        checked_sub(source.width / 2, width / 2, "centred width")?,
                        "location x",
                    )?
                };
                let y = if state.anchor.contains(Anchor::TOP) {
                    checked_add(source.y, i64::from(state.margin.top), "location y")?
                } else if state.anchor.contains(Anchor::BOTTOM) {
                    checked_add(
                        source.y,
                        checked_sub(source.height, height, "bottom anchored height")?,
                        "location y",
                    )?
                } else {
                    checked_add(
                        source.y,
                        checked_sub(source.height / 2, height / 2, "centred height")?,
                        "location y",
                    )?
                };
                validate_rect(
                    CheckedRect {
                        x,
                        y,
                        width,
                        height,
                    },
                    "layer geometry",
                )?;

                if let ExclusiveZone::Exclusive(amount) = state.exclusive_zone {
                    let amount = i64::from(amount);
                    let anchor = state.anchor;
                    if anchor.contains(Anchor::TOP) && anchor.contains(Anchor::BOTTOM) {
                        zone.width = checked_sub(zone.width, amount, "remaining zone width")?;
                        if anchor.contains(Anchor::LEFT) {
                            let delta = checked_add(
                                amount,
                                i64::from(state.margin.left),
                                "left exclusive edge",
                            )?;
                            zone.x = checked_add(zone.x, delta, "remaining zone x")?;
                            zone.width = checked_sub(
                                zone.width,
                                i64::from(state.margin.left),
                                "remaining zone width",
                            )?;
                        }
                        if anchor.contains(Anchor::RIGHT) {
                            zone.width = checked_sub(
                                zone.width,
                                i64::from(state.margin.right),
                                "remaining zone width",
                            )?;
                        }
                    } else if anchor.contains(Anchor::LEFT) && anchor.contains(Anchor::RIGHT) {
                        zone.height = checked_sub(zone.height, amount, "remaining zone height")?;
                        if anchor.contains(Anchor::TOP) {
                            let delta = checked_add(
                                amount,
                                i64::from(state.margin.top),
                                "top exclusive edge",
                            )?;
                            zone.y = checked_add(zone.y, delta, "remaining zone y")?;
                            zone.height = checked_sub(
                                zone.height,
                                i64::from(state.margin.top),
                                "remaining zone height",
                            )?;
                        }
                        if anchor.contains(Anchor::BOTTOM) {
                            zone.height = checked_sub(
                                zone.height,
                                i64::from(state.margin.bottom),
                                "remaining zone height",
                            )?;
                        }
                    } else if anchor == Anchor::all() {
                        zone.width = 0;
                        zone.height = 0;
                    } else if anchor.contains(Anchor::LEFT) && !anchor.contains(Anchor::RIGHT) {
                        let delta = checked_add(
                            amount,
                            i64::from(state.margin.left),
                            "left exclusive edge",
                        )?;
                        zone.x = checked_add(zone.x, delta, "remaining zone x")?;
                        zone.width = checked_sub(zone.width, delta, "remaining zone width")?;
                    } else if anchor.contains(Anchor::TOP) && !anchor.contains(Anchor::BOTTOM) {
                        let delta =
                            checked_add(amount, i64::from(state.margin.top), "top exclusive edge")?;
                        zone.y = checked_add(zone.y, delta, "remaining zone y")?;
                        zone.height = checked_sub(zone.height, delta, "remaining zone height")?;
                    } else if anchor.contains(Anchor::RIGHT) && !anchor.contains(Anchor::LEFT) {
                        let delta = checked_add(
                            amount,
                            i64::from(state.margin.right),
                            "right exclusive edge",
                        )?;
                        zone.width = checked_sub(zone.width, delta, "remaining zone width")?;
                    } else if anchor.contains(Anchor::BOTTOM) && !anchor.contains(Anchor::TOP) {
                        let delta = checked_add(
                            amount,
                            i64::from(state.margin.bottom),
                            "bottom exclusive edge",
                        )?;
                        zone.height = checked_sub(zone.height, delta, "remaining zone height")?;
                    }
                    validate_remaining_zone(zone)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                return Err((surface, error));
            }
        }
        Ok(())
    }

    fn post_invalid_layer_state(&self, surface: &WlSurface, message: String) {
        if let Some(layer) = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::Layer(role) = &record.role else {
                return None;
            };
            Some(role.surface.layer_surface().clone())
        }) {
            layer
                .shell_surface()
                .post_error(zwlr_layer_surface_v1::Error::InvalidSize, message);
        }
    }

    fn apply_acked_layer_state(&mut self, surface: &WlSurface) {
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        if matches!(record.role, SurfaceRole::Layer(_))
            && configure_sequence_is_acked(record.required_configure, record.last_acked_configure)
            && let Some(size) = record.last_acked_size
        {
            record.configured_size = size;
        }
    }

    fn apply_acked_lock_state(&mut self, surface: &WlSurface) {
        let Some((width, height)) = self.surfaces.get(&surface.id()).and_then(|record| {
            matches!(record.role, SurfaceRole::LockSurface(_))
                .then_some(())
                .filter(|_| {
                    configure_sequence_is_acked(
                        record.required_configure,
                        record.last_acked_configure,
                    )
                })?;
            record.last_acked_size
        }) else {
            return;
        };
        let (x, y, _, _) = self.backend.logical_output_rect();
        let _changed_id = self.surfaces.get_mut(&surface.id()).and_then(|record| {
            let changed = record.configured_size != (width, height)
                || record.layout.x != x as f32
                || record.layout.y != y as f32
                || record.layout.width != width as f32
                || record.layout.height != height as f32;
            record.configured_size = (width, height);
            record.layout.x = x as f32;
            record.layout.y = y as f32;
            record.layout.width = width as f32;
            record.layout.height = height as f32;
            changed.then_some(record.id)
        });
        #[cfg(feature = "bus")]
        if let Some(id) = _changed_id {
            self.mark_surface_dirty(id, "output.geometry");
        }
    }

    fn reconfigure_lock_surfaces(&mut self) {
        if !self.session_lock_active() {
            return;
        }
        let (_, _, width, height) = self.backend.logical_output_rect();
        let surfaces = self
            .surfaces
            .values()
            .filter_map(|record| match &record.role {
                SurfaceRole::LockSurface(role) => Some(role.surface.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for lock in surfaces {
            lock.with_pending_state(|state| {
                state.size = Some((width, height).into());
            });
            let _ = self.send_lock_configure(lock.wl_surface());
        }
    }

    fn emit_xdg_configure(
        &mut self,
        surface: &WlSurface,
        request: XdgConfigureRequest,
    ) -> Option<Serial> {
        let target = self.surfaces.get(&surface.id()).and_then(|record| {
            let gate_open = match &request {
                XdgConfigureRequest::Initial => record.required_configure.is_none(),
                XdgConfigureRequest::Toplevel { .. }
                | XdgConfigureRequest::PopupReposition { .. } => {
                    record.required_configure.is_some()
                }
                XdgConfigureRequest::Lock => matches!(record.role, SurfaceRole::LockSurface(_)),
            };
            if !gate_open {
                return None;
            }
            match &record.role {
                SurfaceRole::Toplevel(toplevel) => {
                    Some(ConfigureTarget::Toplevel(toplevel.clone()))
                }
                SurfaceRole::Popup(popup) => Some(ConfigureTarget::Popup(popup.clone())),
                SurfaceRole::Layer(role) if role.output.output().is_some() => {
                    Some(ConfigureTarget::Layer(role.surface.clone()))
                }
                SurfaceRole::LockSurface(role) => Some(ConfigureTarget::Lock(role.surface.clone())),
                SurfaceRole::Layer(_)
                | SurfaceRole::Subsurface { .. }
                | SurfaceRole::Dormant(_) => None,
                // X11 has no xdg configure/ack cycle; its geometry authority
                // is `X11Surface::configure`, never a serial.
                #[cfg(feature = "xwayland")]
                SurfaceRole::X11(_) => None,
            }
        })?;

        let serial = match (target, &request) {
            (ConfigureTarget::Toplevel(toplevel), XdgConfigureRequest::Initial) => toplevel
                .send_pending_configure()
                .unwrap_or_else(|| toplevel.send_configure()),
            (ConfigureTarget::Toplevel(toplevel), XdgConfigureRequest::Toplevel { force }) => {
                if *force {
                    toplevel.send_configure()
                } else {
                    toplevel.send_pending_configure()?
                }
            }
            (ConfigureTarget::Popup(popup), XdgConfigureRequest::PopupReposition { token }) => {
                popup.send_repositioned(*token)
            }
            (ConfigureTarget::Popup(popup), XdgConfigureRequest::Initial) => {
                match popup.send_configure() {
                    Ok(serial) => serial,
                    Err(error) => {
                        tracing::warn!(
                            surface = ?surface.id(),
                            %error,
                            "failed to send initial popup configure"
                        );
                        popup.send_popup_done();
                        return None;
                    }
                }
            }
            (ConfigureTarget::Layer(layer), XdgConfigureRequest::Initial) => {
                layer.layer_surface().send_configure()
            }
            (ConfigureTarget::Lock(lock), XdgConfigureRequest::Lock) => {
                lock.send_configure_with_serial()?
            }
            (ConfigureTarget::Toplevel(_), XdgConfigureRequest::PopupReposition { .. })
            | (ConfigureTarget::Popup(_), XdgConfigureRequest::Toplevel { .. })
            | (ConfigureTarget::Layer(_), XdgConfigureRequest::Toplevel { .. })
            | (ConfigureTarget::Layer(_), XdgConfigureRequest::PopupReposition { .. })
            | (ConfigureTarget::Toplevel(_), XdgConfigureRequest::Lock)
            | (ConfigureTarget::Popup(_), XdgConfigureRequest::Lock)
            | (ConfigureTarget::Layer(_), XdgConfigureRequest::Lock)
            | (ConfigureTarget::Lock(_), XdgConfigureRequest::Initial)
            | (ConfigureTarget::Lock(_), XdgConfigureRequest::Toplevel { .. })
            | (ConfigureTarget::Lock(_), XdgConfigureRequest::PopupReposition { .. }) => {
                return None;
            }
        };
        if matches!(
            request,
            XdgConfigureRequest::Initial
                | XdgConfigureRequest::PopupReposition { .. }
                | XdgConfigureRequest::Lock
        ) {
            let is_layer = self
                .surfaces
                .get(&surface.id())
                .is_some_and(|record| matches!(record.role, SurfaceRole::Layer(_)));
            if is_layer {
                self.record_layer_required_configure(surface, serial, "initial empty commit");
            } else if let Some(record) = self.surfaces.get_mut(&surface.id()) {
                record.required_configure = Some(serial);
            }
        }
        if let Some(record) = self.surfaces.get_mut(&surface.id())
            && matches!(record.role, SurfaceRole::Toplevel(_))
            && let Some(state) = record.pending_window_state
        {
            record
                .configured_window_states
                .push(ConfigureWindowStateSnapshot { serial, state });
        }
        Some(serial)
    }

    fn send_initial_configure(&mut self, surface: &WlSurface) -> bool {
        let Some(serial) = self.emit_xdg_configure(surface, XdgConfigureRequest::Initial) else {
            return false;
        };
        tracing::debug!(
            surface = ?surface.id(),
            ?serial,
            "sent initial shell configure after empty commit"
        );
        true
    }

    fn send_pending_toplevel_configure(
        &mut self,
        surface: &WlSurface,
        force: bool,
    ) -> Option<Serial> {
        self.emit_xdg_configure(surface, XdgConfigureRequest::Toplevel { force })
    }

    fn send_lock_configure(&mut self, surface: &WlSurface) -> Option<Serial> {
        self.emit_xdg_configure(surface, XdgConfigureRequest::Lock)
    }

    fn configure_client_side_decoration(&mut self, surface: &ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
        let wl_surface = surface.wl_surface().clone();
        let _ = self.send_pending_toplevel_configure(&wl_surface, false);
    }

    fn configure_decoration(
        &mut self,
        surface: &ToplevelSurface,
        requested: Option<DecorationMode>,
    ) {
        if !self.decoration.enabled {
            self.configure_client_side_decoration(surface);
            return;
        }
        let mode = match requested {
            Some(DecorationMode::ClientSide) => DecorationMode::ClientSide,
            Some(DecorationMode::ServerSide) | None => DecorationMode::ServerSide,
            #[allow(unreachable_patterns)]
            Some(_) => unreachable!("request_mode only receives known protocol modes"),
        };
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        let wl_surface = surface.wl_surface().clone();
        let _ = self.send_pending_toplevel_configure(&wl_surface, true);
    }

    fn send_popup_repositioned(&mut self, surface: &WlSurface, token: u32) -> Option<Serial> {
        self.emit_xdg_configure(surface, XdgConfigureRequest::PopupReposition { token })
    }

    fn apply_acked_popup_reposition(&mut self, surface: &WlSurface) {
        let pending = self.surfaces.get_mut(&surface.id()).and_then(|record| {
            let pending = record.pending_popup_reposition.as_ref()?;
            configure_sequence_is_acked(Some(pending.serial), record.last_acked_configure)
                .then(|| record.pending_popup_reposition.take())
                .flatten()
        });
        let Some(pending) = pending else {
            return;
        };
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        record.layout = pending.layout;
        record.window_origin = pending.window_origin;
        record.configured_size = pending.configured_size;
        let id = record.id;
        self.events.push(ProtocolEvent::SurfaceRelayout {
            id,
            scene: record.scene_snapshot(),
        });
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(id, "wayland.map");
    }

    fn current_configure_sequence_is_acked(&self, surface: &WlSurface) -> bool {
        self.surfaces.get(&surface.id()).is_some_and(|record| {
            configure_sequence_is_acked(record.required_configure, record.last_acked_configure)
        })
    }

    fn ensure_current_configure_sequence_is_acked(&mut self, surface: &WlSurface) -> bool {
        if self.current_configure_sequence_is_acked(surface) {
            return true;
        }
        let target = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| match &record.role {
                SurfaceRole::Toplevel(toplevel) => {
                    Some(ConfigureTarget::Toplevel(toplevel.clone()))
                }
                SurfaceRole::Popup(popup) => Some(ConfigureTarget::Popup(popup.clone())),
                SurfaceRole::Layer(role) if role.output.output().is_some() => {
                    Some(ConfigureTarget::Layer(role.surface.clone()))
                }
                SurfaceRole::LockSurface(role) => Some(ConfigureTarget::Lock(role.surface.clone())),
                SurfaceRole::Layer(_)
                | SurfaceRole::Subsurface { .. }
                | SurfaceRole::Dormant(_) => None,
                #[cfg(feature = "xwayland")]
                SurfaceRole::X11(_) => None,
            });
        let Some(target) = target else {
            return true;
        };
        match target {
            ConfigureTarget::Toplevel(toplevel) => {
                set_xdg_configured(surface, false);
                let _ = toplevel.ensure_configured();
            }
            ConfigureTarget::Popup(popup) => {
                set_xdg_configured(surface, false);
                let _ = popup.ensure_configured();
            }
            ConfigureTarget::Layer(layer) => {
                let (configured, acked) = compositor::with_states(layer.wl_surface(), |states| {
                    let attributes = states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .expect("desktop layer owns protocol attributes")
                        .lock()
                        .expect("layer attributes lock");
                    (attributes.configured, attributes.configure_serial)
                });
                if !configured {
                    let _ = layer.layer_surface().ensure_configured();
                } else {
                    let gate = self.surfaces.get(&surface.id()).map(|record| {
                        (
                            record.id,
                            record.required_configure,
                            record.last_acked_configure,
                        )
                    });
                    if let Some((surface_id, required_configure, last_acked_configure)) = gate {
                        tracing::debug!(
                            surface_id = surface_id.0,
                            surface = ?surface.id(),
                            ?required_configure,
                            ?last_acked_configure,
                            smithay_configured = configured,
                            smithay_acked = ?acked,
                            "refused layer buffer pending the newest configure acknowledgement"
                        );
                    }
                }
            }
            ConfigureTarget::Lock(_) => {}
        }
        false
    }

    fn reset_configure_sequence(&mut self, surface: &WlSurface) {
        let initial_layer = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::Layer(role) = &record.role else {
                return None;
            };
            Some(role.initial_layer)
        });
        let target = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| match &record.role {
                SurfaceRole::Toplevel(toplevel) => {
                    Some(ConfigureTarget::Toplevel(toplevel.clone()))
                }
                SurfaceRole::Popup(popup) => Some(ConfigureTarget::Popup(popup.clone())),
                SurfaceRole::Layer(role) => Some(ConfigureTarget::Layer(role.surface.clone())),
                SurfaceRole::LockSurface(role) => Some(ConfigureTarget::Lock(role.surface.clone())),
                SurfaceRole::Subsurface { .. } | SurfaceRole::Dormant(_) => None,
                #[cfg(feature = "xwayland")]
                SurfaceRole::X11(_) => None,
            });
        let reset_xdg_protocol_state = match target {
            Some(ConfigureTarget::Toplevel(toplevel)) => {
                toplevel.reset_initial_configure_sent();
                true
            }
            Some(ConfigureTarget::Popup(popup)) => {
                popup.reset_initial_configure_sent();
                true
            }
            Some(ConfigureTarget::Layer(layer)) => {
                layer
                    .layer_surface()
                    .reset_after_unmap(initial_layer.expect("layer configure target owns role"));
                false
            }
            Some(ConfigureTarget::Lock(_)) => false,
            None => return,
        };
        if reset_xdg_protocol_state {
            set_xdg_configured(surface, false);
        }
        if let Some(record) = self.surfaces.get_mut(&surface.id()) {
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.configured_window_states.clear();
            record.pending_popup_reposition = None;
        }
    }

    /// Dismiss every xdg-popup branch rooted at `surface` and retire the
    /// compositor-side mappings for those popups.
    ///
    /// Smithay owns the popup protocol tree and recursively emits
    /// `popup_done`; cosmix owns the scene records and buffer lifetimes. Both
    /// halves must be unwound together or remapping a layer parent can reveal
    /// popup content from the parent's previous map cycle.
    fn dismiss_popup_descendants(&mut self, surface: &WlSurface) {
        let Some(root_id) = self.surfaces.get(&surface.id()).map(|record| record.id) else {
            return;
        };
        let popup_objects = self
            .surfaces
            .iter()
            .filter_map(|(object, record)| {
                matches!(record.role, SurfaceRole::Popup(_))
                    .then(|| record_root_id(&self.surfaces, &self.surface_objects, object.clone()))
                    .flatten()
                    .filter(|candidate| *candidate == root_id)
                    .map(|_| object.clone())
            })
            .collect::<Vec<_>>();
        let tracked_popups = PopupManager::popups_for_surface(surface)
            .map(|(popup, _)| popup)
            .collect::<Vec<_>>();
        for popup in tracked_popups {
            let _ = PopupManager::dismiss_popup(surface, &popup);
        }
        if popup_objects.is_empty() {
            return;
        }

        let mut retired = Vec::new();
        for object in popup_objects {
            let Some(popup_surface) =
                self.surfaces
                    .get(&object)
                    .and_then(|record| match &record.role {
                        SurfaceRole::Popup(popup) => Some(popup.wl_surface().clone()),
                        _ => None,
                    })
            else {
                continue;
            };
            let was_mapped = self
                .surfaces
                .get(&object)
                .is_some_and(|record| record.mapped);
            #[cfg(feature = "bus")]
            self.mark_surface_unmapped(&popup_surface);
            self.reset_configure_sequence(&popup_surface);
            let Some(record) = self.surfaces.get_mut(&object) else {
                continue;
            };
            let released_shm_bytes = record
                .shm_backing
                .take()
                .map_or(0, |backing| backing.rgba.len());
            let released_dmabuf_token = record
                .dmabuf_backing
                .take()
                .map(|backing| backing.retention_token);
            record.buffer_dimensions = None;
            record.minimized = false;
            record.mapped = false;
            let unmapped = was_mapped.then_some(record.id);
            retired.push((
                popup_surface,
                unmapped,
                released_shm_bytes,
                released_dmabuf_token,
            ));
        }

        for (popup_surface, _, released_shm_bytes, released_dmabuf_token) in &retired {
            if *released_shm_bytes > 0 {
                self.release_shm_bytes(popup_surface, *released_shm_bytes);
            }
            if let Some(token) = released_dmabuf_token {
                self.release_buffer_token(*token);
            }
        }
        self.recompute_effective_visibility();
        for (popup_surface, unmapped, _, _) in retired {
            self.clear_focus_for_surface(&popup_surface);
            if let Some(id) = unmapped {
                self.events.push(ProtocolEvent::SurfaceUnmapped { id });
            }
        }
        self.refresh_chrome_pointer_after_scene_change();
    }

    fn recompute_effective_visibility(&mut self) {
        let children = child_surface_ids(&self.surfaces);
        let roots = self
            .surfaces
            .values()
            .filter_map(|record| match record.layout.parent {
                None => Some((record.id, true)),
                Some(parent) if !self.surface_objects.contains_key(&parent) => {
                    Some((record.id, false))
                }
                Some(_) => None,
            })
            .collect::<Vec<_>>();
        let mut stack = roots;
        let mut output_changes = Vec::new();
        #[cfg(feature = "bus")]
        let mut observed = Vec::new();
        while let Some((id, ancestor_visible)) = stack.pop() {
            let Some(object) = self.surface_objects.get(&id).cloned() else {
                continue;
            };
            let Some(record) = self.surfaces.get_mut(&object) else {
                continue;
            };
            let association_visible = match record.role {
                SurfaceRole::Subsurface { .. } => record.parent_association_committed,
                SurfaceRole::Dormant(_) => false,
                SurfaceRole::Toplevel(_)
                | SurfaceRole::Popup(_)
                | SurfaceRole::Layer(_)
                | SurfaceRole::LockSurface(_) => true,
                // The X11 association is established before the record exists
                // (`surface_associated` creates it), so it is always current.
                #[cfg(feature = "xwayland")]
                SurfaceRole::X11(_) => true,
            };
            let visible = effectively_visible(
                record.mapped && !record.minimized,
                ancestor_visible,
                association_visible,
            );
            if record.layout.visible != visible {
                record.layout.visible = visible;
                output_changes.push((record.role.wl_surface().clone(), visible));
                self.events.push(ProtocolEvent::SurfaceRelayout {
                    id: record.id,
                    scene: record.scene_snapshot(),
                });
                #[cfg(feature = "bus")]
                observed.push(record.id);
            }
            if let Some(descendants) = children.get(&id) {
                stack.extend(descendants.iter().copied().map(|child| (child, visible)));
            }
        }
        #[cfg(feature = "bus")]
        for id in observed {
            self.mark_surface_dirty(id, "wayland.map");
        }
        for (surface, visible) in output_changes {
            if visible {
                self.backend.output_enter(&surface);
            } else {
                self.backend.output_leave(&surface);
                self.clear_focus_for_surface(&surface);
            }
        }
    }

    fn commit_subsurface_stack(&mut self, parent: &WlSurface) -> bool {
        if !self.surfaces.contains_key(&parent.id()) {
            return false;
        }
        let mut stack = Vec::new();
        with_surface_tree_upward(
            parent,
            true,
            |_, _, is_parent| {
                if *is_parent {
                    TraversalAction::DoChildren(false)
                } else {
                    TraversalAction::SkipChildren
                }
            },
            |surface, _, _| {
                if self.surfaces.contains_key(&surface.id()) {
                    stack.push(surface.id());
                }
            },
            |_, _, _| true,
        );
        if !stack.iter().any(|id| *id == parent.id()) {
            stack.push(parent.id());
        }
        self.committed_surface_stacks.insert(parent.id(), stack);

        let children = compositor::get_children(parent);
        let mut any_remapped = false;
        for child in children {
            #[cfg(feature = "bus")]
            let remap_id = self
                .surfaces
                .get(&child.id())
                .filter(|record| {
                    matches!(record.role, SurfaceRole::Subsurface { .. })
                        && !record.parent_association_committed
                        && !record.mapped
                        && (record.shm_backing.is_some() || record.dmabuf_backing.is_some())
                })
                .map(|record| record.id);
            #[cfg(feature = "bus")]
            if let Some(id) = remap_id {
                self.mark_surface_before_change(id);
            }
            #[cfg(feature = "bus")]
            let mut remapped_id = None;
            if let Some(record) = self.surfaces.get_mut(&child.id())
                && matches!(record.role, SurfaceRole::Subsurface { .. })
            {
                // A subsurface re-created on a `wl_surface` whose buffer is
                // still applied comes back on *this* commit, from content it
                // already has. `wl_subcompositor.get_subsurface` is
                // double-buffered on the parent, so `new_subsurface` correctly
                // unmapped it and published the removal; the parent applying
                // the new association is what makes it presentable again, and
                // the client owes no further buffer for it. Left out, such a
                // surface stays missing until the client happens to commit
                // one — which a compliant client need never do.
                let remapped = !record.parent_association_committed
                    && !record.mapped
                    && (record.shm_backing.is_some() || record.dmabuf_backing.is_some());
                record.parent_association_committed = true;
                if remapped {
                    record.mapped = true;
                    any_remapped = true;
                    let id = record.id;
                    self.pending_full_upserts.insert(id);
                    #[cfg(feature = "bus")]
                    {
                        remapped_id = Some(id);
                    }
                }
            }
            #[cfg(feature = "bus")]
            if let Some(id) = remapped_id {
                self.mark_surface_dirty(id, "wayland.map");
            }
        }
        self.refresh_committed_surface_stack(parent);
        any_remapped
    }

    fn allocate_stack_key(&mut self, band: StackBand) -> SurfaceStackKey {
        let index = band.index();
        if self.next_stack_sequences[index].checked_add(1).is_none() {
            self.renormalize_stack_band(band);
        }
        let sequence = self.next_stack_sequences[index]
            .checked_add(1)
            .expect("dense stack renormalisation leaves room for one sequence");
        self.next_stack_sequences[index] = sequence;
        SurfaceStackKey::root(band, sequence)
    }

    fn renormalize_stack_band(&mut self, band: StackBand) {
        let mut sequences = self
            .surfaces
            .values()
            .filter(|record| record.layout.z.band == band)
            .map(|record| record.layout.z.sequence)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        sequences.dedup();
        let dense = sequences
            .into_iter()
            .enumerate()
            .map(|(index, sequence)| {
                (
                    sequence,
                    u64::try_from(index + 1).expect("surface bound fits u64"),
                )
            })
            .collect::<HashMap<_, _>>();
        #[cfg(feature = "bus")]
        let mut observed = Vec::new();
        for record in self
            .surfaces
            .values_mut()
            .filter(|record| record.layout.z.band == band)
        {
            let sequence = dense[&record.layout.z.sequence];
            if record.layout.z.sequence != sequence {
                record.layout.z.sequence = sequence;
                #[cfg(feature = "bus")]
                observed.push(record.id);
                if record.mapped {
                    self.events.push(ProtocolEvent::SurfaceRelayout {
                        id: record.id,
                        scene: record.scene_snapshot(),
                    });
                }
            }
        }
        #[cfg(feature = "bus")]
        let observed_stack_change = !observed.is_empty();
        #[cfg(feature = "bus")]
        for id in observed {
            self.mark_surface_dirty(id, "wayland.map");
        }
        #[cfg(feature = "bus")]
        if observed_stack_change {
            self.mark_stack_dirty("wayland.map");
        }
        self.next_stack_sequences[band.index()] =
            u64::try_from(dense.len()).expect("surface bound fits u64");
    }

    fn refresh_committed_surface_stack(&mut self, surface: &WlSurface) {
        let Some(root_id) = record_root_id(&self.surfaces, &self.surface_objects, surface.id())
        else {
            return;
        };
        let Some(root_object) = self.surface_objects.get(&root_id).cloned() else {
            return;
        };
        let Some(base) = self
            .surfaces
            .get(&root_object)
            .map(|record| record.layout.z)
        else {
            return;
        };
        enum StackTask {
            Expand(ObjectId),
            Emit(ObjectId),
        }
        let mut ordered = Vec::new();
        let mut pending = vec![StackTask::Expand(root_object)];
        while let Some(task) = pending.pop() {
            let StackTask::Expand(object) = task else {
                let StackTask::Emit(object) = task else {
                    unreachable!();
                };
                ordered.push(object);
                continue;
            };
            let stack = self
                .committed_surface_stacks
                .get(&object)
                .cloned()
                .unwrap_or_else(|| vec![object.clone()]);
            for entry in stack.into_iter().rev() {
                if entry == object {
                    pending.push(StackTask::Emit(entry));
                } else {
                    pending.push(StackTask::Expand(entry));
                }
            }
        }
        let mut stack_changed = false;
        #[cfg(feature = "bus")]
        let mut observed = Vec::new();
        for (index, object) in ordered.into_iter().enumerate() {
            let Some(record) = self.surfaces.get_mut(&object) else {
                continue;
            };
            let z = SurfaceStackKey {
                tree_index: u32::try_from(index).expect("surface tree bound fits u32"),
                ..base
            };
            if record.layout.z != z {
                stack_changed = true;
                record.layout.z = z;
                #[cfg(feature = "bus")]
                observed.push(record.id);
                if record.mapped {
                    self.events.push(ProtocolEvent::SurfaceRelayout {
                        id: record.id,
                        scene: record.scene_snapshot(),
                    });
                }
            }
        }
        #[cfg(feature = "bus")]
        for id in observed {
            self.mark_surface_dirty(id, "wayland.map");
        }
        #[cfg(feature = "bus")]
        if stack_changed {
            self.mark_stack_dirty("wayland.map");
        }
        if stack_changed {
            self.invalidate_pointer_hit_test();
        }
    }

    fn clear_focus_for_surface(&mut self, surface: &WlSurface) {
        let clears_keyboard =
            focus_targets_surface(self.keyboard.current_focus().as_ref(), surface)
                || self.exclusive_keyboard_focus.as_ref() == Some(&surface.id());
        if clears_keyboard {
            self.arbitrate_keyboard_focus(None, false, true);
        }

        let pointer_focus_matches =
            focus_targets_surface(self.pointer.current_focus().as_ref(), surface);
        let pointer_grab_matches = self
            .pointer
            .grab_start_data()
            .and_then(|start| start.focus)
            .is_some_and(|(focus, _)| {
                focus
                    .surface()
                    .is_some_and(|focused| focused.as_ref() == surface)
            });
        if pointer_focus_matches || pointer_grab_matches {
            let pointer = self.pointer.clone();
            if self.pointer_hit_test_reconciliation_deferred() {
                if pointer_grab_matches {
                    self.pointer_grab_teardown_deferred = true;
                }
                self.mark_pointer_hit_test_dirty();
                return;
            }
            if pointer_grab_matches {
                pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), monotonic_millis());
                tracing::debug!(
                    surface = ?surface.id(),
                    "cancelled pointer grab for unmapped surface"
                );
            }
            let (x, y) = self.cursor_position;
            let replacement = self.surface_at(x, y).map(|record| {
                (
                    self.seat_focus_target_for(record.role.wl_surface()),
                    (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
                )
            });
            let pointer = self.pointer.clone();
            pointer.motion(
                self,
                replacement.clone(),
                &MotionEvent {
                    location: (x, y).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: monotonic_millis(),
                },
            );
            pointer.frame(self);
            self.record_pointer_focus_local_position(replacement.as_ref(), (x, y));
        }
    }

    fn deactivate_surface_role(&mut self, surface: &WlSurface) {
        let was_mapped_before = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| record.mapped);
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface);
        self.close_foreign_toplevel(surface);
        self.cancel_chrome_pointer_grab_for_surface(surface, false);
        self.reset_chrome_pointer_tracking(&surface.id());
        self.minimized_toplevels
            .retain(|object| *object != surface.id());
        if self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Layer(_)))
        {
            self.dismiss_popup_descendants(surface);
        }
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        if matches!(record.role, SurfaceRole::Dormant(_)) {
            return;
        }
        // Whether the renderer can be holding an entity for this surface. A
        // surface the compositor called mapped is one it may have published a
        // complete upsert for, so going dormant has to be *said*, below.
        let was_mapped = was_mapped_before;
        let id = record.id;
        record.role = SurfaceRole::Dormant(surface.clone());
        record.required_configure = None;
        record.last_acked_configure = None;
        record.last_acked_size = None;
        record.configured_window_states.clear();
        record.pending_window_state = None;
        record.pending_popup_reposition = None;
        record.minimized = false;
        record.mapped = false;
        record.parent_association_committed = false;
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| interactive == surface)
        {
            self.interactive_pointer = None;
        }
        // Visibility before focus, and the order is load-bearing.
        // `clear_focus_for_surface` re-hit-tests the pointer through
        // `surface_at`, which selects on `layout.visible` — a field this
        // function has *not* yet updated. Called first it therefore reads this
        // record's stale `visible`, picks the surface just made dormant, and
        // delivers it a `wl_pointer.motion` naming coordinates inside a surface
        // the compositor has already declared gone. The visibility pass then
        // flips it invisible and calls `clear_focus_for_surface` again (see the
        // `output_changes` loop), which is what actually reached the surface
        // underneath. Running the pass first collapses that into the one
        // transition it always should have been, hit-tested against current
        // state. The explicit call below is still needed: a surface that was
        // already invisible produces no visibility change and so is never
        // reached by that loop.
        self.recompute_effective_visibility();
        self.clear_focus_for_surface(surface);
        // Destroying a role object is a removal, and it has to be published as
        // one. Nothing else here says so: `recompute_effective_visibility`
        // emits a relayout only when the surface's *visibility* changes, which
        // it does not for a surface that was already invisible — an offscreen
        // subsurface, or any surface under an unmapped ancestor — and a
        // relayout is not a removal in any case. Without this the renderer went
        // on holding an entity for a surface every predicate here now calls
        // gone: `mapped_surface_ids` omits it, `latest_surface_upsert` answers
        // `Gone` for it, and a roster is installed only when some *other* event
        // is rejected. It is published after the visibility pass so it is the
        // newest word about this id, and it is the last word: a re-created role
        // stays unmapped until a buffer commit publishes a complete upsert.
        if was_mapped {
            self.events.push(ProtocolEvent::SurfaceUnmapped { id });
        }
        tracing::debug!(
            surface = ?surface.id(),
            "retained wl_surface content while its role is dormant"
        );
    }

    fn destroy_surface_record(&mut self, surface: &WlSurface) {
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface);
        if let Some(output) = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::LockSurface(role) = &record.role else {
                return None;
            };
            Some(role.output.name())
        }) {
            self.lock_surfaces_by_output.remove(&output);
        }
        self.close_foreign_toplevel(surface);
        self.cancel_chrome_pointer_grab_for_surface(surface, false);
        self.reset_chrome_pointer_tracking(&surface.id());
        self.minimized_toplevels
            .retain(|object| *object != surface.id());
        if self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Layer(_)))
        {
            self.dismiss_popup_descendants(surface);
        }
        let Some(record) = self.surfaces.remove(&surface.id()) else {
            return;
        };
        // Whichever side dies first — DestroyNotify or the wl_surface — the
        // XID maps must not keep pointing at a removed record.
        #[cfg(feature = "xwayland")]
        if let SurfaceRole::X11(role) = &record.role {
            self.xwayland.surfaces_by_xid.remove(&role.xid);
            self.xwayland.xids_by_object.remove(&surface.id());
        }
        self.surface_objects.remove(&record.id);
        self.foreign_toplevel_identifiers.remove(&record.id);
        if record.layout.visible {
            self.backend.output_leave(surface);
        }
        if let Some(backing) = record.shm_backing.as_ref() {
            self.release_shm_bytes(surface, backing.rgba.len());
        }
        if let Some(backing) = record.dmabuf_backing.as_ref() {
            self.release_buffer_token(backing.retention_token);
        }
        self.committed_surface_stacks.remove(&surface.id());
        for stack in self.committed_surface_stacks.values_mut() {
            stack.retain(|object| *object != surface.id());
        }
        if interactive_surface(self.interactive_pointer.as_ref())
            .is_some_and(|interactive| interactive == surface)
        {
            self.interactive_pointer = None;
        }
        // Visibility before focus, for the same reason as `deactivate_surface_role`
        // and one more besides. Removing this record above stops `surface_at`
        // selecting *this* surface, but says nothing about its **descendants**:
        // `remove_subsurface_topology` clears the `subsurface_topology` links and
        // never touches `record.layout.parent` or `record.layout.visible`, so an
        // orphaned child keeps `visible == true` until the pass below notices its
        // parent has gone from `surface_objects`. Clearing focus first therefore
        // re-hit-tests against that stale flag and hands the client a
        // `wl_pointer.enter` naming a subsurface the compositor is about to hide
        // for having no parent — followed by an immediate `leave` when the pass
        // does hide it. Running the pass first means the one re-hit-test sees
        // current state and lands on whatever is genuinely underneath.
        self.recompute_effective_visibility();
        self.clear_focus_for_surface(surface);
        tracing::info!(surface_id = record.id.0, "Wayland surface destroyed");
        // Published after the visibility pass so it is the newest word about this
        // id, matching `deactivate_surface_role`.
        self.events
            .push(ProtocolEvent::SurfaceDestroyed { id: record.id });
    }

    fn set_cursor_image(&mut self, image: CursorImageStatus) {
        if self.chrome_cursor_override.is_some() && matches!(&image, CursorImageStatus::Named(_)) {
            self.publish_current_cursor();
            return;
        }
        self.cursor_selection = match image {
            CursorImageStatus::Hidden => CursorSelection::Hidden,
            CursorImageStatus::Named(_) => CursorSelection::Default,
            CursorImageStatus::Surface(surface) => {
                let id = surface.id();
                let hotspot = cursor_surface_hotspot(&surface);
                let adopt_committed_state = !self.cursor_surfaces.contains_key(&id);
                self.cursor_surfaces
                    .entry(id.clone())
                    .and_modify(|record| record.hotspot = hotspot)
                    .or_insert(CursorSurfaceRecord {
                        surface: surface.clone(),
                        hotspot,
                        shm_backing: None,
                        dmabuf_backing: None,
                        buffer_dimensions: None,
                        presentation: None,
                    });
                if adopt_committed_state {
                    // Smithay retains roleless commits in SurfaceAttributes::current.
                    // Consume the committed buffer now that set_cursor has supplied
                    // the role. Roleless commits discard damage because this first
                    // import builds a complete backing. Frame callbacks deliberately
                    // stay there: finish_frame drains them only after this image is
                    // presented.
                    let (buffer, damage, buffer_scale, buffer_transform) =
                        compositor::with_states(&surface, |states| {
                            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                            let current = attributes.current();
                            let buffer = current.buffer.take();
                            let damage = mem::take(&mut current.damage);
                            // The offset was committed before this was a pointer
                            // surface. The fresh set_cursor hotspot is already in
                            // the resulting surface coordinate system, so consume
                            // but do not apply that earlier roleless delta.
                            current.buffer_delta.take();
                            (
                                buffer,
                                damage,
                                current.buffer_scale,
                                current.buffer_transform,
                            )
                        });
                    let force_full_damage = damage.len() > MAX_DAMAGE_RECTS;
                    self.commit_cursor_surface(
                        &surface,
                        CursorCommit {
                            buffer: buffer.as_ref(),
                            damage: &damage,
                            force_full_damage,
                            buffer_scale,
                            buffer_transform,
                            buffer_delta: None,
                        },
                    );
                }
                CursorSelection::Surface(id)
            }
        };
        self.publish_current_cursor();
    }

    fn commit_cursor_surface(&mut self, surface: &WlSurface, commit: CursorCommit<'_>) -> bool {
        if compositor::get_role(surface) != Some(CURSOR_IMAGE_ROLE) {
            return false;
        }

        let id = surface.id();
        let requested_hotspot = cursor_surface_hotspot(surface);
        let record = self
            .cursor_surfaces
            .entry(id.clone())
            .or_insert(CursorSurfaceRecord {
                surface: surface.clone(),
                hotspot: requested_hotspot,
                shm_backing: None,
                dmabuf_backing: None,
                buffer_dimensions: None,
                presentation: None,
            });
        if let Some((x, y)) = commit.buffer_delta {
            record.hotspot = (
                record.hotspot.0.saturating_sub(x),
                record.hotspot.1.saturating_sub(y),
            );
        }
        match commit.buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => self.commit_cursor_new_buffer(
                surface,
                buffer.clone(),
                commit.damage,
                commit.force_full_damage,
                commit.buffer_scale,
                commit.buffer_transform,
            ),
            Some(BufferAssignment::Removed) => self.remove_cursor_surface_buffer(surface),
            None => {
                self.relayout_cursor_surface(surface, commit.buffer_scale, commit.buffer_transform)
            }
        }
        true
    }

    fn commit_cursor_new_buffer(
        &mut self,
        surface: &WlSurface,
        buffer: wl_buffer::WlBuffer,
        damage: &[Damage],
        force_full_damage: bool,
        buffer_scale: i32,
        buffer_transform: wl_output_protocol::Transform,
    ) {
        if let Ok(dmabuf) = get_dmabuf(&buffer) {
            let descriptor = match describe_dmabuf(dmabuf) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.retire_buffer_immediately(buffer);
                    tracing::warn!(surface = ?surface.id(), %error, "cursor DMA-BUF could not be described");
                    return;
                }
            };
            let presentation = match surface_presentation(
                surface,
                descriptor.width,
                descriptor.height,
                buffer_scale,
                buffer_transform,
            ) {
                Ok(presentation) => CursorPresentation::from(presentation),
                Err(error) => {
                    self.retire_buffer_immediately(buffer);
                    reject_cursor_presentation(surface, error);
                    return;
                }
            };
            let renderer_descriptor = match descriptor.try_clone() {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.retire_buffer_immediately(buffer);
                    tracing::warn!(surface = ?surface.id(), %error, "failed to duplicate cursor DMA-BUF description");
                    return;
                }
            };
            let (buffer_id, cacheable) = self.dmabuf_buffer_identity(&buffer);
            let backing_buffer = buffer.clone();
            let Some(token) = self.try_retain_dmabuf(surface, buffer) else {
                return;
            };
            let backing_retention_token = self.retain_buffer(backing_buffer.clone());
            let Some(client) = surface.client() else {
                self.release_buffer_token(token);
                self.release_buffer_token(backing_retention_token);
                return;
            };
            let release_point = self.take_committed_release_point(surface);
            let use_id = match self.release_uses.prepare_use(
                client.id(),
                &client,
                backing_buffer.id(),
                release_point,
                backing_retention_token,
                token,
            ) {
                BeginUseDecision::Implicit => None,
                BeginUseDecision::Begun(use_id) => Some(use_id),
                BeginUseDecision::Rejected(_) => {
                    self.release_buffer_token(token);
                    self.release_buffer_token(backing_retention_token);
                    return;
                }
            };
            let Some(record) = self.cursor_surfaces.get_mut(&surface.id()) else {
                self.release_buffer_token(token);
                self.release_buffer_token(backing_retention_token);
                return;
            };
            let released_shm_bytes = record
                .shm_backing
                .take()
                .map_or(0, |backing| backing.rgba.len());
            let previous_dmabuf_token = record
                .dmabuf_backing
                .replace(DmabufBacking {
                    buffer: backing_buffer,
                    buffer_id,
                    descriptor,
                    retention_token: backing_retention_token,
                    use_id,
                })
                .map(|backing| backing.retention_token);
            record.buffer_dimensions =
                Some((renderer_descriptor.width, renderer_descriptor.height));
            record.presentation = Some(presentation);
            if released_shm_bytes > 0 {
                self.release_shm_bytes(surface, released_shm_bytes);
            }
            if let Some(previous_dmabuf_token) = previous_dmabuf_token {
                self.release_buffer_token(previous_dmabuf_token);
            }
            if self.chrome_cursor_override.is_none()
                && self.cursor_selection == CursorSelection::Surface(surface.id())
            {
                let hotspot = self.cursor_surfaces[&surface.id()].hotspot;
                self.pending_cursor_update = false;
                self.events.push(ProtocolEvent::CursorUpdated {
                    image: CursorImage::Surface {
                        id: surface.id(),
                        hotspot,
                        presentation,
                        frame: Some(SurfaceFrame::Dmabuf(DmabufFrame {
                            buffer_id,
                            cacheable,
                            token,
                            descriptor: renderer_descriptor,
                            use_id,
                        })),
                    },
                });
            } else {
                self.release_buffer_token(token);
            }
            return;
        }

        let previous_shm_bytes = self
            .cursor_surfaces
            .get(&surface.id())
            .and_then(|record| record.shm_backing.as_ref())
            .map_or(0, |backing| backing.rgba.len());
        let max_backing_bytes = self.max_shm_backing_bytes(surface, previous_shm_bytes);
        let copied = {
            let record = self
                .cursor_surfaces
                .get_mut(&surface.id())
                .expect("cursor role record exists before buffer import");
            update_shm_buffer(
                &buffer,
                ShmUpdateContext {
                    output_size: self.backend.output_size(),
                    damage,
                    buffer_scale,
                    buffer_transform,
                    viewport: surface_viewport(surface),
                    force_full_damage,
                    max_backing_bytes,
                },
                &mut record.shm_backing,
            )
        };
        self.retire_buffer_immediately(buffer);
        let (frame, _) = match copied {
            Ok(copied) => copied,
            Err(error) => {
                if error.contains("aggregate SHM budget") {
                    self.reject_resource_limit(surface, error);
                } else {
                    tracing::warn!(surface = ?surface.id(), %error, "cursor SHM buffer could not be imported");
                }
                return;
            }
        };
        self.adjust_shm_bytes(surface, previous_shm_bytes, frame.rgba.len());
        let released_dmabuf_token = {
            let record = self
                .cursor_surfaces
                .get_mut(&surface.id())
                .expect("cursor role record exists after SHM import");
            let released = record
                .dmabuf_backing
                .take()
                .map(|backing| backing.retention_token);
            record.buffer_dimensions = Some((frame.width, frame.height));
            released
        };
        if let Some(released_dmabuf_token) = released_dmabuf_token {
            self.release_buffer_token(released_dmabuf_token);
        }
        let presentation = match surface_presentation(
            surface,
            frame.width,
            frame.height,
            buffer_scale,
            buffer_transform,
        ) {
            Ok(presentation) => CursorPresentation::from(presentation),
            Err(error) => {
                if let Some(record) = self.cursor_surfaces.get_mut(&surface.id()) {
                    record.presentation = None;
                }
                reject_cursor_presentation(surface, error);
                if self.cursor_selection == CursorSelection::Surface(surface.id()) {
                    self.publish_current_cursor();
                }
                return;
            }
        };
        let record = self
            .cursor_surfaces
            .get_mut(&surface.id())
            .expect("cursor role record exists after SHM import");
        record.presentation = Some(presentation);
        if self.chrome_cursor_override.is_none()
            && self.cursor_selection == CursorSelection::Surface(surface.id())
        {
            let hotspot = record.hotspot;
            self.pending_cursor_update = false;
            self.events.push(ProtocolEvent::CursorUpdated {
                image: CursorImage::Surface {
                    id: surface.id(),
                    hotspot,
                    presentation,
                    frame: Some(SurfaceFrame::Shm(frame)),
                },
            });
        }
    }

    fn remove_cursor_surface_buffer(&mut self, surface: &WlSurface) {
        let Some(record) = self.cursor_surfaces.get_mut(&surface.id()) else {
            return;
        };
        let released_shm_bytes = record
            .shm_backing
            .take()
            .map_or(0, |backing| backing.rgba.len());
        let released_dmabuf_token = record
            .dmabuf_backing
            .take()
            .map(|backing| backing.retention_token);
        record.buffer_dimensions = None;
        record.presentation = None;
        if released_shm_bytes > 0 {
            self.release_shm_bytes(surface, released_shm_bytes);
        }
        if let Some(released_dmabuf_token) = released_dmabuf_token {
            self.release_buffer_token(released_dmabuf_token);
        }
        if self.cursor_selection == CursorSelection::Surface(surface.id()) {
            self.publish_current_cursor();
        }
    }

    fn relayout_cursor_surface(
        &mut self,
        surface: &WlSurface,
        buffer_scale: i32,
        buffer_transform: wl_output_protocol::Transform,
    ) {
        let dimensions = self
            .cursor_surfaces
            .get(&surface.id())
            .and_then(|record| record.buffer_dimensions);
        let Some((width, height)) = dimensions else {
            return;
        };
        let presentation =
            match surface_presentation(surface, width, height, buffer_scale, buffer_transform) {
                Ok(presentation) => CursorPresentation::from(presentation),
                Err(error) => {
                    reject_cursor_presentation(surface, error);
                    return;
                }
            };
        if let Some(record) = self.cursor_surfaces.get_mut(&surface.id()) {
            record.presentation = Some(presentation);
        }
        if self.cursor_selection == CursorSelection::Surface(surface.id()) {
            self.publish_current_cursor();
        }
    }

    fn destroy_cursor_surface(&mut self, surface: &WlSurface) {
        let id = surface.id();
        self.destroy_cursor_surface_id(&id);
    }

    fn destroy_cursor_surface_id(&mut self, id: &ObjectId) {
        let Some(record) = self.cursor_surfaces.remove(id) else {
            return;
        };
        if let Some(backing) = record.shm_backing {
            self.release_shm_bytes(&record.surface, backing.rgba.len());
        }
        if let Some(backing) = record.dmabuf_backing {
            self.release_buffer_token(backing.retention_token);
        }
        if self.cursor_selection == CursorSelection::Surface(id.clone()) {
            self.cursor_selection = CursorSelection::Default;
            self.publish_current_cursor();
        }
    }

    fn publish_current_cursor(&mut self) {
        match self.latest_cursor_update() {
            Some(event) => {
                self.pending_cursor_update = false;
                self.events.push(event);
            }
            None => self.pending_cursor_update = true,
        }
    }

    fn latest_cursor_update(&mut self) -> Option<ProtocolEvent> {
        if let Some(cursor) = self.chrome_cursor_override {
            return Some(ProtocolEvent::CursorUpdated {
                image: CursorImage::Chrome(cursor),
            });
        }
        let image = match self.cursor_selection.clone() {
            CursorSelection::Default => CursorImage::Default,
            CursorSelection::Hidden => CursorImage::Hidden,
            CursorSelection::Surface(id) => {
                let Some(record) = self.cursor_surfaces.get(&id) else {
                    return Some(ProtocolEvent::CursorUpdated {
                        image: CursorImage::Hidden,
                    });
                };
                let Some(presentation) = record.presentation else {
                    return Some(ProtocolEvent::CursorUpdated {
                        image: CursorImage::Hidden,
                    });
                };
                let hotspot = record.hotspot;
                if let Some(backing) = &record.shm_backing {
                    CursorImage::Surface {
                        id,
                        hotspot,
                        presentation,
                        frame: Some(SurfaceFrame::Shm(ShmFrame {
                            width: backing.width,
                            height: backing.height,
                            opaque: backing.format == wl_shm::Format::Xrgb8888,
                            rgba: Arc::clone(&backing.rgba),
                        })),
                    }
                } else if let Some(backing) = &record.dmabuf_backing {
                    let buffer = backing.buffer.clone();
                    let buffer_id = backing.buffer_id;
                    let cacheable = self.dmabuf_buffer_is_cacheable(&buffer, buffer_id);
                    let use_id = backing.use_id;
                    let descriptor = match backing.descriptor.try_clone() {
                        Ok(descriptor) => descriptor,
                        Err(error) => {
                            tracing::warn!(surface = ?id, %error, "failed to duplicate dirty cursor DMA-BUF state");
                            return None;
                        }
                    };
                    let token = self.try_retain_existing_dmabuf(buffer, use_id)?;
                    CursorImage::Surface {
                        id,
                        hotspot,
                        presentation,
                        frame: Some(SurfaceFrame::Dmabuf(DmabufFrame {
                            buffer_id,
                            cacheable,
                            token,
                            descriptor,
                            use_id,
                        })),
                    }
                } else {
                    CursorImage::Hidden
                }
            }
        };
        Some(ProtocolEvent::CursorUpdated { image })
    }

    fn reconcile_subsurface_roles(&mut self) {
        let stale = self
            .surfaces
            .iter()
            .filter_map(|(object, record)| {
                let SurfaceRole::Subsurface {
                    surface, parent, ..
                } = &record.role
                else {
                    return None;
                };
                (!compositor::get_parent(surface).is_some_and(|current| current == *parent))
                    .then_some((object.clone(), surface.clone()))
            })
            .collect::<Vec<_>>();
        for (object, surface) in stale {
            if self.surfaces.contains_key(&object) {
                let former_root = self.toplevel_root_for_surface(&surface);
                self.detach_subsurface_topology(&surface);
                tracing::debug!(
                    surface = ?surface.id(),
                    "suspending wl_surface whose wl_subsurface role was destroyed"
                );
                self.deactivate_surface_role(&surface);
                if let Some(former_root) = former_root {
                    self.refresh_toplevel_window_geometry(&former_root);
                }
            }
        }
    }

    /// Move the cursor to an absolute position and re-resolve pointer focus.
    ///
    /// The confinement lives here rather than in the callers so both transports
    /// and every future one are bounded by the same rule: an absolute device
    /// reporting a normalised 1.0 scales to exactly the output width, which is
    /// outside the hit test's half-open bounds just as surely as an
    /// over-accumulated relative delta is. See [`clamp_point_to_seat`].
    fn pointer_moved(&mut self, x: f64, y: f64, time: u32) {
        if self
            .titlebar_click_candidate
            .as_ref()
            .is_some_and(|candidate| {
                time.wrapping_sub(candidate.time) > TITLEBAR_DOUBLE_CLICK_MILLIS
            })
        {
            self.titlebar_click_candidate = None;
        }
        let clamped = clamp_point_to_seat((x, y), &self.backend.seat_regions());
        let (x, y) = clamped.position;
        #[cfg(feature = "bus")]
        self.sample_corner_motion(
            clamped.position,
            clamped.region_index,
            clamped.attempted_motion,
        );
        {
            let mut snapshot = self
                .cursor_position_snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.x = x;
            snapshot.y = y;
            snapshot.on_output = true;
            snapshot.revision = snapshot.revision.saturating_add(1);
        }
        self.cursor_position = (x, y);
        let geometry_changed = self.update_interactive_pointer(x, y);
        if self.chrome_pointer_grab.is_some() {
            if let Some(grab) = self.chrome_pointer_grab.as_mut()
                && matches!(grab.kind, ChromePointerGrabKind::Move)
            {
                let dx = x - grab.start_pointer.0;
                let dy = y - grab.start_pointer.1;
                if dx * dx + dy * dy > TITLEBAR_DOUBLE_CLICK_SLOP * TITLEBAR_DOUBLE_CLICK_SLOP {
                    grab.dragged = true;
                    self.titlebar_click_candidate = None;
                }
            }
            if geometry_changed {
                self.retarget_chrome_pointer_after_geometry_change();
            } else {
                self.update_chrome_pointer_from_target(self.pointer_target_at(x, y));
            }
            return;
        }
        let target = if self.pointer.is_grabbed() {
            self.client_pointer_target_at(x, y)
        } else {
            self.pointer_target_at(x, y)
        };
        self.update_chrome_pointer_from_target(target.clone());
        let focus = match target {
            Some(PointerTarget::Client { surface, origin }) => {
                Some((self.seat_focus_target_for(&surface), origin))
            }
            Some(PointerTarget::Chrome { .. }) | None => None,
        };
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            focus.clone(),
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        self.record_pointer_focus_local_position(focus.as_ref(), (x, y));
    }

    /// Apply accelerated relative motion to the compositor's own cursor.
    ///
    /// A relative device reports a delta and nothing else, so the compositor is
    /// the only holder of the cursor position — which is why this resolves to
    /// [`Self::pointer_moved`] rather than duplicating focus and frame handling
    /// — including the confinement, which a bare-metal pointer needs because it
    /// has no host window to be bounded by and would otherwise walk off the
    /// output and silently lose focus at the first edge.
    fn pointer_motion(&mut self, dx: f64, dy: f64, time: u32) {
        self.pointer_moved(
            self.cursor_position.0 + dx,
            self.cursor_position.1 + dy,
            time,
        );
    }

    fn pointer_button(&mut self, button: u32, state: HostButtonState, time: u32) {
        if self
            .titlebar_click_candidate
            .as_ref()
            .is_some_and(|candidate| {
                time.wrapping_sub(candidate.time) > TITLEBAR_DOUBLE_CLICK_MILLIS
            })
        {
            self.titlebar_click_candidate = None;
        }
        if let Some(grab) = self.chrome_pointer_grab.clone() {
            match state {
                HostButtonState::Pressed if button != grab.button => {
                    self.suppressed_chrome_buttons.insert(button);
                }
                HostButtonState::Released if button == grab.button => {
                    self.finish_chrome_pointer_grab(grab, time);
                }
                HostButtonState::Released => {
                    self.suppressed_chrome_buttons.remove(&button);
                }
                HostButtonState::Pressed => {}
            }
            return;
        }

        if state == HostButtonState::Pressed {
            self.suppressed_chrome_buttons.remove(&button);
        }

        if state == HostButtonState::Released && self.suppressed_chrome_buttons.remove(&button) {
            if self
                .chrome_pressed
                .as_ref()
                .is_some_and(|(_, _, pressed_button)| *pressed_button == button)
            {
                self.update_chrome_pressed(None);
            }
            return;
        }

        if !self.pointer.is_grabbed()
            && let Some(PointerTarget::Chrome {
                object,
                part,
                button_cluster_hovered,
            }) = self.pointer_target_at(self.cursor_position.0, self.cursor_position.1)
        {
            if state == HostButtonState::Pressed
                && (button != PRIMARY_POINTER_BUTTON || !matches!(part, ChromePart::TitlebarDrag))
            {
                self.titlebar_click_candidate = None;
            }
            let hover = if button_cluster_hovered {
                Some((
                    object.clone(),
                    match part {
                        ChromePart::Button(caption) => Some(caption),
                        _ => None,
                    },
                ))
            } else {
                None
            };
            self.update_chrome_hover(hover);
            if state == HostButtonState::Pressed
                && (button != PRIMARY_POINTER_BUTTON
                    || !self.begin_chrome_pointer_grab(object, part, button))
            {
                self.suppressed_chrome_buttons.insert(button);
            }
            return;
        }

        if state == HostButtonState::Pressed {
            self.titlebar_click_candidate = None;
            let focused = self
                .surface_at(self.cursor_position.0, self.cursor_position.1)
                .map(|record| record.role.wl_surface().clone());
            if let Some(surface) = focused.as_ref() {
                self.raise_for_focus_interaction(surface);
            }
            self.arbitrate_keyboard_focus(focused, false, true);
        }

        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                button,
                state: smithay_button_state(state),
            },
        );
        pointer.frame(self);
        if state == HostButtonState::Released {
            self.finish_interactive_pointer(true);
        }
    }

    /// Announce the touch capability once a device that has one is attached.
    ///
    /// Counted rather than latched, because a seat with two touchscreens keeps
    /// the capability when one of them is unplugged.
    fn add_touch_device(&mut self) {
        self.touch_devices = self.touch_devices.saturating_add(1);
        if self.touch_devices == 1 {
            self.seat.clone().add_touch();
        }
    }

    /// Cancel the touch session on *any* touch device leaving, and withdraw the
    /// capability once the last one has.
    ///
    /// The cancel is not optional. A client holding contacts when the device
    /// vanishes will never receive their `wl_touch.up` — the hardware that would
    /// have produced it is gone — so without a cancel it is left believing
    /// fingers are still down, forever.
    ///
    /// It fires on every removal, not just the last, and that is deliberately
    /// broader than it looks like it needs to be. [`HostInput`] is the seat's
    /// vocabulary and carries no device, so by the time a removal reaches here
    /// there is no way to ask which contacts belonged to the departed device.
    /// Cancelling only at zero leaves a worse hole than over-cancelling does:
    /// with two touchscreens attached and a contact live on the first, unplugging
    /// it strands that contact *and* its `TouchDownGrab` forever — and because
    /// `touch_down` reads `is_grabbed()` to decide whether this is the first
    /// contact, every later contact on the surviving device would then skip the
    /// focus policy and be forced onto the dead device's surface. The cost of the
    /// broader rule is cancelling a gesture on the surviving device that did not
    /// have to be cancelled; `wl_touch.cancel` is precisely the protocol's way to
    /// say "the compositor lost track, discard these", so paying it is correct.
    /// Per-device attribution needs device identity in `HostInput`, which is
    /// E-5's work alongside the same reconciliation keyboards and pointers lack.
    ///
    /// Cancelling *before* the capability is withdrawn matters too: `remove_touch`
    /// drops the [`TouchHandle`], and the events would then have nothing to
    /// travel through.
    fn remove_touch_device(&mut self) {
        if self.touch_devices == 0 {
            return;
        }
        self.touch_devices -= 1;
        self.cancel_touch();
        if self.touch_devices == 0 {
            self.seat.clone().remove_touch();
        }
    }

    /// The surface under a touch point, paired with its origin.
    ///
    /// The [`Point`] is the surface's origin in global compositor space, not the
    /// contact position: Smithay subtracts it from the event to get
    /// surface-local coordinates (`new_event.location -= *loc` in
    /// `vendor/smithay/src/input/touch/mod.rs`). It is `layout`, not
    /// `window_origin` — the latter is the xdg *window geometry* origin, which
    /// sits inside the surface whenever a client draws its own shadows, and
    /// using it would offset every touch by the shadow width in a way no
    /// bounds check could catch. This is the same pair
    /// [`WaylandState::pointer_moved`] builds, deliberately.
    fn touch_focus_at(&self, x: f64, y: f64) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.surface_at(x, y).map(|record| {
            (
                record.role.wl_surface().clone(),
                (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
            )
        })
    }

    /// Begin one contact, and on the *first* contact only, focus what it landed on.
    ///
    /// Focus policy mirrors [`WaylandState::pointer_button`] — hit test, set
    /// keyboard focus to the compositor root, raise — because on a touch-only
    /// machine a touch is the only way to focus a window at all.
    ///
    /// It is restricted to the first contact for two reasons that both have
    /// teeth. A second finger landing on a different window mid-gesture must not
    /// steal focus or reorder the stack, or a two-finger drag would raise
    /// whatever is under the second finger. And Smithay's `TouchDownGrab`
    /// already forces every subsequent contact onto the first contact's surface
    /// (`vendor/smithay/src/input/touch/grab.rs`), so raising the surface under
    /// a later contact would raise a window that is not even receiving the
    /// events.
    ///
    /// "First" is `!TouchHandle::is_grabbed()` sampled immediately before the
    /// down: `DefaultGrab::down` installs the grab as part of handling the first
    /// contact, and the last `up` removes it.
    fn touch_down(&mut self, slot: TouchSlot, x: f64, y: f64, time: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let (x, y) = clamp_point_to_seat((x, y), &self.backend.seat_regions()).position;
        let focus = self.touch_focus_at(x, y);
        if !touch.is_grabbed() {
            let requested = focus.as_ref().map(|(surface, _)| surface.clone());
            if let Some(surface) = requested.as_ref() {
                self.raise_for_focus_interaction(surface);
            }
            self.arbitrate_keyboard_focus(requested, false, true);
        }
        let focus = focus.map(|(surface, origin)| (self.seat_focus_target_for(&surface), origin));
        touch.down(
            self,
            focus,
            &TouchDownEvent {
                slot,
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Move one contact.
    ///
    /// The focus is passed but does not move the touch focus — Smithay sets that
    /// only on `down`. It is handed over because that is the documented way a
    /// future drag-and-drop grab learns what is under a moving finger.
    fn touch_motion(&mut self, slot: TouchSlot, x: f64, y: f64, time: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let (x, y) = clamp_point_to_seat((x, y), &self.backend.seat_regions()).position;
        let focus = self
            .touch_focus_at(x, y)
            .map(|(surface, origin)| (self.seat_focus_target_for(&surface), origin));
        touch.motion(
            self,
            focus,
            &TouchMotionEvent {
                slot,
                location: (x, y).into(),
                time,
            },
        );
    }

    fn touch_up(&mut self, slot: TouchSlot, time: u32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.up(
            self,
            &TouchUpEvent {
                slot,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Close one batch of simultaneous touch changes.
    ///
    /// Only ever forwarded from a device, never synthesised. `wl_touch.frame`
    /// marks the end of a set of changes that a client is meant to apply
    /// together, so manufacturing one per event would split a two-finger update
    /// into two batches and tell the client something the hardware did not say.
    /// A transport whose backend has no frame event — Smithay's winit backend
    /// has no `TouchFrameEvent` at all — must produce its own at whatever its
    /// real batching boundary is, rather than have this end of the pipe guess.
    fn touch_frame(&mut self) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.frame(self);
    }

    /// End every live contact.
    ///
    /// Deliberately a named helper and not inlined into the dispatch arm: rung F
    /// must cancel touch when the session is paused, and it does so by
    /// submitting [`HostInput::TouchCancel`] so that a VT switch and a
    /// device-reported cancel cannot acquire two different meanings.
    ///
    /// No `up` is synthesised and no frame is sent afterwards. `wl_touch.cancel`
    /// already ends the sequence and is not followed by a frame; keyboard focus
    /// and stacking are left exactly as they are, because a cancelled gesture is
    /// not an undo of the focus the first contact legitimately took.
    fn cancel_touch(&mut self) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.cancel(self);
    }

    /// Emit one `wl_pointer` axis frame, describing only the axes the device
    /// actually reported.
    ///
    /// Two things this must not do, both of which are silent when wrong:
    ///
    /// It must not speak for an axis the device left out. An ordinary vertical
    /// wheel event carries no horizontal axis, and adding a zero for it is not
    /// harmless — on a source whose zero means "stopped" it manufactures a
    /// `wl_pointer.axis_stop` for a direction the user never scrolled.
    ///
    /// It must not swallow a zero the device did report. `axis_stop` is how a
    /// client learns a kinetic scroll ended, and it is exactly a zero-amount
    /// report. Dropping it leaves the client flinging forever. A wheel is the
    /// opposite case: it has no defined end, so a zero from one carries no
    /// information and is not forwarded at all.
    fn pointer_axis(
        &mut self,
        horizontal: Option<HostAxis>,
        vertical: Option<HostAxis>,
        source: AxisSource,
        relative_direction: (AxisRelativeDirection, AxisRelativeDirection),
        time: u32,
    ) {
        if self.chrome_pointer_grab.is_some() {
            return;
        }
        // Finger and continuous sources have a defined end of sequence, so a
        // reported zero from one is a stop. A wheel never promises a
        // terminating event, so a zero from one is just an idle axis.
        let stop_is_meaningful = matches!(source, AxisSource::Finger | AxisSource::Continuous);

        let mut frame = AxisFrame::new(time).source(source);
        let mut carries_anything = false;
        for (axis, direction, report) in [
            (Axis::Horizontal, relative_direction.0, horizontal),
            (Axis::Vertical, relative_direction.1, vertical),
        ] {
            // Not `unwrap_or_default`: an absent axis stays absent from the
            // frame, so the client hears nothing about a direction the device
            // said nothing about.
            let Some(report) = report else {
                continue;
            };
            let stops = report.amount == 0.0 && stop_is_meaningful;
            let scrolls = report.amount != 0.0 || report.v120.is_some_and(|v120| v120 != 0);
            if !stops && !scrolls {
                continue;
            }
            carries_anything = true;
            frame = frame
                .relative_direction(axis, direction)
                .value(axis, report.amount);
            if let Some(v120) = report.v120 {
                frame = frame.v120(axis, v120);
            }
            if stops {
                frame = frame.stop(axis);
            }
        }
        if !carries_anything {
            return;
        }

        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Feed one key through Smithay's real XKB state and client filter.
    ///
    /// Bindings deliberately use `raw_latin_sym_or_raw_current_sym`: mnemonic
    /// `Super+Q` follows logical Q on AZERTY and Dvorak instead of staying on a
    /// physical key. A keymap with no Latin group (for example Cyrillic-only)
    /// may therefore not match it; physical-key configuration is deliberately
    /// outside this Phase 1 table.
    ///
    /// The keycode arrives already in XKB terms — see
    /// [`HostInput::key_from_evdev`] for why the evdev offset is applied by
    /// exactly one transport and never here.
    fn keyboard_keycode(&mut self, keycode: Keycode, state: HostButtonState, time: u32) {
        let keyboard = self.keyboard.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let pressed = state == HostButtonState::Pressed;
        let action = keyboard
            .input::<Option<BindingAction>, _>(
                self,
                keycode,
                smithay_key_state(state),
                serial,
                time,
                |state, modifiers, key_handle| {
                    let keysym = key_handle.raw_latin_sym_or_raw_current_sym();
                    let disposition = if state.session_lock_active() {
                        state
                            .bindings
                            .dispatch_session_locked(keycode, pressed, keysym, modifiers)
                    } else {
                        state.bindings.dispatch(keycode, pressed, keysym, modifiers)
                    };
                    binding_filter_result(disposition)
                },
            )
            .flatten();

        if pressed {
            if action.is_none() {
                self.last_keyboard_action = keyboard
                    .current_focus()
                    .and_then(|target| target.owned_surface())
                    .map(|surface| {
                        (
                            serial,
                            canonical_root_surface(&self.popup_manager, &surface),
                        )
                    });
            } else {
                invalidate_keyboard_action(&mut self.last_keyboard_action);
            }
        } else {
            invalidate_keyboard_action(&mut self.last_keyboard_action);
        }

        if let Some(action) = action {
            self.handle_binding_action(action);
        }
    }

    /// Advance XKB while presentation is closed, retaining only the KMS
    /// Ctrl-Alt-Fn action. The filter intercepts every event, including keys
    /// which are not compositor bindings, so the stalled client sees nothing.
    fn keyboard_keycode_presentation_gated(
        &mut self,
        keycode: Keycode,
        state: HostButtonState,
        time: u32,
    ) {
        let keyboard = self.keyboard.clone();
        let pressed = state == HostButtonState::Pressed;
        let action = keyboard
            .input::<Option<BindingAction>, _>(
                self,
                keycode,
                smithay_key_state(state),
                SERIAL_COUNTER.next_serial(),
                time,
                |state, modifiers, key_handle| {
                    let keysym = key_handle.raw_latin_sym_or_raw_current_sym();
                    match state
                        .bindings
                        .dispatch_session_locked(keycode, pressed, keysym, modifiers)
                    {
                        KeyDisposition::Act(action @ BindingAction::SwitchVt(_)) => {
                            FilterResult::Intercept(Some(action))
                        }
                        KeyDisposition::Forward
                        | KeyDisposition::SwallowRelease
                        | KeyDisposition::Act(_) => FilterResult::Intercept(None),
                    }
                },
            )
            .flatten();
        invalidate_keyboard_action(&mut self.last_keyboard_action);
        if let Some(action) = action {
            self.handle_binding_action(action);
        }
    }

    /// Release every pressed key when input authority is lost.
    ///
    /// Both backends lose authority, for different reasons: the nested one when
    /// the host window loses focus, the bare-metal one when the session is
    /// paused for a VT switch. Either way the compositor stops being told about
    /// releases while remaining the holder of the pressed set, so it must
    /// synthesise them — and through Smithay's normal filter, so its pressed and
    /// forwarded-pressed sets, XKB modifiers, client-visible key state and
    /// `BindingState`'s intercepted presses are reconciled as one
    /// protocol-thread operation.
    ///
    /// This is deliberately one implementation rather than one per authority
    /// loss: a pause-specific variant would be the first place the two disagree
    /// about what a stuck modifier means.
    fn release_pressed_keys(&mut self) {
        let keyboard = self.keyboard.clone();
        let pressed_keys = keyboard.pressed_keys();
        if !pressed_keys.is_empty() {
            tracing::debug!(
                count = pressed_keys.len(),
                "releasing pressed keys after input authority loss"
            );
        }
        let time = monotonic_millis();
        for keycode in pressed_keys {
            self.keyboard_keycode(keycode, HostButtonState::Released, time);
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_all_input_authority_loss(&mut self) {
        self.cancel_chrome_pointer_grab(true);
        self.update_chrome_hover(None);
        self.set_chrome_cursor_override(None);
        let held = input::SeatHeldState {
            keys: self.keyboard.pressed_keys(),
            buttons: self.pointer.current_pressed(),
        };
        let inputs = self.input_ingress.all_devices_lost_authority(&held);
        if !inputs.is_empty() {
            tracing::debug!(
                operations = inputs.len(),
                "reconciling all held input after authority loss"
            );
        }
        for input in inputs {
            self.handle_host_input_with_activity(input, false);
        }
    }

    fn handle_binding_action(&mut self, action: BindingAction) {
        match action {
            BindingAction::RequestCloseFocused => {
                debug_assert!(!action.needs_ecs());
                let Some(focused) = self
                    .keyboard
                    .current_focus()
                    .and_then(|target| target.owned_surface())
                else {
                    tracing::debug!("close-focused binding had no keyboard focus");
                    return;
                };
                let root = canonical_root_surface(&self.popup_manager, &focused);
                match self.surfaces.get(&root.id()).map(|record| &record.role) {
                    Some(SurfaceRole::Toplevel(toplevel)) if toplevel.wl_surface().is_alive() => {
                        toplevel.send_close();
                        tracing::debug!(surface = ?root.id(), "requested focused toplevel close");
                    }
                    // X11 close: WM_DELETE_WINDOW when supported, destroy
                    // otherwise (Smithay decides).
                    #[cfg(feature = "xwayland")]
                    Some(SurfaceRole::X11(role)) => {
                        if let Err(error) = role.surface.close() {
                            tracing::debug!(%error, "failed to request X11 window close");
                        } else {
                            tracing::debug!(surface = ?root.id(), "requested focused X11 close");
                        }
                    }
                    _ => {
                        tracing::debug!(
                            surface = ?focused.id(),
                            root = ?root.id(),
                            "close-focused binding resolved to no live toplevel"
                        );
                    }
                }
            }
            BindingAction::RestoreMostRecentlyMinimized => {
                debug_assert!(!action.needs_ecs());
                self.restore_most_recently_minimized();
            }
            BindingAction::ExitNestedCompositor => {
                debug_assert!(action.needs_ecs());
                match self
                    .ecs_action_sender
                    .try_send(EcsAction::ExitNestedCompositor)
                {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => tracing::warn!(
                        capacity = ECS_ACTION_QUEUE_CAPACITY,
                        "ECS action queue full; nested compositor exit request was dropped"
                    ),
                    Err(TrySendError::Disconnected(_)) => {
                        tracing::debug!("renderer stopped before nested compositor exit request")
                    }
                }
            }
            BindingAction::ToggleInterception => {
                debug_assert!(!action.needs_ecs());
                let enabled = self.bindings.toggle_interception();
                tracing::info!(enabled, "compositor key interception toggled");
            }
            BindingAction::SwitchVt(vt) => {
                debug_assert!(!action.needs_ecs());
                if let Some(request) = self.vt_switch_requested.as_ref() {
                    request(vt);
                } else {
                    tracing::error!(vt, "live VT-switch binding has no coordinator route");
                }
            }
        }
    }

    fn surface_at(&self, x: f64, y: f64) -> Option<&SurfaceRecord> {
        self.surfaces
            .values()
            .filter(|record| {
                if !self.surface_is_input_presentable(record)
                    || !record.layout.visible
                    || x < f64::from(record.layout.x)
                    || y < f64::from(record.layout.y)
                    || x >= f64::from(record.layout.x + record.layout.width)
                    || y >= f64::from(record.layout.y + record.layout.height)
                {
                    return false;
                }
                let local = (
                    (x - f64::from(record.layout.x)).floor() as i32,
                    (y - f64::from(record.layout.y)).floor() as i32,
                );
                record
                    .committed_input_region
                    .as_ref()
                    .is_none_or(|region| region.contains(local))
            })
            .max_by(|left, right| surface_stack_cmp(left, right))
    }

    fn surface_is_session_presentable(&self, record: &SurfaceRecord) -> bool {
        if matches!(self.lock_lifecycle, LockLifecycle::Unlocked) {
            return !matches!(record.role, SurfaceRole::LockSurface(_));
        }
        let lock_generation = match &record.role {
            SurfaceRole::LockSurface(role) => Some(role.lock_generation),
            SurfaceRole::Subsurface { surface, .. } => {
                let root = root_compositor_surface(surface);
                self.surfaces.get(&root.id()).and_then(|root| {
                    let SurfaceRole::LockSurface(role) = &root.role else {
                        return None;
                    };
                    Some(role.lock_generation)
                })
            }
            _ => None,
        };
        match &self.lock_lifecycle {
            LockLifecycle::Unlocked => unreachable!("Unlocked returned on the fast path"),
            LockLifecycle::Locking { generation, .. }
            | LockLifecycle::Locked { generation, .. }
            | LockLifecycle::OrphanedLocked { generation } => lock_generation == Some(*generation),
        }
    }

    fn surface_is_input_presentable(&self, record: &SurfaceRecord) -> bool {
        if matches!(self.lock_lifecycle, LockLifecycle::Unlocked)
            && self.kms_session_lock_gate.normal_scene_restricted()
        {
            return false;
        }
        self.surface_is_session_presentable(record)
    }

    fn surface_is_renderer_presentable(&self, record: &SurfaceRecord) -> bool {
        surface_is_presentable(record) && self.surface_is_session_presentable(record)
    }

    fn refresh_committed_input_region(&mut self, surface: &WlSurface) -> bool {
        let (region, bounded_fallback) = compositor::with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            CommittedInputRegion::from_surface_attributes(attributes.current())
        });
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return false;
        };
        let changed = record.committed_input_region != region;
        record.committed_input_region = region;
        let warn = bounded_fallback.is_some()
            && record
                .logged_diagnostics
                .insert(SurfaceDiagnostic::InputRegionBoundingBox);
        if warn {
            tracing::warn!(
                surface_id = record.id.0,
                surface = ?surface.id(),
                rectangles = bounded_fallback.expect("fallback count was present"),
                limit = MAX_COMMITTED_INPUT_REGION_RECTS,
                "collapsed excessive input-region operations to their bounding box"
            );
        }
        changed
    }

    fn reconcile_pointer_target(&mut self) {
        let (x, y) = self.cursor_position;
        let target = if self.pointer.is_grabbed() {
            self.client_pointer_target_at(x, y)
        } else {
            self.pointer_target_at(x, y)
        };
        let current = self
            .pointer
            .current_focus()
            .and_then(|target| target.surface_id());
        let next = match target.as_ref() {
            Some(PointerTarget::Client { surface, .. }) => Some(surface.id()),
            Some(PointerTarget::Chrome { .. }) | None => None,
        };
        if current != next {
            self.retarget_pointer_after_visibility_change();
        } else {
            self.update_chrome_pointer_from_target(target.clone());
            if let Some(PointerTarget::Client { surface, origin }) = target {
                let local = (x - origin.x, y - origin.y);
                let local_changed = self
                    .pointer_focus_local_position
                    .as_ref()
                    .is_none_or(|(object, previous)| *object != surface.id() || *previous != local);
                if !local_changed {
                    return;
                }
                let pointer = self.pointer.clone();
                pointer.motion(
                    self,
                    Some((self.seat_focus_target_for(&surface), origin)),
                    &MotionEvent {
                        location: (x, y).into(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time: monotonic_millis(),
                    },
                );
                pointer.frame(self);
                self.pointer_focus_local_position = Some((surface.id(), local));
            }
        }
    }

    fn record_pointer_focus_local_position(
        &mut self,
        requested: Option<&(SeatFocusTarget, Point<f64, Logical>)>,
        global: (f64, f64),
    ) {
        let current = self.pointer.current_focus();
        let previous = self.pointer_focus_local_position.take();
        self.pointer_focus_local_position = current.and_then(|current| {
            let current_id = current.surface_id()?;
            if let Some((target, origin)) = requested
                && target.surface_id().as_ref() == Some(&current_id)
            {
                return Some((current_id, (global.0 - origin.x, global.1 - origin.y)));
            }
            previous.filter(|(object, _)| *object == current_id)
        });
    }

    fn mark_pointer_hit_test_dirty(&mut self) {
        self.pointer_hit_test_dirty = true;
    }

    fn pointer_hit_test_reconciliation_deferred(&self) -> bool {
        self.pointer_hit_test_transaction_applying || self.pointer_hit_test_batch_depth > 0
    }

    fn invalidate_pointer_hit_test(&mut self) {
        self.mark_pointer_hit_test_dirty();
        if !self.pointer_hit_test_reconciliation_deferred() {
            self.reconcile_deferred_pointer_hit_test();
        }
    }

    fn invalidate_pointer_hit_test_geometry(&mut self) {
        self.invalidate_pointer_hit_test();
    }

    fn begin_pointer_hit_test_batch(&mut self) {
        self.pointer_hit_test_batch_depth = self.pointer_hit_test_batch_depth.saturating_add(1);
    }

    fn end_pointer_hit_test_batch(&mut self) {
        self.pointer_hit_test_batch_depth = self.pointer_hit_test_batch_depth.saturating_sub(1);
        if !self.pointer_hit_test_reconciliation_deferred() {
            self.reconcile_deferred_pointer_hit_test();
        }
    }

    fn defer_or_cancel_pointer_grab_for_focus_policy(&mut self) {
        if self.pointer_hit_test_reconciliation_deferred() {
            self.pointer_grab_teardown_deferred = true;
            self.mark_pointer_hit_test_dirty();
            return;
        }
        let pointer = self.pointer.clone();
        pointer.unset_grab_without_focus_restore(
            self,
            SERIAL_COUNTER.next_serial(),
            monotonic_millis(),
        );
        self.invalidate_pointer_hit_test();
    }

    fn reconcile_deferred_pointer_hit_test(&mut self) {
        let teardown_grab = mem::take(&mut self.pointer_grab_teardown_deferred);
        if teardown_grab {
            let pointer = self.pointer.clone();
            pointer.unset_grab_without_focus_restore(
                self,
                SERIAL_COUNTER.next_serial(),
                monotonic_millis(),
            );
        }
        if !mem::take(&mut self.pointer_hit_test_dirty) {
            return;
        }
        #[cfg(test)]
        {
            self.pointer_hit_test_reconciliations =
                self.pointer_hit_test_reconciliations.saturating_add(1);
        }
        self.reconcile_pointer_target();
    }

    fn client_pointer_target_at(&self, x: f64, y: f64) -> Option<PointerTarget> {
        self.surface_at(x, y).map(|record| PointerTarget::Client {
            surface: record.role.wl_surface().clone(),
            origin: (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
        })
    }

    fn pointer_target_at(&self, x: f64, y: f64) -> Option<PointerTarget> {
        let client = self.surface_at(x, y);
        if self.session_lock_active() {
            // Compositor chrome belongs to hidden ordinary toplevels. Under
            // lock it must not start grabs, dispatch caption buttons or reveal
            // a chrome cursor through a blank/input-region hole.
            return client.map(|record| PointerTarget::Client {
                surface: record.role.wl_surface().clone(),
                origin: (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
            });
        }
        if !self.decoration.enabled {
            return client.map(|record| PointerTarget::Client {
                surface: record.role.wl_surface().clone(),
                origin: (f64::from(record.layout.x), f64::from(record.layout.y)).into(),
            });
        }
        let chrome = self
            .surfaces
            .values()
            .filter_map(|record| {
                if !record.layout.visible
                    || !record.role.managed_toplevel()
                    || record.committed_decoration != SceneDecorationMode::ServerSide
                {
                    return None;
                }
                let geometry = record.committed_window_geometry?;
                let chrome = ChromeLayout::compute(
                    &self.decoration.theme,
                    vec2(geometry.width, geometry.height),
                );
                let content_offset = chrome.content_offset();
                let outer_origin = (
                    record.layout.x + geometry.x - content_offset.x,
                    record.layout.y + geometry.y - content_offset.y,
                );
                let local = vec2(x as f32 - outer_origin.0, y as f32 - outer_origin.1);
                let part = chrome.hit_test(local);
                let button_cluster_hovered = chrome.button_cluster.contains(local);
                (!matches!(part, ChromePart::Content | ChromePart::Outside)).then_some((
                    record,
                    part,
                    button_cluster_hovered,
                ))
            })
            .max_by(|(left, _, _), (right, _, _)| surface_stack_cmp(left, right));

        match (client, chrome) {
            (Some(client), Some((chrome, _part, _)))
                if surface_stack_cmp(client, chrome).is_gt() =>
            {
                Some(PointerTarget::Client {
                    surface: client.role.wl_surface().clone(),
                    origin: (f64::from(client.layout.x), f64::from(client.layout.y)).into(),
                })
            }
            (_, Some((chrome, part, button_cluster_hovered))) => Some(PointerTarget::Chrome {
                object: chrome.role.wl_surface().id(),
                part,
                button_cluster_hovered,
            }),
            (Some(client), None) => Some(PointerTarget::Client {
                surface: client.role.wl_surface().clone(),
                origin: (f64::from(client.layout.x), f64::from(client.layout.y)).into(),
            }),
            (None, None) => None,
        }
    }

    fn update_chrome_pointer_from_target(&mut self, target: Option<PointerTarget>) {
        let pressed = self.chrome_pointer_grab.as_ref().and_then(|grab| {
            let ChromePointerGrabKind::Button(caption) = grab.kind else {
                return None;
            };
            matches!(
                target.as_ref(),
                Some(PointerTarget::Chrome {
                    object,
                    part: ChromePart::Button(button),
                    ..
                }) if *object == grab.surface.id() && *button == caption
            )
            .then(|| (grab.surface.id(), caption, grab.button))
        });
        self.update_chrome_pressed(pressed);
        let hover = match target.as_ref() {
            Some(PointerTarget::Chrome {
                object,
                part,
                button_cluster_hovered: true,
            }) => Some((
                object.clone(),
                match part {
                    ChromePart::Button(button) => Some(*button),
                    _ => None,
                },
            )),
            _ => None,
        };
        self.update_chrome_hover(hover);
        let cursor = match self.chrome_pointer_grab.as_ref().map(|grab| grab.kind) {
            Some(ChromePointerGrabKind::Move) => Some(ChromeCursorIcon::Move),
            Some(ChromePointerGrabKind::Resize(edge)) => Some(chrome_resize_cursor(edge)),
            Some(ChromePointerGrabKind::Button(_)) | None => match target {
                Some(PointerTarget::Chrome {
                    part: ChromePart::TitlebarDrag | ChromePart::Button(_),
                    ..
                }) => Some(ChromeCursorIcon::Move),
                Some(PointerTarget::Chrome {
                    part: ChromePart::Resize(edge),
                    ..
                }) => Some(chrome_resize_cursor(edge)),
                Some(PointerTarget::Chrome {
                    part: ChromePart::Content | ChromePart::Outside,
                    ..
                })
                | Some(PointerTarget::Client { .. })
                | None => None,
            },
        };
        self.set_chrome_cursor_override(cursor);
    }

    fn set_chrome_cursor_override(&mut self, cursor: Option<ChromeCursorIcon>) {
        if self.chrome_cursor_override == cursor {
            return;
        }
        self.chrome_cursor_override = cursor;
        self.publish_current_cursor();
    }

    fn refresh_chrome_pointer_after_scene_change(&mut self) {
        let (x, y) = self.cursor_position;
        let target = if self.pointer.is_grabbed() {
            self.client_pointer_target_at(x, y)
        } else {
            self.pointer_target_at(x, y)
        };
        self.update_chrome_pointer_from_target(target);
    }

    fn retarget_chrome_pointer_after_geometry_change(&mut self) {
        #[cfg(test)]
        {
            self.chrome_geometry_retarget_count =
                self.chrome_geometry_retarget_count.saturating_add(1);
        }
        self.refresh_chrome_pointer_after_scene_change();
    }

    fn begin_chrome_pointer_grab(
        &mut self,
        object: ObjectId,
        part: ChromePart,
        button: u32,
    ) -> bool {
        if button != PRIMARY_POINTER_BUTTON
            || self.pointer.is_grabbed()
            || self.chrome_pointer_grab.is_some()
            || self.interactive_pointer.is_some()
        {
            return false;
        }
        let Some(record) = self.surfaces.get(&object) else {
            return false;
        };
        if !record.mapped
            || record.committed_decoration != SceneDecorationMode::ServerSide
            || !record.role.managed_toplevel()
        {
            return false;
        }
        let surface = record.role.wl_surface().clone();
        let surface_id = record.id;
        let start_origin = record.window_origin;
        let committed_maximized = record.committed_maximized;
        let Some(geometry) = record.committed_window_geometry else {
            return false;
        };
        let start_size = (
            geometry.width.round().max(1.0) as i32,
            geometry.height.round().max(1.0) as i32,
        );
        let kind = match part {
            ChromePart::TitlebarDrag => ChromePointerGrabKind::Move,
            ChromePart::Resize(edge) => ChromePointerGrabKind::Resize(edge),
            ChromePart::Button(caption) => ChromePointerGrabKind::Button(caption),
            ChromePart::Content | ChromePart::Outside => return false,
        };
        if committed_maximized && matches!(kind, ChromePointerGrabKind::Resize(_)) {
            return false;
        }

        self.raise_surface(&surface);
        self.arbitrate_keyboard_focus(Some(surface.clone()), false, false);
        self.chrome_pointer_grab = Some(ChromePointerGrab {
            surface: surface.clone(),
            button,
            kind,
            start_pointer: self.cursor_position,
            dragged: false,
        });
        self.set_chrome_cursor_override(match kind {
            ChromePointerGrabKind::Move => Some(ChromeCursorIcon::Move),
            ChromePointerGrabKind::Resize(edge) => Some(chrome_resize_cursor(edge)),
            ChromePointerGrabKind::Button(_) => Some(ChromeCursorIcon::Move),
        });

        match kind {
            ChromePointerGrabKind::Move if !committed_maximized => {
                self.interactive_pointer = Some(InteractivePointer::Move {
                    surface,
                    start_pointer: self.cursor_position,
                    start_origin,
                });
            }
            ChromePointerGrabKind::Move => {}
            ChromePointerGrabKind::Resize(edge) => {
                let edges = xdg_resize_edge(edge);
                self.interactive_pointer = Some(InteractivePointer::Resize {
                    surface: surface.clone(),
                    edges,
                    start_pointer: self.cursor_position,
                    start_origin,
                    start_size,
                });
                let toplevel = self.surfaces.get_mut(&object).and_then(|record| {
                    record.configured_size = start_size;
                    record.role.toplevel().cloned()
                });
                // xdg toplevels get a Resizing configure; X11 windows have no
                // configure cycle — `update_interactive_pointer` issues X
                // configures as the size changes.
                if let Some(toplevel) = toplevel {
                    set_toplevel_configuration(&toplevel, start_size);
                    toplevel.with_pending_state(|state| {
                        state.states.set(xdg_toplevel::State::Resizing);
                    });
                    let _ = self.send_pending_toplevel_configure(&surface, false);
                } else {
                    // The only managed toplevel without an xdg handle is an
                    // X11 window; anything else here lost its role mid-grab,
                    // which the old `.expect` used to make loud.
                    #[cfg(feature = "xwayland")]
                    let role_accounted_for = self
                        .surfaces
                        .get(&object)
                        .is_some_and(|record| record.role.x11().is_some());
                    #[cfg(not(feature = "xwayland"))]
                    let role_accounted_for = false;
                    if !role_accounted_for {
                        debug_assert!(false, "captured resize lost its toplevel role");
                        tracing::error!(
                            surface_id = surface_id.0,
                            "captured resize grab on a managed surface with no toplevel role"
                        );
                    }
                }
            }
            ChromePointerGrabKind::Button(caption) => {
                self.update_chrome_pressed(Some((object, caption, button)));
            }
        }
        tracing::debug!(
            surface_id = surface_id.0,
            ?kind,
            "started chrome pointer capture"
        );
        true
    }

    fn finish_chrome_pointer_grab(&mut self, grab: ChromePointerGrab, time: u32) {
        let action = match grab.kind {
            ChromePointerGrabKind::Button(caption) => matches!(
                self.pointer_target_at(self.cursor_position.0, self.cursor_position.1),
                Some(PointerTarget::Chrome {
                    object,
                    part: ChromePart::Button(released),
                    ..
                }) if object == grab.surface.id() && released == caption
            )
            .then_some(caption),
            ChromePointerGrabKind::Move | ChromePointerGrabKind::Resize(_) => None,
        };
        self.chrome_pointer_grab = None;
        self.finish_interactive_pointer(true);
        self.update_chrome_pressed(None);
        let target = self.pointer_target_at(self.cursor_position.0, self.cursor_position.1);
        self.update_chrome_pointer_from_target(target);

        if matches!(grab.kind, ChromePointerGrabKind::Move) && !grab.dragged {
            self.handle_titlebar_click(&grab.surface, time);
        }
        match action {
            Some(CaptionButton::Close) => self.close_managed_toplevel(&grab.surface),
            Some(CaptionButton::Minimize) => self.minimize_toplevel(&grab.surface),
            Some(CaptionButton::Maximize) => self.toggle_managed_maximized(&grab.surface),
            None => {}
        }
    }

    /// Chrome close for either managed-toplevel protocol: xdg `close` event,
    /// or X11 `WM_DELETE_WINDOW`/destroy via Smithay.
    fn close_managed_toplevel(&mut self, surface: &WlSurface) {
        match self.surfaces.get(&surface.id()).map(|record| &record.role) {
            Some(SurfaceRole::Toplevel(toplevel)) if toplevel.wl_surface().is_alive() => {
                toplevel.send_close();
            }
            #[cfg(feature = "xwayland")]
            Some(SurfaceRole::X11(role)) => {
                if let Err(error) = role.surface.close() {
                    tracing::debug!(%error, "failed to request X11 window close");
                }
            }
            _ => {}
        }
    }

    /// Chrome maximize toggle for either managed-toplevel protocol.
    fn toggle_managed_maximized(&mut self, surface: &WlSurface) {
        let Some(record) = self.surfaces.get(&surface.id()) else {
            return;
        };
        let maximized = record.requested_maximized;
        match &record.role {
            SurfaceRole::Toplevel(_) => self.request_maximized_state(surface, !maximized),
            #[cfg(feature = "xwayland")]
            SurfaceRole::X11(role) => {
                let window = role.surface.clone();
                self.request_x11_maximized(&window, !maximized);
            }
            _ => {}
        }
    }

    fn handle_titlebar_click(&mut self, surface: &WlSurface, time: u32) {
        let position = self.cursor_position;
        let double_click = self
            .titlebar_click_candidate
            .take()
            .is_some_and(|candidate| {
                let dx = position.0 - candidate.position.0;
                let dy = position.1 - candidate.position.1;
                candidate.surface == *surface
                    && time.wrapping_sub(candidate.time) <= TITLEBAR_DOUBLE_CLICK_MILLIS
                    && dx * dx + dy * dy <= TITLEBAR_DOUBLE_CLICK_SLOP * TITLEBAR_DOUBLE_CLICK_SLOP
            });
        if double_click {
            self.toggle_managed_maximized(surface);
        } else {
            self.titlebar_click_candidate = Some(TitlebarClickCandidate {
                surface: surface.clone(),
                position,
                time,
            });
        }
    }

    fn cancel_chrome_pointer_grab(&mut self, send_resize_configure: bool) {
        self.titlebar_click_candidate = None;
        let Some(grab) = self.chrome_pointer_grab.take() else {
            return;
        };
        self.suppressed_chrome_buttons.insert(grab.button);
        self.finish_interactive_pointer(send_resize_configure);
        self.update_chrome_pressed(None);
        self.update_chrome_hover(None);
        self.set_chrome_cursor_override(None);
    }

    fn cancel_chrome_pointer_grab_for_surface(
        &mut self,
        surface: &WlSurface,
        send_resize_configure: bool,
    ) {
        if self
            .titlebar_click_candidate
            .as_ref()
            .is_some_and(|candidate| candidate.surface == *surface)
        {
            self.titlebar_click_candidate = None;
        }
        if self
            .chrome_pointer_grab
            .as_ref()
            .is_some_and(|grab| grab.surface == *surface)
        {
            self.cancel_chrome_pointer_grab(send_resize_configure);
        }
    }

    fn update_chrome_hover(&mut self, hover: Option<(ObjectId, Option<CaptionButton>)>) {
        if self.chrome_hover == hover {
            return;
        }
        let previous = mem::replace(&mut self.chrome_hover, hover.clone());
        match (previous, hover) {
            (Some((previous, _)), Some((next, button))) if previous == next => {
                self.set_surface_chrome_hover(next, button, true);
            }
            (previous, next) => {
                if let Some((object, _)) = previous {
                    self.set_surface_chrome_hover(object, None, false);
                }
                if let Some((object, button)) = next {
                    self.set_surface_chrome_hover(object, button, true);
                }
            }
        }
    }

    fn update_chrome_pressed(&mut self, pressed: Option<(ObjectId, CaptionButton, u32)>) {
        if self.chrome_pressed == pressed {
            return;
        }
        let previous = mem::replace(&mut self.chrome_pressed, pressed.clone());
        match (previous, pressed) {
            (Some((previous, _, _)), Some((next, button, _))) if previous == next => {
                self.set_surface_chrome_pressed(next, Some(button));
            }
            (previous, next) => {
                if let Some((object, _, _)) = previous {
                    self.set_surface_chrome_pressed(object, None);
                }
                if let Some((object, button, _)) = next {
                    self.set_surface_chrome_pressed(object, Some(button));
                }
            }
        }
    }

    fn set_surface_chrome_hover(
        &mut self,
        object: ObjectId,
        hovered_button: Option<CaptionButton>,
        cluster_hovered: bool,
    ) {
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        if record.chrome_pointer.hovered_button == hovered_button
            && record.chrome_pointer.cluster_hovered == cluster_hovered
        {
            return;
        }
        record.chrome_pointer.hovered_button = hovered_button;
        record.chrome_pointer.cluster_hovered = cluster_hovered;
        self.publish_surface_chrome_pointer(object);
    }

    fn set_surface_chrome_pressed(
        &mut self,
        object: ObjectId,
        pressed_button: Option<CaptionButton>,
    ) {
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        if record.chrome_pointer.pressed_button == pressed_button {
            return;
        }
        record.chrome_pointer.pressed_button = pressed_button;
        self.publish_surface_chrome_pointer(object);
    }

    fn publish_surface_chrome_pointer(&mut self, object: ObjectId) {
        let Some(record) = self.surfaces.get_mut(&object) else {
            return;
        };
        sync_toplevel_scene_state(record);
        if record.mapped && record.committed_decoration == SceneDecorationMode::ServerSide {
            self.events.push(ProtocolEvent::SurfaceRelayout {
                id: record.id,
                scene: record.scene_snapshot(),
            });
        }
    }

    fn reset_chrome_pointer_tracking(&mut self, object: &ObjectId) {
        if self
            .chrome_hover
            .as_ref()
            .is_some_and(|(hovered, _)| hovered == object)
        {
            self.chrome_hover = None;
        }
        if self
            .chrome_pressed
            .as_ref()
            .is_some_and(|(pressed, _, _)| pressed == object)
        {
            self.chrome_pressed = None;
        }
        if let Some(record) = self.surfaces.get_mut(object)
            && record.chrome_pointer != ChromePointerSceneState::default()
        {
            record.chrome_pointer = ChromePointerSceneState::default();
            sync_toplevel_scene_state(record);
        }
    }

    /// Min/max size constraints for interactive resize: WM_NORMAL_HINTS for
    /// X11 windows, the xdg surface cached state for everything else.
    fn managed_size_constraints(&self, surface: &WlSurface) -> ((i32, i32), (i32, i32)) {
        #[cfg(feature = "xwayland")]
        if let Some(role) = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| record.role.x11())
        {
            let min = role
                .surface
                .min_size()
                .map(|size| (size.w, size.h))
                .unwrap_or((0, 0));
            let max = role
                .surface
                .max_size()
                .map(|size| (size.w, size.h))
                .unwrap_or((0, 0));
            return (min, max);
        }
        surface_size_constraints(surface)
    }

    /// Resolve a `wl_surface` to its seat focus target: X11-managed roots
    /// focus through Smithay's `X11Surface` (which performs the X half of
    /// focus), everything else through the surface itself.
    fn seat_focus_target_for(&self, surface: &WlSurface) -> SeatFocusTarget {
        #[cfg(feature = "xwayland")]
        if let Some(role) = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| record.role.x11())
        {
            return SeatFocusTarget::X11(role.surface.clone());
        }
        SeatFocusTarget::Wayland(surface.clone())
    }

    fn raise_surface(&mut self, surface: &WlSurface) {
        let Some(band) = self
            .surfaces
            .get(&surface.id())
            .map(|record| record.layout.z.band)
        else {
            return;
        };
        self.restack_role_tree(surface, band, "wayland.focus");
    }

    fn layer_root_object_for_surface(&self, surface: &WlSurface) -> Option<ObjectId> {
        let mut object = surface.id();
        let mut visited = HashSet::new();
        while visited.insert(object.clone()) {
            let record = self.surfaces.get(&object)?;
            if matches!(record.role, SurfaceRole::Layer(_)) {
                return Some(object);
            }
            let parent = record.layout.parent?;
            object = self.surface_objects.get(&parent)?.clone();
        }
        None
    }

    fn layer_keyboard_interactivity_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<KeyboardInteractivity> {
        let root = self.layer_root_object_for_surface(surface)?;
        let SurfaceRole::Layer(role) = &self.surfaces.get(&root)?.role else {
            return None;
        };
        Some(role.surface.cached_state().keyboard_interactivity)
    }

    fn sync_committed_layer_focus_policy(&mut self, surface: &WlSurface) -> bool {
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return false;
        };
        let SurfaceRole::Layer(role) = &mut record.role else {
            return false;
        };
        let state = role.surface.cached_state();
        let changed = role.committed_layer != state.layer
            || role.committed_keyboard_interactivity != state.keyboard_interactivity;
        role.committed_layer = state.layer;
        role.committed_keyboard_interactivity = state.keyboard_interactivity;
        changed
    }

    fn highest_exclusive_layer(&self) -> Option<(ObjectId, WlSurface)> {
        self.surfaces
            .iter()
            .filter_map(|(object, record)| {
                let SurfaceRole::Layer(role) = &record.role else {
                    return None;
                };
                (record.mapped
                    && record.layout.visible
                    && role.surface.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::Exclusive)
                    .then_some((object.clone(), role.surface.wl_surface().clone(), record))
            })
            .max_by(|(_, _, left), (_, _, right)| surface_stack_cmp(left, right))
            .map(|(object, surface, _)| (object, surface))
    }

    fn highest_visible_toplevel_surface(&self) -> Option<WlSurface> {
        self.surfaces
            .values()
            .filter(|record| {
                record.mapped && record.layout.visible && record.role.managed_toplevel()
            })
            .max_by(|left, right| surface_stack_cmp(left, right))
            .map(|record| record.role.wl_surface().clone())
    }

    fn highest_visible_lock_surface(&self) -> Option<WlSurface> {
        self.surfaces
            .values()
            .filter(|record| {
                record.layout.visible
                    && self.surface_is_session_presentable(record)
                    && matches!(record.role, SurfaceRole::LockSurface(_))
            })
            .max_by(|left, right| surface_stack_cmp(left, right))
            .map(|record| record.role.wl_surface().clone())
    }

    fn interaction_focus_root(&self, surface: &WlSurface) -> Option<WlSurface> {
        if let Some(layer_root) = self.layer_root_object_for_surface(surface) {
            return self
                .surfaces
                .get(&layer_root)
                .map(|record| record.role.wl_surface().clone());
        }
        Some(root_compositor_surface(surface))
    }

    fn raise_for_focus_interaction(&mut self, surface: &WlSurface) {
        if self.session_lock_active() {
            return;
        }
        if self.layer_keyboard_interactivity_for_surface(surface)
            == Some(KeyboardInteractivity::None)
        {
            return;
        }
        if let Some(root) = self.interaction_focus_root(surface) {
            self.raise_surface(&root);
        }
    }

    /// Resolve every keyboard-focus entry point through layer-shell policy.
    /// `requested` is an interaction target; `fallback` asks for the highest
    /// normal toplevel when no Exclusive layer is mapped.
    fn arbitrate_keyboard_focus(
        &mut self,
        requested: Option<WlSurface>,
        fallback: bool,
        clear_if_unrequested: bool,
    ) {
        #[cfg(feature = "bus")]
        self.mark_focus_before_change("wayland.focus");
        'focus_policy: {
            let previous_exclusive = self.exclusive_keyboard_focus.take();
            let current_focus_surface = self
                .keyboard
                .current_focus()
                .and_then(|target| target.owned_surface());
            let current_layer_became_none = current_focus_surface.as_ref().is_some_and(|focus| {
                self.layer_keyboard_interactivity_for_surface(focus)
                    == Some(KeyboardInteractivity::None)
            });
            let target = if self.session_lock_active() {
                requested
                    .filter(|surface| {
                        self.surfaces
                            .get(&surface.id())
                            .is_some_and(|record| self.surface_is_input_presentable(record))
                    })
                    .or_else(|| self.highest_visible_lock_surface())
            } else if let Some((object, surface)) = self.highest_exclusive_layer() {
                self.exclusive_keyboard_focus = Some(object);
                Some(surface)
            } else {
                let requested = match requested {
                    Some(surface)
                        if self.layer_keyboard_interactivity_for_surface(&surface)
                            == Some(KeyboardInteractivity::None) =>
                    {
                        // A non-interactive layer receives pointer/touch events but
                        // must not disturb whichever surface owns the keyboard.
                        break 'focus_policy;
                    }
                    Some(surface) => self.interaction_focus_root(&surface),
                    None => None,
                };
                if requested.is_some() {
                    requested
                } else if fallback || current_layer_became_none || previous_exclusive.is_some() {
                    self.highest_visible_toplevel_surface()
                } else if clear_if_unrequested {
                    None
                } else {
                    break 'focus_policy;
                }
            };
            if current_focus_surface == target
                || self.keyboard_focus_is_related_grabbing_popup(target.as_ref())
            {
                break 'focus_policy;
            }
            if self.keyboard.is_grabbed() {
                let popup_root = self
                    .keyboard
                    .grab_start_data()
                    .and_then(|start| start.focus)
                    .and_then(|focus| focus.owned_surface());
                let keyboard = self.keyboard.clone();
                keyboard.unset_grab(self);
                if let Some(root) = popup_root {
                    let pointer_grabs_root = self
                        .pointer
                        .grab_start_data()
                        .and_then(|start| start.focus)
                        .is_some_and(|(focus, _)| {
                            focus
                                .surface()
                                .is_some_and(|focused| focused.as_ref() == &root)
                        });
                    if pointer_grabs_root {
                        self.defer_or_cancel_pointer_grab_for_focus_policy();
                    }
                    self.dismiss_popup_descendants(&root);
                }
            }
            let target = target.map(|surface| self.seat_focus_target_for(&surface));
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }

    fn keyboard_focus_is_related_grabbing_popup(&self, target: Option<&WlSurface>) -> bool {
        let Some(target) = target else {
            return false;
        };
        let Some(current) = self
            .keyboard
            .current_focus()
            .and_then(|focus| focus.owned_surface())
        else {
            return false;
        };
        let Some(grab_root) = self
            .keyboard
            .grab_start_data()
            .and_then(|start| start.focus)
            .and_then(|focus| focus.owned_surface())
        else {
            return false;
        };
        let target_root = canonical_root_surface(&self.popup_manager, target);
        canonical_root_surface(&self.popup_manager, &grab_root) == target_root
            && canonical_root_surface(&self.popup_manager, &current) == target_root
    }

    fn focus_highest_visible_toplevel(&mut self) {
        self.arbitrate_keyboard_focus(None, true, false);
    }

    fn retarget_pointer_after_visibility_change(&mut self) {
        let (x, y) = self.cursor_position;
        let target = if self.pointer.is_grabbed() {
            self.client_pointer_target_at(x, y)
        } else {
            self.pointer_target_at(x, y)
        };
        self.update_chrome_pointer_from_target(target.clone());
        let focus = match target {
            Some(PointerTarget::Client { surface, origin }) => {
                Some((self.seat_focus_target_for(&surface), origin))
            }
            Some(PointerTarget::Chrome { .. }) | None => None,
        };
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            focus.clone(),
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: monotonic_millis(),
            },
        );
        pointer.frame(self);
        self.record_pointer_focus_local_position(focus.as_ref(), (x, y));
    }

    fn minimize_toplevel(&mut self, surface: &WlSurface) {
        self.cancel_chrome_pointer_grab_for_surface(surface, true);
        self.titlebar_click_candidate = None;
        self.reset_chrome_pointer_tracking(&surface.id());
        let Some((object, _id)) = self.surfaces.get_mut(&surface.id()).and_then(|record| {
            if !record.mapped || record.minimized || !record.role.managed_toplevel() {
                return None;
            }
            record.minimized = true;
            Some((surface.id(), record.id))
        }) else {
            return;
        };
        // X11 windows also learn the state through EWMH so the client can
        // stop rendering.
        #[cfg(feature = "xwayland")]
        if let Some(role) = self
            .surfaces
            .get(&object)
            .and_then(|record| record.role.x11())
        {
            let _ = role.surface.set_suspended(true);
        }
        self.minimized_toplevels.retain(|entry| *entry != object);
        self.minimized_toplevels.push(object);
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(_id, "wayland.focus");
        self.recompute_effective_visibility();
        self.focus_highest_visible_toplevel();
        self.retarget_pointer_after_visibility_change();
    }

    fn restore_most_recently_minimized(&mut self) {
        while let Some(object) = self.minimized_toplevels.pop() {
            let restored = self.surfaces.get_mut(&object).and_then(|record| {
                if !record.mapped || !record.minimized || !record.role.managed_toplevel() {
                    return None;
                }
                record.minimized = false;
                Some((record.role.wl_surface().clone(), record.id))
            });
            let Some((surface, _id)) = restored else {
                continue;
            };
            #[cfg(feature = "xwayland")]
            if let Some(role) = self
                .surfaces
                .get(&object)
                .and_then(|record| record.role.x11())
            {
                let _ = role.surface.set_suspended(false);
            }
            #[cfg(feature = "bus")]
            self.mark_surface_dirty(_id, "wayland.focus");
            self.recompute_effective_visibility();
            self.raise_surface(&surface);
            self.arbitrate_keyboard_focus(Some(surface), false, false);
            self.retarget_pointer_after_visibility_change();
            return;
        }
    }

    fn logical_output_rect(&self) -> LogicalOutputRect {
        let (x, y, width, height) = self.backend.logical_output_rect();
        LogicalOutputRect {
            x: x as f32,
            y: y as f32,
            width: width as f32,
            height: height as f32,
        }
    }

    fn usable_output_rect(&self) -> LogicalOutputRect {
        let Some(output) = self.backend.default_output() else {
            return self.logical_output_rect();
        };
        let layer_map = layer_map_for_output(&output);
        if layer_map.layers().next().is_none() {
            return self.logical_output_rect();
        }
        let zone = layer_map.non_exclusive_zone();
        let origin = output.current_location();
        LogicalOutputRect {
            x: (origin.x + zone.loc.x) as f32,
            y: (origin.y + zone.loc.y) as f32,
            width: zone.size.w.max(0) as f32,
            height: zone.size.h.max(0) as f32,
        }
    }

    #[cfg(feature = "bus")]
    fn port_usable_output_rect_for(&self, output: &Output) -> Option<LogicalOutputRect> {
        let own_rect = match output.current_mode() {
            Some(mode) => {
                let size = mode
                    .size
                    .to_f64()
                    .to_logical(output.current_scale().fractional_scale())
                    .to_i32_round::<i32>();
                let size = output.current_transform().transform_size(size);
                let origin = output.current_location();
                port_snapshot::exact_logical_output_rect(
                    origin.x,
                    origin.y,
                    size.w.max(0),
                    size.h.max(0),
                )?
            }
            None => {
                let (x, y, width, height) = self.backend.logical_output_rect();
                port_snapshot::exact_logical_output_rect(
                    x,
                    y,
                    i32::try_from(width).ok()?,
                    i32::try_from(height).ok()?,
                )?
            }
        };
        let layer_map = layer_map_for_output(output);
        if layer_map.layers().next().is_none() {
            return Some(own_rect);
        }
        let zone = layer_map.non_exclusive_zone();
        let origin = output.current_location();
        port_snapshot::exact_logical_output_rect(
            origin.x.checked_add(zone.loc.x)?,
            origin.y.checked_add(zone.loc.y)?,
            zone.size.w.max(0),
            zone.size.h.max(0),
        )
    }

    fn request_maximized_state(&mut self, surface: &WlSurface, maximized: bool) {
        self.cancel_chrome_pointer_grab_for_surface(surface, true);
        self.titlebar_click_candidate = None;
        let output = self.usable_output_rect();
        let extents = DecoExtents::of(&self.decoration.theme);
        let theme = self.decoration.theme.clone();
        let configured_server_side = compositor::with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("toplevel owns xdg role state")
                .lock()
                .expect("xdg toplevel state lock");
            attributes
                .server_pending
                .as_ref()
                .unwrap_or_else(|| attributes.current_server_state())
                .decoration_mode
                == Some(DecorationMode::ServerSide)
        });
        let Some((toplevel, snapshot)) = self.surfaces.get_mut(&surface.id()).and_then(|record| {
            let toplevel = record.role.toplevel()?.clone();
            let server_side = configured_server_side;
            let mut normal_restore = record
                .normal_restore
                .or_else(|| {
                    record
                        .pending_window_state
                        .and_then(|state| state.normal_restore)
                })
                .unwrap_or_else(|| NormalRestore {
                    window_origin: record.window_origin,
                    client_size: record.committed_window_geometry.map_or(
                        record.configured_size,
                        |geometry| {
                            (
                                geometry.width.round().max(1.0) as i32,
                                geometry.height.round().max(1.0) as i32,
                            )
                        },
                    ),
                    output,
                    server_side: record.committed_decoration == SceneDecorationMode::ServerSide,
                });
            match (normal_restore.server_side, server_side) {
                (false, true) => {
                    normal_restore.window_origin.0 += extents.left;
                    normal_restore.window_origin.1 += extents.top;
                }
                (true, false) => {
                    normal_restore.window_origin.0 -= extents.left;
                    normal_restore.window_origin.1 -= extents.top;
                }
                _ => {}
            }
            normal_restore.server_side = server_side;
            let (window_origin, client_size, retained_restore) = if maximized {
                let outer = vec2(output.width, output.height);
                let content = if server_side {
                    extents.content_size_for_window(outer)
                } else {
                    outer
                };
                (
                    if server_side {
                        (output.x + extents.left, output.y + extents.top)
                    } else {
                        (output.x, output.y)
                    },
                    (
                        content.x.round().max(1.0) as i32,
                        content.y.round().max(1.0) as i32,
                    ),
                    Some(normal_restore),
                )
            } else {
                let restored = clamp_normal_restore(normal_restore, output, server_side, &theme);
                (restored.window_origin, restored.client_size, Some(restored))
            };
            let snapshot = WindowStateSnapshot {
                maximized,
                window_origin,
                client_size,
                normal_restore: retained_restore,
            };
            record.requested_maximized = maximized;
            if maximized {
                record.normal_restore = Some(normal_restore);
            }
            record.pending_window_state = Some(snapshot);
            record.configured_size = client_size;
            Some((toplevel, snapshot))
        }) else {
            return;
        };

        toplevel.with_pending_state(|state| {
            state.size = Some(snapshot.client_size.into());
            if maximized {
                state.states.set(xdg_toplevel::State::Maximized);
            } else {
                state.states.unset(xdg_toplevel::State::Maximized);
            }
        });
        let _ = self.send_pending_toplevel_configure(surface, true);
    }

    fn reconfigure_window_states_for_output(&mut self) {
        let window_states = self
            .surfaces
            .values()
            .filter(|record| {
                matches!(record.role, SurfaceRole::Toplevel(_))
                    && (record.committed_maximized
                        || record.requested_maximized
                        || record.pending_window_state.is_some())
            })
            .map(|record| (record.role.wl_surface().clone(), record.requested_maximized))
            .collect::<Vec<_>>();
        for (surface, requested_maximized) in window_states {
            self.request_maximized_state(&surface, requested_maximized);
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_output_after_topology_change_if_needed(
        &mut self,
        previous_output: LogicalOutputRect,
        previous_usable: LogicalOutputRect,
    ) {
        if self.logical_output_rect() != previous_output
            || self.usable_output_rect() != previous_usable
        {
            self.reconcile_output_geometry_after_topology_change();
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_output_geometry_after_topology_change(&mut self) {
        self.begin_pointer_hit_test_batch();
        self.arrange_all_layer_outputs();
        self.reconfigure_window_states_for_output();
        self.invalidate_pointer_hit_test_geometry();
        self.end_pointer_hit_test_batch();
    }

    fn resize_output(&mut self, width: u32, height: u32) {
        #[cfg(feature = "bus")]
        if let Some(output) = self.backend.default_output() {
            self.mark_output_before_change(&output, "output.geometry");
        }
        if !self.backend.resize_host_output((width, height)) {
            return;
        }
        let captures = self.capture_frames.keys().copied().collect::<Vec<_>>();
        for id in captures {
            self.fail_capture(id);
        }
        self.events
            .push(ProtocolEvent::OutputResized { width, height });
        self.reconfigure_lock_surfaces();
        self.begin_pointer_hit_test_batch();
        self.arrange_all_layer_outputs();
        let usable = self.usable_output_rect();

        let mut shifted_roots = Vec::new();
        let mut configure_surfaces = Vec::new();
        #[cfg(feature = "bus")]
        let mut observed_toplevels = Vec::new();
        let decoration_theme = self.decoration.theme.clone();
        for record in self.surfaces.values_mut() {
            if !matches!(record.role, SurfaceRole::Toplevel(_)) {
                continue;
            }
            #[cfg(feature = "bus")]
            observed_toplevels.push(record.id);
            if record.committed_maximized
                || record.requested_maximized
                || record.pending_window_state.is_some()
            {
                continue;
            }
            let server_side = record.committed_decoration == SceneDecorationMode::ServerSide;
            let delta = if server_side {
                let extents = DecoExtents::of(&decoration_theme);
                let content_size = record.committed_window_geometry.map_or(
                    vec2(
                        record.configured_size.0 as f32,
                        record.configured_size.1 as f32,
                    ),
                    |geometry| vec2(geometry.width, geometry.height),
                );
                let window = ChromeLayout::compute(&decoration_theme, content_size).window;
                let outer_origin = (
                    record.window_origin.0 - extents.left,
                    record.window_origin.1 - extents.top,
                );
                let max_x = (usable.x + usable.width - window.w - OUTPUT_MARGIN).max(usable.x);
                let max_y = (usable.y + usable.height - window.h - OUTPUT_MARGIN).max(usable.y);
                let clamped = (
                    outer_origin.0.clamp(usable.x, max_x),
                    outer_origin.1.clamp(usable.y, max_y),
                );
                let delta = (clamped.0 - outer_origin.0, clamped.1 - outer_origin.1);
                record.layout.x += delta.0;
                record.layout.y += delta.1;
                record.window_origin.0 += delta.0;
                record.window_origin.1 += delta.1;
                delta
            } else {
                let previous_origin = (record.layout.x, record.layout.y);
                let max_x =
                    (usable.x + usable.width - record.layout.width - OUTPUT_MARGIN).max(usable.x);
                let max_y =
                    (usable.y + usable.height - record.layout.height - OUTPUT_MARGIN).max(usable.y);
                record.layout.x = record.layout.x.clamp(usable.x, max_x);
                record.layout.y = record.layout.y.clamp(usable.y, max_y);
                record.window_origin.0 += record.layout.x - previous_origin.0;
                record.window_origin.1 += record.layout.y - previous_origin.1;
                (
                    record.layout.x - previous_origin.0,
                    record.layout.y - previous_origin.1,
                )
            };
            if delta != (0.0, 0.0) {
                shifted_roots.push((record.id, delta));
            }

            let (max_width, max_height) = if server_side {
                let extents = DecoExtents::of(&decoration_theme);
                let outer_origin = (
                    record.window_origin.0 - extents.left,
                    record.window_origin.1 - extents.top,
                );
                let available = extents.content_size_for_window(vec2(
                    usable.x + usable.width - outer_origin.0 - OUTPUT_MARGIN,
                    usable.y + usable.height - outer_origin.1 - OUTPUT_MARGIN,
                ));
                (available.x.max(240.0) as i32, available.y.max(160.0) as i32)
            } else {
                (
                    (usable.x + usable.width - record.layout.x - OUTPUT_MARGIN).max(240.0) as i32,
                    (usable.y + usable.height - record.layout.y - OUTPUT_MARGIN).max(160.0) as i32,
                )
            };
            let new_size = (
                record.configured_size.0.min(max_width),
                record.configured_size.1.min(max_height),
            );
            if new_size != record.configured_size {
                let surface = record.role.wl_surface().clone();
                set_toplevel_configuration(
                    record
                        .role
                        .toplevel()
                        .expect("toplevel records have a toplevel handle"),
                    new_size,
                );
                record.configured_size = new_size;
                configure_surfaces.push(surface);
            }
            self.events.push(ProtocolEvent::SurfaceRelayout {
                id: record.id,
                scene: record.scene_snapshot(),
            });
        }
        for (id, delta) in shifted_roots {
            self.shift_surface_descendants(id, delta);
        }
        for surface in configure_surfaces {
            let _ = self.send_pending_toplevel_configure(&surface, true);
        }
        #[cfg(feature = "bus")]
        for id in observed_toplevels {
            self.mark_surface_dirty(id, "output.geometry");
        }
        self.reconfigure_window_states_for_output();
        self.invalidate_pointer_hit_test_geometry();
        self.end_pointer_hit_test_batch();
    }

    fn change_output_scale(&mut self, scale: f64) {
        #[cfg(feature = "bus")]
        if let Some(output) = self.backend.default_output() {
            self.mark_output_before_change(&output, "output.geometry");
        }
        if !self.backend.change_host_output_scale(scale) {
            return;
        }
        let captures = self.capture_frames.keys().copied().collect::<Vec<_>>();
        for id in captures {
            self.fail_capture(id);
        }
        // The host window's backend scale is a fractional-scale preference,
        // not the nested output's integer coordinate scale. Keep
        // wl_output.scale at 1 as the legacy-client fallback.
        self.publish_surface_preferred_scale(scale);
        tracing::info!(scale, "nested output scale changed");
    }

    fn publish_surface_preferred_scale(&self, scale: f64) {
        for record in self.surfaces.values() {
            compositor::with_states(record.role.wl_surface(), |states| {
                fractional_scale::with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(scale);
                });
            });
        }
    }

    fn update_interactive_pointer(&mut self, x: f64, y: f64) -> bool {
        let Some(interaction) = self.interactive_pointer.clone() else {
            return false;
        };
        match interaction {
            InteractivePointer::Move {
                surface,
                start_pointer,
                start_origin,
            } => {
                let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                    self.interactive_pointer = None;
                    return false;
                };
                let old_origin = record.window_origin;
                record.window_origin = (
                    start_origin.0 + (x - start_pointer.0) as f32,
                    start_origin.1 + (y - start_pointer.1) as f32,
                );
                let offset = record
                    .committed_window_geometry
                    .map(|geometry| (geometry.x, geometry.y))
                    .unwrap_or_default();
                record.layout.x = record.window_origin.0 - offset.0;
                record.layout.y = record.window_origin.1 - offset.1;
                let delta = (
                    record.window_origin.0 - old_origin.0,
                    record.window_origin.1 - old_origin.1,
                );
                let id = record.id;
                let scene = record.scene_snapshot();
                // An X11 window must learn its new position through an X
                // configure (there is no xdg configure for it), or the client
                // keeps stale global coordinates.
                #[cfg(feature = "xwayland")]
                if delta != (0.0, 0.0)
                    && let SurfaceRole::X11(role) = &mut record.role
                {
                    let rect = Rectangle::new(
                        (record.window_origin.0 as i32, record.window_origin.1 as i32).into(),
                        (
                            record.configured_size.0.max(1),
                            record.configured_size.1.max(1),
                        )
                            .into(),
                    );
                    role.granted_geometry = rect;
                    if let Err(error) = role.surface.configure(Some(rect)) {
                        tracing::debug!(%error, "failed to send X11 move configure");
                    }
                }
                self.events
                    .push(ProtocolEvent::SurfaceRelayout { id, scene });
                #[cfg(feature = "bus")]
                self.mark_surface_dirty(id, "wayland.map");
                self.shift_surface_descendants(id, delta);
                delta != (0.0, 0.0)
            }
            InteractivePointer::Resize {
                surface,
                edges,
                start_pointer,
                start_origin,
                start_size,
            } => {
                let dx = (x - start_pointer.0).round() as i32;
                let dy = (y - start_pointer.1).round() as i32;
                let left = matches!(
                    edges,
                    xdg_toplevel::ResizeEdge::Left
                        | xdg_toplevel::ResizeEdge::TopLeft
                        | xdg_toplevel::ResizeEdge::BottomLeft
                );
                let right = matches!(
                    edges,
                    xdg_toplevel::ResizeEdge::Right
                        | xdg_toplevel::ResizeEdge::TopRight
                        | xdg_toplevel::ResizeEdge::BottomRight
                );
                let top = matches!(
                    edges,
                    xdg_toplevel::ResizeEdge::Top
                        | xdg_toplevel::ResizeEdge::TopLeft
                        | xdg_toplevel::ResizeEdge::TopRight
                );
                let bottom = matches!(
                    edges,
                    xdg_toplevel::ResizeEdge::Bottom
                        | xdg_toplevel::ResizeEdge::BottomLeft
                        | xdg_toplevel::ResizeEdge::BottomRight
                );
                let width_delta = if left {
                    -dx
                } else if right {
                    dx
                } else {
                    0
                };
                let height_delta = if top {
                    -dy
                } else if bottom {
                    dy
                } else {
                    0
                };
                let (min_size, max_size) =
                    clamped_toplevel_constraints(self.managed_size_constraints(&surface));
                let min_width = min_size.0;
                let min_height = min_size.1;
                let max_width = max_size.0;
                let max_height = max_size.1;
                let new_size = (
                    start_size
                        .0
                        .saturating_add(width_delta)
                        .clamp(min_width, max_width),
                    start_size
                        .1
                        .saturating_add(height_delta)
                        .clamp(min_height, max_height),
                );
                let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                    self.interactive_pointer = None;
                    return false;
                };
                if new_size == record.configured_size {
                    return false;
                }
                let old_origin = record.window_origin;
                record.window_origin = (
                    if left {
                        start_origin.0 + (start_size.0 - new_size.0) as f32
                    } else {
                        start_origin.0
                    },
                    if top {
                        start_origin.1 + (start_size.1 - new_size.1) as f32
                    } else {
                        start_origin.1
                    },
                );
                let offset = record
                    .committed_window_geometry
                    .map(|geometry| (geometry.x, geometry.y))
                    .unwrap_or_default();
                record.layout.x = record.window_origin.0 - offset.0;
                record.layout.y = record.window_origin.1 - offset.1;
                record.configured_size = new_size;
                let toplevel = record.role.toplevel().cloned();
                let id = record.id;
                let scene = record.scene_snapshot();
                let delta = (
                    record.window_origin.0 - old_origin.0,
                    record.window_origin.1 - old_origin.1,
                );
                // X11 interactive resize is granted through X configures; the
                // committed buffer remains the presentation authority.
                #[cfg(feature = "xwayland")]
                if let SurfaceRole::X11(role) = &mut record.role {
                    let rect = Rectangle::new(
                        (record.window_origin.0 as i32, record.window_origin.1 as i32).into(),
                        (new_size.0.max(1), new_size.1.max(1)).into(),
                    );
                    role.granted_geometry = rect;
                    if let Err(error) = role.surface.configure(Some(rect)) {
                        tracing::debug!(%error, "failed to send X11 resize configure");
                    }
                }
                if let Some(toplevel) = toplevel {
                    set_toplevel_configuration(&toplevel, new_size);
                    let _ = self.send_pending_toplevel_configure(&surface, true);
                }
                self.events
                    .push(ProtocolEvent::SurfaceRelayout { id, scene });
                #[cfg(feature = "bus")]
                self.mark_surface_dirty(id, "wayland.map");
                self.shift_surface_descendants(id, delta);
                true
            }
        }
    }

    fn shift_surface_descendants(&mut self, parent: SurfaceId, delta: (f32, f32)) {
        let children = child_surface_ids(&self.surfaces);
        let mut stack = children.get(&parent).cloned().unwrap_or_default();
        #[cfg(feature = "bus")]
        let mut observed = Vec::new();
        while let Some(child_id) = stack.pop() {
            let Some(object) = self.surface_objects.get(&child_id).cloned() else {
                continue;
            };
            let Some(record) = self.surfaces.get_mut(&object) else {
                continue;
            };
            record.layout.x += delta.0;
            record.layout.y += delta.1;
            record.window_origin.0 += delta.0;
            record.window_origin.1 += delta.1;
            if let Some(pending) = record.pending_popup_reposition.as_mut() {
                pending.layout.x += delta.0;
                pending.layout.y += delta.1;
                pending.window_origin.0 += delta.0;
                pending.window_origin.1 += delta.1;
            }
            let scene = record.scene_snapshot();
            if record.mapped {
                self.events.push(ProtocolEvent::SurfaceRelayout {
                    id: child_id,
                    scene,
                });
                #[cfg(feature = "bus")]
                observed.push(child_id);
            }
            if let Some(descendants) = children.get(&child_id) {
                stack.extend(descendants.iter().copied());
            }
        }
        #[cfg(feature = "bus")]
        for id in observed {
            self.mark_surface_dirty(id, "wayland.map");
        }
    }

    fn finish_interactive_pointer(&mut self, send_configure: bool) {
        let Some(interaction) = self.interactive_pointer.take() else {
            return;
        };
        if let InteractivePointer::Resize { surface, .. } = interaction
            && let Some(toplevel) = self
                .surfaces
                .get(&surface.id())
                .and_then(|record| record.role.toplevel())
                .cloned()
        {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
            });
            if send_configure {
                let _ = self.send_pending_toplevel_configure(&surface, false);
            }
        }
    }

    fn resolve_popup_geometry(
        &self,
        parent: &WlSurface,
        positioner: PositionerState,
    ) -> Option<ResolvedPopupGeometry> {
        let parent = self.surfaces.get(&parent.id())?;
        let parent_window_offset = parent
            .committed_window_geometry
            .map(|geometry| (geometry.x, geometry.y))
            .unwrap_or_default();
        let parent_window_origin = (
            parent.layout.x + parent_window_offset.0,
            parent.layout.y + parent_window_offset.1,
        );
        let target = Rectangle::new(
            (
                -(parent_window_origin.0.round() as i32),
                -(parent_window_origin.1.round() as i32),
            )
                .into(),
            (
                self.backend.logical_output_size().0 as i32,
                self.backend.logical_output_size().1 as i32,
            )
                .into(),
        );
        let geometry = positioner.get_unconstrained_geometry(target);
        let window_origin = (
            parent_window_origin.0 + geometry.loc.x as f32,
            parent_window_origin.1 + geometry.loc.y as f32,
        );
        Some(ResolvedPopupGeometry {
            geometry,
            layout: SurfaceLayout {
                x: window_origin.0,
                y: window_origin.1,
                width: geometry.size.w as f32,
                height: geometry.size.h as f32,
                z: parent.layout.z,
                source: None,
                parent: Some(parent.id),
                transform: SurfaceTransform::Normal,
                visible: false,
                toplevel: None,
            },
            window_origin,
        })
    }

    fn refresh_subsurface_position(&mut self, surface: &WlSurface) {
        let Some((parent, location)) = self.surfaces.get(&surface.id()).and_then(|record| {
            let parent = record.role.parent_surface()?.clone();
            let location = compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SubsurfaceCachedState>()
                    .current()
                    .location
            });
            Some((parent, location))
        }) else {
            return;
        };
        let Some((parent_id, parent_x, parent_y)) = self
            .surfaces
            .get(&parent.id())
            .map(|record| (record.id, record.layout.x, record.layout.y))
        else {
            return;
        };
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        record.layout.parent = Some(parent_id);
        record.layout.x = parent_x + location.x as f32;
        record.layout.y = parent_y + location.y as f32;
        record.window_origin = (record.layout.x, record.layout.y);
        if record.mapped {
            self.events.push(ProtocolEvent::SurfaceRelayout {
                id: record.id,
                scene: record.scene_snapshot(),
            });
        }
    }

    fn effective_window_geometry(
        &mut self,
        surface: &WlSurface,
        presented_size: (f32, f32),
    ) -> SceneWindowGeometry {
        #[cfg(test)]
        {
            self.effective_window_geometry_calls =
                self.effective_window_geometry_calls.saturating_add(1);
        }
        let Some(root) = self.surfaces.get(&surface.id()) else {
            return clamp_window_geometry(surface, surface_tree_bounds(presented_size, &[]));
        };
        let root_origin = (root.layout.x, root.layout.y);
        let mut left = 0.0_f32;
        let mut top = 0.0_f32;
        let mut right = presented_size.0;
        let mut bottom = presented_size.1;
        with_surface_tree_downward(
            surface,
            (),
            |child, _, &()| {
                if child == surface {
                    return TraversalAction::DoChildren(());
                }
                if self.surfaces.get(&child.id()).is_some_and(|record| {
                    matches!(record.role, SurfaceRole::Subsurface { .. })
                        && record.parent_association_committed
                        && record.mapped
                }) {
                    TraversalAction::DoChildren(())
                } else {
                    TraversalAction::SkipChildren
                }
            },
            |child, _, &()| {
                if child == surface {
                    return;
                }
                let Some(record) = self.surfaces.get(&child.id()) else {
                    return;
                };
                if !matches!(record.role, SurfaceRole::Subsurface { .. })
                    || !record.parent_association_committed
                    || !record.mapped
                {
                    return;
                }
                let x = record.layout.x - root_origin.0;
                let y = record.layout.y - root_origin.1;
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + record.layout.width);
                bottom = bottom.max(y + record.layout.height);
            },
            |_, _, &()| true,
        );
        clamp_window_geometry(
            surface,
            SceneWindowGeometry {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            },
        )
    }

    fn committed_toplevel_window_geometry(
        &mut self,
        surface: &WlSurface,
        presented_size: (f32, f32),
        changed: bool,
    ) -> Option<SceneWindowGeometry> {
        let (explicit, existing) = {
            let record = self.surfaces.get(&surface.id())?;
            // X11 content geometry is always the full committed buffer at
            // origin: SSD sits outside it, and X outer-frame coordinates must
            // never be fed into `SceneWindowGeometry` (they live in
            // `window_origin`/`granted_geometry` instead).
            #[cfg(feature = "xwayland")]
            if matches!(record.role, SurfaceRole::X11(_)) {
                return Some(SceneWindowGeometry {
                    x: 0.0,
                    y: 0.0,
                    width: presented_size.0,
                    height: presented_size.1,
                });
            }
            if !matches!(record.role, SurfaceRole::Toplevel(_)) {
                return None;
            }
            (
                window_geometry_is_explicit(surface),
                record.committed_window_geometry,
            )
        };
        let geometry = match existing {
            Some(existing) if !changed && explicit => existing,
            _ => self.effective_window_geometry(surface, presented_size),
        };
        if let Some(record) = self.surfaces.get_mut(&surface.id()) {
            record.committed_window_geometry_explicit = explicit;
        }
        Some(geometry)
    }

    fn toplevel_root_for_surface(&self, surface: &WlSurface) -> Option<WlSurface> {
        let mut current = surface.clone();
        loop {
            let record = self.surfaces.get(&current.id())?;
            match &record.role {
                SurfaceRole::Subsurface { parent, .. } => current = parent.clone(),
                SurfaceRole::Toplevel(_) => return Some(current),
                #[cfg(feature = "xwayland")]
                SurfaceRole::X11(_) => return Some(current),
                SurfaceRole::Popup(_)
                | SurfaceRole::Layer(_)
                | SurfaceRole::LockSurface(_)
                | SurfaceRole::Dormant(_) => {
                    return None;
                }
            }
        }
    }

    fn refresh_ancestor_window_geometry(&mut self, surface: &WlSurface) {
        let Some(root) = self.toplevel_root_for_surface(surface) else {
            return;
        };
        if root == *surface {
            return;
        }
        self.refresh_toplevel_window_geometry(&root);
    }

    fn refresh_toplevel_window_geometry(&mut self, root: &WlSurface) {
        let Some((size, old_layout, window_origin, mapped, id, explicit)) =
            self.surfaces.get(&root.id()).map(|record| {
                (
                    (record.layout.width, record.layout.height),
                    (record.layout.x, record.layout.y),
                    record.window_origin,
                    record.mapped,
                    record.id,
                    record.committed_window_geometry_explicit,
                )
            })
        else {
            return;
        };
        if explicit {
            return;
        }
        let geometry = self.effective_window_geometry(root, size);
        let new_layout = (window_origin.0 - geometry.x, window_origin.1 - geometry.y);
        let Some(record) = self.surfaces.get_mut(&root.id()) else {
            return;
        };
        if record.committed_window_geometry == Some(geometry) && old_layout == new_layout {
            return;
        }
        record.layout.x = new_layout.0;
        record.layout.y = new_layout.1;
        record.committed_window_geometry = Some(geometry);
        sync_toplevel_scene_state(record);
        let scene = record.scene_snapshot();
        if mapped {
            self.events
                .push(ProtocolEvent::SurfaceRelayout { id, scene });
        }
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(id, "wayland.map");
        let delta = (new_layout.0 - old_layout.0, new_layout.1 - old_layout.1);
        if delta != (0.0, 0.0) {
            self.shift_surface_descendants(id, delta);
        }
        self.refresh_chrome_pointer_after_scene_change();
    }

    fn apply_committed_toplevel_state(
        &mut self,
        surface: &WlSurface,
        scene_commit: SceneCommitCachedState,
    ) {
        let extents = DecoExtents::of(&self.decoration.theme);
        let (shifted, clear_chrome_pointer, chrome_scene_changed) = {
            let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                return;
            };
            if !matches!(record.role, SurfaceRole::Toplevel(_)) {
                return;
            }
            let old_layout = (record.layout.x, record.layout.y);
            let previous = record.committed_decoration;
            if scene_commit.decoration_reverts {
                record.committed_decoration = SceneDecorationMode::ClientSide;
            } else if let Some(decoration) = scene_commit.acknowledged_decoration {
                record.committed_decoration = decoration;
            }
            let delta = match (
                previous == SceneDecorationMode::ServerSide,
                record.committed_decoration == SceneDecorationMode::ServerSide,
            ) {
                (false, true) => (extents.left, extents.top),
                (true, false) => (-extents.left, -extents.top),
                _ => (0.0, 0.0),
            };
            record.window_origin.0 += delta.0;
            record.window_origin.1 += delta.1;
            if let Some(restore) = record.normal_restore.as_mut() {
                let server_side = record.committed_decoration == SceneDecorationMode::ServerSide;
                match (restore.server_side, server_side) {
                    (false, true) => {
                        restore.window_origin.0 += extents.left;
                        restore.window_origin.1 += extents.top;
                    }
                    (true, false) => {
                        restore.window_origin.0 -= extents.left;
                        restore.window_origin.1 -= extents.top;
                    }
                    _ => {}
                }
                restore.server_side = server_side;
            }
            if let Some(window_state) = scene_commit.acknowledged_window_state {
                #[cfg(test)]
                self.committed_window_state_transitions
                    .push(window_state.maximized);
                record.committed_maximized = window_state.maximized;
                record.window_origin = window_state.window_origin;
                record.configured_size = window_state.client_size;
                record.normal_restore = if window_state.maximized {
                    window_state.normal_restore.or(record.normal_restore)
                } else {
                    None
                };
                if record.pending_window_state == Some(window_state) {
                    record.pending_window_state = None;
                }
            }
            let geometry_offset = record
                .committed_window_geometry
                .map(|geometry| (geometry.x, geometry.y))
                .unwrap_or_default();
            record.layout.x = record.window_origin.0 - geometry_offset.0;
            record.layout.y = record.window_origin.1 - geometry_offset.1;
            sync_toplevel_scene_state(record);
            let layout_delta = (
                record.layout.x - old_layout.0,
                record.layout.y - old_layout.1,
            );
            (
                (layout_delta != (0.0, 0.0)).then_some((record.id, layout_delta)),
                previous == SceneDecorationMode::ServerSide
                    && record.committed_decoration != SceneDecorationMode::ServerSide,
                layout_delta != (0.0, 0.0)
                    || previous != record.committed_decoration
                    || scene_commit.acknowledged_window_state.is_some(),
            )
        };
        if clear_chrome_pointer {
            self.cancel_chrome_pointer_grab_for_surface(surface, true);
            self.reset_chrome_pointer_tracking(&surface.id());
        }
        if let Some((id, delta)) = shifted {
            self.shift_surface_descendants(id, delta);
        }
        if chrome_scene_changed {
            #[cfg(feature = "bus")]
            if let Some(id) = self.surfaces.get(&surface.id()).map(|record| record.id) {
                self.mark_surface_dirty(id, "wayland.map");
            }
            self.refresh_chrome_pointer_after_scene_change();
        }
    }

    fn retain_buffer(&mut self, buffer: wl_buffer::WlBuffer) -> u64 {
        let token = loop {
            let candidate = self.next_buffer_token;
            self.next_buffer_token = self.next_buffer_token.wrapping_add(1);
            if !self.retained_buffers.tokens.contains_key(&candidate) {
                break candidate;
            }
        };
        self.retained_buffers.retain(token, buffer.id(), buffer);
        token
    }

    fn dmabuf_buffer_identity(&mut self, buffer: &wl_buffer::WlBuffer) -> (DmabufBufferId, bool) {
        if let Some(buffer_id) = self.dmabuf_buffer_ids.get(&buffer.id()) {
            return (*buffer_id, true);
        }
        let buffer_id = loop {
            let candidate = DmabufBufferId(self.next_dmabuf_buffer_id);
            self.next_dmabuf_buffer_id = self.next_dmabuf_buffer_id.wrapping_add(1);
            if !self
                .dmabuf_buffer_ids
                .values()
                .any(|current| *current == candidate)
            {
                break candidate;
            }
        };
        let cacheable =
            buffer.is_alive() && self.dmabuf_buffer_ids.len() < MAX_DMABUF_CACHE_IDENTITIES;
        if cacheable {
            self.dmabuf_buffer_ids.insert(buffer.id(), buffer_id);
        }
        (buffer_id, cacheable)
    }

    fn dmabuf_buffer_is_cacheable(
        &self,
        buffer: &wl_buffer::WlBuffer,
        buffer_id: DmabufBufferId,
    ) -> bool {
        buffer.is_alive() && self.dmabuf_buffer_ids.get(&buffer.id()) == Some(&buffer_id)
    }

    fn take_committed_release_point(
        &mut self,
        surface: &WlSurface,
    ) -> Option<CommittedReleasePoint> {
        // CompositorHandler::commit is invoked only for an applied transaction.
        #[cfg(test)]
        let fake_point = self.committed_release_point_override.take();
        compositor::with_states(surface, |states| {
            let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
            // Offline tests cannot populate this Smithay slot: DrmSyncPoint requires a DRM-backed timeline.
            let committed = committed_syncobj_state(&mut cached);
            #[cfg(test)]
            if let Some(point) = fake_point {
                return Some(CommittedReleasePoint::Fake(point));
            }
            committed
                .release_point
                // Taking this point deliberately disables both Smithay 0.7.0
                // safety nets: Cacheable::merge_into signals a superseded
                // current point, and destruction_hook signals pending, cached,
                // and current points during surface destruction.
                // From this take onward CosMix is the point's sole owner on
                // every path. Rejection must signal it; success must hand it
                // to retirement, including the early token-release paths in
                // commit_new_buffer after this call.
                .take()
                .map(CommittedReleasePoint::Linux)
        })
    }

    fn try_retain_dmabuf(
        &mut self,
        surface: &WlSurface,
        buffer: wl_buffer::WlBuffer,
    ) -> Option<u64> {
        let client_count = Self::with_client_state(surface, |state| {
            state.retained_dmabufs.load(Ordering::Relaxed)
        })
        .unwrap_or_default();
        if client_count >= MAX_CLIENT_RETAINED_DMABUFS
            || self.retained_buffers.buffers.len() >= MAX_GLOBAL_RETAINED_DMABUFS
        {
            self.retire_buffer_immediately(buffer);
            self.reject_resource_limit(
                surface,
                format!(
                    "retained DMA-BUF budget exceeded (client {client_count}/{MAX_CLIENT_RETAINED_DMABUFS}, global {}/{MAX_GLOBAL_RETAINED_DMABUFS})",
                    self.retained_buffers.buffers.len()
                ),
            );
            return None;
        }
        let _ = Self::with_client_state(surface, |state| {
            state.retained_dmabufs.fetch_add(1, Ordering::Relaxed);
        });
        let token = self.retain_buffer(buffer);
        self.budgeted_dmabuf_tokens.insert(token);
        Some(token)
    }

    fn try_retain_capture_dmabuf(&mut self, buffer: wl_buffer::WlBuffer) -> Option<u64> {
        let client = buffer.client();
        let client_count = client
            .as_ref()
            .and_then(|client| client.get_data::<WaylandClientState>())
            .map(|state| state.retained_dmabufs.load(Ordering::Relaxed))
            .unwrap_or_default();
        let already_retained = self.retained_buffers.buffers.contains_key(&buffer.id());
        if client_count >= MAX_CLIENT_RETAINED_DMABUFS
            || (!already_retained
                && self.retained_buffers.buffers.len() >= MAX_GLOBAL_RETAINED_DMABUFS)
        {
            self.retire_buffer_immediately(buffer);
            return None;
        }
        if let Some(client_state) = client
            .as_ref()
            .and_then(|client| client.get_data::<WaylandClientState>())
        {
            client_state
                .retained_dmabufs
                .fetch_add(1, Ordering::Relaxed);
        }
        let token = self.retain_buffer(buffer);
        self.budgeted_dmabuf_tokens.insert(token);
        Some(token)
    }

    fn try_retain_existing_dmabuf(
        &mut self,
        buffer: wl_buffer::WlBuffer,
        use_id: Option<DmabufUseId>,
    ) -> Option<u64> {
        let client = buffer.client();
        let client_count = client
            .as_ref()
            .and_then(|client| client.get_data::<WaylandClientState>())
            .map(|state| state.retained_dmabufs.load(Ordering::Relaxed))
            .unwrap_or_default();
        let already_retained = self.retained_buffers.buffers.contains_key(&buffer.id());
        if client_count >= MAX_CLIENT_RETAINED_DMABUFS
            || (!already_retained
                && self.retained_buffers.buffers.len() >= MAX_GLOBAL_RETAINED_DMABUFS)
        {
            return None;
        }
        if let Some(client_state) = client
            .as_ref()
            .and_then(|client| client.get_data::<WaylandClientState>())
        {
            client_state
                .retained_dmabufs
                .fetch_add(1, Ordering::Relaxed);
        }
        let token = self.retain_buffer(buffer);
        self.budgeted_dmabuf_tokens.insert(token);
        match self.release_uses.add_renderer_owner(use_id, token) {
            AddRendererOwnerDecision::Implicit | AddRendererOwnerDecision::Added => Some(token),
            AddRendererOwnerDecision::UnknownUse | AddRendererOwnerDecision::TokenAlreadyOwned => {
                self.release_buffer_token(token);
                tracing::error!(
                    use_id = ?use_id,
                    token,
                    "failed to join dirty-recovery renderer token to DMA-BUF use"
                );
                None
            }
        }
    }

    /// Every surface the renderer should currently be showing.
    ///
    /// This is [`Self::latest_surface_upsert`]'s presence predicate and must
    /// stay that way: a surface this omits but that answers anything other
    /// than `Gone` there would be removed by a roster and then immediately
    /// restored by dirty recovery, and one it lists but that answers `Gone`
    /// would survive a roster it should not have.
    fn mapped_surface_ids(&self) -> HashSet<SurfaceId> {
        self.surface_objects
            .iter()
            .filter(|(_, object)| {
                self.surfaces
                    .get(object)
                    .is_some_and(|record| self.surface_is_renderer_presentable(record))
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether `dirty_surfaces` can still put this surface's state right.
    ///
    /// Recovery re-derives an *upsert*, and `latest_surface_upsert` answers
    /// `Gone` for exactly the ids this returns `false` for — one that names no
    /// live record, and one whose record is not presentable. A recovery mark
    /// set for either is dropped again on the next pass, so a caller whose
    /// event was rejected must converge on membership rather than defer.
    fn surface_is_recoverable(&self, id: SurfaceId) -> bool {
        self.surface_objects
            .get(&id)
            .and_then(|object| self.surfaces.get(object))
            .is_some_and(|record| self.surface_is_renderer_presentable(record))
    }

    /// Publish a surface upsert, unless the surface is not presentable.
    ///
    /// The roster and the recovery route both refuse a dormant surface, and a
    /// roster removes it from the renderer. A producer that pushed an upsert
    /// anyway would recreate the very entity the roster had just dropped, for a
    /// surface both predicates answer `Gone` for — the renderer would go on
    /// showing a surface whose role object the client destroyed, and no later
    /// state could remove it. So event *production* carries the same rule, from
    /// the same function. A suppressed DMA-BUF frame's renderer token is
    /// retired here, exactly as a rejected event's is.
    fn push_surface_upsert(&mut self, surface: &WlSurface, event: ProtocolEvent) {
        debug_assert!(
            matches!(event, ProtocolEvent::SurfaceUpserted { .. }),
            "this gate is the presence predicate, and only an upsert asserts presence"
        );
        if self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| self.surface_is_renderer_presentable(record))
        {
            self.events.push(event);
            return;
        }
        if let Some(token) = protocol_event_dmabuf_token(&event) {
            self.release_buffer_token(token);
        }
    }

    fn latest_surface_upsert(&mut self, id: SurfaceId) -> LatestSurfaceUpsert {
        let Some(object) = self.surface_objects.get(&id).cloned() else {
            return LatestSurfaceUpsert::Gone;
        };
        let Some(record) = self.surfaces.get(&object) else {
            return LatestSurfaceUpsert::Gone;
        };
        if !self.surface_is_renderer_presentable(record) {
            return LatestSurfaceUpsert::Gone;
        }
        let scene = record.scene_snapshot();
        if let Some(backing) = &record.shm_backing {
            return LatestSurfaceUpsert::Ready(Box::new(ProtocolEvent::SurfaceUpserted {
                id,
                scene,
                frame: SurfaceFrame::Shm(ShmFrame {
                    width: backing.width,
                    height: backing.height,
                    opaque: backing.format == wl_shm::Format::Xrgb8888,
                    rgba: Arc::clone(&backing.rgba),
                }),
            }));
        }
        let Some(backing) = &record.dmabuf_backing else {
            return LatestSurfaceUpsert::Retry;
        };
        let buffer = backing.buffer.clone();
        let buffer_id = backing.buffer_id;
        let cacheable = self.dmabuf_buffer_is_cacheable(&buffer, buffer_id);
        let use_id = backing.use_id;
        let descriptor = match backing.descriptor.try_clone() {
            Ok(descriptor) => descriptor,
            Err(error) => {
                tracing::warn!(surface_id = id.0, %error, "failed to duplicate dirty DMA-BUF state");
                return LatestSurfaceUpsert::Retry;
            }
        };
        let Some(token) = self.try_retain_existing_dmabuf(buffer, use_id) else {
            return LatestSurfaceUpsert::Retry;
        };
        LatestSurfaceUpsert::Ready(Box::new(ProtocolEvent::SurfaceUpserted {
            id,
            scene,
            frame: SurfaceFrame::Dmabuf(DmabufFrame {
                buffer_id,
                cacheable,
                token,
                descriptor,
                use_id,
            }),
        }))
    }

    fn release_buffer_token(&mut self, token: u64) {
        if matches!(
            self.release_uses.release_owner(token),
            release_use::ReleaseOwnerDecision::Faulted(_)
        ) {
            self.withdraw_explicit_sync_global("release owner fault");
        }
        let budgeted = self.budgeted_dmabuf_tokens.remove(&token);
        if budgeted
            && let Some(surface_client) = self
                .retained_buffers
                .tokens
                .get(&token)
                .and_then(|key| self.retained_buffers.buffers.get(key))
                .and_then(|retained| retained.value.client())
            && let Some(state) = surface_client.get_data::<WaylandClientState>()
        {
            let _ = state.retained_dmabufs.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |count| Some(count.saturating_sub(1)),
            );
        }
        if let Some(buffer) = self.retained_buffers.release(token) {
            buffer.release();
            tracing::debug!(token, "released final retained wl_buffer reference");
        }
    }

    fn retire_buffer_immediately(&mut self, buffer: wl_buffer::WlBuffer) {
        let token = self.retain_buffer(buffer);
        self.release_buffer_token(token);
    }

    fn layer_role_creation_is_already_constructed(&self, surface: &WlSurface) -> bool {
        self.buffer_history_surfaces.contains(&surface.id())
    }

    /// Retire content committed before a role that cannot adopt that commit.
    ///
    /// Subsurfaces deliberately do not use this path. Smithay leaves a
    /// roleless commit in `SurfaceAttributes::current`; after
    /// `get_subsurface`, the parent's next commit includes the synchronized
    /// child and our ordinary [`CompositorHandler::commit`] consumes that
    /// buffer. Its frame callbacks stay alongside it until the adopted child is
    /// presented. Cursor surfaces perform the same adoption explicitly in
    /// `set_cursor_image`, while unsupported drag icons are retired here as
    /// soon as their role is assigned.
    fn retire_unadopted_roleless_buffer(&mut self, surface: &WlSurface) {
        debug_assert_ne!(compositor::get_role(surface), Some(CURSOR_IMAGE_ROLE));
        let buffer = compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .take()
        });
        if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
            self.retire_buffer_immediately(buffer);
        }
    }

    fn retire_untracked_surface_buffer(
        &mut self,
        surface: &WlSurface,
        buffer: wl_buffer::WlBuffer,
    ) {
        if self.warned_unsupported_surfaces.insert(surface.id()) {
            tracing::warn!(
                surface = ?surface.id(),
                "untracked non-shell surface content is not composited"
            );
        }
        self.retire_buffer_immediately(buffer);
    }

    fn max_shm_backing_bytes(&self, surface: &WlSurface, current: usize) -> usize {
        let client_used =
            Self::with_client_state(surface, |state| state.shm_bytes.load(Ordering::Relaxed))
                .unwrap_or_default();
        let client_available = MAX_CLIENT_SHM_BYTES.saturating_sub(client_used);
        let global_available = MAX_GLOBAL_SHM_BYTES.saturating_sub(self.shm_bytes);
        current.saturating_add(client_available.min(global_available))
    }

    fn adjust_shm_bytes(&mut self, surface: &WlSurface, previous: usize, current: usize) {
        if current >= previous {
            let added = current - previous;
            self.shm_bytes = self.shm_bytes.saturating_add(added);
            let _ = Self::with_client_state(surface, |state| {
                state.shm_bytes.fetch_add(added, Ordering::Relaxed);
            });
        } else {
            self.release_shm_bytes(surface, previous - current);
        }
    }

    fn release_shm_bytes(&mut self, surface: &WlSurface, bytes: usize) {
        self.shm_bytes = self.shm_bytes.saturating_sub(bytes);
        let _ = Self::with_client_state(surface, |state| {
            let _ = state
                .shm_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(bytes))
                });
        });
    }

    fn commit_new_buffer(
        &mut self,
        surface: &WlSurface,
        buffer: wl_buffer::WlBuffer,
        commit: SurfaceBufferCommit,
    ) {
        let SurfaceBufferCommit {
            damage,
            force_full_damage,
            buffer_scale,
            buffer_transform,
            window_geometry_changed,
        } = commit;
        let (surface_id, commit_count) = {
            let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                self.retire_buffer_immediately(buffer);
                return;
            };
            record.commit_count = record.commit_count.saturating_add(1);
            (record.id, record.commit_count)
        };
        #[cfg(feature = "bus")]
        self.mark_surface_dirty(surface_id, "wayland.map");
        if let Ok(dmabuf) = get_dmabuf(&buffer) {
            match describe_dmabuf(dmabuf) {
                Ok(descriptor) => {
                    let width = descriptor.width;
                    let height = descriptor.height;
                    let presentation = match surface_presentation(
                        surface,
                        width,
                        height,
                        buffer_scale,
                        buffer_transform,
                    ) {
                        Ok(presentation) => presentation,
                        Err(error) => {
                            self.retire_buffer_immediately(buffer);
                            self.reject_invalid_presentation(
                                surface,
                                surface_id,
                                commit_count,
                                "dmabuf-rejected",
                                error,
                            );
                            return;
                        }
                    };
                    let fourcc = descriptor.fourcc;
                    let modifier = descriptor.modifier;
                    let plane_metadata = descriptor
                        .planes
                        .iter()
                        .map(|plane| (plane.offset, plane.stride))
                        .collect::<Vec<_>>();
                    let renderer_descriptor = match descriptor.try_clone() {
                        Ok(descriptor) => descriptor,
                        Err(error) => {
                            self.retire_buffer_immediately(buffer);
                            tracing::warn!(
                                surface_id = surface_id.0,
                                %error,
                                "failed to duplicate committed DMA-BUF description"
                            );
                            return;
                        }
                    };
                    let (buffer_id, cacheable) = self.dmabuf_buffer_identity(&buffer);
                    let backing_buffer = buffer.clone();
                    let Some(token) = self.try_retain_dmabuf(surface, buffer) else {
                        return;
                    };
                    let backing_retention_token = self.retain_buffer(backing_buffer.clone());
                    #[cfg(test)]
                    let committed_client = if mem::take(&mut self.release_use_force_client_missing)
                    {
                        None
                    } else {
                        surface.client()
                    };
                    #[cfg(not(test))]
                    let committed_client = surface.client();
                    let Some(client) = committed_client else {
                        #[cfg(test)]
                        {
                            self.release_use_client_missing_count =
                                self.release_use_client_missing_count.saturating_add(1);
                        }
                        self.release_buffer_token(token);
                        self.release_buffer_token(backing_retention_token);
                        return;
                    };
                    let release_point = self.take_committed_release_point(surface);
                    let use_id = match self.release_uses.prepare_use(
                        client.id(),
                        &client,
                        backing_buffer.id(),
                        release_point,
                        backing_retention_token,
                        token,
                    ) {
                        BeginUseDecision::Implicit => None,
                        BeginUseDecision::Begun(use_id) => Some(use_id),
                        BeginUseDecision::Rejected(_) => {
                            self.release_buffer_token(token);
                            self.release_buffer_token(backing_retention_token);
                            return;
                        }
                    };
                    #[cfg(test)]
                    if mem::take(&mut self.release_use_remove_record_after_prepare) {
                        self.surfaces.remove(&surface.id());
                    }
                    let window_geometry = self.committed_toplevel_window_geometry(
                        surface,
                        presentation.size,
                        window_geometry_changed,
                    );
                    #[cfg(feature = "bus")]
                    self.mark_surface_mapped(surface);
                    let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                        #[cfg(test)]
                        {
                            self.release_use_record_missing_count =
                                self.release_use_record_missing_count.saturating_add(1);
                        }
                        self.release_buffer_token(token);
                        self.release_buffer_token(backing_retention_token);
                        return;
                    };
                    record.mapped = commit_may_map_surface(record);
                    let old_origin = (record.layout.x, record.layout.y);
                    if let Some(window_geometry) = window_geometry {
                        record.layout.x = record.window_origin.0 - window_geometry.x;
                        record.layout.y = record.window_origin.1 - window_geometry.y;
                        record.committed_window_geometry = Some(window_geometry);
                    }
                    record.layout.width = presentation.size.0;
                    record.layout.height = presentation.size.1;
                    record.layout.source = presentation.source;
                    record.layout.transform = presentation.transform;
                    sync_toplevel_scene_state(record);
                    let origin_delta = (
                        record.layout.x - old_origin.0,
                        record.layout.y - old_origin.1,
                    );
                    let released_shm_bytes = record
                        .shm_backing
                        .take()
                        .map_or(0, |backing| backing.rgba.len());
                    let previous_dmabuf_token = record
                        .dmabuf_backing
                        .replace(DmabufBacking {
                            buffer: backing_buffer,
                            buffer_id,
                            descriptor,
                            retention_token: backing_retention_token,
                            use_id,
                        })
                        .map(|backing| backing.retention_token);
                    record.buffer_dimensions = Some((width, height));
                    let role = record.role.kind();
                    let log_commit = record
                        .logged_diagnostics
                        .insert(SurfaceDiagnostic::DmabufCommitted);
                    if log_commit {
                        tracing::info!(
                            surface_id = surface_id.0,
                            commit = commit_count,
                            buffer_kind = "dmabuf",
                            role,
                            width,
                            height,
                            buffer_scale,
                            ?buffer_transform,
                            fourcc = format_args!("{fourcc:#010x}"),
                            modifier = format_args!("{modifier:#018x}"),
                            plane_count = plane_metadata.len(),
                            planes = ?plane_metadata,
                            damage_rects = damage.len(),
                            token,
                            "first surface buffer committed"
                        );
                    }
                    let id = record.id;
                    if origin_delta != (0.0, 0.0) {
                        self.shift_surface_descendants(id, origin_delta);
                    }
                    if let Some(previous_dmabuf_token) = previous_dmabuf_token {
                        self.release_buffer_token(previous_dmabuf_token);
                    }
                    if released_shm_bytes > 0 {
                        self.release_shm_bytes(surface, released_shm_bytes);
                    }
                    self.recompute_effective_visibility();
                    let scene = self
                        .surfaces
                        .get(&surface.id())
                        .expect("mapped surface remains tracked")
                        .scene_snapshot();
                    self.push_surface_upsert(
                        surface,
                        ProtocolEvent::SurfaceUpserted {
                            id,
                            scene,
                            frame: SurfaceFrame::Dmabuf(DmabufFrame {
                                buffer_id,
                                cacheable,
                                token,
                                descriptor: renderer_descriptor,
                                use_id,
                            }),
                        },
                    );
                }
                Err(error) => {
                    self.retire_buffer_immediately(buffer);
                    if self.log_surface_diagnostic(surface, SurfaceDiagnostic::DmabufDescription) {
                        tracing::warn!(
                            surface_id = surface_id.0,
                            commit = commit_count,
                            buffer_kind = "dmabuf-rejected",
                            %error,
                            "surface DMA-BUF could not be described"
                        );
                    }
                }
            }
            return;
        }

        let previous_shm_bytes = self
            .surfaces
            .get(&surface.id())
            .and_then(|record| record.shm_backing.as_ref())
            .map_or(0, |backing| backing.rgba.len());
        let max_backing_bytes = self.max_shm_backing_bytes(surface, previous_shm_bytes);
        let copied = {
            let record = self
                .surfaces
                .get_mut(&surface.id())
                .expect("surface existence checked before buffer import");
            update_shm_buffer(
                &buffer,
                ShmUpdateContext {
                    output_size: self.backend.output_size(),
                    damage: &damage,
                    buffer_scale,
                    buffer_transform,
                    viewport: surface_viewport(surface),
                    force_full_damage,
                    max_backing_bytes,
                },
                &mut record.shm_backing,
            )
        };
        self.retire_buffer_immediately(buffer);
        match copied {
            Ok((frame, converted_rows)) => {
                let new_shm_bytes = frame.rgba.len();
                self.adjust_shm_bytes(surface, previous_shm_bytes, new_shm_bytes);
                let presentation = match surface_presentation(
                    surface,
                    frame.width,
                    frame.height,
                    buffer_scale,
                    buffer_transform,
                ) {
                    Ok(presentation) => presentation,
                    Err(error) => {
                        self.reject_invalid_presentation(
                            surface,
                            surface_id,
                            commit_count,
                            "shm-rejected",
                            error,
                        );
                        return;
                    }
                };
                let window_geometry = self.committed_toplevel_window_geometry(
                    surface,
                    presentation.size,
                    window_geometry_changed,
                );
                #[cfg(feature = "bus")]
                self.mark_surface_mapped(surface);
                let Some(record) = self.surfaces.get_mut(&surface.id()) else {
                    return;
                };
                record.mapped = commit_may_map_surface(record);
                let old_origin = (record.layout.x, record.layout.y);
                if let Some(window_geometry) = window_geometry {
                    record.layout.x = record.window_origin.0 - window_geometry.x;
                    record.layout.y = record.window_origin.1 - window_geometry.y;
                    record.committed_window_geometry = Some(window_geometry);
                }
                record.layout.width = presentation.size.0;
                record.layout.height = presentation.size.1;
                record.layout.source = presentation.source;
                record.layout.transform = presentation.transform;
                sync_toplevel_scene_state(record);
                let origin_delta = (
                    record.layout.x - old_origin.0,
                    record.layout.y - old_origin.1,
                );
                let released_dmabuf_token = record
                    .dmabuf_backing
                    .take()
                    .map(|backing| backing.retention_token);
                record.buffer_dimensions = Some((frame.width, frame.height));
                let format = record
                    .shm_backing
                    .as_ref()
                    .map(|backing| backing.format)
                    .unwrap_or(wl_shm::Format::Xrgb8888);
                let role = record.role.kind();
                let run_probe = !record.pixel_probe_logged;
                record.pixel_probe_logged = true;
                let log_commit = record
                    .logged_diagnostics
                    .insert(SurfaceDiagnostic::ShmCommitted);
                if log_commit {
                    tracing::info!(
                        surface_id = surface_id.0,
                        commit = commit_count,
                        buffer_kind = "shm",
                        role,
                        ?format,
                        width = frame.width,
                        height = frame.height,
                        buffer_scale,
                        ?buffer_transform,
                        damage_rects = damage.len(),
                        converted_rows,
                        "first surface buffer committed"
                    );
                }
                if run_probe {
                    let diagnostic = ShmDiagnostic {
                        surface_id,
                        commit_count,
                        role,
                        format,
                        buffer_scale,
                        buffer_transform,
                        width: frame.width,
                        height: frame.height,
                        rgba: Arc::clone(&frame.rgba),
                    };
                    if let Err(TrySendError::Full(_)) = self.diagnostic_sender.try_send(diagnostic)
                    {
                        tracing::debug!(
                            surface_id = surface_id.0,
                            "dropped first-frame diagnostic because worker is busy"
                        );
                    }
                }
                // The persistent backing converted only damaged rows. Bevy
                // 0.19 still re-prepares the complete Image asset after this
                // update; switch to RenderQueue::write_texture subregions when
                // the bridge owns SHM GPU uploads directly.
                let id = record.id;
                if origin_delta != (0.0, 0.0) {
                    self.shift_surface_descendants(id, origin_delta);
                }
                if let Some(released_dmabuf_token) = released_dmabuf_token {
                    self.release_buffer_token(released_dmabuf_token);
                }
                self.recompute_effective_visibility();
                let scene = self
                    .surfaces
                    .get(&surface.id())
                    .expect("mapped surface remains tracked")
                    .scene_snapshot();
                self.push_surface_upsert(
                    surface,
                    ProtocolEvent::SurfaceUpserted {
                        id,
                        scene,
                        frame: SurfaceFrame::Shm(frame),
                    },
                );
            }
            Err(error) => {
                if error.contains("aggregate SHM budget") {
                    self.reject_resource_limit(surface, error);
                    return;
                }
                if self.log_surface_diagnostic(surface, SurfaceDiagnostic::BufferImport) {
                    tracing::warn!(
                        surface_id = surface_id.0,
                        commit = commit_count,
                        buffer_kind = "unsupported",
                        %error,
                        "surface buffer could not be imported"
                    );
                }
            }
        }
    }

    fn log_surface_diagnostic(
        &mut self,
        surface: &WlSurface,
        diagnostic: SurfaceDiagnostic,
    ) -> bool {
        self.surfaces
            .get_mut(&surface.id())
            .is_some_and(|record| record.logged_diagnostics.insert(diagnostic))
    }

    fn reject_invalid_presentation(
        &mut self,
        surface: &WlSurface,
        surface_id: SurfaceId,
        commit: u64,
        buffer_kind: &'static str,
        error: SurfacePresentationError,
    ) {
        let diagnostic = match &error {
            SurfacePresentationError::InvalidSize(_) => SurfaceDiagnostic::InvalidSize,
            SurfacePresentationError::InvalidViewport => SurfaceDiagnostic::InvalidViewport,
        };
        let should_log = self.log_surface_diagnostic(surface, diagnostic);
        if let SurfacePresentationError::InvalidSize(message) = &error {
            surface.post_error(wl_surface::Error::InvalidSize, message.clone());
        }
        if should_log {
            tracing::warn!(
                surface_id = surface_id.0,
                commit,
                buffer_kind,
                %error,
                "rejected invalid surface presentation"
            );
        }
    }
}

fn surface_stack_cmp(left: &SurfaceRecord, right: &SurfaceRecord) -> std::cmp::Ordering {
    left.layout
        .z
        .cmp(&right.layout.z)
        .then_with(|| left.id.0.cmp(&right.id.0))
}

fn clamp_normal_restore(
    mut restore: NormalRestore,
    output: LogicalOutputRect,
    server_side: bool,
    theme: &DecoTheme,
) -> NormalRestore {
    let extents = DecoExtents::of(theme);
    let available_content = if server_side {
        extents.content_size_for_window(vec2(output.width, output.height))
    } else {
        vec2(output.width, output.height)
    };
    restore.client_size = (
        restore
            .client_size
            .0
            .min(available_content.x.floor().max(1.0) as i32)
            .max(1),
        restore
            .client_size
            .1
            .min(available_content.y.floor().max(1.0) as i32)
            .max(1),
    );
    let content = vec2(
        restore.client_size.0.max(1) as f32,
        restore.client_size.1.max(1) as f32,
    );
    let (mut outer_origin, outer_size) = if server_side {
        let window = ChromeLayout::compute(theme, content).window;
        (
            (
                restore.window_origin.0 - extents.left,
                restore.window_origin.1 - extents.top,
            ),
            vec2(window.w, window.h),
        )
    } else {
        (restore.window_origin, content)
    };
    let max_x = (output.x + output.width - outer_size.x).max(output.x);
    let max_y = (output.y + output.height - outer_size.y).max(output.y);
    outer_origin.0 = outer_origin.0.clamp(output.x, max_x);
    outer_origin.1 = outer_origin.1.clamp(output.y, max_y);
    NormalRestore {
        window_origin: if server_side {
            (outer_origin.0 + extents.left, outer_origin.1 + extents.top)
        } else {
            outer_origin
        },
        output,
        ..restore
    }
}

fn xdg_resize_edge(edge: DecoResizeEdge) -> xdg_toplevel::ResizeEdge {
    match edge {
        DecoResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
        DecoResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
        DecoResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
        DecoResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
        DecoResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        DecoResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
        DecoResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
        DecoResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
    }
}

fn interactive_surface(interaction: Option<&InteractivePointer>) -> Option<&WlSurface> {
    match interaction? {
        InteractivePointer::Move { surface, .. } | InteractivePointer::Resize { surface, .. } => {
            Some(surface)
        }
    }
}

fn pointer_grab_targets_surface(
    pointer: &PointerHandle<WaylandState>,
    popup_manager: &PopupManager,
    requested: &WlSurface,
) -> bool {
    pointer
        .grab_start_data()
        .and_then(|start| start.focus)
        .and_then(|(focus, _)| focus.owned_surface())
        .is_some_and(|focus| {
            canonical_root_surface(popup_manager, &focus)
                == canonical_root_surface(popup_manager, requested)
        })
}

fn popup_grab_has_live_action(pointer: bool, keyboard: bool, touch: bool) -> bool {
    pointer || keyboard || touch
}

fn keyboard_action_matches_root<T: Eq>(
    action: Option<(Serial, T)>,
    serial: Serial,
    popup_root: T,
) -> bool {
    action.is_some_and(|(action_serial, action_root)| {
        action_serial == serial && action_root == popup_root
    })
}

fn invalidate_keyboard_action<T>(action: &mut Option<T>) {
    *action = None;
}

mod acquire_gate;
mod explicit_sync;
mod focus;
mod handlers;
mod input;
mod release_use;
#[cfg(feature = "xwayland")]
mod xwayland;

use focus::{SeatFocusTarget, focus_targets_surface};

struct WaylandClientState {
    compositor_state: CompositorClientState,
    surface_count: AtomicUsize,
    shm_bytes: AtomicUsize,
    retained_dmabufs: AtomicUsize,
    disconnect_sender: channel::Sender<ClientId>,
    #[cfg(test)]
    disconnect_reason: Mutex<Option<String>>,
}

impl WaylandClientState {
    fn new(disconnect_sender: channel::Sender<ClientId>) -> Self {
        Self {
            compositor_state: CompositorClientState::default(),
            surface_count: AtomicUsize::new(0),
            shm_bytes: AtomicUsize::new(0),
            retained_dmabufs: AtomicUsize::new(0),
            disconnect_sender,
            #[cfg(test)]
            disconnect_reason: Mutex::new(None),
        }
    }
}

impl ClientData for WaylandClientState {
    fn initialized(&self, client_id: ClientId) {
        tracing::info!(client = ?client_id, "Wayland client connected");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        tracing::info!(client = ?client_id, ?reason, "Wayland client disconnected");
        #[cfg(test)]
        {
            *self
                .disconnect_reason
                .lock()
                .expect("test disconnect-reason mutex poisoned") = Some(format!("{reason:?}"));
        }
        let _ = self.disconnect_sender.send(client_id);
    }
}

fn set_toplevel_configuration(surface: &ToplevelSurface, size: (i32, i32)) {
    surface.with_pending_state(|state| {
        state.size = Some(size.into());
    });
}

fn sensible_toplevel_size(output: impl Into<LogicalOutputRect>, x: f32, y: f32) -> (i32, i32) {
    let output = output.into();
    let max_width = (output.x + output.width - x - OUTPUT_MARGIN).max(240.0) as i32;
    let max_height = (output.y + output.height - y - OUTPUT_MARGIN).max(160.0) as i32;
    // Clients honour this configure, so a fixed 640x420 gave a real browser a
    // 210-logical-pixel viewport on a 1080p nested output. Scale to the output
    // and keep the fixed size only as a floor for small outputs.
    let share_width = (output.width * DEFAULT_TOPLEVEL_OUTPUT_SHARE) as i32;
    let share_height = (output.height * DEFAULT_TOPLEVEL_OUTPUT_SHARE) as i32;
    (
        share_width.max(DEFAULT_TOPLEVEL_WIDTH).min(max_width),
        share_height.max(DEFAULT_TOPLEVEL_HEIGHT).min(max_height),
    )
}

fn output_mode(size: (u32, u32)) -> Mode {
    Mode {
        size: (size.0 as i32, size.1 as i32).into(),
        refresh: 60_000,
    }
}

fn smithay_button_state(state: HostButtonState) -> ButtonState {
    match state {
        HostButtonState::Pressed => ButtonState::Pressed,
        HostButtonState::Released => ButtonState::Released,
    }
}

fn smithay_key_state(state: HostButtonState) -> KeyState {
    match state {
        HostButtonState::Pressed => KeyState::Pressed,
        HostButtonState::Released => KeyState::Released,
    }
}

fn binding_filter_result(disposition: KeyDisposition) -> FilterResult<Option<BindingAction>> {
    match disposition {
        KeyDisposition::Forward => FilterResult::Forward,
        KeyDisposition::Act(action) => FilterResult::Intercept(Some(action)),
        KeyDisposition::SwallowRelease => FilterResult::Intercept(None),
    }
}

/// Confine a seat position to the union of the seat's regions.
///
/// Used for the cursor and for touch contacts alike — a fingertip reported
/// slightly outside every admitted output has the same problem a cursor there
/// does, and must resolve to the same point, or the two devices would disagree
/// about which surface the same physical spot belongs to.
///
/// Not to their bounding box. Outputs of unequal height leave dead space
/// inside the box that belongs to no display, and a cursor parked there is one
/// nothing can draw. A point already inside any region is returned untouched;
/// otherwise it moves to the nearest point of the nearest region, so a
/// diagonal overshoot past a corner lands on the corner rather than sliding
/// along one axis into a neighbour's dead space.
///
/// What that buys is the smallest correction, not continuity. This is a
/// stateless projection of one position: a point in the middle of a gap between
/// two disconnected regions goes to whichever side is closer, so a single large
/// delta can still cross the gap. Refusing the crossing would need the previous
/// position and a swept path, and would be the wrong trade — it would let one
/// over-accumulated relative delta wedge the cursor against a gap it should
/// have flown over. Reachability, not travel, is the invariant here.
///
/// Each region's upper bounds are half-open, matching `surface_at`'s
/// `x >= left && x < right`: clamping to exactly the far edge would land
/// outside every surface that touches it and silently drop pointer focus, so
/// pushing the pointer hard against a border would clear focus and the next
/// click would go nowhere. `next_down` is the largest value that satisfies the
/// bound — exact, not a hand-picked epsilon that drifts when either side is
/// edited. A degenerate region — zero extent, an output whose mode has not been
/// admitted — collapses to its own origin rather than inverting the bound.
fn cursor_surface_hotspot(surface: &WlSurface) -> (i32, i32) {
    compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<CursorImageSurfaceData>()
            .map(|attributes| {
                let attributes = attributes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (attributes.hotspot.x, attributes.hotspot.y)
            })
            .unwrap_or((0, 0))
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ClampResult {
    position: (f64, f64),
    region_index: usize,
    attempted_motion: (f64, f64),
}

impl PartialEq<(f64, f64)> for ClampResult {
    fn eq(&self, other: &(f64, f64)) -> bool {
        self.position == *other
    }
}

impl PartialEq<ClampResult> for (f64, f64) {
    fn eq(&self, other: &ClampResult) -> bool {
        *self == other.position
    }
}

fn clamp_point_to_seat(position: (f64, f64), regions: &[SeatRegion]) -> ClampResult {
    let clamp_into = |region: &SeatRegion| {
        let axis = |value: f64, origin: f64, extent: f64| {
            // `max(origin)` covers a zero-extent output, where `next_down`
            // would otherwise fall below the lower bound and invert the range.
            let upper = (origin + extent).next_down().max(origin);
            if value.is_finite() {
                value.clamp(origin, upper)
            } else {
                origin
            }
        };
        (
            axis(position.0, region.x, region.width),
            axis(position.1, region.y, region.height),
        )
    };
    let contains = |region: &SeatRegion| {
        position.0.is_finite()
            && position.1.is_finite()
            && position.0 >= region.x
            && position.0 < region.x + region.width
            && position.1 >= region.y
            && position.1 < region.y + region.height
    };

    if let Some(region_index) = regions.iter().position(contains) {
        return ClampResult {
            position,
            region_index,
            attempted_motion: (0.0, 0.0),
        };
    }
    let (region_index, clamped) = regions
        .iter()
        .enumerate()
        .map(|(index, region)| (index, clamp_into(region)))
        .reduce(|nearest, candidate| {
            let distance = |point: (f64, f64)| {
                if !position.0.is_finite() || !position.1.is_finite() {
                    return 0.0;
                }
                let (dx, dy) = (point.0 - position.0, point.1 - position.1);
                dx * dx + dy * dy
            };
            if distance(candidate.1) < distance(nearest.1) {
                candidate
            } else {
                nearest
            }
        })
        // `seat_regions` is never empty, so this is unreachable in production;
        // the origin is the only answer that cannot be outside a seat.
        .unwrap_or((0, (0.0, 0.0)));
    let attempted_motion = if clamped == position {
        (0.0, 0.0)
    } else {
        (position.0 - clamped.0, position.1 - clamped.1)
    };
    ClampResult {
        position: clamped,
        region_index,
        attempted_motion,
    }
}

pub(crate) fn monotonic_millis() -> u32 {
    // Wayland timestamps have an unspecified monotonic base and wrap naturally.
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}

fn root_compositor_surface(surface: &WlSurface) -> WlSurface {
    compositor_root_with(surface, compositor::get_parent)
}

fn compositor_root_with<T, F>(surface: &T, compositor_parent: F) -> T
where
    T: Clone + Eq + std::hash::Hash,
    F: Fn(&T) -> Option<T>,
{
    let mut root = surface.clone();
    let mut visited = HashSet::new();
    while visited.insert(root.clone()) {
        let Some(parent) = compositor_parent(&root) else {
            break;
        };
        root = parent;
    }
    root
}

fn canonical_root_surface(popup_manager: &PopupManager, surface: &WlSurface) -> WlSurface {
    canonical_root_with(surface, compositor::get_parent, |compositor_root| {
        popup_manager
            .find_popup(compositor_root)
            .and_then(|popup| find_popup_root_surface(&popup).ok())
    })
}

fn canonical_root_with<T, F, P>(surface: &T, compositor_parent: F, popup_root: P) -> T
where
    T: Clone + Eq + std::hash::Hash,
    F: Fn(&T) -> Option<T>,
    P: Fn(&T) -> Option<T>,
{
    let compositor_root = compositor_root_with(surface, &compositor_parent);
    popup_root(&compositor_root)
        .map(|root| compositor_root_with(&root, compositor_parent))
        .unwrap_or(compositor_root)
}

fn surface_buffer_limits(_output_size: (u32, u32)) -> (usize, usize) {
    // Absolute caps only. An output-relative clause was tried first and
    // rejected real clients: a browser's initial hidpi buffer legitimately
    // exceeds a small nested window before the client acks our configure.
    // The DoS guard is the per-axis cap plus MAX_SURFACE_BYTES below.
    (
        MAX_SURFACE_DIMENSION as usize,
        MAX_SURFACE_DIMENSION as usize,
    )
}

fn validate_surface_buffer_size(
    width: usize,
    height: usize,
    output_size: (u32, u32),
) -> Result<usize, String> {
    let (max_width, max_height) = surface_buffer_limits(output_size);
    if width == 0 || height == 0 {
        return Err(format!(
            "surface buffer dimensions must be non-zero: {width}x{height}"
        ));
    }
    if width > max_width || height > max_height {
        return Err(format!(
            "surface buffer {width}x{height} exceeds nested limit {max_width}x{max_height}"
        ));
    }
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "surface buffer byte size overflow".to_string())?;
    if bytes > MAX_SURFACE_BYTES {
        return Err(format!(
            "surface buffer requires {bytes} bytes, limit is {MAX_SURFACE_BYTES}"
        ));
    }
    Ok(bytes)
}

fn logical_surface_size(width: u32, height: u32, buffer_scale: i32) -> Result<(f32, f32), String> {
    let scale = u32::try_from(buffer_scale)
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| format!("invalid buffer scale {buffer_scale}"))?;
    if !width.is_multiple_of(scale) || !height.is_multiple_of(scale) {
        return Err(format!(
            "buffer {width}x{height} is not divisible by scale {scale}"
        ));
    }
    Ok(((width / scale) as f32, (height / scale) as f32))
}

fn surface_tree_bounds(
    presented_size: (f32, f32),
    descendants: &[(f32, f32, f32, f32)],
) -> SceneWindowGeometry {
    let mut left = 0.0_f32;
    let mut top = 0.0_f32;
    let mut right = presented_size.0;
    let mut bottom = presented_size.1;
    for &(x, y, width, height) in descendants {
        left = left.min(x);
        top = top.min(y);
        right = right.max(x + width);
        bottom = bottom.max(y + height);
    }
    SceneWindowGeometry {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

fn clamp_window_geometry(surface: &WlSurface, bounds: SceneWindowGeometry) -> SceneWindowGeometry {
    let requested = compositor::with_states(surface, |states| {
        let mut state = states.cached_state.get::<SurfaceCachedState>();
        state.current().geometry
    });
    let Some(requested) = requested else {
        return bounds;
    };
    let x = (requested.loc.x as f32).max(bounds.x);
    let y = (requested.loc.y as f32).max(bounds.y);
    let right = (requested.loc.x as f32 + requested.size.w as f32).min(bounds.x + bounds.width);
    let bottom = (requested.loc.y as f32 + requested.size.h as f32).min(bounds.y + bounds.height);
    if right <= x || bottom <= y {
        return bounds;
    }
    SceneWindowGeometry {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

fn window_geometry_is_explicit(surface: &WlSurface) -> bool {
    compositor::with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .is_some()
    })
}

fn surface_size_constraints(surface: &WlSurface) -> ((i32, i32), (i32, i32)) {
    compositor::with_states(surface, |states| {
        let mut state = states.cached_state.get::<SurfaceCachedState>();
        let current = state.current();
        (
            (current.min_size.w, current.min_size.h),
            (current.max_size.w, current.max_size.h),
        )
    })
}

fn pending_surface_size_constraints(surface: &WlSurface) -> ((i32, i32), (i32, i32)) {
    compositor::with_states(surface, |states| {
        let mut state = states.cached_state.get::<SurfaceCachedState>();
        let pending = state.pending();
        (
            (pending.min_size.w, pending.min_size.h),
            (pending.max_size.w, pending.max_size.h),
        )
    })
}

fn constraints_after_toplevel_request(
    pending: ((i32, i32), (i32, i32)),
    request: &xdg_toplevel::Request,
) -> Option<((i32, i32), (i32, i32))> {
    let (mut minimum, mut maximum) = pending;
    match request {
        xdg_toplevel::Request::SetMinSize { width, height } => {
            minimum = (*width, *height);
        }
        xdg_toplevel::Request::SetMaxSize { width, height } => {
            maximum = (*width, *height);
        }
        _ => return None,
    }
    Some((minimum, maximum))
}

fn validate_toplevel_constraints(
    (minimum, maximum): ((i32, i32), (i32, i32)),
) -> Result<(), String> {
    if minimum.0 < 0 || minimum.1 < 0 || maximum.0 < 0 || maximum.1 < 0 {
        return Err(format!(
            "negative min/max constraint: min={minimum:?}, max={maximum:?}"
        ));
    }
    if (maximum.0 > 0 && minimum.0 > maximum.0) || (maximum.1 > 0 && minimum.1 > maximum.1) {
        return Err(format!(
            "minimum exceeds non-zero maximum: min={minimum:?}, max={maximum:?}"
        ));
    }
    Ok(())
}

fn clamped_toplevel_constraints(constraints: ((i32, i32), (i32, i32))) -> ((i32, i32), (i32, i32)) {
    let (minimum, maximum) = constraints;
    let absolute = MAX_SURFACE_DIMENSION as i32;
    let minimum = (minimum.0.clamp(1, absolute), minimum.1.clamp(1, absolute));
    let maximum = (
        if maximum.0 > 0 {
            maximum.0.min(absolute)
        } else {
            absolute
        }
        .max(minimum.0),
        if maximum.1 > 0 {
            maximum.1.min(absolute)
        } else {
            absolute
        }
        .max(minimum.1),
    );
    (minimum, maximum)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfacePresentation {
    size: (f32, f32),
    source: Option<TextureSourceRect>,
    transform: SurfaceTransform,
}

impl From<SurfacePresentation> for CursorPresentation {
    fn from(presentation: SurfacePresentation) -> Self {
        Self {
            width: presentation.size.0,
            height: presentation.size.1,
            source: presentation.source,
            transform: presentation.transform,
        }
    }
}

#[derive(Debug)]
enum SurfacePresentationError {
    InvalidSize(String),
    InvalidViewport,
}

impl std::fmt::Display for SurfacePresentationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSize(message) => formatter.write_str(message),
            Self::InvalidViewport => {
                formatter.write_str("viewport source rectangle is outside the surface buffer")
            }
        }
    }
}

fn reject_cursor_presentation(surface: &WlSurface, error: SurfacePresentationError) {
    if let SurfacePresentationError::InvalidSize(message) = &error {
        surface.post_error(wl_surface::Error::InvalidSize, message.clone());
    }
    tracing::warn!(surface = ?surface.id(), %error, "rejected invalid cursor presentation");
}

fn surface_presentation(
    surface: &WlSurface,
    width: u32,
    height: u32,
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
) -> Result<SurfacePresentation, SurfacePresentationError> {
    let transform = surface_transform_from_wayland(buffer_transform);
    let (transformed_width, transformed_height) = if transform.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    };
    let base_size = logical_surface_size(transformed_width, transformed_height, buffer_scale)
        .map_err(SurfacePresentationError::InvalidSize)?;
    let logical_buffer_size = (
        i32::try_from(base_size.0 as i64).map_err(|_| {
            SurfacePresentationError::InvalidSize("logical buffer width exceeds i32".to_string())
        })?,
        i32::try_from(base_size.1 as i64).map_err(|_| {
            SurfacePresentationError::InvalidSize("logical buffer height exceeds i32".to_string())
        })?,
    )
        .into();
    let (valid, viewport) = compositor::with_states(surface, |states| {
        let valid = ensure_viewport_valid(states, logical_buffer_size);
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        (valid, *viewport.current())
    });
    if !valid {
        return Err(SurfacePresentationError::InvalidViewport);
    }

    let size = viewport
        .dst
        .map(|size| (size.w as f32, size.h as f32))
        .or_else(|| {
            viewport
                .src
                .map(|source| (source.size.w as f32, source.size.h as f32))
        })
        .unwrap_or(base_size);
    let scale = f64::from(buffer_scale);
    let source = viewport.src.map(|source| {
        transformed_source_rect(
            source.loc.x * scale,
            source.loc.y * scale,
            source.size.w * scale,
            source.size.h * scale,
            width,
            height,
            transform,
        )
    });

    Ok(SurfacePresentation {
        size,
        source,
        transform,
    })
}

fn surface_transform_from_wayland(transform: wl_output_protocol::Transform) -> SurfaceTransform {
    match transform {
        wl_output_protocol::Transform::Normal => SurfaceTransform::Normal,
        wl_output_protocol::Transform::_90 => SurfaceTransform::Rotate90,
        wl_output_protocol::Transform::_180 => SurfaceTransform::Rotate180,
        wl_output_protocol::Transform::_270 => SurfaceTransform::Rotate270,
        wl_output_protocol::Transform::Flipped => SurfaceTransform::Flipped,
        wl_output_protocol::Transform::Flipped90 => SurfaceTransform::Flipped90,
        wl_output_protocol::Transform::Flipped180 => SurfaceTransform::Flipped180,
        wl_output_protocol::Transform::Flipped270 => SurfaceTransform::Flipped270,
        _ => SurfaceTransform::Normal,
    }
}

impl SurfaceTransform {
    fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }
}

fn transformed_source_rect(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    buffer_width: u32,
    buffer_height: u32,
    transform: SurfaceTransform,
) -> TextureSourceRect {
    let inverse = |x: f64, y: f64| match transform {
        SurfaceTransform::Normal => (x, y),
        SurfaceTransform::Rotate90 => (y, f64::from(buffer_height) - x),
        SurfaceTransform::Rotate180 => (f64::from(buffer_width) - x, f64::from(buffer_height) - y),
        SurfaceTransform::Rotate270 => (f64::from(buffer_width) - y, x),
        SurfaceTransform::Flipped => (f64::from(buffer_width) - x, y),
        SurfaceTransform::Flipped90 => (f64::from(buffer_width) - y, f64::from(buffer_height) - x),
        SurfaceTransform::Flipped180 => (x, f64::from(buffer_height) - y),
        SurfaceTransform::Flipped270 => (y, x),
    };
    let corners = [
        inverse(x, y),
        inverse(x + width, y),
        inverse(x, y + height),
        inverse(x + width, y + height),
    ];
    let min_x = corners
        .iter()
        .map(|point| point.0)
        .fold(f64::INFINITY, f64::min);
    let max_x = corners
        .iter()
        .map(|point| point.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = corners
        .iter()
        .map(|point| point.1)
        .fold(f64::INFINITY, f64::min);
    let max_y = corners
        .iter()
        .map(|point| point.1)
        .fold(f64::NEG_INFINITY, f64::max);
    TextureSourceRect {
        x: min_x as f32,
        y: min_y as f32,
        width: (max_x - min_x) as f32,
        height: (max_y - min_y) as f32,
    }
}

struct ShmDiagnostic {
    surface_id: SurfaceId,
    commit_count: u64,
    role: &'static str,
    format: wl_shm::Format,
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
}

fn spawn_shm_diagnostic_worker() -> SyncSender<ShmDiagnostic> {
    let (sender, receiver) = mpsc::sync_channel::<ShmDiagnostic>(2);
    thread::Builder::new()
        .name("cosmix-shm-diagnostics".into())
        .spawn(move || {
            while let Ok(diagnostic) = receiver.recv() {
                log_and_dump_shm_probe(diagnostic);
            }
        })
        .expect("spawn bounded SHM diagnostic worker");
    sender
}

fn log_and_dump_shm_probe(diagnostic: ShmDiagnostic) {
    let mut non_black = 0_usize;
    let mut non_transparent = 0_usize;
    let mut min_rgb = [u8::MAX; 3];
    let mut max_rgb = [u8::MIN; 3];
    let pixel_count = diagnostic.rgba.len() / 4;
    let sample_stride = diagnostic_sample_stride(pixel_count);
    let mut sampled = 0_usize;
    for pixel in diagnostic.rgba.chunks_exact(4).step_by(sample_stride) {
        sampled += 1;
        if pixel[..3] != [0, 0, 0] {
            non_black += 1;
        }
        if pixel[3] != 0 {
            non_transparent += 1;
        }
        for channel in 0..3 {
            min_rgb[channel] = min_rgb[channel].min(pixel[channel]);
            max_rgb[channel] = max_rgb[channel].max(pixel[channel]);
        }
    }
    tracing::info!(
        surface_id = diagnostic.surface_id.0,
        commit = diagnostic.commit_count,
        role = diagnostic.role,
        format = ?diagnostic.format,
        buffer_scale = diagnostic.buffer_scale,
        buffer_transform = ?diagnostic.buffer_transform,
        sampled,
        sample_stride,
        non_black,
        non_transparent,
        ?min_rgb,
        ?max_rgb,
        "first SHM frame pixel probe"
    );

    static DUMP_DIRECTORY: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let Some(directory) = DUMP_DIRECTORY
        .get_or_init(|| env::var_os("COSMIX_DUMP_SHM_DIR").map(PathBuf::from))
        .as_ref()
    else {
        return;
    };
    if let Err(error) = fs::create_dir_all(directory) {
        tracing::warn!(path = %directory.display(), %error, "failed to create SHM dump directory");
        return;
    }
    let path = directory.join(format!(
        "surface-{}-commit-{}-{}.pam",
        diagnostic.surface_id.0, diagnostic.commit_count, diagnostic.role
    ));
    let header = format!(
        "P7\nWIDTH {}\nHEIGHT {}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
        diagnostic.width, diagnostic.height
    );
    let mut bytes = Vec::with_capacity(header.len() + diagnostic.rgba.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&diagnostic.rgba);
    match fs::write(&path, bytes) {
        Ok(()) => tracing::info!(path = %path.display(), "wrote first converted SHM frame"),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to write converted SHM frame");
        }
    }
}

fn diagnostic_sample_stride(pixel_count: usize) -> usize {
    pixel_count.div_ceil(4096).max(1)
}

struct ShmUpdateContext<'a> {
    output_size: (u32, u32),
    damage: &'a [Damage],
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
    viewport: ViewportCachedState,
    force_full_damage: bool,
    max_backing_bytes: usize,
}

fn update_shm_buffer(
    buffer: &wl_buffer::WlBuffer,
    context: ShmUpdateContext<'_>,
    backing: &mut Option<ShmBacking>,
) -> Result<(ShmFrame, usize), String> {
    with_buffer_contents(buffer, |base, length, data| {
        let width =
            usize::try_from(data.width).map_err(|_| format!("invalid SHM width {}", data.width))?;
        let height = usize::try_from(data.height)
            .map_err(|_| format!("invalid SHM height {}", data.height))?;
        let backing_bytes = validate_surface_buffer_size(width, height, context.output_size)?;
        if backing_bytes > context.max_backing_bytes {
            return Err(format!(
                "aggregate SHM budget rejects {backing_bytes}-byte backing (surface allowance {})",
                context.max_backing_bytes
            ));
        }
        let stride = usize::try_from(data.stride)
            .map_err(|_| format!("invalid SHM stride {}", data.stride))?;
        let offset = usize::try_from(data.offset)
            .map_err(|_| format!("invalid SHM offset {}", data.offset))?;
        if !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        ) {
            return Err(format!("unsupported wl_shm format {:?}", data.format));
        }
        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| "SHM row size overflow".to_string())?;
        if stride < row_bytes {
            return Err(format!(
                "SHM stride {stride} is smaller than packed row {row_bytes}"
            ));
        }
        let source_end = offset
            .checked_add(
                height
                    .saturating_sub(1)
                    .checked_mul(stride)
                    .ok_or_else(|| "SHM row offset overflow".to_string())?,
            )
            .and_then(|last_row| last_row.checked_add(row_bytes))
            .ok_or_else(|| "SHM source range overflow".to_string())?;
        if source_end > length {
            return Err(format!(
                "SHM image exceeds pool: end {source_end}, pool {length}"
            ));
        }

        let dimensions_changed = backing.as_ref().is_none_or(|current| {
            current.width != width as u32
                || current.height != height as u32
                || current.format != data.format
        });
        if dimensions_changed {
            *backing = Some(ShmBacking {
                width: width as u32,
                height: height as u32,
                format: data.format,
                rgba: Arc::new(vec![
                    0;
                    row_bytes.checked_mul(height).ok_or_else(|| {
                        "SHM RGBA allocation overflow".to_string()
                    })?
                ]),
            });
        }
        let rows = if dimensions_changed || context.force_full_damage {
            std::iter::once(0..height).collect()
        } else {
            damage_row_ranges(
                context.damage,
                width,
                height,
                context.buffer_scale,
                context.buffer_transform,
                context.viewport,
            )
        };
        let converted_rows = rows.iter().map(|range| range.len()).sum();
        let current = backing
            .as_mut()
            .expect("SHM backing is initialised before row conversion");
        // This never waits for the renderer/GPU. If a bounded renderer event
        // still aliases the backing, COW may copy at most MAX_SURFACE_BYTES;
        // renderer-side subregion uploads are needed to remove that CPU cost.
        let rgba = Arc::make_mut(&mut current.rgba);
        let mut owned_source_row = vec![0_u8; row_bytes];
        for rows in rows {
            for row in rows {
                let source_offset = offset
                    .checked_add(
                        row.checked_mul(stride)
                            .ok_or_else(|| "SHM row offset overflow".to_string())?,
                    )
                    .ok_or_else(|| "SHM row offset overflow".to_string())?;
                // SAFETY: Smithay keeps the pool mapping alive for this
                // closure and the complete source range was checked above.
                // Client-writable shm must never become a Rust reference:
                // copy through raw pointers into compositor-owned storage.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        base.add(source_offset),
                        owned_source_row.as_mut_ptr(),
                        row_bytes,
                    );
                }
                let destination = &mut rgba[row * row_bytes..(row + 1) * row_bytes];
                convert_shm_row(data.format, &owned_source_row, destination);
            }
        }

        Ok((
            ShmFrame {
                width: current.width,
                height: current.height,
                opaque: current.format == wl_shm::Format::Xrgb8888,
                rgba: Arc::clone(&current.rgba),
            },
            converted_rows,
        ))
    })
    .map_err(|error| format!("buffer is not readable wl_shm: {error}"))?
}

fn surface_viewport(surface: &WlSurface) -> ViewportCachedState {
    compositor::with_states(surface, |states| {
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        *viewport.current()
    })
}

fn damage_row_ranges(
    damage: &[Damage],
    width: usize,
    height: usize,
    buffer_scale: i32,
    buffer_transform: wl_output_protocol::Transform,
    viewport: ViewportCachedState,
) -> Vec<std::ops::Range<usize>> {
    if width == 0 || height == 0 || buffer_scale <= 0 {
        return Vec::new();
    }
    if buffer_transform != wl_output_protocol::Transform::Normal && !damage.is_empty() {
        return std::iter::once(0..height).collect();
    }
    let scale = f64::from(buffer_scale);
    let logical_width = width as f64 / scale;
    let logical_height = height as f64 / scale;
    let source = viewport.src.unwrap_or_else(|| {
        Rectangle::new((0.0, 0.0).into(), (logical_width, logical_height).into())
    });
    let destination = viewport
        .dst
        .map(|size| (f64::from(size.w), f64::from(size.h)))
        .unwrap_or((source.size.w, source.size.h));
    let mut ranges = Vec::new();

    for damaged in damage {
        let (start, end) = match damaged {
            Damage::Buffer(rect) => (
                f64::from(rect.loc.y),
                f64::from(rect.loc.y.saturating_add(rect.size.h)),
            ),
            Damage::Surface(rect) => {
                if destination.1 <= 0.0 {
                    continue;
                }
                let start =
                    source.loc.y + f64::from(rect.loc.y).max(0.0) / destination.1 * source.size.h;
                let end = source.loc.y
                    + f64::from(rect.loc.y.saturating_add(rect.size.h)).max(0.0) / destination.1
                        * source.size.h;
                (start * scale, end * scale)
            }
        };
        let start = start.floor().clamp(0.0, height as f64) as usize;
        let end = end.ceil().clamp(0.0, height as f64) as usize;
        if start < end {
            ranges.push(start..end);
        }
    }

    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn describe_dmabuf(dmabuf: &Dmabuf) -> Result<DmabufDescriptor, String> {
    let size = dmabuf.size();
    let width = u32::try_from(size.w).map_err(|_| format!("invalid DMA-BUF width {}", size.w))?;
    let height = u32::try_from(size.h).map_err(|_| format!("invalid DMA-BUF height {}", size.h))?;
    if width == 0 || height == 0 {
        return Err(format!("invalid DMA-BUF dimensions {width}x{height}"));
    }
    if dmabuf.num_planes() != 1 {
        return Err(format!(
            "multi-plane DMA-BUF rejected: {} planes",
            dmabuf.num_planes()
        ));
    }

    let offsets = dmabuf.offsets().collect::<Vec<_>>();
    let strides = dmabuf.strides().collect::<Vec<_>>();
    let planes = dmabuf
        .handles()
        .enumerate()
        .map(|(index, fd): (usize, BorrowedFd<'_>)| {
            Ok(DmabufPlane {
                fd: fd.try_clone_to_owned().map_err(|error| {
                    format!("failed to duplicate DMA-BUF plane {index}: {error}")
                })?,
                offset: offsets[index],
                stride: strides[index],
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let format = dmabuf.format();
    Ok(DmabufDescriptor {
        width,
        height,
        fourcc: format.code as u32,
        modifier: u64::from(format.modifier),
        planes,
    })
}

fn validate_dmabuf_metadata(
    dmabuf: &Dmabuf,
    supported_formats: &[Format],
    output_size: (u32, u32),
) -> Result<(), String> {
    if dmabuf.num_planes() != 1 {
        return Err(format!(
            "expected one DMA-BUF plane, received {}",
            dmabuf.num_planes()
        ));
    }
    let format = dmabuf.format();
    if !supported_formats.contains(&format) {
        return Err(format!("format/modifier {format:?} was not advertised"));
    }
    let size = dmabuf.size();
    let width = usize::try_from(size.w).map_err(|_| format!("invalid DMA-BUF width {}", size.w))?;
    let height =
        usize::try_from(size.h).map_err(|_| format!("invalid DMA-BUF height {}", size.h))?;
    validate_surface_buffer_size(width, height, output_size)?;

    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| "DMA-BUF row size overflow".to_string())?;
    let offset = dmabuf
        .offsets()
        .next()
        .ok_or_else(|| "DMA-BUF plane has no offset".to_string())? as usize;
    let stride = dmabuf
        .strides()
        .next()
        .ok_or_else(|| "DMA-BUF plane has no stride".to_string())? as usize;
    if stride < row_bytes {
        return Err(format!(
            "DMA-BUF stride {stride} is smaller than packed row {row_bytes}"
        ));
    }
    offset
        .checked_add(
            height
                .saturating_sub(1)
                .checked_mul(stride)
                .ok_or_else(|| "DMA-BUF row offset overflow".to_string())?,
        )
        .and_then(|last_row| last_row.checked_add(row_bytes))
        .ok_or_else(|| "DMA-BUF plane byte range overflow".to_string())?;
    Ok(())
}

// `copy_shm_rows` used to sit here: a `#[cfg(test)]` reimplementation of the
// offset-and-stride walk inside `update_shm_buffer`, with no production caller.
// Its test could only ever prove that the copy *in the test build* was right,
// so the production walk was uncovered while looking covered. Rung E-3 replaced
// it with `shm_commit_reaches_the_renderer_pixel_exact_and_releases_the_buffer`,
// which drives a real client's padded, offset pool through the real import.

#[cfg(test)]
fn convert_shm_pixels(
    format: wl_shm::Format,
    width: usize,
    height: usize,
    packed_bgra: Vec<u8>,
) -> Result<ShmFrame, String> {
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        return Err(format!("unsupported wl_shm format {format:?}"));
    }

    let mut rgba = vec![0; packed_bgra.len()];
    convert_shm_row(format, &packed_bgra, &mut rgba);

    Ok(ShmFrame {
        width: width as u32,
        height: height as u32,
        opaque: format == wl_shm::Format::Xrgb8888,
        rgba: Arc::new(rgba),
    })
}

fn convert_shm_row(format: wl_shm::Format, packed_bgra: &[u8], rgba: &mut [u8]) {
    for (pixel, converted) in packed_bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        let alpha = if format == wl_shm::Format::Argb8888 {
            pixel[3]
        } else {
            255
        };
        converted.copy_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
    }
}

fn send_frames_surface_tree(surface: &WlSurface, time: u32) -> usize {
    let mut delivered = 0;
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surface, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
                delivered += 1;
            }
        },
        |_, _, &()| true,
    );
    delivered
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageCapAction {
    Accept,
    Saturate,
    Drop,
}

fn damage_cap_action(current: usize, pending: usize) -> DamageCapAction {
    match current.saturating_add(pending).cmp(&(MAX_DAMAGE_RECTS + 1)) {
        std::cmp::Ordering::Less => DamageCapAction::Accept,
        std::cmp::Ordering::Equal => DamageCapAction::Saturate,
        std::cmp::Ordering::Greater => DamageCapAction::Drop,
    }
}

fn surface_budget_exhausted(client_surfaces: usize, global_surfaces: usize) -> bool {
    client_surfaces >= MAX_CLIENT_SURFACES || global_surfaces >= MAX_GLOBAL_SURFACES
}

fn proposed_subsurface_depth(parent_depth: usize, subtree_height: usize) -> Option<usize> {
    parent_depth.checked_add(1)?.checked_add(subtree_height)
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) use tests::{
    PendingSsdSubsurfaceSceneClient, RealCursorSceneClient, RealShmSceneClient,
    real_shm_scene_runtime, real_shm_scene_runtime_with_decoration,
};
