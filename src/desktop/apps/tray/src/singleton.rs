//! Process singleton enforced with an exclusive non-blocking flock.

use std::env;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(crate) enum LockOutcome {
    Acquired(InstanceLock),
    AlreadyHeld,
}

pub(crate) struct InstanceLock {
    _file: File,
}

pub(crate) fn acquire() -> io::Result<LockOutcome> {
    try_lock(&lock_path(
        env::var_os("XDG_RUNTIME_DIR"),
        // SAFETY: geteuid has no preconditions and does not access Rust memory.
        unsafe { libc::geteuid() },
    ))
}

fn lock_path(runtime_dir: Option<OsString>, uid: libc::uid_t) -> PathBuf {
    match runtime_dir
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(directory) => directory.join("cosmix-tray.lock"),
        None => PathBuf::from(format!("/tmp/cosmix-tray-{uid}.lock")),
    }
}

fn try_lock(path: &Path) -> io::Result<LockOutcome> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    // A pre-existing file owned by another uid (possible only on the shared
    // /tmp fallback) is tampering, not a running instance — surface it
    // instead of reporting AlreadyHeld.
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and does not access Rust memory.
    if std::os::unix::fs::MetadataExt::uid(&metadata) != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("lock file {} is owned by another user", path.display()),
        ));
    }
    // SAFETY: file owns a valid open descriptor for the duration of the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(LockOutcome::Acquired(InstanceLock { _file: file }));
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Ok(LockOutcome::AlreadyHeld)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn exclusive_lock_rejects_a_second_holder_then_releases() {
        let nonce = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("cosmix-tray-lock-{}-{nonce}", std::process::id()));

        let first = match try_lock(&path).expect("first lock attempt") {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::AlreadyHeld => panic!("fresh fixture must not be held"),
        };
        assert!(matches!(
            try_lock(&path).expect("second lock attempt"),
            LockOutcome::AlreadyHeld
        ));
        drop(first);
        assert!(matches!(
            try_lock(&path).expect("lock after release"),
            LockOutcome::Acquired(_)
        ));
        std::fs::remove_file(path).expect("remove lock fixture");
    }

    #[test]
    fn runtime_path_is_preferred_with_uid_scoped_tmp_fallback() {
        assert_eq!(
            lock_path(Some(OsString::from("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/cosmix-tray.lock")
        );
        assert_eq!(
            lock_path(Some(OsString::from("relative")), 1000),
            PathBuf::from("/tmp/cosmix-tray-1000.lock")
        );
        assert_eq!(
            lock_path(None, 1000),
            PathBuf::from("/tmp/cosmix-tray-1000.lock")
        );
    }
}
