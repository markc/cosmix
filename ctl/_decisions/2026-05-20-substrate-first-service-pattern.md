---
title: Substrate-First Service Pattern
date: 2026-05-20
status: directional — draft, not yet bound in CLAUDE.md
next_review: 2026-08-20
draws_from:
  - "CLAUDE.md"
  - "_spec/2026-05-11-12-property-substrate.md"
  - "_decisions/2026-04-27-core-and-citizen-crate-pattern.md"
  - "_spec/2026-05-11-12-property-substrate.md"  # §18 data-format seam law (promoted 2026-07-23 from substrate-mix-data-formats)
  - "_plan/2026-05-20-cosmix-cross-mesh-authz.md"
  - "_doc/2026-05-20-native-vs-stdlib-split.md"
tags: ["architecture", "amp", "spec-12", "mix", "services", "decision-record"]
---

# Substrate-First Service Pattern

A directional architectural convention for how Cosmix daemons expose their
functionality, so that operator and agent workflows are uniformly composable
across the entire ecosystem rather than each daemon shipping its own bespoke
config DSL, CLI surface, and IPC shape. The convention names a three-layer
cake — Rust daemon, SPEC-12 + ABP primitive API, Mix orchestration — and
specifies the (narrow) conditions under which a daemon should additionally
*embed* the Mix interpreter for per-request scripting.

This document is *not* canon. It is the design memo behind a future
one-paragraph note in `CLAUDE.md`. It graduates to a numbered spec chapter
when at least one in-flight service consumer has shipped against the pattern.

## The pattern

Every Cosmix daemon exposes its functionality through three layers:

- **L0 — Daemon (Rust).** Syscalls, protocol parsers, persistent state, hot
  loops, anything that needs `std::os::unix`, `tokio`, or a battle-tested
  crate ecosystem. Per-daemon, one process per failure domain.

- **L1 — Substrate API (SPEC-12 namespaces + ABP verbs).** The *uniform
  primitive surface* every daemon speaks. State that operators or agents
  need to read, write, watch, or audit is modelled as a SPEC-12 namespace
  with a typed schema and an `AuthPolicy`. Operations that can't be
  atomically expressed as raw `props.set` get a thin ergonomic ABP verb
  (the cosmix-wgd §5.2 wrapper pattern). L1 is still Rust, but it is the
  *contract* every other layer talks to.

- **L2 — Orchestration (Mix).** Operator workflows, deploy scripts,
  grant/revoke/refresh flows, audit summaries, cross-daemon glue. Mix
  scripts call ABP verbs via `send`/`address`/`emit`. L2 is where the
  ecosystem's *stdlib* lives, in the same sense as the analogous
  intra-language proposal in `_doc/2026-05-20-native-vs-stdlib-
  split.md`. That doc is itself labelled "thinking notes — exploration,
  not committed direction"; the three-layer cake here is the
  cross-scale generalisation of the same intuition, not derived from it
  as binding precedent.

The convention's default is **L0 + L1 only**: any new daemon-side
feature whose state can be expressed as substrate writes *should* land
as L1 rather than as a bespoke file format, CLI flag, or ad-hoc IPC.
Embedding Mix inside the daemon (see *In-band embedded Mix*, below) is
a separate decision and remains opt-in per daemon, gated on a specific
need that L1 alone cannot meet. None of this binds new code today;
binding lands when a one-paragraph note in `CLAUDE.md` points at this
memo (see *Status and review*).

## The three layers, made concrete

| Layer | Examples (landed + planned) |
|---|---|
| L0 | landed: `cosmix-maild`, `cosmix-dnsd`, `cosmix-noded`, `cosmix-indexd`, `cosmix-webd`, `cosmix-agentd`. planned: `cosmix-wgd`, `cosmix-vhost`. |
| L1 | landed: maild's `accounts` / `account_overrides` / `engine_config` / `tls_identities` namespaces; dnsd's `zone.snapshot` / `stats` ABP verbs. planned: wgd's `interfaces` / `peers` / `mesh`; the five new namespaces in the cross-mesh authz plan. |
| L2 | landed: `deploy_dnsd.mix`, `normalize_mesh.mix`, `codex_loop.mix`. planned: `add_vhost.mix`, `grant_mesh.mix`, `mailbox_audit.mix`. |

The pattern says: **L1 is the API**. CLI tools and operator workflows
compose L1 verbs from L2 Mix scripts. The daemon does not ship a parallel
config-file surface that bypasses L1, and does not ship a CLI that does
things L1 cannot. If a feature is reachable from the CLI but not from L1,
that is a bug in L1's coverage, not a feature of the CLI.

