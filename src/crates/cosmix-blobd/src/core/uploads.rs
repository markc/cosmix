//! Durable lane sessions. The database offset is authoritative; file length
//! may run ahead after a crash, but must never be used to advance the offset.
use super::*;
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

pub(super) const SCHEMA: &str = "
CREATE TABLE upload_sessions (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    upload_key TEXT,
    size INTEGER NOT NULL CHECK(size >= 0 AND size <= 9007199254740991),
    offset INTEGER NOT NULL DEFAULT 0 CHECK(offset >= 0 AND offset <= size),
    expected_hash TEXT,
    mime TEXT NOT NULL,
    name TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('active','committing','complete','failed','aborted')),
    actual_hash TEXT,
    result TEXT,
    error TEXT,
    UNIQUE(owner, upload_key)
);
CREATE INDEX upload_expiry ON upload_sessions(expires_at);
CREATE INDEX upload_owner ON upload_sessions(owner, state);
";

const SELECT: &str = "SELECT id,owner,upload_key,size,offset,expected_hash,mime,name,
created_at,expires_at,state,actual_hash,result,error FROM upload_sessions";
pub(super) const RECEIPT_TTL_MS: i64 = 86_400_000;

/// Active sessions and terminal receipts have separate finite bounds. Receipts
/// are never evicted early to admit a new upload (commit replay lasts 24 h).
#[derive(Debug, Clone)]
pub struct UploadLimits {
    pub ttl_ms: i64,
    pub per_owner: usize,
    pub total: usize,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            ttl_ms: RECEIPT_TTL_MS,
            per_owner: 16,
            total: 64,
        }
    }
}

