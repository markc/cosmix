# Changelog

## 0.3.0

- Preserve a usable Name column in narrow panes by hiding Modified first,
  shrinking/eliding Size, then hiding Size when necessary. Cap tree indentation
  to the same name budget; keep header and row geometry in step.
- Use advanced text shaping for filename measurement and drawing, including
  complex scripts and fallback fonts. Tint disabled navigation icons with the
  design tokens' muted foreground.
- Fix clippy doc-comment and test-module placement findings.

- Consolidate navigation into one window-centred icon strip acting on the
  active pane through existing actions. Disable Back/Forward for empty
  histories and Up at the root; retain only each pane's editable location bar.
- Resolve Properties owner/group names in the metadata worker using the
  existing nix dependency, with independent numeric fallbacks on failure.

- Middle-elide listing and Places names with measured, grapheme-safe,
  extension-preserving text. Share fixed right-aligned Size/Modified columns
  between headers and rows, reserve the absolute timestamp width and clip
  each column and the list viewport.
- Use the desktop design's selection pair and a tinted active-pane header;
  derive layout spacing from design tokens.
- Add a plain Properties sidebar with asynchronous metadata, count-queue
  folder sizes and an unselected-folder summary matching the status bar.
- Add F9/F10 and clickable Places/Properties toggles, draggable dividers,
  persisted open/width state and schema-1-to-2 migration.
- Serve both toggle actions windowed, refuse headless/busy with UNAVAILABLE,
  and expose `places`/`properties` in `dopus.state`. Keep all `file.*` forbidden.
- Add elision, column/divider geometry, metadata, migration, key and Bus
  availability regression coverage, plus headless Bus e2e assertions.

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
