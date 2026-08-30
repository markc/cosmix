-- cosmix-mds v1.7 schema migration for data.sqlite (per-set metadata).
-- Spec: maild Phase 8e gate 4 (Subject-normalization fallback for
-- mail_threads thread_id resolution, per JMAP §4.1.5 + RFC 5256 §2.1).
--
-- Adds an RFC 5256 §2.1 "base subject" column to mail_envelopes plus
-- an index keyed on it. New emails populate normalized_subject at
-- envelope-INSERT time (cosmix-maild MailStore); the threading
-- resolver consults the index after In-Reply-To / References miss so
-- "Re: Hello" can be clustered with an earlier "Hello" even when the
-- reply omits References headers (a common Outlook / mobile-MUA
-- failure mode).
--
-- Pre-migration rows: the ALTER fills existing envelopes with the
-- empty default. That is deliberately benign — pre-migration emails
-- already have established mail_threads.thread_id values via the
-- message-id chain, and the subject fallback only runs for *new*
-- emails whose References resolve to nothing. The fallback skips the
-- index lookup when the normalized form is empty, so empty-default
-- rows never falsely cluster. A future envelope-rebuild admin verb
-- could backfill historical rows if needed; that is out of scope for
-- gate 4.
--
-- Normalization rules (cosmix-maild::mailstore::subject) match RFC
-- 5256 §2.1 verbatim — collapse whitespace, recursively strip
-- subj-leader (blob* refwd ws*), strip subj-trailer (WSP / "(fwd)"),
-- strip [fwd: ... ] wrapper, lowercase. The schema does NOT pin the
-- normalization algorithm; the column stores whatever maild writes.
-- If the normalization spec changes, a maild-side rebuild rewrites
-- the column in place — no schema migration needed.

PRAGMA user_version = 8;

ALTER TABLE mail_envelopes ADD COLUMN normalized_subject TEXT NOT NULL DEFAULT '';

-- Composite index covers BOTH the WHERE filter
-- (normalized_subject = ?) AND the
-- `ORDER BY date_ts DESC, message_id DESC, item_id DESC LIMIT 1`
-- tail used by the cosmix-maild threading fallback (Phase 8e gate 4 /
-- JMAP §4.1.5). With this composite, SQLite can serve the lookup as
-- a reverse-range walk of the index and skip the TEMP B-TREE sort
-- it would otherwise need to resort matching rows before LIMIT 1.
-- The four columns also give the lookup a strict total order even
-- when `mail_envelopes.message_id` is NULL — `item_id` is the
-- per-set PRIMARY KEY of `mail_envelopes`, so it is always present
-- and unique.
--
-- Planner-guarantee scope: the cosmix-maild MailStore provisions
-- one MDS set per account (see `mailstore::setid`), so in this
-- workspace the JOIN's `mt.account_id = ?1` filter is semantically
-- satisfied by every row in `mail_threads` for the open set. SQLite
-- cannot infer that invariant from the schema, so the no-TEMP-sort
-- behavior is the observed plan today, not a planner-level
-- guarantee: under EXPLAIN QUERY PLAN the optimizer currently drives
-- the loop from `mail_envelopes` against the new composite and serves
-- the ORDER BY as a reverse-range walk. If statistics shift or a
-- future change ever points multiple accounts at one set, the planner
-- may legally pick `mail_threads_thread_idx(account_id, thread_id)`
-- first and fall back to a TEMP B-TREE sort — a re-tuning trigger,
-- not a correctness regression.
CREATE INDEX mail_envelopes_normsubj_idx
  ON mail_envelopes (normalized_subject, date_ts, message_id, item_id);
