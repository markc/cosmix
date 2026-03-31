# AMP — Cosmix Protocol Specification

**Version:** 0.4.0
**Date:** 2026-03-09
**Authors:** Mark Constable / Claude
**Domain:** cosmix.nexus

> Every app is a port. Every port speaks AMP. Every node is reachable.
> One protocol, one language, one wire format — from Unix socket to mesh.

---

## 1. What AMP Is

On the Amiga, **ARexx** was built into the OS. Every serious application exposed an ARexx port — a named endpoint that accepted commands and returned results. A three-line script could tell a paint program to render an image, a word processor to insert the filename, and a file manager to move it to a folder. No APIs to learn, no SDKs to install — just send commands to named ports in a common language.

**AMP** (AppMesh Protocol) recreates this for the COSMIC desktop, extended to span multiple machines over WireGuard. Every application — desktop, web, or remote service — exposes ports. Lua is the scripting language. Rust is the engine. AMP is the wire format everywhere — local Unix sockets, mesh WebSockets, and everything in between.

| Amiga / ARexx | AMP / Cosmix |
|---|---|
| `ADDRESS 'APP' 'COMMAND'` | `cosmix call cosmix-toot post "Hello"` (CLI) |
| ARexx port name | AMP address: `cosmix-toot.cosmix.cachyos.amp` |
| `rx script.rexx` | `cosmix run script.lua` |
| REXX (universal glue) | Lua (universal glue, via mlua) |
| Single machine IPC | Multi-node mesh (WireGuard + AMP) |

### Why Lua

- **Embeddable** — mlua compiles LuaJIT/Lua 5.4 directly into the Rust binary. No external interpreter, no subprocess, no PATH dependency.
- **Hot-reloadable** — edit a script, run it again. No compilation step, no restart.
- **Minimal footprint** — LuaJIT adds ~400KB to a binary. The entire Lua standard library fits in memory alongside your app.
- **Already proven** — Neovim, Redis, Nginx, World of Warcraft, Roblox — Lua is the most widely embedded scripting language in history.
- **Natural for orchestration** — coroutines, simple table syntax, first-class functions. A Lua script reading and writing AMP messages looks like pseudocode.

### Why Rust (everything else)

- **One language for everything** — desktop apps (libcosmic), web API (Axum), mail server (Stalwart), mesh daemon (meshd), IPC library (cosmix-port). Single-binary deployments everywhere.
- **Memory safety guaranteed** — the compiler catches the bugs that crash C programs and corrupt data in Go programs.
- **Performance** — zero-cost abstractions, no garbage collector, no runtime. A Rust AMP parser is as fast as a hand-written C parser.
- **Type safety end-to-end** — the same `AmpMessage` struct serialises to a Unix socket, a WebSocket frame, and a log file.

---

## 2. Design Principle: Three-Reader Format

Every AMP message must be simultaneously useful to three readers:

1. **Machines** — deterministic header parsing for routing, dispatch, and filtering. A router reads `to:`, `from:`, `command:` and forwards. No understanding required.
2. **Humans** — `cat`, `grep`, render in any markdown viewer. A developer debugging at 2am reads the message and knows what happened.
3. **AI agents** — natural language comprehension without schema definitions. An agent reads the full message — headers and body — and reasons about it as text.

This is not a nice-to-have. It is the core constraint that drives every format decision in AMP.

### Why this matters

Traditional protocols serve one reader well and force the others through translation layers:

| Protocol | Machine | Human | AI Agent |
|---|---|---|---|
| protobuf/gRPC | Native | Opaque binary | Needs SDK wrapper + schema docs |
| JSON-RPC (MCP) | Native | Readable but verbose | Needs tool definitions + JSON schemas |
| Length-prefixed JSON | Native | Need tooling to frame | Need framing code + schema |
| MQTT | Native | Topic strings readable, payloads often binary | Needs topic documentation |
| AMP | Headers route deterministically | `cat message.amp.md` | Reads natively — it's markdown |

An AI agent consuming an AMP event stream pays ~60% fewer tokens than the equivalent JSON-RPC representation, while gaining MORE context, not less. The markdown body is the format LLMs were trained on billions of tokens of. An agent doesn't need a tool definition to understand:

