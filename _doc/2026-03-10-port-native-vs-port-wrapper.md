# Port Native vs Port Wrapper

Two complementary approaches to giving any program an ARexx-style port interface.

## Definitions

| Term | Meaning | Example |
|------|---------|---------|
| **Port native** | cosmix-port linked into the binary itself; handler runs inside the app process with direct access to app state | cosmix-calc, cosmix-view, cosmix-mail |
| **Port wrapper** | External Lua script that gives something a cosmix port without modifying it; handler runs outside the app, talking through existing interfaces (CLI, D-Bus, HTTP, filesystem) | cosmix-portd + systemd.lua, diskfree.lua |

This maps directly to the layered architecture:

- Port wrappers are **Layer 1** (external, no app modification)
- Port native is **Layer 3** (integrated into the app)

## Port Native

The handler runs **inside the app process** with direct access to app state.

```rust
// cosmix-calc: handler reads live app state directly
.command("result", "Get current result", |_| {
    let state = CALC_STATE.lock().unwrap();
    Ok(json!({"result": state.display_value}))
})
```

### Pros

- Direct access to in-memory state (current document, selection, undo history, UI state)
- Can mutate the app (open file, change selection, trigger UI update via `PortEvent`)
- Zero latency — no IPC, no subprocess, no serialization boundary
- Type-safe — handler is compiled Rust

### Cons

- Requires modifying the app's source code
- Must recompile to add/change commands
- Only works for apps you control

## Port Wrapper

The handler runs **outside the app**, talking to it through its existing interfaces.

```lua
-- systemd.lua: handler shells out to systemctl
status = function(args)
    local r = exec("systemctl show " .. args.unit .. " --property=ActiveState")
    return { state = r.stdout }
end
```

### Pros

- No app modification needed — wraps anything with a CLI, D-Bus API, HTTP API, or filesystem
- Hot-reloadable — edit the Lua script, restart portd
- One binary (cosmix-portd, 3.3MB) serves unlimited ports
- Anyone can write a wrapper without knowing Rust

### Cons

- No access to app internals — only sees what the external interface exposes
- Slower — subprocess/shell overhead per command
- Fragile — parsing CLI output breaks when output format changes
- Can't trigger UI actions inside the app (no `PortEvent::Activate`)

## When to Use Which

| Situation | Use |
|-----------|-----|
| App you're building (cosmix-calc, cosmix-view) | **Port native** |
| App you don't control (systemd, NetworkManager, pipewire) | **Port wrapper** |
| COSMIC app with libcosmic (Layer 3 goal) | **Port native** via cosmix-port crate |
| Quick prototype / testing | **Port wrapper** |
| Deep integration (read selection, modify document) | **Port native** — only way |
| System administration commands | **Port wrapper** — CLI is the stable interface |

## Why Use a Port Wrapper Instead of Direct Shell Calls?

If the Lua wrapper is just shelling out to `systemctl`, why not call `systemctl` directly from scripts?

The advantage is the **port abstraction itself**.

### Without port wrapper

Every script that wants systemd info writes its own shell calls:

```lua
-- script A
os.execute("systemctl restart cosmix-web.service")

-- script B (different author, different parsing)
local handle = io.popen("systemctl show cosmix-web --property=ActiveState")
local output = handle:read("*a")
-- parse it differently...
```

### With port wrapper

One canonical interface, every script uses the same API:

```lua
-- script A
cosmix.port("systemd"):send("restart", { unit = "cosmix-web.service" })

-- script B (same author or different — same interface)
local status = cosmix.port("systemd"):send("status", { unit = "cosmix-web.service" })
print(status.ActiveState)
```

### Concrete gains

1. **Uniform discovery** — `cosmix.port("systemd"):send("help")` tells you what's available. Raw shell calls are undiscoverable.

2. **Structured data** — the wrapper returns JSON tables, not raw text that every consumer must parse differently.

3. **Network transparency** — `cosmix.port("systemd")` routes through the daemon. Today it's local, tomorrow it routes to `systemd@gcwg.amp` over the mesh. Raw `exec("systemctl ...")` is always local.

4. **Orchestration** — the daemon can log, rate-limit, queue, and coordinate port calls. Direct shell calls are invisible to the system.

