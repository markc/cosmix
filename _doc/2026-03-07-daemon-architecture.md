# Cosmix Daemon Architecture

Date: 2026-03-07

## Problem

Today cosmix is a CLI tool that connects to Wayland fresh on every invocation, forks a child to hold clipboard state, and has no persistent presence. This works for one-shot commands but fails for:

- **Clipboard write** — requires fork() hack, orphans processes
- **Event subscriptions** — no way to react to window/clipboard/workspace changes
- **Port system** — ARexx model needs a running daemon to route messages
- **Mesh bridge** — connecting to meshd requires persistent WebSocket
- **Web bridge** — markweb integration needs persistent connection
- **Performance** — Wayland roundtrips on every CLI call add latency

## Architecture: Single Binary, Two Modes

cosmix stays as one binary with two execution modes:

    cosmix <command>        CLI mode (one-shot, talks to daemon or direct)
    cosmix daemon           Daemon mode (persistent, event-driven)

CLI mode first tries to reach the daemon via Unix socket. If the daemon is not running, it falls back to direct Wayland connection (current behavior). This keeps the tool useful without requiring daemon setup.

```
                         cosmix daemon
  +---------------------------------------------------------+
  |                                                         |
  |  +-------------+  +--------------+  +-----------------+ |
  |  | Wayland     |  | Lua Runtime  |  | Event Bus       | |
  |  | Connection  |  | (mlua/JIT)   |  | (tokio bcast)   | |
  |  |             |  |              |  |                 | |
  |  | - toplevel  |  | - cosmix.*   |  | - window_opened | |
  |  | - workspace |  | - scripts    |  | - clip_change   | |
  |  | - clipboard |  | - hot-reload |  | - ws_switch     | |
  |  | - seat      |  | - event sub  |  | - port_message  | |
  |  +------+------+  +------+-------+  +--------+--------+ |
  |         |                |                   |           |
  |  +------+----------------+-------------------+--------+  |
  |  |                Core State                          |  |
  |  |  Arc<RwLock<DaemonState>>                          |  |
  |  |  windows, workspaces, clipboard, ports, config     |  |
  |  +---+--------------------+-------------------+-------+  |
  |      |                    |                   |          |
  |  +---+-------+  +--------+-------+  +--------+-------+  |
  |  | CLI Socket|  | Port Router    |  | Bridge Manager |  |
  |  | (IPC)     |  | (ARexx model)  |  |                |  |
  |  | /run/user |  | /run/user/     |  | - meshd (WS)   |  |
  |  | cosmix.   |  | cosmix/ports/  |  | - markweb (WS) |  |
  |  | sock      |  | *.sock         |  | - future...    |  |
  |  +-----------+  +----------------+  +----------------+  |
  +---------------------------------------------------------+
```

## Core Components

### 1. Wayland Connection (Persistent)

Instead of connecting per invocation, the daemon maintains a single Wayland connection with a blocking_dispatch loop on a dedicated thread:

```rust
struct WaylandService {
    conn: Connection,
    state: Arc<RwLock<WaylandState>>,
    event_tx: broadcast::Sender<WaylandEvent>,
}

enum WaylandEvent {
    WindowOpened { app_id: String, title: String },
    WindowClosed { app_id: String },
    WindowFocused { app_id: String },
    WorkspaceChanged { name: String },
    ClipboardChanged { mime_types: Vec<String> },
}
```

**Clipboard management** moves into the daemon naturally. The daemon holds the ZwlrDataControlSourceV1 directly (no fork needed). When set_clipboard is called, the daemon creates a new source and serves data from memory via the existing event loop. The source stays alive until another app copies (Cancelled event), which the daemon handles in its dispatch.

**Protocols bound once:**
- ext-foreign-toplevel-list-v1 (window listing and events)
- zcosmic-toplevel-info-v1 / zcosmic-toplevel-manager-v1 (COSMIC window control)
- ext-workspace-manager-v1 (workspace state)
- zwlr-data-control-manager-v1 (clipboard read/write)
- wl_seat (seat for input operations)

