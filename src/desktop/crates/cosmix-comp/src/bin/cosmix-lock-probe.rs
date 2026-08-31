//! Deterministic ext-session-lock-v1 client for the private VT harness.

use signal_hook::{consts::signal::SIGUSR1, flag};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{EventLoop, LoopHandle},
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, raw::RawPool},
};
use std::{
    env, fmt,
    io::{self, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_shm, wl_surface},
};

const DEFAULT_HOLD: Duration = Duration::from_secs(300);
const MAX_HOLD_SECONDS: u64 = 3_600;
const DISPATCH_SLICE: Duration = Duration::from_millis(16);

fn checkpoint(detail: fmt::Arguments<'_>) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "COSMIX_LOCK_PROBE checkpoint={detail}").expect("write lock-probe checkpoint");
    stdout.flush().expect("flush lock-probe checkpoint");
}

struct Probe {
    _loop_handle: LoopHandle<'static, Self>,
    compositor: CompositorState,
    outputs: OutputState,
    registry: RegistryState,
    shm: Shm,
    lock_state: SessionLockState,
    lock: Option<SessionLock>,
    lock_surfaces: Vec<(wayland_client::backend::ObjectId, SessionLockSurface)>,
    locked: bool,
    finished: bool,
    failed: bool,
}

impl Probe {
    fn ensure_lock_surface(&mut self, output: &wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self.lock_surfaces.iter().any(|(id, _)| *id == output.id()) {
            return;
        }
        let Some(lock) = self.lock.clone() else {
            return;
        };
        let surface = self.compositor.create_surface(qh);
        let lock_surface = lock.create_lock_surface(surface, output, qh);
        self.lock_surfaces.push((output.id(), lock_surface));
        checkpoint(format_args!(
            "surface_created outputs={}",
            self.lock_surfaces.len()
        ));
    }
}

fn parse_hold() -> Result<Duration, String> {
    let mut arguments = env::args().skip(1);
    let mut hold = DEFAULT_HOLD;
    while let Some(argument) = arguments.next() {
        if argument != "--hold" {
            return Err(format!("unknown argument: {argument}"));
        }
        let raw = arguments
            .next()
            .ok_or_else(|| "--hold requires seconds".to_string())?;
        let seconds = raw
            .parse::<u64>()
            .map_err(|_| format!("invalid --hold seconds: {raw}"))?;
        if !(5..=MAX_HOLD_SECONDS).contains(&seconds) {
            return Err(format!("--hold seconds must be in 5..={MAX_HOLD_SECONDS}"));
        }
        hold = Duration::from_secs(seconds);
    }
    Ok(hold)
}

fn run() -> Result<(), String> {
    let hold = parse_hold()?;
    let unlock_requested = Arc::new(AtomicBool::new(false));
    flag::register(SIGUSR1, Arc::clone(&unlock_requested))
        .map_err(|error| format!("failed to register SIGUSR1: {error}"))?;

    let connection = Connection::connect_to_env()
        .map_err(|error| format!("failed to connect to Wayland compositor: {error}"))?;
    let (globals, mut event_queue) = registry_queue_init(&connection)
        .map_err(|error| format!("failed to initialise Wayland registry: {error}"))?;
    let qh: QueueHandle<Probe> = event_queue.handle();
    let mut event_loop: EventLoop<Probe> =
        EventLoop::try_new().map_err(|error| format!("failed to create event loop: {error}"))?;
    let mut probe = Probe {
        _loop_handle: event_loop.handle(),
        compositor: CompositorState::bind(&globals, &qh)
            .map_err(|error| format!("wl_compositor unavailable: {error}"))?,
        outputs: OutputState::new(&globals, &qh),
        registry: RegistryState::new(&globals),
        shm: Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm unavailable: {error}"))?,
        lock_state: SessionLockState::new(&globals, &qh),
        lock: None,
        lock_surfaces: Vec::new(),
        locked: false,
        finished: false,
        failed: false,
    };
    event_queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("initial Wayland roundtrip failed: {error}"))?;
    probe.lock = Some(
        probe
            .lock_state
            .lock(&qh)
            .map_err(|error| format!("ext-session-lock-v1 unavailable: {error}"))?,
    );
    for output in probe.outputs.outputs().collect::<Vec<_>>() {
        probe.ensure_lock_surface(&output, &qh);
    }
    if probe.lock_surfaces.is_empty() {
        return Err("compositor advertised no wl_output for the lock".into());
    }
    checkpoint(format_args!(
        "lock_requested outputs={} hold_seconds={}",
        probe.lock_surfaces.len(),
        hold.as_secs()
    ));

    WaylandSource::new(connection.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|error| format!("failed to attach Wayland source: {error}"))?;
    let unlock_deadline = Instant::now() + hold;
    loop {
        event_loop
            .dispatch(DISPATCH_SLICE, &mut probe)
            .map_err(|error| format!("Wayland dispatch failed: {error}"))?;
        if probe.failed || probe.finished {
            return Err("compositor rejected or ended the session lock".into());
        }
        if unlock_requested.load(Ordering::SeqCst) || Instant::now() >= unlock_deadline {
            if !probe.locked {
                return Err("unlock requested before the compositor confirmed locked".into());
            }
            checkpoint(format_args!("unlock_requested"));
            probe
                .lock
                .as_ref()
                .expect("accepted lock remains present")
                .unlock();
            connection
                .flush()
                .map_err(|error| format!("failed to flush unlock request: {error}"))?;
            checkpoint(format_args!("unlock_sent"));
            return Ok(());
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("COSMIX_LOCK_PROBE checkpoint=failed reason={error}");
            ExitCode::from(1)
        }
    }
}

impl SessionLockHandler for Probe {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.locked = true;
        checkpoint(format_args!("locked outputs={}", self.lock_surfaces.len()));
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.finished = true;
        eprintln!("COSMIX_LOCK_PROBE checkpoint=finished");
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let (width, height) = configure.new_size;
        let byte_count = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4));
        let Some(byte_count) = byte_count else {
            self.failed = true;
            return;
        };
        let Ok(mut pool) = RawPool::new(byte_count, &self.shm) else {
            self.failed = true;
            return;
        };
        for pixel in pool.mmap().chunks_exact_mut(4) {
            pixel.copy_from_slice(&0xff_18_24_38_u32.to_le_bytes());
        }
        let Some(stride) = i32::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
        else {
            self.failed = true;
            return;
        };
        let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
            self.failed = true;
            return;
        };
        let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, (), qh);
        surface.wl_surface().attach(Some(&buffer), 0, 0);
        surface.wl_surface().commit();
        buffer.destroy();
        checkpoint(format_args!(
            "surface_configured width={width} height={height}"
        ));
    }
}

impl CompositorHandler for Probe {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Probe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ensure_lock_surface(&output, qh);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.lock_surfaces.retain(|(id, _)| *id != output.id());
        checkpoint(format_args!(
            "output_removed outputs={}",
            self.lock_surfaces.len()
        ));
    }
}

impl ProvidesRegistryState for Probe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState];
}

impl ShmHandler for Probe {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

smithay_client_toolkit::delegate_compositor!(Probe);
smithay_client_toolkit::delegate_output!(Probe);
smithay_client_toolkit::delegate_registry!(Probe);
smithay_client_toolkit::delegate_session_lock!(Probe);
smithay_client_toolkit::delegate_shm!(Probe);
wayland_client::delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
