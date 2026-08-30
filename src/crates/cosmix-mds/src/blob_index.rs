//! `blobs.sqlite` operations: refcount, refcount_pending, blob_verify.
//! Box-wide derived index, rebuildable from data.sqlite + CAS per
//! spec §Invariants 8.
//!
//! Phase 3: writes to `blobs_db.blob` and `blobs_db.blob_ref` happen
//! inside the per-set delivery transaction (the per-set connection
//! has the box-wide blobs.sqlite ATTACH'd as `blobs_db` at open
//! time). One refcount = one item, not one membership: the
//! `(hash, set_id, item_id)` PK on `blob_ref` means copy/move within
//! the same set never touches refcount.

use crate::blob;
use crate::error::{Error, Result};
use crate::types::{BlobHash, GcReport, ItemId, SetId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    Error::Other(format!("{prefix}: {e}"))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Inside an open per-set transaction (with `blobs_db` attached),
/// record a new `(set, item, blob)` reference and bump the blob's
/// refcount. The `blob` row is upserted; only `last_seen` updates on
/// a hit so duplicate-content additions don't reset `first_seen`.
pub(crate) fn add_blob_ref_in_tx(
    tx: &Transaction<'_>,
    set: &SetId,
    item: &ItemId,
    hash: &BlobHash,
    size_bytes: u64,
) -> Result<()> {
    let hex = blob::hex(hash);
    let now = now_ms();
    tx.execute(
        "INSERT INTO blobs_db.blob (hash, size_bytes, first_seen, last_seen, refcount) \
         VALUES (?1, ?2, ?3, ?3, 0) \
         ON CONFLICT(hash) DO UPDATE SET last_seen = excluded.last_seen",
        params![hex, size_bytes as i64, now],
    )
    .map_err(|e| map_sql_err("upsert blob row", e))?;
    tx.execute(
        "UPDATE blobs_db.blob SET refcount = refcount + 1 WHERE hash = ?1",
        params![hex],
    )
    .map_err(|e| map_sql_err("bump blob refcount", e))?;
    tx.execute(
        "INSERT INTO blobs_db.blob_ref (hash, set_id, item_id) VALUES (?1, ?2, ?3)",
        params![hex, set.0.to_string(), item.0.to_string()],
    )
    .map_err(|e| map_sql_err("insert blob_ref", e))?;
    Ok(())
}

/// Inside an open per-set transaction, drop the `(set, item, blob)`
/// reference and decrement the blob's refcount. Errors if the
/// `blob_ref` row does not exist — that is an invariant break, not a
/// no-op.
pub(crate) fn drop_blob_ref_in_tx(
    tx: &Transaction<'_>,
    set: &SetId,
    item: &ItemId,
    hash: &BlobHash,
) -> Result<()> {
    let hex = blob::hex(hash);
    let n = tx
        .execute(
            "DELETE FROM blobs_db.blob_ref \
             WHERE hash = ?1 AND set_id = ?2 AND item_id = ?3",
            params![hex, set.0.to_string(), item.0.to_string()],
        )
        .map_err(|e| map_sql_err("delete blob_ref", e))?;
    if n == 0 {
        return Err(Error::Other(format!(
            "drop_blob_ref_in_tx: blob_ref ({hex}, {}, {}) not present",
            set.0, item.0
        )));
    }
    tx.execute(
        "UPDATE blobs_db.blob SET refcount = refcount - 1 WHERE hash = ?1",
        params![hex],
    )
    .map_err(|e| map_sql_err("decrement blob refcount", e))?;
    Ok(())
}

/// Drop every `blob_ref` row owned by `set` and decrement each
/// affected blob's refcount accordingly. Returns the count of
/// *distinct hashes* whose refcount transitions all the way to zero
/// — that is `delete_set`'s `blob_count_unreffed` headline number,
/// which is intentionally distinct from the row count of deleted
/// blob_refs (a single set can carry many blob_ref rows for the same
/// hash; those collapse into one in the tally).
///
/// Runs as a single `BEGIN IMMEDIATE` on the box-wide blobs.sqlite
/// connection so refcount math is atomic against any concurrent
/// per-set delivery transaction.
pub(crate) fn delete_set_blobs(conn: &mut Connection, set: &SetId) -> Result<u64> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN delete_set_blobs", e))?;

    // Snapshot per-hash impact: how many blob_ref rows for this set
    // and per hash. We need the list materialized because we'll
    // DELETE those rows below.
    let impact: Vec<(String, i64)> = {
        let mut stmt = tx
            .prepare(
                "SELECT hash, COUNT(*) FROM blob_ref \
                 WHERE set_id = ?1 GROUP BY hash",
            )
            .map_err(|e| map_sql_err("prepare impact scan", e))?;
        let rows = stmt
            .query_map(params![set.0.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| map_sql_err("query impact scan", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| map_sql_err("row impact scan", e))?);
        }
        out
    };

    // Distinct-hash tally: only hashes whose entire refcount comes
    // from this set transition to zero. Hashes that another set also
    // pins do not count toward `blob_count_unreffed` even though
    // their refcount drops.
    let mut newly_unreffed: u64 = 0;
    for (hash, in_set) in &impact {
        let rc: Option<i64> = tx
            .query_row(
                "SELECT refcount FROM blob WHERE hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| map_sql_err("read blob.refcount", e))?;
        let rc = rc.ok_or_else(|| {
            Error::Other(format!(
                "delete_set_blobs: blob_ref refers to missing blob row hash={hash}"
            ))
        })?;
        if rc == *in_set {
            newly_unreffed += 1;
        }
    }

    tx.execute(
        "DELETE FROM blob_ref WHERE set_id = ?1",
        params![set.0.to_string()],
    )
    .map_err(|e| map_sql_err("delete blob_refs for set", e))?;

    for (hash, in_set) in &impact {
        tx.execute(
            "UPDATE blob SET refcount = refcount - ?1 WHERE hash = ?2",
            params![in_set, hash],
        )
        .map_err(|e| map_sql_err("decrement blob refcount", e))?;
    }

    tx.commit()
        .map_err(|e| map_sql_err("commit delete_set_blobs", e))?;
    Ok(newly_unreffed)
}

/// Snapshot taken in Pass 1 for each `refcount=0` blob. Pass 2
/// re-reads the live row and compares; a mismatch on either field
/// means a delivery raced and the candidate is no longer collectable
/// in this sweep.
pub(crate) struct Pass1Candidate {
    hash: String,
    size_bytes: i64,
    last_seen: i64,
}

/// Outcome of Pass 1: candidate list + the dormant-table sanity
/// count. The store layer drops the blobs.sqlite lock between Pass 1
/// and Pass 2 so a 60-second quiescence wait does not block
/// `delete_set` (which also locks blobs.sqlite). That's why
/// enumeration is its own function rather than rolled into a
/// single `gc_sweep` call as the earlier draft had it.
pub(crate) struct Pass1Outcome {
    pub candidates: Vec<Pass1Candidate>,
    pub pending_rows_observed: u64,
}

/// Pass 1: enumerate every `blob` row whose refcount is zero, and
/// sanity-check `refcount_pending` (dormant in v1 — non-empty rows
/// are surfaced as a counter and a warn-level log, never fatal).
///
/// The connection is borrowed only for the duration of the read.
/// The caller drops the lock between this and `gc_pass2_apply` so
/// the quiescence wait does not block other writers.
pub(crate) fn gc_pass1(conn: &Connection) -> Result<Pass1Outcome> {
    let pending = pass1_pending_count(conn)?;
    if pending > 0 {
        tracing::warn!(
            target: "cosmix_mds",
            "gc: refcount_pending has {pending} unexpected row(s) — \
             v1 has no producer; check for an out-of-band writer"
        );
    }
    let candidates = pass1_candidates(conn)?;
    Ok(Pass1Outcome {
        candidates,
        pending_rows_observed: pending,
    })
}

/// Pass 2: per-candidate `BEGIN IMMEDIATE`; re-check refcount +
/// `last_seen` + on-disk file presence; if all three still indicate
/// the blob is collectable, unlink the file and delete the row.
///
/// `dry_run = true` performs all checks but skips the unlink and the
/// row delete; the report's counters reflect what *would* have been
/// done. Pass 2 is idempotent across crashes: a process death
/// between unlink and row delete leaves a dangling row that the next
/// sweep's Pass 2 picks up via the file-missing branch and counts as
/// `orphan_rows_swept`.
pub(crate) fn gc_pass2_apply(
    conn: &mut Connection,
    blobs_root: &Path,
    candidates: &[Pass1Candidate],
    dry_run: bool,
    report: &mut GcReport,
) -> Result<()> {
    for cand in candidates {
        pass2_apply(conn, blobs_root, cand, dry_run, report)?;
    }
    Ok(())
}

fn pass1_pending_count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM refcount_pending", params![], |r| {
            r.get(0)
        })
        .map_err(|e| map_sql_err("count refcount_pending", e))?;
    Ok(n as u64)
}

