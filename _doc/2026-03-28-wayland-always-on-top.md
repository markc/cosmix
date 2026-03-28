# 2026-03-28 — Wayland Always-on-Top / Sticky Window Research

## Problem

Cosmix Dioxus desktop apps use tao (via dioxus-desktop) for window management. On Wayland, clicking another window hides the current cosmix app — there is no way for an application to request always-on-top status. This is a Wayland design decision: the compositor owns stacking order, not the client.

tao's `Window::set_always_on_top(bool)` maps to GTK3's `set_keep_above()`, which is a silent no-op on Wayland. Confirmed broken in tao #1134 and tauri #3117.

## Approaches

### 1. wlr-layer-shell (recommended, Wayland-native)

The `wlr-layer-shell` protocol allows surfaces to render on specific compositor layers: background, bottom, top, overlay. A surface on the `top` layer renders above all normal windows.

A layer-shell surface does NOT have to be a panel. With no edge anchors, a fixed size, and `exclusive_zone = 0`, it behaves as a floating always-on-top window.

**Supported compositors:** COSMIC, KDE (Plasma 6), Sway, Hyprland, river, niri, Wayfire — covers essentially all wlroots-based and Smithay-based compositors. Not supported by GNOME (mutter).

**Integration with tao/Dioxus:**

