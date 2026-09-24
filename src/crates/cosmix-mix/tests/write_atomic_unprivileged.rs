//! write_atomic behaviour that only an UNPRIVILEGED writer can show. The cbc
//! gate runs as root, where CAP_FSETID keeps setuid through a write and DAC
//! lets any directory be opened, so these tests would pass vacuously there.
//! Instead, when the test runs as root it launches a copy of the mix binary
//! as uid/gid 65534; unprivileged, it runs mix as itself. Either way the
//! script runs without privilege.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const NOBODY: u32 = 65534;

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// A scratch tree the unprivileged runner can use: `bin/mix` (a world-
/// executable copy, so the cargo target dir's permissions never matter),
/// `script.mix`, and `work/` owned by the runner.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "mix-wa-unpriv-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(root.join("bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_mix"), root.join("bin/mix")).unwrap();
        std::fs::set_permissions(root.join("bin/mix"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        if is_root() {
            std::os::unix::fs::chown(root.join("work"), Some(NOBODY), Some(NOBODY)).unwrap();
        }
        Sandbox { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// Make `path` (inside work/) belong to the runner.
    fn own(&self, path: &Path) {
        if is_root() {
            std::os::unix::fs::chown(path, Some(NOBODY), Some(NOBODY)).unwrap();
        }
    }

    /// Run `source` unprivileged; returns (success, stdout, stderr).
    fn run(&self, source: &str) -> (bool, String, String) {
        let script = self.root.join("script.mix");
        std::fs::File::create(&script)
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut command = Command::new(self.root.join("bin/mix"));
        command
            .arg(&script)
            .current_dir(self.work())
            .env("MIX_STATS", "off")
            .env("HOME", self.work())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if is_root() {
            command.uid(NOBODY).gid(NOBODY);
        }
        let out = command.output().expect("mix must run");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Restore search/read on any locked-down directory so cleanup works.
        if let Ok(entries) = std::fs::read_dir(self.work()) {
            for entry in entries.flatten() {
                let _ = std::fs::set_permissions(
                    entry.path(),
                    std::fs::Permissions::from_mode(0o755),
                );
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Review R3: a write+search-only directory (0300 — no read permission) is
/// writable with write_atomic, as it was before the directory was pinned; the
/// pin is O_PATH. The one level that must READ the directory — "full", which
/// fsyncs it — cannot, and says so with WRITE_NOT_DURABLE and
/// details.replaced, the new content being in place.
#[test]
fn a_write_search_only_directory_works_and_full_durability_says_it_cannot() {
    let sb = Sandbox::new("wx");
    let locked = sb.work().join("locked");
    std::fs::create_dir(&locked).unwrap();
    sb.own(&locked);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o300)).unwrap();
    let (ok, out, err) = sb.run(
        "print(uid() != 0)\n\
         write_atomic(\"locked/f\", \"one\")\n\
         print(\"plain ok\")\n\
         try\n  write_atomic(\"locked/f\", \"two\", {durability: \"full\"})\n\
         catch $m, $e\n  print($e.code .. \" \" .. $e.details.replaced)\nend\n",
    );
    assert!(ok, "script failed: {err}");
    assert_eq!(out, "true\nplain ok\nWRITE_NOT_DURABLE true\n", "stderr: {err}");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        std::fs::read_to_string(locked.join("f")).unwrap(),
        "two",
        "the full-durability write replaced the file before the dir fsync failed"
    );
}

/// Review R5: the final mode is applied AFTER the write, so an unprivileged
/// writer's requested setuid bit survives the kernel's write-clears-setuid
/// rule — on the first write and on a later replace. (Mode applied before
/// the write, the old order, fails this: the bit is gone after the write.)
#[test]
fn an_unprivileged_writer_keeps_a_requested_setuid_bit() {
    let sb = Sandbox::new("suid");
    let (ok, out, err) = sb.run(
        "print(uid() != 0)\n\
         write_atomic(\"tool\", \"#!/bin/sh\\n\", {mode: 0o4755})\n\
         print(stat(\"tool\")[\"perm\"])\n\
         write_atomic(\"tool\", \"#!/bin/sh\\n# v2\\n\")\n\
         print(stat(\"tool\")[\"perm\"])\n",
    );
    assert!(ok, "script failed: {err}");
    assert_eq!(out, "true\n2541\n2541\n", "stderr: {err}");
}
