//! Atomic file writes (invariant #6): write a unique sibling temp file, fsync it,
//! `rename(2)` it over the destination (atomic on POSIX within one filesystem),
//! then best-effort fsync the directory. A crash leaves either the prior file or
//! the complete new one — never a torn file. The temp is removed on error.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use uuid::Uuid;

use crate::error::{FilesError, Result};

/// Stream, verify and publish a file without buffering the body in memory.
/// The parent must already exist. Failed reads, length/hash checks and local IO
/// leave the old target intact and remove staging. Existing mode bits survive;
/// new files use 0666 filtered by the umask. Ownership is not preserved.
/// With `overwrite=false`, Linux uses renameat2(RENAME_NOREPLACE), falling back
/// to link/unlink, then best-effort check/rename on filesystems without links.
/// Overwrites use rename, as `write_atomic` does.
pub fn land_verified(
    path: &Path,
    mut reader: impl Read,
    expected_len: u64,
    expected_hex: &str,
    overwrite: bool,
) -> Result<u64> {
    let dir = path.parent().ok_or_else(|| FilesError::BadRequest("target has no parent".into()))?;
    let name = path.file_name().ok_or_else(|| FilesError::BadRequest("target has no name".into()))?;
    let tmp = dir.join(format!(".{}.tmp.{}", name.to_string_lossy(), Uuid::new_v4()));
    let existing_permissions = if overwrite {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => Some(meta.permissions()),
            Ok(_) => return Err(FilesError::BadRequest("target is not a regular file".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    } else { None };
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Replacement bytes must stay private throughout streaming, before
        // restoring the target's permissions immediately prior to publication.
        options.mode(if existing_permissions.is_some() { 0o600 } else { 0o666 });
    }
    let mut file = options.open(&tmp)?;
    // Arm only after create_new succeeds: never unlink someone else's entry.
    let cleanup = TempFile(tmp.clone());
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FilesError::SourceRead(e)),
        };
        if n == 0 {
            break;
        }
        if n as u64 > expected_len.saturating_sub(count) {
            return Err(FilesError::VerifyFailed("body longer than Content-Length".into()));
        }
        count += n as u64;
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    if count != expected_len {
        return Err(FilesError::VerifyFailed(format!(
            "body length {count}, expected {expected_len}"
        )));
    }
    let actual = hasher.finalize().to_hex().to_string();
    if actual != expected_hex {
        return Err(FilesError::VerifyFailed(format!(
            "hash mismatch: expected {expected_hex}, got {actual}"
        )));
    }
    if let Some(permissions) = existing_permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    drop(file);
    if overwrite {
        fs::rename(&tmp, path)?;
    } else {
        publish_noreplace(&tmp, path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                FilesError::Exists(format!("already exists (overwrite=false): {}", path.display()))
            } else {
                FilesError::Io(e)
            }
        })?;
    }
    drop(cleanup);
    sync_directory_with(dir, |path, error| {
        eprintln!("cosmix-files: directory fsync {}: {error}", path.display());
    });
    Ok(count)
}

fn publish_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    publish_noreplace_with(from, to, rename_noreplace, |from, to| fs::hard_link(from, to))
}

fn publish_noreplace_with(
    from: &Path,
    to: &Path,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match rename(from, to) {
        Ok(()) => return Ok(()),
        Err(e) if rename_needs_fallback(&e) => {}
        Err(e) => return Err(e),
    }
    match link(from, to) {
        Ok(()) => Ok(()), // TempFile removes the staging name.
        Err(e) if link_needs_fallback(&e) => {
            // Last-resort tier: not atomic against a concurrent target create.
            // symlink_metadata counts dangling symlinks as existing entries too.
            match fs::symlink_metadata(to) {
                Ok(_) => Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "target exists")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::rename(from, to),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: the two NUL-terminated paths stay alive throughout the call.
    #[cfg(target_env = "gnu")]
    let rc = unsafe {
        libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE)
    };
    // libc exposes the syscall number but not the wrapper on musl.
    #[cfg(not(target_env = "gnu"))]
    let rc = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE)
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "renameat2 unavailable"))
}

fn rename_needs_fallback(e: &std::io::Error) -> bool {
    #[cfg(target_os = "linux")]
    { matches!(e.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)) }
    #[cfg(not(target_os = "linux"))]
    { e.kind() == std::io::ErrorKind::Unsupported }
}

fn link_needs_fallback(e: &std::io::Error) -> bool {
    #[cfg(target_os = "linux")]
    {
        // ENOTSUP aliases EOPNOTSUPP on Linux: compare without duplicate patterns.
        e.raw_os_error().is_some_and(|code| [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&code))
    }
    #[cfg(not(target_os = "linux"))]
    { e.kind() == std::io::ErrorKind::Unsupported }
}

struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        cleanup_temp_with(&self.0, |path, error| {
            eprintln!("cosmix-files: staging cleanup {}: {error}", path.display());
        });
    }
}

fn cleanup_temp_with(path: &Path, report: impl FnOnce(&Path, &std::io::Error)) {
    if let Err(error) = fs::remove_file(path) {
        // Successful rename has already consumed the staging name.
        if error.kind() != std::io::ErrorKind::NotFound { report(path, &error); }
    }
}

