# Changelog

## 0.4.4 — 2026-07-28

- Subscribe to CTK's local `theme.changed` lane so a shared theme selection
  made in another running desktop app applies immediately.

## 0.4.0 — 2026-07-24

- Rename the app, package and binary to CosMix Studio / `cosmix-studio`, with
  the stable `studio` component slug replacing `midiseq`.
- Derive runtime state, AMP citizen, app-control and window identities from one
  `AppIdentity`, including `dev.cosmix.studio` as the native window app id.
- Join the shared `desktop/` Cargo workspace and target directory.
- **State migration:** the per-app state root moves `cosmix/apps/midiseq` →
  `cosmix/apps/studio` (one-time operator `mv`; the retired `midiseq` slug is
  never reused — `desktop/APPS.md`). No automatic migration code: the app had
  never shipped beyond the build machine.

## 0.3.4 — 2026-07-23

- Validate Fusion's complete non-render runtime plugin schedule in a headless
  startup regression and adopt CTK's corrected disjoint modal scheduling.

## 0.3.3 — 2026-07-23

- Add the shared Themes menu with live light/dark and six-scheme radio
  presentation, keymap hints, instant override-preserving colour application,
  ordered same-frame selection folding, and asynchronous desktop-wide
  persistence.
- Expose `app.theme.set {scheme, mode}` and allow the nullary theme actions
  through Fusion's existing action AMP port.
- Fold direct theme selection and theme actions through one working-state
  reducer, with one final live apply and persistence request per frame.
- Surface asynchronous theme-write failures on Fusion's status line.

## 0.3.2 — 2026-07-23

- Apply CTK colour themes at runtime and re-read the shared/app theme cascade
  when the Fusion window regains focus.
- Replace Settings and status-line literal colours with shared theme tokens.
- Include the theme revision in arranger paint caching so waveform and ruler
  textures regenerate against the current palette.
- Restyle channel-derived lane-header name colours on theme revision without
  rebuilding their controls or arranger structure.

## 0.3.1 — 2026-07-23

- Expose the CTK `action.invoke` and `actions.*` AMP surface from Fusion's
  process-scoped app port.
- Route authorised AMP action requests through the same `ActionRequest` and
  `AudioIntent` path used by menus and keyboard shortcuts.
- Mark every requester/widget-activation action, including the settings
  family, local-only for AMP. Only song and SoundFont loads advertise their
  existing explicit-path direct verbs; app startup cross-checks those names
  against the live app-port registry.
- Explicitly allow AMP only for transport, view and zoom actions, and reject
  invocation while a local modal owner exists or an enabled interactive request
  was produced earlier in the current frame. The frame order is Produce → app
  port → Route → Apply, so current availability and same-frame keyboard capture
  are authoritative without delaying accepted AMP actions.
- Make file-action predicates match consumer availability, including disabling
  Open Song without a song editor, and advance the shared enabled revision in
  the same system that refreshes those atomic mirrors.
