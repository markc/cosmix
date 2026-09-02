# anv/wsi_display: `vkQueuePresentKHR` never returns after DRM master revocation (VT switch)

**Found:** 2026-08-17, during development of
[cosmix-comp](https://github.com/markc/cosmix), the CosMix Wayland compositor.
**Status:** documented here; not yet filed upstream. CosMix's own exposure is
closed — the compositor retired its `VK_KHR_display` presentation path
entirely (2026-08-18) in favour of compositor-owned KMS atomic commits with
cancellable, absolutely-deadlined waits.
**Severity for affected users:** an externally-initiated VT/seat switch
permanently wedges any direct-display Vulkan client with an in-flight
present; the only recovery is killing the process.

## Summary

When a Vulkan application presents through a `VK_KHR_display` (direct
display) swapchain and the kernel revokes its DRM master (a VT switch
initiated by another session), an in-flight `vkQueuePresentKHR` blocks
forever inside ANV's display WSI flip wait instead of failing with
`VK_ERROR_SURFACE_LOST_KHR`. The wait has no timeout and no cancellation
path, so the presenting thread — and everything serialized behind it (queue
locks, the swapchain, the surface) — is lost for the life of the process.

## Environment

- Mesa 26.1.6 (Arch `mesa 3:26.1.6-1`, `vulkan-intel 3:26.1.6-1`)
- Intel Meteor Lake-P / Arc Graphics `8086:7d55`, Xe KMD
  (the "Support for this platform is experimental with Xe KMD" path)
- Kernel 7.1.8
- Wayland compositor presenting via `VK_KHR_display` (direct display),
  wgpu/wgpu-hal 29 on top of ANV; compositor holds DRM master on its VT

## Steps to reproduce

1. Compositor acquires DRM master on a VT and presents through a
   `VK_KHR_display` swapchain (FIFO), steady 60 fps.
2. From another session, switch the VT away (`chvt N` — the kernel revokes
   DRM master / the seat pauses the device). The compositor has an in-flight
   `vkQueuePresentKHR`, or issues one before it can observe the pause.
3. That `vkQueuePresentKHR` call never returns.

## Observed

The presenting thread is stuck indefinitely in ANV's display WSI flip wait.
Stack of the blocked thread (release build, dynamic symbols):

```
#0 usleep                                (libc)
#1-#2 ?? ()                              (libvulkan_intel.so — wsi_display flip/event handling)
#3 drmHandleEvent                        (libdrm.so.2)
#4 ?? ()                                 (libvulkan_intel.so — event loop)
```

The flip event it waits for can never arrive: scanout stopped when master
was revoked, and re-acquiring the VT later does not deliver it either (the
flip belonged to the revoked master epoch). The wait is unbounded — no
timeout, no cancellation — so the thread never comes back.

## Expected

When the DRM event source is dead because master was revoked, present
should complete with `VK_ERROR_SURFACE_LOST_KHR` (or
`VK_ERROR_OUT_OF_DATE_KHR`) so the application can retire the swapchain and
rebuild after re-acquiring the seat. An unbounded wait inside
`vkQueuePresentKHR` makes seat switching unsurvivable for any direct-display
client with an in-flight present, because the revocation *races the client's
own pause handling by design*: the kernel revokes first, the client learns
second. No application-side ordering can close that window.

## Notes

- Reproduces deterministically (2/2 and further repeats) with an in-flight
  FIFO present at the moment of revocation. Diagnosed live with gdb against
  the blocked process.
- The compositor's own orchestrated VT switches (drain presents first, then
  switch) work fine — only externally-initiated revocation hits the window,
  which is exactly the case the application cannot order around.
- Workaround adopted by CosMix: stop presenting through `VK_KHR_display`
  altogether. cosmix-comp now renders offscreen into GBM-allocated scanout
  buffers and issues its own KMS atomic commits (`NONBLOCK` +
  `PAGE_FLIP_EVENT`), waiting on the DRM fd and a cancellation eventfd with
  an absolute deadline — every wait is bounded and cancellable, so master
  revocation is survivable at any instant.
- Full traces, an strace of the wait loop, and patch testing on this
  hardware are available on request.
