use std::{
    collections::VecDeque,
    env,
    io::{Read, Write},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use bevy::{
    app::{AppExit, Last},
    prelude::*,
    window::{
        ExitSystems, PrimaryWindow, RawHandleWrapper, WindowCloseRequested, WindowPlugin,
        WindowResolution,
    },
    winit::{UpdateMode, WinitSettings},
};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use smithay_client_toolkit::{
    data_device_manager::{
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{DataSourceHandler, DragSource},
        DataDeviceManagerState, WritePipe,
    },
    delegate_data_device, delegate_pointer, delegate_registry, delegate_seat,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT},
        Capability, SeatHandler, SeatState,
    },
};
use wayland_backend::{client::ObjectId, sys::client::Backend};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
        wl_data_source::WlDataSource, wl_pointer::WlPointer, wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    Connection, EventQueue, Proxy, QueueHandle,
};

const URI_MIME: &str = "text/uri-list";
const TEXT_MIME: &str = "text/plain;charset=utf-8";
const SPIKE_MIME: &str = "application/x-cosmix-dnd-spike";
const PAYLOAD_PATH: &str = "/tmp/wl-dnd-spike-payload.txt";
const PAYLOAD_TEXT: &str = "wl-dnd-spike test payload\n";

fn main() {
    let reactive_ms = env::var("WLDND_REACTIVE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000);

    App::new()
        .insert_resource(ClearColor(Color::srgb(0.055, 0.065, 0.08)))
        .insert_resource(DndLifecycle::default())
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(reactive_ms)),
            unfocused_mode: UpdateMode::reactive(Duration::from_millis(reactive_ms)),
        })
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "wl-dnd-spike".into(),
                resolution: WindowResolution::new(900, 600),
                ..default()
            }),
            ..default()
        }))
        .add_systems(Startup, setup_ui)
        .add_systems(PreUpdate, teardown_on_window_close)
        .add_systems(Update, (init_wayland, pump_wayland).chain())
        .add_systems(Last, teardown_on_app_exit.before(ExitSystems))
        .run();
}

#[derive(Resource, Default)]
struct DndLifecycle {
    attempted: bool,
    tearing_down: bool,
}

fn setup_ui(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                padding: UiRect::all(px(28)),
                column_gap: px(28),
                align_items: AlignItems::Stretch,
                ..default()
            },
            BackgroundColor(Color::srgb(0.055, 0.065, 0.08)),
        ))
        .with_children(|root| {
            spawn_zone(
                root,
                "DROP HERE",
                "Accepts text/uri-list on this half",
                Color::srgb(0.10, 0.30, 0.26),
            );
            spawn_zone(
                root,
                "DRAG FROM HERE",
                "Press, hold, then move at least 30 px",
                Color::srgb(0.32, 0.16, 0.22),
            );
        });
}

fn spawn_zone(parent: &mut ChildSpawnerCommands, label: &str, detail: &str, colour: Color) {
    parent
        .spawn((
            Node {
                width: percent(50),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(18),
                border: UiRect::all(px(3)),
                ..default()
            },
            BackgroundColor(colour),
            BorderColor::all(Color::srgb(0.70, 0.76, 0.82)),
        ))
        .with_children(|zone| {
            zone.spawn((
                Text::new(label),
                TextFont {
                    font_size: FontSize::Px(48.0),
                    ..default()
                },
                TextColor(Color::WHITE),
            ));
            zone.spawn((
                Text::new(detail),
                TextFont {
                    font_size: FontSize::Px(19.0),
                    ..default()
                },
                TextColor(Color::srgb(0.82, 0.86, 0.90)),
            ));
        });
}

