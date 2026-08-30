---
title: Cosmix Architecture Vision (display stack) — ASPIRATIONAL
chapter: 00b
version: 0.1.0-draft
status: draft
date: 2026-06-05
companion: README.md
---

# Cosmix Architecture Vision — display stack (aspirational)

> **⚠️ UNDER RECONSIDERATION (since 2026-04-25), partly aspirational.** These
> sections were split out of `README.md` (the stable navigational index) by the
> 2026-06-05 spec audit because they are *pre-commitments* to a display-stack
> architecture — the Amiga/Intuition mapping, the cosmix-shell ↔ display-backend
> split, Phases A/B/C, postcard as the hot-path transport, and specific crate
> choices — that are **not settled and not all implemented**.
>
> **Superseded 2026-07-25 — the paragraph below describes 2026-06-05, not now.**
> `cosmix-disp-skia` and the whole `ui.*` display lane were **archived** by the
> 2026-07-18 ABP-control pivot (cos tag `amp-display-archive`); the crate has no
> source left in `$COSMIX`. The live GUI line is the separate `$COSMIX/src/desktop/`
> workspace (ctk + apps). Every `cosmix-disp-skia`-as-current claim in this file
> is historical, pending the sweep tracked in
> `_plan/2026-07-23-consolidated-backlog.md`.
>
> **Implemented reality (as assessed 2026-06-05):** the display *backend* `cosmix-disp-skia`
> runs on `cosmic-comp` via `winit` (CPU stack: softbuffer + tiny-skia +
> cosmic-text). **`cosmix-shell` does not exist**, `postcard` is not on any active
> hot path, and Phases B/C have not begun. Read everything below as direction, not
> contract; the stable glossary, protocol-boundary reference, chapter list, and
> reading order live in `README.md`.

## The Stack

Cosmix is a sovereign intelligence stack: mesh networking, mail, file sync,
desktop rendering, and AI inference — all Rust, all speaking one protocol. The
architecture draws from AmigaOS, where a single ROM held the kernel, graphics,
windowing, and scripting bus in one coherent package. Cosmix recreates that
coherence across Linux process boundaries.

### The Amiga Mapping

AmigaOS Kickstart put Exec (kernel + message passing), Intuition (windows +
gadgets), graphics.library (drawing), and layers.library (overlapping regions)
in one ROM, one address space. Crucially, Intuition was simultaneously the
compositor, the window manager, and the widget toolkit — because they shared
memory structures (RastPort, Window, Screen, Layer) rather than communicating
over IPC.

Linux + Wayland necessarily separates these concerns by process boundary. You
cannot have a single-address-space Intuition. What you can have is two
cooperating components that together provide Intuition's functionality, unified
by shared vocabulary and a clean protocol at the boundary.

| AmigaOS layer | cosmix equivalent | Notes |
|---|---|---|
| Kickstart ROM (Exec kernel) | Linux kernel + DRM/KMS + libinput | Exec's message passing ~ kernel syscalls |
| graphics.library (drawing) | wgpu + cosmic-text + tiny-skia | 2D/3D rendering primitives |
| layers.library (overlapping regions) | Wayland compositor (cosmix-comp) | "Who's on top" logic |
| Intuition (window + gadget manager) | cosmix-shell + display backend (cosmix-disp-skia today) | The split layer (see below) |
| Workbench (shell / launcher) | Native CosMix Desktop Bevy/CTK applications with semantic ABP app ports | User-visible environment |
| ARexx (scripting bus) | ABP + Mix | The direct analog |
| Applications | cosmix apps (dopus, maild, etc.) | Same layer |

### The Intuition Split

Intuition maps to **two cosmix components**:

- **cosmix-shell** — the window-management half. Surface placement, stacking,
  focus policy, workspace semantics, global shortcuts, modal/popover semantics,
  input routing to surfaces. Speaks postcard to the compositor (hot path) and
  ABP to applications (orchestration).

- **display backend** (`cosmix-disp-skia` today) — the widget-toolkit half.
  Window content rendering, widget state, hit-testing, widget-level events,
  text layout, the visual system. Speaks ABP to applications. Does not
  speak directly to the compositor.

The user experience of Intuition-like coherence is achieved through:
1. **Shared vocabulary** — both agree on what a "panel" is, what layers mean
2. **Consistent visual system** — same design tokens, typography, color palette
3. **Unified event flow** — apps don't know which component handled what
4. **ABP as the app-facing protocol** — the split is invisible to applications

### The Protocols

- **Wayland** — standard Linux display protocol between compositor and its
  clients. Third-party apps and (currently) the display backend
  (`cosmix-disp-skia`) speak Wayland to the compositor. cosmix does not
  define or extend Wayland.
- **ABP** (human-readable markdown frontmatter) — everything applications touch.
  Commands, events, UI declarations, data updates. Debuggable with `cat`.
- **postcard** (binary, Rust-to-Rust) — *planned for Phase B/C.* Compact binary
  serde (varint-encoded, no field names, ~60ns serialize) for the hot path
  between compositor and shell (60-165Hz pointer motion, frame callbacks,
  surface damage). Requires both sides to share Rust type definitions —
  unsuitable for cross-language or application-facing use. Also the planned
  transport for bulk Rust-to-Rust data transfer between mesh nodes (file sync
  blocks, binary replication) over WireGuard TCP, replacing the need for WebRTC
  data channels. Applications never see postcard.
