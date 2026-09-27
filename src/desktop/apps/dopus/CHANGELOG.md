# Changelog

## 0.3.1

- Remove the Places and Properties headings; align first-content baselines
  using complete button/icon/font geometry, covered by layout tests.
  Places refreshes with pane relists.
- Resolve one sidebar typography token at the original Places size (Ui × 0.9,
  13.2 px in the embedded design) for both panels; Properties becomes smaller.
  Distinguish field names by muted colour.
- Elide and clip pane summary footers, including at 25-px pane widths; keep
  their boxes capped at the summary's natural width. Add narrow-layout coverage.
- Use the shared widgets tooltip style: neutral `muted` surface/text,
  `border` token outline and rounded corners with token padding.
- Cache first-row geometry by look and footer widths by look/text per pane;
  invalidate on theme reload and tolerate missing baseline metrics.
- Match typed summary/message events; show an ellipsis while listing and the
  pane status on root listing failure. Check the Bus summary field in the e2e.
- Give each pane a small muted summary box below its clipped list. Keep
  panel toggles and messages in the shared status bar; expose the same
  totals as an additive `summary` field in each Bus pane state.

## 0.3.0

- Skip unchanged Size measurements with a cheap listing signature; shape the
  four longest candidates plus all cutoff ties, deduplicating identical values.
  Keep Size grow-only until the pane root or typography changes, and list Ctrl
  alternatives first in tooltips.
- Measure Size from each listing's actual values, bounded by a readable floor
  and count ceiling; share the cached layout between headers and rows.
- Use quiet input borders on location bars, reserving the ring for focused editing.
- Add Tab, Ctrl+R and Ctrl+E alternatives to F6, F5 and F2 for pane switching,
  refresh and rename. Preserve editor/dialog handling and show all bindings in tooltips.
- Add Lucide panel-left/panel-right toolbar buttons at the window edges,
  with docked/hidden state styling and the existing sidebar actions.
- Add token-styled iced tooltips to navigation, panel toggles, Places,
  sort headers and listing icons/chevrons. Tooltips, status labels and Bus
  action listings use effective bindings, including remaps and unbindings.
- Preserve a usable Name column in narrow panes by hiding Modified first,
  then hiding Size when necessary; never elide numeric cells. Cap tree indentation
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
- Use the desktop design's selection pair and a muted active-pane header;
  derive layout spacing from design tokens.
- Add a plain Properties sidebar with asynchronous metadata, count-queue
  folder sizes and an unselected-folder summary matching the status bar.
- Add Ctrl+B/Ctrl+I and clickable Places/Properties toggles with open-state markers, draggable dividers,
  persisted open/width state and schema-1-to-2 migration.
- Serve both toggle actions windowed, refuse headless/busy with UNAVAILABLE,
  and expose `places`/`properties` in `dopus.state`. Keep all `file.*` forbidden.
- Add elision, column/divider geometry, metadata, migration, key and Bus
  availability regression coverage, plus headless Bus e2e assertions.
- Use one actual Name text rectangle for shaping and drawing; only the last
  path component has an extension. Use fixed-width absolute 24-hour timestamps.
- Wrap long Properties values at glyph boundaries, default Properties to 22%
  and Places to 15%, and size dialogs by a responsive 32-em text measure.
- Preserve transient statuses on metadata arrival and use bounded metadata
  retries with five-second UI timeouts. Fix map comparisons in the Bus e2e.
- Use Ctrl+B for Places and Ctrl+I for Properties, replacing F13/F14 with
  standard-keyboard shortcuts free in dopus, ced and inputd's defaults.

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
