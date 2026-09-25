//! Load, save and disk identities (plan §4.7).
//!
//! # Contract: two identities per buffer (frozen)
//! - `base` — the disk content the buffer's `saved_rev` corresponds to; set at
//!   load, save and clean reload. A DIRTY buffer never advances `base` from an
//!   observation (an observation whose content hash equals `base` may refresh
//!   its inode/mtime).
//! - `observed` — the latest identity the watcher saw.
//!
//! # Contract: save (frozen)
//! The actor first fixes the destination precondition [`Expect`]:
//! plain save → `Identity(base)` if the buffer has one, else `Absent`;
//! save-as → reserve the destination with the router (`path_open` if taken),
//! then stat it: absent → `Absent`; present without `force` → release,
//! CONFLICT `exists`; present with `force` → `Identity(that stat, hashed)`.
//! Then: `expect_rev` check (scratch without path → `scratch_needs_path`);
//! write `<dir>/.<name>.editd-<pid>-<n>.tmp` (mode of the file replaced; new
//! files `0o666 & !umask`) and fsync; REVALIDATE the destination against
//! `Expect` immediately before replacement (mismatch → remove temp, release any
//! reservation, CONFLICT `disk_modified`; a plain save with `force` skips the
//! comparison); `rename`; fsync the directory (failure after a committed rename
//! → saved with `durable: false`); record `base`, `mark_saved`; save-as then
//! commits the rebind through the router.
//!
//! Honest guarantee: an atomic REPLACEMENT with a revalidated precondition,
//! not a filesystem compare-and-swap; a writer landing between the final
//! `stat` and the `rename` is overwritten.


use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cosmix_edit_core::buffer::{Buffer, FileMeta};
use cosmix_edit_core::error::{ErrorCode, reason};
use cosmix_edit_core::limits::MAX_BUFFER_BYTES;
use cosmix_edit_core::wire::Refusal;

use crate::refusal::{RefusalExt, from_core, io_error, refusal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub blake3: [u8; 32],
}

/// The `stat` half of a [`DiskIdentity`] (no content hash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

impl DiskIdentity {
    pub fn stat(&self) -> Stat {
        Stat { dev: self.dev, ino: self.ino, size: self.size, mtime_ns: self.mtime_ns }
    }

    pub fn from_parts(stat: Stat, bytes: &[u8]) -> Self {
        Self {
            dev: stat.dev,
            ino: stat.ino,
            size: stat.size,
            mtime_ns: stat.mtime_ns,
            blake3: *blake3::hash(bytes).as_bytes(),
        }
    }
}

/// Save destination precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Absent,
    Identity(DiskIdentity),
}

/// A loaded file.
pub struct Loaded {
    pub buffer: Buffer,
    pub meta: FileMeta,
    pub canonical: PathBuf,
    pub base: DiskIdentity,
}

/// Outcome of a committed save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub base: DiskIdentity,
    pub file_bytes: usize,
    pub durable: bool,
    pub warning: Option<String>,
}

/// Largest file accepted: the text limit plus a UTF-8 BOM.
const MAX_FILE_BYTES: u64 = MAX_BUFFER_BYTES as u64 + 3;

fn bad_path(message: impl Into<String>) -> Refusal {
    refusal(ErrorCode::InvalidArgument, Some(reason::BAD_PATH), message)
}

/// Resolve `~/` against `HOME`, require absolute, canonicalise (`bad_path`).
///
/// An existing path resolves to its canonical target (symlinks followed). A
/// missing file resolves through its canonical parent, so `create: true` and
/// save-as bind the same spelling a later open of the created file will. A
/// DANGLING symlink resolves to the link's own path, so a save replaces the
/// link with a regular file (the one case where a symlink is not kept).
///
/// Both the given spelling and the result must JSON-encode within
/// `PATH_MAX_ENCODED_BYTES` (they are repeated in replies, props and events).
pub fn resolve_path(path: &str) -> Result<PathBuf, Refusal> {
    let max = crate::limits::PATH_MAX_ENCODED_BYTES;
    let too_long = |what: &str, n: usize| bad_path(format!("{what} encodes to {n} bytes; the limit is {max}"));
    let given = crate::events::encoded_len(&path);
    if given > max {
        return Err(too_long("the path", given));
    }
    let resolved = resolve_unbounded(path)?;
    let shown = resolved.display().to_string();
    let n = crate::events::encoded_len(&shown);
    if n > max {
        return Err(too_long("the resolved path", n));
    }
    Ok(resolved)
}

