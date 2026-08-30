-- cosmix-mds v1.3 schema additions for data.sqlite (per-set metadata).
-- Plan: src/_doc/planned/jmap-mds-migration.md Phase 5 Task 5.4a.
--
-- Additive only — extends `mail_envelopes` (v1.2 §A) with three
-- recipient-list columns so JMAP `Email/get` and `EmailSubmission/set`
-- auto-detect (RFC 8621 §4.1.2 / §7.1.2) can carry the full RFC 5322
-- envelope shape without falling back to the legacy `db::email` row:
--
--   cc_addrs       — JSON array of RFC 5322 `Cc` recipients
--   bcc_addrs      — JSON array of RFC 5322 `Bcc` recipients
--   reply_to_addrs — JSON array of RFC 5322 `Reply-To` addresses
--
-- All three default to `'[]'` so v1.2-era rows project as empty
-- arrays after migration (they predate substrate-side capture of
-- these headers and the original blob bytes are the only place the
-- values can still be recovered from — substrate does not backfill
-- to avoid re-parsing every CAS body).

PRAGMA user_version = 4;

ALTER TABLE mail_envelopes ADD COLUMN cc_addrs       TEXT NOT NULL DEFAULT '[]';
ALTER TABLE mail_envelopes ADD COLUMN bcc_addrs      TEXT NOT NULL DEFAULT '[]';
ALTER TABLE mail_envelopes ADD COLUMN reply_to_addrs TEXT NOT NULL DEFAULT '[]';
