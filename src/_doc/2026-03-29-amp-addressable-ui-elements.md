# AMP-Addressable UI Elements — Architecture

## Context

Cosmix apps (Dioxus desktop via WebKitGTK) expose backend commands via AMP today. This extends AMP to address **interactive UI elements** (buttons, toggles, inputs, menu items) so Lua scripts, MCP tools, and remote nodes can drive the UI programmatically.

## Chosen Approach: Explicit Registration (Option A)

Apps opt in by using wrapper components (`AmpButton`, `AmpToggle`, `AmpInput`, etc.) that auto-register into a per-app `UiRegistry` on mount and deregister on unmount. No framework introspection, no accessibility-tree hacking.

## Core Components

### 1. `UiRegistry` (in `cosmix-lib-ui`)

```rust
pub struct UiRegistry {
    elements: HashMap<String, UiElement>,  // dot-separated hierarchical IDs
}

pub struct UiElement {
    pub id: String,           // semantic ID e.g. "file.save", NOT "toolbar-btn-3"
    pub kind: ElementKind,    // Button, MenuItem, Toggle, Input, ...
    pub label: String,        // human-readable, for discovery
    pub state: Signal<ElementState>,  // current value/disabled/checked etc.
}

pub enum ElementState {
    Button { disabled: bool },
    Toggle { checked: bool, disabled: bool },
    Input { value: String, disabled: bool },
    MenuItem { disabled: bool, checked: Option<bool> },
}
```

**Key decision:** Use `Signal<Option<UiCommand>>` for incoming commands, NOT `Sender<UiCommand>` channels. Stays within Dioxus reactivity; avoids spawning per-element async tasks.

**Registry access:** Provided via `use_context_provider` at the app root, retrieved via `use_context` in wrapper components. No prop drilling.

### 2. Wrapper Components

Drop-in replacements that handle registration lifecycle:

```rust
#[component]
fn AmpButton(id: String, label: String, on_click: EventHandler<()>) -> Element { ... }
```

On mount → register in `UiRegistry`. On unmount → deregister. Watches its signal for incoming `UiCommand`. Renders standard button + optional `.cmx-amp-highlight` animation class.

**Critical:** When a `UiCommand::Invoke` or `UiCommand::SetValue` arrives, the wrapper MUST call the same `on_click`/`on_change` handler that a real user interaction would. Do NOT just flip the visual — the app's `use_signal()` state must update through the normal callback path. Otherwise UI and state diverge.

### 3. Standard AMP Commands (in `use_hub_handler`, free for all apps)

```
ui.invoke    { id: "file.save" }                    → activate element
ui.highlight { id: "file.save", ms: 500 }           → visual pulse
ui.focus     { id: "search-input" }                 → focus element
ui.set       { id: "dark-toggle", value: "true" }   → set value (triggers callback)
ui.list      { prefix: "file." }                    → discover elements (IDs, kinds, labels, current state)
```

Handled generically before falling through to app-specific `dispatch_command()`.

### 4. Visual Feedback (CSS)

```css
.cmx-amp-highlight {
    animation: amp-pulse 400ms ease-out;
}
@keyframes amp-pulse {
    0%   { box-shadow: 0 0 0 2px var(--accent); }
    100% { box-shadow: 0 0 0 0 transparent; }
}
```

## ID Convention

- Dot-separated semantic hierarchy: `file.save`, `view.zoom-in`, `edit.find`
- Treat as API contract — stable across refactors
- Never positional (`toolbar-btn-3`) or structural (`panel-left-save`)
- `ui.list` supports prefix filtering for scalable discovery

## Security Model — Trust Domains

A single WireGuard /24 network is a **single trust domain**. All nodes belong to the person who controls the primary node. Mesh membership IS the credential — the ARexx model extended from "one machine" to "the /24."

**Intra-mesh (same /24):** Full `ui.*` access everywhere. No per-widget ACLs, no capability tokens, no auth headers. The hub just routes. If the packet arrived on the WireGuard interface, it's trusted.

**Inter-mesh (bridge between /24s):** This is where gating lives. Bridge node is the policy enforcement point. Only explicitly shared endpoints cross the bridge. All `ui.*` commands from foreign meshes rejected by default.

**Not needed now:** per-widget ACLs, per-app remote-UI policy, capability tokens, auth on AMP messages.

**Needed from day one (zero cost, prevents retrofit):** Every AMP message carries a `from:` origin field. No one checks it today. The bridge checks it when inter-mesh arrives.

