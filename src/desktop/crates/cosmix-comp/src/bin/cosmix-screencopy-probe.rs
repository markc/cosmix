//! Minimal SHM or DMA-BUF client for the public wlr-screencopy contract.

use smithay::backend::allocator::{
    Buffer as _, Fourcc, Modifier,
    dmabuf::{AsDmabuf, Dmabuf, DmabufMappingMode, DmabufSyncFlags},
    gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
};
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
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

#[derive(Clone, Debug)]
enum DestinationMode {
    Shm,
    Dmabuf { drm_node: String },
}

struct Probe {
    mode: DestinationMode,
    expected_output: Option<String>,
    observed_output: Option<String>,
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    linux_dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    dmabuf_modifiers: Vec<(u32, u64)>,
    advertised_dmabuf: Option<(u32, u32, u32)>,
    dmabuf: Option<Dmabuf>,
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
    ready_time: Option<(u32, u32, u32)>,
    failed: bool,
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let mut expected_output = None;
    let mut dmabuf = false;
    let mut drm_node = env::var("COSMIX_DRM_RENDER_NODE").ok();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                expected_output = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--output requires a name".to_string())?,
                );
            }
            "--dmabuf" => dmabuf = true,
            "--drm-node" => {
                drm_node = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--drm-node requires a path".to_string())?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let mode = if dmabuf {
        DestinationMode::Dmabuf {
            drm_node: drm_node.ok_or_else(|| {
                "--dmabuf requires --drm-node or COSMIX_DRM_RENDER_NODE".to_string()
            })?,
        }
    } else {
        DestinationMode::Shm
    };

    let connection = Connection::connect_to_env()
        .map_err(|error| format!("failed to connect to Wayland compositor: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut probe = Probe {
        mode,
        expected_output,
        observed_output: None,
        shm: None,
        output: None,
        manager: None,
        linux_dmabuf: None,
        dmabuf_modifiers: Vec::new(),
        advertised_dmabuf: None,
        dmabuf: None,
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
        ready_time: None,
        failed: false,
    };
    dispatch_until(
        &mut queue,
        &mut probe,
        Instant::now() + DEADLINE,
        |probe| {
            probe.discovery_error.is_some()
                || ((matches!(probe.mode, DestinationMode::Dmabuf { .. }) || probe.shm.is_some())
                    && probe.output.is_some()
                    && probe.manager.is_some()
                    && (!matches!(probe.mode, DestinationMode::Dmabuf { .. })
                        || probe.linux_dmabuf.is_some())
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
        if let Some(error) = probe.discovery_error.take() {
            return Err(error);
        }
        return Err("compositor reported screencopy failed".into());
    }
    if !probe.ready {
        return Err("screencopy did not become ready within 10 seconds".into());
    }
    if let DestinationMode::Dmabuf { .. } = &probe.mode {
        return verify_dmabuf_capture(&probe);
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

fn allocate_dmabuf_destination(
    state: &mut Probe,
    frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    qh: &QueueHandle<Probe>,
) -> Result<(), String> {
    let DestinationMode::Dmabuf { drm_node } = &state.mode else {
        return Err("DMA-BUF allocation requested in SHM mode".into());
    };
    let (format, width, height) = state
        .advertised_dmabuf
        .ok_or_else(|| "buffer_done arrived without a linux_dmabuf advertisement".to_string())?;
    let fourcc = Fourcc::try_from(format)
        .map_err(|_| format!("unsupported advertised DMA-BUF fourcc {format:#010x}"))?;
    let mut modifiers = state
        .dmabuf_modifiers
        .iter()
        .filter_map(|(candidate, modifier)| (*candidate == format).then_some(*modifier))
        .collect::<Vec<_>>();
    modifiers.sort_unstable();
    modifiers.dedup();
    let linear = u64::from(Modifier::Linear);
    if modifiers.binary_search(&linear).is_err() {
        return Err(format!(
            "linux-dmabuf advertised no CPU-readable linear modifier for screencopy fourcc {format:#010x}"
        ));
    }
    let file = File::options()
        .read(true)
        .write(true)
        .open(drm_node)
        .map_err(|error| format!("failed to open DRM render node {drm_node}: {error}"))?;
    let gbm = GbmDevice::new(file)
        .map_err(|error| format!("failed to create GBM device for {drm_node}: {error}"))?;
    let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let modifiers = [Modifier::Linear];
    let buffer = allocator
        .create_buffer_with_flags(width, height, fourcc, &modifiers, GbmBufferFlags::RENDERING)
        .map_err(|error| format!("GBM screencopy destination allocation failed: {error}"))?;
    let dmabuf = buffer
        .export()
        .map_err(|error| format!("GBM screencopy destination export failed: {error}"))?;
    if dmabuf.format().modifier != Modifier::Linear {
        return Err(format!(
            "GBM returned {:?}, but the probe requires an explicit linear modifier",
            dmabuf.format().modifier
        ));
    }
    if dmabuf.num_planes() != 1 {
        return Err(format!(
            "screencopy destination allocation produced {} planes; exactly one is required",
            dmabuf.num_planes()
        ));
    }
    let linux_dmabuf = state
        .linux_dmabuf
        .as_ref()
        .ok_or_else(|| "zwp_linux_dmabuf_v1 unavailable".to_string())?;
    let params = linux_dmabuf.create_params(qh, ());
    let modifier = u64::from(dmabuf.format().modifier);
    for (plane, ((fd, offset), stride)) in dmabuf
        .handles()
        .zip(dmabuf.offsets())
        .zip(dmabuf.strides())
        .enumerate()
    {
        params.add(
            fd,
            plane as u32,
            offset,
            stride,
            (modifier >> 32) as u32,
            modifier as u32,
        );
    }
    let wl_buffer = params.create_immed(
        width as i32,
        height as i32,
        format,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    frame.copy(&wl_buffer);
    state.width = width;
    state.height = height;
    state.stride = dmabuf.strides().next().unwrap_or(width.saturating_mul(4));
    state.image_bytes = usize::try_from(state.stride)
        .ok()
        .and_then(|stride| stride.checked_mul(height as usize))
        .ok_or_else(|| "DMA-BUF image size overflow".to_string())?;
    state.buffer = Some(wl_buffer);
    state.dmabuf = Some(dmabuf);
    Ok(())
}

fn verify_dmabuf_capture(probe: &Probe) -> Result<(), String> {
    let dmabuf = probe
        .dmabuf
        .as_ref()
        .ok_or_else(|| "ready DMA-BUF capture lost its allocation".to_string())?;
    dmabuf
        .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
        .map_err(|error| format!("starting DMA-BUF CPU read failed: {error}"))?;
    let mapping = dmabuf
        .map_plane(0, DmabufMappingMode::READ)
        .map_err(|error| format!("mapping DMA-BUF capture failed: {error}"))?;
    let required = probe
        .stride
        .checked_mul(probe.height)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "DMA-BUF mapped extent overflow".to_string())?;
    if mapping.length() < required {
        return Err(format!(
            "DMA-BUF mapping is too short: {} < {required}",
            mapping.length()
        ));
    }
    // SAFETY: `mapping` owns a readable mapping of `mapping.length()` bytes
    // until the slice is no longer used below.
    let bytes = unsafe { std::slice::from_raw_parts(mapping.ptr().cast::<u8>(), required) };
    let row_bytes = usize::try_from(probe.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| "DMA-BUF visible row size overflow".to_string())?;
    let stride = probe.stride as usize;
    if stride < row_bytes {
        return Err(format!(
            "DMA-BUF stride is shorter than a visible row: {stride} < {row_bytes}"
        ));
    }
    let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
    let mut non_black = false;
    for row in bytes.chunks(stride).take(probe.height as usize) {
        for pixel in row[..row_bytes].chunks_exact(4) {
            non_black |= pixel[..3] != [0, 0, 0];
            for byte in pixel {
                checksum = checksum.wrapping_mul(0x100_0000_01b3) ^ u64::from(*byte);
            }
        }
    }
    drop(mapping);
    dmabuf
        .sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ)
        .map_err(|error| format!("ending DMA-BUF CPU read failed: {error}"))?;
    if !non_black {
        return Err("DMA-BUF capture contains only zero bytes".into());
    }
    let ready = probe.ready_time.unwrap_or_default();
    println!(
        "COSMIX_SCREENCOPY_DMABUF_PROBE ready output={} size={}x{} stride={} ready={}:{}.{:09} checksum={checksum:016x}",
        probe.observed_output.as_deref().unwrap_or("unknown"),
        probe.width,
        probe.height,
        probe.stride,
        ready.0,
        ready.1,
        ready.2,
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
            "zwp_linux_dmabuf_v1"
                if state.linux_dmabuf.is_none()
                    && matches!(state.mode, DestinationMode::Dmabuf { .. }) =>
            {
                if version < 3 {
                    state.discovery_error = Some(format!(
                        "zwp_linux_dmabuf_v1 v3 unavailable (advertised v{version})"
                    ));
                } else {
                    state.linux_dmabuf = Some(registry.bind(name, 3, qh, ()));
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
                if matches!(state.mode, DestinationMode::Dmabuf { .. }) {
                    return;
                }
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
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.advertised_dmabuf = Some((format, width, height));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if matches!(state.mode, DestinationMode::Dmabuf { .. })
                    && let Err(error) = allocate_dmabuf_destination(state, frame, qh)
                {
                    state.discovery_error = Some(error);
                    state.failed = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                state.ready_time = Some((tv_sec_hi, tv_sec_lo, tv_nsec));
                state.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state.dmabuf_modifiers.push((
                format,
                (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
            ));
        }
    }
}

wayland_client::delegate_noop!(Probe: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(Probe: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(Probe: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
