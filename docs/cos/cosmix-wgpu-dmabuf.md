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
- `import(DmabufDescriptor)` for a one-plane client destination;
- `submit_release_to_foreign()` for a wgpu-queued ownership hand-back; and
- `retirement_adapter()` for exact, bounded `SubmissionIndex` proofs.

The capture role maps opaque XRGB8888 to `Bgra8Unorm` and XBGR8888 to
`Rgba8Unorm`. Its wgpu usage is exactly `COPY_DST`, its HAL usage is exactly
`COPY_DST`, and its Vulkan image usage is exactly `TRANSFER_DST`. This is
separate from scan-out, which remains `RENDER_ATTACHMENT | COPY_SRC`; callers
must not broaden one role to implement another.

## Ownership contract

`import()` imports the external memory but does not claim to validate an
allocating DRM device: a submitted `wl_buffer` carries no such identity.
`main_device()` is the real renderer identity used to build linux-dmabuf
feedback and screencopy advertisements; the compositor refuses advertisement
if that identity disagrees with its feedback renderer. Feedback steers
compliant allocators; import failure fails the frame, while a cross-device
import which happens to succeed remains a documented hardware-gated risk.

The returned `ImportedCaptureDestination` offers only its wgpu texture, extent,
format and immutable DRM metadata. `encode_acquire()` records the
`FOREIGN -> renderer` ownership barrier into the caller's wgpu command buffer,
immediately before the destination copy. The acquire and copy therefore enter
the queue through one internally synchronised `Queue::submit`.

After encoding and submitting the copy, retain the imported destination and all
client buffer/file-descriptor lifetime tokens until
`WgpuWaitForSubmittedWork::wait_for_submission()` proves that exact copy
submission retired. Only then call `submit_release_to_foreign()`. It records the
explicit release barrier in a wgpu-owned command buffer and submits through the
thread-safe wgpu queue. Retain the returned pending release until its exact
submission retires, then complete it. The capture path performs no raw
`vkQueueSubmit`, no `u64::MAX` fence wait, and no queue submission from an
externally unsynchronised Vulkan worker.

An acquire failure means no copy may be encoded. Capture retirement retries a
transient bounded timeout, but a terminal wait failure, worker disconnection or
FOREIGN release failure means the imported image has unknown ownership and must
be stranded: do not drop, reuse or report it ready. All queued jobs fail on
worker death and future screencopy frames stop advertising DMA-BUF destinations.
Queue-full handling is immediate and non-blocking. Sender drop wakes an idle
worker; shutdown can wait only for the current bounded GPU wait slice. These
rules are part of memory safety, not optional error recovery.

The automated equivalence gate proves rendering and copying on a real Vulkan
adapter with an ordinary COPY_DST texture. A real GBM allocation, imported
DMA-BUF memory, physical-driver FOREIGN ownership and cross-device behaviour
remain explicit hardware gates.

The public scan-out bridge and its five-state compositor pool are independent
of capture destinations. Screencopy must never add a capture-held scan-out
state or place a destination token in that pool.
