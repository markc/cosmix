# Terminal pixel ownership

The iced terminal's tiny-skia-only renderer (0.2.5) divides each pane into
four-terminal-row bands. Each band's RGBA pixels live in `bytes::Bytes`,
shared with its image handle. There is no separate full-pane pixel buffer.
Painting drops the band's own handle before `Bytes::try_into_mut()`.

With no outstanding handle clones, painting reclaims the allocation without
copying and updates only damaged rows through `Raster::paint` and a per-band
`PaintState`. In normal draw/present use, iced tiny-skia's renderer layers
(`Layer.images`) and compositor history (`surface.history.layers`) retain handle
clones. Painting a partially dirty band then makes one memcpy and calls
`PaintState::rebind` on that byte-for-byte copy, preserving incremental
dirty-row painting. A completely dirty band allocates zeroed replacement
storage without copying pixels it will overwrite. The previous
handle remains immutable. Rebinding requires identical bytes and layout;
geometry changes and raster replacement still force full repaints. An idle
band retains its handle and buffer across other bands' generation changes.
If painting releases a handle but returns no damage, `Painter::repaint` restores it
before returning. Clearing a pane drops all bands.

At rest, the bands together hold one pane's worth of app-side RGBA pixels;
the separate Frame Vec from 0.2.2 remains gone. In addition to iced
tiny-skia's premultiplied cache,
its layers and compositor history can retain up to `max_age` older buffers
after an output burst, until later redraws release them. Unchanged bands
keep their ids, limiting cache conversion and layer damage. The grid widget
touches every current image during drawing to prevent tiny-skia's cache trim
from evicting undamaged bands. It draws at physical-size/output-scale with
nearest filtering and snapped physical origins. Each band's logical top and
bottom come from cumulative physical row boundaries, so the rectangles tile
without accumulating rounding error. The vendored renderer transforms those
cumulative edges into physical pixels before upstream's image-size division
and truncation. Edges within four relative f32 epsilons (capped at 0.01 physical
pixel) of integers with exactly native-size
rounded extents use an integer-translation copy for opaque images; other
draws retain the generic path. The widget corrects round-off before upstream's
source-space truncation too, using each band's cumulative origin and pixel
size. The seven-scale pixel test also runs with both vendor copy shortcuts
disabled (`--features tiny-skia,iced_tiny_skia/reference-raster`).
No half-pixel placement bias is added.

Empty damage still submits an empty softbuffer present after `on_pre_present`,
so unchanged `NextFrame` animations retain Wayland frame pacing. Every
successful commit advances history, even if it carries no damage. Unit tests
cover first frame, older/unknown buffer ages, A → B → A, background changes,
configuration resets and empty commits. These are headless lifecycle checks;
they do not measure live compositor timing. Clean chrome redraws publish no
`Paint` message; wakes, missing frames and explicit invalidation arm painting.
The default is `wgpu`; selecting both renderer features also uses wgpu. It
retains the core Vec-backed `Surface`, incremental damage uploads and
persistent GPU texture.

Regression tests in `cpu_grid.rs` cover allocation reuse and incremental
painting, an outstanding handle forcing a copy with incremental bands and
reference-render pixel equality, and retained handles across generations
without app-side buffer history. They also cover idle handle identity,
no-damage cache restoration, cursor damage and a grid geometry change. These
tests simulate retained handles. `cpu_bands.rs` additionally checks cursor
movement between bands, accumulated damage, partial final bands and resize.
`cpu_bench.rs` checks exact physical placement at 1.0, 1.1, 1.25, 1.5, 1.75,
2.25 and 2.5 scale, including nonzero pane origins, 61-row panes and a final
partial band. It also checks that adjacent logical rectangles share an edge.
The shared frame tests cover pane isolation, clearing, zoom and raster
invalidation in both arms; they do not directly
exercise terminal resizing.

Run both feature configurations from `src/desktop`; the default test run covers
wgpu and shared behaviour, while the second compiles the CPU ownership, band,
placement and shared frame tests. The ignored timing benchmark is separate.
There is no automated CI workflow enforcing these two runs yet.

```text
cargo test -p cosmix-term
cargo test -p cosmix-term --no-default-features --features tiny-skia
cargo clippy -p cosmix-term --all-targets -- -D warnings
cargo clippy -p cosmix-term --no-default-features --features tiny-skia --all-targets -- -D warnings
```

