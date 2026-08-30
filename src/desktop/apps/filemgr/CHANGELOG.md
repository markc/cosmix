# Changelog

## 0.9.5 — 2026-07-28

- Subscribe to CTK's local `theme.changed` lane so a shared theme selection
  made in another running desktop app applies immediately.

## 0.9.4 — 2026-07-27

- The selected row is now the knockout Mark originally asked for: icon and
  filename take the panel colour, so the whole row reads as one solid shape.
  0.9.3 had to settle for a conventional foreground because the knockout was
  unreachable at WCAG AA; CTK 0.44.0 separates the selection bar from the panel
  by 7:1, which makes it both reachable and legible.
- Selected-row metadata columns paint from `ctk.row.selected.text.dim` rather
  than sharing the primary foreground, so the size and date columns stay
  subordinate to the filename inside a selected row.

## 0.9.3 — 2026-07-27

- Paint selected-row foregrounds from CTK's contrast-checked
  `ctk.row.selected.text` instead of the panel token. The panel was the
  knocked-out look asked for, but it measures 1.82 to 3.81 against the selection
  wash and never reaches WCAG AA — worst on crimson/dark. The new token clears
  AA on every scheme in both modes.

## 0.9.2 — 2026-07-27

- Size pane path inputs to exactly one line of the effective UI font. The
  authored `min_height` was not the border-box floor it appeared to be — bevy's
  editable-text measure returns it as the *content* height, so padding and
  border were added on top and the input rendered a line and a half of dead
  space. Removing it lets the one-line measure size the box, which now also
  tracks the theme's `body_px`.
- Opt path inputs into CTK's theme-aware focus border, without changing border
  width.
- Repaint every selected-row foreground — disclosure, file icon, name, size
  and modified time — from the panel token, then restore each entity's exact
  resting token when selection moves.
- Order selection painting after the systems that rebuild pane rows. Rows are
  spawned through deferred commands, so a relist, a sort or a folder expansion
  that retained its selection previously rendered one frame with the row drawn
  as unselected.

## 0.9.1 — 2026-07-27

- Centre the navigation and file-action toolbar groups as one cluster while
  retaining their visible separation; Back and Quit no longer disappear
  underneath the shell controls at the hard edges.
- Quiet the active-pane border to the same dim theme colour as pane status
  text; drag-and-drop target outlines remain independent.

## 0.9.0 — 2026-07-27

- Middle-elide long filenames through CTK while retaining the complete name for
  accessibility.
- Count visible folder contents on a four-task, generation-cancelled background
  queue, display the result in Size and the information panel, and include it
  in folder size sorting without moving rows for each arriving result.
  Superseded queued work is removed as soon as a pane relists, completed count
  batches re-sort when an expanded child corrects its parent's count, and only
  the active pane may repaint the shared information panel.
- Keep modified times relative for the first seven days, refreshing them once a
  minute, then show local date and time; future timestamps are always absolute.
  Out-of-range filesystem times fall back to a dash rather than panicking.
- Replace control characters only in the displayed filename while retaining
  the original path bytes for filesystem operations.

## 0.8.1 — 2026-07-26

- Add the exported filename to the drag icon as a translucent bounded pill, so
  a drag into another application says which file is moving. Rows retain only
  the shared square raster and a string; CTK creates the wide raster once at the
  export threshold rather than once per directory entry.

## 0.8.0 — 2026-07-26

- Hand every moving file-row drag to the Wayland compositor at CTK's four-pixel
  threshold, so the compositor can hit-test other clients despite its implicit
  pointer grab. Pane-to-pane and same-pane drops now make the same nonce-
  correlated round trip back through the compositor instead of taking a
  separate in-app path.
- Activate the prepared 40-logical-pixel file raster as the compositor-owned
  drag icon. If the bridge cannot provide drag-icon globals, file dragging
  remains available through the iconless Wayland path.

## 0.7.1 — 2026-07-26

- Load CTK's icon set with a raster source and build a CPU drag-icon raster per
  file row, so a Wayland drag can hand the compositor an icon of its own once
  the export trigger moves. Nothing consumes the raster yet.
- Derive the icon's buffer scale from the window scale factor, rounding up so a
  fractional or HiDPI scale never under-samples.

## 0.7.0 — 2026-07-26

- Adopt CTK's `os-dnd` glue so drags from other Wayland clients reach the
  browser's drop targets.

## 0.6.4 — 2026-07-25

- Adopt CTK's shared `dnd` module in place of the local drag implementation.

## 0.6.3 — 2026-07-25

- Register AMP provenance with the app's own version rather than CTK's, so the
  broker reports what is actually running.

## 0.6.2 — 2026-07-25

- Port the browser onto the shared `DcsAppShell` chrome extracted into CTK.

## 0.6.1 — 2026-07-25

- Resolve the broker URL from `node.conf.mix` instead of a hardcoded loopback
  address.

## 0.6.0 — 2026-07-24

- Rename the app, package and binary to CosMix FileMgr / `cosmix-filemgr`.
- Derive runtime state, AMP citizen, app-control and window identities from one
  `AppIdentity`, including `dev.cosmix.filemgr` as the native window app id.
- Join the shared `desktop/` Cargo workspace and remove the retired per-app
  dotdir config-discovery fallback.

## 0.5.1 — 2026-07-23

- Fix the startup schedule panic caused by ordering two intersecting CTK modal
  system sets.
- Funnel every interaction producer through one preflight publisher: early
  requests suppress the current shortcut batch, while late producers defer to
  the next frame instead of opening behind already-resolved input.
- Validate the real runtime plugin schedule and modal timing behaviour with
  headless startup/input regressions.

## 0.5.0 — 2026-07-23

- Add an icon menu bar for file, navigation, view, and desktop theme actions,
  with reactive accelerator hints and enabled/check/radio presentation.
- Replace the browser's hard-coded shortcut ladder with the shared
  `cosmix-actions` resolver while retaining focused path/name editing locally.
- Add real create-folder and rename flows alongside the existing copy, move,
  delete, navigation, sorting, hidden-file, refresh, and open operations.
- Add Fable's `filemgr` AMP app port with `action.invoke`, `actions.list`,
  `actions.describe`, `app.theme.set`, and `app.quit`. Navigation, view and
  theme actions are AMP-enabled; picker-opening and mutating actions remain
  local-only.
- Apply theme selections immediately, persist them asynchronously to the shared
  desktop theme file, preserve shared/per-app token overrides during live
  apply, compose multiple same-frame selections in order, and report
  persistence failures through CTK interaction.
- Fold direct `app.theme.set` and theme action ingress through one per-frame
  reducer, producing one final live apply and persistence request.
- Preserve keyboard provenance when Bevy's focused-button activation is
  translated onto the action bus.

## 0.4.0 — 2026-07-23

- Adopt CTK's revisioned runtime theme application and focus-gained reload of
  the shared/app theme cascade.
- Replace browser chrome, rows, selection, hover, borders, text, and SVG icon
  literal colours with shared CTK theme tokens.