- **WebRTC** (via str0m) — *planned, parked until calld.* Media streams
  (audio/video) for voice/video calls between mesh nodes. ABP handles signaling
  (SDP/ICE exchange); WebRTC handles the media plane. calld is the only planned
  WebRTC consumer — WebRTC's complexity (ICE/DTLS/SCTP) exists to solve NAT
  traversal and encryption, both of which WireGuard already provides. For binary
  data transfer, postcard over WireGuard TCP is simpler (no ICE negotiation,
  no double encryption, instant connection establishment).

### Encryption Layering

WireGuard provides network-layer encryption across the mesh. Cosmix-native
protocols (ABP, postcard) rely on this exclusively — no per-protocol TLS, no
certificate management, no key exchange. The WG /24 subnet is the trust domain
and WireGuard membership is the credential.

Third-party wrapped services may add redundant encryption. For example,
syncthing's BEP mandates TLS 1.2+ with mutual certificate auth (the device ID
is the certificate hash). Running syncthing over WireGuard means double
encryption — WG at the network layer, TLS at the application layer. The
performance cost is negligible (AES-NI), but the architectural redundancy is
one motivation for an eventual native sync engine using postcard over raw TCP
on WireGuard, where exactly one encryption layer handles transport security.

### What the Shell Owns

cosmix-shell is the Intuition *window management* half:

- **Surface placement policy** — where new surfaces go, what size, which workspace
- **Stacking and focus policy** — which surface is on top, keyboard focus rules
- **Workspace semantics** — how many, how users move between them
- **Global keyboard shortcuts** — Super+Tab, workspace switches, launcher
- **Panel placement** — tray, status bar, docks
- **Modal and popover semantics** — what is modal to what
- **Input routing** — compositor says "pointer at X,Y"; shell says "route to
  surface S" (in Phase B on cosmic-comp, input routing is handled natively by
  the compositor; cosmix-shell only sees what layer-shell protocol grants it)

### What the Display Service Owns

The display backend (`cosmix-disp-skia` today) is the Intuition *widget toolkit* half:

- **Window content rendering** — given a `ui.window` message, paint pixels
- **Widget state** — scroll positions, selection, column widths, focus-within-window
- **Hit-testing** — which widget did the user click?
- **Widget-level events** — mouse click at X,Y → `select` on widget `files`
- **Text layout, color rendering, the visual system**
- **Internal interactions** — scrolling, drag-resize, column-resize (don't escape the window)

The display service speaks ABP to applications and does not speak directly to
the compositor. It receives input events from the shell (or currently from
winit, which mediates the same flow via Wayland).

### The Rust Ecosystem

| Concern | Crate | Notes |
|---------|-------|-------|
| Compositor | `smithay` | Only serious Rust Wayland compositor library. Used by cosmic-comp, niri, anvil. |
| Window rendering | Bevy/wgpu via CTK | Native per-app rendering; the `ui.*` backend lane is archived under `_decisions/2026-07-18-amp-as-control-plane.md`. |
| Layout | (internal in `cosmix-disp-skia` today) | `taffy` (flexbox-like) is the planned engine for the GPU successor; not yet integrated. |
| Windowing (current) | `winit` | Transitional — will be replaced by direct cosmix-shell integration in Phase B/C |
| Hot-path IPC | `postcard` | Binary serde for compositor↔shell (60-165Hz) |
| App-facing IPC | `cosmix-lib-bus` | ABP wire format for everything else |

### Phasing

| Phase | Scope | Begins after |
|---|---|---|
| A | display backend (`cosmix-disp-skia`) matures as the widget-toolkit half of Intuition, running on cosmic-comp via winit | Current phase |
| B | cosmix-shell emerges as a `wlr-layer-shell` app on cosmic-comp — cosmix window management policy without owning the compositor | First cosmix apps mature enough to stress-test shell policy |
| C | cosmix-comp replaces cosmic-comp, completing the fully cosmix-native stack | Shell is mature and battle-tested |

**Phase B is the key insight.** Before replacing the compositor, cosmix-shell
can run as a *privileged application* using Wayland layer-shell protocols to
implement cosmix-specific window management *on top of* cosmic-comp. This is
what status bars, notification daemons, and launchers do on various Wayland
compositors. It can coexist with cosmic-comp indefinitely, and teaches exactly
what cosmix-comp must provide when the time comes.

**Phase ordering discipline:** build the shell before the compositor. Writing
the compositor first means guessing at what the shell needs. Writing the shell
first, on cosmic-comp, teaches you exactly what cosmix-comp must provide.

### Design Discipline

- **winit is transitional.** Panel lifecycle and input event flow should feel
  like cosmix abstractions with winit as implementation detail. Features that
  depend on winit's specific model will need unwinding in Phase B/C.

- **Sketch the shell↔display service protocol early.** Before cosmix-shell
  exists, sketch what ABP messages between shell and the display backend
  look like: window creation, workspace changes, surface geometry, focus
  changes. This prevents accidentally baking winit assumptions into the
  display backend.

- **Fixed widget registry is correct.** Amiga Boopsi (later Intuition) added
  extensible widget classes. cosmix goes the opposite direction: no runtime
  widget extensibility, all extensibility at the app level via ABP. This is
  right for mesh-addressable UIs where the renderer must be deterministic
  across nodes.

### The Unavoidable IPC Cost

Amiga Intuition was fast partly because window management, rendering, and event
dispatch were function calls in one address space. cosmix accepts the cost
Wayland imposes — every pointer event crosses at least one process boundary.
The mitigation is the postcard-vs-ABP split: binary serde for the hot path,
human-readable text for orchestration. The target is "fast enough that users
don't notice the difference," which is achievable on modern hardware.
