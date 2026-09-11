//! Async-signal-safe stop ingress shared with the process controller.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering::SeqCst};

static FD: AtomicI32 = AtomicI32::new(-1);
static WRITERS: AtomicUsize = AtomicUsize::new(0);
static EDITING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

pub struct Registration(UnixStream);
impl Registration {
    pub fn new(wake: UnixStream) -> io::Result<Self> {
        wake.set_nonblocking(true)?;
        FD.compare_exchange(-1, wake.as_raw_fd(), SeqCst, SeqCst)
            .map_err(|_| io::Error::other("stop wake already registered"))?;
        EDITING.store(true, SeqCst);
        Ok(Self(wake))
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        EDITING.store(false, SeqCst);
        FD.store(-1, SeqCst);
        // A handler which loaded the old fd must finish before it is closed or
        // reused. New handlers see -1. This wait is never in signal context.
        while WRITERS.load(SeqCst) != 0 {
            std::thread::yield_now();
        }
        STOP.store(false, SeqCst);
        let _ = self.0.as_raw_fd();
    }
}
pub fn editing(active: bool) {
    EDITING.store(active, SeqCst);
}
pub fn take_stop() -> bool {
    STOP.swap(false, SeqCst)
}

/// Called only by the controller's signal handler. No allocation or locks.
pub fn request_stop() -> bool {
    WRITERS.fetch_add(1, SeqCst);
    let fd = FD.load(SeqCst);
    let handled = fd >= 0 && EDITING.load(SeqCst);
    if handled {
        STOP.store(true, SeqCst);
        let byte = b'T';
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
    WRITERS.fetch_sub(1, SeqCst);
    handled
}
