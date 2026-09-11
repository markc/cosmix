//! BROKER-019's private, anonymous launch handoff. Never log these contents.
use cosmix_client::session::GrantResult;
use ed25519_dalek::SigningKey;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use zeroize::Zeroizing;

pub const MARKER: &str = "COSMIX_SESSION_FD";

pub fn fresh_key() -> io::Result<SigningKey> {
    let mut seed = Zeroizing::new([0u8; 32]);
    let mut offset = 0;
    while offset < seed.len() {
        // SAFETY: the remaining slice is writable, and getrandom retains no pointer.
        let n = unsafe {
            libc::getrandom(seed[offset..].as_mut_ptr().cast(), seed.len() - offset, 0)
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(io::Error::other("random source returned no bytes"));
        }
        offset += n as usize;
    }
    Ok(SigningKey::from_bytes(&seed))
}

/// Owns both reserved mapping slots; neither is inheritable in the parent.
pub struct LaunchFd {
    source: File,
    target: OwnedFd,
}

impl LaunchFd {
    pub fn new(descriptor: &GrantResult, key: &SigningKey) -> io::Result<Self> {
        // Byte layout v1 (the child-side Mix bootstrap parser seam):
        // [0] = 1; [1..5] = u32 big-endian JSON byte length N;
        // [5..5+N] = UTF-8 JSON {"grant": SessionGrant, "record": SessionRecord}
        // using BUS-016 field encodings; [5+N..37+N] = 32 raw Ed25519 seed
        // bytes (NOT a 64-byte expanded secret). Exact EOF at 37+N. N <= 16384.
        // No JSON serialisation of secrets and no secret-bearing heap buffer.
        let public = serde_json::to_vec(descriptor)?;
        if public.len() > 16384 {
            return Err(io::Error::other("launch descriptor exceeds limit"));
        }
        // SAFETY: static NUL-terminated name, supported Linux flags.
        let raw = unsafe {
            libc::memfd_create(c"cosmix-session".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create transferred a new descriptor to this scope.
        let mut source = unsafe { File::from_raw_fd(raw) };
        source.write_all(&[1])?;
        source.write_all(&(public.len() as u32).to_be_bytes())?;
        source.write_all(&public)?;
        let seed = Zeroizing::new(key.to_bytes());
        source.write_all(seed.as_ref())?;
        source.rewind()?;
        // SAFETY: fcntl operates on our open descriptor. Seal contents first,
        // then seal the seal set, so no child can weaken integrity protection.
        for seals in [libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE, libc::F_SEAL_SEAL] {
            if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // Reserve the target in the parent to prevent collisions with PTY fds
        // and std::process's exec error pipe. dup2 clears CLOEXEC only in child.
        let target = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 64) };
        if target < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            source,
            // SAFETY: fcntl returned a newly owned descriptor.
            target: unsafe { OwnedFd::from_raw_fd(target) },
        })
    }

    pub fn mapping(&self) -> (RawFd, RawFd) {
        (self.source.as_raw_fd(), self.target.as_raw_fd())
    }

    pub fn marker(&self) -> (String, String) {
        (MARKER.into(), self.target.as_raw_fd().to_string())
    }
}
