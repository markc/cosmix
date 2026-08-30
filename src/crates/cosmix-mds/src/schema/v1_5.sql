-- cosmix-mds v1.5 schema additions for data.sqlite (per-set metadata).
-- Spec: _doc/planned/mailbox-changes-substrate.md (MCS Phase 3, 2026-05-14).
--
-- Adds per-stream retention floors to set_state. These are the
-- watermarks `prune_changelog` advances when it deletes the oldest
-- entries from container_change_set / set_change; clients whose
-- since-state is below the floor get a `cannotCalculateChanges`-style
-- rejection from the mailstore read path (see
-- cosmix-maild::mailstore::mailbox_changes).
--
-- DEFAULT 0 is the "never pruned" state: floor=0 means no row has
-- been deleted, so any non-negative since-state is still answerable.
-- NOT NULL is load-bearing: a NULL would force read-side three-state
-- logic; the durable storage layer guarantees the column is present
-- on every set_state row.

PRAGMA user_version = 6;

-- Floor for container_change_set (the lifecycle half of the paired
-- mailbox-changes token). Equal to the highest container_change_set_seq
-- that has been pruned; rows with seq > floor are still present.
-- A client's since-state lifecycle half is stale iff since < floor.
ALTER TABLE set_state
    ADD COLUMN container_change_set_floor INTEGER NOT NULL DEFAULT 0;

-- Floor for set_change (the per-set item-event stream; powers the
-- count half of the paired mailbox-changes token and JMAP
-- Email/changes). Same semantics as above against set_change_seq.
ALTER TABLE set_state
    ADD COLUMN set_change_floor INTEGER NOT NULL DEFAULT 0;
