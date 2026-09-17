//! Connection, event loop, surfaces and input dispatch.

use crate::clipboard::PendingRead;
use crate::event::{
    ButtonState, Event, KeyEvent, KeyState, Modifiers, PointerEvent, PointerKind, Selection,
    WindowState,
};
use crate::geom::{Damage, Rect};
use crate::ime::{self, ImeEvent, ImePlan, ImeSerials, ImeState};
use crate::pool::{AcquireError, Swapchain};
use crate::repeat::{Repeat, RepeatRate, TimerAction};
use crate::scale::{Scale, SurfaceInfo};
use crate::serial::GrabSerials;
use crate::xkb_state::XkbState;
use crate::{App, CursorShape, SurfaceId};
use calloop::channel::{Channel, Sender};
use calloop::ping::Ping;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, EventSource, LoopHandle, LoopSignal, RegistrationToken};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::data_device_manager::data_device::DataDevice;
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSource;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent as SctkKeyEvent, KeyboardData, KeyboardDataExt, KeyboardHandler, Keymap, Keysym,
    Modifiers as SctkModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{
    PointerEvent as SctkPointerEvent, PointerEventKind, PointerHandler,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::popup::{
    ConfigureKind, Popup, PopupConfigure, PopupHandler,
};
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_pointer,
    delegate_primary_selection, delegate_registry, delegate_seat, delegate_shm, delegate_xdg_popup,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_keyboard::{self, WlKeyboard},
    wl_output::WlOutput,
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};

#[derive(Debug)]
pub enum Error {
    Connect(String),
    Global(String),
    Loop(String),
    NoSuchSurface(SurfaceId),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "wayland connect: {e}"),
            Error::Global(e) => write!(f, "required global missing: {e}"),
            Error::Loop(e) => write!(f, "event loop: {e}"),
            Error::NoSuchSurface(id) => write!(f, "no surface {id:?}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub title: String,
    pub app_id: String,
    /// Logical size used until the compositor suggests one.
    pub size: (u32, u32),
    pub min_size: Option<(u32, u32)>,
    pub maximized: bool,
}

impl WindowSpec {
    pub fn new(title: impl Into<String>, size: (u32, u32)) -> Self {
        let title = title.into();
        Self {
            app_id: title.clone(),
            title,
            size,
            min_size: None,
            maximized: false,
        }
    }
}

/// Where a popup goes, in the parent's logical coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PopupSpec {
    pub anchor_rect: Rect,
    pub size: (u32, u32),
    pub anchor: Anchor,
    pub gravity: Gravity,
    pub constraint: ConstraintAdjustment,
    pub offset: (i32, i32),
    /// Take an explicit grab (menus). The first grabbing popup uses the
    /// latest button or key press; popups opened while that chain is open
    /// reuse its serial.
    pub grab: bool,
    /// Ask the compositor to re-place the popup when the parent moves.
    pub reactive: bool,
}

impl PopupSpec {
    /// A menu opening down and right from a point.
    pub fn menu_at(x: i32, y: i32, size: (u32, u32)) -> Self {
        Self {
            anchor_rect: Rect::new(x, y, 1, 1),
            size,
            anchor: Anchor::TopLeft,
            gravity: Gravity::BottomRight,
            constraint: ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY
                | ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY,
            offset: (0, 0),
            grab: true,
            reactive: false,
        }
    }

    /// A submenu beside a row of its parent menu.
    pub fn submenu(row: Rect, size: (u32, u32)) -> Self {
        Self {
            anchor_rect: row,
            size,
            anchor: Anchor::TopRight,
            gravity: Gravity::BottomRight,
            constraint: ConstraintAdjustment::FlipX | ConstraintAdjustment::SlideY,
            offset: (0, 0),
            grab: true,
            reactive: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub frames_committed: u64,
    pub buffer_allocations: u64,
    pub first_commit: Option<Instant>,
}

/// Wakes the loop from any thread; the app receives [`Event::Wake`].
#[derive(Clone, Debug)]
pub struct Waker(Sender<u64>);

impl Waker {
    pub fn wake(&self, token: u64) -> bool {
        self.0.send(token).is_ok()
    }
}

/// When a surface asks for frame callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FramePacing {
    /// Only when the app asks for another redraw while drawing. A one-off
    /// change draws at once and costs no callback wakeup; redraws requested
    /// between frames are not throttled to the display.
    #[default]
    OnDemand,
    /// After every commit: redraws wait for the compositor's frame callback,
    /// so a stream of changes (terminal output) draws at most once a frame.
    Always,
}

/// A source added with [`Ctx::insert_source`].
#[derive(Debug, Clone, Copy)]
pub struct SourceToken(RegistrationToken);

/// A first frame at an unknown fractional scale waits this long for
/// `preferred_scale`.
const SCALE_WAIT: Duration = Duration::from_millis(50);
/// Retry delay after a failed buffer allocation.
const ALLOC_RETRY: Duration = Duration::from_millis(250);
/// Retry delay for a redraw asked for on a surface that is not mapped and
/// so gets no frame callback.
const UNMAPPED_RETRY: Duration = Duration::from_millis(16);
/// Draw passes per loop iteration before yielding to other sources.
const FLUSH_PASSES: usize = 8;

enum Role {
    Window(Window),
    Popup {
        popup: Popup,
        parent: SurfaceId,
        grab: bool,
    },
    /// Created once the parent has shown a buffer.
    PendingPopup {
        parent: SurfaceId,
        spec: PopupSpec,
        serial: Option<u32>,
    },
}

impl Role {
    fn parent(&self) -> Option<SurfaceId> {
        match self {
            Role::Window(_) => None,
            Role::Popup { parent, .. } | Role::PendingPopup { parent, .. } => Some(*parent),
        }
    }

    fn grabbing(&self) -> bool {
        match self {
            Role::Window(_) => false,
            Role::Popup { grab, .. } => *grab,
            Role::PendingPopup { spec, serial, .. } => spec.grab && serial.is_some(),
        }
    }
}

struct Surface {
    // Dropped (destroyed) by hand in `Drop`: buffers, then the scale
    // objects, then the role, which destroys the wl_surface last.
    chain: Option<Swapchain>,
    role: Role,
    wl: WlSurface,
    fractional: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    info: SurfaceInfo,
    configured: bool,
    dirty: bool,
    frame_pending: bool,
    pacing: FramePacing,
    /// No buffer could be had this iteration; retried on the next wakeup.
    stalled: bool,
    /// Hold the first draw until `preferred_scale` arrives or this passes.
    scale_deadline: Option<Instant>,
    /// Scale and size last sent with a buffer.
    applied: Option<SurfaceInfo>,
    window_state: WindowState,
}

impl Surface {
    fn new(
        role: Role,
        wl: WlSurface,
        scale_objects: (Option<WpFractionalScaleV1>, Option<WpViewport>),
        info: SurfaceInfo,
        scale_deadline: Option<Instant>,
    ) -> Self {
        Self {
            chain: None,
            role,
            wl,
            fractional: scale_objects.0,
            viewport: scale_objects.1,
            info,
            configured: false,
            dirty: true,
            frame_pending: false,
            pacing: FramePacing::default(),
            stalled: false,
            scale_deadline,
            applied: None,
            window_state: WindowState::default(),
        }
    }

