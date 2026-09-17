//! Minimal SHM client for the public `wp_presentation` contract.
//!
//! Maps one xdg toplevel, commits `--frames` buffers paced by frame
//! callbacks, asks for presentation feedback on every commit, and checks
//! what comes back. Prints one `COSMIX_PRESENTATION_PROBE` summary line and
//! exits non-zero when an assertion fails.

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
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    process::ExitCode,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Presented {
        time: Duration,
        refresh_ns: u32,
        seq: u64,
        flags: u32,
    },
    Discarded,
}

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    presentation: Option<wp_presentation::WpPresentation>,
    clock_id: Option<u32>,
    configured: Option<u32>,
    frame_done: bool,
    outcomes: Vec<Option<Outcome>>,
    closed: bool,
}

fn monotonic_now() -> Duration {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is valid writable storage for one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) };
    Duration::new(timestamp.tv_sec as u64, timestamp.tv_nsec as u32)
}

fn dispatch_until(
    queue: &mut EventQueue<Probe>,
    probe: &mut Probe,
    deadline: Instant,
    complete: impl Fn(&Probe) -> bool,
    phase: &str,
) -> Result<(), String> {
    while !complete(probe) {
        queue
            .dispatch_pending(probe)
            .map_err(|error| format!("{phase} dispatch failed: {error}"))?;
        if complete(probe) || probe.closed {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        queue
            .flush()
            .map_err(|error| format!("failed to flush {phase} request: {error}"))?;
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: read_guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: descriptor is valid writable storage for one pollfd and the
        // read guard keeps the Wayland connection fd alive during poll.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == 0 {
            drop(read_guard);
            continue;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(read_guard);
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("polling Wayland fd during {phase} failed: {error}"));
        }
        if descriptor.revents & libc::POLLIN != 0 {
            read_guard.read().map_err(|error| {
                format!("reading Wayland events during {phase} failed: {error}")
            })?;
        } else {
            drop(read_guard);
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err(format!("Wayland compositor disconnected during {phase}"));
            }
        }
    }
    if probe.closed {
        return Err(format!("toplevel closed during {phase}"));
    }
    complete(probe)
        .then_some(())
        .ok_or_else(|| format!("{phase} did not complete in time"))
}

struct Options {
    frames: usize,
    width: u32,
    height: u32,
    timeout: Duration,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        frames: 300,
        width: 256,
        height: 256,
        timeout: Duration::from_secs(60),
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--frames" => {
                options.frames = value()?
                    .parse()
                    .map_err(|error| format!("--frames: {error}"))?;
            }
            "--size" => {
                let size = value()?;
                let (width, height) = size
                    .split_once('x')
                    .ok_or_else(|| "--size expects WxH".to_string())?;
                options.width = width.parse().map_err(|error| format!("--size: {error}"))?;
                options.height = height.parse().map_err(|error| format!("--size: {error}"))?;
            }
            "--timeout-s" => {
                options.timeout = Duration::from_secs(
                    value()?
                        .parse()
                        .map_err(|error| format!("--timeout-s: {error}"))?,
                );
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.frames == 0 || options.width == 0 || options.height == 0 {
        return Err("frames and size must be non-zero".into());
    }
    Ok(options)
}

