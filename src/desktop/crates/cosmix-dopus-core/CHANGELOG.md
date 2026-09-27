# Changelog

## 0.2.0

- Add explicit-pane navigation, refresh, hidden toggle and sort methods
  without changing active-pane keyboard behaviour.
- Cache Places directory checks until a relist or explicit Places refresh.
  Both-pane relists after successful and failed operations invalidate the
  cache, with one stat pass on the next view.

## 0.1.0

- Extract the headless file-manager core: twin panes, history, listings,
  single selection, file operations, dialog reservations and schema-1 config.
