# term: foot tactics investigation

2026-09-25. Initial investigation followed by implemented ranks 1, 2, 3, 4 and 6
and the T16 merge validation below. Opening measurements describe the original
tree; the implementation section holds the newer numbers. The current default
is **tiny-skia**, accepted by Mark after testing: "typing in the test term
feels much better". At 2250×1250 the native-copy benchmark measured echo
around **1.7 ms** and full redraw around **11.5 ms**. The optional clean wgpu
build is `--no-default-features --features wgpu`.

The largest measured opportunity is the CPU image pipeline, not missing glyph
caching. The existing banded path costs about 6.4 ms for one changed row and
21.8 ms for a genuinely changed full pane. A standalone, cached, opaque
2250×1250 tiny-skia pixmap takes about 10.5 ms to draw even with an identity
transform; copying its native pixels takes about 0.43 ms. Eliminate that
generic draw first, then eliminate image conversion and avoid painting states
that will never be presented. These are substantial CPU opportunities without
requiring a GPU. Shared scheduling, damage and raster improvements also benefit
wgpu, which still runs the same CPU glyph painter.

The initial investigation kept wgpu as the interim default (superseded by the
acceptance above). It established an optimisation
direction, not foot-equivalent latency. No matched live foot/term latency
measurement or softbuffer presentation timing was obtained in this pass.

## Evidence and reproduction

Repository base: `927e8340f021` (`term/foot-tactics`), term 0.2.4, core 0.5.1.
Source references below use these aliases, avoiding machine-specific paths:

| Alias | Source inspected |
|---|---|
| `term/` | `src/desktop/apps/term/src/` |
| `core/` | `src/desktop/crates/cosmix-term-core/src/` |
| `foot/` | Local reference checkout, foot 1.28.0, commit `9f5c70e` |
| `fcft/` | Local reference checkout, fcft 3.3.3, commit `1ff7e79` |
| `skia/` | Cargo registry `iced_tiny_skia-0.14.1/src/` |
| `graphics/` | Cargo registry `iced_graphics-0.14.0/src/` |
| `tiny/` | Cargo registry `tiny-skia-0.11.4/src/` |
| `softbuffer/` | Cargo registry `softbuffer-0.4.8/src/` |
| `iced-winit/` | Cargo registry `iced_winit-0.14.1/src/` |
| `winit/` | Cargo registry `winit-0.30.13/src/` |

