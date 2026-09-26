//! The blobd store: mds's CAS (bytes) + a blobd-owned `blobd.sqlite`
//! (attrs, pins, quotas) + an exclusive root `flock`.
//!
//! mds's `blobs.sqlite` schema is never touched — `BLOBS_LATEST` stays
//! 1 (D4). blobd holds its own read connection to it for the public
//! row API ([`cosmix_mds::blob_index`]), the refcount truth GC joins
//! against; every blobd write lands in `blobd.sqlite` beside it.
//!
//! One GC owner per root: `Store::open` takes an exclusive non-blocking
//! `flock` on `<root>/.blobd.lock` and holds it for the store's
//! lifetime; a second instance on the same root fails with
//! [`StoreError::Locked`] (main exits 2).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use cosmix_mds::Mds;
use cosmix_mds::SqliteCasMds;
use cosmix_mds::blob::{self, PutMode};
use cosmix_mds::blob_index;
use cosmix_mds::types::BlobHash;
use rusqlite::{Connection, OptionalExtension, params};

use super::config::Config;
use super::mime;
use super::reference::Reference;

/// Name of the exclusive instance lock inside the mds root.
pub const LOCK_FILE: &str = ".blobd.lock";
/// mds's `DEFAULT_GC_QUIESCENCE` (60 s): the grace a CAS file's mtime
/// must exceed before startup reconcile reports it or GC sweeps it —
/// never delete a fetch that has committed its bytes and not yet its
/// pin.
pub const DEFAULT_GC_GRACE_SECS: u64 = 60;

const BLOBD_APPLICATION_ID: i32 = 0x626C_6F62; // 'blob'
const BLOBD_LATEST: u32 = 1;
const BLOBD_V1_SQL: &str = "\
PRAGMA application_id = 0x626C6F62;        -- 'blob'
PRAGMA user_version   = 1;

CREATE TABLE blob_attrs (
    hash      TEXT PRIMARY KEY,
    mime      TEXT NOT NULL,
    name_hint TEXT,
    origin    TEXT NOT NULL,
    first_put INTEGER NOT NULL
);
CREATE INDEX idx_blob_attrs_origin ON blob_attrs (origin);

CREATE TABLE pins (
    hash    TEXT NOT NULL,
    owner   TEXT NOT NULL,
    created INTEGER NOT NULL,
    PRIMARY KEY (hash, owner)
);
CREATE INDEX idx_pins_owner ON pins (owner);

CREATE TABLE quota (
    owner      TEXT PRIMARY KEY,
    used_bytes INTEGER NOT NULL DEFAULT 0
);
";

/// Store-level errors. `Locked` is the exit-2 case; the rest map to
/// Bus rc 10 with `{"error": …}`.
#[derive(Debug)]
pub enum StoreError {
    /// Another blobd instance holds `<root>/.blobd.lock`.
    Locked(PathBuf),
    Mds(cosmix_mds::Error),
    Db(String),
    Io(io::Error),
    /// The `b3:` blob id did not parse.
    InvalidBlob(String),
    /// The blob is not present in the CAS.
    NotPresent(String),
    /// An owner is at its cap: it would use `would_use` of `limit`.
    QuotaOwner {
        owner: String,
        would_use: u64,
        limit: u64,
    },
    /// The total cap would be exceeded.
    QuotaTotal {
        would_use: u64,
        limit: u64,
    },
    /// Bad request (e.g. `mode: hardlink` without `immutable: true`).
    BadRequest(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked(p) => write!(
                f,
                "another blobd instance holds {}; one GC owner per root",
                p.display()
            ),
            Self::Mds(e) => write!(f, "mds: {e}"),
            Self::Db(e) => write!(f, "blobd.sqlite: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::InvalidBlob(s) => write!(f, "invalid blob id: {s:?}"),
            Self::NotPresent(s) => write!(f, "not_present: {s}"),
            Self::QuotaOwner {
                owner,
                would_use,
                limit,
            } => write!(
                f,
                "quota: owner {owner:?} would use {would_use} over its limit {limit}"
            ),
            Self::QuotaTotal { would_use, limit } => {
                write!(f, "quota: total would use {would_use} over the cap {limit}")
            }
            Self::BadRequest(s) => write!(f, "bad request: {s}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<cosmix_mds::Error> for StoreError {
    fn from(e: cosmix_mds::Error) -> Self {
        Self::Mds(e)
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Instance-wide quotas and identity, resolved from [`Config`].
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Node name stamped into every reference (`origin`; never an IP).
    pub origin: String,
    pub quota_total_bytes: u64,
    pub quota_owner_default_bytes: u64,
    pub owner_limits: BTreeMap<String, u64>,
}

impl StoreOptions {
    pub fn from_config(cfg: &Config, origin: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            quota_total_bytes: cfg.quota_total_bytes,
            quota_owner_default_bytes: cfg.quota_owner_default_bytes,
            owner_limits: cfg.owner_limits.clone(),
        }
    }

    fn owner_limit(&self, owner: &str) -> u64 {
        self.owner_limits
            .get(owner)
            .copied()
            .unwrap_or(self.quota_owner_default_bytes)
    }
}

/// Startup housekeeping report (testable; also logged).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    /// Files removed from `blobs/.tmp` — nothing in-flight can survive
    /// a restart.
    pub tmp_removed: u64,
    /// CAS files with no mds row and no pin, mtime older than the
    /// grace window: logged, never deleted at startup — `blob.gc`
    /// owns deletion.
    pub orphans: Vec<String>,
}

/// One `blob.list` row: a reference plus its pin owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub hash: BlobHash,
    pub size: u64,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub origin: Option<String>,
    pub first_put: Option<i64>,
    pub pins: Vec<String>,
}

/// `blob.stat` reply data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatInfo {
    pub present: bool,
    pub size: Option<u64>,
    pub mime: Option<String>,
    pub pins: Vec<String>,
    pub origin: Option<String>,
    pub first_put: Option<i64>,
}

