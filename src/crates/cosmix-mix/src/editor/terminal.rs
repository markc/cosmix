//! Terminal modes and screen output only. Foreground PGIDs belong to Controller.
use super::render::{Layout, Position, Source};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};

pub struct Terminal {
    input: File,
    output: File,
    saved: Option<libc::termios>,
    protocols: bool,
    cursor_row: usize,
}

impl Terminal {
    pub fn new(input: File, output: File) -> Self {
        Self {
            input,
            output,
            saved: None,
            protocols: false,
            cursor_row: 0,
        }
    }
    pub fn fd(&self) -> RawFd {
        self.input.as_raw_fd()
    }
    pub fn enter(&mut self) -> io::Result<()> {
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
        if let Err(error) = self.output.write_all(b"\x1b[?2004h") {
            let _ = self.restore();
            return Err(error);
        }
        Ok(())
    }
    pub fn restore(&mut self) -> io::Result<()> {
        // Try both restorations even if output fails. Never acknowledge success
        // unless both operations succeeded; Drop can retry failed operations.
        let protocol_result = if self.protocols {
            self.output
                .write_all(b"\x1b[?2004l\x1b[0m")
                .map(|()| self.protocols = false)
        } else {
            Ok(())
        };
        let mode_result = if let Some(saved) = self.saved {
            if unsafe { libc::tcsetattr(self.fd(), libc::TCSANOW, &saved) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                self.saved = None;
                Ok(())
            }
        } else {
            Ok(())
        };
        mode_result.and(protocol_result)
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
        self.output.write_all(b"\r\n")?;
        self.cursor_row = 0;
        Ok(())
    }
    pub fn bell(&mut self) -> io::Result<()> {
        self.output.write_all(b"\x07")
    }
    pub fn draw(&mut self, layout: &Layout, prompt: &str) -> io::Result<()> {
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
        self.output.write_all(bytes.as_bytes())?;
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
    fn failed_protocol_restore_still_restores_modes_and_can_retry() {
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
        let mut terminal = Terminal::new(input.try_clone().unwrap(), input.try_clone().unwrap());
        terminal.enter().unwrap();
        let output = std::mem::replace(&mut terminal.output, File::open("/dev/null").unwrap());
        assert!(terminal.restore().is_err());
        let mut restored: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut restored) }, 0);
        assert_eq!(restored.c_lflag, original.c_lflag);
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cc, original.c_cc);
        terminal.output = output;
        terminal.restore().unwrap();
        terminal.enter().unwrap();
        let input = std::mem::replace(&mut terminal.input, File::open("/dev/null").unwrap());
        assert!(terminal.restore().is_err());
        assert!(
            terminal.saved.is_some(),
            "failed tcsetattr must remain retryable"
        );
        terminal.input = input;
        terminal.restore().unwrap();
    }
}
