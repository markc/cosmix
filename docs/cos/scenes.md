# Mix Scenes in Quoin

Quoin accepts renderer-neutral scene documents through `shell.scene.load`.
The request body is the complete AMP document with one `mix` fence. The
`cosmix-scene` parser and registry validate it before any live state changes.
Invalid loads and patches return diagnostics and retain the last good tree.
For file discovery, enablement, supervised behaviours and popup coordination,
see the [Stage A scenes loader](scenes-loader).

Scenes mount as pages in an edge panel, or, for `window.kind:"dialog"`, in
the host's one centred dialog seat (see [Dialog scenes](#dialog-scenes)).
Other floating windows are outside v0.
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

`shell.scene.validate` accepts the same AMP body as load, returning
`{scene,valid:true,diagnostics}` without mounting, reserving a page or changing
revisions. `shell.scene.get {scene,format:"source"}` returns
`{scene,revision,source}` with canonical authored AMP source: expressions,
template definitions and omitted defaults are preserved. The core serializer
escapes interpolation, leading tildes, backticks, controls and Unicode for the
strict Mix parser. Scene refusals use nonzero RC and `{error_code,message}`
with diagnostics/context when available.

Patch paths also accept `model` (replace the complete model map; null clears it)
and `model.<key>...` (map-key updates, null removes a key). The store retains
compiled bindings and reevaluates against the last accepted resolved tree.
The authored model, resolved tree and revision commit together only after
aggregate document/model bounds pass. Evaluation warnings retain last-good
ports; structural/bounds refusals retain the whole previous revision. Accepted
model revisions refresh list instances, including templates whose row data
did not change but whose model dependencies did.
Model patches that change a scene's mount page or edge are refused with
`SUBPANEL_COLLISION`; unload and load explicitly to move that mount.

Subscribe to `<host>.panel.changed` for inner command `shell.panel.changed`:
`{generation,revision,dialog,panels:{left,...}}`. Each panel has the same applied
`visible`, `pinned`, `mode`, `page`, `pages`, `declared`, `width_px` and
`output` values as `shell.props.get`, and `dialog` is the dialog seat or null.
A `conf.mix` order change (`declared`) publishes like any other change. Publication follows scene reconciliation and model
application; unchanged snapshots produce no event. Take a property snapshot
after subscribing and resynchronise after a connection/gap. Enqueue replies
are not applied-state receipts, so page selection and temporary pin release
must wait on these events rather than retry sleeps.

Settings/Appearance follows the same pattern. The `share/scenes/settings`
template (scene `quoin-settings`, page `settings.appearance`) is layout only.
Its behaviour subscribes to `<host>.settings.changed` (inner command
`shell.settings.changed`) and re-reads `shell.settings.get` on each notice,
after reconnecting, and when the host re-registers. It publishes the model
through `scenes.model` and forwards the page's clicks unchanged to
`shell.settings.{scheme,motion,size}`. The effects, the stepper size and the
clamping stay in Quoin. Without the loader, Quoin serves the same node block
as a built-in fallback and yields the page to a loader-managed load. Snapshot
fields and the handover rules are in [Quoin](quoin).

## Dialog scenes

A document whose `window` header says `kind:"dialog"` (cosmix-scene 0.6; `w`
and `h` required, no `edge` or `panel`) mounts into the host's **dialog seat**,
not an edge carousel. There is one seat per host. In v1 the scenes loader
reserves it for the [Scene Editor](scene-editor) and refuses dialog-kind
documents from every other scene.

- **Seat.** A dialog load reserves the seat and keeps the surface unmapped. A
  second dialog load from another scene is refused `DIALOG_BUSY`, unless its
  JSON load envelope carries `preempt_dialog:true`. In that case the
  incumbent is released, and its owner gets a `<host>.scene.changed` notice
  with `ops:["unloaded"]`, `reason:"preempted"` and `by:{scene,owner}`.
  Unloading the scene, or its owner disconnecting, releases the seat.
