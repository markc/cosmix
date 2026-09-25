use crate::metrics::Metrics;
#[path = "mouse.rs"]
mod mouse;
pub use mouse::MouseModifiers;
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
fn dirty_rows(term: &mut Crosswords<Listener>) -> Vec<bool> {
    let mut dirty = vec![false; term.screen_lines()];
    if term.display_offset() != 0 || term.mode().contains(rio_vt::crosswords::Mode::INSERT) {
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
}
#[derive(Default)]
struct Writes {
    #[cfg(test)]
    block_control: bool,
    pty: Option<OwnedFd>,
    group: i32,
    sender: Option<channel::Sender<Msg>>,
    pending: VecDeque<Pending>,
    bytes: usize,
    foreground: u64,
    owner: Option<(String, Arc<crate::control::Permit>)>,
}

#[derive(Clone)]
pub struct Listener {
    damage: SyncSender<()>,
    wake: Arc<OnceLock<Wake>>,
    writes: Arc<Mutex<Writes>>,
    stats: Stats,
    pub quit: Arc<AtomicBool>,
}
impl Listener {
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
        self.enqueue(&mut writes, bytes, key, None)
    }
    fn enqueue(
        &self,
        writes: &mut Writes,
        bytes: Vec<u8>,
        key: Option<Instant>,
        permit: Option<Arc<crate::control::Permit>>,
    ) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.quit.load(Ordering::Acquire) {
            return Err("terminal closing".into());
        }
        if bytes.len() + writes.bytes > 65536 {
            return Err("PTY input queue full".into());
        }
        let sender = writes.sender.as_ref().ok_or("PTY unavailable")?.clone();
        let len = bytes.len();
        // Serialise enqueue and accounting with actual writes, including VT replies.
        sender
            .send(Msg::Input(Cow::Owned(bytes)))
            .map_err(|e| e.to_string())?;
        writes.bytes += len;
        writes.pending.push_back(Pending {
            remaining: len,
            key,
            permit,
        });
        Ok(())
    }
    // Synthetic keys assert foreground authority exactly like real ones:
    // revoke any delegated control writer before enqueuing, mirroring key().
    // The empty-bytes early return is deliberately BEFORE the revoke, unlike
    // key() which revokes unconditionally: a no-op write must not invalidate
    // a live control writer.
    pub fn type_text(&self, text: &str) -> Result<(), String> {
        let bytes = encode_text(text)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writes = self.writes.lock().unwrap();
        Self::revoke_writer(&mut writes);
        self.enqueue(&mut writes, bytes, Some(Instant::now()), None)
    }
    pub fn key(&self, key: Key, at: Instant) -> Result<(), String> {
        let mut writes = self.writes.lock().unwrap();
        Self::revoke_writer(&mut writes);
        self.enqueue(&mut writes, encode(key), Some(at), None)
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
        )
        .map_err(|_| "RESOURCE_LIMIT")?;
        writes.owner = Some((actor.into(), permit));
        Ok(())
    }
}
impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _: WindowId) {
        match event {
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
            if pending.remaining == 0 {
                writes.pending.pop_front();
            }
            writes.bytes -= consumed;
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
    /// previous consuming read. All true after a resize, a scroll-back, a
    /// full-screen mode change or on the first read.
    pub dirty_rows: Vec<bool>,
}
pub struct Terminal {
    #[cfg(test)]
    pub before_pty_cleanup: Option<Box<dyn FnMut() + Send>>,
    session: Option<crate::native_session::PaneSession>,
    pub listener: Listener,
    pub stats: Stats,
    grid: Grid,
    damage: Mutex<Receiver<()>>,
    captured_cursor: Mutex<Option<((usize, usize), bool)>>,
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

impl Terminal {
    pub fn start_session(
        settings: crate::config::Settings,
        native: Option<&crate::native_session::NativeSession>,
        pane_id: u64,
    ) -> Result<Self, String> {
        Self::start_session_with_launch(
            settings,
            native,
            pane_id,
            LaunchSettings {
                program: MIX_BIN,
                home: std::env::var("HOME").ok(),
                cwd: std::env::var("TERM_CWD").ok(),
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
            damage: tx,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes::default())),
            stats: stats.clone(),
            quit: Arc::new(AtomicBool::new(false)),
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
            damage: Mutex::new(rx),
            captured_cursor: Mutex::new(None),
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
        for row in term.visible_rows() {
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
                cells.push(Cell {
                    c: square.c(),
                    fg,
                    bg,
                    bold,
                });
            }
        }
        let pos = term.grid.cursor.pos;
        let cursor = (pos.col.0, pos.row.0.max(0) as usize);
        let cursor_visible = term.mode().contains(rio_vt::crosswords::Mode::SHOW_CURSOR);
        let mut previous = self.captured_cursor.lock().unwrap();
        if let Some(dirty) = dirty {
            *dirty = dirty_rows(&mut term);
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
            *previous = Some((cursor, cursor_visible));
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
        let s = self.screen(false);
        let mut out = format!(
            "cols={} rows={} cursor={},{} child_pid={}\n{}\n--- screen ---\n",
            s.cols,
            s.rows,
            s.cursor.0,
            s.cursor.1,
            self.pid,
            self.stats.lock().unwrap().summary()
        );
        for row in s.cells.chunks(s.cols) {
            for cell in row {
                out.push(cell.c);
            }
            out.push('\n');
        }
        out
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
            damage,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes::default())),
            stats: Arc::new(Mutex::new(Metrics::default())),
            quit: Arc::new(AtomicBool::new(false)),
        };
        let grid = Arc::new(FairMutex::new(Crosswords::new(
            CrosswordsSize::new(80, 24),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(0),
            0,
            0,
        )));
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
                damage,
                wake: Arc::new(OnceLock::new()),
                writes: Arc::new(Mutex::new(Writes::default())),
                stats: stats.clone(),
                quit: Arc::new(AtomicBool::new(false)),
            };
            let grid = Arc::new(FairMutex::new(Crosswords::new(
                CrosswordsSize::new(80, 24),
                CursorShape::Block,
                listener.clone(),
                WindowId::from(0),
                0,
                100,
            )));
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
                damage: Mutex::new(rx),
                captured_cursor: Mutex::new(None),
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
