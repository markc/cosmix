# Dioxus Desktop: Responsive / Mobile-View Layouts

> Research snapshot: March 2026, covering Dioxus 0.6 / 0.7.

---

## The Core Fact: Desktop = WebView Viewport

Dioxus desktop renders inside a system WebView (WebKitGTK on Linux, WebView2 on
Windows, WKWebView on macOS). From the CSS engine's perspective the **viewport is
the WebView content area**, which tracks the native window size 1-to-1. This means:

- Standard `@media (max-width: …)` queries fire correctly when the user resizes the window.
- Tailwind breakpoint prefixes (`sm:`, `md:`, `lg:`, `xl:`, `2xl:`) work out of the box.
- JavaScript APIs like `window.innerWidth` are available via `eval()` if you need them.

There is no special Dioxus API required for the CSS/Tailwind path. It just works.

---

## Approach 1 — CSS Media Queries + Tailwind (Recommended)

This is the lowest-friction path and the one the Dioxus team implicitly endorses.

### 1.1 Tailwind Setup (0.6 / 0.7)

Install the CLI (no Node framework needed, just the CLI):

```bash
npm install -D tailwindcss @tailwindcss/cli
```

Create `assets/input.css`:

```css
@import "tailwindcss";
/* your custom tokens here */
```

Configure `tailwind.config.js` to scan your Rust sources:

```js
module.exports = {
  content: ["./src/**/*.{html,rs}"],
  theme: { extend: {} },
  plugins: [],
};
```

Run the watcher alongside `dx serve`:

```bash
# terminal 1
npx @tailwindcss/cli -i ./assets/input.css -o ./assets/tailwind.css --watch

# terminal 2
dx serve --platform desktop
```

> **Dioxus 0.7 note:** if a `tailwind.css` is present in the app root, the `dx`
> watcher auto-initialises it — no separate terminal needed.

Reference the stylesheet in your root component:

```rust
use dioxus::prelude::*;

#[component]
fn App() -> Element {
    rsx! {
        document::Stylesheet {
            href: asset!("/assets/tailwind.css")
        }
        Layout {}
    }
}
```

### 1.2 Using Tailwind Breakpoints in RSX

Tailwind is mobile-first: unprefixed classes apply at all sizes; prefixed ones kick
in at that breakpoint **and above**.

```rust
rsx! {
    // Single-column on small windows, two-column at md (768 px), three at lg (1024 px)
    div {
        class: "grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4 p-4",
        for item in items.read().iter() {
            Card { item: item.clone() }
        }
    }
}
```

```rust
// Sidebar collapses to top bar below md
div {
    class: "flex flex-col md:flex-row h-screen",
    aside {
        class: "w-full md:w-64 bg-surface shrink-0",
        Sidebar {}
    }
    main {
        class: "flex-1 overflow-auto",
        Content {}
    }
}
```

### 1.3 Custom Breakpoints

Add to `assets/input.css` (Tailwind v4 CSS-first API):

```css
@import "tailwindcss";

@theme {
  /* "panel" breakpoint at 900 px for your mesh dashboard panels */
  --breakpoint-panel: 56.25rem;
  /* tighten xl */
  --breakpoint-xl: 72rem;
}
```

Then use `panel:grid-cols-3` etc. in RSX.

---

## Approach 2 — `onresize` Element Event (Dioxus 0.6+)

Dioxus 0.6 shipped a first-class `onresize` event handler backed by a
cross-platform `ResizeObserver`. It fires whenever the **element** changes size —
useful for component-level adaptive logic independent of the overall window size.

```rust
use dioxus::prelude::*;

#[component]
fn AdaptivePanel() -> Element {
    let mut layout = use_signal(|| "narrow");

    rsx! {
        div {
            class: "flex-1",
            onresize: move |data| {
                if let Ok(size) = data.get_border_box_size() {
                    layout.set(if size.width() > 600.0 { "wide" } else { "narrow" });
                }
            },
            // render differently based on layout signal
            if layout() == "wide" {
                WideView {}
            } else {
                NarrowView {}
            }
        }
    }
}
```

