//! Import a per-set tar archive produced by `Mds::export_set`.
//!
//! Phase 5.8 deliverable backing `cosmix-mds import <TARBALL_PATH>`.
//! The import is the inverse of the export, with the *same* layout
//! contract:
//!
//! ```text
//! manifest.json              — first entry, REQUIRED
//! data.sqlite                — REQUIRED, exactly once
//! blobs/<hh>/<hh>/<hex>      — zero or more, name MUST equal content hash
//! ```
//!
//! ## Why import is more than "untar into place"
//!
//! 1. **Path traversal hardening.** Tarballs come from outside the
//!    process boundary. Even with WG-only operator interfaces, a
//!    misbehaving exporter could craft entries like `../../etc/foo`
//!    or absolute paths. We refuse anything that isn't in the strict
//!    whitelist above, including duplicates.
//!
//! 2. **Hash verification per blob.** Each blob entry's path encodes
//!    the BLAKE3 of its bytes — `blobs/<hh>/<hh>/<hex>` where the
//!    sharded prefix is the first 4 hex chars of `<hex>`. We re-hash
//!    every blob's bytes and reject any entry whose path disagrees
//!    with its content. This catches accidental corruption (rotational
//!    bit flips, network truncations) and a deliberate bytes/path
//!    mismatch from a hostile exporter.
//!
//! 3. **Manifest/data consistency.** `manifest.set_id` must round-trip
//!    as a UUID; `manifest.format == "cosmix-mds-set-export"` and
//!    `format_version == 1`; `manifest.schema_version` must be ≤ the
//!    importing site's `CURRENT_SCHEMA_VERSION` (older tarballs are
//!    migrated forward after staging). After staging, we
//!    verify `item_count`
//!    matches the manifest and every distinct `item.blob_hash` is
//!    present in CAS now.
//!
//! 4. **`blobs.sqlite` re-derivation for this set, in the same
//!    transaction.** The exporting site does NOT ship `blobs.sqlite`
//!    (it's a derived index). After we land `data.sqlite`, we open
//!    the per-set connection (with `blobs_db` ATTACH'd) and ingest
//!    one `(hash, set_id, item_id)` `blob_ref` row per item plus
//!    refcount updates — within a single transaction. Rationale: if
//!    we left this for `rebuild-index` to clean up, a delivery that
//!    raced *into* the imported set between import and rebuild would
//!    UPDATE refcount for shared hashes from 0 → 1 instead of N → N+1,
//!    and a subsequent `gc` could collect a blob still referenced
//!    by the imported items.
//!
//! 5. **Atomic visibility.** Stage `data.sqlite` and CAS files first
//!    (CAS is content-addressed, so partial CAS writes are safe and
//!    `blob::put` already gives atomic per-file visibility). Only at
//!    the end do we mkdir `containers/<set-uuid>/`, rename the staged
//!    `data.sqlite` into it, and insert the cache entry — under the
//!    same write lock create_set uses. A crash between blob staging
//!    and the final rename leaves stranded CAS files, sweepable by
//!    `gc` (orphan rows get cleaned, and `rebuild-index` reports
//!    orphan CAS files); we never end up with a half-attached set.
//!
//! ## What import deliberately does NOT do
//!
//! - It does not create or update `blob_verify` rows. That's the
//!   verify ledger; it's empty for newly-imported blobs because we
//!   haven't actually verified them on this site (we hash-verified
//!   *during* import, but blob_verify is the persistent ledger of
//!   later sweeps). `verify --full` will populate it on first run.
//! - It does not run `rebuild-index`. The per-set blob_ref ingest
//!   above is sufficient for correctness of this set's blobs;
//!   cross-set reconciliation isn't import's job.

