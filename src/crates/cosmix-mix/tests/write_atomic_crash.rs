//! TODO-mix P3 acceptance, out of process: a `mix` rewriting a file in a
//! tight loop with `write_atomic` is SIGKILLed, several times. After every
//! kill the target must be one COMPLETE version — never a prefix, never
//! empty, never a mix of the two. Only a real process can be killed
//! mid-write, so this cannot live in the in-process suite.
//!
//! Review MINOR-13 hardened it two ways, so it cannot pass vacuously:
//! * per-round progress — each round first waits for a NEW write to land
//!   (the target's inode changes), so no round merely inherits the last
//!   round's file;
//! * a synchronised kill — stop the writer and confirm its unpublished
//!   temporary still exists before SIGKILL. Some filesystems expose a write's
//!   final size only when the syscall completes, so observing a partial size
//!   is not a portable proof. At least one round must stop before publication.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIZE: usize = 16 * 1024 * 1024;
const ROUNDS: u64 = 5;

fn inode(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.ino())
}

/// A temporary belonging to the current atomic replacement, before rename.
fn unpublished_temp_present(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with(".state.mixtmp-")
            && entry
                .metadata()
                .map(|m| m.is_file() && m.len() <= SIZE as u64)
                .unwrap_or(false)
    })
}

#[test]
fn sigkill_mid_rewrite_leaves_a_complete_old_or_new_file() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mix-write-atomic-crash-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let target = dir.join("state");
    let script = dir.join("loop.mix");
    {
        let mut f = std::fs::File::create(&script).expect("script");
        write!(
            f,
            "$a = repeat(\"A\", {SIZE})\n\
             $b = repeat(\"B\", {SIZE})\n\
             $i = 0\n\
             while true\n  \
               if $i % 2 == 0 then\n    write_atomic(\"{t}\", $a)\n  else\n    write_atomic(\"{t}\", $b)\n  end\n  \
               $i = $i + 1\n\
             end\n",
            t = target.display()
        )
        .expect("script");
    }

    let mut pre_publish_kills = 0;
    for round in 0..ROUNDS {
        // A killed round leaves its partial temp behind (documented: a
        // SIGKILLed writer cannot clean up). Clear them, or the next round's
        // "unpublished temp present" would be satisfied by a stale one.
        for entry in std::fs::read_dir(&dir).expect("scratch dir").flatten() {
            if entry.file_name().to_string_lossy().starts_with(".state.mixtmp-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let before = inode(&target);
        let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
            .arg(&script)
            .env("MIX_STATS", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("mix binary must run");

        // Progress: a new write must land this round.
        let deadline = Instant::now() + Duration::from_secs(30);
        while inode(&target).is_none() || inode(&target) == before {
            assert!(
                Instant::now() < deadline,
                "round {round}: no new write landed"
            );
            assert!(
                child.try_wait().expect("try_wait").is_none(),
                "round {round}: the rewrite loop exited early — write_atomic failed"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        // Freeze the producer, then prove it still owns an unpublished temp.
        // A rename between observation and SIGSTOP does not count: resume and
        // retry. SIGKILL is delivered only after the stopped-state proof.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut caught = false;
        while Instant::now() < deadline {
            if unpublished_temp_present(&dir) {
                let pid = child.id() as libc::pid_t;
                assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0, "stop writer");
                let stop_deadline = Instant::now() + Duration::from_secs(5);
                let mut stopped = false;
                while Instant::now() < stop_deadline {
                    let mut status = 0;
                    let got = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
                    if got == pid {
                        assert!(libc::WIFSTOPPED(status), "writer exited while stopping");
                        stopped = true;
                        break;
                    }
                    assert!(got >= 0, "wait for stopped writer");
                    std::thread::sleep(Duration::from_millis(1));
                }
                if !stopped {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("writer did not stop within deadline");
                }
                if unpublished_temp_present(&dir) {
                    caught = true;
                    break;
                }
                assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0, "resume writer");
            }
        }
        child.kill().expect("SIGKILL mix");
        let _ = child.wait();
        if caught {
            pre_publish_kills += 1;
        }

        let bytes = std::fs::read(&target).expect("target must still exist");
        let complete_a = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'A');
        let complete_b = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'B');
        assert!(
            complete_a || complete_b,
            "round {round} (killed before publication: {caught}): target is partial or mixed \
             ({} bytes, first {:?}, last {:?})",
            bytes.len(),
            bytes.first().map(|b| *b as char),
            bytes.last().map(|b| *b as char)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        pre_publish_kills >= 1,
        "no round stopped the writer before publication — the test proved nothing"
    );
}
