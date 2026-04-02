# Dioxus App Architecture Reference

Extracted from Dioxus 0.7.4 examples on 2026-04-02.

## Standard App Structure

Every Dioxus app follows this pattern:

```rust
use dioxus::prelude::*;

fn main() {
    dioxus::launch(app);  // Auto-detects platform from features
}

fn app() -> Element {
    rsx! {
        Stylesheet { href: asset!("/assets/tailwind.css") }
        // App content...
    }
}
```

## Platform Features

Control target platform via Cargo features:

```toml
[features]
default = ["desktop"]
web = ["dioxus/web"]
desktop = ["dioxus/desktop"]
native = ["dioxus/native"]
mobile = ["dioxus/mobile"]
server = ["dioxus/server"]
```

`dioxus::launch()` auto-selects the correct renderer based on which feature is enabled.

## Build Tools: dx serve vs cargo run

**Critical distinction for Tailwind apps:**

| Command | Tailwind compiled? | Hot-reload? | Use for |
|---------|-------------------|-------------|---------|
| `dx serve` | Yes | Yes | GUI apps with Tailwind |
| `dx serve --hotpatch` | Yes | Yes + Rust patching | Development |
| `dx build --release` | Yes | No | Release builds |
| `cargo run` | **No** | No | Daemons, non-Tailwind apps |
| `cargo build --release` | **No** | No | **NEVER for GUI with Tailwind** |

`dx serve` runs the Tailwind compiler, watches for changes, and hot-reloads. `cargo run` skips all of that.

## Dioxus.toml Configuration

Optional config file at crate root:

```toml
[application]
name = "my-app"
default_platform = "desktop"
out_dir = "dist"
asset_dir = "assets"

[web.app]
title = "My App"

[web.watcher]
reload_html = true
watch_path = ["src", "assets"]
```

Most simple desktop apps don't need this — `dx serve` works without it.

## Asset Pipeline

Assets are managed via the `asset!()` macro (from `manganis` crate):

```rust
// CSS
Stylesheet { href: asset!("/assets/tailwind.css") }

// Images
static LOGO: Asset = asset!("/assets/logo.png");
img { src: LOGO }
```

- Compile-time path validation
- Content-hashed for cache busting in web builds
- Embedded in binary for mobile/WASM platforms
- Processed by `dx` CLI during build

## Launch Variants

```rust
// Simple (auto-detect platform)
dioxus::launch(app);

// Desktop-specific
dioxus::LaunchBuilder::desktop().launch(app);

// Desktop with configuration
dioxus::LaunchBuilder::desktop()
    .with_cfg(
        dioxus::desktop::Config::new()
            .with_window(WindowBuilder::new()
                .with_title("My App")
                .with_decorations(false))
            .with_menu(menu)
            .with_custom_index(html.into()),
    )
    .launch(app);
```

## Component Patterns

### Basic Component
```rust
#[component]
fn MyComponent(name: String, count: i32) -> Element {
    rsx! { div { "Hello {name}, count: {count}" } }
}
```

### Optional Props
```rust
#[component]
fn MyComponent(name: String, #[props(default)] count: i32) -> Element {
    rsx! { div { "{name}: {count}" } }
}
```

### Event Handlers
```rust
#[component]
fn Button(onclick: EventHandler<MouseEvent>) -> Element {
    rsx! { button { onclick: move |e| onclick.call(e), "Click" } }
}
```

## Example Categories (Dioxus Repo)

All at `~/.cosmix/dioxus/examples/`:

| Category | Count | Key patterns |
|----------|-------|-------------|
| 01-app-demos | 18 | Full apps (hackernews, file-explorer, todomvc) |
| 02-building-ui | 3 | Event bubbling, disabled states, SVG |
| 03-assets-styling | 5 | CSS modules, asset!(), dynamic assets |
| 04-managing-state | 6 | Signals, context, global, memos, reducers |
| 05-using-async | 5 | Futures, suspense, streams |
| 06-routing | 8 | Type-safe routing, query params, scroll restore |
| 07-fullstack | 16 | Server functions, SSR, auth, WebSockets |
| 08-apis | 25 | Desktop APIs (window, menu, shortcuts, eval) |
| 09-reference | 9 | Language reference (events, generics, spread) |
| 10-integrations | 6 | Tailwind, PWA, Bevy, WGPU |

**All 80+ examples compile on our system.** Key desktop examples verified running on WebKitGTK.

## Compilation Results (2026-04-02)

Tested on CachyOS Linux, Intel Arc GPU, WebKitGTK:

- **60+ standalone .rs examples**: All compile, representative set verified running
- **15 full project crates**: All compile (tailwind, file-explorer, hackernews, ecommerce, hotdog, fullstack-desktop, PWA)
- **Special features needed**: `hash_fragment_state` (ciborium + base64), `wgpu_child_window` (gpu + desktop)
- **WebKitGTK quirks**: MESA warnings (cosmetic), GTK menu assertion warnings (cosmetic)
