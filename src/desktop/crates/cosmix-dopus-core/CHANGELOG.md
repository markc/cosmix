# Changelog

## 0.3.2

- Add `transfer_paths` for an explicit Copy or Move on pinned gesture paths,
  using the existing destination validation, worker and single-flight operation
  pipeline. An unresolved Ask cannot start a file operation.

## 0.3.1

- Distinguish summary and message Status events by a typed kind.
- Expose footer summaries that show an ellipsis during root listing and the
  pane status after a root listing error, rather than misleading empty totals.

## 0.3.0

- Expose a per-pane listing/count reply revision for inexpensive UI cache
  invalidation, including multiple replies delivered in one update.
- Retry pending Properties reads automatically when metadata worker capacity
  frees; never cache a transient busy result. Retain timeout error caching.
- Apply the Properties-specific 22% width default to partial schema-2
  records, preserving explicit widths and open state.
- Add non-blocking `properties(pane)` snapshots with bounded metadata
  workers and stale generation/selection reply rejection. Preserve symlink
  metadata and targets; expose timestamps, Unix mode and resolved owner/group
  names via worker-side reentrant lookups, falling back to numeric IDs.
- Reuse the count queue for folder item counts and status-bar totals for
  unselected-folder summaries. MIME descriptions are extension-based hints.
- Persist plain Places/Properties open states and widths in schema 2;
  migrate schema 1 while retaining pane/split state and malformed-config
  overwrite protection. Default both panels open: Places 15%, Properties 22%.
- Add metadata, stale-reply, persistence and config migration tests.
- Format modification times as local absolute `dd/mm/yy HH:MM` only.
- Avoid cloning the entire visible listing for Properties. Preserve transient
  statuses on metadata arrival; let selection changes bypass stuck reads with
  four outstanding reads per pane and a five-second UI timeout.

## 0.2.0

- Add explicit-pane navigation, refresh, hidden toggle and sort methods
  without changing active-pane keyboard behaviour.
- Cache Places directory checks until a relist or explicit Places refresh.
  Both-pane relists after successful and failed operations invalidate the
  cache, with one stat pass on the next view.

## 0.1.0

- Extract the headless file-manager core: twin panes, history, listings,
  single selection, file operations, dialog reservations and schema-1 config.