    fn ready(&self, now: Instant) -> bool {
        self.configured
            && self.dirty
            && !self.frame_pending
            && !self.stalled
            && self.scale_deadline.is_none_or(|d| now >= d)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // Free buffers are destroyed now; one the compositor still holds is
        // destroyed when it is released, which a compositor does when the
        // wl_surface goes.
        drop(self.chain.take());
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        // Window and Popup own their wl_surface and destroy it after their
        // xdg objects; a pending popup has no role object yet.
        if matches!(self.role, Role::PendingPopup { .. }) {
            self.wl.destroy();
        }
    }
}

/// `wl_keyboard` user data: sctk's, plus a hook for raw modifier masks.
pub(crate) struct KbData(KeyboardData<State>);

impl KeyboardDataExt for KbData {
    type State = State;

    fn keyboard_data(&self) -> &KeyboardData<State> {
        &self.0
    }

    fn keyboard_data_mut(&mut self) -> &mut KeyboardData<State> {
        &mut self.0
    }
}

#[derive(Default)]
pub(crate) struct SeatObjects {
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    pub(crate) data_device: Option<DataDevice>,
    pub(crate) primary_device: Option<PrimarySelectionDevice>,
    text_input: Option<ZwpTextInputV3>,
}

pub(crate) enum OwnedSource {
    Clipboard(CopyPasteSource),
    Primary(PrimarySelectionSource),
}

pub(crate) struct Runtime {
    pub(crate) conn: Connection,
    pub(crate) qh: QueueHandle<State>,
    pub(crate) handle: LoopHandle<'static, State>,
    signal: LoopSignal,
    registry: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    xdg: XdgShell,
    shm: Shm,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    cursor_shape: Option<CursorShapeManager>,
    pub(crate) data_manager: Option<DataDeviceManagerState>,
    pub(crate) primary_manager: Option<PrimarySelectionManagerState>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    pub(crate) seat: SeatObjects,
    surfaces: HashMap<SurfaceId, Surface>,
    by_wl: HashMap<ObjectId, SurfaceId>,
    next_id: u64,
    last_scale: Option<Scale>,
    pub(crate) queue: VecDeque<Event>,
    // keyboard
    keyboard_focus: Option<SurfaceId>,
    modifiers: Modifiers,
    repeat: Repeat,
    repeat_timer: Option<RegistrationToken>,
    repeat_key: Option<KeyEvent>,
    xkb: Option<XkbState>,
    /// The last `wl_keyboard.modifiers`: depressed, latched, locked, group.
    raw_mods: [u32; 4],
    // pointer
    pointer_focus: Option<(SurfaceId, u32)>,
    cursor: CursorShape,
    /// The latest input serial of any kind (selection requests).
    pub(crate) last_serial: u32,
    grabs: GrabSerials,
    // text input
    ime: ImeSerials,
    ime_want: Option<ImeState>,
    ime_sent: Option<ImeState>,
    ime_surface: Option<SurfaceId>,
    /// The target of the enabled input-method generation.
    ime_target: u64,
    // selections
    pub(crate) sources: HashMap<Selection, (OwnedSource, String)>,
    pub(crate) next_read: u64,
    pub(crate) selection_gens: HashMap<Selection, u64>,
    pub(crate) reads: HashMap<u64, PendingRead>,
    exit: bool,
    timers: HashMap<u64, RegistrationToken>,
    retry: Option<(Instant, RegistrationToken)>,
    ping: Ping,
    stats: Stats,
    waker: Sender<u64>,
}

pub(crate) struct State {
    pub(crate) rt: Runtime,
    app: Box<dyn App>,
}

/// The app's handle on the runtime.
pub struct Ctx<'a> {
    pub(crate) rt: &'a mut Runtime,
}

/// A buffer ready to draw into. Pixels are ARGB8888 in native (little)
/// endian byte order: B, G, R, A. Premultiplied alpha.
pub struct Frame<'a> {
    surface: SurfaceId,
    info: SurfaceInfo,
    fresh: bool,
    canvas: &'a mut [u8],
    commit: Option<Damage>,
    touched: bool,
    kept: bool,
}

impl Frame<'_> {
    pub fn surface(&self) -> SurfaceId {
        self.surface
    }

    pub fn info(&self) -> SurfaceInfo {
        self.info
    }

    /// True when the buffer holds nothing usable (first frame, resize, scale
    /// change): the app must draw everything. Otherwise the buffer holds the
    /// previously committed frame and only changed areas need drawing.
    pub fn needs_full_redraw(&self) -> bool {
        self.fresh
    }

    /// Pixels, width, height and stride (bytes per row), all physical.
    /// Taking the buffer without committing makes its contents unknown, so
    /// the next frame is a full one, unless [`Frame::keep_contents`] says
    /// nothing was written.
    pub fn buffer_mut(&mut self) -> (&mut [u8], u32, u32, u32) {
        self.touched = true;
        let (w, h) = self.info.physical;
        (self.canvas, w, h, w * 4)
    }

    /// Declare that no pixel was written in this draw, though the buffer
    /// was taken. Only matters when nothing is committed.
    pub fn keep_contents(&mut self) {
        self.kept = true;
    }

    /// Commit what was drawn, damaging the given physical rectangles. Can be
    /// called more than once before returning from `draw`; the damage adds up.
    /// A frame that [`Frame::needs_full_redraw`] is always committed with
    /// full damage.
    pub fn commit_with_damage(&mut self, rects: &[Rect]) {
        let (w, h) = self.info.physical;
        let damage = self.commit.get_or_insert_with(|| Damage::new(w, h));
        damage.extend(rects);
    }

    pub fn commit_full(&mut self) {
        let (w, h) = self.info.physical;
        self.commit
            .get_or_insert_with(|| Damage::new(w, h))
            .add_full();
    }
}

