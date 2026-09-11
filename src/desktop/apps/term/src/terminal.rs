use crate::metrics::Metrics;
use rio_vt::{
    ansi::CursorShape,
    corcovado::{Poll, PollOpt, Ready, Token, channel},
    crosswords::{Crosswords, CrosswordsSize, pos::Column, style::StyleFlags},
    event::{EventListener, Msg, RioEvent, WindowId, WindowSize, sync::FairMutex},
    performer::Machine,
    teletypewriter::{self, ChildEvent, EventedPty, ProcessReadWrite, WinsizeBuilder},
};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{self, Read, Write},
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

struct Pending {
    remaining: usize,
    key: Option<Instant>,
}
#[derive(Default)]
struct Writes {
    sender: Option<channel::Sender<Msg>>,
    pending: VecDeque<Pending>,
    bytes: usize,
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
        });
        Ok(())
    }
    pub fn type_text(&self, text: &str) -> Result<(), String> {
        self.write(encode_text(text)?, Some(Instant::now()))
    }
    pub fn key(&self, key: Key, at: Instant) -> Result<(), String> {
        self.write(encode(key), Some(at))
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
            other => eprintln!("term-spike unsupported VT event, dropped: {other:?}"),
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
        let n = self.pty.write(bytes)?;
        let mut writes = self.listener.writes.lock().unwrap();
        let mut stats = self.listener.stats.lock().unwrap();
        stats.bytes_written += n as u64;
        let mut left = n;
        while left > 0 {
            let Some(pending) = writes.pending.front_mut() else {
                break;
            };
            let consumed = left.min(pending.remaining);
            if let Some(at) = pending.key {
                stats.input_written += consumed as u64;
                stats.key_write.add(at.elapsed());
            } else {
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
pub struct Terminal {
    #[cfg(test)]
    pub before_pty_cleanup: Option<Box<dyn FnMut() + Send>>,
    session: Option<crate::native_session::PaneSession>,
    pub listener: Listener,
    pub stats: Stats,
    grid: Grid,
    damage: Mutex<Receiver<()>>,
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
        Self::start_session_with_launch(
            settings,
            Some(native),
            1,
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
            pid,
            thread: Some(thread),
        })
    }
    pub fn set_wake(&self, wake: Wake) {
        let _ = self.listener.wake.set(wake);
        self.listener.wake();
    }
    pub fn take_damage(&self) -> bool {
        self.damage.lock().unwrap().try_recv().is_ok()
    }
    pub fn screen(&self, consume: bool) -> Screen {
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
        if consume {
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
    use std::os::{fd::AsRawFd, unix::net::UnixStream};

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