fn init_wayland(world: &mut World) {
    let should_attempt = {
        let lifecycle = world.resource::<DndLifecycle>();
        !lifecycle.attempted && !lifecycle.tearing_down
    };
    if !should_attempt {
        return;
    }

    let candidate = {
        let mut windows =
            world.query_filtered::<(&Window, &RawHandleWrapper), With<PrimaryWindow>>();
        windows
            .iter(world)
            .next()
            .map(|(window, handle)| (window.resolution.clone(), handle.clone()))
    };
    let Some((resolution, handle)) = candidate else {
        return;
    };

    world.resource_mut::<DndLifecycle>().attempted = true;
    let mode = Mode::from_env();
    let delay = env::var("WLDND_SEND_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    match DndRuntime::new(&handle, resolution, mode, Duration::from_millis(delay)) {
        Ok(runtime) => world.insert_non_send(runtime),
        Err(InitError::NotWayland) => {
            println!("WLDND ABORT not_wayland");
            world.insert_non_send(DndRuntime { bridge: None });
        }
        Err(InitError::Other(message)) => {
            eprintln!("WLDND ERR init={}", one_line(&message));
            world.insert_non_send(DndRuntime { bridge: None });
        }
    }
}

fn pump_wayland(
    runtime: Option<NonSendMut<DndRuntime>>,
    window: Single<&Window, With<PrimaryWindow>>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };
    let Some(bridge) = runtime.bridge.as_mut() else {
        return;
    };

    bridge.state.window_width = f64::from(window.resolution.width());
    bridge.state.scale_factor = f64::from(window.resolution.scale_factor());

    match bridge.event_queue.dispatch_pending(&mut bridge.state) {
        Ok(dispatched) => {
            bridge.events_this_second += dispatched;
            bridge.state.process_pending(&bridge.qh);
        }
        Err(error) => eprintln!("WLDND ERR dispatch={}", one_line(&error.to_string())),
    }

    bridge.state.poll_threads();
    bridge.state.maybe_start_delayed_drag(&bridge.qh);
    bridge.state.check_stale_serial();

    if let Err(error) = bridge.connection.flush() {
        eprintln!("WLDND ERR flush={}", one_line(&error.to_string()));
    }

    if bridge.heartbeat.elapsed() >= Duration::from_secs(1) {
        println!("WLDND PUMP dispatched={}", bridge.events_this_second);
        bridge.heartbeat = Instant::now();
        bridge.events_this_second = 0;
    }
}

fn teardown_on_window_close(
    mut close: MessageReader<WindowCloseRequested>,
    mut commands: Commands,
    mut lifecycle: ResMut<DndLifecycle>,
) {
    if close.read().next().is_some() {
        queue_teardown(&mut commands, &mut lifecycle);
    }
}

fn teardown_on_app_exit(
    mut exits: MessageReader<AppExit>,
    mut commands: Commands,
    mut lifecycle: ResMut<DndLifecycle>,
) {
    if exits.read().next().is_some() {
        queue_teardown(&mut commands, &mut lifecycle);
    }
}

fn queue_teardown(commands: &mut Commands, lifecycle: &mut DndLifecycle) {
    if lifecycle.tearing_down {
        return;
    }
    lifecycle.tearing_down = true;
    commands.queue(|world: &mut World| {
        if let Some(mut runtime) = world.remove_non_send::<DndRuntime>() {
            runtime.teardown();
            println!("WLDND PROBE3 teardown_ok");
        }
    });
}

struct DndRuntime {
    bridge: Option<WaylandBridge>,
}

impl DndRuntime {
    fn new(
        handle: &RawHandleWrapper,
        resolution: WindowResolution,
        mode: Mode,
        send_delay: Duration,
    ) -> Result<Self, InitError> {
        let (display, surface) = match (handle.get_display_handle(), handle.get_window_handle()) {
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
                (display.display.as_ptr(), window.surface.as_ptr())
            }
            _ => return Err(InitError::NotWayland),
        };

        // SAFETY: Bevy's RawHandleWrapper retains the winit window, so the wl_display
        // remains alive until this NonSend resource is explicitly dropped before window teardown.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, event_queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|error| InitError::Other(error.to_string()))?;
        let qh = event_queue.handle();
        let globals_count = globals
            .contents()
            .with_list(<[wayland_client::globals::Global]>::len);