/// Connect, call [`App::init`], and run until [`Ctx::exit`].
pub fn run(app: impl App + 'static) -> Result<Stats, Error> {
    let conn = Connection::connect_to_env().map_err(|e| Error::Connect(e.to_string()))?;
    let (globals, queue) =
        registry_queue_init::<State>(&conn).map_err(|e| Error::Connect(e.to_string()))?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<'static, State> =
        EventLoop::try_new().map_err(|e| Error::Loop(e.to_string()))?;
    let handle = event_loop.handle();
    let (waker, wakes): (Sender<u64>, Channel<u64>) = calloop::channel::channel();
    handle
        .insert_source(wakes, |event, _, state: &mut State| {
            if let calloop::channel::Event::Msg(token) = event {
                state.emit(Event::Wake(token));
            }
        })
        .map_err(|e| Error::Loop(e.to_string()))?;
    // Wakes the loop for draws left over from a busy iteration.
    let (ping, ping_source) = calloop::ping::make_ping().map_err(|e| Error::Loop(e.to_string()))?;
    handle
        .insert_source(ping_source, |_, _, _: &mut State| {})
        .map_err(|e| Error::Loop(e.to_string()))?;
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| Error::Global(format!("wl_compositor: {e}")))?;
    let xdg =
        XdgShell::bind(&globals, &qh).map_err(|e| Error::Global(format!("xdg_wm_base: {e}")))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| Error::Global(format!("wl_shm: {e}")))?;
    let rt = Runtime {
        registry: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        xdg,
        shm,
        fractional_manager: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        cursor_shape: CursorShapeManager::bind(&globals, &qh).ok(),
        data_manager: DataDeviceManagerState::bind(&globals, &qh).ok(),
        primary_manager: PrimarySelectionManagerState::bind(&globals, &qh).ok(),
        text_input_manager: globals.bind(&qh, 1..=1, ()).ok(),
        seat: SeatObjects::default(),
        surfaces: HashMap::new(),
        by_wl: HashMap::new(),
        next_id: 1,
        last_scale: None,
        queue: VecDeque::new(),
        keyboard_focus: None,
        modifiers: Modifiers::default(),
        repeat: Repeat::default(),
        repeat_timer: None,
        repeat_key: None,
        xkb: None,
        raw_mods: [0; 4],
        pointer_focus: None,
        cursor: CursorShape::Default,
        last_serial: 0,
        grabs: GrabSerials::default(),
        ime: ImeSerials::default(),
        ime_want: None,
        ime_sent: None,
        ime_surface: None,
        ime_target: 0,
        sources: HashMap::new(),
        next_read: 0,
        selection_gens: HashMap::new(),
        reads: HashMap::new(),
        exit: false,
        timers: HashMap::new(),
        retry: None,
        ping,
        stats: Stats::default(),
        waker,
        signal: event_loop.get_signal(),
        conn: conn.clone(),
        qh: qh.clone(),
        handle: handle.clone(),
    };
    let mut state = State {
        rt,
        app: Box::new(app),
    };
    log::debug!(
        "globals: fractional={} viewporter={} cursor_shape={} data_device={} primary={} text_input={}",
        state.rt.fractional_manager.is_some(),
        state.rt.viewporter.is_some(),
        state.rt.cursor_shape.is_some(),
        state.rt.data_manager.is_some(),
        state.rt.primary_manager.is_some(),
        state.rt.text_input_manager.is_some()
    );
    WaylandSource::new(conn, queue)
        .insert(handle)
        .map_err(|e| Error::Loop(e.to_string()))?;
    {
        let mut cx = Ctx { rt: &mut state.rt };
        state.app.init(&mut cx);
    }
    state.drain();
    state.flush();
    event_loop
        .run(None, &mut state, State::flush)
        .map_err(|e| Error::Loop(e.to_string()))?;
    let stats = state.rt.stats;
    state.rt.surfaces.clear();
    let _ = state.rt.conn.flush();
    Ok(stats)
}

impl Ctx<'_> {
    pub fn create_window(&mut self, spec: WindowSpec) -> SurfaceId {
        self.rt.create_window(spec)
    }

    /// Destroy a toplevel and every popup on it.
    pub fn close_window(&mut self, id: SurfaceId) {
        self.rt.close_tree(id);
    }

    pub fn set_title(&mut self, id: SurfaceId, title: &str) {
        if let Some(Role::Window(w)) = self.rt.surfaces.get(&id).map(|s| &s.role) {
            w.set_title(title);
        }
    }

    pub fn set_maximized(&mut self, id: SurfaceId, maximized: bool) {
        if let Some(Role::Window(w)) = self.rt.surfaces.get(&id).map(|s| &s.role) {
            if maximized {
                w.set_maximized();
            } else {
                w.unset_maximized();
            }
        }
    }

    /// Open a popup on a toplevel or another popup.
    pub fn create_popup(
        &mut self,
        parent: SurfaceId,
        spec: &PopupSpec,
    ) -> Result<SurfaceId, Error> {
        self.rt.create_popup(parent, spec)
    }

    /// Move an open popup. The compositor answers with
    /// [`Event::PopupConfigure`] carrying `token` (xdg_wm_base v3+; older
    /// compositors ignore the request).
    pub fn reposition_popup(&mut self, id: SurfaceId, spec: &PopupSpec, token: u32) {
        self.rt.reposition_popup(id, spec, token);
    }

    /// Destroy a popup and every popup above it.
    pub fn close_popup(&mut self, id: SurfaceId) {
        self.rt.close_tree(id);
    }

    pub fn request_redraw(&mut self, id: SurfaceId) {
        if let Some(s) = self.rt.surfaces.get_mut(&id) {
            s.dirty = true;
        }
    }

    pub fn set_frame_pacing(&mut self, id: SurfaceId, pacing: FramePacing) {
        if let Some(s) = self.rt.surfaces.get_mut(&id) {
            s.pacing = pacing;
        }
    }

    pub fn surface_info(&self, id: SurfaceId) -> Option<SurfaceInfo> {
        self.rt.surfaces.get(&id).map(|s| s.info)
    }

    pub fn is_open(&self, id: SurfaceId) -> bool {
        self.rt.surfaces.contains_key(&id)
    }

    /// Popups whose parent is `id`.
    pub fn popups_of(&self, id: SurfaceId) -> Vec<SurfaceId> {
        self.rt.children(id)
    }

    pub fn set_cursor(&mut self, shape: CursorShape) {
        if self.rt.cursor != shape {
            self.rt.cursor = shape;
            self.rt.apply_cursor();
        }
    }

    /// Enable, update (`Some`) or disable (`None`) the input method. The
    /// input method is only active while the compositor has given text
    /// input focus to `state.surface`.
    pub fn set_ime(&mut self, state: Option<ImeState>) {
        self.rt.ime_want = state;
        self.rt.sync_ime();
    }

    /// Own `selection` with `text`, using the latest input serial.
    pub fn set_selection(&mut self, selection: Selection, text: String) -> bool {
        self.rt.set_selection(selection, text)
    }

    /// Read `selection` as text. The answer arrives as
    /// [`Event::SelectionText`] carrying the returned token, within two
    /// seconds.
    pub fn request_selection(&mut self, selection: Selection) -> u64 {
        self.rt.request_selection(selection)
    }

    pub fn modifiers(&self) -> Modifiers {
        self.rt.modifiers
    }

    pub fn keyboard_focus(&self) -> Option<SurfaceId> {
        self.rt.keyboard_focus
    }

    /// Arm (or re-arm) one-shot timer `token` to fire at `at`; it arrives
    /// as [`Event::Timer`]. Timers only exist while armed.
    pub fn set_timer(&mut self, token: u64, at: Instant) {
        self.cancel_timer(token);
        let delay = at.saturating_duration_since(Instant::now());
        let inserted = self.rt.handle.insert_source(
            Timer::from_duration(delay),
            move |_, _, state: &mut State| {
                state.rt.timers.remove(&token);
                state.emit(Event::Timer(token));
                TimeoutAction::Drop
            },
        );
        match inserted {
            Ok(t) => {
                self.rt.timers.insert(token, t);
            }
            Err(e) => log::error!("timer: {e}"),
        }
    }

    pub fn cancel_timer(&mut self, token: u64) {
        if let Some(t) = self.rt.timers.remove(&token) {
            self.rt.handle.remove(t);
        }
    }

    pub fn timer_armed(&self, token: u64) -> bool {
        self.rt.timers.contains_key(&token)
    }

    pub fn waker(&self) -> Waker {
        Waker(self.rt.waker.clone())
    }

    /// Add a calloop source (a pty, a socket) to the loop. The callback
    /// runs on the loop thread with a `Ctx`; to hand data to the app, keep
    /// it in shared state and call [`Ctx::notify`], which delivers
    /// [`Event::Wake`] once the callback returns.
    pub fn insert_source<S, F>(&mut self, source: S, mut callback: F) -> Result<SourceToken, Error>
    where
        S: EventSource + 'static,
        F: FnMut(S::Event, &mut S::Metadata, &mut Ctx<'_>) -> S::Ret + 'static,
    {
        self.rt
            .handle
            .insert_source(source, move |event, meta, state: &mut State| {
                let ret = callback(event, meta, &mut Ctx { rt: &mut state.rt });
                state.drain();
                ret
            })
            .map(SourceToken)
            .map_err(|e| Error::Loop(e.error.to_string()))
    }

    pub fn remove_source(&mut self, token: SourceToken) {
        self.rt.handle.remove(token.0);
    }

    /// Queue [`Event::Wake`] for `token` on this thread.
    pub fn notify(&mut self, token: u64) {
        self.rt.queue.push_back(Event::Wake(token));
    }

    pub fn stats(&self) -> Stats {
        self.rt.stats
    }

    pub fn exit(&mut self) {
        self.rt.exit = true;
    }
}

