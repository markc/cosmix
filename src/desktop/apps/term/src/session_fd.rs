//! BROKER-019's private, anonymous launch handoff. Never log these contents.
use cosmix_client::session::GrantResult;
use ed25519_dalek::SigningKey;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use zeroize::Zeroizing;

pub const MARKER: &str = "COSMIX_SESSION_FD";

/// Run before any threads or child spawns. Term might itself be launched by a
/// native parent; its inherited handoff must never leak into a pane's exec.
pub fn quarantine_inherited() {
    if let Ok(value) = std::env::var(MARKER) {
        match value.parse::<RawFd>() {
            Ok(fd) if fd >= 0 => {
                // SAFETY: fcntl changes flags only; ownership is not assumed.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    eprintln!(
                        "term inherited session descriptor unavailable: {}",
                        io::Error::last_os_error()
                    );
                }
            }
            _ => eprintln!("term inherited session descriptor marker is invalid"),
        }
    }
}

pub fn fresh_key() -> io::Result<SigningKey> {
    let mut seed = Zeroizing::new([0u8; 32]);
    let mut offset = 0;
    while offset < seed.len() {
        // SAFETY: the remaining slice is writable, and getrandom retains no pointer.
        let n =
            unsafe { libc::getrandom(seed[offset..].as_mut_ptr().cast(), seed.len() - offset, 0) };
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
            libc::memfd_create(
                c"cosmix-session".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
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
        for seals in [
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            libc::F_SEAL_SEAL,
        ] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn descriptor(key: &SigningKey) -> GrantResult {
        serde_json::from_value(serde_json::json!({
            "grant": {
                "grant_id": "11".repeat(16), "record_id": "22".repeat(16),
                "incarnation": "33".repeat(16),
                "public_key": cosmix_bus::native_session::HexBytes(key.verifying_key().to_bytes()),
                "parent_key_hash": "44".repeat(32), "expires_ms": "30000", "state": "pending"
            },
            "record": {
                "name": "test-child", "record_assurance": "reserved", "owner_node": "test-node",
                "owner_uid": 1000, "broker_epoch": "55".repeat(16), "record_id": "22".repeat(16),
                "instance_id": "66".repeat(16), "incarnation": "33".repeat(16), "role": "pane-shell",
                "parent_instance": "77".repeat(16), "parent_incarnation": "88".repeat(16),
                "pane_id": "1", "pane_generation": "1", "binding_generation": "0", "state": "pending",
                "capabilities": ["read_state"], "policy": "default-open", "lease_remaining_ms": null
            }
        })).unwrap()
    }

    #[test]
    fn memfd_layout_round_trip_and_seals() {
        let key = fresh_key().unwrap();
        let descriptor = descriptor(&key);
        let mut fd = LaunchFd::new(&descriptor, &key).unwrap();
        let mut bytes = Zeroizing::new(Vec::new());
        fd.source.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes[0], 1);
        let n = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert!(n <= 16384);
        assert_eq!(bytes.len(), 37 + n);
        let decoded: GrantResult = serde_json::from_slice(&bytes[5..5 + n]).unwrap();
        assert_eq!(decoded.record, descriptor.record);
        assert_eq!(decoded.grant, descriptor.grant);
        // Compare public keys rather than including private bytes in an assert
        // failure diagnostic. Test buffers receive the same zeroisation policy.
        let seed = Zeroizing::new(<[u8; 32]>::try_from(&bytes[5 + n..]).unwrap());
        assert_eq!(
            SigningKey::from_bytes(&seed).verifying_key(),
            key.verifying_key()
        );
        let raw = fd.source.as_raw_fd();
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        assert_eq!(
            seals,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL
        );
        assert!(fd.source.write_all(b"x").is_err());
        assert_eq!(unsafe { libc::ftruncate(raw, 0) }, -1);
        assert_eq!(unsafe { libc::ftruncate(raw, bytes.len() as i64 + 1) }, -1);
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, libc::F_SEAL_FUTURE_WRITE) },
            -1
        );
        for raw in [fd.mapping().0, fd.mapping().1] {
            assert_ne!(
                unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
    }

    /// Executed as a fresh process by the inheritance test, never as a shell.
    #[test]
    fn fd_child_reports_open_descriptors() {
        let Ok(expected) = std::env::var("COSMIX_FD_TEST_COUNT") else {
            return;
        };
        let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let target = std::fs::read_link(entry.path()).ok()?;
                target
                    .to_string_lossy()
                    .contains("memfd:cosmix-session")
                    .then(|| entry.file_name().to_str().unwrap().parse().unwrap())
            })
            .collect();
        assert_eq!(fds.len(), expected.parse::<usize>().unwrap());
        if !fds.is_empty() {
            assert_eq!(
                fds[0],
                std::env::var(MARKER).unwrap().parse::<i32>().unwrap()
            );
            assert_eq!(
                unsafe { libc::fcntl(fds[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            // This process models Term itself inheriting a bootstrap fd.
            // Its first child must not inherit it, even without Mix support.
            quarantine_inherited();
            assert_ne!(
                unsafe { libc::fcntl(fds[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session_fd::tests::fd_child_reports_open_descriptors",
                    "--nocapture",
                ])
                .env("COSMIX_FD_TEST_COUNT", "0")
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert!(String::from_utf8_lossy(&child.stdout).contains("FD_DISCIPLINE_OK"));
        }
        println!("FD_DISCIPLINE_OK");
    }

    fn helper(fd: Option<&LaunchFd>) -> String {
        let executable = std::env::current_exe().unwrap();
        let mut env = vec![(
            "COSMIX_FD_TEST_COUNT".into(),
            if fd.is_some() { "1" } else { "0" }.into(),
        )];
        if let Some(fd) = fd {
            env.push(fd.marker());
        }
        let mut pty = teletypewriter::create_pty_with_spawn_fd(
            Some(executable.to_str().unwrap()),
            vec![
                "--exact".into(),
                "session_fd::tests::fd_child_reports_open_descriptors".into(),
                "--nocapture".into(),
            ],
            &None,
            Some(env),
            80,
            24,
            800,
            480,
            fd.map(LaunchFd::mapping),
        )
        .unwrap();
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut buffer = [0; 1024];
            match pty.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let pid = *pty.child.pid;
        // Helper is bounded; always reap it, including assertion-failure paths.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn mapped_memfd_is_present_once_and_absent_in_second_child() {
        let key = fresh_key().unwrap();
        let fd = LaunchFd::new(&descriptor(&key), &key).unwrap();
        let first = helper(Some(&fd));
        assert!(first.contains("FD_DISCIPLINE_OK"), "{first}");
        // Keep both parent descriptors open during the second spawn: this
        // proves CLOEXEC isolation, not merely cleanup after the first launch.
        let second = helper(None);
        assert!(second.contains("FD_DISCIPLINE_OK"), "{second}");
    }
}
