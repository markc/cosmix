//! Native Wayland screenshot capture using ext-image-copy-capture-v1.
//!
//! Captures the full compositor output (all windows, overlays, layers) as a PNG.
//! No external tools needed — pure Wayland protocol.

use std::os::unix::io::AsFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
};

/// Screenshot state machine.
#[derive(Debug)]
struct ScreenshotState {
    // Globals
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,

    // Capture state
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    constraints_done: bool,
    frame_ready: bool,
    frame_failed: bool,

    // Buffer
    shm_fd: Option<std::fs::File>,
}

impl Default for ScreenshotState {
    fn default() -> Self {
        Self {
            shm: None,
            output: None,
            capture_manager: None,
            source_manager: None,
            width: 0,
            height: 0,
            shm_format: None,
            constraints_done: false,
            frame_ready: false,
            frame_failed: false,
            shm_fd: None,
        }
    }
}

/// Capture a full-screen screenshot and save as PNG.
///
/// Returns the path to the saved file.
pub fn capture_screenshot(save_path: &Path) -> Result<PathBuf> {
    let conn = Connection::connect_to_env()
        .context("Failed to connect to Wayland compositor")?;
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();

    let mut state = ScreenshotState::default();

    // Roundtrip 1: bind globals
    display.get_registry(&qh, ());
    eq.roundtrip(&mut state)?;

    // Clone protocol objects before further roundtrips (avoids borrow conflicts)
    let shm = state.shm.clone().context("Compositor has no wl_shm")?;
    let output = state.output.clone().context("No wl_output found")?;
    let capture_mgr = state.capture_manager.clone()
        .context("Compositor doesn't support ext_image_copy_capture_manager_v1")?;
    let source_mgr = state.source_manager.clone()
        .context("Compositor doesn't support ext_output_image_capture_source_manager_v1")?;

    // Create capture source from the output
    let source = source_mgr.create_source(&output, &qh, ());

    // Create session (with cursors painted in)
    let session = capture_mgr.create_session(
        &source,
        Options::PaintCursors,
        &qh,
        (),
    );

    // Roundtrip 2: receive buffer constraints (shm_format, buffer_size, done)
    eq.roundtrip(&mut state)?;

    // May need additional roundtrips for constraints
    while !state.constraints_done {
        eq.roundtrip(&mut state)?;
    }

    let width = state.width;
    let height = state.height;
    let format = state.shm_format.context("No SHM format advertised by compositor")?;

    anyhow::ensure!(width > 0 && height > 0, "Invalid buffer size: {width}x{height}");

    let stride = width * 4; // 4 bytes per pixel for xRGB/ARGB
    let size = (stride * height) as usize;

    // Create SHM buffer via memfd
    let fd = create_memfd(size)?;
    state.shm_fd = Some(fd);
    let fd_ref = state.shm_fd.as_ref().unwrap();

    let pool = shm.create_pool(fd_ref.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,              // offset
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );

    // Create frame, attach buffer, capture
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    // Flush and wait for ready
    conn.flush()?;
    while !state.frame_ready && !state.frame_failed {
        eq.roundtrip(&mut state)?;
    }

    if state.frame_failed {
        anyhow::bail!("Screenshot capture failed");
    }

    // Read pixels from SHM
    let pixels = read_shm_buffer(state.shm_fd.as_ref().unwrap(), size)?;

    // Convert pixel format and save as PNG
    save_png(save_path, &pixels, width, height, format)?;

    // Cleanup protocol objects
    frame.destroy();
    session.destroy();
    source.destroy();
    buffer.destroy();
    pool.destroy();
    conn.flush()?;

    Ok(save_path.to_path_buf())
}

/// Capture screenshot with auto-generated filename in the given directory.
pub fn capture_to_dir(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let filename = format!("Screenshot_{ts}.png");
    let path = dir.join(filename);
    capture_screenshot(&path)
}

/// Default screenshots directory.
pub fn screenshots_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.picture_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Pictures")
        })
        .join("Screenshots")
}

// ── SHM helpers ──

fn create_memfd(size: usize) -> Result<std::fs::File> {
    use std::ffi::CString;
    let name = CString::new("cosmix-screenshot").unwrap();
    let fd = unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    };
    if fd < 0 {
        anyhow::bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file)
}

use std::os::unix::io::FromRawFd;

fn read_shm_buffer(file: &std::fs::File, size: usize) -> Result<Vec<u8>> {
    use std::os::unix::io::AsRawFd;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        anyhow::bail!("mmap failed: {}", std::io::Error::last_os_error());
    }
    let data = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
    unsafe { libc::munmap(ptr, size) };
    Ok(data)
}

fn save_png(path: &Path, pixels: &[u8], width: u32, height: u32, format: wl_shm::Format) -> Result<()> {
    // Convert from Wayland SHM format to RGBA for PNG
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    let needs_bgr_swap = matches!(
        format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    );

    for i in 0..(width * height) as usize {
        let src = i * 4;
        let dst = i * 4;

        if needs_bgr_swap {
            // ARGB8888/XRGB8888 (little-endian) → memory layout is BGRA
            // Convert to RGBA for PNG
            rgba[dst]     = pixels[src + 2]; // R ← from byte 2
            rgba[dst + 1] = pixels[src + 1]; // G ← from byte 1
            rgba[dst + 2] = pixels[src];     // B ← from byte 0
            rgba[dst + 3] = 255;             // A = opaque
        } else {
            // ABGR8888/XBGR8888 → memory layout is RGBA already
            rgba[dst]     = pixels[src];
            rgba[dst + 1] = pixels[src + 1];
            rgba[dst + 2] = pixels[src + 2];
            rgba[dst + 3] = 255;
        }
    }

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;

    Ok(())
}

// ── Wayland dispatches ──

impl Dispatch<wl_registry::WlRegistry, ()> for ScreenshotState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name, version.min(1), qh, (),
                    ));
                }
                "wl_output" if state.output.is_none() => {
                    // Bind first output (primary screen)
                    state.output = Some(registry.bind::<wl_output::WlOutput, _, _>(
                        name, version.min(4), qh, (),
                    ));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_manager = Some(
                        registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                            name, version.min(1), qh, (),
                        ),
                    );
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_manager = Some(
                        registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                            name, version.min(1), qh, (),
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}

// Session events — buffer constraints
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for ScreenshotState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.width = width;
                state.height = height;
            }
            Event::ShmFormat { format } => {
                // Prefer XRGB8888 or ARGB8888
                if let wayland_client::WEnum::Value(fmt) = format {
                    if state.shm_format.is_none()
                        || matches!(fmt, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888)
                    {
                        state.shm_format = Some(fmt);
                    }
                }
            }
            Event::Done => {
                state.constraints_done = true;
            }
            _ => {}
        }
    }
}

// Frame events — capture result
impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for ScreenshotState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => {
                state.frame_ready = true;
            }
            Event::Failed { reason } => {
                tracing::error!("Screenshot frame failed: {reason:?}");
                state.frame_failed = true;
            }
            _ => {} // Transform, Damage, PresentationTime — ignored for simple capture
        }
    }
}

// Stub dispatches for objects we don't handle events on
impl Dispatch<wl_shm::WlShm, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_buffer::WlBuffer, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &ExtImageCopyCaptureManagerV1, _: <ExtImageCopyCaptureManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &ExtOutputImageCaptureSourceManagerV1, _: <ExtOutputImageCaptureSourceManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for ScreenshotState {
    fn event(_: &mut Self, _: &ExtImageCaptureSourceV1, _: <ExtImageCaptureSourceV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
