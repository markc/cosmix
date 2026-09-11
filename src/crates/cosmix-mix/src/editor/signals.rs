//! Async-signal-safe stop ingress shared with the process controller.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering::SeqCst};

static FD: AtomicI32 = AtomicI32::new(-1);
static WRITERS: AtomicUsize = AtomicUsize::new(0);
// An editor thread exists and owns cooperative stops, including while cooked.
// Temporarily false only while that owner performs its already-cooked stop.
static EDITOR_PRESENT: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

/// Covers the ENTIRE handler, including default disposition, raise and reinstall.
/// Registration cannot enter raw while an earlier default stop is in flight.
pub struct StopHandler;
impl StopHandler {
    pub fn enter() -> Self {
        WRITERS.fetch_add(1, SeqCst);
        Self
    }
}
impl Drop for StopHandler {
    fn drop(&mut self) {
        WRITERS.fetch_sub(1, SeqCst);
    }
}

pub struct Registration(UnixStream);
impl Registration {
    pub fn new(wake: UnixStream) -> io::Result<Self> {
        wake.set_nonblocking(true)?;
        FD.compare_exchange(-1, wake.as_raw_fd(), SeqCst, SeqCst)
            .map_err(|_| io::Error::other("stop wake already registered"))?;
        cooperative(true);
        Ok(Self(wake))
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        EDITOR_PRESENT.store(false, SeqCst);
        FD.store(-1, SeqCst);
        // A handler which loaded the old fd must finish before it is closed or
        // reused. New handlers see -1. This wait is never in signal context.
        while WRITERS.load(SeqCst) != 0 {
            std::thread::yield_now();
        }
        // The loop has ended, so we must honour a stop claimed before detach.
        // Terminal cleanup precedes registration teardown; default stop is safe.
        if take_stop() {
            unsafe { libc::raise(libc::SIGTSTP) };
        }
        let _ = self.0.as_raw_fd();
    }
}
pub fn cooperative(active: bool) {
    EDITOR_PRESENT.store(active, SeqCst);
    if active {
        // SeqCst pairs registration with the handler's increment BEFORE its
        // ownership read. A handler seeing false cannot escape this barrier.
        while WRITERS.load(SeqCst) != 0 {
            std::thread::yield_now();
        }
    }
}
pub fn take_stop() -> bool {
    STOP.swap(false, SeqCst)
}

/// Called with StopHandler alive. No allocation or locks.
pub fn request_stop() -> bool {
    let fd = FD.load(SeqCst);
    let handled = fd >= 0 && EDITOR_PRESENT.load(SeqCst);
    if handled {
        STOP.store(true, SeqCst);
        let byte = b'T';
        unsafe {
            // write may set EAGAIN; never clobber the interrupted code's errno.
            let saved_errno = *libc::__errno_location();
            libc::write(fd, (&byte as *const u8).cast(), 1);
            *libc::__errno_location() = saved_errno;
        }
    }
    handled
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_waits_for_default_stop_and_teardown_delivers_claimed_stop() {
        // Emulate a handler which has already chosen the default branch but
        // has not yet raised/reinstalled; raw-entry registration must wait.
        let handler = StopHandler::enter();
        assert!(!request_stop());
        let (read, write) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker =
            std::thread::spawn(move || tx.send(Registration::new(write).unwrap()).unwrap());
        while !EDITOR_PRESENT.load(SeqCst) {
            std::thread::yield_now();
        }
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(handler);
        let registration = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        let delivered = std::sync::Arc::new(AtomicBool::new(false));
        let hook = signal_hook::flag::register(libc::SIGTSTP, delivered.clone()).unwrap();
        {
            let _handler = StopHandler::enter();
            // Fill the wake socket so the write takes its EAGAIN path.
            let bytes = [0u8; 4096];
            while unsafe {
                libc::write(
                    registration.0.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                )
            } > 0
            {}
            unsafe {
                *libc::__errno_location() = libc::EDOM;
            }
            assert!(request_stop());
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EDOM);
        }
        drop(registration);
        assert!(delivered.load(SeqCst));
        assert!(!take_stop());
        signal_hook::low_level::unregister(hook);
        drop(read);
    }
}
