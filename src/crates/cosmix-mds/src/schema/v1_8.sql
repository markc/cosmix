-- cosmix-mds v1.8 schema migration for data.sqlite (per-set metadata).
-- Close maild item-keyed sidecar reap gap on item delete.
--
-- `remove_membership_in_tx` (container.rs) drops the `item` row on the
-- last-membership path (`if remaining == 0 { ... DELETE FROM item }`).
-- Three maild item-keyed sidecars were NOT reaped by that delete:
--
--   * mail_retrain_outbox / mail_threads — added in v1.1 with a bare
--     `item_id TEXT NOT NULL` and no FK, so dangling rows accumulated.
--   * mail_search — FTS5 contentless (v1.6), can't carry an FK at all.
--
-- `mail_envelopes` (v1.2) already has `REFERENCES item(id) ON DELETE
-- CASCADE` — the precedent this migration extends to the other two
-- ordinary tables, plus an AFTER DELETE trigger for the FTS5 table.
--
-- Adding an FK to an existing table requires the SQLite table-rebuild
-- dance (CREATE _v2 / INSERT SELECT / DROP / RENAME). The INSERT
-- backfill is WHERE-filtered to drop any pre-existing dangling rows,
-- so the new FK is satisfied for every inserted row even though
-- `PRAGMA foreign_keys` is ON during the migration transaction (the
-- runner's set_pragmas enables it before any step runs; a connection-
-- level PRAGMA cannot be toggled mid-transaction). The dropped old
-- tables are referenced by nothing, so DROP is also FK-safe.
--
-- mail_retrain_outbox / mail_threads keep their exact v1.1 column set,
-- types, PRIMARY KEY, CHECK and existing index — v1.2..v1.7 never
-- altered either table. The deltas are the FK on item_id and, for
-- mail_retrain_outbox, one extra index backing that FK's parent-delete
-- child probe (mail_threads is already covered by its composite PK).

PRAGMA user_version = 9;

-- Reap historical dangling FTS5 rows (no FK could have protected them).
DELETE FROM mail_search
 WHERE NOT EXISTS (SELECT 1 FROM item WHERE item.id = mail_search.item_id);

CREATE TABLE mail_retrain_outbox_v2 (
    stamp_id    TEXT    NOT NULL,
    account_id  INTEGER NOT NULL,
    item_id     TEXT    NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    label       TEXT    NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT    NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (stamp_id, label),
    CHECK (label IN ('junk', 'ham'))
);
-- Preserve rowid: the retrain worker drains ORDER BY rowid ASC.
INSERT INTO mail_retrain_outbox_v2
    (rowid, stamp_id, account_id, item_id, label, attempts, last_error, created_at)
SELECT rowid, stamp_id, account_id, item_id, label, attempts, last_error, created_at
  FROM mail_retrain_outbox
 WHERE EXISTS (SELECT 1 FROM item WHERE item.id = mail_retrain_outbox.item_id);
DROP TABLE mail_retrain_outbox;
ALTER TABLE mail_retrain_outbox_v2 RENAME TO mail_retrain_outbox;
CREATE INDEX mail_retrain_outbox_attempts_idx ON mail_retrain_outbox (attempts, created_at);
-- Back the new FK: SQLite enforces a parent delete by probing child
-- rows on the FK's `from` column. mail_threads is already covered by
-- its `(item_id, account_id)` PRIMARY KEY, but mail_retrain_outbox's
-- only keys lead with stamp_id / attempts, so without an item_id-
-- leading index a bulk mailbox delete with a large pending outbox
-- degrades to repeated full scans.
CREATE INDEX mail_retrain_outbox_item_idx ON mail_retrain_outbox (item_id);

CREATE TABLE mail_threads_v2 (
    item_id    TEXT    NOT NULL REFERENCES item(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL,
    thread_id  TEXT    NOT NULL,
    PRIMARY KEY (item_id, account_id)
);
INSERT INTO mail_threads_v2 (item_id, account_id, thread_id)
SELECT item_id, account_id, thread_id
  FROM mail_threads
 WHERE EXISTS (SELECT 1 FROM item WHERE item.id = mail_threads.item_id);
DROP TABLE mail_threads;
ALTER TABLE mail_threads_v2 RENAME TO mail_threads;
CREATE INDEX mail_threads_thread_idx ON mail_threads (account_id, thread_id);

DROP TRIGGER IF EXISTS mail_search_item_ad;
CREATE TRIGGER mail_search_item_ad
AFTER DELETE ON item
BEGIN
    DELETE FROM mail_search WHERE item_id = OLD.id;
END;