```yaml
---
command: ui.invoke
to: save-btn.edit.cachyos.amp
from: phone.amp
---
```

## Implementation Order

1. **Menu only** — `MenuCommand` channel on `MenuBar`, proves the pattern
2. **`AmpButton` wrapper** — single component, validates registry lifecycle
3. **`ui.list` / `ui.invoke` / `ui.highlight`** — standard AMP commands in `use_hub_handler`
4. **`ui.list` returns state** — scripts can observe, not just invoke
5. **Expand** — `AmpToggle`, `AmpInput`, panels as needed

## Mesh-Wide Widget Addressing (Future Architecture)

The ARexx analogy extends naturally across the WireGuard mesh. AMP's existing `port.app.node.amp` addressing maps cleanly with a widget leaf layer:

```
save-btn.edit.cachyos.amp          ← specific widget
file-menu.edit.cachyos.amp         ← menu component
edit.cachyos.amp                   ← app level (existing)
cachyos.amp                        ← node level (existing)
```

### AMP Header Layers

AMP headers operate at two distinct levels:

- **Transport:** `id` — manages the WebSocket pipe. Multiplexes request/response pairs on a single connection. Like TCP sequence numbers.
- **Application:** `to` / `from` — identifies the endpoints. Routes messages between apps, widgets, and nodes. Like IP addresses.

The hub uses `id` to route responses back to the correct caller on a shared connection. It uses `to` to route requests to the correct service. A bridge would filter on `from` to enforce mesh boundaries. These are orthogonal — `id` is per-connection, `to`/`from` are per-message.

### Address Segment Convention

Fixed segment count keeps DNS-style resolution deterministic:

```
<widget-id>.<app>.<node>.amp       ← always 4 segments for widget
<app>.<node>.amp                   ← always 3 segments for app
<node>.amp                         ← always 2 segments for node
```

No variable-depth component nesting in the address. Hierarchy within widgets uses dashes in the widget ID: `file-menu-save-as.edit.cachyos.amp`. Address segments stay fixed; the widget ID is a flat semantic string.

### What This Enables

A Lua script on a phone node can drive UI on a workstation without VNC/RDP/screen coordinates:

```lua
-- Check dark mode state in the editor on the workstation
local state = amp.get("dark-toggle.edit.cachyos.amp")
-- Toggle it remotely
amp.send("dark-toggle.edit.cachyos.amp", "ui.set", { value = "true" })
-- List all widgets in the mail client on another node
local widgets = amp.send("mail.netserva.amp", "ui.list")
```

### Latency and Batching

Local ARexx was sub-millisecond. Cross-WireGuard is 10–200ms per hop. Chatty widget scripts need batch operations:

```yaml
---
command: ui.batch
to: edit.cachyos.amp
---
- invoke: file-open
- set: path-input, value: "/tmp/doc.md"
- invoke: open-btn
```

One round trip, three actions. Design for batching early.

### Build Sequence

Each layer delivers value independently:

1. **Local `ui.*` commands** — widget ID is just a string within the app
2. **Local cross-app** — `save-btn.edit` works within the same node's hub, no DNS
3. **Mesh-wide** — `save-btn.edit.cachyos.amp` resolves through WireGuard DNS, hub routes it

Layer 3 is layer 2 with the node hub doing a DNS lookup first. If layer 2 has opaque string IDs, standard AMP messages, and security scoping — layer 3 comes almost free, it's just routing.

## Gotchas to Watch

- **State sync:** Incoming `UiCommand` must go through the same callback as user clicks. Never update visual without updating app state
- **Signal vs Channel:** Use Dioxus `Signal<Option<UiCommand>>`, not async channels. Channels fight the reactive model
- **ID stability:** Semantic IDs are an API surface. Renaming = breaking change. Document them
- **Partial coverage:** If only some elements are addressable, scripts break silently on the rest. Commit per-app or keep to action-level commands
- **`ui.list` must include state:** Without current values, scripts can invoke but can't observe — half-blind automation
- **Performance:** Registry updates on every render. Acceptable now; if it becomes a bottleneck, switch to dirty-flag updates
- **`from:` field from day one:** Stamp every AMP message with origin node. Zero cost now, prevents painful retrofit when inter-mesh bridging arrives
- **Batching:** `ui.batch` exists. Use it for cross-node scripts — chaining sequential round trips feels sluggish
- **Registry in render body, not effects:** `use_effect` captures props by value and doesn't re-run when props change. Register directly in the component body so state is always current (learned the hard way with AmpToggle)
