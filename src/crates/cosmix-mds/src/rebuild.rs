//! Rebuild `blobs.sqlite` from per-set `data.sqlite` + CAS.
//!
//! Phase 5 deliverable backing the `cosmix-mds rebuild-index`
//! operator command. Per spec §Invariants 8, the box-wide
//! `blobs.sqlite` is a *derived* index over the per-set
//! `data.sqlite` truths — rebuildable at any time. This module
//! does that rebuild.
//!
//! Lock discipline: callers MUST hold every per-set Mutex *and*
//! the `blobs.sqlite` Mutex for the duration of the call. The
//! per-set ATTACH'd `blobs_db` connections are how delivery
//! transactions write into `blob`/`blob_ref` — if a delivery
//! commits while we're rebuilding, that delivery's rows are
//! erased. The store layer's `rebuild_index` takes the per-set
//! locks in sorted UUID order *before* taking blobs, which is
//! consistent with `delete_set`'s set→blobs ordering (delete_set
//! holds one set + blobs; rebuild_index holds the superset, so
//! cannot deadlock with it).
//!
//! `blob_verify` is preserved across the rebuild because the
//! last-verify timestamp and status are not derivable from
//! data.sqlite or CAS — they are operationally meaningful state
//! that should not vanish on a maintenance command.
//! `refcount_pending` is dormant in v1; we clear it for hygiene.
//!
//! Orphan counting: after the rebuild commits, snapshot the rebuilt
//! `blob.hash` set and walk CAS directly — every valid hex-named
//! file whose hash is *not* in that set is an orphan. The earlier
//! `cas_count - blobs_indexed` subtraction shape silently
//! cancelled when a referenced CAS file was missing AND an orphan
//! existed at the same time (both sides dropped by one, diff = 0,
//! orphan unreported); per-file membership avoids that cancellation.
//! Files under `blobs/.tmp/` and entries with invalid names are
//! excluded — those are write-staging or operator debris, not
//! "blobs" in the spec sense.

use crate::blob;
use crate::error::{Error, Result};
use crate::types::{RebuildReport, SetId};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    Error::Other(format!("{prefix}: {e}"))
}

/// Rebuild the derived tables in `blobs.sqlite` from the per-set
/// `data.sqlite` connections in `set_conns`. The caller owns the
/// MutexGuards backing each `&Connection`; this function does not
/// touch the cache and does not acquire any additional locks.
///
/// Sets that were drained mid-iteration (a concurrent `delete_set`
/// completed its tear-down before we acquired its Mutex) are not
/// passed in by the store layer — they're filtered out before this
/// call. `sets_scanned` therefore counts only sets whose connection
/// was live and readable.
pub(crate) fn rebuild_blobs_index(
    blobs_conn: &mut Connection,
    blobs_root: &Path,
    set_conns: &[(SetId, &Connection)],
    started: Instant,
) -> Result<RebuildReport> {
    let tx = blobs_conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN rebuild_index", e))?;

    // Wipe the derived tables. blob_verify is preserved (last
    // verify timestamp + status survive independently of derivation).
    tx.execute("DELETE FROM blob_ref", [])
        .map_err(|e| map_sql_err("delete blob_ref", e))?;
    tx.execute("DELETE FROM blob", [])
        .map_err(|e| map_sql_err("delete blob", e))?;
    tx.execute("DELETE FROM refcount_pending", [])
        .map_err(|e| map_sql_err("delete refcount_pending", e))?;

    let mut sets_scanned: u64 = 0;
    let mut items_indexed: u64 = 0;

    for (set, conn) in set_conns {
        sets_scanned += 1;
        let mut stmt = conn
            .prepare("SELECT id, blob_hash, size_bytes, received_at FROM item")
            .map_err(|e| map_sql_err("prepare item scan", e))?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| map_sql_err("query items", e))?;
        for row in rows {
            let (item_id, hash, size, received) =
                row.map_err(|e| map_sql_err("row item scan", e))?;

            // Upsert the blob row. first_seen tracks the minimum
            // received_at across all referencing items; last_seen
            // the maximum. refcount += 1 per item, matching
            // `add_blob_ref_in_tx`'s "one refcount per item, not
            // per membership" semantics — `blob_ref` PK is
            // (hash, set_id, item_id), so the rebuild produces the
            // exact same row set delivery would have.
            tx.execute(
                "INSERT INTO blob (hash, size_bytes, first_seen, last_seen, refcount) \
                 VALUES (?1, ?2, ?3, ?3, 1) \
                 ON CONFLICT(hash) DO UPDATE SET \
                     first_seen = MIN(first_seen, excluded.first_seen), \
                     last_seen  = MAX(last_seen,  excluded.last_seen), \
                     refcount   = refcount + 1",
                params![hash, size, received],
            )
            .map_err(|e| map_sql_err("upsert blob row", e))?;

            tx.execute(
                "INSERT INTO blob_ref (hash, set_id, item_id) VALUES (?1, ?2, ?3)",
                params![hash, set.0.to_string(), item_id],
            )
            .map_err(|e| map_sql_err("insert blob_ref", e))?;

            items_indexed += 1;
        }
    }

    // Snapshot the rebuilt hash set so the orphan walk can check
    // each CAS file against it directly, rather than subtracting
    // two aggregate counts. The aggregate-subtraction shape is
    // wrong when a referenced CAS file is missing AND an orphan
    // exists at the same time: cas_count and blobs_indexed each
    // drop by one against the naive expectation, the diff lands
    // at zero, and the orphan goes unreported. Per-file membership
    // check avoids the cancellation entirely.
    let mut indexed: HashSet<String> = {
        let mut stmt = tx
            .prepare("SELECT hash FROM blob")
            .map_err(|e| map_sql_err("prepare hash snapshot", e))?;
        let rows = stmt
            .query_map(params![], |r| r.get::<_, String>(0))
            .map_err(|e| map_sql_err("query hash snapshot", e))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row.map_err(|e| map_sql_err("row hash snapshot", e))?);
        }
        out
    };
    let blobs_indexed = indexed.len() as u64;

    tx.commit()
        .map_err(|e| map_sql_err("commit rebuild_index", e))?;

    let orphan_blobs_found = count_orphan_cas_files(blobs_root, &mut indexed)?;

    Ok(RebuildReport {
        sets_scanned,
        items_indexed,
        blobs_indexed,
        orphan_blobs_found,
        duration: started.elapsed(),
    })
}