### 2. Event Bus

A tokio broadcast channel distributes events to all interested consumers:

```rust
enum DaemonEvent {
    Wayland(WaylandEvent),
    Port(PortEvent),
    Mesh(AmpMessage),
    Cli(CliRequest),
    Timer { id: String },
}
```

Lua scripts subscribe to events via cosmix.on("event_name", handler). The daemon calls registered handlers when events fire. This replaces polling patterns with reactive scripting.

### 3. Lua Runtime (Persistent VM)

The daemon hosts a single Lua VM that persists across requests. Scripts loaded at startup stay in memory. Hot-reload watches _lib/ and ~/.config/cosmix/ for changes.

```lua
-- Event-driven scripting (daemon mode only)
cosmix.on("window_opened", function(event)
    if event.app_id == "firefox" then
        cosmix.maximize("firefox")
    end
end)

cosmix.on("clipboard_changed", function()
    local text = cosmix.clipboard()
    if text:match("^https?://") then
        cosmix.notify("URL copied", text)
    end
end)

-- Periodic tasks
cosmix.every(60000, function()
    -- check something every minute
end)
```

All existing cosmix.* functions continue to work, operating on persistent state instead of reconnecting each time.

### 4. CLI Socket (IPC)

The daemon listens on /run/user/$UID/cosmix/cosmix.sock for CLI requests:

```
Protocol: length-prefixed JSON over Unix socket
Request:  { "command": "list-windows" }
Response: { "ok": true, "data": [...] }
Request:  { "command": "run", "script": "winpick" }
Response: { "ok": true }
Request:  { "command": "set-clipboard", "text": "hello" }
Response: { "ok": true }
```

Why JSON (not AMP) for CLI-to-daemon IPC: CLI requests are simple command/response pairs. JSON is trivially parseable and has no ambiguity. AMP is for cross-node mesh communication where the three-reader principle (machine, human, AI) matters.

**Auto-start option:** CLI can optionally start the daemon if it is not running. For systemd-managed setups, the daemon is always running.

### 5. Port Router (ARexx Model)

The daemon is the central message router. Apps register ports, scripts address them:

```
/run/user/$UID/cosmix/ports/
    cosmic-files.sock       cosmic-files registered its port
    cosmic-edit.sock        cosmic-edit registered its port
    cosmic-term.sock        cosmic-term registered its port
```

The daemon discovers ports by scanning this directory (inotify watch). When a Lua script calls cosmix.port("cosmic-files"):open("/home"), the daemon:

1. Looks up cosmic-files in the port registry
2. Connects to (or reuses connection to) cosmic-files.sock
3. Sends an AMP request with command: open
4. Returns the AMP response to Lua

```lua
-- The ARexx experience
local files = cosmix.port("cosmic-files")
files:open("/home/cosmic/Documents")

local edit = cosmix.port("cosmic-edit")
edit:open("/tmp/notes.txt")
local selection = edit:selection()

-- Cross-node (transparent, meshd routes it)
local remote = cosmix.port("deploy.markweb.mko")
remote:status()
```

**Port protocol:** AMP messages over Unix sockets (local) or WebSocket (remote via meshd). The same message format works locally and across the mesh.

### 6. Bridge Manager

Persistent connections to external systems:

#### meshd Bridge
- Connects to meshd Unix socket (/run/meshd/meshd.sock) or WebSocket
- Registers as cosmix.<nodename>.amp in the mesh
- Routes AMP messages between Lua scripts and remote nodes
- Receives AMP messages addressed to this node's cosmix ports

```lua
-- Send to remote node
cosmix.mesh.send("deploy.markweb.mko", { command = "status" })

-- Receive from remote (event-driven)
cosmix.on("mesh_message", function(msg)
    if msg.command == "notify" then
        cosmix.notify(msg.summary, msg.body)
    end
end)
```

#### markweb Bridge
- Connects to markweb Reverb WebSocket endpoint
- Authenticates with API token
- Enables bidirectional desktop-to-web communication
- Web UI can trigger desktop actions; desktop can push state to web

