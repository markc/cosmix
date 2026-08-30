//! Per-set export to a tar archive.
//!
//! Phase 5.7 deliverable backing `cosmix-mds export <SET_UUID>
//! <TARBALL_PATH>`. A set export is the per-set `data.sqlite` plus
//! every CAS blob referenced by that set's `item` rows, distinct by
//! hash. Crucially the box-wide `blobs.sqlite` is *not* included —
//! it's a derived index that the importing site reconstructs from
//! `data.sqlite` + CAS via `rebuild-index` (or implicitly on first
//! `add_item` via the existing delivery path).
//!
//! ## Archive layout (root)
//!
//! ```text
//! manifest.json              — emitted FIRST so consumers can stream-read it
//! data.sqlite                — VACUUM INTO snapshot, no WAL/SHM
//! blobs/<hh>/<hh>/<hex>      — distinct CAS files referenced by item rows
//! ```
//!
//! No top-level `<set-uuid>/` directory: the manifest carries the
//! set id, so nesting it as a path component would be redundant. A
//! fixed root layout also makes `import` validation much simpler —
//! reject anything outside this exact set of paths.
//!
//! ## Manifest
//!
//! ```json
//! {
//!   "format": "cosmix-mds-set-export",
//!   "format_version": 1,
//!   "set_id": "<uuid>",
//!   "schema_version": 1,
//!   "exported_at": "2026-05-03T14:23:00Z",
//!   "item_count": 123,
//!   "blob_count": 45
//! }
//! ```
//!
//! `format_version` is the export envelope version; `schema_version`
//! is the per-set `data.sqlite` `PRAGMA user_version`. They evolve
//! independently — the envelope can change (compression, alternate
//! layouts) without touching the data schema, and the schema can
//! migrate without changing the envelope shape.
//!
//! ## Lock discipline
//!
//! The caller (`SqliteCasMds::export_set`) holds the per-set Mutex
//! for the duration. That gives a quiescent snapshot — no concurrent
//! delivery transaction can append rows to `item` between our
//! distinct-hash enumeration and our CAS-read pass. `blobs.sqlite`
//! is not touched.
//!
//! ## Failure modes
//!
//! - Missing CAS file referenced by an item row: fail the export
//!   with `BlobNotFound`, unlink the partial tarball. Never produce
//!   a half-baked archive — re-run `rebuild-index`/`verify` first.
//! - I/O failure mid-write: unlink the partial.
//! - Snapshot via `VACUUM INTO`: produces a compact, WAL-free copy
//!   of `data.sqlite`. We escape the path the same way `ATTACH
//!   DATABASE` does because SQLite does not bind path parameters.
//!
//! ## Atomicity
//!
//! Write to `<dest>.partial`; fsync; rename to `<dest>`. The rename
//! is atomic on the same filesystem, which is the only sensible
//! place to write an export anyway. A crash mid-export leaves the
//! `.partial` for an operator to investigate or delete; the real
//! `<dest>` either exists complete or does not exist.

use crate::blob;
use crate::error::{Error, Result};
use crate::types::{ExportReport, SetId};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Stable on-disk format identifier embedded in `manifest.format`.
/// Importers MUST verify this string matches before trusting any
/// other field — it's how we distinguish a Cosmix MDS export from
/// some other tarball that happened to land at this path.
pub const FORMAT_TAG: &str = "cosmix-mds-set-export";

/// Wire format version of the export envelope (the manifest +
/// archive layout). Bumped if the layout, manifest fields, or the
/// "no `<set-uuid>/` root" decision changes. Independent of
/// `data.sqlite` `PRAGMA user_version` (`schema_version` in the
/// manifest), which evolves with the per-set schema.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub(crate) struct Manifest {
    pub format: &'static str,
    pub format_version: u32,
    pub set_id: String,
    pub schema_version: u32,
    pub exported_at: String,
    pub item_count: u64,
    pub blob_count: u64,
}

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    Error::Other(format!("{prefix}: {e}"))
}

