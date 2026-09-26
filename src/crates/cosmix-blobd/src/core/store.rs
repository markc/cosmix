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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
const BLOBD_LATEST: u32 = 2;
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
/// v2 (F6): the pin row records the size it accounted, so an unpin
/// releases exactly what the pin paid even when the CAS file and the
/// mds row are both gone. Pre-v2 rows carry the default 0 and fall
/// back to the legacy index/file read at unpin time.
const BLOBD_V2_SQL: &str = "ALTER TABLE pins ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0;";

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
    /// The bytes were present when the caller looked, but vanished
    /// before the pin landed — a `blob.gc` race (M1). The pin is
    /// refused, never dangled; retry the put.
    Vanished(String),
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
    /// A bounded resource is taken; the verb replies rc 10 `busy:
    /// <why>` — the `blob.fetch` queue is full (`fetch_queue_max`), or
    /// another `blob.gc` is already sweeping (one GC owner, M2b).
    Busy(&'static str),
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
            Self::Vanished(s) => write!(
                f,
                "vanished: {s} — the bytes disappeared while the pin was landing (blob.gc race); retry the put"
            ),
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
            Self::Busy(why) => write!(f, "busy: {why}; retry later"),
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
    /// Shared-read group for the state root and CAS root (M4):
    /// chgrped and setgid at open, so `blob.path` targets are
    /// traversable by the group's members. Default `cosmix-blob`
    /// (SPEC 10a §3.3).
    pub cas_group: String,
}

