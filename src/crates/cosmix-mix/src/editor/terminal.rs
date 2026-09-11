//! Terminal modes and screen output only. Foreground PGIDs belong to Controller.
use super::render::{Layout, Position, Source};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

pub struct Terminal {
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
            return Err(io::Error::last_os_error());
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
        // Cooked modes are the handoff guarantee. Output backpressure must
        // never gate restoration or acknowledgement; protocol cleanup is best effort.
        if let Some(saved) = self.saved {
            self.require_foreground()?;
            if unsafe { libc::tcsetattr(self.fd(), libc::TCSANOW, &saved) } < 0 {
                return Err(io::Error::last_os_error());
            }
            self.saved = None;
        }
        self.pending.clear();
        self.written = 0;
        self.dirty = false;
        if self.protocols && self.foreground() {
            let _ = self.output.write(b"\x1b[?2004l\x1b[0m");
        }
        self.protocols = false;
        Ok(())
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
        (self.written < self.pending.len()).then(|| self.output.as_raw_fd())
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
    pub fn finish(&mut self, layout: &Layout) -> io::Result<()> {
        let mut bytes = String::from("\r");
        // LF scrolls when necessary; finish below the logical tail, not the cursor.
        for _ in layout.cursor.row..layout.end.row {
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
        let mut restored: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut restored) }, 0);
        assert_eq!(restored.c_lflag, original.c_lflag);
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cc, original.c_cc);
        terminal.restore().unwrap();
    }
}
