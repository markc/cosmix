# Cosmix Vulkan submission attribution

Source: crates.io `wgpu-hal` 29.0.4, checksum
`97ace1c17727311c22a46e4e3faf56ea6de81af99dcc839bdfb54857b94d448d`.

Downstream changes are limited to `src/diagnostics.rs`, its module declaration
in `src/lib.rs`, and timing guards in Vulkan `Queue::submit` in
`src/vulkan/mod.rs`, plus allocation hooks in `src/vulkan/device.rs`.
Keep these marked patches when refreshing the vendor.
The workspace patch and lockfile route every consumer to this same HAL.

The optional, clock-free observer brackets semaphore/fence bookkeeping and the
raw loader/driver `queue_submit` call separately. With no observer installed,
the hooks read no clocks, allocate nothing and invoke no callbacks. Existing
locks, semaphore ordering, submission arguments and error handling are retained.
The compositor supplies the existing bounded frame-trace recorder; this HAL
does not log or perform I/O. The outer core HAL span remains unchanged.

Enabled acquisition tracing performs `dup()` per plane per import and `poll()`
per plane at acquisition, inside the import-registry lock; this adds microsecond-scale
observer cost to the submit spans being measured. Lock restructuring is deferred:
measured attribution matched an independent strace capture, so this observer effect
does not distort the conclusions at current surface counts.

## Allocation identity records

The separately installed allocation observer joins resources to the vendored
gpu-allocator block hooks. It does not change allocation or retention policy.
Fields below are `(subject, detail, aux)`:

- `comp_vk_alloc` / `comp_vk_free`: resource handle, allocation bytes, kind
  (`1` buffer, `2` texture). Free marks the allocator-free request.
- `comp_vk_alloc_memory`: resource handle, backing memory handle, byte offset;
  emitted with both resource alloc and free records.
- `comp_vk_alloc_meta`: resource handle, FNV-1a label hash, HAL usage bits.
- `comp_vk_alloc_properties`: resource handle, Vulkan memory-property bits,
  dedicated-allocation boolean.
- `comp_vk_label_len`: label hash, UTF-8 byte length, zero.
- `comp_vk_label`: label hash, byte offset, up to eight label bytes packed
  little-endian, zero-padded. Repeated dictionary records are intentional;
  a trace window can decode labels without its startup prefix.
- `comp_vk_block_alloc` / `comp_vk_block_free`: see
  [`gpu-allocator/COSMIX-PATCH.md`](../gpu-allocator/COSMIX-PATCH.md).

Use timestamp-ordered lifetimes: Vulkan handles may be reused. Treat numeric
handles and hashes as full-width u64 values, not floating-point numbers.
Unobserved startup blocks and dropped trace records limit lifetime joins.
