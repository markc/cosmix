---
title: Core-and-Citizen Crate Pattern
date: 2026-04-26
status: directional
next_review: 2026-07-26
draws_from: ["CLAUDE.md", "_spec/2026-04-20-00-constitution.md", "_spec/2026-04-13-04-mix-language-reference.md", "_spec/2026-04-27-09-self-improve.md"]
tags: ["architecture", "crates", "autoresearch", "mix", "self-improve", "decision-record"]
---

# Core-and-Citizen Crate Pattern

A directional architectural convention for how Cosmix Rust crates are organised
so that each unit of code is locally testable, individually benchable, and
amenable to per-crate autoresearch loops — without requiring a running mesh.
This document is *not* canon. It is the design memo behind a single line in
`CLAUDE.md`. If a second worked example accumulates, this memo can graduate
into a numbered spec chapter.

## The pattern

Every Cosmix library crate (and binary, where it makes sense) splits into two
configurations along a feature flag:

- **core** — pure logic. No dependency on `cosmix-lib-bus`,
  `cosmix-lib-client`, or `cosmix-lib-config`. `cargo test
  --no-default-features` passes. The crate compiles, tests, and runs in
  isolation, with no awareness of the mesh, the broker, the on-disk Cosmix
  config tree, or any ABP wire format.

