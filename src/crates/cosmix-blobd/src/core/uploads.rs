//! Durable lane sessions. The database offset is authoritative; file length
//! may run ahead after a crash, but must never be used to advance the offset.
use super::*;
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

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

// Decimal TEXT preserves the entire unsigned dev/ino domain at SQLite's
// signed-integer boundary. Legacy unfinished sessions cannot be backfilled
// safely: the current path is not evidence of its create-time identity.
pub(super) const INODE_SCHEMA: &str = "
ALTER TABLE upload_sessions ADD COLUMN staging_dev TEXT;
ALTER TABLE upload_sessions ADD COLUMN staging_ino TEXT;
UPDATE upload_sessions SET state='failed',error='corrupt staging: no recorded create-time inode'
WHERE state IN ('active','committing');
";

const SELECT: &str = "SELECT id,owner,upload_key,size,offset,expected_hash,mime,name,
created_at,expires_at,state,actual_hash,result,error,staging_dev,staging_ino FROM upload_sessions";
pub(super) const RECEIPT_TTL_MS: i64 = 86_400_000;
const MIN_RECEIPT_LIMIT: u64 = 1024;

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
    fn receipt_limit(&self) -> Result<u64> {
        let twice_total = self
            .total
            .checked_mul(2)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| StoreError::BadRequest("upload receipt bound overflow".into()))?;
        Ok(MIN_RECEIPT_LIMIT.max(twice_total))
    }

    pub fn from_config(cfg: &Config) -> Result<Self> {
        let limits = Self {
            ttl_ms: cfg
                .upload_ttl
                .checked_mul(1000)
                .and_then(|n| i64::try_from(n).ok())
                .ok_or_else(|| {
                    StoreError::BadRequest("upload_ttl overflows milliseconds".into())
                })?,
            per_owner: cfg.upload_per_owner,
            total: cfg.upload_total,
        };
        limits.validate()?;
        Ok(limits)
    }

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
    pub staging_dev: Option<String>,
    pub staging_ino: Option<String>,
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
        size: sql_u64(r, 3)?,
        offset: sql_u64(r, 4)?,
        expected_hash: r.get(5)?,
        mime: r.get(6)?,
        name: r.get(7)?,
        created_at: sql_timestamp(r, 8)?,
        expires_at: sql_timestamp(r, 9)?,
        state: r.get(10)?,
        actual_hash: r.get(11)?,
        result,
        error: r.get(13)?,
        staging_dev: r.get(14)?,
        staging_ino: r.get(15)?,
    })
}