        // SAFETY: raw-window-handle's Wayland surface pointer belongs to this same
        // wl_display and RawHandleWrapper keeps it valid for the resource lifetime.
        let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
            .map_err(|error| InitError::Other(error.to_string()))?;
        let wl_surface = WlSurface::from_id(&connection, surface_id)
            .map_err(|error| InitError::Other(error.to_string()))?;

        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        let data_device_manager_state = DataDeviceManagerState::bind(&globals, &qh)
            .map_err(|error| InitError::Other(error.to_string()))?;
        let seat_objects = seat_state
            .seats()
            .map(|seat| SeatObjects {
                data_device: data_device_manager_state.get_data_device(&qh, &seat),
                seat,
                pointer: None,
            })
            .collect();
        let (drop_tx, drop_rx) = mpsc::channel();
        let (io_tx, io_rx) = mpsc::channel();

        let state = WaylandState {
            registry_state,
            seat_state,
            data_device_manager_state,
            surface: wl_surface,
            seat_objects,
            pending: VecDeque::new(),
            mode,
            send_delay,
            window_width: f64::from(resolution.width()),
            scale_factor: f64::from(resolution.scale_factor()),
            grab: None,
            active_drag: None,
            active_receive: None,
            last_motion_log: None,
            drop_tx,
            drop_rx,
            io_tx,
            io_rx,
        };

        println!("WLDND PROBE1 connect_ok globals={globals_count}");
        println!(
            "WLDND PROBE2 reactive_ms={}",
            env::var("WLDND_REACTIVE_MS").unwrap_or_else(|_| "1000".into())
        );

