---
title: Cosmix Specification Suite
chapter: 0
version: 0.3.8
amends: 0.3.7
status: draft
date: 2026-05-16
---

> **Status note (2026-04-25):** Reverted to `draft` while the substrate
> layering (SPECs 07 self-aware, 08 self-repair, 09 self-improve) is being
> drafted. The display-stack pre-commitments below — Phase A/B/C ordering,
> the Intuition split (cosmix-shell + display backend), postcard as hot-path
> protocol, specific Rust crates (smithay, winit, wgpu, taffy, cosmic-text),
> and "build the shell before the compositor" discipline — are **under
> reconsideration**. Treat them as historical exploration, not decided
> architecture, until a renderer-engine choice has been re-investigated.

# Cosmix Specification Suite

Reading guide for the Cosmix protocol and design specifications. Chapters are
ordered bottom-to-top through the stack — start at Chapter 01 (the wire
protocol that everything speaks) and work up to Chapter 19 (the design system
that governs what users see).

---

## Glossary

Terms used across the spec suite. Defined once here; chapters reference this
section rather than re-defining.

| Term | Definition | Defined in |
|------|-----------|------------|
| **ABP** | Agent Bus Protocol. Human-readable wire format using markdown frontmatter headers + body. The application-facing protocol for all cosmix IPC. | Ch01 |
| **body** | The content portion of an ABP message, after the closing `---` of the header block. *(The `ui.window`/`ui.panel`/`ui.data` body formats are historical — Ch01b/05 retired 2026-08-16.)* | Ch01 §1.3 |
| **command** | The `command:` header value identifying the message type (e.g., `noded.ping`; `ui.window` is historical — Ch01b/05 retired). | Ch01 §5.5, Ch02 |
| **gadget** | An interactive region within a rendered widget. Gadgets are HitRects — invisible rectangles the renderer uses for hit-testing and (future) accessibility. Not visible to the user. | Ch06 §2.3 |
| **headers** | Key-value pairs in the `---`-delimited frontmatter of an ABP message. Route messages, set properties, carry metadata. | Ch01 §1.2 |
| **broker** | The per-node ABP message router (cosmix-noded). Routes ABP messages between processes, manages service registry, window registry, and topic broker extension. Brokers peer with each other over WireGuard for mesh traffic. | Ch01 §9 |
| **mesh** | The WireGuard /24 network connecting cosmix nodes. All nodes in the mesh share a single trust domain. | Ch01 §10 |
| **Mix** | ARexx-inspired scripting language with native ABP keywords (`send`, `emit`, `address`, `on`). Works standalone or with a broker. | Ch04 |
| **node** | A machine running cosmix-noded, identified by its WireGuard IP and hostname. | Ch01 §10 |
| **window** | *(Historical — Ch01b/05 retired 2026-08-16; `ui.*` left ABP.)* Was a top-level UI surface created by a process via `ui.window` (canonical from 01b v0.2.0); the previous term was `panel`/`ui.panel`. Live windows are cosmix-comp surfaces (Ch16). | Ch01b §3 (retired), Ch05 §3.1 (retired), Ch06 §2.1, Ch16 |
| **postcard** | Binary Rust-to-Rust serialization format (varint-encoded, serde-based). Planned for compositor↔shell hot path and bulk mesh data transfer. Not application-facing. | Ch00 |
| **process** | Any program connected to its local broker — a Mix script, a daemon, an AI agent. Processes own application state. *(The "send ABP commands to create/update UI" role is historical — Ch01b/05 retired 2026-08-16; `ui.*` left ABP.)* | Ch05 §0.4 (retired) |
| **service** | A process that registers a name with the broker (e.g., `maild`, `toolsd`). Services are addressable by name; anonymous processes are not. | Ch01 §9, Ch02 §3 |
| **surface** | The display model's abstraction of a renderable area. Every window maps to one surface. Surfaces have identity, size, position, layer, and owner. | Ch06 §2.1 |
| **topic** | A named pub/sub channel in the broker's topic extension. Producers publish snapshots; subscribers receive fan-out deliveries. | Ch03 §3.11.1 |
| **widget** | *(Historical — Ch01b/05 retired 2026-08-16.)* Was a UI element within a window, declared via markdown code blocks (`~~~textinput id=to`). Live widgets are CTK's (toolkit docs; styled per Ch19). | Ch05 §6 (retired), Ch06 §2.2, Ch19 |