use crate::blob;
use crate::blob_index;
use crate::error::{Error, Result};
use crate::export::{FORMAT_TAG, FORMAT_VERSION};
use crate::schema;
use crate::types::{ImportReport, SetId};
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MANIFEST_NAME: &str = "manifest.json";
const DATA_NAME: &str = "data.sqlite";
const BLOBS_PREFIX: &str = "blobs/";
/// Cap manifest size at 64 KB. The real manifest is < 1 KB; the cap
/// is a "this isn't a manifest" tripwire so a hostile archive can't
/// exhaust memory by labelling a giant blob "manifest.json".
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// Latest per-set schema version this build understands. Tarballs
/// carrying `manifest.schema_version <= CURRENT_SCHEMA_VERSION` are
/// accepted; ones above it are forward-incompat and rejected.
///
/// The Phase 5 export/import contract treats older tarballs as
/// permanent — see `_doc/2026-04-29-cosmix-mds-plan.md` §"Phase 5".
/// Acceptance of an older `schema_version` relies on
/// `apply_data_migrations` running after the staged file is renamed
/// into place: that call upgrades the per-set DB in-place to
/// `DATA_LATEST` before any blob_ref ingest touches it. The
/// `verify_staged_data` consistency check still enforces
/// `staged user_version == manifest.schema_version`, so a tampered
/// archive whose manifest disagrees with its DB is still rejected.
///
/// MUST track `schema::DATA_LATEST` (currently 9 after the v1.8
/// item-keyed sidecar FK-cascade migration): a fresh export records
/// the live `PRAGMA user_version`, so import has to accept tarballs up
/// to the newest schema this build can produce, or a round-trip of a
/// freshly-exported set is rejected.
const CURRENT_SCHEMA_VERSION: u32 = 9;

#[derive(Debug, Deserialize)]
struct Manifest {
    format: String,
    format_version: u32,
    set_id: String,
    schema_version: u32,
    #[serde(default)]
    #[allow(dead_code)] // RFC3339 string; not used for logic, only audit
    exported_at: String,
    item_count: u64,
    blob_count: u64,
}

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    Error::Other(format!("{prefix}: {e}"))
}

/// Outcome of the (mostly read-only) staging pass — bytes are in CAS,
/// `data.sqlite` is staged in `<root>/.import-staging/<uuid>/`, and we
/// know the set id from the manifest. The store-layer caller takes
/// the cache write lock and finalizes from here.
pub(crate) struct Staged {
    pub set_id: SetId,
    pub manifest_item_count: u64,
    pub manifest_blob_count: u64,
    pub bytes_read: u64,
    pub data_staged_at: PathBuf,
    pub staging_dir: PathBuf,
}

impl Staged {
    /// Best-effort cleanup of the staging directory. The CAS files we
    /// streamed in via `blob::put` are content-addressed, so leaving
    /// them on a finalize failure is safe — `gc` collects them later
    /// because `data.sqlite` (and thus `blob`/`blob_ref`) never
    /// referenced them. Only the staged `data.sqlite` and the
    /// staging dir itself need explicit cleanup.
    pub(crate) fn cleanup_staging(&self) {
        let _ = fs::remove_file(&self.data_staged_at);
        let _ = fs::remove_dir(&self.staging_dir);
    }
}