`data.get_border_box_size()` returns a `ContentBoxSize` with `.width()` and
`.height()` as `f64` in CSS pixels.

This approach is good for components that may be embedded in arbitrary containers
(e.g., a resizable panel in a split-pane layout). It is component-scoped, not
window-scoped.

---

## Approach 3 — `dioxus-resize-observer` Crate

The `dioxus-community/dioxus-resize-observer` crate wraps the browser
`ResizeObserver` API as a reactive hook:

```toml
# Cargo.toml
dioxus-resize-observer = "0.3"   # targets Dioxus 0.6
dioxus-use-mounted = "*"
```

```rust
use dioxus::prelude::*;
use dioxus_resize_observer::use_size;
use dioxus_use_mounted::use_mounted;

#[component]
fn ResponsiveCard() -> Element {
    let mounted = use_mounted();
    let (width, _height) = use_size(mounted);

    let cols = if width() > 800.0 { "grid-cols-3" } else { "grid-cols-1" };

    rsx! {
        div {
            onmounted: move |evt| mounted.onmounted(evt),
            class: "grid {cols} gap-4",
            // children
        }
    }
}
```

**Caveat:** the README explicitly lists "Web renderer (WASM)" as the support
target. In practice, because the Dioxus desktop WebView exposes the same browser
APIs, the `ResizeObserver` JS API is available and it works — but it is not
officially verified against the desktop renderer. Test before relying on it.

---

## Approach 4 — Native Window Size via `DesktopContext`

If you need the *native OS window dimensions* (e.g., to drive Rust-side logic or
to set a minimum size constraint), access `DesktopContext`:

```rust
use dioxus::desktop::DesktopContext;
use dioxus::prelude::*;

fn main() {
    let window = dioxus::desktop::tao::window::WindowBuilder::new()
        .with_resizable(true)
        .with_inner_size(dioxus::desktop::LogicalSize::new(1024.0, 768.0))
        .with_min_inner_size(dioxus::desktop::LogicalSize::new(480.0, 320.0));

    dioxus::LaunchBuilder::new()
        .with_cfg(dioxus::desktop::Config::new().with_window(window))
        .launch(App);
}

#[component]
fn App() -> Element {
    let desktop = use_context::<DesktopContext>();
    let size = desktop.window.inner_size();  // PhysicalSize<u32>

    // Size is a point-in-time read, not reactive.
    // Combine with onresize or a use_effect + eval() to get reactivity.
    rsx! { "Window: {}×{}", size.width, size.height }
}
```

**Important:** `desktop.window.inner_size()` is a snapshot, not a reactive signal.
To make it reactive, either use `onresize` on the root `body` equivalent, or poll
via `use_future` / `use_effect`.

---

## Approach 5 — `dioxus-use-window` Crate (Web-Oriented)

```toml
dioxus-use-window = "0.7"
```

```rust
use dioxus_use_window::use_window_size;

fn App() -> Element {
    let size = use_window_size();
    rsx! { "Viewport: {}×{}", size.width, size.height }
}
```

This crate uses the browser `window.innerWidth/innerHeight` APIs. It works in the
desktop WebView because those JS globals are available there too. It returns a
reactive `WindowSize` signal that updates on resize. However it is primarily
documented and tested for the web/WASM renderer — treat it as a pragmatic workaround.

---

## Approach 6 — Conditional Rendering via Rust Signals

Combine any size source with a Rust enum to switch entire component trees:

```rust
#[derive(Clone, PartialEq)]
enum Breakpoint { Mobile, Tablet, Desktop }

fn breakpoint_from_width(w: f64) -> Breakpoint {
    if w < 640.0 { Breakpoint::Mobile }
    else if w < 1024.0 { Breakpoint::Tablet }
    else { Breakpoint::Desktop }
}

#[component]
fn Root() -> Element {
    let mut bp = use_signal(|| Breakpoint::Desktop);

    rsx! {
        div {
            class: "w-full h-full",
            onresize: move |data| {
                if let Ok(size) = data.get_border_box_size() {
                    bp.set(breakpoint_from_width(size.width()));
                }
            },
            match bp() {
                Breakpoint::Mobile  => rsx! { MobileLayout {} },
                Breakpoint::Tablet  => rsx! { TabletLayout {} },
                Breakpoint::Desktop => rsx! { DesktopLayout {} },
            }
        }
    }
}
```

