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
  (caret rectangle in surface pixels), `wake_at`;
- `draw(buffer, width, height, stride) -> Vec<Rect>`: paints only damage into
  the caller's persistent RGBA8 premultiplied buffer and returns it.

`SceneIcedFactory` (a non-send resource) builds one renderer per mounted
scene.

## Upload path

Each surface owns one texture: an uninitialised `Image`
(`RENDER_WORLD`, `Rgba8UnormSrgb`, nearest sampling) added to
`Assets<Image>` when the surface first gets a size and again only when the
physical size or scale changes. The handle is never mutably borrowed, so the
only asset events are one `Added` per allocation. Damage is clipped and
merged (`upload::plan`), copied out with alpha un-premultiplied (Bevy UI
blends straight alpha), passed to the render world at extract, and written
with `RenderQueue::write_texture` after `PrepareAssets`. An upload waits up
to 120 frames for its texture; a texture Bevy re-created underneath us
triggers a full repaint.

## Counters

`SceneIcedCounters` keeps per-frame and total counts of `AssetEvent<Image>`
Added/Modified (all images and surface textures), allocations, draws, and
rectangles and bytes queued (main world) and written (render world). Quoin
reports the snapshot in `shell.debug.status` under `scene_iced`. With
`COSMIX_SCENE_ICED_TRACE=1` a `SCENE_ICED_STATS` line is logged every 120
updates.

## Input, focus and IME

Pointer input is read from Bevy's `PointerInput` messages, routed to the
surface under the pointer (`HoverMap`) and captured from press to release.
Positions are converted to surface physical pixels using the window scale.
A press on a surface sets `InputFocus`; a press on no surface clears it if a
surface held it. `SceneIcedFocus` records the owning surface, its IME request
(caret in window-logical coordinates) and the hovered cursor shape. Keys and
Bevy `Ime` preedit/commit go to the owner only.

Seams: the layer host enables text-input-v3 only for a focused
`EditableText` and has no cursor-shape support, so in Quoin the IME request
and cursor shape are recorded but not applied, and no IME events reach a
surface yet. Pointer capture does not check that later events come from the
same window.

`SceneIcedWake` is the earliest renderer wake time; Quoin copies it into
`LayerHostDeadline` so a caret can blink on an otherwise idle host.