impl StoreOptions {
    pub fn from_config(cfg: &Config, origin: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            quota_total_bytes: cfg.quota_total_bytes,
            quota_owner_default_bytes: cfg.quota_owner_default_bytes,
            owner_limits: cfg.owner_limits.clone(),
            cas_group: cfg.cas_group.clone(),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaReport {
    pub owners: BTreeMap<String, OwnerQuota>,
    pub total: OwnerQuota,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OwnerQuota {
    pub used: u64,
    pub limit: u64,
    /// Bytes admitted to in-flight uploads but not yet pinned (M3's
    /// reservation table) — visible beside `used` so an operator can
    /// see why a fresh upload is refused while `used` looks low.
    pub reserved: u64,
}

/// One in-flight upload's quota hold (M3): the bytes admitted when the
/// upload started, released exactly once when this guard drops —
/// abort, error, panic or the pin's accounting, all run `Drop` — so a
/// reservation can never leak. The uploading path keeps the guard
/// alive until `record_upload`/`record_fetch` has settled the real
/// size.
#[derive(Debug)]
pub struct Reservation {
    reserved: Arc<Mutex<BTreeMap<String, u64>>>,
    owner: String,
    amount: u64,
}

impl Reservation {
    /// The mid-stream byte bound this reservation grants: the declared
    /// `Content-Length`, or the owner's whole remaining room when the
    /// length was unknown.
    pub fn cap(&self) -> u64 {
        self.amount
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut reserved = self.reserved.lock().unwrap();
        let left = reserved
            .get(&self.owner)
            .copied()
            .unwrap_or(0)
            .saturating_sub(self.amount);
        if left == 0 {
            reserved.remove(&self.owner);
        } else {
            reserved.insert(self.owner.clone(), left);
        }
    }
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

/// [`Store::record_fetch`]'s per-owner outcome (R1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchPins {
    /// Owners whose pin row this call added.
    pub pinned: Vec<String>,
    /// Owners that already held a pin (idempotent, nothing written).
    pub held: Vec<String>,
    /// Owners the owner or total cap refused; nothing written for them.
    pub refused: Vec<String>,
}

impl FetchPins {
    /// No owner holds the bytes: every one was refused.
    pub fn none_fit(&self) -> bool {
        self.pinned.is_empty() && self.held.is_empty() && !self.refused.is_empty()
    }
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
    /// In-flight upload reservations (M3): owner → bytes admitted but
    /// not yet pinned. Lock order is always reserved → db — a path
    /// holding the db mutex never takes this one.
    reserved: Arc<Mutex<BTreeMap<String, u64>>>,
    /// Holds the root `flock` for the store's lifetime. Never read:
    /// closing it on drop is what releases the lock.
    _lock_file: File,
    options: StoreOptions,
    startup: StartupReport,
    generation: AtomicU64,
    /// TEST ONLY (F2): when true, `record_fetch` fails — the fetch
    /// completion must publish outcome io, never a pinless ok.
    #[cfg(test)]
    pub(crate) fail_record_fetch: AtomicBool,
    /// Held for a sweep's whole duration (M2b); `try_lock` is the
    /// one-GC-owner admission.
    gc_running: Mutex<()>,
    /// TEST ONLY (M2b): a pause at the top of a sweep, so a second
    /// `blob.gc` can race a running one deterministically.
    #[cfg(test)]
    pub(crate) gc_hold: Mutex<Option<Duration>>,
}

impl Store {
    /// Open (or create) the store at `root`: exclusive `flock`, mds
    /// root, `blobd.sqlite` migrations, then startup housekeeping
    /// (`.tmp` sweep + orphan reconcile).
    pub fn open(root: impl Into<PathBuf>, options: StoreOptions) -> Result<Self> {
        let root = root.into();

        // One GC owner per root. flock locks are per open file
        // description, so a second open — even in-process — conflicts.
        // Taken before anything else touches the root (m2): mds's open
        // creates directories and may migrate `blobs.sqlite`, and a
        // second instance must exit 2 without having done either.
        fs::create_dir_all(&root)?;
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

        let mds = SqliteCasMds::open(&root)?;
        let db = open_blobd_db(&root)?;
        let index = open_index_conn(&root)?;

        // M4: the CAS must be traversable by the shared-read group —
        // the unit's 0750 StateDirectory + 0027 UMask alone leaves the
        // tree owned cosmix-blobd:cosmix-blobd and no member can walk
        // it. Chgrp the state root and the CAS root, setgid them: every
        // shard directory mds creates below inherits group and bit, so
        // CAS files inherit the group.
        apply_cas_group(&root, &mds.blobs_root(), &options.cas_group);

        let mut store = Self {
            root,
            mds,
            db: Mutex::new(db),
            index: Mutex::new(index),
            reserved: Arc::new(Mutex::new(BTreeMap::new())),
            _lock_file: lock_file,
            options,
            startup: StartupReport::default(),
            generation: AtomicU64::new(0),
            #[cfg(test)]
            fail_record_fetch: AtomicBool::new(false),
            gc_running: Mutex::new(()),
            #[cfg(test)]
            gc_hold: Mutex::new(None),
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

    /// Atomically check the caps and reserve room for one upload
    /// (M3). `declared` is the `Content-Length` when the client sent
    /// one; when absent the owner's whole remaining room is reserved
    /// (the mid-stream counter then aborts at that bound). The check
    /// counts used bytes and every in-flight reservation under the
    /// reserved→db lock order, so N concurrent uploads for one owner
    /// can no longer each spend the full headroom — the old
    /// check-then-act read the cap once and never reserved.
    pub fn reserve_upload(&self, owner: &str, declared: Option<u64>) -> Result<Reservation> {
        let mut reserved = self.reserved.lock().unwrap();
        let owner_reserved = reserved.get(owner).copied().unwrap_or(0);
        let total_reserved: u64 = reserved.values().copied().sum();
        let owner_used = self.owner_used(owner)?;
        let total_used = self.total_used()?;
        // Owner cap first — the order Store::put has always checked.
        if let Some(amount) = declared {
            let limit = self.options.owner_limit(owner);
            let would_use = owner_used
                .saturating_add(owner_reserved)
                .saturating_add(amount);
            if would_use > limit {
                return Err(StoreError::QuotaOwner {
                    owner: owner.to_string(),
                    would_use,
                    limit,
                });
            }
            let total_would = total_used
                .saturating_add(total_reserved)
                .saturating_add(amount);
            if total_would > self.options.quota_total_bytes {
                return Err(StoreError::QuotaTotal {
                    would_use: total_would,
                    limit: self.options.quota_total_bytes,
                });
            }
        }
        let amount = declared.unwrap_or_else(|| {
            let owner_room = self
                .options
                .owner_limit(owner)
                .saturating_sub(owner_used.saturating_add(owner_reserved));
            let total_room = self
                .options
                .quota_total_bytes
                .saturating_sub(total_used.saturating_add(total_reserved));
            owner_room.min(total_room)
        });
        *reserved.entry(owner.to_string()).or_insert(0) += amount;
        Ok(Reservation {
            reserved: Arc::clone(&self.reserved),
            owner: owner.to_string(),
            amount,
        })
    }

    /// Daemon-local ingest via `mds::put_blob_path`. The quota check
    /// reserves at admission (M3) — concurrent puts cannot overshoot a
    /// cap — and accounting settles to the real size when the pin
    /// lands (the reservation releases as this frame unwinds).
    /// Idempotent: a re-put returns the same reference and pins
    /// nothing new.
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

        // Quota reserved for the copy's duration; the mid-copy race
        // that read the cap once is closed.
        let size = md.len();
        let _reservation = self.reserve_upload(opts.owner, Some(size))?;

        let hash = self.mds.put_blob_path(src, opts.mode)?;
        let size = blob::size(&self.blobs_root(), &hash)?;

        let mime = opts
            .mime
            .map(str::to_string)
            .unwrap_or_else(|| mime::sniff(&os_str_lossy(opts.name, src)).to_string());
        self.record_upload(&hash, size, &mime, opts.name, opts.owner)
    }

    /// Post-stream ingest bookkeeping, shared by `blob.put` and the
    /// byte lane: attrs (mime, name, `origin` = this node), the owner
    /// pin and its quota accounting. The bytes must already be
    /// committed in the CAS; the existence re-check under the db lock
    /// refuses `Vanished` if a concurrent `blob.gc` swept them (M1) —
    /// an acknowledged pin never dangles. Idempotent per owner: a
    /// re-put pins nothing new. The returned reference describes the
    /// attrs as they stand after the call — an already-recorded blob
    /// keeps its original mime/name, so the reference always matches
    /// `blob.stat`.
    pub fn record_upload(
        &self,
        hash: &BlobHash,
        size: u64,
        mime: &str,
        name: Option<&str>,
        owner: &str,
    ) -> Result<PutOutcome> {
        let mut db = self.db.lock().unwrap();
        if !blob::blob_path(&self.blobs_root(), hash).exists() {
            return Err(StoreError::Vanished(blob::hex(hash)));
        }
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO blob_attrs (hash, mime, name_hint, origin, first_put) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![blob::hex(hash), mime, name, self.options.origin, now_ms()],
        )
        .map_err(db_err)?;
        // Read back what the table now holds: INSERT OR IGNORE keeps a
        // pre-existing row, and the reference must not claim a mime or
        // name the store is not serving.
        let (mime, name): (String, Option<String>) = tx
            .query_row(
                "SELECT mime, name_hint FROM blob_attrs WHERE hash = ?1",
                params![blob::hex(hash)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_err)?;
        let pinned = pin_with_cap(
            &tx,
            &blob::hex(hash),
            owner,
            size,
            self.options.owner_limit(owner),
            self.options.quota_total_bytes,
        )?;
        tx.commit().map_err(db_err)?;
        drop(db);
        if pinned {
            self.bump_generation();
        }

        Ok(PutOutcome {
            reference: Reference {
                hash: *hash,
                size,
                mime,
                name,
                origin: self.options.origin.clone(),
            },
            newly_pinned: pinned,
        })
    }

    /// Post-fetch ingest bookkeeping: attributes with
    /// `origin = source_node` (the node the bytes came from — a
    /// fetched blob's reference should point at its origin, not claim
    /// this node minted it) and one pin per joining owner. The bytes
    /// must already be committed in the CAS; the existence re-check
    /// under the db lock refuses `Vanished` if a concurrent `blob.gc`
    /// swept them (M1).
    ///
    /// Quota is per owner (R1): an owner whose cap (or the total cap)
    /// refuses the pin lands in [`FetchPins::refused`] with nothing
    /// written for it, and the owners that fit still pin and commit —
    /// one over-cap joiner never rolls back everyone else's pin. An
    /// owner that already pinned the blob is skipped, idempotently,
    /// and counts as held.
    pub fn record_fetch(
        &self,
        hash: &BlobHash,
        size: u64,
        mime: &str,
        source_node: &str,
        owners: &[String],
    ) -> Result<FetchPins> {
        #[cfg(test)]
        if self.fail_record_fetch.load(Ordering::Relaxed) {
            return Err(StoreError::Db(
                "injected record_fetch failure (test)".to_string(),
            ));
        }
        let mut db = self.db.lock().unwrap();
        if !blob::blob_path(&self.blobs_root(), hash).exists() {
            return Err(StoreError::Vanished(blob::hex(hash)));
        }
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO blob_attrs (hash, mime, name_hint, origin, first_put) \
             VALUES (?1, ?2, NULL, ?3, ?4)",
            params![blob::hex(hash), mime, source_node, now_ms()],
        )
        .map_err(db_err)?;
        let mut pins = FetchPins::default();
        for owner in owners {
            match pin_with_cap(
                &tx,
                &blob::hex(hash),
                owner,
                size,
                self.options.owner_limit(owner),
                self.options.quota_total_bytes,
            ) {
                Ok(true) => pins.pinned.push(owner.clone()),
                Ok(false) => pins.held.push(owner.clone()),
                // A refusal writes nothing; the transaction stays good
                // for the owners that fit.
                Err(StoreError::QuotaOwner { .. } | StoreError::QuotaTotal { .. }) => {
                    pins.refused.push(owner.clone())
                }
                Err(error) => return Err(error),
            }
        }
        tx.commit().map_err(db_err)?;
        drop(db);
        if !pins.pinned.is_empty() {
            self.bump_generation();
        }
        Ok(pins)
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
    /// was added. Requires the bytes to be present — re-checked under
    /// the db lock so a concurrent `blob.gc` sweep surfaces as
    /// `Vanished` instead of a dangling pin (M1). The pin pays the
    /// owner's cap at insert time (F6): a present-hash pin still
    /// costs its owner the bytes, and a refusal is
    /// `QuotaOwner`/`QuotaTotal` with nothing written.
    pub fn pin(&self, hash: &BlobHash, owner: &str) -> Result<bool> {
        if !blob::exists(&self.blobs_root(), hash)? {
            return Err(StoreError::NotPresent(blob::hex(hash)));
        }
        let size = blob::size(&self.blobs_root(), hash)?;
        let mut db = self.db.lock().unwrap();
        if !blob::blob_path(&self.blobs_root(), hash).exists() {
            return Err(StoreError::Vanished(blob::hex(hash)));
        }
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let inserted = pin_with_cap(
            &tx,
            &blob::hex(hash),
            owner,
            size,
            self.options.owner_limit(owner),
            self.options.quota_total_bytes,
        )?;
        tx.commit().map_err(db_err)?;
        drop(db);
        if inserted {
            self.bump_generation();
        }
        Ok(inserted)
    }

    /// Drop `owner`'s pin on `hash`. Idempotent; returns whether a pin
    /// row was removed. The bytes need not be present (a dangling pin
    /// is dropped all the same). Quota releases exactly what the pin
    /// row recorded (F6, schema v2); a pre-v2 row (size 0 — an empty
    /// blob's real size is 0 too, so the fallback is harmless) falls
    /// back to the mds index row, then the file on disk.
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
        let recorded: Option<i64> = tx
            .query_row(
                "SELECT size_bytes FROM pins WHERE hash = ?1 AND owner = ?2",
                params![blob::hex(hash), owner],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let removed = tx
            .execute(
                "DELETE FROM pins WHERE hash = ?1 AND owner = ?2",
                params![blob::hex(hash), owner],
            )
            .map_err(db_err)?
            == 1;
        if removed {
            let delta = match recorded {
                // A v2 row: release exactly what the pin paid.
                Some(bytes) if bytes > 0 => bytes as u64,
                // A pre-v2 row (or an empty blob): the legacy read.
                _ => index_size.unwrap_or_else(|| {
                    fs::metadata(blob::blob_path(&self.blobs_root(), hash))
                        .map(|m| m.len())
                        .unwrap_or(0)
                }),
            };
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
        // The reservation snapshot first, never nested with the db
        // lock (lock order is reserved → db everywhere else).
        let reserved_map: BTreeMap<String, u64> = self.reserved.lock().unwrap().clone();
        let total_reserved: u64 = reserved_map.values().copied().sum();
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
                        reserved: reserved_map.get(one).copied().unwrap_or(0),
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
                            reserved: reserved_map.get(&o).copied().unwrap_or(0),
                        },
                    );
                }
                for (o, limit) in &self.options.owner_limits {
                    owners.entry(o.clone()).or_insert(OwnerQuota {
                        used: 0,
                        limit: *limit,
                        reserved: reserved_map.get(o).copied().unwrap_or(0),
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
                reserved: total_reserved,
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
    ///
    /// The db mutex is held across each candidate's pin check, mtime
    /// re-stat and unlink (M1): `record_upload`/`record_fetch`/`pin`
    /// insert their pin row (and re-check existence) under the same
    /// mutex, so either the pin lands first and the candidate is
    /// skipped, or the sweep wins and the pin is refused `Vanished` —
    /// never an acknowledged pin whose bytes were just unlinked.
    ///
    /// One sweep at a time (M2b): a second concurrent call answers
    /// `Busy` at once instead of queueing a duplicate scan behind the
    /// first.
    pub fn gc(&self, dry_run: bool) -> Result<GcSweep> {
        let _sweeping = match self.gc_running.try_lock() {
            Ok(guard) => guard,
            // A sweep that panicked holds no state worth refusing on.
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(StoreError::Busy(
                    "a blob.gc is already running (one GC owner)",
                ));
            }
        };
        #[cfg(test)]
        if let Some(hold) = *self.gc_hold.lock().unwrap() {
            std::thread::sleep(hold);
        }
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
            let mut db = self.db.lock().unwrap();
            // Pin check under the lock (the stray-pin cleanup below
            // keeps quota from drifting if a row raced in anyway).
            let pinned: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pins WHERE hash = ?1)",
                    params![file.hash_hex()],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(db_err)?
                != 0;
            if pinned {
                report.skipped_pinned += 1;
                continue;
            }
            // Re-stat the mtime under the lock: the scan's metadata is
            // stale, and an idempotent re-put's touch (M1c) is exactly
            // the concurrent modification it can miss.
            let Ok(md) = fs::metadata(&file.path) else {
                continue; // already gone (another sweep, manual removal)
            };
            let age = md
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or(Duration::ZERO);
            if age <= grace {
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

/// Insert one pin row when the caps allow it (F6: every pin path pays
/// the size — a present-hash pin still costs its owner the bytes),
/// recording the size on the row so a later unpin releases exactly
/// what was paid. `Ok(false)` = the owner already pinned (idempotent
/// no-op); `Err(QuotaOwner|QuotaTotal)` = refused, nothing written.
fn pin_with_cap(
    tx: &rusqlite::Transaction<'_>,
    hash_hex: &str,
    owner: &str,
    size: u64,
    owner_limit: u64,
    total_limit: u64,
) -> Result<bool> {
    let already: i64 = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pins WHERE hash = ?1 AND owner = ?2)",
            params![hash_hex, owner],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if already != 0 {
        return Ok(false);
    }
    let owner_used: i64 = tx
        .query_row(
            "SELECT COALESCE((SELECT used_bytes FROM quota WHERE owner = ?1), 0)",
            params![owner],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    let would_use = owner_used as u64 + size;
    if would_use > owner_limit {
        return Err(StoreError::QuotaOwner {
            owner: owner.to_string(),
            would_use,
            limit: owner_limit,
        });
    }
    let total_used: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(used_bytes), 0) FROM quota",
            params![],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    let total_would = total_used as u64 + size;
    if total_would > total_limit {
        return Err(StoreError::QuotaTotal {
            would_use: total_would,
            limit: total_limit,
        });
    }
    tx.execute(
        "INSERT INTO pins (hash, owner, created, size_bytes) VALUES (?1, ?2, ?3, ?4)",
        params![hash_hex, owner, now_ms(), size as i64],
    )
    .map_err(db_err)?;
    bump_owner_used(tx, owner, size)?;
    Ok(true)
}

fn shrink_owner_used(tx: &rusqlite::Transaction<'_>, owner: &str, delta: u64) -> Result<()> {
    tx.execute(
        "UPDATE quota SET used_bytes = MAX(0, used_bytes - ?2) WHERE owner = ?1",
        params![owner, delta as i64],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Chgrp `dirs` to `group` and set the setgid bit (mode 2750), so
/// members of the group traverse the roots and every directory or file
/// created below the CAS root inherits the group (M4). Best-effort
/// with a log line per failure mode: an absent group (a dev host —
/// the tree stays daemon-owned), a refused chown (the daemon is not a
/// member; the unit's `SupplementaryGroups=cosmix-blob` is what grants
/// it), a refused chmod. Never fatal: a private CAS still serves
/// verbs, it just has no same-node zero-copy readers.
fn apply_cas_group(state_root: &Path, blobs_root: &Path, group: &str) {
    let Some(gid) = group_gid(group) else {
        tracing::warn!(
            target: "cosmix_blobd",
            "cas_group {group:?} does not exist on this host; the CAS stays owned by this \
             daemon (no shared blob.path reads — SPEC 10a §3.3 expects the group)"
        );
        return;
    };
    for dir in [state_root, blobs_root] {
        if !dir.is_dir() {
            continue;
        }
        let cpath = match std::ffi::CString::new(dir.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // SAFETY: chown(2) on a live path; uid -1 leaves the owner
        // unchanged, so only the group moves.
        let rc = unsafe { libc::chown(cpath.as_ptr(), u32::MAX, gid) };
        if rc != 0 {
            tracing::warn!(
                target: "cosmix_blobd",
                "chgrp {} to {group:?} failed (is the daemon a member? the unit carries \
                 SupplementaryGroups=cosmix-blob): {}",
                dir.display(),
                io::Error::last_os_error()
            );
            continue;
        }
        if let Err(error) = fs::set_permissions(dir, PermissionsExt::from_mode(0o2750)) {
            tracing::warn!(
                target: "cosmix_blobd",
                "setgid {} (mode 2750) failed: {error}",
                dir.display()
            );
        }
    }
}

/// Resolve a group name to its gid (`getgrnam`); `None` when the group
/// does not exist on this host.
fn group_gid(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getgrnam returns a pointer to libc-owned storage valid
    // until the next group call; the gid is copied out immediately.
    unsafe {
        let gr = libc::getgrnam(c.as_ptr());
        if gr.is_null() {
            None
        } else {
            Some((*gr).gr_gid)
        }
    }
}

/// Resolve a gid to its group name (`getgrgid`); `None` on failure.
/// Test-side twin of [`group_gid`].
#[cfg(test)]
fn gid_group(gid: u32) -> Option<String> {
    // SAFETY: as group_gid — the name is copied out immediately.
    unsafe {
        let gr = libc::getgrgid(gid);
        if gr.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*gr).gr_name)
                .to_string_lossy()
                .into_owned(),
        )
    }
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
            2 => BLOBD_V2_SQL,
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
    use std::sync::Arc;
    use tempfile::TempDir;

    use super::super::config::{DEFAULT_QUOTA_OWNER_BYTES, DEFAULT_QUOTA_TOTAL_BYTES};

    fn options() -> StoreOptions {
        StoreOptions {
            origin: "testnode".into(),
            quota_total_bytes: DEFAULT_QUOTA_TOTAL_BYTES,
            quota_owner_default_bytes: DEFAULT_QUOTA_OWNER_BYTES,
            owner_limits: BTreeMap::new(),
            cas_group: "cosmix-blob".into(),
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
            cas_group: "cosmix-blob".into(),
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

    // ---- M1: GC may never delete bytes a pin just acknowledged ----

    #[test]
    fn record_upload_and_record_fetch_refuse_vanished_bytes() {
        // The existence re-check under the db lock: bytes that a
        // concurrent sweep (or anything else) removed never get a pin
        // row — the refusal is Vanished, not a dangling pin.
        let (dir, store) = store();
        let src = write_src(&dir, "gone.bin", b"vanishing act");
        let out = store.put(&src, &PutOptions::new("maild")).unwrap();
        store.unpin(&out.reference.hash, "maild").unwrap();
        fs::remove_file(store.path(&out.reference.hash).unwrap()).unwrap();

        let err = store
            .record_upload(
                &out.reference.hash,
                15,
                "application/octet-stream",
                None,
                "lane:127.0.0.1",
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Vanished(_)), "got {err:?}");

        let err = store
            .record_fetch(
                &out.reference.hash,
                15,
                "application/octet-stream",
                "A",
                &["maild".to_string()],
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Vanished(_)), "got {err:?}");

        // No pin rows landed; quota is untouched.
        assert!(store.pin_owners(&out.reference.hash).unwrap().is_empty());
        assert_eq!(store.quota_report(None).unwrap().total.used, 0);
    }

    #[test]
    fn record_fetch_refuses_over_cap_owners_alone() {
        // R1: the owners that fit pin and commit; the over-cap one is
        // refused with nothing written; all refused = none_fit.
        let dir = TempDir::new().unwrap();
        let options = StoreOptions {
            owner_limits: BTreeMap::from([("tiny".to_string(), 10)]),
            ..options()
        };
        let store = Store::open(dir.path(), options).unwrap();
        let hash = blob::put(&store.blobs_root(), &[7u8; 50]).unwrap();
        let owners = ["maild".to_string(), "tiny".to_string()];
        let pins = store
            .record_fetch(&hash, 50, "application/octet-stream", "A", &owners)
            .unwrap();
        assert_eq!(pins.pinned, vec!["maild".to_string()]);
        assert_eq!(pins.refused, vec!["tiny".to_string()]);
        assert!(!pins.none_fit());
        assert_eq!(store.pin_owners(&hash).unwrap(), vec!["maild".to_string()]);
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["maild"].used, 50);
        assert!(report.owners.get("tiny").is_none_or(|q| q.used == 0));

        let alone = store
            .record_fetch(&hash, 50, "application/octet-stream", "A", &owners[1..])
            .unwrap();
        assert!(alone.none_fit(), "{alone:?}");
    }

    #[test]
    fn gc_never_sweeps_bytes_a_concurrent_pin_just_acked() {
        // The M1 race, hammered: an unpinned blob older than the grace
        // window; a lane-style idempotent re-put (record_upload) runs
        // concurrently with blob.gc. The db mutex is held across GC's
        // pin check, mtime re-stat and unlink, and record_upload
        // re-checks existence inside the same mutex, so every
        // interleaving ends either pinned-and-present or
        // refused-vanished — never an acknowledged pin on unlinked
        // bytes.
        for _ in 0..25 {
            let (dir, store) = store();
            let src = write_src(&dir, "race.bin", b"race me");
            let out = store.put(&src, &PutOptions::new("first")).unwrap();
            store.unpin(&out.reference.hash, "first").unwrap();
            age_file(&store.path(&out.reference.hash).unwrap());
            let store = Arc::new(store);

            let pinning = {
                let store = Arc::clone(&store);
                let hash = out.reference.hash;
                std::thread::spawn(move || {
                    store.record_upload(
                        &hash,
                        7,
                        "application/octet-stream",
                        None,
                        "lane:127.0.0.1",
                    )
                })
            };
            let _sweep = store.gc(false).unwrap();
            let pinned = pinning.join().unwrap();

            let present = blob::exists(&store.blobs_root(), &out.reference.hash).unwrap();
            match &pinned {
                Ok(_) => assert!(
                    present,
                    "an acknowledged pin must never dangle (gc swept it underneath)"
                ),
                Err(e) => {
                    assert!(
                        matches!(e, StoreError::Vanished(_)),
                        "the losing interleaving must be Vanished, got {e:?}"
                    );
                    assert!(!present);
                }
            }
        }
    }

    #[test]
    fn idempotent_re_put_refreshes_the_grace_window() {
        // M1c: bytes already in the CAS keep their 60 s grace on a
        // re-put — the idempotent branch touches the file's mtime, so
        // a dry run that swept the aged blob before the re-put must
        // find it young after.
        let (dir, store) = store();
        let src = write_src(&dir, "idem.bin", b"idempotent me");
        let out = store.put(&src, &PutOptions::new("maild")).unwrap();
        store.unpin(&out.reference.hash, "maild").unwrap();
        age_file(&store.path(&out.reference.hash).unwrap());
        let before = store.gc(true).unwrap();
        assert_eq!(before.swept.len(), 1, "aged and unpinned: a candidate");

        let again = store.put(&src, &PutOptions::new("maild")).unwrap();
        assert_eq!(again.reference.hash, out.reference.hash);
        assert!(
            again.newly_pinned,
            "the unpin removed the row; the re-put re-pins"
        );
        store.unpin(&out.reference.hash, "maild").unwrap();

        let after = store.gc(true).unwrap();
        assert!(
            after.swept.is_empty(),
            "the re-put refreshed the grace window"
        );
        assert_eq!(after.skipped_young, 1);
    }

    // ---- M3: concurrent uploads cannot overshoot a cap ----

    #[test]
    fn reservations_bound_concurrent_admissions_and_report() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                origin: "testnode".into(),
                quota_total_bytes: 2 * 1024 * 1024,
                quota_owner_default_bytes: 2 * 1024 * 1024,
                owner_limits: BTreeMap::from([(("race").to_string(), 1024 * 1024)]),
                cas_group: "cosmix-blob".into(),
            },
        )
        .unwrap();

        // A declared 700 KiB holds 700 KiB of the owner's 1 MiB…
        let r1 = store.reserve_upload("race", Some(700 * 1024)).unwrap();
        assert_eq!(r1.cap(), 700 * 1024);
        // …so a second 700 KiB admission against the same cap is
        // refused — the old check-then-act would have granted both.
        let err = store.reserve_upload("race", Some(700 * 1024)).unwrap_err();
        assert!(
            matches!(err, StoreError::QuotaOwner { ref owner, would_use, limit } if owner == "race" && would_use == 1400 * 1024 && limit == 1024 * 1024),
            "got {err:?}"
        );

        // blob.quota reports the hold beside used.
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["race"].used, 0);
        assert_eq!(report.owners["race"].reserved, 700 * 1024);
        assert_eq!(report.total.reserved, 700 * 1024);

        // Release on drop, then the same admission succeeds.
        drop(r1);
        assert_eq!(store.quota_report(None).unwrap().total.reserved, 0);
        let _again = store.reserve_upload("race", Some(700 * 1024)).unwrap();

        // An absent length reserves the owner's whole remaining room:
        // the mid-stream counter enforces it from the first byte.
        let r2 = store.reserve_upload("race", None).unwrap();
        assert_eq!(r2.cap(), 324 * 1024);
    }

    // ---- M4: the CAS carries the shared-read group, setgid ----

    #[test]
    fn cas_roots_and_shard_dirs_carry_the_group_and_setgid() {
        // The test host may not have cosmix-blob, so drive the fix with
        // this process's own primary group — the mechanics under test
        // (chgrp, setgid, inheritance into mds's shard dirs and the CAS
        // files) are group-name-agnostic.
        let group = {
            // SAFETY: getgid is a pure syscall.
            let gid = unsafe { libc::getgid() };
            match gid_group(gid) {
                Some(name) => name,
                None => {
                    eprintln!("skipping: no group name for this process's gid");
                    return;
                }
            }
        };
        let dir = TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                cas_group: group.clone(),
                ..options()
            },
        )
        .unwrap();
        let src = write_src(&dir, "grouped.bin", b"shared read");
        let out = store.put(&src, &PutOptions::new("maild")).unwrap();