/// Walk CAS files under `blobs_root` and count any whose hash is
/// NOT in `indexed` — i.e., on disk but unreferenced by any item.
/// Skips `.tmp/` (write-staging) and any entry whose name isn't a
/// valid two-char (shard dir) or 64-char (blob file) lowercase hex
/// string; operator debris under `blobs/` is not a blob in the
/// spec's sense.
///
/// `indexed` is taken `&mut` so this function can drain entries it
/// observes — the caller has no further use for it after the orphan
/// count, and the move-out avoids cloning the hash strings.
fn count_orphan_cas_files(blobs_root: &Path, indexed: &mut HashSet<String>) -> Result<u64> {
    if !blobs_root.exists() {
        return Ok(0);
    }
    let mut orphans: u64 = 0;
    for first in std::fs::read_dir(blobs_root)? {
        let first = first?;
        if !first.file_type()?.is_dir() {
            continue;
        }
        let first_name = first.file_name();
        let first_str = match first_name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !is_two_lower_hex(first_str) {
            // Skips `.tmp` and any other non-shard directory.
            continue;
        }
        for second in std::fs::read_dir(first.path())? {
            let second = second?;
            if !second.file_type()?.is_dir() {
                continue;
            }
            let second_name = second.file_name();
            let second_str = match second_name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if !is_two_lower_hex(second_str) {
                continue;
            }
            for entry in std::fs::read_dir(second.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let s = match name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                if blob::from_hex(s).is_none() {
                    continue;
                }
                if !indexed.remove(s) {
                    // CAS file present, no item references it.
                    orphans += 1;
                }
            }
        }
    }
    Ok(orphans)
}

fn is_two_lower_hex(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_lower_hex_accepts_lower_rejects_upper_and_other() {
        assert!(is_two_lower_hex("00"));
        assert!(is_two_lower_hex("ab"));
        assert!(is_two_lower_hex("ff"));
        assert!(!is_two_lower_hex("AB"));
        assert!(!is_two_lower_hex("a"));
        assert!(!is_two_lower_hex("abc"));
        assert!(!is_two_lower_hex(".tmp"));
        assert!(!is_two_lower_hex("gg"));
    }

    #[test]
    fn count_orphan_cas_files_handles_missing_root() {
        let p = std::path::Path::new("/nonexistent/cosmix-mds-test-root-does-not-exist");
        let mut indexed = HashSet::new();
        assert_eq!(count_orphan_cas_files(p, &mut indexed).unwrap(), 0);
    }
}