This is the heaviest approach (full re-renders on breakpoint change) but gives
maximum control. Use it for coarse layout switching, not fine-grained style tweaks.

---

## Recommendation Matrix

| Goal | Best Approach |
|---|---|
| Fluid column/spacing changes as window resizes | **Tailwind breakpoint classes** |
| Swap entire sidebar / nav layout | **Tailwind** or **Rust signal + onresize** |
| Per-component size awareness | **`onresize` event handler** (Dioxus 0.6+) |
| Drive Rust logic from window dimensions | **`DesktopContext.window.inner_size()`** |
| Quick prototype / web parity | **`dioxus-use-window`** |
| Component-level hook with clean API | **`dioxus-resize-observer`** (WASM-primary) |

For Cosmix's mesh dashboard — multi-panel layout that needs to collapse gracefully
when the window narrows — the pragmatic combination is:

1. **Tailwind breakpoints** for all CSS-level layout shifts (zero runtime cost).
2. **`onresize` on the root `div`** to drive a `use_signal::<Breakpoint>` for
   structural panel changes (e.g., hiding the sidebar entirely below 640 px).
3. `with_min_inner_size` in `WindowBuilder` to prevent the window from going so
   narrow that the layout breaks before the CSS can respond.

---

## Tailwind Viewport Meta Tag

In a desktop WebView you do **not** need the `<meta name="viewport" ...>` tag that
web apps require for mobile browsers, but if you are cross-compiling the same
codebase to web/WASM, keep it in your `index.html`:

```html
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
```

It is harmless on desktop and critical on mobile.

---

## Known Gotchas

- **Asset path**: use `asset!("/assets/tailwind.css")` (root-relative). The path
  resolution differs between desktop and web builds; the `asset!` macro normalises
  this as of Dioxus 0.6.
- **Tailwind scanning `.rs` files**: the default `content` glob must include
  `**/*.rs` or Tailwind will not see your class names and will tree-shake them out.
- **`onresize` is element-scoped**: it fires for that DOM element's resize, not
  the window. Attach it to a full-width container if you want window-level behaviour.
- **`dioxus-resize-observer` v0.3.0** targets Dioxus 0.6. Check the repo for the
  v0.4+ release tracking Dioxus 0.7.
- **Linux/Wayland height mismatch**: there is an open bug (#3736) where the HTML
  root element height does not exactly match the window inner size on some Wayland
  setups. Setting `height: 100vh` on the root div instead of `height: 100%` works
  around it.
- **DPI changes**: when dragging a window between monitors with different DPI,
  Dioxus may not immediately recalculate. A manual resize triggers the correct
  recalculation. This is a tao/WebKitGTK upstream issue, not Dioxus-specific.

---

## Reference Links

- [Dioxus 0.6 release — `onresize` / `onvisible`](https://dioxuslabs.com/blog/release-060/#tracking-size-with-onresize)
- [Official Tailwind guide (0.7 docs)](https://dioxuslabs.com/learn/0.7/guides/utilities/tailwind/)
- [Dioxus Tailwind example](https://github.com/DioxusLabs/dioxus/tree/main/examples/tailwind)
- [dioxus-resize-observer](https://github.com/dioxus-community/dioxus-resize-observer)
- [dioxus-use-window on docs.rs](https://docs.rs/dioxus-use-window)
- [dioxus-desktop WindowBuilder](https://docs.rs/dioxus-desktop/latest/dioxus_desktop/struct.WindowBuilder.html)
- [Tailwind responsive design docs](https://tailwindcss.com/docs/responsive-design)
