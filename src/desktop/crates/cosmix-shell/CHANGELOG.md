# Changelog

## 0.5.0 — 2026-09-02

- Add the renderer-neutral semantic panel/page adapter. Bus verbs now produce
  the same `ShellCommand` values as direct UI ingress.
- Add `PanelInput::Toggle` to the core panel state machine. The toggle
  direction binds at Model time against the authoritative mode (never a caller
  frame snapshot), so a mid-conceal panel toggles straight back open and two
  toggles in one drained batch net to identity. The semantic adapter emits it
  and no longer takes a `ShellFrame`.

## 0.4.0 — 2026-09-02

- Breaking: `chrome` now selects Wayland only instead of Bevy's combined X11
  and Wayland default-platform surface. X11 consumers must replace `chrome`
  with `chrome-core` plus `platform-x11`; `chrome-core` must be paired with at
  least one platform feature on Unix. Both platform features may be selected
  when a host deliberately supports both backends.
- Preserve the non-backend members formerly supplied by Bevy's `ui` umbrella,
  including its embedded default font, multi-threaded scheduler, cursor,
  clipboard, gamepad, web and system-information support.
