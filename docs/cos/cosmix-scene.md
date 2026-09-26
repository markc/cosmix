# cosmix-scene

For the retained CTK renderer and shell verbs, see [Mix Scenes in Quoin](scenes).

`cosmix-scene` parses a bounded Mix Scenes v0 document into a renderer-neutral
tree. A document is a Bus envelope with required `scene: 1`, `name` and
`citizen` headers. `window`, `subscribe`, `targets` and `model` are optional
single-line JSON headers. The envelope is the mount request and a
window-family node is optional. If both exist, they must agree.

`window.kind` is `edge` or `dialog` (0.6). Absent on the header, it means
`edge`. An edge window takes an `edge` (`right` by default), optional `w`/`h`
and `title`. A `dialog` is a centred window, not a page on an edge. It needs
numeric `w` and `h` in 240..=2048 logical px and takes no `edge` or `panel`,
which lint reports as `window-dialog-edge`. `chrome` defaults to true for
both kinds. From 0.6 the header is validated on its own, not only against a
window node: an unknown `kind` is `window-kind`, a bad `edge` is
`enum-value`, and a non-numeric or negative `w`/`h` or a non-string `title`
is `port-type`. Earlier versions accepted a header-only kind they could not
honour and mounted it on the right edge, so citizens that author a dialog
check `shell.scene.describe {family:"window"}` for the `dialog` enum value
first. In v1 only the Scene Editor mounts as a dialog: the scenes loader
refuses `kind:"dialog"` for every other scene.

The body contains exactly one fenced `mix` strict-data map. Nodes have the
shape `id: { widget: family, ...ports }`. The ten families are `window`,
`column`, `row`, `text`, `field`, `button`, `toggle`, `list`, `image` and
`spacer`. Their required and optional ports, defaults and handler ports are
described by `describe()`. Enum ports (`window.kind`, `window.edge`,
`row.align`, and `button.tone`) are validated during lint. Numeric ports are
always JSON floating-point values: `13` resolves and travels as `13.0`.

Version 0.4 adds the [curated layout vocabulary](scenes#curated-layout-ports-scene-crates-04)
without changing the `scene: 1` header. Citizens discover support through
`shell.scene.describe`, by family and port. `layout-conflict` rejects
contradictory legacy/explicit sizing and inverted min/max bounds; an old
host instead rejects the new ports as `unknown-port`. See the renderer manual
for the text centring correction and the preserved zero text-wrapper minimum width.

Lists require rows of `{id: string, cells: [string]}`, a sibling `row`
template and a positive `row_height`. IDs must be non-empty and unique within
each list; extra item fields remain available through `$item` and click bodies.
`flow: "horizontal"` opts into natural-width repeated rows with `gap` and
`align`; omitted flow remains the vertical VirtualList. Horizontal flow ignores
vertical viewport sizing (`row_height` and `max_rows`). Template subtrees may contain only row,
column, text, spacer and image. `{cells[i]}` is allowed only in `text.text`,
and must be within the minimum cell count across all rows. Template nodes are
marked in `ResolvedScene`; their ids are not rendered. `@` is reserved for
renderer instance identities and is rejected in source node ids. Instance IDs
include the owning list and item ID; consumers use the event's `item`, never
parse these IDs.

Every non-window family accepts the boolean `hidden` port, including bindings.
Its absence preserves previous resolved defaults (`text.hidden` still defaults
to false). Hidden containers consume no layout space.

`bindings::template_instantiate_with` shares a `TemplateEvaluation` across an
entire scene revision: one model conversion, at most 16,384 instantiated nodes,
and the core's 250 ms evaluation budget. The convenience
`template_instantiate` creates a context for a single node; hosts rendering
repeated trees must use the shared context. Both evaluate against the live
`$model` and supplied `$item`, coerce ports and validate layout bounds.
Quoin runs that preflight for loads, port patches and model-only patches
before committing a revision. A template failure preserves the authored
document, resolved tree, compiled bindings and revision. Results that cross
the shared deadline are rejected even when the final expression never yields.
The renderer retains the prepared instances with that accepted revision and
applies them without evaluating again. Mix expression evaluation reuses one
Tokio runtime per calling thread; each call still has fresh globals and limits.

Lint reports bounded-document, schema, graph, template, row and header
diagnostics. `orphan-node` is a warning; other violations are errors. The
resolver returns diagnostics for unknown families rather than panicking.

`to_source(&SceneDocument)` serialises the authored AMP document, including
model, optional headers, expressions and template nodes. It does not serialise
the flattened resolved tree. Strict Mix escaping preserves literal `${...}`,
leading `~`, backticks (including fence-looking text), Unicode and control
characters. Quoin exposes this as `shell.scene.get {scene,format:"source"}`.

`bindings::reevaluate` accepts `model` to replace a complete model map (null
clears it), as well as `model.*` map paths. Both individual patch values and the
aggregate model are bounded before evaluation. The host retains the compiled
binding set, commits authored model/resolved tree/revision together, and checks
the complete canonical document bound before accepting a patch. A refused
patch leaves the prior revision intact.
The host also refuses model patches that move the scene's mount page or edge,
preserving the loading citizen's existing reservation.

`diff(old, new)` emits `Remove`, `Insert { parent, index }`, `SetPort`,
`Reparent`, and scene-level `SetScene` operations for name, citizen, window
and subscribe changes. A dropped port emits `SetPort` with JSON `null` to
clear it. Consumers apply operations in this order: Remove, Insert, SetPort,
Reparent. Scene operations should update the mount metadata as part of the
same reload.
