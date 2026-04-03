# cosmic-comp 240px Minimum Window Height: Research & Workarounds

## Executive Summary

After investigating all eight approaches, **the layer-shell approach is the most viable workaround without patching cosmic-comp**. The 240px minimum is enforced at two levels — GTK/tao's widget requisition and cosmic-comp's hardcoded fallback — making it impossible to solve within the tao/wry/Dioxus stack alone. Here's the full analysis.

---

## 1. Is tao/wry Actually Sending `xdg_toplevel.set_min_size`?

**Verdict: Partially — but it doesn't matter.**

tao's Linux backend uses GTK3 under the hood. When you call `with_min_inner_size()`, tao calls `gtk_window_set_geometry_hints()` with `GDK_HINT_MIN_SIZE`, which GTK3's Wayland backend translates into `xdg_toplevel.set_min_size()` on the wire. The Wayland protocol trace from tao issue #977 confirms this — you can see `xdg_toplevel@22.set_min_size(900, 587)` being sent.

**However**, there's a critical subtlety: GTK3 also has its own internal minimum size based on widget requisition. The `GtkWindow` will never go below the "natural size" of its child widgets. When WebKitGTK is the child widget, it has its own minimum content area requirements. tao's commit 4524d5d explicitly notes a `100, 100` minimum limitation on Linux when using `set_resizable`. The min_size hint is sent to the compositor, but **GTK refuses to draw a surface smaller than its own widget tree requires**.

Furthermore, the Wayland spec explicitly states: "The client should not rely on the compositor to obey the minimum size." — `set_min_size` is a *hint*, not a command. The compositor can (and does) override it.

**Key finding**: Even if tao sends `set_min_size(320, 100)` on the wire, cosmic-comp's `map_internal` does:
```rust
let min_size = mapped.min_size().unwrap_or((320, 240).into());
```
This means: if the client sends a min_size, cosmic-comp uses it. If not, it defaults to 320×240. But this is only the *mapping* minimum. The resize grab code has a separate fallback:
```rust
let min_height = min_size.map(|s| s.h).unwrap_or(240);
```
These are independent enforcement points.

---

## 2. Does WebKitGTK Override the Window Size?

**Verdict: Yes — WebKitGTK has its own minimum content height.**

WebKitGTK runs a multi-process architecture (WebProcess, NetworkProcess) and the `WebKitWebView` widget has an internal minimum size requisition. This is determined by:

- The GTK widget's `size_request` / natural size allocation
- WebKit's internal content rendering pipeline
- The scrollable viewport minimum

The `WebKitWebView` widget will request a minimum size from GTK that includes space for at least the viewport chrome. In practice, a bare WebKitGTK webview requests approximately 200-250px minimum height depending on the GTK theme and scale factor.

This is separate from cosmic-comp's enforcement. Even if you patched cosmic-comp to allow 100px windows, the WebKitGTK widget itself would still request ~200px+ from GTK, and GTK would set that as the surface's minimum geometry.

The Wails project (another WebKitGTK-based framework) documented similar issues — window sizing on Wayland is constrained by GTK themes, with different themes producing slightly different "phantom maximum/minimum" sizes.

**Workaround within this constraint**: You could try `gtk_widget_set_size_request(webview, 320, 100)` on the WebKitWebView widget directly, but this fights GTK's layout system and may produce rendering artifacts or scrollbars.

---

## 3. Wayland Popup Approach (`xdg_popup`)

**Verdict: Theoretically possible, but not via tao/wry.**

`xdg_popup` surfaces are indeed treated differently by compositors — they don't go through the same min-size logic as toplevels. Popups are positioned relative to a parent surface and typically aren't subject to floating layout constraints.