/// `blob.quota` reply data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReport {
    pub owners: BTreeMap<String, OwnerQuota>,
    pub total: OwnerQuota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerQuota {
    pub used: u64,
    pub limit: u64,
}

/// `blob.gc` outcome. `swept` holds blob ids in hash order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcSweep {
    pub swept: Vec<String>,
    pub bytes_freed: u64,
    pub skipped_referenced: u64,
    pub skipped_pinned: u64,
    pub skipped_young: u64,
}

/// Arguments to [`Store::put`].
#[derive(Debug, Clone)]
pub struct PutOptions<'a> {
    /// Pin owner; the caller's service name when not given.
    pub owner: &'a str,
    pub mime: Option<&'a str>,
    pub name: Option<&'a str>,
    pub mode: PutMode,
    /// Required for `PutMode::HardLink`: only an immutable publisher
    /// may alias its source inode into the CAS.
    pub immutable: bool,
}

impl<'a> PutOptions<'a> {
    pub fn new(owner: &'a str) -> Self {
        Self {
            owner,
            mime: None,
            name: None,
            mode: PutMode::Copy,
            immutable: false,
        }
    }
}

/// Result of [`Store::put`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    pub reference: Reference,
    /// Whether this put added a new pin row (an idempotent re-put
    /// pins nothing).
    pub newly_pinned: bool,
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// The blobd store. `Send + Sync`: both connections live behind mutexes.
pub struct Store {
    root: PathBuf,
    mds: SqliteCasMds,
    /// blobd-owned `blobd.sqlite`: attrs, pins, quotas.
    db: Mutex<Connection>,
    /// Read connection to mds's `blobs.sqlite` for the public row API.
    /// blobd never writes here (D4: mds's schema is mds's).
    index: Mutex<Connection>,
    /// Holds the root `flock` for the store's lifetime. Never read:
    /// closing it on drop is what releases the lock.
    _lock_file: File,
    options: StoreOptions,
    startup: StartupReport,
    generation: AtomicU64,
}

