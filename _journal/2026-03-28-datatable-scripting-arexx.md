# 2026-03-28 (session 3) — DataTable component, cosmix-mon redesign, ARexx-style scripting POC

## DataTable component (cosmix-lib-ui)

New `DataTable` component in `crates/cosmix-lib-ui/src/components/data_table.rs`:

- `DataColumn` struct: key, label, width, sortable, optional `fn` formatter
- `SortDir` enum (None/Asc/Desc) with click-to-cycle on column headers
- Client-side JSON value sorting (numbers, strings, bools, nulls)
- Row selection with accent highlight + alternating row colors
- Optional pagination with prev/next footer
- Data as `Vec<serde_json::Value>` — avoids generics, apps already work with JSON from hub
- Uses existing OKLCH CSS custom properties throughout

Also made `serde_json` an unconditional dependency of cosmix-lib-ui (was behind `hub` feature).

## cosmix-mon redesign — master-detail node list

Rewrote cosmix-mon with a master-detail layout:

- **Master view (startup):** DataTable of all mesh nodes with CPU, Mem, Uptime, Load columns. Calls `hub.peers` to discover nodes, fetches `mon.status` from each. Local node shows "(local)" suffix. Offline nodes shown with "offline" status.
- **Detail view (click a node):** Back button, stat cards (CPU/Memory/Swap), Disks/Processes tab bar with DataTables. Process list uses the existing `mon.processes` daemon command (previously unused by any UI).
- Refreshes every 10 seconds in both views.

## ARexx-style inter-app scripting — the breakthrough

### New crate: cosmix-lib-script

Created `crates/cosmix-lib-script/` — the ARexx engine for cosmix. 7 source files:

- **types.rs** — `ScriptDef`, `ScriptStep`, `ScriptMeta`, `ScriptContext`, `ScriptResult`
- **discovery.rs** — scans `~/.config/cosmix/scripts/{global,service}/` for TOML script definitions
- **variables.rs** — `$VAR` substitution in JSON templates with proper escaping (uses `serde_json::to_string` for correctness)
- **executor.rs** — sequential step execution via `HubClient.call()`, with result storage between steps
- **menu.rs** — generates dynamic `MenuItem::Submenu("User", ...)` from discovered scripts, parses shortcut strings
- **lib.rs** — public API: `user_menu(service)`, `handle_script_action(id, service, hub, vars)`, `discover_scripts(service)`, `execute(script, ctx, hub)`

Script format (TOML):
```toml
[script]
name = "Live Markdown Preview"
[[steps]]
to = "edit"
command = "edit.get-content"
store = "content"
[[steps]]
to = "view"
command = "view.show-markdown"
args = '{"content": "$content"}'
```

### cosmix-view gains inbound commands

cosmix-view previously had NO inbound command handlers (only outbound calls). Added:
- `view.open` — open a file by path
- `view.show-markdown` — render markdown content directly (key for scripting)
- `view.get-path` — return current file path
- `use_hub_handler(hub_client, "view", dispatch_command)` — was entirely missing
- `VIEW_REQUEST` global signal + polling loop (same pattern as cosmix-edit)

### cosmix-edit extended

- `edit.get-content` — returns actual editor content (was stub returning `{"status":"ok"}`)
- `edit.get-path` — returns current file path
- Global signals `EDITOR_CONTENT` / `EDITOR_PATH` keep hub handler in sync with UI state
- Uses `peek()` not `read()` for signal access from hub handler thread (non-reactive context)

### Both apps have dynamic User menu

- Populated from `~/.config/cosmix/scripts/{edit,view}/*.toml` at startup
- "Reload Scripts" and "Open Scripts Folder" utility items
- Script actions execute via AMP hub with `$CURRENT_FILE` and `$content` variable substitution

### Three working example scripts

1. `edit/preview-in-viewer.toml` — opens current file in viewer (Ctrl+Shift+V)
2. `edit/markdown-preview.toml` — gets editor content, renders as markdown in viewer (2-step multi-app script!)
3. `view/edit-this-file.toml` — opens current file in editor

### Critical bug fix: hub client self-call

**Bug:** When an app called itself through the hub (e.g., edit running a script that calls `edit.get-content`), the incoming **request** had the same `id` as the pending outbound call. The reader task in `HubClient` matched the ID without checking `type: "response"` and incorrectly resolved the request as its own response (with null body).

**Fix:** Added `msg.get("type").is_some_and(|t| t == "response")` check before matching pending IDs in `crates/cosmix-lib-client/src/native.rs`. This matches the hub's own response routing logic. Self-calls are fundamental to the ARexx pattern (apps querying their own state via scripts).

## Wayland always-on-top research

Documented in `_doc/2026-03-28-wayland-always-on-top.md`:

- `tao::Window::set_always_on_top()` is a no-op on Wayland (by design)
- `wlr-layer-shell` is the correct Wayland-native approach — COSMIC supports it
- tao 0.34 has `WindowExtUnix::new_from_gtk_window()` escape hatch for layer-shell integration
- Forking cosmic-comp rejected — use standard Wayland protocols + upstream contributions instead
- Short-term: `GDK_BACKEND=x11` for XWayland fallback
- Medium-term: layer-shell integration in `launch_desktop()`

## Workspace state

- 26 crates (added cosmix-lib-script)
- First working ARexx-style multi-app script: editor content → rendered markdown in viewer
- Hub client self-call bug fixed — enables the fundamental ARexx "app scripting itself" pattern
- DataTable component ready for adoption across backup, dns, wg, files

## The ARexx moment (terminal log)

```
INFO cosmix_edit: edit.get-content: 13 bytes
INFO cosmix_script::executor: Script step 1 response: {"content":"# Hello World"}
INFO cosmix_script::executor: Script step 2 substituted args: {"content": "# Hello World"}
INFO cosmix_script::executor: Script step 2 response: {"status":"ok"}
INFO cosmix_script::menu: Script 'markdown-preview' completed (rc=0)
```

Two apps, two AMP commands, one user-defined TOML script, zero recompilation.

## Next steps

- cosmix-logd daemon for centralized AMP traffic logging
- AMP-aware MCP server for Claude Code to interact with running apps directly
- Embedded mlua runtime for multi-step scripts with conditionals/loops
- Standard ARexx command vocabulary (OPEN, SAVE, CLOSE, QUIT, HELP, INFO) across all apps
- Filesystem watcher for hot-reload of script directory
- Migrate backup/dns/wg/files to DataTable component