### Trust gradients — four DISTINCT ladders (don't conflate)

The suite has four "level"-shaped scales that look alike but are **orthogonal axes**; an `Ln` in one is unrelated to an `Ln`/`Tn` in another. Always qualify which one you mean:

| Ladder | Axis (what it grades) | Home |
|---|---|---|
| **T0–T3** | *Autonomy tier* — how far an agent may act without operator sign-off (propose-only → autonomous). | Ch00 Article IV |
| **L0–L4 (change class)** | *Risk class of a proposed change* — read-only learning → code-modifying/destructive. | Ch09 |
| **L0–L3 (conformance)** | *Self-observability conformance* — how far up a daemon implements: bare universals (L0), then `props.*` read surface, then `world.*` retained topics. | Ch07 (L4–L5 = mutation, Ch12) |
| **AuthPolicy capabilities** | *Per-action permission* — read / write / describe / audit on a property namespace. | Ch12 §7 |

(Reconciliation item from the 2026-06-05 spec audit: these were drifting into looking like one ladder. Until renamed, cite the home + axis.)

---

## Architecture vision (moved)

The display-stack architecture narrative — the Amiga/Intuition mapping, the cosmix-shell ↔ display-backend split, phasing (A/B/C), crate choices, and the postcard/WebRTC hot-path plans — is **under reconsideration and partly aspirational**, so it now lives in [`2026-06-05-00b-architecture-vision.md`](2026-06-05-00b-architecture-vision.md). This index keeps the stable navigation, glossary, and the protocol-boundary reference below.

---

## Protocol Boundary

Two layers, decided independently. **Application protocol** is what processes speak — almost always ABP, with rare exceptions for things that aren't application traffic at all (shared-memory function calls, real-time media). **Transport** is what carries the bytes underneath — chosen per workload from the working set, with candidates flagged for evaluation. ABP frames are portable across transports; swapping transports does not change the application protocol.

For the workload-classification view and candidate-evaluation criteria, see `_spec/2026-03-24-01-bus-wire-protocol.md` §8.

| Boundary | Application protocol | Transport | Rationale |
|----------|---------------------|-----------|-----------|
| App↔app (same node) | ABP | WebSocket via broker | Language-agnostic, debuggable |
| App↔app (cross-node) | ABP | WebSocket over WG (working); iroh transports under evaluation | Mesh-transparent; transport pluggable per workload |
| Widget↔widget (same window) | function calls | (in-process) | Same process, same memory — not a transport boundary |
| Script↔display backend | ABP | WebSocket via broker | Apps must stay language-agnostic |
| Shell↔display backend (Phase B) | postcard types | Unix socket / TCP | Rust↔Rust, 165Hz, shared types — below ABP |
| Shell↔compositor (Phase C) | postcard types | Unix socket | Rust↔Rust, frame-rate events — below ABP |
| File sync blocks | (binary content) | postcard over TCP/WG (planned); **iroh-blobs** candidate | Bulk binary; candidate evaluation pending — see SPEC 01 §8.3 |
| Audio/video calls (calld) | (media frames) | WebRTC (str0m) | Media pipeline, jitter, echo cancellation |
| Federated panels (cross-mesh) | ABP | SMTP or WebSocket | Sandboxed, origin-tracked |

The rule: if an application could ever need to send or receive it, the application protocol is ABP. Transport is then chosen per workload. Real-time media and Rust↔Rust hot paths are not application traffic and are below the ABP layer; bulk binary is application-adjacent (orchestrated via ABP, content moved via a binary transport).

---

## Chapter Format

