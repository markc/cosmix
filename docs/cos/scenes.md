# Mix Scenes in Quoin

Quoin accepts renderer-neutral scene documents through `shell.scene.load`.
The request body is the complete AMP document with one `mix` fence. The
`cosmix-scene` parser and registry validate it before any live state changes.
Invalid loads and patches return diagnostics and retain the last good tree.

Scenes mount as pages in an edge panel. Floating windows are outside v0.
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
