//! "Something changed" signal for a frontend's event loop.
//!
//! The core already reports change through a [`Wake`] callback (PTY output,
//! resize, pane exit, Bus mutations). A Bevy frontend turns that into a winit
//! user event; a raw-Wayland frontend wants a descriptor it can put in the same
//! poll set as the display socket. [`WakeFd`] is that descriptor: an eventfd
//! the callback writes to and the frontend drains. Wakes coalesce, so a burst
//! of PTY output costs one readiness, and nothing here ever blocks the writer.
use crate::terminal::Wake;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

pub struct WakeFd(Arc<OwnedFd>);

impl WakeFd {
    /// Non-blocking and close-on-exec: a PTY child never inherits it.
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers; a non-negative result is a new
        // descriptor this process owns exclusively.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just returned by eventfd and has no other owner.
        Ok(Self(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// The callback to hand to `TabSet::set_wake` / `Terminal::set_wake`.
    pub fn waker(&self) -> Wake {
        let fd = self.0.clone();
        Arc::new(move || signal(fd.as_raw_fd()))
    }

    /// Consume every pending wake. True when at least one arrived since the
    /// last drain; false when nothing was pending (or the read failed).
    ///
    /// Drain BEFORE taking snapshots, never after: a change that lands while
    /// the frontend is reading the grid then leaves the descriptor readable
    /// for the next loop turn instead of being swallowed.
    pub fn drain(&self) -> bool {
        let mut count = 0u64;
        loop {
            // SAFETY: reads exactly eight bytes into a live u64 from an owned fd.
            let read = unsafe {
                libc::read(
                    self.0.as_raw_fd(),
                    (&raw mut count).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return read == std::mem::size_of::<u64>() as isize && count > 0;
        }
    }
}

impl AsFd for WakeFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for WakeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

fn signal(fd: RawFd) {
    let one = 1u64;
    loop {
        // SAFETY: writes eight bytes from a live u64. EAGAIN means the counter
        // is saturated, which already reads as "woken", so it is not retried.
        let written =
            unsafe { libc::write(fd, (&raw const one).cast(), std::mem::size_of::<u64>()) };
        if written >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(fd: &WakeFd) -> bool {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd, zero timeout.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        ready == 1 && poll.revents & libc::POLLIN != 0
    }

    #[test]
    fn wakes_coalesce_and_drain_clears_readiness() {
        let fd = WakeFd::new().unwrap();
        assert!(!readable(&fd));
        assert!(!fd.drain(), "an idle descriptor reports no wake");
        let wake = fd.waker();
        for _ in 0..1000 {
            wake();
        }
        assert!(readable(&fd));
        assert!(fd.drain());
        assert!(!readable(&fd), "one drain consumes the whole burst");
        assert!(!fd.drain());
    }

    #[test]
    fn waker_outlives_the_frontend_handle() {
        let fd = WakeFd::new().unwrap();
        let wake = fd.waker();
        drop(fd);
        // The PTY thread may still fire after the frontend exits.
        wake();
    }

    #[test]
    fn descriptor_is_close_on_exec() {
        let fd = WakeFd::new().unwrap();
        // SAFETY: F_GETFD on an owned descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
}
