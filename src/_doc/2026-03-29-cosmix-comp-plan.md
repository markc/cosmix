# cosmix-comp: Implementation Plan

> **Status:** Deferred — pending dioxus-desktop field results  
> **Audience:** Claude Code session kickoff document  
> **Goal:** A thin Smithay-based Wayland compositor that delegates all window management policy to `cosmix-shell` via a binary postcard socket, giving Dioxus apps full placement authority without depending on cosmic-comp or any third-party compositor.

---

## 1. Context and Motivation

### The Problem

Standard Wayland clients are supplicants. The compositor owns all window placement policy — stacking order, sticky surfaces, always-on-top, geometry — and may ignore or deny client requests at will. Dioxus apps running via `dioxus-desktop` (WebKitGTK/winit) are further removed: GTK abstracts the Wayland protocol, reducing what can even be requested. Achieving behaviours like sticky overlays, custom stacking, or shell-chrome integration requires compositor-level authority, not client-level requests.

### The Solution: River's Architecture in Rust

River WM demonstrated that splitting the compositor (mechanism) from the window manager (policy) via a stable protocol is viable and produces a clean system. The insight: if the window manager is a separate process with full placement authority, it can be written in anything and implement any policy — including a Dioxus UI acting as the shell.

`cosmix-comp` adopts this architecture with the following differences from River:

- Written in Rust using Smithay (not Zig/wlroots)
- Policy delegated over a `postcard` binary socket (not `river-window-management-v1` or AMP)
- No GPL entanglement; fully owned by the Cosmix project
- `cosmix-shell` (a Dioxus app) IS the window manager — not a plugin to someone else's compositor
- Designed to eventually collapse the compositor/shell boundary further as `dioxus-native` matures

### Architectural Position

```
Hardware: DRM/KMS, libinput
          │
    cosmix-comp          ← Smithay compositor kernel
          │                 no WM policy lives here
    postcard socket      ← binary, length-prefixed, high-frequency safe
          │
    cosmix-shell         ← Dioxus app, IS the window manager
          │                 owns: placement, stacking, focus, sticky, chrome
    Wayland socket
          │
    other apps           ← normal Wayland clients (Dioxus or otherwise)
          │
    AMP mesh             ← orchestration layer above the compositor
```

---

## 2. Protocol Architecture

This is a foundational decision that affects the entire Cosmix stack, not just `cosmix-comp`. The compositor boundary exposed the need for a clear protocol stratification across all Cosmix layers.

### Why Not AMP at the Compositor Boundary

AMP with markdown frontmatter is the right format for Cosmix's **orchestration layer** — infrequent, human-readable messages between services, AI inference requests, JMAP events, agent coordination. The overhead of parsing frontmatter per message is negligible when messages are sparse.

The compositor↔WM channel is a completely different traffic class:

- Pointer motion fires at display refresh rate — 60-165 events/sec, continuously
- Surface commits happen on every frame for every animating client
- Frame callbacks are on the compositor's critical render path
- Configure/ack cycles are synchronous round-trips that block client rendering

Parsing frontmatter on every pointer move adds measurable latency jitter on exactly the path where latency matters most. AMP is not the wrong protocol for Cosmix — it's the wrong protocol *for this boundary*.

### The Pragmatic Split: Protocol by Layer

The key principle: **if a human or Lua script needs to read or write it, keep it AMP. If it's Rust-to-Rust on a hot path, use postcard binary.** This boundary maps cleanly onto Cosmix's existing architectural layers.

| Layer | Traffic type | Protocol |
|---|---|---|
| compositor ↔ shell | Pointer, frame, surface events | **postcard binary** |
| intra-node fast paths | Health, metrics, event streams | **postcard binary** |
| vector/embedding payloads | f32 arrays for pgvector/Ollama | **postcard binary** |
| inter-service orchestration | Agent commands, JMAP events | **AMP (JSON body)** |
| node-to-node control plane | Config, topology, capability | **AMP (JSON body)** |
| Lua scripting interface | All orchestration | **AMP (JSON body)** |
| bulk data transfers | Log streams, file chunks | **postcard or raw bytes** |

### Why postcard for the Hot Path

