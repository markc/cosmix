//! Minimal shm client for the public wlr-screencopy compatibility contract.

use smithay::reexports::wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
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
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};

const GUARD_BYTES: usize = 64;
const DEADLINE: Duration = Duration::from_secs(10);

struct Probe {
    expected_output: Option<String>,
    observed_output: Option<String>,
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    discovery_error: Option<String>,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    backing: Option<File>,
    offset: usize,
    image_bytes: usize,
    width: u32,
    height: u32,
    stride: u32,
    ready: bool,
    failed: bool,
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let mut expected_output = None;
    while let Some(argument) = arguments.next() {
        if argument != "--output" {
            return Err(format!("unknown argument: {argument}"));
        }
        expected_output = Some(
            arguments
                .next()
                .ok_or_else(|| "--output requires a name".to_string())?,
        );
    }

    let connection = Connection::connect_to_env()
        .map_err(|error| format!("failed to connect to Wayland compositor: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut probe = Probe {
        expected_output,
        observed_output: None,
        shm: None,
        output: None,
        manager: None,
        discovery_error: None,
        pool: None,
        buffer: None,
        backing: None,
        offset: GUARD_BYTES,
        image_bytes: 0,
        width: 0,
        height: 0,
        stride: 0,
        ready: false,
        failed: false,
    };
    dispatch_until(
        &mut queue,
        &mut probe,
        Instant::now() + DEADLINE,
        |probe| {
            probe.discovery_error.is_some()
                || (probe.shm.is_some()
                    && probe.output.is_some()
                    && probe.manager.is_some()
                    && probe.observed_output.is_some())
        },
        "registry/output discovery",
    )?;
    if let Some(error) = probe.discovery_error.take() {
        return Err(error);
    }
    if let Some(expected) = &probe.expected_output
        && probe.observed_output.as_deref() != Some(expected)
    {
        return Err(format!(
            "requested output {expected:?}, first advertised output was {:?}",
            probe.observed_output
        ));
    }
    let manager = probe
        .manager
        .as_ref()
        .ok_or_else(|| "zwlr_screencopy_manager_v1 v3 unavailable".to_string())?;
    let output = probe
        .output
        .as_ref()
        .ok_or_else(|| "wl_output unavailable".to_string())?;
    let _frame = manager.capture_output(0, output, &qh, ());
    connection
        .flush()
        .map_err(|error| format!("failed to flush capture request: {error}"))?;
    dispatch_until(
        &mut queue,
        &mut probe,
        Instant::now() + DEADLINE,
        |probe| probe.ready || probe.failed,
        "screencopy",
    )?;
    if probe.failed {
        return Err("compositor reported screencopy failed".into());
    }
    if !probe.ready {
        return Err("screencopy did not become ready within 10 seconds".into());
    }
    let backing = probe
        .backing
        .as_ref()
        .expect("ready capture retains backing");
    let mut before = vec![0_u8; GUARD_BYTES];
    let mut pixels = vec![0_u8; probe.image_bytes];
    let mut after = vec![0_u8; GUARD_BYTES];
    backing
        .read_exact_at(&mut before, 0)
        .map_err(|error| error.to_string())?;
    backing
        .read_exact_at(&mut pixels, probe.offset as u64)
        .map_err(|error| error.to_string())?;
    backing
        .read_exact_at(&mut after, (probe.offset + probe.image_bytes) as u64)
        .map_err(|error| error.to_string())?;
    if !before.iter().chain(&after).all(|byte| *byte == 0xa5) {
        return Err("capture modified shm guard bytes".into());
    }
    if !pixels.chunks_exact(4).any(|pixel| pixel[..3] != [0, 0, 0]) {
        return Err("capture contains only black pixels".into());
    }
    println!(
        "COSMIX_SCREENCOPY_PROBE ready output={} size={}x{} stride={} offset={}",
        probe.observed_output.as_deref().unwrap_or("unknown"),
        probe.width,
        probe.height,
        probe.stride,
        probe.offset
    );
    Ok(())
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
        if complete(probe) {
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
    complete(probe)
        .then_some(())
        .ok_or_else(|| format!("{phase} did not complete within 10 seconds"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("COSMIX_SCREENCOPY_PROBE failed: {error}");
            ExitCode::FAILURE
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.observed_output = Some(name);
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
            "wl_shm" if state.shm.is_none() => {
                state.shm = Some(registry.bind(name, version.min(1), qh, ()));
            }
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "zwlr_screencopy_manager_v1" if state.manager.is_none() => {
                if version < 3 {
                    state.discovery_error = Some(format!(
                        "zwlr_screencopy_manager_v1 v3 unavailable (advertised v{version})"
                    ));
                } else {
                    state.manager = Some(registry.bind(name, 3, qh, ()));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for Probe {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let Ok(format) = format.into_result() else {
                    state.failed = true;
                    return;
                };
                if format != wl_shm::Format::Xrgb8888 || stride != width.saturating_mul(4) {
                    state.failed = true;
                    return;
                }
                let Some(image_bytes) = usize::try_from(stride)
                    .ok()
                    .and_then(|stride| stride.checked_mul(height as usize))
                else {
                    state.failed = true;
                    return;
                };
                let Some(total) = GUARD_BYTES
                    .checked_add(image_bytes)
                    .and_then(|length| length.checked_add(GUARD_BYTES))
                else {
                    state.failed = true;
                    return;
                };
                let name = CString::new("cosmix-screencopy-probe").unwrap();
                // SAFETY: name is valid and the successful descriptor is owned below.
                let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
                if raw < 0 {
                    state.failed = true;
                    return;
                }
                // SAFETY: memfd_create returned a new owned descriptor.
                let file = unsafe { File::from_raw_fd(raw) };
                if file.set_len(total as u64).is_err()
                    || file.write_all_at(&[0xa5; GUARD_BYTES], 0).is_err()
                    || file
                        .write_all_at(&[0xa5; GUARD_BYTES], (GUARD_BYTES + image_bytes) as u64)
                        .is_err()
                {
                    state.failed = true;
                    return;
                }
                let Some(shm) = state.shm.as_ref() else {
                    state.failed = true;
                    return;
                };
                let pool = shm.create_pool(file.as_fd(), total as i32, qh, ());
                let buffer = pool.create_buffer(
                    GUARD_BYTES as i32,
                    width as i32,
                    height as i32,
                    stride as i32,
                    format,
                    qh,
                    (),
                );
                frame.copy(&buffer);
                state.pool = Some(pool);
                state.buffer = Some(buffer);
                state.backing = Some(file);
                state.image_bytes = image_bytes;
                state.width = width;
                state.height = height;
                state.stride = stride;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(Probe: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Probe: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