All chapters use YAML frontmatter — `---` delimited `key: value` headers
followed by a markdown body. This is structurally identical to ABP wire format:
a spec document is a valid ABP message (the frontmatter fields parse as ABP
headers, the document body parses as the ABP body). The three-reader principle
applied to the specs themselves.

Required fields:

```yaml
---
title: <chapter title>
chapter: <number>
version: <semver or semver-draft>
status: stable | draft | retired    # retired: kept verbatim as dated history
date: <last meaningful update, YYYY-MM-DD>
supersedes: <filename or none>      # optional
amends: <filename>                  # optional — for spec deltas
companion: <filename>               # optional — for paired specs
---
```

Specs reference each other by chapter filename (e.g.,
`2026-04-07-05-amp-display-protocol.md`), not by date-prefixed names.

---

## Chapters

| Ch | Title | Layer | Status |
|----|-------|-------|--------|
| [00b](2026-06-05-00b-architecture-vision.md) | Architecture Vision — display stack (**ASPIRATIONAL**, companion to this index) | Display | v0.1.0 draft |
| [01](2026-03-24-01-bus-wire-protocol.md) | ABP Wire Protocol | Foundation | v0.6.0 draft |
| [01b](2026-04-27-01b-amp-ui-vocabulary.md) | ABP UI Vocabulary | Display | v0.2.0 **RETIRED** 2026-08-16 (`ui.*` left ABP — control-plane ADR; see Ch16) |
| [02](2026-03-29-02-bus-command-vocabulary.md) | ABP Command Vocabulary | Foundation | v0.3.0 draft |
| [03](2026-04-10-03-bus-topic-pubsub.md) | ABP Topic Pub/Sub | Messaging | v0.3.0 draft |
| [04](2026-04-13-04-mix-language-reference.md) | Mix Language Reference | Scripting | v0.3.0 draft |
| [05](2026-04-07-05-amp-display-protocol.md) | ABP Display Protocol | Display | v0.3.0 **RETIRED** 2026-08-16 (`ui.*` left ABP — control-plane ADR; see Ch16) |
| [06](2026-04-18-06-cosmix-display-model.md) | Cosmix Display Model | Display | v0.3.0 draft |
| [07](2026-04-27-07-self-aware.md) | Self-Aware — conformance + event-emission + `world.*` contract (core) | Substrate | v0.1.0 draft |
| [07a](2026-06-05-07a-activity-events.md) | Self-Aware — Activity Events (sister taxonomy, **unbuilt**) | Substrate | v0.1.0 draft |
| [07b](2026-06-05-07b-property-surface.md) | Self-Aware — Property Surface (read model; extended by 12) | Substrate | v0.1.0 |
| [07c](2026-06-05-07c-spec-distribution.md) | Self-Aware — Spec Distribution & Capability Discovery | Substrate | v0.1.0 |
| [08](2026-04-27-08-self-repair.md) | Cosmix Self-Repair Layer | Substrate | v0.1.0 draft |
| [09](2026-04-27-09-self-improve.md) | Cosmix Self-Improve Layer | Substrate | v0.1.0 draft |
| [10](2026-05-09-10-cosmix-daemon-identity.md) | Cosmix Daemon Identity | Substrate | v1.4.3 stable |
| [11](2026-05-09-11-netserva-package-install.md) | NetServa NS 4.0 — Cosmix Mesh Node Package Install (Debian 13) | Substrate | v1.0.0-rc.1 draft |
| [12](2026-05-11-12-property-substrate.md) | Property Substrate — model + conformance + rationale (core, amends 07) | Substrate | v0.2.2 draft |
| [12a](2026-06-05-12a-property-verbs.md) | Property Substrate — Verbs (`props.*`; `validate` **unbuilt**) | Substrate | v0.2.2 draft |
| [12b](2026-06-05-12b-namespace-registration.md) | Property Substrate — Namespace Registration & Schema (backends/derive **unbuilt**) | Substrate | v0.2.2 draft |
| [12c](2026-06-05-12c-authz-transport-audit.md) | Property Substrate — Authorization, Transport, Errors, Audit | Substrate | v0.2.2 draft |
| [12d](2026-06-05-12d-recovery-migration.md) | Property Substrate — Bootstrap, Recovery, Versioning, Migration | Substrate | v0.2.2 draft |
| [13](2026-06-02-13-cosmix-mesh-architecture.md) | Cosmix Mesh Architecture — Layers, Topology, Membership, and Discovery | Infrastructure | v0.4.7 draft |
| [16](2026-08-05-16-cosmix-compositor-wm.md) | Cosmix Compositor — cosmix-comp | Display | v0.1.1 draft |
| [18](2026-05-15-18-mix-citizen-runtime.md) | Mix Citizen Runtime (companion to 10) | Substrate | v0.1.1 draft |
| [19](2026-08-09-19-cosmix-design-system.md) | Cosmix Design System — the Resolved Design Graph | Display | v0.7.0 draft |

