# Shared iced widgets

`cosmix-iced-widgets` 0.1.0 provides a single-line `TextField`, a menu bar,
context menus and a `cosmix-design` colour/metric adapter. It uses upstream
iced exactly 0.14.0, with defaults disabled and Wayland enabled. Neither
renderer is selected by the library's defaults. Hosts select `wgpu` or
`tiny-skia`; renderer-generic widgets can also be embedded in a host's UI.
Upstream iced requires a renderer feature for release builds.

The text field wraps iced's `text_input`: selection, clipboard, placeholder,
password presentation and IME remain in the upstream widget. Focused fields
handle Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y. Applications own the string and apply
`on_input` messages as with iced's text input. Undo history belongs to the
widget tree; keep the widget's identity stable across views. Password mode
obscures presentation; it does not encrypt the application value or history.

Menus publish application messages. `Item::action`, `Item::submenu` and
`Item::separator` define the tree; `.enabled(false)` disables an entry and
`.accelerator("Ctrl+S")` displays a label. Accelerator labels do not register
global shortcuts: the application owns those bindings. `Menu::bar` displays
top-level items; `Menu::context(content, items)` wraps a context-menu target.
Menus use in-surface iced overlays, not Wayland popup windows.

F10 activates the bar; arrow keys navigate, Home/End select the first/last
enabled item, Enter/Space activate and Escape closes. A context target opens
on right-click, or Shift+F10 after focusing it. Disabled entries and separators
are skipped. These controls add no timer or periodic subscription.

`Tokens::from_dictionary` consumes resolved design colours and `radius.md`.
`Tokens::from_colours` uses those colours with a six-pixel radius. Both return
an error for missing required colours. Text input uses the base, muted and
accent pairs plus input/border/ring; menus use popover, muted and accent plus
border. Pair colours use the resolved rendered values. This adapter does not
extend the design compiler's closed family schema. `Tokens::default()` is a
standalone preview palette; applications should pass their resolved design.

From `src/desktop`, build the gallery on a build worker, then launch it only
inside the nested test compositor with its `WAYLAND_DISPLAY`:

```text
cargo run -p cosmix-iced-widgets --example gallery --features gallery-wgpu --locked
cargo run -p cosmix-iced-widgets --example gallery --features gallery-tiny-skia --locked
```

Use one renderer per comparison arm. The example uses iced's winit host, which
provides the IME bridge. Raw Wayland and compositor hosts must provide their
own input-method bridge. Runtime rendering, IME seat journeys and the idle
wakeup target are separate nested-harness checks, not claims made by unit tests.