/// Stream-validate the tarball, hash-verify every blob entry, drop
/// blobs into CAS, and stage `data.sqlite` under the import-staging
/// dir. Returns `Staged` on success; the store-layer wrapper does
/// the cache-write-lock finalize from there.
///
/// Caller is the store-layer `import_set` wrapper. We don't take any
/// locks here — staging touches CAS (which is concurrency-safe via
/// `blob::put`'s atomic hard-link protocol) and a fresh
/// `<root>/.import-staging/<uuid>/` directory. The set's UUID is read
/// from the manifest before any side effect that would conflict with
/// the cache.
pub(crate) fn stage(root: &Path, tarball: &Path) -> Result<Staged> {
    let bytes_read = fs::metadata(tarball).map_err(Error::Io)?.len();
    let f = File::open(tarball).map_err(Error::Io)?;
    let mut ar = tar::Archive::new(f);

    let blobs_root = root.join("blobs");
    fs::create_dir_all(blobs_root.join(".tmp")).map_err(Error::Io)?;

    let mut manifest_seen = false;
    let mut manifest: Option<Manifest> = None;
    let mut data_staged: Option<PathBuf> = None;
    let mut staging_dir: Option<PathBuf> = None;
    let mut blobs_staged: BTreeSet<String> = BTreeSet::new();

    let entries = ar.entries().map_err(Error::Io)?;
    for (entry_index, entry) in entries.enumerate() {
        let mut entry = entry.map_err(Error::Io)?;
        let header = entry.header();
        if header.entry_type() != tar::EntryType::Regular {
            return Err(Error::Other(format!(
                "import: rejecting non-regular tar entry at index {entry_index}"
            )));
        }
        let path = entry.path().map_err(Error::Io)?.into_owned();
        let name = path
            .to_str()
            .ok_or_else(|| {
                Error::Other(format!(
                    "import: tar entry path is not UTF-8 at index {entry_index}"
                ))
            })?
            .to_string();
        validate_entry_name(&name, entry_index)?;

        if entry_index == 0 {
            // First entry must be the manifest (export protocol).
            if name != MANIFEST_NAME {
                return Err(Error::Other(format!(
                    "import: expected first entry {MANIFEST_NAME:?}, got {name:?}"
                )));
            }
        }

        match name.as_str() {
            MANIFEST_NAME => {
                if manifest_seen {
                    return Err(Error::Other("import: duplicate manifest.json entry".into()));
                }
                manifest_seen = true;
                let size = header.size().map_err(Error::Io)? as usize;
                if size > MAX_MANIFEST_BYTES {
                    return Err(Error::Other(format!(
                        "import: manifest.json size {size} exceeds {MAX_MANIFEST_BYTES} cap"
                    )));
                }
                let mut bytes = Vec::with_capacity(size);
                entry.read_to_end(&mut bytes).map_err(Error::Io)?;
                let m: Manifest = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Other(format!("import: parse manifest.json: {e}")))?;
                validate_manifest(&m)?;
                let set_uuid = uuid::Uuid::parse_str(&m.set_id).map_err(|e| {
                    Error::Other(format!(
                        "import: manifest.set_id {set_id:?}: {e}",
                        set_id = m.set_id
                    ))
                })?;
                let stage_dir = root.join(".import-staging").join(set_uuid.to_string());
                if stage_dir.exists() {
                    fs::remove_dir_all(&stage_dir).map_err(Error::Io)?;
                }
                fs::create_dir_all(&stage_dir).map_err(Error::Io)?;
                staging_dir = Some(stage_dir);
                manifest = Some(m);
            }
            DATA_NAME => {
                if data_staged.is_some() {
                    return Err(Error::Other("import: duplicate data.sqlite entry".into()));
                }
                let stage = staging_dir.as_ref().ok_or_else(|| {
                    // Manifest-first ordering enforced above; this
                    // branch only triggers if manifest validation
                    // returned Ok but didn't set staging_dir, which
                    // would be a programmer error.
                    Error::Other("import: data.sqlite seen before manifest".into())
                })?;
                let dst = stage.join(DATA_NAME);
                let mut out = File::create(&dst).map_err(Error::Io)?;
                std::io::copy(&mut entry, &mut out).map_err(Error::Io)?;
                out.sync_all().map_err(Error::Io)?;
                data_staged = Some(dst);
            }
            _ if name.starts_with(BLOBS_PREFIX) => {
                let claimed_hex = expected_blob_hex(&name)?;
                if !blobs_staged.insert(claimed_hex.clone()) {
                    return Err(Error::Other(format!(
                        "import: duplicate blob entry {name:?}"
                    )));
                }
                let mut bytes = Vec::with_capacity(header.size().map_err(Error::Io)? as usize);
                entry.read_to_end(&mut bytes).map_err(Error::Io)?;
                let actual = blob::hash_bytes(&bytes);
                let actual_hex = blob::hex(&actual);
                if actual_hex != claimed_hex {
                    return Err(Error::Other(format!(
                        "import: blob {name:?} content hash mismatch \
                         (path claims {claimed_hex}, bytes hash to {actual_hex})"
                    )));
                }
                // CAS placement is atomic and idempotent — a shared
                // hash already on disk is fine, blob::put short-
                // circuits at the existence check.
                let _placed = blob::put(&blobs_root, &bytes)?;
            }
            _ => {
                return Err(Error::Other(format!(
                    "import: unexpected tar entry {name:?}"
                )));
            }
        }
    }

    if !manifest_seen {
        return Err(Error::Other("import: manifest.json missing".into()));
    }
    let manifest = manifest.expect("manifest set under manifest_seen guard");
    let staging_dir = staging_dir.expect("staging_dir set when manifest parsed");
    let data_staged_at = data_staged.ok_or_else(|| {
        // Failure path: nothing visible to other writers, but the
        // staging dir is on disk. Best-effort cleanup so a re-run
        // doesn't trip over leftovers.
        let _ = fs::remove_dir_all(&staging_dir);
        Error::Other("import: data.sqlite missing".into())
    })?;

    if blobs_staged.len() as u64 != manifest.blob_count {
        let _ = fs::remove_file(&data_staged_at);
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(Error::Other(format!(
            "import: manifest.blob_count={} disagrees with {} tar blob entries",
            manifest.blob_count,
            blobs_staged.len()
        )));
    }

    // Verify staged data.sqlite is structurally sound, consistent
    // with the manifest, AND that the set of distinct hashes it
    // references is exactly equal to the set of blob entries we
    // staged from the tar. The count-only check above is necessary
    // but not sufficient: a corrupted archive could have data.sqlite
    // referencing hash H while shipping a blob for unrelated hash X
    // — same count, disjoint sets. Without this cross-check the
    // imported set would be queryable but with bytes missing from
    // CAS (for H) and an orphan (X) sitting around. We also pre-
    // validate every item row's `id` and `blob_hash` parse cleanly
    // here so `ingest_blob_refs` in `finalize` cannot fail after
    // the rename has made the half-installed set visible on disk.
    if let Err(e) = verify_staged_data(&data_staged_at, &manifest, &blobs_staged) {
        let _ = fs::remove_file(&data_staged_at);
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(e);
    }

    let set_id = SetId(uuid::Uuid::parse_str(&manifest.set_id).expect("validated above"));
    Ok(Staged {
        set_id,
        manifest_item_count: manifest.item_count,
        manifest_blob_count: manifest.blob_count,
        bytes_read,
        data_staged_at,
        staging_dir,
    })
}

