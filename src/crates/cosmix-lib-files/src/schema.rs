//! The per-corpus `index.db` schema (design §5) as constants. The daemon
//! (`cosmix-filesd`) executes this via the mds migration discipline
//! (`PRAGMA user_version` + `application_id` magic); this crate stays
//! rusqlite-free so it remains pure-logic and `--no-default-features` testable.
//!
//! Truth direction (invariant #1): this is a 100%-rebuildable PROJECTION of the
//! markdown files on disk. `rm index.db && rebuild` is always safe. The projection
//! keys on `rel_path` (a surrogate rowid PK), with `id` as a NON-unique indexed
//! attribute — so a `cp a.md b.md` that duplicates a frontmatter id neither clobbers
//! the original (rel_path is the key, review BLOCKER-2) nor errors (two live files
//! may share an id; the diff picks a deterministic keeper and flags the rest via
//! `dup_of`).

/// SQLite `application_id` magic for a corpus index ("fils").
pub const APPLICATION_ID: i32 = 0x6669_6c73;

/// Current schema version (`PRAGMA user_version`).
pub const USER_VERSION: i32 = 1;

/// The v1 schema. Apply inside the daemon after setting `application_id` and the
/// WAL / foreign_keys / busy_timeout / auto_vacuum pragmas (mirroring cosmix-mds).
pub const INDEX_SCHEMA_V1: &str = r#"
-- The per-document projection (one row per live or tombstoned corpus file).
-- Keyed on rel_path (surrogate rowid PK); id is a non-clobbering indexed
-- attribute the daemon enforces/repairs.
CREATE TABLE doc (
  doc_rowid        INTEGER PRIMARY KEY,
  rel_path         TEXT NOT NULL,
  id               TEXT,
  content_hash     TEXT NOT NULL,
  title            TEXT,
  slug             TEXT,
  content_type     TEXT,
  created          TEXT,
  raw_header       TEXT NOT NULL,
  frontmatter_json TEXT NOT NULL,
  tags_json        TEXT,
  category         TEXT,
  word_count       INTEGER,
  size_bytes       INTEGER,
  mtime            INTEGER,
  parse_warnings   TEXT,
  dup_of           TEXT,
  unmanaged        INTEGER NOT NULL DEFAULT 0,
  modseq           INTEGER NOT NULL,
  deleted          INTEGER NOT NULL DEFAULT 0,
  deleted_at       INTEGER,
  last_indexed     INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_doc_path_live ON doc(rel_path) WHERE deleted = 0;
-- id is NOT unique: two live files legitimately share an id after a copy / Syncthing
-- duplicate, so the index must represent that rather than forbid it. rel_path is the
-- projection key (idx_doc_path_live); `dup_of` flags the non-keeper copies
-- informationally. This non-unique index just speeds rename/keeper lookups.
CREATE INDEX idx_doc_id ON doc(id) WHERE deleted = 0 AND id IS NOT NULL;
CREATE INDEX idx_doc_modseq ON doc(modseq);
CREATE INDEX idx_doc_type   ON doc(content_type);

-- Singleton modseq allocator (copy mds set_state).
CREATE TABLE corpus_state (
  corpus_id TEXT PRIMARY KEY,
  modseq    INTEGER NOT NULL DEFAULT 0
);

-- Change stream (per-node liveness cursor, NOT a cross-node sync token).
CREATE TABLE corpus_change (
  modseq       INTEGER PRIMARY KEY,
  doc_rowid    INTEGER NOT NULL,
  doc_id       TEXT,
  rel_path     TEXT NOT NULL DEFAULT '',
  old_rel_path TEXT,
  kind         TEXT NOT NULL CHECK (kind IN ('ADDED','CHANGED','REMOVED','RENAMED')),
  changed_at   INTEGER NOT NULL
);

-- Derived link graph (recomputed on reconcile; sqlite-only).
CREATE TABLE link (
  src_id     TEXT NOT NULL,
  target_ref TEXT NOT NULL,
  target_id  TEXT,
  kind       TEXT NOT NULL,
  broken     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_link_src    ON link(src_id);
CREATE INDEX idx_link_target ON link(target_id);

-- Attachment refs into the BLAKE3 CAS.
CREATE TABLE attachment (
  doc_id    TEXT NOT NULL,
  blob_hash TEXT NOT NULL,
  name      TEXT,
  PRIMARY KEY (doc_id, blob_hash)
);

-- Syncthing whole-file conflicts seen on the reconcile walk.
CREATE TABLE conflict (
  rel_path    TEXT PRIMARY KEY,
  base_id     TEXT,
  detected_at INTEGER NOT NULL
);

-- Keyword/exact search (indexd is vector-only; FTS lives here).
-- doc_fts.body is derived, rebuildable, never authoritative.
CREATE VIRTUAL TABLE doc_fts USING fts5(id UNINDEXED, title, body);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_declares_expected_objects() {
        for needle in [
            "CREATE TABLE doc",
            "CREATE TABLE corpus_state",
            "CREATE TABLE corpus_change",
            "CREATE TABLE link",
            "CREATE TABLE attachment",
            "CREATE TABLE conflict",
            "USING fts5",
            "idx_doc_path_live",
        ] {
            assert!(INDEX_SCHEMA_V1.contains(needle), "schema missing {needle}");
        }
        // id is intentionally NOT uniquely indexed (copies share an id).
        assert!(!INDEX_SCHEMA_V1.contains("idx_doc_id_live"));
    }

    #[test]
    fn version_constants() {
        assert_eq!(USER_VERSION, 1);
        assert_eq!(APPLICATION_ID, 0x6669_6c73);
    }
}