fn sync_directory_with(path: &Path, report: impl FnOnce(&Path, &std::io::Error)) {
    if let Err(error) = File::open(path).and_then(|dir| dir.sync_all()) {
        report(path, &error);
    }
}

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

    fn hash(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn no_temp(dir: &Path) {
        assert!(fs::read_dir(dir).unwrap().all(|e| {
            !e.unwrap().file_name().to_string_lossy().contains(".tmp.")
        }));
    }

    #[test]
    fn verified_land_and_no_clobber() {
        let dir = scratch_dir();
        let path = dir.join("blob");
        assert_eq!(land_verified(&path, &b"hello"[..], 5, &hash(b"hello"), false).unwrap(), 5);
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
        }
        assert!(matches!(
            land_verified(&path, &b"other"[..], 5, &hash(b"other"), false),
            Err(FilesError::Exists(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        no_temp(&dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verification_failures_leave_no_target_or_temp() {
        let dir = scratch_dir();
        let path = dir.join("blob");
        for (len, expected) in [(5, hash(b"wrong")), (6, hash(b"hello")), (4, hash(b"hello"))] {
            assert!(matches!(
                land_verified(&path, &b"hello"[..], len, &expected, false),
                Err(FilesError::VerifyFailed(_))
            ));
            assert!(!path.exists());
            no_temp(&dir);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_replace_fallback_selection() {
        let dir = scratch_dir();
        let from = dir.join("staging");
        let to = dir.join("target");
        fs::write(&from, b"new").unwrap();
        for code in [libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP] {
            let called = std::cell::Cell::new(false);
            publish_noreplace_with(&from, &to,
                |_, _| Err(std::io::Error::from_raw_os_error(code)),
                |_, _| { called.set(true); Ok(()) }).unwrap();
            assert!(called.get());
        }
        assert!(publish_noreplace_with(&from, &to,
            |_, _| Err(std::io::Error::from_raw_os_error(libc::EACCES)),
            |_, _| panic!("permission error must not fall through")).is_err());
        for code in [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP] {
            fs::write(&to, b"old").unwrap();
            let error = publish_noreplace_with(&from, &to,
                |_, _| Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
                |_, _| Err(std::io::Error::from_raw_os_error(code))).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(&to).unwrap(), b"old");
            fs::remove_file(&to).unwrap();
        }
        publish_noreplace_with(&from, &to,
            |_, _| Err(std::io::Error::from_raw_os_error(libc::ENOSYS)),
            |_, _| Err(std::io::Error::from_raw_os_error(libc::EPERM))).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reader_error_preserves_existing_target_and_cleans_temp() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("connection reset"))
            }
        }
        let dir = scratch_dir();
        let path = dir.join("blob");
        fs::write(&path, b"old").unwrap();
        assert!(matches!(land_verified(&path, Broken, 5, &hash(b"hello"), true), Err(FilesError::SourceRead(_))));
        assert_eq!(fs::read(&path).unwrap(), b"old");
        no_temp(&dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_overwrite_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir();
        let path = dir.join("blob");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        land_verified(&path, &b"new"[..], 3, &hash(b"new"), true).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        no_temp(&dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_land_requires_parent() {
        let dir = scratch_dir();
        assert!(land_verified(&dir.join("absent/blob"), &b"x"[..], 1, &hash(b"x"), false).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_and_directory_sync_failures_are_reported_nonfatally() {
        let dir = scratch_dir();
        let mut cleanup_reported = false;
        // remove_file cannot unlink a directory: preserve it and report it.
        cleanup_temp_with(&dir, |path, _| {
            assert_eq!(path, dir);
            cleanup_reported = true;
        });
        assert!(cleanup_reported && dir.is_dir());
        let absent = dir.join("absent");
        cleanup_temp_with(&absent, |_, _| panic!("rename-consumed temp is not a failure"));
        let mut sync_reported = false;
        sync_directory_with(&absent, |path, error| {
            assert_eq!(path, absent);
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            sync_reported = true;
        });
        assert!(sync_reported);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_overwrite_staging_is_private_while_streaming() {
        use std::os::unix::fs::PermissionsExt;
        struct InspectReader<'a> {
            dir: &'a Path,
            body: &'a [u8],
            reads: usize,
        }
        impl Read for InspectReader<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let tmp = fs::read_dir(self.dir)?.map(|e| e.unwrap().path())
                    .find(|p| p.file_name().unwrap().to_string_lossy().contains(".tmp."))
                    .expect("staging file must exist while reading");
                assert_eq!(fs::metadata(tmp)?.permissions().mode() & 0o777, 0o600);
                self.reads += 1;
                // Several reads inspect the file both before and after bytes land.
                let count = buf.len().min(2).min(self.body.len());
                buf[..count].copy_from_slice(&self.body[..count]);
                self.body = &self.body[count..];
                Ok(count)
            }
        }
        let dir = scratch_dir();
        let path = dir.join("private");
        for mode in [0o600, 0o640] {
            fs::write(&path, b"old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let mut reader = InspectReader { dir: &dir, body: b"secret", reads: 0 };
            land_verified(&path, &mut reader, 6, &hash(b"secret"), true).unwrap();
            assert!(reader.reads > 2);
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, mode);
            assert_eq!(fs::read(&path).unwrap(), b"secret");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_new_file_matches_write_atomic_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir();
        let ordinary = dir.join("ordinary");
        let blob = dir.join("blob");
        write_atomic(&ordinary, b"x").unwrap();
        land_verified(&blob, &b"x"[..], 1, &hash(b"x"), false).unwrap();
        assert_eq!(fs::metadata(&blob).unwrap().permissions().mode() & 0o777,
            fs::metadata(&ordinary).unwrap().permissions().mode() & 0o777);
        fs::remove_dir_all(dir).unwrap();
    }

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
