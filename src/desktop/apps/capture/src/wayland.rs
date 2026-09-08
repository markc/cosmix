//! Native screencopy consumer of the compositor's renderer-owned capture lane.
use std::{
    collections::BTreeMap,
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle,
    backend::WaylandError,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager,
};

pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
#[derive(Clone, Copy, Default)]
pub struct FrameTiming {
    pub wait_ms: u64,
    pub read_ms: u64,
    pub normalise_ms: u64,
}
struct State {
    outputs: BTreeMap<u32, (wl_output::WlOutput, Option<String>)>,
    shm: Option<wl_shm::WlShm>,
    manager: Option<manager::ZwlrScreencopyManagerV1>,
    frames: BTreeMap<u64, PendingFrame>,
    error: Option<String>,
    selected: Option<u32>,
    expected_extent: Option<(u32, u32)>,
    free_slots: Vec<ShmSlot>,
    slot_limit: usize,
    last_success: Instant,
}
type ShmOffer = (wl_shm::Format, u32, u32, u32);
struct ShmSlot {
    backing: MappedShm,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    offer: ShmOffer,
}
/// Fixed-size owned SHM, mapped once for the slot's lifetime. The compositor
/// may write only between Copy and Ready; no Rust slice exists in that interval.
struct MappedShm {
    file: File,
    address: std::ptr::NonNull<u8>,
    size: usize,
}
// SAFETY: ownership moves into the single capture producer. Moving the mapping
// does not move its pages, and all reads and Copy requests stay on that thread.
unsafe impl Send for MappedShm {}
impl MappedShm {
    fn new(size: usize) -> Result<Self, String> {
        if size == 0 || size > 128 * 1024 * 1024 {
            return Err("SHM mapping size exceeds bounded allocation".into());
        }
        // SAFETY: static nul-terminated name, no borrowed descriptor escapes.
        let fd = unsafe {
            libc::memfd_create(
                c"cosmix-capture".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: fd is newly created and transferred exactly once to File.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size as u64).map_err(|e| e.to_string())?;
        // Prevent either endpoint from truncating the mapping (SIGBUS) or
        // growing it beyond the budget. Writes remain permitted for Copy.
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: fixed, sealed extent of this owned regular memfd; mapping is
        // read-only and retained until Drop. No slice is created here.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let Some(address) = std::ptr::NonNull::new(address.cast::<u8>()) else {
            // SAFETY: even a successful address-zero mapping must be released.
            unsafe {
                libc::munmap(address, size);
            }
            return Err("SHM mapping returned null address".into());
        };
        Ok(Self {
            file,
            address,
            size,
        })
    }
    /// # Safety
    /// The compositor must have completed its copy (Ready), with no new Copy
    /// using this slot until the returned borrow has ended.
    unsafe fn completed_bytes(&self) -> &[u8] {
        // SAFETY: constructor seals the valid mapping extent; caller guarantees
        // no writer for the lifetime of this borrowed slice.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.size) }
    }
}
impl Drop for MappedShm {
    fn drop(&mut self) {
        // SAFETY: this owns the unique mapping; no borrowed slice can outlive it.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), self.size);
        }
    }
}
impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
struct PendingFrame {
    frame: frame::ZwlrScreencopyFrameV1,
    started: Instant,
    slot: Option<ShmSlot>,
    shm_offer: Option<ShmOffer>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    flipped: bool,
    ready: bool,
    ready_at: Option<Instant>,
    error: Option<FrameFailure>,
    timestamp: Option<(u32, u32, u32)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum FrameFailure {
    Refused,
    Fatal(String),
}
impl FrameFailure {
    fn refuse(error: &mut Option<Self>) {
        // A later refusal cannot downgrade a malformed frame to retryable.
        error.get_or_insert(Self::Refused);
    }
    fn terminal(&self, streaming: bool) -> Option<String> {
        match self {
            Self::Refused if streaming => None,
            Self::Refused => Some("compositor refused screenshot".into()),
            Self::Fatal(error) => Some(error.clone()),
        }
    }
}
impl PendingFrame {
    fn new(frame: frame::ZwlrScreencopyFrameV1) -> Self {
        Self {
            frame,
            started: Instant::now(),
            slot: None,
            shm_offer: None,
            width: 0,
            height: 0,
            stride: 0,
            format: wl_shm::Format::Xrgb8888,
            flipped: false,
            ready: false,
            ready_at: None,
            error: None,
            timestamp: None,
        }
    }
}
impl Drop for PendingFrame {
    fn drop(&mut self) {
        self.frame.destroy();
    }
}
pub struct Capture {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    cancel: Arc<AtomicBool>,
    pub timing: FrameTiming,
    next_id: u64,
    next_due: Instant,
    pub repeated_frames: u64,
    last_timestamp: Option<(u32, u32, u32)>,
    streaming: bool,
    pub refused_frames: u64,
}
impl Capture {
    pub fn presentation_sequence(&self) -> u128 {
        presentation_sequence(self.last_timestamp.unwrap_or_default())
    }
    fn dispatch_pending(&mut self) -> Result<(), String> {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => Ok(()),
            Err(DispatchError::Backend(error)) if retryable_io(&error) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    pub fn connect(output: Option<&str>, cancel: Arc<AtomicBool>) -> Result<Self, String> {
        let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let state = State {
            outputs: BTreeMap::new(),
            shm: None,
            manager: None,
            frames: BTreeMap::new(),
            error: None,
            selected: None,
            expected_extent: None,
            free_slots: Vec::new(),
            slot_limit: 1,
            last_success: Instant::now(),
        };
        let mut this = Self {
            connection,
            queue,
            state,
            cancel,
            timing: FrameTiming::default(),
            next_id: 0,
            next_due: Instant::now(),
            repeated_frames: 0,
            last_timestamp: None,
            streaming: false,
            refused_frames: 0,
        };
        this.wait(Duration::from_secs(3), |s| {
            s.shm.is_some()
                && s.manager.is_some()
                && s.outputs.values().any(|(_, n)| {
                    n.as_deref()
                        .is_some_and(|n| output.is_none_or(|wanted| n == wanted))
                })
        })?;
        this.state.selected = this
            .state
            .outputs
            .iter()
            .find(|(_, (_, n))| {
                n.as_deref()
                    .is_some_and(|n| output.is_none_or(|wanted| n == wanted))
            })
            .map(|(id, _)| *id);
        Ok(this)
    }
    fn wait(&mut self, timeout: Duration, done: impl Fn(&State) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("capture cancelled".into());
            }
            self.dispatch_pending()?;
            if let Some(error) = &self.state.error {
                return Err(error.clone());
            }
            if let Some(error) = self
                .state
                .frames
                .values()
                .find_map(|f| f.error.as_ref().and_then(|e| e.terminal(self.streaming)))
            {
                return Err(error);
            }
            if done(&self.state) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "Wayland capture deadline elapsed (output inactive or unavailable)".into(),
                );
            }
            let needs_write = flush_pending(&self.connection)?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN | if needs_write { libc::POLLOUT } else { 0 },
                revents: 0,
            };
            // SAFETY: one valid pollfd, connection guard retains its descriptor.
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 20) as i32;
            let result = unsafe { libc::poll(&mut fd, 1, wait_ms) };
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err("Wayland compositor disconnected".into());
            }
            if fd.revents & libc::POLLIN != 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(error) if retryable_io(&error) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    pub fn frame(&mut self) -> Result<Pixels, String> {
        self.streaming = false;
        self.state.free_slots.clear();
        let id = self.request()?;
        self.wait(Duration::from_secs(2), |s| {
            s.frames.get(&id).is_some_and(|f| f.ready)
        })?;
        self.take_frame(id)
    }
    fn request(&mut self) -> Result<u64, String> {
        let output = self
            .state
            .selected
            .and_then(|id| self.state.outputs.get(&id))
            .ok_or("selected output removed")?;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("capture request ID exhausted")?;
        let id = self.next_id;
        let frame = self
            .state
            .manager
            .as_ref()
            .ok_or("screencopy unavailable")?
            .capture_output(1, &output.0, &self.queue.handle(), id);
        self.state.frames.insert(id, PendingFrame::new(frame));
        // A full socket retains queued requests. The dispatcher retries on
        // POLLOUT while preserving the request's original deadline.
        flush_pending(&self.connection)?;
        Ok(id)
    }
    fn take_frame(&mut self, id: u64) -> Result<Pixels, String> {
        let mut frame = self
            .state
            .frames
            .remove(&id)
            .ok_or("missing completed frame")?;
        self.state.expected_extent = Some((frame.width, frame.height));
        if new_presentation(self.last_timestamp, frame.timestamp) {
            self.last_timestamp = frame.timestamp;
        }
        self.timing = FrameTiming::default();
        self.timing.wait_ms = frame
            .ready_at
            .unwrap_or_else(Instant::now)
            .duration_since(frame.started)
            .as_millis() as u64;
        if !frame.ready || frame.error.is_some() {
            return Err("cannot read incomplete capture".into());
        }
        let normalise_started = Instant::now();
        // SAFETY: Ready is the SHM write completion barrier. The slot is still
        // exclusively owned by this removed PendingFrame, and is returned to
        // the free list only after normalise finishes. Its fixed extent is
        // sealed against truncation. No source slice escapes normalise.
        let bytes = unsafe {
            frame
                .slot
                .as_ref()
                .ok_or("capture has no pixels")?
                .backing
                .completed_bytes()
        };
        let result = normalise(
            bytes,
            frame.width,
            frame.height,
            frame.stride,
            frame.format,
            frame.flipped,
        );
        self.timing.normalise_ms = normalise_started.elapsed().as_millis() as u64;
        // Ready transfers the completed SHM contents to the reader; this lane
        // does not use wl_buffer.release. Only recycle after our read finishes,
        // and never recycle a failed/partial copy or a screenshot allocation.
        if self.streaming && result.is_ok() && frame.ready && frame.error.is_none() {
            let occupied = self
                .state
                .frames
                .values()
                .filter(|f| f.slot.is_some())
                .count();
            if occupied + self.state.free_slots.len() < self.state.slot_limit
                && let Some(slot) = frame.slot.take()
            {
                self.state.free_slots.push(slot);
            }
        }
        drop(frame);
        let _ = self.connection.flush();
        result
    }
    /// Request at a stable cadence while previous requests wait for display
    /// presentation. Completed request IDs are consumed in order.
    pub fn stream_frame(&mut self, fps: u32, in_flight: usize) -> Result<(Pixels, u128), String> {
        self.streaming = true;
        self.state.slot_limit = in_flight.clamp(1, 3);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("capture cancelled".into());
            }
            self.dispatch_pending()?;
            if let Some(error) = &self.state.error {
                return Err(error.clone());
            }
            if let Some(error) = self
                .state
                .frames
                .values()
                .find_map(|f| f.error.as_ref().and_then(|e| e.terminal(true)))
            {
                return Err(error);
            }
            let refused = self
                .state
                .frames
                .values()
                .filter(|f| f.error == Some(FrameFailure::Refused))
                .count();
            self.refused_frames += refused as u64;
            self.state
                .frames
                .retain(|_, frame| frame.error != Some(FrameFailure::Refused));
            let now = Instant::now();
            if success_deadline_elapsed(self.state.last_success, now) {
                return Err("no successful capture response within two seconds".into());
            }
            if self
                .state
                .frames
                .values()
                .any(|f| now.duration_since(f.started) >= Duration::from_secs(2))
            {
                return Err("pipelined capture deadline elapsed".into());
            }
            if now >= self.next_due && self.state.frames.len() < self.state.slot_limit {
                self.request()?;
                let late = now.duration_since(self.next_due).as_nanos() % interval.as_nanos();
                self.next_due = now + interval - Duration::from_nanos(late as u64);
            }
            if let Some((&id, first)) = self.state.frames.first_key_value()
                && first.ready
            {
                let fresh = new_presentation(self.last_timestamp, first.timestamp);
                let sequence =
                    presentation_sequence(first.timestamp.ok_or("Ready missing timestamp")?);
                if !fresh {
                    self.repeated_frames += 1;
                }
                // Cursor overlay can change independently of the output's
                // presentation timestamp. Read every successful response.
                return self.take_frame(id).map(|pixels| (pixels, sequence));
            }
            // Reuse the same cancellation-aware dispatcher; reaching this
            // short scheduling deadline is normal, not a capture timeout.
            let wake = self
                .next_due
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
                .min(Duration::from_millis(20));
            match self.wait(wake, |s| {
                s.frames.first_key_value().is_some_and(|(_, f)| f.ready)
            }) {
                Ok(()) => {}
                Err(error) if error.starts_with("Wayland capture deadline elapsed") => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn retryable_io(error: &WaylandError) -> bool {
    matches!(error, WaylandError::Io(error) if matches!(error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted))
}

fn success_deadline_elapsed(last_success: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_success) >= Duration::from_secs(2)
}