Chapters 14, 15 and 17 are reserved for not-yet-written
infrastructure/display chapters (see *Planned Chapters* below); Chapter 16
was drafted 2026-08-05 alongside the shipped cosmix-comp; Chapter 19 was
commissioned by Mark 2026-08-09 with the Resolved Design Graph adoption
(`_decisions/2026-08-09-resolved-design-graph-adopted.md`). Chapter 13
(Mesh Architecture) was
drafted 2026-06-02 from a measured testbed. Chapter 18 was drafted ahead of them
because it specifies an existing runtime surface (Mix scripts as ABP daemons)
rather than a deferred component. **Decided (SPEC 18 §10.1): Chapter 18
stays at 18 and is not renumbered.** Slot 15 ("Daemon Infrastructure") is
not back-filled with this content; the 13–17 band's disposition is downstream
of authoring 13–17, not a slot SPEC 18 pre-claims or vacates now.

### Reading Order

**Start here:** Chapter 01 defines the ABP message format — the single wire
protocol that every cosmix component speaks. This is the Exec of the stack:
understand it and you understand how everything communicates.

**Then:** Chapter 02 (command naming conventions) and Chapter 03 (reactive
pub/sub) extend the protocol with standard patterns. These are the shared
libraries that all services use.

**Scripting:** Chapter 04 defines Mix, a systems scripting language in the
ARexx tradition. Mix works as a standalone shell (filesystem, HTTP, JSON, regex,
crypto, process management) and gains ABP mesh messaging (`send`, `address`,
`emit`) when a broker is available. Like ARexx, the language is useful in both
contexts.

**Display:** the live display stack is the cosmix-comp compositor
(chapter 16) plus the CTK toolkit (the 2026-07-30 Bevy+CTK decision; CTK's
widget surface is out of chapter 16's scope). Chapters 01b and 05 — the ABP
`ui.*` display protocol — are **retired** (2026-08-16; `ui.*` left ABP at
the control-plane pivot, `_decisions/2026-07-18-amp-as-control-plane.md`)
and kept as dated history. Chapter 06 remains the display-model "Intuition"
chapter (visual language, interaction model, composition patterns); the
accepted design-system contract is chapter 19.

### Planned Chapters

These chapters don't exist yet. "Planned" means the component exists and could
be documented now. "Deferred" means the component doesn't exist yet and the
chapter will be written alongside it.

| Ch | Title | Layer | Status |
|----|-------|-------|--------|
| 04a | Mix Builtin Reference | Scripting | planned (115 builtins, currently inline in Ch 04) |
| 14 | Broker Architecture | Infrastructure | planned (was 08, then 11, then 12, then 13; renumbered) |
| 15 | Daemon Infrastructure | Infrastructure | planned (was 09, then 12, then 13, then 14; renumbered) |
| 17 | Shell | Display | deferred (Phase B) |

---

## Historical Note

The spec suite was reorganized on 2026-04-18 from date-prefixed filenames
(e.g., `2026-03-09-amp-v04-cosmix-specification.md`) to numbered chapters.
Chapters are versioned by content, not by date. Historical discussions in
`_doc/` and `_journal/` may reference the older filenames — those references
describe what was true at the time and are not updated retroactively.
