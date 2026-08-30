# Changelog

## 0.3.0 — 2026-07-24

- Rename the public `fusion` and `fable` action modules to `studio` and
  `filemgr`.
- Rename the packaged default keymaps and their exported constants, including
  the Studio modal scopes and FileMgr action/modal kinds.

## 0.2.1 — 2026-07-23

- Add canonical Fable action ids and its checked-in default `.mix` keymap.
- Add shared desktop theme action ids used by Fable and Fusion menus.

## 0.2.0 — 2026-07-23

- Add a serialisable, fail-closed per-action source allowlist. AMP, MIDI and
  OSC require explicit opt-in; local app/UI sources remain enabled by default.
- Add interactive-action metadata for either a real typed direct AMP verb or
  an explicitly local-only interaction. Interactive metadata is an absolute
  AMP prohibition: metadata decoding and live registration reject any
  interactive action whose source policy allows AMP.
- In 0.2, AMP is the authoritative adapter-enforced source flag; key, mouse and
  menu flags remain advisory until their adapters adopt source-aware dispatch.
- Add non-interning metadata lookup and validation-only registry APIs for safe
  event-bus ingress adapters.
- Bound live registries to 4096 actions, 1 MiB aggregate owned metadata and 128
  argument fields per action; expose structural revisions for query caches.
