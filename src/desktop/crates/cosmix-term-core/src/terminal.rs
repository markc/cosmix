use crate::metrics::Metrics;
#[path = "mouse.rs"]
mod mouse;
pub use mouse::MouseModifiers;
#[path = "selection.rs"]
mod selection;
pub use rio_vt::{crosswords::pos::Side as SelectionSide, selection::SelectionType};
use rio_vt::selection::SelectionRange;
use rio_vt::{
    ansi::CursorShape,
    corcovado::{Poll, PollOpt, Ready, Token, channel},
    crosswords::{Crosswords, CrosswordsSize, TermDamage, pos::Column, style::StyleFlags},
    event::{EventListener, Msg, RioEvent, WindowId, WindowSize, sync::FairMutex},
    performer::Machine,
    teletypewriter::{self, ChildEvent, EventedPty, ProcessReadWrite, WinsizeBuilder},
};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub type Wake = Arc<dyn Fn() + Send + Sync>;
pub type Stats = Arc<Mutex<Metrics>>;
type Grid = Arc<FairMutex<Crosswords<Listener>>>;
type WeakGrid = std::sync::Weak<FairMutex<Crosswords<Listener>>>;

fn rearm_damage(term: &mut Crosswords<Listener>) {
    term.reset_damage();
    term.damage_event_in_flight = false;
}

/// Rows changed since the last `rearm_damage`. Cursor changes are added by
/// capture, relative to the last consuming read. Scrolled-back views
/// are repainted whole: rio reports their damage in scrollback coordinates.
///
/// rio's `damage()` is not read-only. In insert mode (IRM) it calls
/// `mark_fully_damaged`, which after a rearm emits `RenderRoute`, which
/// `Listener` turns into a fresh damage token and wake: every snapshot would
/// schedule the next. Insert mode repaints whole anyway, so skip the call.
fn dirty_rows(term: &mut Crosswords<Listener>, previous_offset: usize) -> Vec<bool> {
    let mut dirty = vec![false; term.screen_lines()];
    if term.display_offset() != previous_offset
        || term.display_offset() != 0
        || term.mode().contains(rio_vt::crosswords::Mode::INSERT)
    {
        dirty.fill(true);
        return dirty;
    }
    match term.damage() {
        TermDamage::Partial(lines) => {
            for line in lines {
                if let Some(row) = dirty.get_mut(line.line) {
                    *row = true;
                }
            }
        }
        _ => dirty.fill(true),
    }
    dirty
}

struct Pending {
    remaining: usize,
    key: Option<Instant>,
    permit: Option<Arc<crate::control::Permit>>,
    control: bool,
}
#[derive(Default)]
struct Writes {
    #[cfg(test)]
    block_control: bool,
    pty: Option<OwnedFd>,
    group: i32,
    sender: Option<channel::Sender<Msg>>,
    pending: VecDeque<Pending>,
    // Only Bus/control input consumes the bounded admission budget. Human
    // input and VT replies must remain admissible while a paste drains.
    control_bytes: usize,
    foreground: u64,
    owner: Option<(String, Arc<crate::control::Permit>)>,
}