impl Store {
    /// Open (or create) the store at `root`: mds root, exclusive
    /// `flock`, `blobd.sqlite` migrations, then startup housekeeping
    /// (`.tmp` sweep + orphan reconcile).
    pub fn open(root: impl Into<PathBuf>, options: StoreOptions) -> Result<Self> {
        let root = root.into();
        let mds = SqliteCasMds::open(&root)?;

        // One GC owner per root. flock locks are per open file
        // description, so a second open — even in-process — conflicts.
        let lock_path = root.join(LOCK_FILE);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        // SAFETY: flock(2) on a live fd; no pointers.
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(StoreError::Locked(lock_path));
            }
            return Err(StoreError::Io(err));
        }

        let db = open_blobd_db(&root)?;
        let index = open_index_conn(&root)?;

        let mut store = Self {
            root,
            mds,
            db: Mutex::new(db),
            index: Mutex::new(index),
            _lock_file: lock_file,
            options,
            startup: StartupReport::default(),
            generation: AtomicU64::new(0),
        };
        store.startup_housekeeping()?;
        Ok(store)
    }

    fn startup_housekeeping(&mut self) -> Result<()> {
        let tmp = self.blobs_root().join(".tmp");
        if tmp.is_dir() {
            for entry in fs::read_dir(&tmp)? {
                let entry = entry?;
                let p = entry.path();
                if p.is_dir() {
                    fs::remove_dir_all(&p)?;
                } else {
                    fs::remove_file(&p)?;
                }
                self.startup.tmp_removed += 1;
            }
        }
        if self.startup.tmp_removed > 0 {
            tracing::info!(
                target: "cosmix_blobd",
                "startup: removed {} staging file(s) under blobs/.tmp",
                self.startup.tmp_removed
            );
        }

        let grace = Duration::from_secs(DEFAULT_GC_GRACE_SECS);
        for file in self.cas_scan()? {
            let has_row = blob_index::blob_row(&self.index.lock().unwrap(), &file.hash)?.is_some();
            let pinned = !self.pin_owners(&file.hash)?.is_empty();
            if !has_row && !pinned && file.age().unwrap_or(Duration::ZERO) > grace {
                tracing::warn!(
                    target: "cosmix_blobd",
                    "startup reconcile: orphan CAS file {} (no mds row, no pin, mtime older than {}s) — left for blob.gc",
                    file.hash_hex(),
                    DEFAULT_GC_GRACE_SECS
                );
                self.startup.orphans.push(file.hash_hex());
            }
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blobs_root(&self) -> PathBuf {
        self.mds.blobs_root()
    }

    pub fn options(&self) -> &StoreOptions {
        &self.options
    }

    pub fn startup_report(&self) -> &StartupReport {
        &self.startup
    }

    /// Monotonic mutation counter (`lifecycle.generation` in props).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    // ---- Ingest ----

    /// Daemon-local ingest via `mds::put_blob_path`. The quota check
    /// runs before the copy (size from `stat`); accounting lands with
    /// the pin after the CAS commit. Idempotent: a re-put returns the
    /// same reference and pins nothing new.
    pub fn put(&self, src: &Path, opts: &PutOptions<'_>) -> Result<PutOutcome> {
        if opts.mode == PutMode::HardLink && !opts.immutable {
            return Err(StoreError::BadRequest(
                "mode \"hardlink\" requires \"immutable\": true — only a publisher that \
                 promises the source path immutable may alias its inode into the CAS"
                    .into(),
            ));
        }
        let md = fs::metadata(src)?;
        if !md.is_file() {
            return Err(StoreError::BadRequest(format!(
                "path {} is not a regular file",
                src.display()
            )));
        }

        // Quota before the copy: owner cap, then total cap.
        let size = md.len();
        let owner_used = self.owner_used(opts.owner)?;
        let owner_limit = self.options.owner_limit(opts.owner);
        let would_use = owner_used.saturating_add(size);
        if would_use > owner_limit {
            return Err(StoreError::QuotaOwner {
                owner: opts.owner.to_string(),
                would_use,
                limit: owner_limit,
            });
        }
        let total_used = self.total_used()?;
        let total_would = total_used.saturating_add(size);
        if total_would > self.options.quota_total_bytes {
            return Err(StoreError::QuotaTotal {
                would_use: total_would,
                limit: self.options.quota_total_bytes,
            });
        }

        let hash = self.mds.put_blob_path(src, opts.mode)?;
        let size = blob::size(&self.blobs_root(), &hash)?;

        let mime = opts
            .mime
            .map(str::to_string)
            .unwrap_or_else(|| mime::sniff(&os_str_lossy(opts.name, src)).to_string());
        let name = opts.name.map(str::to_string);

        let mut db = self.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO blob_attrs (hash, mime, name_hint, origin, first_put) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![blob::hex(&hash), mime, name, self.options.origin, now_ms()],
        )
        .map_err(db_err)?;
        let pinned = tx
            .execute(
                "INSERT OR IGNORE INTO pins (hash, owner, created) VALUES (?1, ?2, ?3)",
                params![blob::hex(&hash), opts.owner, now_ms()],
            )
            .map_err(db_err)?
            == 1;
        if pinned {
            bump_owner_used(&tx, opts.owner, size)?;
        }
        tx.commit().map_err(db_err)?;
        drop(db);
        self.bump_generation();

        Ok(PutOutcome {
            reference: Reference {
                hash,
                size,
                mime,
                name,
                origin: self.options.origin.clone(),
            },
            newly_pinned: pinned,
        })
    }

    // ---- Reads ----

    pub fn stat(&self, hash: &BlobHash) -> Result<StatInfo> {
        let present = blob::exists(&self.blobs_root(), hash)?;
        let size = if present {
            Some(blob::size(&self.blobs_root(), hash)?)
        } else {
            self.index
                .lock()
                .unwrap()
                .query_row(
                    "SELECT size_bytes FROM blob WHERE hash = ?1",
                    params![blob::hex(hash)],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
                .map_err(db_err)?
                .map(|n| n as u64)
        };
        let (mime, origin, first_put): (Option<String>, Option<String>, Option<i64>) = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT mime, origin, first_put FROM blob_attrs WHERE hash = ?1",
                params![blob::hex(hash)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db_err)?
            .unwrap_or((None, None, None));
        Ok(StatInfo {
            present,
            size,
            mime,
            pins: self.pin_owners(hash)?,
            origin,
            first_put,
        })
    }

    /// Absolute CAS path; `NotPresent` when the bytes are not held.
    pub fn path(&self, hash: &BlobHash) -> Result<PathBuf> {
        let p = blob::blob_path(&self.blobs_root(), hash);
        if p.exists() {
            Ok(p)
        } else {
            Err(StoreError::NotPresent(blob::hex(hash)))
        }
    }

    /// Bulk presence check, preserving input order in each bucket.
    pub fn has(&self, hashes: &[BlobHash]) -> Result<(Vec<BlobHash>, Vec<BlobHash>)> {
        let mut present = Vec::new();
        let mut missing = Vec::new();
        for h in hashes {
            if blob::exists(&self.blobs_root(), h)? {
                present.push(*h);
            } else {
                missing.push(*h);
            }
        }
        Ok((present, missing))
    }

    // ---- Pins ----

    /// Pin `hash` to `owner`. Idempotent; returns whether a pin row
    /// was added. Requires the bytes to be present.
    pub fn pin(&self, hash: &BlobHash, owner: &str) -> Result<bool> {
        if !blob::exists(&self.blobs_root(), hash)? {
            return Err(StoreError::NotPresent(blob::hex(hash)));
        }
        let size = blob::size(&self.blobs_root(), hash)?;
        let mut db = self.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO pins (hash, owner, created) VALUES (?1, ?2, ?3)",
                params![blob::hex(hash), owner, now_ms()],
            )
            .map_err(db_err)?
            == 1;
        if inserted {
            bump_owner_used(&tx, owner, size)?;
        }
        tx.commit().map_err(db_err)?;
        drop(db);
        if inserted {
            self.bump_generation();
        }
        Ok(inserted)
    }

    /// Drop `owner`'s pin on `hash`. Idempotent; returns whether a pin
    /// row was removed. The bytes need not be present (a dangling pin
    /// is dropped all the same).
    pub fn unpin(&self, hash: &BlobHash, owner: &str) -> Result<bool> {
        // Index read first, outside the db lock, so the two mutexes are
        // never nested in this path (lock order elsewhere is one at a
        // time; keep it that way).
        let index_size: Option<u64> = self
            .index
            .lock()
            .unwrap()
            .query_row(
                "SELECT size_bytes FROM blob WHERE hash = ?1",
                params![blob::hex(hash)],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(db_err)?
            .map(|n| n as u64);
        let mut db = self.db.lock().unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let removed = tx
            .execute(
                "DELETE FROM pins WHERE hash = ?1 AND owner = ?2",
                params![blob::hex(hash), owner],
            )
            .map_err(db_err)?
            == 1;
        if removed {
            let delta = index_size.unwrap_or_else(|| {
                fs::metadata(blob::blob_path(&self.blobs_root(), hash))
                    .map(|m| m.len())
                    .unwrap_or(0)
            });
            shrink_owner_used(&tx, owner, delta)?;
        }
        tx.commit().map_err(db_err)?;
        drop(db);
        if removed {
            self.bump_generation();
        }
        Ok(removed)
    }

    fn pin_owners(&self, hash: &BlobHash) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT owner FROM pins WHERE hash = ?1 ORDER BY owner")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![blob::hex(hash)], |r| r.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    // ---- Inventory ----

    /// List blobs blobd knows (attrs ∪ pins — an orphan file with
    /// neither is invisible to `blob.list`, which is what the restart
    /// gate arm asserts). Cursor = last hash; `owner` filters to that
    /// owner's pins.
    pub fn list(
        &self,
        owner: Option<&str>,
        limit: usize,
        cursor: Option<&BlobHash>,
    ) -> Result<Vec<ListEntry>> {
        let limit = limit.clamp(1, 1000);
        let hashes: Vec<String> = {
            let db = self.db.lock().unwrap();
            let mut out = Vec::new();
            if let Some(owner) = owner {
                let mut stmt = db
                    .prepare(
                        "SELECT DISTINCT hash FROM pins \
                         WHERE owner = ?1 AND (?2 IS NULL OR hash > ?2) \
                         ORDER BY hash LIMIT ?3",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![owner, cursor.map(blob::hex), limit as i64], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(db_err)?;
                for row in rows {
                    out.push(row.map_err(db_err)?);
                }
            } else {
                let mut stmt = db
                    .prepare(
                        "SELECT hash FROM (SELECT hash FROM blob_attrs UNION SELECT hash FROM pins) \
                         WHERE (?1 IS NULL OR hash > ?1) ORDER BY hash LIMIT ?2",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![cursor.map(blob::hex), limit as i64], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(db_err)?;
                for row in rows {
                    out.push(row.map_err(db_err)?);
                }
            }
            out
        };

        let mut entries = Vec::with_capacity(hashes.len());
        for hex in hashes {
            let Some(hash) = blob::from_hex(&hex) else {
                return Err(StoreError::Db(format!(
                    "invalid hex in blobd.sqlite: {hex:?}"
                )));
            };
            let (mime, name, origin, first_put) = self
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT mime, name_hint, origin, first_put FROM blob_attrs WHERE hash = ?1",
                    params![hex],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(db_err)?
                .map(|(m, n, o, f)| (Some(m), n, Some(o), Some(f)))
                .unwrap_or((None, None, None, None));
            let size = blob::size(&self.blobs_root(), &hash)
                .ok()
                .or_else(|| {
                    self.index
                        .lock()
                        .unwrap()
                        .query_row(
                            "SELECT size_bytes FROM blob WHERE hash = ?1",
                            params![blob::hex(&hash)],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                        .map(|n| n as u64)
                })
                .unwrap_or(0);
            entries.push(ListEntry {
                hash,
                size,
                mime,
                name,
                origin,
                first_put,
                pins: self.pin_owners(&hash)?,
            });
        }
        Ok(entries)
    }

    /// `(blobs known to blobd, pin rows)`.
    pub fn counts(&self) -> Result<(u64, u64)> {
        let db = self.db.lock().unwrap();
        let blobs: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM (SELECT hash FROM blob_attrs UNION SELECT hash FROM pins)",
                params![],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        let pins: i64 = db
            .query_row("SELECT COUNT(*) FROM pins", params![], |r| r.get(0))
            .map_err(db_err)?;
        Ok((blobs as u64, pins as u64))
    }

    // ---- Quota ----

    pub fn quota_report(&self, owner: Option<&str>) -> Result<QuotaReport> {
        let db = self.db.lock().unwrap();
        let mut owners = BTreeMap::new();
        match owner {
            Some(one) => {
                let used: i64 = db
                    .query_row(
                        "SELECT used_bytes FROM quota WHERE owner = ?1",
                        params![one],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db_err)?
                    .unwrap_or(0);
                owners.insert(
                    one.to_string(),
                    OwnerQuota {
                        used: used as u64,
                        limit: self.options.owner_limit(one),
                    },
                );
            }
            None => {
                let mut stmt = db
                    .prepare("SELECT owner, used_bytes FROM quota ORDER BY owner")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                    })
                    .map_err(db_err)?;
                for row in rows {
                    let (o, used) = row.map_err(db_err)?;
                    owners.insert(
                        o.clone(),
                        OwnerQuota {
                            used: used as u64,
                            limit: self.options.owner_limit(&o),
                        },
                    );
                }
                for (o, limit) in &self.options.owner_limits {
                    owners.entry(o.clone()).or_insert(OwnerQuota {
                        used: 0,
                        limit: *limit,
                    });
                }
            }
        }
        let total_used: i64 = db
            .query_row(
                "SELECT COALESCE(SUM(used_bytes), 0) FROM quota",
                params![],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(QuotaReport {
            owners,
            total: OwnerQuota {
                used: total_used as u64,
                limit: self.options.quota_total_bytes,
            },
        })
    }

    fn owner_used(&self, owner: &str) -> Result<u64> {
        let used: Option<i64> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT used_bytes FROM quota WHERE owner = ?1",
                params![owner],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(used.map(|n| n as u64).unwrap_or(0))
    }

    fn total_used(&self) -> Result<u64> {
        let used: i64 = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COALESCE(SUM(used_bytes), 0) FROM quota",
                params![],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(used as u64)
    }

    // ---- GC ----

    /// Sweep CAS files whose mds refcount is 0 (no row counts as 0 —
    /// every blobd put is rowless), that carry no pin, and whose mtime
    /// is older than the grace window. Dry run lists candidates; a
    /// live run unlinks, drops attrs and (defensively) pins with their
    /// quota accounting, and reports the freed bytes.
    pub fn gc(&self, dry_run: bool) -> Result<GcSweep> {
        let grace = Duration::from_secs(DEFAULT_GC_GRACE_SECS);
        let mut report = GcSweep::default();
        for file in self.cas_scan()? {
            let refcount = blob_index::blob_row(&self.index.lock().unwrap(), &file.hash)?
                .map(|row| row.refcount)
                .unwrap_or(0);
            if refcount > 0 {
                report.skipped_referenced += 1;
                continue;
            }
            let pins = self.pin_owners(&file.hash)?;
            if !pins.is_empty() {
                report.skipped_pinned += 1;
                continue;
            }
            if file.age().unwrap_or(Duration::ZERO) <= grace {
                report.skipped_young += 1;
                continue;
            }
            if dry_run {
                report.swept.push(file.hash_hex());
                report.bytes_freed = report.bytes_freed.saturating_add(file.size);
                continue;
            }
            match fs::remove_file(&file.path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::Io(e)),
            }
            let mut db = self.db.lock().unwrap();
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_err)?;
            // Pinned files never reach here; deleting (and
            // un-accounting) any stray rows anyway keeps quota from
            // drifting if one races in.
            let stray: Vec<String> = {
                let mut stmt = tx
                    .prepare("SELECT owner FROM pins WHERE hash = ?1")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![file.hash_hex()], |r| r.get::<_, String>(0))
                    .map_err(db_err)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(db_err)?);
                }
                out
            };
            for owner in stray {
                shrink_owner_used(&tx, &owner, file.size)?;
            }
            tx.execute("DELETE FROM pins WHERE hash = ?1", params![file.hash_hex()])
                .map_err(db_err)?;
            tx.execute(
                "DELETE FROM blob_attrs WHERE hash = ?1",
                params![file.hash_hex()],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            drop(db);
            report.swept.push(file.hash_hex());
            report.bytes_freed = report.bytes_freed.saturating_add(file.size);
        }
        if !dry_run && !report.swept.is_empty() {
            self.bump_generation();
        }
        Ok(report)
    }

    // ---- Internals ----

    /// Walk the sharded CAS (`<h2>/<h2>/<hash64>`), skipping `.tmp`.
    fn cas_scan(&self) -> Result<Vec<CasFile>> {
        let root = self.blobs_root();
        let mut out = Vec::new();
        let top = match fs::read_dir(&root) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for shard in top {
            let shard = shard?;
            let shard_name = shard.file_name();
            if shard_name == ".tmp" || !shard.file_type()?.is_dir() {
                continue;
            }
            for mid in fs::read_dir(shard.path())? {
                let mid = mid?;
                if !mid.file_type()?.is_dir() {
                    continue;
                }
                for file in fs::read_dir(mid.path())? {
                    let file = file?;
                    if !file.file_type()?.is_file() {
                        continue;
                    }
                    let name = file.file_name();
                    let Some(hash) = blob::from_hex(&name.to_string_lossy()) else {
                        tracing::warn!(
                            target: "cosmix_blobd",
                            "cas_scan: non-hash file {} ignored",
                            file.path().display()
                        );
                        continue;
                    };
                    let md = fs::metadata(file.path())?;
                    out.push(CasFile {
                        hash,
                        path: file.path(),
                        size: md.len(),
                        modified: md.modified().ok(),
                    });
                }
            }
        }
        out.sort_by_key(|a| blob::hex(&a.hash));
        Ok(out)
    }
}

