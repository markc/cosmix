-- cosmix-mds v1 schema for blobs.sqlite (box-wide derived index).
-- Verbatim from _doc/2026-04-29-cosmix-mds-spec.md §Blob-index schema.

PRAGMA application_id = 0x62696478;        -- 'bidx'
PRAGMA user_version   = 1;

CREATE TABLE blob (
    hash         TEXT PRIMARY KEY,
    size_bytes   INTEGER NOT NULL,
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    refcount     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE blob_ref (
    hash       TEXT NOT NULL,
    set_id     TEXT NOT NULL,
    item_id    TEXT NOT NULL,
    PRIMARY KEY (hash, set_id, item_id)
);
CREATE INDEX idx_blob_ref_set  ON blob_ref (set_id);
CREATE INDEX idx_blob_ref_hash ON blob_ref (hash);

CREATE TABLE blob_verify (
    hash             TEXT PRIMARY KEY,
    last_verified_at INTEGER NOT NULL,
    status           INTEGER NOT NULL
);

CREATE TABLE refcount_pending (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    op         INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    set_id     TEXT NOT NULL,
    item_id    TEXT NOT NULL,
    queued_at  INTEGER NOT NULL
);

CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
