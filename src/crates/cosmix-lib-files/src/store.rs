//! The rusqlite-backed corpus index (feature `sqlite`).
//!
//! One `index.db` per corpus, a 100%-rebuildable PROJECTION of the markdown files
//! on disk (invariant #1) — the daemon stores it in its state dir, outside the
//! synced tree, so `rm index.db && rebuild` is always safe. This module owns:
//!
//! - migration: `application_id` magic + `PRAGMA user_version`, applying
//!   [`crate::schema::INDEX_SCHEMA_V1`] (the cosmix-mds discipline);
//! - the monotonic per-corpus **modseq allocator** (`corpus_state` + `RETURNING`,
//!   the single point of truth, invariant #9);
//! - the **change stream** (`corpus_change`, queried `changes_since`);
//! - **upsert / tombstone / rename** keyed by `rel_path` (id is a non-clobbering
//!   indexed attribute, so a copy never overwrites the original — review BLOCKER-2).
//!
//! Mutating methods take an explicit `now_ms` so the store reads no clock: the
//! daemon controls time, and a rebuild from the same records + timestamps is
//! byte-reproducible. modseq and ids cross Bus as strings elsewhere (f64), but are
//! native `i64`/`TEXT` here.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{FilesError, Result};
use crate::schema;

/// Positional params for a doc INSERT/UPDATE: `?1` rowid (UPDATE's WHERE; unused by
/// INSERT), `?2..?18` the record fields, `?19` modseq, `?20` last_indexed. A macro
/// (not a fn) so each expansion borrows the live locals at its own call site.
macro_rules! doc_params {
    ($rowid:expr, $rec:expr, $modseq:expr, $now:expr) => {
        params![
            $rowid,
            $rec.rel_path,
            $rec.id,
            $rec.content_hash,
            $rec.title,
            $rec.slug,
            $rec.content_type,
            $rec.created,
            $rec.raw_header,
            $rec.frontmatter_json,
            $rec.tags_json,
            $rec.category,
            $rec.word_count,
            $rec.size_bytes,
            $rec.mtime,
            $rec.parse_warnings,
            $rec.dup_of,
            $rec.unmanaged as i64,
            $modseq,
            $now,
        ]
    };
}

/// A single corpus index.
pub struct Store {
    conn: Connection,
    corpus_id: String,
}

/// The change kind recorded for a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A new (or resurrected-from-tombstone) managed file.
    Added,
    /// An existing live path whose projection changed.
    Changed,
    /// A live path tombstoned (soft delete, invariant #8).
    Removed,
    /// A live path moved (id stable), preserving its row identity.
    Renamed,
}

impl ChangeKind {
    fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "ADDED",
            ChangeKind::Changed => "CHANGED",
            ChangeKind::Removed => "REMOVED",
            ChangeKind::Renamed => "RENAMED",
        }
    }
}

/// The projected fields of one document — the daemon computes this from a file
/// (frontmatter parse + hash) and hands it to the store. The store manages the
/// derived `modseq` / `deleted` / `last_indexed` columns itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocRecord {
    /// Path within the corpus root (the projection key).
    pub rel_path: String,
    /// Frontmatter id, or `None` for an unmanaged (id-less) file.
    pub id: Option<String>,
    /// BLAKE3 hex of the whole file.
    pub content_hash: String,
    /// Hot parsed fields.
    pub title: Option<String>,
    /// URL slug, if any.
    pub slug: Option<String>,
    /// Content type (spec | doc | journal | note | page | ...).
    pub content_type: Option<String>,
    /// Authored `created` value, verbatim.
    pub created: Option<String>,
    /// The verbatim authored header block (audit; not for reconstruction).
    pub raw_header: String,
    /// All parsed headers as a JSON object.
    pub frontmatter_json: String,
    /// Denormalized tags facet (JSON array text).
    pub tags_json: Option<String>,
    /// Denormalized category facet.
    pub category: Option<String>,
    /// Derived word count.
    pub word_count: Option<i64>,
    /// File size in bytes.
    pub size_bytes: Option<i64>,
    /// Filesystem mtime (epoch).
    pub mtime: Option<i64>,
    /// Ingest warnings JSON, if any.
    pub parse_warnings: Option<String>,
    /// Set when this file duplicates another's id (a copy); else `None`.
    pub dup_of: Option<String>,
    /// `true` for an id-less file a passive scan must not mint.
    pub unmanaged: bool,
}