impl State {
    pub(crate) fn emit(&mut self, event: Event) {
        self.rt.queue.push_back(event);
        self.drain();
    }

    pub(crate) fn drain(&mut self) {
        while let Some(event) = self.rt.queue.pop_front() {
            let mut cx = Ctx { rt: &mut self.rt };
            self.app.event(&mut cx, event);
        }
    }

    /// Runs after every loop dispatch. Draws until no surface is both dirty
    /// and free to draw, so a redraw asked for by an event that a draw
    /// caused is not left waiting for a wakeup that never comes.
    fn flush(&mut self) {
        self.drain();
        for s in self.rt.surfaces.values_mut() {
            s.stalled = false;
        }
        let mut settled = false;
        for _ in 0..FLUSH_PASSES {
            self.rt.open_pending_popups();
            self.drain();
            let ready = self.rt.ready_surfaces(Instant::now());
            if ready.is_empty() {
                settled = true;
                break;
            }
            for id in ready {
                self.draw(id);
                self.drain();
            }
        }
        if !settled {
            // Let other sources run, then come straight back.
            self.rt.ping.ping();
        }
        let grabbing = self.rt.surfaces.values().any(|s| s.role.grabbing());
        self.rt.grabs.settle(grabbing);
        if self.rt.exit {
            self.rt.signal.stop();
        }
        let _ = self.rt.conn.flush();
    }

    fn draw(&mut self, id: SurfaceId) {
        let rt = &mut self.rt;
        let Some(surface) = rt.surfaces.get_mut(&id) else {
            return;
        };
        let info = surface.info;
        let mut chain = surface.chain.take().unwrap_or_else(Swapchain::new);
        let (w, h) = info.physical;
        let resized = chain.size() != (w, h);
        let acquired = match chain.acquire(&rt.shm, w, h) {
            Ok(acquired) => acquired,
            Err(e) => {
                surface.chain = Some(chain);
                surface.stalled = true;
                // A held buffer's release wakes the loop; a failed
                // allocation needs a timer.
                if let AcquireError::Alloc(msg) = e {
                    log::error!("surface {id:?}: {msg}; retrying");
                    rt.arm_retry(Instant::now() + ALLOC_RETRY);
                }
                return;
            }
        };
        let scale_changed = surface.applied.is_some_and(|a| a.scale != info.scale);
        surface.dirty = false;
        let fresh = acquired.fresh || resized || scale_changed;
        let index = acquired.index;
        let (commit, touched) = {
            let mut frame = Frame {
                surface: id,
                info,
                fresh,
                canvas: chain.canvas(index),
                commit: None,
                touched: false,
                kept: false,
            };
            let mut cx = Ctx { rt: &mut self.rt };
            self.app.draw(&mut cx, &mut frame);
            (frame.commit, frame.touched && !frame.kept)
        };
        let rt = &mut self.rt;
        rt.stats.buffer_allocations += chain.allocations;
        chain.allocations = 0;
        let Some(surface) = rt.surfaces.get_mut(&id) else {
            return;
        };
        let Some(mut damage) = commit.filter(|d| !d.is_empty()) else {
            chain.discard(index, touched);
            surface.chain = Some(chain);
            // Nothing to show, but the app asked for another frame while
            // drawing: wait for the display rather than spin.
            if surface.dirty {
                if surface.applied.is_some() {
                    surface.wl.frame(&rt.qh, surface.wl.clone());
                    surface.frame_pending = true;
                    surface.wl.commit();
                } else {
                    surface.stalled = true;
                    rt.arm_retry(Instant::now() + UNMAPPED_RETRY);
                }
            }
            return;
        };
        if fresh {
            // The buffer's old contents mean nothing to the compositor.
            damage.add_full();
        }
        let wl = &surface.wl;
        if surface.applied != Some(info) {
            match info.scale {
                Scale::Fractional(_) => {
                    if wl.version() >= 3 {
                        wl.set_buffer_scale(1);
                    }
                    if let Some(vp) = &surface.viewport {
                        vp.set_destination(info.logical.0 as i32, info.logical.1 as i32);
                    }
                }
                Scale::Integer(s) => {
                    if wl.version() >= 3 {
                        wl.set_buffer_scale(s.max(1));
                    }
                }
            }
            surface.applied = Some(info);
        }
        if let Err(e) = chain.buffer(index).attach_to(wl) {
            log::error!("attach: {e:?}");
            surface.chain = Some(chain);
            return;
        }
        for r in damage.rects() {
            if wl.version() >= 4 {
                wl.damage_buffer(r.x, r.y, r.width, r.height);
            } else {
                wl.damage(0, 0, i32::MAX, i32::MAX);
            }
        }
        // On demand, a frame callback only when the app already wants
        // another frame (animation, caret); otherwise the next change draws
        // at once and an idle surface gets no callback wakeup.
        if surface.dirty || surface.pacing == FramePacing::Always {
            wl.frame(&rt.qh, wl.clone());
            surface.frame_pending = true;
        }
        wl.commit();
        chain.committed(index, damage.rects());
        surface.chain = Some(chain);
        rt.stats.frames_committed += 1;
        rt.stats.first_commit.get_or_insert_with(Instant::now);
    }

