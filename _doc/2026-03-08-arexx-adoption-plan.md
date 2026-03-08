# Cosmix ARexx Adoption Plan — The Modern ARexx for COSMIC Desktop

> **Purpose:** Comprehensive implementation plan to adopt the full spirit of AmigaOS ARexx into cosmix, mapping every ARexx concept to Rust+Lua on the COSMIC desktop, extended with modern networking via nodemesh and web access via markweb.

> **Key Insight:** Linux has NO equivalent to ARexx or AppleScript. KDE has KWin scripting (JavaScript, window management only). GNOME has D-Bus extensions (introspection-based, no unified scripting layer). Neither provides a universal inter-application command bus where any app can be scripted by any other app. **Cosmix would be the first.**

---

## 0. What We Already Have (Current State Assessment)

Before planning what to build, here's what exists today:

| ARexx Concept | Current State | Gap |
|---|---|---|
| RexxMast daemon | `cosmix-daemon` with Lua runtime, event bus, Wayland/D-Bus integration, **port registry + discovery** | ~~No port registry, no script routing to apps~~ **DONE** |
| Named ports | `cosmix-port` crate — apps register commands on Unix sockets at `/run/user/$UID/cosmix/ports/{name}.sock` | ~~Daemon doesn't discover or route to these sockets~~ **DONE** |
| ADDRESS command | `cosmix.port("name")` in Lua — **connects to actual app sockets via registry** | ~~Doesn't connect to actual app sockets~~ **DONE** |
| RESULT variable | JSON `PortResponse { ok, data, error }` | No standardized RC codes (0/5/10/20) |
| Clip List | `DaemonState.clipboard_text` (single value) | No key/value store, no history, no SETCLIP/GETCLIP |
| Script execution | `cosmix run script.lua` — Lua runtime with 30+ cosmix.* functions | No pre-addressing, no script-per-app directories |
| Macro menus | None | Critical gap |
| Event system | `DaemonEvent` enum with broadcast channel, `cosmix.on()` / `cosmix.every()` Lua API | Timer scheduling not executing, PortMessage events never fired |
| Standard vocabulary | Ad-hoc per app (calc: calc/result/history, view: open/save/rotate) | No enforced standard |
| Port discovery | **PortRegistry with directory scanning, HELP handshake, heartbeat** | ~~Critical gap~~ **DONE** |
| Cross-node | nodemesh has WebSocket AMP routing, markweb has Laravel bridge | Not connected to cosmix |
| Apps with ports | cosmix-calc (6 commands), cosmix-view (10 commands) | cosmix-mail has no port |

**Bottom line:** The plumbing exists. The semantic layer that made ARexx powerful does not.

---

## 1. Architecture — Five Layers

```
┌─────────────────────────────────────────────────────────────────┐
│  Layer 5: NETWORK MESH                                         │
│  nodemesh AMP routing, markweb web gateway, AI agents          │
│  ADDRESS 'TEXTEDITOR.1@mko.amp' — cross-node scripting         │
├─────────────────────────────────────────────────────────────────┤
│  Layer 4: SOCIAL CONTRACT                                      │
│  Standard command vocabulary, port naming, About dialog,        │
│  HELP introspection, community script repository               │
├─────────────────────────────────────────────────────────────────┤
│  Layer 3: LUA SCRIPTING (the "ARexx language")                 │
│  ADDRESS equivalent, pre-addressed scripts, macro menus,        │
│  function libraries (Lua modules), script directories          │
├─────────────────────────────────────────────────────────────────┤
│  Layer 2: PER-APP INTEGRATION (the "ARexx port")               │
│  cosmix-port crate, command registration, standard commands,    │
│  RC error codes, HELP command, port naming (APPNAME.N)         │
├─────────────────────────────────────────────────────────────────┤
│  Layer 1: THE DAEMON (the "RexxMast")                          │
│  Port registry + discovery, script execution, Clip List,        │
│  event bus, message routing, process management                │
└─────────────────────────────────────────────────────────────────┘
```

---

## 2. Layer 1: The Daemon — RexxMast Equivalent

### 2.1 Port Registry and Discovery

The daemon must maintain a live registry of all running cosmix-port sockets.

**Implementation:**

```rust
// In daemon state
pub struct PortRegistry {
    /// Known ports: name → socket path + metadata
    ports: HashMap<String, PortInfo>,
}

pub struct PortInfo {
    pub name: String,           // e.g., "COSMIX-VIEW.1"
    pub socket: PathBuf,        // /run/user/$UID/cosmix/ports/cosmix-view.sock
    pub pid: u32,               // OS process ID
    pub commands: Vec<String>,  // ["open", "save", "rotate", "help", ...]
    pub registered_at: Instant,
}
```

**Discovery mechanisms (in priority order):**

1. **Explicit registration:** Apps call `cosmix-port::Port::new().start()` which writes a socket file. Daemon watches `/run/user/$UID/cosmix/ports/` with inotify for new `.sock` files appearing.

2. **Handshake on connect:** When daemon detects a new socket, it connects and sends a `HELP` command. The app responds with its full command list. Daemon populates `PortInfo.commands`.

3. **Heartbeat/cleanup:** Daemon periodically pings registered ports. Dead sockets (app crashed) get removed from registry.

