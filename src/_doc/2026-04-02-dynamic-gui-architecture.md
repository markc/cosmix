# Dynamic GUI Architecture — ARexx++ 2.0

> Every cosmix GUI/WASM app is a living, scriptable, remotely-controllable surface.
> Colors, dimensions, components, menus, and layouts can all be changed at runtime
> from any source: a central daemon, a Mix script, a remote mesh node, or direct
> user interaction. This is the Amiga ARexx vision rebuilt for 2026 — with reactive
> signals, cross-machine mesh networking, and WASM hot-loading.

## The Parallel

| System | IPC | Scope | Scripting | UI Reactivity |
|--------|-----|-------|-----------|---------------|
| **Amiga ARexx** | ARexx ports | Local machine | ARexx REPL/scripts | Polling, manual refresh |
| **KDE/Plasma** | D-Bus | Local machine | D-Bus CLI / KDE scripting | Real-time via Qt signals |
| **cosmix AMP** | AMP over Unix socket + WebSocket | **Cross-mesh** (WireGuard) | **Mix shell/REPL** | **Real-time via Dioxus signals + CSS vars** |

cosmix goes further than both predecessors:
- ARexx was local-only; AMP spans the WireGuard mesh transparently
- D-Bus requires process co-location; AMP works across nodes and in WASM browsers
- Neither had a reactive UI framework; Dioxus signals propagate changes instantly
- Neither could hot-swap compiled UI components; WASM can

## Architecture Layers

### Layer 1: CSS Custom Properties (Styling)

Every visual property is a CSS custom property injected by `use_theme_css()`:

```
Colors:      --bg-primary, --fg-primary, --accent, --border, --danger, ...
Dimensions:  --menubar-height, --sidebar-width, --padding-sm, --padding-md, ...
Typography:  --font-size, --font-size-sm, --font-size-lg, --font-sans, --font-mono
Spacing:     --radius-sm, --radius-md, --radius-lg
Animation:   --duration-fast, --duration-base
```

**How it works:**
1. `generate_css()` in `theme.rs` outputs `:root { --name: value; ... }`
2. `use_theme_css()` injects this via `document::eval()` into a `<style>` element
3. All components reference `var(--name, fallback)` — instant reflow on change
4. `THEME` signal change → `use_effect` re-runs → CSS re-injected → all UI updates

**Remote control path:**
```
Mix script or remote node
  → AMP "config.set" { "global.menubar_height_rem": 2.0 }
  → cosmix-confd saves to settings.toml
  → cosmix-confd broadcasts "config.changed" to all watchers
  → Each app's use_theme_hub_watch() calls reload_theme()
  → THEME signal updates → use_theme_css() re-runs
  → New CSS injected → browser reflows all affected elements
```

**Latency:** Sub-millisecond for the CSS reflow. The AMP round-trip is the bottleneck (~1-5ms local, ~10-50ms mesh).

### Layer 2: Component Registry (UI Elements)

Every significant UI element is registered and addressable via AMP:

```rust
// Already implemented:
UI_REGISTRY   — global registry of named UI elements
SLOT_REGISTRY — dynamic menu slot injection
MENU_DEF      — app's menu definition, queryable via AMP
MENU_CMD      — write to this signal to remote-control menus
UI_CMD        — write to this signal to remote-control UI elements
```

**AMP commands (already implemented):**
```
menu.list                          → list all menu items
menu.invoke  { id: "file.save" }   → trigger a menu action
menu.highlight { id: "edit.undo" } → visually pulse a menu item
ui.list      { prefix: "sidebar" } → list registered UI elements
ui.get       { id: "editor.mode" } → read element state
ui.set       { id: "font-size", value: "18" } → set element state
ui.invoke    { id: "toolbar.bold" } → trigger an element action
ui.batch     [...]                  → multiple actions in one round-trip
```

**Future commands (designed, not yet implemented):**
```
ui.inject    { target: "panel-1", component: "calendar" }  → insert component
ui.remove    { id: "sidebar.weather" }                      → remove component
ui.replace   { id: "panel-1", component: "new-panel" }     → swap component
ui.move      { id: "sidebar", position: "right" }          → reposition
slot.inject  { name: "tools", items: [...] }                → add menu items
slot.clear   { name: "tools" }                              → remove injected items
```

### Layer 3: AMP Control Protocol

AMP (AppMesh Protocol) is the universal IPC. Every cosmix app registers on the hub and can be addressed by name:

```
---
to: preview
command: style.set
---
{"--menubar-height": "2rem", "--bg-primary": "#1a1a2e"}
```

**Style control commands (to be implemented):**
```
style.set    { "--name": "value", ... }   → override specific CSS vars
style.reset  { vars: ["--name", ...] }    → reset to defaults
style.theme  { preset: "ocean" }          → apply a named preset
style.export                               → dump current CSS vars as JSON
style.import { vars: {...} }               → bulk import CSS vars
```

These commands would be handled by `use_theme_css()` or a dedicated style handler in each app. The CSS var injection mechanism is already in place — we just need to extend the AMP command vocabulary.

### Layer 4: Mix Scripting

Mix (`~/.mix/`) is the ARexx of cosmix — a pure-Rust scripting language with native AMP IPC via `send`/`address`/`emit` keywords.

