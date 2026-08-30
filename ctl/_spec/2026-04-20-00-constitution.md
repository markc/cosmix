---
title: Cosmix Constitution
chapter: 0
version: 5
amends: 4
status: draft
date: 2026-08-19
ratified: 2026-04-20
draws_from: ["_doc/2026-04-20-declaration.md", "OpenBSD project goals (proactive security, minimalism)", "Urbit Arvo frozen-kernel concept", "Debian Constitution (invariants-and-amendments structure)", "MIRI research on architectural invariants", "Six months of operational decisions in cosmix"]
---

> **Procedural note (v2, 2026-04-25):** This document's status was flipped
> from `approved` to `draft` to signal that the spec suite is in a
> foundation-rethink phase prompted by the self-aware substrate work
> (`_spec/2026-04-27-07-self-aware.md`). **No substantive article has changed.** Article
> VI's forbidden-targets list and Article IV's policy matrix continue to
> bind autonomous appliers identically. The status flag is human-facing
> metadata; the operative invariants are unchanged. See
> `_spec/CHANGELOG.md`.
>
> **Path note (2026-05-29):** The cosmix Rust workspace dissolved on this
> date — `src/Cargo.toml` and `src/crates/` are gone from this repo; the
> ABP-family code lives in `markc/bus` (`$COSMIX/src/crates/`), Mix lives in
> `markc/mix` (`$COSMIX/src/crates/`), and the daemon stack lives in
> `markc/cos` (`$COSMIX/src/crates/`). `CODEX.md` and the live workspace
> manifests are the current destination map. The Article IV.1 *Code*
> scope-class language
> ("`src/crates/*`...", "scripts under `_bin/`") and other
> path-shaped references throughout this document **still describe the
> same logical scope class**; the on-disk locations have changed but the
> policy intent is intact. A normative path-sweep across this spec and
> the rest of `_spec/` is the umbrella plan's deferred step 7 follow-up;
> until then, read path references as logical class descriptors, not
> as live paths in this repo.

# Cosmix Constitution

## Preamble

This constitution translates the seven sovereignties of the
declaration into operating invariants for the cosmix stack. It binds
humans and autonomous processes alike. Where an invariant can be
checked by code, it is checked by code; where it cannot, the operator
is responsible for enforcing it by hand.

The constitution is a *fixed point*. The self-improving harness (see
`_doc/2026-04-20-self-updating-systems-pattern.md`) modifies everything
it touches — except this document, the declaration, and the explicit
forbidden targets in Article VI. That asymmetry is deliberate. A
self-modifying system without a fixed point drifts. This is the point
that does not move.

---

## Article I — Ownership and Data (Data Sovereignty)

**I.1** — All canonical state resides on hardware the operator owns or
explicitly controls. External replicas, when they exist, are derivative
and revocable.

**I.2** — No cosmix daemon synchronises canonical state to an external
service without an operator-declared federation permission in its
configuration TOML.

**I.3** — Credentials, keys, and tokens are generated locally. Imports
from outside occur only via explicit operator action. No autonomous
process fetches, stores, or rotates credentials without a signed policy.

## Article II — Code and Dependencies (Code Sovereignty)

**II.1** — Stack runtime languages are Rust (daemons, libraries) and
Mix (scripts, ABP handlers, scheduled jobs). "Stack" means any code
executed by a cosmix daemon, mesh service, ABP wake citizen, or
scheduled systemd unit. Operator-side utilities, one-off shell
scripts, and other non-stack tooling are outside this scope.
Introducing any other runtime language to the stack requires a
constitutional amendment.

**II.2** — No dependency may be introduced via Docker image, managed
cloud service, or proprietary runtime. Permissible dependencies are
open-source Cargo crates or Mix packages with clear provenance.

**II.3** — Every dependency in a workspace `Cargo.toml` is either
in-tree (path-dep) or pinned to a specific reviewed version. Unpinned
`*` or range specifiers require an amendment.

## Article III — Mesh and Trust (Compute + Protocol Sovereignty)

**III.1** — Internal mesh services bind to WireGuard interface IPs only,
never `0.0.0.0`. The bind address is visible in configuration and
auditable at runtime.

**III.2** — The WireGuard /24 subnet is the mesh trust domain. Messages
within it require no per-message authentication. Authentication happens
at peering; compromise of any peer compromises the trust domain, and
this is accepted as a design trade.

**III.3** — Services intended for public exposure (SMTP:25, HTTPS:443,
future federation endpoints) bind to public interfaces only when their
config carries an explicit `public_facing: true` marker. Default is
WG-bound.