**Lua API:**

```lua
-- ARexx's SHOW('P', 'PORTNAME') equivalent
cosmix.port_exists("COSMIX-VIEW.1")     --> true/false

-- ARexx's SHOW('P') — list all ports
cosmix.list_ports()   --> { {name="COSMIX-CALC.1", pid=12345, commands={...}}, ... }

-- Wait for a port to appear (with timeout)
cosmix.wait_for_port("COSMIX-VIEW.1", 5000)  --> true/false
```

**Work items:**
- [ ] Add inotify watcher for `/run/user/$UID/cosmix/ports/` to daemon
- [ ] Add `PortRegistry` to `DaemonState`
- [ ] Implement handshake protocol (connect → HELP → populate commands)
- [ ] Add heartbeat/cleanup loop (every 10s, try ping, remove dead)
- [ ] Expose `port_exists`, `list_ports`, `wait_for_port` to Lua

### 2.2 Message Routing — The Bridge

The critical missing piece: when Lua calls `cosmix.port("cosmix-view"):send("open", {file="..."})`, the daemon must route that to the actual app's Unix socket, not just call an internal function.

**Current state:** `cosmix.port("clipboard")` calls `cosmix.clipboard()` internally. This works for daemon-owned services but not for external apps.

**Target state:** Two kinds of ports:
1. **Built-in ports** (clipboard, windows, notify, input, screenshot, dbus, config, mail, midi) — handled internally by daemon, no socket needed
2. **App ports** (cosmix-calc, cosmix-view, cosmix-mail, any future app) — routed via Unix socket to the app's cosmix-port listener

**Implementation:**

```rust
// In Lua port dispatch
fn resolve_port(name: &str) -> PortTarget {
    if BUILTIN_PORTS.contains(name) {
        PortTarget::Internal(name)
    } else if let Some(info) = registry.get(name) {
        PortTarget::Socket(info.socket.clone())
    } else {
        PortTarget::NotFound
    }
}
```

```lua
-- This call:
local view = cosmix.port("COSMIX-VIEW.1")
local info = view:send("info")
-- Becomes: daemon connects to /run/user/$UID/cosmix/ports/cosmix-view.sock
-- Sends: {"command":"info","args":null}
-- Receives: {"ok":true,"data":{"file":"photo.jpg","width":1920,...}}
```

