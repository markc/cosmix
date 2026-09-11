//! Absence stays silent; malformed inherited input is scrubbed before user code.
#![cfg(target_os = "linux")]
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

const MARKER: &str = "COSMIX_SESSION_FD";
fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mix"));
    command
        .args([
            "--no-prelude",
            "-c",
            "print(\"USER_SOURCE=[\" .. env(\"COSMIX_SESSION_FD\") .. \"]\")",
        ])
        .env_remove(MARKER)
        .env("MIX_STATS", "off");
    command
}

#[test]
fn ordinary_dash_c_has_no_session_noise_or_environment_state() {
    let output = command().output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"USER_SOURCE=[]\n");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_marker_never_closes_stdio_and_is_scrubbed_before_user_source() {
    for value in ["0", "1", "2", "-1", "bogus", "2147483648", "99999"] {
        let output = command().env(MARKER, value).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"USER_SOURCE=[]\n");
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        assert!(diagnostic.contains("mix native-session FAILED at"));
    }
}

#[test]
fn bad_seals_or_layout_warn_once_without_delaying_or_failing_shell() {
    for seals in [
        0,
        libc::F_SEAL_SEAL,
        libc::F_SEAL_SEAL | libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK,
    ] {
        let raw = unsafe {
            libc::memfd_create(
                c"mix-malformed-test".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(raw >= 3);
        let mut file = unsafe { File::from_raw_fd(raw) };
        file.write_all(b"not a launch descriptor").unwrap();
        if seals != 0 {
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) },
                0
            );
        }
        let mut child = command();
        child.env(MARKER, "64");
        unsafe {
            child.pre_exec(move || {
                if libc::dup2(raw, 64) < 0 || libc::fcntl(64, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let start = std::time::Instant::now();
        let output = child.output().unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(output.status.success());
        assert_eq!(output.stdout, b"USER_SOURCE=[]\n");
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        assert!(diagnostic.contains(if seals & libc::F_SEAL_WRITE == 0 {
            "at seals:"
        } else {
            "at layout:"
        }));
    }
}