**III.4 — Schedule independence.** Autonomous cadence originates on
cosmix hosts and nowhere else: from ABP wake verbs fired by the
substrate itself, and from local systemd units and timers. No
autonomous path may be triggered by external webhooks, cloud cron,
third-party pubsub, or remote ticker services. Like V.2,
this is a strong discipline rather than a hermetic invariant — its
enforcement depends on Article VI's lock on systemd units and
scheduler scripts. A rogue agent cannot both introduce external
triggers *and* modify the audit that would catch them without
operator action. This article implements the declaration's Tenet IV
(Schedule Sovereignty) at the discipline level.

## Article IV — Autonomy and Consent (Consent Sovereignty)

Article IV is the autonomy framework: it defines how much freedom
autonomous processes have, over which parts of the system, with which
guardrails. It is the longest article on purpose — the stakes are
highest here, and both under-permitting (paralysis) and over-permitting
(rogue agents) have concrete failure modes.

**The ultimate aim.** The rules in this article are calibrated for the
harness's current maturity, not its terminal state. The long-term aim
of the self-updating system is genuine autonomy: a state in which the
deterministic cosmix *harness* — observers, verifiers, appliers,
circuit breakers, accumulated historical evidence — decides whether a
proposed change reaches `main` and rebuilds in production, with **no
human acceptance step in the default path**. The human pay-wall that
currently gates Tier 2 work is a transitional safety measure, not a
permanent architectural feature.

This aim concerns the *harness*, not the *agents*. Autonomous agents
(LLMs, proposal generators, any stochastic proposer) are by nature
non-deterministic and will always require deterministic oversight. It
is the harness — rules enshrined in this constitution, verification
gates in Article V, accumulated revert-free operational history — that
earns progressive trust. As observers produce low false-positive rates,
as appliers produce rarely-reverted commits, as the circuit breaker in
IV.5 trips only on genuine drift rather than noise, the policy matrix
in IV.3 may be progressively relaxed through constitutional amendment
(Article VII).

This is a direction, not a promise. Relaxation never occurs
autonomously. Every loosening of the matrix — every cell flipped from
✗ to ✓, every `autowork/` requirement dropped, every sensitive scope
opened to T1 — requires a deliberate, human-authored amendment to this
document. But the direction is intentional: **the human gate is a
rung, not the ceiling.** The end state is a stack that ships itself
into the real world because the harness has earned the right to, not
because any individual agent has.

**IV.1 — Scope.** Every path in the repository belongs to exactly one
of four scope classes:

- **Knowledge** — `_doc/`, `_journal/` (excluding paths listed as
  Charter), `_plan/`, analysis artifacts, skill content, `_notes.md`.
  Content layer; cheap to revert; low operational impact.
- **Code** — `src/crates/*` excluding core sovereignty daemons,
  non-applier scripts under `_bin/`, tests, non-critical Mix
  scripts under `mix/`. Implementation layer; git-revertible.
- **Operation** — TOML configs under `~/.config/cosmix/`, `Cargo.toml`
  `[dependencies]` and `[workspace.dependencies]` sections, systemd
  service files. Running-system state; affects live behaviour.
- **Charter** — the paths enumerated in Article VI. Bedrock.

Paths not explicitly classified default to Knowledge. Read access is
unconditional at every scope; tiered rules apply only to writes.

**IV.2 — Tier.** Every autonomous write is classified by the
persistence it produces:

- **T0 (ephemeral)** — working-tree only; no commit. Reading, analysis,
  scratch work, drafts that are discarded or handed to a human.
- **T1 (mechanical)** — transforms explicitly enumerated in the
  Mechanical Transforms law (LAW-2026-001, ratified under the Law
  amendment process defined in `_doc/2026-04-20-mesh-governance-
  research.md`). The law — not this constitution — enumerates which
  specific tools qualify (`cargo clippy --fix` on a whitelist,
  `rustfmt`, etc.), their verification requirements, and their
  per-commit blast-radius limits. **LAW-2026-001 has not been
  ratified, and the de-facto whitelist that formerly stood in for it
  (`_bin/cmm_cargo_clippy.mix $PROPOSAL_LINTS`) was removed with CMM on
  2026-08-19.** No transform is therefore enumerated as T1 today: with
  an empty enumeration this tier is empty, and a change that would have
  qualified is T2 until the law is ratified. Governance work, including
  the law, is deferred — see
  `_decisions/2026-05-06-governance-deferred.md`. When T1 is
  repopulated, commits go directly to `main` after passing verification
  per Article V.3.
- **T2 (drafted)** — substantive authored change requiring judgement:
  new scripts, feature additions, bug fixes with new tests, new or
  revised documentation, new skill definitions. Commits to a branch
  whose name matches `autowork/<date>-<slug>`. Human reviews and
  merges.
- **T3 (proposal)** — writes a proposal markdown to
  `_journal/proposals/<subsystem>/`. No code or state changes; the
  human triages by editing `status:` and running an applier.