pub(crate) struct UploadGuard {
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

fn check_upload_device(cas_device: u64, staging_device: u64) -> Result<()> {
    if cas_device != staging_device {
        return Err(StoreError::BadRequest(
            "blobs/.uploads must be on the CAS filesystem (hard-link publication requires the same device)".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static UPLOAD_LINK_PROBE_EXDEV: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static UPLOAD_SETGID_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

// Group inheritance is optional: unprivileged containers can refuse setgid.
// The caller establishes private 0700 permissions first, including on restore.
fn apply_upload_setgid(path: &Path) {
    let setgid = || -> io::Result<()> {
        #[cfg(test)]
        if let Some(errno) = UPLOAD_SETGID_ERROR.with(|error| error.take()) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o2700))
    };
    if let Err(error) = setgid() {
        tracing::warn!(
            target: "cosmix_blobd",
            "setgid {} (mode 2700) failed: {error}; upload staging stays private (mode 0700)",
            path.display()
        );
    }
}

fn probe_upload_links(uploads: &Path, temporary: &Path) -> Result<()> {
    let name = format!(".upload-link-probe-{}", uuid::Uuid::new_v4());
    let source = uploads.join(&name);
    let target = temporary.join(&name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&source)?;
    drop(file);
    let link = || -> io::Result<()> {
        #[cfg(test)]
        if UPLOAD_LINK_PROBE_EXDEV.with(|flag| flag.get()) {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        fs::hard_link(&source, &target)
    };
    let result = link();
    // Always remove our source. Only remove the target if our atomic link
    // created it; even a UUID collision must not delete somebody else's file.
    let target_cleanup = if result.is_ok() {
        fs::remove_file(&target)
    } else {
        Ok(())
    };
    let source_cleanup = fs::remove_file(&source);
    if let Err(error) = result {
        if error.raw_os_error() == Some(libc::EXDEV) {
            return Err(StoreError::BadRequest(
                "upload staging hard-link probe failed: blobs/.uploads and blobs/.tmp must share a link-compatible mount (EXDEV)".into(),
            ));
        }
        return Err(error.into());
    }
    target_cleanup?;
    source_cleanup?;
    Ok(())
}

fn permanent_commit_error(error: &StoreError) -> bool {
    match error {
        StoreError::QuotaOwner { .. }
        | StoreError::QuotaTotal { .. }
        | StoreError::UploadVerify
        | StoreError::Mds(cosmix_mds::Error::BlobCorrupt(_)) => true,
        StoreError::Io(e) | StoreError::Mds(cosmix_mds::Error::Io(e)) => {
            matches!(
                e.raw_os_error(),
                Some(libc::EXDEV | libc::EACCES | libc::EPERM)
            )
        }
        _ => false,
    }
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

    pub(crate) fn upload_guard(&self, id: &str) -> Result<UploadGuard> {
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
            .query_map([session], |r| Ok((r.get::<_, String>(0)?, sql_u64(r, 1)?)))
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
        let sql_size = sql_int(opts.size)?;
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
        let (active, owner_active, receipts): (u64, u64, u64) = tx
            .query_row(
                "SELECT COALESCE(SUM(state IN ('active','committing')),0),
            COALESCE(SUM(owner=?1 AND state IN ('active','committing')),0),
            COALESCE(SUM(state IN ('complete','failed')),0) FROM upload_sessions",
                [&opts.owner],
                |r| Ok((sql_u64(r, 0)?, sql_u64(r, 1)?, sql_u64(r, 2)?)),
            )
            .map_err(db_err)?;
        // Reserve one future receipt slot per live session. Never evict an
        // unexpired receipt, and never charge terminal rows to an owner slot.
        if active >= self.upload_limits.total as u64
            || owner_active >= self.upload_limits.per_owner as u64
            || receipts.saturating_add(active) >= self.upload_limits.receipt_limit()?
        {
            return Err(StoreError::UploadLimit);
        }
        let reserved = self.reservation_totals(&tx, &holds, None, None)?;
        let used: u64 = tx
            .query_row(
                "SELECT COALESCE((SELECT used_bytes FROM quota WHERE owner=?1),0)",
                [&opts.owner],
                |r| sql_u64(r, 0),
            )
            .map_err(db_err)?;
        let total: u64 = tx
            .query_row("SELECT COALESCE(SUM(used_bytes),0) FROM quota", [], |r| {
                sql_u64(r, 0)
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
        let identity = file.metadata()?;
        let created = now_ms();
        tx.execute(
            "INSERT INTO upload_sessions
            (id,owner,upload_key,size,expected_hash,mime,name,created_at,expires_at,state,staging_dev,staging_ino)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'active',?10,?11)",
            params![
                id,
                opts.owner,
                opts.key,
                sql_size,
                opts.expected_hash.as_ref().map(blob::hex),
                opts.mime,
                opts.name,
                created,
                created + self.upload_limits.ttl_ms,
                identity.dev().to_string(),
                identity.ino().to_string()
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
        let guard = self.upload_begin_patch(id, start, end, total)?;
        self.upload_append_guarded(&guard, start, end, total, reader)
    }

    /// Acquire and validate before the lane starts consuming its body.
    pub(crate) fn upload_begin_patch(
        &self,
        id: &str,
        start: u64,
        end: u64,
        total: u64,
    ) -> Result<UploadGuard> {
        let guard = self.upload_guard(id)?;
        let s = self.upload_row(id)?;
        if s.expires_at <= now_ms() && s.state != "committing" {
            self.expire_upload(&s)?;
            return Err(StoreError::UploadMissing);
        }
        if s.state != "active" {
            return Err(s.conflict("session is not active"));
        }
        if s.offset != start {
            return Err(s.conflict("offset mismatch"));
        }
        if total != s.size || start > end || end >= total {
            return Err(StoreError::BadRequest("invalid Content-Range".into()));
        }
        Ok(guard)
    }

    pub(crate) fn upload_append_guarded(
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
            let next_offset = sql_int(end + 1)?;
            let old_offset = sql_int(s.offset)?;
            db.execute(
                "UPDATE upload_sessions SET offset=?1 WHERE id=?2 AND offset=?3 AND state='active'",
                params![next_offset, s.id, old_offset],
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
            // A damaged staging entry must not block startup on a FIFO.
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.upload_path(&s.id)?);
        match file {
            Ok(f) => {
                // fstat the descriptor before any truncate/write, including
                // restore and commit preparation. Never modify an aliased CAS
                // inode, even when a link was created outside the Bus API.
                let md = f.metadata()?;
                if !md.is_file()
                    || md.len() < s.offset
                    || md.nlink() != 1
                    || s.staging_dev.as_deref() != Some(md.dev().to_string().as_str())
                    || s.staging_ino.as_deref() != Some(md.ino().to_string().as_str())
                {
                    self.fail_upload(
                        s,
                        "corrupt staging: short, non-regular, linked or replaced inode",
                    )?;
                    return Err(s.conflict("corrupt staging inode"));
                }
                Ok(f)
            }
            Err(e)
                if matches!(e.kind(), io::ErrorKind::NotFound)
                    || matches!(e.raw_os_error(), Some(libc::ELOOP | libc::EISDIR)) =>
            {
                self.fail_upload(s, "staging is missing or not regular")?;
                Err(s.conflict("staging is missing or not regular"))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn remove_staging(&self, id: &str) -> Result<()> {
        let path = self.upload_path(id)?;
        match fs::symlink_metadata(&path) {
            Ok(md) if md.is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => fs::remove_file(path)?,
            // Retained receipts are swept repeatedly; an already absent
            // source needs no directory sync on every sweep or create.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        sync_dir(&self.uploads_root())
    }

    fn fail_upload(&self, s: &UploadSession, why: &str) -> Result<()> {
        // The durable reservation is the active/committing row itself. This
        // transaction drops both its charge and GC protection atomically.
        let holds = self.reserved.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute(
            "UPDATE upload_sessions SET state='failed',error=?1,expires_at=?3 WHERE id=?2",
            params![why, s.id, now_ms() + RECEIPT_TTL_MS],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        drop(db);
        drop(holds);
        if let Err(error) = self.remove_staging(&s.id) {
            tracing::warn!("failed upload staging cleanup deferred: {error}");
        }
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
            .execute("DELETE FROM upload_sessions WHERE id=?1", [id])
            .map_err(db_err)?;
        self.bump_generation();
        Ok(())
    }

    /// Commit a complete session, or replay its durable receipt. The bool is
    /// true only for a new completion (HTTP 201); replay returns HTTP 200.
    pub fn upload_commit(&self, id: &str) -> Result<(Reference, bool)> {
        let _guard = self.upload_guard(id)?;
        let s = self.upload_row(id)?;
        if s.expires_at <= now_ms() && s.state != "committing" {
            self.expire_upload(&s)?;
            return Err(StoreError::UploadMissing);
        }
        if s.state == "complete" {
            let reference = s
                .result
                .as_ref()
                .and_then(Reference::from_json)
                .ok_or_else(|| StoreError::Db("invalid upload completion receipt".into()))?;
            return Ok((reference, false));
        }
        let prepared = if s.state == "active" {
            self.prepare_upload_commit(&s)?
        } else if s.state == "committing" {
            s
        } else {
            return Err(s.conflict("session is not active"));
        };
        self.finish_upload_commit(&prepared).map(|r| (r, true))
    }

    fn prepare_upload_commit(&self, s: &UploadSession) -> Result<UploadSession> {
        if s.offset != s.size {
            return Err(s.conflict("upload incomplete"));
        }
        let mut file = self.open_upload_file(s)?;
        file.set_len(s.offset)?; // Discard any uncommitted tail before hashing.
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hash = hasher.finalize().to_hex().to_string();
        if s.expected_hash
            .as_ref()
            .is_some_and(|expected| expected != &hash)
        {
            self.fail_upload(s, "expected BLAKE3 mismatch")?;
            return Err(StoreError::UploadVerify);
        }
        file.sync_all()?;
        if s.expires_at <= now_ms() {
            self.expire_upload(s)?;
            return Err(StoreError::UploadMissing);
        }
        let holds = self.reserved.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let totals = self.reservation_totals(&db, &holds, None, Some(&s.id))?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        if let Err(error) = check_pin_capacity(
            &tx,
            &hash,
            &s.owner,
            s.size,
            self.options.owner_limit(&s.owner),
            self.options.quota_total_bytes,
            &totals,
        ) {
            drop(tx);
            drop(db);
            drop(holds);
            if permanent_commit_error(&error) {
                self.fail_upload(s, &error.to_string())?;
            }
            return Err(error);
        }
        tx.execute(
            "UPDATE upload_sessions SET state='committing',actual_hash=?1
            WHERE id=?2 AND state='active'",
            params![hash, s.id],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        drop(db);
        drop(holds);
        self.upload_row(&s.id)
    }

    fn finish_upload_commit(&self, s: &UploadSession) -> Result<Reference> {
        self.finish_upload_commit_using(s, blob::publish_staged_preserving_source)
    }

    fn finish_upload_commit_using(
        &self,
        s: &UploadSession,
        publish: impl FnOnce(&Path, &Path, &BlobHash) -> cosmix_mds::Result<u64>,
    ) -> Result<Reference> {
        let result = self.finish_upload_commit_inner(s, publish);
        if let Err(error) = &result
            && permanent_commit_error(error)
        {
            self.fail_upload(s, &error.to_string())?;
        }
        result
    }

    fn finish_upload_commit_inner(
        &self,
        s: &UploadSession,
        publish: impl FnOnce(&Path, &Path, &BlobHash) -> cosmix_mds::Result<u64>,
    ) -> Result<Reference> {
        let hash = s
            .actual_hash
            .as_deref()
            .and_then(blob::from_hex)
            .ok_or_else(|| StoreError::Db("committing session has no valid hash".into()))?;
        // The preserve-source primitive is replayable after every filesystem
        // boundary. A committing row protects this hash from blob.gc.
        let staged = self.upload_path(&s.id)?;
        match fs::symlink_metadata(&staged) {
            Ok(md)
                if md.is_file()
                    && md.len() == s.size
                    && s.staging_dev.as_deref() == Some(md.dev().to_string().as_str())
                    && s.staging_ino.as_deref() == Some(md.ino().to_string().as_str()) =>
            {
                publish(&self.blobs_root(), &staged, &hash)?;
            }
            Ok(_) => {
                self.fail_upload(s, "committing staging is short or not regular")?;
                return Err(s.conflict("invalid committing staging"));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Missing staging is explicit failure even if a CAS entry was
                // published. No invented offset, no unverified pin; GC owns
                // any now-unpinned CAS entry after its grace window.
                self.fail_upload(s, "committing staging is missing")?;
                return Err(s.conflict("missing committing staging"));
            }
            Err(e) => return Err(e.into()),
        }
        let holds = self.reserved.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let totals = self.reservation_totals(&db, &holds, None, Some(&s.id))?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute("INSERT OR IGNORE INTO blob_attrs(hash,mime,name_hint,origin,first_put) VALUES (?1,?2,?3,?4,?5)",
            params![blob::hex(&hash),s.mime,s.name,self.options.origin,now_ms()]).map_err(db_err)?;
        let (mime, name, origin): (String, Option<String>, String) = tx
            .query_row(
                "SELECT mime,name_hint,origin FROM blob_attrs WHERE hash=?1",
                [blob::hex(&hash)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(db_err)?;
        pin_with_cap(
            &tx,
            &blob::hex(&hash),
            &s.owner,
            s.size,
            self.options.owner_limit(&s.owner),
            self.options.quota_total_bytes,
            &totals,
        )?;
        let reference = Reference {
            hash,
            size: s.size,
            mime,
            name,
            origin,
        };
        tx.execute("UPDATE upload_sessions SET state='complete',result=?1,expires_at=?2 WHERE id=?3 AND state='committing'",
            params![reference.to_json().to_string(),now_ms() + RECEIPT_TTL_MS,s.id]).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        drop(db);
        drop(holds);
        self.bump_generation();
        // Receipt is the truth now. A cleanup failure must not turn a durable
        // completion into a failure; restart/sweep retries the unlink.
        if let Err(error) = self.remove_staging(&s.id) {
            tracing::warn!("completed upload staging cleanup deferred: {error}");
        }
        Ok(reference)
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
                |r| Ok((sql_u64(r, 0)?, sql_u64(r, 1)?)),
            )
            .map_err(db_err)
    }

    pub fn sweep_uploads(&self) -> Result<()> {
        for s in self.all_uploads(None)? {
            if matches!(s.state.as_str(), "complete" | "failed" | "aborted")
                && let Ok(_guard) = self.upload_guard(&s.id)
            {
                self.remove_staging(&s.id)?;
            }
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
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.uploads_root())?;
        check_upload_device(
            fs::metadata(self.blobs_root())?.dev(),
            fs::metadata(self.uploads_root())?.dev(),
        )?;
        // Privacy is required; the setgid upgrade is only best-effort.
        fs::set_permissions(self.uploads_root(), fs::Permissions::from_mode(0o700))?;
        apply_upload_setgid(&self.uploads_root());
        probe_upload_links(&self.uploads_root(), &self.blobs_root().join(".tmp"))?;
        sync_dir(&self.blobs_root())?;
        self.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE upload_sessions SET expires_at=MIN(expires_at,?1) WHERE state='failed'",
                [now_ms() + RECEIPT_TTL_MS],
            )
            .map_err(db_err)?;
        for s in self.all_uploads(None)? {
            if s.state == "aborted" {
                self.expire_upload(&s)?;
            } else if s.state == "committing" {
                // Recover publication and settlement before expiry/reconcile.
                // A transient disk error leaves the protected row for a
                // client commit retry; one damaged session never blocks open.
                if let Err(error) = self.finish_upload_commit(&s) {
                    tracing::warn!("upload commit recovery deferred: {error}");
                }
            } else if s.state == "active" {
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
    fn startup_setgid_refusal_keeps_private_staging_and_opens_store() {
        for errno in [libc::EPERM, libc::ENOTSUP] {
            let dir = tempfile::TempDir::new().unwrap();
            // Exercise both initial creation and reopening an existing store.
            for _ in 0..2 {
                UPLOAD_SETGID_ERROR.with(|error| error.set(Some(errno)));
                let result = Store::open(dir.path(), options());
                let pending = UPLOAD_SETGID_ERROR.with(|error| error.take());
                let store = result.unwrap();
                assert_eq!(pending, None, "setgid failure injection was not consumed");
                assert_eq!(
                    fs::metadata(store.uploads_root()).unwrap().mode() & 0o7777,
                    0o700
                );
                let (session, _) = store.upload_create(&create(0)).unwrap();
                assert_eq!(session.offset, 0);
                store.upload_abort(&session.id).unwrap();
            }
        }
    }

    #[test]
    fn startup_link_probe_exdev_refuses_and_removes_both_probe_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        UPLOAD_LINK_PROBE_EXDEV.with(|flag| flag.set(true));
        let result = Store::open(dir.path(), options());
        UPLOAD_LINK_PROBE_EXDEV.with(|flag| flag.set(false));
        assert!(
            matches!(result, Err(StoreError::BadRequest(message)) if message.contains("hard-link probe") && message.contains("EXDEV"))
        );
        for child in [".uploads", ".tmp"] {
            assert_eq!(
                fs::read_dir(dir.path().join("blobs").join(child))
                    .unwrap()
                    .count(),
                0
            );
        }
        // The successful startup probe also leaves no files behind.
        let store = Store::open(dir.path(), options()).unwrap();
        for child in [".uploads", ".tmp"] {
            assert_eq!(
                fs::read_dir(store.blobs_root().join(child))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn staging_device_mismatch_is_a_clear_startup_refusal() {
        assert!(check_upload_device(1, 1).is_ok());
        assert!(
            matches!(check_upload_device(1, 2), Err(StoreError::BadRequest(message)) if message.contains("CAS filesystem"))
        );
        // Exercise actual startup when the host supplies a second filesystem.
        let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let dir = tempfile::TempDir::new().unwrap();
        if fs::metadata(dir.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
            return;
        }
        fs::create_dir_all(dir.path().join("blobs")).unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("blobs/.uploads")).unwrap();
        assert!(
            matches!(Store::open(dir.path(), options()), Err(StoreError::BadRequest(message)) if message.contains("CAS filesystem"))
        );
    }

    #[test]
    fn permanent_commit_publish_errors_release_reservation_and_list_failure() {
        for errno in [libc::EXDEV, libc::EACCES, libc::EPERM] {
            let (_dir, store) = store();
            let s = complete_bytes(&store, b"upload");
            let s = store.prepare_upload_commit(&s).unwrap();
            assert!(
                store
                    .finish_upload_commit_using(&s, |_, _, _| {
                        Err(cosmix_mds::Error::Io(io::Error::from_raw_os_error(errno)))
                    })
                    .is_err()
            );
            assert_failed_and_released(&store, &s.id);
        }
    }

    fn assert_failed_and_released(store: &Store, id: &str) {
        let rows = store.upload_list(Some("uploader")).unwrap();
        let row = rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.state, "failed");
        assert!(!row.to_json()["error"].as_str().unwrap().is_empty());
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
        assert_eq!(store.upload_counts().unwrap().0, 0);
    }

    #[test]
    fn lowered_quota_fails_before_intent_and_during_recovery() {
        for committing in [false, true] {
            let (_dir, mut store) = store();
            let s = complete_bytes(&store, b"upload");
            if committing {
                store.prepare_upload_commit(&s).unwrap();
            }
            store.options.quota_total_bytes = 1;
            assert!(matches!(
                store.upload_commit(&s.id),
                Err(StoreError::QuotaTotal { .. })
            ));
            assert_failed_and_released(&store, &s.id);
            if !committing {
                assert!(store.upload_row(&s.id).unwrap().actual_hash.is_none());
                assert!(
                    !blob::blob_path(&store.blobs_root(), &blob::hash_bytes(b"upload")).exists()
                );
            }
        }
    }

    #[test]
    fn corrupt_existing_cas_and_staging_bitrot_fail_permanently() {
        for existing in [false, true] {
            let (_dir, store) = store();
            let s = complete_bytes(&store, b"upload");
            let s = store.prepare_upload_commit(&s).unwrap();
            if existing {
                let path = blob::blob_path(&store.blobs_root(), &blob::hash_bytes(b"upload"));
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"broken").unwrap();
            } else {
                fs::write(store.upload_path(&s.id).unwrap(), b"broken").unwrap();
            }
            assert!(matches!(
                store.upload_commit(&s.id),
                Err(StoreError::Mds(cosmix_mds::Error::BlobCorrupt(_)))
            ));
            assert_failed_and_released(&store, &s.id);
        }
    }

    #[test]
    fn transient_commit_error_keeps_reservation_for_retry() {
        let (_dir, store) = store();
        let s = complete_bytes(&store, b"upload");
        let s = store.prepare_upload_commit(&s).unwrap();
        assert!(
            store
                .finish_upload_commit_using(&s, |_, _, _| {
                    Err(cosmix_mds::Error::Io(io::Error::from_raw_os_error(
                        libc::EIO,
                    )))
                })
                .is_err()
        );
        assert_eq!(store.upload_row(&s.id).unwrap().state, "committing");
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 6);
        store.upload_commit(&s.id).unwrap();
    }

    #[test]
    fn external_staging_hardlink_fails_patch_without_mutating_published_bytes() {
        let (dir, store) = store();
        let (s, _) = store.upload_create(&create(2)).unwrap();
        store.upload_append(&s.id, 0, 0, 2, &b"a"[..]).unwrap();
        let alias = dir.path().join("external-link");
        fs::hard_link(store.upload_path(&s.id).unwrap(), &alias).unwrap();
        let mut opts = PutOptions::new("other");
        opts.mode = PutMode::HardLink;
        opts.immutable = true;
        assert!(store.put(&alias, &opts).is_err());
        // A process with filesystem access can still create a CAS link itself;
        // PATCH's independent inode guard must protect that externally linked file.
        let cas = blob::blob_path(&store.blobs_root(), &blob::hash_bytes(b"a"));
        fs::create_dir_all(cas.parent().unwrap()).unwrap();
        fs::hard_link(&alias, &cas).unwrap();
        assert!(matches!(
            store.upload_append(&s.id, 1, 1, 2, &b"b"[..]),
            Err(StoreError::UploadConflict { .. })
        ));
        assert_eq!(store.upload_status(&s.id).unwrap().state, "failed");
        assert!(
            store
                .upload_status(&s.id)
                .unwrap()
                .error
                .unwrap()
                .contains("corrupt staging")
        );
        assert_eq!(fs::read(&alias).unwrap(), b"a");
        assert_eq!(fs::read(cas).unwrap(), b"a");
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
    }

    #[test]
    fn replaced_staging_inode_fails_before_truncation_or_patch() {
        let (_dir, store) = store();
        let (s, _) = store.upload_create(&create(2)).unwrap();
        let path = store.upload_path(&s.id).unwrap();
        let md = fs::metadata(&path).unwrap();
        assert_eq!(s.staging_dev, Some(md.dev().to_string()));
        assert_eq!(s.staging_ino, Some(md.ino().to_string()));
        fs::rename(&path, path.with_extension("original")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let held = File::open(&path).unwrap();
        assert!(matches!(
            store.upload_append(&s.id, 0, 0, 2, &b"a"[..]),
            Err(StoreError::UploadConflict { .. })
        ));
        assert_eq!(held.metadata().unwrap().len(), 11);
        assert_eq!(store.upload_status(&s.id).unwrap().state, "failed");
    }

    #[test]
    fn patch_sqlite_failure_after_fsync_truncates_to_durable_offset_before_retry() {
        let (_dir, store) = store();
        let (s, _) = store.upload_create(&create(4)).unwrap();
        store.upload_append(&s.id, 0, 1, 4, &b"ab"[..]).unwrap();
        // The offset UPDATE runs only after the chunk's sync_all. Inject an
        // error there, after the file has advanced but before a durable offset.
        // The caller sees a database failure and must re-read authoritative
        // state instead of inferring whether the write committed.
        store
            .db
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_patch_offset AFTER UPDATE OF offset ON upload_sessions
             BEGIN SELECT RAISE(ABORT, 'injected offset failure after fsync'); END;",
            )
            .unwrap();
        let error = store.upload_append(&s.id, 2, 3, 4, &b"cd"[..]).unwrap_err();
        assert!(
            matches!(error, StoreError::Db(ref message) if message.contains("injected offset failure"))
        );
        let current = store.upload_status(&s.id).unwrap();
        assert_eq!(current.offset, 2);
        assert_eq!(current.state, "active");
        assert_eq!(fs::read(store.upload_path(&s.id).unwrap()).unwrap(), b"ab");
        // Admission has been released only after truncate-back and fsync.
        let guard = store.upload_guard(&s.id).unwrap();
        assert_eq!(
            fs::metadata(store.upload_path(&s.id).unwrap())
                .unwrap()
                .len(),
            2
        );
        drop(guard);
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_patch_offset")
            .unwrap();
        let done = store.upload_append(&s.id, 2, 3, 4, &b"cd"[..]).unwrap();
        assert_eq!(done.offset, 4);
        assert_eq!(
            fs::read(store.upload_path(&s.id).unwrap()).unwrap(),
            b"abcd"
        );
    }

    #[test]
    fn negative_upload_integers_are_corruption_and_checks_remain() {
        let (_dir, store) = store();
        let (s, _) = store.upload_create(&create(0)).unwrap();
        for (column, original) in [
            ("size", 0_i64),
            ("offset", 0_i64),
            ("created_at", s.created_at),
            ("expires_at", s.expires_at),
        ] {
            let update = format!("UPDATE upload_sessions SET {column}=?1 WHERE id=?2");
            {
                let db = store.db.lock().unwrap();
                db.execute_batch("PRAGMA ignore_check_constraints=ON")
                    .unwrap();
                db.execute(&update, params![-1_i64, s.id]).unwrap();
                db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                    .unwrap();
            }
            assert!(
                matches!(
                    store.upload_status(&s.id),
                    Err(StoreError::CorruptInteger { value: -1, .. })
                ),
                "{column}"
            );
            store
                .db
                .lock()
                .unwrap()
                .execute(&update, params![original, s.id])
                .unwrap();
        }
        let db = store.db.lock().unwrap();
        assert!(
            db.execute("UPDATE upload_sessions SET size=-1 WHERE id=?1", [&s.id])
                .is_err()
        );
        assert!(
            db.execute("UPDATE upload_sessions SET offset=-1 WHERE id=?1", [&s.id])
                .is_err()
        );
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
        assert!(store.upload_append(&s.id, 3, 5, 6, &b"defg"[..]).is_err());
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
        let store = Store::open(dir.path().join("store"), options()).unwrap();
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
        let store = Store::open(dir.path().join("store"), options()).unwrap();
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
    fn missing_staging_fails_only_its_session_on_restart() {
        let (dir, store) = store();
        let (missing, _) = store.upload_create(&create(3)).unwrap();
        let (valid, _) = store
            .upload_create(&UploadCreate {
                key: None,
                ..create(3)
            })
            .unwrap();
        fs::remove_file(store.upload_path(&missing.id).unwrap()).unwrap();
        drop(store);
        let store = Store::open(dir.path().join("store"), options()).unwrap();
        assert_eq!(store.upload_status(&missing.id).unwrap().state, "failed");
        assert_eq!(store.upload_status(&valid.id).unwrap().state, "active");
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 3);
        store
            .upload_append(&valid.id, 0, 2, 3, &b"abc"[..])
            .unwrap();
    }

    #[test]
    fn durable_reservations_block_ephemeral_admission_and_pins_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let opts = StoreOptions {
            quota_total_bytes: 100,
            ..options()
        };
        let store = Store::open(dir.path().join("store"), opts.clone()).unwrap();
        store.upload_create(&create(80)).unwrap();
        drop(store);
        let store = Store::open(dir.path().join("store"), opts).unwrap();
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
            dir.path().join("store"),
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
    fn terminal_rows_do_not_consume_owner_slots_and_abort_frees_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open_with_uploads(
            dir.path(),
            options(),
            UploadLimits {
                per_owner: 1,
                total: 1,
                ttl_ms: 365 * RECEIPT_TTL_MS,
            },
        )
        .unwrap();
        let (first, _) = store.upload_create(&create(0)).unwrap();
        store.upload_abort(&first.id).unwrap();
        assert!(matches!(
            store.upload_status(&first.id),
            Err(StoreError::UploadMissing)
        ));
        let (second, new) = store.upload_create(&create(0)).unwrap();
        assert!(new);
        assert_ne!(first.id, second.id);
        let before = now_ms();
        store.fail_upload(&second, "test failure").unwrap();
        assert!(store.upload_status(&second.id).unwrap().expires_at <= now_ms() + RECEIPT_TTL_MS);
        assert!(store.upload_status(&second.id).unwrap().expires_at >= before + RECEIPT_TTL_MS);
        // More receipts than the former active limit remain replayable.
        for _ in 0..3 {
            let (s, _) = store
                .upload_create(&UploadCreate {
                    key: None,
                    ..create(0)
                })
                .unwrap();
            store.upload_commit(&s.id).unwrap();
            assert_eq!(store.upload_status(&s.id).unwrap().state, "complete");
        }
        assert_eq!(store.upload_list(None).unwrap().len(), 4);
    }

    #[test]
    fn receipt_budget_never_evicts_early_and_reclaims_expired_rows() {
        let (_dir, store) = store();
        let (seed, _) = store.upload_create(&create(0)).unwrap();
        store.upload_commit(&seed.id).unwrap();
        {
            let db = store.db.lock().unwrap();
            for _ in 1..store.upload_limits.receipt_limit().unwrap() {
                db.execute("INSERT INTO upload_sessions(id,owner,size,mime,created_at,expires_at,state,result)
                    SELECT ?1,owner,size,mime,created_at,expires_at,state,result FROM upload_sessions WHERE id=?2",
                    params![uuid::Uuid::new_v4().to_string(), seed.id]).unwrap();
            }
        }
        let next = UploadCreate {
            key: None,
            ..create(0)
        };
        assert!(matches!(
            store.upload_create(&next),
            Err(StoreError::UploadLimit)
        ));
        assert_eq!(store.upload_status(&seed.id).unwrap().state, "complete");
        store
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE upload_sessions SET expires_at=0 WHERE id=?1",
                [&seed.id],
            )
            .unwrap();
        store.upload_create(&next).unwrap();
        assert!(matches!(
            store.upload_status(&seed.id),
            Err(StoreError::UploadMissing)
        ));
    }

    #[test]
    fn receipt_bound_scales_with_configured_total_and_admits_past_1024() {
        for (total, expected) in [(64, 1024), (512, 1024), (513, 1026), (4096, 8192)] {
            let limits = UploadLimits {
                total,
                ..UploadLimits::default()
            };
            assert_eq!(limits.receipt_limit().unwrap(), expected);
        }
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open_with_uploads(
            dir.path(),
            options(),
            UploadLimits {
                total: 4096,
                ..UploadLimits::default()
            },
        )
        .unwrap();
        let (seed, _) = store.upload_create(&create(0)).unwrap();
        store.upload_commit(&seed.id).unwrap();
        {
            let mut db = store.db.lock().unwrap();
            let tx = db.transaction().unwrap();
            for _ in 1..MIN_RECEIPT_LIMIT {
                tx.execute("INSERT INTO upload_sessions(id,owner,size,mime,created_at,expires_at,state,result)
                    SELECT ?1,owner,size,mime,created_at,expires_at,state,result FROM upload_sessions WHERE id=?2",
                    params![uuid::Uuid::new_v4().to_string(), seed.id]).unwrap();
            }
            tx.commit().unwrap();
        }
        let (next, _) = store
            .upload_create(&UploadCreate {
                key: None,
                ..create(0)
            })
            .unwrap();
        assert_eq!(next.state, "active");
        assert_eq!(store.all_uploads(None).unwrap().len(), 1025);
        assert_eq!(store.upload_status(&seed.id).unwrap().state, "complete");
    }

    #[test]
    fn active_session_limit_counts_zero_byte_uploads() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open_with_uploads(
            dir.path().join("store"),
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

    fn complete_bytes(store: &Store, bytes: &[u8]) -> UploadSession {
        let (s, _) = store.upload_create(&create(bytes.len() as u64)).unwrap();
        if !bytes.is_empty() {
            store
                .upload_append(&s.id, 0, bytes.len() as u64 - 1, bytes.len() as u64, bytes)
                .unwrap();
        }
        store.upload_status(&s.id).unwrap()
    }

    #[test]
    fn crash_after_commit_intent_before_publication_recovers() {
        let (dir, store) = store();
        let s = complete_bytes(&store, b"recover");
        store.prepare_upload_commit(&s).unwrap();
        drop(store);
        let store = Store::open(dir.path().join("store"), options()).unwrap();
        let (reference, new) = store.upload_commit(&s.id).unwrap();
        assert!(!new);
        assert_eq!(reference.hash, blob::hash_bytes(b"recover"));
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
        assert_eq!(store.quota_report(None).unwrap().total.used, 7);
    }

    #[test]
    fn crash_after_publication_before_settlement_is_gc_protected_and_recovers() {
        let (dir, store) = store();
        let s = complete_bytes(&store, b"recover");
        let prepared = store.prepare_upload_commit(&s).unwrap();
        let hash = blob::from_hex(prepared.actual_hash.as_ref().unwrap()).unwrap();
        blob::publish_staged_preserving_source(
            &store.blobs_root(),
            &store.upload_path(&s.id).unwrap(),
            &hash,
        )
        .unwrap();
        let cas = blob::blob_path(&store.blobs_root(), &hash);
        File::open(&cas)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(store.gc(false).unwrap().skipped_pinned, 1);
        assert!(cas.exists());
        drop(store);
        let store = Store::open(dir.path().join("store"), options()).unwrap();
        assert_eq!(store.upload_status(&s.id).unwrap().state, "complete");
        assert_eq!(store.stat(&hash).unwrap().pins, vec!["uploader"]);
    }

    #[test]
    fn crash_after_settlement_before_cleanup_or_response_replays_same_receipt() {
        let (dir, store) = store();
        let s = complete_bytes(&store, b"recover");
        let (reference, new) = store.upload_commit(&s.id).unwrap();
        assert!(new);
        fs::hard_link(
            store.path(&reference.hash).unwrap(),
            store.upload_path(&s.id).unwrap(),
        )
        .unwrap();
        drop(store);
        let store = Store::open(dir.path().join("store"), options()).unwrap();
        assert!(!store.upload_path(&s.id).unwrap().exists());
        assert_eq!(
            store.upload_commit(&s.id).unwrap(),
            (reference.clone(), false)
        );
        store.upload_abort(&s.id).unwrap();
        assert_eq!(store.stat(&reference.hash).unwrap().pins, vec!["uploader"]);
        assert_eq!(store.quota_report(None).unwrap().total.used, 7);
    }

    #[test]
    fn hash_mismatch_discards_only_session_and_never_publishes() {
        let (_dir, store) = store();
        let mut opts = create(3);
        opts.expected_hash = Some(blob::hash_bytes(b"yes"));
        let (s, _) = store.upload_create(&opts).unwrap();
        store.upload_append(&s.id, 0, 2, 3, &b"bad"[..]).unwrap();
        assert!(matches!(
            store.upload_commit(&s.id),
            Err(StoreError::UploadVerify)
        ));
        assert_eq!(store.upload_status(&s.id).unwrap().state, "failed");
        assert!(!store.upload_path(&s.id).unwrap().exists());
        for hash in [blob::hash_bytes(b"yes"), blob::hash_bytes(b"bad")] {
            assert!(!blob::exists(&store.blobs_root(), &hash).unwrap());
        }
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
    }

    #[test]
    fn empty_session_commits_without_patch_and_receipt_is_24_hours() {
        let (_dir, store) = store();
        let (s, _) = store.upload_create(&create(0)).unwrap();
        let before = now_ms();
        let (r, new) = store.upload_commit(&s.id).unwrap();
        assert!(new);
        assert_eq!(r.hash, blob::hash_bytes(b""));
        assert!(store.upload_status(&s.id).unwrap().expires_at >= before + RECEIPT_TTL_MS);
        assert_eq!(store.upload_commit(&s.id).unwrap(), (r, false));
    }
}