#[derive(Clone)]
pub struct Listener {
    grid: Arc<OnceLock<WeakGrid>>,
    damage: SyncSender<()>,
    wake: Arc<OnceLock<Wake>>,
    writes: Arc<Mutex<Writes>>,
    stats: Stats,
    pub quit: Arc<AtomicBool>,
    title: Arc<Mutex<String>>,
    title_changed: Arc<OnceLock<Arc<tokio::sync::Notify>>>,
}
impl Listener {
    /// Call only after dropping the writes lock: the parser takes grid then
    /// writes when answering VT queries. A weak link avoids a grid/listener cycle.
    fn follow_input(&self) {
        if let Some(grid) = self.grid.get().and_then(std::sync::Weak::upgrade) {
            let mut term = grid.lock();
            if term.display_offset() != 0 {
                term.scroll_display(rio_vt::crosswords::grid::Scroll::Bottom);
                self.dirty();
            }
        }
    }
    pub(crate) fn title(&self) -> String {
        self.title.lock().unwrap().clone()
    }
    pub(crate) fn watch_title(&self, notify: Arc<tokio::sync::Notify>) {
        let _ = self.title_changed.set(notify);
    }
    fn set_title(&self, title: String) {
        let title = sanitise_title(&title);
        let mut current = self.title.lock().unwrap();
        if *current == title {
            return;
        }
        *current = title;
        drop(current);
        if let Some(notify) = self.title_changed.get() {
            notify.notify_one();
        }
        self.wake();
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn test_input_receiver(&self) -> channel::Receiver<Msg> {
        let (sender, receiver) = channel::channel();
        self.writes.lock().unwrap().sender = Some(sender);
        receiver
    }
    /// Capture queued PTY input without exposing Rio types to frontend tests.
    #[cfg(feature = "test-support")]
    pub fn test_input_reader(&self) -> impl Fn() -> Option<Vec<u8>> + use<> {
        let receiver = self.test_input_receiver();
        move || {
            while let Ok(message) = receiver.try_recv() {
                if let Msg::Input(bytes) = message {
                    return Some(bytes.into_owned());
                }
            }
            None
        }
    }
    #[cfg(test)]
    pub(crate) fn block_control_writes(&self, block: bool) {
        self.writes.lock().unwrap().block_control = block;
    }
    pub fn wake(&self) {
        if let Some(wake) = self.wake.get() {
            wake();
        }
    }
    fn dirty(&self) {
        self.stats.lock().unwrap().wakes += 1;
        let _ = self.damage.try_send(());
        self.wake();
    }
    fn write(&self, bytes: Vec<u8>, key: Option<Instant>) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writes = self.writes.lock().unwrap();
        self.enqueue(&mut writes, bytes, key, None, false)
    }
    fn enqueue(
        &self,
        writes: &mut Writes,
        bytes: Vec<u8>,
        key: Option<Instant>,
        permit: Option<Arc<crate::control::Permit>>,
        control: bool,
    ) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.quit.load(Ordering::Acquire) {
            return Err("terminal closing".into());
        }
        let control = control || permit.is_some();
        if control && bytes.len() > 65536_usize.saturating_sub(writes.control_bytes) {
            return Err("PTY input queue full".into());
        }
        let sender = writes.sender.as_ref().ok_or("PTY unavailable")?.clone();
        let len = bytes.len();
        // Serialise enqueue and accounting with actual writes, including VT replies.
        sender
            .send(Msg::Input(Cow::Owned(bytes)))
            .map_err(|e| e.to_string())?;
        if control {
            writes.control_bytes += len;
        }
        writes.pending.push_back(Pending {
            remaining: len,
            key,
            permit,
            control,
        });
        Ok(())
    }
    // Synthetic keys assert foreground authority exactly like real ones:
    // revoke any delegated control writer before enqueuing, mirroring key().
    // The empty-bytes early return is deliberately BEFORE the revoke, unlike
    // key() which revokes unconditionally: a no-op write must not invalidate
    // a live control writer.
    pub fn type_text(&self, text: &str) -> Result<(), String> {
        self.type_text_in_lane(text, false)
    }
    /// Legacy diagnostic Bus input is still bounded, even without a permit.
    pub(crate) fn bus_text(&self, text: &str) -> Result<(), String> {
        self.type_text_in_lane(text, true)
    }
    fn type_text_in_lane(&self, text: &str, control: bool) -> Result<(), String> {
        let bytes = encode_text(text)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writes = self.writes.lock().unwrap();
        Self::revoke_writer(&mut writes);
        self.enqueue(&mut writes, bytes, Some(Instant::now()), None, control)?;
        drop(writes);
        self.follow_input();
        Ok(())
    }
    pub fn key(&self, key: Key, at: Instant) -> Result<(), String> {
        let mut writes = self.writes.lock().unwrap();
        Self::revoke_writer(&mut writes);
        let bytes = encode(key);
        let sends_bytes = !bytes.is_empty();
        self.enqueue(&mut writes, bytes, Some(at), None, false)?;
        drop(writes);
        if sends_bytes {
            self.follow_input();
        }
        Ok(())
    }
    fn revoke_writer(writes: &mut Writes) {
        writes.foreground = writes.foreground.saturating_add(1);
        if let Some((_, permit)) = writes.owner.take() {
            permit.revoke();
        }
        for pending in &writes.pending {
            if let Some(permit) = &pending.permit {
                permit.revoke();
            }
        }
    }
    pub fn revoke_control(&self) {
        Self::revoke_writer(&mut self.writes.lock().unwrap());
    }
    pub fn foreground_generation(&self) -> u64 {
        let mut writes = self.writes.lock().unwrap();
        Self::check_foreground(&mut writes);
        writes.foreground + 1
    }
    fn check_foreground(writes: &mut Writes) -> bool {
        let Some(fd) = &writes.pty else {
            return false;
        };
        // The descriptor is owned for the entire locked query; PID/name
        // inference never establishes input authority.
        let group = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) };
        if group != writes.group {
            Self::revoke_writer(writes);
            writes.group = group;
        }
        group > 0
    }
    pub fn control_text(
        &self,
        text: &str,
        generation: u64,
        actor: &str,
        permit: Arc<crate::control::Permit>,
    ) -> Result<(), &'static str> {
        let bytes = encode_text(text).map_err(|_| "INVALID_ARGUMENT")?;
        let sends_bytes = !bytes.is_empty();
        let mut writes = self.writes.lock().unwrap();
        if !Self::check_foreground(&mut writes) {
            return Err("FORBIDDEN");
        }
        if generation != writes.foreground + 1 {
            return Err("STALE_GENERATION");
        }
        if !permit.valid() {
            return Err("FORBIDDEN");
        }
        if !crate::control::mesh_open()
            && writes
                .owner
                .as_ref()
                .is_some_and(|(owner, p)| owner != actor && p.valid())
        {
            return Err("BUSY");
        }
        self.enqueue(
            &mut writes,
            bytes,
            Some(Instant::now()),
            Some(permit.clone()),
            true,
        )
        .map_err(|_| "RESOURCE_LIMIT")?;
        writes.owner = Some((actor.into(), permit));
        drop(writes);
        if sends_bytes {
            self.follow_input();
        }
        Ok(())
    }
}
impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _: WindowId) {
        match event {
            RioEvent::Title(title) | RioEvent::TitleWithSubtitle(title, _) => self.set_title(title),
            RioEvent::ResetTitle => self.set_title(String::new()),
            RioEvent::PtyWrite(_, text) => {
                if let Err(e) = self.write(text.into_bytes(), None) {
                    eprintln!("PTY reply failed: {e}");
                }
            }
            RioEvent::TerminalDamaged(_) => {
                // Machine calls this after parser.advance, while holding the grid lock.
                self.stats.lock().unwrap().parsed_boundary();
                self.dirty();
            }
            RioEvent::Render | RioEvent::RenderRoute(_) => self.dirty(),
            RioEvent::ChildExited(_, status) => {
                eprintln!("DIAGNOSTIC child exited/reaped by Machine: {status:?}");
                self.quit.store(true, Ordering::Release);
                self.dirty();
            }
            RioEvent::Exit | RioEvent::Quit | RioEvent::CloseTerminal(_) => {
                self.quit.store(true, Ordering::Release);
                self.wake();
            }
            other => eprintln!(
                "term unsupported VT event dropped: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Interrupt,
    Eof,
    Escape,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    Control(char),
}
pub fn encode(key: Key) -> Vec<u8> {
    match key {
        Key::Char(c) if c.is_ascii() && !c.is_control() => vec![c as u8],
        Key::Char(_) => Vec::new(),
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![127],
        Key::Tab => vec![b'\t'],
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right => b"\x1b[C".to_vec(),
        Key::Left => b"\x1b[D".to_vec(),
        Key::Interrupt => vec![3],
        Key::Eof => vec![4],
        Key::Escape => vec![27],
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Control(c) if c.is_ascii_alphabetic() => vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
        Key::Control(_) => Vec::new(),
    }
}
pub fn encode_text(text: &str) -> Result<Vec<u8>, String> {
    if text.len() > 8192 {
        return Err("DIAGNOSTIC type limit is 8192 bytes".into());
    }
    let mut bytes = Vec::new();
    for c in text.chars() {
        let key = match c {
            '\r' | '\n' => Key::Enter,
            '\t' => Key::Tab,
            '\u{7f}' | '\u{8}' => Key::Backspace,
            '\u{3}' => Key::Interrupt,
            '\u{4}' => Key::Eof,
            c if c.is_ascii() && !c.is_control() => Key::Char(c),
            _ => {
                return Err(
                    "DIAGNOSTIC type accepts ASCII, newline, tab, backspace, Ctrl+C/D".into(),
                );
            }
        };
        bytes.extend(encode(key));
    }
    Ok(bytes)
}

// Delegate readiness and session ownership to Rio, instrument only real I/O.
struct MeteredPty {
    pty: teletypewriter::Pty,
    listener: Listener,
    session_exit: Option<Box<dyn Fn() + Send + Sync>>,
}
impl Read for MeteredPty {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.pty.read(bytes)?;
        if n > 0 {
            let mut stats = self.listener.stats.lock().unwrap();
            stats.reads += 1;
            stats.bytes_read += n as u64;
            stats.last_read = Some(Instant::now());
        }
        Ok(n)
    }
}
impl Write for MeteredPty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut writes = self.listener.writes.lock().unwrap();
        Listener::check_foreground(&mut writes);
        #[cfg(test)]
        if writes.block_control && writes.pending.front().is_some_and(|p| p.permit.is_some()) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        // Keep revocation, human admission and actual writes ordered. Rio may
        // retain an unwritten remainder; report discarded bytes as consumed to
        // its queue, never write them or count them as delivered PTY input.
        let limit = writes
            .pending
            .front()
            .map_or(bytes.len(), |p| p.remaining.min(bytes.len()));
        let discarded = writes
            .pending
            .front()
            .is_some_and(|p| p.permit.as_ref().is_some_and(|p| !p.valid()));
        let n = if discarded {
            limit
        } else {
            self.pty.write(&bytes[..limit])?
        };
        let mut stats = self.listener.stats.lock().unwrap();
        if !discarded {
            stats.bytes_written += n as u64;
        }
        let mut left = n;
        while left > 0 {
            let Some(pending) = writes.pending.front_mut() else {
                break;
            };
            let consumed = left.min(pending.remaining);
            if !discarded && let Some(permit) = &pending.permit {
                permit.written.fetch_add(consumed as u64, Ordering::Release);
            }
            if !discarded && let Some(at) = pending.key {
                stats.input_written += consumed as u64;
                stats.key_write.add(at.elapsed());
            } else if !discarded {
                stats.reply_written += consumed as u64;
            }
            pending.remaining -= consumed;
            left -= consumed;
            let control = pending.control;
            if pending.remaining == 0 {
                writes.pending.pop_front();
            }
            if control {
                writes.control_bytes -= consumed;
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.pty.flush()
    }
}
impl ProcessReadWrite for MeteredPty {
    type Reader = Self;
    type Writer = Self;
    fn reader(&mut self) -> &mut Self {
        self
    }
    fn writer(&mut self) -> &mut Self {
        self
    }
    fn read_token(&self) -> Token {
        self.pty.read_token()
    }
    fn write_token(&self) -> Token {
        self.pty.write_token()
    }
    fn set_winsize(&mut self, size: WinsizeBuilder) -> io::Result<()> {
        self.pty.set_winsize(size)
    }
    fn register(
        &mut self,
        poll: &Poll,
        tokens: &mut dyn Iterator<Item = Token>,
        ready: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.pty.register(poll, tokens, ready, opts)
    }
    fn reregister(&mut self, poll: &Poll, ready: Ready, opts: PollOpt) -> io::Result<()> {
        self.pty.reregister(poll, ready, opts)
    }
    fn deregister(&mut self, poll: &Poll) -> io::Result<()> {
        self.pty.deregister(poll)
    }
}
impl EventedPty for MeteredPty {
    fn child_event_token(&self) -> Token {
        self.pty.child_event_token()
    }
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let event = self.pty.next_child_event();
        if event.is_some()
            && let Some(notify) = self.session_exit.take()
        {
            notify();
        }
        event
    }
}

