//! `which()` answers "can I run this?" (0.52.0).
//!
//! Until 0.52.0 it used `is_file()`, so any regular file on PATH came back as
//! if it were runnable — a caller that branched on `which("foo") != nil` and
//! then ran `foo` got a spawn failure from the probe whose whole job was to
//! prevent one.
//!
//! Out-of-process, for the reason `env_fallback_lc.rs` documents: `which`
//! reads PATH, and mutating it in-process with `std::env::set_var` races every
//! other test's `getenv` under the parallel runner. The child gets its PATH
//! from `Command::env` and nothing in the parent is mutated.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A private PATH directory holding one of each candidate shape.
struct Bed {
    dir: PathBuf,
}

impl Bed {
    fn new(tag: &str) -> Self {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "mix-which-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // A regular file with NO execute bit — the bug's exhibit.
        let plain = dir.join("notrunnable");
        fs::write(&plain, "#!/bin/sh\necho nope\n").unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();

        // A genuinely executable file.
        let exe = dir.join("runnable");
        fs::write(&exe, "#!/bin/sh\necho yes\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

        // A DIRECTORY whose name looks like a command. X_OK is true for a
        // searchable directory, so an execute-only check would return it.
        fs::create_dir_all(dir.join("adirectory")).unwrap();

        Bed { dir }
    }

    /// Run `expr` with PATH set to exactly this bed, and return trimmed stdout.
    fn eval(&self, expr: &str) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_mix"))
            .arg("-c")
            .arg(expr)
            .env("MIX_STATS", "off")
            .env("PATH", &self.dir)
            .output()
            .expect("mix binary must run");
        assert!(
            out.status.success(),
            "mix failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn non_executable_file_on_path_is_not_found() {
    let bed = Bed::new("plain");
    assert_eq!(
        bed.eval("print(\"\" .. which(\"notrunnable\"))"),
        "nil",
        "a regular file with no execute bit is not a command"
    );
}

#[test]
fn executable_file_on_path_is_found() {
    let bed = Bed::new("exe");
    let got = bed.eval("print(\"\" .. which(\"runnable\"))");
    assert_eq!(
        got,
        bed.dir.join("runnable").to_string_lossy(),
        "the executable must still be found — this is the half that must not regress"
    );
}

#[test]
fn a_directory_on_path_is_not_a_command() {
    // The trap in fixing this: access(X_OK) alone is TRUE for a searchable
    // directory, so dropping is_file() would make `which("adirectory")`
    // confidently return a directory.
    let bed = Bed::new("dir");
    assert_eq!(
        bed.eval("print(\"\" .. which(\"adirectory\"))"),
        "nil",
        "a searchable directory satisfies X_OK but is not runnable"
    );
}

#[test]
fn losing_the_execute_bit_changes_the_answer() {
    // Ties the result to the actual permission rather than to the filename:
    // the same path, chmod'd, must flip from found to nil.
    let bed = Bed::new("chmod");
    let target = bed.dir.join("runnable");

    assert_ne!(bed.eval("print(\"\" .. which(\"runnable\"))"), "nil");

    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        bed.eval("print(\"\" .. which(\"runnable\"))"),
        "nil",
        "after chmod -x the same path must stop being a command"
    );

    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(
        bed.eval("print(\"\" .. which(\"runnable\"))"),
        "nil",
        "and come back when the bit returns"
    );
}

#[test]
fn non_string_argument_raises_rather_than_being_coerced() {
    let bed = Bed::new("coerce");
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-c")
        .arg("which([\"runnable\"])")
        .env("MIX_STATS", "off")
        .env("PATH", &bed.dir)
        .output()
        .expect("mix binary must run");
    assert!(!out.status.success(), "which(list) must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cmd must be a string"), "stderr={stderr}");
}

/// The probe must agree with reality: whatever `which` returns has to be
/// something the process can actually execute.
#[test]
fn what_which_returns_can_actually_be_run() {
    let bed = Bed::new("agree");
    let path = bed.eval("print(\"\" .. which(\"runnable\"))");
    assert_ne!(path, "nil");
    let out = Command::new(Path::new(&path))
        .output()
        .expect("which's answer must be executable");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "yes");
}