/// Take staged artifacts and atomically install them under
/// `containers/<set-uuid>/`. Caller (`SqliteCasMds::import_set`) MUST
/// hold the box-wide write gate and then the cache write lock for the
/// duration of this call, mirroring `create_set`'s discipline against
/// same-UUID races while also serializing the attached blobs writer.
///
/// We deliberately do NOT take the direct blobs-connection mutex
/// here: the new per-set connection drives the write through its
/// ATTACH'd `blobs_db`. The caller's gate-before-cache order matches
/// the store module's global lock order and avoids inversion against
/// `delete_set`.
pub(crate) fn finalize(
    root: &Path,
    staged: &Staged,
    started: Instant,
    busy_timeout: Duration,
) -> Result<(ImportReport, Connection)> {
    let target_dir = root.join("containers").join(staged.set_id.0.to_string());
    if target_dir.exists() {
        return Err(Error::SetAlreadyExists { set: staged.set_id });
    }
    fs::create_dir_all(&target_dir).map_err(Error::Io)?;

    // Move data.sqlite into place. rename is atomic on the same
    // filesystem; `<root>/.import-staging/<uuid>/data.sqlite` is in
    // the same fs as `<root>/containers/<uuid>/data.sqlite` by
    // construction (both under root).
    let target_data = target_dir.join(DATA_NAME);
    fs::rename(&staged.data_staged_at, &target_data).map_err(|e| {
        // rename failed: nothing visible to readers (the dir is
        // empty); remove it so a re-run can succeed.
        let _ = fs::remove_dir(&target_dir);
        Error::Io(e)
    })?;

    // fsync the parent so the rename is durable.
    if let Ok(f) = File::open(&target_dir) {
        let _ = f.sync_all();
    }

    // Past the rename, `containers/<uuid>/` is observable as a
    // "real" set by `SqliteCasMds::open()`'s discover_sets sweep.
    // If anything from here through tx.commit() fails, we MUST
    // unwind that visibility — otherwise a process restart picks
    // up a half-installed set whose blob_ref rows never landed,
    // and a subsequent `gc` could collect bytes the set still
    // references through `data.sqlite`. Pre-validation in
    // `verify_staged_data` makes ingest_blob_refs catastrophe-only
    // (item.id and item.blob_hash format already verified, hash set
    // already cross-checked), but I/O is I/O — defense in depth.
    let unwind = || {
        let _ = fs::remove_file(&target_data);
        let _ = fs::remove_dir(&target_dir);
    };

    // Open the per-set connection with blobs_db ATTACH'd, then run
    // `apply_data_migrations` to step the staged db from its declared
    // version (≤ CURRENT_SCHEMA_VERSION) up to current. For a tarball
    // already at current, the runner is a no-op; for a legacy v1 or
    // v1.1 tarball it applies the missing migration steps in order.
    let blobs_path = root.join("blobs.sqlite");
    let mut conn = match Connection::open(&target_data) {
        Ok(c) => c,
        Err(e) => {
            unwind();
            return Err(Error::Other(format!(
                "open imported {}: {e}",
                target_data.display()
            )));
        }
    };
    if let Err(e) = schema::apply_data_migrations(&mut conn) {
        drop(conn);
        unwind();
        return Err(e);
    }
    // v1.1: seed set_state so the set_change_seq allocator finds its
    // singleton row on first mutation. Same helper open_set_db uses;
    // this keeps imported sets behaving identically to natively-opened
    // ones from the first transaction.
    if let Err(e) = schema::seed_set_state(&conn, &staged.set_id) {
        drop(conn);
        unwind();
        return Err(e);
    }
    if let Err(e) = attach_blobs_db(&conn, &blobs_path) {
        drop(conn);
        unwind();
        return Err(e);
    }

    // After ATTACH, matching `open_set_db`: the timeout is
    // connection-wide and must cover the attached blobs database.
    if let Err(e) = crate::store::set_busy_timeout(&conn, busy_timeout) {
        drop(conn);
        unwind();
        return Err(e);
    }

    // Ingest blob_ref + refcount + blob rows for the imported set in
    // a single per-set transaction. blobs_db is ATTACH'd, so the
    // INSERT/UPDATE statements span both databases atomically.
    let ingest = (|| -> Result<()> {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| map_sql_err("BEGIN import blob_ref ingest", e))?;
        ingest_blob_refs(&tx, staged.set_id)?;
        tx.commit()
            .map_err(|e| map_sql_err("COMMIT import blob_ref ingest", e))?;
        Ok(())
    })();
    if let Err(e) = ingest {
        drop(conn);
        unwind();
        return Err(e);
    }

    let _ = fs::remove_dir(&staged.staging_dir);

    let report = ImportReport {
        set_id: staged.set_id,
        item_count: staged.manifest_item_count,
        blob_count: staged.manifest_blob_count,
        bytes_read: staged.bytes_read,
        duration: started.elapsed(),
    };
    Ok((report, conn))
}