tao 0.34 provides `WindowExtUnix::new_from_gtk_window(event_loop, gtk_window)` (added in tao PR #938 for exactly this use case). The sequence:

1. Create `gtk::ApplicationWindow` manually
2. Call `gtk_layer_shell::init_for_window(&window)` BEFORE realization
3. Configure: `set_layer(Top)`, unanchor all edges (floating), set size
4. Hand window to tao via `new_from_gtk_window()`
5. Dioxus/wry renders into it normally

**Timing constraint:** `gtk_layer_init_for_window()` MUST be called before the GtkWindow is realized (shown). This means we must intercept Dioxus's window creation.

**Implementation:** Modify `cosmix-lib-ui/src/desktop.rs` `launch_desktop()` to optionally create a layer-shell window, then pass it to tao instead of letting dioxus-desktop create its own window.

**Rust crates:**
- `gtk-layer-shell` 0.8.2 (GTK3 bindings, unmaintained but functional — matches tao's GTK3)
- System library: `gtk-layer-shell` package on Arch/CachyOS (`paru -S gtk-layer-shell`)

**Trade-offs:**
- (+) Wayland-native, correct protocol for the job
- (+) Works on COSMIC, KDE, Sway, Hyprland
- (-) Layer-shell surfaces have no compositor-provided title bar or window management (but cosmix already uses frameless CSD)
- (-) `gtk-layer-shell` Rust crate is unmaintained (GTK3 is archived); C library is still maintained
- (-) Does not work on GNOME/mutter
- (-) User cannot drag/resize via compositor (must handle in app — already done via MenuBar drag region)

### 2. XWayland fallback (simplest, works now)

Set `GDK_BACKEND=x11` before launching the app. Forces tao/GTK to use X11 via XWayland, where `set_keep_above()` works.

```rust
// In init_linux_env() or per-app
std::env::set_var("GDK_BACKEND", "x11");
```

**Trade-offs:**
- (+) Zero code changes, works immediately
- (+) `set_always_on_top(true)` works perfectly under X11
- (-) Loses native Wayland benefits (fractional scaling, reduced latency, security isolation)
- (-) XWayland may have rendering quirks on HiDPI
- (-) Philosophical regression — running X11 on a Wayland desktop

### 3. COSMIC keyboard shortcut (zero effort)

COSMIC supports per-window "Always on Top" via right-click on the title bar or a keyboard shortcut. This is user-initiated, not programmatic.

**Trade-offs:**
- (+) Zero code, zero dependencies
- (-) Manual per-window, per-launch — not automatic
- (-) Only works on COSMIC, not portable

### 4. D-Bus to cosmic-comp (speculative)

COSMIC's compositor may expose window state management via D-Bus. Could potentially find the window's toplevel handle and request sticky/always-on-top state after creation.

**Trade-offs:**
- (-) No stable API exists
- (-) COSMIC-specific, fragile
- (-) Would break on compositor updates

### 5. smithay-client-toolkit (bypass GTK entirely)

Use Smithay's Wayland client toolkit to create a layer-shell surface directly, without GTK. Render with wgpu or femtovg.

**Trade-offs:**
- (-) Loses wry/WebView — would need a completely different rendering approach
- (-) Massive effort, essentially rewriting the windowing stack

## Recommendation

**Short-term:** Use approach 2 (XWayland) for apps that critically need always-on-top. Set `GDK_BACKEND=x11` conditionally per-app or via a `--pin` CLI flag.

**Medium-term:** Implement approach 1 (wlr-layer-shell) in `cosmix-lib-ui/src/desktop.rs`. The tao escape hatch exists (`new_from_gtk_window`), the protocol is widely supported, and cosmix already uses frameless windows. This is the correct Wayland-native solution.

**Key files to modify:**
- `crates/cosmix-lib-ui/src/desktop.rs` — `launch_desktop()` and `window_config()`
- `crates/cosmix-lib-ui/Cargo.toml` — add `gtk-layer-shell` optional dep behind a `layer-shell` feature

## Why not fork cosmic-comp?

Considered and rejected: forking cosmic-comp to build AMP/hub awareness directly into the compositor.

**Against:**
- COSMIC is under heavy active development — rebasing a fork is a full-time maintenance burden for a one-person project
- Compositor bugs become your problem
- AMP/hub is a userspace concern; the compositor's job is rendering surfaces and managing input
- Mixing application IPC into the compositor violates Wayland's separation of concerns

**The right architecture:**
- The hub stays in userspace as the inter-app communication layer
- Apps use standard Wayland protocols to request compositor behavior
- The compositor doesn't need to know about AMP — it just honors protocols it already supports

**What to use instead:**
- `wlr-layer-shell` — sticky/always-on-top windows (already supported by COSMIC)
- `cosmic-workspace` — workspace management (existing COSMIC protocol)
- `xdg-toplevel` — window resize, fullscreen, minimize (standard Wayland)
- `cosmic-toplevel-info` — read window state (existing COSMIC extension)

**If existing protocols fall short:**
- Write a Wayland protocol extension (`.xml` file) for the specific capability needed
- Submit as a PR to cosmic-comp — Pop's team has been receptive to protocol additions
- Example: a `cosmix-window-hints-v1` protocol that lets registered clients request always-on-top, or a more general "window pinning" hint that any compositor could adopt
- This gives the same result as a fork without the maintenance burden

**The hub's role in window management:**
- Hub sends command to app → app calls the appropriate Wayland protocol
- Example flow: `hub → "window.pin" → cosmix-mon → layer-shell set_layer(Top)`
- Example flow: `hub → "window.move-workspace 2" → app → cosmic-workspace protocol`
- The compositor never sees AMP traffic — it just sees standard protocol requests

This keeps cosmix portable across compositors (COSMIC, KDE, Sway, Hyprland) rather than locked to a fork.

## References

- tao issue #1134: Wayland + with_always_on_top not working — https://github.com/tauri-apps/tao/issues/1134
- tao PR #938: new_from_gtk_window — https://github.com/tauri-apps/tao/issues/925
- tauri issue #3117: Always on top not working on Wayland — https://github.com/tauri-apps/tauri/issues/3117
- wlr-layer-shell protocol — https://wayland.app/protocols/wlr-layer-shell-unstable-v1
- gtk-layer-shell C library — https://github.com/wmww/gtk-layer-shell
- gtk-layer-shell Rust crate — https://crates.io/crates/gtk-layer-shell
- WindowExtUnix docs (tao 0.34) — https://docs.rs/tao/0.34.8/tao/platform/unix/trait.WindowExtUnix.html
- cosmic-comp issue #934: Sticky windows — https://github.com/pop-os/cosmic-comp/issues/934