/// One row of the change stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    /// The corpus modseq at which the change happened (the cursor value).
    pub modseq: i64,
    /// The affected doc's surrogate rowid (stable across id-less/unmanaged rows).
    pub doc_rowid: i64,
    /// The affected doc's id, if it has one.
    pub doc_id: Option<String>,
    /// The affected doc's current path (the new path for a `RENAMED`).
    pub rel_path: String,
    /// The prior path — only set for `RENAMED` (the path to purge downstream).
    pub old_rel_path: Option<String>,
    /// `ADDED` | `CHANGED` | `REMOVED` | `RENAMED`.
    pub kind: String,
    /// Epoch ms the change was recorded.
    pub changed_at: i64,
}

/// Aggregate corpus counts for the read-only props surface (`filesd.props`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusStats {
    /// Live (non-tombstoned) documents.
    pub files: u64,
    /// Live id-less documents (a passive scan won't mint).
    pub unmanaged: u64,
    /// Recorded Syncthing conflict files.
    pub conflicts: u64,
    /// Sum of live document `size_bytes`.
    pub bytes: u64,
    /// Current per-corpus modseq tip.
    pub modseq: i64,
}

/// The doc columns the read verbs project, in the order [`row_to_json`] expects.
const DOC_COLS: &str = "rel_path, id, title, slug, content_type, created, tags_json, \
     category, word_count, size_bytes, mtime, dup_of, unmanaged, modseq";

/// Map a `SELECT DOC_COLS` row to a JSON object. `modseq` is a STRING (f64 precision),
/// `tags` is parsed back to JSON when present, `unmanaged` is a bool.
fn row_to_json(r: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let rel_path: String = r.get(0)?;
    let id: Option<String> = r.get(1)?;
    let title: Option<String> = r.get(2)?;
    let slug: Option<String> = r.get(3)?;
    let content_type: Option<String> = r.get(4)?;
    let created: Option<String> = r.get(5)?;
    let tags_json: Option<String> = r.get(6)?;
    let category: Option<String> = r.get(7)?;
    let word_count: Option<i64> = r.get(8)?;
    let size_bytes: Option<i64> = r.get(9)?;
    let mtime: Option<i64> = r.get(10)?;
    let dup_of: Option<String> = r.get(11)?;
    let unmanaged: i64 = r.get(12)?;
    let modseq: i64 = r.get(13)?;
    let tags = tags_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    Ok(serde_json::json!({
        "rel_path": rel_path,
        "id": id,
        "title": title,
        "slug": slug,
        "content_type": content_type,
        "created": created,
        "tags": tags,
        "category": category,
        "word_count": word_count,
        "size_bytes": size_bytes,
        "mtime": mtime,
        "dup_of": dup_of,
        "unmanaged": unmanaged != 0,
        "modseq": modseq.to_string(),
    }))
}

impl Store {
    /// Open (creating + migrating if new) the index at `path` for `corpus_id`.
    pub fn open(path: impl AsRef<Path>, corpus_id: &str) -> Result<Self> {
        Self::init(Connection::open(path)?, corpus_id)
    }

