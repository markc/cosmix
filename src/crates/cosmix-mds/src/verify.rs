//! Bitrot detection sweep.
//!
//! Walks blobs (Full / Since(SystemTime) / Container), recomputes
//! BLAKE3 from the on-disk CAS file, and writes the outcome to
//! `blobs.sqlite.blob_verify`. Per-blob mismatches are surfaced
//! through an `on_mismatch` callback so the cosmix-feature build
//! can emit `mds.verify.failed` over Bus without the verify core
//! depending on the Bus layer.
//!
//! Status codes mirror the SQL column:
//!   0 = ok, 1 = hash mismatch (bytes present but wrong), 2 = missing.
//!
//! For `Since(t)`, the candidate set is "blobs whose newest
//! `blob_verify.last_verified_at` is older than `t`, plus blobs that
//! have never been verified." That matches the operational reading:
//! "re-verify anything not checked since T."
//!
//! For `Container(cid)`, the verifier needs the union of blob hashes
//! referenced by items in that container across every set. Container
//! ids are globally unique UUIDv7, but `data.sqlite` is sharded per
//! set, so the store layer hands the verifier a closure that walks
//! each per-set connection and unions the matched hashes.

use crate::blob;
use crate::error::{Error, Result};
use crate::types::{ContainerId, VerifyReport, VerifyScope};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) const STATUS_OK: i64 = 0;
pub(crate) const STATUS_HASH_MISMATCH: i64 = 1;
pub(crate) const STATUS_MISSING: i64 = 2;

/// Per-blob mismatch surfaced to the caller. `status` is `1` (bytes
/// hashed wrong) or `2` (file missing); never `0`. The store layer
/// translates this into the `mds.verify.failed` Bus event under the
/// cosmix feature.
#[derive(Debug, Clone)]
pub struct VerifyMismatch {
    pub hash: String,
    pub status: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ms_from(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    Error::Other(format!("{prefix}: {e}"))
}

/// Resolve `Full` / `Since(t)` candidate hashes from `blobs.sqlite`.
/// Container scope is *not* handled here: it requires per-set
/// `data.sqlite` connections and resolving it inside this function
/// would force the caller to hold the blobs.sqlite lock while
/// taking per-set Mutexes — the inverse of `delete_set`'s set→blobs
/// order. The store layer resolves Container hashes separately and
/// passes them to `run`.
pub(crate) fn list_for_scope(blobs_conn: &Connection, scope: &VerifyScope) -> Result<Vec<String>> {
    match scope {
        VerifyScope::Full => list_all(blobs_conn),
        VerifyScope::Since(t) => list_since(blobs_conn, ms_from(*t)),
        VerifyScope::Container(_) => Err(Error::Other(
            "verify::list_for_scope: Container scope must be \
             resolved by the store layer to keep set→blobs lock \
             ordering"
                .into(),
        )),
    }
}

/// Walk `hashes`, recompute BLAKE3 from CAS, record each result, and
/// return a `VerifyReport`. `record` lets the store take and release
/// the fair write gate around each short `blob_verify` upsert rather
/// than blocking all writers for the full filesystem sweep.
pub(crate) fn run<R, M>(
    blobs_root: &Path,
    hashes: &[String],
    mut record: R,
    mut on_mismatch: M,
) -> Result<VerifyReport>
where
    R: FnMut(&str, i64, i64) -> Result<()>,
    M: FnMut(&VerifyMismatch),
{
    let started = Instant::now();

    let mut blobs_checked: u64 = 0;
    let mut mismatches: u64 = 0;
    let mut mismatches_hash: u64 = 0;
    let mut mismatches_missing: u64 = 0;

    for h in hashes {
        let parsed = match blob::from_hex(h) {
            Some(p) => p,
            None => {
                // A hash that survived a write into `blob.hash` must
                // be 64 valid hex chars. If we hit a bad row, refuse
                // to silently skip — it means something has corrupted
                // the row and the operator should know.
                return Err(Error::Other(format!(
                    "verify: invalid hex in blob.hash = {h:?}"
                )));
            }
        };
        let path = blob::blob_path(blobs_root, &parsed);
        let status = match std::fs::read(&path) {
            Ok(bytes) => {
                let actual = blob::hash_bytes(&bytes);
                if &blob::hex(&actual) == h {
                    STATUS_OK
                } else {
                    STATUS_HASH_MISMATCH
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => STATUS_MISSING,
            Err(e) => return Err(Error::Io(e)),
        };
        record(h, now_ms(), status)?;
        blobs_checked += 1;
        if status != STATUS_OK {
            mismatches += 1;
            match status {
                STATUS_HASH_MISMATCH => mismatches_hash += 1,
                STATUS_MISSING => mismatches_missing += 1,
                _ => {} // STATUS_OK is filtered above; defensive fallthrough
            }
            on_mismatch(&VerifyMismatch {
                hash: h.clone(),
                status,
            });
        }
    }

    Ok(VerifyReport {
        blobs_checked,
        mismatches,
        mismatches_hash,
        mismatches_missing,
        duration: started.elapsed(),
    })
}

fn list_all(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT hash FROM blob")
        .map_err(|e| map_sql_err("prepare verify list_all", e))?;
    let rows = stmt
        .query_map(params![], |r| r.get::<_, String>(0))
        .map_err(|e| map_sql_err("query verify list_all", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_sql_err("row verify list_all", e))?);
    }
    Ok(out)
}

fn list_since(conn: &Connection, since_ms: i64) -> Result<Vec<String>> {
    // Hashes that have never been verified, OR whose last verify
    // predates `since_ms`. LEFT JOIN keeps unverified rows in the
    // candidate set; the WHERE clause filters out only the recently
    // verified ones.
    let mut stmt = conn
        .prepare(
            "SELECT b.hash FROM blob b \
             LEFT JOIN blob_verify v ON v.hash = b.hash \
             WHERE v.last_verified_at IS NULL OR v.last_verified_at < ?1",
        )
        .map_err(|e| map_sql_err("prepare verify list_since", e))?;
    let rows = stmt
        .query_map(params![since_ms], |r| r.get::<_, String>(0))
        .map_err(|e| map_sql_err("query verify list_since", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_sql_err("row verify list_since", e))?);
    }
    Ok(out)
}

pub(crate) fn upsert_verify(conn: &Connection, hash: &str, now: i64, status: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO blob_verify (hash, last_verified_at, status) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(hash) DO UPDATE SET \
             last_verified_at = excluded.last_verified_at, \
             status           = excluded.status",
        params![hash, now, status],
    )
    .map_err(|e| map_sql_err("upsert blob_verify", e))?;
    Ok(())
}

/// Helper used by the store layer's `Container` scope: given an
/// already-open per-set `data.sqlite` connection, return the distinct
/// blob hashes referenced by items in `cid`. Returns an empty vector
/// if the set has no membership rows for that container — that's the
/// common case across the N-1 sets that don't own this container.
pub(crate) fn container_hashes_in_set(
    set_conn: &Connection,
    cid: &ContainerId,
) -> Result<Vec<String>> {
    let mut stmt = set_conn
        .prepare(
            "SELECT DISTINCT i.blob_hash \
             FROM membership m \
             JOIN item i ON i.id = m.item_id \
             WHERE m.container_id = ?1",
        )
        .map_err(|e| map_sql_err("prepare verify container_hashes", e))?;
    let rows = stmt
        .query_map(params![cid.0.to_string()], |r| r.get::<_, String>(0))
        .map_err(|e| map_sql_err("query verify container_hashes", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_sql_err("row verify container_hashes", e))?);
    }
    Ok(out)
}
