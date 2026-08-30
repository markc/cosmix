# cosmix-wgpu-dmabuf

MIT-licensed CosMix bridge for importing Wayland DMA-BUF buffers into Bevy
0.19 / wgpu 29 on Vulkan.

The implementation was written against the local wgpu, wgpu-hal, Vulkan and
Bevy APIs. Its architecture was informed by
[`Schmarni-Dev/bevy-dmabuf`](https://github.com/Schmarni-Dev/bevy-dmabuf) at
commit `4dc77184620bfa3c9dbc4d1a1baef33501506242`: manual Vulkan creation,
external-memory image import, wrapping through `texture_from_raw`, replacing
Bevy `GpuImage` assets, and queue-family ownership barriers.

No upstream source text is copied. The upstream manifest declares
`MIT/Apache-2.0`; the inspected checkout did not contain licence files, so it
was used only as conceptual prior art.

Each Wayland surface retains one Bevy `Image` asset ID. A new client commit
replaces that asset's Vulkan texture view and clears Bevy 0.19's sprite image
bind-group cache before `PrepareBindGroups`. Imports are cached strongly by a
compositor-assigned `wl_buffer` identity: a four-buffer client ring imports each
buffer once, then reuses the same wgpu texture while the client updates the
aliased memory in place. `wl_buffer.destroy` evicts that identity; an active
render use retains its backing until normal replacement or teardown, and any
later dirty-recovery replay of still-attached destroyed content is deliberately
non-cacheable.

Every committed use acquires its image from `VK_QUEUE_FAMILY_FOREIGN_EXT`
after the protocol's implicit- or explicit-sync gate, preserving `GENERAL`
contents while making foreign writes available to shader reads. The compositor
then keeps local ownership while that content remains displayed, so static
render updates sample the locally owned `SHADER_READ_ONLY_OPTIMAL` image with
no recurring ownership barriers. Replacement or teardown releases the retired
use back to `FOREIGN_EXT` before its protocol release fires. The device enables
`VK_EXT_queue_family_foreign`; `VK_QUEUE_FAMILY_EXTERNAL` is not used because
Wayland does not constrain the producer to another Vulkan instance on the
compositor's physical device and driver.

Release leases remain per commit, not per cached import. Replacement requests
retain the previous `Applied` use until the new import or cache lookup succeeds;
a failed request drops only its new callback and restores the old use to the
locally owned display set. Both explicit and implicit leases wait in the
ownership-retirement queue until bind-group invalidation and the render-cleanup
release barrier have completed. The cached backing may then stay alive for a
later ring reuse, but it is foreign-owned while idle.

Release is fail-closed. If the local-to-`FOREIGN_EXT` barrier fails, ownership
and layout are unknowable: the cache entry is evicted, but the retired use and
its callback are deliberately leaked for the rest of the process. Neither
`wl_buffer.release` nor an explicit release point is published without a
completed handback. Terminal compositor-App teardown applies the same rule to
every still-displayed or cached use before either Bevy world is dropped; those
callbacks remain silent while the session proceeds to disconnect its clients.

Stock wgpu-core 29.0.4 records `create_texture_from_hal` textures as
`UNINITIALIZED`, making the first sample derive Vulkan `UNDEFINED` and allowing
the client contents to be discarded. The workspace patches wgpu and wgpu-core
with `create_texture_from_hal_with_initial_usage`; this importer passes
`TextureUses::RESOURCE`, matching the completed
`GENERAL -> SHADER_READ_ONLY_OPTIMAL` raw acquire barrier. The ordinary wgpu
HAL-import API remains unchanged and still seeds `UNINITIALIZED`. Provenance,
the exact patch boundary and the mandatory `cargo tree` assertion are recorded
in `vendor/README.md`.

The registration itself is owned by the main-world surface. Render extraction
may temporarily have no `GpuImage`; that defers import rather than removing the
registration. Unmap, destruction, or switching away from DMA-BUF explicitly
unregisters it and retires the final buffer.

Current scope is alpha-bearing ARGB/ABGR desktop surfaces. XRGB is deliberately
not advertised because Bevy's alpha-blended sprite path cannot force its
undefined X channel opaque without a separate sampling pipeline. Multi-planar
YCbCr formats are also not advertised. H3a/H3b track explicit-sync use eviction
and retire release points behind a bounded wait for submitted wgpu 29 work, but the
client-visible explicit-sync global remains disabled until H4. The advertised
Rung D path still relies on Mesa implicit DMA-BUF synchronisation plus Vulkan
external queue-family ownership transfers.
