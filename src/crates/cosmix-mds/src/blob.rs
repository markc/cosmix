//! Content-addressable blob storage.
//!
//! Atomic CAS write protocol per spec §Invariants 6:
//!   1. Write bytes to `blobs/.tmp/<uuid>`.
//!   2. fsync the temp file.
//!   3. Hard-link the temp file to `blobs/<hh>/<hh>/<hash>`.
//!   4. fsync the parent directory.
//!   5. Unlink the temp file (best-effort).
//!
//! Crash between steps 3 and 4 leaves a discoverable but un-fsynced
//! link, which an OS-level crash may erase; that's acceptable —
//! the data.sqlite truth has not yet committed and the missing
//! blob will surface as a verify failure that the caller can
//! retry. Crash between any earlier steps leaves only the temp
//! file, swept by `gc()`.
//!
//! Phase 1b deliberately does NOT update blobs.sqlite (refcount,
//! blob_ref, refcount_pending). That cross-DB write lands in
//! Phase 3 once the ATTACH DATABASE pattern is wired.

use crate::error::{Error, Result};
use crate::types::BlobHash;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Compute the BLAKE3 of `bytes` and return our newtype wrapper.
pub fn hash_bytes(bytes: &[u8]) -> BlobHash {
    BlobHash(blake3::hash(bytes).into())
}

/// Hex-encoded blob hash (lowercase, 64 chars). Used both for the
/// on-disk file path and for the SQL string column.
pub fn hex(hash: &BlobHash) -> String {
    let mut out = String::with_capacity(64);
    for b in hash.0.iter() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

pub fn blob_path(blobs_root: &Path, hash: &BlobHash) -> PathBuf {
    let h = hex(hash);
    blobs_root.join(&h[0..2]).join(&h[2..4]).join(h)
}

/// Inverse of `hex`: parse a 64-char lowercase hex string back into
/// a `BlobHash`. Returns `None` for any other length / character set
/// — used by GC to round-trip hashes from blobs.sqlite TEXT columns
/// to the on-disk path layout.
pub fn from_hex(s: &str) -> Option<BlobHash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        out[i] = byte;
    }
    Some(BlobHash(out))
}

/// How [`put_path`] lands the source file's bytes in the CAS. Every
/// mode ends with the same stage → fsync → hard-link-into-CAS commit
/// and re-hashes the staged bytes against the ingest-time hash before
/// linking, so the choice is purely about the copy, not the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMode {
    /// Plain userspace copy. Always available; the baseline the other
    /// modes fall back to.
    Copy,
    /// Kernel-side copy: `FICLONE` reflink where the filesystem has
    /// one (btrfs, XFS `reflink=1`, OpenZFS ≥ 2.3), then
    /// `copy_file_range` (block clone on OpenZFS 2.2, an ordinary
    /// copy elsewhere), then a userspace copy. Never fails merely
    /// because the kernel path did not work — any error from `FICLONE`
    /// or `copy_file_range` falls through to the next option (OpenZFS
    /// with block cloning off answers `EPERM`, not `EOPNOTSUPP`).
    Reflink,
    /// Hard-link the source inode into the CAS — no bytes copied.
    /// Only for publishers that promise the source path immutable
    /// from the call onward (the blob store's own staging); never for
    /// a mutable path such as a filesd place, whose contract is
    /// "someone else may rewrite this path". The CAS entry is the
    /// source inode, so it keeps the producer's owner, group and mode —
    /// shared-group readers (blobd's `cosmix-blob`) see it only if the
    /// producer's file was group-readable; `Copy`/`Reflink` entries
    /// always inherit the CAS directory's group.
    HardLink,
}

/// Set a file's atime and mtime to now (`utimensat` with null
/// timespecs). The GC grace window is mtime-based, so every path that
/// lands on or re-acknowledges an existing CAS entry refreshes it: an
/// idempotent re-put of bytes whose file has gone quiescent must not
/// present a months-old mtime to a concurrent sweep (blobd's M1/F3).
pub fn touch_path(p: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| Error::Io(std::io::Error::other("path contains a NUL byte")))?;
    // SAFETY: utimensat on a live path with a null timespec pointer
    // sets both timestamps to now; no pointers to Rust data.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) };
    if rc != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Stream `r` into the CAS, hashing with BLAKE3 while the bytes are
