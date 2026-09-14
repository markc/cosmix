//! systemd `Type=notify` readiness, self-contained (no libsystemd, no crate).
//!
//! The persistent boot-desktop compositor unit (`cosmix-desktop.service`) is
//! ordered by other units against comp being *usable*, not merely started.
//! comp signals `READY=1` exactly once, at its first presented frame, so the
//! Wayland socket existing (which precedes first light) is not mistaken for
//! readiness. A `.notify()` is a no-op unless `NOTIFY_SOCKET` is set, so the
//! nested backend, the test harness and dev runs are unaffected, and it is
//! strictly fire-and-forget: every failure path is a silent return, because a
//! readiness notification must never perturb the compositor.

/// Send `READY=1` to systemd's notify socket, once, when comp is usable.
/// No-op when `NOTIFY_SOCKET` is unset; never panics or blocks meaningfully.
pub(crate) fn notify_ready() {
    let Ok(socket) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let bytes = socket.into_bytes();
    if bytes.is_empty() {
        return;
    }
    // NOTIFY_SOCKET is a filesystem path, or — when it begins with '@' — an
    // abstract-namespace name whose leading wire byte is NUL.
    let abstract_socket = bytes[0] == b'@';
    let name: &[u8] = if abstract_socket { &bytes[1..] } else { &bytes };
    // SAFETY: a textbook AF_UNIX/SOCK_DGRAM sendto. Every pointer references a
    // local that outlives the call, the address length is bounded to sun_path,
    // and the fd is closed on every path before return.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        // Abstract sockets carry a leading NUL (index 0 stays zero) then the
        // name and NO terminator; path sockets start at index 0 and are
        // NUL-terminated. `sun_path` is already fully zeroed.
        let start = usize::from(abstract_socket);
        // Leave room for the terminating NUL on path sockets.
        if start + name.len() >= addr.sun_path.len() {
            libc::close(fd);
            return;
        }
        for (i, byte) in name.iter().enumerate() {
            addr.sun_path[start + i] = *byte as libc::c_char;
        }
        let used = start + name.len() + usize::from(!abstract_socket);
        let addr_len = (std::mem::size_of::<libc::sa_family_t>() + used) as libc::socklen_t;
        let msg = b"READY=1\n";
        libc::sendto(
            fd,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
            libc::MSG_NOSIGNAL,
            std::ptr::addr_of!(addr) as *const libc::sockaddr,
            addr_len,
        );
        libc::close(fd);
    }
}
