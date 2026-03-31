# 2026-03-26 — Appmesh Phase 4: productivity apps and hub delegation

## What was done

### cosmix-edit (new crate)
Text editor service for the appmesh:
- Dioxus 0.7 desktop app with dark monospace theme, line numbers, status bar
- Registers as "edit" service on the hub
- Service commands: `edit.open` (path + optional line), `edit.goto` (line number),
  `edit.compose` (prefilled content for email drafts etc.), `edit.get` (return content)
- Hub commands update editor via GlobalSignal (OPEN_REQUEST)
- File open via Ctrl+O (cosmix-ui file picker), save via Ctrl+S
- Modified indicator in title bar
- ~300 lines

### cosmix-mail hub retrofit
- Added `cosmix-client` dependency
- New `src/hub.rs` module — connects as "mail" service, handles `mail.status` command
- Hub connection on app startup via `use_effect`, silent fallback if hub unavailable
- Pattern established for future: `mail.compose` can delegate to `edit.compose`,
  attachment picker can delegate to `file.pick`

## Inter-app delegation pattern

The Phase 4 goal: apps delegate to each other through the hub.

```
User clicks "Compose" in cosmix-mail
    → mail calls edit.compose via hub
    → cosmix-edit opens with draft content
    → user edits, saves
    → mail retrieves via edit.get

User clicks "Attach" in cosmix-mail
    → mail calls file.pick via hub
    → cosmix-files opens file picker
    → user selects file
    → files returns path to mail
```

This works locally (same machine) and across the mesh (remote edit.mko.amp).

## New/modified files
- `crates/cosmix-edit/` — new text editor crate
- `crates/cosmix-mail/Cargo.toml` — added cosmix-client dep
- `crates/cosmix-mail/src/hub.rs` — new hub integration module
- `crates/cosmix-mail/src/main.rs` — hub connection on startup
- `Cargo.toml` — added cosmix-edit to workspace

## Decisions
- **Textarea over CodeMirror 6**: start simple, upgrade later. The service port
  interface is the same regardless of editor implementation.
- **GlobalSignal for hub→UI**: hub commands write to a static signal, UI polls it.
  Works with Dioxus's reactive model without threading complexity.
- **Silent hub fallback**: all apps work standalone if hub isn't running.
  Hub integration is additive, never blocking.
