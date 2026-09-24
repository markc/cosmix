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
//! * a synchronised kill — the test watches the directory and SIGKILLs the
//!   writer the moment a partially written temp (0 < size < SIZE) is on
//!   disk, i.e. provably mid-write. At least one round must land that way.

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

/// A temp beside the target that is partially written right now.
fn partial_temp_present(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with(".state.mixtmp-")
            && entry
                .metadata()
                .map(|m| m.len() > 0 && (m.len() as usize) < SIZE)
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
             while true do\n  \
               if $i % 2 == 0 then\n    write_atomic(\"{t}\", $a)\n  else\n    write_atomic(\"{t}\", $b)\n  end\n  \
               $i = $i + 1\n\
             end\n",
            t = target.display()
        )
        .expect("script");
    }

    let mut mid_write_kills = 0;
    for round in 0..ROUNDS {
        // A killed round leaves its partial temp behind (documented: a
        // SIGKILLed writer cannot clean up). Clear them, or the next round's
        // "partial temp present" would be satisfied by a stale one.
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

        // Synchronised kill: the moment a partial temp is on disk.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut caught = false;
        while Instant::now() < deadline {
            if partial_temp_present(&dir) {
                caught = true;
                break;
            }
        }
        child.kill().expect("SIGKILL mix");
        let _ = child.wait();
        if caught {
            mid_write_kills += 1;
        }

        let bytes = std::fs::read(&target).expect("target must still exist");
        let complete_a = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'A');
        let complete_b = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'B');
        assert!(
            complete_a || complete_b,
            "round {round} (killed mid-write: {caught}): target is partial or mixed \
             ({} bytes, first {:?}, last {:?})",
            bytes.len(),
            bytes.first().map(|b| *b as char),
            bytes.last().map(|b| *b as char)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        mid_write_kills >= 1,
        "no round caught the writer mid-write — the test proved nothing"
    );
}
