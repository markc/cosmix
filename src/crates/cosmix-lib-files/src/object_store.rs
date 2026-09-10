//! Private, immutable SHA-256 objects and snapshots. Callers must authorise the
//! collection before dispatch and serialise access to the single store instance.
//! The root must be daemon-owned and unavailable to untrusted local writers.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, String>;
pub const CHUNK_BYTES: usize = 256 * 1024;
const MANIFEST_BYTES: usize = 128 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_STAGING: usize = 64;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    // Declaration order is the canonical lexical JSON key order. Serialising
    // these structs directly does not depend on serde_json's map features.
    files: Vec<ManifestFile>,
    schema_version: u64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: String,
    size: u64,
}

/// Validate schema 1 and return compact JSON with fixed lexical field order.
/// File-array order is significant; object field insertion order is not.
pub fn canonical_manifest_bytes(value: &Value) -> Result<Vec<u8>> {
    let manifest: Manifest = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if manifest.schema_version != 1 || manifest.files.len() > 1000 {
        return Err("invalid manifest schema/file count".into());
    }
    let mut paths = HashSet::new();
    for entry in &manifest.files {
        let path = entry.path.as_str();
        if path.is_empty()
            || path.len() > 1024
            || path.contains(['\\', ':'])
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || !paths.insert(path)
            || !valid_hash(&entry.sha256)
        {
            return Err("unsafe, duplicate or invalid manifest entry".into());
        }
    }
    for path in &paths {
        for (i, _) in path.match_indices('/') {
            if paths.contains(&path[..i]) {
                return Err("snapshot path collision".into());
            }
        }
    }
    let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if bytes.len() > MANIFEST_BYTES {
        return Err("manifest too large".into());
    }
    Ok(bytes)
}

/// One daemon-owned store. Never share its directory with another writer.
pub struct ObjectStore {
    root: PathBuf,
    max_bytes: u64,
    max_staging_bytes: u64,
    _writer_lock: File,
}

fn io<T>(r: std::io::Result<T>) -> Result<T> {
    r.map_err(|e| e.to_string())
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string: {key}"))
}
fn number(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing unsigned integer: {key}"))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn checked_hash(args: &Value) -> Result<&str> {
    let h = text(args, "sha256")?;
    if !valid_hash(h) {
        return Err("invalid SHA-256".into());
    }
    Ok(h)
}
fn safe(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("parent path refused".into());
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => return Err("symlink refused".into()),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    io(io(File::open(path))?.sync_all())
}
fn mkdir(path: &Path) -> Result<()> {
    safe(path)?;
    if !path.exists() {
        io(fs::create_dir(path))?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
    }
    if !io(fs::metadata(path))?.is_dir() {
        return Err("expected directory".into());
    }
    Ok(())
}
fn regular(path: &Path) -> Result<Option<u64>> {
    safe(path)?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(m.len())),
        Ok(_) => Err("expected regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn bounded_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let len = regular(path)?.ok_or("file missing")?;
    if len > limit as u64 {
        return Err("file limit exceeded".into());
    }
    let mut data = Vec::new();
    io(io(File::open(path))?
        .take(limit as u64 + 1)
        .read_to_end(&mut data))?;
    if data.len() > limit {
        return Err("file limit exceeded".into());
    }
    Ok(data)
}
fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    safe(path)?;
    let mut out = Vec::new();
    for e in io(fs::read_dir(path))? {
        out.push(io(e)?.path());
        if out.len() > MAX_ENTRIES {
            return Err("store entry limit exceeded".into());
        }
    }
    out.sort();
    Ok(out)
}

// Count prospective names, including temporary hard-link publication entries,
// before creating anything. This preserves the bounded reader's openability.
fn entry_capacity(directory: &Path, proposed: &[&Path], limit: usize) -> Result<()> {
    let existing = entries(directory)?;
    let mut missing = HashSet::new();
    for path in proposed {
        safe(path)?;
        if !existing.iter().any(|p| p.as_path() == *path) {
            missing.insert(*path);
        }
    }
    if existing
        .len()
        .checked_add(missing.len())
        .is_none_or(|n| n > limit)
    {
        return Err("store entry capacity exceeded".into());
    }
    Ok(())
}

