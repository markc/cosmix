# Cosmix Layered Architecture

Date: 2026-03-07

## Problem

Getting Lua scripting into every COSMIC app individually would require patching each app separately — slow, fragile, and politically impossible upstream. We need an approach that delivers value incrementally while building toward deep integration.

## Key Insight

All COSMIC apps use **libcosmic** as their toolkit. If scripting infrastructure lives in libcosmic (behind a feature flag), every app gets it for free with minimal per-app code. But we don't need to wait for that — useful automation is possible today with zero app modifications.

## Three Layers

### Layer 1: External Daemon (Zero App Changes)

A `cosmix` daemon that controls apps from the outside using existing system interfaces:

| Interface | What It Provides |
|-----------|-----------------|
| **ext-foreign-toplevel-list** | Window listing, activation, minimize/maximize/close |
| **EIS/libei** | Keyboard and mouse input injection |
| **AT-SPI2** | Read UI state — widget tree, text content, button labels |
| **D-Bus (freedesktop)** | Notifications, media control, etc. |
| **D-Bus (COSMIC)** | Whatever COSMIC apps expose (discovery needed) |

This is the AutoHotkey/Hammerspoon model. Powerful for window management and input automation, but can't call app-internal logic.

**Status:** Not started. First implementation target.

### Layer 2: Compositor Scripting (One Binary to Patch)

Embed mlua directly in cosmic-comp. The compositor already knows about all windows, workspaces, outputs, and input devices. We're already patching it for animation overrides.

Lua scripts at this level can:
- Define window rules (auto-tile by app class, workspace assignment)
- Create custom keybindings that trigger Lua functions
- Script workspace management (named workspaces, layouts)
- React to window events (new window, focus change, close)

This is the **Awesome WM** model — the compositor IS the scripting host.

**Status:** Not started. Depends on Layer 1 for daemon infrastructure.

### Layer 3: libcosmic Integration (The ARexx Endgame)

A `cosmix-port` crate that provides per-app command registration:

```rust
// The crate API (what goes into libcosmic)
pub trait CosmixApp {
    fn cosmix_commands(&self) -> Vec<CosmixCommand>;
}

// What each app adds (minimal)
fn cosmix_commands(&self) -> Vec<CosmixCommand> {
    vec![
        command!("open", |path: String| self.open_file(path)),
        command!("search", |query: String| self.search(query)),
    ]
}
```

The crate handles:
- **IPC listener** — Unix socket at `/run/user/$UID/cosmix/<app-name>.sock`
- **Command registry** — type-safe command registration with serde
- **Auto-discovery** — daemon finds all running apps via socket directory
- **AMP messages** — uses the existing wire format

The crate does NOT contain Lua. Lua lives only in the cosmix daemon. Per-app overhead is minimal: one Unix socket listener thread, a command lookup table.

**Status:** Design only. Requires Layers 1+2 working to make the upstream pitch.

## Transport Decision

**Unix socket** for all local per-app IPC, not D-Bus or WebSocket.

| Transport | Verdict | Why |
|-----------|---------|-----|
| Unix socket | **Use** | Fast, simple, auto-discoverable via filesystem, no broker |
| D-Bus | Consume only | Good for reading existing interfaces, bad for building new ones (complex, schema-heavy) |
| WebSocket | Cross-node only | meshd handles inter-node communication over WireGuard |

Socket path convention: `/run/user/$UID/cosmix/<app-id>.sock`

The daemon scans this directory to discover running apps. Apps create their socket on startup, remove on shutdown.

## Why This Order

1. **Layer 1 delivers value immediately** — window management, input automation, accessibility introspection. No upstream dependencies.

2. **Layer 2 is a natural extension** — we're already patching cosmic-comp. Adding mlua there gives us compositor scripting with one binary.

3. **Layer 3 needs a demo** — System76 won't accept a PR adding mlua to libcosmic based on a pitch deck. They need to see Layers 1+2 working, see the community demand, and see that the per-app integration is truly minimal.

## Comparison to ARexx

| ARexx (Amiga, 1987) | Cosmix (COSMIC, 2026) |
|----------------------|------------------------|
| ARexx interpreter | cosmix daemon + LuaJIT (via mlua) |
| ARexx ports per app | cosmix-port crate per app |
| `rexxsyslib.library` | `cosmix-port` crate in libcosmic |
| AREXX: address command | `cosmix.port("name"):command()` |
| String-based IPC | AMP (markdown frontmatter) |
| AmigaOS message ports | Unix sockets |
| CLI `rx script.rexx` | `cosmix run script.lua` |
| Local machine only | Cross-node via meshd + WireGuard |

The key parallel: ARexx didn't modify apps either. Apps voluntarily linked `rexxsyslib.library` and registered a port. The interpreter was a separate process. Cosmix follows the exact same model — the `cosmix-port` crate is `rexxsyslib.library`, the daemon is the interpreter.

## Success Criteria

| Layer | "Done" when... |
|-------|----------------|
| L1 | A Lua script can list windows, activate one, type text into it, and read a widget label via AT-SPI2 |
| L2 | A Lua config in cosmic-comp defines window rules and custom keybindings |
| L3 | cosmic-files exposes "open directory" and "get selection" commands callable from a Lua script via cosmix-port |