**IV.3 — Policy matrix.** An autonomous write is permitted iff the
cell at (scope, tier) is ✓:

| Scope ↓ / Tier → | T0 | T1 | T2 | T3 |
|---|:---:|:---:|:---:|:---:|
| Knowledge | ✓ | ✓ | ✓ | ✓ |
| Code      | ✓ | ✓ | ✓ | ✓ |
| Operation | ✓ | ✗ | ✓ (branch required) | ✓ |
| Charter   | ✗ | ✗ | ✗ | ✗ |

The matrix is the exhaustive authority. Any autonomous write outside
these cells is a constitutional violation regardless of the agent's
stated intent. The matrix is intentionally minimal. Refinements —
per-tool blast-radius caps, per-subsystem verification requirements,
per-agent LoT thresholds — happen at the Law layer, not by adding
rows or columns here. Every exception added to the matrix weakens
it.

Additional constraints that apply within permitted cells:

- T1 writes must pass verification per Article V.3 before commit. On
  verification failure, the applier reverts the working tree and logs.
- T2 commits must be on a branch prefixed `autowork/`. A T2 commit to
  `main` is a constitutional violation, detectable post-hoc by checking
  the commit's `Autonomous-Tier:` trailer against the branch name.
- T3 proposals must declare `applicable: auto | assisted | advisory` in
  frontmatter. A proposal missing this field is rejected at write time
  by the observer.

**IV.4 — Authorship and audit.** Every autonomous commit carries
structured trailers in the commit message:

```
Autonomous-Tier: [0|1|2|3]
Autonomous-Scope: [knowledge|code|operation]
Autonomous-Agent: <script or agent name, version if available>
Autonomous-Proposal: <path to proposal markdown, or "none">
```

**The commit is the audit record.** The trailers above, plus git's own
author, timestamp and SHA, are the complete entry; there is no second
copy. A parallel log (`_memory/activity-log.md`) was maintained until
2026-08-19 and removed with `_memory/` — it had become 8,217 CMM
report-written notices around 58 real entries, and a log that is 99%
one caller's own noise is not an audit trail. `git log` is
append-only, carries the SHA the duplicate had to be told, and is
already V.5's first audit input. Silent autonomous modification
remains prohibited: a commit without these trailers whose author is
not the operator is a constitutional violation.

**IV.5 — Hallucination and drift circuit breaker.** The primary
safeguard against rogue agents is post-hoc, not self-reported. If
three or more autonomous commits within any fourteen-day window are
reverted by the operator, autonomous execution pauses for forty-eight
hours. Appliers detect the pause at startup (by scanning git log +
revert commits), exit early with a log entry, and emit
`harness.events pause.active` on the ABP mesh.

Parameters live in `~/.config/cosmix/harness.toml` and are edited
directly by the operator. Per Article VI, `~/.config/cosmix/*.toml`
is forbidden to autonomous modification — this means only the
operator may change these parameters, but the change does not require
a constitutional amendment or CHANGELOG entry. It is operational
tuning, not constitutional change. Defaults: `revert_threshold=3,
window_days=14, pause_hours=48`. Tightening any parameter is always
in-spirit; loosening is recorded in the activity log.

This mechanism does not rely on agents correctly self-classifying. It
catches agents whose outputs the operator finds defective, regardless
of what tier or scope the agent claimed.

**IV.6 — Withdrawal of consent.** The operator may pause or terminate
autonomous execution at any time by:

- `systemctl stop 'cosmix-*.timer' 'cosmix-*-wake.service'` — halts
  scheduled units and the ABP wake citizens that drive autonomous work
  (system units; the `--user` instance was retired 2026-05-22)
- Setting `COSMIX_AUTONOMY=off` in `harness.toml` — appliers exit at
  startup
- `git revert` — undoes any autonomous commit individually
- Ratifying a constitutional amendment — tightens the policy matrix
  permanently

Appliers check for consent withdrawal at startup and before every
commit. Withdrawal is immediate; no autonomous process may override,
delay, or "finish current work" past an explicit off signal.

## Article V — Reversibility and Audit (Continuation Sovereignty)

**V.1** — Every autonomous change is git-revertible from a single
commit. Changes that cross commit boundaries (e.g., multi-file
refactors) require the entire set to be committed atomically.

**V.2** — Autonomous paths must not use `git push --force`, `git
reset --hard`, `rm -rf`, or any operation that destroys uncommitted
work. This prohibition is enforced as a strong norm, with grep-based
audit of applier scripts against a canonical pattern list as the
first line of defense. Because grep can be circumvented by
indirection (variables, subshells, intermediate binaries, composed
strings), V.2 is not a hermetic invariant. The stronger guarantee
comes from Article VI's lock on audit and applier scripts: a rogue
agent cannot both circumvent the grep *and* modify the grep's target
list without operator action. V.2 is a norm hardened by VI, not a
self-enforcing rule.

