// Test-fixture support shared by unit and integration tests: ONE definition,
// no public API, no Cargo manifest edit (versioning is the refinery's job at
// landing). Compiled into the lib's unit tests via `#[cfg(test)] mod
// fixture;` in lib.rs, and textually included into every integration-test
// binary via tests/support/mod.rs (`include!("../../src/fixture.rs")`). The
// file must therefore avoid inner attributes and `crate::` paths — neither
// survives both contexts. Items carry no `#[allow(dead_code)]` here; the
// integration side allows it once in tests/support/mod.rs because a test
// binary may include the file while using only part of it.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs::OpenOptions, io::Write};

/// Distinct staging name per call, so concurrent fixture writers in one test
/// process never stage over each other.
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `body` to `path` and make it executable (0o755), such that NO
/// descriptor on `path`'s final inode is ever open in a forkable process
/// once this returns.
///
/// Why the shape (task 98, 2026-08-29): a test writes a fake executable and
/// execs it microseconds later. `std::fs::write` opens O_CLOEXEC and closes
/// the descriptor before returning, so the writing thread holds no handle at
/// exec time — there was never anything to close. The observed flake (task
/// 95's landing: `Text file busy (os error 26)` at tests/sandbox.rs:127) is
/// a descriptor INHERITED BY A CONCURRENT FORK: while the write's fd was
/// open, a sibling test thread's `Command::spawn` forked and copied the fd
/// table; the forked child (and anything it forks before its own exec)
/// holds that write-mode fd — kernel `i_writecount > 0` — and any `execve`
/// of the inode in that window fails ETXTBSY, process-wide, for every exec
/// shape: the test's own driver spawn, the bwrap sandbox's inner exec of
/// the same file, and the `foreman` child's later `FOREMAN_*_BIN` exec. An
/// exec-side retry cannot reach the last two, and a post-write scan of
/// /proc for holders is a heuristic that can report "clear" while a fork
/// still in flight has not yet shown up. So the treatment is neither: it is
/// construction, on the write side.
///
/// The final bytes are written by a CHILD `cp` from a staging file: the
/// body is first written in-process to a unique staging file (that inode is
/// never exec'd, so any descriptor a concurrent fork inherits on it is
/// harmless), then `cp stage dest` creates/opens the destination and the
/// helper waits for it to exit. The only write-mode descriptor that ever
/// exists on the destination inode is opened INSIDE the cp child, after
/// cp's own exec — forks of the test process copy the test's table, which
/// contains no destination fd, and cp forks nothing, so no other process
/// can acquire one. When cp exits, `i_writecount` is 0 and can only stay
/// 0; the 0o755 step is `chmod(2)` by path, which opens no descriptor.
/// Every later exec of every shape therefore sees a clean inode — the race
/// is impossible, not merely rarer, and no exec-side retry is needed.
pub fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    let stage = staging_path(path);
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)
        .unwrap_or_else(|e| panic!("open fixture stage {}: {e}", stage.display()));
    writer
        .write_all(body.as_ref())
        .unwrap_or_else(|e| panic!("write fixture stage {}: {e}", stage.display()));
    writer
        .flush()
        .unwrap_or_else(|e| panic!("flush fixture stage {}: {e}", stage.display()));
    // Do not rely on scope end: the parent must close its only writable
    // fixture descriptor before it forks cp. A concurrent fork can inherit
    // this staging fd, but that inode is never exec'd.
    drop(writer);
    let copied = Command::new("cp")
        .arg(&stage)
        .arg(path)
        .status()
        .unwrap_or_else(|e| panic!("spawn cp for {}: {e}", path.display()));
    // Best-effort staging cleanup before any assert: a panic must not leave
    // stage files behind in non-tempdir fixture homes.
    let _ = std::fs::remove_file(&stage);
    assert!(
        copied.success(),
        "cp {:?} -> {:?} failed with {copied}",
        stage,
        path
    );
    let mut permissions = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat fixture {}: {e}", path.display()))
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .unwrap_or_else(|e| panic!("chmod fixture {}: {e}", path.display()));
}

/// `<dir>/<name>.<pid>.<n>.stage` beside the destination: unique per call
/// and per process, and on the destination's own filesystem.
fn staging_path(path: &Path) -> std::path::PathBuf {
    let n = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{}.stage", std::process::id(), n));
    path.with_file_name(name)
}