/// staged — the hash is only known at stream end, and the write
/// protocol already tolerates that (the idempotence check happens
/// after staging instead of before). Returns the hash and the byte
/// count. A reader error mid-stream removes the staged file and
/// leaves no CAS entry.
///
/// Idempotent hits refresh the existing file's mtime (best-effort —
/// see [`touch_path`]) so a GC grace window judged on mtime covers the
/// re-acknowledged bytes, not their first landing.
pub fn put_reader(blobs_root: &Path, r: impl Read) -> Result<(BlobHash, u64)> {
    let (hash, size, tmp_path) = stage(blobs_root, r)?;
    if blob_path(blobs_root, &hash).exists() {
        // Idempotent: identical bytes are already committed. Drop the
        // staged copy rather than rewriting the entry, and refresh the
        // committed file's grace window.
        let _ = fs::remove_file(&tmp_path);
        let _ = touch_path(&blob_path(blobs_root, &hash));
        return Ok((hash, size));
    }

    commit_staged(blobs_root, &tmp_path, &hash)?;
    Ok((hash, size))
}

/// [`put_reader`] for a caller that already knows the hash the bytes
/// must land under (a lane `PUT /blob/<hex>`, a cross-node fetch by
/// reference). The stream is staged and hashed exactly like
/// `put_reader`, but the comparison happens **before**
/// [`commit_staged`]: a body that does not hash to `expected` is
/// removed from staging and reported as [`Error::BlobCorrupt`] — it
/// never enters the CAS under either its real hash or the expected
/// one, so no caller has to unlink a wrong-hash landing afterwards.
///
/// Idempotent: the stream is always consumed and hashed (whether the
/// body is read at all is the caller's decision — the blob lane
/// answers `200` without reading when the expected hash is already
/// present), and a mismatch is reported even then: the arriving bytes
/// are wrong for the id regardless of what the CAS already holds.
pub fn put_reader_expect(
    blobs_root: &Path,
    r: impl Read,
    expected: &BlobHash,
) -> Result<(BlobHash, u64)> {
    let (landed, size, tmp_path) = stage(blobs_root, r)?;
    if &landed != expected {
        let _ = fs::remove_file(&tmp_path);
        return Err(Error::BlobCorrupt(format!(
            "hash mismatch: body hashes to {}, not the expected {}",
            hex(&landed),
            hex(expected)
        )));
    }
    if blob_path(blobs_root, &landed).exists() {
        let _ = fs::remove_file(&tmp_path);
        let _ = touch_path(&blob_path(blobs_root, &landed));
        return Ok((landed, size));
    }

    commit_staged(blobs_root, &tmp_path, &landed)?;
    Ok((landed, size))
}

/// Step 1 of the write protocol, shared by [`put_reader`] and
/// [`put_reader_expect`] so the two can never drift: stage `r` in
/// `.tmp/<uuid>` while hashing with BLAKE3. A reader error mid-stream
/// removes the staged file.
fn stage(blobs_root: &Path, r: impl Read) -> Result<(BlobHash, u64, PathBuf)> {
    // UUIDv4 is fine here — uniqueness over a tiny temp window, no
    // ordering needed.
    let tmp_dir = blobs_root.join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(uuid::Uuid::new_v4().to_string());
    let mut hasher = blake3::Hasher::new();
    let mut size: u64 = 0;
    if let Err(e) = stream_to_tmp(r, &tmp_path, &mut hasher, &mut size) {
        let _ = fs::remove_file(&tmp_path);
        return Err(Error::Io(e));
    }
    Ok((BlobHash(hasher.finalize().into()), size, tmp_path))
}

/// In-memory ingest; a thin wrapper over [`put_reader`].
pub fn put(blobs_root: &Path, bytes: &[u8]) -> Result<BlobHash> {
    let (hash, _) = put_reader(blobs_root, bytes)?;
    Ok(hash)
}