5. **Swappable implementation** — the systemd wrapper uses `exec("systemctl ...")` today. Tomorrow you replace the Lua script with a D-Bus implementation — or even port native. Every consumer's code stays identical because the port interface didn't change.

The wrapper isn't about "accessing the binary" — it's about **putting a stable, discoverable, network-transparent interface in front of it**. The Lua script is the adapter between the port contract and the messy reality of each tool's CLI.

## cosmix-portd Architecture

A single binary that loads Lua wrapper scripts from `~/.config/cosmix/wrappers/`:

```
~/.config/cosmix/wrappers/
├── systemd.lua      → port "systemd" (9 commands)
├── diskfree.lua     → port "diskfree" (4 commands)
├── pipewire.lua     → port "pipewire" (drop in to add)
└── anything.lua     → port "anything" (drop in to add)
```

Each wrapper script returns a table with a `port` name and `commands` map:

```lua
return {
    port = "myservice",
    commands = {
        my_command = function(args)
            local r = exec("some-cli --flag " .. args.param)
            return { output = r.stdout, code = r.code }
        end,
    }
}
```

### Built-in Lua functions

| Function | Purpose |
|----------|---------|
| `exec(cmd)` | Run shell command, return `{ stdout, stderr, code }` |
| `read_file(path)` | Read file to string |
| `write_file(path, content)` | Write string to file |
| `json_encode(value)` | Lua value to JSON string |
| `json_decode(string)` | JSON string to Lua value |

### Standard commands (auto-generated)

Every port wrapper automatically gets `help` and `info` commands from cosmix-port's standard command infrastructure — no Lua code needed.

### Per-call isolation

Each command invocation creates a fresh Lua state. This satisfies Rust's `Send + Sync + 'static` requirements for cosmix-port handlers and ensures no shared mutable state between concurrent calls.

## The Two Approaches Are Complementary

Port native and port wrapper aren't competing — they're complementary layers:

- **Port native** is the goal for apps in the COSMIC ecosystem
- **Port wrapper** is how you integrate everything else without waiting for upstream to add cosmix-port support
- Both present the same interface to scripts: `cosmix.port("name"):send("command", args)`
- Both are discoverable by the daemon via socket directory scanning
- Both support the full ARexx command vocabulary (HELP, INFO, etc.)

A port can start as a wrapper and graduate to native as the app adopts cosmix-port — the consumer-side API never changes.

## libcosmic Integration: The Layer 3 Endgame

Port native today requires ~50-100 lines of per-app setup code. The endgame is to embed cosmix-port into libcosmic behind a feature flag, so every COSMIC app gets a port automatically.

### Three levels of integration

**Level 1: Per-app port native (current state)**

Every app manually adds cosmix-port, registers commands, handles events:

```rust
Port::new("calc")
    .command("result", "Get result", handler)
    .standard_help()
    .standard_info("cosmix-calc", "0.1")
    .standard_activate()
    .start()?;
```

Coverage: only apps that explicitly opt in.

**Level 2: libcosmic integration (the goal)**

cosmix-port merged into libcosmic behind a feature flag. `cosmic::app::run()` — which every COSMIC app already calls — automatically starts a port listener with zero per-app code.

```rust
// This is ALL that exists in cosmic-files, cosmic-edit, cosmic-term today:
cosmic::app::run::<MyApp>(settings)?;