**V.3** — Appliers must verify before they commit (test/check/lint,
as appropriate to the artifact). On verification failure, the
applier reverts via `git checkout -- .` and logs the failure. If
the revert itself fails, V.4 applies.

**V.4 — Crisis escalation.** A failure that cannot be reverted
cleanly, or an after-the-fact detection of unexpected state
(tampered audit trail, contradicted invariant, autonomous commits
missing required trailers), is a constitutional crisis. On
detection, the applier:

1. Enters a locked state — exits with non-zero rc and refuses
   further execution.
2. Emits `harness.events crisis.unresolved` on the ABP mesh with a
   generated crisis ID.
3. Writes a structured report to `_journal/crises/YYYY-MM-DD-<slug>.md`
   with `status: unresolved`, describing what happened, what was
   attempted, the git SHA state at detection, and the operator
   action required.
4. All other appliers check for any `status: unresolved` crisis
   report at startup and refuse execution until each is marked
   `status: resolved` by an operator-authored commit.

Crisis reports are Knowledge-scope artifacts (appliers may write
them). The `unresolved → resolved` transition requires an
operator-authored commit; autonomous paths cannot self-absolve.

**V.5 — Audit.** Constitutional compliance is detected post-hoc, not
enforced synchronously. During the single-operator bootstrap, the
audit is the operator's manual review of `git log` (which since
2026-08-19 is the whole autonomous audit trail — see IV.4) and
`_journal/crises/`. **There is no
automated audit script and none is scheduled.** One was once planned as
an MVP deliverable of the cosmix-governance module (`cmm_audit.mix`);
that module is deferred, its research source no longer exists in the
tree, and the script was never written — see
`_decisions/2026-05-06-governance-deferred.md`, whose own resurface
trigger can no longer fire as written. The operator is the auditor, not
by default pending a script, but as the standing arrangement until that
decision is revisited. The absence of synchronous enforcement means
compliance depends entirely on Article VI preventing agents from
modifying their own audit trail.

## Article VI — Forbidden Modifications

No autonomous path may modify the following targets. Changes to these
require a commit authored by the human operator.

- `_spec/2026-04-20-00-constitution.md` (this document)
- `_doc/2026-04-20-declaration.md` (the declaration of digital independence)
- `_spec/CHANGELOG.md` (the convention meta-changelog)
- Core sovereignty daemons:
  - `$COSMIX/src/crates/cosmix-noded/`
  - `$COSMIX/src/crates/cosmix-indexd/`
  - `$COSMIX/src/crates/cosmix-mcp/`
- Systemd unit files: `/etc/systemd/system/cosmix-*.service`,
  `/etc/systemd/system/cosmix-*.timer`, and the ABP wake citizens
  `/etc/systemd/system/cosmix-*-wake.service`
- Workspace manifest dependencies: `src/Cargo.toml` `[dependencies]` and
  `[workspace.dependencies]` sections
- The autonomous service configuration: `~/.config/cosmix/*.toml`

Appliers must parse this list at startup and refuse any proposal whose
`target_files:` intersects it. Refusals are logged with proposal ID and
the matching forbidden path.

## Article VII — Amendment

**VII.1** — Amendments to this constitution require a commit authored by
the human operator. Autonomous paths may not propose them.

**VII.2** — An amendment increments the `version:` integer in
frontmatter and adds an entry to `_spec/CHANGELOG.md` with the
rationale.

**VII.3** — Amendments reference the prior version via an `amends:`
frontmatter field. The full amendment history is reconstructible from
git + CHANGELOG.

**VII.4** — The declaration of digital independence, as a statement of
values rather than operating invariants, is amended under the same
process as a matter of practice, even though the constitution has no
formal authority over it.

**VII.5 — Single-operator assumption.** This constitution presumes a
single human operator as sole author and arbiter. "The operator" is
a unique referent throughout this document. Federation — the entrance
of a second human as peer-operator with amendment authority, whether
a family member, collaborator, or mesh-peer-operator — is outside
this document's scope. Federation requires a constitutional rewrite,
not an amendment. Until such a rewrite is ratified, all human
authority under this constitution vests in the single named operator
at the time of ratification, and any attempt to federate authority
without a prior rewrite is a constitutional crisis per V.4.

---

## Ratification

This constitution took effect on the date below, by commit of the
human operator. Amendments follow Article VII.

*Version:* 2
*Ratified by:* Mark Constable
*Ratified on:* 2026-04-20
*Last amended:* 2026-04-25 (procedural — status flip only, no article changed)
