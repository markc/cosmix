# cosmix-ui Component System

**Date:** 2026-03-27
**Status:** MenuBar complete (Phases 1–5), global font_size implemented, theme system planned
**Crate:** `crates/cosmix-ui/`

---

## Overview

`cosmix-ui` is the shared Dioxus component and utility library for all cosmix GUI apps. It
provides theme constants, icons, markdown rendering, desktop utilities, and — as of 2026-03-27
— a complete HTML-based MenuBar component that works on both desktop and WASM.

---

## What Exists (2026-03-27)

### Module structure

```
crates/cosmix-ui/src/
  lib.rs              ← module declarations, pub re-exports
  theme.rs            ← colour constants (migrating to OKLCH CSS vars)
  icons.rs            ← SVG icon strings
  menu/               ← HTML MenuBar component (new)
    mod.rs            ← re-exports
    types.rs          ← MenuItem, MenuAction, Shortcut, MenuBarDef
    builder.rs        ← action(), separator(), submenu(), standard_file_menu()
    component.rs      ← MenuBar Dioxus component
    shortcuts.rs      ← use_menu_shortcuts() keyboard hook
  desktop.rs          ← init_linux_env(), pick_file(), etc.
  markdown.rs         ← GFM rendering
  canvas.rs           ← canvas utilities
  util.rs             ← is_image(), mime_from_ext(), etc.
```

### Features

```toml
[features]
default = ["desktop"]
desktop = ["dep:dioxus-desktop", "dep:tao", "dep:rfd"]
hub = ["dep:cosmix-client", "dep:serde_json"]   # enables Amp menu actions
web = []                                          # WASM target
```

The `hub` feature is optional — apps that don't need AMP menu actions (e.g. cosmix-dialog)
can omit it and get a smaller binary.

---

## MenuBar Component

### The problem it solves

Native `muda` menus (bundled with dioxus-desktop) only work on desktop — no WASM support, and
they render as OS-native menus (adding unwanted system menus on GTK). The cosmix-ui MenuBar is
a pure HTML component that works identically on desktop and WASM.

### Data model (`types.rs`)

```rust
pub struct Shortcut {
    pub ctrl: bool, pub shift: bool, pub alt: bool, pub key: char,
}
impl Shortcut {
    pub fn ctrl(key: char) -> Self { ... }
    pub fn ctrl_shift(key: char) -> Self { ... }
    pub fn matches(&self, e: &KeyboardEvent) -> bool { ... }
    pub fn label(&self) -> String { ... }  // e.g. "Ctrl+S"
}

pub enum MenuAction {
    Local(String),                              // app handles by ID
    #[cfg(feature = "hub")]
    Amp { to: String, command: String, args: serde_json::Value },
    None,
}

pub enum MenuItem {
    Action { id: String, label: String, shortcut: Option<Shortcut>,
             action: MenuAction, enabled: bool },
    Separator,
    Submenu { label: String, items: Vec<MenuItem> },
}

pub struct MenuBarDef {
    pub menus: Vec<MenuItem>,   // top-level items are always Submenu
}
```

### Builder API (`builder.rs`)

```rust
action("id", "Label")
action_shortcut("id", "Label", Shortcut::ctrl('s'))
amp_action("id", "Label", "service", "command")       // hub feature
amp_action_args("id", "Label", "service", "cmd", json!({...}))
separator()
submenu("Label", vec![...])
menubar(vec![submenu1, submenu2])                       // top-level
standard_file_menu(extra_items)   // prepends extras, appends Sep + Quit
standard_help_menu("AppName")
```

### Component usage

```rust
// Without hub (cosmix-files):
MenuBar {
    menu: app_menu,
    on_action: move |id: String| match id.as_str() {
        "quit" => std::process::exit(0),
        _ => {}
    },
}

// With hub (cosmix-edit, cosmix-view):
MenuBar {
    menu: app_menu,
    hub: Some(hub_client),      // Signal<Option<Arc<HubClient>>>
    on_action: move |id: String| match id.as_str() {
        "open" => do_open(),
        "save" => do_save(),
        _ => {}
    },
}
```

`Amp` actions are dispatched directly by the MenuBar component — it spawns an async
`client.call()` internally. `Local` actions call `on_action`.

### Keyboard shortcut hook