/// Steps 2–5 of the write protocol for a staged temp file whose
/// content hashes to `hash`: fsync the staged bytes, hard-link the
/// temp file into the sharded CAS path (tolerating the concurrent
/// same-hash race), fsync the parent directory, and unlink the temp
/// file. The single home of the protocol — `put_reader` and
/// `put_path` both land here.
///
/// Every error path removes the staged file (m1): the streaming
/// callers hand `tmp_path` over with no guard of their own, so a
/// failed fsync or shard `create_dir_all` here would otherwise leave
/// a `.tmp` corpse until the next restart sweep.
fn commit_staged(blobs_root: &Path, tmp_path: &Path, hash: &BlobHash) -> Result<()> {
    let committed = commit_staged_inner(blobs_root, tmp_path, hash);
    if committed.is_err() {
        let _ = fs::remove_file(tmp_path);
    }
    committed
}

fn commit_staged_inner(blobs_root: &Path, tmp_path: &Path, hash: &BlobHash) -> Result<()> {
    // 2. fsync temp. Opened read-only: fsync flushes the inode, not
    // the fd's write mode.
    File::open(tmp_path)?.sync_all()?;

    // 3. Ensure parent shard exists, then hard-link.
    let final_path = blob_path(blobs_root, hash);
    let parent = final_path.parent().expect("blob path has parent");
    fs::create_dir_all(parent)?;
    match fs::hard_link(tmp_path, &final_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Race: another writer placed the same hash. Both inputs
            // are identical (CAS), so dropping ours is safe.
        }
        Err(e) => return Err(Error::Io(e)),
    }

    // 4. fsync the parent directory so the link survives a crash.
    fsync_dir(parent)?;

    // 5. Best-effort temp cleanup.
    let _ = fs::remove_file(tmp_path);

    Ok(())
}

fn stream_to_tmp(
    mut r: impl Read,
    tmp_path: &Path,
    hasher: &mut blake3::Hasher,
    size: &mut u64,
) -> std::io::Result<()> {
    let mut f = File::create(tmp_path)?;
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        f.write_all(&buf[..n])?;
        *size += n as u64;
    }
    Ok(())
}

/// Ingest the file at `src` into the CAS under `mode`. The source is
/// hashed first — the ingest-time hash is the truth; any index hash
/// the caller holds is advisory — then landed, re-hashed at the
/// staging point, and only then linked in. A mismatch between the
/// two ([`Error::BlobCorrupt`]) removes the staged file and leaves
/// the CAS untouched: a source rewritten mid-copy never enters the
/// store under a stale hash. For `HardLink` the staged file *is* the
/// source inode, so the second hash also catches a broken
/// immutability promise mid-call.
///
/// Idempotent: if the CAS already holds the hash, nothing is written
/// (the source is still read once — the hash must be computed) and the
/// existing file's mtime is refreshed (best-effort, see
/// [`touch_path`]) so the GC grace window covers the re-acknowledged
/// bytes.
pub fn put_path(blobs_root: &Path, src: &Path, mode: PutMode) -> Result<(BlobHash, u64)> {
    let (hash, size) = hash_file(src)?;
    if blob_path(blobs_root, &hash).exists() {
        let _ = touch_path(&blob_path(blobs_root, &hash));
        return Ok((hash, size));
    }

    let tmp_dir = blobs_root.join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(uuid::Uuid::new_v4().to_string());

    // One remove-on-error guard over the whole staging window (F4):
    // every failure below — the copy, the staged re-hash, the commit —
    // removes the staged file before the error surfaces, exactly like
    // stage() does for the streaming paths.
    let staged = put_path_staged(blobs_root, src, mode, &hash, &tmp_path);
    if staged.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    staged.map(|_| (hash, size))
}

