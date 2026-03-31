# AMP Standard Command Vocabulary

Every cosmix app exposes AMP commands via the hub. This document defines the
canonical command patterns so scripts can address any app uniformly.

## Naming Convention

```
<service>.<verb>[-<noun>]
```

- **service** — the hub registration name (`edit`, `view`, `mon`, `files`)
- **verb** — what to do (`get`, `set`, `open`, `list`, `close`)
- **noun** — optional target within the app (`path`, `content`, `status`)

Hyphen-separated for multi-word nouns: `get-content`, not `getContent`.

## Universal Commands (every app SHOULD support)

These are handled automatically by `use_hub_handler` in cosmix-lib-ui:

| Command | Args | Returns | Notes |
|---------|------|---------|-------|
| `config.changed` | `{}` | `{"status":"ok"}` | Theme reload (auto-handled) |
| `menu.list` | `{}` | `[{id, label, shortcut, enabled, menu}]` | All menu items |
| `menu.invoke` | `{"id": "..."}` | `{"status":"ok"}` | Simulate menu click |
| `menu.highlight` | `{"id": "...", "ms": N}` | `{"status":"ok"}` | Visual pulse |
| `menu.close` | `{}` | `{"status":"ok"}` | Close open dropdown |
| `ui.list` | `{"prefix": "..."}` | `[{id, kind, label, state...}]` | All registered widgets |
| `ui.get` | `{"id": "..."}` or `{"ids": [...]}` | `[{id, kind, state...}]` | Read widget state |
| `ui.invoke` | `{"id": "..."}` | `{"status":"ok"}` | Click/toggle a widget |
| `ui.highlight` | `{"id": "...", "ms": N}` | `{"status":"ok"}` | Visual pulse |
| `ui.set` | `{"id": "...", "value": "..."}` | `{"status":"ok"}` | Set widget value |
| `ui.batch` | `[{command, id, ...}]` | `[{command, rc, ...}]` | Multiple actions |

## App-Level Standard Verbs

Each app defines its own commands under its service prefix. The following
verb patterns are recommended for consistency:

### Content apps (edit, view, files)

| Verb | Purpose | Example |
|------|---------|---------|
| `open` | Load/display a resource | `edit.open {"path": "..."}` |
| `close` | Close the current resource | `edit.close` |
| `get-path` | Return current file path | `edit.get-path` → `{"path": "..."}` |
| `get-content` | Return current content | `edit.get-content` → `{"content": "..."}` |
| `set-content` | Replace current content | `edit.set-content {"content": "..."}` |

### Service apps (mon, dns, backup, wg)

| Verb | Purpose | Example |
|------|---------|---------|
| `status` | Return current state/health | `mon.status` → `{cpu, mem, ...}` |
| `list` | Enumerate items | `dns.list` → `[{zone, records}]` |
| `get` | Get specific item | `dns.get {"zone": "..."}` |
| `set` | Update specific item | `dns.set {"zone": "...", ...}` |
| `refresh` | Force data reload | `mon.refresh` |

## Current App Commands

### cosmix-edit (`edit`)

| Command | Args | Returns |
|---------|------|---------|
| `edit.open` | `{"path": "...", "line": N}` | `{"opened": "..."}` |
| `edit.goto` | `{"line": N}` | `{"line": N}` |
| `edit.compose` | `{"content": "...", "path": "..."}` | `{"composing": true}` |
| `edit.get-content` | `{}` | `{"content": "..."}` |
| `edit.get-path` | `{}` | `{"path": "..."}` |
| `edit.get` | `{}` | `{"status": "ok"}` |

Widgets: `edit.line-numbers` (toggle), `edit.path` (input, read-only)

### cosmix-view (`view`)

| Command | Args | Returns |
|---------|------|---------|
| `view.open` | `{"path": "..."}` | `{"opened": "..."}` |
| `view.show-markdown` | `{"content": "..."}` | `{"status":"ok"}` |
| `view.get-path` | `{}` | `{"path": "..."}` |

Widgets: `view.word-wrap` (toggle), `view.path` (input, read-only), `file.open` (button)

### cosmix-mon (`mon`)

| Command | Args | Returns |
|---------|------|---------|
| `mon.status` | `{}` | `{cpu, mem, disk, ...}` |
| `mon.processes` | `{}` | `[{pid, name, cpu, mem}]` |

## RC Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 5 | Warning (partial success) |
| 10 | Error (bad args, not found) |
| 20 | Failure (internal error, crash) |

## Discovery Flow for Scripts

A script targeting an unknown app should:

1. `hub.list` → get registered services
2. `menu.list` → discover menu items and their IDs
3. `ui.list` → discover interactive widgets and their current state
4. `ui.get` → read specific widget values before acting
5. Then invoke/set as needed

This is the ARexx pattern: discover first, script second. Never hardcode IDs
without verifying they exist via `ui.list`.