- **Surface.** The dialog is an overlay layer surface with no anchors and
  exclusive zone 0. comp centres it in the output's usable area, net of the
  other layers' exclusive zones: with a 52 px bottom panel docked, it sits
  26 px above the output centre. On every show it takes the keyboard
  (`Exclusive`), then demotes to `OnDemand` once the keyboard enter lands, and
  keeps focus while mapped. Pointer, keyboard and touch reach it like a panel
  surface, but it never drives panel reveal, hold, pin or resize.
- **Chrome.** A title bar with a **×**, which is frame chrome rather than a
  scene node. × hides the dialog, and so does Escape while the dialog holds
  the keyboard and no IME preedit is active. Neither involves the scene's
  behaviour. Output removal unmaps the dialog; the next show maps it on the
  current output.

| Verb | Args | Reply |
| --- | --- | --- |
| `shell.dialog.show` | `{scene}` | `{scene,visible:true,applied}`; maps and takes the keyboard |
| `shell.dialog.hide` | `{scene}` | `{scene,visible:false,applied}`; unmaps and keeps scene state |
| `shell.scene.layout` | `{scene,node?}` | `{scene,revision,applied_revision,visible,surface:{kind:"panel"\|"dialog",edge?,output,x,y,w,h},nodes:{<id>:{x,y,w,h,hidden}},instances:{<list>:{<item>:{x,y,w,h}}},chrome:{close?:{x,y,w,h}}}` |