/// True means outbound bytes remain queued and the poll must also watch
/// writability. Neither EAGAIN nor EINTR is a terminal protocol failure.
fn flush_pending(connection: &Connection) -> Result<bool, String> {
    match connection.flush() {
        Ok(()) => Ok(false),
        Err(error) if retryable_io(&error) => Ok(true),
        Err(error) => Err(error.to_string()),
    }
}

pub fn pipeline_depth(width: u32, height: u32) -> usize {
    let row = u64::from(width) * 4;
    let staging_row = row.div_ceil(256) * 256;
    // Mirrors the compositor's conservative source*2 + staging + region
    // reservation for a full-output SHM request, not just the client buffer.
    let reserved = (staging_row + row * 3) * u64::from(height);
    if reserved == 0 {
        return 1;
    }
    // Leave room for one completed request whose compositor reservation has
    // not yet retired. Keep the single-request path for large outputs.
    ((512 * 1024 * 1024 / reserved) as usize)
        .saturating_sub(1)
        .clamp(1, 3)
}

fn new_presentation(last: Option<(u32, u32, u32)>, next: Option<(u32, u32, u32)>) -> bool {
    next.is_some_and(|next| last.is_none_or(|last| next > last))
}

fn presentation_sequence((hi, lo, nanos): (u32, u32, u32)) -> u128 {
    (u128::from(hi) << 64) | (u128::from(lo) << 32) | u128::from(nanos)
}

