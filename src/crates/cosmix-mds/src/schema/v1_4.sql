-- cosmix-mds v1.4 schema additions for data.sqlite (per-set metadata).
-- Spec: _doc/planned/mailbox-changes-substrate.md (rev 1, 2026-05-14).
--
-- Adds one table: container_change_set — the durable account-wide
-- container lifecycle stream that powers JMAP Mailbox/changes.
-- This migration is schema-only: no writers, no readers. Writes are
-- wired in later Phase 1 commits (MCS-P1-B/C/D). The JMAP read path
-- continues to use db::changelog until Phase 2 cuts it over and
-- retires the legacy table.

PRAGMA user_version = 5;

-- Container lifecycle stream. AUTOINCREMENT is load-bearing, not a
-- default: SQLite's plain INTEGER PRIMARY KEY reuses the high rowid
-- after a retention sweep deletes the tail, breaking the "seq is
-- monotonically non-decreasing" property a paginating JMAP client
-- depends on. AUTOINCREMENT (backed by sqlite_sequence) guarantees
-- strict monotonicity at the cost of permitting gaps — fine because
-- Mailbox/changes paging is `seq > ?` and gaps don't matter.
--
-- `kind` is text (not integer-coded like container_change.kind):
-- container lifecycle events are rare, the storage overhead is
-- negligible, and a text CHECK constraint surfaces a malformed
-- write at INSERT time rather than at the read-side decode in some
-- consumer six months later.
--
-- `payload` is kind-specific JSON and is NOT NULL — every kind has a
-- documented payload shape (below), and a NULL payload would mean a
-- read-side decode crash six months from now. Enforce the invariant
-- at the schema layer before any writers exist; `'{}'` is *not* a
-- valid default because none of the documented payloads are
-- empty-object-equivalent. The shape per kind, from
-- _doc/planned/mailbox-changes-substrate.md §Design — Option B:
--   CONTAINER_CREATED         {name, parent_id?, full_path}
--   CONTAINER_RENAMED         {old_name, new_name, old_parent_id?,
--                              new_parent_id?, old_full_path,
--                              new_full_path, changed_props}
--   CONTAINER_DESTROYED       {name, full_path}
--   CONTAINER_ATTRS_CHANGED   {changed_props, old_values?, new_values?}
--
-- CONTAINER_REPARENTED is deliberately absent — a combined
-- rename-and-reparent is one atomic SQL UPDATE (container.rs
-- rename_container_in_tx sets name and parent_id in one statement),
-- and the stream mirrors that atomicity. The CONTAINER_RENAMED
-- row's `changed_props` discriminates: ["name"], ["parentId"], or
-- ["name", "parentId"].
CREATE TABLE container_change_set (
    container_change_set_seq  INTEGER PRIMARY KEY AUTOINCREMENT,
    container_id              TEXT    NOT NULL,
    kind                      TEXT    NOT NULL,
    payload                   TEXT    NOT NULL,
    changed_at                INTEGER NOT NULL,
    CHECK (kind IN (
        'CONTAINER_CREATED',
        'CONTAINER_RENAMED',
        'CONTAINER_DESTROYED',
        'CONTAINER_ATTRS_CHANGED'
    ))
);
CREATE INDEX container_change_set_container_idx
    ON container_change_set (container_id, container_change_set_seq);
