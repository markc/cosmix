//! `foreman gc-cache` through the real binary — the nightly tier-2 step's
//! own surface.
//!
//! The unit tests in `gc` prove the trimming; these prove the thing that
//! actually failed review: what the OPERATOR sees. A step that is mis-wired,
//! or that cannot bring the cache down to the cap, must be visibly red, not
//! a green "nothing to do" line that lets the cache grow forever.

use std::path::Path;
use std::process::{Command, Output};

fn foreman(args: &[&str], env: &[(&str, &str)], unset: &[&str]) -> Output {
    let verify_root = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_foreman"));
    cmd.args(args)
        .env(
            "FOREMAN_VERIFY_LANE",
            verify_root.path().join("verify.lock"),
        )
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30");
    for key in unset {
        cmd.env_remove(key);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawning foreman")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A cache tree: `debug/deps/stale.rlib` is reclaimable,
/// `debug/incremental/session.bin` is not (incremental is not a GC subdir).
fn fixture(root: &Path) {
    let deps = root.join("debug/deps");
    let incremental = root.join("debug/incremental");
    std::fs::create_dir_all(&deps).unwrap();
    std::fs::create_dir_all(&incremental).unwrap();
    std::fs::write(deps.join("stale.rlib"), vec![0u8; 4096]).unwrap();
    std::fs::write(incremental.join("session.bin"), vec![0u8; 4096]).unwrap();
}

#[test]
fn under_cap_is_green_and_says_nothing_to_do() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fixture(&target);

    let out = foreman(
        &["gc-cache", "--dir", target.to_str().unwrap()],
        &[],
        &["FOREMAN_CACHE_MAX_GB"],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("already under cap"),
        "{}",
        stdout(&out)
    );
    assert!(target.join("debug/deps/stale.rlib").exists());
}

/// The wiring bug this command was rejected over: `FOREMAN_TIER2_COMMANDS`
/// is whitespace-split into an argv with no shell, so `--dir
/// $CARGO_TARGET_DIR` passes that literal string as the path. It used to
/// measure a nonexistent dir as 0 bytes, print "nothing to do", and exit 0
/// forever. It must now fail loudly.
#[test]
fn an_unexpanded_variable_as_dir_fails_loudly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bogus = tmp.path().join("$CARGO_TARGET_DIR");

    let out = foreman(
        &["gc-cache", "--dir", bogus.to_str().unwrap()],
        &[],
        &["FOREMAN_CACHE_MAX_GB"],
    );

    assert!(!out.status.success(), "must not exit 0: {}", stdout(&out));
    assert!(stderr(&out).contains("does not exist"), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains("nothing to do"),
        "a missing cache must never read as success: {}",
        stdout(&out)
    );
}

/// The other half of the same wiring: with no `--dir`, the directory comes
/// from `CARGO_TARGET_DIR` in foreman's OWN environment — resolved
/// in-process, which is why it works where `$VAR` in the argv does not.
#[test]
fn no_dir_flag_takes_the_directory_from_the_environment() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fixture(&target);

    let out = foreman(
        &["gc-cache"],
        &[("CARGO_TARGET_DIR", target.to_str().unwrap())],
        &["FOREMAN_CACHE_MAX_GB"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("already under cap"));

    // …and with neither, it refuses rather than guessing a relative path.
    let out = foreman(
        &["gc-cache"],
        &[],
        &["CARGO_TARGET_DIR", "FOREMAN_CACHE_MAX_GB"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no cache directory"),
        "{}",
        stderr(&out)
    );
}

/// Over the cap, having reclaimed everything it is allowed to touch and
/// still over it: red, with the bloat named. Previously this printed the
/// same "nothing to do" as success.
#[test]
fn still_over_cap_after_gc_exits_non_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fixture(&target);

    let out = foreman(
        &[
            "gc-cache",
            "--dir",
            target.to_str().unwrap(),
            "--max-gb",
            "0",
        ],
        &[],
        &["FOREMAN_CACHE_MAX_GB"],
    );

    assert!(
        !out.status.success(),
        "still-over-cap must be a red step: {}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("STILL OVER CAP"), "{}", stdout(&out));
    assert!(!stdout(&out).contains("nothing to do"), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("still over the cap"),
        "{}",
        stderr(&out)
    );

    // It still did its job on the way there: the reclaimable entry is gone,
    // the out-of-bounds one and the GC subdirs survive.
    assert!(!target.join("debug/deps/stale.rlib").exists());
    assert!(target.join("debug/incremental/session.bin").exists());
    assert!(target.join("debug/deps").is_dir());
}

/// `FOREMAN_CACHE_MAX_GB` is the documented env knob; prove the binary
/// reads it, not just the library.
#[test]
fn the_env_cap_reaches_the_binary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("target");
    fixture(&target);

    let out = foreman(
        &["gc-cache", "--dir", target.to_str().unwrap()],
        &[("FOREMAN_CACHE_MAX_GB", "0")],
        &[],
    );

    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("cap 0.00 GB"), "{}", stdout(&out));
    assert!(
        !target.join("debug/deps/stale.rlib").exists(),
        "a 0 GB cap must reclaim the stale entry"
    );
}