## When (and only when) to embed Mix in-band

Embedding the `cosmix-lib-mix` interpreter *inside* a daemon — nginx-Lua
style — is the second axis of this pattern. It is the right shape only
when a per-request or per-event decision depends on data only the daemon
sees in that instant, and therefore cannot be expressed as a substrate
read against a pre-written record.

The filter is a three-way split — "per-request" alone is not enough,
because protocol code (signature verification, DNS query handling) also
sees live request bytes but should *never* be operator-scripted:

- **Substrate-shaped decision** → L1 only. The decision data is
  structured state that operators write ahead of time; the daemon reads
  it on the fast path. *cross-mesh trust + grants, maild account
  records, dnsd zones, wgd peer config are substrate-shaped.*
- **Per-request decision, protocol/security machinery** → keep in Rust,
  no embed. The decision depends on live request bytes, but the logic is
  protocol parsing, signature verification, freshness checking, replay
  defence, or other security-critical code that operators should not be
  scripting. *the cross-mesh envelope verifier sees live bytes but is
  protocol code; dnsd's query handler sees live queries but is protocol
  code.*
- **Per-request decision, operator-authored policy** → embedded Mix is a
  candidate. The decision varies by request content *and* is the kind of
  rule an operator would otherwise write by hand in a bespoke DSL, and
  benefits from being able to script. *HTTP routing rules, conditional
  header rewrites, Sieve-style mail filters are operator-authored
  per-request policy.*

The third bucket is the only one where embedding pays. The second
bucket is the trap — "it touches live request data" is a tempting but
wrong reason to embed.

Examples that should embed Mix:

- **`cosmix-webd`** (was framed as a future `cosmix-vhost` — that single-binary
  merge is rejected, see `2026-06-04-maild-webd-trust-split.md` 2026-06-04) — per-request
  routing rules, conditional auth, response transforms. The canonical
  nginx-Lua / HAProxy-Lua case. NB the embed filter here applies to the
  **trusted** embedded mode; **untrusted** customer scripts run out-of-process
  (pooled FPM workers) per that ADR, not in-process.
- **`cosmix-maild`** for Sieve-style content filters — message body
  inspection, conditional move/reject/forward. Operators today reach for
  `maild.rules.*` ABP actions, which would be more honestly expressed as
  Mix scripts running inside maild's delivery path.

Examples that should *not* embed Mix:

- **`cosmix-wgd` cross-mesh authz** — the *authorization control plane*
  is substrate-shaped (operators write `trusted_meshes` and `grants`
  rows ahead of time, the verifier reads them on the fast path), and
  the *verifier itself* — envelope parsing, signature verification,
  freshness checking, replay defence — is per-request protocol/security
  machinery that should stay Rust. Neither half wants embedded Mix:
  the first is bucket 1, the second is bucket 2.
- **`cosmix-dnsd`** zone serving — zones are substrate state, queries are
  protocol-shaped, neither is a Mix-scriptable per-request decision.
- **`cosmix-noded`** broker — pure routing infrastructure.

Embedding Mix in a daemon is a *complement* to L0+L1+L2, never a
replacement for it. A daemon that embeds Mix still exposes its config and
state through L1.

## Why this serves the mandate

Mapping to the three design criteria from `CLAUDE.md`:

1. **Legibility.** Operators and agents read daemon state through one
   uniform vocabulary: `*.props.list`, `*.props.get`, `*.props.watch`,
   `*.props.audit.watch`. No per-daemon config-file dialect to learn.
   Each registered namespace is self-describing via SPEC-12's
   `*.props.describe` verb. A uniform daemon-level "list every namespace
   I expose" verb is not yet standardised (see §Open questions, item
   4) — until it is, agents discover namespaces by reading the daemon's
   `_doc/` or by knowing the names; the open question is a small fix,
   not a load-bearing absence.

2. **Modifiability.** Every operator action is a substrate write that
   propagates via the existing SPEC-12 change-stream. No daemon restart,
   no config-file edit, no out-of-band reload signal. Revocation is a
   `props.set` against an `enabled` field, not a deploy.

3. **Reconstructibility.** A daemon's runtime state reconstructs from its
   namespace storage backends alone. Disaster recovery is "restore the
   substrate files," not "restore the substrate files plus six bespoke
   config files plus three sqlite databases plus a `/etc/cosmix/*.toml`
   tree."