impl UploadLimits {
    pub(super) fn validate(&self) -> Result<()> {
        if self.ttl_ms <= 0
            || self.ttl_ms > 365 * RECEIPT_TTL_MS
            || self.per_owner == 0
            || self.total == 0
            || self.total > 4096
            || self.per_owner > self.total
        {
            return Err(StoreError::BadRequest("invalid upload limits".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct UploadCreate {
    pub owner: String,
    pub size: u64,
    pub expected_hash: Option<BlobHash>,
    pub mime: String,
    pub name: Option<String>,
    pub key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadSession {
    pub id: String,
    pub owner: String,
    pub key: Option<String>,
    pub size: u64,
    pub offset: u64,
    pub expected_hash: Option<String>,
    pub mime: String,
    pub name: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: String,
    pub actual_hash: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl UploadSession {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"upload": self.id, "owner": self.owner, "key": self.key,
            "size": self.size, "offset": self.offset, "expect": self.expected_hash.as_ref().map(|h| format!("b3:{h}")),
            "mime": self.mime, "name": self.name, "created_at": self.created_at,
            "expires_at": self.expires_at, "state": self.state, "result": self.result, "error": self.error})
    }

    fn conflict(&self, reason: &str) -> StoreError {
        StoreError::UploadConflict {
            offset: self.offset,
            reason: reason.into(),
        }
    }
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadSession> {
    let result: Option<String> = r.get(12)?;
    let result = result
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(UploadSession {
        id: r.get(0)?,
        owner: r.get(1)?,
        key: r.get(2)?,
        size: r.get(3)?,
        offset: r.get(4)?,
        expected_hash: r.get(5)?,
        mime: r.get(6)?,
        name: r.get(7)?,
        created_at: r.get(8)?,
        expires_at: r.get(9)?,
        state: r.get(10)?,
        actual_hash: r.get(11)?,
        result,
        error: r.get(13)?,
    })
}

pub(super) struct UploadGuard {
    id: String,
    writers: Arc<Mutex<BTreeSet<String>>>,
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        self.writers.lock().unwrap().remove(&self.id);
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

impl Store {
    pub(super) fn uploads_root(&self) -> PathBuf {
        self.blobs_root().join(".uploads")
    }

    fn upload_path(&self, id: &str) -> Result<PathBuf> {
        if uuid::Uuid::parse_str(id)
            .ok()
            .is_none_or(|u| u.to_string() != id)
        {
            return Err(StoreError::UploadMissing);
        }
        Ok(self.uploads_root().join(id))
    }

    fn upload_row(&self, id: &str) -> Result<UploadSession> {
        self.upload_path(id)?;
        self.db
            .lock()
            .unwrap()
            .query_row(&format!("{SELECT} WHERE id=?1"), [id], row)
            .optional()
            .map_err(db_err)?
            .ok_or(StoreError::UploadMissing)
    }

    pub(super) fn upload_guard(&self, id: &str) -> Result<UploadGuard> {
        let session = self.upload_row(id)?;
        if !self.upload_writers.lock().unwrap().insert(id.into()) {
            return Err(session.conflict("another mutation is running"));
        }
        Ok(UploadGuard {
            id: id.into(),
            writers: Arc::clone(&self.upload_writers),
        })
    }

    pub(super) fn reservation_totals(
        &self,
        db: &Connection,
        holds: &Reservations,
        ephemeral: Option<u64>,
        session: Option<&str>,
    ) -> Result<BTreeMap<String, u64>> {
        let mut totals = holds.totals(ephemeral);
        let mut stmt = db
            .prepare(
                "SELECT owner,size FROM upload_sessions
            WHERE state IN ('active','committing') AND (?1 IS NULL OR id != ?1)",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([session], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
            })
            .map_err(db_err)?;
        for row in rows {
            let (owner, size) = row.map_err(db_err)?;
            let n = totals.entry(owner).or_default();
            *n = n.saturating_add(size);
        }
        Ok(totals)
    }

    /// Create or retrieve an owner+key session. No body bytes are read here.
    /// A key replay must agree on the entire immutable creation identity.
    pub fn upload_create(&self, opts: &UploadCreate) -> Result<(UploadSession, bool)> {
        if opts.owner.is_empty()
            || opts.owner.len() > 128
            || opts.owner.chars().any(char::is_control)
            || opts.size > 9_007_199_254_740_991
            || opts
                .key
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
            || opts.mime.is_empty()
            || opts.mime.len() > 256
            || opts.mime.chars().any(char::is_control)
            || opts
                .name
                .as_ref()
                .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
        {
            return Err(StoreError::BadRequest(
                "invalid upload owner, size, key, mime or name".into(),
            ));
        }
        self.sweep_uploads()?;
        let holds = self.reserved.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        if let Some(key) = &opts.key {
            let existing = tx
                .query_row(
                    &format!("{SELECT} WHERE owner=?1 AND upload_key=?2"),
                    params![opts.owner, key],
                    row,
                )
                .optional()
                .map_err(db_err)?;
            if let Some(s) = existing {
                if s.expires_at <= now_ms() && s.state != "committing" {
                    return Err(StoreError::UploadMissing);
                }
                if s.size != opts.size
                    || s.expected_hash != opts.expected_hash.as_ref().map(blob::hex)
                    || s.mime != opts.mime
                    || s.name != opts.name
                {
                    return Err(s.conflict("idempotency key identity mismatch"));
                }
                return Ok((s, false));
            }
        }
        let (active, owner_active, count, owner_count): (u64, u64, u64, u64) = tx
            .query_row(
                "SELECT COALESCE(SUM(state IN ('active','committing')),0),
            COALESCE(SUM(owner=?1 AND state IN ('active','committing')),0), COUNT(*),
            COALESCE(SUM(owner=?1),0) FROM upload_sessions",
                [&opts.owner],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(db_err)?;
        // A full receipt budget refuses creation instead of dropping a promise.
        if active >= self.upload_limits.total as u64
            || owner_active >= self.upload_limits.per_owner as u64
            || count >= self.upload_limits.total as u64 + 1024
            || owner_count >= self.upload_limits.per_owner as u64 + 256
        {
            return Err(StoreError::UploadLimit);
        }
        let reserved = self.reservation_totals(&tx, &holds, None, None)?;
        let used: u64 = tx
            .query_row(
                "SELECT COALESCE((SELECT used_bytes FROM quota WHERE owner=?1),0)",
                [&opts.owner],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        let total: u64 = tx
            .query_row("SELECT COALESCE(SUM(used_bytes),0) FROM quota", [], |r| {
                r.get(0)
            })
            .map_err(db_err)?;
        let would_use = used
            .saturating_add(reserved.get(&opts.owner).copied().unwrap_or(0))
            .saturating_add(opts.size);
        let limit = self.options.owner_limit(&opts.owner);
        if would_use > limit {
            return Err(StoreError::QuotaOwner {
                owner: opts.owner.clone(),
                would_use,
                limit,
            });
        }
        let would_use = total
            .saturating_add(sum_reserved(&reserved))
            .saturating_add(opts.size);
        if would_use > self.options.quota_total_bytes {
            return Err(StoreError::QuotaTotal {
                would_use,
                limit: self.options.quota_total_bytes,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.upload_path(&id)?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o640)
            .open(&path)?;
        file.sync_all()?;
        sync_dir(&self.uploads_root())?;
        let created = now_ms();
        tx.execute(
            "INSERT INTO upload_sessions
            (id,owner,upload_key,size,expected_hash,mime,name,created_at,expires_at,state)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'active')",
            params![
                id,
                opts.owner,
                opts.key,
                opts.size,
                opts.expected_hash.as_ref().map(blob::hex),
                opts.mime,
                opts.name,
                created,
                created + self.upload_limits.ttl_ms
            ],
        )
        .map_err(db_err)?;
        // On an ambiguous COMMIT error leave staging intact. Restore removes
        // it only if the authoritative database has no row.
        tx.commit().map_err(db_err)?;
        drop(db);
        drop(holds);
        self.bump_generation();
        Ok((self.upload_row(&id)?, true))
    }

    pub fn upload_status(&self, id: &str) -> Result<UploadSession> {
        let s = self.upload_row(id)?;
        if s.expires_at <= now_ms() && s.state != "committing" {
            // Readers never wait behind a body writer. The worker will reclaim
            // its reservation; an expired resource is already unavailable.
            if let Ok(_guard) = self.upload_guard(id) {
                let current = self.upload_row(id)?;
                if current.expires_at > now_ms() || current.state == "committing" {
                    return Ok(current);
                }
                self.expire_upload(&current)?;
            }
            return Err(StoreError::UploadMissing);
        }
        Ok(s)
    }

    /// Append exactly one chunk under nonblocking per-session admission.
    /// The reader must signal a clean EOF; excess and short bodies roll back.
    pub fn upload_append(
        &self,
        id: &str,
        start: u64,
        end: u64,
        total: u64,
        reader: impl Read,
    ) -> Result<UploadSession> {
        let guard = self.upload_guard(id)?;
        self.upload_append_guarded(&guard, start, end, total, reader)
    }

    pub(super) fn upload_append_guarded(
        &self,
        guard: &UploadGuard,
        start: u64,
        end: u64,
        total: u64,
        mut reader: impl Read,
    ) -> Result<UploadSession> {
        let s = self.upload_row(&guard.id)?;
        if s.expires_at <= now_ms() {
            self.expire_upload(&s)?;
            return Err(StoreError::UploadMissing);
        }
        if s.state != "active" {
            return Err(s.conflict("session is not active"));
        }
        if start != s.offset {
            return Err(s.conflict("offset mismatch"));
        }
        if total != s.size || end < start || end >= total {
            return Err(StoreError::BadRequest("invalid Content-Range".into()));
        }
        let mut f = self.open_upload_file(&s)?;
        f.set_len(s.offset)?;
        f.seek(SeekFrom::Start(s.offset))?;
        let write = (|| -> Result<()> {
            let mut remaining = end - start + 1;
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                let count = (remaining.min(buf.len() as u64)) as usize;
                let n = reader.read(&mut buf[..count])?;
                if n == 0 {
                    return Err(StoreError::BadRequest("short PATCH body".into()));
                }
                f.write_all(&buf[..n])?;
                remaining -= n as u64;
            }
            if reader.read(&mut [0u8; 1])? != 0 {
                return Err(StoreError::BadRequest("excess PATCH body".into()));
            }
            if now_ms() >= s.expires_at {
                return Err(StoreError::UploadMissing);
            }
            f.sync_all()?;
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE upload_sessions SET offset=?1 WHERE id=?2 AND offset=?3 AND state='active'",
                params![end + 1, s.id, s.offset],
            )
            .map_err(db_err)?;
            Ok(())
        })();
        if let Err(error) = write {
            // A failed SQLite commit may have committed. Re-read before
            // truncating so we never undo an acknowledged durable offset.
            let authoritative = self.upload_row(&s.id)?;
            f.set_len(authoritative.offset)?;
            f.sync_all()?;
            return Err(error);
        }
        self.bump_generation();
        self.upload_row(&s.id)
    }

    fn open_upload_file(&self, s: &UploadSession) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.upload_path(&s.id)?);
        match file {
            Ok(f) if f.metadata()?.is_file() && f.metadata()?.len() >= s.offset => Ok(f),
            Ok(_) => {
                self.fail_upload(s, "staging is short or not regular")?;
                Err(s.conflict("staging is short or not regular"))
            }
            Err(e)
                if matches!(e.kind(), io::ErrorKind::NotFound)
                    || e.raw_os_error() == Some(libc::ELOOP) =>
            {
                self.fail_upload(s, "staging is missing or a symlink")?;
                Err(s.conflict("staging is missing or a symlink"))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn remove_staging(&self, id: &str) -> Result<()> {
        let path = self.upload_path(id)?;
        match fs::symlink_metadata(&path) {
            Ok(md) if md.is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => fs::remove_file(path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.uploads_root())
    }

    fn fail_upload(&self, s: &UploadSession, why: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE upload_sessions SET state='failed',error=?1 WHERE id=?2",
                params![why, s.id],
            )
            .map_err(db_err)?;
        self.remove_staging(&s.id)?;
        self.bump_generation();
        Ok(())
    }

    fn expire_upload(&self, s: &UploadSession) -> Result<()> {
        if s.state == "committing" {
            return Ok(());
        }
        self.remove_staging(&s.id)?;
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM upload_sessions WHERE id=?1", [&s.id])
            .map_err(db_err)?;
        self.bump_generation();
        Ok(())
    }

    pub fn upload_abort(&self, id: &str) -> Result<()> {
        let _guard = self.upload_guard(id)?;
        let s = self.upload_row(id)?;
        if s.state == "committing" {
            return Err(s.conflict("commit must finish before abort"));
        }
        if s.expires_at <= now_ms() {
            self.expire_upload(&s)?;
            return Err(StoreError::UploadMissing);
        }
        if s.state == "complete" {
            return Ok(());
        } // Never unpin a completed object.
        self.remove_staging(id)?;
        self.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE upload_sessions SET state='aborted' WHERE id=?1",
                [id],
            )
            .map_err(db_err)?;
        self.bump_generation();
        Ok(())
    }

    fn all_uploads(&self, owner: Option<&str>) -> Result<Vec<UploadSession>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare(&format!(
                "{SELECT} WHERE (?1 IS NULL OR owner=?1) ORDER BY created_at,id"
            ))
            .map_err(db_err)?;
        stmt.query_map([owner], row)
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }

    pub fn upload_list(&self, owner: Option<&str>) -> Result<Vec<UploadSession>> {
        self.sweep_uploads()?;
        Ok(self
            .all_uploads(owner)?
            .into_iter()
            .filter(|s| s.expires_at > now_ms() || s.state == "committing")
            .collect())
    }

    pub fn upload_counts(&self) -> Result<(u64, u64)> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(size),0) FROM upload_sessions
            WHERE state IN ('active','committing')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_err)
    }

    pub fn sweep_uploads(&self) -> Result<()> {
        for s in self.all_uploads(None)? {
            if s.expires_at <= now_ms() && s.state != "committing" {
                match self.upload_guard(&s.id) {
                    Ok(_guard) => {
                        let current = self.upload_row(&s.id)?;
                        if current.expires_at <= now_ms() {
                            self.expire_upload(&current)?;
                        }
                    }
                    Err(StoreError::UploadConflict { .. } | StoreError::UploadMissing) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    pub(super) fn restore_uploads(&self) -> Result<()> {
        fs::create_dir_all(self.uploads_root())?;
        fs::set_permissions(self.uploads_root(), fs::Permissions::from_mode(0o2700))?;
        sync_dir(&self.blobs_root())?;
        for s in self.all_uploads(None)? {
            if s.state == "active" {
                match self.open_upload_file(&s) {
                    Ok(f) => {
                        f.set_len(s.offset)?;
                        f.sync_all()?;
                    }
                    Err(StoreError::UploadConflict { .. }) => {} // Explicit failed row, not zero extension.
                    Err(e) => return Err(e),
                }
            } else if s.state != "committing" {
                self.remove_staging(&s.id)?;
            }
        }
        self.sweep_uploads()?;
        let known = self
            .all_uploads(None)?
            .into_iter()
            .map(|s| s.id)
            .collect::<BTreeSet<_>>();
        for entry in fs::read_dir(self.uploads_root())? {
            let entry = entry?;
            if !known.contains(&entry.file_name().to_string_lossy().to_string()) {
                if entry.file_type()?.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                } else {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        sync_dir(&self.uploads_root())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{options, store};
    use super::*;

    fn create(size: u64) -> UploadCreate {
        UploadCreate {
            owner: "uploader".into(),
            size,
            expected_hash: None,
            mime: "application/octet-stream".into(),
            name: None,
            key: Some("retry-key".into()),
        }
    }

    #[test]
    fn upload_lifecycle_offset_mismatch_and_failed_patch_rollback() {
        let (_dir, store) = store();
        let opts = create(6);
        let (s, new) = store.upload_create(&opts).unwrap();
        assert!(new);
        assert_eq!(store.upload_create(&opts).unwrap().0.id, s.id);
        assert_eq!(
            store
                .upload_append(&s.id, 0, 2, 6, &b"abc"[..])
                .unwrap()
                .offset,
            3
        );
        assert!(matches!(
            store.upload_append(&s.id, 0, 2, 6, &b"abc"[..]),
            Err(StoreError::UploadConflict { offset: 3, .. })
        ));
        assert!(store.upload_append(&s.id, 3, 5, 6, &b"d"[..]).is_err());
        assert_eq!(
            fs::metadata(store.upload_path(&s.id).unwrap())
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            store
                .upload_append(&s.id, 3, 5, 6, &b"def"[..])
                .unwrap()
                .offset,
            6
        );
        store.upload_abort(&s.id).unwrap();
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
    }

    #[test]
    fn crash_after_patch_fsync_before_row_restores_previous_offset() {
        let (dir, store) = store();
        let (s, _) = store.upload_create(&create(6)).unwrap();
        store.upload_append(&s.id, 0, 2, 6, &b"abc"[..]).unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(store.upload_path(&s.id).unwrap())
            .unwrap();
        f.write_all(b"def").unwrap();
        f.sync_all().unwrap();
        drop(f);
        fs::write(store.blobs_root().join(".tmp/junk"), b"temporary").unwrap();
        let orphan = store.uploads_root().join(uuid::Uuid::new_v4().to_string());
        fs::write(&orphan, b"orphan").unwrap();
        drop(store);
        let store = Store::open(dir.path(), options()).unwrap();
        assert_eq!(store.upload_status(&s.id).unwrap().offset, 3);
        assert_eq!(fs::read(store.upload_path(&s.id).unwrap()).unwrap(), b"abc");
        assert!(!orphan.exists());
        assert!(!store.blobs_root().join(".tmp/junk").exists());
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 6);
        store.upload_append(&s.id, 3, 5, 6, &b"def"[..]).unwrap();
    }

    #[test]
    fn short_staging_becomes_failed_without_extending_or_failing_startup() {
        let (dir, store) = store();
        let (s, _) = store.upload_create(&create(3)).unwrap();
        store.upload_append(&s.id, 0, 2, 3, &b"abc"[..]).unwrap();
        fs::write(store.upload_path(&s.id).unwrap(), b"a").unwrap();
        drop(store);
        let store = Store::open(dir.path(), options()).unwrap();
        assert_eq!(store.upload_status(&s.id).unwrap().state, "failed");
        assert!(!store.upload_path(&s.id).unwrap().exists());
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
    }

    #[test]
    fn upload_expiry_and_nonblocking_writer_admission() {
        let (_dir, store) = store();
        let (s, _) = store.upload_create(&create(3)).unwrap();
        let guard = store.upload_guard(&s.id).unwrap();
        assert!(matches!(
            store.upload_append(&s.id, 0, 2, 3, &b"abc"[..]),
            Err(StoreError::UploadConflict { .. })
        ));
        assert!(matches!(
            store.upload_abort(&s.id),
            Err(StoreError::UploadConflict { .. })
        ));
        drop(guard);
        store
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE upload_sessions SET expires_at=0 WHERE id=?1",
                [&s.id],
            )
            .unwrap();
        assert!(matches!(
            store.upload_status(&s.id),
            Err(StoreError::UploadMissing)
        ));
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
    }

    #[test]
    fn durable_reservations_block_ephemeral_admission_and_pins_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let opts = StoreOptions {
            quota_total_bytes: 100,
            ..options()
        };
        let store = Store::open(dir.path(), opts.clone()).unwrap();
        store.upload_create(&create(80)).unwrap();
        drop(store);
        let store = Store::open(dir.path(), opts).unwrap();
        assert!(matches!(
            store.reserve_upload("other", Some(30)),
            Err(StoreError::QuotaTotal { .. })
        ));
        let hash = blob::put(&store.blobs_root(), &[1; 30]).unwrap();
        assert!(matches!(
            store.pin(&hash, "other"),
            Err(StoreError::QuotaTotal { .. })
        ));
    }

    #[test]
    fn upload_refusal_creates_no_staging_and_full_sync_is_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                quota_total_bytes: 2,
                ..options()
            },
        )
        .unwrap();
        assert!(matches!(
            store.upload_create(&create(3)),
            Err(StoreError::QuotaTotal { .. })
        ));
        assert_eq!(fs::read_dir(store.uploads_root()).unwrap().count(), 0);
        assert_eq!(
            store
                .db
                .lock()
                .unwrap()
                .query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn active_session_limit_counts_zero_byte_uploads() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open_with_uploads(
            dir.path(),
            options(),
            UploadLimits {
                per_owner: 1,
                total: 1,
                ..UploadLimits::default()
            },
        )
        .unwrap();
        store.upload_create(&create(0)).unwrap();
        let mut second = create(0);
        second.key = None;
        assert!(matches!(
            store.upload_create(&second),
            Err(StoreError::UploadLimit)
        ));
        assert_eq!(fs::read_dir(store.uploads_root()).unwrap().count(), 1);
    }
}