impl ObjectStore {
    pub fn open(root: PathBuf, max_bytes: u64, max_staging_bytes: u64) -> Result<Self> {
        if !root.is_absolute() {
            return Err("store root must be absolute".into());
        }
        safe(&root)?;
        // Installation creates parents; this avoids silently creating an unsafe tree.
        mkdir(&root)?;
        let lock_path = root.join(".writer-lock");
        entry_capacity(&root, &[&lock_path], MAX_ENTRIES)?;
        safe(&lock_path)?;
        let lock = io(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path))?;
        lock.try_lock()
            .map_err(|e| format!("store writer lock: {e}"))?;
        let store = Self {
            root,
            max_bytes,
            max_staging_bytes,
            _writer_lock: lock,
        };
        store.usage()?;
        Ok(store)
    }

    fn collection(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("invalid collection".into());
        }
        let root = self.root.join(name);
        entry_capacity(&self.root, &[&root], MAX_ENTRIES)?;
        mkdir(&root)?;
        for child in ["objects", "staging", "snapshots"] {
            entry_capacity(&root, &[&root.join(child)], MAX_ENTRIES)?;
            mkdir(&root.join(child))?;
        }
        Ok(root)
    }

    // Rebuild reservations from durable metadata, including after restart. Global
    // totals stop users bypassing quotas by opening another collection.
    fn usage(&self) -> Result<(u64, u64, usize)> {
        let mut committed = 0u64;
        let mut reserved = 0u64;
        let mut active = 0;
        for collection in entries(&self.root)? {
            if collection.file_name().is_some_and(|x| x == ".writer-lock") {
                regular(&collection)?;
                continue;
            }
            safe(&collection)?;
            if !io(fs::metadata(&collection))?.is_dir() {
                return Err("unexpected root entry".into());
            }
            for kind in ["objects", "snapshots", "staging"] {
                let dir = collection.join(kind);
                if !dir.exists() {
                    continue;
                }
                for path in entries(&dir)? {
                    let size = regular(&path)?.ok_or("entry disappeared")?;
                    if kind == "staging" {
                        if path.extension().is_some_and(|x| x == "json") {
                            let value: Value = serde_json::from_slice(&bounded_read(&path, 1024)?)
                                .map_err(|e| e.to_string())?;
                            reserved = reserved
                                .checked_add(number(&value, "size")?)
                                .ok_or("quota overflow")?;
                            active += 1;
                        }
                    } else {
                        committed = committed.checked_add(size).ok_or("quota overflow")?;
                    }
                }
            }
        }
        Ok((committed, reserved, active))
    }

    fn immutable(path: &Path, bytes: &[u8]) -> Result<()> {
        if regular(path)?.is_some() {
            if bounded_read(path, bytes.len())? == bytes {
                io(io(File::open(path))?.sync_all())?;
                sync_dir(path.parent().ok_or("no parent")?)?;
                let tmp = path.with_extension("pending");
                if regular(&tmp)?.is_some() {
                    io(fs::remove_file(&tmp))?;
                    sync_dir(path.parent().ok_or("no parent")?)?;
                }
                return Ok(());
            }
            return Err("immutable content conflict".into());
        }
        let tmp = path.with_extension("pending");
        entry_capacity(
            path.parent().ok_or("no parent")?,
            &[path, &tmp],
            MAX_ENTRIES,
        )?;
        safe(&tmp)?;
        let mut file = io(OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp))?;
        io(file.write_all(bytes))?;
        io(file.sync_all())?;
        // hard_link is atomic create-if-absent, unlike replacing rename.
        io(fs::hard_link(&tmp, path))?;
        sync_dir(path.parent().ok_or("no parent")?)?;
        io(fs::remove_file(&tmp))?;
        sync_dir(path.parent().ok_or("no parent")?)
    }

    pub fn execute(&mut self, collection: &str, operation: &str, args: &Value) -> Result<Value> {
        let root = self.collection(collection)?;
        if operation.starts_with("snapshot.") {
            return self.snapshot(&root, operation, args);
        }
        let hash = checked_hash(args)?;
        let object = root.join("objects").join(hash);
        let part = root.join("staging").join(format!("{hash}.part"));
        let meta = root.join("staging").join(format!("{hash}.json"));
        match operation {
            "object.abort" => {
                // This authority cannot remove committed objects or snapshots.
                // All three names are derived from the validated SHA-256.
                for path in [&part, &meta, &meta.with_extension("pending")] {
                    if regular(path)?.is_some() {
                        io(fs::remove_file(path))?;
                    }
                }
                sync_dir(meta.parent().ok_or("no parent")?)?;
                Ok(json!({"sha256":hash,"aborted":true}))
            }
            "object.begin" => {
                let size = number(args, "size")?;
                if let Some(existing) = regular(&object)? {
                    if existing != size {
                        return Err("object size mismatch".into());
                    }
                    sync_dir(object.parent().ok_or("no parent")?)?;
                    for path in [&part, &meta] {
                        if regular(path)?.is_some() {
                            io(fs::remove_file(path))?;
                        }
                    }
                    sync_dir(meta.parent().ok_or("no parent")?)?;
                    return Ok(json!({"offset":size,"complete":true}));
                }
                if regular(&meta)?.is_none() {
                    let (used, reserved, count) = self.usage()?;
                    if count >= MAX_STAGING
                        || reserved
                            .checked_add(size)
                            .is_none_or(|n| n > self.max_staging_bytes)
                        || used
                            .checked_add(reserved)
                            .and_then(|n| n.checked_add(size))
                            .is_none_or(|n| n > self.max_bytes)
                    {
                        return Err("storage quota exceeded".into());
                    }
                    Self::immutable(
                        &meta,
                        &serde_json::to_vec(&json!({"size":size})).map_err(|e| e.to_string())?,
                    )?;
                }
                let expected = self.expected(&meta)?;
                if expected != size {
                    return Err("reservation size mismatch".into());
                }
                safe(&part)?;
                entry_capacity(part.parent().ok_or("no parent")?, &[&part], MAX_ENTRIES)?;
                let file = io(OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&part))?;
                io(file.sync_all())?;
                sync_dir(part.parent().ok_or("no parent")?)?;
                let offset = io(file.metadata())?.len();
                if offset > size {
                    return Err("staging length exceeds reservation".into());
                }
                Ok(json!({"offset":offset,"complete":false}))
            }
            "object.chunk" => {
                let expected = self.expected(&meta)?;
                let encoded = text(args, "data_base64")?;
                if encoded.len() > CHUNK_BYTES.div_ceil(3) * 4 {
                    return Err("chunk too large".into());
                }
                let bytes = STANDARD.decode(encoded).map_err(|_| "invalid base64")?;
                if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
                    return Err("invalid chunk size".into());
                }
                let offset = number(args, "offset")?;
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or("offset overflow")?;
                let length = regular(&part)?.ok_or("upload not begun")?;
                if end > expected || offset > length {
                    return Err("invalid chunk offset".into());
                }
                let mut file = io(OpenOptions::new().read(true).write(true).open(&part))?;
                io(file.seek(SeekFrom::Start(offset)))?;
                if offset < length {
                    if end > length {
                        return Err("overlapping retry".into());
                    }
                    let mut previous = vec![0; bytes.len()];
                    io(file.read_exact(&mut previous))?;
                    if previous != bytes {
                        return Err("retry content mismatch".into());
                    }
                } else {
                    io(file.write_all(&bytes))?;
                }
                io(file.sync_all())?;
                Ok(json!({"offset":length.max(end)}))
            }
            "object.commit" => {
                if let Some(size) = regular(&object)? {
                    // A crash after publishing but before removing the reservation
                    // must not strand quota. Reconfirm durable directory state first.
                    sync_dir(object.parent().ok_or("no parent")?)?;
                    for path in [&part, &meta] {
                        if regular(path)?.is_some() {
                            io(fs::remove_file(path))?;
                        }
                    }
                    sync_dir(meta.parent().ok_or("no parent")?)?;
                    return Ok(json!({"sha256":hash,"size":size,"complete":true}));
                }
                let expected = self.expected(&meta)?;
                if regular(&part)? != Some(expected) {
                    return Err("upload incomplete".into());
                }
                let mut file = io(File::open(&part))?;
                let mut hasher = Sha256::new();
                let mut buf = vec![0; CHUNK_BYTES];
                loop {
                    let n = io(file.read(&mut buf))?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                if format!("{:x}", hasher.finalize()) != hash {
                    return Err("SHA-256 mismatch".into());
                }
                io(file.sync_all())?;
                entry_capacity(object.parent().ok_or("no parent")?, &[&object], MAX_ENTRIES)?;
                io(fs::hard_link(&part, &object))?;
                sync_dir(object.parent().ok_or("no parent")?)?;
                io(fs::remove_file(&part))?;
                io(fs::remove_file(&meta))?;
                sync_dir(meta.parent().ok_or("no parent")?)?;
                Ok(json!({"sha256":hash,"size":expected,"complete":true}))
            }
            "object.read" => {
                let size = regular(&object)?.ok_or("object missing")?;
                let offset = number(args, "offset")?;
                let max = number(args, "max")?;
                if offset > size || max == 0 || max > CHUNK_BYTES as u64 {
                    return Err("invalid read range".into());
                }
                let mut file = io(File::open(&object))?;
                io(file.seek(SeekFrom::Start(offset)))?;
                let mut bytes = vec![0; (size - offset).min(max) as usize];
                io(file.read_exact(&mut bytes))?;
                Ok(
                    json!({"sha256":hash,"size":size,"offset":offset,"data_base64":STANDARD.encode(bytes)}),
                )
            }
            _ => Err("unknown storage operation".into()),
        }
    }

    fn expected(&self, path: &Path) -> Result<u64> {
        let value =
            serde_json::from_slice(&bounded_read(path, 1024)?).map_err(|e| e.to_string())?;
        number(&value, "size")
    }

    fn snapshot(&self, root: &Path, operation: &str, args: &Value) -> Result<Value> {
        let directory = root.join("snapshots");
        match operation {
            "snapshot.commit" => {
                let manifest = args.get("manifest").ok_or("missing manifest")?;
                let bytes = canonical_manifest_bytes(manifest)?;
                let files = manifest
                    .get("files")
                    .and_then(Value::as_array)
                    .ok_or("missing files")?;
                for entry in files {
                    let hash = checked_hash(entry)?;
                    if regular(&root.join("objects").join(hash))? != Some(number(entry, "size")?) {
                        return Err("snapshot object missing or wrong size".into());
                    }
                }
                let hash = digest(&bytes);
                let target = directory.join(&hash);
                if regular(&target)?.is_none() {
                    let (used, reserved, _) = self.usage()?;
                    if used
                        .checked_add(reserved)
                        .and_then(|n| n.checked_add(bytes.len() as u64))
                        .is_none_or(|n| n > self.max_bytes)
                    {
                        return Err("storage quota exceeded".into());
                    }
                }
                Self::immutable(&target, &bytes)?;
                Ok(json!({"sha256":hash,"files":files.len()}))
            }
            "snapshot.get" => {
                let hash = checked_hash(args)?;
                let bytes = bounded_read(&directory.join(hash), MANIFEST_BYTES)?;
                if digest(&bytes) != hash {
                    return Err("snapshot corruption".into());
                }
                let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if canonical_manifest_bytes(&manifest)? != bytes {
                    return Err("noncanonical stored manifest".into());
                }
                Ok(json!({"sha256":hash,"manifest":manifest}))
            }
            "snapshot.list" => {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100);
                if limit == 0 || limit > 100 {
                    return Err("invalid list limit".into());
                }
                let after = args.get("after").and_then(Value::as_str).unwrap_or("");
                if !after.is_empty() && !valid_hash(after) {
                    return Err("invalid list cursor".into());
                }
                let mut hashes = Vec::new();
                for path in entries(&directory)? {
                    let name = path
                        .file_name()
                        .and_then(|x| x.to_str())
                        .ok_or("invalid filename")?;
                    if valid_hash(name) && name > after {
                        regular(&path)?;
                        hashes.push(name.to_string());
                    }
                    if hashes.len() == limit as usize {
                        break;
                    }
                }
                Ok(json!({"snapshots":hashes,"next":hashes.last()}))
            }
            _ => Err("unknown storage operation".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                path: std::env::temp_dir().join(format!("cosmix-objects-{}", uuid::Uuid::new_v4())),
            }
        }
        fn open(&self, quota: u64) -> ObjectStore {
            ObjectStore::open(self.path.clone(), quota, quota).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn begin(store: &mut ObjectStore, bytes: &[u8]) -> String {
        let hash = digest(bytes);
        store
            .execute(
                "coast",
                "object.begin",
                &json!({"sha256":hash,"size":bytes.len()}),
            )
            .unwrap();
        hash
    }
    fn put(store: &mut ObjectStore, bytes: &[u8]) -> String {
        let hash = begin(store, bytes);
        if !bytes.is_empty() {
            store
                .execute(
                    "coast",
                    "object.chunk",
                    &json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode(bytes)}),
                )
                .unwrap();
        }
        store
            .execute("coast", "object.commit", &json!({"sha256":hash}))
            .unwrap();
        hash
    }

    #[test]
    fn resumes_binary_after_restart_and_retries_are_exact() {
        let f = Fixture::new();
        let mut s = f.open(1_000_000);
        let bytes = [0, 255, 1, 128, 42, 0];
        let hash = begin(&mut s, &bytes);
        let first = json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode(&bytes[..3])});
        assert_eq!(
            s.execute("coast", "object.chunk", &first).unwrap()["offset"],
            3
        );
        drop(s);
        let mut s = f.open(1_000_000);
        assert_eq!(
            s.execute("coast", "object.begin", &json!({"sha256":hash,"size":6}))
                .unwrap()["offset"],
            3
        );
        assert_eq!(
            s.execute("coast", "object.chunk", &first).unwrap()["offset"],
            3
        );
        assert!(
            s.execute(
                "coast",
                "object.chunk",
                &json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode([7,7,7])})
            )
            .is_err()
        );
        assert!(
            s.execute("coast", "object.commit", &json!({"sha256":hash}))
                .is_err()
        );
        s.execute(
            "coast",
            "object.chunk",
            &json!({"sha256":hash,"offset":3,"data_base64":STANDARD.encode(&bytes[3..])}),
        )
        .unwrap();
        s.execute("coast", "object.commit", &json!({"sha256":hash}))
            .unwrap();
        s.execute("coast", "object.commit", &json!({"sha256":hash}))
            .unwrap();
        let read = s
            .execute(
                "coast",
                "object.read",
                &json!({"sha256":hash,"offset":0,"max":256}),
            )
            .unwrap();
        assert_eq!(
            STANDARD
                .decode(read["data_base64"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert!(
            s.execute(
                "other",
                "object.read",
                &json!({"sha256":hash,"offset":0,"max":256})
            )
            .is_err()
        );
    }

    #[test]
    fn reservations_survive_restart_and_are_global() {
        let f = Fixture::new();
        let mut s = f.open(10);
        begin(&mut s, &[1; 8]);
        drop(s);
        let mut s = f.open(10);
        assert!(
            s.execute(
                "other",
                "object.begin",
                &json!({"sha256":digest(&[2;3]),"size":3})
            )
            .is_err()
        );
        assert!(
            s.execute(
                "coast",
                "object.begin",
                &json!({"sha256":digest(&[1;8]),"size":9})
            )
            .is_err()
        );
    }

    #[test]
    fn second_writer_is_rejected_until_first_drops() {
        let f = Fixture::new();
        let s = f.open(10000);
        assert!(
            ObjectStore::open(f.path.clone(), 10000, 10000)
                .err()
                .unwrap()
                .contains("writer lock")
        );
        drop(s);
        assert!(ObjectStore::open(f.path.clone(), 10000, 10000).is_ok());
    }

    #[test]
    fn corrupt_upload_cannot_be_committed() {
        let f = Fixture::new();
        let mut s = f.open(1000);
        let hash = begin(&mut s, b"good");
        s.execute(
            "coast",
            "object.chunk",
            &json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode(b"evil")}),
        )
        .unwrap();
        assert!(
            s.execute("coast", "object.commit", &json!({"sha256":hash}))
                .unwrap_err()
                .contains("SHA-256")
        );
        assert!(!f.path.join("coast/objects").join(hash).exists());
    }

    #[test]
    fn abort_poisoned_stage_then_retry_survives_restart() {
        let f = Fixture::new();
        let mut s = f.open(1000);
        let hash = begin(&mut s, b"good");
        s.execute(
            "coast",
            "object.chunk",
            &json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode(b"evil")}),
        )
        .unwrap();
        assert!(
            s.execute("coast", "object.commit", &json!({"sha256":hash}))
                .is_err()
        );
        drop(s);
        let mut s = f.open(1000);
        // A failed metadata publication can leave this bounded temporary file.
        fs::write(
            f.path.join("coast/staging").join(format!("{hash}.pending")),
            b"{}",
        )
        .unwrap();
        s.execute("coast", "object.abort", &json!({"sha256":hash}))
            .unwrap();
        s.execute("coast", "object.abort", &json!({"sha256":hash}))
            .unwrap();
        assert_eq!(s.usage().unwrap(), (0, 0, 0));
        drop(s);
        let mut s = f.open(1000);
        assert_eq!(put(&mut s, b"good"), hash);
        s.execute("coast", "object.abort", &json!({"sha256":hash}))
            .unwrap();
        assert_eq!(
            s.execute(
                "coast",
                "object.read",
                &json!({"sha256":hash,"offset":0,"max":4})
            )
            .unwrap()["data_base64"],
            STANDARD.encode(b"good")
        );
    }

    #[test]
    fn entry_capacity_reserves_temporary_names_before_creation() {
        let f = Fixture::new();
        let _s = f.open(1000);
        let final_path = f.path.join("new");
        let temporary = f.path.join("new.pending");
        // One lock file plus two proposed publication entries cannot fit two.
        assert!(entry_capacity(&f.path, &[&final_path, &temporary], 2).is_err());
        assert!(!final_path.exists() && !temporary.exists());
        assert_eq!(entries(&f.path).unwrap().len(), 1);
        entry_capacity(&f.path, &[&final_path, &temporary], 3).unwrap();
        fs::write(&temporary, b"x").unwrap();
        entry_capacity(&f.path, &[&final_path, &temporary], 3).unwrap();
        // Existing temporary entries are counted once, retries can finish.
        fs::hard_link(&temporary, &final_path).unwrap();
        assert_eq!(entries(&f.path).unwrap().len(), 3);
        assert!(entry_capacity(&f.path, &[&f.path.join("overflow")], 3).is_err());
        assert_eq!(entries(&f.path).unwrap().len(), 3);
    }

    #[test]
    fn canonical_manifest_has_explicit_field_order_and_strict_schema() {
        let hash = "a".repeat(64);
        let first: Value = serde_json::from_str(&format!(
            r#"{{"schema_version":1,"files":[{{"size":0,"sha256":"{hash}","path":"empty"}}]}}"#
        ))
        .unwrap();
        let second = json!({"files":[{"path":"empty","sha256":hash,"size":0}],"schema_version":1});
        let bytes = canonical_manifest_bytes(&first).unwrap();
        assert_eq!(bytes, canonical_manifest_bytes(&second).unwrap());
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!(
                r#"{{"files":[{{"path":"empty","sha256":"{hash}","size":0}}],"schema_version":1}}"#
            )
        );
        let mut invalid = first.clone();
        invalid["extra"] = json!(true);
        assert!(canonical_manifest_bytes(&invalid).is_err());
        let mut invalid = first.clone();
        invalid["files"][0]["mode"] = json!(420);
        assert!(canonical_manifest_bytes(&invalid).is_err());
        for path in ["C:file", "line\nbreak", "../bad", "/root"] {
            let mut invalid = first.clone();
            invalid["files"][0]["path"] = json!(path);
            assert!(canonical_manifest_bytes(&invalid).is_err());
        }
        let many = json!({"schema_version":1,"files":vec![first["files"][0].clone();1001]});
        assert!(canonical_manifest_bytes(&many).is_err());
    }

    #[test]
    fn retry_after_publish_crash_reclaims_reservation_and_empty_files_work() {
        let f = Fixture::new();
        let mut s = f.open(10000);
        let empty = put(&mut s, b"");
        assert_eq!(
            s.execute(
                "coast",
                "object.read",
                &json!({"sha256":empty,"offset":0,"max":1})
            )
            .unwrap()["data_base64"],
            ""
        );
        let hash = begin(&mut s, b"data");
        s.execute(
            "coast",
            "object.chunk",
            &json!({"sha256":hash,"offset":0,"data_base64":STANDARD.encode(b"data")}),
        )
        .unwrap();
        let root = f.path.join("coast");
        fs::hard_link(
            root.join("staging").join(format!("{hash}.part")),
            root.join("objects").join(&hash),
        )
        .unwrap();
        drop(s);
        let mut s = f.open(10000);
        s.execute("coast", "object.commit", &json!({"sha256":hash}))
            .unwrap();
        assert_eq!(s.usage().unwrap(), (4, 0, 0));
    }

    #[test]
    fn snapshots_are_immutable_and_reject_paths_or_missing_objects() {
        let f = Fixture::new();
        let mut s = f.open(10000);
        let hash = put(&mut s, b"waves");
        let manifest =
            json!({"schema_version":1,"files":[{"path":"coast/waves.bin","sha256":hash,"size":5}]});
        let commit = s
            .execute("coast", "snapshot.commit", &json!({"manifest":manifest}))
            .unwrap();
        assert_eq!(
            s.execute("coast", "snapshot.commit", &json!({"manifest":manifest}))
                .unwrap(),
            commit
        );
        let get = s
            .execute("coast", "snapshot.get", &json!({"sha256":commit["sha256"]}))
            .unwrap();
        assert_eq!(get["manifest"], manifest);
        for path in ["../escape", "/etc/passwd", "a//b", "a/./b", "a\\b"] {
            assert!(s.execute("coast","snapshot.commit",&json!({"manifest":{"schema_version":1,"files":[{"path":path,"sha256":hash,"size":5}]}})).is_err());
        }
        assert!(s.execute("coast","snapshot.commit",&json!({"manifest":{"schema_version":1,"files":[{"path":"a","sha256":digest(b"absent"),"size":6}]}})).is_err());
        assert!(s.execute("coast","snapshot.commit",&json!({"manifest":{"schema_version":1,"files":[{"path":"a","sha256":hash,"size":5},{"path":"a/b","sha256":hash,"size":5}]}})).is_err());
        fs::write(
            f.path
                .join("coast/snapshots")
                .join(commit["sha256"].as_str().unwrap()),
            b"{}",
        )
        .unwrap();
        assert!(
            s.execute("coast", "snapshot.get", &json!({"sha256":commit["sha256"]}))
                .unwrap_err()
                .contains("corruption")
        );
    }

    #[test]
    fn invalid_ranges_and_oversized_chunks_are_refused() {
        let f = Fixture::new();
        let mut s = f.open(10000);
        let hash = begin(&mut s, b"abc");
        for offset in [1, u64::MAX] {
            assert!(
                s.execute(
                    "coast",
                    "object.chunk",
                    &json!({"sha256":hash,"offset":offset,"data_base64":"YQ=="})
                )
                .is_err()
            );
        }
        assert!(
            s.execute(
                "coast",
                "object.chunk",
                &json!({"sha256":hash,"offset":0,"data_base64":"!"})
            )
            .is_err()
        );
        assert!(
            s.execute(
                "coast",
                "object.chunk",
                &json!({"sha256":hash,"offset":0,"data_base64":"A".repeat(CHUNK_BYTES*2)})
            )
            .is_err()
        );
        assert!(
            s.execute(
                "../escape",
                "object.begin",
                &json!({"sha256":hash,"size":3})
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_and_objects_are_refused() {
        let f = Fixture::new();
        let mut s = f.open(10000);
        let hash = put(&mut s, b"ok");
        let path = f.path.join("coast/objects").join(&hash);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", path).unwrap();
        assert!(
            s.execute(
                "coast",
                "object.read",
                &json!({"sha256":hash,"offset":0,"max":100})
            )
            .is_err()
        );
        assert!(f.open_result().is_err());
    }

    #[cfg(unix)]
    impl Fixture {
        fn open_result(&self) -> Result<ObjectStore> {
            ObjectStore::open(self.path.clone(), 10000, 10000)
        }
    }
}