fn resolve_unbounded(path: &str) -> Result<PathBuf, Refusal> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| bad_path("HOME is not set, so ~/ cannot be resolved"))?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    };
    if !expanded.is_absolute() {
        return Err(bad_path("path must be absolute or start with ~/"));
    }
    if expanded.file_name().is_none() {
        return Err(bad_path(format!("{path} does not name a file")));
    }
    if let Ok(canonical) = std::fs::canonicalize(&expanded) {
        return Ok(canonical);
    }
    // Missing (or dangling): resolve the parent, keep the final name.
    let name = expanded.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    match expanded.parent().map(std::fs::canonicalize) {
        Some(Ok(parent)) => Ok(parent.join(name)),
        _ => Ok(expanded),
    }
}

/// The current `stat` of `path` (following symlinks), `None` when absent.
pub fn stat(path: &Path) -> std::io::Result<Option<Stat>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(Stat::of(&meta))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read a whole file bounded by `MAX_FILE_BYTES + 1`, with its `stat`.
pub fn read_bounded(path: &Path) -> std::io::Result<(Stat, Vec<u8>)> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::IsADirectory, "is a directory"));
    }
    let mut bytes = Vec::with_capacity(meta.len().min(MAX_FILE_BYTES + 1) as usize);
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    Ok((Stat::of(&meta), bytes))
}

/// Bounded load (stat size first; read at most `MAX_BUFFER_BYTES + 1`).
/// Blocking: call from `spawn_blocking`.
pub fn load(canonical: &Path) -> Result<Loaded, Refusal> {
    let shown = canonical.display().to_string();
    match std::fs::metadata(canonical) {
        Ok(meta) if meta.is_dir() => return Err(bad_path(format!("{shown} is a directory")).with("path", shown)),
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            return Err(too_large(&shown, meta.len()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(file_not_found(&shown)),
        Err(e) => return Err(io_error(&format!("reading {shown}"), &e).with("path", shown)),
    }
    let (stat, bytes) = match read_bounded(canonical) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(file_not_found(&shown)),
        Err(e) => return Err(io_error(&format!("reading {shown}"), &e).with("path", shown)),
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(&shown, bytes.len() as u64));
    }
    let base = DiskIdentity::from_parts(stat, &bytes);
    let (buffer, meta) = Buffer::from_bytes(&bytes).map_err(|e| {
        let mut r = from_core(e, None).with("path", shown.clone());
        if r.reason.as_deref() == Some(reason::NOT_UTF8) {
            r.message = format!("{shown} is not valid UTF-8 ({})", r.message);
        }
        r
    })?;
    Ok(Loaded { buffer, meta, canonical: canonical.to_path_buf(), base })
}

pub fn file_not_found(shown: &str) -> Refusal {
    refusal(
        ErrorCode::NotFound,
        Some(reason::FILE_NOT_FOUND),
        format!("{shown} does not exist; pass create:true to start it"),
    )
    .with("path", shown.to_string())
}

fn too_large(shown: &str, size: u64) -> Refusal {
    refusal(
        ErrorCode::ResourceLimit,
        Some(reason::TOO_LARGE),
        format!("{shown} is {size} bytes; the limit is {MAX_BUFFER_BYTES}"),
    )
    .with("path", shown.to_string())
}