    fn surface_id(&self, wl: &WlSurface) -> Option<SurfaceId> {
        self.rt.by_wl.get(&wl.id()).copied()
    }

    fn key_event(&self, event: SctkKeyEvent, state: KeyState) -> KeyEvent {
        let xkb = self.rt.xkb.as_ref();
        KeyEvent {
            surface: self.rt.keyboard_focus,
            state,
            keysym: event.keysym,
            base_keysym: xkb
                .and_then(|x| x.base_keysym(event.raw_code))
                .unwrap_or(event.keysym),
            raw_code: event.raw_code,
            text: event.utf8.filter(|t| !t.is_empty()),
            modifiers: self.rt.modifiers,
            consumed: xkb.map(|x| x.consumed(event.raw_code)).unwrap_or_default(),
            time: event.time,
        }
    }

    fn apply_repeat(&mut self, action: TimerAction) {
        match action {
            TimerAction::Keep => {}
            TimerAction::Disarm => {
                if let Some(token) = self.rt.repeat_timer.take() {
                    self.rt.handle.remove(token);
                }
                self.rt.repeat_key = None;
            }
            TimerAction::Arm(delay) => {
                if let Some(token) = self.rt.repeat_timer.take() {
                    self.rt.handle.remove(token);
                }
                let token = self
                    .rt
                    .handle
                    .insert_source(Timer::from_duration(delay), |_, _, state: &mut State| {
                        state.on_repeat()
                    });
                match token {
                    Ok(token) => self.rt.repeat_timer = Some(token),
                    Err(e) => log::error!("repeat timer: {e}"),
                }
            }
        }
    }

    fn on_repeat(&mut self) -> TimeoutAction {
        match self.rt.repeat.fire() {
            Some((raw, interval)) => {
                let Some(key) = self.rt.repeat_key.as_mut().filter(|k| k.raw_code == raw) else {
                    self.rt.repeat_timer = None;
                    return TimeoutAction::Drop;
                };
                key.time = key.time.wrapping_add(interval.as_millis() as u32);
                key.surface = self.rt.keyboard_focus;
                // Like X autorepeat and sctk's own repeat, a repeat reads the
                // key with the modifiers held now: hold `a`, press Shift, and
                // the repeats become `A`. Keysym, text and modifiers stay
                // consistent with each other.
                key.modifiers = self.rt.modifiers;
                if let Some(xkb) = &self.rt.xkb {
                    let t = xkb.translate(raw);
                    key.keysym = t.keysym;
                    key.text = t.text;
                    key.consumed = t.consumed;
                }
                let key = key.clone();
                self.emit(Event::Key(key));
                if self.rt.repeat_timer.is_none() {
                    // The app's handling disarmed the repeat.
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(interval)
            }
            None => {
                self.rt.repeat_timer = None;
                self.rt.repeat_key = None;
                TimeoutAction::Drop
            }
        }
    }
}

impl Runtime {
    fn alloc_id(&mut self) -> SurfaceId {
        let id = SurfaceId(self.next_id);
        self.next_id += 1;
        id
    }

    fn create_window(&mut self, spec: WindowSpec) -> SurfaceId {
        let id = self.alloc_id();
        let wl = self.compositor.create_surface(&self.qh);
        self.by_wl.insert(wl.id(), id);
        let scale_objects = self.scale_objects(&wl, id);
        let fractional = scale_objects.0.is_some();
        let scale = self.initial_scale(fractional, None);
        // With no scale seen yet, give preferred_scale a moment so the
        // first frame is not drawn at 1.0 and thrown away.
        let deadline =
            (fractional && self.last_scale.is_none()).then(|| Instant::now() + SCALE_WAIT);
        let window = self
            .xdg
            .create_window(wl.clone(), WindowDecorations::RequestServer, &self.qh);
        window.set_title(spec.title);
        window.set_app_id(spec.app_id);
        window.set_min_size(spec.min_size);
        if spec.maximized {
            window.set_maximized();
        }
        window.commit();
        self.surfaces.insert(
            id,
            Surface::new(
                Role::Window(window),
                wl,
                scale_objects,
                SurfaceInfo::new(spec.size, scale),
                deadline,
            ),
        );
        id
    }

    fn scale_objects(
        &self,
        wl: &WlSurface,
        id: SurfaceId,
    ) -> (Option<WpFractionalScaleV1>, Option<WpViewport>) {
        match (&self.fractional_manager, &self.viewporter) {
            (Some(m), Some(v)) => (
                Some(m.get_fractional_scale(wl, &self.qh, id)),
                Some(v.get_viewport(wl, &self.qh, ())),
            ),
            _ => (None, None),
        }
    }

    fn initial_scale(&self, fractional: bool, parent: Option<Scale>) -> Scale {
        match (fractional, parent.or(self.last_scale)) {
            (true, Some(s @ Scale::Fractional(_))) => s,
            (true, _) => Scale::Fractional(120),
            (false, Some(s @ Scale::Integer(_))) => s,
            (false, _) => Scale::Integer(1),
        }
    }

    fn positioner(&self, spec: &PopupSpec) -> Option<XdgPositioner> {
        let p = XdgPositioner::new(&self.xdg)
            .map_err(|e| log::error!("positioner: {e}"))
            .ok()?;
        let r = spec.anchor_rect;
        p.set_size(spec.size.0.max(1) as i32, spec.size.1.max(1) as i32);
        p.set_anchor_rect(r.x, r.y, r.width.max(1), r.height.max(1));
        p.set_anchor(spec.anchor);
        p.set_gravity(spec.gravity);
        p.set_constraint_adjustment(spec.constraint);
        p.set_offset(spec.offset.0, spec.offset.1);
        if spec.reactive && self.xdg.xdg_wm_base().version() >= 3 {
            p.set_reactive();
        }
        Some(p)
    }

