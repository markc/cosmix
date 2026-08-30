//! Small filesystem helpers shared by CTK apps.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local nonce so concurrent writers (even to the same target) never
/// collide on a temp filename.
static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path` with `bytes`: write to a freshly-created sibling
/// temp file, fsync it, rename it over the target, then fsync the parent
/// directory. Creates the parent directory if missing.
///
/// Durability/atomicity contract:
/// - The temp file is created with `create_new` (O_EXCL) under a per-call
///   unique name, so a symlink pre-planted at the temp path is rejected and
///   two concurrent calls never share an inode.
/// - The rename is atomic: a reader sees either the old or the new file, never
///   a partial one. The temp is a sibling of the target, so the rename stays
///   on one filesystem.
/// - If anything fails **before** the rename, the original `path` is untouched
///   and the temp file is removed. If the rename succeeds but the final
///   parent-directory fsync fails, the new content **is** in place — the call
///   returns `Err` to signal that its durability across a crash is not
///   guaranteed, NOT that the write was rolled back.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("creating {}: {error}", parent.display()))?;

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;

    // Create a uniquely-named temp file, retrying on the (vanishingly rare)
    // AlreadyExists so a stale temp never wedges the write.
    let pid = std::process::id();
    let (mut file, temporary) = {
        let mut attempt = 0;
        loop {
            let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".{name}.tmp-{pid}-{nonce}"));
            match OpenOptions::new()
                .create_new(true) // O_EXCL: fails on any existing path incl. a symlink
                .write(true)
                .open(&candidate)
            {
                Ok(file) => break (file, candidate),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempt += 1;
                    if attempt >= 1000 {
                        return Err(format!(
                            "could not create a unique temp file next to {}",
                            path.display()
                        ));
                    }
                }
                Err(error) => return Err(format!("opening temp for {}: {error}", path.display())),
            }
        }
    };

    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| format!("writing {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("syncing {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("replacing {}: {error}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("syncing {}: {error}", parent.display()))
    })();

    if result.is_err() {
        // Best-effort: only removes the temp if the rename never happened
        // (after a successful rename the temp no longer exists at this path).
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_then_reads_back() {
        let dir = std::env::temp_dir().join(format!("ctk-fs-test-{}", std::process::id()));
        let target = dir.join("nested/out.txt");
        write_atomic(&target, b"hello").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        // overwrite is atomic + replaces cleanly
        write_atomic(&target, b"world").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"world");
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
