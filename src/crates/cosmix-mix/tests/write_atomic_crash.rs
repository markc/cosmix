//! TODO-mix P3 acceptance, out of process: a `mix` rewriting a file in a
//! tight loop with `write_atomic` is SIGKILLed at arbitrary points, several
//! times. After every kill the target must be one COMPLETE version — never a
//! prefix, never empty, never a mix of the two. Only a real process can be
//! killed mid-write, so this cannot live in the in-process suite.
//!
//! The payloads are large (4 MiB) so a kill lands inside a write far more
//! often than between writes; the assertion holds wherever it lands.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIZE: usize = 4 * 1024 * 1024;

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

    for round in 0..5u64 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
            .arg(&script)
            .env("MIX_STATS", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("mix binary must run");
        // Let at least one write land, then kill at a varying point.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !target.exists() {
            assert!(Instant::now() < deadline, "the loop never wrote the target");
            assert!(
                child.try_wait().expect("try_wait").is_none(),
                "the rewrite loop exited early — write_atomic failed"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(40 + round * 37));
        child.kill().expect("SIGKILL mix");
        let _ = child.wait();

        let bytes = std::fs::read(&target).expect("target must still exist");
        let complete_a = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'A');
        let complete_b = bytes.len() == SIZE && bytes.iter().all(|b| *b == b'B');
        assert!(
            complete_a || complete_b,
            "round {round}: target is partial or mixed ({} bytes, first {:?}, last {:?})",
            bytes.len(),
            bytes.first().map(|b| *b as char),
            bytes.last().map(|b| *b as char)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
