//! Atomic file writes (invariant #6): write a unique sibling temp file, fsync it,
//! `rename(2)` it over the destination (atomic on POSIX within one filesystem),
//! then best-effort fsync the directory. A crash leaves either the prior file or
//! the complete new one — never a torn file. The temp is removed on error.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use uuid::Uuid;

use crate::error::{FilesError, Result};

/// Atomically replace (or create) `path` with `bytes`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        FilesError::Malformed(format!("path has no parent directory: {}", path.display()))
    })?;
    let fname = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
        FilesError::Malformed(format!("path has no file name: {}", path.display()))
    })?;
    let tmp = dir.join(format!(".{fname}.tmp.{}", Uuid::new_v4()));

    let written = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(FilesError::Io(e));
    }

    // Preserve the destination's permission bits if it already exists, so a 0600
    // note stays 0600 rather than reverting to the umask default. Best-effort, and
    // MODE only — ownership is not preserved (a root-run overwrite of a user-owned
    // file resets the owner; chown is deferred to the daemon if it matters).
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(FilesError::Io(e));
    }

    // Durability of the rename itself; best-effort (not fatal if unsupported).
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cosmix_files_atomic_{}", Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_then_overwrites_leaving_no_temp() {
        let dir = scratch_dir();
        let path = dir.join("note.md");

        write_atomic(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        // No leftover temp files in the directory.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_when_no_parent() {
        // Root has no parent component to host the temp file.
        let err = write_atomic(Path::new("/"), b"x");
        assert!(err.is_err());
    }
}
