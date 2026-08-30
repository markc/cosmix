# body_view ships the text arm; the Blitz engine arm is deferred

**Date:** 2026-07-31
**Status:** Accepted
**Scope:** `ctk` 0.47.0 `body_view` (`$COSMIX/src/desktop/ctk`), and any future HTML
rendering in CosMix Mail.

## Decision

CTK's `body_view` renders sanitised message bodies with its own block/run
projection and ordinary Bevy text ("Stage A"). The planned Blitz engine arm
("Stage B", `_plan/2026-07-30-ctk-three-widgets-prompt.md`) is **deferred**:
`RenderArm::Engine` resolves to `Text`, and no Blitz dependency enters the
workspace. Verdict recorded as FEASIBLE-WITH-CONDITIONS, not abandoned.

## Why

Blitz 0.3.0-beta.1 was not rejected on availability or integration difficulty —
it composes on corrected pins, renders headless on CPU with no wgpu, and can be
made non-fetching by construction. It was rejected on value and on bounds, both
established by running a hostile-fixture harness rather than by reading docs:

- **A UA-stylesheet-only arm is worse than Stage A.** Stage A's sanitizer strips
  author CSS entirely; feeding Blitz that same input buys embedded rasters and
  real table layout at the cost of theme integration, selection/copy and a
  usable AccessKit tree, plus a texture boundary, a beta API surface and ~70
  additional dependency nodes.
- **The reason to want Blitz is author CSS, and author CSS is the hazard.**
  Nested `display:flex` in 2 KB of markup takes 108 ms at depth 16 and 176 s at
  depth 27 — memory flat throughout, so no memory limit catches it, and
  `resolve()` is synchronous with no cancellation (`abort_signal` cancels
  fetches only).
- **Deep nesting aborts the process uncatchably.** `resolve()` recurses and
  stack-overflows at ~3.9 k nested `<div>` on an 8 MiB stack, linear in stack
  size. Bounded by a pre-parse depth cap and a sized layout thread — cheap, but
  a day-one requirement, not a later hardening.

The two-day timebox was spent proving exactly this. The plan's own instruction
was to stop and write the blocker up precisely rather than burn days silently.

## Consequences

- CosMix Mail reads mail through the Text arm. That arm is converged (17 cold
  review rounds) and is the permanent fallback regardless of what Stage B
  becomes.
- Entry criteria and the three non-negotiable day-one requirements (depth bound,
  execution boundary, property-level CSS default-deny) live in
  `_doc/2026-07-31-blitz-stage-b-spike.md`.
- The harness is preserved and re-runnable at `_lab/2026-07-31-blitz-dos/`; it
  is the first thing to re-run when Stage B is reopened, because its numbers are
  the gate.
- `_doc/2026-07-30-cosmix-desktop-options.md` has been corrected — its "Blitz is
  pre-alpha, beta slipped" line was stale.

## Alternatives rejected

- **Build a minimal engine arm now anyway.** Rejected: it would land a large
  dependency and a new abort surface for no user-visible win, and could not be
  converged inside the remaining budget. A half-converged arm shipping violates
  the standing no-partial-fixes rule.
- **Abandon Blitz outright.** Rejected: the failures are bounded by measures we
  know how to build, and the corrected pin set is recorded. This is a deferral
  with a gate, not a dead end.
