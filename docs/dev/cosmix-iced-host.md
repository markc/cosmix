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
4. `draw(buffer, width, height, stride, format)` on the next frame, or
   `draw_aged(.., age)` when the buffer is older than the last frame. The
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
- Two things the renderer paints without the clip mask are added to the
  damage, or they would be drawn outside it (accumulating shadow, and in
  `Rgba8` bytes left unswapped): a quad's **shadow**, drawn whole whenever
  the quad body meets the damage, and the **ink of text that lies wholly
  inside** the damage, which can overhang its measured bounds. Text that
  crosses a damage edge is clipped, so only fully covered text is expanded;
  the margin is a quarter of the run's line height, at least 2 px, since ink
  overhangs more the larger the text. A shadow whose quad body meets two
  damage rectangles would be blended twice, so those rectangles are merged.
  (Live primitives and images take the same mask-skip path, but this crate
  builds with `image`, `svg` and `geometry` off, so no layer can hold any.)
  A shadow spanning the surface turns every partial redraw into a full one:
  keep shadows off large containers, or draw them yourself.
- Rounding, coalescing, the bounding-box collapse and an older buffer's
  history only ever grow rectangles, and the renderer applies its mask-skip
  rule to the final list, so growing can reach a shadow or text run that was
  not reached before. Growth and expansion therefore run to a fixpoint: each
  candidate is added at most once, so it ends.
- `draw_aged(.., age)` (and `draw_aged_at`) draws into a buffer that is
  `age` **draws** old — draws of this surface, not compositor commits, so a
  host that skips a commit must still count the draw. 1 is the buffer of the
  last draw (what `draw` assumes), `n` one holding the contents of `n` draws
  ago, and 0 unknown contents. The damage of the draws in between is added
  from an eight-draw history, so a client cycling `wl_shm` buffers does not
  repaint everything. An age past the history repaints everything, as does a
  resize, which drops the history. (`cosmix-wl-app` does not need this: its
  pool copies damage forward, so every buffer it hands over is one draw
  old.)
- A buffer of the same size but a different `stride` or `PixelFormat` is
  not the previous frame's buffer, and repaints everything.
- Outside the damage a partial redraw matches a full one exactly. Inside it,
  a repainted pixel can land a few channel levels off: a quad or glyph cut
  by a damage rectangle goes through tiny-skia's masked pipeline, which
  rounds coverage differently from the unmasked path taken when it lies
  wholly inside. `tests/damage.rs` checks all three (coverage exactly, no
  difference outside the damage, at most four levels inside) over blinks,
  edits and 240 random small changes; `tests/shadows.rs` drives stacked
  shadowed cards, a twelve-card chain and three rotating buffers in both
  pixel formats, and there every frame matches byte for byte.
- The per-frame snapshot clones the text of every run on screen (its lines
  and attributes) so paragraphs can be compared by content. That is a copy
  of the visible text per draw: fine for chrome and furniture, worth
  measuring before hosting a wall of text.

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
