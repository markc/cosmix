# cosmix-iced-host

`src/desktop/crates/cosmix-iced-host` runs one iced 0.14 program without
winit and rasterises it with `iced_tiny_skia` into a CPU buffer the caller
owns. It is the shared core for a raw Wayland client (wl_shm) and for a
Bevy bridge that uploads damage rectangles into a persistent texture. It
knows neither.

Dependencies are the iced component crates pinned exactly (`iced_core`,
`iced_runtime`, `iced_graphics`, `iced_renderer` with only `tiny-skia`,
`iced_tiny_skia` without default features, `iced_widget`). The `iced`
umbrella crate is not used because it always depends on `iced_winit`. The
graph contains no winit and no wgpu. `iced_tiny_skia` always compiles
`softbuffer`, which does not build on Linux without a backend, so its
dlopen'd Wayland backend is enabled (`wayland-client` joins the graph,
libwayland is not linked). This crate never calls softbuffer.

## Driving a surface

1. `Surface::new(program, Settings)` with the physical size and scale (a
   scale that is not finite and positive becomes 1.0; `resize` keeps the
   current one instead). A surface is not `Send`: keep it on its thread.
2. Input: `cursor_moved` / `cursor_left` (logical points), `queue_event`
   for anything else. Hosts must deliver `Event::InputMethod`
   (Opened/Preedit/Commit/Closed), `keyboard::Event::ModifiersChanged` and
   window `Focused`/`Unfocused` (`input::focus_event`); text fields read all
   three. `input::*` converts evdev buttons and wheel axes; `keys::*`
   (feature `xkb`) converts xkb keysyms.
3. `process()` when events are queued. Every event queued since the last
   call goes through one `UserInterface::update`, on a tree rebuilt from
   `view()` with the retained cache. It returns `needs_redraw`, the cursor
   shape, per-event capture status and `redraw`: `NextFrame` when a draw
   is due, otherwise the pending deadline (a focused field's 500 ms caret
   blink, `At`) or `Wait`. With nothing queued it builds nothing, but still
   reports pending work: a deadline that has passed, `program_mut`,
   `operate`, `invalidate`, `set_background`. `process_at(now)` takes the
   time; `needs_redraw()` asks without processing.

   A rebuilt tree has lost the widgets' remembered status, so `process`
   first replays the last draw's redraw pass (its time and cursor) before
   the events. Time-based widgets see no elapsed time in that pass; a
   widget that counts redraw passes sees one extra. Messages from the
   replay are applied before the events, as iced_winit does.
4. `draw(buffer, width, height, stride, format)` on the next frame. The
   result lists the physical rectangles rewritten, whether the draw was a
   full repaint, and the `Requests` to act on: cursor shape, IME state and
   `Redraw` (`Wait`, `NextFrame`, or `At(instant)` for a one-shot timer).
   An empty damage list means the buffer did not change.

The crate starts no threads and no timers. A caret blink arrives as
`Redraw::At`; the host arms one one-shot timer and calls `draw` then.

Overlays (menus, pick lists) take part in both `process` and `draw` and are
drawn inside the surface. For `xdg_popup`, run one `Surface` per popup:
instances are independent and share iced's global font system, so an extra
instance costs its widget state, layer list and tiny-skia glyph cache.

## Buffers and damage

- One persistent buffer per surface. Rows may be padded (`stride >= width * 4`,
  whole pixels, every row `stride` long, as in a texture wider than the
  surface); padding is never written. A new width or height repaints everything. After a lost or swapped
  buffer, call `invalidate()`.
- `PixelFormat::Argb8888` is wl_shm's little-endian B, G, R, A, which
  tiny-skia draws directly. `PixelFormat::Rgba8` swaps red and blue on the
  damaged rectangles only. Both are premultiplied.
- Damage is computed per primitive (`src/diff.rs`), not with
  `iced_tiny_skia`'s `Layer::damage`. That one pairs primitives by index,
  so a caret quad blinking off misaligned the rest of its layer, and it
  treats live primitives as always changed. Here each layer's quads, text
  items, primitives and images are matched by a longest common subsequence,
  and only unmatched items' old and new bounds are damaged (text expanded
  by 2 logical px for ink overhang, quads by 1). Paragraphs are compared
  through a per-frame snapshot of their lines, attributes, metrics and
  layout size: the weak references a layer holds are detached whenever a
  widget re-lays out a paragraph, even if nothing visible changed.
  Rectangles are rounded outward to physical pixels, merged where the union
  wastes at most 4096 pixels (collapsed to one box past 24), and made
  disjoint. Resize, scale, theme, background and `invalidate()` still
  repaint everything.
- A caret blink damages about the caret (3×21 px at scale 1.0, 8×51 px at
  2.5 in the tests); a one-character edit damages the field's text run and
  any text that echoes it.
- Partial redraws match a full redraw except for occasional one-level
  channel differences on antialiased edges. A quad cut by a damage
  rectangle is drawn through tiny-skia's masked pipeline, which rounds
  differently from the unmasked path used when the quad lies wholly
  inside. `tests/damage.rs` checks damage coverage exactly (every pixel
  whose full redraw changed lies in the damage) and bounds the rest to one
  level.

## IME

`Requests::ime` is `Disabled` or `Enabled` with the caret rectangle in
logical and physical coordinates, the purpose, and the preedit the widget
holds. `Frame::ime_changed` says when to send text-input-v3 requests.
Input method events go in as `Event::InputMethod`. The preedit is drawn under
the caret, as iced_winit does, unless `Settings::draw_preedit` is off.

## Fonts

`load_font` / `load_font_file` feed iced's single global cosmic-text font
system. Load fonts before building surfaces. On first use the font system
scans the installed system fonts, which iced 0.14's public API cannot
disable; the cost is face metadata for every installed face plus the
memory-mapped pages glyph shaping touches. `loaded_face_count()` reports the
database size. Memory is not measured here.

## Limits

- `Program::update` returns no `Task`; widget operations go through
  `Surface::operate`.
- The widget tree is rebuilt on every `process`, `draw` and `operate`
  (`view()` plus layout), because the program cannot be borrowed across
  calls.
- No image or SVG support (features off), no touch helpers, big-endian
  hosts unsupported for `Argb8888`.

## Gates

```
cargo build  --manifest-path desktop/Cargo.toml --release -p cosmix-iced-host --all-features --locked
cargo clippy --manifest-path desktop/Cargo.toml --release -p cosmix-iced-host --all-targets --all-features --locked -- -D warnings
cargo test   --manifest-path desktop/Cargo.toml --release -p cosmix-iced-host --all-features --locked
```

Run from `src/` (the build workers' working directory); from the repository
root the manifest is `src/desktop/Cargo.toml`. The tests render into
buffers and need no compositor. They load
`DejaVuSans.ttf` from `cosmix-comp`'s assets so text does not depend on
installed fonts.
