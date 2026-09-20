# Shared iced widgets

`cosmix-iced-widgets` 0.1.0 provides a single-line `TextField`, a menu bar,
context menus, pro-audio controls (fader, pan knob, level meter, toggle), a
waveform and a piano roll, and a `cosmix-design` colour/metric adapter. It
uses upstream iced 0.14 component crates, pinned exactly, with defaults
disabled and no window shell: the library links no winit, so raw Wayland and
compositor hosts can embed it. Neither renderer is selected by default. Hosts
select `wgpu` or `tiny-skia`. Upstream iced_renderer requires a renderer
feature for release builds. Only the gallery example uses iced's winit shell
(Wayland-only).

## Public API (0.1.0)

This is the whole surface other crates may rely on. Everything else is private.

```rust
// Dependencies: iced_core, iced_widget (feature `advanced`), iced_graphics
// (`geometry`) and iced_renderer, pinned exactly. Never the `iced` umbrella,
// which always links iced_winit/winit: `cargo tree -p cosmix-iced-widgets
// -e normal` shows no winit, and tests/feature_graph.rs enforces it.
// Cargo features: `wgpu`, `tiny-skia` (enable that backend's geometry
// support; pick one in the host; default neither), `gallery-wgpu`,
// `gallery-tiny-skia` (example only; these pull in the umbrella).

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
    .external_popups(impl Fn(MenuState) -> Message)  // no overlay; see below
    .state(&MenuState)                      // host-driven state (external mode)
MenuStyle { background, text, disabled, selected, selected_text, border,
            radius, text_size, row_height, padding }   // Default impl
// impl From<Menu> for Element<'a, Message, Theme, Renderer>
item.label() .accelerator_label() .is_enabled() .is_separator()
    .children() .message()                  // read-only, for hosts

// Menus on external surfaces (xdg_popup). Message: Clone.
MenuState { root: Option<usize>, path: Vec<Option<usize>>,
            anchors: Vec<Rectangle> }       // Default = closed
    .is_open()
Navigator::bar(&items) | Navigator::context(&items)   // Copy
    .panel(&state, level) -> &[Item]        // items shown in panel `level`
    .open(&mut state, root, keyboard) .close(&mut state)
    .key(&mut state, &keyboard::Key)        // arrows, Home/End, Enter/Space, Escape, Tab
    .hover(&mut state, level, Option<row>)  // None: pointer on no row
    .click(&mut state, level, Option<row>)  // None: press outside every panel
    .hover_root(&mut state, index) .click_root(&mut state, index)   // bar titles
    .validate(&mut state)                   // close if the items changed under it
    .open_panels(&state) -> Vec<PanelSpec { level, items, selected, anchor }>
state.anchor(level) -> Option<Rectangle>
    // each returns NavOutcome::{None, Changed, Activated(Message), Closed}
menu::Panel::new(&items, selected: Option<usize>)   // one panel, fills its surface
    .style(MenuStyle)
    .on_hover(impl Fn(Option<usize>) -> Message)    // row under the pointer changed
    .on_press(impl Fn(usize) -> Message)            // left press or touch on a row
menu::panel_size(&renderer, &items, style) -> Size  // popup surface size
menu::row_bounds(&items, row, panel_width, style) -> Option<Rectangle>  // panel-local
menu::row_at(&items, y, style) -> Option<usize>
menu::{MIN_PANEL_WIDTH, SEPARATOR_HEIGHT}

// Design tokens (cosmix-design stays iced-free).
Tokens::from_dictionary(&ResolvedDictionary) -> Result<Tokens, TokenError>
Tokens::from_colours(&ResolvedColours) -> Result<Tokens, TokenError>
Tokens::default()                           // preview palette only
tokens.text_input(status) -> text_input::Style
tokens.menu_style() -> MenuStyle
tokens.audio_style() -> AudioStyle
tokens::colour(LinearRgba) -> iced::Color   // linear -> encoded sRGB
Tokens { surface, text, popover, popover_text, card, card_text, primary,
         primary_text, destructive, destructive_text, muted_surface,
         muted_text, selection, selection_text, border, input, ring,
         radius }                            // pub fields