fn layout(width: u32, height: u32, stride: u32) -> Result<usize, String> {
    let bytes = u64::from(stride) * u64::from(height);
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || stride < width * 4
        || bytes > 128 * 1024 * 1024
    {
        return Err("capture dimensions/stride exceed bounded allocation".into());
    }
    Ok(bytes as usize)
}
fn normalise(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    flipped: bool,
) -> Result<Pixels, String> {
    let size = layout(width, height, stride)?;
    if bytes.len() != size {
        return Err("incomplete captured pixels".into());
    }
    if !matches!(
        format,
        wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Abgr8888
    ) {
        return Err("unsupported capture pixel format".into());
    }
    let row_bytes = width as usize * 4;
    let mut rgba = vec![0; row_bytes * height as usize];
    let bgra = matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
    for (row, destination) in rgba.chunks_exact_mut(row_bytes).enumerate() {
        let source_row = if flipped {
            height as usize - 1 - row
        } else {
            row
        };
        let start = source_row * stride as usize;
        let source = &bytes[start..start + row_bytes];
        // Specialise outside the pixel loop. Presized disjoint slices avoid
        // Vec length/capacity updates for every pixel and permit vectorisation.
        if bgra {
            convert_row::<true>(source, destination);
        } else {
            convert_row::<false>(source, destination);
        }
    }
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

fn convert_row<const BGRA: bool>(source: &[u8], destination: &mut [u8]) {
    for (pixel, output) in source.chunks_exact(4).zip(destination.chunks_exact_mut(4)) {
        let word = u32::from_ne_bytes(pixel.try_into().unwrap());
        let (r, b) = if BGRA {
            ((word >> 16) as u8, word as u8)
        } else {
            (word as u8, (word >> 16) as u8)
        };
        // Composed output is opaque; disregard undefined X/alpha bits.
        output.copy_from_slice(&[r, (word >> 8) as u8, b, 255]);
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => s.shm = Some(registry.bind(name, 1, q, ())),
                "wl_output" if version >= 4 => {
                    s.outputs
                        .insert(name, (registry.bind(name, 4, q, name), None));
                }
                "zwlr_screencopy_manager_v1" if version >= 3 => {
                    s.manager = Some(registry.bind(name, 3, q, ()))
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                s.outputs.remove(&name);
                if s.selected == Some(name) {
                    s.error = Some("selected output removed".into());
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = s.outputs.get_mut(id)
        {
            output.1 = Some(name);
        }
    }
}
impl Dispatch<frame::ZwlrScreencopyFrameV1, u64> for State {
    fn event(
        state: &mut Self,
        frame: &frame::ZwlrScreencopyFrameV1,
        event: frame::Event,
        id: &u64,
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        let shm = state.shm.clone();
        let expected_extent = state.expected_extent;
        let Some(s) = state.frames.get_mut(id) else {
            return;
        };
        match event {
            frame::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if expected_extent.is_some_and(|extent| extent != (width, height)) {
                    s.error = Some(FrameFailure::Fatal(
                        "output dimensions changed during capture".into(),
                    ));
                    return;
                }
                match format.into_result() {
                    Ok(format) => s.shm_offer = Some((format, width, height, stride)),
                    Err(_) => s.error = Some(FrameFailure::Fatal("unknown pixel format".into())),
                }
            }
            frame::Event::BufferDone => {
                let result = (|| -> Result<(), String> {
                    if s.slot.is_some() {
                        return Err("duplicate screencopy buffer_done".into());
                    }
                    let (format, width, height, stride) = s
                        .shm_offer
                        .take()
                        .ok_or("compositor offered no SHM capture buffer")?;
                    if !matches!(
                        format,
                        wl_shm::Format::Xrgb8888
                            | wl_shm::Format::Argb8888
                            | wl_shm::Format::Xbgr8888
                            | wl_shm::Format::Abgr8888
                    ) {
                        return Err("unsupported capture format".into());
                    }
                    let size = layout(width, height, stride)?;
                    let offer = (format, width, height, stride);
                    // Free slots hold no compositor write in progress. Purge
                    // incompatible layouts before allocation so free+occupied
                    // cannot grow past the bounded number of requests.
                    state.free_slots.retain(|slot| slot.offer == offer);
                    let slot = match state.free_slots.pop() {
                        Some(slot) => slot,
                        None => {
                            let backing = MappedShm::new(size)?;
                            let pool = shm.as_ref().ok_or("no SHM global")?.create_pool(
                                backing.file.as_fd(),
                                size as i32,
                                q,
                                (),
                            );
                            let buffer = pool.create_buffer(
                                0,
                                width as i32,
                                height as i32,
                                stride as i32,
                                format,
                                q,
                                (),
                            );
                            ShmSlot {
                                backing,
                                pool,
                                buffer,
                                offer,
                            }
                        }
                    };
                    frame.copy(&slot.buffer);
                    s.slot = Some(slot);
                    s.width = width;
                    s.height = height;
                    s.stride = stride;
                    s.format = format;
                    Ok(())
                })();
                if let Err(e) = result {
                    s.error = Some(FrameFailure::Fatal(e));
                }
            }
            frame::Event::Flags { flags } => {
                if let Ok(flags) = flags.into_result() {
                    s.flipped = flags.contains(frame::Flags::YInvert);
                }
            }
            frame::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                if tv_nsec > 999_999_999 {
                    s.error = Some(FrameFailure::Fatal("invalid presentation timestamp".into()));
                    return;
                }
                s.timestamp = Some((tv_sec_hi, tv_sec_lo, tv_nsec));
                s.ready = true;
                s.ready_at = Some(Instant::now());
                state.last_success = s.ready_at.unwrap();
            }
            frame::Event::Failed => FrameFailure::refuse(&mut s.error),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore manager::ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;
    #[test]
    fn persistent_mapping_observes_reused_slot_and_seals_its_extent() {
        let backing = MappedShm::new(4).unwrap();
        let address = backing.address;
        for word in [0x00010203u32, 0x00040506] {
            // Model Copy followed by Ready: complete the writer before making
            // a borrowed read slice; that slice ends before the next write.
            backing.file.write_all_at(&word.to_ne_bytes(), 0).unwrap();
            let pixels = normalise(
                unsafe { backing.completed_bytes() },
                1,
                1,
                4,
                wl_shm::Format::Xrgb8888,
                false,
            )
            .unwrap();
            assert_eq!(
                pixels.rgba,
                [(word >> 16) as u8, (word >> 8) as u8, word as u8, 255]
            );
            assert_eq!(backing.address, address);
        }
        assert!(backing.file.set_len(2).is_err());
        assert!(backing.file.set_len(8).is_err());
        assert_eq!(backing.file.metadata().unwrap().len(), 4);
        assert!(MappedShm::new(0).is_err());
        assert!(MappedShm::new(128 * 1024 * 1024 + 1).is_err());
    }
    #[test]
    fn only_individual_refusal_is_retryable_in_video() {
        let mut failure = Some(FrameFailure::Fatal("bad format".into()));
        FrameFailure::refuse(&mut failure);
        assert_eq!(failure.unwrap().terminal(true), Some("bad format".into()));
        assert!(FrameFailure::Refused.terminal(true).is_none());
        assert!(FrameFailure::Refused.terminal(false).is_some());
        assert_eq!(
            FrameFailure::Fatal("bad format".into()).terminal(true),
            Some("bad format".into())
        );
    }
    #[test]
    fn refusals_do_not_extend_the_successful_response_deadline() {
        let last_fresh = Instant::now();
        for millis in [0, 33, 66, 1000, 1999] {
            assert!(FrameFailure::Refused.terminal(true).is_none());
            assert!(!success_deadline_elapsed(
                last_fresh,
                last_fresh + Duration::from_millis(millis)
            ));
        }
        assert!(success_deadline_elapsed(
            last_fresh,
            last_fresh + Duration::from_secs(2)
        ));
    }
    #[test]
    fn valid_repeated_responses_keep_static_capture_alive() {
        let start = Instant::now();
        let timestamp = Some((0, 42, 0));
        let mut last_success = start;
        for second in 1..=10 {
            let now = start + Duration::from_secs(second);
            assert!(!success_deadline_elapsed(last_success, now));
            assert!(!new_presentation(timestamp, timestamp));
            last_success = now;
        }
        assert!(success_deadline_elapsed(
            last_success,
            last_success + Duration::from_secs(2)
        ));
    }
    #[test]
    fn all_formats_preserve_channels_odd_width_padding_and_flip() {
        let words: [u32; 8] = [
            0x12010203, 0x34040506, 0x56070809, 0xdeadbeef, 0x780a0b0c, 0x9a0d0e0f, 0xbc101112,
            0xdeadbeef,
        ];
        let bytes: Vec<_> = words.into_iter().flat_map(u32::to_ne_bytes).collect();
        for format in [
            wl_shm::Format::Xrgb8888,
            wl_shm::Format::Argb8888,
            wl_shm::Format::Xbgr8888,
            wl_shm::Format::Abgr8888,
        ] {
            for flipped in [false, true] {
                let image = normalise(&bytes, 3, 2, 16, format, flipped).unwrap();
                let mut expected = Vec::new();
                for row in if flipped { [1, 0] } else { [0, 1] } {
                    for column in 0..3 {
                        let first = (row * 9 + column * 3 + 1) as u8;
                        if matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) {
                            expected.extend_from_slice(&[first, first + 1, first + 2, 255]);
                        } else {
                            expected.extend_from_slice(&[first + 2, first + 1, first, 255]);
                        }
                    }
                }
                assert_eq!(image.rgba, expected, "{format:?}, flipped={flipped}");
            }
        }
    }
    #[test]
    fn nonblocking_socket_retry_does_not_hide_disconnects() {
        for kind in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::Interrupted,
        ] {
            assert!(retryable_io(&WaylandError::Io(std::io::Error::from(kind))));
        }
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(!retryable_io(&WaylandError::Io(std::io::Error::from(kind))));
        }
    }
    #[test]
    fn pipeline_stays_within_compositor_per_client_reservations() {
        assert_eq!(pipeline_depth(1920, 1280), 3);
        assert_eq!(pipeline_depth(3840, 2160), 3);
        assert_eq!(pipeline_depth(5120, 2880), 1);
        // No subtraction underflow when even one request exceeds the budget;
        // admission still belongs to the compositor.
        assert_eq!(pipeline_depth(8192, 8192), 1);
        assert_eq!(pipeline_depth(0, 0), 1);
    }
    #[test]
    fn duplicate_and_out_of_order_presentations_are_not_fresh() {
        let time = (1, 42, 100);
        assert!(new_presentation(None, Some(time)));
        assert!(!new_presentation(Some(time), Some(time)));
        assert!(!new_presentation(Some(time), Some((1, 42, 99))));
        assert!(new_presentation(Some(time), Some((1, 42, 101))));
        assert!(!new_presentation(Some(time), None));
    }
    #[test]
    fn padded_inverted_xrgb_is_normalised() {
        let words = [0x00ff0000u32, 0xdeadbeef, 0x000000ff, 0xdeadbeef];
        let bytes: Vec<_> = words.into_iter().flat_map(u32::to_ne_bytes).collect();
        let p = normalise(&bytes, 1, 2, 8, wl_shm::Format::Xrgb8888, true).unwrap();
        assert_eq!(p.rgba, [0, 0, 255, 255, 255, 0, 0, 255]);
    }
    #[test]
    fn black_is_valid_and_allocations_are_bounded() {
        assert_eq!(
            normalise(&[0; 4], 1, 1, 4, wl_shm::Format::Xrgb8888, false)
                .unwrap()
                .rgba,
            [0, 0, 0, 255]
        );
        assert!(layout(0, 1, 4).is_err());
        assert!(layout(8192, 8192, 32768).is_err());
        assert!(layout(2, 1, 4).is_err());
        assert!(normalise(&[0; 3], 1, 1, 4, wl_shm::Format::Xrgb8888, false).is_err());
    }
}