/// Export `set_id`'s `data.sqlite` + referenced blobs to a tar
/// archive at `dest`. Caller (`SqliteCasMds::export_set`) holds
/// the per-set Mutex backing `conn` for the entire call.
pub(crate) fn export_set(
    conn: &Connection,
    blobs_root: &Path,
    set_id: SetId,
    dest: &Path,
) -> Result<ExportReport> {
    let started = Instant::now();

    let parent = dest.parent().ok_or_else(|| {
        Error::Other(format!(
            "export destination {} has no parent directory",
            dest.display()
        ))
    })?;
    if !parent.as_os_str().is_empty() && !parent.exists() {
        return Err(Error::Other(format!(
            "export destination parent {} does not exist",
            parent.display()
        )));
    }

    // Enumerate item count + distinct blob hashes from the per-set
    // data.sqlite. Hashes go in a BTreeSet so the tar layout is
    // deterministic across exports — useful for diff/golden tests
    // and for reproducible-build-style operator workflows.
    let item_count: u64 = conn
        .query_row("SELECT COUNT(*) FROM item", params![], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|e| map_sql_err("count item", e))? as u64;
    let mut hashes: BTreeSet<String> = BTreeSet::new();
    {
        let mut stmt = conn
            .prepare("SELECT DISTINCT blob_hash FROM item")
            .map_err(|e| map_sql_err("prepare distinct blob_hash", e))?;
        let rows = stmt
            .query_map(params![], |r| r.get::<_, String>(0))
            .map_err(|e| map_sql_err("query distinct blob_hash", e))?;
        for row in rows {
            hashes.insert(row.map_err(|e| map_sql_err("row distinct blob_hash", e))?);
        }
    }
    let blob_count = hashes.len() as u64;

    let schema_version: u32 = conn
        .query_row("PRAGMA user_version", params![], |r| r.get::<_, i64>(0))
        .map_err(|e| map_sql_err("PRAGMA user_version", e))? as u32;

    let manifest = Manifest {
        format: FORMAT_TAG,
        format_version: FORMAT_VERSION,
        set_id: set_id.0.to_string(),
        schema_version,
        exported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        item_count,
        blob_count,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| Error::Other(format!("serialize manifest: {e}")))?;

    // Snapshot data.sqlite via VACUUM INTO. This produces a clean,
    // WAL-free, compacted copy at a sibling path. Holding the
    // per-set Mutex means no writes are in flight, but plain file
    // copy of data.sqlite would still miss WAL contents on a busy
    // db. VACUUM INTO is the rusqlite-supported way to get an
    // atomic point-in-time snapshot.
    let snap_path: PathBuf = sibling_tmp_path(dest, "data-snap");
    if snap_path.exists() {
        let _ = fs::remove_file(&snap_path);
    }
    let snap_str = snap_path.to_str().ok_or_else(|| {
        Error::Other(format!(
            "snapshot path {} is not valid UTF-8",
            snap_path.display()
        ))
    })?;
    // SQLite does not bind path parameters for VACUUM INTO; embed
    // with single-quote escaping (same pattern as ATTACH DATABASE
    // in store::attach_blobs_db).
    let escaped = snap_str.replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}';"))
        .map_err(|e| map_sql_err("VACUUM INTO snapshot", e))?;

    // Now write the tarball. On any failure below, unlink both the
    // partial tarball and the snapshot so we never leave a torn
    // archive next to the operator's chosen path.
    let partial = sibling_tmp_path(dest, "partial");
    if partial.exists() {
        let _ = fs::remove_file(&partial);
    }
    let result = write_tarball(&partial, &manifest_bytes, &snap_path, blobs_root, &hashes);
    let _ = fs::remove_file(&snap_path);
    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }

    let bytes_written = fs::metadata(&partial).map_err(Error::Io)?.len();

    fs::rename(&partial, dest).map_err(|e| {
        let _ = fs::remove_file(&partial);
        Error::Io(e)
    })?;

    Ok(ExportReport {
        set_id,
        item_count,
        blob_count,
        bytes_written,
        duration: started.elapsed(),
    })
}

fn sibling_tmp_path(dest: &Path, suffix: &str) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    let mut tmp_name = name;
    tmp_name.push(".");
    tmp_name.push(suffix);
    parent.join(tmp_name)
}

fn write_tarball(
    partial: &Path,
    manifest_bytes: &[u8],
    snap_path: &Path,
    blobs_root: &Path,
    hashes: &BTreeSet<String>,
) -> Result<()> {
    let file = File::create(partial).map_err(Error::Io)?;
    let mut tar = tar::Builder::new(file);
    // Tar headers carry uid/gid/mtime by default; for a portable
    // operator-facing archive we don't want operator account
    // metadata to bleed into the file. Builder defaults are fine
    // for our use; we set sane file modes per-entry.

    append_bytes(&mut tar, "manifest.json", manifest_bytes)?;
    append_path(&mut tar, "data.sqlite", snap_path)?;

    for hash in hashes {
        // hashes came from item.blob_hash — they're trusted lowercase
        // 64-char hex (delivery path enforces it). Defensive recheck
        // here so a corrupt row can't push us into bogus path
        // territory; the cost is one parse per blob.
        let parsed = blob::from_hex(hash).ok_or_else(|| {
            Error::Other(format!(
                "item.blob_hash {hash:?} is not valid 64-char lowercase hex"
            ))
        })?;
        let cas_path = blob::blob_path(blobs_root, &parsed);
        if !cas_path.exists() {
            return Err(Error::BlobNotFound(hash.clone()));
        }
        let entry = format!("blobs/{}/{}/{}", &hash[0..2], &hash[2..4], hash);
        append_path(&mut tar, &entry, &cas_path)?;
    }

    // Finalize the tar (writes the two-block end marker) and
    // fsync the underlying file before the rename so a crash
    // can't promote a torn write to dest.
    let mut file = tar.into_inner().map_err(Error::Io)?;
    file.flush().map_err(Error::Io)?;
    file.sync_all().map_err(Error::Io)?;
    Ok(())
}

fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, path, bytes)
        .map_err(Error::Io)?;
    Ok(())
}

fn append_path<W: Write>(tar: &mut tar::Builder<W>, path: &str, src: &Path) -> Result<()> {
    let mut f = File::open(src).map_err(Error::Io)?;
    let len = f.metadata().map_err(Error::Io)?.len();
    let mut header = tar::Header::new_gnu();
    header.set_size(len);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, path, &mut f)
        .map_err(Error::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_tmp_path_appends_suffix_in_same_dir() {
        let dest = Path::new("/var/backup/mds-018f.tar");
        let tmp = sibling_tmp_path(dest, "partial");
        assert_eq!(tmp, Path::new("/var/backup/mds-018f.tar.partial"));
    }

    #[test]
    fn manifest_round_trips_via_serde() {
        let m = Manifest {
            format: FORMAT_TAG,
            format_version: FORMAT_VERSION,
            set_id: "01900000-0000-7000-8000-000000000000".to_string(),
            schema_version: 1,
            exported_at: "2026-05-03T00:00:00Z".to_string(),
            item_count: 42,
            blob_count: 17,
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["format"], FORMAT_TAG);
        assert_eq!(v["format_version"], 1);
        assert_eq!(v["item_count"], 42);
    }
}