fn ingest_blob_refs(tx: &rusqlite::Transaction<'_>, set: SetId) -> Result<()> {
    // Pull (item_id, blob_hash, size_bytes) from the imported set's
    // item table. We rebuild the blob/blob_ref rows the same way a
    // delivery transaction would have over the lifetime of the
    // exporting site — one refcount tick per item, one blob_ref row
    // per (hash, set, item). This matches `add_blob_ref_in_tx`
    // exactly so a future change to delivery's refcount semantics
    // doesn't silently drift import out of step.
    let pairs: Vec<(String, String, i64)> = {
        let mut stmt = tx
            .prepare("SELECT id, blob_hash, size_bytes FROM item")
            .map_err(|e| map_sql_err("prepare imported item scan", e))?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| map_sql_err("query imported items", e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| map_sql_err("row imported items", e))?
    };

    for (item_id_s, hash_hex, size) in pairs {
        let item_uuid = uuid::Uuid::parse_str(&item_id_s).map_err(|e| {
            Error::Other(format!(
                "import: imported item.id {item_id_s:?} is not a UUID: {e}"
            ))
        })?;
        let hash = blob::from_hex(&hash_hex).ok_or_else(|| {
            Error::Other(format!(
                "import: imported item.blob_hash {hash_hex:?} is not 64-char hex"
            ))
        })?;
        let item = crate::types::ItemId(item_uuid);
        blob_index::add_blob_ref_in_tx(tx, &set, &item, &hash, size as u64)?;
    }
    Ok(())
}