The pattern amortises the agent-operability cost. Each daemon that
ships substrate-first becomes *eligible* for shared L2 tooling — Mix
orchestration, agent verbs, audit streaming, change-stream subscribers,
future dashboards — without re-inventing the wiring. Per-domain
scripts and helpers still have to be written; the L2 stdlib for
operator workflows doesn't exist yet (see *Costs* below). The win is
uniform integration, not zero work.

## The worked example: cosmix-cross-mesh-authz

The cross-mesh authz plan (`_plan/2026-05-20-cosmix-cross-mesh-authz.md`,
rev 9) is the proof-of-method. It solves a non-trivial problem
(authorising verb calls across mutually-suspicious meshes, with
revocation, audit, replay defence, and key rotation) by combining:

- **The authorization control plane as L1.** Five new SPEC-12
  namespaces (`trusted_meshes`, `grants`, `cross_mesh_exposable`,
  `replay_nonces`, `cross_mesh_audit`) carry *all* of the system's
  state. Four ergonomic ABP verbs (`mesh.trust.refresh`,
  `cross_mesh.claim_nonce`, `cross_mesh.audit.{reserve,finalize}`)
  cover the operations raw `props.set` can't atomically express. One
  `AuthPolicy` combinator maps a verified cross-mesh principal to a
  capability set, opting any namespace into cross-mesh reachability via
  a single line of registration.

- **A thin transport over reused infrastructure.** A signed HTTPS
  envelope (`POST /v1/mesh-call`, Ed25519 over canonical bytes) carried
  over each service's existing TLS listener, with LE certs and the
  mesh KSK from wgd §12 doing the heavy lifting. The `HttpsVerifier`
  in `cosmix-lib-mesh-trust` runs the verify → dispatch → audit
  pipeline that hands authenticated calls to the same L1 dispatcher
  every other transport uses.

Notably absent:

- No new daemon. (`cosmix-mesh-trustd` was explicitly refused.)
- No new config-file format.
- No new transport stack and no new daemon-local operator IPC. (The
  signed envelope is a structured payload over reused HTTPS; the wire
  stack is unchanged.)
- No embedded Mix. (The authorization decisions are all
  substrate-shaped, so L2 Mix scripts compose the verbs from outside;
  the verifier itself is protocol/security machinery — exactly the
  bucket 2 case from the embedding filter above.)

The plan's own framing — *"complexity is policy plumbing + per-namespace
opt-in, not protocols"* — is the same lesson generalised here. When the
substrate is the API, sophisticated features compose into it rather than
piling new bespoke surfaces beside it.

## Anti-patterns this convention rules out

- **Per-daemon TOML config trees** that aren't reachable via L1. If
  operators edit `/etc/cosmix/<svc>/config.toml` directly, the daemon's
  L1 surface is incomplete; fix L1, don't keep the TOML as a parallel
  source of truth.
- **CLI flags that mutate daemon state without going through L1.** The
  CLI should be a thin convenience layer over ABP verbs, not a separate
  control plane.
- **Special-case IPC** (Unix sockets with bespoke protocols, signals,
  named pipes) for operator control. ABP is the IPC; SPEC-12 is the
  state model.
- **Embedding Mix to avoid building L1.** If a daemon embeds Mix because
  "operators can just script the missing functionality," that's a sign
  L1 is under-specified. Build L1 first; embed Mix only for genuinely
  per-request decisions.
- **Inventing a new daemon for state that belongs to an existing one.**
  Cross-mesh authz lives in wgd because trust is wgd-shaped. Don't spawn
  daemons to hold state another daemon naturally owns.

## Library-reserved namespace names

A handful of namespace names are reserved across every service that
opens a `<svc>.props.*` surface, because a Cosmix-wide library (not the
service itself) registers them on the host's `PropsRouter`. The
service hosts the record; the library owns the schema, hooks, and
behavioural contract. Reserving the names here keeps two libraries
from ever trying to claim the same name in different daemons.

| Reserved name | Owning crate | Cardinality | Purpose |
|---|---|---|---|
| `log` | `cosmix-lib-log` | Singleton | Runtime-mutable filter / format / origin trail for the binary's `tracing` subscriber. Live implementation: `cosmix-lib-log-props/src/log_namespace.rs` and `log_attach.rs`. |
| `stats` | `cosmix-lib-log` | Singleton | Substrate-mutable recorder + roll-up config (enable, cadence, output path, category allowlist, cardinality cap, byte budget / origin trail) for the binary's metrics recorder. Schema, validation, and observe-only watcher are live; runtime application of mutations to the recorder lands with the slice-4 cadence applier. See `_plan/2026-05-21-cosmix-lib-log-stats.md` §4.3. |