Foot and fcft references are to the inspected snapshots, not an assumption
about whatever upstream releases next. Upstream projects:
[foot](https://codeberg.org/dnkl/foot),
[fcft](https://codeberg.org/dnkl/fcft). The upstream
[fcft README](https://sources.debian.org/src/fcft/3.3.3-1/README.md)
also describes its pixman output and concurrent glyph lookup design.

Run from `src/desktop` through Mix (the command is an argv array, not shell
source for another interpreter):

```mix
print(run_argv(["cargo", "test", "-p", "cosmix-term", "--release",
    "--no-default-features", "--features", "tiny-skia",
    "--", "--ignored", "--nocapture", "--test-threads=1"], {timeout: 600}))
```

The original two ignored tests are `cpu_grid::bench::tiny_skia_frame_bench` and
`cpu_grid::bench::tiny_skia_foot_phases_bench`. `--test-threads=1` matters:
running performance tests concurrently would contaminate their results.
Rank 6 adds `cpu_grid::bench::raster_warm_spans_bench` (described below).
These historical measurements used the then-default wgpu build; both this report and
[term-rendering.md](term-rendering.md) use explicit tiny-skia feature
selection for CPU measurements. Only the term release test target and its dependencies were
built in that investigation. No workspace build, installation or push was made.

Hardware: Intel Core Ultra 5 125H; normal desktop workload; DejaVu Sans Mono,
13 logical px at scale 2.5. Cells are deliberately padded to 25×50 physical px,
90 columns × 25 rows = 2250×1250 pixels (11.25 MB). Actual font metrics are
20×38; this is the same padded fixture as the earlier report. Each case warms
20 iterations and measures 200, with p99 at sorted index 198. Times are elapsed
wall time, not CPU cycles or input-to-photon latency.

The frame test changes background bytes on every iteration. “Echo” changes
**all 90 cells in one row**, not just one typed glyph. “Full redraw” changes
every row's pixel content; it is not a redundant ANSI repaint of unchanged
text. The fixture simulates three rotating targets and buffer age 3. Actual
softbuffer Wayland uses front/back buffers and release handling; age 3 is a
deliberately retained benchmark scenario, not a measurement of the live window.
Final banded and whole-image pixels must match byte for byte.

## How foot avoids the work

### Glyphs are reusable masks, independent of foreground colour

`foot/render.c:974` asks `fcft_rasterize_char_utf32` for a glyph. This is a
cache lookup, not necessarily rasterisation. `fcft/fcft.c:1903–1931` keys the
per-font table by codepoint and subpixel mode. At `:2019–2048`, an rwlock
protects the hit path; a miss acquires the font lock and rechecks before
rendering, accommodating another worker populating or resizing the table.
The table grows at 75% occupancy (`:1934–1973`). Separate grapheme caching
begins at `:2187`; colour glyphs have their own image format handling.

The cached result is a pixman image: for example grayscale coverage is A8,
LCD coverage is XRGB, and colour glyphs can be ARGB (`fcft/fcft.c:1494–1520`).
Foot fills the cell background with `PIXMAN_OP_SRC` and composites foreground
through the glyph mask; colour images are composited directly
(`foot/render.c:1018–1134`). It does not re-rasterise an outline because the
foreground colour changed. This is a useful cache-key correction for us,
though it does not explain our warmed ASCII benchmark's cost.

### The render destination already is the Wayland buffer

`foot/shm.c:326–342` documents and implements one memfd/mmap allocation shared
by pixman and `wl_shm`. `instantiate_offset` at `:264–305` creates the wl_buffer
and pixman images over the same pointer and stride. Each worker gets its own
pixman image wrapper over that memory, rather than an independently copied
frame. There is no terminal RGBA image → toolkit conversion → full-pane image
draw between glyph composition and the buffer attached to Wayland.

The format is selected as a matched pixman/wl_shm pair. The usual 8-bit pair
is `PIXMAN_a8r8g8b8` / `WL_SHM_FORMAT_ARGB8888`; newer formats are negotiated
when available (`foot/shm.c:998–1074`). On this little-endian system the
8-bit bytes are native premultiplied BGRA; opaque pixels make premultiplication
trivial. Do not describe foot as universally using XRGB: its alpha and higher
bit-depth paths exist. Our softbuffer backend, by comparison, creates
`Xrgb8888` buffers (`softbuffer/backends/wayland/buffer.rs:102`).

### Cell damage and buffer repair are separate concerns

`foot/render.c:701–704` skips clean cells and marks a painted cell clean.
`grid_render` skips clean rows before scheduling any work (`:3567–3581`).
Old and current cursor cells are dirtied at `:3352–3354`; that does not require
repainting both entire terminal rows. `render_cell` unions its actual rendered
rectangle into damage at `:1024–1026`, allowing overhang/clipping semantics to
be represented.

`foot/shm.c:590–655` refuses busy buffers, ages them and prefers the youngest
available allocation. Foot's age convention is its own: an age-zero reusable
buffer can already contain the last frame. Do not equate it to softbuffer's
age zero, which means contents cannot be relied on.

`foot/render.c:3334–3404` selects/reuses a buffer and decides whether it needs
repair. `reapply_old_damage` (`:3198–3292`) copies the whole previous buffer
when the new buffer's age exceeds one. For the recent-buffer case it copies
previously changed regions, subtracting rows this frame will fully overwrite.
A completely dirty grid instead forces repaint without that copy. With scroll
damage it conservatively copies all previous damage; replaying old scroll
operations is explicitly disabled as formerly buggy. This is **repair of
stale contents**, not blindly copying all undamaged screen pixels each time.

There is an optional background pre-apply path (`render.c:2244–2287`), enabled
under sustained delayed buffer release when configured (`:3375–3385`). This
hides repair latency; it does not make its bandwidth cost disappear.

### Scrolling moves existing pixels and repaints newly exposed cells

The terminal advances a circular row offset and swaps row pointers for fixed
regions (`foot/terminal.c:3065–3130`, `foot/grid.c:422–435`). It erases only
newly exposed rows. Adjacent compatible scroll-damage operations accumulate
their line counts (`foot/terminal.c:2628–2653`).

`foot/render.c:1320–1428` applies forward pixel scrolling; `:1433–1505`
handles reverse scrolling. The ordinary path is overlapping `memmove` of the
retained area. The SHM path changes the backing-buffer offset and repairs
fixed regions/margins, selected when its estimated touched area is smaller;
see `foot/shm.c:659–925`. This is not simply “always memmove”, nor should we
start by importing the considerably more complex SHM-offset optimisation.

The compositor still needs damage for the **moved destination area**
(`render.c:1419–1428`) as well as newly painted cells. Sending damage only for
new rows after moving client pixels would be incorrect.

### Compositor damage, pacing and worker threads

Foot merges worker damage and emits `wl_surface_damage_buffer` for each
rectangle (`foot/render.c:3595–3610`), then attaches/commits its buffer
(`:3716–3717`). It requests a frame callback at `:3678–3681`.
`fdm_hook_refresh_pending_terminals` (`:5146–5207`) collects refresh flags;
if a callback is outstanding it only sets pending flags. The callback
(`:4313–4350`) renders the pending state once. This gates actual raster work,
not just presentation.

PTY input also starts a resettable lower timer and a non-resettable upper
deadline (`foot/terminal.c:300–365`). This avoids rendering an erase between
two writes belonging to one application update while bounding starvation.
The inspected defaults are 0.5 ms and about 8.33 ms respectively
(`foot/config.c:3652–3653`), not a universal 16.7 ms delay. Synchronous-update
mode suppresses ordinary grid refresh in the refresh hook (`render.c:5165`).

Workers wait on semaphores, take dirty rows from a protected queue, render
disjoint rows into the shared destination using their own pixman wrapper and
damage region, then signal completion (`foot/render.c:2184–2241`,
`:3557–3598`; creation in `foot/terminal.c:709–739`). A zero-worker path renders
on the main thread. This is useful parallelism after unnecessary work is
removed; copying the threading arrangement alone would not repair our image
pipeline. Foot's server mode can additionally share font/glyph caches between
windows (`foot/doc/foot.1.scd:108–118`); our `Painter` already shares one Raster
between panes (`term/frame.rs:115`, `:209`).

## Where term spends time

### Measured frame costs

Release run with both ignored tests serialised, no CPU affinity. This table
is one complete run, not the best samples selected from several runs.

| Path | Case | Mean ms | p50 | p99 | Paint + handle | Prepare + convert | Draw | Damaged pixels |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| Whole image baseline | echo | 15.172 | 15.126 | 16.960 | 0.823 | 2.637 | 11.709 | 2,812,500 |
| Whole image baseline | full | 21.084 | 21.939 | 25.854 | 4.671 | 4.454 | 11.952 | 2,812,500 |
| Four-row bands | echo | 6.403 | 6.403 | 6.801 | 0.308 | 0.425 | 5.161 | 461,250 |
| Four-row bands | full | 21.823 | 21.885 | 22.868 | 4.856 | 3.419 | 13.538 | 2,812,500 |

The unmodified benchmark run before adding instrumentation also passed:
bands echo 6.388 ms, full 21.527 ms. These reproduce the earlier finding in
`term-rendering.md`: bands help sparse updates but do not solve changed
full-screen content. Total includes layer reset, cache/history release and
damage bookkeeping, so it is not exactly the sum of the three columns.

A final repeat pinned the compiled test executable to CPU 0 using
`taskset -c 0` (after builds/tests had finished). Banded echo was 6.439 ms
(p50 6.415, p99 6.820; paint 0.315, prepare 0.427, draw 5.197), and full was
21.916 ms (p50 21.939, p99 22.945; paint 4.892, prepare 3.439, draw 13.576).
Native full copy was 0.400 ms versus 10.523 ms for identity `draw_pixmap`.
Both tests passed. An earlier pinned repeat overlapped recompilation and had
a 37.820 ms full-frame mean with a 187.905 ms p99; it was rejected as a
controlled comparison and repeated, not treated as an algorithm regression.

### Isolated phase probes

These are from the same serial run. They isolate hypotheses, not replacement
renderers; **do not add them together**. The native-copy probes omit toolkit
bookkeeping, clipping, age repair and presentation. The draw probes use an
opaque constant-colour image, after the space/background probe; production
frame results above use the actual varied glyph fixture.

| Probe | Mean ms | p50 | p99 |
|---|---:|---:|---:|
| Warm `Raster::paint`, one row / 90 glyph cells | 0.163 | 0.161 | 0.197 |
| Warm `Raster::paint`, full / 2250 glyph cells | 3.982 | 3.937 | 4.470 |
| Cold full paint, including fresh Raster construction | 6.021 | 6.034 | 6.505 |
| Full background-only paint, spaces | 2.055 | 2.054 | 2.231 |
| RGBA → premultiplied BGRA loop, preallocated output | 1.749 | 1.742 | 1.965 |
| New image id: load, allocation, conversion, cache bookkeeping | 2.500 | 2.508 | 2.705 |
| Native copy, one terminal row / 50 pixel rows | 0.008 | 0.008 | 0.010 |
| Native copy, four-row band / 200 pixel rows | 0.067 | 0.068 | 0.074 |
| Native copy, full pane | 0.428 | 0.429 | 0.494 |
| Native overlapping scroll, 24 retained terminal rows | 0.387 | 0.390 | 0.401 |
| tiny-skia `draw_pixmap`, identity transform, full pane | 10.486 | 10.456 | 11.385 |
| iced draw with already cached full image | 11.053 | 11.003 | 11.781 |
| iced empty scene, clear full target | 0.468 | 0.468 | 0.523 |

Warm full paint averages 1.77 microseconds per visited nonblank cell including
background writes; the one-row result is 1.81 microseconds. Subtracting the
space case suggests roughly 0.86 microseconds per glyph cell for lookup,
clipping and mask blending. This is a differential estimate, **not** a
per-glyph timer inside Swash. Cold-minus-warm includes Raster/ScaleContext
construction as well as misses, so dividing it by 90 is not a valid isolated
Swash rasterisation measurement.

`core/raster.rs:194–201` already has a glyph cache. `:479–493` inserts an
alpha-only Swash image on a miss, keyed by `(char, bold, fg)`, with a complete
clear at 4096 entries. From that code and this fixture: **90 Swash render calls
on the first full paint, zero on each warmed full redraw**. These are exact
source-derived counts for this fixture, not instrumented counters or claims
about the live TUI. The warmup populates every key. Changing only backgrounds
does not miss. Foreground colour and bold unnecessarily multiply entries:
neither affects the Outline/Alpha render performed here. The cache-clear
cliff and colour churn can matter in other workloads. No glyph-cache rewrite
can remove the measured 10–14 ms image draw.

### The CPU image pipeline

`term/cpu_grid.rs:95–127` must keep old immutable image handles valid. Retained
layers/history normally prevent Bytes reclamation: a partial band is copied,
while a fully dirty band receives zeroed replacement storage. `cpu_bands.rs:155`
also copies each band's cell slice into a temporary Screen. The banded
`paint+handle` measurement therefore is **not just Raster::paint**.

`skia/raster.rs:107–140` caches converted images by handle id. A new id creates
a native pixel vector and traverses every pixel through channel swap and
premultiplication. `graphics/image.rs:144–152` clones the Bytes reference for
an RGBA handle, not another full RGBA allocation: do not blame image loading
for a nonexistent extra full copy. The isolated loop excludes allocation;
the real miss probe includes it and cache trimming. All term alpha bytes are
255, but the generic image pipeline must support other images.

`skia/raster.rs:43–83` composes scale/placement and calls `draw_pixmap`.
`tiny/painter.rs:469–500` constructs a `Pattern` shader and fills a rectangle;
this is not a memcpy fast path, even when the net transform is identity.
`skia/engine.rs:550–593` culls images by clip intersection and uses a mask
when the image extends beyond that clip. `skia/lib.rs:78–114` also clears
each damaged region and prepares a layer clip mask; `engine.rs:836` clears
and refills that full-sized mask. The standalone identity probe demonstrates
that fixing fractional coordinate arithmetic alone cannot remove this cost.

The CPU redraw area is not invariably the whole window. Stable band ids
bound layer damage; damage expansion/grouping explains the measured 461,250
pixels for a nominal 450,000-pixel band. `graphics/damage.rs:55–89` groups
nearby rectangles with a 20,000-logical-pixel area threshold. Full dirty
content changes every band and still touches the whole pane.

### Presentation and event scheduling

`skia/window/compositor.rs:156–215` obtains the softbuffer target, uses its age
to select layer history and redraws only the computed regions **directly into
that acquired target**. The inefficiency is the grid's route into this target;
iced does not add a final whole-window copy after `Renderer::draw`.

At `:218–219` it calls `buffer.present()` even when damage is empty. In the
Wayland backend, `softbuffer/backends/wayland/mod.rs:301–311` turns that into a
full-buffer rectangle. `:97–148` attaches, damages, commits and flushes. Thus
**CPU drawing can be partial while compositor damage is always full-window**.
The older surface-version fallback also damages everything. A narrow
`present_with_damage` patch is justified, but its compositor saving is not
part of the headless numbers. Acquiring a released buffer can also wait;
measuring only the commit call would miss that latency.

Softbuffer acquire/present time, compositor processing, PTY parsing, snapshot
capture, and input-to-display delay are **unmeasured here**, not zero. The
headless test has neither a Wayland surface nor frame callbacks. A follow-up
live capture must time acquire, draw, pre-present, commit/flush and frame
callback separately. The reported 44% CPU with two panes is workload evidence
from the request, not a baseline reproduced or explained quantitatively by
this single-pane test.

It is also wrong to say every PTY read immediately draws a full frame:

- `core/terminal.rs:105–109,602` has a bounded damage token and wake;
  `term/main.rs:390–401` coalesces pending eventfd wakes into one queued message.
  `core/terminal.rs:31–33,816–818` rearms Rio's damage latch under the grid lock.
- `Message::Wake` clears pending **before** `sync()` (`term/main.rs:424–430`).
  `sync` snapshots/paints every visible pane (`:710–758`) without consulting
  `take_damage`. Even an unchanged pane has its cursor row marked dirty
  (`core/terminal.rs:44–66`), and Raster repaints old/current cursor rows.
  Consequently, a redundant wake or output in a neighbour can do real work;
  the wake comment claiming a spurious repaint costs nothing is too strong.
- Every snapshot allocates/translates all visible cells under the grid lock
  (`core/terminal.rs:765–818`), even when the eventual row damage is sparse.
  Row flags discard Rio's finer horizontal damage. No scroll operation reaches
  `GridSnapshot`, `DamageBand` or the raster; scrolling repaints rows.
- iced processes messages and requests redraw after rebuilding UI
  (`iced-winit/lib.rs:1123–1255`). Even a logically uninteresting Wake can
  request a redraw. `listen_with` filters RedrawRequested (`term/main.rs:326`),
  so this is not evidence of an unconditional redraw feedback loop.
- Presentation **is** Wayland-frame-paced: iced calls `pre_present_notify`
  (`iced-winit/lib.rs:987`), winit requests a callback
  (`winit/platform_impl/linux/wayland/window/mod.rs:301`), and its event loop
  withholds RedrawRequested while that callback is outstanding
  (`event_loop/mod.rs:486`). But our expensive painting is in update, before
  this gate. Multiple processed wakes can paint intermediate states before
  the next presented frame. `Frame` coalesces upload damage, not those paints.

Both arms inherit that scheduling and Raster cost. The wgpu arm already has
persistent per-pane textures and damage-band uploads (`term/wgpu_grid.rs:95–127,
182–233,239–278`); its shader samples one texture, with nearest filtering and
REPLACE blending (`wgpu_grid.rs:326–355`, `grid.wgsl:27–29`). It avoids the CPU
conversion/pattern draw. It does **not** render glyphs on the GPU. No wgpu
timing is inferred from the CPU benchmark.

## Ranked implementation plan

Savings below are hypotheses in **ms per pane frame at 2250×1250**, relative
to the measured banded path unless stated otherwise. Ranges allow for
integration overhead. They overlap and must not be summed. “Both” means the
shared mechanism helps both arms; the CPU image-stage savings do not apply to
wgpu. A genuine full content change cannot be optimised away as a no-op.

| Rank / tactic | Echo saving | Full-redraw saving | Scope and files | Risk / qualification |
|---|---:|---:|---|---|
| 1. Opaque, 1:1 integer-aligned native copy fast path after existing conversion | 3–5 | 10–12 | tiny-skia; vendor `iced_tiny_skia` raster/engine, desktop Cargo patch and vendor README | Medium. Validate opacity, net transform, clipping, stride, format and physical origin; preserve generic fallback. Native copy probes establish headroom, not an integrated result. |
| 2. Paint at most once per presentation opportunity; skip clean neighbour panes | 0 for a single necessary paint; about 0.16–0.34 per redundant row paint avoided | about 4–5 per discarded full paint avoided | Both; `term/main.rs`, `frame.rs`, `core/terminal.rs`; possibly a small iced lifecycle hook | Medium/high. Preserve damage until consumed and handle hidden windows, resize, shutdown, sync-update deadlines. Burst count is unmeasured; no fixed per-present saving claimed. |
| 3. Native-format grid primitive with persistent storage; bypass RGBA handles/conversion | 0.3–0.6 after rank 1 | 3–4 after rank 1 | tiny-skia transport; `core/raster.rs` destination format, `cpu_grid.rs`, `cpu_bands.rs`, `cpu_widget.rs`, vendored renderer API | Medium/high. Keep wgpu RGBA correct; forbid mutation of historic handles. Only remove conversions, not colour semantics. |
| 4. Pass physical damage to softbuffer; suppress unnecessary unchanged commits where lifecycle permits | 0 in headless test; live saving unknown | approximately 0 for truly full damage | tiny-skia; vendored `window/compositor.rs` | Low/medium. Conservative rounded age-repair union is safe; resize/background changes full. Maintain history when skipping a present; do not advance age history for an unsubmitted buffer. |
| 5. Cell/range damage and compare final visual cells on dirty rows; separately track cursor changes | 0.1–0.3 paint; potentially 0.4–0.8 total after ranks 1/3 for a real one-cell echo | 0 for genuinely changed full content; potentially most of a redundant repaint | Both; `core/terminal.rs`, `raster.rs`, `term/frame.rs`, both grid paths | Medium. Last-painted comparison must include colours/style, cursor, dimensions, scale and invalidation. Range damage needs width/x support beyond current full-width `DamageBand`. |
| 6. Faster background spans and glyph-mask blending | 0.03–0.1 | 1–2.5 | Both; `core/raster.rs` | Medium. Background alone costs ~2.05 ms. Start with contiguous packed fills/runs, preclip glyph bounds, alpha-zero/255 shortcuts; then SIMD or pixman comparison. Preserve current rounding/clipping pixel results. |
| 7. Glyph key independent of fg/bold when outline unchanged; bounded replacement instead of clear-all | near 0 on warmed fixture | near 0 on warmed fixture; cold/colour-churn saving unknown | Both; `core/raster.rs` | Low/medium. Current bold is colour-only; future real font/style must enter the key. Do not advertise a 4 ms warm saving from this change. |
| 8. Explicit scroll operations, native `copy_within` plus exposed-row paint | 0 for ordinary echo | 0 for arbitrary full changes; ~3–4 of shared paint for a one-row scroll | Both; core terminal snapshot/raster, `frame.rs`, both grid paths; perhaps Rio adapter | High. Carry ordered operations and damage across coalesced snapshots. CPU native move is ~0.39 ms; wgpu needs safe texture copy/ping-pong or still uploads moved pixels. Damage moved destination too. |
| 9. Borrow band cell slices / reuse snapshot storage, avoid allocator churn | 0–0.1 | 0–0.5 provisional, not isolated | Both for capture, tiny-skia for band copies; `terminal.rs`, `cpu_bands.rs`, `cpu_grid.rs` | Low/medium. Profile after ranks 1–6; don't attribute all paint-minus-raster time to allocations. |
| 10. Bounded render workers for independent rows/panes | 0 or a regression | perhaps 1–2 of remaining paint with 2–4 workers | Both CPU raster stages; `raster.rs`, `frame.rs` | High. Wall-time win can increase total CPU. Cache concurrency, locks and oversubscription matter; parallelise only above a dirty-area threshold. |

For rank 2, adopt foot's quiet-period/upper-deadline idea only after connecting
paint to presentation. A 0.5 ms initial quiet period with a bounded deadline is
a reasonable experiment, not a fixed prescription: it intentionally adds
latency to isolated echo and may reduce intermediate erase/repaint work.
Do not debounce keyboard delivery or discard PTY bytes. Coalesce **render
intent**, and snapshot the latest state once. Continue processing input while
waiting for a frame callback. Preserve any Rio synchronous-update handling;
test partial writes with and without the application using that protocol.

### Rank 6 implemented (2026-09-25)

The shared Raster painter now coalesces equal-background cells within each
dirty row and fills each pixel-row span through a safe `[u8; 4]` slice and
`fill`. This removes per-cell/per-pixel background stores without adding a
dependency or requiring pointer/stride alignment. Glyph bounds are clipped
once to their cell, then mask and destination row slices are zipped. Zero
coverage skips the store; full coverage copies the foreground; intermediate
coverage retains the original unsigned integer expression and division by
255. Foreground/background constants are hoisted out of the mask loop.

Filling backgrounds before glyphs is equivalent because glyphs remain
strictly cell-clipped. Each covered pixel is visited once and still contains
its cell background, so the blend can use the hoisted background directly.
Background fills are never omitted. `destination_pixel` is the optimised
loops' single colour-order boundary for rank 3 integration; the public API
and the rank 7 cache key/eviction policy are unchanged.

The original painter is retained as test-only `paint_reference`, including
its independent RGBA stores. Differential tests compare every byte and damage
band over deterministic varied grids, scales 1/1.25/1.5/2.5, bold and wide or
combining characters (still cell-clipped), both cursor styles, partial final
rows, partial repaint sequences, odd padding and nonzero unaligned buffer
origins. Synthetic masks exercise all 256 coverage values and each clipping
edge, including fully clipped glyphs. Tests require an installed monospace
font or `TERM_SPIKE_FONT`, as existing raster tests do.

The ignored `cpu_grid::bench::raster_warm_spans_bench` sits beside the existing
phase probes. It measures the same padded 2250×1250, scale-2.5 fixture for
glyphs and spaces, with background runs of 90, 7 and 1 cells; 20 warmups and
200 samples report mean/p50/p99 milliseconds. Setup and colour changes are
outside the timer. Run it in release mode serially with the existing probes
using the reproduction command above. ("Padded" refers to the cell padding
described at the top of this report, not buffer stride; the bench stride is
exactly `width * 4`.)

Measured on cbc3 (release, serial, `--include-ignored`, commit 818d96b1),
core 108 passed and term tiny-skia 45 passed, clippy clean on both:

| Probe | Before (foot-tactics) | Rank 6 |
|---|---:|---:|
| Warm full paint, 2250 glyph cells | 3.98 ms | **1.23 ms** |
| Background only (spaces) | 2.05 ms | **0.37 ms** |
| Four-row bands, full redraw | 11.58 ms | **7.00 ms** |
| Four-row bands, echo | 1.77 ms | **1.24 ms** |

The before column is from the merge validation run on a different host, so
compare the phase probes rather than read the totals as exact; the full
redraw target of < 8 ms is met by rank 6 alone. Swash's Outline/Alpha image layout (one byte per mask
pixel) and representable grid/buffer size arithmetic remain existing caller
and library assumptions.

## The direct path and its architecture cost

There are three materially different designs:

1. **Narrow image fast path:** retain the current converted cache and immutable
   handles, but copy opaque native spans for an exactly 1:1 image. This can be
   largely contained in vendored iced_tiny_skia and fixes the biggest measured
   cost. It still pays new-id conversion and retained RGBA allocations.
2. **Native grid primitive:** retain a canonical per-pane native pixel buffer
   and copy only clipped/dirty spans into iced's acquired softbuffer target.
   This removes image ids and conversion while preserving iced for chrome and
   layout. The renderer must own a coherent versioned snapshot/damage contract;
   using a stable id whose bytes mutate would defeat normal layer diffing.
3. **Paint directly into the presented buffer:** after softbuffer acquisition,
   let a CPU grid primitive invoke the shared glyph painter on the target
   subrectangle with native BGRA/XRGB pixels and the window stride. This skips
   **both** the intermediate pane pixels and the iced image pipeline. Iced
   already renders into the acquired buffer, so no second wl_surface, transport
   or window system is required. Relative to rank 1 it can remove conversion
   and the remaining copy/allocation work; relative to today's full frame,
   roughly 15–17 ms is plausible, leaving about 4–7 ms before presentation
   with the current painter. This is a design estimate, not a measured frame.

Rank 1 is the first implementation recommendation; prototype rank 2 next.
Rank 3 is explicitly viable but is a larger ownership/lifecycle change for
only about 0.4 ms of full-pane native copy beyond a good rank-2 implementation
(plus allocation/repair differences). Measure those before taking the extra
architecture cost.

The existing tiny-skia `Primitive` supports only Fill and Stroke
(`skia/primitive.rs:4–24`); an app widget cannot simply borrow the compositor
target through today's public API. A custom CPU primitive needs a vendored
renderer extension, potentially renderer-dispatch glue, or an app-specific
compositor/renderer implementation. It must support rectangle bounds, clipping,
explicit generation/damage and safe lifetime ownership. Follow the repository's
`src/desktop/vendor/README.md` convention: pinned provenance, narrowly documented
patch, removal condition and pixel regressions. No vendoring is done here.

For direct painting, split terminal capture from painting. Produce an immutable
render plan/snapshot outside the draw callback; never hold the terminal/PTY
grid lock while waiting for presentation. The glyph cache can remain shared,
but damage/cursor/geometry state must belong to each pane **and acquired target
version**. A fresh or unknown-age target owes full valid pixels; use conservative
damage replay or copy repaired contents from a canonical previous buffer.
Pointer equality alone is not a buffer-validity proof. Exposed regions after
an overlay, pane movement, removed widgets, resize or background change also
owe repaint, even if terminal cells have not changed.

Retain iced draw order: background, grid at its widget position, then overlapping
UI as required by the layer tree. Do not paint grids after all UI and overwrite
menus. Clear only where necessary and redraw any overlay affected by damage.
Represent physical x/y, row stride, clip and native format explicitly. Handle
fractional scale by snapping as the existing widget does; test 1.25, 1.5 and
2.5 and nonzero pane origins. Keep RGBA as an explicit destination option for
wgpu or deliberately switch its texture format too; silently swapping bytes
would invert red/blue. Shared glyph-mask/cache/range-damage work survives either
choice. A native CPU destination itself is a tiny-skia-specific benefit.

## Recommendation and acceptance gates

Implement ranks 1 and 2 first, then native-format transport and precise
presentation damage. Aim first for an integrated CPU full-frame cost below
8 ms and sparse echo below 2 ms on this fixture; these are engineering targets,
not a definition of foot parity. Then optimise the remaining ~4 ms shared
painter and carry cell/scroll damage through both arms. Do not start with worker
threads, a new glyph library, or another GPU workaround.

Before calling this foot-class latency, run the same font/geometry, scale,
pane count and workload against foot, tiny-skia and wgpu on the same compositor:

- Existing byte-equality and fractional-position tests; add clipping/overlap,
  cursor crossing bands, colour extremes, resize and target-age loss cases for
  the new copy/primitive contract. Exercise partial damage with rotating targets.
- Replay a public synthetic full-screen TUI stream with fragmented erase/write
  sequences, redundant content, real changed content and sustained scroll.
  Record PTY reads, queued/processed wakes, snapshots, paints and presentations.
  Required property: bounded pending render work; clean neighbour panes do not
  repaint; no update lost at a snapshot/rearm race.
- Time softbuffer acquire separately from drawing and commit/flush; collect
  frame callback/presentation timestamps and keyboard-to-visible p50/p99.
  Confirm emitted Wayland damage is sparse for echo and includes scroll moves.
  CPU percentage alone cannot establish input latency or compositor work.
- Measure total CPU and memory as well as wall time, with one and two panes,
  steady idle and output bursts. Include colour churn/cache capacity, mixed
  glyphs and zoom. Compare wgpu upload bytes and CPU paint counts after shared
  changes; a CPU copy microbenchmark is not a wgpu performance result.

The investigation's interim wgpu recommendation applied while the CPU arm
missed a 60 Hz budget for one changed pane before presentation. The native-copy
implementation and Mark's typing retest have since restored tiny-skia as the
default. The goal is a
CPU terminal that avoids the unnecessary work, with wgpu retaining the shared
gains rather than hiding their absence.

Validation in this pass: 41 ordinary term release tests passed, two ignored
performance tests passed when explicitly selected; banded/full-image pixel
equality remains checked. The existing vendored teletypewriter unused-variable
warning is unrelated. Changes are limited to this report and the cfg(test),
ignored phase benchmark; no crate version bump is needed for unchanged runtime
behaviour.

## Implemented ranks 1, 2 and 4 (2026-09-25)

This follow-up implements the approved narrow CPU path and shared redraw
scheduling. The investigation and original measurements above describe the
pre-change tree; wgpu was still the default at this implementation stage.

- **Rank 1:** pristine crates.io `iced_tiny_skia` 0.14.1 import in its own
  commit, followed by a separate local patch. `raster.rs` records opacity
  during the existing RGBA-to-native conversion. Unit-scale, integer-placed,
  opaque images copy clipped rows into the acquired target. The rectangular
  clip uses tiny-skia's non-antialiased 26.6 edge rounding, including fractional
  damage bounds; the copy invokes neither Pattern nor a mask. Renderer-wide
  clip-mask preparation is still present for generic drawing. Fractional net
  scale/translation, rotation and non-opaque draws use the original path.
  Negative local bounds with an identity transform after image scaling also keep upstream's
  specialised rectangle rounding; tests exposed a different edge footprint
  there. Translated negative physical origins are supported by the copy.
- **Rank 4:** `window/compositor.rs` passes outward-rounded physical rectangles
  to `present_with_damage`. Damage includes both age repair and changes from
  the displayed frame, covering A → B → A with rotating buffers. Background
  changes invalidate retained histories so older buffers owe a full clear.
  Empty damage calls pre-present and commits empty damage, preserving Wayland
  callback pacing for unchanged `NextFrame` animations. Softbuffer's Wayland
  implementation changes ages and swaps buffers on every successful commit;
  history advances with it. Failed commits do not enter history. Resize/unknown age still repaint
  fully. Old Wayland surface versions can expand damage within softbuffer.
- **Rank 2:** the app's existing root input widget publishes a paint message
  from `RedrawRequested` only after a wake, for a missing frame or explicit
  invalidation. Clean chrome/hover redraws publish nothing and do not force
  another UI rebuild. iced-winit drains widget messages, rebuilds the UI,
  then draws within that same redraw; a timestamp guard prevents its retry
  from painting twice. Wake messages continue lifecycle/layout work but no
  longer snapshot or paint. Each visible pane consumes `take_damage` before
  capture; a clean existing pane skips both operations. New panes and font or
  scale invalidation still owe a paint. Hidden panes retain their core damage
  token and receive a new frame when shown. Snapshot/rearm remains under the
  grid lock, and no terminal lock is held during presentation. No quiet-period
  debounce or change to Rio's synchronous-update handling is introduced.
  Core snapshots add cursor rows only when position or visibility changes;
  old/current cursor rows remain covered, including hide/show transitions.

Changed files: desktop `Cargo.toml` and `Cargo.lock`; vendor README and
`iced_tiny_skia/{src/raster.rs,src/engine.rs,src/window/compositor.rs}`;
`apps/term/{Cargo.toml,src/main.rs,src/keys.rs}`; and
`crates/cosmix-term-core/{Cargo.toml,src/terminal.rs,src/mouse.rs}` (the mouse
file only initialises the new snapshot state in its test fixture).
Versions: cosmix-term **0.2.5**, cosmix-term-core **0.5.2**.

The vendor README records the tarball SHA-256, upstream revision, patch
removal conditions and routing/test commands. `cargo tree -p cosmix-term
--no-default-features --features tiny-skia -i iced_tiny_skia` confirms the
vendored path. Extracting the pristine import commit and comparing it recursively
against the downloaded tarball produced no differences. Cargo cannot make a
dev-dependency optional, so the two CPU test helpers are optional normal
dependencies selected by `tiny-skia`. Clean wgpu tests omit these direct test
dependencies (winit may still use tiny-skia for decorations); the CPU arm
explicitly enables softbuffer's Wayland backend.

### Before/after measurements

Release, headless, 2250×1250 pixels, scale 2.5, age 3, the same DejaVu Sans Mono
fixture and benchmark code, no CPU affinity. No builds overlapped the final
benchmark execution. These are complete runs, not best samples; host scheduling
noise is visible particularly in the before whole-image p99.

| Path | Case | Before mean / p50 / p99 ms | After mean / p50 / p99 ms | Before paint / convert / draw ms | After paint / convert / draw ms |
|---|---|---:|---:|---:|---:|
| Whole image | echo | 18.666 / 15.974 / 38.023 | 6.499 / 6.195 / 10.835 | 1.793 / 3.698 / 13.168 | 0.891 / 3.198 / 2.406 |
| Whole image | full | 23.341 / 22.818 / 35.937 | 10.035 / 9.936 / 11.087 | 5.251 / 4.932 / 13.152 | 4.943 / 2.797 / 2.290 |
| Four-row bands | echo | 6.436 / 6.428 / 6.766 | 1.671 / 1.657 / 1.865 | 0.325 / 0.416 / 5.203 | 0.305 / 0.454 / 0.333 |
| Four-row bands | full | 21.643 / 21.602 / 22.371 | 11.310 / 11.370 / 12.112 | 4.874 / 3.170 / 13.590 | 5.196 / 3.761 / 2.342 |

The app's banded path improves mean echo by **74%** and full redraw by **48%**.
Damage area stays 461,250 pixels for banded echo and 2,812,500 for full redraw.
The sparse **<2 ms** target passes; the full **<8 ms** target does not. Full
paint and new-image conversion remain material costs. The headless benchmark
does not exercise Wake coalescing or softbuffer presentation, so these timing
savings belong to rank 1; no numeric saving is assigned to ranks 2 or 4.

Before command, from `src/desktop`:

```text
cargo test -p cosmix-term --release --no-default-features --features tiny-skia tiny_skia_frame_bench -- --ignored --nocapture --test-threads=1
```

Final CPU run (includes the identical frame benchmark plus phase probes):

```text
cargo test -p cosmix-term --release --no-default-features --features tiny-skia -- --include-ignored --nocapture --test-threads=1
```

### Validation and remaining limits

- CPU: **44 passed**, including both ignored benchmarks explicitly enabled.
  Band placement now has exact-pixel coverage at scales 1.0, 1.1, 1.25, 1.5,
  1.75, 2.25 and 2.5, including nonzero origins and final partial bands;
  retained-history, cursor-band crossing, resize and invalidation tests pass.
- Default wgpu: **34 passed** with `cargo test -p cosmix-term --release`.
  This compiles the default arm and checks shared app behaviour; it is not a
  live GPU presentation or performance measurement.
- Vendor: **4 passed** using the README command. Byte equality compares native
  copy with original Pattern drawing for varied opaque colours, clipped and
  negative translated origins; forced fallback comparisons cover fractional
  clip boundaries, rotation and negative identity placement. Non-unit scales
  1.25/1.5/2.5, fractional translations, image alpha and draw opacity retain
  upstream pixels. Physical damage rounding, clamping and empty input are
  unit-tested.
- The two-pane app regression runs in both arms: 20 Wake messages leave frame
  generations unchanged; one redraw paints the dirty pane once; a redraw retry
  does not repaint; the neighbour keeps its generation and has no snapshot row
  damage from its unchanged cursor. Zoom is checked after the pre-draw paint.
- Cargo runs were restricted to term and the vendored crate as requested.
  Core changes compile through term and the clean-cursor behaviour is exercised
  by the app regression; the standalone core test suite was not run. Its
  snapshot expectations and fixture initialisers were updated with the change.
- Remaining risk: real compositor acquire/commit/frame-callback timing and
  emitted Wayland damage have not been captured. Empty-present lifecycle and
  history handling are source-audited, not validated against a live rotating
  Wayland surface. Hidden/resumed windows, synchronous-update bursts and actual
  keyboard-to-visible latency still need the live acceptance session described
  above. Existing steady-cursor policy is unchanged; visibility transitions
  still dirty cursor rows. No foot-parity or live CPU-percentage claim is made.

The existing teletypewriter unused-variable warning remains unrelated. There
was no deployment or push in this implementation. The later default decision
is recorded at the top of this report.

## T16 merge validation (2026-09-25)

Merged `term/skia-perf` at `49cd1e7a` into `term/foot-tactics`, preserving
term 0.2.5, core 0.5.2 and the then-current wgpu default. This is the historical
merge result before the review fixes and C6 integration below. Cumulative band edges feed the
vendored native-copy path directly. It resolves physical edges before image
scaling/truncation, tolerates at most 0.001 pixel of floating-point round-off,
and requires rounded extents to equal the image dimensions. The resulting
copy transform is an exact integer translation with unit scale. The former
widget origin bias is removed. The seven-scale pixel regression and an
additional vendor eligibility regression cover nonzero origins and partial
final bands. Redraw coalescing, clean-pane skipping and presentation damage
remain intact. A redraw callback type alias resolves the clippy complexity
warning without changing behaviour.

All gates used release builds, restricted to term and the vendor crate:
default term **34 passed**; tiny-skia **42 passed, 2 ignored**; both clippy
feature configurations passed with `--all-targets -- -D warnings`.
The vendor manifest's default suite passed **1 test** (plus zero doctests);
`--no-default-features --features image,wayland --lib` passed **5 tests**,
including the image-copy regressions omitted by the vendor's default features.
The final serial tiny-skia run with `--include-ignored --nocapture
--test-threads=1` passed **44 tests**, including both benchmarks. The existing
teletypewriter dependency warning remains unrelated.

Latest frame measurements: 2250×1250, scale 2.5, age 3, DejaVu Sans Mono,
20 warm-ups and 200 samples, no affinity and no overlapping builds. Both
whole-image and banded cases use the patched vendor renderer.

| Path | Case | Mean ms | p50 ms | p99 ms | Paint + handle ms | Prepare + convert ms | Draw ms |
|---|---|---:|---:|---:|---:|---:|---:|
| Whole image | echo | 6.547 | 6.364 | 7.697 | 0.901 | 3.084 | 2.558 |
| Whole image | full | 10.532 | 10.412 | 12.632 | 5.005 | 2.984 | 2.539 |
| Four-row bands | echo | 1.770 | 1.737 | 2.242 | 0.323 | 0.474 | 0.384 |
| Four-row bands | full | 11.578 | 11.528 | 12.617 | 5.259 | 3.849 | 2.461 |

Damage remains 461,250 pixels for banded echo and 2,812,500 for the other
cases. Final whole-image/banded pixels match. Mean echo remains below 2 ms;
full redraw remains above the 8 ms target. These measurements exclude live
Wayland presentation and input-to-visible latency.

Isolated phase probes from the same run (not additive frame costs):

| Phase | Mean ms | p50 ms | p99 ms |
|---|---:|---:|---:|
| Warm echo paint, 90 cells | 0.172 | 0.171 | 0.197 |
| Warm full paint, 2250 cells | 4.284 | 4.233 | 4.929 |
| Cold full paint + new raster | 6.457 | 6.418 | 6.889 |
| Background-only full paint | 2.340 | 2.326 | 3.065 |
| RGBA → native BGRA loop | 1.856 | 1.774 | 2.381 |
| Native copy, 50 pixel rows | 0.008 | 0.008 | 0.010 |
| Native copy, 200 pixel rows | 0.067 | 0.067 | 0.073 |
| Native copy, 1250 pixel rows | 0.431 | 0.417 | 0.597 |
| Native scroll, 24 terminal rows | 0.385 | 0.383 | 0.425 |
| Generic identity draw_pixmap | 10.540 | 10.456 | 11.461 |
| New image id: load + allocation + conversion | 2.855 | 2.819 | 3.294 |
| iced cached full image draw | 1.515 | 1.447 | 2.128 |
| iced empty-layer full clear | 0.516 | 0.486 | 0.720 |

## Rank 3 implemented (2026-09-25)

The tiny-skia arm now paints each persistent four-row band directly in BGRA
channel order and records a native grid primitive. `Raster::paint` and the
core `Surface` still default to RGBA for wgpu; `paint_format` selects BGRA
explicitly and invalidates all rows if the destination format changes. Only
the per-cell colour/write boundary changes; fill and glyph-blend loop structure,
rounding, glyph cache keys and cursor operations are retained for the parallel
rank-6 work. All raster output is opaque, making the native bytes already
premultiplied without a post-paint traversal.

`grid::Grid` in the vendored renderer holds immutable `Bytes` and a fresh
generation token. It shares the existing image sublayer's ordering and damage
expansion, but never enters the RGBA image cache. Unchanged bands keep their
generation. The rank-1 placement and clipped native copy are reused; generic
placement draws the native pixmap without conversion. Grid bounds, widget clip,
layer clip and damage remain distinct. Renderer-wide clear/mask work remains.
Paint-once scheduling, clean-pane skipping and `present_with_damage` are unchanged.

Storage is generation-owned, not an unsafe two-frame ping-pong. The app drops
its cached generation before requesting mutable bytes. Sole ownership permits
in-place reuse. Any retained widget or history forces fresh storage; partial
updates copy unchanged rows and rebind paint state, fully dirty bands discard
the old pixels and repaint. Old generations can outlive any number of frames.
The app retains no additional history or converted native cache. This removes
conversion/cache allocation, not every allocation in a changed frame.

Cluster gates cover the term tiny-skia suite (now the default) and clean wgpu
suite (`--no-default-features --features wgpu`), ignored CPU benchmarks
serialised, the core format regression, and the vendor suite
with `image,wayland`. The new ordinary rotating-target test compares every
repaired frame to a fresh old RGBA+convert draw at scales 1.0, 1.25 and 2.5;
it covers clipping, translucent overlays, moving panes, resize/partial final
bands, cursor changes, invalidation and age loss. The existing seven-scale
fixture checks exact physical placement against RGBA-painted reference bytes;
the rotating-target test checks old/new rendered equality. Existing
storage/generation tests retain earlier native bytes across
updates. The core test checks both cursor styles, padded strides and switching
formats with otherwise clean damage. Vendor tests cover constructor validation,
generation equality and native-copy/fallback equivalence. The vendor layer
regression also checks clean generations produce no damage, while new
generations, movement and changed clips do. A test-only copy of
the old whole-band transport preserves the frame benchmark's RGBA reference.

The grid honours its widget clip (`layout.bounds() ∩ viewport`) as well as
the layer clip, unlike ordinary images, which use only the layer clip. That is
intended: the term widget is sized to exactly `cols × cell_width`, its origin
is snapped, and the copy uses 26.6 edge rounding, so the widget clip lands on
the grid edges and crops nothing in practice.

Measured on the build cluster (release, serial, `--include-ignored`), four-row
bands at 2250×1250, scale 2.5, age 3. Both configurations passed every gate:
term-core, term tiny-skia and default wgpu suites, the vendor
`image,wayland` suite, and clippy `-D warnings` for both feature sets.

| Build | Echo mean ms | Full redraw mean ms | Full paint+handle / prepare+convert / render |
|---|---:|---:|---|
| T16 merge (foot-tactics) | 1.770 | 11.578 | 5.259 / 3.849 / 2.461 |
| Rank 3 alone (cbc2, 4b884fe5) | 0.473 | 6.384 | 4.549 / 0.001 / 1.829 |
| Ranks 3 + 6 (cbc3, 2a487869) | **0.370** | **3.742** | 1.971 / 0.001 / 1.765 |

Prepare+convert is gone, as designed; with rank 6's faster paint the full
redraw is well inside the < 8 ms target and echo is under 0.4 ms. The T16
merge row is from a different host, so the totals are indicative; the phase
columns show where the time went. The main integration retains term 0.2.5 and
core 0.6.2 without another version bump.

Unenforced assumptions/contracts: arbitrary external producers of `grid::Grid`
must supply premultiplied BGRA (the constructor validates shape, not channels);
term's opaque writes satisfy that contract. `PaintState::rebind` still relies
on a complete byte-for-byte copy, as before. Upstream iced's current single-CPU
renderer alias and layer ordering are source-audited and must be revisited on
an upgrade. The CPU-only build requires dependency feature unification not to
enable iced's wgpu arm independently of term's feature; both-feature term builds
continue selecting wgpu. Performance estimates assume the same font, hardware
and benchmark conditions as the merge
table. Headless regressions do not establish live Wayland lifecycle, timing,
memory pressure, compositor damage or input-to-visible latency. No age bound
or renderer-history release timing is assumed for storage safety.

## Review fixes and C6 landing preparation (2026-09-25)

This section and the final fractional-band measurements below record main's
image-handle implementation before the rank-3 integration described at the end.

The round-one fixes check negative origins against the transform after image
scaling, use coordinate-relative tolerance, and retain a widget-side correction
for upstream truncation. The exact-pixel regression covers seven scales with
both native copies enabled and both disabled by `reference-raster`.
Empty damage now presents an empty commit with its pre-present hook, preserving
Wayland frame pacing. Shared history tests cover first frame, buffer age greater
than one, A → B → A, background changes, resize/scale reconfiguration, empty
commits and failed presents. Varied-colour image tests cover negative scale
cancellation, scaling, translucency, no mask and a later clipped overlay.
Clean redraws publish no paint message; wakes, missing frames, resizes and
explicit invalidation arm the next redraw. The vendor README includes the
provenance table, ordered bump procedure and executable pristine/fixes checks.

C6 from `origin/main` is retained: pane/tab selectors, bounded scrollback
snapshots and `props.watch`. Combined versions are **term 0.2.5**, **term-core
0.6.2**, and main's unchanged **bterm 0.9.3**. Tiny-skia is the accepted default;
wgpu remains available with `--no-default-features --features wgpu`. This
preparation makes local commits only, with no installation or push.

The core's nested Mix end-to-end proof requires a completely clean committed
checkout; running it during merge resolution fails its provenance guard before
the fixtures run. Run that proof after committing. Live Wayland presentation
timing remains unmeasured; the pacing regression here exercises the production
history and submission path with a fake present callback.

## Final fractional-band fix (2026-09-25)

Band draw extents now use the source image's integer physical dimensions
divided by output scale. The cumulative origins and fallback truncation
correction remain. This avoids amplifying the subtraction error in lower
bands' logical heights without widening the vendor's native-copy tolerance.

The integrated 61-row pane regression checks exact reference pixels and,
with `--features raster-probe`, counts every successful native-placement copy.
All 16 bands, including the final partial band, qualify at scales 1.25, 1.5,
1.75, 2.0, 2.25 and 2.5 with 41-pixel cells and five pane offsets. Existing
1.0/1.1 and 20-pixel fixtures remain covered. The same pixel test passes with
`--features iced_tiny_skia/reference-raster`, disabling both copy shortcuts.
Temporarily removing the extent fix makes the counter assertion fail (12 of
16 native copies at scale 1.1), confirming it detects the regression.

Release gates: instrumented CPU **42 passed, 2 ignored**, clean wgpu
**34 passed**, vendor **9 passed**, and the forced-fallback pane test passed.
Clippy with `--all-targets -- -D warnings` passed for both term renderer
configurations and the instrumented vendor. The existing teletypewriter
dependency warning is unchanged. The two edited term Rust files pass rustfmt;
the crate-wide format check reports pre-existing formatting in other files.
Versions are unchanged. No further review round, deployment or push is part
of this fix.

The final serial CPU run with `--include-ignored --nocapture --test-threads=1`
passed **44 tests**, including both benchmarks, without overlapping builds
or probe instrumentation. Frame fixture: 2250×1250, scale 2.5, buffer age 3,
20 warm-ups and 200 samples; these are headless costs, excluding presentation.

| Path | Case | Mean ms | p50 ms | p99 ms |
|---|---|---:|---:|---:|
| Whole image | echo | 6.679 | 6.529 | 7.685 |
| Whole image | full | 9.807 | 9.664 | 11.879 |
| Four-row bands | echo | 1.746 | 1.677 | 2.449 |
| Four-row bands | full | 11.136 | 11.129 | 12.189 |

Whole-image and banded final pixels match. Banded echo still meets the 2 ms
mean target; full redraw remains above 8 ms. The benchmark fixture is unchanged;
the taller 41-pixel-cell regression establishes routing for the lower bands.

## Rank 3/main integration (2026-09-25)

The default tiny-skia arm retains native BGRA grid generations and
`Renderer::draw_grid`. Main's integer-extent fix is applied at grid submission:
width and height come from each grid's integer pixel dimensions divided by
output scale; cumulative band origins and snapping remain. RGBA matching,
`measure_image` cache touches, the generic image-renderer widget and
`truncating_origin` are removed from this path: grids have no image-cache entry
and native placement resolves their physical edges directly.

`raster-probe` counts successful grid native copies as well as the existing
ordinary-image shortcut. The expanded 61-row exact-pixel regression and its
every-band counter assertion are retained, including lower and partial bands
at fractional scales. The separate rotating-target regression retains the
RGBA-pipeline comparison. `reference-raster` continues to disable the two
ordinary-image shortcuts only; it does not disable grid copies or prove the
grid fallback. The vendor grid tests exercise non-native-size fallback directly.

Both histories and their measurements above are retained; none measures this
merged tree. Term stays at 0.2.5 and core at 0.6.2. Tiny-skia is the default,
clean wgpu uses `--no-default-features --features wgpu`, and enabling both
selects wgpu. Merge resolution is source-checked only; build, regression and
clippy validation of this integration remain for the cluster.

## Rank 5: cell comparison and range damage (2026-09-25)

Implemented on the ranks 1/2/3/4/6 base; **not built, tested or measured locally**.
Cluster validation remains required. Versions are unchanged. The comparison
baseline remains banded row echo **0.370 ms** (about **0.13 ms paint + 0.24 ms
render**) and full redraw **3.742 ms**; there are no new timing claims here.

`PaintState` retains the captured `Cell` values. On correctly sized dirty-row
hints, it compares `char`, final foreground/background RGB and bold using
`Cell` equality. It paints contiguous changed-cell ranges and coalesces equal
backgrounds only within those ranges. No palette, selection or semantic VT
state is reconstructed: capture-time selection colour swaps work automatically
when capture marks their rows dirty. Old/new cursor cells are independently
repainted on movement or visibility changes. A stationary cursor is composited
only over a cell repainted this call; an unchanged dirty row costs comparisons,
with no pixel writes, generation change or buffer copy.

Full invalidation covers columns, paintable rows, physical cell size, output
scale, destination format, stride, changed buffer address/length, explicit
invalidation, and replacement raster identity (including a different font/size
with identical rounded metrics). A byte-identical complete-copy `rebind` is the
explicit storage exception. Malformed damage lengths and incomplete cell rows
retain the conservative full-paint behaviour. `Screen::display_offset` is
captured under the terminal lock and carried through CPU band snapshots: an
offset change forces full rows even if all cells compare equal. Capture keeps
the cursor visible while its live row remains inside the viewport, including
at nonzero offsets; raster honours that captured visibility. An unchanged
scrolled viewport may skip identical cells despite conservative row hints.

`DamageBand` now includes physical `x` and `width`. Its legacy `byte_range`
still returns conservative full scanlines, including padding. Frame damage
coalescing preserves horizontal extents. Pending frame damage collapses to a
full rectangle above twice the row count, bounding undrained hidden/stalled
panes even when each read changes a different horizontal range. Both raster
and native-grid vertical merging search only the preceding row's open runs;
above twice the row count they fall back to full-width damaged-row bands.
Wgpu uploads narrow rectangles from the existing full-stride RGBA allocation
using x/y texture origins and byte
offsets; new textures still receive a full upload. No packing allocation or
texture-format change is introduced. GPU execution remains unvalidated here.

The tiny-skia primitive keeps a bounded cell-revision array beside each
immutable native BGRA generation. Its separate `Damage` tracker holds metadata,
never old pixels. Updating stamps uses copy-on-write if a retained generation
still owns the old array; a full overwrite replaces it with fresh stamps
without copying the old array. Damage rectangles pass from the borrowed band
slice through an iterator, without an intermediate rectangle allocation.
Comparing two generations from the same lineage finds changed ranges at any
buffer age, including A → B → A and multiple
paints/publications before a presentation. Unrelated geometry/lineage, changed
placement/clip and added/removed primitives retain full-bound damage. Equal
generations stay a constant-time comparison.

The four-row allocation remains the storage unit. A retained generation always
forces separate pixels before mutation. Partial updates copy the **complete**
band and rebind; an update proven to overwrite every cell may discard its old
pixels. This overwrite check runs only when the allocation is shared. Full
paints skip change-mask allocation/clearing/comparison and record cells row by
row while painting. Dirty rows compare and record `Copy` cells in one pass,
counting changes without later row scans or cell copies. The warm full/echo
probe prepares background changes outside the timer, matching the rank-6
probe. Row flags alone no longer authorise discarding bytes. No storage safety
depends on a maximum age, a two-frame rotation or history release timing.
The widget still draws complete immutable grids; layer diffing supplies narrow
regions to the existing clipped copy and `present_with_damage` path. Iced's
damage expansion/grouping, age repair and redraws beneath overlays can enlarge
those regions: cell damage and buffer repair remain separate concerns.

Added/extended validation for the cluster:

- 2,048 deterministic random frames per cursor style compare **every frame**
  against a fresh full paint using the retained original per-pixel painter.
  Cases include typing, colours/style and selection-like swaps, redundant
  dirtiness, cursor movement/hide/out-of-bounds, resize, partial last rows,
  scale/font/format/stride changes, replacement buffers, complete-copy rebinds,
  explicit invalidation and viewport changes. Guard bytes/padding and damage
  coverage of every changed byte are checked.
- Focused tests require one-cell damage, two disjoint ranges, unchanged cursor
  no-ops and full viewport invalidation with identical cells. Pixel assertions
  cover block and underline cursors at nonzero offsets in RGBA and BGRA.
  Tests also cover untouched full-paint change scratch, varied undrained
  ranges, continuing vertical runs and bounded checkerboard damage. Existing row,
  format, storage and frame tests use the new rectangle semantics.
- The rotating-target/RGBA comparison now checks 384 frames for each of three
  initial scales, including output-scale changes, retained historical pixels,
  skipped intermediate generations, clips/overlays, resize and unknown ages.
  It uses production `PresentHistory`, physical rectangle conversion and
  submission sequencing. A headless displayed buffer copies only submitted
  rectangles and must match the full RGBA oracle, as must each repaired target;
  only the softbuffer/Wayland commit itself is simulated.
- Vendor regressions check arbitrary-age cell stamps, reverts, narrow layer
  diffing and the physical rectangles passed to presentation. At scale 2.5,
  one 25×50 cell yields a 31×56 outward-rounded rectangle after iced's margin,
  instead of damaging the full 2250×200 storage band.
- A wgpu-side host test replays the production upload layouts into a simulated
  texture after coalesced cell/cursor paints and compares every byte with the
  current RGBA surface. It does not exercise a device or driver.
- The ignored frame benchmark retains row echo/full redraw and adds genuine
  one-cell glyph echo plus cursor-only moves across a band boundary. It prints
  the existing phase timing and damaged-area columns for both transports.
  Warm phase probes now change actual cell backgrounds so they measure paint,
  rather than the newly cheap redundant-damage path (that setup is timed).

Expected savings are structural, pending measurement: one-cell echo paints
1/90 of a row's cells and submits roughly 1,736 pixels instead of about 450,000
band pixels in the isolated fixture. Cursor-only movement paints two cells.
The previous ~0.13 ms paint phase and ~0.24 ms render phase provide headroom,
but full-band retained-storage copies, snapshot/band cell copies, mask work,
metadata comparison and grouping remain. A genuinely changed full pane still
paints/presents all cells; no full-redraw speedup is claimed.

Unenforced contracts and limits:

- Dirty-row hints must cover every captured visual change, including future
  selection changes, and consumers must not drop consuming snapshots between
  paints. Pane identity changes or undetectable same-address buffer reuse
  require explicit invalidation; external writes to retained pixels are not
  detectable. `rebind` requires an exact complete copy with unchanged layout.
- External native-grid producers must supply premultiplied BGRA and mark
  every written region before publishing with `Grid::with_damage`. Shape is
  validated; channel validity and completeness of reported damage are not.
  Term derives marks from the actual painter ranges and writes opaque pixels.
- The existing Swash Outline/Alpha mask layout and representable grid/stride
  arithmetic are still library/caller contracts. Public raster dimensions
  must remain valid nonzero font metrics. New pixel-affecting `Cell` fields
  must be captured and painted; derived equality includes them automatically.
- The existing iced CPU renderer alias, dependency feature unification and
  ordered sublayer contracts remain source-audited integration dependencies.
  Headless tests do not establish live Wayland acquire/present pacing, old
  surface-version damage expansion, compositor cost or input-to-visible latency.
  Timing estimates assume comparable hardware/font/fixture conditions.

## Unicode clusters and colour emoji

Implemented on the ranks 1–6 integration; source and formatting review only.
No local build, check, test, clippy or timing gate was run for this change, and
versions are unchanged. The existing warm full-paint (~1.24 ms), one-cell echo
(~0.16 ms) and banded full-redraw (~3.66 ms) baselines remain cluster acceptance
targets, not measurements of this implementation.

Rio `932c1a7` already provides complete grapheme extras, Unicode 17 widths,
default-on DEC mode 2027 and the four `Wide` states. Capture now copies those
states and all cell text under the grid lock. It does not recalculate widths
from fonts, Unicode-width crates or shaping advances, including when an
application disables mode 2027. Copy/paste and keyboard details are in
[term-clipboard](term-clipboard.md).

`Cell` remains `Copy`: an inline base `char`, a `u32` extended-text ID (zero for
a scalar), a byte-sized width enum and the original visual attributes. Tests
require its size to remain at most 16 bytes. Equality remains constant-time;
the scalar row comparison retains the zipped compare-and-record pass and full
paints retain `copy_from_slice`. Wide rows expand changed cells through the
union of old and new pair ownership before recording cells or producing damage.
That covers extras-only changes, spacer background changes, shifted pairs,
pair breakup and cursor width changes. Both cursor styles normalise a trailing
half to its lead and cover both cells. Incomplete or malformed pairs are clipped
to the available cell; `Spacer` and `LeadingSpacer` paint only backgrounds.

Each terminal owns a bounded interner: 4,096 entries, 1 MiB of UTF-8 text and
1,024 bytes per cluster. Immutable `Arc<str>` entries live in a copy-on-write
table carried by each `Screen`, including CPU band snapshots. Appending keeps
the generation identity and existing IDs. Saturation uses a missing-cluster
sentinel for further new entries; the next capture starts a fresh generation.
Raster state compares that generation identity even with clean damage hints,
forcing repaint when IDs are reused. Old screens retain their own text. Oversize
or unrepresentable clusters render as bounded tofu; Rio selection text remains
complete. Limits are per table: callers retaining many historical screens also
retain their historical tables. There is no global cap on caller-owned frames.

The font set is loaded once and shared across resize, including stable Swash
cache identities. Existing monospace discovery and `TERM_SPIKE_FONT` continue
to set cell metrics. Optional fallbacks are Noto Color Emoji followed by Noto
Sans Symbols 2, searched in common `/usr/share/fonts` Noto locations. Missing
or unreadable fallbacks are skipped. VS16 and wide emoji presentation prefer
the emoji face; VS15 retains normal font priority. `CharCluster::map` checks
whole-cluster coverage, ignoring joiners/selectors as independent visible-glyph
requirements. `ShapeContext` shapes the full text with advances and offsets,
then verifies the result before caching it. No font can move terminal columns.

The ASCII glyph loop, background spans and mask blend arithmetic are retained.
`paint_reference` is unchanged as the frozen ASCII oracle. Unicode has a
separate size-specific cache keyed by text, bold and cell span; the immutable
font set fixes face selection, and each entry records its chosen face identity.
Entries include negative/tofu results, never foreground or background colours.
The cache clears at 4,096 text keys or 16 MiB of pixel storage. Shaping, decoding
and resampling occur only on misses. Layers and individual bitmap dimensions
are bounded; excessive output falls back to tofu.

Colour bitmap selection uses Swash's nearest suitable strike (BestFit), then
decodes at that strike's exact ppem. CBLC image formats 17–19 establish that the
source is PNG/straight RGBA: RGB is premultiplied before a single bilinear resize
on the cache miss. Other bitmap encodings, including sbix, currently fall
through to outlines or tofu. Colour outlines are rasterised as ordered layers:
CPAL colours become premultiplied RGBA and current-foreground layers remain
alpha masks. This avoids treating Swash's composited colour-outline output as
straight PNG data, and keeps foreground changes independent of cached colours.
The supported COLR outline functionality is that provided by pinned Swash.

Colour clusters fit proportionally within the VT's one- or two-cell box and are
centred; ordinary outline text preserves the primary baseline. Both backgrounds
are filled before the lead paints. Masks and colour layers blend source-over
the actual destination, so a pair can have different backgrounds. RGBA and
BGRA use the same data and write opaque destination alpha. Both frontends
receive pair-expanded rectangles through their existing damage paths.

Added cluster gates:

- Capture, selection, parser chunk boundaries, paste, keyboard and IME tests.
- Required installed Noto Color Emoji fixtures: ZWJ, flag, skin tone and VS16
  ligatures, chromatic pixels in both halves, strict clipping, odd stride
  padding and RGBA/BGRA at scales 1, 1.25, 1.5 and 2.5. Font absence fails these
  tests loudly; separate tests exercise missing fallback and invalid IDs.
- An independent full-frame Unicode compositor uses fresh glyph caches and
  destination-driven per-pixel loops. It shares the Swash/font backend, but no
  production damage expansion, clipping loop or blending helper. Synthetic
  alpha fixtures independently check PNG premultiplication, transparent-edge
  resampling, palette colour and ordered current-foreground masks.
- The rank-5 random differential test now runs 4,096 frames per cursor style:
  2,048 against the frozen ASCII painter and 2,048 with Unicode transitions
  against the full Unicode oracle, retaining storage, padding, damage, cursor,
  resize, viewport, scale and format variations.
- CPU band tests retain old emoji generations and move pair cursors across
  bands; wgpu host tests replay a two-column upload. The ignored
  `cached_ascii_and_emoji_paint_benchmark` reports cached ASCII/emoji timings and
  asserts that warm painting causes no new shaping/resampling. Existing rank-5
  and rank-6 benchmarks remain the fixed-size performance acceptance probes.

Remaining assumptions and validation limits:

- Dirty-row hints, consecutive consuming snapshots, explicit invalidation on
  pane/storage reuse, exact-copy `rebind`, valid nonzero raster geometry and
  representable buffer arithmetic retain the existing rank-5 contracts.
- Font files are trusted local inputs and their Swash parsing/rasterisation
  contracts remain dependencies. Optional face discovery validates font headers,
  not every table. Resource checks bound retained results, not all allocations
  internal to a font decoder before it returns.
- Noto test files are discovered by their fixed filenames but their byte versions
  are not pinned in the repository. Glyph design and coverage depend on the
  installed font; the tests enforce the required sequence behaviour.
- Rio and Swash have separate segmentation tables. Shaping is per terminal cell
  cluster, not paragraph bidi or joining across neighbouring terminal cells.
  Swash may segment a long Rio cluster internally; bounded unsupported results
  become tofu rather than changing grid width.
- IME delivery, keyboard/commit deduplication and composition UI depend on
  iced/winit and the compositor. The host tests do not establish live IME,
  clipboard, Wayland presentation or GPU-driver behaviour. Performance and
  visual font quality still require cluster gates and a live desktop check.
