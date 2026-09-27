# Changelog

## 0.2.0

- Add optional pane targets to dopus.v1 actions and open requests, preserving
  existing default routing; add `dopus.info.pane_states` with the same row
  shape as `dopus.state.panes`, retaining the existing `info.panes` count.
- Use TextField submission for location editing and add the canonical
  location.focus action with Ctrl+L, also added to `dopus.actions.list`.
- Serve `location.focus` over the Bus in windowed mode; report UNAVAILABLE
  and disable its action-list row headless or while the window is busy.
- Preserve an unfinished location draft on repeated Bus focus of the same
  pane; focusing the other pane still switches editors.
- Accept numeric pane aliases 0/1 alongside canonical left/right/active names.
- Dismiss location editing on outside presses, pane controls and split
  changes; resolve custom editable bindings before suppressing unhandled
  modified Enter.
- Render cached Places and support explicit refresh.
- Document the app contract, Bus surface, keymap and schema-1 config.

## 0.1.0

- Initial iced twin-pane file manager and dopus.v1 Bus port. File operations
  are local keyboard/dialog actions; every file.* id remains Bus-forbidden.