        Ok(Self {
            bridge: Some(WaylandBridge {
                connection,
                event_queue,
                qh,
                state,
                heartbeat: Instant::now(),
                events_this_second: 0,
            }),
        })
    }

    fn teardown(&mut self) {
        if let Some(mut bridge) = self.bridge.take() {
            bridge.state.active_drag.take();
            bridge.state.grab.take();
            for seat in &mut bridge.state.seat_objects {
                if let Some(pointer) = seat.pointer.take() {
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            bridge.state.seat_objects.clear();
            let _ = bridge.connection.flush();
        }
    }
}

struct WaylandBridge {
    connection: Connection,
    event_queue: EventQueue<WaylandState>,
    qh: QueueHandle<WaylandState>,
    state: WaylandState,
    heartbeat: Instant,
    events_this_second: usize,
}

#[derive(Debug)]
enum InitError {
    NotWayland,
    Other(String),
}

#[derive(Clone, Copy)]
struct Mode {
    receive: bool,
    send: bool,
}

impl Mode {
    fn from_env() -> Self {
        match env::var("WLDND_MODE")
            .unwrap_or_else(|_| "all".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "receive" | "recv" => Self {
                receive: true,
                send: false,
            },
            "send" => Self {
                receive: false,
                send: true,
            },
            "echo" | "all" => Self {
                receive: true,
                send: true,
            },
            other => {
                eprintln!("WLDND ERR unknown_mode={}; using all", one_line(other));
                Self {
                    receive: true,
                    send: true,
                }
            }
        }
    }
}

struct SeatObjects {
    seat: WlSeat,
    pointer: Option<WlPointer>,
    data_device: DataDevice,
}

struct GrabToken {
    seat: WlSeat,
    pointer: WlPointer,
    serial: u32,
    origin: WlSurface,
    button: u32,
    pressed: bool,
    press_position: (f64, f64),
    current_position: (f64, f64),
    pressed_at: Instant,
    threshold_at: Option<Instant>,
    drag_attempted: bool,
}

struct ActiveDrag {
    source: DragSource,
    started_at: Instant,
    saw_source_event: bool,
    stale_logged: bool,
}

struct ActiveReceive {
    offer: DragOffer,
    mime: Option<String>,
    accepted: bool,
}

enum Pending {
    SeatAdded(WlSeat),
    SeatRemoved(WlSeat),
    CapabilityAdded(WlSeat, Capability),
    CapabilityRemoved(WlSeat, Capability),
    PointerFrame(WlPointer, Vec<PointerEvent>),
    ReceiveEnter(WlDataDevice, f64, f64, WlSurface),
    ReceiveMotion(WlDataDevice, f64, f64),
    ReceiveLeave(WlDataDevice),
    ReceiveDrop(WlDataDevice),
    ReceiveSourceActions(DragOffer, DndAction),
    ReceiveSelectedAction(DragOffer, DndAction),
    Selection,
    SourceAccept(WlDataSource, Option<String>),
    SourceSend(WlDataSource, String, WritePipe),
    SourceCancelled(WlDataSource),
    SourceDropped(WlDataSource),
    SourceFinished(WlDataSource),
    SourceAction(WlDataSource, DndAction),
}

struct DropResult {
    offer: DragOffer,
    payload: Result<String, String>,
}

struct IoResult {
    context: String,
    result: Result<(), String>,
}

struct WaylandState {
    registry_state: RegistryState,
    seat_state: SeatState,
    data_device_manager_state: DataDeviceManagerState,
    surface: WlSurface,
    seat_objects: Vec<SeatObjects>,
    pending: VecDeque<Pending>,
    mode: Mode,
    send_delay: Duration,
    window_width: f64,
    scale_factor: f64,
    grab: Option<GrabToken>,
    active_drag: Option<ActiveDrag>,
    active_receive: Option<ActiveReceive>,
    last_motion_log: Option<Instant>,
    drop_tx: Sender<DropResult>,
    drop_rx: Receiver<DropResult>,
    io_tx: Sender<IoResult>,
    io_rx: Receiver<IoResult>,
}

impl WaylandState {
    fn process_pending(&mut self, qh: &QueueHandle<Self>) {
        while let Some(event) = self.pending.pop_front() {
            match event {
                Pending::SeatAdded(seat) => self.add_seat(qh, seat),
                Pending::SeatRemoved(seat) => self.remove_seat_objects(&seat),
                Pending::CapabilityAdded(seat, capability) => {
                    self.add_capability(qh, &seat, capability)
                }
                Pending::CapabilityRemoved(seat, capability) => {
                    self.remove_capability_objects(&seat, capability)
                }
                Pending::PointerFrame(pointer, events) => {
                    self.process_pointer_frame(qh, &pointer, events)
                }
                Pending::ReceiveEnter(device, x, y, surface) => {
                    self.receive_enter(&device, x, y, &surface)
                }
                Pending::ReceiveMotion(device, x, y) => self.receive_motion(&device, x, y),
                Pending::ReceiveLeave(device) => self.receive_leave(&device),
                Pending::ReceiveDrop(device) => self.receive_drop(&device),
                Pending::ReceiveSourceActions(offer, actions) => {
                    if let Some(active) = self.active_receive.as_mut() {
                        if active.offer == offer {
                            active.offer = offer;
                        }
                    }
                    if self.mode.receive {
                        println!("WLDND RECV source_actions={actions:?}");
                    }
                }
                Pending::ReceiveSelectedAction(offer, action) => {
                    if let Some(active) = self.active_receive.as_mut() {
                        if active.offer == offer {
                            active.offer = offer;
                        }
                    }
                    if self.mode.receive {
                        println!("WLDND RECV action={action:?}");
                    }
                }
                Pending::Selection => {}
                Pending::SourceAccept(source, mime) => {
                    self.note_source_event(&source);
                    let accepted = mime.as_deref().filter(|value| !value.is_empty());
                    println!("WLDND SEND target_accepts={}", accepted.unwrap_or("none"));
                }
                Pending::SourceSend(source, mime, pipe) => {
                    self.note_source_event(&source);
                    println!("WLDND SEND send_request mime={}", one_line(&mime));
                    self.spawn_source_write(mime, pipe);
                }
                Pending::SourceCancelled(source) => {
                    self.note_source_event(&source);
                    println!("WLDND SEND cancelled");
                    self.finish_source(&source);
                }
                Pending::SourceDropped(source) => {
                    self.note_source_event(&source);
                    println!("WLDND SEND dnd_drop_performed");
                }
                Pending::SourceFinished(source) => {
                    self.note_source_event(&source);
                    println!("WLDND SEND dnd_finished");
                    self.finish_source(&source);
                }
                Pending::SourceAction(source, action) => {
                    self.note_source_event(&source);
                    println!("WLDND SEND action={action:?}");
                }
            }
        }
    }

    fn add_seat(&mut self, qh: &QueueHandle<Self>, seat: WlSeat) {
        if self.seat_objects.iter().any(|objects| objects.seat == seat) {
            return;
        }
        let data_device = self.data_device_manager_state.get_data_device(qh, &seat);
        self.seat_objects.push(SeatObjects {
            seat,
            pointer: None,
            data_device,
        });
    }

    fn add_capability(&mut self, qh: &QueueHandle<Self>, seat: &WlSeat, capability: Capability) {
        if !self
            .seat_objects
            .iter()
            .any(|objects| &objects.seat == seat)
        {
            self.add_seat(qh, seat.clone());
        }
        if capability == Capability::Pointer {
            let needs_pointer = self
                .seat_objects
                .iter()
                .find(|objects| &objects.seat == seat)
                .is_some_and(|objects| objects.pointer.is_none());
            if needs_pointer {
                match self.seat_state.get_pointer(qh, seat) {
                    Ok(pointer) => {
                        if let Some(objects) = self
                            .seat_objects
                            .iter_mut()
                            .find(|objects| &objects.seat == seat)
                        {
                            objects.pointer = Some(pointer);
                        }
                    }
                    Err(error) => {
                        eprintln!("WLDND ERR pointer_bind={}", one_line(&error.to_string()))
                    }
                }
            }
        }
        self.log_seat(seat);
    }

    fn log_seat(&self, seat: &WlSeat) {
        let Some(info) = self.seat_state.info(seat) else {
            eprintln!("WLDND ERR seat_info=dead");
            return;
        };
        let mut caps = Vec::new();
        if info.has_keyboard {
            caps.push("keyboard");
        }
        if info.has_pointer {
            caps.push("pointer");
        }
        if info.has_touch {
            caps.push("touch");
        }
        println!(
            "WLDND PROBE1 seat={} caps={}",
            info.name.as_deref().unwrap_or("unknown"),
            if caps.is_empty() {
                "none".into()
            } else {
                caps.join("|")
            }
        );
    }

    fn remove_capability_objects(&mut self, seat: &WlSeat, capability: Capability) {
        if capability != Capability::Pointer {
            self.log_seat(seat);
            return;
        }
        if let Some(objects) = self
            .seat_objects
            .iter_mut()
            .find(|objects| &objects.seat == seat)
        {
            if let Some(pointer) = objects.pointer.take() {
                if pointer.version() >= 3 {
                    pointer.release();
                }
            }
        }
        self.cancel_sessions_for_seat(seat);
        self.log_seat(seat);
    }

    fn remove_seat_objects(&mut self, seat: &WlSeat) {
        self.cancel_sessions_for_seat(seat);
        if let Some(index) = self
            .seat_objects
            .iter()
            .position(|objects| &objects.seat == seat)
        {
            let mut objects = self.seat_objects.remove(index);
            if let Some(pointer) = objects.pointer.take() {
                if pointer.version() >= 3 {
                    pointer.release();
                }
            }
        }
    }

    fn cancel_sessions_for_seat(&mut self, seat: &WlSeat) {
        if self.grab.as_ref().is_some_and(|grab| &grab.seat == seat) {
            self.grab.take();
            self.active_drag.take();
            println!("WLDND SEND cancelled seat_removed");
        }
    }

    fn process_pointer_frame(
        &mut self,
        qh: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: Vec<PointerEvent>,
    ) {
        let Some(seat) = self
            .seat_objects
            .iter()
            .find(|objects| objects.pointer.as_ref() == Some(pointer))
            .map(|objects| objects.seat.clone())
        else {
            eprintln!("WLDND ERR pointer_frame=unknown_pointer");
            return;
        };

        for event in events {
            if event.surface != self.surface {
                continue;
            }
            match event.kind {
                PointerEventKind::Press { button, serial, .. } => {
                    self.grab = Some(GrabToken {
                        seat: seat.clone(),
                        pointer: pointer.clone(),
                        serial,
                        origin: event.surface,
                        button,
                        pressed: true,
                        press_position: event.position,
                        current_position: event.position,
                        pressed_at: Instant::now(),
                        threshold_at: None,
                        drag_attempted: false,
                    });
                }
                PointerEventKind::Motion { .. } => {
                    if let Some(grab) = self.grab.as_mut() {
                        if grab.pointer == *pointer && grab.pressed {
                            grab.current_position = event.position;
                            let dx = event.position.0 - grab.press_position.0;
                            let dy = event.position.1 - grab.press_position.1;
                            if dx.hypot(dy) > 30.0 && grab.threshold_at.is_none() {
                                grab.threshold_at = Some(Instant::now());
                            }
                        }
                    }
                    self.maybe_start_delayed_drag(qh);
                }
                PointerEventKind::Release { button, .. }
                    if self
                        .grab
                        .as_ref()
                        .is_some_and(|grab| grab.pointer == *pointer && grab.button == button) =>
                {
                    if let Some(grab) = self.grab.as_mut() {
                        grab.pressed = false;
                    }
                    self.grab.take();
                }
                _ => {}
            }
        }
    }

    fn maybe_start_delayed_drag(&mut self, qh: &QueueHandle<Self>) {
        if !self.mode.send || self.active_drag.is_some() {
            return;
        }
        let Some(grab) = self.grab.as_ref() else {
            return;
        };
        let Some(threshold_at) = grab.threshold_at else {
            return;
        };
        if !grab.pressed
            || grab.button != BTN_LEFT
            || grab.drag_attempted
            || grab.press_position.0 < self.window_width / 2.0
            || threshold_at.elapsed() < self.send_delay
        {
            return;
        }

        let serial = grab.serial;
        let age_ms = grab.pressed_at.elapsed().as_millis();
        let seat = grab.seat.clone();
        let origin = grab.origin.clone();
        if let Some(grab) = self.grab.as_mut() {
            grab.drag_attempted = true;
        }
        if let Err(error) = std::fs::write(PAYLOAD_PATH, PAYLOAD_TEXT) {
            eprintln!("WLDND ERR payload_write={}", one_line(&error.to_string()));
            return;
        }
        let Some(data_device) = self
            .seat_objects
            .iter()
            .find(|objects| objects.seat == seat)
            .map(|objects| &objects.data_device)
        else {
            eprintln!("WLDND ERR start_drag=no_data_device");
            return;
        };

        let source = self.data_device_manager_state.create_drag_and_drop_source(
            qh,
            [URI_MIME, TEXT_MIME, SPIKE_MIME],
            DndAction::Copy | DndAction::Move,
        );
        source.start_drag(data_device, &origin, None, serial);
        println!("WLDND SEND started serial={serial} age_ms={age_ms}");
        self.active_drag = Some(ActiveDrag {
            source,
            started_at: Instant::now(),
            saw_source_event: false,
            stale_logged: false,
        });
    }

    fn check_stale_serial(&mut self) {
        let held = self.grab.as_ref().is_some_and(|grab| grab.pressed);
        let Some(active) = self.active_drag.as_mut() else {
            return;
        };
        if held
            && !active.saw_source_event
            && !active.stale_logged
            && active.started_at.elapsed() >= Duration::from_secs(2)
        {
            active.stale_logged = true;
            println!("WLDND SEND probe7 serial_stale suspected");
        }
    }

    fn receive_enter(&mut self, device: &WlDataDevice, x: f64, y: f64, surface: &WlSurface) {
        if surface != &self.surface {
            return;
        }
        let Some(offer) = self.drag_offer(device) else {
            eprintln!("WLDND ERR recv_enter=no_offer");
            return;
        };
        let mimes = offer.with_mime_types(<[String]>::to_vec);
        if mimes.iter().any(|mime| mime == SPIKE_MIME) && self.active_drag.is_some() {
            println!("WLDND ECHO own_drag_entered mimes={mimes:?}");
        }
        if self.mode.receive {
            println!(
                "WLDND RECV enter x={x:.2} y={y:.2} mimes={mimes:?} source_actions={:?}",
                offer.source_actions
            );
            println!(
                "WLDND SCALE factor={:.3} surface=({x:.2},{y:.2}) logical=({:.2},{:.2})",
                self.scale_factor, x, y
            );
        }

        let mime = pick_mime(&mimes);
        self.update_acceptance(&offer, x, mime.as_deref());
        let accepted = self.mode.receive && x < self.window_width / 2.0 && mime.is_some();
        self.active_receive = Some(ActiveReceive {
            offer,
            mime,
            accepted,
        });
    }

    fn receive_motion(&mut self, device: &WlDataDevice, x: f64, y: f64) {
        let Some(offer) = self.drag_offer(device) else {
            return;
        };
        let mimes = offer.with_mime_types(<[String]>::to_vec);
        let mime = pick_mime(&mimes);
        self.update_acceptance(&offer, x, mime.as_deref());
        if let Some(active) = self.active_receive.as_mut() {
            active.offer = offer;
            active.mime = mime;
            active.accepted =
                self.mode.receive && x < self.window_width / 2.0 && active.mime.is_some();
        }

        if self.mode.receive
            && self
                .last_motion_log
                .is_none_or(|last| last.elapsed() >= Duration::from_millis(500))
        {
            println!("WLDND RECV motion x={x:.2} y={y:.2}");
            self.last_motion_log = Some(Instant::now());
        }
    }

    fn update_acceptance(&self, offer: &DragOffer, x: f64, mime: Option<&str>) {
        if self.mode.receive && x < self.window_width / 2.0 {
            offer.accept_mime_type(offer.serial, mime.map(ToOwned::to_owned));
            if mime.is_some() {
                offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
            }
        } else {
            offer.accept_mime_type(offer.serial, None);
            offer.set_actions(DndAction::empty(), DndAction::empty());
        }
    }

    fn receive_drop(&mut self, device: &WlDataDevice) {
        if !self.mode.receive {
            return;
        }
        let offer = self.drag_offer(device).or_else(|| {
            self.active_receive
                .as_ref()
                .map(|active| active.offer.clone())
        });
        let Some(offer) = offer else {
            eprintln!("WLDND ERR recv_drop=no_offer");
            return;
        };
        if self
            .active_receive
            .as_ref()
            .is_none_or(|active| !active.accepted)
        {
            offer.destroy();
            self.active_receive.take();
            return;
        }
        let mime = self
            .active_receive
            .as_ref()
            .and_then(|active| active.mime.clone())
            .or_else(|| offer.with_mime_types(pick_mime));
        let Some(mime) = mime else {
            eprintln!("WLDND ERR recv_drop=no_supported_mime");
            offer.destroy();
            self.active_receive.take();
            return;
        };
        match offer.receive(mime) {
            Ok(mut pipe) => {
                let tx = self.drop_tx.clone();
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let payload = pipe
                        .by_ref()
                        .take(1024 * 1024)
                        .read_to_end(&mut bytes)
                        .map(|_| String::from_utf8_lossy(&bytes).into_owned())
                        .map_err(|error| error.to_string());
                    let _ = tx.send(DropResult { offer, payload });
                });
            }
            Err(error) => {
                eprintln!("WLDND ERR recv_pipe={}", one_line(&error.to_string()));
                offer.destroy();
            }
        }
        self.active_receive.take();
    }

    fn receive_leave(&mut self, device: &WlDataDevice) {
        let postdrop = self.drag_offer(device).is_some_and(|offer| offer.dropped);
        if self.mode.receive {
            println!(
                "WLDND RECV leave {}",
                if postdrop { "postdrop" } else { "predrop" }
            );
        }
        if !postdrop {
            self.active_receive.take();
        }
    }

    fn drag_offer(&self, device: &WlDataDevice) -> Option<DragOffer> {
        self.seat_objects
            .iter()
            .find(|objects| objects.data_device.inner() == device)
            .and_then(|objects| objects.data_device.data().drag_offer())
    }

    fn note_source_event(&mut self, source: &WlDataSource) {
        if let Some(active) = self.active_drag.as_mut() {
            if active.source.inner() == source {
                active.saw_source_event = true;
            }
        }
    }

    fn finish_source(&mut self, source: &WlDataSource) {
        if self
            .active_drag
            .as_ref()
            .is_some_and(|active| active.source.inner() == source)
        {
            self.active_drag.take();
        }
    }

    fn spawn_source_write(&self, mime: String, mut pipe: WritePipe) {
        let tx = self.io_tx.clone();
        thread::spawn(move || {
            let body = match mime.as_str() {
                URI_MIME => "file:///tmp/wl-dnd-spike-payload.txt\r\n",
                TEXT_MIME => PAYLOAD_TEXT,
                SPIKE_MIME => "wl-dnd-spike\n",
                _ => "",
            };
            let result = pipe
                .write_all(body.as_bytes())
                .and_then(|()| pipe.flush())
                .map_err(|error| error.to_string());
            drop(pipe);
            let _ = tx.send(IoResult {
                context: format!("send mime={mime}"),
                result,
            });
        });
    }

    fn poll_threads(&mut self) {
        while let Ok(result) = self.drop_rx.try_recv() {
            match result.payload {
                Ok(payload) => {
                    println!("WLDND RECV drop payload={}", escape_payload(&payload));
                    result.offer.finish();
                    result.offer.destroy();
                }
                Err(error) => {
                    eprintln!("WLDND ERR recv_read={}", one_line(&error));
                    result.offer.destroy();
                }
            }
        }
        while let Ok(result) = self.io_rx.try_recv() {
            if let Err(error) = result.result {
                eprintln!(
                    "WLDND ERR {}={}",
                    one_line(&result.context),
                    one_line(&error)
                );
            }
        }
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![SeatState];
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.pending.push_back(Pending::SeatAdded(seat));
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        self.pending
            .push_back(Pending::CapabilityAdded(seat, capability));
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        self.pending
            .push_back(Pending::CapabilityRemoved(seat, capability));
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.pending.push_back(Pending::SeatRemoved(seat));
    }
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        self.pending
            .push_back(Pending::PointerFrame(pointer.clone(), events.to_vec()));
    }
}

