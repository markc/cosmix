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

1. `Surface::new(program, Settings)` with the physical size and scale.
2. Input: `cursor_moved` / `cursor_left` (logical points), `queue_event`
   for anything else. `input::*` converts evdev buttons, wheel axes and
   focus; `keys::*` (feature `xkb`) converts xkb keysyms.
3. `process()` when events are queued. It returns `needs_redraw`, the
   cursor shape and per-event capture status. With nothing queued it
   returns at once and asks for nothing.
4. `draw(buffer, width, height, stride, format)` on the next frame. The
   result lists the physical rectangles rewritten, whether the draw was a
   full repaint, and the `Requests` to act on: cursor shape, IME state and
   `Redraw` (`Wait`, `NextFrame`, or `At(instant)` for a one-shot timer).
   An empty damage list means the buffer did not change.

The crate starts no threads and no timers. A caret blink arrives as
`Redraw::At`; the host arms its own timer and calls `draw` then.

## Buffers and damage

- One persistent buffer per surface, tight rows (`stride == width * 4`).
  A new width or height repaints everything. After a lost or swapped
  buffer, call `invalidate()`.
- `PixelFormat::Argb8888` is wl_shm's little-endian B, G, R, A, which
  tiny-skia draws directly. `PixelFormat::Rgba8` swaps red and blue on the
  damaged rectangles only. Both are premultiplied.
- Damage is the layer diff `iced_tiny_skia`'s own compositor uses
  (`Layer::damage`, `damage::group`), rounded outward to whole physical
  pixels and merged until disjoint. Only those rectangles are cleared and
  redrawn, clipped by the tiny-skia mask.

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

The tests render into buffers and need no compositor. They load
`DejaVuSans.ttf` from `cosmix-comp`'s assets so text does not depend on
installed fonts.