// Pro-audio controls. All are controlled: store each published value and
// pass it back. Generic over Theme; Fader and Knob need Message: Clone.
scale::Taper::DEFAULT                       // -inf..=+6 dB <-> 0..=1, 0 dB at 0.8
scale::Taper::new(&[(position, db), …])     // a host curve; unusable points
    .position(db) .db(position) .floor_db() .max_db() .points()   // fall back
scale::{db_to_position, position_to_db, format_db, FLOOR_DB, MAX_DB, DEFAULT_POINTS}
Fader::new(value_db: f32)                   // vertical, 28 x 160 by default
    .on_change(impl Fn(f32) -> Message)     // dB; NEG_INFINITY at the bottom
    .on_release(Message)                    // once per gesture
    .default_db(f32)                        // double-click value, default 0
    .width(f32) .height(impl Into<Length>) .style(AudioStyle)
    .taper(Taper)                           // curve for travel + unity tick
Knob::new(value: f32)                       // pan, -1..=1, 28 px
    .on_change(impl Fn(f32) -> Message) .on_release(Message)
    .size(f32) .style(AudioStyle)
LevelMeter::new(level_db: f32)              // 8 x 160, same scale as Fader
    .peak(Option<f32>) .hold(Option<f32>)   // the host's own peak and hold
    .clipped(bool)                          // the host's clip latch
    .width(f32) .height(impl Into<Length>) .style(AudioStyle) .taper(Taper)
    // meter::{PEAK_HOLD, PEAK_FALL_DB_PER_SEC, PEAK_FRAME, HIGH_DB, CLIP_DB}
Toggle::new(label, on: bool)                // mute/solo, 24 x 20
    .on_toggle(impl Fn(bool) -> Message)
    .alert(bool)                            // alert (mute) vs active (solo) colour
    .size(width, height) .style(AudioStyle)

// Canvases. Renderer: advanced::graphics::geometry::Renderer + 'static.
WaveformPeaks::from_samples(&[f32], samples_per_bucket)
WaveformPeaks::from_min_max(impl IntoIterator<Item = (f32, f32)>)
    .len() .is_empty()
Waveform::new(&WaveformPeaks)               // fill x 64
    .playhead(Option<f32>)                  // 0..=1
    .on_seek(impl Fn(f32) -> Message)       // 0..=1 under a left press
    .width(..) .height(..) .style(AudioStyle)
Note { start: f32, length: f32, pitch: u8, velocity: u8, track: u16 }  // beats
RollNotes::new(Vec<Note>)                   // sorts; drops invalid notes
    .notes() .len() .is_empty() .end_beat()
    .visible(from, to) -> impl Iterator<Item = (usize, &Note)>
RollView { scroll_beats, scroll_y, pixels_per_beat, row_height }  // Default
    .x_of(beat) .beat_at(x) .y_of(pitch) .pitch_at(y)
    .zoomed(factor, anchor_x) .scrolled(dx, dy, viewport_height, end_beat)
    .note_at(&RollNotes, Point) .is_valid()
PianoRoll::new(&RollNotes, RollView)        // fill x fill
    .playhead(Option<f32>)                  // beats
    .on_view(impl Fn(RollView) -> Message)  // wheel, Shift+wheel, Ctrl+wheel zoom
    .on_note(impl Fn(usize) -> Message)     // index into RollNotes::notes()
    .track_colours(&[Color])                // Note::track picks one, wrapping;
                                            // empty = AudioStyle::note
    .width(..) .height(..) .style(AudioStyle)
    // piano_roll::{TILE_WIDTH, MAX_TILES, MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT}
