-- cosmix-mds v1 schema for data.sqlite (per-set metadata).
-- Verbatim from _doc/2026-04-29-cosmix-mds-spec.md §Account/container schema.

PRAGMA application_id = 0x636D6473;        -- 'cmds'
PRAGMA user_version   = 1;

CREATE TABLE container (
    id              TEXT PRIMARY KEY,
    parent_id       TEXT REFERENCES container(id) ON DELETE RESTRICT,
    name            TEXT NOT NULL,
    seq_validity    INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER) * 1000),
    next_seq        INTEGER NOT NULL DEFAULT 1,
    change_seq      INTEGER NOT NULL DEFAULT 1,
    exists_count    INTEGER NOT NULL DEFAULT 0,
    unread_count    INTEGER NOT NULL DEFAULT 0,
    attrs           TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    UNIQUE (parent_id, name)
);
-- SQLite UNIQUE treats NULL as distinct, so two root containers
-- with the same name would otherwise both insert. The intent of
-- the UNIQUE clause is "no duplicate name under the same parent
-- including the no-parent case"; a partial index covers it.
CREATE UNIQUE INDEX idx_container_root_name
    ON container (name) WHERE parent_id IS NULL;

CREATE TABLE item (
    id            TEXT PRIMARY KEY,
    blob_hash     TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    received_at   INTEGER NOT NULL,
    cache_blob    BLOB,
    cache_version TEXT
);
CREATE INDEX idx_item_blob ON item (blob_hash);

CREATE TABLE membership (
    item_id       TEXT NOT NULL REFERENCES item(id)      ON DELETE CASCADE,
    container_id  TEXT NOT NULL REFERENCES container(id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    change_seq    INTEGER NOT NULL,
    flags         INTEGER NOT NULL DEFAULT 0,
    tags          TEXT,
    added_at      INTEGER NOT NULL,
    PRIMARY KEY (item_id, container_id)
);
CREATE UNIQUE INDEX idx_mbr_container_seq        ON membership (container_id, seq);
CREATE        INDEX idx_mbr_container_change_seq ON membership (container_id, change_seq);
CREATE        INDEX idx_mbr_container_added_at   ON membership (container_id, added_at);

CREATE TABLE container_change (
    container_id  TEXT NOT NULL,
    change_seq    INTEGER NOT NULL,
    kind          INTEGER NOT NULL,
    seq           INTEGER NOT NULL,
    item_id       TEXT,
    changed_at    INTEGER NOT NULL,
    PRIMARY KEY (container_id, change_seq),
    CHECK (kind IN (0, 1, 2))
);

CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
