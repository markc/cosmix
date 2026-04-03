# Iced Layer-Shell: Replacing GTK3 with Pure Rust Rendering

## Context

On 2026-04-03 we built GTK3 layer-shell dialogs for cosmix-dialog with thin FFI bindings to system `libgtk-layer-shell`. This works but has fundamental limitations:

- **C dependency chain**: libgtk3 + libgtk-layer-shell + Cairo — not pure Rust
- **Unsafe FFI**: 8 hand-written `unsafe extern "C"` function wrappers
- **Cannot be shared**: cosmix-dialog pulls in Dioxus/cosmix-lib-ui, making it too heavy for Mix
- **Dated rendering**: Cairo CPU rasterizer, GtkCssProvider for styling
- **RGBA hack**: transparent window + border-radius workaround for rounded corners

Meanwhile, `iced_layershell` v0.17.1 on crates.io provides:

- **Pure Rust**: zero C FFI, zero `unsafe` in our code
- **Native layer-shell**: built on smithay-client-toolkit, actively maintained (March 2026)
- **GPU or CPU**: wgpu (GPU) or tiny-skia (CPU software renderer) — both optional
- **Modern widget system**: Iced's Elm architecture with button, text_input, container, column, row, theme
- **Multi-window**: `daemon()` pattern for dynamic window creation (panels, notifications, dialogs simultaneously)
- **Session lock**: `iced_sessionlock` in the same ecosystem

## Proposed Architecture

### New Shared Crate: `cosmix-lib-layer`

A shared rendering library usable by both cosmix-dialog and mix-dialog:

```
src/crates/cosmix-lib-layer/
  Cargo.toml
  src/
    lib.rs              — Public API: show_dialog(), show_panel(), show_toast()
    dialog.rs           — Dialog Iced views (message, question, entry, password, combobox, progress)
    theme.rs            — Cosmix dark theme for Iced (colors, button styles, input styles)
    blocking.rs         — Persistent Iced thread + channel pattern for sync callers
```

### Dependency Chain

```
cosmix-lib-layer (new)
  └── iced 0.14 (default-features = false, features = ["tiny-skia", "wayland"])
  └── iced_layershell 0.17

cosmix-dialog [layer-shell feature]  →  cosmix-lib-layer
mix-dialog (future)                  →  cosmix-lib-layer (or vendored copy)
```

No Dioxus, no cosmix-lib-ui, no GTK, no C libraries. Pure Rust from API to pixels.

### Renderer Choice

Both renderers are available:

| Renderer | Binary size | GPU required | Startup time | Use case |
|----------|------------|-------------|-------------|----------|
| tiny-skia | ~8-12MB | No | Fast | Dialogs, notifications, simple panels |
| wgpu | ~20-30MB | Yes | Slower (shader compile) | Rich panels, animations |

**Recommendation: tiny-skia for dialogs.** A 120px dialog doesn't need GPU acceleration. tiny-skia is a pure Rust 2D rasterizer — no GPU drivers, no shader compilation, works on headless with virtual framebuffer. wgpu can be enabled as an optional feature for rich panels later.

## Iced Layer-Shell API (Key Patterns)

### Simple Dialog App

```rust
use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length, Theme};
use iced_layershell::application;
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings};
use iced_layershell::to_layer_message;

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Ok,
    Cancel,
    InputChanged(String),
}

struct EntryDialog {
    prompt: String,
    value: String,
}

fn update(state: &mut EntryDialog, message: Message) -> iced::Task<Message> {
    match message {
        Message::Ok => iced::Task::done(Message::RemoveWindow),
        Message::Cancel => iced::Task::done(Message::RemoveWindow),
        Message::InputChanged(s) => { state.value = s; iced::Task::none() }
        _ => iced::Task::none(),
    }
}

fn view(state: &EntryDialog) -> Element<Message> {
    container(
        column![
            text(&state.prompt),
            text_input("", &state.value).on_input(Message::InputChanged),
            row![
                button("Cancel").on_press(Message::Cancel),
                button("OK").on_press(Message::Ok),
            ].spacing(8),
        ]
        .spacing(12)
        .padding(16),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center(Length::Fill)
    .into()
}

fn main() -> Result<(), iced_layershell::Error> {
    application(
        || EntryDialog { prompt: "Name:".into(), value: String::new() },
        |_| String::from("cosmix-dialog"),
        update,
        view,
    )
    .settings(Settings {
        layer_settings: LayerShellSettings {
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            size: Some((360, 140)),
            exclusive_zone: -1,
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
}
```

### Key Differences from GTK3 Approach

| Aspect | GTK3 + FFI | Iced layer-shell |
|--------|-----------|-----------------|
| Window creation | `gtk::Window::new()` + FFI calls | `LayerShellSettings` struct |
| Widget tree | `gtk::Box`, `gtk::Label`, `gtk::Button` | `column![]`, `text()`, `button()` |
| Styling | GtkCssProvider string | Iced `Theme` + `Style` traits |
| Event handling | GTK signal callbacks (`connect_clicked`) | Elm messages (`Message::Ok`) |
| Lifecycle | `gtk::main()` blocks | `application().run()` blocks |
| Rounded corners | RGBA visual hack | Native (Iced renders into buffer) |
| Thread safety | Must init on one thread, `OnceLock<Mutex<Sender>>` | Same pattern needed |