    /// Open an in-memory index (tests / ephemeral).
    pub fn open_in_memory(corpus_id: &str) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, corpus_id)
    }

    fn init(conn: Connection, corpus_id: &str) -> Result<Self> {
        let mut conn = conn;
        // Pragmas BEFORE table creation (auto_vacuum only takes effect pre-creation,
        // and journal_mode/WAL cannot run inside a transaction).
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\
             PRAGMA synchronous=NORMAL;\
             PRAGMA foreign_keys=ON;\
             PRAGMA busy_timeout=5000;\
             PRAGMA auto_vacuum=INCREMENTAL;",
        )?;
        Self::migrate(&mut conn)?;
        // Enforce one-corpus-per-db: the allocator and the corpus_change PK both
        // assume a single corpus, so reject a db already bound to a different one.
        let other: Option<String> = conn
            .query_row(
                "SELECT corpus_id FROM corpus_state WHERE corpus_id <> ?1 LIMIT 1",
                params![corpus_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = other {
            return Err(FilesError::Db(format!(
                "index db belongs to corpus {other:?}, not {corpus_id:?}"
            )));
        }
        conn.execute(
            "INSERT OR IGNORE INTO corpus_state(corpus_id, modseq) VALUES (?1, 0)",
            params![corpus_id],
        )?;
        Ok(Store {
            conn,
            corpus_id: corpus_id.to_string(),
        })
    }

    fn migrate(conn: &mut Connection) -> Result<()> {
        let app_id: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if app_id == schema::APPLICATION_ID {
            let uv: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            return if uv == schema::USER_VERSION {
                Ok(())
            } else {
                Err(FilesError::Db(format!(
                    "unsupported corpus index version {uv} (expected {})",
                    schema::USER_VERSION
                )))
            };
        }
        if app_id != 0 {
            return Err(FilesError::Db(format!(
                "not a corpus index db (application_id {app_id:#010x})"
            )));
        }
        // Fresh db: create schema + stamp identity atomically, so a crash mid-create
        // leaves an empty (application_id == 0) db that re-migrates cleanly rather
        // than a half-built one that errors on "table already exists".
        let tx = conn.transaction()?;
        tx.execute_batch(schema::INDEX_SCHEMA_V1)?;
        tx.pragma_update(None, "application_id", schema::APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", schema::USER_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    /// The corpus's current modseq tip (0 if no writes yet).
    pub fn current_modseq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT modseq FROM corpus_state WHERE corpus_id = ?1",
                params![self.corpus_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// All live (non-tombstoned) docs as reconcile entries, ordered by path.
    pub fn live_entries(&self) -> Result<Vec<crate::diff::Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, id, content_hash, dup_of FROM doc \
             WHERE deleted = 0 ORDER BY rel_path ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::diff::Entry {
                rel_path: r.get(0)?,
                id: r.get(1)?,
                content_hash: r.get(2)?,
                dup_of: r.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Changes strictly after `since`, ascending, capped at `limit`.
    pub fn changes_since(&self, since: i64, limit: i64) -> Result<Vec<ChangeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT modseq, doc_rowid, doc_id, rel_path, old_rel_path, kind, changed_at \
             FROM corpus_change WHERE modseq > ?1 ORDER BY modseq ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            Ok(ChangeRow {
                modseq: r.get(0)?,
                doc_rowid: r.get(1)?,
                doc_id: r.get(2)?,
                rel_path: r.get(3)?,
                old_rel_path: r.get(4)?,
                kind: r.get(5)?,
                changed_at: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Aggregate corpus counts for the read-only props surface.
    pub fn corpus_stats(&self) -> Result<CorpusStats> {
        let (files, unmanaged, bytes): (i64, i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(unmanaged), 0), coalesce(sum(size_bytes), 0) \
             FROM doc WHERE deleted = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let conflicts: i64 = self
            .conn
            .query_row("SELECT count(*) FROM conflict", [], |r| r.get(0))?;
        Ok(CorpusStats {
            files: files.max(0) as u64,
            unmanaged: unmanaged.max(0) as u64,
            conflicts: conflicts.max(0) as u64,
            bytes: bytes.max(0) as u64,
            modseq: self.current_modseq()?,
        })
    }

    /// Count of live (non-tombstoned) docs.
    pub fn live_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM doc WHERE deleted = 0", [], |r| {
                r.get(0)
            })?)
    }

    /// Paths of all tombstoned (soft-deleted) docs — used by a full indexd resync to
    /// purge entries that may be stale in indexd (e.g. a delete dropped during an
    /// indexd outage while the broker stayed up).
    pub fn tombstoned_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM doc WHERE deleted = 1 ORDER BY rel_path ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Live docs as JSON metadata rows, ordered by path, for `filesd.list`.
    pub fn list_docs(&self, limit: i64, offset: i64) -> Result<Vec<serde_json::Value>> {
        let sql = format!(
            "SELECT {DOC_COLS} FROM doc WHERE deleted = 0 ORDER BY rel_path ASC LIMIT ?1 OFFSET ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit, offset], row_to_json)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// One live doc's metadata by frontmatter id (prefers the keeper), for `filesd.read`.
    pub fn get_doc_by_id(&self, id: &str) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {DOC_COLS} FROM doc WHERE id = ?1 AND deleted = 0 \
             ORDER BY (dup_of IS NOT NULL) ASC, rel_path ASC LIMIT 1"
        );
        self.conn
            .query_row(&sql, params![id], row_to_json)
            .optional()
            .map_err(Into::into)
    }

    /// One live doc's metadata by path, for `filesd.read`.
    pub fn get_doc_by_path(&self, rel_path: &str) -> Result<Option<serde_json::Value>> {
        let sql = format!("SELECT {DOC_COLS} FROM doc WHERE rel_path = ?1 AND deleted = 0 LIMIT 1");
        self.conn
            .query_row(&sql, params![rel_path], row_to_json)
            .optional()
            .map_err(Into::into)
    }

    /// Substring search over title / path / frontmatter, for `filesd.search`. (A
    /// metadata `LIKE` for now; the FTS5 `doc_fts` table is reserved for a later slice.)
    pub fn search_docs(&self, query: &str, limit: i64) -> Result<Vec<serde_json::Value>> {
        let like = format!("%{query}%");
        let sql = format!(
            "SELECT {DOC_COLS} FROM doc WHERE deleted = 0 AND \
             (title LIKE ?1 OR rel_path LIKE ?1 OR frontmatter_json LIKE ?1) \
             ORDER BY rel_path ASC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![like, limit], row_to_json)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Insert or update the doc at `rec.rel_path`, allocate a modseq, and emit a
    /// change. A new (or tombstoned) path yields `Added`; an existing live path
    /// yields `Changed`. No id-uniqueness is enforced — two live files may share an
    /// id (a copy); `rel_path` is the key, and the diff flags copies via `dup_of`.
    pub fn upsert(&mut self, rec: &DocRecord, now_ms: i64) -> Result<(i64, ChangeKind)> {
        let corpus_id = self.corpus_id.clone();
        let tx = self.conn.transaction()?;

        let existing: Option<(i64, bool)> = tx
            .query_row(
                "SELECT doc_rowid, deleted FROM doc WHERE rel_path = ?1 \
                 ORDER BY deleted ASC LIMIT 1",
                params![rec.rel_path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .optional()?;

        let modseq = alloc_modseq(&tx, &corpus_id)?;
        let (doc_rowid, kind) = match existing {
            Some((rowid, was_deleted)) => {
                tx.execute(
                    "UPDATE doc SET rel_path=?2, id=?3, content_hash=?4, title=?5, slug=?6, \
                     content_type=?7, created=?8, raw_header=?9, frontmatter_json=?10, \
                     tags_json=?11, category=?12, word_count=?13, size_bytes=?14, mtime=?15, \
                     parse_warnings=?16, dup_of=?17, unmanaged=?18, modseq=?19, deleted=0, \
                     deleted_at=NULL, last_indexed=?20 WHERE doc_rowid=?1",
                    doc_params!(rowid, rec, modseq, now_ms),
                )?;
                (
                    rowid,
                    if was_deleted {
                        ChangeKind::Added
                    } else {
                        ChangeKind::Changed
                    },
                )
            }
            None => {
                tx.execute(
                    "INSERT INTO doc (rel_path, id, content_hash, title, slug, content_type, \
                     created, raw_header, frontmatter_json, tags_json, category, word_count, \
                     size_bytes, mtime, parse_warnings, dup_of, unmanaged, modseq, deleted, \
                     deleted_at, last_indexed) \
                     VALUES (?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,0,NULL,?20)",
                    doc_params!(0_i64, rec, modseq, now_ms),
                )?;
                (tx.last_insert_rowid(), ChangeKind::Added)
            }
        };

        emit_change(
            &tx,
            modseq,
            doc_rowid,
            rec.id.as_deref(),
            &rec.rel_path,
            None,
            kind,
            now_ms,
        )?;
        tx.commit()?;
        Ok((modseq, kind))
    }

    /// Soft-delete the live doc at `rel_path` (tombstone, invariant #8). Returns the
    /// allocated modseq, or `None` if there was no live row (no change emitted).
    pub fn tombstone(&mut self, rel_path: &str, now_ms: i64) -> Result<Option<i64>> {
        let corpus_id = self.corpus_id.clone();
        let tx = self.conn.transaction()?;
        let row: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT doc_rowid, id FROM doc WHERE rel_path = ?1 AND deleted = 0",
                params![rel_path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((rowid, id)) = row else {
            return Ok(None);
        };
        let modseq = alloc_modseq(&tx, &corpus_id)?;
        tx.execute(
            "UPDATE doc SET deleted=1, deleted_at=?2, modseq=?3 WHERE doc_rowid=?1",
            params![rowid, now_ms, modseq],
        )?;
        emit_change(
            &tx,
            modseq,
            rowid,
            id.as_deref(),
            rel_path,
            None,
            ChangeKind::Removed,
            now_ms,
        )?;
        tx.commit()?;
        Ok(Some(modseq))
    }

    /// Move a live doc from `from` to `to` (id stable), preserving its row identity
    /// (`doc_rowid`) and applying the new projection `rec`. Returns the modseq, or
    /// `None` if there was no live row at `from`.
    pub fn rename(
        &mut self,
        from: &str,
        to: &str,
        rec: &DocRecord,
        now_ms: i64,
    ) -> Result<Option<i64>> {
        let corpus_id = self.corpus_id.clone();
        let tx = self.conn.transaction()?;
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT doc_rowid FROM doc WHERE rel_path = ?1 AND deleted = 0",
                params![from],
                |r| r.get(0),
            )
            .optional()?;
        let Some(rowid) = rowid else {
            return Ok(None);
        };
        // Clear any stale tombstone already occupying `to`: the partial live-unique
        // index does not constrain tombstoned rows, so without this the moved (live)
        // row and a dead tombstone would both sit at `to` and wedge a later upsert.
        // It cannot touch the row being moved (still live at `from`).
        tx.execute(
            "DELETE FROM doc WHERE rel_path = ?1 AND deleted = 1",
            params![to],
        )?;
        let modseq = alloc_modseq(&tx, &corpus_id)?;
        // Bind with rel_path forced to `to` (rec may or may not carry it).
        let mut moved = rec.clone();
        moved.rel_path = to.to_string();
        tx.execute(
            "UPDATE doc SET rel_path=?2, id=?3, content_hash=?4, title=?5, slug=?6, \
             content_type=?7, created=?8, raw_header=?9, frontmatter_json=?10, tags_json=?11, \
             category=?12, word_count=?13, size_bytes=?14, mtime=?15, parse_warnings=?16, \
             dup_of=?17, unmanaged=?18, modseq=?19, last_indexed=?20 WHERE doc_rowid=?1",
            doc_params!(rowid, moved, modseq, now_ms),
        )?;
        emit_change(
            &tx,
            modseq,
            rowid,
            moved.id.as_deref(),
            to,
            Some(from),
            ChangeKind::Renamed,
            now_ms,
        )?;
        tx.commit()?;
        Ok(Some(modseq))
    }

    /// Wipe the projection (doc + derived tables) and reset modseq — the
    /// destructive half of a full "rebuild from disk". The caller replays records.
    pub fn wipe(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM corpus_change; DELETE FROM link; DELETE FROM attachment; \
             DELETE FROM conflict; DELETE FROM doc_fts; DELETE FROM doc;",
        )?;
        tx.execute("UPDATE corpus_state SET modseq = 0", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Record (idempotently) a Syncthing whole-file conflict sighting (Q6). Conflicts
    /// are surfaced data, not document changes — they allocate no modseq.
    pub fn record_conflict(&self, rel_path: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conflict (rel_path, base_id, detected_at) VALUES (?1, NULL, ?2) \
             ON CONFLICT(rel_path) DO NOTHING",
            params![rel_path, now_ms],
        )?;
        Ok(())
    }

    /// All currently-recorded conflict file paths.
    pub fn conflict_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM conflict ORDER BY rel_path ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop a conflict row (the file was resolved/removed — reconcile-down, Q6).
    pub fn remove_conflict(&self, rel_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM conflict WHERE rel_path = ?1",
            params![rel_path],
        )?;
        Ok(())
    }
}

/// The single-point-of-truth modseq allocator: bump and return inside the caller's
/// transaction (copy of the cosmix-mds `set_state` pattern).
fn alloc_modseq(tx: &rusqlite::Transaction, corpus_id: &str) -> Result<i64> {
    tx.execute(
        "INSERT OR IGNORE INTO corpus_state(corpus_id, modseq) VALUES (?1, 0)",
        params![corpus_id],
    )?;
    Ok(tx.query_row(
        "UPDATE corpus_state SET modseq = modseq + 1 WHERE corpus_id = ?1 RETURNING modseq",
        params![corpus_id],
        |r| r.get(0),
    )?)
}

#[allow(clippy::too_many_arguments)]
fn emit_change(
    tx: &rusqlite::Transaction,
    modseq: i64,
    doc_rowid: i64,
    doc_id: Option<&str>,
    rel_path: &str,
    old_rel_path: Option<&str>,
    kind: ChangeKind,
    now_ms: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO corpus_change (modseq, doc_rowid, doc_id, rel_path, old_rel_path, kind, changed_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![modseq, doc_rowid, doc_id, rel_path, old_rel_path, kind.as_str(), now_ms],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(rel_path: &str, id: Option<&str>, hash: &str) -> DocRecord {
        DocRecord {
            rel_path: rel_path.to_string(),
            id: id.map(str::to_string),
            content_hash: hash.to_string(),
            raw_header: "---\n---\n".to_string(),
            frontmatter_json: "{}".to_string(),
            ..Default::default()
        }
    }

    fn store() -> Store {
        Store::open_in_memory("notes").unwrap()
    }

    fn rowid_of(s: &Store, rel_path: &str) -> Option<i64> {
        s.conn
            .query_row(
                "SELECT doc_rowid FROM doc WHERE rel_path = ?1",
                params![rel_path],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn migrate_sets_identity() {
        let s = store();
        let app: i32 = s
            .conn
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .unwrap();
        let uv: i32 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(app, schema::APPLICATION_ID);
        assert_eq!(uv, schema::USER_VERSION);
        // doc table exists and is empty.
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM doc", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn add_then_change_bumps_modseq() {
        let mut s = store();
        assert_eq!(
            s.upsert(&rec("a.md", Some("id-a"), "h1"), 10).unwrap(),
            (1, ChangeKind::Added)
        );
        assert_eq!(
            s.upsert(&rec("a.md", Some("id-a"), "h2"), 20).unwrap(),
            (2, ChangeKind::Changed)
        );
        assert_eq!(s.current_modseq().unwrap(), 2);
        assert_eq!(s.live_entries().unwrap().len(), 1);
    }

    #[test]
    fn tombstone_removes_and_emits() {
        let mut s = store();
        s.upsert(&rec("a.md", Some("id-a"), "h1"), 10).unwrap();
        assert_eq!(s.tombstone("a.md", 20).unwrap(), Some(2));
        assert!(s.live_entries().unwrap().is_empty());
        let changes = s.changes_since(0, 100).unwrap();
        assert_eq!(changes.last().unwrap().kind, "REMOVED");
    }

    #[test]
    fn tombstone_absent_is_none_and_allocates_nothing() {
        let mut s = store();
        assert_eq!(s.tombstone("nope.md", 10).unwrap(), None);
        assert_eq!(s.current_modseq().unwrap(), 0);
    }

    #[test]
    fn resurrect_after_tombstone_is_added() {
        let mut s = store();
        s.upsert(&rec("a.md", Some("id-a"), "h1"), 10).unwrap();
        s.tombstone("a.md", 20).unwrap();
        let (modseq, kind) = s.upsert(&rec("a.md", Some("id-a"), "h3"), 30).unwrap();
        assert_eq!(kind, ChangeKind::Added);
        assert_eq!(modseq, 3);
        assert_eq!(s.live_entries().unwrap().len(), 1);
    }

    #[test]
    fn rename_preserves_rowid() {
        let mut s = store();
        s.upsert(&rec("old.md", Some("id-x"), "h1"), 10).unwrap();
        let before = rowid_of(&s, "old.md").unwrap();
        assert_eq!(
            s.rename("old.md", "new.md", &rec("new.md", Some("id-x"), "h2"), 20)
                .unwrap(),
            Some(2)
        );
        assert_eq!(rowid_of(&s, "old.md"), None);
        assert_eq!(rowid_of(&s, "new.md"), Some(before));
        let paths: Vec<_> = s
            .live_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.rel_path)
            .collect();
        assert_eq!(paths, vec!["new.md".to_string()]);
    }

    #[test]
    fn changes_carry_paths_for_indexd_push() {
        let mut s = store();
        s.upsert(&rec("a.md", Some("id-a"), "h1"), 1).unwrap();
        s.rename("a.md", "b.md", &rec("b.md", Some("id-a"), "h1"), 2)
            .unwrap();
        s.tombstone("b.md", 3).unwrap();
        let ch = s.changes_since(0, 100).unwrap();
        assert_eq!(
            (
                ch[0].kind.as_str(),
                ch[0].rel_path.as_str(),
                ch[0].old_rel_path.as_deref()
            ),
            ("ADDED", "a.md", None)
        );
        assert_eq!(
            (
                ch[1].kind.as_str(),
                ch[1].rel_path.as_str(),
                ch[1].old_rel_path.as_deref()
            ),
            ("RENAMED", "b.md", Some("a.md"))
        );
        assert_eq!(
            (ch[2].kind.as_str(), ch[2].rel_path.as_str()),
            ("REMOVED", "b.md")
        );
    }

    #[test]
    fn changes_since_pagination() {
        let mut s = store();
        s.upsert(&rec("a.md", Some("id-a"), "h"), 1).unwrap();
        s.upsert(&rec("b.md", Some("id-b"), "h"), 2).unwrap();
        s.upsert(&rec("c.md", Some("id-c"), "h"), 3).unwrap();
        assert_eq!(s.changes_since(0, 100).unwrap().len(), 3);
        let after_one = s.changes_since(1, 100).unwrap();
        assert_eq!(after_one.len(), 2);
        assert_eq!(after_one[0].modseq, 2);
        assert_eq!(s.changes_since(0, 2).unwrap().len(), 2);
    }

    #[test]
    fn modseq_strictly_monotonic_across_mixed_ops() {
        let mut s = store();
        s.upsert(&rec("a.md", Some("id-a"), "h1"), 1).unwrap();
        s.upsert(&rec("b.md", Some("id-b"), "h1"), 2).unwrap();
        s.upsert(&rec("a.md", Some("id-a"), "h2"), 3).unwrap();
        s.tombstone("b.md", 4).unwrap();
        s.upsert(&rec("c.md", Some("id-c"), "h1"), 5).unwrap();
        let seqs: Vec<i64> = s
            .changes_since(0, 100)
            .unwrap()
            .iter()
            .map(|c| c.modseq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn rebuild_from_same_records_is_reproducible() {
        let records = [
            rec("a.md", Some("id-a"), "h1"),
            rec("b.md", Some("id-b"), "h2"),
            rec("c.md", Some("id-c"), "h3"),
        ];
        let mut a = store();
        for r in &records {
            a.upsert(r, 100).unwrap();
        }
        let mut b = Store::open_in_memory("notes").unwrap();
        for r in &records {
            b.upsert(r, 100).unwrap();
        }
        assert_eq!(a.live_entries().unwrap(), b.live_entries().unwrap());
        assert_eq!(a.current_modseq().unwrap(), b.current_modseq().unwrap());
    }

    #[test]
    fn two_live_rows_may_share_an_id() {
        // The store enforces no id-uniqueness: a copy (two live files, same id) is
        // real and must persist without error. rel_path is the projection key, so the
        // original is never clobbered (review BLOCKER-2); the diff designates a keeper
        // and flags the rest via `dup_of` (informational).
        let mut s = store();
        s.upsert(&rec("a.md", Some("dup"), "h1"), 1).unwrap();
        let mut copy = rec("b.md", Some("dup"), "h1");
        copy.dup_of = Some("a.md".to_string());
        s.upsert(&copy, 2).unwrap(); // no error
        assert_eq!(s.live_entries().unwrap().len(), 2);
    }

    #[test]
    fn rename_onto_tombstoned_path_clears_stale_and_stays_updatable() {
        let mut s = store();
        s.upsert(&rec("x.md", Some("id-x"), "h1"), 1).unwrap(); // R1 at x.md
        s.tombstone("x.md", 2).unwrap(); // R1 tombstoned, still occupies x.md
        s.upsert(&rec("y.md", Some("id-y"), "h1"), 3).unwrap(); // R2 at y.md
        // Move R2 onto the tombstoned path; the stale tombstone must be cleared.
        assert_eq!(
            s.rename("y.md", "x.md", &rec("x.md", Some("id-y"), "h2"), 4)
                .unwrap(),
            Some(4)
        );
        // Exactly one row at x.md (no lingering tombstone), and it stays updatable.
        let total: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM doc WHERE rel_path = 'x.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1);
        let (_, kind) = s.upsert(&rec("x.md", Some("id-y"), "h3"), 5).unwrap();
        assert_eq!(kind, ChangeKind::Changed);
        assert_eq!(s.live_entries().unwrap().len(), 1);
    }

    #[test]
    fn open_rejects_foreign_corpus_id() {
        let dir = std::env::temp_dir().join(format!("cosmix_files_store_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.db");
        drop(Store::open(&path, "alpha").unwrap());
        assert!(matches!(Store::open(&path, "beta"), Err(FilesError::Db(_))));
        // The same corpus reopens fine.
        drop(Store::open(&path, "alpha").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unmanaged_idless_doc_stored() {
        let mut s = store();
        let mut r = rec("draft.md", None, "h1");
        r.unmanaged = true;
        s.upsert(&r, 1).unwrap();
        let entries = s.live_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, None);
    }

    #[test]
    fn read_helpers_project_count_and_search() {
        let mut s = store();
        let mut a = rec("a.md", Some("id-a"), "h1");
        a.title = Some("Alpha Notes".to_string());
        a.tags_json = Some("[\"x\",\"y\"]".to_string());
        a.size_bytes = Some(100);
        s.upsert(&a, 1).unwrap();
        let mut b = rec("b.md", Some("id-b"), "h2");
        b.title = Some("Beta".to_string());
        b.unmanaged = false;
        s.upsert(&b, 2).unwrap();

        // corpus_stats
        let st = s.corpus_stats().unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.bytes, 100);
        assert_eq!(st.modseq, 2);

        // list_docs + count
        assert_eq!(s.live_count().unwrap(), 2);
        let listed = s.list_docs(10, 0).unwrap();
        assert_eq!(listed.len(), 2);
        // modseq is a STRING; tags parsed back to a JSON array.
        assert!(listed[0]["modseq"].is_string());
        assert_eq!(listed[0]["tags"], serde_json::json!(["x", "y"]));

        // get_doc_by_id / by_path
        let doc = s.get_doc_by_id("id-a").unwrap().unwrap();
        assert_eq!(doc["rel_path"], "a.md");
        assert_eq!(doc["title"], "Alpha Notes");
        assert!(s.get_doc_by_id("nope").unwrap().is_none());
        assert_eq!(s.get_doc_by_path("b.md").unwrap().unwrap()["id"], "id-b");

        // search (substring over title/path)
        let hits = s.search_docs("Alpha", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["rel_path"], "a.md");
        assert_eq!(s.search_docs("zzz", 10).unwrap().len(), 0);
    }

    #[test]
    fn conflict_record_list_remove_is_idempotent() {
        let s = store();
        s.record_conflict("a.sync-conflict-x.md", 1).unwrap();
        s.record_conflict("a.sync-conflict-x.md", 2).unwrap(); // idempotent
        s.record_conflict("b.sync-conflict-y.md", 3).unwrap();
        assert_eq!(
            s.conflict_paths().unwrap(),
            vec![
                "a.sync-conflict-x.md".to_string(),
                "b.sync-conflict-y.md".to_string()
            ]
        );
        s.remove_conflict("a.sync-conflict-x.md").unwrap();
        assert_eq!(
            s.conflict_paths().unwrap(),
            vec!["b.sync-conflict-y.md".to_string()]
        );
    }
}
