---
title: ABP Display Protocol Specification
chapter: 5
version: 0.3.0
status: retired
date: 2026-04-06
retired: 2026-08-16
supersedes: All prior narrative design documents for ABP display
---

# ABP Display Protocol Specification

> **RETIRED 2026-08-16 (Mark).** The `ui.*` display surface this chapter
> specifies was retired from ABP by
> `_decisions/2026-07-18-amp-as-control-plane.md` — ABP is control-plane only;
> webd is the agent surface, and the desktop's display stack is the
> cosmix-comp compositor (chapter 16) plus the CTK toolkit (the 2026-07-30
> Bevy+CTK decision; CTK's widget surface is out of chapter 16's scope).
> Kept verbatim as dated history (hence the `-amp-` filename stem). Do not
> cite as live protocol. Carve-outs: chapter 18 (Mix citizen runtime) still
> cites §7.3 (non-reentrant handler model) and §15.2 (origin/`source_peer`
> discipline) as its normative reference, and chapter 03 cites §15.1 (the v1
> WireGuard /24 trust domain, still an accurate description of the deployed
> system) — those non-display contracts survive the retirement until their
> citing chapters inline them.

---

> **Vocabulary note (post-2026-04-26):** Chapter 01b
> (`2026-04-27-01b-amp-ui-vocabulary.md` v0.2.0) is the current canonical UI
> vocabulary. The `ui.panel` / `Panel` / `cosmix-deskd` names used
> throughout this chapter were renamed to `ui.window` / `Window`
> (and the reference backend renamed `cosmix-disp-wgpu`, now
> `cosmix-disp-skia`). Backends
> MUST accept `ui.panel` as a deprecated alias for `ui.window`
> through the 0.2.x line; this chapter has not yet been refreshed
> for the rename. Treat 01b as authoritative for command names
> until 05 is regenerated.

## 0. Preamble

### 0.1 Scope

This specification defines the ABP Display Protocol — the rules by which processes declare user interfaces via ABP messages and display services render them. It covers message formats, command vocabulary, content mapping, property semantics, scripted behavior, widget types, composition, theming, transport tiers, and security.

A conformant display service (renderer) can be implemented from this document alone. A conformant process (UI producer) can generate valid display messages from this document alone.

### 0.2 The Three-Layer Model

The ABP Display Protocol separates UI into three orthogonal layers, each carried as plain text within the same ABP message:

| Layer | Carries | Analogy | Format |
|-------|---------|---------|--------|
| **Markdown** | Content + structure | HTML | Message body (GFM) |
| **ABP headers** | Properties + layout | CSS | Message frontmatter (key: value) |
| **Mix scripts** | Behavior + logic | JavaScript | `~~~mix` code blocks or `script` header |

All three are plain text. All three travel over the same ABP transport (Unix socket, WebSocket, SMTP). The display service receives one ABP message and extracts all three layers from it.

Unlike the web platform, where HTML, CSS, and JavaScript evolved independently over 30 years with different authors, transport mechanisms, and mental models, these three layers are designed together on a unified message format. There is no loading order, no cascade, no cross-origin policy — one message, three layers, one parse.

### 0.3 Design Principles

1. **Plain text everywhere.** Every message is human-readable (`cat`, `grep`, markdown viewers work directly). Every message is machine-parseable (deterministic header extraction). Every message is AI-comprehensible (markdown is native LLM format).

2. **Protocol over framework.** The protocol defines how UI is declared. Renderers are swappable implementations. Apps are processes that speak the protocol, not binaries linked against a widget library.

3. **Fixed widget set.** The widget types defined in this spec are the complete vocabulary. No runtime extensibility, no plugin widgets, no code shipping. New widget types require a protocol version bump. (Audited 2026-06-05: the registry is now **28** types — the original 22 plus NumberInput, TreeView, TagList, Spinner, Breadcrumb, Accordion. Whether each addition carried the §3-required protocol-version bump is a reconciliation item: either record the bumps or restate the count as "the current protocol version's fixed vocabulary".)

4. **Headers route, bodies reason.** Routers parse only headers. Display services parse headers + body. Scripts reason over events. The same message serves all three readers at different depths.

5. **Graceful degradation.** A `ui.panel` message renders as a native window on a desktop, as HTML in a browser, as styled text in a terminal, and as readable plain text in a non-cosmix email client. Fidelity varies; readability is preserved.

6. **ABP at every app-facing boundary.** All communication that applications send or receive uses ABP — text, human-readable, language-agnostic. Internal display infrastructure (widget-to-widget within the same panel, shell↔deskd in Phase B) may use function calls or postcard for performance, but applications never see these. See `README.md` "Protocol Boundary Table" for the full matrix.

7. **Process owns state, display owns pixels.** The process is the single source of truth for all application state — data models, widget IDs, panel hierarchy, and mutation logic. The display service has no application state: it draws what it receives and reports user interactions, but does not generate IDs, manage data models, diff content, or reconcile updates. (It does maintain *rendering state* — the current panel tree, widget input values, scroll positions, focus — but this is derived entirely from incoming messages, never invented.) The broker routes messages between them but owns no application state. There is no virtual DOM, no reconciliation engine, no framework-managed component lifecycle. Every mutation is an explicit message from the process. This is the Tk/ARexx model, not the browser model.

### 0.4 State Ownership

The three participants in the display protocol have distinct, non-overlapping responsibilities:

| Concern | Process | Broker | Display Service |
|---------|---------|-----|-----------------|
| **ID allocation** | Assigns all panel, widget, and data item IDs | — | — |
| **Application state** | Source of truth (data models, selection, mode) | — | — |
| **State mutations** | Sends explicit commands (`patch`, `insert`, `remove`) | Routes them | Applies them to rendered output |
| **Panel registry** | — | Tracks which panel IDs exist (for routing, wildcards, orphan detection) | Tracks which panels are rendered (for hit testing, focus, scroll) |
| **Rendering** | — | — | Draws widgets, handles input, emits `ui.event` |
| **Event handling** | Receives `ui.event`, decides response, sends mutations | Routes events to subscribed processes | Detects user interaction, emits events |

**ID allocation rules:**

- Panel IDs: assigned by the process in the `id` header of `ui.panel`. The process chooses semantic, stable names (`mail-sidebar`, `file-browser`, `compose`).
- Widget IDs: assigned by the process in the `id` property of code block declarations (`~~~textinput id=to`).
- Data item IDs: assigned by the process (or its upstream data service) in the `id` field of JSON data objects. For example, `maild` generates email IDs; the process passes them through in `ui.data`.

No component of the system auto-generates IDs. If a process does not assign an `id`, the panel/widget/item cannot be targeted by subsequent commands.

**Recovery model:** If a process disconnects, its panels become orphaned (Section 10.3). When the process restarts, it sends fresh `ui.panel` messages — full state reconstruction from the process, not recovery from the display service. There is no "reconnect and diff against what's currently rendered."

### 0.5 Terminology

| Term | Definition |
|------|-----------|
| **ABP** | Agent Bus Protocol — markdown frontmatter wire format with BTreeMap headers + body |
| **Panel** | A rectangular UI surface declared by a `ui.panel` message. May be a top-level window or a nested region within another panel. |
| **Widget** | An interactive element within a panel, declared by a code block in the markdown body or created imperatively via `ui.panel` with a `type` header. |
| **Display service** | A process that receives `ui.*` messages and renders them as visible surfaces (Wayland windows, HTML elements, terminal regions). It is a stateless renderer — it does not own application state. |
| **Process** | Any program connected to the ABP broker that sends `ui.*` messages — a Mix script, a Rust daemon, a remote mesh peer. The process is the source of truth for application state. |
| **Broker** | The per-node ABP message router (cosmix-noded) that connects processes, display services, and mesh peers. Tracks panel IDs for routing but owns no application state. |
| **Mix** | The ARexx-inspired scripting language with native ABP keywords (`send`, `address`, `emit`, `on`). |
| **Mesh** | A WireGuard /24 network of cosmix nodes sharing a single trust domain. |
| **RC** | Return code. 0 = success, 5 = warning, 10 = error, 20 = failure. |

### 0.6 Notation

This specification uses RFC 2119 keywords: **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY**.

Header names are shown in `monospace`. Message examples use the ABP wire format with `---` delimiters.

---

## 1. Wire Format

### 1.1 Message Structure

Every ABP message consists of two parts: **headers** (a sorted key-value map) and a **body** (UTF-8 text). The wire format uses markdown frontmatter delimiters:

```
---
key1: value1
key2: value2
---
body content here
```

The opening `---\n` begins headers. Each subsequent line until the closing `---\n` is a header in `key: value` format. Everything after the closing delimiter is the body. The body is terminated by end-of-stream or the next message's opening `---\n`.

### 1.2 Header Syntax

- Headers are `key: value` pairs, one per line.
- Keys are case-sensitive, lowercase, using underscores or hyphens (`text_color`, `reply-to`).
- Values are UTF-8 strings. Leading whitespace after `:` is trimmed.
- Header order is deterministic (lexicographic by key) for consistent serialization.
- Duplicate keys: last value wins (but producers SHOULD NOT emit duplicates).

### 1.3 Body

- UTF-8 encoded text.
- Trailing whitespace is trimmed by parsers.
- May be empty (command-only messages).
- For `ui.panel` messages, the body is GFM markdown.
- For `ui.data` messages, the body is JSON.
- For `ui.style` and `ui.theme` messages, the body is `key: value` pairs.
- Body termination and framing follow the Chapter 01 v0.5 wire format. Messages are terminated by the `---\nEOM\n` end-of-message marker. Markdown horizontal rules (`---`) in the body are unambiguous because the parser requires both `---\n` and `EOM\n` in sequence to terminate a message. Panel body content MUST NOT contain the literal sequence `---\nEOM\n` (the ABP stream terminator). In practice, markdown bodies never do. See `2026-03-24-01-bus-wire-protocol.md` §5.1–5.2 for the grammar and stream framing rules.

### 1.4 Message Shapes

| Shape | Headers | Body | Use |
|-------|---------|------|-----|
| **Full** | Headers + fields | Markdown/text/JSON | Panels, events, rich responses |
| **Command** | `command` + args | (empty) | Requests, simple responses |
| **Data** | `command: ui.data` | JSON | Data-driven widget payloads |
| **Empty** | (none) | (none) | Heartbeat, ACK, keepalive, stream separator |

The empty message (`---\n---\n---\nEOM\n`) is the minimum valid ABP message. It carries no headers and no body. It serves as heartbeat on idle connections and as a natural separator in concatenated message streams. See `2026-03-24-01-bus-wire-protocol.md` §5.1 for the wire grammar.

### 1.5 Message Types

The `type` header identifies the message role:

| Type | Direction | Semantics |
|------|-----------|-----------|
| `request` | Process → service | Expects a response |
| `response` | Service → process | Reply to a request (carries `reply-to`) |
| `event` | Any → any | Fire-and-forget notification |
| `stream` | Service → process | Ongoing data feed |

Display protocol messages (`ui.panel`, `ui.style`, etc.) are typically `event` type — fire-and-forget from process to display service. `ui.event` messages from display to process are also `event` type. `ui.subscribe` is `request` type (expects acknowledgment).

### 1.6 Return Codes

| RC | Meaning | Use |
|----|---------|-----|
| 0 | Success | Normal completion |
| 5 | Warning | Partial success, degraded |
| 10 | Error | Bad arguments, not found |
| 20 | Failure | Severe error, service degraded |

### 1.7 Stream Framing

Stream framing, the parsing algorithm, transport-level message boundaries, and
error recovery are defined in `2026-03-24-01-bus-wire-protocol.md` §5.1–5.2. This chapter
does not restate those rules — Chapter 01 is authoritative for all wire-format
concerns.

Key points for display protocol implementors:

- Messages are terminated by the `---\nEOM\n` two-line marker (Ch01 v0.5).
- Markdown horizontal rules (`---`) in `ui.panel` bodies are unambiguous.
- On WebSocket transports, one text frame = one complete ABP message.
- On SMTP transports, the MIME boundary delimits the `text/x-amp-panel` part.

### 1.8 Required Headers

For non-empty messages, the following headers SHOULD be present:

| Header | Type | Purpose |
|--------|------|---------|
| `command` | string | The command name (e.g., `ui.panel`) |
| `msg_id` | string | Message identity for request/response correlation (UUIDv7 recommended) |
| `from` | string | Source address |