/// The post-staging half of [`put_path`]: land the bytes under `mode`,
/// re-hash them at the staging point, and commit. Only called with a
/// fresh `tmp_path`; the caller owns the remove-on-error guard.
fn put_path_staged(
    blobs_root: &Path,
    src: &Path,
    mode: PutMode,
    hash: &BlobHash,
    tmp_path: &Path,
) -> Result<()> {
    match mode {
        PutMode::Copy => {
            let mut src_f = File::open(src)?;
            let mut tmp_f = File::create(tmp_path)?;
            std::io::copy(&mut src_f, &mut tmp_f)?;
        }
        PutMode::Reflink => {
            let mut src_f = File::open(src)?;
            let mut tmp_f = File::create(tmp_path)?;
            if !try_kernel_copy(&mut src_f, &mut tmp_f)? {
                // Soft fall-through: both fds sit at offset 0 with an
                // empty staging file (FICLONE is atomic; a mid-copy
                // fall-through rewinds itself), so the userspace copy
                // starts clean.
                std::io::copy(&mut src_f, &mut tmp_f)?;
            }
        }
        PutMode::HardLink => {
            // Stage a link to the source inode; commit_staged then
            // fsyncs it, links it into the CAS, and drops the staging
            // link. The caller has promised the source immutable.
            fs::hard_link(src, tmp_path)?;
            // The staged link shares the source's inode, so without
            // this the CAS file would carry the *source's* mtime — a
            // months-old file is "old" the instant it commits, and a
            // GC that judges on mtime sweeps it in the commit→pin
            // window (F3). Refresh it to now. The inode is shared, so
            // the source's mtime moves with it — the caller already
            // promised the source immutable from the call onward, and
            // an mtime is not content.
            touch_path(tmp_path)?;
        }
    }

    let (landed_hash, _) = hash_file(tmp_path)?;
    if landed_hash != *hash {
        return Err(Error::BlobCorrupt(format!(
            "put_path: staged bytes hash to {} but source hashed to {}",
            hex(&landed_hash),
            hex(hash)
        )));
    }

    commit_staged(blobs_root, tmp_path, hash)?;
    Ok(())
}

/// Hash the file at `p` with BLAKE3, streaming — never more than the
/// buffer in memory. Returns the hash and the byte count.
fn hash_file(p: &Path) -> Result<(BlobHash, u64)> {
    let mut f = File::open(p)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((BlobHash(hasher.finalize().into()), size))
}

/// Kernel-side copy for [`PutMode::Reflink`]: `FICLONE` reflink
/// first, `copy_file_range` second. Returns `Ok(false)` when neither
/// completed and the caller should finish with a userspace copy.
///
/// The kernel path is best-effort by contract, so *any* failure falls
/// through — not an errno allowlist. The allowlist version
/// (`EOPNOTSUPP`/`EXDEV`/`EINVAL`/`ENOSYS`) failed on the build
/// cluster: OpenZFS answers `FICLONE` with `EPERM` when block cloning
/// is disabled (`zfs_bclone_enabled=0`, the 2.2.x default), and a
/// container's seccomp profile can refuse the ioctl outright. Whatever
/// the errno, the userspace copy that follows either succeeds or
/// surfaces the real error itself — so nothing is hidden by falling
/// through, and something is lost by not doing so.
///
/// Only `Err` for the rewind/truncate that resets the staging file: if
/// that fails the fallback cannot start clean and the put must stop.
fn try_kernel_copy(src: &mut File, dst: &mut File) -> std::io::Result<bool> {
    // SAFETY: both fds are live files; FICLONE takes the source fd as
    // its ioctl argument and clones from it into `dst`.
    let ret = unsafe {
        libc::ioctl(
            dst.as_raw_fd(),
            libc::FICLONE,
            src.as_raw_fd() as libc::c_ulong,
        )
    };
    if ret == 0 {
        return Ok(true);
    }
    // FICLONE is atomic: on failure nothing was written and neither fd
    // moved, so copy_file_range starts from offset 0.
    if copy_file_range_all(src, dst).is_ok() {
        return Ok(true);
    }
    // Restart the userspace fallback from a clean slate: a failure
    // mid-copy leaves both fds advanced and the staging file partially
    // written.
    src.rewind()?;
    dst.set_len(0)?;
    dst.rewind()?;
    Ok(false)
}