struct CasFile {
    hash: BlobHash,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl CasFile {
    fn hash_hex(&self) -> String {
        blob::hex(&self.hash)
    }

    fn age(&self) -> Option<Duration> {
        self.modified
            .and_then(|m| SystemTime::now().duration_since(m).ok())
    }
}

fn os_str_lossy(name: Option<&str>, path: &Path) -> String {
    match name {
        Some(n) => n.to_string(),
        None => path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Db(e.to_string())
}

fn bump_owner_used(tx: &rusqlite::Transaction<'_>, owner: &str, delta: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO quota (owner, used_bytes) VALUES (?1, ?2) \
         ON CONFLICT(owner) DO UPDATE SET used_bytes = used_bytes + excluded.used_bytes",
        params![owner, delta as i64],
    )
    .map_err(db_err)?;
    Ok(())
}

fn shrink_owner_used(tx: &rusqlite::Transaction<'_>, owner: &str, delta: u64) -> Result<()> {
    tx.execute(
        "UPDATE quota SET used_bytes = MAX(0, used_bytes - ?2) WHERE owner = ?1",
        params![owner, delta as i64],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Open `<root>/blobd.sqlite`, apply migrations in the mds style
/// (WAL, busy_timeout 5 s, `PRAGMA user_version` steps, refuse a db
/// newer than the code).
fn open_blobd_db(root: &Path) -> Result<Connection> {
    let mut conn =
        Connection::open(root.join("blobd.sqlite")).map_err(|e| StoreError::Db(e.to_string()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\
         PRAGMA synchronous  = NORMAL;\
         PRAGMA busy_timeout = 5000;\
         PRAGMA foreign_keys = ON;",
    )
    .map_err(db_err)?;
    apply_blobd_migrations(&mut conn)?;
    Ok(conn)
}

fn apply_blobd_migrations(conn: &mut Connection) -> Result<()> {
    let app_id: i32 = conn
        .query_row("PRAGMA application_id;", [], |r| r.get(0))
        .map_err(db_err)?;
    let version: u32 = conn
        .query_row("PRAGMA user_version;", [], |r| r.get(0))
        .map_err(db_err)?;
    if app_id != 0 && app_id != BLOBD_APPLICATION_ID {
        return Err(StoreError::Db(format!(
            "blobd.sqlite: wrong application_id 0x{app_id:08x} (expected 0x{BLOBD_APPLICATION_ID:08x})"
        )));
    }
    if version > BLOBD_LATEST {
        return Err(StoreError::Db(format!(
            "blobd.sqlite: db at v{version}, code only knows up to v{BLOBD_LATEST}"
        )));
    }
    for v in (version + 1)..=BLOBD_LATEST {
        let sql = match v {
            1 => BLOBD_V1_SQL,
            _ => {
                return Err(StoreError::Db(format!(
                    "blobd.sqlite: missing migration v{v}"
                )));
            }
        };
        let tx = conn
            .transaction()
            .map_err(|e| StoreError::Db(format!("begin v{v}: {e}")))?;
        tx.execute_batch(sql)
            .map_err(|e| StoreError::Db(format!("apply v{v}: {e}")))?;
        tx.commit()
            .map_err(|e| StoreError::Db(format!("commit v{v}: {e}")))?;
        conn.pragma_update(None, "user_version", v)
            .map_err(db_err)?;
        conn.pragma_update(None, "application_id", BLOBD_APPLICATION_ID)
            .map_err(db_err)?;
    }
    Ok(())
}

/// Read connection to mds's `blobs.sqlite` for the public row API.
/// mds's own open (a moment earlier in [`Store::open`]) applied its
/// schema; blobd only ever reads here (D4).
fn open_index_conn(root: &Path) -> Result<Connection> {
    let conn =
        Connection::open(root.join("blobs.sqlite")).map_err(|e| StoreError::Db(e.to_string()))?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(db_err)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    use super::super::config::{DEFAULT_QUOTA_OWNER_BYTES, DEFAULT_QUOTA_TOTAL_BYTES};

    fn options() -> StoreOptions {
        StoreOptions {
            origin: "testnode".into(),
            quota_total_bytes: DEFAULT_QUOTA_TOTAL_BYTES,
            quota_owner_default_bytes: DEFAULT_QUOTA_OWNER_BYTES,
            owner_limits: BTreeMap::new(),
        }
    }

    fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path(), options()).unwrap();
        (dir, s)
    }

    fn write_src(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// Set a file's mtime 2 minutes into the past (beyond the grace
    /// window) using only std.
    fn age_file(path: &Path) {
        let f = File::open(path).unwrap();
        let past = SystemTime::now() - Duration::from_secs(2 * DEFAULT_GC_GRACE_SECS);
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(past)
                .set_modified(past),
        )
        .unwrap();
    }

    #[test]
    fn put_stats_pins_and_lists() {
        let (dir, store) = store();
        let src = write_src(&dir, "hello.txt", b"hello blobd");
        let out = store
            .put(
                &src,
                &PutOptions {
                    owner: "maild",
                    mime: None,
                    name: Some("hello.txt"),
                    mode: PutMode::Copy,
                    immutable: false,
                },
            )
            .unwrap();
        assert_eq!(out.reference.size, 11);
        assert_eq!(out.reference.mime, "text/plain");
        assert_eq!(out.reference.origin, "testnode");
        assert!(out.newly_pinned);

        let stat = store.stat(&out.reference.hash).unwrap();
        assert!(stat.present);
        assert_eq!(stat.size, Some(11));
        assert_eq!(stat.mime.as_deref(), Some("text/plain"));
        assert_eq!(stat.pins, vec!["maild".to_string()]);
        assert_eq!(stat.origin.as_deref(), Some("testnode"));
        assert!(stat.first_put.is_some());

        let listed = store.list(None, 10, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].hash, out.reference.hash);
        assert_eq!(listed[0].pins, vec!["maild".to_string()]);
        let owned = store.list(Some("maild"), 10, None).unwrap();
        assert_eq!(owned.len(), 1);
        assert!(store.list(Some("nobody"), 10, None).unwrap().is_empty());
    }

    #[test]
    fn put_is_idempotent_same_reference_no_extra_pin() {
        let (dir, store) = store();
        let src = write_src(&dir, "same.bin", b"same bytes");
        let opts = PutOptions::new("capture");
        let a = store.put(&src, &opts).unwrap();
        let b = store.put(&src, &opts).unwrap();
        assert_eq!(a.reference, b.reference);
        assert!(a.newly_pinned);
        assert!(!b.newly_pinned);
        assert_eq!(store.counts().unwrap(), (1, 1));
        // A second owner's pin is a new row.
        assert!(store.pin(&a.reference.hash, "filesd").unwrap());
        assert_eq!(store.stat(&a.reference.hash).unwrap().pins.len(), 2);
    }

    #[test]
    fn hardlink_requires_immutable_promise() {
        let (dir, store) = store();
        let src = write_src(&dir, "mut.bin", b"mutable");
        let err = store
            .put(
                &src,
                &PutOptions {
                    mode: PutMode::HardLink,
                    immutable: false,
                    ..PutOptions::new("t")
                },
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::BadRequest(_)), "got {err:?}");
        let ok = store
            .put(
                &src,
                &PutOptions {
                    mode: PutMode::HardLink,
                    immutable: true,
                    ..PutOptions::new("t")
                },
            )
            .unwrap();
        // The CAS file aliases the source inode.
        assert_eq!(
            std::fs::metadata(store.path(&ok.reference.hash).unwrap())
                .unwrap()
                .ino(),
            std::fs::metadata(&src).unwrap().ino()
        );
    }

