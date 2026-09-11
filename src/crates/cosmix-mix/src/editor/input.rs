//! Incremental, bounded terminal decoding. No tty reads outside `wait`/`read`.
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

const ESC_DELAY: Duration = Duration::from_millis(40);
const MAX_SEQUENCE: usize = 64;
const MAX_PASTE: usize = 64 * 1024;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Text(String),
    Paste(String),
    PasteStart,
    PasteOverflow,
    Redo,
    YankPop,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    WordLeft,
    WordRight,
    Control(u8),
    Invalid,
}

#[derive(Default, Debug, Clone)]
pub struct Decoder {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
    paste_tail: Vec<u8>,
    overflow: bool,
    escape_since: Option<Instant>,
}

impl Decoder {
    pub fn pending(&self) -> bool {
        // A trailing partial UTF-8 scalar keeps admission Busy until completed;
        // this is the deliberate cost of having only an escape timer.
        !self.pending.is_empty()
    }
    pub fn pasting(&self) -> bool {
        self.paste.is_some()
    }
    pub fn resume(&mut self) {
        if self.escape_since.is_some() {
            self.escape_since = Some(Instant::now());
        }
    }
    pub fn timeout(&self) -> i32 {
        self.escape_since.map_or(-1, |start| {
            let left = ESC_DELAY.saturating_sub(start.elapsed());
            left.as_millis()
                .saturating_add(u128::from(!left.is_zero()))
                .min(i32::MAX as u128) as i32
        })
    }
    pub fn expire(&mut self) -> Option<Key> {
        if self.escape_since.is_some_and(|t| t.elapsed() >= ESC_DELAY) {
            let key = if self.pending == b"\x1b" {
                Key::Escape
            } else {
                Key::Invalid
            };
            self.pending.clear();
            self.escape_since = None;
            Some(key)
        } else {
            None
        }
    }
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        if let Some(paste) = self.paste.as_mut() {
            self.paste_tail.push(byte);
            while !PASTE_END.starts_with(&self.paste_tail) {
                let first = self.paste_tail.remove(0);
                if paste.len() < MAX_PASTE {
                    paste.push(first);
                } else {
                    self.overflow = true;
                }
            }
            if self.paste_tail == PASTE_END {
                self.paste_tail.clear();
                let bytes = self.paste.take().unwrap();
                return Some(if std::mem::take(&mut self.overflow) {
                    Key::PasteOverflow
                } else {
                    Key::Paste(
                        String::from_utf8_lossy(&bytes)
                            .replace("\r\n", "\n")
                            .replace('\r', "\n"),
                    )
                });
            }
            return None;
        }
        if self.pending.is_empty() {
            match byte {
                0x1b => {
                    self.pending.push(byte);
                    self.escape_since = Some(Instant::now());
                    return None;
                }
                0..=31 | 127 => return Some(Key::Control(byte)),
                _ => {}
            }
        }
        self.pending.push(byte);
        if self.pending[0] == 0x1b {
            if self.pending.len() == 2 && matches!(byte, b'[' | b'O') {
                return None;
            }
            if self.pending.len() > 2
                && !(0x40..=0x7e).contains(&byte)
                && self.pending.len() < MAX_SEQUENCE
            {
                return None;
            }
            let key = match self.pending.as_slice() {
                b"\x1b[A" | b"\x1bOA" => Key::Up,
                b"\x1b[B" | b"\x1bOB" => Key::Down,
                b"\x1b[C" | b"\x1bOC" => Key::Right,
                b"\x1b[D" | b"\x1bOD" => Key::Left,
                b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => Key::Home,
                b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => Key::End,
                b"\x1b[3~" => Key::Delete,
                b"\x1bb" | b"\x1b[1;5D" => Key::WordLeft,
                b"\x1bf" | b"\x1b[1;5C" => Key::WordRight,
                b"\x1br" | b"\x1b_" => Key::Redo,
                b"\x1by" => Key::YankPop,
                b"\x1b[200~" => {
                    self.paste = Some(Vec::new());
                    Key::PasteStart
                }
                _ => Key::Invalid,
            };
            self.pending.clear();
            self.escape_since = None;
            return Some(key);
        }
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let key = Key::Text(s.to_owned());
                self.pending.clear();
                Some(key)
            }
            Err(e) if e.error_len().is_none() && self.pending.len() < 4 => None,
            Err(_) => {
                self.pending.clear();
                Some(Key::Invalid)
            }
        }
    }
}

/// A single indefinite poll, except while disambiguating an escape sequence.
/// Suspended/idle owners pass no tty fd: even HUP cannot cause a tty read.
pub fn wait(
    tty: Option<RawFd>,
    control: RawFd,
    signal: RawFd,
    output: Option<RawFd>,
    timeout: i32,
) -> io::Result<[bool; 4]> {
    let mut fds =
        [tty.unwrap_or(-1), control, signal, output.unwrap_or(-1)].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    fds[3].events = libc::POLLOUT;
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
    if rc < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok([false; 4]);
        }
        return Err(error);
    }
    Ok(fds.map(|fd| fd.revents != 0))
}

/// Read exactly one byte so accepting Enter never prefetches child input.
pub fn read(fd: RawFd) -> io::Result<Option<u8>> {
    let mut byte = 0;
    match unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) } {
        1 => Ok(Some(byte)),
        0 => Ok(None),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(d: &mut Decoder, bytes: &[u8]) -> Vec<Key> {
        bytes.iter().filter_map(|b| d.feed(*b)).collect()
    }
    #[test]
    fn sequences_and_split_utf8() {
        let mut d = Decoder::default();
        assert_eq!(
            decode(&mut d, b"\x1b[A\x1b[3~\x1b[1;5D"),
            vec![Key::Up, Key::Delete, Key::WordLeft]
        );
        for ch in ["界", "👩", "\u{301}"] {
            let bytes = ch.as_bytes();
            assert!(decode(&mut d, &bytes[..bytes.len() - 1]).is_empty());
            assert!(d.pending());
            d.resume();
            assert_eq!(d.feed(bytes[bytes.len() - 1]), Some(Key::Text(ch.into())));
        }
    }
    #[test]
    fn paste_survives_pause_and_never_executes_controls() {
        let mut d = Decoder::default();
        assert_eq!(
            decode(&mut d, b"\x1b[200~a\r\n\x03\x1b[20"),
            vec![Key::PasteStart]
        );
        assert!(d.pasting());
        assert_eq!(d.timeout(), -1);
        d.resume();
        assert_eq!(decode(&mut d, b"1~"), vec![Key::Paste("a\n\x03".into())]);
    }
    #[test]
    fn bounds_and_escape_timeout() {
        let mut d = Decoder::default();
        decode(&mut d, b"\x1b[200~");
        decode(&mut d, &vec![b'x'; MAX_PASTE + 1]);
        assert_eq!(decode(&mut d, PASTE_END), vec![Key::PasteOverflow]);
        d.feed(27);
        d.escape_since = Some(Instant::now() - ESC_DELAY);
        assert_eq!(d.expire(), Some(Key::Escape));
        assert_eq!(d.timeout(), -1);
        assert_eq!(d.feed(255), Some(Key::Invalid));
    }
}
