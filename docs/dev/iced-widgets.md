# Shared iced widgets

`cosmix-iced-widgets` 0.1.0 provides a single-line `TextField`, a menu bar,
context menus and a `cosmix-design` colour/metric adapter. It uses upstream
iced exactly 0.14.0, with defaults disabled and Wayland enabled. Neither
renderer is selected by the library's defaults. Hosts select `wgpu` or
`tiny-skia`; renderer-generic widgets can also be embedded in a host's UI.
Upstream iced requires a renderer feature for release builds.

## Public API (0.1.0)

This is the whole surface other crates may rely on. Everything else is private.

```rust
// Cargo features: `wgpu`, `tiny-skia` (pick one in the host; default neither),
// `gallery-wgpu`, `gallery-tiny-skia` (example only).

// Text field. Theme is iced::Theme; Renderer is generic over text::Renderer.
TextField::new(placeholder: &str, value: &str) -> TextField<'a, Message, Renderer>
    .on_input(impl Fn(String) -> Message)   // omit for a read-only field
    .secure(bool)                           // password presentation + IME purpose
    .id(impl Into<widget::Id>)              // for iced focus/select operations
    .width(..) .padding(..) .size(..)
    .style(impl Fn(&Theme, text_input::Status) -> text_input::Style)
// impl From<TextField> for Element<'a, Message, Theme, Renderer>

// Menus. Generic over any Theme; Message: Clone.
Item::action(label, message) | Item::submenu(label, Vec<Item>) | Item::separator()
    .accelerator(label)                     // display only
    .enabled(bool)
Menu::bar(Vec<Item<Message>>)               // full-width, row_height tall
Menu::context(content: impl Into<Element>, Vec<Item<Message>>)
    .style(MenuStyle)
MenuStyle { background, text, disabled, selected, selected_text, border,
            radius, text_size, row_height, padding }   // Default impl
// impl From<Menu> for Element<'a, Message, Theme, Renderer>

// Design tokens (cosmix-design stays iced-free).
Tokens::from_dictionary(&ResolvedDictionary) -> Result<Tokens, TokenError>
Tokens::from_colours(&ResolvedColours) -> Result<Tokens, TokenError>
Tokens::default()                           // preview palette only
tokens.text_input(status) -> text_input::Style
tokens.menu_style() -> MenuStyle
tokens::colour(LinearRgba) -> iced::Color   // linear -> encoded sRGB
Tokens { surface, text, popover, popover_text, muted_surface, muted_text,
         selection, selection_text, border, input, ring, radius }  // pub fields
```

Contracts a host must honour:

- **Controlled value.** Store every `on_input` value; pass it back to
  `TextField::new` on the next view. A value that differs from the last one
  the field emitted counts as an external replacement and clears history.
- **Stable identity.** History and menu state live in the iced widget tree.
  Rebuilding the view with the widget at a different tree position loses them.
- **Events.** Both widgets behave correctly when iced delivers several events
  in one `UserInterface::update` batch (undo then type, F10 then arrows,
  Escape then typing). A non-winit host must still deliver
  `Event::InputMethod`, `ModifiersChanged` and `window::Event::Unfocused`.
- **Overlays.** Menus draw through iced's `overlay` path inside the host
  surface, so a host must draw and route `UserInterface` overlays. A menu is
  always present as an inert overlay; it returns `mouse::Interaction::None`
  while closed, so it does not steal the pointer from the base layer.
- **Modal while open.** An open menu captures every key press and IME
  preedit/commit, so neither the wrapped content nor `keyboard::listen`
  subscriptions see them. Modifier, IME open/close and window events still
  reach the content.
- **iced internals.** The batch handling relies on private behaviour of
  `iced_runtime` 0.14 (`UserInterface::update` drops the rest of a batch when
  an overlay disappears). The lock pins `iced_runtime`, `iced_widget` and the
  other `iced_*` crates; any change to them must pass this crate's
  `runtime_*` tests first.
- **Known limit.** iced 0.14 has no focus-change event. A context target with
  no focusable child remembers a click as focus until the next press, so if
  keyboard focus then moves into another context target's field, Shift+F10
  can open the clicked one instead.
- **Popup clamping** uses the overlay bounds the host passes to
  `UserInterface::build`, so a popup never leaves the surface.

## Behaviour

The text field wraps iced's `text_input`: selection, clipboard, placeholder,
password presentation and IME remain in the upstream widget. Focused fields
handle Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y (resolved like iced's clipboard
shortcuts, so they work by key position on non-Latin layouts). Undo is off
while an IME composition is open. A focused, editable field always consumes
these keys, even with nothing to undo, as native entries do; an app-level undo
binding fires only when no field has focus. Applications own the string and apply
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
The wrapped upstream text input does schedule 500 ms caret redraws while
focused with a collapsed selection. Measure focused and unfocused idle
separately; preserving that behaviour does not establish the < 1 wakeup/s
target for a focused field.

`Tokens::from_dictionary` consumes resolved design colours and `radius.md`.
`Tokens::from_colours` uses those colours with a six-pixel radius. Both return
an error for missing required colours. Text input uses the base, muted and
accent pairs plus input/border/ring; menus use popover, muted and accent plus
border. Pair colours use the resolved rendered values. This adapter does not
extend the design compiler's closed family schema. `Tokens::default()` is a
standalone preview palette; applications should pass their resolved design.

From `src/desktop`, build the gallery on a build worker:

```text
cargo build -p cosmix-iced-widgets --example gallery --features gallery-wgpu --locked
cargo build -p cosmix-iced-widgets --example gallery --features gallery-tiny-skia --locked
```

Preserve each arm's `debug/examples/gallery` executable from the worker's Cargo
target directory separately. Transfer the executables to the nested harness
and launch each with that compositor's `WAYLAND_DISPLAY`, never the live seat.
No local rebuild is needed. The `gallery` feature alone intentionally refuses
to compile; choose one of the two renderer-specific gallery features above.

Use one renderer per comparison arm. The example uses iced's winit host, which
provides the IME bridge. Raw Wayland and compositor hosts must provide their
own input-method bridge. Runtime rendering, IME seat journeys and the idle
wakeup target are separate nested-harness checks, not claims made by unit tests.