Adding to this list requires the same review bar as any normative
amendment to this memo. A service that wants to expose state under one
of these names must take a separate name (`<thing>_log`, `app_log`,
`<thing>_stats`, `app_stats`) — the reservation is unconditional.

## What this is *not*

- **Not a retrofit mandate.** Existing daemons (`cosmix-maild`,
  `cosmix-wgd`, `cosmix-dnsd`) are not converted on principle. The
  pattern applies to *new* daemon features and to *new* daemons.
  Conversion of existing surfaces happens when a concrete second use
  case justifies the work.
- **Not universal.** Daemons whose entire purpose is mesh infrastructure
  (`cosmix-noded`) don't have a meaningful L1 over L0 — they *are* L1
  for everyone else. The pattern applies to daemons with separable
  domain logic, not to the substrate machinery itself.
- **Not the same as the Mix native-vs-stdlib split.** That split is
  internal to one language; this is across the ecosystem. The two are
  *isomorphic* (native primitive + scripted convenience) but operate at
  different scales. Read them together — the Mix doc is the small
  example; this doc is the large one.
- **Not a numbered spec yet.** SPEC-12 specifies the property substrate.
  This doc is the architectural convention that says "use SPEC-12 by
  default for daemon state." It graduates to spec status when at least
  one new daemon (cosmix-vhost is the obvious candidate) ships
  substrate-first from day one.

## Costs the convention imposes

- **Schema discipline.** Every state element needs a SPEC-12 namespace
  with a typed schema, an `AuthPolicy`, and validation hooks. This is
  more upfront work than "shove it in a TOML file." The payoff is in
  modifiability and reconstructibility downstream.
- **Two-phase atomicity work.** Operations that span multiple namespaces
  or need lock-stepping with daemon-internal state require ergonomic
  verbs (the cross-mesh authz `audit.reserve` / `audit.finalize`
  pattern). Each one is small but non-trivial Rust.
- **Tooling investment.** L2 Mix scripts assume a stdlib of operator
  helpers (`grant_mesh`, `revoke_grant`, `audit_window`) that doesn't
  exist yet. Each substrate-first daemon adds a small but real demand on
  the Mix stdlib (see `_doc/2026-05-19-mix-stdlib-expansion.md`).
- **No escape hatch for "just write the config file."** Operators who
  reach for a text editor on a TOML file get redirected to `mix -c 'send
  <svc>.props.set ...'`. That's deliberate — and it's the single biggest
  cultural shift this convention asks for.

## Open questions

1. **L1 schema versioning when L0 evolves.** SPEC-12 has `if_version` and
   `nseq` for value-level versioning, but schema-level migration when a
   namespace adds a field is not formally specified. Cross-mesh authz
   sidesteps this by being net-new; an existing namespace gaining fields
   needs a schema-migration story.
2. **CLI tooling.** Today most operator commands are ad-hoc `mix -c
   'send ...'`. A thin CLI wrapper layer (per-service or generic) might
   improve ergonomics without violating the convention. Out of scope for
   this doc, but worth its own design memo.
3. **When does a daemon graduate from "no embedded Mix needed" to
   "embed"?** cosmix-vhost will need it from day one. cosmix-maild has a
   plausible case for Sieve rules. cosmix-wgd does not. The filter in
   *When (and only when) to embed Mix in-band* above is the current best
   guidance; a clearer heuristic may emerge with more worked examples.
4. **Discovery of L1 surface for fresh agents.** A `*.props.describe`
   verb exists per SPEC-12, but a top-level "list every namespace this
   daemon exposes" verb is not yet uniform. Worth standardising.

## Status and review

This memo is *directional*. It does not yet bind anything in
`CLAUDE.md`. The intent is:

- Land this memo first as a draft.
- Run it through the dual-reviewer loop (the doc is normative under
  CLAUDE.md's *Doc scope* rule).
- After it settles, add a one-paragraph note to `CLAUDE.md` pointing
  at it, parallel to the existing core/citizen and substrate-data-format
  notes.
- Promote to a numbered spec chapter (`_spec/13_substrate_first_services.md`
  or similar) once cosmix-vhost ships substrate-first and provides the
  second worked example after cross-mesh authz.

Next review: 2026-08-20, or earlier if cosmix-vhost begins serious
design work before then.
