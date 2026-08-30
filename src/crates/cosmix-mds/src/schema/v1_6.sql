-- cosmix-mds v1.6 schema migration for data.sqlite (per-set metadata).
-- Spec: _doc/planned/maild-sidecar-reap.md + maild Phase 8e gate 2
-- (synchronous mail_search projection writes inside with_set_tx).
--
-- DROPs and re-creates the v1.1 `mail_search` FTS5 virtual table so
-- that existing v2+ databases gain two FTS5 options that the v1.1
-- definition omitted. Both options are load-bearing for the gate-2
-- reap pattern (`DELETE FROM mail_search WHERE item_id = ?`):
--
--   * contentless_unindexed=1 (SQLite 3.47+) — lets UNINDEXED columns
--     (item_id, account_id) store readable byte values under
--     content=''. Without it, `WHERE item_id = ?` probes silently
--     match nothing because the column returns NULL on read.
--
--   * contentless_delete=1 (SQLite 3.43+) — enables normal
--     `DELETE FROM mail_search WHERE …` against contentless tables.
--     Without it, the table only accepts the special FTS5 "delete"
--     pragma form, which is incompatible with the trait-level
--     `tx.tx().execute("DELETE …")` shape MailStore uses.
--
-- The bundled libsqlite3-sys ships 3.51.x, well past both floors.
--
-- DROP is safe: mail_search has never carried persistent data prior
-- to Phase 8e gate 2 (no writers existed). Any existing rows on
-- in-flight v2..v5 deployments are projection-only and re-derivable
-- from the underlying email storage; gate 3 (rebuild command) will
-- backfill from `item` regardless.

PRAGMA user_version = 7;

DROP TABLE IF EXISTS mail_search;

CREATE VIRTUAL TABLE mail_search USING fts5(
    item_id           UNINDEXED,
    account_id        UNINDEXED,
    headers,
    subject,
    body_text,
    normalized_addrs,
    content='',
    contentless_unindexed=1,
    contentless_delete=1
);