fn attach_blobs_db(conn: &Connection, blobs_path: &Path) -> Result<()> {
    let s = blobs_path.to_str().ok_or_else(|| {
        Error::Other(format!(
            "blobs.sqlite path {} is not valid UTF-8",
            blobs_path.display()
        ))
    })?;
    let escaped = s.replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS blobs_db;"))
        .map_err(|e| Error::Other(format!("ATTACH blobs.sqlite as blobs_db: {e}")))?;
    Ok(())
}

fn validate_manifest(m: &Manifest) -> Result<()> {
    if m.format != FORMAT_TAG {
        return Err(Error::Other(format!(
            "import: manifest.format {:?} is not {FORMAT_TAG:?}",
            m.format
        )));
    }
    if m.format_version != FORMAT_VERSION {
        return Err(Error::Other(format!(
            "import: manifest.format_version {} is not supported (this build expects {FORMAT_VERSION})",
            m.format_version
        )));
    }
    if m.schema_version == 0 || m.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(Error::Other(format!(
            "import: manifest.schema_version {} is not supported (this build understands up to v{CURRENT_SCHEMA_VERSION})",
            m.schema_version
        )));
    }
    Ok(())
}

fn verify_staged_data(
    path: &Path,
    m: &Manifest,
    staged_blob_hexes: &BTreeSet<String>,
) -> Result<()> {
    let conn = Connection::open(path)
        .map_err(|e| Error::Other(format!("open staged data.sqlite: {e}")))?;
    let user_version: u32 = conn
        .query_row("PRAGMA user_version", params![], |r| r.get::<_, i64>(0))
        .map_err(|e| map_sql_err("staged user_version", e))? as u32;
    if user_version != m.schema_version {
        return Err(Error::Other(format!(
            "import: staged data.sqlite PRAGMA user_version={user_version} disagrees with manifest schema_version={}",
            m.schema_version
        )));
    }
    let item_count: u64 = conn
        .query_row("SELECT COUNT(*) FROM item", params![], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|e| map_sql_err("staged count item", e))? as u64;
    if item_count != m.item_count {
        return Err(Error::Other(format!(
            "import: staged data.sqlite item count {item_count} disagrees with manifest.item_count={}",
            m.item_count
        )));
    }

    // Pre-validate every item row's id (UUID) and blob_hash
    // (64-char lowercase hex) so `ingest_blob_refs` in `finalize`
    // cannot fail after rename — its only possible failure modes
    // are exactly these parse errors plus catastrophic SQLite I/O.
    // While iterating, accumulate the distinct-hash set for the
    // cross-check below (cheaper than a second SELECT DISTINCT
    // pass on the same table).
    let mut data_hexes: BTreeSet<String> = BTreeSet::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, blob_hash FROM item")
            .map_err(|e| map_sql_err("prepare staged item scan", e))?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| map_sql_err("query staged items", e))?;
        for row in rows {
            let (id_s, hash_hex) = row.map_err(|e| map_sql_err("row staged items", e))?;
            uuid::Uuid::parse_str(&id_s).map_err(|e| {
                Error::Other(format!(
                    "import: staged item.id {id_s:?} is not a UUID: {e}"
                ))
            })?;
            if hash_hex.len() != 64
                || !hash_hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Other(format!(
                    "import: staged item.blob_hash {hash_hex:?} is not 64-char lowercase hex"
                )));
            }
            data_hexes.insert(hash_hex);
        }
    }

    if data_hexes.len() as u64 != m.blob_count {
        return Err(Error::Other(format!(
            "import: data.sqlite references {} distinct blobs but manifest.blob_count={}",
            data_hexes.len(),
            m.blob_count
        )));
    }

    // The load-bearing cross-check: every hash referenced by an
    // imported item row must have been shipped as a tar blob entry
    // (and vice versa). Without this a corrupted archive whose
    // data.sqlite + tar blob set merely have the same *count* would
    // import successfully and leave the set queryable with bytes
    // missing from CAS.
    if &data_hexes != staged_blob_hexes {
        let missing_in_tar: Vec<&String> = data_hexes.difference(staged_blob_hexes).collect();
        let missing_in_data: Vec<&String> = staged_blob_hexes.difference(&data_hexes).collect();
        return Err(Error::Other(format!(
            "import: data.sqlite blob_hash set disagrees with tar blob entries \
             (referenced by data but not shipped: {missing_in_tar:?}; \
             shipped but not referenced: {missing_in_data:?})"
        )));
    }

    Ok(())
}