`applied` is false when the dialog was already in the requested state.
Refusals are `NOT_FOUND` (no such scene) and `NOT_DIALOG` (an edge page).
`shell.scene.layout` reads the applied revision's computed layout in logical
px. The surface rect is in output coordinates and node rects are relative to
the surface. While unmapped it reports `visible:false` and empty `nodes`. It
is how gates and agents click a node: take the centre of its rect.
A dialog is fitted to the zone it centres in (the output less Quoin's docked
panels' exclusive zones): each authored side is kept when it fits, else it
becomes the zone less 24 px on both sides, never below 240 px, i.e.
`w = max(240, min(declared_w, zone_w − 48))` and likewise for `h`. It re-fits
on every output or exclusive-zone change. `props.dialog.w/h`,
`panel.changed`'s `dialog` and this verb's `surface.w/h` all report that
actual size; the scene's content reflows into it (its lists scroll).
A dialog's surface `x`/`y` on the layer host is Quoin's placement estimate
(the output centred within its own docked panels' exclusive zones); comp
places the pixels, so another client's exclusive zone can move the real
surface. Node rects are unaffected: they are measured inside the surface.
`chrome.close` is the dialog's × frame control, measured the same way and in
the same surface coordinates, present only while a `chrome:true` dialog is
mapped (`chrome` is `{}` otherwise). Clicking its centre hides the dialog.

Every change of `dialog.visible` or `dialog.scene` publishes
`shell.panel.changed` with a strictly greater revision; the snapshot's
`dialog` is `{scene,visible,w,h,output}` or null. Page order is written with
`shell.panel.order`, described in [Quoin](quoin).

## V1 bindings

A port value beginning `= ` is one Mix expression. `== x` escapes to the
literal `= x`, while `=x` remains a literal. For example:

```mix
root: {widget: "text", text: "= $model.title", hidden: "= !$model.visible"}
```

Bindings read the scene's JSON `$model`; row templates may also read the
current `$item`. A binding is a pure function of `$model` and `$item`.
Nested lists and their row subtrees use the same template rule. Row
instantiation takes a node ID and the live model, so sibling templates
keep their own bindings and see the latest model patch.
When a model path changes, a binding reruns when its dependency is that path,
an ancestor, or a descendant of it; unrelated bindings do not run.
Interpolation and heredocs record dependencies too: `"Hi ${model.user.name}"`
depends on `model.user.name`, including dependencies inside coalesce defaults.
`${NAME}` with any root other than `model` (or template-only `item`) is a
`binding-policy` error; bindings cannot use interpolation to read process
environment variables. Index access collapses its dependency to the base
path and also tracks dependencies in the index expression.

Compilation uses lib-mix's static expression-mode check. Denied constructs
fail compilation, including untaken branches; evaluation follows the engine's
short-circuit and coalesce semantics. At load, a failed binding takes the
schema default, or the port is absent when there is no default. The binding
source is never a literal fallback. A nil result also restores the default
or removes the port. Later failures preserve the last good value.

Lint and resolve share cached compilation and load results; changing the
document invalidates the cache. Each binding runs at most once per load.
Load evaluation and each re-evaluation pass have a 250 ms total wall budget,
with at most 50 ms per expression. Remaining bindings report `binding-eval`
with `evaluation budget exhausted`; they take defaults at load or retain
last-good values on a patch. As with lib-mix limits, a non-yielding builtin
can overshoot until it returns, but the late result is refused even for the
final binding in a pass. Template instantiation uses the same budget.
Null model patches remove keys without creating missing parent maps. Patch
values and the aggregate runtime model are limited to 256 KiB of serialised
JSON (`model-path` on excess). The host additionally enforces the aggregate
authored-document bound, including metadata and ports.
Empty model and binding fields are omitted from resolved-tree serialisation.

| diagnostic | meaning |
| --- | --- |
| `invalid-binding` | expression syntax, statement count or depth is invalid |
| `binding-policy` | a disallowed root or operation was used |
| `binding-not-allowed` | a structural port was bound |
| `binding-eval` | evaluation failed or budget exhausted; default at load, last good on patch |
| `binding-type` | the result failed strict port validation |
| `model-path` | the model patch path is malformed or its value exceeds the size limit |

Calling `time()` is allowed but emits a `binding-nondeterministic` warning;
it is not a policy violation.

`shell.scene.watch {scene}` returns
`{scene,revision,digest,applied_revision,diagnostics}`. Subscribe to
`shell.scene.changed` for summaries `{scene,revision,ops,diagnostics}`;
fetch the complete tree with `get`. Revisions increase on accepted loads and
patches. Digests are SHA-256 of the serialised resolved tree. `applied_revision`
is the last revision applied by the renderer (zero before first application).
Watch replies and scene inventory retain render diagnostics for state-based
resynchronisation; notifications are hints, not proof of application.

UI handlers are directed Bus requests to the document's citizen with
`{scene,node,kind,value?,item?}`. Kinds are `click`, `change` and `submit`.
Requests time out after two seconds; unavailable citizens are logged at most
once per minute. Hover, focus and pointer motion stay local.

Stable node IDs retain CTK field entities, selection, focus, composition and
undo state. Plain fields use CTK's single-line editing transactions. Active
edits take precedence over incoming value replacements while focused.
Changing a field's family or password mode replaces that widget.

List rows use CTK VirtualList by default. `flow: "horizontal"` uses natural-width
rows with `gap` and `align` (`start`, `center`, `end`, `stretch`), without a
vertical viewport. `row_height` remains required for schema compatibility;
horizontal flow ignores it and `max_rows`. Rows require unique non-empty IDs.
Row templates evaluate core bindings against live `$model` and `$item`, then
substitute legacy `{cells[i]}` only in unbound `text.text` and `image.src`.
Binding results are literal values and are never substituted again.
Instance identities include the template, owning list and item ID. Retained
items keep their template entities across rebinds/reordering. Scene lists opt
into `VirtualListModel::retain_content`; CTK's default still clears recycled
content, and changing an item's ID always creates fresh content.
Each template evaluation pass shares a 250 ms / 16,384-node budget across all
lists. Ingress preflights templates before committing a revision or mount;
failure returns `{error_code:"scene_template",message,scene,diagnostics}`.
Loads, port patches and model-only patches retain those prepared instances with
the accepted revision. Rendering applies them directly, including when display
scale changes; there is no second expression evaluation or deadline. Expression
evaluations reuse a thread-local Tokio runtime while keeping globals, policy and
limits isolated. A renderer state failure retains the last applied revision and
a readable `scene-render` diagnostic in watch/inventory replies. A new accepted
revision clears that diagnostic and permits another application attempt.
Nested-list rendering remains outside this renderer's supported template path.
Clicks on repeated rows use the owning list's handler and node ID, with the
complete row in `item`; descendant row handlers cannot redirect that click.
Outside templates, cell markers in other ports are literal data.
All non-window families expose `hidden`. Hidden containers and horizontal
row wrappers use `Display::None`, so their padding and gap allocation disappear.
A hidden template inside a vertical VirtualList hides its content; the fixed
row slot remains. Filter the model's `rows` to remove a vertical slot entirely.
Text elision uses CTK's middle-elision policy.
`column.align` sets cross-axis alignment to `start`, `center`,
`end` or `stretch` (default).
`text.align` sets justification to `left` (default), `center` or `right`
within the text's `width` or the space allocated by `fill: true`.

### Curated layout ports (scene crates 0.4)

These ports retain the `scene: 1` envelope. Before emitting them, citizens
must call `shell.scene.describe` for each affected family and check the
returned port paths and enum values. Do not infer support from `scene: 1` or
the shell's version. An older host rejects an unknown port and preserves its
last-good document; a citizen can then submit a separately authored legacy
document or report that the host is too old.

| Families | Port | Values / meaning |
|---|---|---|
| row, column | `justify` | `start`, `center`, `end`, `between`, `around`, `evenly`; main-axis distribution |
| Every family except window | `align_self` | `auto`, `start`, `center`, `end`, `stretch`; this child's cross-axis override |
| Every family except window | `grow`, `shrink` | Non-negative flex weights |
| Every family except window | `basis` | Non-negative initial main-axis size in logical px; omission retains the native/legacy basis |
| Every family except window | `min_width`, `max_width`, `min_height`, `max_height` | Non-negative logical px constraints |
| row, column | `row_gap`, `column_gap` | Non-negative logical px, overriding the respective axis of `gap` |
| row, column | `padding_top`, `padding_right`, `padding_bottom`, `padding_left` | Non-negative logical px, overriding that side of `padding` |

`row.align` and `column.align` keep their cross-axis meanings and their
existing defaults (`start` and `stretch`, respectively). `text.align` keeps
its text-alignment meaning. No reverse direction, flex wrapping, baseline
alignment or absolute-position ports are exposed. `basis` is numeric only;
clear it with null to restore the native basis. All new ports are optional
without schema defaults, so old resolved documents retain their canonical
ports. Clearing an axis/side override restores its shorthand's value.

`fill` retains its existing behaviour when explicit flex sizing is absent,
including the fixed-height row exception and fill-text stretch in a stretched
column. Authored `fill` (including false) cannot coexist with `grow`, `shrink`
or `basis`. Fixed row `height` and spacer `size` likewise cannot coexist with
those explicit flex ports: remove the legacy declaration when migrating.
`list.max_rows` and `max_height` are mutually exclusive. Inverted min/max
bounds are errors. These produce `layout-conflict`, not silent precedence.
Bounds are also checked after model binding evaluation and template
instantiation; a rejected model patch retains the previous tree.
Gap/padding shorthands with their documented overrides are intentionally
allowed, as is `fill` with `align_self`.

Text remains single-line unless the authored string contains explicit
newlines. It does not acquire soft wrapping from this change. In Bevy 0.19.1,
`NoWrap` uses an unbounded text layout even if the UI label has a wider
percentage width. The adapter therefore leaves the label at its intrinsic
width and positions it inside the wrapper using Taffy. Natural-width text
stays natural. Width-constrained text without `elide` may overflow; `elide`
uses CTK's existing middle-elision system and the wrapper's width budget.
Hosts must install `CtkThemePlugin` for that production elision system.

Text wrappers retain the legacy zero minimum width, whether or not `elide`
is enabled. Authors can set an explicit numeric `min_width` to constrain
compression. The intrinsic label used for centring does not change that
wrapper default. The text centring correction deliberately changes text
placement for existing centred/right-aligned text with a wider allocation.

### Layout regression fixtures

`cosmix-scene-bevy/src/render/layout_tests.rs` runs the real document ingress,
reconciliation and Bevy/Taffy layout against a synthetic 800×560 logical
viewport at scale 1.25. It rejects an unsettled font measurement and compares
logical geometry with a 0.5px tolerance. The deliberately displaced scene
must trigger the centring assertion. The authored-width text regression is
enabled in P1.

The legacy freeze compares every document node's border/content boxes and
shaped text-run boxes against a test-only copy of the P0 renderer mapping.
It collects every mismatch across all four fixtures before failing once.
The report orders differing nodes by their largest absolute edge-coordinate
delta, and prints old/new/delta values for all three rectangles, including
left/top/right/bottom and width/height in logical px. The freeze requires
exact equality after restoring the legacy zero text-wrapper minimum;
width/height deltas are additional diagnostics. Other layout tests retain
their 0.5px tolerance.
Missing or non-finite geometry ranks first. Camera, font and readback validity
guards remain prerequisites: invalid measurements are not geometry results.

Independent node-ID manifests pin coverage to 53 nodes: panel 24, popup 19,
row 5 and column 5. Global and per-fixture summaries report compared and
differing counts, along with expected/old/new counts and coverage errors.
Deleting a node from both mappings cannot silently shrink the freeze.
The panel's `root`, `fill`, `clock`, `clock_col`, `clock_time`, `clock_date`
and `peek` are all compared; their rectangles are also printed when exactly
unchanged, so the clock's right-edge arithmetic can be checked.

The former fail-fast implementation stopped on the first coordinate of
`panel/clock`. That failure gave no comparison result for the remaining 52
nodes or even the clock's other edges. All fixture IDs were mapped; this was
premature termination, not an intentional exclusion from the geometry map.

It covers rows/columns with gaps and padding, plus static reconstructions of
Quoin's launcher fallback, workspace buttons, task label, flexible spacer,
status badges, nested clock and calendar/notification popup shapes. Internal
label UI boxes intentionally become intrinsic; the freeze compares visible
text geometry instead. Fixtures contain no new ports and no intentionally
corrected centred-text allocation. Separate tests cover the corrected defect,
new vocabulary, port clearing, no soft wrapping, explicit newlines and elision
with the production CTK theme plugin.

Of the real Quoin shapes exposed to non-elided text sizing, the current panel
fixture represents the Apps fallback, two workspace labels, a simplified
status bullet/count, the nested clock and the peek fallback. The popup has a
rough month/arrows analogue, three weekday/day cells and a generic notice;
it does not reproduce the actual calendar navigation or notification header.
Tray fallback initials, launcher heading/count/category chips, calendar
Today/day/full-date headings, and notification heading/Clear-all controls
are absent. Exact builder output and dynamic states require the extraction
below (additional hand-authored fixtures could cover individual shapes).

These are representative shapes, not captured executions of
`scripts/quoin-panel.mix`. That citizen builds documents from live services,
time and icon lookup and cannot be loaded as a static scene. For full
record/replay coverage, add a pure document-export seam before `load_scene`:
provide deterministic service snapshots and a fixed clock to each builder,
replace icon paths and private content with public fixtures, and save the
exact document returned by `lib/panel.mix`'s `document` function. Replay those
bytes through the same harness against both mappings. Include empty and busy
taskbars, tray variants, launcher lists, calendar months and notifications;
record the font/scale inputs alongside any numeric geometry goldens. The
current freeze does not claim to cover those dynamic states or Quoin's edge
chrome/mount geometry.

Stage A now adds exact, sanitised launcher/calendar/notes builder captures in
`src/desktop/scripts/tests/fixtures/scenes/`. The Mix extraction gate compares
the legacy builder bodies and returned document bytes, and records the models
separately. Renderer tests compare fully instantiated visible trees, including
the hidden notes alternative, against those documents. Cases cover empty and
filtered launchers, the 500-row cap, empty/full notifications, leap day, a
non-leap century and year rollover. Scheduled Taffy tests cover horizontal
natural widths, gap/alignment, and real VirtualList rebind/reorder identity;
the Bus bridge test checks the outgoing citizen, command and item body.
These are cluster gates, not a claim of live GPU screenshot acceptance.

Stage B adds the bottom panel (`share/scenes/panel`, page `scene-panel`) with
empty, busy, tray-change and task-cap captures in their own
`panel-cases.json`. Its pager, tasks and tray are horizontal-flow lists, so
the old builder's generated button nodes become list instances and node IDs
no longer match one to one; the Mix gate instead evaluates the template's
bindings against each captured model and compares visible trees by shape,
with each list's click handler applied to its instances. A Rust renderer
comparison of those cases (normalising horizontal instances) is still to be
added. Settings and live migration are the remaining Stage B work; see
[quoin-panel](quoin-panel.md#stage-b-the-panel-template).

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
