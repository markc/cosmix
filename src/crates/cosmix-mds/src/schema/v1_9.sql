-- cosmix-mds v1.9 schema migration for data.sqlite (per-set metadata).
-- Record a microsecond enqueue instant on mail_retrain_outbox rows.
--
-- maild trains a message inline (a JMAP move, maild.bayesian.train /
-- untrain) and also queues IMAP moves in this outbox. An inline label
-- supersedes the queued rows that are OLDER than the inline event, and
-- only those: a row queued after the inline event began is a newer user
-- action and must still drain. `created_at` is whole seconds, too coarse
-- to order two events in the same second, so rows now also carry
-- `created_us`: microseconds since the Unix epoch from a per-process
-- strictly increasing clock.
--
-- NULL on rows that existed before this migration. Readers treat NULL as
-- older than any event, which is true: every such row was queued before
-- this build started.

PRAGMA user_version = 10;

ALTER TABLE mail_retrain_outbox ADD COLUMN created_us INTEGER;
