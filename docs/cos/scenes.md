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
can overshoot until it returns. Template instantiation uses the same budget.
Null model patches remove keys without creating missing parent maps. Patch
values are limited to 256 KiB of serialised JSON (`model-path` on excess).
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

**Known scene: 1 behaviour change:** the text wrapper's default `min_width`
changes from zero to automatic intrinsic sizing when `elide` is false.
Crowded rows can therefore retain more text width and overflow rather than
silently compressing its wrapper. Use `min_width: 0` to opt into compression,
or `elide: true` for bounded middle-elision (which retains the zero minimum).
This is not only an additive vocabulary change. The text centring correction
also deliberately changes geometry for existing centred/right-aligned text
with a wider allocation.

### Layout regression fixtures

`cosmix-scene-bevy/src/render/layout_tests.rs` runs the real document ingress,
reconciliation and Bevy/Taffy layout against a synthetic 800×560 logical
viewport at scale 1.25. It rejects an unsettled font measurement and compares
logical geometry with a 0.5px tolerance. The deliberately displaced scene
must trigger the centring assertion. The authored-width text regression is
enabled in P1.

The legacy freeze compares every document node's border/content boxes and
shaped text-run boxes against a test-only copy of the P0 renderer mapping.
It covers rows/columns with gaps and padding, plus static reconstructions of
Quoin's launcher fallback, workspace buttons, task label, flexible spacer,
status badges, nested clock and calendar/notification popup shapes. Internal
label UI boxes intentionally become intrinsic; the freeze compares visible
text geometry instead. Fixtures contain no new ports and no intentionally
corrected centred-text allocation. Separate tests cover the corrected defect,
new vocabulary, port clearing, no soft wrapping, explicit newlines and elision
with the production CTK theme plugin.

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
