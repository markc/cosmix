//! Terminal modes and screen output only. Foreground PGIDs belong to Controller.
use super::render::{Layout, Position, Source};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// Process-local diagnostic record, even when the tty cannot accept a warning.
pub static OUTPUT_TEARS: AtomicUsize = AtomicUsize::new(0);

pub struct Terminal {
    // The controller separately records job modes. The ordering contract is
    // editor restore before line return, controller foreground before re-entry.
    input: File,
    output: File,
    saved: Option<libc::termios>,
    protocols: bool,
    cursor_row: usize,
    pending: Vec<u8>,
    written: usize,
    dirty: bool,
}

impl Terminal {
    pub fn new(input: File, output: File) -> io::Result<Self> {
        // Reopen, do not dup/setfl: dup shares O_NONBLOCK with evaluator/child
        // stdout. This description belongs exclusively to the editor writer.
        let output = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", output.as_raw_fd()))?;
        Ok(Self {
            input,
            output,
            saved: None,
            protocols: false,
            cursor_row: 0,
            pending: Vec::new(),
            written: 0,
            dirty: false,
        })
    }
    pub fn fd(&self) -> RawFd {
        self.input.as_raw_fd()
    }
    pub fn enter(&mut self) -> io::Result<()> {
        self.require_foreground()?;
        if self.saved.is_some() {
            return Err(io::Error::other("editor already raw"));
        }
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(self.fd(), &mut saved) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // TCSANOW deliberately preserves pending typeahead.
        if unsafe { libc::tcsetattr(self.fd(), libc::TCSANOW, &raw) } < 0 {
            let error = io::Error::last_os_error();
            self.require_foreground()?;
            return Err(error);
        }
        self.saved = Some(saved);
        self.protocols = true;
        if let Err(error) = self.queue(b"\x1b[?2004h") {
            let _ = self.restore();
            return Err(error);
        }
        Ok(())
    }
    pub fn restore(&mut self) -> io::Result<()> {
        // Cooked modes are the handoff guarantee and precede output cleanup.
        // One shared 250 ms deadline bounds the pending
        // drain AND protocol cleanup: acknowledgements never wait indefinitely.
        if let Some(saved) = self.saved {
            self.require_foreground()?;
            if unsafe { libc::tcsetattr(self.fd(), libc::TCSANOW, &saved) } < 0 {
                return Err(io::Error::last_os_error());
            }
            self.saved = None;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut complete = true;
        while self.written < self.pending.len() {
            if !self.drain_once(deadline) {
                complete = false;
                break;
            }
        }
        self.pending.clear();
        self.written = 0;
        self.dirty = false;
        if self.protocols {
            self.pending.extend_from_slice(b"\x1b[?2004l\x1b[0m");
            while self.written < self.pending.len() {
                if !self.drain_once(deadline) {
                    complete = false;
                    break;
                }
            }
        }
        // On expiry/error we accept a torn escape sequence: the emulator may
        // need a reset. Record it without attempting a potentially blocked log.
        if !complete {
            OUTPUT_TEARS.fetch_add(1, Ordering::Relaxed);
        }
        self.pending.clear();
        self.written = 0;
        self.protocols = false;
        Ok(())
    }
    fn drain_once(&mut self, deadline: Instant) -> bool {
        if !self.foreground() || Instant::now() >= deadline {
            return false;
        }
        match self.output.write(&self.pending[self.written..]) {
            Ok(0) => return false,
            Ok(n) => {
                self.written += n;
                return true;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
        let mut fd = libc::pollfd {
            fd: self.output.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        if millis == 0 {
            return false;
        }
        unsafe { libc::poll(&mut fd, 1, millis.min(250) as i32) };
        Instant::now() < deadline
    }
    pub fn foreground(&self) -> bool {
        unsafe { libc::tcgetpgrp(self.fd()) == libc::getpgrp() }
    }
    fn require_foreground(&self) -> io::Result<()> {
        if self.foreground() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "editor is not foreground",
            ))
        }
    }
    pub fn output_fd(&self) -> Option<RawFd> {
        (self.written < self.pending.len() && self.foreground()).then(|| self.output.as_raw_fd())
    }
    #[cfg(test)]
    pub fn output_progress(&self) -> (usize, usize) {
        (self.written, self.pending.len())
    }
    pub fn flush_ready(&mut self) -> io::Result<bool> {
        // One bounded write per poll iteration; controls/signals run between retries.
        if self.written < self.pending.len() && self.foreground() {
            let end = (self.written + 16 * 1024).min(self.pending.len());
            match self.output.write(&self.pending[self.written..end]) {
                Ok(n) => self.written += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        if self.written == self.pending.len() {
            self.pending.clear();
            self.written = 0;
            return Ok(std::mem::take(&mut self.dirty));
        }
        Ok(false)
    }
    fn queue(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.pending.len() + bytes.len() > 2 * 1024 * 1024 {
            return Err(io::Error::other("editor output bound"));
        }
        self.pending.extend_from_slice(bytes);
        self.flush_ready()?;
        Ok(())
    }
    pub fn size(&self) -> (usize, usize) {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd(), libc::TIOCGWINSZ, &mut size) } < 0 {
            return (80, 24);
        }
        (
            usize::from(size.ws_col).clamp(1, 1000),
            usize::from(size.ws_row).clamp(1, 500),
        )
    }
    pub fn fresh_line(&mut self) -> io::Result<()> {
        self.queue(b"\r\n")?;
        self.cursor_row = 0;
        Ok(())
    }
    pub fn bell(&mut self) -> io::Result<()> {
        self.queue(b"\x07")
    }
    /// Write one line and DRAIN it before returning, unlike every other writer
    /// here. The admission echo has to be on the glass before the evaluation it
    /// announces produces any output of its own, and the evaluator writes
    /// straight to the shared stdout — it does not go through this buffer. A
    /// queued echo would race that output and could surface after it.
    ///
    /// Called only from the admission path, with modes already restored and the
    /// editor idle, so the bounded wait below is for tty flow control alone.
    ///
    /// `drain` is supplied by the caller and must fit INSIDE the budget that
    /// caller is itself being held to. A fixed deadline longer than the
    /// admission budget is what let a timed-out admission keep writing to the
    /// pane after its caller had been answered.
    pub fn echo(&mut self, text: &str, drain: Duration) -> io::Result<()> {
        // ONE queue call, so a failure part-way cannot leave the announcement
        // split across two writes with only the first on screen.
        let mut line = String::with_capacity(text.len() + 2);
        line.push_str(text);
        line.push_str("\r\n");
        self.queue(line.as_bytes())?;
        self.cursor_row = 0;
        let deadline = Instant::now() + drain;
        while self.written < self.pending.len() {
            let failure = if !self.foreground() {
                // Writing would raise SIGTTOU against a shell that no longer
                // owns the terminal. Refusing is correct: the caller turns this
                // into a refusal and nothing executes unannounced.
                Some("editor lost the terminal before the echo")
            } else if Instant::now() >= deadline {
                OUTPUT_TEARS.fetch_add(1, Ordering::Relaxed);
                Some("echo could not be flushed")
            } else {
                None
            };
            if let Some(failure) = failure {
                // An announcement is zero-or-whole. A partial one reads exactly
                // like a real admission, so if the whole line could not be put
                // on the glass, say on the glass that it was abandoned — and
                // never let the absence of the rest be mistaken for silence.
                self.abort_echo();
                return Err(io::Error::other(failure));
            }
            self.flush_ready()?;
            if self.written < self.pending.len() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        // flush_ready consumed the dirty bit above; the caller is not drawing.
        self.dirty = false;
        Ok(())
    }
    /// Best effort by construction: the write that failed is the reason this is
    /// needed, so it may fail too. Dropping whatever is still queued first is
    /// what stops the abandoned announcement from arriving later anyway.
    fn abort_echo(&mut self) {
        self.pending.truncate(self.written);
        let notice = b"\r\nmix: execute: announcement abandoned; nothing executed\r\n";
        let _ = self.output.write(notice);
        self.pending.clear();
        self.written = 0;
        self.cursor_row = 0;
    }
    pub fn notice(&mut self, layout: &Layout, message: &str) -> io::Result<()> {
        self.finish(layout)?;
        self.queue(message.as_bytes())?;
        self.fresh_line()
    }
    pub fn finish(&mut self, layout: &Layout) -> io::Result<()> {
        let mut bytes = String::from("\r");
        // Finish below the visible tail; hidden logical rows must not scroll.
        let (_, height) = self.size();
        let top = layout.cursor.row.saturating_sub(height - 1);
        let bottom = layout.rows.min(top + height);
        for _ in layout.cursor.row..layout.end.row.min(bottom.saturating_sub(1)) {
            bytes.push('\n');
        }
        bytes.push_str("\r\n");
        self.queue(bytes.as_bytes())?;
        self.cursor_row = 0;
        Ok(())
    }
    pub fn draw(&mut self, layout: &Layout, prompt: &str) -> io::Result<()> {
        if self.output_fd().is_some() {
            self.dirty = true;
            return Ok(());
        }
        let (_, height) = self.size();
        // Keep the logical cursor visible for buffers taller than the tty.
        let top = layout.cursor.row.saturating_sub(height - 1);
        let bottom = layout.rows.min(top + height);
        let mut bytes = String::from("\r");
        if self.cursor_row > 0 {
            bytes.push_str(&format!("\x1b[{}A", self.cursor_row.min(height - 1)));
        }
        bytes.push_str("\x1b[J");
        let (_, styles) = super::render::prompt_parts(prompt);
        let mut styles = styles.into_iter().peekable();
        let mut prompt_done = false;
        let mut position = Position {
            row: top,
            column: 0,
        };
        for run in layout
            .runs
            .iter()
            .filter(|r| r.position.row >= top && r.position.row < bottom)
        {
            if let Source::Prompt(range) = &run.source {
                while styles.peek().is_some_and(|(at, _)| *at <= range.start) {
                    bytes.push_str(&styles.next().unwrap().1);
                }
            } else if !prompt_done {
                // End-of-prompt reset must precede buffer text.
                for (_, style) in styles.by_ref() {
                    bytes.push_str(&style);
                }
                bytes.push_str("\x1b[0m");
                prompt_done = true;
            }
            while position.row < run.position.row {
                bytes.push_str("\r\n");
                position.row += 1;
                position.column = 0;
            }
            if position.column < run.position.column {
                bytes.push_str(&" ".repeat(run.position.column - position.column));
            }
            bytes.push_str(&run.text);
            position.column = run.position.column + run.width;
        }
        while position.row + 1 < bottom {
            bytes.push_str("\r\n");
            position.row += 1;
        }
        bytes.push_str("\x1b[0m");
        // CR cancels pending autowrap before positioning, including exact fits.
        bytes.push('\r');
        if position.row > layout.cursor.row {
            bytes.push_str(&format!("\x1b[{}A", position.row - layout.cursor.row));
        }
        if layout.cursor.column > 0 {
            bytes.push_str(&format!("\x1b[{}C", layout.cursor.column));
        }
        self.queue(bytes.as_bytes())?;
        self.cursor_row = layout.cursor.row - top;
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn mode_entry_requires_foreground_ownership() {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let input = unsafe { File::from_raw_fd(slave) };
        let mut original = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
        let mut terminal =
            Terminal::new(input.try_clone().unwrap(), input.try_clone().unwrap()).unwrap();
        assert!(terminal.enter().is_err());
        terminal.queue(b"pending").unwrap();
        assert_eq!(terminal.output_progress(), (0, 7));
        assert!(
            terminal.output_fd().is_none(),
            "background POLLOUT must not spin"
        );
        let mut restored: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut restored) }, 0);
        assert_eq!(restored.c_lflag, original.c_lflag);
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cc, original.c_cc);
        terminal.restore().unwrap();
    }
}