fn validate_entry_name(name: &str, idx: usize) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Other(format!(
            "import: empty tar entry name at index {idx}"
        )));
    }
    if name.starts_with('/') {
        return Err(Error::Other(format!(
            "import: absolute path in tar entry {name:?}"
        )));
    }
    for component in name.split('/') {
        if component == ".." || component == "." || component.is_empty() {
            return Err(Error::Other(format!(
                "import: path traversal or empty component in {name:?}"
            )));
        }
    }
    if name.contains('\\') {
        // tar paths are slash-separated; backslash is suspicious and
        // never produced by our exporter.
        return Err(Error::Other(format!(
            "import: backslash in tar entry {name:?}"
        )));
    }
    Ok(())
}

fn expected_blob_hex(entry_name: &str) -> Result<String> {
    // Layout: blobs/<aa>/<bb>/<aabb...> exactly. Reject anything
    // else, including extra slashes, missing shard dirs, or
    // mismatched prefixes.
    let parts: Vec<&str> = entry_name.split('/').collect();
    if parts.len() != 4 || parts[0] != "blobs" {
        return Err(Error::Other(format!(
            "import: malformed blob entry path {entry_name:?}"
        )));
    }
    let p1 = parts[1];
    let p2 = parts[2];
    let hex = parts[3];
    if hex.len() != 64 {
        return Err(Error::Other(format!(
            "import: blob entry {entry_name:?} hex segment is {} chars, expected 64",
            hex.len()
        )));
    }
    // Enforce *lowercase* hex specifically. `blob::from_hex` is
    // case-insensitive (it uses `u8::from_str_radix`), so we can't
    // delegate to it for the canonical-form check; the bytes/path
    // comparison one layer up would still reject uppercase via
    // `blob::hex`'s lowercase output, but rejecting at the shard
    // layer is defense in depth.
    if !hex
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Other(format!(
            "import: blob entry {entry_name:?} hex segment is not lowercase hex"
        )));
    }
    if p1 != &hex[0..2] || p2 != &hex[2..4] {
        return Err(Error::Other(format!(
            "import: blob entry {entry_name:?} shard prefix disagrees with hash"
        )));
    }
    Ok(hex.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_entry_name_rejects_traversal_and_absolute() {
        assert!(validate_entry_name("../etc/passwd", 1).is_err());
        assert!(validate_entry_name("/etc/passwd", 1).is_err());
        assert!(validate_entry_name("a/../b", 1).is_err());
        assert!(validate_entry_name("a//b", 1).is_err());
        assert!(validate_entry_name("a/b\\c", 1).is_err());
    }

    #[test]
    fn validate_entry_name_accepts_layout_paths() {
        assert!(validate_entry_name("manifest.json", 0).is_ok());
        assert!(validate_entry_name("data.sqlite", 1).is_ok());
        assert!(
            validate_entry_name(
                "blobs/00/11/0011223344556677889900112233445566778899001122334455667788990011",
                2
            )
            .is_ok()
        );
    }

    #[test]
    fn expected_blob_hex_round_trips_layout() {
        let hex = "0011223344556677889900112233445566778899001122334455667788990011";
        let entry = format!("blobs/00/11/{hex}");
        assert_eq!(expected_blob_hex(&entry).unwrap(), hex);
    }

    #[test]
    fn expected_blob_hex_rejects_shard_prefix_mismatch() {
        let hex = "0011223344556677889900112233445566778899001122334455667788990011";
        let entry = format!("blobs/aa/bb/{hex}");
        assert!(expected_blob_hex(&entry).is_err());
    }

    #[test]
    fn expected_blob_hex_rejects_uppercase_hex() {
        let hex = "abcdef0011223344556677889900112233445566778899001122334455667788";
        let upper = hex.to_uppercase();
        // Shard prefix is taken from the (uppercase) string itself
        // so the prefix-equality check cannot be the one rejecting:
        // it must be the lowercase-only check upstream.
        let entry = format!("blobs/{}/{}/{}", &upper[0..2], &upper[2..4], upper);
        assert!(expected_blob_hex(&entry).is_err());
    }
}
