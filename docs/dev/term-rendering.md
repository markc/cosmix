# Terminal pixel ownership

The iced terminal's tiny-skia-only renderer keeps each pane's RGBA pixels in
`bytes::Bytes`. Its generation-keyed image handle shares that allocation;
there is no separate copy for the handle. The handle cache lives beside the
pixels, so painting can drop its own handle before calling
`Bytes::try_into_mut()`.

With no outstanding handle clones, painting reclaims the allocation without
copying and updates only damaged rows through `Raster::paint` and a per-pane
`PaintState`. In normal draw/present use, iced tiny-skia's renderer layers
(`Layer.images`) and compositor history (`surface.layer_stack`) retain handle
clones. Painting then makes one memcpy and calls `PaintState::rebind` on that
byte-for-byte copy, preserving incremental dirty-row painting. The previous
handle remains immutable. Rebinding requires identical bytes and layout;
geometry changes and raster replacement still force full repaints. An idle
generation retains its handle and buffer. If painting releases the cache but
returns no damage, the cache is rebuilt at the unchanged generation before
returning, so a populated pane cannot fall back to the 1×1 placeholder.

At rest, each pane has one app-side RGBA buffer shared with its current handle:
the separate Frame Vec from 0.2.2 is gone. This saves one buffer per pane,
not all retained copies. In addition to iced tiny-skia's premultiplied cache,
its layers and compositor history can retain up to `max_age` older buffers
after an output burst, until later redraws release them. Upstream ownership
and caching remain unchanged. Selecting
`wgpu`, including alongside the default `tiny-skia` feature, retains the core
Vec-backed `Surface`, incremental damage uploads and persistent GPU texture.

Regression tests in `cpu_grid.rs` cover allocation reuse and incremental
painting, an outstanding handle forcing a copy with incremental bands and
reference-render pixel equality, and retained handles across generations
without app-side buffer history. They also cover idle handle identity,
no-damage cache restoration, cursor damage and a grid geometry change. These
tests simulate retained handles; they do not run iced's draw/present lifecycle
or measure live allocations. The shared frame tests cover pane isolation,
clearing, zoom and raster invalidation in both arms; they do not directly
exercise terminal resizing.