    /// The popup surface exists at once; its `xdg_popup` is made once the
    /// parent has shown a buffer (a popup must not map before its parent).
    fn create_popup(&mut self, parent: SurfaceId, spec: &PopupSpec) -> Result<SurfaceId, Error> {
        let parent_scale = self
            .surfaces
            .get(&parent)
            .ok_or(Error::NoSuchSurface(parent))?
            .info
            .scale;
        let id = self.alloc_id();
        let wl = self.compositor.create_surface(&self.qh);
        self.by_wl.insert(wl.id(), id);
        let scale_objects = self.scale_objects(&wl, id);
        let scale = self.initial_scale(scale_objects.0.is_some(), Some(parent_scale));
        // The grab serial is fixed now, while the input that asked for the
        // popup is still the latest.
        let serial = if spec.grab {
            self.grabs.for_grab()
        } else {
            None
        };
        if spec.grab && serial.is_none() {
            log::warn!("popup {id:?}: no press to grab with; opening without a grab");
        }
        self.surfaces.insert(
            id,
            Surface::new(
                Role::PendingPopup {
                    parent,
                    spec: spec.clone(),
                    serial,
                },
                wl,
                scale_objects,
                SurfaceInfo::new(spec.size, scale),
                None,
            ),
        );
        self.open_pending_popups();
        Ok(id)
    }

    /// Make the `xdg_popup` of every pending popup whose parent is mapped,
    /// parents before children.
    pub(crate) fn open_pending_popups(&mut self) {
        loop {
            let next = self
                .surfaces
                .iter()
                .filter(|(_, s)| {
                    matches!(&s.role, Role::PendingPopup { parent, .. }
                        if self.surfaces.get(parent).is_some_and(|p| p.applied.is_some()))
                })
                .map(|(id, _)| *id)
                .min();
            let Some(id) = next else {
                break;
            };
            if let Err(e) = self.open_popup(id) {
                log::error!("popup {id:?}: {e}");
                self.close_tree(id);
                self.queue.push_back(Event::PopupDone { surface: id });
            }
        }
    }

    fn open_popup(&mut self, id: SurfaceId) -> Result<(), Error> {
        let Some(surface) = self.surfaces.get(&id) else {
            return Ok(());
        };
        let Role::PendingPopup {
            parent,
            spec,
            serial,
        } = &surface.role
        else {
            return Ok(());
        };
        let (parent, serial, wl) = (*parent, *serial, surface.wl.clone());
        let positioner = self
            .positioner(spec)
            .ok_or_else(|| Error::Global("xdg_positioner".into()))?;
        let parent_xdg = match self.surfaces.get(&parent).map(|p| &p.role) {
            Some(Role::Window(w)) => w.xdg_surface().clone(),
            Some(Role::Popup { popup, .. }) => popup.xdg_surface().clone(),
            _ => return Err(Error::NoSuchSurface(parent)),
        };
        let popup = Popup::from_surface(
            Some(&parent_xdg),
            &positioner,
            &self.qh,
            wl.clone(),
            &self.xdg,
        )
        .map_err(|e| Error::Global(format!("xdg_popup: {e}")))?;
        let grab = match (serial, &self.seat.seat) {
            (Some(serial), Some(seat)) => {
                popup.xdg_popup().grab(seat, serial);
                true
            }
            _ => false,
        };
        wl.commit();
        if let Some(surface) = self.surfaces.get_mut(&id) {
            surface.role = Role::Popup {
                popup,
                parent,
                grab,
            };
        }
        Ok(())
    }

    fn reposition_popup(&mut self, id: SurfaceId, spec: &PopupSpec, token: u32) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        if let Role::PendingPopup { spec: pending, .. } = &mut surface.role {
            // Not placed yet: it opens where it was last asked to be.
            let grab = pending.grab;
            *pending = PopupSpec {
                grab,
                ..spec.clone()
            };
            return;
        }
        if self.xdg.xdg_wm_base().version() < 3 {
            return;
        }
        let Some(positioner) = self.positioner(spec) else {
            return;
        };
        if let Some(Role::Popup { popup, .. }) = self.surfaces.get(&id).map(|s| &s.role) {
            popup.reposition(&positioner, token);
        }
    }

    fn children(&self, id: SurfaceId) -> Vec<SurfaceId> {
        let mut out: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.role.parent() == Some(id))
            .map(|(k, _)| *k)
            .collect();
        out.sort();
        out
    }

    /// Destroy `id` and its popups, topmost first.
    fn close_tree(&mut self, id: SurfaceId) {
        for child in self.children(id) {
            self.close_tree(child);
        }
        if let Some(surface) = self.surfaces.remove(&id) {
            self.by_wl.remove(&surface.wl.id());
            if self.keyboard_focus == Some(id) {
                self.keyboard_focus = None;
            }
            if self.pointer_focus.is_some_and(|(p, _)| p == id) {
                self.pointer_focus = None;
            }
            if self.ime_surface == Some(id) {
                self.ime_surface = None;
            }
            drop(surface);
            self.sync_ime();
        }
    }

    fn apply_cursor(&self) {
        if let (Some(device), Some((_, serial))) = (&self.seat.cursor_device, self.pointer_focus) {
            device.set_shape(serial, self.cursor);
        }
    }

    fn set_scale(&mut self, id: SurfaceId, scale: Scale) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        surface.scale_deadline = None;
        if surface.info.scale == scale {
            return;
        }
        self.last_scale = Some(scale);
        surface.info = SurfaceInfo::new(surface.info.logical, scale);
        surface.dirty = true;
        if surface.configured {
            self.queue.push_back(Event::ScaleChanged {
                surface: id,
                info: surface.info,
            });
        }
    }

    /// Surfaces to draw now, in id order (parents first). Arms the retry
    /// timer for the earliest one held back for its scale.
    fn ready_surfaces(&mut self, now: Instant) -> Vec<SurfaceId> {
        let mut ready: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.ready(now))
            .map(|(id, _)| *id)
            .collect();
        ready.sort();
        let waiting = self
            .surfaces
            .values()
            .filter(|s| s.configured && s.dirty && !s.ready(now))
            .filter_map(|s| s.scale_deadline.filter(|d| now < *d))
            .min();
        if let Some(at) = waiting {
            self.arm_retry(at);
        }
        ready
    }

    /// Wake the loop at `at` (the earliest of all requests wins).
    fn arm_retry(&mut self, at: Instant) {
        if let Some((armed, token)) = self.retry {
            if armed <= at {
                return;
            }
            self.handle.remove(token);
            self.retry = None;
        }
        let timer = Timer::from_deadline(at);
        match self.handle.insert_source(timer, |_, _, state: &mut State| {
            state.rt.retry = None;
            TimeoutAction::Drop
        }) {
            Ok(token) => self.retry = Some((at, token)),
            Err(e) => log::error!("retry timer: {e}"),
        }
    }

    fn update_xkb_mask(&mut self, mask: [u32; 4]) {
        self.raw_mods = mask;
        if let Some(xkb) = &mut self.xkb {
            xkb.update_mask(mask);
        }
    }

    pub(crate) fn sync_ime(&mut self) {
        let Some(input) = self.seat.text_input.clone() else {
            return;
        };
        let want = self.ime_want.clone().filter(|w| {
            Some(w.surface) == self.ime_surface && self.surfaces.contains_key(&w.surface)
        });
        let mut events = Vec::new();
        let step = ime::plan(self.ime.enabled(), self.ime_sent.as_ref(), want.as_ref());
        if matches!(step, ImePlan::Disable | ImePlan::Restart) {
            input.disable();
            input.commit();
            // What is dropped (a showing preedit) belongs to the old owner.
            let old = self.ime_target;
            events.extend(
                self.ime
                    .disabled_and_committed()
                    .into_iter()
                    .map(|e| (old, e)),
            );
            self.ime_sent = None;
        }
        match (want, step) {
            (Some(want), ImePlan::Enable | ImePlan::Restart) => {
                input.enable();
                send_ime_state(&input, None, &want);
                input.commit();
                self.ime.enabled_and_committed();
                self.ime_target = want.target;
                events.push((want.target, ImeEvent::Focus { active: true }));
                self.ime_sent = Some(want);
            }
            (Some(want), ImePlan::Update) => {
                send_ime_state(&input, self.ime_sent.as_ref(), &want);
                input.commit();
                self.ime.committed();
                self.ime_sent = Some(want);
            }
            (None, _) => self.ime_sent = None,
            _ => {}
        }
        let surface = self.ime_surface;
        self.queue
            .extend(events.into_iter().map(|(target, event)| Event::Ime {
                surface,
                target,
                event,
            }));
    }
}