#[derive(Clone)]
pub struct Cell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
}
pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub cells: Vec<Cell>,
    pub updated: Instant,
}
/// A consuming read for a frontend that repaints by row.
pub struct GridSnapshot {
    pub screen: Screen,
    /// One flag per visible row: true when that row may differ from the
    /// previous consuming read. All true after a resize, a viewport change, a
    /// full-screen mode change or on the first read.
    pub dirty_rows: Vec<bool>,
}
/// Frontend-neutral requests to move the history viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollRequest {
    Lines(i64),
    Pages(i64),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

pub struct Terminal {
    #[cfg(test)]
    pub before_pty_cleanup: Option<Box<dyn FnMut() + Send>>,
    session: Option<crate::native_session::PaneSession>,
    pub listener: Listener,
    pub stats: Stats,
    grid: Grid,
    /// Updated only by consuming captures, under the grid lock.
    captured_offset: Mutex<usize>,
    damage: Mutex<Receiver<()>>,
    captured_cursor: Mutex<Option<((usize, usize), bool)>>,
    captured_selection: Mutex<Option<SelectionRange>>,
    pub pid: i32,
    thread: Option<JoinHandle<(Machine<MeteredPty, Listener>, rio_vt::performer::State)>>,
}
/// Bounded WNOHANG reap of a single direct child (mirrors the shutdown reaper):
/// used on a start-time Machine-spawn failure so the SIGHUP'd child can't zombie.
/// ECHILD means it was already reaped; timeout logs and returns rather than hang.
fn reap_child(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid || (rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!("DIAGNOSTIC start-failure child reap timed out for pid {pid}");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// The canonical system Mix — the only shell Term spawns (mandate: Mix is the
/// shell). Probed for executability before spawn; see `Terminal::start_session`.
const MIX_BIN: &str = "/opt/cosmix/bin/mix";

struct LaunchSettings<'a> {
    program: &'a str,
    home: Option<String>,
    cwd: Option<String>,
    environment: Vec<(String, String)>,
}

pub(crate) fn validate_cwd(cwd: &str) -> Result<(), String> {
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() || !path.is_dir() {
        return Err("invalid-argument: cwd must be an absolute existing directory".into());
    }
    let path = std::ffi::CString::new(cwd)
        .map_err(|_| "invalid-argument: cwd contains NUL".to_string())?;
    // SAFETY: path is NUL-terminated and alive for this effective-ID check.
    if unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) } != 0
    {
        return Err("invalid-argument: cwd is not searchable by the current user".into());
    }
    Ok(())
}

fn launch_directory(term_cwd: Option<String>, home: Option<String>) -> Result<String, String> {
    term_cwd
        .filter(|dir| {
            if dir.is_empty() || !std::path::Path::new(dir).is_dir() {
                return false;
            }
            let Ok(path) = std::ffi::CString::new(dir.as_bytes()) else {
                return false;
            };
            // SAFETY: path is NUL-terminated and alive for this effective-ID check.
            unsafe {
                libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0
            }
        })
        .or(home)
        .ok_or_else(|| "HOME is required".into())
}

/// Titles are single-line labels, bounded in UTF-8 bytes at both ingest points.
pub(crate) fn sanitise_title(title: &str) -> String {
    let mut out = String::new();
    for c in title
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
    {
        if out.len() + c.len_utf8() > 256 {
            break;
        }
        out.push(c);
    }
    out
}

// Reserve ample space for metadata, identity and transport headers below the
// MCP 1 MiB and Bus 8 MiB limits. Budget JSON-escaped bytes too, even though
// the native reply body is plain text.
const SNAPSHOT_TEXT_BYTES: usize = 512 * 1024;

pub(crate) struct TextSnapshot {
    cols: usize,
    rows: usize,
    cursor: (usize, usize),
    pid: i32,
    summary: String,
    contents: bool,
    lines: Vec<Vec<char>>,
    truncated: bool,
}

impl TextSnapshot {
    pub(crate) fn render(self) -> String {
        let mut out = format!(
            "cols={} rows={} cursor={},{} child_pid={} truncated={} lines_returned={}\n{}\n",
            self.cols,
            self.rows,
            self.cursor.0,
            self.cursor.1,
            self.pid,
            self.truncated,
            self.lines.len(),
            self.summary
        );
        if self.contents {
            out.push_str("--- screen ---\n");
            for line in self.lines {
                out.extend(line);
                out.push('\n');
            }
        }
        out
    }
}

