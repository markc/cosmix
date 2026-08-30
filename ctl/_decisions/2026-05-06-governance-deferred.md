# Mesh governance — deferred research direction

> **2026-07-23 triage note:** the resurface trigger named below ("ABP
> Display Protocol renderer interactive-capable") can no longer fire as
> written — the ui.*/disp lane was retired 2026-07-18 and archived
> 2026-07-20. The governance direction itself is NOT ruled out; if resumed,
> the interactive surface would be webd or a native ctk app, and §1's
> trigger needs rewriting first.

**Status:** deferred / exploratory. Single source:
`2026-04-20-mesh-governance-research.md`. This decision record captures
the direction Cosmix has *committed to as a research direction* (not as
near-term implementation). Two open questions gate MVP governance-module
implementation; until they have written answers, the governance module
is architecture, not code — though near-term keep-compatible work
(activity-log identity, outcome-link tracking, Mix call-log script
identity) proceeds in parallel (see §8). The doc is consolidated into
`decisions/` rather than `planned/` because the substance is a research
direction with current design implications, not a queued build.

> The source carries `next_review: 2026-06-20`, `status: exploratory`,
> and `promotes_to: _plan/cosmix-governance-mvp.md` with an MVP target
> of 1-2 months from 2026-04-20. The source itself describes the
> direction as "actively committed to on a 1-2 month timeline. Not
> aspirational long-term; near-term scaffolding that affects current
> design decisions." This consolidation lives in `decisions/` because
> it captures a research direction with current design implications,
> not because the work has been deprioritised; promotion to
> `_plan/cosmix-governance-mvp.md` is gated on the renderer trigger
> in §1 and the two open questions in §7.

---

## 1. Trigger and timing

- **Resurface when:** the ABP Display Protocol renderer
  (`cosmix-disp-skia`, registered as the `display` service) is usable
  enough to carry an interactive governance surface.
- **Target:** 1-2 months from 2026-04-20 for an MVP cosmix-governance
  module.
- **Why the acceleration:** Mark is developing a comprehensive
  civic-democracy framework in parallel to cosmix. The technical and
  political architectures are intentionally co-evolving. Cosmix is the
  lab; the civic thinking is the target domain. Both inform each other.

---

## 2. Vision in one paragraph

Cosmix grows a parallel comprehensive governance module that takes the
ratified Declaration of Digital Independence, the constitution (Article
IV especially), and a new Laws-of-the-Land layer (new location:
`src/_laws/`) as its inputs, maintains a Level of Trust (LoT) score for
every participating actor (human operator, AI agent, mesh node, Mix
script identity), and uses LoT-weighted quorums to ratify amendments to
laws, pause autonomous execution on detected drift, and eventually —
when the harness has earned it — remove the human pay-wall entirely.
The mechanism is modelled on Bayesian spam filtering (spamlite) where
classification evidence continuously tunes per-actor scores. The same
architecture is intended to inform Mark's civic-democracy framework
design — cosmix acts as a real-stakes testbed for governance mechanisms
that could eventually apply to human society (with significant caveats —
see Risks below).

---

## 3. Trust and confidence primitives

The governance module tracks reputation for four ontologically distinct
actor types. Each uses related Bayesian math but separate storage,
decay profiles, and semantics. **They are not directly summable or
commensurable.** Forcing all four into a single "LoT" primitive was
conceptual elegance at the cost of operational clarity.

### 3.1 Actor taxonomy

| Actor type | Primitive name | Domain | Evidence stream | Governance role |
|---|---|---|---|---|
| **Human** (operator; future federation peer) | **LoT** (Level of Trust) | [0, 1] civic reliability | peer review, ratification outcomes, revocation events | ratifier and voter |
| **Agent** (LLM, stochastic proposer) | **Proposer-LoT** | [0, 1] proposal quality | accept-rate, revert-rate, verification-pass rate | proposer; not a direct voter |
| **Script** (Mix / Rust / SQL tool with stable identity) | **Confidence** (shared substrate with skills) | [0, 1] execution reliability | success rate, crash-free rate, verification pass rate | executor; not a voter |
| **Node** (mesh peer daemon) | **Reliability** | [0, 1] serve quality | uptime, response correctness, peering stability | server; not a voter |

**Same math family, separate tables.** All four use Bayesian updates on
observed outcomes (the spamlite analogue — per-entity classifier tuned
by evidence). All four decay toward a prior on inactivity. All four are
per-scope partitioned. But they live in **separate** SQLite tables with
distinct schemas, and their scoring functions carry different priors,
decay half-lives, and outcome-event definitions.

**Why separation matters.** Averaging an agent's Proposer-LoT with a
script's Confidence produces a number with no semantic meaning. Using
script Confidence as a voting weight is category-confusion — scripts
execute, they don't vote. Unifying these into one primitive collapses
real distinctions and invites downstream misuse.

**"LoT" as umbrella vs. specific term.** Mark uses "LoT" in his
civic-democracy framework specifically for humans. This document
preserves that usage. Where the doc says "LoT" unqualified, human-LoT
is meant. Where reputation across actor types is discussed, the
specific primitive name is used.

**Structural parallels Mark has drawn (all four share these):**

- Spamlite ham/spam Bayesian classification → actor good/bad outcome
  classification
- Stature in society → primitive-weighted voice weight (humans vote,
  agents propose, scripts execute, nodes serve)
- Lifetime of good/bad → lifetime decay + evidence
- Basic income (in Mark's civic framework) → LoT-tied reward allocation
  (humans only)

### 3.2 Key properties (shared across all four primitives)

- **Per-scope partitioning.** An actor can score 0.9 in Knowledge scope
  and 0.2 in Operation scope. Preserve partition at the schema level,
  not just in reporting.
- **Bayesian update.** Priors matter. Suggested starting priors
  (pending operator view and law-ification as LAW-2026-005):
  - Humans: 0.5 (prior: uncertain)
  - Agents: 0.3 (prior: skeptical)
  - Scripts: use existing skills-substrate default (0.5 for confidence)
  - Nodes: 0.7 (WG-gated peers start with elevated prior; compromise
    means a bigger fall)
- **Decay over inactivity.** Trust earned 6 months ago without
  subsequent activity decays toward prior. Prevents zombie authority.
  Half-life varies by primitive — human LoT probably ≥ 1 year, agent
  Proposer-LoT probably 60-90 days, script Confidence 90-180 days,
  node Reliability ≤ 30 days (uptime decay is fast).
- **Evidence-grounded, not popularity-based.** Unlike GitHub stars or
  most DAO reputation, all four primitives measure outcome quality, not
  endorsement volume.

**Cold-start problem:** on day 1, only Mark has a meaningful LoT (he is
the operator and the ratifier). Everything else starts at prior. Until
evidence accumulates, the system degenerates to "operator decides" —
fine as bootstrap, not as terminal.

---

## 4. Three-layer law model

| Layer | Location | Amendment mechanics | Frequency |
|---|---|---|---|
| **Declaration** | `_doc/2026-04-20-declaration.md` | Operator-authored, rare | years |
| **Constitution** | `_spec/2026-04-20-00-constitution.md` | Operator-authored + CHANGELOG | quarters |
| **Laws** | `src/_laws/*.md` (new) | LoT-weighted quorum, operator co-sign for some classes | weeks to months |
| **Implementation** | code, configs, runtime | Automatic per policy matrix | continuous |

### 4.1 What qualifies as a law (examples)

- Clippy autofix whitelist content
- CMM tier definitions and cadences
- Skill graduation thresholds (confidence/usage/age)
- Spamlite training sample policies
- Mesh routing and announcement policies
- Mail retention rules
- Proposal summary caps, email recipients
- Rejection ledger retention period

These change too often for constitutional ceremony but deserve
first-class structured declaration (not buried in code). They have
their own amendment cycle with LoT-weighted quorum.

### 4.2 Law file format sketch

```yaml
---
title: <law name>
law_id: LAW-YYYY-NNN
ratified: YYYY-MM-DD
amends: <prior law_id or null>
status: active | amended | repealed
quorum_required: <fraction of total LoT in relevant scope — see "Quorum math" open question>
scope: knowledge | code | operation
---
# <Law title>
## Authority
(references to constitutional articles this derives from)
## Provisions
(numbered, parseable)
## Review schedule
(when this law auto-proposes amendment based on metrics)
```

### 4.3 Laws are constraint-tightening only

A law may add conditions, add new forbidden targets, narrow a ✓ cell in
the constitution's IV.3 matrix to ✗ or to ✓-with-conditions, shorten a
verification deadline, or otherwise *restrict* autonomous behavior
below what the constitution grants. A law may **not** expand autonomous
permission: flipping a ✗ to ✓ in IV.3, adding a scope class, relaxing a
Charter-scope lock, removing an invariant — all require constitutional
amendment (Article VII), not a law. This prevents LoT-quorum on laws
from routing around the amendment ceremony.

In real legal terms: statutory law is below constitutional law and
cannot override it. Only constitutional amendment overrides the
constitution. The cosmix layer mapping preserves this hierarchy
exactly.

### 4.4 Parameter migration table

Current implementation details and config parameters split into two
groups: multi-actor policy (LAW candidates, LoT-quorum-governed once
governance MVP ships) and operator-operational (stays in config,
operator-only edited, never law-ified).

| Parameter | Currently | Planned | When | Rationale |
|---|---|---|---|---|
| clippy autofix whitelist | `cmm_cargo_clippy.mix $PROPOSAL_LINTS` | LAW-2026-001 | MVP | multi-actor policy |
| CMM tier cadence | systemd user timers | LAW-2026-002 | MVP | multi-actor scheduling |
| skill graduation thresholds | `cosmix-skills-cli` hardcoded | LAW-2026-003 | MVP | multi-actor policy |
| Mix stdlib graduation criteria | implicit | LAW-2026-NNN | MVP | multi-actor (usage-weighted) |
| circuit-breaker params (Const. IV.5) | `harness.toml` | LAW-2026-004 | post-MVP | cross-scope policy |
| LoT decay half-life | n/a yet | LAW-2026-005 | MVP | scoring math |
| LoT prior distributions | n/a yet | LAW-2026-005 | MVP | same law |
| TLS cert paths | config | never | — | operator-operational |
| mesh peer list + WG keys | config | never | — | operator-operational |
| blob storage paths | config | never | — | operator-operational |
| SMTP inbound/outbound config | config | never | — | operator-operational |

The distinction is intent, not file type: if changing the value affects
how autonomous actors make decisions, it's a LAW candidate. If it
affects only which disks, certs, or hosts the stack runs on, it stays
operator-operational.

---

## 5. The cosmix-governance module (proposed shape)

### 5.1 New crates

- **`cosmix-lib-governance`** — core types: `Actor`, `Lot`, `Proposal`,
  `Ballot`, `Quorum`, `Amendment`, `Law`, `RatificationEvent`. Pure
  logic + serde, no I/O.
- **`cosmix-governed`** — daemon that holds LoT state, serves
  governance ABP commands, integrates with appliers and CMM.
  SQLite-backed.

### 5.2 ABP command vocabulary

- `governance.score` — read LoT for an actor + scope
- `governance.update` — Bayesian update after an outcome observation
- `governance.propose` — submit an amendment proposal
- `governance.vote` — cast LoT-weighted vote
- `governance.quorum` — check if a proposal has met its threshold
- `governance.ratify` — finalise passed proposal (operator-only for
  Charter scope; LoT-quorum for lower scopes)
- `governance.laws.list` — enumerate active laws
- `governance.laws.read` — fetch a specific law

### 5.3 Integration points

- `cmm_apply_proposals.mix` and all future appliers call
  `governance.score` on the proposal's proposer (observer identity) to
  weight acceptance; check LoT-gated thresholds.
- After each applier outcome, call `governance.update` with the result.
- Activity-log entries carry `governance_event_id` when a governance
  action was part of the commit.
- Circuit breaker (Article IV.5) becomes LoT-aware: reverts specifically
  *lower* the committer's LoT, so repeated agent failure is reflected
  in future voting weight.

---

## 6. MVP deliverables (2-month target)

In rough order:

1. **`cosmix-lib-governance` core types** — define `Actor`, `Lot`,
   `Scope`, `Outcome` enums + serde. Pure types, no daemon yet.
2. **LoT SQLite schema** in indexd DB (sibling to skills table):
   `lot(actor_id, scope, score, prior, last_update, decay_half_life)`.

   > **Source-internal tension (preserved, not resolved):** §3.1
   > requires *separate* tables per primitive (LoT, Proposer-LoT,
   > Confidence, Reliability) with distinct schemas, but the MVP step
   > here proposes a single unified `lot(...)` table. Both phrasings
   > are present in the source; reconcile when MVP coding starts (Q1
   > in §7.1).
3. **LoT computation** — Bayesian update function + decay function.
   Write as Mix-callable builtin AND library function. Test on
   historical activity-log data first (back-compute from git history).
4. **First laws extracted** — manually extract existing implicit laws
   into `src/_laws/`: clippy whitelist (LAW-2026-001), CMM tiers
   (LAW-2026-002), skill thresholds (LAW-2026-003). Each file has
   ratification metadata.
5. **Applier integration (minimal)** — `cmm_apply_proposals.mix` logs a
   governance event per action. Does not yet block on LoT; just
   observes.
6. **`governance.score` + `governance.update` ABP commands** — read and
   write. Still no voting.
7. **Simple amendment flow for laws** — a Mix script that reads a
   proposed law edit, checks LoT-weighted endorsement, ratifies if
   threshold met. Constitutional amendments still operator-only.
8. **Governance surface in `cosmix-disp-skia`** — a UI window showing current
   LoT / Proposer-LoT / Confidence / Reliability scores, pending
   proposals, active laws, recent ratifications. This is the GUI
   trigger condition.
9. **`cmm_audit.mix` audit script** — implements constitutional Article
   V.5 (automated audit). Walks recent commits, checks for required
   `Autonomous-*` trailers, checks branch names against tier rules,
   checks target paths for Article VI intersections, greps applier
   scripts against V.2 canonical pattern list, emits audit report to
   `_memory/audit-YYYY-MM-DD.md`, and raises a V.4 crisis on detected
   violations. Runs 1440m. Script itself is Charter scope once
   deployed.

---

## 7. Open questions that must be resolved before MVP coding begins

Two questions gate the start of implementation. Neither is drafted in
this document; both require dedicated thinking time beyond what is
possible in a single session. **Until they have written answers, the
governance module is architecture, not code.**

### 7.1 Q1: Actor-taxonomy schema and parameters

The four-primitive section above sketches distinct trust measures.
What's unresolved: exact SQLite schemas per primitive, initial prior
distributions, decay half-life defaults (per-primitive, per-scope),
and the outcome-event vocabulary each primitive's update function
consumes. These choices persist in stored data — every row carries the
interpretation of the scoring function that wrote it — so switching
mid-stream carries back-compat cost. Resolve as a written schema
document before `cosmix-lib-governance` core types land.

### 7.2 Q2: Quorum math on probabilistic scores

"Quorum = fraction of total LoT in scope" assumes LoT values sum, but
probabilities in [0, 1] do not sum meaningfully. Candidate resolutions:

- **Sum of log-odds.** Additive, natural for Bayesian updates. But what
  does "fraction of total" mean when total is unbounded?
- **Weighted contribution.** vote_weight = f(LoT), where f is sigmoid,
  identity, or threshold. Choice of f determines whether the system is
  concentration-resistant or concentration-favoring.
- **Cutoff + count.** Only actors above threshold T may vote; each
  counts 1. Simple but loses graduation in weight.
- **Opinion pooling (Genest & Zidek 1986).** Linear vs. logarithmic
  pooling of probability distributions. Has a mature literature.

Each option has different properties re: concentration-resistance,
consensus-preservation, threshold sensitivity, and gaming surface.
**This is the load-bearing math question.** Once shipped, changing
interpretation breaks every historical vote. Spend a week on it before
the LoT computation step (MVP #3). Relevant literature: Bayesian
opinion pooling (Genest & Zidek 1986); weighted majority learning
(Littlestone & Warmuth); quadratic voting (Weyl & Lalley); futarchy
bet-market math (Hanson).

### 7.3 Explicitly deferred for MVP

- Multi-human federation (cross-operator LoT reconciliation)
- Basic-income / reward dispensation (that is a civic-framework
  concern, not a cosmix-lab concern, for now)
- Liquid-delegation of LoT votes
- Futarchy-style bet markets
- Quadratic voting mechanics

---

## 8. Keep-compatible design moves for near-term work

Things being built in the next 30 days that must not foreclose this
direction:

1. **Every autonomous action must have a stable `agent_id`.** Not just
   a script name — something stable across versions. E.g.,
   `cmm_apply_proposals@v1` or a UUID the script self-assigns on first
   run and persists. Activity-log format needs this field; add now.
2. **Outcomes must be link-tracked.** When a commit is reverted, the
   revert must reference the original commit's SHA in a structured way
   (revert trailers already partly handle this). Later, LoT math will
   join revert events back to original actions via this link.
3. **Mix function-call logs must carry script identity.** The log of
   "function X was called" should carry "by script Y version Z". This
   is the first live LoT signal. Usage-weighted LAW-2026-NNN "Mix
   stdlib graduation criteria" would read these logs directly.
4. **Reputation decay half-life as a config parameter.** Pick a starting
   value (suggest 90 days) and make it amendable via a law, not baked
   into code.
5. **Separate "operator approved" from "operator silent."** A commit
   that the operator actively co-signed is different evidence than one
   they did not revert. Git trailers can distinguish. Today they do
   not. Add this distinction before appliers start updating LoT.

---

## 9. The Mix function-call log → LoT connection (live today)

Mark is already logging all function calls in Mix. That log is
literally a stream of "who used what how often." Interpretations:

- **Function popularity → function LoT.** A Mix builtin called 10,000
  times a day has earned implicit community trust. It should graduate
  slowly; removing it requires higher quorum.
- **Script identity → script LoT.** Scripts that get invoked frequently
  without breaking have passive evidence of trust.
- **User identity → user LoT.** (In the multi-operator future.)

> **Source-internal terminology drift (preserved, not resolved):** §3.1
> states that "where the doc says 'LoT' unqualified, human-LoT is
> meant" and that scripts use Confidence and functions are not voters.
> The bullets above (verbatim from source) use "function LoT" and
> "script LoT" anyway. The taxonomic-rigor reading is "function
> Confidence" / "script Confidence"; the source's looser usage here
> is preserved so the consolidation doesn't quietly retype the
> primitives.

MVP task 3 above should include a back-analysis of existing Mix
call-logs to produce initial LoT scores for installed scripts and
builtins. Retrospective bootstrapping.

---

## 10. Substrate limits

The aim of "no human acceptance step in the default code path" has a
physical ceiling. The operator owns the hardware, pays the network
bills, holds the WG keys, controls the legal identity registrations,
and decides whether the mesh stays up. The harness can earn authority
over code, configuration, scheduling, and knowledge *within the stack*.
It cannot earn authority over:

- Electricity and hardware uptime
- Network access, ISP relationships, IP allocations
- Legal identity (domains, company registrations, trademarks)
- Commercial relationships (hosting bills, cert authorities, SaaS
  boundaries to external services)
- Cryptographic root-of-trust (WG keys, SSH keys, signing keys)

"No human pay-wall" refers to default paths for code-and-config
changes *inside* the stack. It does not imply removal of human agency
over the substrate. This paragraph exists so the claim isn't misread;
the vision is about default gating, not about hardware-level autonomy,
which is physically and legally impossible for an operator-owned
system.

---

## 11. Risks to navigate carefully

1. **Bootstrap collapse.** Day 1, only operator has meaningful LoT. All
   agent scores are at prior. System degenerates to "operator decides"
   until evidence accrues. This is fine AS A BOOTSTRAP but not as a
   terminal state. Resist the temptation to seed fake evidence.
2. **Gaming the signal.** If operator rubber-stamps autonomous work,
   agents farm LoT undeservedly. Distinguish active approval from
   passive non-revert (see keep-compatible #5).
3. **Governing the governance algorithm.** The LoT update function is
   itself a policy. Should it be a law? A constitutional article? A
   hard-coded invariant? Probably: hard-coded for v1, promoted to
   constitutional article when stable, amendable only via Article VII
   ceremony. Do not let the voting system vote on how voting works,
   not at MVP.
4. **Scope leakage.** Knowledge-scope LoT should not transfer
   automatically to Operation-scope. Preserve partition even if harder
   to implement.
5. **Human-LoT ethical complexity.** The LoT concept applied to humans
   has genuine resemblance to social-credit systems. Most civic
   reputation systems (credit scores, academic reputation) have
   well-documented failure modes: opacity, racial/class bias,
   irreversibility, corporate capture. Cosmix-as-lab is valuable
   EXACTLY because failures are recoverable and the operator is
   reputation-subject-and-authority in one; extrapolating to human
   society requires extreme care about who defines "good", what
   privacy is sacrificed, and how catastrophically bad classifications
   propagate. This document is not a recommendation that LoT should
   govern human civic life. It is a recommendation that cosmix can
   test governance mechanics under conditions where ethical failure
   is recoverable.
6. **Sybil and patience attacks.** An agent produces N boring-correct
   commits to accumulate Operation-scope Proposer-LoT, then spends the
   accumulated trust on one deliberately bad commit. LoT decay is only
   a defense if the attacker cares about decay — patient attackers
   don't. Available mitigations:
   - **Per-action blast-radius caps.** A single T1 action cannot
     modify more than X paths in Y minutes regardless of actor score.
     Prevents single-action concentration.
   - **Authorization consumption.** Each T2 apply consumes a token
     that regenerates slowly. 500 boring commits don't buy a 501st
     "free" bad action.
   - **Circuit breaker (constitutional IV.5).** Reverts pause the
     system regardless of attacker patience or accumulated score.
   - **Perimeter.** cosmix trust domain is WG-gated; sybil requires
     insider access or compromised peer. Perimeter is the primary
     defense; LoT is a backup signal, not a hermetic barrier.

   This risk is **accepted as inherent** to trust-based systems with
   long-lived actors. Document explicitly so future-implementers do not
   assume LoT catches patient attackers.

---

## 12. Patterns explicitly to avoid

- **Stake-weighted voting** (the DAO failure mode). Compute, storage,
  or money = voting weight is plutocratic. LoT is outcome-weighted.
- **Flat actor-agnostic reputation** (GitHub stars failure mode). Scope
  partitioning is not optional.
- **Anonymous voting** (most DAO failure mode). Identity is a
  prerequisite; cosmix WG mesh already has this.
- **Winner-take-all ratification.** Amendments should carry persistent
  records of endorsers and dissenters, not just outcome.
- **Self-governing-governance at day 1.** The governance module must be
  governed by the operator directly until its own LoT earns it
  self-management rights.

---

## 13. Intellectual anchors for deeper research

- **Elinor Ostrom's eight design principles for long-enduring commons
  governance** — most relevant theoretical frame for self-organising
  rule systems; closer fit than US constitutional theory.
- **Futarchy (Robin Hanson)** — vote on values, bet on outcomes.
  Applicable to the *measurable subset* of proposals: those whose
  outcomes the harness can score honestly (test-pass rate,
  revert-within-N-days, performance delta, uptime impact). Not
  applicable to proposals requiring aesthetic or design judgment ("is
  this a good name", "is this API ergonomic", "is this abstraction
  pulling its weight"). The honest-measurement constraint is strong —
  any Goodhart-able metric invites optimisation against the metric
  rather than against reality. Per-law flag which proposal classes
  admit futarchy-style bet markets and which require operator
  adjudication.
- **Liquid democracy / LiquidFeedback** — delegated, revocable vote
  weight. Maps onto: an agent can delegate its LoT-vote-weight to
  another agent for specific scopes.
- **Apache Foundation / IETF "rough consensus + running code"** — a
  governance model that rewards active contribution over formal voting.
  Aligned with cosmix's tendency toward meritocratic autonomy.
- **Bayesian spam filtering literature (Graham, Robinson)** — direct
  prior art for the update function. Paul Graham's *A Plan for Spam*
  is still the canonical introduction.
- **Anthropic Constitutional AI** — adjacent, not parallel: constrains
  model behaviour via training. Cosmix's version constrains running
  system behaviour via enforcement. Both descend from human
  constitutional tradition but apply differently.
- **Switzerland's cantonal + direct democracy model** — the only
  real-world example of direct democratic ratification at scale working
  sustainably. Worth studying for subsidiarity principles.
- **Social credit systems (as cautionary tale)** — document specifically
  WHY China's implementation went bad so cosmix avoids structurally
  equivalent mistakes.

---

## 14. Relation to existing cosmix artifacts

- **Declaration of Digital Independence** (`_doc/2026-04-20-declaration.md`,
  status: approved): Tenet VII (Consent Sovereignty, as amended)
  already anchors the direction — "rules we set today are a
  calibration, not a permanent fence".
- **Constitution** (`_spec/2026-04-20-00-constitution.md`, status: draft,
  operative invariants unchanged from 2026-04-20 ratification):
  Article IV preamble states the trusted-autonomy
  aim; Article IV.5 (circuit breaker) is a LoT precursor (uses
  revert-count as a crude trust proxy).
- **Self-updating systems pattern**
  (`_doc/2026-04-20-self-updating-systems-pattern.md`): the five-stage
  loop (observe → propose → triage → apply → learn-back) becomes six
  stages with governance (observe → propose → vote/weigh → apply →
  verify → update-LoT).
- **Activity log** (`_memory/activity-log.md`): the existing structured
  format is the LoT evidence stream once outcome-linking is added.
- **Spamlite** (`~/.gh/spamlite`): direct architectural precedent. The
  LoT update function is spamlite's Bayesian classifier applied to
  actor outcomes instead of email content.
- **Mix function-call logging**: live LoT signal source, already
  operational.
- **Skills substrate**: LoT of an agent and confidence of a skill are
  the same structural concept applied to different targets. Skills
  already have confidence tracking — LoT generalises this to all
  actors, not just skills.

---

## 15. Promotion path

On promotion: when MVP work begins, this document is split into
`_plan/cosmix-governance-mvp.md` (concrete implementation plan), and
this document is updated with `status: stable` and `promotes_to:`
removed. Constitutional-scope questions (if governance ratifies them)
may promote further to a future `_spec/` chapter once stable.
(Spec chapters 08 and 09 are taken — self-repair and self-improve
respectively — so the slot would be 10 or later.)

---

## 16. References

- `2026-04-20-mesh-governance-research.md` — single source for this
  consolidation (`status: exploratory`, `next_review: 2026-06-20`,
  `promotes_to: _plan/cosmix-governance-mvp.md`)
- `_doc/2026-04-20-declaration.md` — Tenet VII (Consent Sovereignty, as amended)
- `_spec/2026-04-20-00-constitution.md` — Article IV preamble + IV.5
- `_doc/2026-04-20-self-updating-systems-pattern.md` — five-stage loop
- `_plan/2026-04-20-skills-self-updating.md` — concrete applier precedent
- `~/.gh/spamlite` — Bayesian classifier architectural precedent
