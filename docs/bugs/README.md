# CosMix bug reports — public record

Defects in **upstream components** (drivers, libraries, kernels, toolchains)
found and root-caused during CosMix development. Each report is a dated,
self-contained markdown file: environment, reproduction steps, evidence
(stack traces, probes), expected behaviour, and whatever workaround CosMix
adopted.

Why this exists, rather than (or before) filing with each upstream:

- **A public, timestamped record.** The git history of this repository is
  the authority for *when* each defect was found and documented. Anyone —
  including us, years later — can point here and say "this was noted on
  that date, with this evidence."
- **Organic discovery.** Reports are written with searchable error strings
  and symbol names, so other people hitting the same failure can find them,
  confirm them, and carry them upstream with skin in the game.
- **Zero-friction upstream filing, any time.** Each report is written so it
  can be pasted into an upstream tracker with minimal editing. If a report
  gets filed upstream, its status line links the issue.

Every report here is **human-verified on real hardware** before it lands:
the reproduction was run, the evidence was captured live, and the root-cause
claim was reviewed. These are not speculative or generated-and-unchecked
reports.

## Reports

| Date | Component | Title | Status |
|---|---|---|---|
| 2026-08-17 | Mesa / ANV (wsi_display) | [`vkQueuePresentKHR` never returns after DRM master revocation](2026-08-17-mesa-anv-present-blocks-after-drm-master-revocation.md) | Documented; workaround shipped (CosMix no longer uses the affected path) |
