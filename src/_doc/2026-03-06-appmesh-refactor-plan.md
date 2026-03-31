# AppMesh Refactor: PHP/QML → Rust/Lua

Date: 2026-03-06

## Current State

AppMesh is a desktop automation platform with:
- **Rust FFI core** (3,155 LOC): 6 ports (clipboard, windows, notify, screenshot, input/EIS, mail/JMAP)
- **PHP MCP server** (3,429 LOC): 11 plugins, 66 tools, JSON-RPC 2.0 stdio
- **QML/Qt6 UI**: Kirigami mail client, 2 Plasma plasmoids, AppMeshBridge singleton
- **C++ Kate plugin**: KTextEditor D-Bus integration (19 methods, 2 signals)
- **Target desktop**: KDE Plasma 6

## Target State

- **Rust runtime** with mlua: embeds LuaJIT, exposes ports + D-Bus + mesh to Lua
- **Lua plugins**: replace PHP plugins, hot-reloadable
- **Target desktop**: COSMIC (not KDE Plasma)
- **No QML/Qt6**: drop entirely
- **No PHP**: all scripting in Lua
- **No C++**: Kate plugin becomes irrelevant (COSMIC has cosmic-edit)

## What Stays (Rust Core)

These Rust components are sound and carry forward:

| Component | File | LOC | Notes |
|-----------|------|-----|-------|
| AMP protocol | `amp.rs` | 371 | Wire format, unchanged |
| Unix socket server | `socket.rs` | 249 | Transport, unchanged |
| AppMeshPort trait | `port.rs` | 61 | Core abstraction, unchanged |
| FFI exports | `ffi.rs` | 176 | Rework: expose to Lua instead of C ABI |
| ClipboardPort | `ports/clipboard.rs` | 92 | Rework: COSMIC clipboard D-Bus |
| NotifyPort | `ports/notify.rs` | 102 | Keep: freedesktop standard |
| ScreenshotPort | `ports/screenshot.rs` | 89 | Rework: COSMIC screenshot |
| WindowsPort | `ports/windows.rs` | 205 | Rework: cosmic-comp D-Bus |
| InputPort/EIS | `eis.rs` + `ports/input.rs` | 454 | Rework: COSMIC input portal |
| MailPort | `ports/mail.rs` | 1115 | Keep: JMAP is desktop-agnostic |

## What Gets Dropped

| Component | Why |
|-----------|-----|
| PHP MCP server (`server/`) | Replaced by Lua plugin system |
| PHP FFI bridge (`appmesh-ffi.php`) | No longer needed |
| QML plugin (`qml/appmeshplugin.*`) | COSMIC uses iced, not Qt |
| Kirigami mail client (`qml/mail/`) | Replace with iced UI or Lua-driven |
| Plasma plasmoids (`qml/plasmoids/`) | Replace with COSMIC panel applets |
| Kate D-Bus plugin (`kate-dbus-text-plugin/`) | COSMIC has cosmic-edit |
| KDE-specific D-Bus calls | KWin → cosmic-comp, Klipper → cosmic clipboard, Spectacle → cosmic screenshot |

## What Gets Added

| Component | Purpose |
|-----------|---------|
| `mlua` dependency | Embed LuaJIT in Rust runtime |
| Lua API module | Expose ports, D-Bus, mesh, HTTP to Lua |
| Lua plugin loader | Scan `plugins/` dir, load `.lua` files |
| COSMIC D-Bus bindings | cosmic-comp, cosmic-files, cosmic-edit, cosmic-term, cosmic-settings |
| `cosmix` binary | Main entry point: `cosmix run script.lua`, `cosmix daemon`, `cosmix shell` |

## Plugin Migration Map

Each PHP plugin becomes a Lua module:

| PHP Plugin | LOC | Lua Equivalent | Notes |
|------------|-----|----------------|-------|
| `dbus.php` (8 tools) | 255 | `dbus.lua` | Generic D-Bus → keep, swap qdbus6 for zbus |
| `ports.php` (23 tools) | 136 | Built-in | Ports exposed directly to Lua via mlua |
| `config.php` (11 tools) | 721 | `config.lua` | KDE config → COSMIC cosmic-config |
| `editor.php` (12 tools) | 192 | `editor.lua` | Kate D-Bus → cosmic-edit D-Bus |
| `keyboard.php` (2 tools) | 88 | Built-in | EIS port exposed directly |
| `cdp.php` (6 tools) | 408 | `browser.lua` | Chrome DevTools Protocol, desktop-agnostic |
| `websocket.php` (6 tools) | 584 | Built-in | meshd handles WebSocket natively |
| `tts.php` (6 tools) | 337 | `media.lua` | TTS + screen recording, uses ffmpeg |
| `midi.php` (8 tools) | 397 | `midi.lua` | PipeWire MIDI, desktop-agnostic |
| `osc.php` (3 tools) | 209 | `osc.lua` | OSC/UDP, desktop-agnostic |
| `socket.php` (4 tools) | 102 | Built-in | Socket lifecycle managed by runtime |

## COSMIC D-Bus Discovery Needed

COSMIC 1.0.0 is new. Before porting, we need to discover what D-Bus interfaces COSMIC apps expose:

```bash
# Discover COSMIC D-Bus services
dbus-send --session --dest=org.freedesktop.DBus --print-reply \
  /org/freedesktop/DBus org.freedesktop.DBus.ListNames \
  | grep -i cosmic

# Introspect a COSMIC service
dbus-send --session --dest=com.system76.CosmicComp --print-reply \
  --type=method_call /com/system76/CosmicComp \
  org.freedesktop.DBus.Introspectable.Introspect
```

This discovery work is Phase 0 — understanding what COSMIC exposes before writing bindings.

## Phase Timeline

| Phase | Work | Estimate |
|-------|------|----------|
| 0 | COSMIC D-Bus discovery | 1-2 days |
| 1 | Add mlua, expose existing ports to Lua | 1 week |
| 2 | Port desktop-agnostic plugins (CDP, MIDI, OSC, TTS) | 1 week |
| 3 | COSMIC-specific ports (windows, clipboard, screenshot, editor) | 2-3 weeks |
| 4 | `cosmix` binary (run, daemon, shell modes) | 1 week |
| 5 | Drop PHP/QML, clean up | 2-3 days |