fn pass1_candidates(conn: &Connection) -> Result<Vec<Pass1Candidate>> {
    let mut stmt = conn
        .prepare("SELECT hash, size_bytes, last_seen FROM blob WHERE refcount = 0")
        .map_err(|e| map_sql_err("prepare pass1 candidates", e))?;
    let rows = stmt
        .query_map(params![], |r| {
            Ok(Pass1Candidate {
                hash: r.get::<_, String>(0)?,
                size_bytes: r.get::<_, i64>(1)?,
                last_seen: r.get::<_, i64>(2)?,
            })
        })
        .map_err(|e| map_sql_err("query pass1 candidates", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| map_sql_err("row pass1 candidate", e))?);
    }
    Ok(out)
}

fn pass2_apply(
    conn: &mut Connection,
    blobs_root: &Path,
    cand: &Pass1Candidate,
    dry_run: bool,
    report: &mut GcReport,
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN pass2", e))?;

    // Re-read the live row. If it was deleted between Pass 1 and
    // here (e.g. a previous Pass 2 in this same sweep already
    // collected it; cannot happen with distinct candidates but is
    // cheap to handle), nothing left to do.
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT refcount, last_seen FROM blob WHERE hash = ?1",
            params![cand.hash],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("re-read blob row", e))?;

    let (refcount, last_seen) = match row {
        Some(rr) => rr,
        None => {
            // Row vanished out from under us — count as a sweep but
            // not a deletion (we did not unlink anything here).
            tx.commit()
                .map_err(|e| map_sql_err("commit pass2 nop", e))?;
            return Ok(());
        }
    };

    if refcount > 0 {
        report.skipped_re_referenced += 1;
        tx.commit()
            .map_err(|e| map_sql_err("commit pass2 re-ref skip", e))?;
        return Ok(());
    }
    if last_seen != cand.last_seen {
        report.skipped_re_touched += 1;
        tx.commit()
            .map_err(|e| map_sql_err("commit pass2 touch skip", e))?;
        return Ok(());
    }

    // Resolve the on-disk path. A hash that survived a write into
    // `blob.hash` must be 64 valid hex chars; if `from_hex` fails
    // here something has corrupted the row and we refuse to act.
    let parsed = blob::from_hex(&cand.hash).ok_or_else(|| {
        Error::Other(format!(
            "gc pass2: invalid hex in blob.hash = {:?}",
            cand.hash
        ))
    })?;
    let path = blob::blob_path(blobs_root, &parsed);

    if !path.exists() {
        // Dangling row (post-crash recovery) or never-materialized
        // CAS write. Either way, deleting the row is safe — there
        // is no on-disk file to lose.
        if !dry_run {
            tx.execute(
                "DELETE FROM blob WHERE hash = ?1 AND refcount = 0 \
                 AND last_seen = ?2",
                params![cand.hash, cand.last_seen],
            )
            .map_err(|e| map_sql_err("delete dangling blob row", e))?;
        }
        report.orphan_rows_swept += 1;
        tx.commit()
            .map_err(|e| map_sql_err("commit pass2 orphan", e))?;
        return Ok(());
    }

    // Real candidate: unlink + delete row in a single tx. Order
    // matters for crash recovery: unlink first, then row delete.
    // Pre-row-delete crash leaves the row dangling; the next sweep
    // picks it up via the file-missing branch above.
    if !dry_run {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Raced with another deleter — proceed to row delete.
            }
            Err(e) => {
                return Err(Error::Other(format!(
                    "gc pass2: unlink {} failed: {e}",
                    path.display()
                )));
            }
        }
        tx.execute(
            "DELETE FROM blob WHERE hash = ?1 AND refcount = 0 \
             AND last_seen = ?2",
            params![cand.hash, cand.last_seen],
        )
        .map_err(|e| map_sql_err("delete blob row", e))?;
    }
    tx.commit()
        .map_err(|e| map_sql_err("commit pass2 delete", e))?;

    report.blobs_deleted += 1;
    report.bytes_freed = report.bytes_freed.saturating_add(cand.size_bytes as u64);
    Ok(())
}
