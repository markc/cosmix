# Terminal pixel ownership

The iced terminal's tiny-skia-only renderer keeps each pane's RGBA pixels in
`bytes::Bytes`. Its generation-keyed image handle shares that allocation;
there is no separate copy for the handle. The handle cache lives beside the
pixels, so painting can drop its own handle before calling
`Bytes::try_into_mut()`.

With no outstanding widget handle, painting reclaims the allocation without
copying and updates only damaged rows through `Raster::paint` and a per-pane
`PaintState`. If a widget still holds the previous handle, painting makes one
copy and invalidates the paint state for a full repaint. The previous handle
remains immutable. Geometry changes and raster replacement also force full
repaints; an idle generation retains its handle and buffer.

At rest, each pane has one shared RGBA allocation plus iced tiny-skia's own
premultiplied renderer cache. That upstream cache remains unchanged. Selecting
`wgpu`, including alongside the default `tiny-skia` feature, retains the core
Vec-backed `Surface`, incremental damage uploads and persistent GPU texture.

Regression tests in `cpu_grid.rs` cover allocation reuse and incremental
painting, an outstanding handle forcing a fresh buffer and full repaint, and
idle handle identity with an outstanding clone. The shared frame tests cover
pane isolation, clearing, resizing and raster invalidation in both arms.