### Dynamic Properties (Runtime Changeable)

All layer-shell properties can be changed at runtime by returning messages:

```rust
Message::AnchorChange(Anchor::Top | Anchor::Left)     // Reposition
Message::SizeChange((400, 200))                         // Resize
Message::LayerChange(Layer::Top)                        // Change layer
Message::MarginChange((100, 50, 0, 50))                // Reposition via margins
Message::KeyboardInteractivityChange(Exclusive)         // Grab keyboard
Message::RemoveWindow                                   // Close
```

This maps perfectly to AMP control — a Mix script could send `dialog.resize 400 200` which triggers `Message::SizeChange`.

### Multi-Window (Daemon Pattern)

For future panel + notifications + dialogs simultaneously:

```rust
use iced_layershell::to_layer_message;

#[to_layer_message(multi)]
#[derive(Debug, Clone)]
enum Message {
    ShowNotification(String),
    DismissNotification(IcedId),
    // Macro auto-adds: NewLayerShell { settings, id }, RemoveWindow(id), ...
}

// Create a new notification surface dynamically:
fn update(state: &mut State, message: Message) -> iced::Task<Message> {
    match message {
        Message::ShowNotification(text) => {
            let id = IcedId::unique();
            state.notifications.insert(id, text);
            iced::Task::done(Message::NewLayerShell {
                settings: NewLayerShellSettings {
                    size: Some((320, 80)),
                    layer: Layer::Overlay,
                    anchor: Anchor::Top | Anchor::Right,
                    margin: Some((8, 0, 0, 8)),
                    keyboard_interactivity: KeyboardInteractivity::None,
                    ..Default::default()
                },
                id,
            })
        }
        Message::DismissNotification(id) => {
            state.notifications.remove(&id);
            iced::Task::done(Message::RemoveWindow(id))
        }
        _ => iced::Task::none(),
    }
}
```

## Migration Plan

### Phase 1: POC — Iced Layer-Shell Dialog

New standalone crate `cosmix-dialog-iced` (parallel to `cosmix-dialog-layer` GTK POC):

- Single message dialog: text + OK button on Overlay layer
- Verify: renders at 320x100, no compositor min-size clamping
- Verify: rounded corners work natively
- Verify: keyboard focus (Escape to dismiss, Enter to confirm)
- Verify: tiny-skia renderer works (no GPU requirement)
- Compare: visual quality, startup time, binary size vs GTK3 POC

### Phase 2: Full Dialog Set

Build all compact dialog types in Iced:

- Message (info/warning/error with icon)
- Question (yes/no/cancel)
- Entry (text input with default/placeholder)
- Password (masked input)
- ComboBox (dropdown selection)
- Progress (determinate/indeterminate bar)

Extract into `cosmix-lib-layer` shared crate.

### Phase 3: Replace GTK3 Backend in cosmix-dialog

- `cosmix-dialog` backend dispatch routes to `cosmix-lib-layer` instead of `layer/` GTK module
- Remove GTK3 deps (gtk, gdk, glib) and build.rs pkg-config probe
- Remove all `unsafe` FFI code
- Keep Dioxus backend for complex dialogs (Form, TextViewer, CheckList)

### Phase 4: mix-dialog Uses cosmix-lib-layer

- `mix-dialog` crate at `~/.mix/src/crates/mix-dialog/` depends on `cosmix-lib-layer`
- Or vendors a copy if zero-cosmix-dep is required
- Registers same `dialog_info`, `dialog_entry`, etc. ExtFns
- Terminal fallback for headless environments

### Phase 5: Desktop Shell Widgets (Future)

Using `daemon()` multi-window pattern:

- `layer_panel()` — persistent panel surface
- `layer_toast()` — notification popups
- `layer_launcher()` — app launcher overlay
- All controlled by Mix scripts via AMP

## Risks

| Risk | Mitigation |
|------|-----------|
| iced_layershell 0.17 API stability | Actively maintained, 61 releases, used by multiple projects |
| tiny-skia visual quality for dialogs | Test in POC — should be fine for flat UI (no complex gradients) |
| Iced Elm architecture learning curve | Simpler than GTK for new code; the pattern is mechanical |
| Binary size with iced | tiny-skia feature only, no wgpu — keeps it reasonable |
| Startup time (Iced init) | Measure in POC; should be <100ms for tiny-skia |
| Persistent thread pattern | Same OnceLock/channel pattern works — Iced's `run()` blocks like `gtk::main()` |

## Decision

**Replace the GTK3 layer-shell backend with Iced + iced_layershell.** The GTK3 work proved the concept and established the architecture (backend dispatch, persistent thread, Mix ExtFn). The rendering layer should be pure Rust before shipping.

The GTK3 code is not wasted — it validated the UX, sizing, and interaction patterns. The Iced implementation reuses the same `DialogRequest → DialogResult` data flow, the same backend dispatch in `cosmix-dialog`, and the same `blocking.rs` persistent thread pattern. Only the widget rendering changes.