**Work items:**
- [ ] Modify `lua/ports.rs` to check registry before falling back to built-in
- [ ] Add `call_port()` async function that connects to app socket and sends/receives JSON
- [ ] Handle connection failures gracefully (port died, timeout)
- [ ] Cache connections (don't reconnect for every command)

### 2.3 Clip List — Global Key/Value Store

ARexx's `SETCLIP`/`GETCLIP` was a system-wide named string store. This is distinct from the clipboard — it's a shared memory space for scripts and apps to exchange named values.

**Implementation:**

```rust
// In DaemonState
pub clip_list: HashMap<String, ClipEntry>,

pub struct ClipEntry {
    pub value: serde_json::Value,  // String, Number, Bool, or structured JSON
    pub set_by: String,            // port name or "script:filename.lua"
    pub set_at: Instant,
    pub ttl: Option<Duration>,     // optional expiry
}
```

**Lua API (matching ARexx names):**

```lua
-- Set a clip value (any app or script can do this)
cosmix.setclip("CURRENT_PROJECT", "/home/user/Projects/myapp")
cosmix.setclip("ACTIVE_CUSTOMER_ID", "CUS-0042")
cosmix.setclip("SESSION_TOKEN", "abc123", { ttl = 3600 })  -- expires in 1h

-- Get a clip value (any app or script can read)
local project = cosmix.getclip("CURRENT_PROJECT")
local token = cosmix.getclip("SESSION_TOKEN")  -- nil if expired

-- List all clips
local clips = cosmix.listclips()  -- { {key="...", value="...", set_by="..."}, ... }

-- Delete a clip
cosmix.delclip("SESSION_TOKEN")
```

**Persistence:** Save clip list to `~/.config/cosmix/cliplist.json` on shutdown, restore on startup. TTL entries cleaned on load.

**Work items:**
- [ ] Add `clip_list` to DaemonState
- [ ] Expose setclip/getclip/listclips/delclip to Lua runtime
- [ ] Add to IPC protocol for non-Lua clients
- [ ] Persistence to disk on shutdown/periodic flush
- [ ] TTL expiry background task

### 2.4 Script Execution Engine

**Current:** `cosmix run script.lua` loads and executes a Lua file in the daemon's embedded mlua runtime.

**Needed enhancements:**

1. **Pre-addressed scripts:** When an app triggers a script from its macro menu, the daemon sets a default port context:

```lua
-- In ~/.config/cosmix/scripts/cosmix-view/batch_watermark.lua
-- Launched from cosmix-view's Scripts menu
-- cosmix.self() returns the calling app's port, pre-set by daemon
local app = cosmix.self()  -- returns port handle for "COSMIX-VIEW.1"

local info = app:send("info")
print("Processing: " .. info.file)
```

2. **Script isolation:** Each script runs in its own Lua state (or at minimum, its own coroutine with isolated globals) so scripts don't pollute each other.

3. **Async script execution:** Scripts should be able to run in the background without blocking the daemon's main loop. Use `cosmix.async()` or just spawn in a tokio task.

4. **Script search path:** Resolve scripts from (in order):
   - Explicit path
   - `~/.config/cosmix/scripts/`
   - `~/.config/cosmix/scripts/<appname>/` (for app-specific macros)
   - `/usr/share/cosmix/scripts/` (system-wide)

**Work items:**
- [ ] Add `cosmix.self()` Lua function that returns pre-addressed port
- [ ] Pass caller port context when daemon executes a script on behalf of an app
- [ ] Script search path resolution
- [ ] Background script execution (tokio::spawn + separate Lua state)

### 2.5 External Data Stack — Named Async Queues

ARexx's PUSH/PULL stack enabled parent/child script communication. Modern equivalent: named message queues in the daemon.

```lua
-- Producer script
local q = cosmix.queue("batch_work")
q:push("/path/to/file1.png")
q:push("/path/to/file2.png")
q:push("/path/to/file3.png")

-- Consumer script (can be same or different)
local q = cosmix.queue("batch_work")
while q:size() > 0 do
    local item = q:pop()
    -- process item...
end
```

**Implementation:** `HashMap<String, VecDeque<serde_json::Value>>` in daemon state. Not persistent — queues are session-scoped.

**Work items:**
- [ ] Add queue store to DaemonState
- [ ] Expose queue/push/pop/size to Lua
- [ ] Optional: blocking pop with timeout (`q:wait(5000)`)

---

## 3. Layer 2: Per-App Integration — The ARexx Port

### 3.1 Port Naming Convention

ARexx used `APPNAME.N` (uppercase, dot-separated instance number). Adopt this:

```
COSMIX-CALC.1    — first instance of cosmix-calc
COSMIX-VIEW.1    — first instance of cosmix-view
COSMIX-VIEW.2    — second instance (if user opens two)
COSMIX-MAIL.1    — mail client
COSMIC-EDIT.1    — if cosmic-edit adds cosmix-port support
COSMIC-FILES.1   — if cosmic-files adds cosmix-port support
```

**Instance numbering:** cosmix-port checks for existing sockets at startup. If `cosmix-view.sock` exists and is alive, the second instance creates `cosmix-view-2.sock`. The daemon normalizes these to `COSMIX-VIEW.1`, `COSMIX-VIEW.2` in the registry.

**Socket naming:** Keep lowercase for filesystem paths: `/run/user/$UID/cosmix/ports/cosmix-view.sock`. The daemon maps to uppercase port names for the Lua API.

**Work items:**
- [ ] Add instance numbering to `Port::start()` in cosmix-port
- [ ] Daemon maps socket filenames to uppercase port names
- [ ] Handle instance deregistration on app exit

### 3.2 Standard Command Vocabulary

Directly from ARexx's Amiga UI Style Guide. Every cosmix-port app MUST support these:

| Command | Args | Returns | Purpose |
|---|---|---|---|
| `OPEN` | `{file: "path"}` | `{file, width?, height?}` | Open a file |
| `SAVE` | `{}` or `{quality?: n}` | `{file}` | Save current file |
| `SAVEAS` | `{file: "path"}` | `{file}` | Save with new name |
| `CLOSE` | `{force?: bool}` | `{}` | Close document |
| `QUIT` | `{force?: bool}` | `{}` | Quit application |
| `CUT` | `{}` | `{text?}` | Cut selection |
| `COPY` | `{}` | `{text?}` | Copy selection |
| `PASTE` | `{text?: "..."}` | `{}` | Paste |
| `UNDO` | `{}` | `{}` | Undo last action |
| `REDO` | `{}` | `{}` | Redo |
| `ACTIVATE` | `{}` | `{}` | Bring window to front |
| `HELP` | `{command?: "name"}` | `{commands: [...]}` or `{args, description}` | Introspection |
| `INFO` | `{}` | `{port, app, version, file?, ...}` | App state summary |

Apps add their own domain-specific commands on top. cosmix-view might add: `ROTATE`, `FLIP`, `ZOOM`, `CROP`, `SCALE`, `SCREENSHOT`, `ANNOTATE`, `SETWALLPAPER`, `GALLERY`. cosmix-calc might add: `CALC`, `RESULT`, `HISTORY`, `MEMORY`, `PRESS`.

**The HELP command is critical.** It must return machine-readable command descriptions:

```json
// HELP with no args → list all commands
{"ok": true, "data": {
    "commands": ["OPEN", "SAVE", "SAVEAS", "CLOSE", "QUIT", "HELP", "INFO",
                 "ROTATE", "FLIP", "ZOOM", "CROP", "SCREENSHOT"]
}}

// HELP with command arg → describe that command
// HELP {command: "ROTATE"}
{"ok": true, "data": {
    "command": "ROTATE",
    "args": {"direction": "cw|ccw", "degrees?": "number"},
    "description": "Rotate the current image",
    "examples": [
        {"args": {"direction": "cw"}, "description": "Rotate 90° clockwise"},
        {"args": {"degrees": 180}, "description": "Rotate 180°"}
    ]
}}
```

**Work items:**
- [ ] Add `HELP` command auto-generation to cosmix-port (introspect registered commands)
- [ ] Add `INFO` standard command (return port name, app version, current state)
- [ ] Add `ACTIVATE` command (bring window to front via Wayland)
- [ ] Document standard vocabulary in `_doc/`
- [ ] Update cosmix-calc, cosmix-view, cosmix-mail to implement standard commands
- [ ] Provide `cosmix_port::standard_commands!()` macro for common implementations

### 3.3 Error Code Convention (RC Codes)

Adopt ARexx's return code system exactly:

```rust
// In cosmix-port
pub enum ReturnCode {
    Success = 0,
    Warning = 5,    // e.g., user cancelled, file already open
    Error = 10,     // e.g., file not found, invalid args
    Failure = 20,   // e.g., app crashed, resource unavailable
}

// Enhanced PortResponse
pub struct PortResponse {
    pub rc: u8,                         // 0, 5, 10, or 20
    pub ok: bool,                       // rc == 0
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,          // human-readable error message
}
```

```lua
-- Lua scripts check RC just like ARexx
local result = view:send("OPEN", {file = "/nonexistent.png"})
if result.rc == 0 then
    print("Opened:", result.data.file)
elseif result.rc == 5 then
    print("Warning:", result.error)
elseif result.rc == 10 then
    print("Error:", result.error)
elseif result.rc == 20 then
    error("Fatal: " .. result.error)
end
```

**Work items:**
- [ ] Add `rc` field to PortResponse
- [ ] Provide `CmdResult::success()`, `CmdResult::warning()`, `CmdResult::error()`, `CmdResult::failure()` constructors
- [ ] Map to Lua table with `.rc`, `.ok`, `.data`, `.error` fields

### 3.4 cosmix-port Crate Enhancements

The crate needs several additions to match ARexx's ease of integration:

```rust
// Target API — app adds cosmix support in ~15 lines
use cosmix_port::{Port, CmdResult, standard};

let port = Port::new("cosmix-myapp")
    // Standard commands (auto-implemented from app state)
    .standard_help()           // HELP — auto-generated from registered commands
    .standard_info("MyApp", "1.0")  // INFO — returns app metadata
    .standard_activate(window_id)    // ACTIVATE — bring to front
    // App-specific commands
    .command("dowork", "Do the work", |args| {
        let param = args["param"].as_str().unwrap_or("");
        // ... do work ...
        CmdResult::success(json!({"result": "done"}))
    })
    .command("getdata", "Get current data", |_| {
        CmdResult::success(json!({"value": 42}))
    });

let handle = port.start()?;  // auto-discovers instance number, registers socket
```

**Work items:**
- [ ] Add command description metadata to `.command()` builder
- [ ] Add `standard_help()` — auto-generates HELP from registered command metadata
- [ ] Add `standard_info()` — returns port name, app name, version
- [ ] Add `standard_activate()` — Wayland window activation
- [ ] Instance number auto-discovery
- [ ] Registration notification (daemon learns about new port)

---

## 4. Layer 3: Lua Scripting — The ARexx Language

### 4.1 ADDRESS Equivalent — Port Switching

ARexx's `ADDRESS 'APPNAME.1'` set the default command target. In Lua, this is cleaner with variables:

```lua
local amp = require("cosmix")

-- Direct port access (most common)
local view = amp.port("COSMIX-VIEW.1")
view:send("OPEN", {file = "/home/user/photo.jpg"})
view:send("ROTATE", {direction = "cw"})

-- ADDRESS equivalent — set default target
amp.address("COSMIX-VIEW.1")
amp.send("OPEN", {file = "/home/user/photo.jpg"})   -- sent to current address
amp.send("ROTATE", {direction = "cw"})                -- sent to current address

-- Switch to another app mid-script (exactly like ARexx)
amp.address("COSMIX-CALC.1")
amp.send("CALC", {expression = "2+2"})
local result = amp.send("RESULT")
print("2+2 =", result.data.value)

-- Switch back
amp.address("COSMIX-VIEW.1")
amp.send("SAVE")
```

**Work items:**
- [ ] Add `cosmix.address(port_name)` — sets default port in Lua state
- [ ] Add `cosmix.send(cmd, args)` — sends to current address
- [ ] `cosmix.port(name):send()` continues to work for explicit targeting
- [ ] Error if `cosmix.send()` called without prior `cosmix.address()`

### 4.2 Pre-Addressed Scripts (Macro Scripts)

When a cosmix app launches a script from its "Scripts" menu, the daemon pre-sets the calling app as the default address:

```lua
-- ~/.config/cosmix/scripts/cosmix-view/add_watermark.lua
-- Launched from cosmix-view's Scripts menu
-- cosmix.self() automatically returns the calling app's port

local app = cosmix.self()
local info = app:send("INFO")
print("Processing: " .. info.data.file)

-- Add watermark text annotation
app:send("SETTEXT", {
    position = {x = 10, y = 10},
    text = "© 2026 Mark Constable",
    color = "white",
    size = 18
})

app:send("SAVE")
print("Watermark added and saved")
```

**Implementation:** When daemon runs a script with a caller context:

```rust
// Daemon script execution
fn run_script_for_app(script_path: &Path, caller_port: &str) {
    let lua = create_lua_state();
    // Set the caller port as the pre-addressed target
    lua.globals().set("__cosmix_caller_port__", caller_port)?;
    // cosmix.self() reads this global
    lua.load(script_path).exec()?;
}
```

**Work items:**
- [ ] Add `cosmix.self()` Lua function
- [ ] Pass caller port context through daemon's script execution
- [ ] When script is run from CLI (`cosmix run`), `cosmix.self()` returns nil

### 4.3 Script Directory Convention

```
~/.config/cosmix/
├── scripts/                          # Global scripts
│   ├── orchestrate_production.lua    # General automation
│   ├── batch_convert.lua
│   └── daily_report.lua
├── scripts/cosmix-view/             # cosmix-view macro scripts
│   ├── add_watermark.lua            # Shows in View's Scripts menu
│   ├── batch_sharpen.lua
│   └── export_gallery.lua
├── scripts/cosmix-calc/             # cosmix-calc macro scripts
│   ├── unit_converter.lua
│   └── mortgage_calc.lua
├── scripts/cosmix-mail/             # cosmix-mail macro scripts
│   ├── auto_reply.lua
│   └── sort_newsletters.lua
├── modules/                         # Lua modules (function libraries)
│   ├── imagetools.lua               # require("imagetools")
│   ├── textutils.lua
│   └── reporting.lua
└── cliplist.json                    # Persisted clip list
```

**Work items:**
- [ ] Define directory structure in CLAUDE.md
- [ ] Daemon watches `~/.config/cosmix/scripts/` for changes (inotify)
- [ ] Add `modules/` to Lua `package.path`
- [ ] Create initial example scripts

### 4.4 Dynamic Macro Menus

This is the "killer feature" that made ARexx culturally dominant. Every cosmix app gets a "Scripts" menu populated from its script directory.

**Implementation approach:**

1. **Daemon side:** When an app registers its port, daemon scans `~/.config/cosmix/scripts/{appname}/` and sends the script list to the app via a special `__SCRIPTS__` message.

2. **cosmix-port side:** The port library receives script lists and exposes them to the app as menu items.

3. **App side (libcosmic):** The app adds a "Scripts" menu populated from the script list. When clicked, it tells the daemon to run the script with the app pre-addressed.

```rust
// In cosmix-port — automatic script menu support
impl Port {
    /// Enable the Scripts menu. The port will receive __SCRIPTS__ updates
    /// from the daemon and provide them via scripts() method.
    pub fn with_scripts_menu(mut self) -> Self {
        self.scripts_enabled = true;
        self
    }

    /// Get current list of available scripts (for menu building)
    pub fn scripts(&self) -> Vec<ScriptInfo> { ... }

    /// Request daemon to run a script with this port pre-addressed
    pub fn run_script(&self, script_path: &Path) { ... }
}
```

```rust
// In any cosmix app's header_start() menu building:
if !self.port_scripts.is_empty() {
    let script_items: Vec<_> = self.port_scripts.iter().enumerate().map(|(i, s)| {
        menu::Item::Button(
            s.display_name.clone(),
            Some(icon::from_name("text-x-script-symbolic").into()),
            MenuAction::RunScript(i),
        )
    }).collect();
    menus.push(("Scripts".into(), script_items));
}
```

**Work items:**
- [ ] Daemon scans app script directories on port registration
- [ ] Daemon sends `__SCRIPTS__` message to apps when scripts change
- [ ] cosmix-port stores script list and exposes `scripts()` method
- [ ] Each app adds "Scripts" menu populated from port scripts
- [ ] File watcher for live updates (add script → appears in menu without restart)
- [ ] "Rescan Scripts" menu item (like GoldED had)

### 4.5 Function Libraries — Lua Modules

ARexx's `.library` files extended the language. Lua's `require()` already does this naturally.

```lua
-- ~/.config/cosmix/modules/imagetools.lua
local M = {}

function M.batch_process(files, effect)
    local view = cosmix.port("COSMIX-VIEW.1")
    local results = {}
    for _, file in ipairs(files) do
        view:send("OPEN", {file = file})
        view:send(effect.command, effect.args)
        view:send("SAVE")
        table.insert(results, file)
    end
    return results
end

function M.watermark(text, position)
    local app = cosmix.self() or cosmix.port("COSMIX-VIEW.1")
    app:send("ANNOTATE", {
        tool = "text",
        text = text,
        position = position or {x = 10, y = 10},
        color = "white",
        size = 18,
    })
end

return M
```

```lua
-- Usage in a script
local imagetools = require("imagetools")
imagetools.batch_process(
    cosmix.glob("~/Photos/*.jpg"),
    {command = "ROTATE", args = {direction = "cw"}}
)
```

**Work items:**
- [ ] Add `~/.config/cosmix/modules/` to Lua package.path in daemon
- [ ] Create starter modules: imagetools, textutils, reporting
- [ ] Document module authoring pattern

---

## 5. Layer 4: Social Contract — Making It Cultural

ARexx succeeded because of a social contract: **apps without ARexx ports were not taken seriously.** We need to establish the same expectation for cosmix.

### 5.1 Port Name in About Dialog

Every cosmix app must display its port name in its About notification or dialog:

```
Cosmix View 0.1.0
Port: COSMIX-VIEW.1
Commands: 22 registered
Scripts: 3 available (~/.config/cosmix/scripts/cosmix-view/)
```

**Work items:**
- [ ] Update About handler in calc, view, mail to show port info
- [ ] cosmix-port provides `port.about_text()` helper

### 5.2 HELP Command is Mandatory

Any port that doesn't respond to `HELP` is broken. The HELP response is the machine-readable API documentation that lets scripts and AI agents discover what an app can do.

```bash
# CLI equivalent of testing an app's ARexx port
$ cosmix call COSMIX-VIEW.1 HELP
Commands: OPEN, SAVE, SAVEAS, CLOSE, QUIT, COPY, PASTE, UNDO, REDO,
          ACTIVATE, HELP, INFO, ROTATE, FLIP, ZOOM, CROP, SCALE,
          SCREENSHOT, ANNOTATE, SETWALLPAPER, GALLERY, EXIF

$ cosmix call COSMIX-VIEW.1 HELP '{"command":"ROTATE"}'
ROTATE: Rotate the current image
  direction: "cw" | "ccw" (required)
  degrees: number (optional, default: 90)
```

### 5.3 Standard Vocabulary Documentation

Create a reference document that app developers follow:

```
_doc/standard-commands.md
```

This becomes the equivalent of the Amiga UI Style Guide's ARexx section.

### 5.4 Community Script Repository

Eventually: a public repository of cosmix scripts, organized by app and workflow. Start with a `scripts/` directory in the cosmix repo with examples:

```
scripts/
├── examples/
│   ├── hello_world.lua
│   ├── batch_image_convert.lua
│   ├── database_to_report.lua
│   └── production_environment.lua
├── cosmix-view/
│   ├── add_watermark.lua
│   ├── batch_sharpen.lua
│   └── export_thumbnails.lua
├── cosmix-calc/
│   ├── unit_converter.lua
│   └── mortgage_calculator.lua
└── cosmix-mail/
    ├── auto_archive.lua
    └── newsletter_digest.lua
```

---

## 6. Layer 5: Network Mesh — The Modern Extension

ARexx was single-machine. The modern twist: **cosmix scripts can address ports on remote nodes.**

### 6.1 Cross-Node Port Addressing

Extend the port naming with network addresses using AMP format:

```
COSMIX-VIEW.1                    — local port
COSMIX-VIEW.1@cachyos.amp       — port on cachyos node
COSMIC-EDIT.1@mko.amp           — port on mko production node
```

```lua
-- Local script controlling a remote app
local remote_view = cosmix.port("COSMIX-VIEW.1@mko.amp")
remote_view:send("SCREENSHOT", {mode = "full"})
remote_view:send("SAVE", {file = "/tmp/remote_screenshot.png"})

-- Copy the file back via mesh
cosmix.mesh.fetch("mko.amp", "/tmp/remote_screenshot.png", "/tmp/local_copy.png")
```

**Transport:** Daemon routes cross-node messages through nodemesh's meshd daemon via AMP over WebSocket:

```
Lua script → cosmix daemon → meshd (local) → WebSocket/WireGuard → meshd (remote) → cosmix daemon (remote) → app socket
```

### 6.2 nodemesh Integration

**Current state:** nodemesh has a working WebSocket peer-to-peer connection and AMP message routing. It has a bridge Unix socket at `/run/meshd/meshd.sock`.

**Integration plan:**

1. cosmix daemon connects to meshd's bridge socket on startup
2. When a Lua script addresses a remote port (`APPNAME@node.amp`), daemon wraps the command in AMP and sends to meshd
3. meshd routes to the target node
4. Remote node's cosmix daemon receives the AMP message and dispatches to the local app's port

```rust
// In cosmix daemon — meshd bridge client
struct MeshBridge {
    socket: Option<PathBuf>,  // /run/meshd/meshd.sock
    conn: Option<UnixStream>,
}

impl MeshBridge {
    async fn send_to_remote(&self, target_node: &str, port: &str, cmd: &str, args: Value) -> Result<PortResponse> {
        let amp = AmpMessage::new()
            .header("type", "port_command")
            .header("to", &format!("{port}@{target_node}"))
            .header("from", &format!("{port}@{local_node}"))
            .header("command", cmd)
            .body(&serde_json::to_string(&args)?);

        self.conn.send(amp.serialize()).await?;
        let response = self.conn.recv().await?;
        Ok(serde_json::from_str(&response.body)?)
    }
}
```

**Work items:**
- [ ] Add meshd bridge client to cosmix daemon
- [ ] Parse `@node.amp` suffix in port names
- [ ] Route cross-node commands through meshd
- [ ] Handle timeouts and connection failures for remote ports

### 6.3 markweb Web Gateway

markweb provides web access to the mesh. This means:

1. **Web UI can trigger cosmix scripts:** markweb sends AMP to meshd, which routes to the target node's cosmix daemon, which runs a Lua script.

2. **Cosmix scripts can call web APIs:** Through markweb's REST endpoints.

3. **Browser-to-desktop bridge:** A React component in markweb can show real-time app state by subscribing to cosmix events via meshd → markweb → Reverb WebSocket → browser.

```lua
-- Script that bridges web and desktop
local amp = require("cosmix")

-- React to a web request (received via mesh event)
cosmix.on("port_message", function(msg)
    if msg.command == "generate_report" then
        -- Use desktop apps to generate a report
        local calc = amp.port("COSMIX-CALC.1")
        calc:send("CALC", {expression = msg.data.formula})
        local result = calc:send("RESULT")

        -- Send result back to web via mesh
        cosmix.mesh.send(msg.from, "report_result", {
            value = result.data.value,
            timestamp = os.date(),
        })
    end
end)
```

**Work items:**
- [ ] markweb: Add REST endpoint `POST /api/mesh/cosmix/{port}/{command}` that wraps calls in AMP
- [ ] markweb: Add React hook `useCosmixPort(portName)` for real-time app state
- [ ] cosmix daemon: Forward events to meshd for web consumption
- [ ] Document web-to-desktop scripting patterns

### 6.4 AI Agent Integration

The mesh enables AI agents (running in markweb or locally) to script desktop apps:

```lua
-- AI agent script: "Organize my photos by date"
local view = cosmix.port("COSMIX-VIEW.1")
local files = cosmix.glob("~/Pictures/*.jpg")

for _, file in ipairs(files) do
    view:send("OPEN", {file = file})
    local exif = view:send("EXIF")

    if exif.data and exif.data.date then
        local year, month = exif.data.date:match("(%d+):(%d+)")
        local dest_dir = string.format("~/Pictures/Organized/%s/%s/", year, month)
        cosmix.exec("mkdir -p " .. dest_dir)
        cosmix.exec(string.format("mv %q %s", file, dest_dir))
    end
end

cosmix.notify("Photo organization complete", #files .. " photos processed")
```

This is the ARexx dream realized with modern AI: **natural language → Lua script → desktop app orchestration.**

---

## 7. Implementation Phases

### Phase 1: Port Registry and Routing (Foundation) ✓ COMPLETE
**Priority: CRITICAL — everything else depends on this**
**Completed: 2026-03-08**

- [x] Port discovery via directory polling on `/run/user/$UID/cosmix/ports/` (2s interval)
- [x] PortRegistry in DaemonState
- [x] HELP handshake on port discovery
- [x] Heartbeat/cleanup loop (~10s)
- [x] Route `cosmix.port("APPNAME"):send()` to actual app sockets
- [x] `cosmix.port_exists()`, `cosmix.list_ports()`, `cosmix.wait_for_port()` Lua API
- [x] `cosmix call PORTNAME COMMAND [args]` CLI command
- [x] GUI live sync via AtomicBool dirty flag + 100ms subscription (cosmix-calc)

### Phase 2: Standard Vocabulary and Error Codes
**Priority: HIGH — establishes the contract**
**Effort: ~2 sessions**

- [ ] Define standard command vocabulary document
- [ ] Add `rc` field to PortResponse (0/5/10/20)
- [ ] Auto-generated HELP command in cosmix-port
- [ ] INFO standard command
- [ ] ACTIVATE standard command
- [ ] Update cosmix-calc with standard commands
- [ ] Update cosmix-view with standard commands
- [ ] Add cosmix-port to cosmix-mail with standard commands

### Phase 3: Clip List and Queues
**Priority: HIGH — enables inter-script communication**
**Effort: ~1 session**

- [ ] Clip List in DaemonState with TTL support
- [ ] setclip/getclip/listclips/delclip Lua API
- [ ] Named queues (push/pop/size)
- [ ] Persistence to disk
- [ ] IPC protocol support for non-Lua clients

### Phase 4: Script Macro Menus
**Priority: HIGH — the "killer feature"**
**Effort: ~2 sessions**

- [ ] Script directory scanning in daemon
- [ ] `__SCRIPTS__` notification to apps
- [ ] cosmix-port: `with_scripts_menu()` builder, `scripts()` accessor
- [ ] "Scripts" menu in cosmix-view, cosmix-calc, cosmix-mail
- [ ] Pre-addressed script execution (`cosmix.self()`)
- [ ] inotify watch for live script menu updates
- [ ] Create 3-5 example scripts per app

### Phase 5: ADDRESS and Orchestration
**Priority: MEDIUM — power user features**
**Effort: ~1 session**

- [ ] `cosmix.address()` / `cosmix.send()` Lua API
- [ ] Process spawning: `cosmix.launch("cosmix-view")`
- [ ] `cosmix.wait_for_port()` with timeout
- [ ] Orchestrator script examples
- [ ] Watcher script pattern (event-driven long-running scripts)

### Phase 6: Function Libraries and Modules
**Priority: MEDIUM — ecosystem enabler**
**Effort: ~1 session**

- [ ] Add `~/.config/cosmix/modules/` to Lua package.path
- [ ] Create starter modules: imagetools, textutils
- [ ] Document module authoring pattern
- [ ] Module discovery and listing

### Phase 7: Network Mesh Integration
**Priority: MEDIUM — modern extension beyond ARexx**
**Effort: ~3 sessions**

- [ ] meshd bridge client in cosmix daemon
- [ ] `@node.amp` suffix parsing in port names
- [ ] Cross-node command routing
- [ ] markweb REST endpoint for cosmix commands
- [ ] Event forwarding to mesh
- [ ] Cross-node script examples

### Phase 8: AI Agent Integration
**Priority: LOW — future vision**
**Effort: ~2 sessions**

- [ ] MCP server interface for cosmix daemon (like kwin-mcp)
- [ ] Natural language → Lua script generation
- [ ] Agent script templates
- [ ] markweb agent runtime integration

---

## 8. Comparison: ARexx vs Cosmix (Target State)

| Feature | ARexx (1987) | Cosmix (Target) | Enhancement |
|---|---|---|---|
| Daemon | RexxMast | cosmix-daemon | + Lua runtime, + event bus |
| Language | REXX | Lua (via mlua) | + coroutines, + modules, + FFI |
| Ports | OS message ports | Unix sockets | + auto-discovery, + heartbeat |
| Port naming | APPNAME.N | APPNAME.N[@node.amp] | + network-transparent |
| Clip List | SETCLIP/GETCLIP strings | JSON values + TTL | + structured data, + expiry |
| Data Stack | PUSH/PULL | Named queues | + multiple queues, + JSON values |
| Error codes | RC 0/5/10/20 | RC 0/5/10/20 | Identical convention |
| Macro menus | App scans REXX: dir | Daemon pushes script lists | + live updates, + file watcher |
| Pre-addressing | ADDRESS set by launcher | cosmix.self() from daemon | Identical concept |
| Discovery | SHOW('P', name) | cosmix.port_exists() | + list_ports(), + commands list |
| HELP | Text string | JSON with args/examples | + machine-readable for AI |
| Async | ASYNC keyword | cosmix.async() / background | + event-driven |
| Standard vocab | Amiga UI Style Guide | cosmix standard commands doc | Same principle |
| Libraries | .library files | Lua require() modules | + natural, + infinite |
| Scope | Single machine | Mesh network (WireGuard) | + cross-node, + web access |
| Web access | None | markweb gateway | + browser UI, + REST API |
| AI integration | None | MCP + LLM agents | + natural language scripting |

---

## 9. The Cosmix Manifesto (Social Contract)

Adapted from the Amiga UI Style Guide's ARexx section:

> **Every COSMIC application should have a cosmix port.** Adding one is approximately as much work as adding a menu to your application. An app without a cosmix port cannot participate in the desktop automation ecosystem.
>
> **Requirements for cosmix-enabled apps:**
> 1. Register a cosmix-port with a consistent name (APPNAME.N)
> 2. Implement the standard command vocabulary (OPEN, SAVE, QUIT, HELP, INFO, ACTIVATE at minimum)
> 3. Use standard error codes (RC 0/5/10/20)
> 4. Display port name in About dialog
> 5. Document all commands via the HELP command
> 6. Support the Scripts menu (scan `~/.config/cosmix/scripts/{appname}/`)
>
> **For script authors:**
> 1. Scripts live in `~/.config/cosmix/scripts/`
> 2. App-specific scripts go in `~/.config/cosmix/scripts/{appname}/`
> 3. Use `cosmix.self()` for pre-addressed scripts
> 4. Check RC codes after every command
> 5. Use the Clip List for cross-script state, file paths for binary data
> 6. Reusable logic goes in `~/.config/cosmix/modules/`

---

## 10. First Milestone: The Demo Script

The proof that cosmix has achieved ARexx parity is when this script works:

```lua
-- production_pipeline.lua
-- The canonical ARexx power demo, translated to cosmix
local cosmix = require("cosmix")

-- Launch apps if not running
if not cosmix.port_exists("COSMIX-VIEW.1") then
    cosmix.launch("cosmix-view")
    assert(cosmix.wait_for_port("COSMIX-VIEW.1", 5000))
end

if not cosmix.port_exists("COSMIX-CALC.1") then
    cosmix.launch("cosmix-calc")
    assert(cosmix.wait_for_port("COSMIX-CALC.1", 5000))
end

-- Screenshot → annotate → calculate → report
local view = cosmix.port("COSMIX-VIEW.1")
view:send("SCREENSHOT", {mode = "interactive"})
view:send("ANNOTATE", {tool = "text", text = "Q3 Report", position = {x=50, y=30}})

-- Calculate something
local calc = cosmix.port("COSMIX-CALC.1")
calc:send("CALC", {expression = "1250 * 1.1"})
local total = calc:send("RESULT")

-- Annotate the total onto the screenshot
view:send("ANNOTATE", {
    tool = "text",
    text = "Total: $" .. total.data.value,
    position = {x=50, y=60},
    color = "green",
})

-- Save and notify
view:send("SAVEAS", {file = "~/Reports/q3_report.png"})
cosmix.notify("Pipeline Complete", "Q3 report saved with total: $" .. total.data.value)

-- Publish to clip list so other scripts can find it
cosmix.setclip("LAST_REPORT", "~/Reports/q3_report.png")
cosmix.setclip("LAST_REPORT_TOTAL", total.data.value)

print("Done. Report saved and published to clip list.")
```

When this script runs end-to-end, cosmix has achieved ARexx parity. Everything after that — mesh networking, web access, AI agents — is the modern extension.

---

*Plan authored 2026-03-08. Based on analysis of the ARexx deep-dive reference document, current cosmix/appmesh/nodemesh/markweb codebases, and research into KDE KWin scripting, GNOME D-Bus extensions, and AppleScript OSA.*
