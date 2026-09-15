# Cosmix allocation diagnostics

Source: crates.io gpu-allocator 0.28.0, checksum
`51255ea7cfaadb6c5f1528d43e92a82acb2b96c43365989a28b2d44ee38f8795`.

Patched only for opt-in Vulkan memory-block
lifetime observations. `src/vulkan/diagnostics.rs` owns the callback;
`MemoryBlock::new` and `MemoryBlock::destroy` report allocation and release.
The mapping-error cleanup path also reports release. No allocation policy,
block sizes, retention, synchronisation or lifetime decisions change.

Preserve these marked hooks when refreshing the vendor. Consumers install
the observer only when frame tracing is enabled. Callbacks must remain
bounded and must not call back into the allocator.

Record fields (`subject`, `detail`, `aux`):

- `comp_vk_block_alloc`: Vulkan memory handle, block bytes, memory-type index.
- `comp_vk_block_free`: Vulkan memory handle, block bytes, zero.

The HAL resource hooks join these blocks via `comp_vk_alloc_memory`.
Handles may be reused; analyse lifetimes in timestamp order, not as permanent
identities. Hooks installed after device creation do not inventory older blocks.

## Acknowledged deviation: unconditional `extern crate std;`

This workspace resolves gpu-allocator with `default-features = false` (no
`std` feature), yet `vulkan/diagnostics.rs` links std unconditionally for its
`OnceLock` observer. On this hosted target that is harmless; it does change
the crate's std-ness against the declared feature set, so a genuine no_std
consumer of this vendored copy would need to cfg-gate the module (see
vendor/wgpu-core/src/diagnostics.rs for the shape). Deliberate, 2026-09-16.