impl DataDeviceHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        self.pending
            .push_back(Pending::ReceiveEnter(device.clone(), x, y, surface.clone()));
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        self.pending
            .push_back(Pending::ReceiveLeave(device.clone()));
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        self.pending
            .push_back(Pending::ReceiveMotion(device.clone(), x, y));
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.pending.push_back(Pending::Selection);
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        self.pending.push_back(Pending::ReceiveDrop(device.clone()));
    }
}

impl DataOfferHandler for WaylandState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: DndAction,
    ) {
        self.pending
            .push_back(Pending::ReceiveSourceActions(offer.clone(), actions));
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        action: DndAction,
    ) {
        self.pending
            .push_back(Pending::ReceiveSelectedAction(offer.clone(), action));
    }
}

impl DataSourceHandler for WaylandState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: Option<String>,
    ) {
        self.pending
            .push_back(Pending::SourceAccept(source.clone(), mime));
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: WritePipe,
    ) {
        self.pending
            .push_back(Pending::SourceSend(source.clone(), mime, pipe));
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.pending
            .push_back(Pending::SourceCancelled(source.clone()));
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.pending
            .push_back(Pending::SourceDropped(source.clone()));
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.pending
            .push_back(Pending::SourceFinished(source.clone()));
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        action: DndAction,
    ) {
        self.pending
            .push_back(Pending::SourceAction(source.clone(), action));
    }
}

delegate_registry!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_data_device!(WaylandState);

fn pick_mime(mimes: &[String]) -> Option<String> {
    [URI_MIME, TEXT_MIME]
        .into_iter()
        .find(|wanted| mimes.iter().any(|mime| mime == wanted))
        .map(ToOwned::to_owned)
}

fn escape_payload(payload: &str) -> String {
    payload
        .chars()
        .take(200)
        .flat_map(char::escape_default)
        .collect()
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}
