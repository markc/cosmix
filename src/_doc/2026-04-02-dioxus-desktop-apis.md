# Dioxus Desktop APIs Reference

Extracted from Dioxus 0.7.4 examples on 2026-04-02. All patterns verified on Linux WebKitGTK.

## Window Management

### window() API

Access from any component via `dioxus::desktop::window()`:

```rust
use dioxus::desktop::window;

// Window dragging (use on header/titlebar)
header { onmousedown: move |_| window().drag() }

// Window operations
window().set_minimized(true);
window().set_fullscreen(true);
window().set_always_on_top(true);
window().set_decorations(false);
window().set_resizable(true);
window().set_title("My App");
window().close();
```

**Critical**: Buttons inside a draggable area must call `evt.stop_propagation()` on `onmousedown` to prevent the drag handler from capturing their clicks:

```rust
button {
    onmousedown: |evt| evt.stop_propagation(),
    onclick: move |_| window().set_minimized(true),
    "Minimize"
}
```

Source: `examples/08-apis/window_event.rs`

### Window Configuration at Launch

```rust
use dioxus::desktop::{Config, WindowBuilder};

dioxus::LaunchBuilder::desktop()
    .with_cfg(
        Config::new().with_window(
            WindowBuilder::new()
                .with_title("My App")
                .with_decorations(false),
        ),
    )
    .launch(app);
```

Source: `examples/08-apis/window_event.rs:15-25`

### Custom HTML Index

```rust
Config::new().with_custom_index(r#"<!DOCTYPE html>..."#.into())
```

Must contain `<div id="main"></div>` for the Dioxus root.

Source: `examples/08-apis/custom_html.rs`

## Menu Bar (Native)

Uses the `muda` crate re-exported from `dioxus::desktop::muda`:

```rust
use dioxus::desktop::{muda::*, use_muda_event_handler};

// Build menu at launch
let menu = Menu::new();
let edit_menu = Submenu::new("Edit", true);
edit_menu.append_items(&[
    &PredefinedMenuItem::undo(None),
    &PredefinedMenuItem::redo(None),
    &PredefinedMenuItem::separator(),
    &MenuItem::with_id("my-action", "My Action", true, None),
]).unwrap();
menu.append(&edit_menu).unwrap();

let config = Config::new().with_menu(menu);
dioxus::LaunchBuilder::new().with_cfg(config).launch(app);

// Handle menu events in component
fn app() -> Element {
    use_muda_event_handler(move |event| {
        if event.id() == "my-action" {
            // handle action
        }
    });
    // ...
}
```

**Note**: This creates OS-native menus. Cosmix uses custom HTML MenuBar instead — more portable, more flexible, but not native.

Source: `examples/08-apis/custom_menu.rs`

## Global Keyboard Shortcuts

OS-level shortcuts that work even when the app is not focused:

```rust
use dioxus::desktop::{HotKeyState, use_global_shortcut};

fn app() -> Element {
    let mut toggled = use_signal(|| false);
    _ = use_global_shortcut("ctrl+s", move |state| {
        if state == HotKeyState::Pressed {
            toggled.toggle();
        }
    });
    rsx!("toggle: {toggled}")
}
```

Shortcut format: `"ctrl+s"`, `"shift+alt+p"`, etc.

Source: `examples/08-apis/shortcut.rs`

## JavaScript Interop (eval)

Two-way communication between Rust and JavaScript in the webview:

```rust
// Execute JS and get result
let mut eval = document::eval(r#"
    dioxus.send("Hi from JS!");
    let msg = await dioxus.recv();
    console.log(msg);
    return "result";
"#);

// Rust → JS
eval.send("Hi from Rust!").unwrap();

// JS → Rust
let msg: String = eval.recv().await.unwrap();

// Await final return value
let result = eval.await;
```

**Only works on webview renderers** (desktop/web/mobile). Native renderers will throw "unsupported".

Use cases: DOM manipulation, theme attribute toggling, reading browser APIs.

Source: `examples/08-apis/eval.rs`

## Launch Patterns

### Simple (auto-detect platform)
```rust
dioxus::launch(app);
```

### Desktop-specific
```rust
dioxus::LaunchBuilder::desktop().launch(app);
```

### Desktop with config
```rust
dioxus::LaunchBuilder::desktop()
    .with_cfg(Config::new()
        .with_window(WindowBuilder::new().with_title("My App"))
        .with_menu(menu)
        .with_custom_index(html.into()))
    .launch(app);
```

## WebKitGTK Gotchas (Linux Desktop)

Observed during testing on CachyOS with Intel Arc GPU:

1. **MESA warnings**: `"Support for this platform is experimental with Xe KMD"` — cosmetic, ignore
2. **GTK menu warnings**: `"gtk_window_set_mnemonics_visible: assertion failed"` — cosmetic with muda menus
3. **Compositing**: Some apps need `WEBKIT_DISABLE_COMPOSITING_MODE=1` to avoid black screen
4. **Font weight**: WebKitGTK renders ~100 weight units heavier than Chromium (specify lighter than intended)
5. **No backdrop-filter**: Not implemented in WebKitGTK
6. **No native HTML5 drag**: Can crash Wayland compositor — use mousedown/mousemove/mouseup instead
7. **rem vs px**: `rem` renders more consistently than `px` across WebKitGTK and Chromium