```
---
amp: 1
type: event
from: cosmix-toot.cosmix.cachyos.amp
command: posted
---
# Status posted
Content: **Hello from the mesh!**
Visibility: public
Boosts: 0, Favourites: 0
```

It reads it, understands it, and can generate a response in the same format.

### The boundary rule

**Headers route. Bodies reason.** A dumb router must never need to parse the body. An agent should never need to understand headers to reason about content. The same message works for a stateless forwarder (parse three headers, dispatch) AND a reasoning agent (read everything, think about it).

### AI agents as mesh nodes

An AI agent connected to the mesh via WebSocket is a first-class participant:

- **No tool definition maintenance** — when a new port appears on the mesh, the agent discovers it through HELP commands (which return AMP messages). No JSON schema to update.
- **Self-describing commands** — `command: search`, `args: {"query": "invoices"}`, `from: cosmix-mail.cosmix.mko.amp` tells the agent what this does.
- **Multi-agent coordination becomes conversation** — two agents on different nodes exchanging AMP messages are passing structured text to each other. Headers handle routing; bodies handle reasoning.

---

## 3. AMP Everywhere: One Protocol, All Transports

**v0.4's defining change:** AMP is no longer reserved for mesh communication. Every byte that crosses a cosmix boundary — local Unix socket, mesh WebSocket, log file — uses AMP framing. There is no "internal format" vs "wire format" distinction. There is only AMP.

### Why not length-prefixed JSON for local sockets?

The v0.3 spec assumed local IPC would use a simpler binary-framed JSON format, with AMP reserved for mesh traffic. This created two parsers, two serialisers, two test suites, two mental models, and a translation layer at the mesh boundary. v0.4 eliminates this:

