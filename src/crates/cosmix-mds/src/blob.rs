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
    /// because the filesystem lacks reflink support — `EOPNOTSUPP`,
    /// `EXDEV`, `EINVAL` and `ENOSYS` all fall through to the next
    /// option.
    Reflink,
    /// Hard-link the source inode into the CAS — no bytes copied.
    /// Only for publishers that promise the source path immutable
    /// from the call onward (the blob store's own staging); never for
    /// a mutable path such as a filesd place, whose contract is
    /// "someone else may rewrite this path".
    HardLink,
}

/// Stream `r` into the CAS, hashing with BLAKE3 while the bytes are
/// staged — the hash is only known at stream end, and the write
/// protocol already tolerates that (the idempotence check happens
/// after staging instead of before). Returns the hash and the byte
/// count. A reader error mid-stream removes the staged file and
/// leaves no CAS entry.
pub fn put_reader(blobs_root: &Path, r: impl Read) -> Result<(BlobHash, u64)> {
    // 1. Stage in .tmp/<uuid>. UUIDv4 is fine here — uniqueness over
    // a tiny temp window, no ordering needed.
    let tmp_dir = blobs_root.join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(uuid::Uuid::new_v4().to_string());
    let mut hasher = blake3::Hasher::new();
    let mut size: u64 = 0;
    if let Err(e) = stream_to_tmp(r, &tmp_path, &mut hasher, &mut size) {
        let _ = fs::remove_file(&tmp_path);
        return Err(Error::Io(e));
    }

    let hash = BlobHash(hasher.finalize().into());
    if blob_path(blobs_root, &hash).exists() {
        // Idempotent: identical bytes are already committed. Drop the
        // staged copy rather than rewriting the entry.
        let _ = fs::remove_file(&tmp_path);
        return Ok((hash, size));
    }

    commit_staged(blobs_root, &tmp_path, &hash)?;
    Ok((hash, size))
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
fn commit_staged(blobs_root: &Path, tmp_path: &Path, hash: &BlobHash) -> Result<()> {
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
        Err(e) => {
            let _ = fs::remove_file(tmp_path);
            return Err(Error::Io(e));
        }
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
/// (the source is still read once — the hash must be computed).
pub fn put_path(blobs_root: &Path, src: &Path, mode: PutMode) -> Result<(BlobHash, u64)> {
    let (hash, size) = hash_file(src)?;
    if blob_path(blobs_root, &hash).exists() {
        return Ok((hash, size));
    }

    let tmp_dir = blobs_root.join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(uuid::Uuid::new_v4().to_string());

    match mode {
        PutMode::Copy => {
            let mut src_f = File::open(src)?;
            let mut tmp_f = File::create(&tmp_path)?;
            std::io::copy(&mut src_f, &mut tmp_f)?;
        }
        PutMode::Reflink => {
            let mut src_f = File::open(src)?;
            let mut tmp_f = File::create(&tmp_path)?;
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
            fs::hard_link(src, &tmp_path)?;
        }
    }

    let (landed_hash, _) = hash_file(&tmp_path)?;
    if landed_hash != hash {
        let _ = fs::remove_file(&tmp_path);
        return Err(Error::BlobCorrupt(format!(
            "put_path: staged bytes hash to {} but source hashed to {}",
            hex(&landed_hash),
            hex(&hash)
        )));
    }

    commit_staged(blobs_root, &tmp_path, &hash)?;
    Ok((hash, size))
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

/// Errnos on which `Reflink` falls through to the next copy method
/// instead of failing: the filesystem or kernel merely lacks
/// server-side copy. Anything else is a real error.
fn is_reflink_fallthrough(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) | Some(libc::EINVAL) | Some(libc::ENOSYS)
    )
}

/// Kernel-side copy for [`PutMode::Reflink`]: `FICLONE` reflink
/// first, `copy_file_range` second. Returns `Ok(false)` when the
/// filesystem offers neither and the caller should finish with a
/// userspace copy; `Err` only for real failures.
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
    let e = std::io::Error::last_os_error();
    if !is_reflink_fallthrough(&e) {
        return Err(e);
    }
    match copy_file_range_all(src, dst) {
        Ok(()) => Ok(true),
        Err(e) if is_reflink_fallthrough(&e) => {
            // Restart the userspace fallback from a clean slate: a
            // fall-through mid-copy leaves both fds advanced and the
            // staging file partially written.
            src.rewind()?;
            dst.set_len(0)?;
            dst.rewind()?;
            Ok(false)
        }
        Err(e) => Err(e),
    }
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
        // copy_file_range report a fall-through errno and the put must
        // still succeed via the userspace copy; on one with reflink
        // the kernel path runs instead. Either way the landed bytes
        // hash to the source — that is the contract.
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
