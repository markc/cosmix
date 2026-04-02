# Dioxus CSS Patterns Reference

Extracted from Dioxus 0.7.4 examples on 2026-04-02. All patterns verified compiling and running on Linux WebKitGTK.

## CSS Loading Methods

### 1. Stylesheet component (preferred)

The standard way to load CSS. Works on all platforms.

```rust
use dioxus::prelude::*;

fn app() -> Element {
    rsx! {
        Stylesheet { href: asset!("/assets/tailwind.css") }
        // app content...
    }
}
```

- `asset!()` macro embeds path at compile time
- Works for both local files and the compiled Tailwind output
- Source: `examples/10-integrations/tailwind/src/main.rs:11`

### 2. CSS Modules (scoped class names)

Avoids class name collisions by hashing class names per-file.

```rust
#[css_module("/path/to/styles.css")]
struct Styles;

rsx! {
    div { class: Styles::container,
        div { class: Styles::global_class, "no hash" }
    }
}
```

- Type-safe: class names are struct fields
- Supports `AssetOptions::css_module().with_minify(true).with_preload(false)`
- Source: `examples/03-assets-styling/css_modules.rs`

### 3. External CDN link

For quick prototyping (used in window_event.rs):

```rust
rsx! {
    document::Link {
        href: "https://unpkg.com/tailwindcss@^2/dist/tailwind.min.css",
        rel: "stylesheet"
    }
}
```

- Not recommended for production — CDN dependency
- Source: `examples/08-apis/window_event.rs:29`

### 4. Custom index.html

Inject CSS via a full custom HTML page:

```rust
dioxus::LaunchBuilder::new()
    .with_cfg(
        dioxus::desktop::Config::new().with_custom_index(
            r#"<!DOCTYPE html>
<html>
  <head>
    <style>body { background-color: olive; }</style>
  </head>
  <body><div id="main"></div></body>
</html>"#.into(),
        ),
    )
    .launch(app);
```

- Must contain `<div id="main"></div>`
- Source: `examples/08-apis/custom_html.rs`

### 5. Dynamic CSS injection via eval

Run JavaScript to inject styles at runtime:

```rust
document::eval(r#"
    let style = document.createElement('style');
    style.textContent = ':root { --my-color: red; }';
    document.head.appendChild(style);
"#);
```

- Two-way communication: `dioxus.send()` / `await dioxus.recv()`
- Only works on webview renderers (desktop/web/mobile), not native
- Source: `examples/08-apis/eval.rs`

## Tailwind v4 Integration (Official Pattern)

The canonical Tailwind setup is minimal. Two files:

**`tailwind.css`** (crate root):
```css
@import "tailwindcss";
@source "./src/**/*.{rs,html,css}";
```

That's it. Two lines. No `@theme`, no custom variables, no bridge layers.

**`src/main.rs`**:
```rust
fn app() -> Element {
    rsx! {
        Stylesheet { href: asset!("/assets/tailwind.css") }
        div { class: "text-gray-400 bg-gray-900",
            // Use stock Tailwind classes directly
        }
    }
}
```

**`Cargo.toml`**:
```toml
[dependencies]
manganis = { workspace = true }
dioxus = { workspace = true }

[features]
default = ["desktop"]
web = ["dioxus/web"]
desktop = ["dioxus/desktop"]
```

**Build**: `dx serve` compiles Tailwind. `cargo run` does NOT — use `dx serve` for any app with Tailwind classes.

Source: `examples/10-integrations/tailwind/`

## Dark Mode Approaches

### Stock Tailwind: `dark:` variant

The simplest approach — Tailwind's built-in dark mode:

```rust
div { class: "bg-white dark:bg-gray-900 text-black dark:text-gray-400",
    // Automatically switches based on OS preference or class toggle
}
```

### Context API theme toggle

For manual toggle (from `examples/04-managing-state/context_api.rs`):

```rust
#[derive(Clone, Copy, PartialEq)]
enum Theme { Light, Dark }

// Root component provides context
use_context_provider(|| Signal::new(Theme::Light));

// Any child can toggle it
let mut theme = try_use_context::<Signal<Theme>>().unwrap();
theme.set(Theme::Dark);

// Apply via class name
div { class: "display {theme.read().stylesheet()}" }
```

### data-theme attribute (dx-components pattern)

The dx-components library uses `html[data-theme="dark"]` selectors. Toggle via eval:

```rust
document::eval(r#"document.documentElement.setAttribute('data-theme', 'dark');"#);
```

## What NOT To Do: The --dark/--light Toggle Bug

The dx-components CSS uses a toggle pattern:
```css
background-color: var(--dark, #333) var(--light, #fff);
```

This relies on CSS `var()` fallback behaviour:
- When `--dark: initial` → the fallback `#333` is used
- When `--dark` is **empty string** → it resolves to empty, effectively hiding that half

**The bug**: Setting the inactive toggle to `" "` (space) instead of `""` (empty) produces `" " #fff` — invalid CSS. This broke 18 of 41 dx-components (borders, button outlines, card borders).

**Lesson**: Never set CSS custom properties to space when empty string is required. Better yet, don't use this pattern at all — use `data-theme` selectors or Tailwind `dark:` classes instead.

## Asset Pipeline

- `asset!()` embeds metadata via linker symbols at compile time
- The `dx` CLI extracts and processes assets during build
- Assets are content-hashed for cache busting
- For images: `img { src: asset!("/assets/logo.png") }`
- For CSS: `Stylesheet { href: asset!("/assets/tailwind.css") }`

Source: `examples/03-assets-styling/custom_assets.rs`