**Styling from Mix:**
```rexx
/* Change menubar height across all apps */
address config "config.set" '{"global.menubar_height_rem": 2.0}'

/* Direct style override on a specific app */
send preview "style.set" '{"--menubar-height": "2rem"}'

/* Apply a theme preset to all apps */
address config "style.theme" '{"preset": "crimson"}'

/* Read current theme from an app */
result = send preview "style.export"
say result
```

**Component control from Mix:**
```rexx
/* Inject a new menu into the file manager */
send files "slot.inject" '{"name": "tools", "items": [
  {"id": "tool.compress", "label": "Compress", "action": {"type": "amp", "to": "scripts", "command": "run.compress"}}
]}'

/* Swap the shell's main panel */
send shell "ui.replace" '{"id": "panel-main", "component": "calendar"}'

/* Automate a multi-app workflow */
send editor "file.open" '{"path": "/etc/hosts"}'
send editor "ui.set" '{"id": "font-size", "value": "20"}'
send editor "menu.invoke" '{"id": "view.line-numbers"}'
```

**Scripted theme animation:**
```rexx
/* Smooth hue rotation across all apps */
do h = 0 to 360 by 10
  address config "style.theme" '{"hue":' h '}'
  call sleep 0.1
end
```

### Layer 5: Mesh Propagation

AMP messages route transparently across the WireGuard mesh. A Mix script on node `mko` can control apps on node `cachyos`:

```rexx
/* From mko, change the theme on cachyos */
address cachyos.config "style.theme" '{"preset": "forest"}'

/* From cachyos, open a file in the editor on mko */
address mko.editor "file.open" '{"path": "/var/log/syslog"}'
```

The mesh bridge handles routing — the script doesn't need to know network topology.

## Implementation Roadmap

### Already Done (2026-04-02)
- [x] CSS custom properties for colors (theme.rs `generate_css()`)
- [x] `use_theme_css()` injection via `document::eval()`
- [x] `data-theme` attribute for dx-components dark/light toggle
- [x] `THEME` global signal with reactive updates
- [x] `config.changed` AMP notification → `reload_theme()`
- [x] `UI_REGISTRY` and `SLOT_REGISTRY` for element/menu registration
- [x] AMP commands: `menu.list/invoke/highlight`, `ui.list/get/set/invoke/batch`
- [x] Mix `send`/`address` keywords for AMP IPC

### Phase Next: Dimension Variables
- [ ] Add dimension CSS vars to `generate_css()`: `--menubar-height`, `--padding-sm/md/lg`, `--sidebar-width`
- [ ] Add corresponding fields to `GlobalSettings` in cosmix-lib-config
- [ ] Update MenuBar CSS to use `var(--menubar-height, 1.75rem)`
- [ ] Update other components to use dimension vars where configurable
- [ ] Verify live update: change setting → all apps reflow

### Phase Future: Dynamic Components
- [ ] `style.set` / `style.reset` AMP commands (per-app CSS var override)
- [ ] `ui.inject` / `ui.remove` / `ui.replace` AMP commands
- [ ] WASM component hot-loading (load compiled components at runtime)
- [ ] Mix helper functions: `theme()`, `inject()`, `swap()`
- [ ] Cross-mesh style sync (change on one node → propagates to all)

## Design Principles

1. **Every visual property is a CSS custom property** — `var(--name, fallback)` everywhere. No magic numbers that can't be changed at runtime.

2. **Every UI element is AMP-addressable** — register with `UI_REGISTRY`, respond to `ui.*` commands. If it's on screen, a script can find it and control it.

3. **Config is the single source of truth** — `settings.toml` → cosmix-confd → AMP broadcast. Apps are consumers, not owners, of their styling.

4. **Mix scripts are first-class controllers** — anything the GUI can do, a Mix script can do via AMP. The REPL is a live debugging tool for UI development.

5. **Mesh-transparent** — the same AMP commands work locally and across nodes. Scripts don't need to know where apps are running.

6. **Dioxus signals are the reactive glue** — AMP message → signal write → `use_effect` → DOM update. No polling, no manual refresh. Sub-millisecond propagation.

7. **rem everywhere, px for hairlines** — all configurable dimensions in `rem` (scales with font size). Only `1px`/`2px`/`3px` for borders and hairlines below `0.25rem`.

8. **Fallbacks always** — `var(--name, sensible-default)` means apps work without any theme injection. The defaults are the Tailwind gray/blue palette. Config override is an enhancement, not a requirement.

## The ARexx++ 2.0 Vision

On the Amiga, ARexx was the glue that made the platform magical. Any application could be scripted, any workflow automated, any UI controlled from a script. But ARexx was limited:
- Text-only commands (no structured data)
- Local machine only (no networking)
- No reactive UI (scripts had to poll for state)
- No component injection (fixed UI at compile time)

cosmix AMP + Mix + Dioxus signals removes all those limitations:
- **Structured JSON** arguments with typed responses
- **Cross-mesh networking** via WireGuard — control apps across machines
- **Reactive signals** — changes propagate instantly, no polling
- **Dynamic components** — inject, swap, remove UI elements at runtime
- **WASM hot-loading** — load new compiled components without restart
- **CSS custom properties** — change any visual property live, across all apps

This is what "sovereign computing" means at the UI layer: the user (or their scripts) have complete, fine-grained, real-time control over every aspect of every application's interface, locally or across the mesh.