## Headless performance gate

From `src/desktop`, run the term-only release test:

```text
cargo test -p cosmix-term --no-default-features --features tiny-skia --release -- --ignored --nocapture tiny_skia_frame_bench
```

The benchmark retains the 0.2.3 whole-pane algorithm as its baseline and runs
the production band surface and widget drawing path in the same binary.
Both run real `Raster::paint`, Bytes handles, iced image conversion,
`damage::diff`/`damage::group`, `Renderer::draw`, and a rotating three-buffer
offscreen target with three retained layer histories. This is the renderer
used by iced's `Headless::screenshot`, with compositor-style incremental
damage instead of screenshot allocation and BGRA readback on every frame.
The final banded pixels must match the whole-pane baseline byte for byte.

Each case warms 20 frames and measures 200. The target is exactly
2250×1250 physical pixels at scale 2.5: 90×25 cells padded to 25×50 pixels,
using real 13px glyphs at 2.5 scale from `Raster::new`'s resolved font
(`TERM_SPIKE_FONT` or system monospace). Record the resolved font when comparing
machines. Echo changes one row; redraw changes every row, including pixel
content, on every iteration.
This is a CPU frame-cost benchmark, not a PTY-to-display latency test.
It excludes event scheduling, the pane clip-container layer, borders, tab labels,
softbuffer buffer acquisition/presentation and the display compositor. It only
models buffer age 3; none of the excluded costs or other ages can be inferred
from these results. A one-row echo redraws its entire four-row band plus the
logical damage expansion margin, not just the changed row.

### 2026-09-25 historical banding results (before native copy)

These 0.2.4 measurements predate the vendored native-copy path. The latest
0.2.5 measurements and remaining acceptance work are in
[term-foot-tactics.md](term-foot-tactics.md#t16-merge-validation-2026-09-25).

Intel Core Ultra 5 125H, release profile, final comparison pinned to CPU 0
with `taskset -c 0` around the compiled test executable. The workstation was
running its normal desktop workload; these are wall-clock measurements, not
isolated throughput guarantees. Times are milliseconds per frame.

| Path | Case | Mean | p50 | p99 | Paint + handle | Prepare + convert | Draw |
|---|---|---:|---:|---:|---:|---:|---:|
| 0.2.3 algorithm | echo | 17.758 | 16.203 | 27.992 | 1.774 | 3.509 | 12.467 |
| 0.2.4 bands | echo | 6.690 | 6.630 | 8.877 | 0.366 | 0.456 | 5.327 |
| 0.2.3 algorithm | redraw | 22.281 | 22.929 | 30.160 | 4.878 | 4.876 | 12.517 |
| 0.2.4 bands | redraw | 22.978 | 22.398 | 35.111 | 5.053 | 3.901 | 14.013 |

Total includes layer reset/history maintenance and damage diff/grouping;
the three reported components do not include that bookkeeping. Preparation
touches image dimensions like the original image widget, so conversion is
charged before draw. Echo damages 461,250 pixels after grouping/expansion,
down from 2,812,500; full redraw still damages all 2,812,500. The first
unmodified-path run, before implementation, measured 16.686 ms echo and
18.811 ms redraw without CPU affinity. Later measurements varied with load;
the consistent result is substantially cheaper echo and no full-redraw win.

The hypothesis is confirmed: retained handles force a whole-pane copy,
each new id triggers whole-image RGBA-to-premultiplied-BGRA conversion, and
the old single image damages the whole pane. Banding bounds the first two
costs and the redraw area for echo. It does not make a genuinely changed
full-screen TUI cheap. The existing scale factors already largely cancelled;
the concrete geometry defect found was fractional-origin truncation.

**Recommendation: default to wgpu for the reported full-screen TUI workload.**
That 0.2.4 CPU implementation had not achieved foot-like latency. Full redraw was
about 23 ms for one pane before presentation, above the 16.7 ms budget at
60 Hz, and slightly slower than the whole-pane baseline in this comparison.
The banded arm remains useful for lower CPU cost on sparse updates and zero
GPU allocation. Version 0.2.5 keeps wgpu as the default while native copy,
paint-once-per-redraw and clean-pane skipping reduce CPU work. A live typing
retest remains necessary. No live foot/wgpu
latency comparison or compositor presentation measurement was made in this work.