```rust
// Instead of manual onkeydown:
let onkeydown = use_menu_shortcuts(app_menu.clone(), on_action_handler, Some(hub_client));

div { onkeydown: onkeydown, ... }
```

### Visual behaviour

- Horizontal bar of submenu trigger buttons (BG_SURFACE background, 28px height)
- Click trigger → dropdown opens; click again or click-outside → closes
- Hover between triggers while open → switches dropdown (standard menu behaviour)
- Transparent overlay div catches click-outside events
- Keyboard shortcuts rendered right-aligned in dropdown items (TEXT_DIM colour)
- Disabled items at 40% opacity

### Suppressing GTK system menus

All dioxus-desktop apps that use the cosmix-ui MenuBar must add `.with_menu(Menu::new())` to
their desktop Config to suppress the GTK-generated system menus (Window, Edit, etc.):

```rust
use dioxus_desktop::muda::Menu;
let cfg = Config::new()
    .with_window(...)
    .with_menu(Menu::new());  // ← empty menu suppresses GTK defaults
```

---

## Global Font Size System

### Implementation (completed 2026-03-27)

Every GUI app has a `static FONT_SIZE: GlobalSignal<u16>` initialised from
`cosmix_config::store::load().global.font_size` (default 14).

**cosmix-edit**: hub-based live reload — registers `config.watch`, handles `config.changed`
to update `FONT_SIZE`.

**cosmix-files, cosmix-mon, cosmix-view**: 30-second polling loop (non-WASM only) reloads
config and updates `FONT_SIZE` if changed.

Apps compute derived sizes in `app()`:

```rust
let fs = *FONT_SIZE.read();
let fs_sm = fs.saturating_sub(2);   // secondary text, status bar
let fs_lg = fs + 2;                  // headings, hostname
// Used in RSX style strings:
style: "font-size: {fs}px;"
```

### Config

```toml
[global]
font_size = 14    # default; 12–20 is sensible range
```

Edited via cosmix-settings "Global" section (first section in sidebar).

---

## Apps That Use cosmix-ui (2026-03-27)

| App | MenuBar | hub feature | Font size | Notes |
|---|---|---|---|---|
| cosmix-edit | ✓ | ✓ | ✓ | File/View menus, Preview in Viewer AMP action |
| cosmix-files | ✓ | ✗ | ✓ | File menu only |
| cosmix-mon | ✓ | ✗ | ✓ | File menu only, WASM dual-target |
| cosmix-view | ✓ | ✓ | ✓ | File/Edit/Services menus, Open in Editor AMP action |
| cosmix-settings | ✓ (partial) | ✗ | planned | Empty muda menu suppressed |
| cosmix-launcher | planned | ✓ | planned | |
| cosmix-dialog | planned | ✗ | planned | |
| cosmix-shell | planned | ✓ | from day 1 | Uses OKLCH theme system from start |

---

## Planned: OKLCH Theme System

See `2026-03-27-oklch-theme-system.md` for the full specification.

**Summary of change:** Replace `const BG_BASE: &str = "#111827"` etc. with a
`generate_css(params) -> String` function that produces a CSS `:root { --bg-primary: oklch(...) }`
block injected via `document::Style`. All app style strings migrate from inline hex values to
`var(--bg-primary)` etc.

The transition is backward-compatible: apps can be migrated one at a time. The existing
`const` values remain until all apps are migrated.

---

## Planned: Additional Components

As cosmix-shell and other apps require them:

| Component | Purpose |
|---|---|
| `Carousel` | Sidebar panel carousel with dot indicators |
| `Sidebar` | Pinnable/collapsible sidebar with carousel |
| `TopNav` | Fixed top navigation bar |
| `Panel` | Card-style panel with title bar and pop-out button |
| `StatusBar` | Bottom status bar with left/right slots |
| `Input` | Styled text input respecting theme variables |
| `Button` | Primary/secondary/ghost variants |
| `Select` | Dropdown select (theme-consistent) |
| `Slider` | Range slider (for hue, font size in settings) |
| `Toast` | Notification toast (feeds from hub event stream) |

These will be added incrementally as cosmix-shell is built. No premature abstraction —
each component is added when a second app needs it.

---

## Related Documents

- `2026-03-27-cosmix-shell-vision.md` — Shell architecture
- `2026-03-27-oklch-theme-system.md` — Theme system plan
- `2026-03-26-appmesh-ecosystem-roadmap.md` — Roadmap