[`postcard`](https://github.com/jamesmunns/postcard) is a `no_std`-compatible, compact binary serde format. Key properties:

- Derives directly from existing `serde` types — no separate schema or IDL
- Minimal wire size (no field names, variable-length encoding)
- Zero external dependencies beyond `serde`
- The Rust enum definition IS the protocol spec — schema and implementation cannot diverge
- A logging/debug layer can be added as a thin wrapper without changing the transport

The one cost: not human-readable on the wire. Mitigated by a `--features debug-ipc` flag that decodes and logs frames to stderr during development.

### Why AMP Remains Right Above the Compositor

AMP's properties — human-readable, Lua-accessible, schema-tolerant across versions, legible to any language — are genuine operational advantages for the orchestration layer:

- Lua scripts driving cross-app behaviour read JSON naturally; postcard requires a Rust decoder
- `socat` onto an AMP socket during a 2am incident lets you read what's happening in plain text
- Rolling upgrades across nodes: AMP with `#[serde(default)]` handles additive changes transparently; postcard requires careful schema discipline
- External integrators (RMP Systems, future Cosmix contributors) can speak AMP without a Rust toolchain

The compositor socket being binary is fine because nothing external should ever address it directly.

---

## 3. Impact on Existing Codebase

Audited 2026-03-29 against the live monorepo. The protocol split is **additive** — no existing AMP infrastructure changes.

### Current state

- **No postcard anywhere** in the workspace — zero dependencies, zero usage
- **AMP text format** is the sole IPC protocol across all 26 crates
- **Traffic patterns are moderate** — event-driven messaging and periodic polling, not bandwidth-constrained
- **cosmix-hubd** parses/routes every message; its `hub.tap` observability depends on human-readable AMP headers

### What stays AMP (no changes needed)

| Service | Reason |
|---|---|
| cosmix-hubd | Routing + tap observability requires text headers |
| cosmix-lib-client | WebSocket AMP client, all apps depend on it |
| cosmix-logd | Human-readable logging is the entire purpose |
| cosmix-maild | JMAP is JSON by RFC spec |
| cosmix-configd | Low-frequency config distribution |
| All GUI apps | Event-driven commands through the hub |
| cosmix-lib-mesh | Node-to-node control plane, rolling upgrades need schema tolerance |
| Lua scripting | Scripts read JSON naturally; postcard requires a Rust decoder |

### Preparation items (minor, not blocking)

**cosmix-mond metrics types** — `SystemStatus`, `DiskInfo`, `ProcessInfo` currently derive `Serialize` only. Adding `Deserialize` (one-line change) makes them postcard-ready. This is the natural first postcard adoption point if the binary transport pattern needs proving before compositor work begins. Not urgent.

**cosmix-indexd** — Already uses direct Unix sockets (not AMP) for vector embeddings. When AI inference integration happens, f32 embedding arrays should go postcard, not JSON. The current architecture is accidentally correct here.

**cosmix-shell dual-socket design** — When shell work begins, it must be architected from day one with two communication channels:
- **Down:** postcard binary socket to `cosmix-comp` (placement, input, surface lifecycle)
- **Up:** AMP WebSocket to `cosmix-hubd` (orchestration, agent commands, Lua scripts, mesh)

These are distinct sockets with distinct protocols. The shell is the bridge between the compositor layer and the orchestration layer. This boundary must not be muddled — `cosmix-shell` should have separate modules for each connection, not a unified "IPC" abstraction that tries to paper over the difference.

### What this means for implementation order

`cosmix-comp` and its `ipc/` module are entirely new crates. They do not modify existing AMP infrastructure. The only integration point is `cosmix-shell` being a client of both protocols simultaneously. All existing services, daemons, and GUI apps continue to use AMP through the hub exactly as they do today.

---

## 4. Scope and Non-Goals

### In Scope

- Smithay-based DRM/KMS and Wayland socket initialisation
- Surface lifecycle management (create, map, unmap, destroy)
- Input routing (seat, keyboard, pointer, touch)
- XWayland support (optional, toggled at build time)
- `postcard` binary framing over Unix socket for WM policy delegation
- `zwlr-layer-shell-v1` support for shell surfaces (panels, overlays, HUDs)
- Basic damage tracking and frame scheduling
- `xdg-shell` surface support
- `cosmix-shell` stub: minimal Dioxus window manager client over the binary socket

### Out of Scope (for this milestone)

- Animations, blur, shadows — no compositor effects
- Virtual desktops / workspaces (implement in `cosmix-shell` policy layer)
- Screencasting / PipeWire (`xdg-output-management` deferred)
- GPU compositing / hardware overlay planes (software rendering first)
- Multi-GPU setups
- Tablet/stylus input
- Any general-purpose compositor ambitions — this is a Cosmix-internal component

---

## 5. Dependency Stack

```toml
[dependencies]
# Smithay — the compositor foundation
smithay = { version = "0.4", features = [
    "backend_drm",
    "backend_libinput",
    "backend_udev",
    "backend_winit",        # for nested/dev testing
    "renderer_gl",
    "wayland_frontend",
    "xwayland",             # feature-flagged
    "desktop",              # smithay::desktop::Space abstractions
] }

# Binary IPC (compositor ↔ shell socket)
postcard = { version = "1", features = ["alloc"] }
serde = { version = "1", features = ["derive"] }

# Async runtime (socket server, event dispatch)
tokio = { version = "1", features = ["full"] }

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# Config
toml = "0.8"

# Error handling
anyhow = "1"
thiserror = "1"

[features]
default = []
xwayland = ["smithay/xwayland"]
winit-backend = ["smithay/backend_winit"]   # dev/nested mode
debug-ipc = []                              # hex-dump + decode IPC frames to stderr
```

Key crate notes:
- **Smithay 0.4** is the current stable series. Check for 0.5 at implementation time.
- **`smithay::desktop::Space`** handles most surface tracking boilerplate — use it, don't hand-roll surface lists.
- **`backend_winit`** enables nested compositor mode for development without a spare TTY.
- **`postcard` with `alloc`** feature required for `Vec`-based serialisation.

---

## 6. Module Structure

```
cosmix-comp/
├── Cargo.toml
├── src/
│   ├── main.rs              # entry point, backend selection, event loop
│   ├── state.rs             # CompositorState: all shared state
│   ├── handlers/
│   │   ├── mod.rs
│   │   ├── compositor.rs    # CompositorHandler, SurfaceData
│   │   ├── xdg_shell.rs     # XdgShellHandler: toplevel/popup lifecycle
│   │   ├── layer_shell.rs   # WlrLayerShellHandler: panels, overlays
│   │   ├── seat.rs          # SeatHandler: keyboard, pointer, touch
│   │   └── xwayland.rs      # XWaylandHandler (feature-flagged)
│   ├── backend/
│   │   ├── mod.rs
│   │   ├── udev.rs          # DRM/KMS/udev production backend
│   │   └── winit.rs         # nested/dev backend
│   ├── ipc/
│   │   ├── mod.rs
│   │   ├── protocol.rs      # CompToShell / ShellToComp enums (serde + postcard)
│   │   ├── server.rs        # Unix socket listener, frame dispatch
│   │   ├── client.rs        # Handle to connected cosmix-shell
│   │   └── debug.rs         # frame hex-dump + decode (debug-ipc feature)
│   └── config.rs            # toml config loading
```

Note: the IPC module is `ipc/`, not `amp/`. AMP lives in the orchestration layer above `cosmix-shell`. The compositor has no AMP dependency.

---

## 7. IPC Protocol Definition

The compositor↔shell socket uses `postcard` binary framing. The Rust enums are the authoritative protocol definition — there is no separate IDL.

### Compositor → Shell

```rust
#[derive(Debug, Serialize, Deserialize)]
pub enum CompToShell {
    // Surface lifecycle
    SurfaceMapped    { id: SurfaceId, app_id: String, title: String, initial_geometry: Rect },
    SurfaceUnmapped  { id: SurfaceId },
    SurfaceDestroyed { id: SurfaceId },
    TitleChanged     { id: SurfaceId, title: String },
    AppIdChanged     { id: SurfaceId, app_id: String },

    // Input events — focus management delegated to shell
    PointerMoved     { id: SurfaceId, x: f64, y: f64 },
    PointerEntered   { id: SurfaceId, x: f64, y: f64 },
    PointerLeft      { id: SurfaceId },
    KeyEvent         { key: u32, state: KeyState, modifiers: Modifiers },

    // Output topology
    OutputAdded      { id: OutputId, name: String, geometry: Rect, refresh: u32 },
    OutputRemoved    { id: OutputId },
}
```

### Shell → Compositor

```rust
#[derive(Debug, Serialize, Deserialize)]
pub enum ShellToComp {
    // Handshake — first message sent after connect
    Register         { name: String, version: u32 },

    // Placement
    SetGeometry      { id: SurfaceId, geometry: Rect },
    SetFullscreen    { id: SurfaceId, output: Option<OutputId> },
    UnsetFullscreen  { id: SurfaceId },
    SetMaximized     { id: SurfaceId },
    UnsetMaximized   { id: SurfaceId },

    // Stacking
    RaiseToTop       { id: SurfaceId },
    LowerToBottom    { id: SurfaceId },
    SetLayer         { id: SurfaceId, layer: StackLayer },

    // Focus
    SetKeyboardFocus { id: SurfaceId },
    SetPointerFocus  { id: SurfaceId, x: f64, y: f64 },
    ClearFocus,

    // Lifecycle
    CloseSurface     { id: SurfaceId },
}
```

`SurfaceId` and `OutputId` are `[u8; 16]` (UUID-style), generated by the compositor on map/add.

### Wire Framing

4-byte big-endian length prefix followed by `postcard`-serialised body:

```rust
// Sending
async fn send_msg(writer: &mut WriteHalf<'_>, msg: &CompToShell) -> anyhow::Result<()> {
    let bytes = postcard::to_stdvec(msg)?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;

    #[cfg(feature = "debug-ipc")]
    tracing::debug!(direction = "out", ?bytes, "IPC frame");

    Ok(())
}

// Receiving
async fn recv_msg(reader: &mut ReadHalf<'_>) -> anyhow::Result<ShellToComp> {
    let len = reader.read_u32().await? as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;

    #[cfg(feature = "debug-ipc")]
    tracing::debug!(direction = "in", ?buf, "IPC frame");

    Ok(postcard::from_bytes(&buf)?)
}
```

### Schema Evolution Rules

postcard is not self-describing. Additive changes require discipline:

- **Adding an enum variant:** append only — never reorder existing variants (postcard encodes by index)
- **Adding a field to a struct variant:** use `Option<T>`; old senders omitting it deserialise to `None`
- **Removing or renaming anything:** bump `version` in `Register`, handle both versions explicitly
- **Never reorder struct fields** — postcard serialises positionally

---

## 8. CompositorState

```rust
pub struct CompositorState {
    // Smithay core
    pub display: Display<Self>,
    pub space: Space<Window>,
    pub loop_handle: LoopHandle<'static, Self>,

    // Seat / input
    pub seat: Seat<Self>,
    pub seat_name: String,
    pub keyboard: Option<KeyboardHandle<Self>>,
    pub pointer: Option<PointerHandle<Self>>,

    // Surfaces
    pub surfaces: HashMap<SurfaceId, SurfaceRecord>,
    pub pending_configure: HashSet<SurfaceId>,

    // IPC — None until cosmix-shell connects and sends Register
    pub shell_client: Option<IpcShellClient>,
    pub queued_events: Vec<CompToShell>,  // flushed on Register receipt

    // Output
    pub output_manager: OutputManagerState,

    // XWayland (feature-flagged)
    #[cfg(feature = "xwayland")]
    pub xwayland: Option<XWayland>,
}

pub struct SurfaceRecord {
    pub id: SurfaceId,
    pub wl_surface: WlSurface,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub geometry: Rect,
    pub layer: StackLayer,
}
```

---

## 9. Backend Strategy

### Development: winit (nested)

`smithay::backend::winit` runs `cosmix-comp` as a nested compositor inside an existing session. Enable with `--features winit-backend` and `--backend winit`. No spare TTY required for development.

### Production: udev/DRM

`smithay::backend::udev` + `libinput` for bare-metal. Enumerates GPU devices via udev, opens DRM nodes with logind/seatd permissions, creates GBM buffers and EGL contexts per CRTC, handles monitor hotplug. Target single-GPU, single-monitor for the initial implementation.

---

## 10. Implementation Phases

### Phase 0: Scaffold (est. 2-3 days)

- [ ] `cargo new cosmix-comp`
- [ ] Add all dependencies per §5
- [ ] Module skeleton per §6
- [ ] `main.rs`: backend selection via `--backend [winit|udev]`
- [ ] Winit backend: open a window, clear to solid colour, event loop running
- [ ] Tracing to stderr, `RUST_LOG` respected
- [ ] Config: `~/.config/cosmix/comp.toml` — `socket_path`, `backend`, `log_level`
- **Exit criterion:** `cargo run --features winit-backend -- --backend winit` opens a grey window with no panics.

### Phase 1: Wayland Surface Basics (est. 1 week)

- [ ] `CompositorHandler` — `commit()`, surface roles
- [ ] `XdgShellHandler` — toplevel map/unmap, popup handling
- [ ] `smithay::desktop::Space` integration — `map_element()`, `unmap_element()`
- [ ] Rendering: iterate Space elements, draw surface textures to output
- [ ] Basic `SeatHandler` — pointer motion, button events, keyboard events
- [ ] Hardcoded cascade placement — no IPC yet
- **Exit criterion:** `foot` or `weston-terminal` runs as a Wayland client and accepts keyboard input.

### Phase 2: Binary IPC Server (est. 3-4 days)

- [ ] `ipc/server.rs`: `tokio::net::UnixListener` on configured socket path
- [ ] Accept single connection (cosmix-shell only)
- [ ] Deserialise incoming `ShellToComp` frames via postcard
- [ ] Serialise and send `CompToShell` events on surface lifecycle
- [ ] `Register` handshake: flush `queued_events` on receipt
- [ ] Detect shell disconnect, re-enter cascade fallback, log warning
- [ ] `--features debug-ipc` decode mode working
- **Exit criterion:** Test binary connects, sends `Register`, prints decoded `SurfaceMapped` events to stdout.

### Phase 3: IPC Window Management (est. 1 week)

- [ ] `SetGeometry` — resize/reposition via `xdg_toplevel.configure()`
- [ ] `RaiseToTop` / `LowerToBottom` — `Space::raise_element()`
- [ ] `SetKeyboardFocus` — `seat.get_keyboard().set_focus()`
- [ ] `SetPointerFocus` — `wl_pointer.enter()` to target surface
- [ ] `CloseSurface` — `xdg_toplevel.send_close()`
- [ ] `SetFullscreen` / `SetMaximized`
- [ ] Pointer enter/leave tracking → `PointerEntered` / `PointerLeft` to shell
- **Exit criterion:** Test binary can raise/lower/close/move windows by sending postcard commands over the socket.

### Phase 4: Layer Shell (est. 3-4 days)

- [ ] `WlrLayerShellHandler` impl
- [ ] Layer surface anchoring and exclusive zones
- [ ] Stacking layer surfaces above/below regular toplevels
- [ ] Report layer surfaces via `SurfaceMapped` with `layer` field set
- **Exit criterion:** A Dioxus app using `zwlr_layer_shell_v1` anchors to screen top as a panel and survives other windows opening/closing.

### Phase 5: cosmix-shell Stub (est. 1 week)

Minimal Dioxus desktop app acting as window manager:

- [ ] IPC client connecting to `cosmix-comp` socket, sending `Register`
- [ ] Receives `SurfaceMapped` → renders draggable title bar / window chrome in Dioxus
- [ ] Sends `SetGeometry` on drag, `RaiseToTop` on click, `CloseSurface` on X button
- [ ] Window list as Dioxus state, taskbar strip rendered
- [ ] Shell's own surface registered as layer-shell overlay — always visible, sticky
- [ ] Separate AMP connection upward to Cosmix mesh (distinct socket from compositor IPC)
- **Exit criterion:** Open `foot`, drag it, close it. cosmix-shell panel stays pinned. AMP events for window open/close propagate upward to the mesh.

### Phase 6: XWayland (optional, est. 3-4 days)

- [ ] Feature-flag `xwayland`
- [ ] `XWaylandHandler` — spawn Xwayland, bridge X11 surfaces into Space
- [ ] Report X11 windows via same `SurfaceMapped` IPC path
- [ ] WM hints passthrough where meaningful

---

## 11. Key Smithay Patterns

### Surface Commit Handler

```rust
impl CompositorHandler for CompositorState {
    fn compositor_state(&mut self) -> &mut CompositorClientState {
        &mut self.compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if let Some(window) = self.space.elements()
            .find(|w| w.wl_surface().as_ref() == Some(surface))
        {
            if !self.is_mapped(surface) {
                self.handle_surface_mapped(surface);
            }
        }
    }
}
```

### Applying Shell Geometry Command

```rust
fn apply_geometry(&mut self, id: SurfaceId, rect: Rect) {
    if let Some(record) = self.surfaces.get_mut(&id) {
        record.geometry = rect;
        if let Some(toplevel) = self.get_toplevel(&record.wl_surface) {
            toplevel.with_pending_state(|state| {
                state.size = Some((rect.w, rect.h).into());
            });
            toplevel.send_configure();
        }
        self.space.map_element(
            self.get_window(&record.wl_surface).unwrap(),
            (rect.x, rect.y),
            false,
        );
    }
}
```

---

## 12. Configuration File

`~/.config/cosmix/comp.toml`:

```toml
[compositor]
backend = "udev"          # or "winit" for dev
socket_path = "/run/user/1000/cosmix-comp.sock"
log_level = "info"

[xwayland]
enabled = false

[seat]
name = "seat0"

[output]
# future: multi-monitor config
```

---

## 13. Failure Modes and Mitigations

| Scenario | Mitigation |
|---|---|
| cosmix-shell not connected | Fall back to cascade placement, no focus management |
| cosmix-shell crashes | Detect disconnect, re-enter fallback, flush event queue on reconnect |
| Surface maps before shell connects | Queue `SurfaceMapped` events, flush on `Register` |
| Shell sends invalid geometry | Clamp to output bounds before applying |
| postcard deserialise error | Log and drop the frame — do not panic; likely a version mismatch |
| XWayland fails to start | Log error, continue Wayland-only |
| DRM device unavailable | Fall back to winit if `--backend auto` |

---

## 14. Testing Approach

- **Unit:** postcard round-trip for every `CompToShell` and `ShellToComp` variant — add a test for each new variant at the time it's added
- **Schema regression:** serialise each message type, commit the raw bytes as fixtures, assert identical deserialisation after any change
- **Integration (winit mode):** spin compositor in a thread, connect a mock IPC client, map surfaces programmatically, assert correct event sequence emitted
- **Manual:** `--backend winit` under existing session, spawn `foot` and a Dioxus test app, drive placement via IPC
- **Hardware:** DRM backend on spare TTY, single monitor, before declaring Phase 3 complete

---

## 15. Future: Collapsing the Boundary

As `dioxus-native` matures (Blitz renderer, native widgets):

- `cosmix-comp` and `cosmix-shell` merge into a single process
- Dioxus renders shell chrome AND compositor output in the same frame
- The postcard Unix socket becomes an in-process `tokio::sync::mpsc` channel — same message types, zero serialisation cost, zero syscall overhead
- The compositor "window" becomes a Dioxus component tree

The postcard enum types survive this transition unchanged. The transport swaps under them. Keep `CompositorState` accessible enough that it can eventually be driven from a library interface rather than a socket endpoint.

---

## 16. Reference Material

- [Smithay book](https://smithay.github.io/smithay/) — read Desktop and Backends chapters first
- [anvil](https://github.com/Smithay/smithay/tree/master/anvil) — Smithay's reference compositor; read this before writing any `cosmix-comp` code
- [niri source](https://github.com/YaLTeR/niri) — production Smithay compositor, clean Rust, good patterns to study
- [postcard docs](https://docs.rs/postcard) — pay attention to the schema evolution guidance
- [river-window-management-v1 spec](https://isaacfreund.com/docs/wayland/river-window-management-v1) — read for design inspiration, do not implement
- [zwlr-layer-shell-v1 spec](https://wayland.app/protocols/wlr-layer-shell-unstable-v1) — implement this, MIT licensed
- [cage source](https://github.com/cage-kiosk/cage) — minimal kiosk compositor, useful DRM backend patterns

---

## 17. Kickoff Prompt for Claude Code

When starting a Claude Code session, open with:

> "We are implementing `cosmix-comp`, a thin Smithay-based Wayland compositor that delegates all window management policy to `cosmix-shell` via a `postcard` binary-framed Unix socket. The full design is in `cosmix-comp-plan.md`. Start at Phase 0: set up the Cargo project, module skeleton per §6, and winit backend scaffold. Do not implement any IPC or surface handling yet — just an event loop and a grey winit window with tracing output."

Advance one phase at a time. Do not skip ahead. The enum definitions in §7 are the IPC contract — do not rename variants or reorder fields without bumping the `Register` version and updating both sides. The IPC module is `ipc/`, not `amp/`. AMP lives above `cosmix-shell` in the orchestration mesh and has no presence inside the compositor.
