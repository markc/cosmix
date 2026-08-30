-- cosmix-mds v1.2 schema additions for data.sqlite (per-set metadata).
-- Plan: src/_doc/planned/jmap-mds-migration.md Phase 1 Tasks 1.6 / 1.7.
--
-- Additive only — no v1 / v1.1 table changes. The v1.2 step bumps
-- user_version to 3 and adds one MailStore-owned sidecar:
--
--   §A  mail_envelopes    JMAP Email/get envelope sidecar
--
-- Rationale for landing this in mds (not maild) schema:
-- pre-existing mail-domain sidecars (`blob_refs`, `mail_retrain_outbox`,
-- `mail_threads`, `mail_search`) already live in v1.1, so the same
-- migration runner owns the table inventory the MailStore adapter
-- depends on.

PRAGMA user_version = 3;

-- §A: per-item envelope sidecar. Populated on `MailStore::create_email`
-- (Phase 1 Task 1.7) and read by `MailStore::get_email` (Task 1.6).
--
-- Field choice tracks JMAP `Email/get` baseline (RFC 8621 §4.1.x):
--   from_addr  — RFC 5322 `From` header (raw addr-spec; no parsing)
--   to_addrs   — JSON array of RFC 5322 `To` recipients
--   subject    — RFC 5322 `Subject` (decoded MIME if needed at write)
--   date_ts    — RFC 5322 `Date` as unix-seconds; origination, not delivery
--   message_id — RFC 5322 `Message-ID` (input to thread normalisation)
--
-- The body lives in CAS via `item.blob_hash`; envelope is a slim
-- projection so JMAP `Email/get` can answer header-only properties
-- without re-parsing the RFC 5322 source on every read.
--
-- ON DELETE CASCADE on `item(id)` matches `membership` and lets the
-- `remove_membership` last-ref cascade reap envelope rows automatically
-- when the underlying item is dropped — no GC bookkeeping needed.
CREATE TABLE mail_envelopes (
    item_id      TEXT PRIMARY KEY REFERENCES item(id) ON DELETE CASCADE,
    from_addr    TEXT NOT NULL,
    to_addrs     TEXT NOT NULL DEFAULT '[]',
    subject      TEXT NOT NULL DEFAULT '',
    date_ts      INTEGER NOT NULL DEFAULT 0,
    message_id   TEXT
);
