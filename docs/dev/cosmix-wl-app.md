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
  `Ctx` is the handle passed to all three. `Event` is `#[non_exhaustive]`.
- Surfaces are identified by `SurfaceId`. `Ctx::create_window` makes an
  `xdg_toplevel` that asks for server-side decorations; decorations stay
  server-side, the runtime draws none. `Ctx::create_popup` makes an
  `xdg_popup` on a toplevel or another popup; `PopupSpec` holds the
  positioner and the grab flag. The id is returned at once, but the
  `xdg_popup` is only created once the parent has shown a buffer, so a
  submenu asked for before its parent menu is configured (F10 then Right)
  still opens. Popups can be repositioned and closed; closing a surface
  closes the popups above it first.
- **Grab serials.** A grab must carry a serial the compositor still counts
  as a live input action: a button press that started the current pointer
  grab, or the latest key press. Releases, enters and motion never qualify,
  and a press inside the app's own popups does not start a new pointer grab
  while the popup grab holds. So the first grabbing popup takes the latest
  press serial, and every popup opened while that chain is open (a
  submenu, or the next root menu when the pointer moves along the bar)
  reuses it. The chain ends when the compositor dismisses it or when an
  iteration ends with no grabbing popup open (`serial.rs`).

## Drawing and pacing

`draw` is called for a surface only when all of these hold: it is
configured, the app asked for a redraw (or a configure or scale change did),
no frame callback is pending, and it has a buffer to draw into. After each
loop dispatch the runtime draws in passes until no surface is left in that
state, so a redraw requested by an event that a draw caused (an IME focus
event, say) is drawn at once rather than lost; after eight passes it pings
the loop and continues on the next iteration. `Frame::buffer_mut()` returns
`(pixels, width, height, stride)` at physical size, ARGB8888 in B, G, R, A
byte order. `Frame::commit_with_damage(&[Rect])` records physical damage;
the runtime then clips and merges it, sends `damage_buffer` and commits. A
frame that `needs_full_redraw` is always committed with full damage.

Pacing is per surface (`Ctx::set_frame_pacing`). The default, `OnDemand`,
requests a frame callback only if the app asked for another redraw during
the draw (an animation), so a one-off change costs no callback wakeup; the
flip side is that redraws requested between frames are not throttled to the
display rate. `Always` requests a callback after every commit, so a stream
of changes (terminal output) draws at most once per display frame.

A draw that commits nothing leaves the surface as it was. If the app
requested another redraw during such a draw, the runtime commits a frame
callback without a buffer (or, before the surface is mapped, retries after
16 ms) instead of spinning. If the app took the buffer with
`Frame::buffer_mut` and did not commit, the slot's contents count as unknown
and the next frame is a full one, unless it called `Frame::keep_contents`.

The runtime's own timers are all armed only while needed: key repeat while
a key is held, a selection read's timeout while it runs, and a retry timer
after a failed buffer allocation (250 ms) or while the first frame waits
for its scale.

Each surface has up to three `wl_shm` slots. The runtime reuses the newest
committed slot once the compositor releases it, so a steady-state frame
copies nothing. If that slot is still held, it takes another free slot and
copies into it the damage committed since that slot was last current. It
allocates a slot only when every slot is held; if all three are held, the
surface waits for the compositor's release (which wakes the loop). A failed
allocation is logged and retried on a timer.

On close, free buffers are destroyed at once; a buffer the compositor still
holds is destroyed when it is released, which the compositor does when the
`wl_surface` is destroyed. The surface's buffers go first, then the scale
objects, then the role objects and the `wl_surface`. `Frame::needs_full_redraw()`
is true when the buffer holds no usable contents (the first frame, or after
a resize or scale change).

## Scale

With `wp_fractional_scale_v1` and `wp_viewporter`, buffers are allocated at
`round(logical × scale)` and shown through a viewport at logical size.
Without them, the integer scale from `wl_surface` (preferred buffer scale, or
the output) becomes the buffer scale. `SurfaceInfo` carries the logical size,
the physical size and the `Scale`. A new popup starts at its parent's scale.
The first window, when no scale has been seen yet, holds its first draw for
up to 50 ms waiting for `preferred_scale`. cosmix-comp sends it as soon as
the fractional-scale object is created, so there the first frame is drawn
at the right scale with no wait; a compositor that only sends it after the
surface is mapped gets a first frame at 1.0 followed by a full redraw.

## Input

- **Keyboard:** xkbcommon, through sctk. `KeyEvent` has the keysym, the
  base keysym (level 0 of the key in the active layout), the evdev code,
  the text (after compose), the modifiers, the modifiers xkb consumed to
  produce the keysym, and `Pressed`/`Released`/`Repeated`. The runtime
  keeps its own xkb state beside sctk's, fed the raw modifier masks, for the
  base keysym and consumed modifiers. Repeats are translated with the
  modifiers held at repeat time, as X autorepeat and sctk's own repeat do:
  hold `a`, press Shift, and the repeats read `A`. Keysym, text and
  modifiers of a repeat always agree; compose is not applied to repeats.
