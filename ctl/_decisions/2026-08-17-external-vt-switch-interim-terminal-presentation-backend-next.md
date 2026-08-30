# External VT switch while active: interim terminal-but-legible; cancellable presentation backend is the fix arc

**Status:** decided (Mark ratified the session recommendation, 2026-08-17)
**Context:** defect #2 —
`_journal/2026-08-17-defect2-external-pause-root-cause.md`

## Decision

1. **Interim (now):** an EXTERNAL VT switch away from an active kms-live
   session (host `chvt`, another seat authority — anything comp did not
   initiate) remains **terminal-but-legible**: the in-flight present is
   unrescuable (Mesa ANV direct-display `vkQueuePresentKHR` has no
   timeout and its flip wait never returns after DRM master revocation —
   gdb-proven), comp dies with the named reconcile-deadline error and
   exit 1 within ~33 s. No coordinator-level mitigation ships: deadline
   tweaks, `begin_stop()`, and split-phase acknowledgement were assessed
   and rejected — the wedged thread owns the surface, queue locks and
   DRM resources any resume needs. The desktop's own VT chords use the
   SELF-SWITCH path (comp orchestrates: reconcile → suspend → close DRM
   → then switch), which is proven across 21+ live cycles and stays the
   supported way to leave a comp VT.
2. **The fix arc (active):** replace Vulkan direct-display presentation
   with a **compositor-owned, cancellable presentation backend** —
   render offscreen, scan out via DRM atomic commit/pageflip on the
   session's master fd, waitable on the DRM fd + a cancellation eventfd
   with a real deadline. Plan doc lands separately under `_plan/`; the
   ≥25-external-cycle acceptance bar is recorded in the defect journal.
3. **Upstream (parallel):** report the ANV behaviour to Mesa — direct
   display present should fail with `VK_ERROR_SURFACE_LOST_KHR` on
   revocation instead of waiting forever. Draft prepared for Mark to
   file (external account action).

## Why not wait for Mesa / why not a workaround

A vendor fix helps every consumer but is external and slow, and comp
would still be wedded to an uncancellable driver wait on every other
driver. Owning the flip makes the pause/resume machinery waitable on
fds comp controls — and likely makes seamless resume (parked slice A)
tractable, since re-lighting the connector becomes re-committing the
last framebuffer instead of rebuilding a Vulkan swapchain.

## Consequences

- desk CT / V-4 smoke runs must NOT chord away via host `chvt` while
  comp is active and expect survival, until the fix arc lands; the
  launcher's died-during-hold verdict names such deaths honestly.
- `COSMIX_DMABUF_PROBE=1` keeps its own two `wait_indefinitely()` calls
  (render.rs:4190, import.rs:1425) — never part of ordinary acceptance
  runs; the probe path is unaffected by this ruling.
- comp 0.23.1's capture repair (cos cd56597) stands independent of this
  ruling and is live-verified.
