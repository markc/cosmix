# cosmix-wgpu-dmabuf

`cosmix-wgpu-dmabuf` is the Vulkan external-memory boundary used by the CosMix
desktop renderer. It keeps raw Vulkan and `wgpu-hal` objects private and exposes
owned DMA-BUF descriptors, role-specific capability queries and opaque import
tokens.

## Capture destination API

`ManualVulkanRenderer::capture_destination_bridge()` creates a
`CaptureDestinationBridge` for nested and DRM-pinned renderers. The bridge
retains the renderer's Vulkan device identity and exposes:

- `capabilities()` for exact `TRANSFER_DST` external-import queries;
- `CaptureDestinationCapabilities::query()` for one fourcc, modifier and
  extent;
- `supported_modifiers()` for the sorted, deduplicated intersection of caller
  feedback modifiers and the exact query;
- `import(drm_device, DmabufDescriptor)` for a one-plane client destination;
  and
- `retirement_adapter()` for the renderer's blocking submitted-work proof.

The capture role maps opaque XRGB8888 to `Bgra8Unorm` and XBGR8888 to
`Rgba8Unorm`. Its wgpu usage is exactly `COPY_DST`, its HAL usage is exactly
`COPY_DST`, and its Vulkan image usage is exactly `TRANSFER_DST`. This is
separate from scan-out, which remains `RENDER_ATTACHMENT | COPY_SRC`; callers
must not broaden one role to implement another.

## Ownership contract

`import()` validates that the destination's DRM `dev_t` is the renderer's
device, imports the external memory, and explicitly acquires the image from the
Vulkan `FOREIGN` queue family into `TRANSFER_DST_OPTIMAL`. The returned
`ImportedCaptureDestination` owns that acquired image and offers only its wgpu
texture, extent, format and immutable DRM metadata.

After encoding and submitting the copy, retain the imported destination and
all client buffer/file-descriptor lifetime tokens until
`WgpuWaitForSubmittedWork` proves the dependent work retired. Only then consume
the token with `release_to_foreign()`. That call submits and fences the explicit
release barrier back to `FOREIGN`.

An acquire failure means no copy may be encoded. A retirement timeout,
retirement-worker disconnection or FOREIGN release failure means the imported
image has unknown ownership and must be stranded: do not drop, reuse or report
it ready. Queue-full handling must be immediate and non-blocking. These rules
are part of memory safety, not optional error recovery.

The public scan-out bridge and its five-state compositor pool are independent
of capture destinations. Screencopy must never add a capture-held scan-out
state or place a destination token in that pool.