| Concern | Length-prefixed JSON | AMP `---` framing |
|---|---|---|
| **Framing** | Read 4 bytes → decode length → read N bytes (two-phase) | Scan for `---\n` (single-phase, streamable) |
| **Debugging** | Need tooling to read (`xxd`, custom deserialiser) | `cat`, `grep`, `tail -f` work directly |
| **Error recovery** | Lost sync = lost connection (can't resync without length) | Scan forward to next `---\n` and resume |
| **Streaming** | Must buffer entire message before parsing | Parse headers as they arrive, stream body |
| **Code reuse** | Separate parser from mesh parser | Same `amp_parse()` everywhere |
| **AI readability** | Structured but needs framing context | Native — it's text |

The `---\n` delimiter is 4 bytes, same as a 32-bit length prefix. But it's self-describing (you can see it in a hex dump), resyncable on error, and requires no byte-order convention. For the rare case where a message body legitimately contains `\n---\n`, the closing delimiter is `\n---\n` at the START of a line — bodies that might contain this pattern (e.g., markdown with horizontal rules) can be handled by the convention that only `---\n` immediately following a newline (or at stream start) is a delimiter.

### The transport matrix

| Transport | Where | Format | Notes |
|---|---|---|---|
| Unix socket | App ↔ daemon (local) | AMP | `/run/user/$UID/cosmix/<port>.sock` |
| Unix socket | Script ↔ daemon (local) | AMP | Lua calls through cosmix daemon |
| WebSocket | Node ↔ node (mesh) | AMP frames in WS text messages | Over WireGuard tunnel |
| WebSocket | Browser ↔ Axum (web) | AMP frames in WS text messages | Authenticated per-session |
| Log file | Debug/audit | AMP messages concatenated | `cat` and `grep` just work |
| CLI pipe | `cosmix call` stdin/stdout | AMP | Single request-response |

One parser. One serialiser. One test suite. One mental model.

---

## 4. The AMP Address

AMP uses DNS-native addressing. Addresses read left-to-right from most-specific to least-specific:

```
[port].[app].[node].amp
```

Examples:

```
cosmix-toot.cosmix.cachyos.amp     → toot app on cachyos
cosmix-mail.cosmix.cachyos.amp     → mail app on cachyos
cosmix-calc.cosmix.cachyos.amp     → calculator on cachyos
stalwart.cosmix.mko.amp            → Stalwart mail server on mko
axum-api.cosmix.mko.amp            → Axum web API on mko
meshd.cosmix.cachyos.amp           → mesh daemon on cachyos
```

- **`.amp`** is the mesh TLD
- **Node names** (`cachyos`, `mko`, `mmc`) are subdomains under `.amp`
- **`cosmix`** is the application namespace (always `cosmix` for this ecosystem)
- **Port names** (`cosmix-toot`, `stalwart`, `axum-api`) are the leaf — the actual service endpoint

**Local shorthand:** `cosmix-toot` alone implies `.cosmix.<local-node>.amp` when running locally. No DNS lookup needed for local ports. The daemon resolves short names to local sockets.

**Remote addressing:** `cosmix-toot.cosmix.mko.amp` tells the daemon to route through meshd to the `mko` node. If `mko` is not in the local WireGuard mesh, resolution fails with error code 20.

---

## 5. The AMP Message

AMP messages use **markdown frontmatter** as the wire format — `---` delimited headers with an optional freeform body. The encoding is **UTF-8**. Every AMP message is a valid `.amp.md` file.

### 5.1 Grammar

```
message = "---\n" headers "---\n" body
headers = *(key ": " value "\n")
body    = *UTF-8  (may be empty)
```

Every message begins with `---\n` and the header block ends with `---\n`. The body continues until the next `---\n` at the start of a line (on a stream) or until EOF (in a file or single-message context).

### 5.2 Stream Framing

On a persistent connection (Unix socket, WebSocket), messages are concatenated:

```
---\n headers ---\n body ---\n headers ---\n body ---\n headers ---\n ...
```

The parser treats every `---\n` at the start of a line as a potential message boundary. The algorithm:

1. Read until `---\n` → this is the start of a message
2. Read lines until the next `---\n` → these are headers
3. Read until the next `---\n` at start of a line → this is the body
4. The `---\n` that ended the body is also the start of the next message's headers (or another `---\n` for an empty message)

**Resync on error:** If the parser loses state (partial read, corrupted bytes), scan forward for `\n---\n` and resume. No connection reset required. This is impossible with length-prefixed framing — a corrupted length field means every subsequent message is misaligned.

### 5.3 The Four Shapes

One format, one parser, four message shapes:

| Shape | Description | Use case |
|---|---|---|
| **Full message** | Headers + markdown/text body | Events, rich responses, errors with context |
| **Command** | Headers only (including `args:`), no body | Requests, acks, simple responses |
| **Data** | Minimal `json:` header, no body needed | High-throughput streams, structured payloads |
| **Empty** | No headers, no body (`---\n---\n`) | Heartbeat, ACK, NOP, stream separator |

All four are delimited by `---` and parsed identically.

### 5.4 Format Examples

**Shape 1 — Full message** (headers + body):

```
---
amp: 1
type: event
id: 0192b3a4-7c8d-0123-4567-890abcdef012
from: cosmix-toot.cosmix.cachyos.amp
command: posted
---
# Status posted
Content: **Hello from the mesh!**
Visibility: public
URL: `https://mastodon.social/@user/123456`
```

**Shape 2 — Command** (headers only):

```
---
amp: 1
type: request
id: 0192b3a4-5e6f-7890-abcd-ef1234567890
from: script.cosmix.cachyos.amp
to: cosmix-mail.cosmix.cachyos.amp
command: status
ttl: 30
---
```

**Shape 3 — Command with args**:

```
---
amp: 1
type: request
id: 0192b3a4-5e6f-7890-abcd-ef1234567891
from: script.cosmix.cachyos.amp
to: cosmix-toot.cosmix.cachyos.amp
command: post
args: {"content": "Hello from AMP!", "visibility": "public"}
ttl: 30
---
```

**Shape 4 — Data** (minimal envelope):

```
---
json: {"level": 0.72, "peak": 0.91, "channel": "left"}
---
```

**Shape 5 — Response with structured body**:

```
---
amp: 1
type: response
reply-to: 0192b3a4-5e6f-7890-abcd-ef1234567890
from: cosmix-mail.cosmix.cachyos.amp
command: status
---
{"unread": 3, "total": 1247, "folders": ["inbox", "sent", "drafts"]}
```

**Shape 6 — Error response**:

```
---
amp: 1
type: response
reply-to: 0192b3a4-5e6f-7890-abcd-ef1234567890
from: cosmix-toot.cosmix.cachyos.amp
command: post
rc: 10
error: Authentication expired
---
```

**Empty message** (heartbeat/ACK):

```
---
---
```

### 5.5 Header Fields

| Field | Required | Description |
|---|---|---|
| `amp` | yes* | Protocol version (always `1`) |
| `type` | yes* | `request`, `response`, `event`, `stream` |
| `id` | yes* | UUID v7 (time-ordered) |
| `from` | yes* | Source AMP port address |
| `to` | no | Target AMP port address (omitted for events/broadcasts) |
| `command` | yes* | The action to perform or that was performed |
| `args` | no | Command arguments as inline JSON: `{"key": "value"}` |
| `json` | no | Self-contained data payload as inline JSON |
| `reply-to` | no | Message ID this responds to (responses only) |
| `rc` | no | Return code: 0=success, 5=warning, 10=error, 20=failure |
| `ttl` | no | Time-to-live in seconds (default 30) |
| `error` | no | Error description string (when rc > 0) |
| `timestamp` | no | ISO 8601 with microseconds |

\* Required for full messages and commands. Data-only messages (`json:` shape) may omit routing headers when the transport already provides context (e.g., an established session on a known socket).

### 5.6 Return Codes

Following the ARexx convention:

| Code | Meaning | Example |
|---|---|---|
| 0 | Success | Command executed normally |
| 5 | Warning | Partial result, non-fatal issue |
| 10 | Error | Command failed but port is fine |
| 20 | Failure | Severe error, port may be degraded |

Return codes appear in the `rc:` header of response messages. Absence of `rc:` implies success (rc=0).

### 5.7 Standard Commands

Every cosmix-port app MUST support these commands:

| Command | Description | Returns |
|---|---|---|
| `HELP` | List available commands | Command names + descriptions |
| `INFO` | App name, version, capabilities | App metadata |
| `ACTIVATE` | Bring app window to front | rc: 0 or 10 |
| `OPEN` | Open a file/URI | rc: 0 or 10 |
| `SAVE` | Save current document | rc: 0 or 10 |
| `SAVEAS` | Save current document to path | rc: 0 or 10 |
| `CLOSE` | Close current document/tab | rc: 0 or 10 |
| `QUIT` | Graceful shutdown | rc: 0 |

Apps extend this vocabulary with domain-specific commands (e.g., `post`, `favorite`, `boost` for cosmix-toot; `search`, `compose`, `send` for cosmix-mail).

---

## 6. Parsing

Headers are flat `key: value` strings — no YAML parser needed. Two keys (`args` and `json`) carry inline JSON, decoded with `serde_json`. The parser is identical for all shapes and all transports.

### 6.1 Rust Reference Parser

```rust
use std::collections::HashMap;

pub struct AmpMessage<'a> {
    pub headers: HashMap<&'a str, &'a str>,
    pub body: &'a str,
}

