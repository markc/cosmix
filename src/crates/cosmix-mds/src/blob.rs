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
use std::io::Write;
use std::os::unix::fs::MetadataExt;
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

pub fn put(blobs_root: &Path, bytes: &[u8]) -> Result<BlobHash> {
    let hash = hash_bytes(bytes);
    let final_path = blob_path(blobs_root, &hash);
    if final_path.exists() {
        return Ok(hash);
    }

    // 1. Stage in .tmp/<uuid>. UUIDv4 is fine here — uniqueness over
    // a tiny temp window, no ordering needed.
    let tmp_dir = blobs_root.join(".tmp");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(uuid::Uuid::new_v4().to_string());
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?; // 2. fsync temp
    }

    // 3. Ensure parent shard exists, then hard-link.
    let parent = final_path.parent().expect("blob path has parent");
    fs::create_dir_all(parent)?;
    match fs::hard_link(&tmp_path, &final_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Race: another writer placed the same hash. Both inputs
            // are identical (CAS), so dropping ours is safe.
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(e));
        }
    }

    // 4. fsync the parent directory so the link survives a crash.
    fsync_dir(parent)?;

    // 5. Best-effort temp cleanup.
    let _ = fs::remove_file(&tmp_path);

    Ok(hash)
}

pub fn get(blobs_root: &Path, hash: &BlobHash) -> Result<Vec<u8>> {
    let p = blob_path(blobs_root, hash);
    match fs::read(&p) {
        Ok(bytes) => Ok(bytes),
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
}