/// `copy_file_range(2)` loop using (and advancing) the fds' own
/// positions; a return of 0 means EOF — the copy is complete.
fn copy_file_range_all(src: &File, dst: &File) -> std::io::Result<()> {
    // The kernel copies what it can server-side per call; 4 MiB keeps
    // each call inside ssize_t with no syscall storm.
    const CHUNK: usize = 4 * 1024 * 1024;
    loop {
        // SAFETY: both fds are live files; null offset pointers make
        // the call use and advance the fds' own positions.
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                CHUNK,
                0,
            )
        };
        if n == 0 {
            return Ok(());
        }
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
}

pub fn get(blobs_root: &Path, hash: &BlobHash) -> Result<Vec<u8>> {
    let p = blob_path(blobs_root, hash);
    match fs::read(&p) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::BlobNotFound(hex(hash))),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Open the CAS file for `hash` for streaming reads — the read-side
/// counterpart of [`put_reader`], so a caller never has to hold a
/// blob in memory as a whole. `BlobNotFound` when the hash is not
/// present.
pub fn open(blobs_root: &Path, hash: &BlobHash) -> Result<File> {
    let p = blob_path(blobs_root, hash);
    match File::open(&p) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::BlobNotFound(hex(hash))),
        Err(e) => Err(Error::Io(e)),
    }
}

pub fn size(blobs_root: &Path, hash: &BlobHash) -> Result<u64> {
    let p = blob_path(blobs_root, hash);
    match fs::metadata(&p) {
        Ok(m) => Ok(m.size()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::BlobNotFound(hex(hash))),
        Err(e) => Err(Error::Io(e)),
    }
}

pub fn exists(blobs_root: &Path, hash: &BlobHash) -> Result<bool> {
    let p = blob_path(blobs_root, hash);
    Ok(p.exists())
}