fn run() -> Result<bool, String> {
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
    let presentation = probe
        .presentation
        .clone()
        .ok_or("wp_presentation is not advertised")?;
    queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("clock roundtrip failed: {error}"))?;

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("cosmix-presentation-probe".into());
    toplevel.set_app_id("dev.cosmix.PresentationProbe".into());
    surface.commit();
    dispatch_until(
        &mut queue,
        &mut probe,
        Instant::now() + Duration::from_secs(10),
        |probe| probe.configured.is_some(),
        "initial configure",
    )?;
    xdg.ack_configure(probe.configured.take().unwrap_or_default());

    let (width, height) = (options.width, options.height);
    let stride = width * 4;
    let buffer_bytes = (stride * height) as usize;
    let name = CString::new("cosmix-presentation-probe").unwrap();
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
        .set_len((buffer_bytes * 2) as u64)
        .map_err(|error| error.to_string())?;
    let pool = shm.create_pool(backing.as_fd(), (buffer_bytes * 2) as i32, &qh, ());
    let buffers = [0, 1].map(|index| {
        pool.create_buffer(
            (index * buffer_bytes) as i32,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        )
    });

    probe.outcomes = vec![None; options.frames];
    let deadline = Instant::now() + options.timeout;
    let started = monotonic_now();
    let mut pixels = vec![0_u8; buffer_bytes];
    for frame in 0..options.frames {
        let slot = frame % 2;
        let shade = (frame % 256) as u8;
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[shade, 255 - shade, 0x40, 0xff]);
        }
        backing
            .write_all_at(&pixels, (slot * buffer_bytes) as u64)
            .map_err(|error| error.to_string())?;
        surface.attach(Some(&buffers[slot]), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(&qh, ());
        presentation.feedback(&surface, &qh, frame);
        probe.frame_done = false;
        surface.commit();
        dispatch_until(
            &mut queue,
            &mut probe,
            deadline,
            |probe| probe.frame_done,
            "frame callback",
        )?;
    }
    let committed = monotonic_now();
    dispatch_until(
        &mut queue,
        &mut probe,
        deadline,
        |probe| probe.outcomes.iter().all(Option::is_some),
        "feedback resolution",
    )?;
    let finished = monotonic_now();

    let outcomes = probe.outcomes.iter().flatten().copied().collect::<Vec<_>>();
    let presented = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Outcome::Presented {
                time,
                refresh_ns,
                seq,
                flags,
            } => Some((*time, *refresh_ns, *seq, *flags)),
            Outcome::Discarded => None,
        })
        .collect::<Vec<_>>();
    let discarded = outcomes.len() - presented.len();
    let increasing = presented.windows(2).all(|pair| pair[1].0 > pair[0].0);
    let in_window = presented
        .iter()
        .all(|(time, ..)| *time >= started && *time <= finished);
    let flags_zero = presented.iter().all(|(.., flags)| *flags == 0);
    let seq_zero = presented.iter().all(|(_, _, seq, _)| *seq == 0);
    let refresh_zero = presented.iter().all(|(_, refresh, ..)| *refresh == 0);
    let intervals = presented
        .windows(2)
        .map(|pair| pair[1].0.saturating_sub(pair[0].0).as_micros() as u64)
        .collect::<Vec<_>>();
    let mut sorted = intervals.clone();
    sorted.sort_unstable();
    let percentile = |p: usize| {
        if sorted.is_empty() {
            0
        } else {
            sorted[(sorted.len() - 1) * p / 100]
        }
    };
    let min_presented = options.frames * 9 / 10;
    let clock_ok = probe.clock_id == Some(libc::CLOCK_MONOTONIC as u32);
    let pass = outcomes.len() == options.frames
        && presented.len() >= min_presented
        && increasing
        && in_window
        && flags_zero
        && seq_zero
        && refresh_zero
        && clock_ok;
    println!(
        "COSMIX_PRESENTATION_PROBE {} frames={} presented={} discarded={} min_presented={} \
         clock_id={} tv_first_us={} tv_last_us={} window_start_us={} commits_done_us={} \
         window_end_us={} increasing={} in_window={} flags_zero={} seq_zero={} \
         refresh_zero={} interval_p50_us={} interval_p99_us={} interval_max_us={}",
        if pass { "PASS" } else { "FAIL" },
        options.frames,
        presented.len(),
        discarded,
        min_presented,
        probe
            .clock_id
            .map_or_else(|| "none".to_string(), |clock| clock.to_string()),
        presented.first().map_or(0, |entry| entry.0.as_micros()),
        presented.last().map_or(0, |entry| entry.0.as_micros()),
        started.as_micros(),
        committed.as_micros(),
        finished.as_micros(),
        increasing,
        in_window,
        flags_zero,
        seq_zero,
        refresh_zero,
        percentile(50),
        percentile(99),
        sorted.last().copied().unwrap_or(0),
    );
    toplevel.destroy();
    Ok(pass)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("COSMIX_PRESENTATION_PROBE failed: {error}");
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
            "wp_presentation" => {
                state.presentation = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, usize> for Probe {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        frame: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let outcome = match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                flags,
            } => Outcome::Presented {
                time: Duration::new((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec),
                refresh_ns: refresh,
                seq: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                flags: match flags {
                    wayland_client::WEnum::Value(flags) => flags.bits(),
                    wayland_client::WEnum::Unknown(bits) => bits,
                },
            },
            wp_presentation_feedback::Event::Discarded => Outcome::Discarded,
            _ => return,
        };
        if let Some(slot) = state.outcomes.get_mut(*frame) {
            if slot.is_some() {
                eprintln!("COSMIX_PRESENTATION_PROBE feedback {frame} resolved twice");
                state.closed = true;
            }
            *slot = Some(outcome);
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
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            if state.configured.is_none() && !state.outcomes.is_empty() {
                // Later configures (focus, size hints) are acked immediately;
                // the probe keeps its own buffer size.
                xdg.ack_configure(serial);
            } else {
                state.configured = Some(serial);
            }
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
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
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
);