```lua
-- Push to web dashboard
cosmix.web.send("window-state", { windows = cosmix.windows() })

-- Receive from web (user clicked "deploy" in browser)
cosmix.on("web_message", function(msg)
    if msg.type == "deploy" then
        local result = cosmix.exec("cd /path && git pull && make")
        cosmix.web.send("deploy-result", { ok = true, output = result })
    end
end)
```

### 7. State Management

All daemon state lives in a single protected structure:

```rust
struct DaemonState {
    // Wayland state (updated by Wayland thread)
    windows: HashMap<u32, ToplevelInfo>,
    workspaces: HashMap<u32, WorkspaceInfo>,
    clipboard_content: Option<String>,
    clipboard_source: Option<ZwlrDataControlSourceV1>,

    // Port registry
    ports: HashMap<String, PortConnection>,

    // Bridge connections
    mesh_connected: bool,
    web_connected: bool,

    // Lua state
    script_watchers: Vec<PathBuf>,

    // Config
    config: DaemonConfig,
}
```

## Module Structure

```
crates/cosmix-daemon/src/
    main.rs                 CLI entry + daemon startup
    cli.rs                  CLI command handling (direct + daemon-routed)
    daemon/
        mod.rs              Daemon startup, tokio runtime, shutdown
        state.rs            DaemonState, thread-safe wrapper
        events.rs           DaemonEvent enum, broadcast setup
        config.rs           TOML config loading
    wayland/
        mod.rs              Persistent Wayland connection (refactored)
        toplevel.rs         Window management (existing, adapted)
        workspace.rs        Workspace management (existing, adapted)
        clipboard.rs        Clipboard (moved from dbus/, no fork)
    ipc/
        mod.rs              Unix socket server for CLI
        protocol.rs         JSON request/response types
        port_router.rs      ARexx port discovery and routing
    bridge/
        mod.rs              Bridge manager
        mesh.rs             meshd connection (WebSocket/Unix)
        web.rs              markweb connection (Reverb WebSocket)
    lua/
        mod.rs              Lua VM setup, hot-reload
        api.rs              cosmix.* function registration
        events.rs           cosmix.on() event subscription
        ports.rs            cosmix.port() Lua bindings
    dbus/
        mod.rs
        notify.rs           Notifications (existing)
    desktop.rs              .desktop file parser (existing)
    dialog.rs               iced dialogs (existing)
```

## Crate Dependencies (New/Changed)

```toml
# Added to workspace
notify = "7"                # File watcher for hot-reload
inotify = "0.11"            # Port directory watching
tokio-tungstenite = "0.26"  # WebSocket for bridges
toml = "0.8"                # Config file
directories = "6"           # XDG paths
```

## Config File

~/.config/cosmix/config.toml:

```toml
[daemon]
socket = "/run/user/1000/cosmix/cosmix.sock"
port_dir = "/run/user/1000/cosmix/ports"

[mesh]
enabled = false
meshd_socket = "/run/meshd/meshd.sock"
node_name = "cachyos"

[web]
enabled = false
url = "wss://web.kanary.org/reverb"
token = ""

[lua]
watch_dirs = ["_lib", "~/.config/cosmix/lib"]
startup_scripts = ["~/.config/cosmix/init.lua"]

[clipboard]
serve = true
```

## systemd Integration

```ini
# ~/.config/systemd/user/cosmix.service
[Unit]
Description=Cosmix Desktop Automation Daemon
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/local/bin/cosmix daemon
Restart=on-failure
Environment=WAYLAND_DISPLAY=wayland-0

[Install]
WantedBy=graphical-session.target
```

## Backward Compatibility

The refactor is non-breaking:

| Current behavior | After daemon | Notes |
|---|---|---|
| cosmix lw (direct Wayland) | cosmix lw (via daemon, fallback to direct) | Transparent |
| cosmix clipboard (direct) | cosmix clipboard (via daemon, fallback) | Faster via daemon |
| cosmix set-clipboard (fork) | cosmix set-clipboard (daemon holds source) | No more orphan processes |
| cosmix run script.lua (fresh VM) | cosmix run script.lua (daemon VM or fresh) | Event subscriptions only in daemon |
| cosmix shell (REPL) | cosmix shell (connects to daemon VM) | Shared state with running scripts |
| cosmix dialog (iced) | cosmix dialog (unchanged, always direct) | Dialogs are inherently one-shot |

## Integration with Existing Codebases

### From appmesh

**Carry forward into cosmix:**
- AppMeshPort trait becomes the port registration interface in cosmix-port crate
- AMP wire format used for port-to-daemon and mesh communication
- NotifyPort already reimplemented as dbus::notify
- ClipboardPort already reimplemented natively
- MailPort (JMAP, 1115 LOC) becomes a Lua-callable port
- InputPort + EIS: future input injection via daemon
- WindowsPort already reimplemented via Wayland protocols

**Not carried forward:**
- PHP MCP server (replaced by Lua runtime)
- QML UI (replaced by iced dialogs)
- KDE-specific D-Bus (replaced by COSMIC protocols)

The 89 appmesh tools become Lua functions callable through the daemon. The Rust port implementations move into cosmix-daemon or remain as separate port binaries that register with the daemon.

### From nodemesh

**Integration point:** meshd bridge
- Daemon connects to meshd Unix socket
- Registers as a mesh peer
- AMP messages route transparently between Lua scripts and remote nodes
- No code moves: meshd stays independent, cosmix is a client

**Reusable crate:** nodemesh/crates/amp/ can be shared as a workspace dependency or vendored into cosmix.

### From markweb

**Integration point:** Reverb WebSocket bridge
- Daemon connects to markweb WebSocket endpoint
- Authenticates, subscribes to channels
- Desktop scripts can trigger web actions and vice versa
- markweb adds a cosmix channel in Reverb for bidirectional events

No Laravel code changes needed initially. Just use existing WebSocket infrastructure.

## Implementation Phases

### Phase 1: Daemon Core (foundation)
- Tokio runtime with graceful shutdown
- Persistent Wayland connection (refactor connect() to long-lived)
- Clipboard management in daemon (eliminate fork)
- CLI-to-daemon Unix socket IPC
- Existing commands work through daemon
- systemd service file

### Phase 2: Event System
- tokio::broadcast event bus
- Wayland events (window open/close/focus, workspace switch, clipboard change)
- cosmix.on() Lua API for event subscription
- cosmix.every() for periodic tasks
- Persistent Lua VM in daemon

### Phase 3: Port System
- Port directory with inotify watch
- AMP message routing between ports and Lua
- cosmix.port("name"):command() Lua API
- Port protocol specification (AMP over Unix socket)
- Example port: standalone mail service (from appmesh MailPort)

### Phase 4: Mesh Bridge
- meshd WebSocket/Unix socket client
- Register as cosmix.nodename.amp
- cosmix.mesh.* Lua API
- Cross-node port addressing (cosmix.port("service.node"))

### Phase 5: Web Bridge
- markweb Reverb WebSocket client
- cosmix.web.* Lua API
- Bidirectional event channel
- Desktop state push to web dashboard

### Phase 6: Advanced
- Input injection (EIS/libei integration)
- AT-SPI2 UI introspection
- Hot-reload with file watching
- cosmix-port crate for Layer 3 (libcosmic PR)

## Design Principles

1. **Always fallback to direct** — daemon is optional, CLI works without it
2. **Lua is the user interface** — Rust adds capabilities, Lua consumes them
3. **AMP for interop, JSON for internal** — AMP crosses boundaries (apps, nodes, humans), JSON stays inside the daemon
4. **One binary** — no separate daemon binary, cosmix daemon is just a mode
5. **Progressive opt-in** — mesh and web bridges are disabled by default
6. **No Python. Ever.** — Lua for scripting, Rust for systems, PHP+React for web
7. **Native COSMIC** — no external tools (wl-copy, grim, zenity), use Wayland protocols and iced directly
