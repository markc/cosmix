# Virtual Keyboard — Future On-Screen Keyboard App

## Context

`cosmix-daemon` has native Wayland input injection via `zwp_virtual_keyboard_v1` in
`crates/cosmix-daemon/src/wayland/virtual_keyboard.rs`. This is the same protocol
that on-screen keyboards (squeekboard, wvkbd) use.

## Idea

Build a touch-screen-enabled visual on-screen keyboard (OSK) as a COSMIC app,
reusing the existing virtual keyboard protocol plumbing.

## What's Already Done

- XKB keymap generation (ONE_LEVEL, per-keysym mapping)
- memfd-based keymap upload to compositor
- Key event sending (`vk.key()`) and modifier handling (`vk.modifiers()`)
- Support for all printable ASCII + function keys + named keys
- Works in all app types (iced, GTK, terminal emulators)

## What's Needed

- **GUI layer** — iced/libcosmic app rendering a keyboard layout
- **Persistent connection** — keep `Connection` + `ZwpVirtualKeyboardV1` alive
  for the app lifetime (current code reconnects per invocation)
- **Layout engine** — QWERTY, numpad, symbols, language variants
- **Touch input** — map touch/click on key buttons to `vk.key()` calls
- **Layer shell** — use `zwlr_layer_shell_v1` or COSMIC equivalent to anchor
  the keyboard at screen bottom without stealing focus
- **Auto-show/hide** — respond to `text_input_v3` events to appear when a text
  field is focused (tablet mode)

## Architecture Sketch

```
┌─────────────────────────────┐
│  OSK App (iced/libcosmic)   │
│  ┌───────────────────────┐  │
│  │ Keyboard Layout UI    │  │
│  │ (touch/click targets) │  │
│  └──────────┬────────────┘  │
│             │ on press      │
│  ┌──────────▼────────────┐  │
│  │ virtual_keyboard.rs   │  │
│  │ (reused from cosmix)  │  │
│  │ vk.key() / modifiers()│  │
│  └──────────┬────────────┘  │
│             │ Wayland       │
└─────────────┼───────────────┘
              ▼
      cosmic-comp (compositor)
              │
              ▼
      focused application
```

## Reference Projects

- **squeekboard** — GNOME/Phosh OSK (Rust + GTK)
- **wvkbd** — minimal Wayland OSK (C + wlr-layer-shell)
- Both use `zwp_virtual_keyboard_v1` — same protocol we already implement
