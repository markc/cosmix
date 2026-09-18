//! Minimal SHM toplevel that echoes the seat input it receives.
//!
//! Gate client for the `comp.input.*` and `comp.window.*` verbs. Maps one
//! xdg toplevel (title, app id and size from the arguments), follows the
//! compositor's configure size, redraws on every frame callback with
//! presentation feedback, and prints one `PROBE ...` line per event:
//!
//! ```text
//! PROBE configure <w> <h>          PROBE enter <x> <y>
//! PROBE motion <x> <y>             PROBE button <code> <0|1> <x> <y>
//! PROBE key <evdev> <0|1>          PROBE keyboard_enter / keyboard_leave
//! PROBE presented <count>          PROBE close
//! ```
//!
//! Runs until `--seconds` elapse or the compositor closes it. With
//! `--hide-on-close` a close request unmaps the window (a null buffer)
//! instead, like a tray app, and prints `PROBE hidden`; with
//! `--remap-once-ms N` the first such hide is undone after N ms
//! (`PROBE remapped`).

use smithay::reexports::wayland_protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback,
};
use smithay::reexports::wayland_protocols::xdg::shell::client::{
    xdg_surface, xdg_toplevel, xdg_wm_base,
};
use std::{
    env,
    ffi::CString,
    fs::File,
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    process::ExitCode,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat,
        wl_shm, wl_shm_pool, wl_surface,
    },
};

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    presentation: Option<wp_presentation::WpPresentation>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The latest unacknowledged configure: `(serial, width, height)`.
    pending_configure: Option<(u32, i32, i32)>,
    toplevel_size: (i32, i32),
    frame_done: bool,
    presented: u64,
    pointer_at: (f64, f64),
    closed: bool,
}

fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "PROBE {line}");
    let _ = stdout.flush();
}