- **Pointer:** enter, leave, motion, button and axis. Axis events carry
  pixel values and 120ths per wheel step. sctk binds `wl_seat` v7, so the
  120ths are derived from `axis_discrete`; true `value120` is not available.
- **Cursor:** `Ctx::set_cursor` uses `wp_cursor_shape_v1` with the enter
  serial. There is no themed-cursor fallback.
- **IME:** `Ctx::set_ime(Some(ImeState))` enables text-input-v3 and updates
  it (cursor rectangle, content type, surrounding text; going from some
  surrounding text to none sends empty text); `None` disables it. Results
  arrive as `Event::Ime`, in protocol order: delete surrounding, commit,
  preedit. A batch without a preedit clears the preedit.
  `ime::ImeSerials` counts commits. It applies a `done` whose serial lies
  between the enabling commit and the latest commit, and it drops batches
  that arrived for an earlier focus.
- **IME owners.** `ImeState::target` is an app-chosen token for who owns
  the input (a grid, a text field) when several share one `wl_surface`.
  Changing it sends disable + commit, then enable + commit, which starts a
  new generation: a batch still in flight for the old owner is dropped,
  and a preedit it was showing is cleared with an event for the old owner.
  Every `Event::Ime` carries the target of the generation it belongs to,
  so the app routes by target, not by its own idea of who has focus.

## Clipboard

`Ctx::set_selection(Selection::Clipboard | Primary, text)` offers the text
with the latest input serial. `Ctx::request_selection` returns a token and
reads the current selection through a non-blocking pipe in the loop; the
result arrives as `Event::SelectionText` with that token and a
`ReadStatus`. A selection this app still owns is answered directly. A read
that has not finished after two seconds is cancelled and reported as
`TimedOut`. A read during which the selection changed is reported as
`Superseded` with no text, because what arrived may be cut short. Writes to
other clients are also non-blocking loop sources.

Offered and accepted types: `text/plain;charset=utf-8`, `UTF8_STRING` and
`text/plain` carry UTF-8. `STRING` is ISO 8859-1: written with `?` for
characters outside it, read byte for byte. `TEXT` has no fixed encoding; it
is written as UTF-8 and read as UTF-8, falling back to ISO 8859-1, as is
`text/plain`.

## Timers and other threads

`Ctx::set_timer(token, instant)` arms a one-shot loop timer that arrives as
`Event::Timer(token)`; `cancel_timer` removes it. A timer exists only while
armed, so an app that arms one only for a pending deadline (a caret blink)
stays idle otherwise. `Ctx::waker()` returns a `Send` handle;
`Waker::wake(token)` delivers `Event::Wake(token)` on the loop thread.

`Event::SelectionChanged` reports that another client set or cleared a
selection, for apps that keep a synchronous clipboard cache. Such an app
should fetch on every change and keep only the answer to its newest fetch
(the iced demo's `iced/clipboard.rs` does).

`Ctx::insert_source(source, callback)` adds any calloop source (a pty fd
through `calloop::generic::Generic`) to the loop; `cosmix_wl_app::calloop`
re-exports the pinned calloop. The callback gets a `Ctx`. To hand data to
the app, keep it in shared state and call `Ctx::notify(token)`, which
delivers `Event::Wake(token)` when the callback returns.
`Ctx::remove_source` removes it.

## Not yet

These are not implemented yet: xdg-activation, touch, drag and drop,
multiple seats, SIGTERM handling, and client-side decorations.

## Demo

`src/desktop/apps/wl-iced-demo` (package `cosmix-wl-iced-demo`) builds
`wl-raw-demo`: a text grid drawn with swash, a right-click popup menu with a
nested submenu, inline IME preedit, and Ctrl+Shift+C / Ctrl+Shift+V
clipboard.

- `WL_DEMO_TRACE=1` logs notable events.
- `WL_DEMO_EXIT_AFTER=N` exits after N seconds.
- The demo prints its startup time and, on exit, the frame count.
- `wl-iced-demo` (feature `iced`, built in its own cargo invocation) draws
  the same grid under iced chrome from `cosmix-iced-host`: a tab bar, a menu
  bar and a search `TextField` from `cosmix-iced-widgets`, rasterised into the
  same buffer, with grid and chrome damage merged into one commit. Menu
  panels are `xdg_popup`s, each with its own iced `Surface`; the state
  machine is `menus.rs` (pure), the popup reconciler `iced/popups.rs`. The
  panel program is a local stand-in until `cosmix-iced-widgets` exposes a
  standalone panel and its menu state.