        let assert_grouped = |path: &Path| {
            let md = fs::metadata(path).unwrap();
            assert_eq!(
                md.gid(),
                group_gid(&group).unwrap(),
                "{} must carry the configured group",
                path.display()
            );
            assert!(
                md.mode() & 0o2000 != 0 || md.is_file(),
                "{} must carry setgid (directories)",
                path.display()
            );
        };
        assert_grouped(dir.path());
        assert_grouped(&store.blobs_root());
        let cas = store.path(&out.reference.hash).unwrap();
        // Both shard dirs inherit the setgid bit and the group; the CAS
        // file inherits the group.
        let shard = cas.parent().unwrap();
        let top = shard.parent().unwrap();
        assert_grouped(top);
        assert_grouped(shard);
        assert_eq!(
            fs::metadata(&cas).unwrap().gid(),
            group_gid(&group).unwrap()
        );
        // The setgid bit is on the shard dirs themselves.
        assert!(fs::metadata(top).unwrap().mode() & 0o2000 != 0);
        assert!(fs::metadata(shard).unwrap().mode() & 0o2000 != 0);
    }

    #[test]
    fn an_absent_cas_group_is_skipped_without_failing_open() {
        // A dev host without the group: open succeeds, the tree stays
        // daemon-owned, and the store still serves verbs.
        let dir = TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                cas_group: "cosmix-blob-definitely-not-on-any-host".into(),
                ..options()
            },
        )
        .unwrap();
        let src = write_src(&dir, "plain.bin", b"still works");
        assert!(store.put(&src, &PutOptions::new("maild")).is_ok());
    }

    // ---- F6: every pin pays its cap; unpin releases what was paid ----

    #[test]
    fn pin_beyond_the_owner_cap_is_refused() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(
            dir.path(),
            StoreOptions {
                origin: "testnode".into(),
                owner_limits: BTreeMap::from([(("tightside").to_string(), 16u64)]),
                ..options()
            },
        )
        .unwrap();
        // Land 20 bytes under an owner whose cap allows it, then pin
        // the same present bytes as the 16-byte owner: a present-hash
        // pin still costs the bytes (F6) — refused, nothing written.
        let src = write_src(&dir, "pinme.bin", &[3u8; 20]);
        let out = store.put(&src, &PutOptions::new("roomy")).unwrap();
        let err = store.pin(&out.reference.hash, "tightside").unwrap_err();
        assert!(
            matches!(err, StoreError::QuotaOwner { ref owner, would_use: 20, limit: 16 } if owner == "tightside"),
            "got {err:?}"
        );
        assert!(store.pin_owners(&out.reference.hash).unwrap() == vec!["roomy".to_string()]);
        assert_eq!(
            store.quota_report(None).unwrap().owners["tightside"].used,
            0
        );
    }

    #[test]
    fn unpin_releases_the_recorded_size_with_file_and_row_gone() {
        // The F6 bug: unpin of a hash with no mds row and no CAS file
        // released 0 and the owner's quota leaked forever. The v2 pin
        // row records what the pin paid; that is what unpin releases.
        let (dir, store) = store();
        let src = write_src(&dir, "leaky.bin", &[5u8; 50]);
        let out = store.put(&src, &PutOptions::new("leaker")).unwrap();
        assert_eq!(store.quota_report(None).unwrap().owners["leaker"].used, 50);
        // No mds row (blobd puts are rowless) and no file: only the
        // pin row remembers the size.
        fs::remove_file(store.path(&out.reference.hash).unwrap()).unwrap();
        assert!(store.unpin(&out.reference.hash, "leaker").unwrap());
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["leaker"].used, 0, "quota must not leak");
        assert_eq!(report.total.used, 0);
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
    fn second_open_touches_nothing_under_the_root() {
        // m2: the flock comes before mds's open, so a refused second
        // instance creates, migrates and rewrites nothing. Removing a
        // directory mds's open would recreate makes the check sharp:
        // opening mds first brings `containers/` back.
        fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, u64)> {
            let mut out = BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(&dir).unwrap() {
                    let p = entry.unwrap().path();
                    let md = fs::symlink_metadata(&p).unwrap();
                    if md.is_dir() {
                        stack.push(p.clone());
                    }
                    out.insert(p, (md.modified().unwrap(), md.len()));
                }
            }
            out
        }
        let (dir, _store) = store();
        fs::remove_dir(dir.path().join("containers")).unwrap();
        let before = snapshot(dir.path());
        std::thread::sleep(Duration::from_millis(20));
        let err = match Store::open(dir.path(), options()) {
            Err(err) => err,
            Ok(_) => panic!("second open of a locked root must fail"),
        };
        assert!(matches!(err, StoreError::Locked(_)), "got {err:?}");
        assert_eq!(
            snapshot(dir.path()),
            before,
            "the refused open touched the root"
        );
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
    fn blobd_sqlite_schema_is_v2_with_expected_tables() {
        let (dir, store) = store();
        let conn = Connection::open(dir.path().join("blobd.sqlite")).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
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
        // v2 (F6): the pins row records the size it accounted.
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pins') WHERE name='size_bytes';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 1, "pins.size_bytes missing");
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