    #[test]
    fn quota_refuses_at_limit_accounts_and_releases() {
        let dir = TempDir::new().unwrap();
        let opts = StoreOptions {
            origin: "testnode".into(),
            quota_total_bytes: 100,
            quota_owner_default_bytes: 100,
            owner_limits: BTreeMap::from([("small".into(), 10u64), ("other".into(), 1000u64)]),
        };
        let store = Store::open(dir.path(), opts).unwrap();
        let big = write_src(&dir, "big.bin", &[7u8; 80]);
        let small = write_src(&dir, "small.bin", &[7u8; 16]);

        // Owner "small" is capped at 10: refused before any copy.
        let err = store.put(&small, &PutOptions::new("small")).unwrap_err();
        assert!(
            matches!(err, StoreError::QuotaOwner { ref owner, would_use, limit } if owner == "small" && would_use == 16 && limit == 10),
            "got {err:?}"
        );
        assert!(store.list(Some("small"), 10, None).unwrap().is_empty());

        // Success accounts for the owner and the total.
        let out = store.put(&big, &PutOptions::new("other")).unwrap();
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["other"].used, 80);
        assert_eq!(report.total.used, 80);

        // Total cap 100: a second 80-byte blob is refused.
        let err = store
            .put(
                &write_src(&dir, "big2.bin", &[9u8; 80]),
                &PutOptions::new("other"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::QuotaTotal {
                would_use: 160,
                limit: 100
            }
        ));

