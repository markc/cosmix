# Changelog

## 0.2.0

- Add optional pane targets to dopus.v1 actions and open requests, preserving
  existing default routing; expose matching pane state rows in info.
- Use TextField submission for location editing and add the canonical
  location.focus action with Ctrl+L.
- Render cached Places and support explicit refresh.
- Document the app contract, Bus surface, keymap and schema-1 config.

## 0.1.0

- Initial iced twin-pane file manager and dopus.v1 Bus port. File operations
  are local keyboard/dialog actions; every file.* id remains Bus-forbidden.
