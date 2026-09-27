# Changelog

## 0.3.0

- Add non-blocking `properties(pane)` snapshots with bounded metadata
  workers and stale generation/selection reply rejection. Preserve symlink
  metadata and targets; expose timestamps, Unix mode and numeric owner/group.
- Reuse the count queue for folder item counts and status-bar totals for
  unselected-folder summaries. MIME descriptions are extension-based hints.
- Persist plain Places/Properties open states and widths in schema 2;
  migrate schema 1 while retaining pane/split state and malformed-config
  overwrite protection. Default both panels open at 15%.
- Add metadata, stale-reply, persistence and config migration tests.

## 0.2.0

- Add explicit-pane navigation, refresh, hidden toggle and sort methods
  without changing active-pane keyboard behaviour.
- Cache Places directory checks until a relist or explicit Places refresh.
  Both-pane relists after successful and failed operations invalidate the
  cache, with one stat pass on the next view.

## 0.1.0

- Extract the headless file-manager core: twin panes, history, listings,
  single selection, file operations, dialog reservations and schema-1 config.
