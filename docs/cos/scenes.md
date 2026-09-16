# Mix Scenes in Quoin

Quoin accepts renderer-neutral scene documents through `shell.scene.load`.
The request body is the complete AMP document with one `mix` fence. The
`cosmix-scene` parser and registry validate it before any live state changes.
Invalid loads and patches return diagnostics and retain the last good tree.

Scenes mount as pages in an edge panel. Floating windows are outside v0.
For a nested development host, select its compositor with `--comp-service`
and a distinct registration with `--bus-service`; the default registration
remains `shell`. Scene verb names retain the `shell.scene.` prefix.
`shell.scene.get {scene}` returns the P1 resolved tree, including defaults,
template metadata and numeric ports as floating-point values. An optional
`path` selects `node.port`. `shell.scene.patch {scene,path,value}` validates
a candidate before committing it; null clears an optional authored port.
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
`template-node@row-id` identities and substitute only `{cells[i]}` in text.
Text elision uses CTK's middle-elision policy.

The standalone shell host bridges Wayland text-input-v3 preedit and commit
events to Bevy's editable text pipeline. Candidate positions come from the
focused field's layout in its panel surface, without a primary Winit window.
The synthetic `cosmix-imeprobe --external-field` mode exercises an existing
client field without opening the probe's own text window.

Development builds may enable Quoin's `scene-gates` feature and set
`COSMIX_SCENE_EDIT_GATE=clippanel`. The opt-in probe types, selects and starts
composition through CTK, logs `SCENE_RETAINED_GATE READY`, then waits for a
Bus reload. It checks entity identity, focus, selection, history and preedit,
then performs undo and logs `SCENE_RETAINED_GATE PASS`. This is an in-process
editing probe, not a physical keyboard or pointer injection test. It is absent
from production builds.