        // Unpin releases; gc then reclaims the bytes; quota is clean.
        assert!(store.unpin(&out.reference.hash, "other").unwrap());
        age_file(&store.path(&out.reference.hash).unwrap());
        let sweep = store.gc(false).unwrap();
        assert_eq!(sweep.swept.len(), 1);
        assert_eq!(sweep.bytes_freed, 80);
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.total.used, 0);
        assert!(store.path(&out.reference.hash).is_err());
    }

    #[test]
    fn pins_and_unpins_are_idempotent() {
        let (dir, store) = store();
        let src = write_src(&dir, "p.bin", b"pin me");
        let out = store.put(&src, &PutOptions::new("a")).unwrap();
        assert!(!store.pin(&out.reference.hash, "a").unwrap());
        assert!(store.pin(&out.reference.hash, "b").unwrap());
        assert!(!store.pin(&out.reference.hash, "b").unwrap());
        assert!(store.unpin(&out.reference.hash, "b").unwrap());
        assert!(!store.unpin(&out.reference.hash, "b").unwrap());
        assert_eq!(
            store.stat(&out.reference.hash).unwrap().pins,
            vec!["a".to_string()]
        );
        // Pinning bytes we do not hold is refused.
        let missing = cosmix_mds::blob::hash_bytes(b"not here");
        assert!(matches!(
            store.pin(&missing, "a"),
            Err(StoreError::NotPresent(_))
        ));
        // Unpinning a missing blob is still a clean no-op.
        assert!(!store.unpin(&missing, "a").unwrap());
    }

    #[test]
    fn gc_dry_run_lists_live_run_sweeps_but_pins_survive() {
        let (dir, store) = store();
        let pinned = store
            .put(
                &write_src(&dir, "keep.bin", b"keep me pinned"),
                &PutOptions::new("maild"),
            )
            .unwrap();
        let loose = store
            .put(
                &write_src(&dir, "loose.bin", b"loose unpinned"),
                &PutOptions::new("tmp"),
            )
            .unwrap();
        store.unpin(&loose.reference.hash, "tmp").unwrap();
        // Both mtimes are young: nothing is collectable yet.
        let young = store.gc(true).unwrap();
        assert!(young.swept.is_empty());
        // The pinned blob skips at the pin check; only the loose one
        // reaches (and fails) the age check.
        assert_eq!(young.skipped_young, 1);
        assert_eq!(young.skipped_pinned, 1);

        // Age both past the grace window.
        age_file(&store.path(&pinned.reference.hash).unwrap());
        age_file(&store.path(&loose.reference.hash).unwrap());

        let dry = store.gc(true).unwrap();
        assert_eq!(dry.swept, vec![blob::hex(&loose.reference.hash)]);
        assert_eq!(dry.bytes_freed, 14);
        assert_eq!(dry.skipped_pinned, 1);
        // Dry run left the file in place.
        assert!(store.path(&loose.reference.hash).is_ok());

        let live = store.gc(false).unwrap();
        assert_eq!(live.swept.len(), 1);
        assert!(matches!(
            store.path(&loose.reference.hash),
            Err(StoreError::NotPresent(_))
        ));
        assert!(store.path(&pinned.reference.hash).is_ok());
        // Attrs dropped, pinned blob still listed, orphan invisible.
        let listed = store.list(None, 10, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].hash, pinned.reference.hash);
    }

    #[test]
    fn gc_sweeps_zero_refcount_row_from_mds_too() {
        // A blob referenced by an mds row with refcount 0 (mds's own
        // collectable state) is a blobd candidate as well.
        let (dir, store) = store();
        // Land bytes rowless through the raw blob API, then give it an
        // mds row with refcount 0 via a direct blobs.sqlite insert.
        let bytes = b"row with refcount zero";
        let hash = cosmix_mds::blob::put(&store.blobs_root(), bytes).unwrap();
        {
            let conn = Connection::open(dir.path().join("blobs.sqlite")).unwrap();
            conn.execute(
                "INSERT INTO blob (hash, size_bytes, first_seen, last_seen, refcount) \
                 VALUES (?1, ?2, 0, 0, 0)",
                params![blob::hex(&hash), bytes.len() as i64],
            )
            .unwrap();
        }
        age_file(&store.path(&hash).unwrap());
        let dry = store.gc(true).unwrap();
        assert_eq!(dry.swept, vec![blob::hex(&hash)]);
    }

    #[test]
    fn second_open_of_same_root_fails_on_flock() {
        let (dir, store) = store();
        // The live store holds the flock; a second open must refuse.
        let err = match Store::open(dir.path(), options()) {
            Err(err) => err,
            Ok(_) => panic!("second open of a locked root must fail"),
        };
        assert!(matches!(err, StoreError::Locked(_)), "got {err:?}");
        // Dropping the holder releases it (fd closed).
        drop(store);
        assert!(Store::open(dir.path(), options()).is_ok());
    }

    #[test]
    fn startup_removes_tmp_and_reports_orphans_without_deleting() {
        let dir = TempDir::new().unwrap();
        {
            // Pre-create the root via mds, drop staging junk plus an
            // aged orphan CAS file, then open the store.
            let mds = SqliteCasMds::open(dir.path()).unwrap();
            let tmp = mds.blobs_root().join(".tmp");
            std::fs::write(tmp.join("junk1"), b"x").unwrap();
            std::fs::write(tmp.join("junk2"), b"y").unwrap();
            let orphan_hash = cosmix_mds::blob::put(&mds.blobs_root(), b"orphaned bytes").unwrap();
            let orphan_path = cosmix_mds::blob::blob_path(&mds.blobs_root(), &orphan_hash);
            age_file(&orphan_path);
        }
        let store = Store::open(dir.path(), options()).unwrap();
        let report = store.startup_report();
        assert_eq!(report.tmp_removed, 2);
        assert!(
            store
                .blobs_root()
                .join(".tmp")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(report.orphans.len(), 1);
        // Not deleted at startup — blob.gc owns deletion.
        assert!(
            report
                .orphans
                .first()
                .is_some_and(|h| blob::from_hex(h).is_some())
        );
        let hash = blob::from_hex(&report.orphans[0]).unwrap();
        assert!(store.path(&hash).is_ok());
    }

    #[test]
    fn young_orphan_is_not_reported_at_startup() {
        let dir = TempDir::new().unwrap();
        {
            let mds = SqliteCasMds::open(dir.path()).unwrap();
            cosmix_mds::blob::put(&mds.blobs_root(), b"fresh bytes").unwrap();
        }
        let store = Store::open(dir.path(), options()).unwrap();
        assert!(store.startup_report().orphans.is_empty());
    }

    #[test]
    fn has_splits_present_and_missing_in_order() {
        let (dir, store) = store();
        let out = store
            .put(&write_src(&dir, "h.bin", b"here"), &PutOptions::new("t"))
            .unwrap();
        let missing = cosmix_mds::blob::hash_bytes(b"absent");
        let (present, missing_out) = store
            .has(&[out.reference.hash, missing, out.reference.hash])
            .unwrap();
        assert_eq!(present, vec![out.reference.hash, out.reference.hash]);
        assert_eq!(missing_out, vec![missing]);
    }

    #[test]
    fn blobd_sqlite_schema_is_v1_with_expected_tables() {
        let (dir, store) = store();
        let conn = Connection::open(dir.path().join("blobd.sqlite")).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        for tbl in ["blob_attrs", "pins", "quota"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1;",
                    params![tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {tbl}");
        }
        // mds's blobs.sqlite is untouched: still BLOBS_LATEST = 1.
        let blobs = Connection::open(dir.path().join("blobs.sqlite")).unwrap();
        let bv: u32 = blobs
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bv, 1);
        drop(store);
    }

    #[test]
    fn open_refuses_wrong_magic_db() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(dir.path().join("blobd.sqlite")).unwrap();
            conn.pragma_update(None, "application_id", 0x1234_5678)
                .unwrap();
        }
        let err = match Store::open(dir.path(), options()) {
            Err(err) => err,
            Ok(_) => panic!("a wrong-magic blobd.sqlite must refuse to open"),
        };
        assert!(
            matches!(err, StoreError::Db(ref msg) if msg.contains("wrong application_id")),
            "got {err:?}"
        );
    }
}
