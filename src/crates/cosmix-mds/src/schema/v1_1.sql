-- cosmix-mds v1.1 schema additions for data.sqlite (per-set metadata).
-- Spec: _doc/2026-05-02-cosmix-mds-spec-v1_1-amendment.md (rev 4).
-- Plan: _doc/2026-04-29-cosmix-mds-plan.md §"Phase 8 — v1.1 amendment work".
--
-- Additive only — no v1 backfill, no v1 table changes. The v1.1
-- step bumps user_version to 2 and adds six tables:
--
--   §2  set_state             singleton allocator for set_change_seq
--   §2  set_change            set-monotonic change stream
--   §1  blob_refs             JMAP non-owning aliases over the CAS
--   §4b mail_retrain_outbox   bayesian retrain outbox
--   §6  mail_threads          JMAP thread membership
--   §5  mail_search           FTS5 search projection (sync writes)

PRAGMA user_version = 2;

-- §2: set-monotonic counter. data.sqlite is per-set so this is
-- effectively a singleton, but the PK matches the spec's allocator
-- pattern (UPDATE ... WHERE set_id = ? RETURNING).
CREATE TABLE set_state (
    set_id          TEXT    PRIMARY KEY,
    set_change_seq  INTEGER NOT NULL DEFAULT 0
);

-- §2: set-wide change stream. PK is the allocated seq itself, so
-- per-membership keyword fan-out produces N rows in one transaction
-- (one allocation per row). Moves emit two rows: REMOVED on src,
-- ADDED on dest. MOVED is not a kind here — that's a notifier-only
-- correlation event (ContainerEvent::ItemMoved), §4a.
CREATE TABLE set_change (
    set_change_seq        INTEGER PRIMARY KEY,
    container_id          TEXT    NOT NULL,
    container_change_seq  INTEGER NOT NULL,
    item_id               TEXT    NOT NULL,
    kind                  TEXT    NOT NULL,
    seq                   INTEGER NOT NULL,
    changed_at            INTEGER NOT NULL,
    CHECK (kind IN ('ADDED', 'METADATA_CHANGED', 'REMOVED'))
);
CREATE INDEX set_change_container_idx
    ON set_change (container_id, container_change_seq);

-- §1: JMAP blob_refs (non-owning aliases). Does NOT keep the
-- underlying CAS blob alive on its own. `temp_item_id` is set when
-- the ref points at an upload-staging temp item; the staging worker
-- removes the temp item at expiry, after which core MDS GC reclaims
-- the blob.
CREATE TABLE blob_refs (
    blob_id      TEXT PRIMARY KEY,
    account_id   INTEGER NOT NULL,
    blob_hash    TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NULL,
    temp_item_id TEXT    NULL,
    CHECK ((temp_item_id IS NULL) = (expires_at IS NULL))
);
CREATE INDEX blob_refs_hash_idx    ON blob_refs (account_id, blob_hash);
CREATE INDEX blob_refs_expires_idx ON blob_refs (expires_at)
    WHERE expires_at IS NOT NULL;

-- §4b: bayesian retrain outbox. Idempotent on (stamp_id, label) so
-- at-least-once delivery from the worker is safe (record_label is
-- already idempotent on stamp_id).
CREATE TABLE mail_retrain_outbox (
    stamp_id    TEXT    NOT NULL,
    account_id  INTEGER NOT NULL,
    item_id     TEXT    NOT NULL,
    label       TEXT    NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT    NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (stamp_id, label),
    CHECK (label IN ('junk', 'ham'))
);
CREATE INDEX mail_retrain_outbox_attempts_idx
    ON mail_retrain_outbox (attempts, created_at);

-- §6: thread membership, populated at store_message time using
-- References + In-Reply-To + Subject normalization (JMAP §4.1.5).
CREATE TABLE mail_threads (
    item_id    TEXT    NOT NULL,
    account_id INTEGER NOT NULL,
    thread_id  TEXT    NOT NULL,
    PRIMARY KEY (item_id, account_id)
);
CREATE INDEX mail_threads_thread_idx ON mail_threads (account_id, thread_id);

-- §5: mail_search FTS5 sidecar. Inline plain-text only for v1.1 —
-- attachment extraction is explicitly out of scope. content='' marks
-- this as a contentless FTS5 table; the projection is written
-- synchronously by MailStore mutations inside the same with_set_tx.
--
-- NOTE: v1.6 (`v1_6.sql`) drops and re-creates this table with two
-- additional FTS5 options (contentless_unindexed=1,
-- contentless_delete=1) that are load-bearing for the Phase 8e gate-2
-- reap pattern. This historical v1.1 definition is preserved
-- byte-for-byte; existing v2+ databases get the option upgrade via
-- the v1.6 migration step, not by re-running this file.
CREATE VIRTUAL TABLE mail_search USING fts5(
    item_id           UNINDEXED,
    account_id        UNINDEXED,
    headers,
    subject,
    body_text,
    normalized_addrs,
    content=''
);
