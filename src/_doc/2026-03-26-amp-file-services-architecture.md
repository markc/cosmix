# AMP File Services Architecture — WASM, Mesh, and the ARexx Model

> Date: 2026-03-26

## The Problem

Dioxus apps have two build targets:
- **Desktop** — full filesystem access, system APIs, native file pickers (rfd/portal)
- **WASM** — runs in browser, sandboxed, no filesystem, no system APIs

The traditional approach (D-Bus, xdg-desktop-portal) only works locally and only
for native apps. WASM clients are second-class citizens.

## The Insight

WASM limitations disappear when the UI is a thin client that sends AMP commands
over WebSocket to a daemon that has full system access. The daemon does the heavy
lifting (filesystem, process execution, database access); the client just renders.

This is the ARexx model: applications are message ports on a bus. The UI is just
a message sender. The transport is the mesh, not the local bus.

## File Services via AMP

Instead of linking a file picker library into each app, file operations are a
**service namespace** on the AMP mesh:

```
---
command: file.list
path: /home/cosmix/_doc/
filters: ["md", "dot", "png"]
---
```

```
---
command: file.list
rc: 0
count: 5
---
[{"name":"network-topology.md","size":3421,"modified":"2026-03-26T12:00:00Z"}, ...]
```

### Core commands

| Command | Purpose |
|---------|---------|
| `file.list` | List directory contents (with optional filters) |
| `file.read` | Read file contents (text or base64 for binary) |
| `file.stat` | File metadata (size, type, modified) |
| `file.write` | Write file contents |
| `file.pick` | Open a file picker UI, return selected path(s) |
| `blob.get` | Read binary file as base64 |

### File picker as a service

The file picker is not a component — it's a service call:

```
---
command: file.pick
filters: ["md", "dot", "png"]
title: Open file
---
```

Any app on the mesh that implements `file.pick` can serve as the picker. This
could be a dedicated file manager app, or any app that has file browsing UI
(e.g. cosmix-view could act as a picker for other apps since it already knows
how to preview markdown, images, and DOT files).

## Uniform Architecture

The same message works regardless of topology:

| Scenario | Transport | Behaviour |
|----------|-----------|-----------|
| Desktop app → local daemon | WebSocket localhost | Daemon reads local FS |
| WASM app → local daemon | WebSocket localhost | Same as above |
| Phone browser → cachyos daemon | WebSocket over WireGuard | Browse remote FS |
| WASM app → mko mail server | WebSocket over mesh | Browse server files |

There is no distinction between "local file picker" and "remote file browser" —
it's the same AMP command, same UI component, different WebSocket endpoint.

## Comparison with D-Bus/Portal

| | D-Bus + Portal | AMP over WebSocket |
|---|---|---|
| Scope | Local machine only | Any machine on the mesh |
| Transport | Unix socket | WebSocket (NAT/firewall/WG friendly) |
| Clients | Native apps only | Browser, desktop, mobile |
| Auth | System user/polkit | Token/session based |
| Discovery | Bus names | Mesh peer directory |
| File picker | Separate portal process | Any app with file.pick port |

## The ARexx Principle

Every app is both a UI and a service. cosmix-view is a file viewer, but it's
also a `view.pick` service that other apps can call. cosmix-mail is an email
client, but it's also a `mail.send` service.

The distinction between "library", "component", and "application" dissolves.
They are all ports on the mesh.

## Implications for cosmix-view

Current state: uses `rfd::AsyncFileDialog` which calls xdg-desktop-portal via
D-Bus. Works on desktop, fails in WASM.

Future state: sends `file.pick` or `file.list`/`file.read` via AMP WebSocket.
Works identically on desktop, WASM, and across the mesh. The file picker UI
is either an in-app Dioxus component (browsing the daemon's FS) or a separate
app that responds to `file.pick` commands.

The `rfd` approach remains as a fallback for standalone desktop use without a
running daemon.

## Implementation Path

1. Add `file.*` command namespace to cosmix-jmap's AMP handler (it already has
   axum + WebSocket via cosmix-mesh)
2. Build a Dioxus `FileBrowser` component that sends AMP commands and renders
   the results (directory listing, navigation, preview)
3. cosmix-view and cosmix-mail both use the same component
4. WASM builds work automatically — same code, same commands, different endpoint