// With cosmix-port in libcosmic, that single line also:
// 1. Creates a port named after the app's ID (e.g., "cosmic-files")
// 2. Listens on /run/user/$UID/cosmix/ports/cosmic-files.sock
// 3. Auto-registers standard commands from what libcosmic already knows
```

**Level 3: App-specific extensions (optional)**

Apps that want deep scripting add a trait method:

```rust
impl cosmic::app::Application for CosmicFiles {
    fn cosmix_commands() -> Vec<CosmixCommand> {
        vec![
            command!("open", |path: String| self.open_path(path)),
            command!("list", || self.current_directory_listing()),
            command!("selection", || self.selected_items()),
        ]
    }
}
```

### What libcosmic provides for free (no per-app code)

libcosmic already manages these — it can expose them as port commands automatically:

| Command | Source | What it does |
|---------|--------|-------------|
| `ACTIVATE` | Window focus | Bring window to front |
| `QUIT` | `cosmic::app::Message::Close` | Close the app |
| `INFO` | App ID, version from Cargo.toml | Return app metadata |
| `HELP` | Auto-generated from registered commands | List available commands |
| `WINDOW` | cosmic-comp window state | Return size, position, workspace |
| `MINIMIZE` / `MAXIMIZE` | Window management | Control window state |

Apps that don't implement `cosmix_commands()` still get these. Apps that do get both.

### The analogy

Like D-Bus in GNOME/KDE: every GTK app gets a D-Bus interface for free (Activate, Quit, window properties) from `GtkApplication`. Individual apps optionally expose additional methods. cosmix-port in libcosmic is the same pattern — the framework gives you the baseline, apps optionally extend it.

This is also exactly what made ARexx powerful on the Amiga — the OS framework (Intuition) gave every app a basic port, and apps that cared could extend it.

The current per-app approach (Level 1) is the bootstrap phase — proving the concept before proposing the libcosmic PR upstream.

## ARexx Feature Parity: Gap Analysis

Comparison of RexxMast capabilities vs cosmix daemon as of Phase 6.

### Implemented

| RexxMast Feature | Cosmix Equivalent | Phase |
|-----------------|-------------------|-------|
| Script execution (`RX`) | `cosmix run script.lua` | 1 |
| Port discovery (`SHOW('PORTS')`) | Directory scan + HELP handshake | 1 |
| Message routing to app ports | Daemon routes `cosmix.port()` calls | 1 |
| Clip List (`SETCLIP`/`GETCLIP`) | `cosmix.setclip()` / `cosmix.getclip()` | 3 |
| RC codes (0/5/10/20) | `PortResponse::success/warning/error/failure` | 2 |
| Standard commands (HELP/INFO) | `standard_help()` / `standard_info()` | 2 |
| ADDRESS instruction | `cosmix.address()` / `cosmix.send()` | 5 |
| Script launch + wait | `cosmix.launch()` / `cosmix.wait_for_port()` | 5 |
| Function libraries (modules) | `~/.config/cosmix/modules/` in Lua `package.path` | 6 |
| ACTIVATE command | `standard_activate()` + `PortEvent::Activate` | 2 |
| Script menus in apps | `daemon/scripts.rs` pushes to apps | 4 |
| Named queues | `cosmix.queue()` with push/pop/size/clear | 3 |
| INTERPRET (dynamic code exec) | Lua has `load()` natively | Built-in |
| OPTIONS RESULTS | Always enabled (no opt-out needed) | Simpler |

### Missing — Significant Gaps

**1. Function host chain with priorities**

RexxMast searched: internal → built-in → library list (priority-sorted) → external scripts. This allowed function libraries to intercept and override functions based on priority. Cosmix has flat module loading with no priority resolution or function interception.

**2. Global tracing and debugging**

RexxMast provided powerful debugging commands:
- `TS` — force all running scripts into interactive trace mode
- `TE` — clear global trace, turn tracing OFF for all scripts
- `TCO` / `TCC` — open/close a global tracing console window
- `HI` — halt all running scripts
- `TRACE ?R` — per-script interactive tracing with step-through

Cosmix has no equivalent. Scripts either work or fail with an error message.

**3. Script-to-script messaging**

In ARexx, scripts could `OPENPORT(name)` to become addressable, then other scripts could `ADDRESS portname 'command'` to communicate. Scripts could act as servers, processing messages from other scripts or apps.

Cosmix scripts can call ports but cannot become ports themselves.

**4. Structured error signals**

ARexx `SIGNAL ON ERROR/SYNTAX/HALT` with automatic stack unwinding and transfer to labeled error handlers. Integrated with RC codes from port commands — if a command returned RC >= FAILAT, the ERROR signal fired automatically.

Lua has `pcall`/`xpcall` for error handling, but no integration with the port system's RC codes or automatic threshold-based error trapping.

### Not applicable / deliberately different

| RexxMast Feature | Why not needed |
|-----------------|----------------|
| Memory access (`STORAGE`/`IMPORT`/`EXPORT`) | Unsafe, not appropriate for modern systems |
| String-typed variables | Lua has proper types — better |
| PARSE instruction | Lua has `string.match()` patterns — different but equivalent |
| `CreateArgstring` memory management | Rust/serde handles this automatically |
| Separate RX/RXC/RXSET/RXLIB commands | Unified `cosmix` CLI handles all subcommands |