/// Current identity of `path`, or `None` when absent. Blocking.
pub fn identity(path: &Path) -> std::io::Result<Option<DiskIdentity>> {
    match read_bounded(path) {
        Ok((stat, bytes)) => Ok(Some(DiskIdentity::from_parts(stat, &bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Test hook (ced E1 plan §6 E1c): saves into these directories fail their
/// post-rename directory fsync, i.e. commit with `durable: false`. Empty
/// outside tests.
#[doc(hidden)]
pub static FAIL_DIR_FSYNC: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    if FAIL_DIR_FSYNC.lock().map(|dirs| dirs.iter().any(|d| d == dir)).unwrap_or(false) {
        return Err(std::io::Error::other("injected directory fsync failure"));
    }
    File::open(dir).and_then(|d| d.sync_all())
}

/// Steps 2-5 of the save contract. Blocking.
///
/// `force` skips the step-3 comparison and is passed only for a PLAIN save
/// with `force: true`; a forced save-as passes `false` with
/// `Expect::Identity(<the stat it observed>)`, so nothing newer is overwritten.
///
/// The new `base` is the WRITTEN inode's own metadata, read through the temp
/// file's descriptor after its fsync (a rename changes neither its inode nor
/// its mtime) — never a post-rename `stat` of the path, which could observe
/// another writer's replacement and adopt its identity for our content.
pub fn save(dest: &Path, bytes: &[u8], expect: Expect, force: bool) -> Result<Saved, Refusal> {
    save_with(dest, bytes, expect, force, || {})
}

/// [`save`] with a hook run right after the rename (tests race it).
fn save_with(
    dest: &Path,
    bytes: &[u8],
    expect: Expect,
    force: bool,
    after_rename: impl FnOnce(),
) -> Result<Saved, Refusal> {
    let shown = dest.display().to_string();
    let (Some(dir), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Err(bad_path(format!("{shown} does not name a file")));
    };
    let tmp = dir.join(format!(
        ".{}.editd-{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write_err = |e: std::io::Error| io_error(&format!("writing {shown}"), &e).with("path", shown.clone());
    let existing_mode = std::fs::metadata(dest).ok().map(|m| m.permissions().mode() & 0o7777);

    // 2. Temp file (new files get 0o666 & !umask from the kernel), fsync.
    let written = (|| -> std::io::Result<Stat> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(&tmp)?;
        if let Some(mode) = existing_mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(Stat::of(&file.metadata()?))
    })();
    let written = match written {
        Ok(stat) => stat,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(write_err(e));
        }
    };

    // 3. Revalidate the destination immediately before replacing it.
    if !force {
        let matches = match expect {
            Expect::Absent => matches!(
                std::fs::symlink_metadata(dest),
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
            ),
            Expect::Identity(id) => matches!(stat(dest), Ok(Some(now)) if now == id.stat()),
        };
        if !matches {
            let _ = std::fs::remove_file(&tmp);
            let message = match expect {
                Expect::Absent => format!("{shown} was created on disk before the save; pass force:true to overwrite"),
                Expect::Identity(_) => {
                    format!("{shown} changed on disk since it was loaded; pass force:true to overwrite")
                }
            };
            return Err(refusal(ErrorCode::Conflict, Some(reason::DISK_MODIFIED), message).with("path", shown));
        }
    }

    // 4. Replace, then make the rename durable. From here the save HAS
    // happened: nothing below can turn it into a failed save.
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_err(e));
    }
    after_rename();
    let (durable, warning) = match sync_dir(dir) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(format!("saved, but fsync of {} failed: {e}", dir.display()))),
    };

    // 5. The new base: the inode we wrote (see the doc above).
    Ok(Saved { base: DiskIdentity::from_parts(written, bytes), file_bytes: bytes.len(), durable, warning })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".editd-"))
            .collect()
    }

    #[test]
    fn resolve_path_rules() {
        assert_eq!(resolve_path("rel/x").unwrap_err().reason.as_deref(), Some("bad_path"));
        let dir = tempfile::tempdir().unwrap();
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let missing = dir.path().join("new.txt");
        assert_eq!(resolve_path(missing.to_str().unwrap()).unwrap(), canonical_dir.join("new.txt"));
        let target = dir.path().join("t.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("l.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(resolve_path(link.to_str().unwrap()).unwrap(), canonical_dir.join("t.txt"));
    }

    #[test]
    fn save_new_file_absent_precondition_and_no_temp_left() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.txt");
        let saved = save(&dest, b"hello\n", Expect::Absent, false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello\n");
        assert_eq!(saved.file_bytes, 6);
        assert!(saved.durable);
        assert_eq!(saved.base, identity(&dest).unwrap().unwrap());
        assert!(temp_names(dir.path()).is_empty());
    }

    #[test]
    fn save_absent_refuses_when_created_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.txt");
        std::fs::write(&dest, "someone else").unwrap();
        let err = save(&dest, b"mine", Expect::Absent, false).unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("disk_modified"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"someone else");
        assert!(temp_names(dir.path()).is_empty());
    }

    #[test]
    fn save_preserves_mode_and_symlink_and_revalidates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t.sh");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canonical = resolve_path(link.to_str().unwrap()).unwrap();
        let base = identity(&canonical).unwrap().unwrap();
        save(&canonical, b"new", Expect::Identity(base), false).unwrap();
        assert_eq!(std::fs::read(&link).unwrap(), b"new");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777, 0o750);

        // The old base no longer matches: refused, file untouched.
        let err = save(&canonical, b"stale", Expect::Identity(base), false).unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("disk_modified"));
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        // A plain forced save skips the comparison.
        save(&canonical, b"forced", Expect::Identity(base), true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"forced");
        assert!(temp_names(dir.path()).is_empty());
    }

    #[test]
    fn save_base_is_the_written_inode_not_a_later_writer() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.txt");
        std::fs::write(&dest, "old").unwrap();
        let base = identity(&dest).unwrap().unwrap();
        // Another writer replaces the file right after our rename.
        let theirs = dir.path().join("theirs");
        let saved = save_with(&dest, b"mine", Expect::Identity(base), false, || {
            std::fs::write(&theirs, "their content").unwrap();
            std::fs::rename(&theirs, &dest).unwrap();
        })
        .unwrap();
        let now = stat(&dest).unwrap().unwrap();
        assert_ne!(saved.base.stat(), now, "the save adopted the other writer's identity");
        assert_eq!(saved.base.size, 4);
        assert_eq!(saved.base.blake3, *blake3::hash(b"mine").as_bytes());

        // The path vanishing after the rename is still a completed save.
        let saved = save_with(&dest, b"again", Expect::Identity(identity(&dest).unwrap().unwrap()), false, || {
            std::fs::remove_file(&dest).unwrap();
        });
        assert!(saved.is_ok(), "{saved:?}");
    }

    #[test]
    fn overlong_paths_are_bad_path() {
        let long = format!("/tmp/{}", "\u{1}".repeat(200)); // 6 encoded bytes each
        let err = resolve_path(&long).unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("bad_path"));
        assert!(crate::refusal::render(&err).1.len() < 4096, "the refusal does not echo the path");
    }

    #[test]
    fn save_into_missing_directory_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nope").join("a.txt");
        let err = save(&dest, b"x", Expect::Absent, false).unwrap_err();
        assert_eq!(err.error_code, ErrorCode::IoError);
        assert!(err.context.contains_key("errno"));
    }

    #[test]
    fn load_refuses_missing_directory_and_oversize_before_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&dir.path().join("missing")).err().unwrap();
        assert_eq!((err.error_code, err.reason.as_deref()), (ErrorCode::NotFound, Some("file_not_found")));
        let err = load(dir.path()).err().unwrap();
        assert_eq!(err.reason.as_deref(), Some("bad_path"));
        let big = dir.path().join("big");
        let file = File::create(&big).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        let err = load(&big).err().unwrap();
        assert_eq!((err.error_code, err.reason.as_deref()), (ErrorCode::ResourceLimit, Some("too_large")));
    }
}
