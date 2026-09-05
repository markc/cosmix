---
title: Desktop Protocol, Rendering and Control
chapter: 11
version: 0.1.1
status: draft
date: 2026-09-05
---

# Desktop protocol, rendering and control

The Cosmix compositor is a Linux Wayland compositor with Smithay protocol handling and Bevy rendering. This chapter preserves architectural direction while distinguishing it from the implementation inspected at public revision `96d12fdf`.

## Authority and the frame path

**DESKTOP-001 — Protocol authority.** The protocol plane owns surfaces, focus, input routing, committed geometry, buffer lifetime and session state. Render entities mirror that state; they do not become a competing source of truth. Presentation owns KMS submission. Keep this seam independent of scenic rendering internals.

**DESKTOP-002 — Bus independence.** Frame production must not wait for a broker response. Control ingress and observation output must be bounded. The overflow policy must match the operation: rejecting a mutating request as busy is different from dropping an observation and reporting a gap. Do not generalise the old “drop-oldest” sketch to every queue.

The implementation is inside [cosmix-comp](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/): [protocol](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/protocol/mod.rs), [scene](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/compositor_scene.rs), [presentation](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/backend/atomic_presentation.rs) and [Bus worker](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/port.rs). The earlier name `cosmix-comp-port` describes a role; there is no separate crate at this baseline.

## Implemented surface and limits

| Surface | Baseline evidence | Limit of this audit |
|---|---|---|
| Bus control | `PortCommand` variants for snapshots, watches, sets and watch state; bounded admission capacity 16 | Does not imply every old `comp.window.*`/camera/place sketch is implemented |
| Decorations | Protocol decoration state and scene rendering exist | Old claim that negotiation always selects client-side decoration is stale |
| XWayland | Feature enabled by default, runtime control present | Individual guest interoperability requires recorded runs |
| Session lock, idle and capture | Protocol handlers, capture module and probe binaries exist | Handler presence does not certify all security/lifecycle cases |
| Real KMS execution | `kms-live` feature and backend present | Feature is not enabled by default; no hardware gate was run here |
| Explicit synchronisation | Acquire/release and buffer-use modules present | Correctness depends on all paths, including revoke, destroy and reuse |

Feature defaults are in the [manifest](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/Cargo.toml): `bus`, `frame-capture`, `xwayland`. The inspected compositor version is 0.48.0. Version and dependency pins belong in manifests; a later build must record its own values.

## Control, observation and persistence

**DESKTOP-003 — Applied versus durable.** A successful in-memory property change and successful persistence are separate outcomes. The current set reply carries an optional `persisted` result for file-backed leaves; `false` must not be presented as durable success. Process-lifetime leaves need no such claim.

**DESKTOP-004 — Snapshot and event consistency.** Consumers must recognise lost observations and reconcile from a fresh snapshot. Sequence/loss information is part of usable observation, not cosmetic telemetry. A command must produce a result or a documented timeout/busy outcome; it must not disappear because an observation queue is lossy.

The [snapshot dispatcher](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/protocol/port_snapshot.rs) and [observation model](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/protocol/port_observation.rs) define the current supported read and mutation surface. Future verbs must declare input validation, authorisation, ordering and reply semantics before advertising them. Old camera/place property examples remain intended vocabulary until an implementation and compatibility decision confirm them.

## Rendering, power and containment

**DESKTOP-005 — Reactive rendering.** Static scenes should quiesce; damage, animation and client progress drive work. Measure wakeups, frame timing and idle power before claiming the floor complete. An event-driven design does not by itself prove low power.

**DESKTOP-006 — Failure containment.** Session revocation, GPU failure and shutdown require bounded handling with correct buffer/synchronisation cleanup. A failed renderer must not publish fabricated presentation success. Automatic renderer fallback is not promised until an alternate renderer and its transition contract exist.

Bevy remains the intended primary renderer. Alternate software/utility rendering and direct scanout require a demonstrated deployment, reliability, latency or power need. Preserve renderer-independent geometry and presentation ownership so an alternate can be introduced without changing protocol authority. The old one-wedge-per-month tripwire was a design heuristic, not a measured current service objective.

## Window management direction

**DESKTOP-007 — Geometry ownership.** The intended canvas is bounded and elastic per output. Places are semantic regions/bookmarks; a camera selects the view. Layout remains 2D plus semantic layers. Rendered depth may animate the view, but at rest surfaces return to a flat, pixel-correct presentation. Input transformations must follow the authoritative geometry, including motion caused by camera movement.

This remains a design contract, not a statement that the complete canvas, place lifecycle, session restore or cross-output reparent API has landed. Planned policy includes floating/snap behaviour and optional per-place tiling, with policy exposed through ordinary control operations. Place identity, collisions in imported arrangements, edge gestures and multi-output behaviour need explicit API decisions and live evaluation.

Retained edge-push policy is default-off and requires a dwell threshold and corner deadzones. Parameter values and integration still need explicit decisions and live evaluation; this is not an implemented-feature claim.

XR, depth-composited application scenes and scene-switchboard clients are research directions. They impose no requirement to build a new display transport or 3D storage model now. Preserve the phase-3 constraint: no architectural decision may assume exactly one flat camera/output; the camera pose-set model must remain capable of plural views. This is a design constraint, not a requirement to implement XR now.

## Trusted surfaces and guests

**DESKTOP-008 — Trusted presentation.** Session lock and consent require compositor-enforced ordering and input isolation. Ordinary client appearance cannot establish authority. A lock protocol implementation is distinct from the full intended compositor-drawn consent/palette experience. Capture while locked and client disconnect during lock require explicit fail-closed tests.

**DESKTOP-009 — Furniture isolation.** Panels, docks, notifications and pagers must remain separate client processes; furniture must never move in-process into the compositor. Preserve crash isolation and independent release cadence. Shared design tokens provide coherent appearance without removing this boundary. This does not reclassify compositor-owned trusted lock/consent surfaces as furniture. Guest toolkits remain compatible through standard protocols without requiring native CTK styling.

The old shell-before-compositor sequence and remote `ui.*` drawing protocol are superseded. Wayland carries desktop protocol interactions; Bus carries semantic control and observation. Existing protocol handlers do not establish complete portal, workspace, multi-seat or guest support. Enumerate support by actual advertised version, implemented semantics and interoperability evidence.

## Validation gates

**DESKTOP-010 — Environment-specific evidence.** Record revision, features, toolchain, GPU/driver, session backend and client versions for live acceptance. Run protocol/model tests without assuming a GPU; separately test the default, no-default and `kms-live` feature configurations relevant to a change. Compilation is not a substitute for live presentation.

Required live scenarios are first presentation, SHM and dmabuf clients, explicit-sync ownership, VT revoke/resume, output change, idle/wake, XWayland lifecycle, locked capture refusal, shutdown and recovery. Flood and disconnect the broker while checking frame progress and bounded resources. Preserve logs and captured evidence; do not call a skipped fixture conformant.

Source entry points include [protocol tests](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/protocol/tests.rs), [lock probe](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/bin/cosmix-lock-probe.rs) and [screencopy probe](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/crates/cosmix-comp/src/bin/cosmix-screencopy-probe.rs). These were inspected, not executed. The release ledger must supply actual results before promoting defaults or claiming daily-drive readiness.