struct Options {
    title: String,
    app_id: String,
    width: i32,
    height: i32,
    duration: Duration,
    hide_on_close: bool,
    remap_once: Option<Duration>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        title: "cosmix-input-probe".into(),
        app_id: "dev.cosmix.InputProbe".into(),
        width: 320,
        height: 240,
        duration: Duration::from_secs(30),
        hide_on_close: false,
        remap_once: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--title" => options.title = value()?,
            "--app-id" => options.app_id = value()?,
            "--size" => {
                let size = value()?;
                let (width, height) = size
                    .split_once('x')
                    .ok_or_else(|| "--size expects WxH".to_string())?;
                options.width = width.parse().map_err(|error| format!("--size: {error}"))?;
                options.height = height.parse().map_err(|error| format!("--size: {error}"))?;
            }
            "--seconds" => {
                options.duration = Duration::from_secs(
                    value()?
                        .parse()
                        .map_err(|error| format!("--seconds: {error}"))?,
                );
            }
            "--hide-on-close" => options.hide_on_close = true,
            "--remap-once-ms" => {
                options.remap_once = Some(Duration::from_millis(
                    value()?
                        .parse()
                        .map_err(|error| format!("--remap-once-ms: {error}"))?,
                ));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.width <= 0 || options.height <= 0 {
        return Err("size must be positive".into());
    }
    Ok(options)
}

/// One shared-memory buffer of a given size.
struct Canvas {
    backing: File,
    buffer: wl_buffer::WlBuffer,
    width: i32,
    height: i32,
}

fn canvas(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<Probe>,
    width: i32,
    height: i32,
) -> Result<Canvas, String> {
    let bytes = (width * height * 4) as usize;
    let name = CString::new("cosmix-input-probe").unwrap();
    // SAFETY: name is valid and the successful descriptor is owned below.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(format!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: memfd_create returned a new owned descriptor.
    let backing = unsafe { File::from_raw_fd(raw) };
    backing
        .set_len(bytes as u64)
        .map_err(|error| error.to_string())?;
    let pool = shm.create_pool(backing.as_fd(), bytes as i32, qh, ());
    let buffer = pool.create_buffer(0, width, height, width * 4, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    Ok(Canvas {
        backing,
        buffer,
        width,
        height,
    })
}

fn wait_readable(queue: &mut EventQueue<Probe>, timeout: Duration) -> Result<(), String> {
    queue
        .flush()
        .map_err(|error| format!("flush failed: {error}"))?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let mut descriptor = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
    // SAFETY: descriptor is valid writable storage for one pollfd and the
    // guard keeps the connection fd alive during poll.
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready <= 0 {
        return Ok(());
    }
    if descriptor.revents & libc::POLLIN != 0 {
        guard
            .read()
            .map_err(|error| format!("read failed: {error}"))?;
        Ok(())
    } else if descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
        Err("compositor disconnected".into())
    } else {
        Ok(())
    }
}

fn run() -> Result<(), String> {
    let options = options()?;
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("failed to connect to Wayland: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("registry roundtrip failed: {error}"))?;
    let compositor = probe
        .compositor
        .clone()
        .ok_or("wl_compositor unavailable")?;
    let shm = probe.shm.clone().ok_or("wl_shm unavailable")?;
    let wm_base = probe.wm_base.clone().ok_or("xdg_wm_base unavailable")?;
    probe.seat.as_ref().ok_or("wl_seat unavailable")?;
    queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("seat roundtrip failed: {error}"))?;

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(options.title.clone());
    toplevel.set_app_id(options.app_id.clone());
    surface.commit();

    let deadline = Instant::now() + options.duration;
    let mut current: Option<Canvas> = None;
    let mut frame = 0_u32;
    probe.frame_done = true;
    let mut hidden = false;
    // After a remap nothing may be attached until the new configure is
    // acknowledged.
    let mut awaiting_configure = false;
    let mut remap_at: Option<Instant> = None;
    let mut remap_left = options.remap_once;
    while Instant::now() < deadline && (!probe.closed || options.hide_on_close) {
        if probe.closed && !hidden {
            probe.closed = false;
            hidden = true;
            surface.attach(None, 0, 0);
            surface.commit();
            say("hidden");
            if let Some(after) = remap_left.take() {
                remap_at = Some(Instant::now() + after);
            }
        }
        if hidden && remap_at.is_some_and(|at| Instant::now() >= at) {
            remap_at = None;
            hidden = false;
            awaiting_configure = true;
            // A fresh initial commit; the configure it earns re-attaches.
            surface.commit();
            say("remapped");
        }
        if hidden {
            probe.pending_configure = None;
            wait_readable(&mut queue, Duration::from_millis(20))?;
            queue
                .dispatch_pending(&mut probe)
                .map_err(|error| format!("dispatch failed: {error}"))?;
            continue;
        }
        queue
            .dispatch_pending(&mut probe)
            .map_err(|error| format!("dispatch failed: {error}"))?;
        let mut dirty = false;
        if let Some((serial, width, height)) = probe.pending_configure.take() {
            xdg.ack_configure(serial);
            let width = if width > 0 { width } else { options.width };
            let height = if height > 0 { height } else { options.height };
            if current
                .as_ref()
                .is_none_or(|canvas| (canvas.width, canvas.height) != (width, height))
            {
                current = Some(canvas(&shm, &qh, width, height)?);
                say(&format!("configure {width} {height}"));
            }
            dirty = true;
            awaiting_configure = false;
        }
        if let Some(canvas) = current.as_ref()
            && (dirty || (probe.frame_done && !awaiting_configure))
        {
            frame = frame.wrapping_add(1);
            let shade = (frame % 256) as u8;
            let pixels = [shade, 0x80, 255 - shade, 0xff].repeat((canvas.width * canvas.height) as usize);
            canvas
                .backing
                .write_all_at(&pixels, 0)
                .map_err(|error| error.to_string())?;
            surface.attach(Some(&canvas.buffer), 0, 0);
            surface.damage_buffer(0, 0, canvas.width, canvas.height);
            surface.frame(&qh, ());
            if let Some(presentation) = &probe.presentation {
                presentation.feedback(&surface, &qh, ());
            }
            surface.commit();
            probe.frame_done = false;
        }
        wait_readable(&mut queue, Duration::from_millis(50))?;
    }
    if probe.closed {
        say("close");
    }
    say(&format!("presented {}", probe.presented));
    say("exit");
    toplevel.destroy();
    let _ = queue.roundtrip(&mut probe);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cosmix-input-probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
            "wp_presentation" => {
                state.presentation = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Probe {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
        if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_at = (surface_x, surface_y);
                say(&format!("enter {surface_x} {surface_y}"));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_at = (surface_x, surface_y);
                say(&format!("motion {surface_x} {surface_y}"));
            }
            wl_pointer::Event::Leave { .. } => say("leave"),
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                let pressed = matches!(
                    button_state,
                    WEnum::Value(wl_pointer::ButtonState::Pressed)
                );
                say(&format!(
                    "button {button} {} {} {}",
                    u8::from(pressed),
                    state.pointer_at.0,
                    state.pointer_at.1
                ));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => say("keyboard_enter"),
            wl_keyboard::Event::Leave { .. } => say("keyboard_leave"),
            wl_keyboard::Event::Key {
                key, state: pressed, ..
            } => {
                let pressed = matches!(pressed, WEnum::Value(wl_keyboard::KeyState::Pressed));
                say(&format!("key {key} {}", u8::from(pressed)));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.pending_configure = Some((serial, state.toplevel_size.0, state.toplevel_size.1));
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.toplevel_size = (width, height);
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation_feedback::Event::Presented { .. } = event {
            state.presented += 1;
            if state.presented == 1 {
                say("presented 1");
            }
        }
    }
}

macro_rules! ignore_events {
    ($($interface:ty),+ $(,)?) => {
        $(
            impl Dispatch<$interface, ()> for Probe {
                fn event(
                    _: &mut Self,
                    _: &$interface,
                    _: <$interface as wayland_client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )+
    };
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wp_presentation::WpPresentation,
);
