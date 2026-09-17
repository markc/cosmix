# cosmix-wl-app

`src/desktop/crates/cosmix-wl-app` is a small raw Wayland client runtime on
smithay-client-toolkit 0.19.2 and calloop 0.13. It has no widgets and draws
nothing: the app gets pixel buffers, input and popups. It is the planned base
of the raw-Wayland term (P2 of the term rebuild) and the Wayland half of
test B (iced chrome in a raw window).

## Shape

- `run(app)` connects, calls `App::init`, and runs the loop until
  `Ctx::exit`. It returns `Stats` (frames committed, buffer allocations,
  first-commit time).
- `App` has three callbacks: `init`, `event(Event)` and `draw(Frame)`.
  `Ctx` is the handle passed to all three.
- Surfaces are identified by `SurfaceId`. `Ctx::create_window` makes an
  `xdg_toplevel` that asks for server-side decorations. `Ctx::create_popup`
  makes an `xdg_popup` on a toplevel or another popup; `PopupSpec` holds the
  positioner and the grab flag. Popups can be repositioned and closed;
  closing a surface closes the popups above it first.

## Drawing and pacing

`draw` is called for a surface only when all of these hold: it is
configured, the app asked for a redraw (or a configure or scale change did),
and no frame callback is pending. `Frame::buffer_mut()` returns
`(pixels, width, height, stride)` at physical size, ARGB8888 in B, G, R, A
byte order. `Frame::commit_with_damage(&[Rect])` records physical damage;
the runtime then clips and merges it, sends `damage_buffer`, requests one
frame callback and commits. A draw that commits nothing leaves the surface
untouched. There are no timers, so an idle app has no wakeups. The only
timer is key repeat, and it is armed only while a key is held.

Each surface has up to three `wl_shm` slots. The runtime reuses the newest
committed slot once the compositor releases it, so a steady-state frame
copies nothing. If that slot is still held, it takes another free slot and
copies into it the damage committed since that slot was last current. It
allocates a slot only when every slot is held. `Frame::needs_full_redraw()`
is true when the buffer holds no usable contents (the first frame, or after
a resize or scale change).

## Scale

With `wp_fractional_scale_v1` and `wp_viewporter`, buffers are allocated at
`round(logical × scale)` and shown through a viewport at logical size.
Without them, the integer scale from `wl_surface` (preferred buffer scale, or
the output) becomes the buffer scale. `SurfaceInfo` carries the logical size,
the physical size and the `Scale`. A new popup starts at its parent's scale.

## Input

- **Keyboard:** xkbcommon, through sctk. `KeyEvent` has the keysym, the
  evdev code, the text (after compose), the modifiers and
  `Pressed`/`Released`/`Repeated`.
- **Pointer:** enter, leave, motion, button and axis. Axis events carry
  pixel values and 120ths per wheel step. sctk binds `wl_seat` v7, so the
  120ths are derived from `axis_discrete`; true `value120` is not available.
- **Cursor:** `Ctx::set_cursor` uses `wp_cursor_shape_v1` with the enter
  serial. There is no themed-cursor fallback.
- **IME:** `Ctx::set_ime(Some(ImeState))` enables text-input-v3 and updates
  it (cursor rectangle, content type, surrounding text); `None` disables
  it. Results arrive as `Event::Ime`, in protocol order: delete surrounding,
  commit, preedit. A batch without a preedit clears the preedit.
  `ime::ImeSerials` counts commits. It applies a `done` whose serial lies
  between the enabling commit and the latest commit, and it drops batches
  that arrived for an earlier focus.

## Clipboard

`Ctx::set_selection(Selection::Clipboard | Primary, text)` offers the text
with the latest input serial. `Ctx::request_selection` reads the current
selection through a non-blocking pipe in the loop; the result arrives as
`Event::SelectionText`. A selection this app still owns is answered
directly. Writes to other clients are also non-blocking loop sources.

## Other threads

`Ctx::waker()` returns a `Send` handle; `Waker::wake(token)` delivers
`Event::Wake(token)` on the loop thread.

## Not yet

These are not implemented yet: xdg-activation, touch, drag and drop,
multiple seats, SIGTERM handling, and a way for the app to add its own
calloop sources (use `Waker` from a thread for now).

## Demo

`src/desktop/apps/wl-iced-demo` (package `cosmix-wl-iced-demo`) builds
`wl-raw-demo`: a text grid drawn with swash, a right-click popup menu with a
nested submenu, inline IME preedit, and Ctrl+Shift+C / Ctrl+Shift+V
clipboard.

- `WL_DEMO_TRACE=1` logs notable events.
- `WL_DEMO_EXIT_AFTER=N` exits after N seconds.
- The demo prints its startup time and, on exit, the frame count.
- The iced twin, `wl-iced-demo`, is added to the same package.
