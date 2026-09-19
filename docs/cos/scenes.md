# Mix Scenes in Quoin

Quoin accepts renderer-neutral scene documents through `shell.scene.load`.
The request body is the complete AMP document with one `mix` fence. The
`cosmix-scene` parser and registry validate it before any live state changes.
Invalid loads and patches return diagnostics and retain the last good tree.

Scenes mount as pages in an edge panel. Floating windows are outside v0.
The envelope's `window` header is the mount request; when absent, the window
node supplies edge, title and extent. Patching that node reapplies the mount.
`window.chrome` must be a boolean and defaults to true; false lets the page
fill its panel without the Quoin header bar, for panel-style furniture.
Chromeless pages have no in-panel page navigation or pin control: use them
alone on an edge or drive navigation and pinning through Bus verbs.
An absent or cleared extent uses the shell's output-derived default thickness.
Authored extents fit the output space left by opposing exclusive zones;
pinning and output changes recheck that budget. Pointer resizes exceeding the
remaining budget are rejected without changing panel thickness. Clearing ports resets their
derived layout constraints. A spacer without a size flexes into free space.
For a nested development host, select its compositor with `--comp-service`
and a distinct registration with `--bus-service`; the default registration
remains `shell`. Scene verb names retain the `shell.scene.` prefix.
`shell.scene.get {scene}` returns the P1 resolved tree, including defaults,
template metadata and numeric ports as floating-point values. An optional
`path` selects `node.port`. `shell.scene.patch {scene,path,value}` validates
a candidate before committing it; null clears an optional authored port.
The complete serialised patch candidate must fit the same 256 KiB bound as
loads. Rejected patches retain both the tree and its revision.
`shell.scene.describe {family?}` reports the shared P1 registry.
`shell.scene.unload {scene}` removes the page.

`shell.scene.watch {scene}` returns `{scene,revision,digest}`. Subscribe to
`shell.scene.changed` for summaries `{scene,revision,ops,diagnostics}`;
fetch the complete tree with `get`. Revisions increase on accepted loads and
patches. Digests are SHA-256 of the serialised resolved tree.

UI handlers are directed Bus requests to the document's citizen with
`{scene,node,kind,value?,item?}`. Kinds are `click`, `change` and `submit`.
Requests time out after two seconds; unavailable citizens are logged at most
once per minute. Hover, focus and pointer motion stay local.

Stable node IDs retain CTK field entities, selection, focus, composition and
undo state. Plain fields use CTK's single-line editing transactions. Active
edits take precedence over incoming value replacements while focused.
Changing a field's family or password mode replaces that widget.

List rows use CTK VirtualList. Row templates are instantiated with
`template-node@row-id` identities and substitute `{cells[i]}` only in
`text.text` and `image.src` inside list templates.
Outside templates, cell markers in other ports are literal data.
Text elision uses CTK's middle-elision policy.
`column.align` sets cross-axis alignment to `start`, `center`,
`end` or `stretch` (default).
`text.align` sets justification to `left` (default), `center` or `right`
within the text's `width` or the space allocated by `fill: true`.

Absolute image `src` paths load PNG and SVG directly, rasterised or resized
at `UiScale` multiplied by the primary window's scale factor, or `UiScale`
alone in a compositor host without a primary window. Scale changes reapply
icons. The 512-entry LRU cache keys path, file modification time, file size,
pixel target and effective scale; eviction drops the cache's image handle.
Only regular files up to 4 MiB and targets up to 1024 pixels per side are
accepted. PNG input is limited to 4096 pixels per side and 64 MiB of decoder
allocation. SVG paths and gradients are supported; SVG text and embedded
images (including data URLs and filesystem references) are disabled. SVGs
retain their aspect ratio and are centred in the target.
Invalid files render nothing, with decode/type failures remembered per target
and file version. Missing files are retried on a scene load/revision; other
I/O failures are retried on the next apply. Diagnostics are logged once per
cached failure; all cache entries, including failures, share the LRU bound.
The host also substitutes `{cells[i]}` in
an image template's `src`, including when loaded through `shell.scene.load`.

CTK shares the font source cache through weak references. Fonts still used by
retained layouts keep the same atlas identity after idle cache pruning, so
periodically updated labels do not accumulate duplicate font-atlas textures.

The standalone shell host bridges Wayland text-input-v3 preedit and commit
events to Bevy's editable text pipeline. Candidate positions come from the
focused field's layout in its panel surface, without a primary Winit window.
Focus generations and text-input `done` serials discard delayed batches for
previously focused fields. Cursor-rectangle commits extend the current focus
generation's serial range; input already in flight for that field still applies.
The synthetic `cosmix-imeprobe --external-field` mode exercises an existing
client field without opening the probe's own text window.

Development builds may enable Quoin's `scene-gates` feature and set
`COSMIX_SCENE_EDIT_GATE=clippanel`. The opt-in probe types, selects and starts
composition through CTK, logs `SCENE_RETAINED_GATE READY`, then waits for a
Bus reload. It checks entity identity, focus, selection, history and preedit,
then performs undo and logs `SCENE_RETAINED_GATE PASS`. This is an in-process
editing probe, not a physical keyboard or pointer injection test. It is absent
from production builds.