AudioStyle { background, track, fill, thumb, text, muted_text, border,
             meter_low, meter_high, meter_clip, peak, active, active_text,
             alert, alert_text, grid, lane, note, waveform, playhead,
             radius }                       // Default = Tokens::default()
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
- **External popups.** `Menu::bar(items).external_popups(Msg::Menu)` draws
  the bar only. Every change to the open state is published as a
  `MenuState` whose `anchors` are filled in: `anchors[0]` is the open title
  in the bar widget's window coordinates (a 1 x 1 rectangle at the pointer
  for a context menu), `anchors[n]` the parent row inside panel `n - 1`,
  relative to that panel. Store it exactly as received and pass it back with
  `.state(&state)` every view. This is required: without it the host cannot
  close the menu, and a host that edits or drops anchors makes the bar
  republish on every event. For each `spec` in
  `navigator.open_panels(&state)`, show a popup of
  `panel_size(renderer, spec.items, style)` anchored at `spec.anchor`,
  containing `Panel::new(spec.items, spec.selected)`. `open_panels` skips
  panels with no rows (a popup cannot have zero size). It also skips levels
  whose anchor is not known yet: a navigator step that adds or changes a
  level drops the stale anchors, and the bar refills them on its next event,
  which is the redraw after your update. Map a panel's `on_hover(row)` to
  `navigator.hover(&mut state, spec.level, row)`, its `on_press(row)` to
  `navigator.click(&mut state, spec.level, Some(row))`, and keys on a popup
  surface to `navigator.key`. Known limits:
  - `panel_size` of an empty item list has height 0, so never size a
    popup from it directly; use `open_panels`.
  - `Panel` forgets which hover it last reported whenever its item slice
    moves, which happens on every view that rebuilds the items. The only
    cost is one extra hover message on the next pointer motion.
  - A context menu rebuilt from host state reopens at `anchors[0]`, which
    is off by the scroll offset inside a scrollable.
  - A finger moving over bar titles does not switch menus in external
    mode. Publish the
  message of `NavOutcome::Activated`. Map a compositor dismissal
  (`popup_done`) to `navigator.close`. Pass the new state back to the bar,
  which republishes it with fresh anchors. The bar itself still handles
  F10, title clicks and title hover, keys that reach its surface, and
  presses outside (which close the menu). It does not close on window
  unfocus, because a popup taking keyboard focus is expected. The
  in-surface overlay mode uses the same `Navigator`, so both modes behave
  the same.
- **Pro-audio gestures.** Fader and knob drags are relative (a press never
  jumps the value). Shift divides travel by ten and rebases, so toggling it
  mid-drag never jumps. A double-click resets (fader: `default_db`, knob: 0).
  Toggles flip on press. Each change is published at once; hosts that record
  automation or undo should group on `on_release`.
- **Gain taper.** `Fader` and `LevelMeter` share one curve. Give both the
  same `Taper` and their scales line up: the fader's travel and unity tick,
  the meter's zone boundaries (-12 dB and -3 dB), its markers, and the floor
  below which nothing is drawn, all follow it. Points that do not increase in
  both coordinates fall back to the default curve rather than drawing a
  nonsense scale.
- **Meter state.** The widget always keeps its own falling peak line. A host
  with its own meter feed adds `peak`, `hold` and `clipped`: peak and hold
  draw as lines (peak colour, text colour), and the clip latch as a block at
  the top in the clip colour, until the host clears it.
- **Idle is zero redraws.** No widget subscribes to time. Only `LevelMeter`
  schedules redraws, and only while its peak line is above the level: it
  waits for `PEAK_HOLD`, then redraws every `PEAK_FRAME` until the line
  lands. A silent or steady meter schedules nothing. A host animating meters
  re-renders at its own meter rate.
- **Canvas caching.** `Waveform` tessellates its body once per peaks value,
  size and style (build `WaveformPeaks` once; a clone keeps its identity).
  `PianoRoll` draws 512 px tiles cached per zoom level (pixels per beat and
  row height), keeps up to 64 tiles across zoom levels, and scrolling only
  translates them. Only notes overlapping the view are visited, found by
  binary search on start time. Sub-pixel notes collapse to one rectangle per
  pixel per row, so a 131k-note song zoomed right out stays bounded. A new
  `RollNotes` value (or style) drops the tiles. Grid, notes and playhead are
  separate layers; moving the playhead touches no note geometry.
- **Popup clamping** uses the overlay bounds the host passes to
  `UserInterface::build`, so a popup never leaves the surface.

## Behaviour

The mixer + roll benchmark host (`cosmix-bench-mixer-iced` 0.1.1) mounts
its zero-sized feed clock with `Stack::from_vec`. iced 0.14's `stack!`,
`with_children` and `push` discard children with void size hints, preventing
such a clock from receiving redraw events. The retained clock requests
continuous redraws for the first second, then wakes at feed tick boundaries
(30 Hz) in animated modes and requests no further redraws in idle mode.
This uses widget redraw requests without an async timer runtime.

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