fn send_ime_state(input: &ZwpTextInputV3, old: Option<&ImeState>, new: &ImeState) {
    if old.map(|o| o.cursor) != Some(new.cursor) {
        let c = new.cursor;
        input.set_cursor_rectangle(c.x, c.y, c.width.max(1), c.height.max(1));
    }
    if old.map(|o| (o.hint, o.purpose)) != Some((new.hint, new.purpose)) {
        input.set_content_type(new.hint, new.purpose);
    }
    let old_surrounding = old.and_then(|o| o.surrounding.as_ref());
    match &new.surrounding {
        Some(s) if old_surrounding != Some(s) => {
            input.set_surrounding_text(s.0.clone(), s.1, s.2);
        }
        // The protocol has no "unset"; empty text is how a client says it
        // has none. After an enable the state starts empty anyway.
        None if old_surrounding.is_some() => input.set_surrounding_text(String::new(), 0, 0),
        _ => {}
    }
}

fn convert_modifiers(m: SctkModifiers) -> Modifiers {
    Modifiers {
        ctrl: m.ctrl,
        alt: m.alt,
        shift: m.shift,
        logo: m.logo,
        caps_lock: m.caps_lock,
        num_lock: m.num_lock,
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        let Some(id) = self.surface_id(surface) else {
            return;
        };
        if self
            .rt
            .surfaces
            .get(&id)
            .is_some_and(|s| s.fractional.is_none())
        {
            self.rt.set_scale(id, Scale::Integer(new_factor.max(1)));
            self.drain();
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if let Some(id) = self.surface_id(surface)
            && let Some(s) = self.rt.surfaces.get_mut(&id)
        {
            s.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.rt.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(id) = self.surface_id(window.wl_surface()) {
            self.emit(Event::CloseRequested { surface: id });
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let Some(id) = self.surface_id(window.wl_surface()) else {
            return;
        };
        let Some(surface) = self.rt.surfaces.get_mut(&id) else {
            return;
        };
        let logical = (
            configure
                .new_size
                .0
                .map_or(surface.info.logical.0, |v| v.get()),
            configure
                .new_size
                .1
                .map_or(surface.info.logical.1, |v| v.get()),
        );
        let first = !surface.configured;
        let state = WindowState {
            maximized: configure.is_maximized(),
            fullscreen: configure.is_fullscreen(),
            activated: configure.is_activated(),
            resizing: configure.is_resizing(),
            server_decorations: configure.decoration_mode == DecorationMode::Server,
        };
        let info = SurfaceInfo::new(logical, surface.info.scale);
        let changed = first || info != surface.info || state != surface.window_state;
        surface.info = info;
        surface.window_state = state;
        surface.configured = true;
        if changed {
            surface.dirty = true;
            self.emit(Event::Configure {
                surface: id,
                info,
                state,
                first,
            });
        } else if surface.applied.is_none() {
            surface.dirty = true;
        }
    }
}

impl PopupHandler for State {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        config: PopupConfigure,
    ) {
        let Some(id) = self.surface_id(popup.wl_surface()) else {
            return;
        };
        let Some(surface) = self.rt.surfaces.get_mut(&id) else {
            return;
        };
        let size = (
            u32::try_from(config.width).unwrap_or(1),
            u32::try_from(config.height).unwrap_or(1),
        );
        let first = !surface.configured;
        let info = SurfaceInfo::new(size, surface.info.scale);
        surface.info = info;
        surface.configured = true;
        surface.dirty = true;
        let repositioned = match config.kind {
            ConfigureKind::Reposition { token } => Some(token),
            _ => None,
        };
        self.emit(Event::PopupConfigure {
            surface: id,
            info,
            position: config.position,
            first,
            repositioned,
        });
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        let Some(id) = self.surface_id(popup.wl_surface()) else {
            return;
        };
        if self.rt.surfaces.get(&id).is_some_and(|s| s.role.grabbing()) {
            self.rt.grabs.dismissed();
        }
        self.rt.close_tree(id);
        self.emit(Event::PopupDone { surface: id });
    }
}

impl Dispatch<WpFractionalScaleV1, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.rt.set_scale(*id, Scale::Fractional(scale.max(1)));
            state.drain();
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.rt.seat_state
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        if self.rt.seat.seat.is_some() {
            return;
        }
        let rt = &mut self.rt;
        rt.seat.data_device = rt
            .data_manager
            .as_ref()
            .map(|m| m.get_data_device(qh, &seat));
        rt.seat.primary_device = rt
            .primary_manager
            .as_ref()
            .map(|m| m.get_selection_device(qh, &seat));
        rt.seat.text_input = rt
            .text_input_manager
            .as_ref()
            .map(|m| m.get_text_input(&seat, qh, ()));
        rt.ime.reset();
        rt.seat.seat = Some(seat);
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if self.rt.seat.seat.as_ref() != Some(&seat) {
            return;
        }
        match capability {
            Capability::Keyboard if self.rt.seat.keyboard.is_none() => {
                let data = KbData(KeyboardData::new(seat.clone()));
                match self.rt.seat_state.get_keyboard_with_data(qh, &seat, data) {
                    Ok(k) => self.rt.seat.keyboard = Some(k),
                    Err(e) => log::error!("keyboard: {e}"),
                }
            }
            Capability::Pointer if self.rt.seat.pointer.is_none() => {
                match self.rt.seat_state.get_pointer(qh, &seat) {
                    Ok(p) => {
                        self.rt.seat.cursor_device = self
                            .rt
                            .cursor_shape
                            .as_ref()
                            .map(|m| m.get_shape_device(&p, qh));
                        self.rt.seat.pointer = Some(p);
                    }
                    Err(e) => log::error!("pointer: {e}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.rt.seat.keyboard.take() {
                    k.release();
                }
                let action = self.rt.repeat.leave();
                self.apply_repeat(action);
                self.rt.keyboard_focus = None;
            }
            Capability::Pointer => {
                if let Some(d) = self.rt.seat.cursor_device.take() {
                    d.destroy();
                }
                if let Some(p) = self.rt.seat.pointer.take() {
                    p.release();
                }
                self.rt.pointer_focus = None;
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        if self.rt.seat.seat.as_ref() != Some(&seat) {
            return;
        }
        let events = self.rt.ime.leave();
        if let Some(ti) = self.rt.seat.text_input.take() {
            ti.destroy();
        }
        let action = self.rt.repeat.leave();
        self.apply_repeat(action);
        self.rt.seat = SeatObjects::default();
        self.rt.keyboard_focus = None;
        self.rt.pointer_focus = None;
        self.rt.ime_surface = None;
        self.rt.ime_sent = None;
        let target = self.rt.ime_target;
        for event in events {
            self.rt.queue.push_back(Event::Ime {
                surface: None,
                target,
                event,
            });
        }
        self.drain();
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        let Some(id) = self.surface_id(surface) else {
            return;
        };
        self.rt.last_serial = serial;
        self.rt.keyboard_focus = Some(id);
        self.emit(Event::KeyboardFocus {
            surface: id,
            focused: true,
        });
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        _: u32,
    ) {
        let action = self.rt.repeat.leave();
        self.apply_repeat(action);
        let id = self.surface_id(surface);
        if self.rt.keyboard_focus == id {
            self.rt.keyboard_focus = None;
        }
        if let Some(id) = id {
            self.emit(Event::KeyboardFocus {
                surface: id,
                focused: false,
            });
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        serial: u32,
        event: SctkKeyEvent,
    ) {
        self.rt.last_serial = serial;
        self.rt.grabs.pressed(serial);
        let repeats = self
            .rt
            .xkb
            .as_ref()
            .is_none_or(|x| x.repeats(event.raw_code));
        let key = self.key_event(event, KeyState::Pressed);
        let action = self.rt.repeat.press(key.raw_code, repeats);
        self.apply_repeat(action);
        if matches!(action, TimerAction::Arm(_)) {
            self.rt.repeat_key = Some(KeyEvent {
                state: KeyState::Repeated,
                ..key.clone()
            });
        }
        self.emit(Event::Key(key));
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        serial: u32,
        event: SctkKeyEvent,
    ) {
        self.rt.last_serial = serial;
        let key = self.key_event(event, KeyState::Released);
        let action = self.rt.repeat.release(key.raw_code);
        self.apply_repeat(action);
        self.emit(Event::Key(key));
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        modifiers: SctkModifiers,
        _: u32,
    ) {
        let m = convert_modifiers(modifiers);
        if m != self.rt.modifiers {
            self.rt.modifiers = m;
            self.emit(Event::Modifiers(m));
        }
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        info: RepeatInfo,
    ) {
        let rate = match info {
            RepeatInfo::Repeat { rate, delay } => RepeatRate::Repeat {
                rate: rate.get(),
                delay,
            },
            RepeatInfo::Disable => RepeatRate::Disabled,
        };
        let action = self.rt.repeat.set_rate(rate);
        self.apply_repeat(action);
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        self.rt.xkb = XkbState::new(keymap.as_string(), self.rt.raw_mods);
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[SctkPointerEvent],
    ) {
        for event in events {
            let Some(id) = self.surface_id(&event.surface) else {
                continue;
            };
            let kind = match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.rt.pointer_focus = Some((id, serial));
                    self.rt.apply_cursor();
                    PointerKind::Enter
                }
                PointerEventKind::Leave { .. } => {
                    if self.rt.pointer_focus.is_some_and(|(p, _)| p == id) {
                        self.rt.pointer_focus = None;
                    }
                    PointerKind::Leave
                }
                PointerEventKind::Motion { .. } => PointerKind::Motion,
                PointerEventKind::Press { button, serial, .. } => {
                    self.rt.last_serial = serial;
                    self.rt.grabs.pressed(serial);
                    PointerKind::Button {
                        button,
                        state: ButtonState::Pressed,
                    }
                }
                PointerEventKind::Release { button, serial, .. } => {
                    self.rt.last_serial = serial;
                    PointerKind::Button {
                        button,
                        state: ButtonState::Released,
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => PointerKind::Axis {
                    horizontal: horizontal.absolute,
                    vertical: vertical.absolute,
                    horizontal_120: horizontal.discrete.saturating_mul(120),
                    vertical_120: vertical.discrete.saturating_mul(120),
                    stop: horizontal.stop || vertical.stop,
                },
            };
            self.emit(Event::Pointer(PointerEvent {
                surface: id,
                position: event.position,
                kind,
                modifiers: self.rt.modifiers,
            }));
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.rt.seat.text_input.as_ref() != Some(input) {
            return;
        }
        let rt = &mut state.rt;
        let surface = rt.ime_surface;
        // Results belong to the owner of the generation they arrived in.
        let target = rt.ime_target;
        let events = match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                rt.ime_surface = rt.by_wl.get(&surface.id()).copied();
                rt.sync_ime();
                Vec::new()
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                // The compositor ignores requests until the next enter, so
                // nothing is sent; the next enter re-enables.
                let events = rt.ime.leave();
                rt.ime_surface = None;
                rt.ime_sent = None;
                events
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                rt.ime.preedit(text, cursor_begin, cursor_end);
                Vec::new()
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                rt.ime.commit_string(text);
                Vec::new()
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                rt.ime.delete_surrounding(before_length, after_length);
                Vec::new()
            }
            zwp_text_input_v3::Event::Done { serial } => rt.ime.done(serial),
            _ => Vec::new(),
        };
        rt.queue.extend(events.into_iter().map(|event| Event::Ime {
            surface,
            target,
            event,
        }));
        state.drain();
    }
}

impl Dispatch<WlKeyboard, KbData> for State {
    fn event(
        state: &mut Self,
        keyboard: &WlKeyboard,
        event: wl_keyboard::Event,
        data: &KbData,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // Our xkb state follows the same masks as sctk's, before sctk calls
        // `update_modifiers`.
        if let wl_keyboard::Event::Modifiers {
            mods_depressed,
            mods_latched,
            mods_locked,
            group,
            ..
        } = &event
        {
            state
                .rt
                .update_xkb_mask([*mods_depressed, *mods_latched, *mods_locked, *group]);
        }
        <SeatState as Dispatch<WlKeyboard, KbData, State>>::event(
            state, keyboard, event, data, conn, qh,
        );
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.rt.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.rt.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_xdg_popup!(State);
delegate_registry!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore ZwpTextInputManagerV3);