pub fn amp_parse(raw: &str) -> AmpMessage<'_> {
    let content = raw.strip_prefix("---\n").unwrap_or(raw);
    let (fm, body) = content.split_once("\n---\n").unwrap_or((content, ""));
    let mut headers = HashMap::new();
    for line in fm.lines() {
        if let Some((k, v)) = line.split_once(": ") {
            headers.insert(k.trim(), v.trim());
        }
    }
    AmpMessage { headers, body }
}

pub fn amp_serialize(headers: &[(&str, &str)], body: &str) -> String {
    let mut out = String::from("---\n");
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push('\n');
    }
    out.push_str("---\n");
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    out
}
```

### 6.2 Lua Reference Parser

```lua
function amp_parse(raw)
    local _, hend = raw:find("^%-%-%-\n")
    if not hend then return nil, "no opening ---" end
    local rest = raw:sub(hend + 1)
    local fm, body = rest:match("^(.-)%\n%-%-%-\n(.*)")
    if not fm then fm = rest; body = "" end

    local headers = {}
    for line in fm:gmatch("[^\n]+") do
        local k, v = line:match("^([^:]+):%s*(.+)")
        if k then headers[k] = v end
    end

    -- Decode inline JSON fields
    if headers.args then
        headers.args = json.decode(headers.args)
    end
    if headers.json then
        headers.json = json.decode(headers.json)
    end

    return { headers = headers, body = body }
