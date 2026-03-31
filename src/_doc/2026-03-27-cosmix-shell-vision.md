# cosmix-shell Vision

**Date:** 2026-03-27
**Status:** Architectural vision — not yet implemented
**Origin:** Emerged from a conversation about WASM menu bars, global theming, and the dcs.spa layout

---

## The Core Insight

cosmix-shell is a **dioxus-desktop WebView window** that contains a complete micro-world of
applications. To any Wayland compositor (or X11 window manager) it is simply an ordinary
application window. Inside that window lives a DCS-style (Dual Carousel Sidebar) layout that
can absorb cosmix apps as embedded panels, launch floating windows for task-focused tools, and
connect to the AMP mesh for everything that requires native capabilities.

Because it is built with Dioxus 0.7, the exact same component code compiles to:

1. A **native desktop binary** — runs on any Linux with WebKitGTK, any compositor
2. A **WASM bundle** — runs in any modern browser, connects to a remote hub over WebSocket
3. Simultaneously, as both, from a single codebase

---

## Layout: Dual Carousel Sidebars (DCS)

Inspired by [dcs.spa](https://dcs.spa/) (github.com/markc/dcs.spa). The layout is a
three-column design with a fixed top navigation bar:

```
┌─────────────────────────────────────────────────────────────────────┐
│  TOPNAV  [☰ left]  cosmix-shell              [☰ right]  [theme]    │
├──────────────┬──────────────────────────────────┬───────────────────┤
│              │                                  │                   │
│  LEFT        │         CENTRE PANEL             │  RIGHT            │
│  CAROUSEL    │                                  │  CAROUSEL         │
│              │  ← active absorbed app renders   │                   │
│  Panel 0:    │    here — files, launcher,       │  Panel 0:         │
│   Launcher   │    settings, mail summary etc.   │   System Monitor  │
│              │                                  │                   │
│  Panel 1:    │                                  │  Panel 1:         │
│   Files      │                                  │   Theme / Global  │
│   Browser    │                                  │   Settings        │
│              │                                  │                   │
│  Panel 2:    │                                  │  Panel 2:         │
│   Bookmarks  │                                  │   Notifications   │
│   / Nav      │                                  │                   │
│              │                                  │  Panel 3:         │
│  [< ● ● ●>]  │                                  │   Help / Docs     │
│  [Pin]       │                                  │  [< ● ● ● ●>]    │
│              │                                  │  [Pin]            │
└──────────────┴──────────────────────────────────┴───────────────────┘
```

### Sidebar behaviour

- **Pinned**: sidebar occupies a fixed column, centre panel narrows
- **Unpinned**: sidebar slides in as an off-canvas overlay, centre panel stays full width
- **Carousel**: each sidebar holds N panels, navigated with chevrons or dot indicators
- **Pop-out button** on any panel: extracts the panel into a floating native window
- **Pull-in button** on any floating window: absorbs it back into a carousel panel slot
- Width configurable per sidebar (200–400px), persisted to cosmix-config

### What lives where (default layout)

| Position | Panel | Content |
|---|---|---|
| Left 0 | Launcher | App grid, search, recent files |
| Left 1 | Files | cosmix-files component |
| Left 2 | Navigator | Bookmarks, quick-jump tree |
| Centre | Active app | Whichever panel was "opened" to full |
| Right 0 | Monitor | cosmix-mon component |
| Right 1 | Settings | cosmix-settings component |
| Right 2 | Notifications | Hub event feed |
| Right 3 | Help | Contextual docs |

This is a sensible default, not a fixed constraint. Panel assignment is user-configurable.

---

## Absorbed vs Floating: The Dual Mode

Every cosmix app component can run in two modes without any code change:

### Absorbed (embedded in shell panel)
- The component renders inside the shell's WebView
- Shares the shell's process, tokio runtime, and hub WebSocket connection
- No AMP round-trips for intra-shell state — direct Dioxus signal reads
- Transition: instant (no process spawn)

### Floating (separate native window)
- Launched via `DesktopContext::new_window(cfg, props)` from within the shell binary
- Runs in the same process as the shell (same tokio runtime, same hub client)
- Has its own `tao` window with its own WebView
- OR: launched as a fully separate binary (`cosmix-edit`, `cosmix-view`) communicating
  back via AMP hub — shell sends `edit.open { path }`, editor appears

### Decision heuristic

| Absorb | Float |
|---|---|
| Ambient / frequently consulted | Task-focused, benefits from full screen |
| Files browser | Text editor |
| System monitor | Markdown viewer |
| Settings | Mail compose |
| Launcher | Terminal (future) |

The pop-out / pull-in affordance means users override this at will.

---

## Multi-Instance: Multiple Workspaces and Monitors

Because cosmix-shell is just a Wayland client window, cosmic-comp (or any compositor) can
place multiple instances on different workspaces and/or different outputs:

```
cosmic-comp
├── Output 1 (primary monitor)
│   ├── Workspace 1: cosmix-shell instance A
│   │   └── (files, launcher, monitor panels)
│   └── Workspace 2: cosmix-shell instance B
│       └── (mail summary, calendar, notes panels)
└── Output 2 (secondary monitor)
    └── Workspace 1: cosmix-shell instance C
        └── (different node or same node, different context)
```

Each shell instance is completely independent in its UI state. All instances connect to the
same cosmix-hub (or different hubs on different nodes). Hub broadcasts (e.g. `config.changed`)
reach all connected clients simultaneously — all shells update theme/font instantly.

### Launching multiple instances

```bash
cosmix-shell &          # instance A, default hub
cosmix-shell &          # instance B, same hub
cosmix-shell --hub ws://remotenode:4200/ws &  # connects to remote node
```

Each instance registers with a unique name: `shell`, `shell.1`, `shell.2` etc. (the hub
assigns suffixes for duplicate names). Each gets its own AMP inbox.

---

## Cross-Node: Another Machine's Monitor

A cosmix-shell instance on machine B can connect to machine A's hub over the mesh:

```
Machine A                                    Machine B
─────────────────────────────────            ─────────────────────
cosmix-hub (node A)                          cosmic-comp
  ├── cosmix-edit                              └── cosmix-shell
  ├── cosmix-files                                 └── connects to:
  ├── cosmix-configd                                   ws://nodeA:4200/ws
  └── cosmix-mond
```

The shell on machine B has full access to all services on machine A's hub — it can open
files (via `file.read`), edit them (via `edit.open`), read system metrics (via `mon.status`),
get config (via `config.get`) — all mediated by AMP over WebSocket. No SSH tunnels, no NFS
mounts, no special remote desktop protocol.

This is not screen sharing. The shell is rendering locally with data from a remote node. It is
fast, low-bandwidth, and works over any network that allows a WebSocket connection.

---

## WASM: The Browser as a First-Class Target

The same cosmix-shell Dioxus components compile to WASM. A browser tab connecting to
`wss://yournode.local/hub` has exactly the same capabilities as the native desktop shell —
same layout, same panels, same AMP commands.

```bash
cd crates/cosmix-shell
dx build --platform web
# → serves at localhost:8080, connects to hub over wss://
```

### What WASM cannot do natively (and why it doesn't matter)

| Native capability | AMP solution | Daemon |
|---|---|---|
| File picker dialog | `dialog.pick` → returns path | cosmix-dialog (exists) |
| Read config file | `config.get` → returns value | cosmix-configd (exists) |
| List/read files | `file.list`, `file.read` | cosmix-files (exists) |
| Spawn processes | `shell.exec` → streams stdout | cosmix-shelld (new, trivial) |
| System metrics | `mon.status` | cosmix-mond (exists) |

The WASM shell is a **deliberately thin render + input layer**. Every side-effect is delegated
to a daemon on the node via AMP. The browser tab has zero native dependencies.

Consequence: you can open cosmix-shell in Safari on an iPhone and have full access to your
desktop node — file system, running processes, config, system metrics — mediated entirely by
AMP over WebSocket. No app install required.

---

## Compositor Compatibility

cosmix-shell (native binary) is compatible with every compositor that supports the `xdg-shell`
Wayland protocol, and every X11 window manager via XWayland:

**Wayland:**
- cosmic-comp (COSMIC desktop)
- KWin (KDE Plasma)
- Mutter (GNOME)
- sway (wlroots)
- Hyprland (wlroots)
- niri (Smithay)
- river, wayfire, labwc, weston

**X11:**
- Any X11 WM — i3, openbox, awesome, bspwm, xfwm, kwin-x11, mutter-x11

The only runtime dependency beyond standard Linux libraries is **WebKitGTK** (for the WebView
rendering engine). This is available in the package repositories of every major Linux
distribution.

cosmix-shell does **not** require:
- A specific desktop environment
- D-Bus session (though it can use it optionally)
- systemd (though it integrates with it when available)
- Root/compositor privileges

---

## WebRTC: The Remote Collaboration Layer (Future)

The AMP hub already provides WebSocket transport. WebRTC signalling (SDP offer/answer exchange)
can ride over AMP as a new command type:

```
shell A: send "webrtc.offer" to shell.nodeB with SDP payload
shell B: send "webrtc.answer" back with SDP payload
→ peer connection established
→ shell A panel streams its canvas to shell B via WebRTC DataChannel/MediaStream
```

Because cosmix-shell is a WebView, WebRTC is a browser API — available natively in WebKitGTK
without any additional native code. The cosmix-hub already handles the signalling rendezvous.

This enables:
- **Screen sharing a specific panel** (not the whole screen)
- **Collaborative editing** — both shells show the same editor state
- **Remote assistance** — one user observes another's shell
- **Multi-node dashboards** — one shell displays panels from several remote nodes simultaneously

WebRTC requires no architectural changes — it is an AMP command pair (`webrtc.offer` /
`webrtc.answer`) plus browser WebRTC APIs. The mesh transport already exists.

---

## The Protocol-First Insight

This architecture was not designed top-down as a "remote desktop system". It emerged from
bottom-up decisions:

1. Apps communicate via AMP over WebSocket → hub is the authority
2. Hub is accessible over any network → remote nodes are first-class
3. Dioxus compiles to WASM → browser is a first-class render target
4. Every native capability has an AMP delegate → WASM has no blockers
5. Shell is just a Wayland window → any compositor works

The result is a **protocol-first remote computing environment** where:
- The local desktop window and the browser tab are two render targets for the same mesh client
- "Local app" and "remote app" are not architectural categories — only "connected" or not
- The sovereign, self-hosted nature means no cloud dependency, no third-party mediation

This is adjacent to what ChromeOS attempted (browser as OS shell) and what web IDEs approximate
(Gitpod, Codespaces), but with critical differences:
- **Local-first**: works fully offline, hub runs on your own hardware
- **Self-hosted**: no Anthropic/Google/Microsoft in the data path
- **Uniform protocol**: every capability is AMP — no special cases, no platform APIs leaking
  through the abstraction
- **Sovereign**: you own the stack from WebKitGTK to the JMAP mail server

---

## Migration Path from Current State

### Phase 1 — cosmix-shell crate scaffold
Create `crates/cosmix-shell/` with the DCS three-column layout. No app absorption yet — just
the structural skeleton with placeholder panels and the OKLCH theme system (see
`2026-03-27-oklch-theme-system.md`).

### Phase 2 — Extract app components
Each existing app exposes its UI as a public component function alongside its standalone binary:

```
crates/cosmix-files/src/
  lib.rs          ← pub fn files_panel() -> Element  (new)
  main.rs         ← thin wrapper: fn app() { files_panel() }
```

The standalone binary continues to work. cosmix-shell imports the component from the library
target. No duplication, no rewrite.

### Phase 3 — Absorb into shell panels
cosmix-shell imports and renders:
- `cosmix_files::files_panel()` in left carousel panel 1
- `cosmix_mon::mon_panel()` in right carousel panel 0
- `cosmix_settings::settings_panel()` in right carousel panel 1

### Phase 4 — Floating window launch
cosmix-shell uses `DesktopContext::new_window()` to float cosmix-edit and cosmix-view as
separate windows within the same process. The standalone binaries remain for users who prefer
launching them directly.

### Phase 5 — WASM build
Add `web` feature to cosmix-shell. Guard native-only code behind
`#[cfg(not(target_arch = "wasm32"))]`. Build with `dx build --platform web`.

### Phase 6 — Multi-instance and cross-node
Shell registers with unique name per instance. Hub connection accepts `--hub` flag for
connecting to remote nodes. Remote shell panels show data from remote daemons.

### Phase 7 — WebRTC signalling
Add `webrtc.offer` / `webrtc.answer` AMP command handlers. Implement panel streaming.

---

## Crate Structure (target)

```
crates/
  cosmix-shell/          ← new: DCS shell binary + lib
    src/
      main.rs            ← desktop entry point
      lib.rs             ← pub shell_app() component
      layout/            ← topnav, sidebar, carousel, centre panel
      panels/            ← launcher, notifications, help
  cosmix-files/
    src/
      lib.rs             ← pub files_panel() (new)
      main.rs            ← unchanged standalone binary
  cosmix-mon/
    src/
      lib.rs             ← pub mon_panel() (new)
      main.rs            ← unchanged standalone binary
  cosmix-settings/
    src/
      lib.rs             ← pub settings_panel() (new)
      main.rs            ← unchanged standalone binary
```

cosmix-edit and cosmix-view remain standalone — they are task-focused tools that float as
separate windows. They do not need lib targets.

---

## Key Constraints and Non-Goals

**cosmix-shell is NOT a window manager.** It cannot:
- Control z-order of other application windows
- Tile or manage windows it did not create
- Act as a Wayland compositor

**cosmix-shell IS:**
- A sovereign application window that happens to contain a world of apps
- A Wayland client (or browser tab) that speaks AMP to the mesh
- The primary user-facing surface of the cosmix stack

If a true compositor-level shell is desired in the future, `smithay` (pure Rust Wayland
compositor library) would be the foundation. cosmix-shell would then become a Wayland client
rendered by a cosmix-authored compositor. That is out of scope for the current roadmap.

---

## Related Documents

- `2026-03-27-oklch-theme-system.md` — CSS custom property theme system for the shell
- `2026-03-26-appmesh-ecosystem-roadmap.md` — Overall roadmap including shell phases
- `2026-03-26-dioxus-responsive-layouts.md` — Dioxus layout patterns
- `2026-03-27-alternate-compositors.md` — Compositor research
- `2026-03-09-amp-v04-cosmix-specification.md` — AMP protocol specification
- `2026-03-07-daemon-architecture.md` — Daemon/service architecture