The `command` header is REQUIRED for all display protocol messages.

> **Note:** `msg_id` is message identity (request/response correlation, deduplication, logging). `id` on `ui.panel` is panel identity (targeting, hierarchy, lifecycle). These are distinct concepts and MUST NOT be conflated. A single `ui.panel` message may have both: `msg_id` for transport and `id` for the panel it creates.

---

## 2. Addressing

### 2.1 ABP Address Format

Addresses use a DNS-style dot-separated hierarchy with a `.amp` suffix:

```
node.amp                    — node level (2 segments)
app.node.amp                — app on a node (3 segments)
port.app.node.amp           — specific endpoint (4 segments)
widget-id.app.node.amp      — widget within an app (4 segments)
```

Segment count is fixed at each level. Internal hierarchy within a segment uses hyphens: `file-menu-save-as.edit.alpha.amp`.

Local shorthand: when addressing within the same node, the node and `.amp` suffix MAY be omitted. `cosmix-mail` is equivalent to `cosmix-mail.local-node.amp`.

### 2.2 Panel IDs

Panel IDs are strings that uniquely identify a panel within the broker's scope.

- IDs MUST be unique across the broker at any given time.
- IDs SHOULD be semantic and stable: `mail-compose`, `file-browser`, `statusbar`.
- IDs MUST NOT contain whitespace or the `*` wildcard character.
- Dot-separated prefixes MAY be used for logical grouping: `mail.sidebar`, `mail.compose`, `mail.list`.

### 2.3 Widget IDs

Widgets within a panel are identified by the `id` property in their code block declaration:

````markdown
```textinput id=to placeholder="To..."
```
````

Widget IDs are scoped to their parent panel. The fully-qualified widget reference
is `widget-id.panel-id` (for local) or `widget-id.app.node.amp` (for mesh).

> **Routing clarification:** The mesh form (`widget-id.app.node.amp`) is a
> logical reference for documentation and Mix scripts, not a broker-routable
> endpoint. The broker routes messages to processes based on the application
> address (`app.node.amp`). Widget resolution within a panel is performed
> internally by the display service using the `target` or `source` header.
> Mix scripts address widgets via `ui.event` headers, not by sending directly
> to a four-segment widget address.

### 2.4 Wildcard Targeting

The `target` header in `ui.style` messages supports glob-style wildcards:

| Pattern | Matches |
|---------|---------|
| `mail.*` | All panels with IDs starting with `mail.` |
| `*.sidebar` | All panels with IDs ending with `.sidebar` |
| `*` | All panels (use with extreme care) |

Wildcards expand at the broker level. The display service receives individual targeted messages.

> **Implementation note:** Wildcard expansion requires the broker to maintain a registry of all active panel IDs. This makes the broker stateful for display routing — it tracks panel creation and removal, not just connection routing. The broker already maintains process/port registries; the panel registry is a natural extension.

---

## 3. Display Protocol Commands

All display protocol commands use the `ui.` prefix. Commands are divided into two categories: **display commands** (process → display service) and **interaction commands** (display service → process or process → broker).

### 3.1 `ui.panel` — Create or Update Panel

Creates a new panel or updates an existing one. This is the primary display command.

**Direction:** Process → display service
**Semantics:** If the `id` does not exist, create a new panel. If it exists, update it.

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.panel"` | Command identifier |
| `id` | string | Panel identifier |

**Optional headers (window):**

> **Phase B migration:** Window-management headers (`position`, `layer`,
> `decorations`, `sticky`, `parent: desktop`) are handled by `cosmix-deskd` in
> Phase A. In Phase B, these concerns migrate to a dedicated `cosmix-shell`
> command surface. Applications SHOULD treat these headers as layout hints, not
> contracts. A conformant renderer MAY ignore window-management headers in
> constrained environments (TUI, nested child-panel, federated display).

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `parent` | string | `"desktop"` | Parent panel ID. `desktop` = top-level Wayland surface. |
| `title` | string | (none) | Window title for CSD title bar |
| `width` | size | `auto` | Panel width: `px`, `%`, `rem`, or `auto` |
| `height` | size | `auto` | Panel height |
| `position` | enum/coord | `auto` | `left`, `right`, `center`, `top`, `bottom`, `top-right`, `top-left`, `bottom-right`, `bottom-left`, or `x,y` coordinates |
| `decorations` | list | `close,minimize,maximize,resize,move` | Comma-separated CSD decorations |
| `layer` | enum | `normal` | `background`, `normal`, `overlay`, `notification` |
| `sticky` | bool | `false` | Survives workspace switches |
| `ttl` | integer | (none) | Auto-remove after N milliseconds from creation |

**Optional headers (layout):**

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `layout` | enum | `column` | `column`, `row`, `grid`, `stack` |
| `grid_template` | string | (none) | Grid track definition for `layout: grid`. See Section 3.1.1. |
| `gap` | float | `0` | Space between children (rem) |
| `padding` | float/quad | `0` | Inner padding (rem). Single value = all sides. Four values = top right bottom left. |
| `align` | enum | `stretch` | `start`, `center`, `end`, `stretch` |
| `scrollable` | bool | `false` | Enable scroll container |
| `overflow` | enum | `clip` | `clip`, `scroll`, `visible` |

**Optional headers (style):**

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `background` | color | `var(surface)` | Panel background. Hex or `var(name)`. |
| `text_color` | color | `var(text)` | Default text color |
| `border_color` | color | (none) | Panel border color |
| `border_width` | float | `0` | Border thickness (px) |
| `border_radius` | float | `0` | Corner rounding (rem) |
| `font_size` | float | `1` | Base font size (rem) |
| `opacity` | float | `1.0` | Panel opacity, 0.0–1.0 |

**Optional headers (behavior):**

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `script` | string | (none) | Path or URI to a Mix script file. See Section 7. |
| `collect_values` | bool | `false` | Include all widget values in `ui.event` payloads. See Section 7.5. |
| `subscribe` | string | (none) | Topic name for reactive data binding. See `2026-04-10-03-bus-topic-pubsub.md`. |

**Optional headers (federation):**

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `source_peer` | string | (none) | Originating mesh peer address. Set by transport, not by process. |
| `permissions` | string | (none) | `display` for federated display-only panels |

**Body:** GFM markdown content. See Section 4.

#### 3.1.1 Grid Template Layout

When `layout: grid` is set, the `grid_template` header defines named regions with explicit sizes. This allows a single panel to express complex spatial layouts without spawning child panels for every region.

**Syntax:** Pipe-separated track definitions. Each track is `name size`:

```
grid_template: sidebar 250px | content 1fr | details 300px
```

**Track sizes:**

| Unit | Meaning |
|------|---------|
| `px` | Fixed pixel width/height |
| `%` | Percentage of parent |
| `fr` | Fractional unit (fills remaining space proportionally) |
| `auto` | Fit to content |
| `min` | Minimum content size |
| `max` | Maximum content size |

**Rows:** By default, `grid_template` defines columns. For row definitions, use `grid_rows`:

```
grid_template: sidebar 250px | content 1fr
grid_rows: header 3rem | body 1fr | footer 2rem
```

**Child placement:** Child panels or markdown sections are placed into named grid areas by setting `grid_area: name` on the child panel:

```
---
command: ui.panel
id: mail-app
layout: grid
grid_template: sidebar 250px | content 1fr
grid_rows: toolbar 3rem | body 1fr | status 2rem
---
```

```
---
command: ui.panel
id: mail-sidebar
parent: mail-app
grid_area: sidebar
---
# Mailboxes
- Inbox (3)
- Sent
```

```
---
command: ui.panel
id: mail-toolbar
parent: mail-app
grid_area: toolbar
---
[Compose](mail.compose) [Refresh](mail.refresh)
```

When `grid_template` is set without child panels using `grid_area`, the grid auto-places children in order (first child → first cell, etc.), matching CSS Grid auto-placement behavior. If a child specifies a `grid_area` name that does not exist in the parent's `grid_template`, that child falls into the auto-placement flow as if `grid_area` were not set.

**When to use grid vs child panels:**

| Layout need | Approach |
|-------------|----------|
| Simple sidebar + content | `layout: row` with two child panels |
| Header/body/footer | `layout: column` with three child panels |
| Complex multi-region app | `layout: grid` with `grid_template` — fewer panels, one layout message |
| Dynamic regions (add/remove panes) | Child panels — each pane has independent lifecycle |

Grid reduces panel count for static layouts. Child panels remain the right choice when regions have independent lifecycles (created/removed at different times, different processes owning different regions).

**Update semantics:** When `ui.panel` targets an existing ID:
- The body is **replaced** entirely.
- Headers **present** in the update message **override** the stored values.
- Headers **absent** from the update message are **preserved** from the original.
- To clear a header, set it to an empty string.

**Example:**

```
---
command: ui.panel
id: mail-compose
parent: desktop
layout: column
width: 40%
height: 60%
title: Compose
decorations: close,minimize
---
# New Message

~~~textinput id=to placeholder="To..."
~~~

~~~textinput id=subject placeholder="Subject"
~~~

---

~~~textarea id=body rows=15
~~~

---

[Send](mail.send) [Attach](mail.attach) [Discard](mail.discard)
```

### 3.2 `ui.style` — Restyle Panel or Widget

Changes style properties of an existing panel or widget without replacing content.

**Direction:** Process → display service

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.style"` | Command identifier |
| `target` | string | Panel ID, widget ID, or wildcard pattern |

**Body:** `key: value` pairs, one per line. Keys are style property names from the property registry (Section 5). Values support `var(name)` references.

**Example:**

```
---
command: ui.style
target: file-browser
---
background: var(surface-dim)
border_color: var(primary)
border_width: 2
```

### 3.3 `ui.remove` — Remove Panel

Destroys a panel and frees its resources. All child panels are removed recursively (depth-first).

**Direction:** Process → display service

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.remove"` | Command identifier |
| `target` | string | Panel ID to remove |

**Body:** Empty.

**Cascade:** When a panel is removed, all panels with `parent` equal to the removed panel's `id` are also removed, recursively. Events in flight for removed panels are silently dropped.

### 3.4 `ui.event` — User Interaction

Sent by the display service when the user interacts with a panel or widget. This is the primary feedback channel from display to process.

**Direction:** Display service → process

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.event"` | Command identifier |
| `source` | string | Panel ID where the event originated |

**Body:** `key: value` pairs describing the event:

| Key | Type | Description |
|-----|------|-------------|
| `action` | string | Event type (e.g., `click`, `select`, `input`, `submit`, `expand`, `collapse`) |
| `widget` | string | Widget ID that generated the event (if widget-level) |
| `value` | string | Current value of the widget (for input widgets) |
| `row` | integer | Row index (for list/table selections) |
| `item` | string | Item ID (for list selections) |
| `checked` | bool | Checkbox/toggle state |

Additional keys MAY be present depending on the widget type. See Section 6 for per-widget event payloads.

**Example:**

```
---
command: ui.event
source: file-browser
---
action: select
widget: file-list
row: 0
item: notes-md
value: notes.md
```

### 3.5 `ui.theme` — Set Theme Variables

Sets or switches the active theme. All `var(name)` references across all panels resolve against the current theme.

**Direction:** Process → display service

**Optional headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.theme"` | Command identifier |
| `name` | string | Theme name (for identification) |

**Body:** `key: value` pairs mapping variable names to values. See Section 9 for standard variable names.

**Example:**

```
---
command: ui.theme
name: midnight
---
primary: #6366f1
primary-dim: #4f46e5
background: #0f0f1a
surface: #1a1a2e
surface-dim: #12122a
text: #e0e0e0
text-secondary: #999999
border: #333355
error: #ef4444
warning: #f59e0b
success: #10b981
```

### 3.6 `ui.data` — Push Data to Widget

Sends data to a data-driven widget (VirtualList, DataTable). See Section 12 for full data binding semantics.

**Direction:** Process → display service

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.data"` | Command identifier |
| `target` | string | Widget ID (within a panel) |

**Optional headers:**

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `action` | enum | `replace` | `replace`, `insert`, `remove`, `update`, `patch`, `clear` |
| `index` | integer | (none) | Row index for `insert` |
| `item` | string | (none) | Item ID for `remove`/`update`/`patch` |

**Body:** JSON. Format depends on `action`:

| Action | Body format |
|--------|-------------|
| `replace` | JSON array of objects (full dataset) |
| `insert` | Single JSON object |
| `update` | Single JSON object (full replacement of item) |
| `patch` | JSON-Patch array per RFC 6902, or partial JSON object (merge-patch per RFC 7396) |
| `remove` | (empty) |
| `clear` | (empty) |

Every data object MUST contain an `id` field (string) for incremental operations.

**Example (full replace):**

```
---
command: ui.data
target: mailbox-list
---
[
  {"id": "inbox", "name": "Inbox", "unread": 3, "icon": "inbox"},
  {"id": "sent", "name": "Sent", "unread": 0, "icon": "send"},
  {"id": "trash", "name": "Trash", "unread": 0, "icon": "trash-2"}
]
```

**Example (insert at position):**

```
---
command: ui.data
target: mailbox-list
action: insert
index: 1
---
{"id": "drafts", "name": "Drafts", "unread": 1, "icon": "file-text"}
```

### 3.7 `ui.template` — Set Data Template

Defines a markdown template for rendering data items in a data-driven widget. The template is stamped once per data item with `{field}` interpolation. See Section 12.

**Direction:** Process → display service

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.template"` | Command identifier |
| `target` | string | Widget ID |

**Body:** Markdown with `{field}` placeholders. Each placeholder is replaced with the corresponding field value from the data object.

**Example:**

```
---
command: ui.template
target: mailbox-list
---
- ![{icon}](lucide:{icon}) **{name}** ({unread})
```

### 3.8 `ui.batch` — Atomic Multi-Command

Executes multiple display commands atomically. The display service applies all commands before rendering a frame — no partial updates are visible to the user.

**Direction:** Process → display service

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.batch"` | Command identifier |

**Body:** JSON array of command objects. Each object has `command` (string), `headers` (object), and optional `body` (string).

> **Design note:** This is the only display command with a JSON body (all others use markdown or key-value pairs). JSON is chosen here because batch commands must be parsed atomically — concatenated ABP messages would require the renderer to buffer and group them, defeating the atomicity guarantee. The trade-off against the "cat and grep" principle is accepted for this single command.

**Example:**

```
---
command: ui.batch
---
[
  {
    "command": "ui.style",
    "headers": {"target": "sidebar"},
    "body": "width: 250\nbackground: var(surface-dim)"
  },
  {
    "command": "ui.style",
    "headers": {"target": "content"},
    "body": "width: 100%"
  }
]
```

### 3.9 `ui.subscribe` — Register Event Filter

Registers a process to receive `ui.event` messages matching a filter. This is the broker-level primitive that enables Mix's `on` keyword (Section 7).

**Direction:** Process → broker

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.subscribe"` | Command identifier |
| `type` | `"request"` | Expects acknowledgment |

**Body:** `key: value` filter criteria:

| Key | Type | Description |
|-----|------|-------------|
| `source` | string | Panel ID to watch (exact match or glob) |
| `action` | string | Event action to filter (optional — omit for all actions) |

**Response:** RC 0 with subscription ID.

**Semantics:** After subscription, the broker routes matching `ui.event` messages to the subscribing process. Multiple subscriptions from the same process are allowed.

**Example:**

```
---
command: ui.subscribe
type: request
msg_id: sub-001
---
source: file-browser
action: select
```

### 3.10 `ui.unsubscribe` — Deregister Event Filter

Removes a previously registered event subscription.

**Direction:** Process → broker

**Required headers:**

| Header | Type | Description |
|--------|------|-------------|
| `command` | `"ui.unsubscribe"` | Command identifier |
| `target` | string | Subscription ID (from `ui.subscribe` response) or panel source to unsubscribe from |

**Body:** Empty.

### 3.11 Widget and Menu Introspection

These commands enable ARexx-style discovery and scripting of the display
surface. A script can enumerate widgets, read their state, and invoke them
without hardcoding IDs — the same discover-then-script pattern used on the
Amiga. These are handled by the display service, not individual apps.

#### Widget introspection

| Command | Args | Returns | Notes |
|---------|------|---------|-------|
| `ui.list` | `{"prefix": "..."}` | `[{id, kind, label, state...}]` | All registered widgets (optional prefix filter) |
| `ui.get` | `{"id": "..."}` or `{"ids": [...]}` | `[{id, kind, state...}]` | Read specific widget state |
| `ui.invoke` | `{"id": "..."}` | `{"status":"ok"}` | Click/toggle a widget |
| `ui.highlight` | `{"id": "...", "ms": N}` | `{"status":"ok"}` | Visual pulse on a widget |
| `ui.set` | `{"id": "...", "value": "..."}` | `{"status":"ok"}` | Set widget value |

#### Menu introspection

| Command | Args | Returns | Notes |
|---------|------|---------|-------|
| `menu.list` | `{}` | `[{id, label, shortcut, enabled, menu}]` | All menu items |
| `menu.invoke` | `{"id": "..."}` | `{"status":"ok"}` | Simulate menu click |
| `menu.highlight` | `{"id": "...", "ms": N}` | `{"status":"ok"}` | Visual pulse on menu item |
| `menu.close` | `{}` | `{"status":"ok"}` | Close open dropdown |

#### Lifecycle

| Command | Args | Returns | Notes |
|---------|------|---------|-------|
| `config.changed` | `{}` | `{"status":"ok"}` | Theme reload notification (auto-handled) |

#### Discovery flow for scripts

A script targeting an unknown panel should:

1. `noded.list` → get registered services
2. `menu.list` → discover menu items and their IDs
3. `ui.list` → discover interactive widgets and their current state
4. `ui.get` → read specific widget values before acting
5. Then invoke/set as needed

This is the ARexx pattern: discover first, script second. Never hardcode widget
IDs without verifying they exist via `ui.list`.

---

## 4. Layer 1 — Markdown Content

### 4.1 Parser

The markdown body of a `ui.panel` message is parsed as GitHub Flavored Markdown (GFM) using the following extensions:

- Strikethrough (`~~text~~`)
- Task lists (`- [x] item`)
- Tables (`| col | col |`)

Parsers MUST use these extensions. Parsers MUST NOT add custom syntax beyond the code-block-as-widget convention defined below.

### 4.2 Element Mapping

Every GFM element maps to a visual widget in the rendered panel:

| Markdown Element | Widget Role | Renderer Behavior |
|-----------------|-------------|-------------------|
| `# Heading` (1-6) | Section title | Container boundary. Level determines visual hierarchy. |
| Paragraph | Text block | Inline-formatted text with wrapping. |
| `- item` | Unordered list item | Bullet + indented content. Clickable — emits `ui.event` with `action: select`. |
| `1. item` | Ordered list item | Number + indented content. |
| `- [x] item` | Checkbox item | Checkbox + label. Toggle emits `ui.event` with `action: toggle`, `checked: true/false`. |
| `**bold**` | Strong emphasis | Increased font weight. |
| `*italic*` | Light emphasis | Italic style. |
| `~~strike~~` | Strikethrough | Line-through decoration. |
| `` `code` `` | Inline code | Monospace font + background. |
| `[text](uri)` | Action button | Clickable. Click dispatches action URI (Section 8). |
| `![alt](src)` | Icon or image | If `src` starts with `lucide:`, render as Lucide icon. Otherwise load as image. |
| `> blockquote` | Info/status panel | Inset container with accent left border. |
| `---` | Divider | Horizontal rule (1px). |
| Table | Data grid | Column headers + rows. Rendered as grid layout. |
| Fenced code block | Widget or code display | If language hint is a known widget type → interactive widget (Section 4.3). Otherwise → monospace code block. |

### 4.3 Code Blocks as Widget Declarations