- **citizen** — adds ABP/mesh integration behind a single feature flag
  (recommended name: `cosmix`, though the existing `mix` binary already uses
  `amp` and that's fine). The feature pulls in some subset of the ABP
  trio (`cosmix-lib-bus` for wire format, `cosmix-lib-client` for broker
  WebSocket transport, `cosmix-lib-config` for typed settings + path
  conventions) and exposes the crate's functionality as a mesh citizen:
  registered port, ABP-addressable, config-loadable, observable.

The default build is citizen. `--no-default-features` strips back to core.
Binaries follow the same convention: the default `cargo build` produces the
mesh-integrated daemon or tool; `cargo build --no-default-features` produces a
standalone executable that exercises the same logic without any mesh side
effects.

Some crates are inherently mesh infrastructure — `cosmix-lib-mesh`,
`cosmix-lib-daemon`, `cosmix-noded` — and the split is meaningless because
the crate's whole purpose is the mesh. That's fine. Name them as such and
exempt them. The pattern applies to crates with separable logic, not to the
substrate of the mesh itself.

## Why this serves the mandate

The Cosmix mandate (see `CLAUDE.md`) names three design criteria. The
core/citizen split scores on all three:

1. **Legibility.** A core crate is far easier for a fresh agent session to
   reason about. No mesh side effects to trace, no implicit `cosmix_config`
   reads inside constructors, no IPC envelopes leaking into function
   signatures. The boundary makes it obvious which code is domain logic and
   which is integration.

2. **Modifiability.** This is the load-bearing criterion. A core crate that
   compiles and tests in isolation is a crate an agent can iterate on with a
   tight feedback loop. The agent edits, builds, runs the corpus, gets a
   number — without standing up the mesh, the broker, the config store, or any
   surrounding daemon. That feedback loop is what makes Karpathy-style
   autoresearch (`_doc/2026-04-27-autoresearch-loop-template.md`) tractable per crate. Every crate that adopts
   the pattern becomes autoresearch-eligible.

3. **Reconstructibility.** Each crate becomes individually rebuildable in
   isolation. The dependency graph compresses around the feature flag: a core
   build of `cosmix-lib-X` has fewer transitive deps, builds faster, and is
   safer to swap in and out.

The pattern is hex-arch / ports-and-adapters with a Cosmix-specific shape.
The novelty isn't the pattern — it's that "the edge" has a single named
target (the ABP mesh) and the discipline is in service of agent-operability
rather than testability per se.

## How the seam already exists

`cosmix-lib-mix` is the canonical core-shaped library in this workspace:
zero Cosmix-internal deps, only generic crates (`indexmap`, `regex`,
`serde_json`, etc.). It tests standalone with `cargo test
--no-default-features` and is reusable outside the mesh as a pure-Rust
scripting library.

`cosmix-mix` (the `mix` binary) was the original prototype of the
core/citizen split, gating `cosmix-lib-bus` and `cosmix-lib-client`
behind a `default = ["amp"]` feature. That feature was collapsed during the
2026-05 carve-out: the binary now ships in one flavour with unconditional
ABP/citizen-runtime dependencies. The bare-vs-mesh distinction is a runtime
concern implemented by the live runtime probe in
`$COSMIX/src/crates/cosmix-mix`; the current dependency contract is recorded in
that crate's `Cargo.toml`. The split-at-the-library-level discipline survives
in `cosmix-lib-mix`; the two-binary-flavour `mix` was retired because the
runtime probe provides the same bare-vs-mesh property without the build-matrix
cost.

The convention this doc establishes still applies: new library crates
default to core, with mesh integration behind a feature. Binary
flavour splits are evaluated case-by-case — `mix`'s collapse is the
intended worked example of substituting a runtime probe for a
build-time gate when the gate's only job was "is the mesh available."

## Why merge wasn't chosen

A natural-looking refactor is to merge `cosmix-lib-bus` + `cosmix-lib-client`
+ `cosmix-lib-config` into a single `cosmix-lib-ext` crate. Rejected, because
the three have meaningfully different dimensions of independence:

- `cosmix-lib-bus` is a wire format — small, stable, useful alone (e.g.
  an HTTP service that speaks ABP envelopes but doesn't join the mesh).
- `cosmix-lib-client` is the WebSocket transport — some apps want envelopes
  without the mesh client.
- `cosmix-lib-config` is typed settings + paths — some apps want path
  helpers without the Settings registry.

Merging trades fine-grained dependency control for naming clarity. The
preferred approach is to keep the three crates separate and let each
adopting crate gate them collectively behind its own single feature. This
gives uniform vocabulary across the stack ("a crate is core or citizen")
without forcing a merge that loses fine-grained control elsewhere.

## What the convention is *not*

- It is not a constitutional invariant. It does not belong in
  `_spec/2026-04-20-00-constitution.md`. The constitution governs sovereignty and
  autonomy boundaries; this is engineering style.
- It is not a retrofit mandate. The 23 existing crates are not converted on
  principle. Conversion happens when a concrete second use case appears
  (autoresearch on a second crate, an isolated test harness, an alternate
  frontend, a WASM target).
- It is not universal. Crates whose entire reason for existing is mesh
  infrastructure are exempt. Forcing the split there produces an empty core
  layer and a meaningless citizen layer.
- It is not a numbered spec yet. It graduates to one when at least one
  additional crate has adopted the pattern with measurable benefit, so the
  spec can be written from two worked examples rather than one
  generalisation.

## The triggering use case: autoresearch on Mix

The immediate motivation is `_doc/2026-04-27-autoresearch-loop-template.md`: a Karpathy-style keep-or-revert
loop that mutates the Mix interpreter's edit zone (`src/eval.rs`,
`src/interp/*.rs` in `cosmix-lib-mix`), runs a fixed bench corpus + the test
suite, scores `bench_ms + failed_tests * 60_000`, and either keeps or
reverts. The loop runs hundreds of experiments overnight on a dedicated
branch.

For this to work, the bench harness must be cheap and side-effect-free.
Booting ABP, joining the mesh, loading config, registering a port — all
noise the autoresearch loop doesn't want and shouldn't measure. The
core/citizen split lets the loop run against `--no-default-features` (or
against a forked `$COSMIX/` repo containing only `cosmix-lib-mix` plus a
stripped binary), with the ABP citizen path untouched in the canonical
Cosmix tree.

If autoresearch on Mix produces measurable wins, the same harness shape
applies to:

- `cosmix-lib-llm` — token streaming throughput, prompt assembly cost.
- `cosmix-lib-skills` — retrieval ranking, vector search latency.
- `cosmix-lib-display` — layout computation under realistic widget trees.
- `cosmix-indexd` core — search query latency over a fixed corpus.

The pattern earns its place in proportion to how many crates become
autoresearch-eligible because of it.

## Drift management

The risk of a forked `$COSMIX/` (if that path is taken instead of just
exploiting `--no-default-features` in-tree) is divergence from canonical
Cosmix. Mitigation: after each autoresearch run, cherry-pick the kept
commits back into Cosmix as a single squashed PR, with the run's
`results.tsv` as evidence. The fork is a sandbox, not a parallel universe;
the canonical tree is always the source of truth.

If the loop runs cleanly against `cargo build --no-default-features` in
the main tree, no fork is needed and the drift question dissolves. The
fork option exists for the case where ABP-related dependencies leak into
the build graph in ways that confound the metric.

## Costs the convention imposes

- **CI matrix doubles** per adopting crate: `cargo test` and `cargo test
  --no-default-features` both have to pass.
- **API discipline**: no ABP types in core function signatures, no
  `cosmix_config::load_*` calls inside core constructors. Config gets
  injected. This is policed by review (and, eventually, by a lint).
- **Cognitive overhead**: contributors decide "core or citizen?" for every
  new function. Small individually, accumulates over time. The protection
  against this is to keep the pattern *opt-in per crate* and the convention
  *short* (one paragraph in CLAUDE.md, this memo for rationale).

## ABP citizenship is the default (merged 2026-07-23 from all-daemons-amp-citizens.md)

The complementary posture ruling, absorbed here when its standalone ADR was
retired (full text in git history):

- **Every Cosmix daemon is a full ABP citizen by default — there is no
  "edge daemon, no ABP" carve-out.** The default build of a daemon crate is
  the citizen build; core-only is the special case, not the other way round.
- **Sidecar control planes are a defensive escalation, never the default.**
  No custom IPC, control files, or signal-based control surfaces that bypass
  ABP.
- **Boundary protection (WG-only binding) and blast-radius control (per-verb
  AuthPolicy) are distinct concerns** — conflating them was the original
  error that produced the webd exception.
- **New-daemon checklist:** connect via `NodedClient`, register the service
  name, retry-with-backoff on initial connect (the dnsd silent-invisibility
  trap; reference fix cos `04c1021`), and an AuthPolicy review per verb
  before merge. Now also stated normatively in SPEC 10 §1.2.
- **Security stance:** enforcement is reactive per-verb AuthPolicy as
  capability is built — never preemptive withholding of ABP membership
  itself.

## Status and review

This memo is *directional*. It binds new code via a one-paragraph note in
`CLAUDE.md`. It does not retrofit existing code. It will be reviewed when
either (a) a second crate has adopted the pattern, at which point promotion
to a numbered spec chapter becomes appropriate; or (b) the autoresearch
experiment on Mix produces a clear thumbs-up or thumbs-down on the value of
the split.

Next review: 2026-07-26 (or earlier if the autoresearch run completes
sooner).