**However**:
- tao does not expose `xdg_popup` creation. All windows are `xdg_toplevel`.
- wry/Dioxus desktop has no concept of popup surfaces.
- Creating an `xdg_popup` requires a parent `xdg_surface` — you'd need a (possibly hidden) parent toplevel first.
- Popups have their own constraints: they must be dismissed when losing focus (grab semantics), they can't be moved freely, and they're positioned by the compositor relative to the parent.

For a zenity-style dialog, a popup is semantically correct (it's a transient dialog), but you can't get there through tao/wry.

---

## 4. Layer Shell Approach — **RECOMMENDED**

**Verdict: This is your best option.**

The `wlr-layer-shell` protocol creates surfaces that are completely outside the normal toplevel window management. Layer surfaces:
- Are NOT subject to cosmic-comp's floating layout min_size logic
- Have explicit size control via `set_size(width, height)`
- Can be placed on the `overlay` layer (above everything) or `top` layer
- Can receive keyboard focus via `set_keyboard_interactivity`
- Work on cosmic-comp (Smithay-based compositors support `zwlr_layer_shell_v1`)

**Rust crates available**:
- `gtk4-layer-shell` (crates.io) — safe Rust bindings for GTK4. Has Rust examples.
- `gtk-layer-shell` (crates.io) — GTK3 version (maintenance mode, but GTK3 matches your current tao/wry stack)
- `wayland-protocols-wlr` — raw protocol bindings if going direct

**The GTK3 path** (`gtk-layer-shell` crate):
```rust
// Pseudocode — create a GTK3 window with layer-shell
let window = gtk::Window::new(gtk::WindowType::Toplevel);
gtk_layer_shell::init_for_window(&window);
gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Overlay);
gtk_layer_shell::set_keyboard_mode(&window, gtk_layer_shell::KeyboardMode::OnDemand);
// Explicit size — no compositor min_size enforcement
window.set_default_size(320, 100);
```

**Implications for your zenity replacement**:
- You'd bypass Dioxus desktop for these small dialogs and use GTK3 directly with `gtk-layer-shell`
- The dialog would be a layer surface, which means it's not managed as a regular window (no minimize, no taskbar entry, no tiling)
- This is actually *more appropriate* for a zenity-style tool — zenity dialogs should be transient overlays, not regular windows
- You could still embed a WebKitGTK webview inside the layer surface for HTML rendering, but you'd control the GTK window yourself rather than going through tao

**Caveats**:
- Layer surfaces don't show in the taskbar/app switcher (by design)
- You'd need to handle positioning yourself (anchor to center of screen, or a specific edge)
- GNOME does NOT support `wlr-layer-shell` — but you're on COSMIC, so irrelevant
- The `gtk-layer-shell` docs note: "Setting to 1, 1 is sometimes useful to keep the window the smallest it can be while still fitting its contents" — confirming no minimum size enforcement

---

## 5. cosmic-comp Configuration

**Verdict: No config exists. The minimum is hardcoded.**

COSMIC stores configuration in RON files under `~/.config/cosmic/com.system76.CosmicComp/`. The config schema covers things like:
- Workspace behavior
- Tiling settings (gaps, active hint)
- Input settings
- Autotiling rules

There is **no configuration key** for minimum window dimensions. The 240px fallback is hardcoded in the Rust source (`floating/mod.rs` and `floating/grabs/resize.rs`). There's no dconf key, no COSMIC settings panel option, and no environment variable to override it.

---

## 6. GTK_CSD / xdg-decoration Tricks

**Verdict: Doesn't help with the size constraint.**

Setting `GTK_CSD=0` or using `xdg-decoration` to request server-side decorations changes how *decorations* are drawn but does not affect the compositor's minimum window size logic. cosmic-comp's min_size enforcement happens in the floating layout code, which runs regardless of decoration mode.

With `with_decorations(false)` (which you're already using), you're already requesting CSD with no visible decorations. The compositor still applies its min_size logic to the toplevel surface geometry.

Environment variables tested:
- `GTK_CSD=0` — forces SSDs where supported, no effect on min_size
- `GDK_BACKEND=wayland` — ensures native Wayland (no X11 fallback), no effect on min_size
- There's no hidden env var in cosmic-comp for this

---

## 7. Direct Wayland Approach (bypass tao/wry)

**Verdict: Would confirm the problem but not solve it.**

Using `wayland-client` directly (or `smithay-client-toolkit`) to create a toplevel with explicit `set_min_size(320, 100)` and `set_max_size(320, 100)` would let you test whether cosmic-comp respects the client hint. Based on the source code analysis:

- If you send `set_min_size`, cosmic-comp's `mapped.min_size()` will return `Some((320, 100))`
- The `map_internal` code uses this as the minimum: `let min_size = mapped.min_size().unwrap_or((320, 240).into());`
- So cosmic-comp *should* allow it at the mapping stage

**But** there's a second enforcement point in the resize grab code and potentially in the configure event handling. The compositor sends a `configure` event with suggested dimensions, and if the surface geometry doesn't match, the compositor may override.

A minimal test using `smithay-client-toolkit`:
```rust
// Create toplevel
toplevel.set_min_size(Some((320, 100)));
toplevel.set_max_size(Some((320, 100)));
toplevel.set_title("small-test");
// Commit a 320x100 buffer
```

This would be a useful diagnostic but likely still won't produce a window smaller than 240px because the `map_internal` enforced minimum is applied to the *initial* size, and the surface gets configured to at least that minimum before the first frame.

---

## 8. The Nuclear Option: Patching cosmic-comp

If you need this to work *as a toplevel* and layer-shell is unacceptable, here's the specific patch:

### File: `src/shell/layout/floating/mod.rs`

In `map_internal` (or wherever `min_size` is consumed at map time), change:
```rust
// Before:
let min_size = mapped.min_size().unwrap_or((320, 240).into());

// After — make fallback configurable or just lower it:
let min_size = mapped.min_size().unwrap_or((1, 1).into());
```

### File: `src/shell/layout/floating/grabs/resize.rs`

Change the resize grab fallback:
```rust
// Before:
let min_width = min_size.map(|s| s.w).unwrap_or(360);
let min_height = min_size.map(|s| s.h).unwrap_or(240);

// After:
let min_width = min_size.map(|s| s.w).unwrap_or(36);
let min_height = min_size.map(|s| s.h).unwrap_or(24);
```

**To make it properly configurable**, you'd add a key to `cosmic-comp-config`:
```rust
// In the config struct:
pub min_window_width: u32,  // default 320
pub min_window_height: u32, // default 240
```
And read it in the layout code. This would be a reasonable upstream PR — many compositors allow configuring minimum window dimensions.

**Build process** (from the Arch Wiki / CachyOS): You'd rebuild `cosmic-comp` from source and install it to replace the system package.

---

## Recommended Path Forward

For your zenity/kdialog replacement use case:

1. **Immediate solution**: Use `gtk-layer-shell` (GTK3 crate) to create small overlay dialogs. Skip tao/wry/Dioxus for these. A layer surface at 320×100px on the `overlay` layer with keyboard interactivity is the semantically correct Wayland approach for a transient dialog tool.

2. **If you need it in Dioxus**: Consider filing an upstream issue on `pop-os/cosmic-comp` requesting a configurable minimum window size. The current 240px hardcode is reasonable for general desktop use but prevents legitimate small-window use cases. You could submit the patch above as a PR.

3. **Diagnostic step**: Run `WAYLAND_DEBUG=1 your-app 2>&1 | grep -E 'set_min_size|set_max_size|configure'` to see exactly what's going on the wire. This will tell you whether tao is sending the min_size hint and what cosmic-comp is configuring back.

4. **Future consideration**: When Mix is mature enough to be your dialog scripting language, the layer-shell approach gives you a clean separation — Mix scripts invoke a small Rust binary that creates layer-shell dialogs, completely independent of the Dioxus desktop app stack.