Fenced code blocks (backtick ` ``` ` or tilde `~~~`) with a language hint declare interactive widgets when the hint matches a known widget type (Section 6):

````markdown
```textinput id=to placeholder="To..." value="alice@example.com"
```
````

**Parsing rules:**

1. Extract the first word of the language hint as the widget type name.
2. Look up the name in the widget type registry (Section 6). If not found, render as a plain code block.
3. Parse remaining words as `key=value` properties. Quoted values (`key="multi word"`) preserve spaces.
4. The code block content (lines between the fences) is the widget's initial value/content.

**Example with content:**

````markdown
```textarea id=body rows=10
Default text content goes here.
It can span multiple lines.
```
````

### 4.4 `~~~mix` Code Blocks — Behavior Attachment

A fenced code block with language hint `mix` declares an inline script:

````markdown
~~~mix
on ui.event from "my-panel" action "submit"
  $name = $event.name
  send "greeting-service" greet name=$name
end
~~~
````

**Rules:**

1. `~~~mix` blocks MUST NOT be rendered as visible content.
2. The display service MUST extract `~~~mix` blocks and forward them to the script execution service (Section 7).
3. Multiple `~~~mix` blocks in a single panel body are concatenated in order.
4. `~~~mix` blocks in federated panels (`source_peer` is set) MUST be stripped silently.

### 4.5 Links as Actions

Markdown links are action buttons. The URL component is an action URI (Section 8):

```markdown
[Send](mail.send)
[Reply](mail.reply:msg-id-123)
[Delete](mail.delete:msg-id-123?confirm=true)
[Open Settings](ui.panel:settings)
[Visit Site](xdg-open:https://example.com)
```

When the user clicks a link, the display service dispatches the action URI. For ABP commands, this emits an ABP message. For `xdg-open`, this opens the URL in the system browser. See Section 8 for the complete scheme.

### 4.6 Images as Icons

Image syntax references icons or loads images:

```markdown
![inbox](lucide:inbox)              — Lucide icon by name
![avatar](https://example.com/a.jpg) — Remote image
![screenshot](/tmp/capture.png)      — Local file
```

The `lucide:` prefix resolves to the Lucide icon set. Renderers SHOULD support at minimum the Lucide icon set. Unknown icon names render as a placeholder.

---

## 5. Layer 2 — ABP Header Properties

### 5.1 Size Units

Size values in headers (`width`, `height`, `gap`, `padding`, etc.) accept these units:

| Unit | Syntax | Meaning |
|------|--------|---------|
| Pixels | `400` or `400px` | Absolute device pixels |
| Percent | `40%` | Relative to parent dimension |
| Rem | `2rem` | Relative to base font size (16px default) |
| Auto | `auto` | Fit to content |

Bare numbers without a unit suffix are interpreted as pixels for `width`/`height` and rem for `gap`/`padding`/`border_radius`/`font_size`.

### 5.2 Color Values

Color values in headers and style bodies accept:

| Format | Example | Description |
|--------|---------|-------------|
| Hex | `#6366f1` | 6-digit hex RGB |
| Hex+alpha | `#6366f180` | 8-digit hex RGBA |
| Short hex | `#63f` | 3-digit shorthand |
| Variable | `var(primary)` | Theme variable reference (Section 9) |

Named colors (e.g., `red`, `blue`) are NOT supported. Use hex values.

### 5.3 Variable References

The `var(name)` syntax resolves a value from the current theme (Section 9):

```
background: var(surface)
text_color: var(text-secondary)
border_color: var(primary)
```

Resolution is flat — one lookup, no cascade, no fallback chain. If the variable is not defined in the current theme, the renderer SHOULD use a sensible default (black for text, white for background, transparent for borders).

### 5.4 Property Categories

All properties that appear in `ui.panel` headers or `ui.style` bodies are defined in Section 3.1. This section serves as a cross-reference index.

**Window properties:** `id`, `parent`, `title`, `width`, `height`, `position`, `decorations`, `layer`, `sticky`, `ttl`
**Layout properties:** `layout`, `gap`, `padding`, `align`, `scrollable`, `overflow`
**Style properties:** `background`, `text_color`, `border_color`, `border_width`, `border_radius`, `font_size`, `opacity`
**Behavior properties:** `script`
**Federation properties:** `source_peer`, `permissions`

### 5.5 The `script` Header

The `script` header on a `ui.panel` message attaches a Mix script to the panel:

```
---
command: ui.panel
id: file-browser
script: ~/.config/cosmix/scripts/file-browser.mx
---
# Files
...
```

The value is a file path or URI. The script is loaded and executed when the panel is created, and terminated when the panel is removed. See Section 7 for full lifecycle semantics.

---

## 6. Widget Type Registry

This section defines the widget types recognized by the display protocol (28 as of 2026-06-05; the original 22 documented below plus NumberInput, TreeView, TagList, Spinner, Breadcrumb, Accordion — see §3 note), plus the `tooltip` cross-cutting property. For each widget type: accepted properties, emitted events, code block syntax, and degradation behavior.

### 6.1 Display Widgets

#### 6.1.1 Label

A text display element.

**Aliases:** `label`
**Code block:** `~~~label id=status` with text content.
**Degradation:** Rendered as plain text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `text_color` | color | `var(text)` | Text color |
| `font_size` | float | `1` | Font size (rem) |
| `font_weight` | integer | `400` | Font weight (100–900) |
| `align` | enum | `start` | Text alignment: `start`, `center`, `end` |
| `wrap` | bool | `true` | Enable text wrapping |

**Events:** None. Labels are non-interactive.

#### 6.1.2 Icon

An icon from the Lucide icon set or a custom SVG.

**Aliases:** `icon`
**Code block:** `~~~icon id=status name=check-circle`
**Degradation:** Rendered as `[icon-name]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `name` | string | (required) | Lucide icon name |
| `size` | float | `1.25` | Icon size (rem) |
| `color` | color | `var(text)` | Icon color |

**Events:** None.

#### 6.1.3 Image

Displays a raster image (PNG, JPEG, WebP) or SVG.

**Aliases:** `image`, `img`
**Code block:** `~~~image id=avatar src=/path/to/image.png`
**Degradation:** Rendered as `[alt-text]`.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `src` | string | (required) | Image path, URL, or blob reference |
| `alt` | string | `""` | Alt text for accessibility |
| `width` | size | `auto` | Image width |
| `height` | size | `auto` | Image height |
| `fit` | enum | `contain` | `contain`, `cover`, `fill`, `none` |

**Events:** None.

**Security:** For federated panels (`source_peer` set), `src` values MUST be
validated. Local file paths (`/path/to/...`, `~/...`, `file://`) MUST be
rejected — federated panels may only reference blob IDs or HTTPS URLs.
Renderers MUST NOT resolve relative paths against the local filesystem for
federated content.

#### 6.1.4 Markdown

A sub-document rendered as markdown. Enables nested markdown content within imperatively-created widget trees.

**Aliases:** `markdown`, `md`
**Code block:** `~~~markdown` with markdown content.
**Degradation:** Rendered as plain text (markdown is already readable).

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |

**Events:** Action link clicks within the markdown are dispatched as `ui.event` with the action URI. Same behavior as panel-level markdown.

### 6.2 Layout Widgets

#### 6.2.1 Container

A layout container that holds other widgets. Uses flexbox semantics via taffy.

**Aliases:** `container`, `div`
**Code block:** `~~~container id=toolbar layout=row gap=0.5`
**Degradation:** Contents rendered sequentially.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `layout` | enum | `column` | `column`, `row`, `grid`, `stack` |
| `gap` | float | `0` | Space between children (rem) |
| `padding` | float/quad | `0` | Inner padding (rem) |
| `align` | enum | `stretch` | `start`, `center`, `end`, `stretch` |
| `background` | color | transparent | Container background |
| `border_color` | color | (none) | Border color |
| `border_width` | float | `0` | Border thickness (px) |
| `border_radius` | float | `0` | Corner rounding (rem) |

**Events:** None. Containers are structural.

#### 6.2.2 ScrollContainer

A container with scrollable overflow.

**Aliases:** `scroll`, `scrollcontainer`
**Code block:** `~~~scroll id=content direction=vertical`
**Degradation:** Contents rendered sequentially.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `direction` | enum | `vertical` | `vertical`, `horizontal`, `both` |
| `max_height` | size | (none) | Maximum height before scrolling |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `scroll` | `offset: float, max: float` | User scrolls |

#### 6.2.3 Tabs

A tabbed container with tab bar and content switching.

**Aliases:** `tabs`
**Code block:** `~~~tabs id=mail-tabs active=inbox tabs="Inbox,Sent,Drafts"`
**Degradation:** All tab contents rendered sequentially with tab names as headings.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `tabs` | string | (required) | Comma-separated tab labels |
| `active` | string | (first tab) | ID or label of the active tab |
| `position` | enum | `top` | Tab bar position: `top`, `bottom`, `left`, `right` |
| `closable` | bool | `false` | Show close button on tabs |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `select` | `value: string` (tab label) | User clicks a tab |
| `close` | `value: string` (tab label) | User clicks tab close button |

#### 6.2.4 SplitPane

A container with two children separated by a draggable divider.

**Aliases:** `splitpane`, `split`
**Code block:** `~~~split id=main direction=horizontal ratio=30:70`
**Degradation:** Both panes rendered sequentially.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `direction` | enum | `horizontal` | `horizontal` (left/right) or `vertical` (top/bottom) |
| `ratio` | string | `50:50` | Initial split ratio (e.g., `30:70`) |
| `min_size` | float | `5` | Minimum pane size (%) |
| `collapsible` | bool | `false` | Allow collapsing to zero |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `resize` | `ratio: string` (new ratio) | User drags divider |

### 6.3 Input Widgets

#### 6.3.1 Button

A clickable button.

**Aliases:** `button`, `btn`
**Code block:** `~~~button id=send label="Send Message" variant=primary`
**Degradation:** Rendered as `[label]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `label` | string | `""` | Button text |
| `variant` | enum | `default` | `default`, `primary`, `danger`, `ghost` |
| `disabled` | bool | `false` | Disable interaction |
| `icon` | string | (none) | Lucide icon name |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `click` | (none) | User clicks button |

#### 6.3.2 TextInput

A single-line text input field.

**Aliases:** `textinput`, `input`
**Code block:** `~~~textinput id=email placeholder="Email" value="user@example.com"`
**Degradation:** Rendered as `[value]` or `[placeholder]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `value` | string | `""` | Current text value |
| `placeholder` | string | `""` | Placeholder text |
| `disabled` | bool | `false` | Disable interaction |
| `password` | bool | `false` | Mask input characters |
| `max_length` | integer | (none) | Maximum character count |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `input` | `value: string` | Text changes (debounced) |
| `submit` | `value: string` | User presses Enter |
| `focus` | (none) | Widget gains focus |
| `blur` | `value: string` | Widget loses focus |

#### 6.3.3 TextArea

A multi-line text input. Implementation complexity: very hard (cosmic-text integration for cursor, selection, wrapping, IME). Renderers handle local text buffering, cursor, selection, and IME state; `ui.event` with `action: input` SHOULD be debounced or emitted on blur/submit to avoid per-keystroke protocol overhead.

**Aliases:** `textarea`
**Code block:** `~~~textarea id=body rows=10` with initial content.
**Degradation:** Rendered as indented text block.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `value` | string | (content) | Current text. Initial value is the code block content. |
| `placeholder` | string | `""` | Placeholder text |
| `rows` | integer | `5` | Visible row count |
| `disabled` | bool | `false` | Disable interaction |
| `max_length` | integer | (none) | Maximum character count |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `input` | `value: string` | Text changes (debounced) |
| `focus` | (none) | Widget gains focus |
| `blur` | `value: string` | Widget loses focus |

#### 6.3.4 Dropdown

A selection dropdown (single-select).

**Aliases:** `dropdown`, `select`
**Code block:** `~~~dropdown id=priority options="Low,Normal,High" value="Normal"`
**Degradation:** Rendered as `[value]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `options` | string | (required) | Comma-separated option labels. Or JSON array for label+value: `[{"label":"Low","value":"1"}]` |
| `value` | string | (first option) | Currently selected value |
| `placeholder` | string | `""` | Placeholder when no selection |
| `disabled` | bool | `false` | Disable interaction |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `change` | `value: string` | Selection changes |

#### 6.3.5 Checkbox

A boolean checkbox with label.

**Aliases:** `checkbox`
**Code block:** `~~~checkbox id=agree label="I agree" checked=true`
**Degradation:** Rendered as `[x]` or `[ ]` with label.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `label` | string | `""` | Checkbox label text |
| `checked` | bool | `false` | Current checked state |
| `disabled` | bool | `false` | Disable interaction |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `change` | `checked: bool` | User toggles checkbox |

#### 6.3.6 Toggle

A slide-switch toggle (on/off).

**Aliases:** `toggle`, `switch`
**Code block:** `~~~toggle id=dark_mode label="Dark mode" checked=true`
**Degradation:** Rendered as `(on)` or `(off)` with label.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `label` | string | `""` | Toggle label text |
| `checked` | bool | `false` | Current state |
| `disabled` | bool | `false` | Disable interaction |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `change` | `checked: bool` | User toggles switch |

#### 6.3.7 RadioGroup

A mutually exclusive set of options (only one can be selected).

**Aliases:** `radiogroup`, `radio`
**Code block:** `~~~radio id=priority options="Low,Normal,High" value="Normal"`
**Degradation:** Rendered as `(x) selected / ( ) unselected` text list.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `options` | string | (required) | Comma-separated option labels. Or JSON: `[{"label":"Low","value":"1"}]` |
| `value` | string | (first option) | Currently selected value |
| `direction` | enum | `column` | `column` or `row` layout |
| `disabled` | bool | `false` | Disable interaction |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `change` | `value: string` | Selection changes |

#### 6.3.8 Slider

A range slider for numeric values.

**Aliases:** `slider`, `range`
**Code block:** `~~~slider id=opacity min=0 max=100 value=80 step=1`
**Degradation:** Rendered as `[value]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `min` | float | `0` | Minimum value |
| `max` | float | `100` | Maximum value |
| `value` | float | `min` | Current value |
| `step` | float | `1` | Step increment |
| `label` | string | (none) | Optional label |
| `disabled` | bool | `false` | Disable interaction |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `input` | `value: float` | User drags slider (continuous, may be coalesced) |
| `change` | `value: float` | User releases slider (final value) |

#### 6.3.9 Progress

A progress bar or indeterminate spinner.

**Aliases:** `progress`
**Code block:** `~~~progress id=upload value=65 max=100 label="Uploading..."`
**Degradation:** Rendered as `[65%]` or `[loading...]` text.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `value` | float | (none) | Current progress. Omit for indeterminate (spinner). |
| `max` | float | `100` | Maximum value |
| `label` | string | (none) | Text label beside the bar |
| `variant` | enum | `default` | `default`, `success`, `warning`, `error` |

**Events:** None. Progress bars are display-only.

**Updates:** Use `ui.set` to update `value` (widget state): `send "ui" set target="upload" value="75"`. Use `ui.style` to update `variant` and `label` (presentation). The distinction: `value` is data the widget tracks, `variant`/`label` are how it looks.

### 6.4 Data Widgets

#### 6.4.1 VirtualList

A scrollable list that only renders visible items (windowed/virtualized rendering). For large datasets. Uses `ui.template` for item rendering and `ui.data` for data.

**Aliases:** `virtuallist`, `vlist`
**Code block:** `~~~vlist id=email-list item_height=2.5`
**Degradation:** First N items rendered as list items (N = renderer discretion).

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `item_height` | float | `2.5` | Height of each item (rem) |
| `indent` | bool | `false` | Enable indentation (for tree-like lists) |
| `selectable` | bool | `true` | Items are selectable |
| `multi_select` | bool | `false` | Allow multi-selection |
| `background` | color | transparent | List background |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `select` | `item: string, index: int` | User selects an item |
| `activate` | `item: string, index: int` | User double-clicks/enters an item |
| `expand` | `item: string` | User expands a tree node (when `indent: true`) |
| `collapse` | `item: string` | User collapses a tree node |
| `scroll` | `offset: int, visible: int` | Scroll position changes |

**Data format:** JSON array of objects. Each object MUST have an `id` field. For indented/tree lists, objects MAY have a `depth` field (integer, 0-based) and `expandable` field (bool).

```json
[
  {"id": "inbox", "name": "Inbox", "unread": 3, "icon": "inbox"},
  {"id": "sent", "name": "Sent", "unread": 0, "icon": "send"}
]
```

**Template:** Markdown with `{field}` interpolation. Rendered once per item.

```markdown
- ![{icon}](lucide:{icon}) **{name}** ({unread})
```

#### 6.4.2 DataTable

A tabular data display with column headers, sorting, and virtual scrolling.

**Aliases:** `datatable`, `table`
**Code block:** `~~~datatable id=files columns="Name,Size,Modified" sortable=true`
**Degradation:** Rendered as a GFM markdown table (first N rows).

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `columns` | string | (required) | Comma-separated column names. Or JSON: `[{"name":"Name","key":"name","width":"40%"}]` |
| `sortable` | bool | `false` | Enable column header click to sort |
| `sort_column` | string | (none) | Currently sorted column key |
| `sort_order` | enum | `asc` | `asc` or `desc` |
| `selectable` | bool | `true` | Rows are selectable |
| `row_height` | float | `2` | Row height (rem) |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `select` | `item: string, row: int` | User selects a row |
| `activate` | `item: string, row: int` | User double-clicks a row |
| `sort` | `column: string, order: string` | User clicks column header |

**Data format:** JSON array of objects. Keys correspond to column keys.

```json
[
  {"id": "notes-md", "name": "notes.md", "size": "2.1 KB", "modified": "2026-04-06"},
  {"id": "report-pdf", "name": "report.pdf", "size": "145 KB", "modified": "2026-04-05"}
]
```

### 6.5 Chrome Widgets

#### 6.5.1 MenuBar

Application menu bar with CSD (client-side decoration) integration.

**Aliases:** `menubar`
**Code block:** Not typically used in markdown. Created imperatively or by app frameworks.
**Degradation:** Menu items rendered as a list.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `menus` | JSON | (required) | Menu structure: `[{"label":"File","items":[{"id":"file.new","label":"New","shortcut":"Ctrl+N"}]}]` |
| `caption_buttons` | bool | `true` | Show minimize/maximize/close buttons |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `invoke` | `item: string` (menu item ID) | User clicks menu item |

#### 6.5.2 ContextMenu

Right-click popup menu.

**Aliases:** `contextmenu`
**Code block:** Not used in markdown. Triggered via `ui.event` with `action: context-menu`.
**Degradation:** Not rendered (context menus are inherently interactive).

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `items` | JSON | (required) | Menu items: `[{"id":"edit.copy","label":"Copy","shortcut":"Ctrl+C"}]` |
| `position` | string | (required) | `x,y` coordinates |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `invoke` | `item: string` | User clicks menu item |
| `close` | (none) | Menu dismissed |

#### 6.5.3 Dialog

A modal overlay anchored to its parent panel.

**Aliases:** `dialog`, `modal`
**Code block:** `~~~dialog id=confirm title="Confirm Delete"` with markdown body.
**Degradation:** Content rendered as blockquote.

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `id` | string | (required) | Widget identifier |
| `title` | string | (none) | Dialog title |
| `width` | size | `400` | Dialog width |
| `closable` | bool | `true` | Show close button |
| `backdrop` | bool | `true` | Show dimmed backdrop behind dialog |

**Events:**

| Event | Payload | Trigger |
|-------|---------|---------|
| `close` | (none) | User clicks close or backdrop |

Dialog content is markdown (headings, text, buttons via action links). The dialog is modal to its parent panel — other panels remain interactive.

**Nesting:** Dialogs MAY spawn child dialogs (e.g., a confirmation inside a settings dialog). Each child dialog is modal to its parent dialog. When a parent dialog is closed, all child dialogs are closed first (depth-first cascade, same as panel removal in Section 3.3).

**Parent removal:** If a dialog's parent panel is removed, the dialog is removed as part of the cascade. Events in flight for the dialog are dropped.

#### 6.5.4 Tooltip

A hover-triggered popup with informational content.

**Aliases:** `tooltip`
**Code block:** Not a standalone widget. Applied as a property on other widgets via `tooltip="Help text"`.
**Degradation:** Not rendered (content is supplementary).

Any interactive widget MAY include a `tooltip` property:

```
~~~button id=send label="Send" tooltip="Send the message (Ctrl+Enter)"
~~~
```

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `tooltip` | string | (none) | Tooltip text (available on all interactive widgets) |
| `tooltip_position` | enum | `auto` | `auto`, `top`, `bottom`, `left`, `right` |

The renderer displays the tooltip on hover (mouse) or long-focus (keyboard/touch). Tooltips are plain text, not markdown.

**Events:** None.

### 6.6 Focus and Keyboard Navigation

Renderers at Level 1 or above MUST implement focus management for interactive widgets.

**Focus order:** Widgets receive focus in document order (the order they appear in the markdown body or the order they were created via `ui.panel`). The `tabindex` property overrides document order:

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `tabindex` | integer | `0` | Focus order. `0` = document order. Positive = explicit order. `-1` = skip in tab sequence (focusable only programmatically). |

**Keyboard requirements:**

| Key | Behavior |
|-----|----------|
| Tab | Move focus to next focusable widget |
| Shift+Tab | Move focus to previous focusable widget |
| Enter | Activate focused button/link, submit focused TextInput |
| Space | Toggle focused Checkbox/Toggle, activate focused Button |
| Escape | Close focused Dialog/ContextMenu/Dropdown |
| Arrow keys | Navigate within Dropdown options, VirtualList/DataTable rows, Slider value |

**Focus events:** When a widget gains focus, the renderer MAY emit a `ui.event` with `action: focus`. When it loses focus, `action: blur` with the current `value`.

**Keyboard shortcuts:** MenuBar items with a `shortcut` property (e.g., `"Ctrl+N"`) are global within the panel's window. The renderer MUST intercept these key combinations and emit the corresponding `invoke` event.

### 6.7 Accessibility

> **Note:** Full accessibility semantics are deferred to v1.1. This section defines the minimum requirements for v1.0.

Renderers SHOULD map widget types to platform accessibility roles:

| Widget | Accessibility Role |
|--------|--------------------|
| Button | `button` |
| TextInput | `textbox` |
| TextArea | `textbox` (multiline) |
| Checkbox | `checkbox` |
| Toggle | `switch` |
| Slider | `slider` |
| Dropdown | `combobox` |
| VirtualList | `list` / `listitem` |
| DataTable | `table` / `row` / `cell` |
| Dialog | `dialog` |
| MenuBar | `menubar` / `menuitem` |
| Heading | `heading` (with level) |

Renderers using accesskit SHOULD build an accessibility tree from the widget tree. Image widgets MUST expose their `alt` text to screen readers.

---

## 7. Layer 3 — Mix Script Behavior

### 7.1 Overview

Mix scripts provide the behavior layer — the logic that responds to user interactions, manages data, and updates panels dynamically. Mix is an ARexx-inspired scripting language with native ABP keywords (`send`, `address`, `emit`) already implemented, and a new `on` keyword defined by this specification.

The display service NEVER executes scripts. Scripts run as processes connected to the broker. The broker routes `ui.*` commands from script → display and `ui.event` from display → script. This separation keeps the renderer simple and secure.

### 7.2 Attachment Modes

#### 7.2.1 Inline Scripts

A `~~~mix` code block in a `ui.panel` markdown body attaches behavior to that panel:

````
---
command: ui.panel
id: notepad
title: Notes
width: 400
height: 500
---
# Quick Notes

~~~textarea id=editor rows=20
~~~

[Save](note.save) [Clear](note.clear)

~~~mix
on ui.event from "notepad" action "note.save"
  $content = $event.editor
  send "file-service" write path="~/notes.md" content=$content
  emit "ui" panel id="toast" layer="notification" ttl="2000" body="> Saved."
end
~~~
````

**Processing:**

1. Display service receives `ui.panel` message.
2. Display service parses the markdown body, encounters `~~~mix` block.
3. Display service extracts the script text and forwards it to a script execution service. The handoff mechanism is implementation-specific — it could be an internal `ui.script` message on the broker, a direct function call within the same process, or a Unix socket to a dedicated script runner. The protocol does not mandate how scripts are delivered to the executor, only that the display service does not execute them itself.
4. The script service creates a Mix evaluator, injects `$PANEL_ID` as a context variable, and executes the script.
5. The script's `on` handlers register event subscriptions with the broker.
6. The remaining markdown (without the `~~~mix` block) is rendered normally.

#### 7.2.2 Referenced Scripts

The `script` header on a `ui.panel` message names an external Mix script:

```
---
command: ui.panel
id: file-browser
script: ~/.config/cosmix/scripts/file-browser.mx
title: Files
---
# ~/Documents
```

**Processing:**

1. Display service receives `ui.panel`, notes `script` header.
2. Display service forwards the script path to the script execution service.
3. Script service loads the file, creates a Mix evaluator, injects `$PANEL_ID`, and executes.
4. The script creates sub-widgets, loads data, registers event handlers.

#### 7.2.3 Standalone Scripts

A Mix script launched independently (via `mix file-browser.mx` or by a daemon) connects to the broker and sends `ui.*` messages directly. No `ui.panel` message is needed first — the script IS the process:

```mix
#!/usr/bin/env mix
-- File browser — standalone script app
address "ui"

send "panel" id="file-browser" parent="desktop" title="Files" +
     width="800" height="600" layout="column"

send "panel" id="file-sidebar" parent="file-browser" width="250" +
     body markdown("
# Folders

- ![home](lucide:home) Home
- ![documents](lucide:folder) Documents
- ![downloads](lucide:download) Downloads
")

on ui.event from "file-sidebar" action "select"
  $path = $event.value
  $files = send "file-service" list path=$path
  send "panel" id="file-list" parent="file-browser" body format_file_list($files)
end
```

**Lifecycle:** Standalone scripts manage their own lifecycle. They SHOULD remove their panels on exit. The broker MAY auto-remove orphaned panels when a process disconnects.

### 7.3 The `on` Keyword

The `on` keyword registers an asynchronous event handler. It is a Mix language keyword defined by this specification and implemented by Mix as amended by SPEC 18 Phase 2 (the `async` modifier and per-`send` `timeout=` kwarg below).

**Syntax:**

```
on command.name [from "source"] [action "type"] [async]
  -- handler body
done
```

The command name is a bare dotted identifier (unquoted), matching the Mix
parser's `parse_on` rule (`2026-04-13-04-mix-language-reference.md`). Filter values
(`from`, `action`) are quoted strings. The optional trailing `async`
modifier selects the Class C concurrency model (see semantic rule 6
below); it is a contextual identifier, not a global reserved word
(existing scripts using `async` as a variable name are unaffected).
Handlers close with `done` (canonical); the legacy `end` terminator
is also accepted by the parser.

**Semantics:**

1. `on` sends a `ui.subscribe` message to the broker with the specified filter criteria.
2. The broker routes matching `ui.event` messages to the script's process.
3. When a matching event arrives, the Mix evaluator executes the handler body.
4. The `$event` variable is implicitly available inside the handler, containing all event payload fields as a Mix object.
5. Multiple `on` handlers for the same source are allowed. All matching handlers execute in registration order.
6. **Concurrency model: cooperative, single-threaded, two classes.**
   Handlers execute under a single-threaded cooperative scheduler
   consistent with ARexx's single-threaded message loop. **A handler
   is never re-entered while it is suspended** — every invocation
   carries its own private activation frame (its own locals, its own
   `$event`), so an invocation's state is stable even when other
   invocations of the same handler are interleaving alongside it.
   The two classes differ only in *whether another invocation may
   interleave during this one's awaits*:
   - **Class S (default — no `async` modifier).** A Class S handler
     holds the dispatch loop for its entire body. While a Class S
     handler is running (including across its `send` awaits), no
     other event is dispatched. Handlers that block on a slow `send`
     stall the queue — for a UI this is "feels frozen"; for a
     registered request-serving citizen, concurrent callers serialise
     behind the slowest downstream `send` (head-of-line blocking).
     This preserves run-to-completion atomicity for pre-Phase-2
     scripts. Long-running Class S handlers SHOULD use `emit`
     (fire-and-forget) instead of `send` (blocking) where possible.
   - **Class C (`on <cmd> async`, SPEC 18 §3.7 Phase 2).** A Class C
     handler **releases** the dispatch reader at every `send`,
     `reply`, and `sleep_ms` await point (and reacquires it before
     resuming), letting another invocation (typically a different
     caller's event chain) interleave through. Class C handlers run
     on a single-threaded cooperative task set, so two Class C
     invocations share the citizen's local state without cross-
     thread races. The scheduler enforces "one writer or many
     readers": Class S runs as a writer (locks the loop); Class C
     invocations coexist as readers (concurrent reader permits) and
     yield around every await so they don't starve each other.
     Synchronous request cycles (A→B→A across two single-threaded
     citizens) remain prohibited *by design* (SPEC 18 §3.7 deadlock
     corollary) — Class C does *not* legalise reentrancy; it only
     removes head-of-line blocking for *acyclic* slow-downstream
     handlers. **When to use Class C:** the handler issues a `send`
     to a possibly-slow downstream AND the citizen is registered
     (request-addressable, structurally concurrently-callable).
     Leave the modifier off for pure-local-state or fast handlers,
     and for unregistered transient ABP clients (they have no
     concurrent callers to head-of-line-block; see SPEC 18 §3.7
     sole-caller carve-out).
7. **Per-`send` timeout (SPEC 18 Phase 2 WS4).** Any `send` inside
   either handler class accepts a `timeout=<sec>` kwarg (fractional
   seconds permitted). On timeout the `send` writes ARexx-shaped
   result vars and returns `nil` to its expression position:
   `$rc = "-1"` (string — same convention as other transport
   failures, not a new `rc="timeout"` namespace) and
   `$result = "timeout: send to <target> exceeded <sec>s"`.
   Cancellation is cooperative: the citizen frees its pending-reply
   slot immediately, but does not abort the in-flight downstream
   request — a late downstream reply arrives at the broker with no
   matching correlation id and is dropped. Scope is one `send`;
   there is no handler-wide timeout. See
   `2026-04-13-04-mix-language-reference.md` § *Event handlers (`on`)* for the
   syntax example.

**Filter parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| command | bare identifier | Dotted ABP command to match (typically `ui.event`) |
| `from "source"` | string | Panel or widget ID to filter on (exact match or glob) |
| `action "type"` | string | Event action type to filter on |

**Examples:**

```mix
-- Handle any event from a panel
on ui.event from "file-browser"
  say "Event:" .. $event.action .. " from " .. $event.widget
end

-- Handle only select events
on ui.event from "mailbox-list" action "select"
  $mailbox = $event.item
  $emails = send "maild" list mailbox=$mailbox
  send "panel" id="email-list" body format_emails($emails)
end

-- Handle events from any panel matching a glob
on ui.event from "settings.*" action "change"
  send "config-service" set key=$event.widget value=$event.value
end
```

### 7.4 The `off` Keyword

Deregisters an event handler previously registered with `on`.

**Syntax:**

```
off command.name [from "source"] [action "type"]
```

Under the hood, sends `ui.unsubscribe` to the broker. The filter parameters must match those used in the corresponding `on` call.

### 7.5 The `$event` Variable

Inside an `on` handler body, the `$event` variable is a Mix object containing all key-value pairs from the `ui.event` body:

```mix
on ui.event from "file-list" action "select"
  say $event.action     -- "select"
  say $event.widget     -- "file-list"
  say $event.item       -- "notes-md"
  say $event.row        -- 0
  say $event.value      -- "notes.md"
end
```

By default, `$event` contains only the fields emitted by the triggering widget. For form-style panels where a script needs all widget values at once, the panel MAY declare `collect_values: true`:

```
---
command: ui.panel
id: compose
collect_values: true
---
```

When `collect_values` is `true`, the display service includes the current value of every named input widget in the panel as additional fields on every `ui.event` from that panel. This allows form submission to access all field values:

```mix
on ui.event from "compose" action "mail.send"
  $to = $event.to           -- value of textinput id=to
  $subject = $event.subject -- value of textinput id=subject
  $body = $event.body       -- value of textarea id=body
  send "maild" send to=$to subject=$subject body=$body
end
```

Without `collect_values`, the script would need to query individual widget values via `ui.get` or track state manually. The opt-in design avoids serializing 30 widget values on every keystroke in large panels.

### 7.6 Script Lifecycle

| Mode | Created | Destroyed | Cleanup |
|------|---------|-----------|---------|
| Inline (`~~~mix`) | When panel is created | When panel is removed | Subscriptions auto-deregistered |
| Referenced (`script:`) | When panel is created | When panel is removed | Subscriptions auto-deregistered |
| Standalone | When script starts | When script exits | Broker MAY auto-remove orphaned panels |

When a script's process disconnects from the broker, all its event subscriptions are automatically deregistered. For inline and referenced scripts, panel removal triggers script termination, which triggers subscription cleanup — the cascade is automatic.

### 7.7 Script Security

**Local and mesh scripts:** Full ABP access. Scripts can create panels, send commands to any service, read files (via file service), and register for any event.

**Federated panels:** Scripts are FORBIDDEN. The display service MUST strip `~~~mix` code blocks and ignore the `script` header for any panel with `source_peer` set. This prevents remote code execution.

### 7.8 Reusable Components

Mix functions serve as reusable UI components:

```mix
-- lib/components.mx — shared UI patterns

function file_tree($parent_id, $root_path, $widget_id)
  address "ui"

  send "panel" id=$widget_id parent=$parent_id scrollable="true"
  send "template" target=$widget_id body="- ![{icon}](lucide:{icon}) **{name}**"

  $entries = send "file-service" list path=$root_path
  send "data" target=$widget_id body=$entries

  on ui.event from $widget_id action "expand"
    $children = send "file-service" list path=$event.path
    send "data" target=$widget_id action="insert" index=$event.index body=$children
  end

  on ui.event from $widget_id action "collapse"
    send "data" target=$widget_id action="remove" item=$event.item
  end

  return $widget_id
end

function searchable_list($parent_id, $widget_id, $placeholder)
  address "ui"

  $cid = $widget_id .. "-container"
  $sid = $widget_id .. "-search"
  send "panel" id=$cid parent=$parent_id layout="column"
  send "panel" id=$sid parent=$cid +
       body="~~~textinput id=search placeholder=\"" .. $placeholder .. "\"\n~~~"
  send "panel" id=$widget_id parent=$cid

  on ui.event from $sid action "input"
    send "data" target=$widget_id filter=$event.value
  end

  return $widget_id
end
```

Usage:

```mix
#!/usr/bin/env mix
source "lib/components.mx"
address "ui"

send "panel" id="browser" parent="desktop" title="Files" layout="row" +
     width="800" height="600"

-- Compose from reusable components
$tree = file_tree("browser", expand("~/"), "nav")
$list = searchable_list("browser", "files", "Search files...")

on ui.event from $tree action "select"
  if not is_dir($event.path) then
    $content = send "file-service" read path=$event.path
    send "panel" id="preview" parent="browser" body=$content
  end
end
```

---

## 8. Action URI Scheme

Action URIs appear in markdown links: `[Label](uri)`. When the user clicks the link, the display service dispatches the URI.

### 8.1 Schemes

| Scheme | Syntax | Behavior |
|--------|--------|----------|
| ABP command | `command.verb` | Send ABP request to the source process |
| ABP command with param | `command.verb:param` | ABP request with positional parameter |
| ABP command with named params | `command.verb:param?key=val` | ABP request with named parameters |
| Panel navigation | `ui.panel:panel-id` | Focus or create the named panel |
| Launch | `launch:target` | Spawn a process (local/mesh only) |
| External URL | `xdg-open:https://...` | Open in system browser |
| Direct URL | `https://...` or `http://...` | Open in system browser (shorthand) |
| Federated reply | `amp-reply:command` | Compose reply email with ABP command |
| Federated request | `amp-request:command` | WebSocket request if live, else amp-reply |
| Email | `mailto:address` | Compose plain email |

### 8.2 ABP Command Dispatch

When a link with an ABP command URI is clicked:

1. The display service MUST emit a `ui.event` with `action` set to the full URI string.
2. The broker routes the event to subscribed processes.
3. The subscribing process handles the command.

If no process is subscribed for the panel's events, the event is logged and dropped. The display service MUST NOT interpret or dispatch ABP commands directly — all command logic flows through processes. This ensures deterministic behavior regardless of runtime subscription state.

### 8.3 Security Restrictions

| Scheme | Local | Mesh | Federated |
|--------|-------|------|-----------|
| ABP command | allowed | allowed | via `amp-reply` only |
| `launch:` | allowed | allowed | BLOCKED |
| `xdg-open:` | allowed | allowed | confirmation required |
| `amp-reply:` | N/A | N/A | allowed |
| `amp-request:` | N/A | N/A | allowed (if WebSocket live) |

---

## 9. Theme System

### 9.1 Theme Variables

A theme is a flat map of variable names to values. There is no cascade, no inheritance, no scoping. One theme is active at a time. All `var(name)` references across all panels resolve against the active theme.

### 9.2 Standard Variables

Renderers SHOULD support these standard variable names:

| Variable | Semantic | Typical Light | Typical Dark |
|----------|----------|---------------|-------------|
| `primary` | Accent color | `#6366f1` | `#818cf8` |
| `primary-dim` | Dimmed accent | `#4f46e5` | `#6366f1` |
| `background` | Root background | `#ffffff` | `#0f0f1a` |
| `surface` | Panel/card background | `#f8f8f8` | `#1a1a2e` |
| `surface-dim` | Sidebar/secondary surface | `#f0f0f0` | `#12122a` |
| `text` | Primary text | `#1a1a1a` | `#e0e0e0` |
| `text-secondary` | Dimmed text | `#666666` | `#999999` |
| `border` | Default border color | `#e0e0e0` | `#333355` |
| `error` | Error/danger | `#ef4444` | `#ef4444` |
| `warning` | Warning/caution | `#f59e0b` | `#f59e0b` |
| `success` | Success/positive | `#10b981` | `#10b981` |

Additional variables MAY be defined. Unknown variable names resolve to a renderer-chosen default.

### 9.3 Theme Files

Themes are stored as TOML files, trivially convertible to `ui.theme` messages:

```toml
# ~/.config/cosmix/themes/midnight.toml
[theme]
name = "midnight"
primary = "#6366f1"
primary-dim = "#4f46e5"
background = "#0f0f1a"
surface = "#1a1a2e"
surface-dim = "#12122a"
text = "#e0e0e0"
text-secondary = "#999999"
border = "#333355"
error = "#ef4444"
warning = "#f59e0b"
success = "#10b981"
```

### 9.4 Theme Switching

Sending a `ui.theme` message replaces the active theme. All panels re-resolve `var()` references and re-render. This is instant — no reload, no panel recreation.

---

## 10. Panel Lifecycle

### 10.1 State Machine

```
                    ui.panel (new id)
                         |
                         v
     [Created] -----> [Active] <----> [Updated]
                         |                ^
                         |                |
                         |     ui.panel (same id)
                         |     ui.style
                         |     ui.data
                         v
                     [Removed]
                    ui.remove
```

### 10.2 State Transitions

| From | Trigger | To | Effect |
|------|---------|----|--------|
| (none) | `ui.panel` with new ID | Active | Create Wayland surface (if top-level), render markdown, attach script |
| Active | `ui.panel` with same ID | Active (updated) | Replace body, merge headers. Preserve geometry/position if not in update. |
| Active | `ui.style` | Active (updated) | Update style properties. Re-render affected elements. |
| Active | `ui.data` | Active (updated) | Update data widget contents. |
| Active | `ui.remove` | Removed | Destroy surface. Remove children (cascade). Stop attached script. |
| Active | TTL expires | Removed | Same as `ui.remove`. |

### 10.3 Orphan Handling

When a process disconnects from the broker, its panels are NOT automatically removed (the user may still be looking at them). However:

- Event subscriptions for the disconnected process are deregistered.
- Clicking action links in orphaned panels produces no response.
- The broker MAY mark orphaned panels visually (dimmed, badge, etc.).
- The broker SHOULD remove orphaned panels after a configurable timeout (default: 60 seconds).

### 10.4 TTL

The `ttl` header specifies auto-removal in milliseconds from panel creation time. When TTL expires, the panel is removed as if `ui.remove` were received.

By default, updating a panel does NOT reset its TTL. A panel created with `ttl: 5000` that is updated at t=4s still expires at t=5s. To reset TTL on update, include `ttl` in the update message — this restarts the timer from the update time.

Commonly used for notifications:

```
---
command: ui.panel
id: toast-001
layer: notification
position: top-right
ttl: 5000
---
> ![check](lucide:check-circle) **Mail sent** to alice@example.com
```

---

## 11. Composition Model

### 11.1 Panel Hierarchy

Panels form a tree rooted at the implicit `desktop` panel:

```
desktop (implicit root)
├── launcher (parent: desktop)
├── mail-app (parent: desktop)
│   ├── mail-sidebar (parent: mail-app)
│   └── mail-content (parent: mail-app)
│       ├── email-list (parent: mail-content)
│       └── email-view (parent: mail-content)
├── statusbar (parent: desktop)
└── toast-001 (parent: desktop, layer: notification)
```

### 11.2 Top-Level Panels

Panels with `parent: desktop` (or no `parent` header) are top-level. Each creates a Wayland surface (window) on native desktop renderers, or a root `<div>` in WASM renderers.

### 11.3 Child Panels

Panels with `parent: some-panel-id` render inside their parent. The parent's `layout` header determines how children are arranged:

| Layout | Behavior |
|--------|----------|
| `column` | Children stack vertically (default) |
| `row` | Children arrange horizontally |
| `grid` | Children flow into grid cells |
| `stack` | Children overlap (last on top) |

### 11.4 Container Panels and Layout Widgets

Layout widgets (Container, Tabs, SplitPane) from Section 6.2 do NOT nest children inside their markdown code block. Instead, complex layouts are composed from multiple panels using the `parent` header:

```
SplitPane ≠ a code block containing two regions
SplitPane = a container panel with layout:row, children are separate panels
```

This is a deliberate design choice. Markdown is a linear format — it cannot naturally express spatial layout like side-by-side panes. Rather than inventing non-GFM nesting syntax, the protocol uses panel composition: each region is a separate `ui.panel` message with `parent` pointing to the container. The container's `layout` header determines arrangement.

For complex multi-region layouts, `layout: grid` with `grid_template` (Section 3.1.1) allows a single panel to define named regions — reducing panel count for static layouts while keeping each region's content in a separate child panel. This is the recommended approach for application-level layouts (sidebar + toolbar + content + status bar).

A panel with layout headers but no body (or empty body) is a pure container — it provides structure without content:

```
---
command: ui.panel
id: mail-app
parent: desktop
layout: row
width: 100%
height: 100%
---
```

Child panels populate it:

```
---
command: ui.panel
id: mail-sidebar
parent: mail-app
width: 250
---
# Mailboxes
- Inbox (3)
- Sent
- Drafts
```

### 11.5 Layer Ordering

The `layer` header controls z-ordering across the display:

| Layer | Z-order | Purpose |
|-------|---------|---------|
| `background` | Lowest | Wallpaper, desktop widgets |
| `normal` | Default | Application windows |
| `overlay` | Above normal | Floating panels, tooltips |
| `notification` | Highest | Alerts, toasts |

Within a layer, the most recently focused panel renders on top.

### 11.6 Reparenting

A panel can be moved to a different parent by sending `ui.panel` with the same `id` and a new `parent` header. The panel is removed from its old parent and inserted into the new one.

---

## 12. Data-Driven Widgets

### 12.1 Overview

VirtualList and DataTable widgets use a template + data model. The template defines how each data item renders. The data provides the content. This separation enables:

- Virtual scrolling (only template-stamp visible items)
- Incremental updates (add/remove/update individual items)
- Re-templating without re-sending data

### 12.2 Template Syntax

Templates are markdown with `{field}` interpolation placeholders:

```markdown
- ![{icon}](lucide:{icon}) **{name}** ({unread})
```

Given a data item `{"icon": "inbox", "name": "Inbox", "unread": 3}`, this produces:

```markdown
- ![inbox](lucide:inbox) **Inbox** (3)
```

**Rules:**

- `{field}` is replaced with the string value of the field from the data object.
- Missing fields produce an empty string.
- `{{` produces a literal `{` in the output. `}}` produces a literal `}`.
- No conditionals, no loops, no expressions — pure field substitution.
- Templates are set via `ui.template` and persist until replaced.
- If no template is set, the renderer SHOULD display each item's `id` as a list item.

### 12.3 Data Format

Data is sent via `ui.data` as a JSON array of objects. Every object MUST have an `id` field:

```json
[
  {"id": "inbox", "name": "Inbox", "unread": 3, "icon": "inbox"},
  {"id": "sent", "name": "Sent", "unread": 0, "icon": "send"}
]
```

### 12.4 Incremental Updates

| Action | Headers | Body | Effect |
|--------|---------|------|--------|
| `replace` (default) | `target` | JSON array | Replace all data |
| `insert` | `target`, `index` | JSON object | Insert at position |
| `update` | `target`, `item` | JSON object | Replace entire item with matching ID |
| `patch` | `target`, `item` | Partial JSON object or JSON-Patch array | Merge fields into item with matching ID |
| `remove` | `target`, `item` | (empty) | Remove item with matching ID |
| `clear` | `target` | (empty) | Remove all data |

#### 12.4.1 Patch: Partial Updates

The `patch` action updates individual fields on an existing data item without replacing the entire object. This is the preferred update mechanism for live data — it minimizes payload size and avoids race conditions where a full `update` could overwrite concurrent changes to other fields.

**Merge-patch (default):** The body is a partial JSON object. Fields present in the patch are merged into the existing item. Fields absent are preserved. Fields set to `null` are removed.

```
---
command: ui.data
target: email-list
action: patch
item: msg-42
---
{"unread": false, "labels": ["important", "work"]}
```

This updates only `unread` and `labels` on item `msg-42`. All other fields (`from`, `subject`, `date`, etc.) are preserved untouched.

**JSON-Patch (RFC 6902):** For operations that merge-patch cannot express (array manipulation, moves, tests), the body MAY be a JSON-Patch array. Renderers distinguish the two formats by checking whether the body is an array of `{"op":...}` objects or a plain object:

```
---
command: ui.data
target: email-list
action: patch
item: msg-42
---
[
  {"op": "replace", "path": "/unread", "value": false},
  {"op": "add", "path": "/labels/-", "value": "urgent"}
]
```

**When to use each action:**

| Scenario | Action | Why |
|----------|--------|-----|
| Initial load | `replace` | Full dataset, clean slate |
| New item arrives (e.g., new email) | `insert` | Add without touching existing items |
| One field changes (e.g., read status) | `patch` | Minimal payload, no overwrite risk |
| Item fully rewritten (e.g., edited draft) | `update` | Full object replacement is intentional |
| Item deleted | `remove` | Remove by ID |
| Bulk field update across many items | Multiple `patch` in `ui.batch` | Atomic multi-item partial update |

In a typical SPA-like workflow, the process loads data with `replace` once, then uses `patch` for all subsequent mutations. Full `replace` is reserved for navigation events (switching mailboxes, changing directories) where the entire dataset changes.

### 12.5 Tree Data

VirtualList with `indent: true` supports hierarchical data. Items include `depth` (integer) and `expandable` (bool) fields:

```json
[
  {"id": "docs", "name": "Documents", "depth": 0, "expandable": true, "icon": "folder"},
  {"id": "notes", "name": "notes.md", "depth": 1, "expandable": false, "icon": "file-text"},
  {"id": "pics", "name": "Pictures", "depth": 0, "expandable": true, "icon": "folder"}
]
```

Expanding/collapsing emits `expand`/`collapse` events. The process is responsible for inserting/removing child items via `ui.data`.

---

## 13. Transport Tiers and Degradation

### 13.1 Transport Matrix

| Tier | Transport | Protocol | Scripts | Latency |
|------|-----------|----------|---------|---------|
| **Local** | Unix socket | Full | Yes | <1ms |
| **Mesh** | WebSocket over WireGuard | Full | Yes | ~10ms |
| **Federated (SMTP)** | Email + MIME | Markdown only | No | seconds–minutes |
| **Federated (WebSocket)** | Direct WebSocket | Display + events | Sandboxed | ~100ms |

### 13.2 SMTP Wire Format

ABP panels sent via SMTP use the `text/x-amp-panel` MIME content-type. Senders MUST include a `text/plain` multipart alternative:

```
Content-Type: multipart/alternative; boundary="cosmix-boundary"

--cosmix-boundary
Content-Type: text/plain; charset=utf-8

[Plain text rendering of the markdown body]

--cosmix-boundary
Content-Type: text/x-amp-panel; charset=utf-8

---
command: ui.panel
id: remote-status
source_peer: mark@cosmix.mesh
permissions: display
---
# Server Status
| Service | Status |
|---------|--------|
| maild | running |
| noded | running |

[Refresh](amp-reply:status.refresh)

--cosmix-boundary--
```

### 13.3 Degradation Rules

When a renderer does not support a feature, it degrades gracefully:

| Feature | Degradation |
|---------|-------------|
| `~~~mix` code blocks | Stripped (not rendered) |
| Interactive widgets (code blocks) | Rendered as their text content or placeholder |
| Action links | Rendered as plain text with URI visible |
| `var()` references | Renderer default colors |
| VirtualList/DataTable | First N items as plain list/table |
| Icons (`lucide:name`) | Rendered as `[name]` text |
| Themes | Ignored (renderer defaults) |
| SplitPane/Tabs | Contents rendered sequentially |

### 13.4 Conformance and Degradation

A Level 0 renderer receives a full `ui.panel` message with interactive widgets and scripts. It ignores the code block widgets (renders them as code), ignores scripts (strips `~~~mix`), and renders the remaining markdown. The result is readable, static, and useful — not broken.

---

## 14. Conformance Levels

### 14.1 Levels

| Level | Name | Required Capabilities |
|-------|------|-----------------------|
| **0** | Markdown | GFM rendering. `ui.panel` create/update/remove lifecycle. Headings, paragraphs, lists, tables, blockquotes, rules, inline formatting, images. Code blocks as monospace text. Links as clickable text. |
| **1** | Interactive | Level 0 + code block widget recognition (TextInput, TextArea, Button, Checkbox, Toggle, Dropdown, Slider). `ui.event` emission on user interaction. Action link dispatch. Focus management. |
| **2** | Data | Level 1 + VirtualList, DataTable. `ui.template` + `ui.data` commands. Incremental data updates. Virtual scrolling for large datasets. |
| **3** | Scripted | Level 2 + Mix script execution (inline, referenced, standalone). `on`/`off` event handlers. `ui.subscribe`/`ui.unsubscribe` broker commands. |
| **4** | Federated | Level 3 + `text/x-amp-panel` MIME parsing. `amp-reply`/`amp-request` action URIs. Permission prompts per sender. DKIM/SPF/DMARC validation. Visual sandbox for federated panels. |

### 14.2 Renderer Classification

| Renderer | Expected Level |
|----------|---------------|
| Native desktop (winit+wgpu) | Level 4 |
| WASM browser client | Level 3 |
| TUI terminal client | Level 1 |
| Headless/testing | Level 3 (no rendering, full protocol) |
| Non-cosmix email client | Level 0 (reads text/plain fallback) |

### 14.3 Conformance Requirements

A renderer at Level N MUST support ALL capabilities of Levels 0 through N. It MUST gracefully degrade features from higher levels (not crash, not produce garbled output).

---

## 15. Security Model

### 15.1 Trust Domains

| Domain | Transport | Trust Level |
|--------|-----------|-------------|
| **Local** | Unix socket | Full — unrestricted ABP access |
| **Mesh** | WebSocket over WireGuard /24 | Full — same trust as local (single owner) |
| **Federated** | SMTP or direct WebSocket | Sandboxed — restricted capabilities |

A WireGuard /24 network IS the trust boundary. All nodes within it belong to the same person. Mesh membership IS the credential. No per-widget ACLs, no capability tokens within a mesh.

### 15.2 Origin Tracking

Every ABP message SHOULD carry a `from` header identifying the source. For federated messages, the `source_peer` header identifies the originating mesh.

**`source_peer` flow:** The *receiving* broker sets `source_peer` when a message arrives from an external transport (SMTP gateway, federated WebSocket). Local processes MUST NOT set `source_peer` themselves — the broker overwrites any process-supplied value. In SMTP delivery, the sending mesh's gateway composes the MIME message; the receiving mesh's broker parses it and injects `source_peer` based on the validated sender identity (DKIM signature).

### 15.3 Federated Panel Restrictions

| Capability | Local/Mesh | Federated |
|-----------|------------|-----------|
| Display panels | yes | yes (with permission prompt) |
| Execute scripts | yes | **NO** |
| `launch:` actions | yes | **NO** |
| File system access | yes | **NO** |
| Config writes | yes | **NO** |
| `xdg-open:` URLs | yes | confirmation dialog |
| Style own panels | yes | yes |
| Style other panels | yes | **NO** |

### 15.4 Permission Prompts

First-time display of a federated panel triggers a permission prompt:

```
"mark@cosmix.mesh wants to display a panel."
[Allow always] [Allow once] [Block sender]
```

DKIM signature validation is REQUIRED for SMTP-delivered panels. Invalid or missing signatures → panel rejected silently. SPF and DMARC alignment SHOULD be checked.

### 15.5 Visual Sandbox

Federated panels MUST render with a visual indicator of their origin:
- A distinct border, badge, or banner showing the `source_peer` address.
- Federated panels MUST NOT overlay local panels.
- Federated panels MUST NOT impersonate system UI (desktop, statusbar, notifications).

---

## 16. Examples

> **Note:** Examples below show ABP message content without the `---\nEOM\n`
> stream terminator for readability. On the wire, every message ends with
> `---\nEOM\n` per `2026-03-24-01-bus-wire-protocol.md` §5.1.

### 16.1 Minimal Panel — Status Dashboard

A simple dashboard with no script. Pure markdown content + ABP headers.

```
---
command: ui.panel
id: status
parent: desktop
title: System Status
width: 400
height: 300
---
# System Status

| Metric | Value |
|--------|-------|
| CPU | 12% |
| RAM | 4.2 / 16 GB |
| Disk | 120 / 500 GB |
| Uptime | 5d 14h |

> ![check](lucide:check-circle) All services running

---

[Refresh](status.refresh) [Details](ui.panel:status-detail)
```

**Layers used:** Markdown (content), ABP headers (window properties). No script.

### 16.2 Form with Inline Script — Email Compose

A form with interactive widgets and an inline Mix script for handling submission.

````
---
command: ui.panel
id: compose
parent: desktop
title: Compose
width: 500
height: 600
layout: column
collect_values: true
---
# New Message

~~~textinput id=to placeholder="To..."
~~~

~~~textinput id=subject placeholder="Subject"
~~~

~~~textarea id=body rows=15
~~~

[Send](compose.send) [Attach](compose.attach) [Discard](compose.discard)

~~~mix
on ui.event from "compose" action "compose.send"
  send "maild" send to=$event.to subject=$event.subject body=$event.body
  send "ui" remove target="compose"
  emit "ui" panel id="toast" layer="notification" ttl="3000" +
       body="> ![check](lucide:check-circle) **Sent** to " .. $event.to
end

on ui.event from "compose" action "compose.discard"
  send "ui" remove target="compose"
end
~~~
````

**Layers used:** All three. Markdown for labels and structure. Headers for window geometry. Mix for send/discard logic.

### 16.3 Data-Driven List — Mail Client Sidebar

A VirtualList with template and data binding, driven by a standalone Mix script.

```mix
#!/usr/bin/env mix
-- Mail sidebar — standalone script
address "ui"

-- Create the sidebar panel
send "panel" id="mail-sidebar" parent="mail-app" width="250" +
     background="var(surface-dim)" scrollable="true"

-- Set template for mailbox items
send "template" target="mail-sidebar" +
     body="- ![{icon}](lucide:{icon}) **{name}** ({total})"

-- Load and push data
$mailboxes = send "maild" mailbox.list
send "data" target="mail-sidebar" body=$mailboxes

-- Handle selection
on ui.event from "mail-sidebar" action "select"
  $emails = send "maild" email.list mailbox=$event.item limit=50
  send "data" target="email-list" body=$emails
end
```

### 16.4 Desktop Composition — Full Desktop Layout

Multiple processes composing the desktop:

```
Process: cosmix-noded (startup)
├── ui.panel id=launcher parent=desktop position=left width=15%
├── ui.panel id=statusbar parent=desktop position=bottom height=2rem
└── ui.theme name=midnight

Process: mail-script.mx
├── ui.panel id=mail-app parent=desktop layout=row
├── ui.panel id=mail-sidebar parent=mail-app width=250
├── ui.panel id=mail-list parent=mail-app width=auto
└── ui.subscribe source=mail-sidebar

Process: monitor-service
├── ui.panel id=statusbar body="![cpu](lucide:cpu) `12%` | ..."
└── (updates statusbar every 5 seconds)
```

Each process sends its panels independently. The display service composes them. No central coordinator.

### 16.5 Federated Dashboard — SMTP Delivery

An ABP panel sent via email with graceful degradation:

```
From: mark@cosmix.mesh
To: sally@herserver.com
Subject: Nightly Status Report
Content-Type: multipart/alternative; boundary="cosmix"

--cosmix
Content-Type: text/plain; charset=utf-8

Nightly Status Report

Server   | CPU  | RAM      | Disk
---------|------|----------|----------
web-01   | 12%  | 4.2 GB   | 120 GB
db-01    | 45%  | 12.8 GB  | 234 GB
api-01   | 8%   | 2.1 GB   | 89 GB

All services running. Last updated: 2026-04-06 22:00 AEST

--cosmix
Content-Type: text/x-amp-panel; charset=utf-8

---
command: ui.panel
id: nightly-status-mark
source_peer: mark@cosmix.mesh
permissions: display
---
# Nightly Status — 2026-04-06

| Server | CPU | RAM | Disk |
|--------|-----|-----|------|
| web-01 | 12% | 4.2 GB | 120 GB |
| db-01 | 45% | 12.8 GB | 234 GB |
| api-01 | 8% | 2.1 GB | 89 GB |

> ![check](lucide:check-circle) All services running

---

[Acknowledge](amp-reply:status.ack) [Request Details](amp-reply:status.detail)

--cosmix--
```

Sally's cosmix-mail renders the interactive panel. Thunderbird renders the plain text fallback. Both are readable.

---

## Appendix A: Header Reference

Alphabetical listing of all headers recognized by the display protocol.

| Header | Type | Commands | Description |
|--------|------|----------|-------------|
| `action` | string | `ui.data` | Data operation: `replace`, `insert`, `remove`, `update`, `clear` |
| `align` | enum | `ui.panel` | Child alignment: `start`, `center`, `end`, `stretch` |
| `background` | color | `ui.panel`, `ui.style` | Background color or `var(name)` |
| `border_color` | color | `ui.panel`, `ui.style` | Border color |
| `border_radius` | float | `ui.panel`, `ui.style` | Corner rounding (rem) |
| `border_width` | float | `ui.panel`, `ui.style` | Border thickness (px) |
| `collect_values` | bool | `ui.panel` | Include all widget values in event payloads |
| `command` | string | all | Command identifier |
| `decorations` | list | `ui.panel` | CSD decorations (comma-separated) |
| `font_size` | float | `ui.panel`, `ui.style` | Base font size (rem) |
| `from` | string | all | Source address |
| `gap` | float | `ui.panel` | Space between children (rem) |
| `grid_area` | string | `ui.panel` | Named grid region this child occupies |
| `grid_rows` | string | `ui.panel` | Grid row track definitions |
| `grid_template` | string | `ui.panel` | Grid column track definitions |
| `height` | size | `ui.panel` | Panel height |
| `id` | string | `ui.panel` | Panel identifier |
| `index` | integer | `ui.data` | Row index for insert |
| `item` | string | `ui.data` | Item ID for update/remove |
| `layer` | enum | `ui.panel` | Z-layer: `background`, `normal`, `overlay`, `notification` |
| `layout` | enum | `ui.panel` | Child layout: `column`, `row`, `grid`, `stack` |
| `msg_id` | string | all | Message identity for request/response correlation (distinct from `id`) |
| `name` | string | `ui.theme` | Theme name |
| `opacity` | float | `ui.panel`, `ui.style` | Panel opacity (0.0–1.0) |
| `overflow` | enum | `ui.panel` | Overflow behavior: `clip`, `scroll`, `visible` |
| `padding` | float/quad | `ui.panel` | Inner padding (rem) |
| `parent` | string | `ui.panel` | Parent panel ID |
| `permissions` | string | `ui.panel` | `display` for federated panels |
| `position` | enum/coord | `ui.panel` | Panel position |
| `script` | string | `ui.panel` | Mix script path/URI |
| `scrollable` | bool | `ui.panel` | Enable scroll container |
| `source` | string | `ui.event` | Panel ID where event originated |
| `source_peer` | string | `ui.panel` | Originating mesh peer (set by transport) |
| `sticky` | bool | `ui.panel` | Survives workspace switches |
| `target` | string | `ui.style`, `ui.remove`, `ui.data`, `ui.template` | Target panel/widget ID |
| `text_color` | color | `ui.panel`, `ui.style` | Default text color |
| `title` | string | `ui.panel` | Window title |
| `ttl` | integer | `ui.panel` | Auto-remove timeout (ms) |
| `width` | size | `ui.panel` | Panel width |

## Appendix B: Widget Quick Reference

| Widget | Aliases | Category | Key Properties | Key Events |
|--------|---------|----------|---------------|------------|
| Label | `label` | Display | `text_color`, `font_size`, `align` | — |
| Icon | `icon` | Display | `name`, `size`, `color` | — |
| Image | `image`, `img` | Display | `src`, `alt`, `fit` | — |
| Markdown | `markdown`, `md` | Display | — | Action clicks |
| Container | `container`, `div` | Layout | `layout`, `gap`, `padding` | — |
| ScrollContainer | `scroll` | Layout | `direction`, `max_height` | `scroll` |
| Tabs | `tabs` | Layout | `tabs`, `active`, `closable` | `select`, `close` |
| SplitPane | `splitpane`, `split` | Layout | `direction`, `ratio`, `min_size` | `resize` |
| Button | `button`, `btn` | Input | `label`, `variant`, `disabled` | `click` |
| TextInput | `textinput`, `input` | Input | `value`, `placeholder`, `password` | `input`, `submit`, `focus`, `blur` |
| TextArea | `textarea` | Input | `value`, `rows`, `placeholder` | `input`, `focus`, `blur` |
| Dropdown | `dropdown`, `select` | Input | `options`, `value` | `change` |
| Checkbox | `checkbox` | Input | `label`, `checked` | `change` |
| Toggle | `toggle`, `switch` | Input | `label`, `checked` | `change` |
| Slider | `slider`, `range` | Input | `min`, `max`, `value`, `step` | `input`, `change` |
| RadioGroup | `radiogroup`, `radio` | Input | `options`, `value`, `direction` | `change` |
| Progress | `progress` | Input | `value`, `max`, `label`, `variant` | — |
| VirtualList | `virtuallist`, `vlist` | Data | `item_height`, `indent`, `selectable` | `select`, `activate`, `expand`, `collapse` |
| DataTable | `datatable`, `table` | Data | `columns`, `sortable`, `sort_column` | `select`, `activate`, `sort` |
| MenuBar | `menubar` | Chrome | `menus`, `caption_buttons` | `invoke` |
| ContextMenu | `contextmenu` | Chrome | `items`, `position` | `invoke`, `close` |
| Dialog | `dialog`, `modal` | Chrome | `title`, `closable`, `backdrop` | `close` |

## Appendix C: Event Payload Reference

All `ui.event` messages include these base fields:

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `action` | string | Yes | Event type |
| `widget` | string | When widget-level | Widget ID |

Additional fields by event action:

| Action | Fields | Emitted by |
|--------|--------|-----------|
| `click` | — | Button, action links |
| `select` | `item`, `index`, `value` | VirtualList, DataTable, Tabs, list items |
| `activate` | `item`, `index` | VirtualList, DataTable (double-click) |
| `input` | `value` | TextInput, TextArea, Slider (continuous) |
| `change` | `value` or `checked` | Dropdown, Checkbox, Toggle, Slider (final) |
| `submit` | `value` | TextInput (Enter key) |
| `focus` | — | TextInput, TextArea |
| `blur` | `value` | TextInput, TextArea |
| `expand` | `item` | VirtualList (tree) |
| `collapse` | `item` | VirtualList (tree) |
| `sort` | `column`, `order` | DataTable |
| `resize` | `ratio` | SplitPane |
| `scroll` | `offset`, `max` or `visible` | ScrollContainer, VirtualList |
| `invoke` | `item` | MenuBar, ContextMenu |
| `close` | — | Tabs (tab close), ContextMenu, Dialog |
| `toggle` | `checked` | Task list checkboxes in markdown |
| `change` (radio) | `value` | RadioGroup |

## Appendix D: Theme Variable Reference

See Section 9.2 for the standard variable set. Variables are arbitrary strings. This appendix lists the complete recommended set:

**Core palette:**
`primary`, `primary-dim`, `background`, `surface`, `surface-dim`

**Text:**
`text`, `text-secondary`

**Borders:**
`border`

**Semantic:**
`error`, `warning`, `success`

**Extended (optional):**
`info`, `muted`, `accent`, `highlight`, `selection`, `focus-ring`

All variables resolve via `var(name)` in style property values. Resolution is a single flat lookup — no fallback chains, no cascade, no computed values.
