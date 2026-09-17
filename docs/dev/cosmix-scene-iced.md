# cosmix-scene-iced

A Mix Scenes adapter that draws a scene with a CPU renderer and shows it in
Bevy UI. The renderer is meant to be iced through `iced_tiny_skia`; until
`cosmix-iced-host` lands a stand-in draws a solid panel, a hover square and a
caret that blinks only while focused.

Spike crate (`spike/iced-in-comp`), test A of the one-GUI-toolkit ADR.

## Selecting the adapter

Quoin built with `--features scene-iced` registers the adapter `iced`.
`shell.scene.load` takes an `adapter` header (the body is the scene
document):

```mix
send shell shell.scene.load body=$doc adapter="iced"
```

A load without the header keeps the scene's current adapter; `adapter=bevy`
hands it back to CTK. Naming an adapter the binary was built without is an
error and changes nothing. `patch`, `get`, `watch` and `unload` work as for
CTK scenes.

Both adapters use the page id `scene-<name>`, and `mount_page` treats an
existing id as already mounted. The iced adapter therefore releases pages
before the CTK pass (`SceneReconcile`) and registers pages after it.

## Renderer seam

`surface::SurfaceRenderer` has no Bevy types:

- `resize(width, height, scale)`: physical pixels; the next draw repaints all;
- `set_scene(&ResolvedScene)`: a new accepted revision;
- `queue(SurfaceEvent)`: pointer (surface physical pixels), keys, focus, IME;
- `process(now) -> Processed`: `needs_redraw`, cursor shape, IME request
  (caret rectangle in surface pixels and purpose: normal, secure, terminal;
  the bridge forwards secure as the host's password purpose), `wake_at`;
- `draw(buffer, width, height, stride) -> Vec<Rect>`: paints only damage into
  the caller's persistent RGBA8 premultiplied buffer and returns it.

`SceneIcedFactory` (a non-send resource) builds one renderer per mounted
scene.

## Upload path

Each surface owns one texture: an uninitialised `Image`
(`RENDER_WORLD`, `Rgba8UnormSrgb`, nearest sampling) added to
`Assets<Image>` when the surface first gets a size. Textures are sized in
128 px buckets and the surface shows its top-left part through
`ImageNode.rect` (`NodeImageMode::Stretch`). A new texture is allocated only
when the surface outgrows it or needs less than a quarter of it in a
dimension; other size and scale changes count as `resizes` and repaint the
visible part. The handle is never mutably borrowed, so the
only asset events are one `Added` per allocation. Damage is clipped and
merged (`upload::plan`), copied out with alpha un-premultiplied (Bevy UI
blends straight alpha), passed to the render world at extract, and written
with `RenderQueue::write_texture` after `PrepareAssets`. The texture is
added before the frame's asset events (`AssetEventSystems`), so it is
extracted in the same update. An upload waits up to 120 frames for its
texture, requesting a redraw meanwhile so an idle host keeps updating; a
texture Bevy re-created underneath us triggers a full repaint. Those redraw
requests stop after 240 consecutive frames and rearm when an upload lands,
so a texture that never prepares leaves a blank surface instead of holding
the desktop at full frame rate. The `waiting` flag they read is a Relaxed
cross-world store, so it can be one frame stale under pipelined rendering. A surface that
cannot draw (no size yet) disables its IME target and keeps any repaint for
its next draw.

## Counters

`SceneIcedCounters` keeps per-frame and total counts of `AssetEvent<Image>`
Added/Modified (all images and surface textures), allocations, draws, and
rectangles and bytes queued (main world) and written (render world). Quoin
reports the snapshot in `shell.debug.status` under `scene_iced`. With
`COSMIX_SCENE_ICED_TRACE=1` a `SCENE_ICED_STATS` line is logged every 120
updates.

## Input, focus and IME

Pointer input is read from Bevy's `PointerInput` messages, routed to the
surface under the pointer (`HoverMap`) and captured from press to release,
per pointer. Positions are converted to surface pixels with the camera's
target scale, which is what Bevy's UI picking uses; the surface itself
renders at the UI target scale (window scale x `UiScale`). The two are equal
under Quoin and differ in comp, whose native shell already multiplies
`UiScale` into pointer positions. The capture window check needs the UI
camera to target `WindowRef::Entity`, as Quoin's panels do.
A press on a surface sets `InputFocus`; a press on no surface clears it if a
surface held it. `SceneIcedFocus` records the owning surface, its IME request
(caret in window-logical coordinates) and the hovered cursor shape. Keys and
IME input (`ExternalImeEvent`) go to the owner only. Keyboard input is
copied to the owner, not consumed: `ButtonInput<KeyCode>`, Quoin's and the
layer host's handlers and global shortcut systems still see every key; CTK
fields only act on keys while they hold `InputFocus`.

The owning surface carries `cosmix_shell::runtime::ExternalImeTarget`
(enabled, purpose, caret in window-logical coordinates). The layer host
enables text-input-v3 for it as for a focused `EditableText`, sends the
content type and the caret rectangle (origin rounded down, far edges rounded
up) and delivers `ExternalImeEvent`s (enabled, disabled, delete, commit,
preedit, in protocol order) under the same focus-generation and serial rules.
Commit and preedit reach the renderer; surrounding-text deletion is dropped
because no surrounding text is sent and iced 0.14 has no such event.

The hovered surface's cursor shape goes to `CursorShapeRequest`, which the
layer host applies with `wp_cursor_shape_v1` on change and after each pointer
enter. The request names its owner; leaving all surfaces resets it to the
default shape only if a surface still owns it.

A captured pointer that reports from another window is not given that
position: the surface loses hover, and a release there still ends the
capture.

`SceneIcedWake` is the earliest renderer wake time. The host registers what
to do with it on `SceneIcedWaker` (`set(|world, at| …)`, called every update
while a wake is pending, so a host that consumes its deadline re-arms):
Quoin folds it into `LayerHostDeadline` so a caret can blink on an otherwise
idle host, and a host that mounts the bridge in-process registers its own.

A natively mounted Quoin registers on the Bus as `shell`, or as
`COSMIX_NATIVE_SHELL_SERVICE`, or as `NativeQuoin::with_service(..)`. One
node holds one `shell`, so a second mount beside a live desktop must be
given another name.

## The iced renderer (feature `iced`)

`iced_scene::IcedRendererPlugin` replaces the stand-in factory with
`IcedSceneRenderer`: one `cosmix_iced_host::Surface` per mounted scene,
drawing RGBA8 with tiny-skia. Quoin's `scene-iced` feature selects it;
`scene-iced-standin` builds the bridge with the stand-in only.

Scene families map as the CTK adapter maps them:

| family | iced | notes |
|---|---|---|
| `column` | keyed column (children keyed by node id), stretch across | `gap`, `padding`, `fill` |
| `row` | row in a styled container, `mouse_area` for `hover`/`on_click` | `height`, `radius`, `background`, `align` |
| `text` | text, no wrapping, `bold` weight, `mono` family | `elide` clips; iced 0.14 has no middle elision |
| `field` | `cosmix-iced-widgets` `TextField` (design tokens), Enter wrapper for `on_submit` | `password` uses secure mode |
| `button` | button; `tone` primary / danger / secondary styles | |
| `toggle` | toggler | |
| `list` | scrollable keyed column of template instances, row click with `item` | `{cells[n]}` substitution, `max_rows`, `hidden_if_empty` |
| `spacer`, `window` | space | as CTK's sizes |
| `image` | space of `w` x `h` | not drawn: iced's image feature is off |

Handler ports produce `{scene, node, kind, value?, item?}` calls. The
renderer queues them; `IcedRendererPlugin` sends them with
`cosmix_scene_bevy::SceneEvents::send_handler`, the same request ids and
reply bookkeeping as CTK-mounted scenes.

Editing state lives in the iced widget tree and survives reloads and
patches that keep the node: focus, selection, undo history and an open
preedit. A changed `value` port replaces a field's text only while it is not
focused, as CTK does.

The look follows CTK: `CtkDesign`'s resolved dictionary gives the
`cosmix-iced-widgets` tokens, `CtkTypography` the family (effective, else
requested) and body size. A new renderer takes the look current at mount;
the text field keeps the renderer's default font from then on.

There are no font files in `cosmix-design`: both stacks resolve the
configured family from the system font set (fontique for Bevy, fontdb for
iced). The font test registers one file in both and compares family names.

`process` returns `wake_at = now` when it asks for a draw, because the next
deadline (caret blink) is known only after drawing; the following update
returns the real deadline or none.