impl Terminal {
    /// A synchronous VT fixture with no PTY, child process or reader thread.
    /// Frontend tests opt in through their dev-dependency's `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_test_vt(cols: usize, rows: usize, bytes: &[u8]) -> Self {
        let stats = Arc::new(Mutex::new(Metrics::default()));
        let (damage, rx) = mpsc::sync_channel(1);
        let listener = Listener {
            grid: Arc::new(OnceLock::new()),
            damage,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes::default())),
            stats: stats.clone(),
            quit: Arc::new(AtomicBool::new(false)),
            title: Arc::new(Mutex::new(String::new())),
            title_changed: Arc::new(OnceLock::new()),
        };
        let mut grid = Crosswords::new(
            CrosswordsSize::new(cols, rows),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(0),
            0,
            1000,
        );
        rio_vt::performer::handler::Processor::default().advance(&mut grid, bytes);
        let grid = Arc::new(FairMutex::new(grid));
        let _ = listener.grid.set(Arc::downgrade(&grid));
        Self {
            #[cfg(test)]
            before_pty_cleanup: None,
            session: None,
            listener,
            stats,
            grid,
            captured_offset: Mutex::new(0),
            damage: Mutex::new(rx),
            captured_cursor: Mutex::new(None),
            captured_selection: Mutex::new(None),
            pid: 0,
            thread: None,
        }
    }

    pub fn start_session(
        settings: crate::config::Settings,
        native: Option<&crate::native_session::NativeSession>,
        pane_id: u64,
    ) -> Result<Self, String> {
        Self::start_session_in(settings, native, pane_id, None)
    }

    pub(crate) fn start_session_in(
        settings: crate::config::Settings,
        native: Option<&crate::native_session::NativeSession>,
        pane_id: u64,
        cwd: Option<String>,
    ) -> Result<Self, String> {
        if let Some(cwd) = &cwd {
            validate_cwd(cwd)?;
        }
        let explicit_cwd = cwd.is_some();
        Self::start_session_with_launch(
            settings,
            native,
            pane_id,
            LaunchSettings {
                program: MIX_BIN,
                // Explicit cwd must never silently fall back to HOME, even
                // when a directory disappears between validation and chdir.
                home: if explicit_cwd {
                    None
                } else {
                    std::env::var("HOME").ok()
                },
                cwd: cwd.or_else(|| std::env::var("TERM_CWD").ok()),
                environment: Vec::new(),
            },
        )
    }

    /// Test inputs only; every spawn, fd mapping, Machine and exit-notifier
    /// operation below is shared with start_session, not a fixture launcher.
    #[cfg(test)]
    pub(crate) fn start_session_e2e(
        settings: crate::config::Settings,
        native: &crate::native_session::NativeSession,
        program: &str,
        home: String,
        environment: Vec<(String, String)>,
    ) -> Result<Self, String> {
        Self::start_session_scoped_e2e(settings, native, 1, program, home, environment)
    }

    #[cfg(test)]
    pub(crate) fn start_session_scoped_e2e(
        settings: crate::config::Settings,
        native: &crate::native_session::NativeSession,
        pane_id: u64,
        program: &str,
        home: String,
        environment: Vec<(String, String)>,
    ) -> Result<Self, String> {
        Self::start_session_with_launch(
            settings,
            Some(native),
            pane_id,
            LaunchSettings {
                program,
                cwd: Some(home.clone()),
                home: Some(home),
                environment,
            },
        )
    }

    fn start_session_with_launch(
        settings: crate::config::Settings,
        native: Option<&crate::native_session::NativeSession>,
        pane_id: u64,
        launch_settings: LaunchSettings<'_>,
    ) -> Result<Self, String> {
        if std::path::Path::new("/.flatpak-info").exists() {
            return Err("spike requires native session (controlling PTY)".into());
        }
        let stats = Arc::new(Mutex::new(Metrics::default()));
        let (tx, rx) = mpsc::sync_channel(1);
        let listener = Listener {
            grid: Arc::new(OnceLock::new()),
            damage: tx,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes::default())),
            stats: stats.clone(),
            quit: Arc::new(AtomicBool::new(false)),
            title: Arc::new(Mutex::new(String::new())),
            title_changed: Arc::new(OnceLock::new()),
        };
        let grid = Arc::new(FairMutex::new(Crosswords::new(
            CrosswordsSize::new(80, 24),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(0),
            0,
            settings.config.scrollback,
        )));
        // "Open here": a valid TERM_CWD directory is the child shell's working
        // directory (the desktop launcher / `mix --gui` stamps it from the
        // invoking cwd). Absent or invalid, fall back to HOME so a bare
        // desktop launch keeps its historical home-directory default.
        // The pinned PTY API takes String; non-UTF-8 TERM_CWD falls back to HOME.
        let home = launch_settings.home;
        let cwd = launch_directory(launch_settings.cwd, home.clone())?;
        // The PTY API only adds environment entries. env removes TERM_CWD in
        // the child before execing Mix, without mutating our threaded process's
        // environment; later mix --gui launches can stamp their own cwd.
        // env(1) in front of Mix hides a missing/non-executable Mix from the
        // spawn error path: env itself execs fine, then exits 126/127 inside
        // the pty, which would read as a spontaneous shell exit (and notify).
        // Probe the real target up front so startup fails loudly instead; the
        // probe-to-exec race is a broken install mid-launch, not a state this
        // check needs to survive.
        let program = launch_settings.program;
        let mix_bin = std::ffi::CString::new(program).map_err(|e| e.to_string())?;
        // SAFETY: mix_bin is NUL-terminated and alive for this effective-ID check.
        let mix_executable = unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                mix_bin.as_ptr(),
                libc::X_OK,
                libc::AT_EACCESS,
            ) == 0
        };
        if !mix_executable {
            return Err(format!("{program} is not installed or not executable"));
        }
        // Explicit program + argv: native create_pty_with_spawn selects
        // setsid + TIOCSCTTY (Flatpak's non-controlling branch refused above).
        let launch = native.and_then(|native| native.prepare(pane_id));
        let (session, fd) = match launch {
            Some((session, fd)) => (Some(session), Some(fd)),
            None => (None, None),
        };
        let spawn = |dir: String| {
            let mut env = launch_settings.environment.clone();
            env.push(("TERM".into(), settings.term.into()));
            let mut args = vec!["-u".into(), "TERM_CWD".into()];
            // Never propagate a marker inherited by Term itself. The one
            // current launch marker is supplied explicitly after env's unsets.
            args.extend(["-u".into(), crate::session_fd::MARKER.into()]);
            if let Some(fd) = &fd {
                let (name, value) = fd.marker();
                args.push(format!("{name}={value}"));
            }
            args.push(program.into());
            teletypewriter::create_pty_with_spawn_fd(
                Some("/usr/bin/env"),
                args,
                &Some(dir),
                Some(env),
                80,
                24,
                800,
                480,
                fd.as_ref().map(crate::session_fd::LaunchFd::mapping),
            )
        };
        // Search access can change after the probe. A spawn/chdir error gets
        // one HOME retry, rather than reporting completion for an unrun shell.
        let pty = spawn(cwd.clone())
            .or_else(|error| match home {
                Some(home) if home != cwd => spawn(home),
                _ => Err(error),
            })
            .map_err(|e| e.to_string())?;
        // No parent key material or memfd survives the successful spawn.
        drop(fd);
        let pid = *pty.child.pid;
        // INVARIANT: MeteredPty below is the ONLY writer to this PTY, and that is
        // the whole of permit enforcement for agent input. The permit is
        // rechecked there, at the actual write, which is the last point where
        // revoked bytes can still be discarded. Any second write path to this
        // descriptor — a direct write elsewhere, another wrapper, a helper that
        // takes the fd — silently bypasses every check in control.rs. The dup
        // below exists only so the foreground process group can be read; it
        // must never be written to.
        let fd = unsafe { libc::fcntl(*pty.child, libc::F_DUPFD_CLOEXEC, 3) };
        if fd >= 0 {
            let mut writes = listener.writes.lock().unwrap();
            writes.pty = Some(unsafe { OwnedFd::from_raw_fd(fd) });
            Listener::check_foreground(&mut writes);
        }
        let _ = listener.grid.set(Arc::downgrade(&grid));
        let machine = Machine::new(
            grid.clone(),
            MeteredPty {
                pty,
                listener: listener.clone(),
                session_exit: session
                    .as_ref()
                    .map(|pane| Box::new(pane.exit_notifier()) as Box<dyn Fn() + Send + Sync>),
            },
            listener.clone(),
            WindowId::from(0),
            0,
        )
        .map_err(|e| e.to_string())?;
        listener.writes.lock().unwrap().sender = Some(machine.channel());
        // machine.spawn() delegates to std::thread::Builder::spawn().expect(),
        // which panics under thread exhaustion. If it does, the child we just
        // created is dropped together with the Machine — Child::drop sends
        // SIGHUP but never waits — which would leave a zombie. Catch the panic,
        // reap the (now-signalled) child, and surface an error instead.
        let thread =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| machine.spawn())) {
                Ok(thread) => thread,
                Err(_) => {
                    reap_child(pid, Duration::from_millis(1800));
                    return Err("terminal Machine thread failed to start".into());
                }
            };
        listener.dirty();
        Ok(Self {
            #[cfg(test)]
            before_pty_cleanup: None,
            session,
            listener,
            stats,
            grid,
            captured_offset: Mutex::new(0),
            damage: Mutex::new(rx),
            captured_cursor: Mutex::new(None),
            captured_selection: Mutex::new(None),
            pid,
            thread: Some(thread),
        })
    }
    /// Install the change callback. Once only: a second call is ignored
    /// (debug builds assert), so wrap a changing target inside one callback.
    pub fn set_wake(&self, wake: Wake) {
        let installed = self.listener.wake.set(wake).is_ok();
        debug_assert!(
            installed,
            "Terminal::set_wake called twice; the second waker is ignored"
        );
        self.listener.wake();
    }
    pub fn take_damage(&self) -> bool {
        self.damage.lock().unwrap().try_recv().is_ok()
    }
    pub fn screen(&self, consume: bool) -> Screen {
        self.capture(consume, None)
    }
    pub fn scroll_view(&self, request: ScrollRequest) {
        self.scroll_view_state(request);
    }

    /// Move only the viewport and capture offset/history atomically for Bus replies.
    pub(crate) fn scroll_view_state(&self, request: ScrollRequest) -> (usize, usize, bool) {
        use rio_vt::crosswords::grid::Dimensions;
        use rio_vt::crosswords::grid::Scroll;
        let mut term = self.grid.lock();
        // Rio's PageUp/PageDown move by rows, but terminal pages overlap by
        // one row. Delta also preserves that contract at either history edge.
        let page = term.screen_lines().saturating_sub(1) as i32;
        let before = term.display_offset();
        let history = term.grid.history_size();
        // Clamp before entering Rio: its Delta adds in i32 and can overflow
        // for a large positive request while already viewing history.
        let delta = |lines: i64| {
            let target = (before as i64).saturating_add(lines);
            if target <= 0 {
                Scroll::Bottom
            } else if target >= history as i64 {
                Scroll::Top
            } else {
                Scroll::Delta(lines as i32)
            }
        };
        let scroll = match request {
            ScrollRequest::Lines(lines) => delta(lines),
            ScrollRequest::Pages(pages) => delta(pages.saturating_mul(page as i64)),
            ScrollRequest::PageUp => Scroll::Delta(page),
            ScrollRequest::PageDown => Scroll::Delta(-page),
            ScrollRequest::Top => Scroll::Top,
            ScrollRequest::Bottom => Scroll::Bottom,
        };
        term.scroll_display(scroll);
        let offset = term.display_offset();
        drop(term);
        if offset != before {
            self.listener.dirty();
        }
        (offset, history, offset != before)
    }

    /// Human key input follows the live output once bytes reach the queue.
    /// VT replies use Listener directly and must not move the viewport.
    pub fn key(&self, key: Key, at: Instant) -> Result<(), String> {
        self.listener.key(key, at)
    }

    pub fn alternate_screen(&self) -> bool {
        self.grid
            .lock()
            .mode()
            .contains(rio_vt::crosswords::Mode::ALT_SCREEN)
    }

    pub fn display_offset(&self) -> usize {
        self.grid.lock().display_offset()
    }
    /// Like `screen(true)`, and also reports which rows changed since the
    /// previous consuming read (by either method).
    pub fn grid_snapshot(&self) -> GridSnapshot {
        let mut dirty_rows = Vec::new();
        let screen = self.capture(true, Some(&mut dirty_rows));
        GridSnapshot { screen, dirty_rows }
    }
    fn capture(&self, consume: bool, dirty: Option<&mut Vec<bool>>) -> Screen {
        use rio_vt::config::{
            Colors,
            colors::{AnsiColor, term::List},
        };
        let mut term = self.grid.lock();
        let palette = List::from(&Colors::default());
        let colour = |c: AnsiColor| -> [u8; 3] {
            let index = match c {
                AnsiColor::Named(n) => n as usize,
                AnsiColor::Indexed(n) => n as usize,
                AnsiColor::Spec(rgb) => return [rgb.r, rgb.g, rgb.b],
            };
            let rgba = term.colors()[index].unwrap_or(palette[index]);
            [
                (rgba[0] * 255.0) as u8,
                (rgba[1] * 255.0) as u8,
                (rgba[2] * 255.0) as u8,
            ]
        };
        let cols = term.columns();
        let rows = term.screen_lines();
        let mut cells = Vec::with_capacity(cols * rows);
        let offset = term.display_offset();
        let selection = term.selection.as_ref().and_then(|s| s.to_range(&term));
        for y in 0..rows {
            let row = &term.grid[rio_vt::crosswords::pos::Line(y as i32 - offset as i32)];
            for x in 0..cols {
                let square = &row[Column(x)];
                let style = term.grid.style_of(square);
                let mut fg = colour(style.fg);
                let mut bg = colour(style.bg);
                let bold = style.flags.contains(StyleFlags::BOLD);
                if bold {
                    fg = fg.map(|v| v.saturating_add(40));
                }
                if style.flags.contains(StyleFlags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if selection.is_some_and(|range| {
                    let point = rio_vt::crosswords::pos::Pos::new(
                        rio_vt::crosswords::pos::Line(y as i32 - offset as i32),
                        Column(x),
                    );
                    // Rio's contains_square membership, without its block
                    // cursor exception (our painter owns cursor rendering).
                    range.contains(point)
                        || (matches!(square.wide(), rio_vt::crosswords::square::Wide::Wide)
                            && range.contains(rio_vt::crosswords::pos::Pos::new(
                                point.row,
                                point.col + 1,
                            )))
                }) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                cells.push(Cell {
                    c: square.c(),
                    fg,
                    bg,
                    bold,
                });
            }
        }
        let pos = term.grid.cursor.pos;
        let cursor = (
            pos.col.0,
            (pos.row.0.max(0) as usize).saturating_add(offset),
        );
        let cursor_visible =
            cursor.1 < rows && term.mode().contains(rio_vt::crosswords::Mode::SHOW_CURSOR);
        let mut previous = self.captured_cursor.lock().unwrap();
        let mut previous_selection = self.captured_selection.lock().unwrap();
        if let Some(dirty) = dirty {
            *dirty = dirty_rows(&mut term, *self.captured_offset.lock().unwrap());
            // Compare at capture time too: the parser can rotate or clear Rio's
            // selection without going through the frontend's selection methods.
            if *previous_selection != selection {
                for range in previous_selection.iter().chain(selection.iter()) {
                    for (y, row) in dirty.iter_mut().enumerate() {
                        let line = y as i64 - offset as i64;
                        if (i64::from(range.start.row.0)..=i64::from(range.end.row.0))
                            .contains(&line)
                        {
                            *row = true;
                        }
                    }
                }
            }
            if *previous != Some((cursor, cursor_visible)) {
                for ((_, row), visible) in previous
                    .iter()
                    .copied()
                    .chain(std::iter::once((cursor, cursor_visible)))
                {
                    if visible && let Some(row) = dirty.get_mut(row) {
                        *row = true;
                    }
                }
            }
        }
        if consume {
            *previous_selection = selection;
            *previous = Some((cursor, cursor_visible));
            *self.captured_offset.lock().unwrap() = term.display_offset();
            // Both operations must remain under this same grid lock. reset_damage
            // alone does not re-arm Machine's damage notification latch.
            rearm_damage(&mut term);
        }
        let updated = self
            .stats
            .lock()
            .unwrap()
            .vt_updated
            .unwrap_or_else(Instant::now);
        Screen {
            cols,
            rows,
            cursor,
            cursor_visible,
            cells,
            updated,
        }
    }
    pub fn snapshot(&self) -> String {
        self.snapshot_with(true, 0)
    }
    pub fn snapshot_with(&self, contents: bool, scrollback_lines: usize) -> String {
        self.capture_snapshot(contents, scrollback_lines).render()
    }
    pub(crate) fn capture_snapshot(&self, contents: bool, scrollback_lines: usize) -> TextSnapshot {
        use rio_vt::crosswords::{grid::Dimensions, pos::Line};
        // One grid lock makes history and live screen a coherent capture, without
        // moving the scroll offset or consuming the renderer's damage.
        let term = self.grid.lock();
        let cols = term.columns();
        let rows = term.screen_lines();
        let cursor = term.grid.cursor.pos;
        let history = scrollback_lines.min(10000).min(term.grid.history_size());
        let mut lines = Vec::new();
        let mut bytes = 0;
        let mut truncated = false;
        let count = if contents { rows + history } else { 0 };
        // Spend the budget on the newest rows first, preserving the live screen
        // before recent history. Restore oldest-first output after capture.
        for y in (-(history as i32)..rows as i32).rev().take(count) {
            let row = &term.grid[Line(y)];
            let mut line = Vec::new();
            let mut line_bytes = 2; // JSON-escaped newline.
            for x in 0..cols {
                // Rio stores untouched cells as NUL; text snapshots use spaces
                // so blank cells preserve columns without leaking that sentinel.
                let c = row[Column(x)].c();
                let c = if c == '\0' { ' ' } else { c };
                line_bytes += match c {
                    '\\' | '"' => 2,
                    '\u{0000}'..='\u{001f}' => 6,
                    _ => c.len_utf8(),
                };
                if bytes + line_bytes > SNAPSHOT_TEXT_BYTES {
                    truncated = true;
                    break;
                }
                line.push(c);
            }
            if truncated || bytes + line_bytes > SNAPSHOT_TEXT_BYTES {
                truncated = true;
                break;
            }
            bytes += line_bytes;
            lines.push(line);
        }
        drop(term);
        lines.reverse();
        TextSnapshot {
            cols,
            rows,
            cursor: (cursor.col.0, cursor.row.0.max(0) as usize),
            pid: self.pid,
            summary: self.stats.lock().unwrap().summary(),
            contents,
            lines,
            truncated,
        }
    }
    pub fn resize(&self, cols: u16, rows: u16, width: u16, height: u16) {
        self.grid
            .lock()
            .resize(CrosswordsSize::new(cols as usize, rows as usize));
        if let Some(sender) = &self.listener.writes.lock().unwrap().sender
            && let Err(e) = sender.send(Msg::Resize(WindowSize {
                cols,
                rows,
                width,
                height,
            }))
        {
            eprintln!("PTY resize failed: {e}");
        }
        self.stats.lock().unwrap().vt_updated = Some(Instant::now());
        self.listener.dirty();
    }
    pub fn shutdown(&mut self) {
        if let Some(session) = self.session.take() {
            session.revoke_before_cleanup();
        }
        let Some(thread) = self.thread.take() else {
            return;
        };
        #[cfg(test)]
        if let Some(mut probe) = self.before_pty_cleanup.take() {
            probe();
        }
        self.listener.quit.store(true, Ordering::Release);
        if let Some(sender) = self.listener.writes.lock().unwrap().sender.take() {
            let _ = sender.send(Msg::Shutdown);
        }
        let pid = self.pid;
        let (done, completed) = mpsc::sync_channel(1);
        // Join/drop in a reaper thread so a stuck Machine cannot hang UI teardown.
        std::thread::spawn(move || {
            let result = thread.join();
            drop(result); // Drops Machine, master fd, and Child (SIGHUP).
            let deadline = Instant::now() + Duration::from_millis(1800);
            loop {
                let mut status = 0;
                // Only the direct child; WNOHANG keeps close bounded. ECHILD means
                // Machine already reaped it. No process-group kill is introduced.
                let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if rc == pid
                    || (rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                {
                    let _ = done.send(format!(
                        "master dropped; direct child reaped (rc={rc}, status={status})"
                    ));
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = done.send("master dropped; child reap timed out".into());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        eprintln!(
            "DIAGNOSTIC close: {}",
            completed
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| "Machine shutdown timed out".into())
        );
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_vt::corcovado;
    use rio_vt::crosswords::{Mode, grid::Scroll};
    use std::os::{fd::AsRawFd, unix::net::UnixStream};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn explicit_cwd_refuses_a_directory_without_search_permission() {
        use std::os::unix::fs::PermissionsExt;
        // Root can search a mode-000 directory; exercise the effective-user
        // refusal only where the permission restriction actually applies.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "term-c6-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = validate_cwd(path.to_str().unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir(&path).unwrap();
        assert!(
            result
                .unwrap_err()
                .starts_with("invalid-argument: cwd is not searchable")
        );
    }

    #[test]
    fn launch_directory_selects_valid_cwd_or_home() {
        let home = Some("/home/example".to_string());
        assert_eq!(
            launch_directory(Some("/".into()), home.clone()).unwrap(),
            "/"
        );
        for cwd in [
            None,
            Some("".into()),
            Some("/dev/null".into()),
            Some("/dev/null/missing".into()),
        ] {
            assert_eq!(
                launch_directory(cwd, home.clone()).unwrap(),
                "/home/example"
            );
        }
        assert!(launch_directory(None, None).is_err());
    }

    // Headless pollable byte-stream fixture. Machine still owns scheduling,
    // parsing and damage events; this test never launches a GUI or shell.
    struct FixturePty {
        stream: UnixStream,
        token: Token,
    }
    impl ProcessReadWrite for FixturePty {
        type Reader = UnixStream;
        type Writer = UnixStream;
        fn reader(&mut self) -> &mut UnixStream {
            &mut self.stream
        }
        fn writer(&mut self) -> &mut UnixStream {
            &mut self.stream
        }
        fn read_token(&self) -> Token {
            self.token
        }
        fn write_token(&self) -> Token {
            self.token
        }
        fn set_winsize(&mut self, _: WinsizeBuilder) -> io::Result<()> {
            Ok(())
        }
        fn register(
            &mut self,
            poll: &Poll,
            tokens: &mut dyn Iterator<Item = Token>,
            ready: Ready,
            opts: PollOpt,
        ) -> io::Result<()> {
            self.token = tokens.next().unwrap();
            poll.register(
                &corcovado::unix::EventedFd(&self.stream.as_raw_fd()),
                self.token,
                ready,
                opts,
            )
        }
        fn reregister(&mut self, poll: &Poll, ready: Ready, opts: PollOpt) -> io::Result<()> {
            poll.reregister(
                &corcovado::unix::EventedFd(&self.stream.as_raw_fd()),
                self.token,
                ready,
                opts,
            )
        }
        fn deregister(&mut self, poll: &Poll) -> io::Result<()> {
            poll.deregister(&corcovado::unix::EventedFd(&self.stream.as_raw_fd()))
        }
    }
    impl EventedPty for FixturePty {
        fn child_event_token(&self) -> Token {
            Token(usize::MAX)
        }
        fn next_child_event(&mut self) -> Option<ChildEvent> {
            None
        }
    }

    #[test]
    fn machine_output_idle_output_rearms_and_forwards_query_reply() {
        let (stream, mut child) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        child
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (damage, rx) = mpsc::sync_channel(1);
        let listener = Listener {
            grid: Arc::new(OnceLock::new()),
            damage,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes::default())),
            stats: Arc::new(Mutex::new(Metrics::default())),
            quit: Arc::new(AtomicBool::new(false)),
            title: Arc::new(Mutex::new(String::new())),
            title_changed: Arc::new(OnceLock::new()),
        };
        let grid = Arc::new(FairMutex::new(Crosswords::new(
            CrosswordsSize::new(80, 24),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(0),
            0,
            0,
        )));
        let _ = listener.grid.set(Arc::downgrade(&grid));
        let machine = Machine::new(
            grid.clone(),
            FixturePty {
                stream,
                token: Token(1),
            },
            listener.clone(),
            WindowId::from(0),
            0,
        )
        .unwrap();
        let channel = machine.channel();
        listener.writes.lock().unwrap().sender = Some(channel.clone());
        let thread = machine.spawn();
        child.write_all(b"A").unwrap();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("first damage wake");
        {
            let mut term = grid.lock();
            assert_eq!(term.visible_rows()[0][Column(0)].c(), 'A');
            assert!(term.damage_event_in_flight);
            term.reset_damage();
            assert!(
                term.damage_event_in_flight,
                "upstream reset alone does not re-arm"
            );
            rearm_damage(&mut term);
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "idle has no wake"
        );
        child.write_all(b"B\x1b[5n").unwrap();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second damage wake after idle");
        assert_eq!(grid.lock().visible_rows()[0][Column(1)].c(), 'B');
        let mut reply = [0; 4];
        child
            .read_exact(&mut reply)
            .expect("VT DSR reply forwarded to PTY");
        assert_eq!(&reply, b"\x1b[0n");
        channel.send(Msg::Shutdown).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(thread.is_finished(), "Machine shutdown must finish");
        drop(thread.join().unwrap());
    }
    #[test]
    fn machine_delivers_large_paste_before_the_following_key_and_vt_reply() {
        let mut f = GridFixture::new();
        f.child
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        f.feed(b"\x1b[?2004h", |t| t.mode().contains(Mode::BRACKETED_PASTE));
        let text = "p".repeat(1024 * 1024);
        f.terminal.paste(&text).unwrap();
        f.terminal.key(Key::Char('k'), Instant::now()).unwrap();
        // Real parser reply, while the paste is blocked on the socket's
        // small send buffer. Machine must retain one atomic write buffer.
        f.feed(b"\x1b[6nZ", |t| cell(t, 0, 0) == 'Z');
        let expected = format!("\x1b[200~{text}\x1b[201~k\x1b[1;1R");
        let mut received = vec![0; expected.len()];
        f.child.read_exact(&mut received).unwrap();
        assert_eq!(received, expected.as_bytes());
    }

    type FixtureThread = JoinHandle<(Machine<FixturePty, Listener>, rio_vt::performer::State)>;

    /// A real Terminal whose PTY is a socketpair: grid, Machine, Listener and
    /// damage channel are the production ones, and no child process exists
    /// (`thread: None` makes `shutdown` a no-op; Drop stops the Machine).
    struct GridFixture {
        terminal: Terminal,
        child: UnixStream,
        channel: channel::Sender<Msg>,
        thread: Option<FixtureThread>,
        wakes: Arc<AtomicUsize>,
    }

    impl GridFixture {
        fn new() -> Self {
            let (stream, child) = UnixStream::pair().unwrap();
            stream.set_nonblocking(true).unwrap();
            let stats = Arc::new(Mutex::new(Metrics::default()));
            let (damage, rx) = mpsc::sync_channel(1);
            let listener = Listener {
                grid: Arc::new(OnceLock::new()),
                damage,
                wake: Arc::new(OnceLock::new()),
                writes: Arc::new(Mutex::new(Writes::default())),
                stats: stats.clone(),
                quit: Arc::new(AtomicBool::new(false)),
                title: Arc::new(Mutex::new(String::new())),
                title_changed: Arc::new(OnceLock::new()),
            };
            let grid = Arc::new(FairMutex::new(Crosswords::new(
                CrosswordsSize::new(80, 24),
                CursorShape::Block,
                listener.clone(),
                WindowId::from(0),
                0,
                100,
            )));
            let _ = listener.grid.set(Arc::downgrade(&grid));
            let machine = Machine::new(
                grid.clone(),
                FixturePty {
                    stream,
                    token: Token(1),
                },
                listener.clone(),
                WindowId::from(0),
                0,
            )
            .unwrap();
            let channel = machine.channel();
            listener.writes.lock().unwrap().sender = Some(channel.clone());
            let thread = machine.spawn();
            let terminal = Terminal {
                before_pty_cleanup: None,
                session: None,
                listener,
                stats,
                grid,
                captured_offset: Mutex::new(0),
                damage: Mutex::new(rx),
                captured_cursor: Mutex::new(None),
                captured_selection: Mutex::new(None),
                pid: 0,
                thread: None,
            };
            let wakes = Arc::new(AtomicUsize::new(0));
            let counter = wakes.clone();
            terminal.set_wake(Arc::new(move || {
                counter.fetch_add(1, AtomicOrdering::SeqCst);
            }));
            Self {
                terminal,
                child,
                channel,
                thread: Some(thread),
                wakes,
            }
        }

        /// Write, then wait until `parsed` holds. Machine reports damage under
        /// the grid lock, so once the last byte is visible its event is sent.
        fn feed(&mut self, bytes: &[u8], parsed: impl Fn(&Crosswords<Listener>) -> bool) {
            self.child.write_all(bytes).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !parsed(&self.terminal.grid.lock()) {
                assert!(Instant::now() < deadline, "PTY bytes were never parsed");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Consume the pending change, as a frontend does before it reads.
        fn settled_snapshot(&self) -> GridSnapshot {
            let _ = self.terminal.take_damage();
            self.terminal.grid_snapshot()
        }

        /// A snapshot with nothing pending must not schedule another one.
        fn quiet_snapshot(&self) -> GridSnapshot {
            assert!(!self.terminal.take_damage(), "no change was pending");
            let before = self.wakes.load(AtomicOrdering::SeqCst);
            let snapshot = self.terminal.grid_snapshot();
            assert!(
                !self.terminal.take_damage(),
                "a snapshot left a damage token behind"
            );
            assert_eq!(
                self.wakes.load(AtomicOrdering::SeqCst),
                before,
                "a snapshot woke the frontend"
            );
            snapshot
        }
    }

    impl Drop for GridFixture {
        fn drop(&mut self) {
            let _ = self.channel.send(Msg::Shutdown);
            if let Some(thread) = self.thread.take() {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !thread.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if thread.is_finished() {
                    drop(thread.join());
                }
            }
        }
    }

    fn cell(term: &Crosswords<Listener>, row: usize, col: usize) -> char {
        term.visible_rows()[row][Column(col)].c()
    }

    fn all(rows: &[bool]) -> bool {
        rows.iter().all(|row| *row)
    }

    #[test]
    fn grid_snapshot_reports_changed_rows_and_the_cursor() {
        let mut f = GridFixture::new();
        f.feed(b"A", |t| cell(t, 0, 0) == 'A');
        let first = f.settled_snapshot();
        assert_eq!(first.dirty_rows.len(), 24);
        assert!(all(&first.dirty_rows), "a fresh grid is fully dirty");
        assert_eq!(first.screen.cells[0].c, 'A');
        assert_eq!(
            f.quiet_snapshot().dirty_rows.iter().positions(),
            Vec::<usize>::new(),
            "an unchanged cursor does not dirty an idle grid"
        );
        f.feed(b"\r\n\r\nC", |t| cell(t, 2, 0) == 'C');
        assert_eq!(
            f.settled_snapshot().dirty_rows.iter().positions(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn grid_snapshot_is_fully_dirty_after_resize_alt_screen_palette_and_scrollback() {
        let mut f = GridFixture::new();
        f.feed(b"A", |t| cell(t, 0, 0) == 'A');
        f.settled_snapshot();

        f.terminal.resize(100, 30, 800, 600);
        let resized = f.settled_snapshot();
        assert_eq!(resized.dirty_rows.len(), 30);
        assert_eq!(resized.screen.cols, 100);
        assert!(all(&resized.dirty_rows), "resize");
        assert!(f.quiet_snapshot().dirty_rows.iter().positions().is_empty());

        f.feed(b"\x1b[?1049hB", |t| {
            t.mode().contains(Mode::ALT_SCREEN) && cell(t, 0, 1) == 'B'
        });
        assert!(all(&f.settled_snapshot().dirty_rows), "alt-screen entry");
        assert!(f.quiet_snapshot().dirty_rows.iter().positions().is_empty());
        f.feed(b"\x1b[?1049l", |t| !t.mode().contains(Mode::ALT_SCREEN));
        assert!(all(&f.settled_snapshot().dirty_rows), "alt-screen exit");

        f.feed(b"\x1b]4;1;rgb:ff/00/00\x07", |t| {
            t.colors()[1].is_some_and(|c| c[0] == 1.0 && c[1] == 0.0 && c[2] == 0.0)
        });
        assert!(all(&f.settled_snapshot().dirty_rows), "palette change");
        assert!(f.quiet_snapshot().dirty_rows.iter().positions().is_empty());

        let mut lines = b"\r\n".repeat(40);
        lines.push(b'Z');
        f.feed(&lines, |t| cell(t, 29, 0) == 'Z');
        f.settled_snapshot();
        {
            let mut term = f.terminal.grid.lock();
            term.scroll_display(Scroll::Delta(5));
            assert_ne!(term.display_offset(), 0);
        }
        assert!(all(&f.settled_snapshot().dirty_rows), "scroll-back");
        // Still scrolled back: repainted whole, and still no self-wake.
        assert!(all(&f.quiet_snapshot().dirty_rows), "scrolled view");
    }

    #[test]
    fn snapshot_blank_cells_are_spaces_and_preserve_columns() {
        let mut f = GridFixture::new();
        f.feed(b"A\x1b[3CB", |t| cell(t, 0, 4) == 'B');
        let snapshot = f.terminal.snapshot();
        let text = snapshot.split_once("--- screen ---\n").unwrap().1;
        let rows = text.lines().collect::<Vec<_>>();
        assert!(!text.contains('\0'));
        assert_eq!(rows.len(), 24);
        assert!(rows.iter().all(|row| row.chars().count() == 80));
        assert_eq!(rows[0], format!("A   B{}", " ".repeat(75)));
        assert!(rows[1..].iter().all(|row| *row == " ".repeat(80)));
    }

    #[test]
    fn snapshot_byte_budget_bounds_wide_unicode_history_and_releases_grid() {
        let mut f = GridFixture::new();
        f.terminal.resize(3000, 24, 0, 0);
        let mut expected: Vec<String> = (0..120)
            .map(|n| format!("{n:04}{}", "𝐀".repeat(2996)))
            .collect();
        let mut input = format!("{}\r\n", expected.join("\r\n")).into_bytes();
        input.push(b'Z');
        expected.push(format!("Z{}", " ".repeat(2999)));
        f.feed(&input, |t| cell(t, 23, 0) == 'Z');
        f.terminal.scroll_view(ScrollRequest::Top);
        assert!(f.terminal.display_offset() > 0);
        let snapshot = f.terminal.capture_snapshot(true, 10000);
        assert!(snapshot.truncated);
        let returned = snapshot.lines.len();
        assert!(returned > 24 && returned < 121);
        let retained: Vec<String> = snapshot
            .lines
            .iter()
            .map(|row| row.iter().collect())
            .collect();
        assert_eq!(retained, expected[expected.len() - returned..]);
        let encoded_bytes: usize = retained
            .iter()
            .map(|row| serde_json::to_string(row).unwrap().len())
            .sum(); // JSON quotes cost the same two bytes as an escaped newline.
        let omitted = &expected[expected.len() - returned - 1];
        assert!(encoded_bytes <= SNAPSHOT_TEXT_BYTES);
        assert!(
            encoded_bytes + serde_json::to_string(omitted).unwrap().len() > SNAPSHOT_TEXT_BYTES
        );
        // Rendering owns only the captured data; it needs none of these locks.
        let _grid = f.terminal.grid.lock();
        let _stats = f.terminal.stats.lock().unwrap();
        let reply = snapshot.render();
        let text = reply.split_once("--- screen ---\n").unwrap().1;
        assert_eq!(text.lines().count(), returned);
        assert!(text.lines().all(|line| line.chars().count() == 3000));
        assert!(reply.contains(&format!("truncated=true lines_returned={returned}")));
        assert!(serde_json::to_string(&reply).unwrap().len() < 1024 * 1024);
        assert!(reply.len() < SNAPSHOT_TEXT_BYTES + 4096);
    }

    #[test]
    fn titles_strip_controls_and_cap_utf8_at_ingest() {
        assert_eq!(sanitise_title("a\n\r\t\x1b\u{0085}\u{2028}\u{2029}b"), "ab");
        assert_eq!(sanitise_title(&"𝐀".repeat(100)), "𝐀".repeat(64));
        let f = GridFixture::new();
        f.terminal.listener.send_event(
            RioEvent::Title(format!("bad\n{}", "𝐀".repeat(100))),
            WindowId::from(0),
        );
        let title = f.terminal.listener.title();
        assert!(!title.contains('\n'));
        assert!(title.len() <= 256);
    }

    #[test]
    fn snapshot_history_caps_at_available_lines_and_preserves_viewport() {
        use rio_vt::crosswords::grid::Dimensions;
        let mut f = GridFixture::new();
        let mut bytes = Vec::new();
        for n in 0..40 {
            bytes.extend_from_slice(format!("line{n:02}\r\n").as_bytes());
        }
        bytes.push(b'Z');
        f.feed(&bytes, |t| cell(t, 23, 0) == 'Z');
        f.settled_snapshot();
        let lines = |n| {
            f.terminal
                .snapshot_with(true, n)
                .split_once("--- screen ---\n")
                .unwrap()
                .1
                .lines()
                .map(str::trim_end)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let history = f.terminal.grid.lock().grid.history_size();
        assert_eq!(history, 17);
        assert_eq!(lines(0).len(), 24);
        assert_eq!(lines(3).len(), 27);
        assert_eq!(lines(10000).len(), 41);
        assert_eq!(lines(10000)[0], "line00");
        assert_eq!(lines(10000)[40], "Z");
        assert!(
            !f.terminal
                .snapshot_with(false, 10000)
                .contains("--- screen ---")
        );
        f.quiet_snapshot();
        f.terminal.grid.lock().scroll_display(Scroll::Delta(5));
        f.settled_snapshot();
        assert_eq!(lines(10000).len(), 41);
        assert_eq!(lines(10000)[0], "line00");
        assert_eq!(lines(0)[0], "line17");
        assert_eq!(lines(3)[0], "line14");
        assert_eq!(lines(10000)[40], "Z");
        assert_eq!(f.terminal.grid.lock().display_offset(), 5);
        f.quiet_snapshot();
    }

    #[test]
    fn osc_titles_wake_without_taking_the_tab_set_lock() {
        let mut f = GridFixture::new();
        let notify = Arc::new(tokio::sync::Notify::new());
        f.terminal.listener.watch_title(notify.clone());
        f.feed(b"\x1b]2;program title\x07X", |t| cell(t, 0, 0) == 'X');
        assert_eq!(f.terminal.listener.title(), "program title");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), notify.notified())
                .await
                .unwrap();
        });
        f.terminal
            .listener
            .send_event(RioEvent::ResetTitle, WindowId::from(0));
        assert_eq!(f.terminal.listener.title(), "");
    }

    #[test]
    fn insert_mode_snapshot_neither_wakes_nor_leaves_a_token() {
        let mut f = GridFixture::new();
        f.feed(b"\x1b[4hX", |t| {
            t.mode().contains(Mode::INSERT) && cell(t, 0, 0) == 'X'
        });
        assert!(all(&f.settled_snapshot().dirty_rows));
        for _ in 0..3 {
            assert!(all(&f.quiet_snapshot().dirty_rows), "insert mode");
        }
    }

    #[test]
    fn screen_consume_is_seen_by_the_next_snapshot() {
        let mut f = GridFixture::new();
        f.feed(b"A", |t| cell(t, 0, 0) == 'A');
        f.settled_snapshot();
        f.feed(b"\r\n\r\nC", |t| cell(t, 2, 0) == 'C');
        assert!(f.terminal.take_damage());
        let _ = f.terminal.screen(true);
        // screen(true) consumed both row damage and cursor movement.
        assert!(f.quiet_snapshot().dirty_rows.iter().positions().is_empty());
    }

    trait Positions {
        fn positions(self) -> Vec<usize>;
    }
    impl<'a, I: Iterator<Item = &'a bool>> Positions for I {
        fn positions(self) -> Vec<usize> {
            self.enumerate()
                .filter_map(|(row, dirty)| dirty.then_some(row))
                .collect()
        }
    }
    #[test]
    fn diagnostic_and_keyboard_share_encoding() {
        assert_eq!(
            encode_text("a\n\t\u{3}\u{4}\u{7f}").unwrap(),
            [97, 13, 9, 3, 4, 127]
        );
        assert_eq!(encode(Key::Up), b"\x1b[A");
        assert!(encode_text("aé").is_err());
    }
    #[test]
    fn basic_shell_encodings() {
        for (key, bytes) in [
            (Key::Escape, &b"\x1b"[..]),
            (Key::Home, &b"\x1b[H"[..]),
            (Key::End, &b"\x1b[F"[..]),
            (Key::Delete, &b"\x1b[3~"[..]),
            (Key::PageUp, &b"\x1b[5~"[..]),
            (Key::PageDown, &b"\x1b[6~"[..]),
        ] {
            assert_eq!(encode(key), bytes);
        }
        for c in b'a'..=b'z' {
            assert_eq!(encode(Key::Control(c as char)), [c - b'a' + 1]);
            assert_eq!(
                encode(Key::Control((c as char).to_ascii_uppercase())),
                [c - b'a' + 1]
            );
        }
        assert!(encode(Key::Control('1')).is_empty());
    }
}