fn fsync_dir(p: &Path) -> Result<()> {
    let f = File::open(p)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root() -> TempDir {
        let d = TempDir::new().unwrap();
        std::fs::create_dir_all(d.path().join(".tmp")).unwrap();
        d
    }

    #[test]
    fn put_then_get_roundtrips() {
        let d = root();
        let h = put(d.path(), b"hello world").unwrap();
        let bytes = get(d.path(), &h).unwrap();
        assert_eq!(bytes, b"hello world");
        assert_eq!(size(d.path(), &h).unwrap(), 11);
        assert!(exists(d.path(), &h).unwrap());
    }

    #[test]
    fn put_is_idempotent_for_identical_bytes() {
        let d = root();
        let h1 = put(d.path(), b"identical").unwrap();
        let h2 = put(d.path(), b"identical").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn empty_blob_is_supported() {
        let d = root();
        let h = put(d.path(), b"").unwrap();
        assert_eq!(get(d.path(), &h).unwrap(), b"");
        assert_eq!(size(d.path(), &h).unwrap(), 0);
    }

    #[test]
    fn missing_hash_returns_not_found() {
        let d = root();
        let bogus = hash_bytes(b"never written");
        match get(d.path(), &bogus) {
            Err(Error::BlobNotFound(_)) => {}
            other => panic!("expected BlobNotFound, got {other:?}"),
        }
        assert!(!exists(d.path(), &bogus).unwrap());
    }

    #[test]
    fn shard_layout_uses_first_four_hex_chars() {
        let d = root();
        let h = put(d.path(), b"shard test").unwrap();
        let hx = hex(&h);
        let p = d.path().join(&hx[0..2]).join(&hx[2..4]).join(&hx);
        assert!(p.exists(), "expected shard path {} to exist", p.display());
    }

    #[test]
    fn temp_dir_is_cleaned_after_put() {
        let d = root();
        let _ = put(d.path(), b"clean").unwrap();
        let leftover: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(leftover.is_empty(), "tmp leftovers: {:?}", leftover);
    }

    /// Deterministic pseudo-random bytes (xorshift64) so a multi-MiB
    /// input needs no RNG dependency and every run hashes the same.
    struct Rng(u64);

    impl Rng {
        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                chunk.copy_from_slice(&self.0.to_le_bytes()[..chunk.len()]);
            }
        }
    }

    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        rng.fill(&mut v);
        v
    }

    /// Count files under the CAS tree, excluding `.tmp` staging.
    fn cas_file_count(root: &Path) -> usize {
        fn walk(p: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(p).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), out);
                } else {
                    out.push(entry.path());
                }
            }
        }
        let mut files = Vec::new();
        walk(root, &mut files);
        files.retain(|p| !p.starts_with(root.join(".tmp")));
        files.len()
    }

    #[test]
    fn put_reader_hashes_while_streaming_multi_mib_input() {
        let d = root();
        let bytes = pseudo_random(3 * 1024 * 1024 + 17);
        let (h, size) = put_reader(d.path(), &bytes[..]).unwrap();
        assert_eq!(h, BlobHash(blake3::hash(&bytes).into()));
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(get(d.path(), &h).unwrap(), bytes);
        assert_eq!(cas_file_count(d.path()), 1);
    }

    #[test]
    fn put_reader_is_idempotent_one_cas_file() {
        let d = root();
        let bytes = pseudo_random(1024 * 1024 + 5);
        let (h1, s1) = put_reader(d.path(), &bytes[..]).unwrap();
        let (h2, s2) = put_reader(d.path(), &bytes[..]).unwrap();
        assert_eq!((h1, s1), (h2, s2));
        assert_eq!(
            cas_file_count(d.path()),
            1,
            "second put must not add a CAS file"
        );
    }

    /// Yields `left` bytes, then errors — the mid-stream failure shape.
    struct ExplodingReader {
        left: usize,
    }

    impl Read for ExplodingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                return Err(std::io::Error::other("mid-stream failure"));
            }
            let n = self.left.min(buf.len());
            buf[..n].fill(b'x');
            self.left -= n;
            Ok(n)
        }
    }

    #[test]
    fn put_reader_error_mid_stream_leaves_no_residue() {
        let d = root();
        let err = put_reader(d.path(), ExplodingReader { left: 4096 }).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
        let tmp: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "tmp leftovers: {:?}", tmp);
        assert_eq!(cas_file_count(d.path()), 0, "no CAS entry may exist");
    }

    #[test]
    fn put_reader_expect_roundtrips_matching_stream() {
        let d = root();
        let bytes = pseudo_random(1024 * 1024 + 99);
        let expected = hash_bytes(&bytes);
        let (h, size) = put_reader_expect(d.path(), &bytes[..], &expected).unwrap();
        assert_eq!(h, expected);
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(get(d.path(), &h).unwrap(), bytes);
        // Idempotent: a second identical stream is consumed and lands
        // on the same entry, still one CAS file.
        let (h2, _) = put_reader_expect(d.path(), &bytes[..], &expected).unwrap();
        assert_eq!(h2, expected);
        assert_eq!(cas_file_count(d.path()), 1);
    }

    #[test]
    fn put_reader_expect_mismatch_never_enters_the_cas() {
        let d = root();
        let bytes = pseudo_random(64 * 1024);
        let landed = hash_bytes(&bytes);
        let expected = hash_bytes(b"what the caller promised");
        let err = put_reader_expect(d.path(), &bytes[..], &expected).unwrap_err();
        assert!(matches!(err, Error::BlobCorrupt(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains(&hex(&landed)), "message names landed: {msg}");
        assert!(
            msg.contains(&hex(&expected)),
            "message names expected: {msg}"
        );

        // No CAS file under either hash, nothing staging.
        assert!(!blob_path(d.path(), &expected).exists());
        assert!(!blob_path(d.path(), &landed).exists());
        assert_eq!(cas_file_count(d.path()), 0);
        let tmp: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "tmp leftovers: {:?}", tmp);
    }

    #[test]
    fn put_reader_expect_mismatch_when_expected_already_exists() {
        // The CAS already holds verified bytes for the expected hash;
        // a wrong body must still be refused (it is wrong for the id),
        // must not disturb the existing entry, and must never land
        // under its own hash.
        let d = root();
        let good = b"the real bytes";
        let expected = hash_bytes(good);
        put(d.path(), good).unwrap();

        let wrong = pseudo_random(32 * 1024);
        let landed = hash_bytes(&wrong);
        let err = put_reader_expect(d.path(), &wrong[..], &expected).unwrap_err();
        assert!(matches!(err, Error::BlobCorrupt(_)), "got {err:?}");

        assert!(
            blob_path(d.path(), &expected).exists(),
            "the pre-existing expected entry is untouched"
        );
        assert_eq!(get(d.path(), &expected).unwrap(), good);
        assert!(!blob_path(d.path(), &landed).exists());
        let tmp: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "tmp leftovers: {:?}", tmp);
        assert_eq!(cas_file_count(d.path()), 1);
    }

    #[test]
    fn put_path_copy_mode_roundtrips() {
        let d = root();
        let bytes = pseudo_random(1024 * 1024 + 31);
        let src = d.path().join("src.bin");
        std::fs::write(&src, &bytes).unwrap();
        let (h, size) = put_path(d.path(), &src, PutMode::Copy).unwrap();
        assert_eq!(h, BlobHash(blake3::hash(&bytes).into()));
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(get(d.path(), &h).unwrap(), bytes);
    }

    #[test]
    fn put_path_reflink_falls_through_to_copy() {
        // On a filesystem without reflink (tmpfs, ext4, …) FICLONE and
        // copy_file_range fail and the put must still succeed via the
        // userspace copy; on one with reflink the kernel path runs
        // instead. The errno is deliberately not inspected: OpenZFS
        // with block cloning disabled answers EPERM (caught on the
        // build cluster, ZFS-in-LXC, 2026-09-26), which no allowlist
        // anticipated. Either way the landed bytes hash to the source
        // — that is the contract.
        let d = root();
        let bytes = pseudo_random(1024 * 1024 + 7);
        let src = d.path().join("src.bin");
        std::fs::write(&src, &bytes).unwrap();
        let (h, size) = put_path(d.path(), &src, PutMode::Reflink).unwrap();
        assert_eq!(h, BlobHash(blake3::hash(&bytes).into()));
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(get(d.path(), &h).unwrap(), bytes);
    }

    #[test]
    fn put_path_hardlink_aliases_the_source_inode() {
        let d = root();
        let src = d.path().join("src.bin");
        std::fs::write(&src, b"hard link me").unwrap();
        let (h, size) = put_path(d.path(), &src, PutMode::HardLink).unwrap();
        assert_eq!(size, 12);
        let cas = blob_path(d.path(), &h);
        assert_eq!(
            std::fs::metadata(&cas).unwrap().ino(),
            std::fs::metadata(&src).unwrap().ino(),
            "HardLink must alias the source inode, not copy it"
        );
        assert_eq!(get(d.path(), &h).unwrap(), b"hard link me");
    }

    #[test]
    fn put_path_hardlink_of_an_old_file_commits_young() {
        // F3: the staged link shares the source inode, so an aged
        // source would otherwise land in the CAS already "old" —
        // sweepable inside the commit→pin grace window. The staged
        // link is touched to now before commit, and (the documented
        // cost of HardLink) the source's mtime moves with the shared
        // inode.
        let d = root();
        let src = d.path().join("ancient.bin");
        std::fs::write(&src, b"ancient but immutable").unwrap();
        age_path(&src);
        assert!(mtime_age(&src) >= std::time::Duration::from_secs(60));

        let (h, _) = put_path(d.path(), &src, PutMode::HardLink).unwrap();
        let cas = blob_path(d.path(), &h);
        assert!(
            mtime_age(&cas) < std::time::Duration::from_secs(60),
            "a HardLink put of an old file must commit young (measured {:?})",
            mtime_age(&cas)
        );
        assert_eq!(
            std::fs::metadata(&cas).unwrap().ino(),
            std::fs::metadata(&src).unwrap().ino()
        );
    }

    /// Set a file's mtime two minutes into the past (beyond blobd's
    /// GC grace window) using only std.
    fn age_path(path: &Path) {
        let f = File::open(path).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(past)
                .set_modified(past),
        )
        .unwrap();
    }

    fn mtime_age(path: &Path) -> std::time::Duration {
        std::time::SystemTime::now()
            .duration_since(std::fs::metadata(path).unwrap().modified().unwrap())
            .unwrap()
    }

    #[test]
    fn idempotent_hits_refresh_the_cas_mtime() {
        // A GC grace window judged on mtime must cover a
        // re-acknowledged blob, not its first landing: an idempotent
        // hit on months-old bytes leaves them young (blobd M1c).
        let d = root();
        let cas = {
            let bytes = b"refresh my grace window";
            let h = put(d.path(), bytes).unwrap();
            let p = blob_path(d.path(), &h);
            age_path(&p);
            assert!(mtime_age(&p) >= std::time::Duration::from_secs(60));
            // put_reader's idempotent branch…
            let (h2, _) = put_reader(d.path(), &bytes[..]).unwrap();
            assert_eq!(h, h2);
            p
        };
        assert!(
            mtime_age(&cas) < std::time::Duration::from_secs(60),
            "put_reader idempotent hit must touch the CAS file"
        );

        // …put_reader_expect's…
        let bytes = b"expect a refresh";
        let h = put(d.path(), bytes).unwrap();
        let p = blob_path(d.path(), &h);
        age_path(&p);
        let _ = put_reader_expect(d.path(), &bytes[..], &h).unwrap();
        assert!(mtime_age(&p) < std::time::Duration::from_secs(60));

        // …and put_path's.
        let src = d.path().join("path-src.bin");
        std::fs::write(&src, b"put_path refresh").unwrap();
        let (h, _) = put_path(d.path(), &src, PutMode::Copy).unwrap();
        let p = blob_path(d.path(), &h);
        age_path(&p);
        let _ = put_path(d.path(), &src, PutMode::Copy).unwrap();
        assert!(mtime_age(&p) < std::time::Duration::from_secs(60));
    }

    #[test]
    fn put_path_error_paths_leave_no_tmp_residue() {
        // F4: a failure anywhere inside the staging window — the copy,
        // the staged re-hash, the commit — must remove the staged
        // file. A regular file squatting where the first shard
        // directory must be created makes commit_staged's
        // create_dir_all fail deterministically, after a fully
        // successful copy and re-hash.
        let d = root();
        let bytes = b"doomed staging";
        let h = hex(&hash_bytes(bytes));
        std::fs::write(d.path().join(&h[0..2]), b"not a directory").unwrap();
        let src = d.path().join("doomed.bin");
        std::fs::write(&src, bytes).unwrap();

        let err = put_path(d.path(), &src, PutMode::Copy).unwrap_err();
        assert!(
            matches!(err, Error::Io(_)),
            "the squatted shard dir must surface as io, got {err:?}"
        );
        let tmp: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "tmp leftovers: {tmp:?}");
        assert!(!blob_path(d.path(), &hash_bytes(bytes)).exists());
    }

    #[test]
    fn streaming_commit_failures_leave_no_tmp_residue() {
        // m1: put_reader and put_reader_expect hand the staged file to
        // commit_staged with no guard of their own; the same squatted
        // shard dir must not strand a .tmp entry on either path.
        let d = root();
        let bytes = b"doomed stream";
        let hb = hash_bytes(bytes);
        std::fs::write(d.path().join(&hex(&hb)[0..2]), b"not a directory").unwrap();

        assert!(matches!(
            put_reader(d.path(), &bytes[..]),
            Err(Error::Io(_))
        ));
        assert!(matches!(
            put_reader_expect(d.path(), &bytes[..], &hb),
            Err(Error::Io(_))
        ));
        let tmp: Vec<_> = std::fs::read_dir(d.path().join(".tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "tmp leftovers: {tmp:?}");
    }

    #[test]
    fn open_streams_back_identical_bytes() {
        let d = root();
        let bytes = pseudo_random(300_000);
        let (h, _) = put_reader(d.path(), &bytes[..]).unwrap();
        let mut f = open(d.path(), &h).unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes);
        match open(d.path(), &hash_bytes(b"never written")) {
            Err(Error::BlobNotFound(_)) => {}
            other => panic!("expected BlobNotFound, got {other:?}"),
        }
    }
}
