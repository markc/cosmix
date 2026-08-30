---
title: Cosmix Self-Aware Layer — Spec Distribution and Capability Discovery
chapter: 7c
version: 0.1.0
status: stable
date: 2026-06-05
substrate_layer: aware
amends: _spec/2026-04-27-07-self-aware.md (spec distribution; was SPEC 07 §5)
draws_from: _spec/2026-04-10-03-bus-topic-pubsub.md (retained topics carry world.specs.*)
---

# Cosmix Self-Aware Layer — Spec Distribution and Capability Discovery

> **Split out of SPEC 07 §5 (2026-06-05).** A distinct subsystem from the
> observability contract: how an agent fetches the specs themselves over ABP
> (`spec.get`, the `world.specs.*` retained topics, the `SPEC` universal command,
> the agent bootstrap sequence) so it can join a mesh with no out-of-band docs.
> Implemented + live (`spec.get`, `world.specs.*`). Section numbers are preserved as
> **§5.x** so existing "SPEC 07 §5.x" cross-references resolve here.

## 5. Spec Distribution and Capability Discovery

This section is the canonical Tier 1 spec-distribution contract. The live
broker implementation is `$COSMIX/src/crates/cosmix-noded/src/spec.rs` plus the
retained `world.specs.*` publisher in `noded.rs`. It is the bootstrap path for
any agent joining the mesh.

### 5.1 `spec.get` broker command

The broker (cosmix-noded) MUST implement `spec.get`:

```
---
amp: 1
type: request
to: noded
command: spec.get
args: {"chapter": 1}
---
```

Response: the spec chapter as a full ABP message — its frontmatter as
headers, its body as the body. The same parser that handles wire messages
handles spec delivery; specs and ABP messages share a format by
construction (Ch 01 §6).

`args` accepts either `{"chapter": <int>}` or `{"name": "<filename>"}`.
If neither matches a chapter under `_spec/`, response is `rc: 10` with an
error body listing the available chapters.

### 5.2 `world.specs.<n>` retained topics

The broker MUST publish each spec chapter as a retained topic named
`world.specs.<n>` (e.g., `world.specs.01`, `world.specs.07`). New
subscribers receive the current spec immediately. Spec edits trigger
republish.

### 5.3 `SPEC` universal command

A daemon MAY implement the `SPEC` universal command (extending Ch 02 §3):

```
---
to: maild
command: SPEC
---
```

Response body: the spec section relevant to the daemon's protocol surface
(typically a subset of the relevant chapter). Daemons whose entire surface
is covered by a single chapter return that chapter via `spec.get` rather
than duplicating content; `SPEC` is for daemons whose contract spans
multiple chapters or carries daemon-specific extensions.

### 5.4 Agent bootstrap sequence

A fresh agent joining the mesh:

1. `noded.ping` — confirm liveness, read `extensions` map (Ch 03 §2.6).
2. `spec.get chapter=1` — wire protocol.
3. `spec.get chapter=2` — command vocabulary.
4. `spec.get chapter=7` — this chapter, the introspection contract.
5. `noded.list` — enumerate registered services.
6. For each service: `<svc>.HELP`, `<svc>.INFO`, `<svc>.props.list`,
   `<svc>.props.describe path=<each>` — build the local model.
7. Subscribe to `world.*` — receive live state.

No SDK, no schema repository, no out-of-band documentation. The protocol
is self-describing over itself. This is the realisation of the mandate's
"legible to agents" criterion at the wire level.

---