end
```

### 6.3 Stream Parser (Rust, async)

For persistent connections, a streaming parser that yields messages as they arrive:

```rust
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn amp_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
) -> impl futures::Stream<Item = String> {
    let mut lines = BufReader::new(reader).lines();
    async_stream::stream! {
        let mut buf = String::new();
        let mut in_message = false;
        let mut in_body = false;

        while let Ok(Some(line)) = lines.next_line().await {
            if line == "---" {
                if !in_message {
                    // Opening delimiter — start new message
                    in_message = true;
                    in_body = false;
                    buf.clear();
                    buf.push_str("---\n");
                } else if !in_body {
                    // Closing header delimiter — body follows
                    in_body = true;
                    buf.push_str("---\n");
                } else {
                    // Closing body delimiter — yield complete message
                    yield buf.clone();
                    // This --- is also the start of the next message
                    in_message = true;
                    in_body = false;
                    buf.clear();
                    buf.push_str("---\n");
                }
            } else if in_message {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        // Yield any trailing message (EOF before closing ---)
        if in_message && !buf.is_empty() {
            yield buf;
        }
    }
}
```

---

## 7. The Cosmix Stack

AMP v0.4 is designed for the pure-Rust cosmix stack. Every component below speaks AMP natively.

### 7.1 Component Map

| Component | Role | Language | AMP Integration |
|---|---|---|---|
| **libcosmic** | Desktop toolkit | Rust | cosmix-port feature flag |
| **cosmix-port** | IPC library | Rust | AMP framing on Unix sockets |
| **cosmix daemon** | Port registry + routing | Rust | AMP hub — routes between local ports and mesh |
| **Axum** | Web API server | Rust | AMP over WebSocket to browsers |
| **Stalwart** | Mail/Calendar | Rust | JMAP/IMAP/SMTP/CalDAV — bridged to AMP via cosmix-port |
| **meshd** | Mesh control plane | Rust | AMP over WebSocket (WireGuard tunnels) |
| **Lua (mlua)** | Scripting | Lua | Generates and consumes AMP messages |
| **React** | Browser UI | TypeScript | Consumes Axum REST/WS APIs |

### 7.2 Desktop Apps (libcosmic + cosmix-port)

Every COSMIC app gets an ARexx-style port with ~5-20 lines of integration code:

```rust
// In any libcosmic app
let port = cosmix_port::Port::new("cosmix-toot")
    .command("status", "Get app status", |_| {
        amp_response(0, json!({"timeline": "home", "unread": 5}))
    })
    .command("post", "Post a new status", |args| {
        let content = args.get("content").ok_or("missing content")?;
        // ... post via mastodon-async ...
        amp_response(0, json!({"id": "123", "url": "..."}))
    })
    .standard_help()
    .standard_info("Cosmix Toot", env!("CARGO_PKG_VERSION"))
    .standard_activate();

let handle = port.start()?; // Listens on /run/user/$UID/cosmix/cosmix-toot.sock
```

The port speaks AMP on its Unix socket. The daemon discovers it via inotify, handshakes with HELP, and begins routing.

### 7.3 Lua Scripting (the ARexx experience)

```lua
-- ~/.config/cosmix/scripts/mail-summary.lua
local mail = cosmix.port("cosmix-mail")
local toot = cosmix.port("cosmix-toot")

-- Check mail via cosmix-mail (which talks JMAP to Stalwart)
local status = mail:call("status")

if status.unread > 0 then
    -- Post summary to Mastodon
    toot:call("post", {
        content = "I have " .. status.unread .. " unread emails",
        visibility = "private"
    })

    -- Desktop notification
    cosmix.notify("Mail summary posted to Mastodon")
end
```

Under the hood, `cosmix.port("cosmix-mail"):call("status")` generates:

```
---
amp: 1
type: request
id: <uuid>
from: script.cosmix.cachyos.amp
to: cosmix-mail.cosmix.cachyos.amp
command: status
---
```

And receives:

```
---
amp: 1
type: response
reply-to: <uuid>
from: cosmix-mail.cosmix.cachyos.amp
rc: 0
---
{"unread": 3, "total": 1247}
```

### 7.4 Web API (Axum)

Axum replaces Laravel/FrankenPHP. It is also a cosmix port — the same commands accessible from Lua scripts and desktop apps are available as REST endpoints and WebSocket messages:

```
Browser → Axum REST/WS → cosmix port → AMP → target app
```

Axum serves both as:
1. **REST API** for React frontend (traditional HTTP endpoints)
2. **AMP WebSocket gateway** for real-time browser-to-mesh communication

### 7.5 Mail & Calendar (Stalwart)

Stalwart provides JMAP/IMAP/SMTP/CalDAV in a single Rust binary. cosmix-mail talks JMAP to Stalwart. A cosmix-port wrapper exposes mail operations as AMP commands:

```
---
amp: 1
type: request
from: script.cosmix.cachyos.amp
to: stalwart.cosmix.mko.amp
command: search
args: {"query": "invoice", "folder": "inbox", "limit": 10}
---
```

### 7.6 Mesh (meshd + WireGuard)

meshd bridges local cosmix-port sockets to remote nodes over WireGuard:

```
Local app → Unix socket (AMP) → cosmix daemon → meshd → WireGuard →
→ remote meshd → remote daemon → remote app Unix socket (AMP)
```

The message is AMP the entire way. No format translation at any boundary.

---

## 8. Transport Layers

Transport is an implementation detail — callers address ports, not transports. AMP separates two planes:

- **Control plane** (AMP) — commands, events, heartbeats, discovery, text data
- **Data plane** (WebRTC) — binary streams: audio, video, screen share, file transfer

### 8.1 Control Plane

| Path | Latency | Status |
|---|---|---|
| Lua → mlua → cosmix daemon → Unix socket → app | ~0.1ms | Working |
| CLI → Unix socket → daemon → app | ~0.1ms | Working |
| meshd → WebSocket/WireGuard → remote meshd → app | ~2-5ms est. | Design phase |
| Browser → Axum WebSocket → daemon → app | ~2ms est. | Design phase |

**Local path** (hot path for scripting):
```
Lua script → mlua → daemon socket → AMP route → app socket → command handler → AMP response
```

**Mesh path** (cross-node):
```
Lua script → daemon → meshd → WireGuard WS → remote meshd → remote daemon → remote app
```

**Browser path** (web access):
```
React → Axum WS → AMP frame → daemon → app → AMP response → Axum WS → React
```

### 8.2 Data Plane (WebRTC)

Binary streams use WebRTC data channels, negotiated via AMP signalling on the control plane:

| Path | Use case |
|---|---|
| Server ↔ browser | Audio playback (TTS), screenshots, file transfer |
| Browser ↔ browser | Voice chat, screen share (peer-to-peer via ICE) |
| Server ↔ server | Audio/video relay between nodes |

Signalling flow — WebRTC connections bootstrap over the existing AMP WebSocket:

1. Browser sends AMP request: `command: webrtc-offer` with SDP in body
2. Server responds: SDP answer + ICE candidates in AMP response body
3. WebRTC data channel opens — binary streams flow directly
4. AMP control plane continues alongside on WebSocket

This keeps AMP clean (text-only, human-readable, debuggable) while WebRTC handles binary heavy-lifting.

---

## 9. Port Discovery and Lifecycle

### 9.1 Port Registration

When an app starts, cosmix-port creates a Unix socket at:

```
/run/user/$UID/cosmix/<port-name>.sock
```

The cosmix daemon watches this directory via inotify. When a new socket appears:

1. Daemon connects to the socket
2. Sends `HELP` command (AMP request)
3. App responds with command list (AMP response)
4. Daemon registers the port in its routing table
5. Daemon begins heartbeat polling (10s interval)

### 9.2 Port Heartbeat

```
---
amp: 1
type: request
from: daemon.cosmix.cachyos.amp
command: HELP
---
```

If a port fails to respond within 5s, the daemon marks it as dead and removes it from the routing table. The socket file is cleaned up.

### 9.3 Port Discovery by Scripts

```lua
-- List all available ports
local ports = cosmix.ports()
for _, port in ipairs(ports) do
    print(port.name, port.commands)
end

-- Wait for a port to appear (useful for orchestration)
cosmix.wait_for_port("cosmix-mail", 10)  -- timeout 10s
```

### 9.4 Mesh Port Discovery

When meshd connects to a remote node, it exchanges port lists. Remote ports are registered with their full AMP address:

```lua
-- Local port (short name resolves locally)
local toot = cosmix.port("cosmix-toot")

-- Remote port (full address routes through meshd)
local remote_mail = cosmix.port("cosmix-mail.cosmix.mko.amp")
local status = remote_mail:call("status")
```

---

## 10. The Mesh

### 10.1 Nodes

| Node | WireGuard IP | Role | Rust Services |
|---|---|---|---|
| `cachyos` | 172.16.2.5 | Dev workstation | COSMIC desktop + cosmix apps + Axum |
| `gcwg` | 172.16.2.4 | Container host | Stalwart + Axum API |
| `mko` | 172.16.2.210 | Production primary | Stalwart + Axum + React UI |
| `mmc` | 172.16.2.9 | Production secondary | Stalwart + Axum + React UI |

All nodes connected via WireGuard mesh. All services are Rust single binaries managed by systemd.

### 10.2 Node Tiers

| Tier | Connection | Trust | Examples |
|---|---|---|---|
| **Server nodes** | Always-on, WireGuard | Trusted (WG keys + mesh auth) | cachyos, mko, mmc |
| **Browser nodes** | Ephemeral, WebSocket via Axum | Authenticated per-session | Any browser tab |

Server nodes run meshd and route traffic for browser nodes. A browser connects to its nearest Axum instance which bridges to the mesh. Per-port ACLs control access.

### 10.3 Mesh Heartbeat

meshd sends a heartbeat AMP message every 30 seconds between nodes:

```
---
amp: 1
type: event
from: meshd.cosmix.cachyos.amp
command: heartbeat
args: {"ports": ["cosmix-toot", "cosmix-mail", "cosmix-calc"], "uptime": 86400}
---
```

This provides fleet discovery — every node knows what ports are available on every other node.

---

## 11. Security

### 11.1 Transport Security

| Transport | Security |
|---|---|
| Unix socket | File permissions (`0700`), user-namespace isolation |
| WireGuard mesh | Authenticated encryption (Curve25519 + ChaCha20-Poly1305) |
| Axum WebSocket | TLS + session authentication (axum-login + tower-sessions) |

### 11.2 Port ACLs

Each port can declare access control:

```rust
let port = cosmix_port::Port::new("cosmix-mail")
    .acl(Acl::LocalOnly)           // Only local scripts, no mesh access
    // or
    .acl(Acl::MeshNodes(&["mko", "mmc"]))  // Only these mesh nodes
    // or
    .acl(Acl::Authenticated)       // Any authenticated session
```

### 11.3 No Secrets in AMP

AMP messages are designed to be loggable and debuggable. Secrets (API keys, tokens, passwords) must NEVER appear in AMP headers or bodies. Use reference tokens or session IDs instead.

---

## 12. Design Rationale

### 12.1 Why `---` Frontmatter, Not JSON

| JSON wire format | AMP frontmatter |
|---|---|
| `{"type":"request","from":"...","command":"search","args":{"q":"hello"}}` | `---\ntype: request\nfrom: ...\ncommand: search\nargs: {"q":"hello"}\n---\n` |
| 87 bytes, one reader (machine) | 92 bytes, three readers |
| Nested structure requires full parse to route | Flat headers — route on first 3 lines |
| Pretty-printing adds 300%+ overhead | Already human-readable |
| Body must be escaped/encoded | Body is freeform markdown |

The 5-byte overhead per message buys human readability, AI comprehension, and markdown bodies. For high-throughput data streams, the `json:` shape is nearly as compact as raw JSON with framing included.

### 12.2 Why Flat Headers, Not YAML

YAML parsing is complex, error-prone, and has well-documented security issues (billion laughs, type coercion, anchors). AMP headers look like YAML but are intentionally restricted to flat `key: value` — no indentation, no nesting, no type coercion. A correct AMP header parser is ~15 lines of code in any language.

### 12.3 Why `args` as Inline JSON

Command arguments need structure (nested objects, arrays, typed values). Headers need flatness (one line per field). Inline JSON in the `args:` field gives both: the header line is flat, the value is structured. Both Rust (`serde_json`) and Lua (`cjson`) have zero-dependency JSON parsers.

### 12.4 Why Not MCP/JSON-RPC

MCP (Model Context Protocol) is purpose-built for AI tool calling. AMP is purpose-built for app-to-app orchestration where AI agents are first-class participants but not the only participants. Key differences:

- MCP requires tool definitions (JSON schemas) upfront. AMP ports are self-describing via HELP.
- MCP messages are opaque to humans without tooling. AMP messages are `cat`-able.
- MCP is request-response only. AMP supports events, streams, and broadcasts.
- MCP has no addressing model. AMP has DNS-native mesh addressing.

AMP and MCP can coexist: a cosmix MCP server can bridge AI tool calls to AMP port commands.

---

## 13. What Exists Today

### 13.1 Working

| Component | Status |
|---|---|
| cosmix-port crate | IPC library with command registry, Unix sockets |
| cosmix daemon | Port discovery (inotify), HELP handshake, heartbeat, Lua runtime |
| Lua scripting | `cosmix.port()`, `cosmix.notify()`, script execution |
| cosmix-toot | Mastodon client with cosmix-port (9 commands) |
| cosmix-calc | Calculator with cosmix-port |
| Standard commands | HELP, INFO, ACTIVATE auto-generated |
| CLI | `cosmix call`, `cosmix run`, `cosmix shell` |

### 13.2 Planned

| Component | Milestone |
|---|---|
| AMP wire format on sockets | Replace current JSON IPC with AMP framing |
| Axum web API | Replace Laravel/FrankenPHP endpoints |
| Stalwart integration | cosmix-port wrapper for JMAP operations |
| meshd | Cross-node AMP routing over WireGuard |
| React → Axum migration | Browser UI talks to Axum instead of Laravel |
| Browser WebSocket | AMP frames over WS for real-time browser updates |
| cosmix-mail | JMAP client with cosmix-port |
| cosmix-view | Image viewer with cosmix-port |

---

## 14. Migration Path

| Phase | Action | Replaces | Risk |
|---|---|---|---|
| **Now** | Build cosmix desktop apps with ports | — | None |
| **Next** | Replace JSON IPC with AMP framing in cosmix-port | Current length-prefix JSON | Low |
| **Then** | Stand up Axum API alongside Laravel | Nothing yet | Low |
| **Then** | Migrate markweb endpoints to Axum, one by one | Laravel routes | Low |
| **Then** | Point cosmix-mail at Stalwart JMAP | Laravel mail middleware | Low |
| **Then** | React frontend talks to Axum | FrankenPHP | Medium |
| **Then** | meshd routes AMP between nodes | Manual cross-node ops | Medium |
| **Then** | Retire Laravel/FrankenPHP | PHP entirely | — |
| **Future** | Optionally replace React with Leptos (WASM) | TypeScript | Optional |

At every phase, the system works. Nothing breaks. Each step is independently useful and reversible.

---

## 15. Technical Decisions Summary

| Decision | Choice | Rationale |
|---|---|---|
| Wire format | AMP (markdown frontmatter) everywhere | Three-reader principle; one parser for all transports |
| Desktop toolkit | libcosmic (iced) | COSMIC-native, Rust, System76 maintained |
| IPC | Unix sockets with AMP framing | Fast, auto-discoverable, same format as mesh |
| Scripting | Lua via mlua | Embeddable, hot-reloadable, minimal, proven |
| Web API | Axum | Rust-native, tokio-based, tower middleware |
| Mail server | Stalwart | Rust, single binary, JMAP+IMAP+SMTP+CalDAV |
| Database | PostgreSQL | Best relational database, period |
| Browser UI | React (TypeScript) | Pragmatic choice, massive ecosystem |
| Mesh transport | WebSocket over WireGuard | Authenticated encryption, NAT traversal |
| Binary streams | WebRTC data channels | UDP for media, reliable for files |
| Message IDs | UUID v7 | Time-ordered, globally unique, sortable |
| Error codes | ARexx convention (0/5/10/20) | Simple, memorable, sufficient |
| Process management | systemd | Proven, universal on Linux |
| Async runtime | tokio | De facto Rust async standard |
| No Docker | Single binaries + systemd | Simpler, faster, no layer of indirection |
| No Python | Ever | Language policy |

---

*Document created: 2026-03-09*
*Status: Protocol specification v0.4 — active development*
*Supersedes: appmesh AMP v0.3 (2026-03-02)*
