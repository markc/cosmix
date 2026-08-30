---
title: Cosmix Self-Improve Layer — Proposal Pipeline and Trust Gradient
chapter: 9
version: 0.1.0
status: draft
date: 2026-04-25
substrate_layer: improve
advances: modifiability (primary), reconstructibility (via revert contract)
draws_from:
  - _spec/2026-04-20-00-constitution.md (Article IV tier ladder, IV.1 propose-only, IV.2 first-execution gate, VI forbidden targets)
  - _spec/2026-04-27-07-self-aware.md (signals via props.changed, world.*, harness.events, §3.5 activity events, §3.5.7 agent session identity)
  - _spec/2026-04-27-08-self-repair.md (atomic deploy + LKG, rollback contract)
  - _doc/2026-04-20-self-updating-systems-pattern.md (subsumed; remains as historical exploration)
  - _doc/2026-05-03-cosmix-agent-runtime-unification-plan.md (§3.1 tool-surface authority projection — implementation-side anchor)
---

# Cosmix Self-Improve Layer — Proposal Pipeline and Trust Gradient

> Improvement is gated. The substrate may learn, may propose, may even apply —
> but only along an explicit gradient where each step up costs more trust and
> earns it differently. Read-only learning is automatic; first execution is
> human-in-loop; subsequent execution is content-addressed; code-modifying
> changes require explicit approval. Drift is prevented by binding identity
> to content, not name.

This is the third of three substrate-layer SPECs (07 self-aware, 08
self-repair, **09 self-improve**). Where SPEC 07 specifies legibility and
SPEC 08 specifies recovery within a fixed action space, this SPEC specifies
how the substrate's action space itself changes over time — how new skills,
new heuristics, new code, and new policies enter the system without breaking
the trust model.

The constitution (Ch 00) governs **autonomy and authority**: who may apply
what, under which tier, with which audit trailer. This SPEC governs the
**mechanism** through which improvements are surfaced, evaluated, gated, and
either applied or rejected. The two are interlocked but distinct. A
constitution amendment without an improvement-mechanism update is procedurally
valid but operationally meaningless; an improvement-mechanism update without
constitution alignment is forbidden by Article VI.

This SPEC subsumes `_doc/2026-04-20-self-updating-systems-pattern.md`. That
doc remains as historical exploration; the canonical pattern lives here.

---

## 1. Purpose and Non-Goals

**Purpose.** Specify the mechanism by which Cosmix improves itself: how
signals become proposals, how proposals are triaged, how applications are
gated by trust class, how outcomes feed back into future selection. Bind the
mechanism tightly enough that an agent reading this SPEC can implement and
operate the loop without consulting the operator on routine cases.

**Non-goals.**

- **Replace the constitution.** Article IV defines tier ladder and policy
  matrix; this SPEC defines mechanism within those constraints. Where they
  appear to conflict, the constitution wins.
- **Specify the renderer or display surface for proposal review.** That
  belongs in a future GUI SPEC. The contract here is over ABP topics and
  capability namespaces; any surface meeting the contract is conformant.
- **Settle the autonomous-rebuild question.** Whether the substrate will
  ever apply code-modifying proposals without explicit approval is a
  constitutional question (Article IV). This SPEC enumerates the gating
  required *if* such autonomy is ever extended; it does not advocate for it.
- **Re-derive AI-safety canon.** The genuine risks here are operational:
  feedback collapse, drift, identity confusion, irreversible application.
  ARA-frontier framings do not apply (cf. project memory
  `project_substrate_layer_split.md`).

---

## 2. The Five-Stage Loop

Every improvement traverses five stages. Each stage has a dedicated topic
family on ABP and a dedicated capability on the originating service.

```
observe → propose → triage → apply → learn-back
   ▲                                       │
   └───────────────────────────────────────┘
```

| Stage | Trigger | Surface | Output |
|---|---|---|---|
| **observe** | signal from SPEC 07 (props.changed, world.*, harness.events) or human input | service-internal | candidate set |
| **propose** | candidate ranked above threshold | `improve.proposals` topic + Proposal record | proposal id |
| **triage** | proposal received | triage capability per service + `improve.triage` topic | accept / defer / reject + rationale |
| **apply** | triage = accept AND gate satisfied | service-internal + `improve.applied` topic | applied id + outcome record |
| **learn-back** | outcome observed within window | feedback capability + `improve.outcomes` topic | adjusted scoring for future observe |

**Idempotency.** Every stage MUST be idempotent on its input identifier.
Replaying a stage with the same id MUST NOT produce duplicate side effects.
This is the substrate's hedge against the loop firing twice during a partial
restart.

**Audit trailer.** Every transition MUST emit an audit trailer per
constitution Article IV.4, with `proposal_id`, `from_stage`, `to_stage`, and
`gate_class` (see §3).

---

## 3. Change Classes and the Trust Gradient

Five change classes, each with explicit gating. The class is determined at
the **propose** stage and travels with the proposal through all subsequent
stages. The class MUST NOT be downgraded after triage; if a proposal turns
out to require a higher class, it MUST be rejected and re-proposed.

| Class | Examples | Gating | Reversal |
|---|---|---|---|
| **L0 — read-only learning** | scoring updates, retrieval-rank tweaks, summarisation cache | automatic; no review | discard cache |
| **L1 — proposing** | a candidate skill, a candidate config change, a candidate doc edit | automatic to surface; no apply | drop proposal |
| **L2 — first execution** | first run of a newly proposed skill or action | **human-in-loop** (explicit accept) | abort / undo per skill |
| **L3 — content-addressed re-execution** | nth run of a skill whose content hash matches a previously approved hash | TOFU (trust on first use) bound to `<name>:<sha256-of-content>` | revert by content hash; refuse hash drift |
| **L4 — code-modifying / destructive** | binary swap, schema migration, ABP topic deletion, constitution edit, `cosmix-*.lkg` rotation, secret rotation | explicit operator approval per occurrence; quorum per §6 if autonomous quorum is ever enabled | per SPEC 08 §6 atomic deploy + LKG |

**The L2 → L3 transition is the only place TOFU is granted.** All other
class transitions are forbidden. A skill at L3 that is refined into a new
content (different sha256) re-enters at L2 — the previous TOFU does not
transfer (cf. §4).

**Article VI invariants override class.** A proposal whose target is on the
forbidden-targets list (constitution VI) is rejected at triage regardless
of class, with cause = `forbidden_target`.

### 3.1 Tool-surface authority — a projection of the change-class gradient

The L0–L4 gradient above governs *change proposals* (skills, configs,
code edits) — artifacts that travel through the five-stage loop (§2).
The same gradient also governs *agent tool surfaces*: any tool exposed
to an agent runtime (cf. SPEC 07 §3.5.7) is classified by the same
gradient at registration time, and the runtime MUST refuse to invoke a
tool whose authority class exceeds the runtime's mounted ceiling.

Tool surfaces project onto a **subset** of the gradient, because L1 and
L3 are skill-lifecycle concepts that don't exist at single-call
granularity:

| Class | Tool meaning | Examples |
|---|---|---|
| **L0 — read-only** | The tool reads substrate state; no observable side effect outside the agent's view. | `props.get`, `world.<svc>` snapshot, `context_search`, `log_search`, `amp_list_services`. |
| **L2 — staged write** | The tool mutates substrate state but only through a path that supports dry-run and is scoped to a bounded blast radius (single workspace, single set, single file under an explicit path). First invocation of a newly-mounted L2 provider requires operator acceptance per §3 L2. | `index_workspace` (deletes + re-stores under a workspace root), `write_file` (path-scoped), `mds.export` to an operator-supplied path. |
| **L4 — code-modifying or unbounded write** | The tool mutates code, schemas, secrets, or unbounded substrate state. | `amp_call` to arbitrary service, `mix_execute`, binary swap, schema migration, ABP topic deletion, secret rotation, `cosmix-*.lkg` rotation. |

L1 (proposing) and L3 (TOFU on content-addressed re-execution) are the
skill-lifecycle states a *proposal* passes through; a tool *invocation*
is one event in time and projects onto L0/L2/L4. A skill execution is
the special case where an L3 *proposal* state is enacted via an L2 or
L4 *tool invocation* — the gradient agrees on what the action is even
when the lifecycle state and the call state differ.

**Why the same gradient, not a parallel one.** A tool that ultimately
swaps a binary is L4 in both senses — the danger doesn't change because
the artifact is invoked rather than proposed. Inventing a parallel
`Authority::*` taxonomy would make every audit join (constitution V.5)
have to reconcile two enums for the same concept. The implementation in
`cosmix-lib-tools` (per the unification plan) uses the L0/L2/L4 enum
exactly because this SPEC owns the gradient.

**Catalog vs mount.** A runtime maintains two distinct registers:

- The **catalog** lists every provider known to the runtime, regardless
  of authority class. Catalog entries appear in `tools.list` (or the
  equivalent agent-facing enumeration) so the agent can see *what
  exists* and reason about it.
- The **mount set** is the subset of catalog entries the runtime will
  actually invoke. Membership is governed by the runtime's
  authority-class ceiling (declared at startup) and by per-provider
  grants.

A provider whose `authority()` exceeds the runtime's ceiling MUST be
catalogued but MUST NOT be mounted. The runtime MUST NOT silently omit
it. In `tools.list` it appears with `gated_authority: <class>` and
`mountable: false`, plus a stable `grant_token` the agent can present
when requesting a per-call grant. Invocation of an un-mounted provider
returns `Refused { reason: "authority_gated", required: <class> }` —
not "tool not found". This refusal-with-context is what lets an agent
know to ask for an upgrade rather than work around the absence.

**Defaults.** `cosmix-mcp` (in-the-loop operator implicit) ceilings at
L4; every provider is catalogued *and* mounted. `cosmix-agentd`
(autonomous) ceilings at L2; L4 providers are catalogued but not
mounted, awaiting a per-call grant from the operator (Tier 1
acceptance per constitution IV.2) or a session-scoped policy upgrade.

**Why startup-failure isn't the right default.** An earlier draft of
this section required over-ceiling providers to refuse registration as
a startup error. That made every agentd startup against the full
provider catalog impossible, contradicting the unification plan's
explicit goal of mounting one shared catalog in both runtimes. The
catalog/mount split preserves the *visibility* property (operator and
agent both see the gap) without forcing it through a registration
failure that would conflate "this tool is absent" with "this tool is
gated."

**Cross-reference.** Per-tool capability scoping (paths, set UUIDs,
HTTP origins) is orthogonal to authority class — a tool can be L2 with
a workspace-root scope, L2 with a per-set scope, etc. The
unification plan's `CapabilityRequest` carries the scope; this SPEC's
class carries the gradient. Both apply at every invocation.

---

## 4. Skill Content Addressing

Skills are the canonical L3 case. Their identity MUST be bound on **content
hash**, not name. Otherwise L3's "trust on first use" degrades into "trust
all future regenerations of anything called X" — a drift surface the
constitution's audit trailers cannot detect.

**Identity.** A skill's pinned identity is `<name>:<sha256-of-canonical-content>`,
where canonical-content is the deterministic serialisation defined in the
skills-store contract (TBD; see §10 open question). The bare name is a
convenience handle, not an identity.

**TOFU rule.** Granting L3 to `skill_X:hash_A` does NOT grant L3 to
`skill_X:hash_B`. The latter is L2 and re-prompts the operator on first
execution. This applies regardless of how the content changed (manual
refinement, automated refinement, store migration, model-version drift).

**Storage contract.** The skills store MUST persist the content hash with
each skill record and MUST refuse to return a skill under a TOFU grant if
the stored hash does not match the granted hash. The grant record itself is
indexed by `<name>:<hash>` and has no fallback.

**`skills_refine` semantics.** Refining a skill produces a new content and
therefore a new hash. The grant on the old hash is preserved (audit) but
no longer applicable to retrieval; the new hash starts at L2.

---

## 5. Feedback-Collapse Guard

The current skills loop has a known feedback-collapse problem: `skills_refine`
is called by the same agent that just executed the skill. The agent that
ran the skill is not an independent evaluator — it has every incentive to
confirm its own selection.

**Rule.** `skills_refine` (and any equivalent learn-back signal) MUST NOT
be the sole input to L1 → L2 promotion or to L3 grant renewal. At least one
of the following independent signals MUST also be present:

- a different agent (different session id, different operator, or a
  designated evaluator agent) ran the same skill on a related task and
  emitted a refine signal;
- an out-of-band check (test pass, schema validation, lint pass) succeeded;
- a downstream consumer (different service in the mesh) emitted a
  positive `world.*` signal correlated to the skill's effect window.

**Implementation hook.** The triage stage (§2) MUST read the proposal's
signal set and reject (or defer) any proposal whose only positive signal
is from the originating agent identity. The cause field is `feedback_collapse`.

**Why this isn't paranoia.** A skill that survives only because its author
keeps confirming it has not been validated; it has been laundered. The
substrate's modifiability criterion requires that improvements be
*detectable improvements*, which requires independent measurement.

---

## 6. Multi-Agent Quorum (For L4 Code-Modifying Proposals)

L4 proposals (code-modifying or destructive) require explicit operator
approval per occurrence under the current constitution (Article IV.2). If
constitution Article IV is ever amended to permit autonomous L4 application,
the gate MUST satisfy the minimum quorum contract below.

**Minimum contract for quorum (when enabled).**

- At least three agents from non-correlated sessions (different model
  versions or different operator origins) MUST independently approve.
- Each agent MUST have read the proposal, its diff, its triage record, and
  the affected SPEC chapter(s).
- A single dissent MUST defer the proposal to operator review; quorum is
  not majority-rule.
- The quorum record (agent identities, timestamps, signed approvals) MUST
  be persisted as an audit artefact and is itself L4 (immutable).

**Until the constitution permits autonomous L4, this section is dormant.**
It is specified now so that any future amendment has a concrete mechanism
to point at, rather than re-litigating quorum at amendment time.

---

## 7. Rollback for Self-Modifying Components

L4 changes that modify executables, schemas, or specs MUST be revertible
through the SPEC 08 §6 atomic deploy + LKG contract. This SPEC adds three
proposal-side requirements:

1. **Pre-apply LKG capture.** Before applying an L4 proposal, the apply
   stage MUST verify that an LKG snapshot of every artefact the proposal
   touches exists and is healthy (smoke-test rc 0).
2. **Apply window.** L4 application MUST occur within a single ABP
   transaction window: either all artefacts swap or none. Partial L4 is
   forbidden; on partial failure, SPEC 08 §6 rollback applies.
3. **Revert proposal.** Every applied L4 proposal MUST emit a
   `revert_proposal_id` referring to a pre-generated, validated reversal.
   The reversal does not itself need approval at apply time — it inherits
   the original's grant. This makes Article IV.5 circuit-breaker reverts
   mechanical rather than discretionary.

Schema migrations follow SPEC 08 §7 drift discipline: forward auto, backward
refused, breaking requires operator. Self-improve does not relax those
rules; it only enforces that L4 schema proposals carry a backward-compatible
revert proposal alongside.

---

## 8. Constitution Interlock

This SPEC operates within constitution Article IV's policy matrix. The
mapping:

| Constitution clause | This SPEC's mechanism |
|---|---|
| **IV.1 propose-only autonomy** | L1 (propose) automatic; surfaces to `improve.proposals` topic |
| **IV.2 first-execution gate** | L2 → L3 transition; TOFU on content hash (§4) |
| **IV.3 scope classes** | proposal record carries scope class; triage rejects mismatched class+target combinations |
| **IV.4 audit trailers** | every stage transition emits trailer (§2) |
| **IV.5 circuit breaker** | revert_threshold/window_days/pause_hours apply at the apply stage; revert_proposal_id (§7) makes breaker mechanical |
| **VI forbidden targets** | triage rejects with cause = forbidden_target regardless of class |
| **VII amendment process** | constitution edits are L4 by definition; require operator-authored commit per VII.1 |

**Constitution amendments are L4 with extra constraints.** A proposal to
edit `_spec/2026-04-20-00-constitution.md` MUST: (a) carry an Article VII amends
field, (b) have a corresponding CHANGELOG entry, (c) be operator-authored
in the commit step. This SPEC does not relax VII.1; the proposal mechanism
prepares the diff but does not commit it.

---

## 9. Required Topics and Capabilities

**Topics (retained snapshots per SPEC 03).**

- `improve.proposals.<service>` — open proposals targeting `<service>`
- `improve.triage.<service>` — triage decisions
- `improve.applied.<service>` — apply outcomes
- `improve.outcomes.<service>` — learn-back records
- `improve.grants` — active L3 grants by `<name>:<hash>` (mesh-wide)
- `improve.quorum` — quorum records, when §6 is enabled

**Capabilities (per service, discoverable via SPEC 07 §5 `spec.get`).**

- `<svc>.improve.propose` — submit a proposal (returns proposal_id)
- `<svc>.improve.triage` — request triage (idempotent on proposal_id)
- `<svc>.improve.apply` — apply an accepted proposal (idempotent)
- `<svc>.improve.refine` — submit learn-back signal (subject to §5 guard)

A service that bears no improvement surface (e.g., a pure transit daemon)
MAY omit these capabilities, but MUST advertise the omission via
`spec.get` so agents do not retry.

---

## 10. Open Questions

- **Rejection ledger.** Where do rejected proposals live? Per-service
  topic, or a mesh-wide ledger? An agent that re-proposes a rejected idea
  needs a way to find the prior rejection; otherwise rejection is silent
  and the loop wastes triage cycles.
- **First-execution human gate surface.** L2 requires human-in-loop, but
  the surface is not specified here (intentionally — display SPEC pending).
  The contract is "operator must explicitly accept"; the implementation
  may be a CLI prompt, a desktop notification, an SMS, or a queued review
  list. Which is canonical?
- **Quorum applicability.** §6 enumerates the contract for autonomous L4
  quorum, but the constitution does not currently permit autonomous L4.
  Should §6 be removed until the constitution permits it, or kept as
  forward provision? Current draft keeps it as forward provision.
- **Skill canonicalisation.** §4 references "deterministic serialisation
  defined in the skills-store contract" — that contract does not yet
  fully specify whitespace, metadata ordering, or refinement-history
  inclusion. Pending in the skills-store implementation.
- **Cross-mesh proposals.** A proposal originating on mesh node A
  affecting service on node B: who triages, who applies, where the audit
  trailer lives. Defer to a cross-mesh extension SPEC.
- **`_doc/2026-04-20-self-updating-systems-pattern.md` retirement.**
  When does the doc move to historical? Suggested: when SPEC 09 reaches
  status `stable` and at least one service implements the full loop end
  to end.

---

## Conformance

A service is **L0-conformant** if it does not participate in the improve
loop and advertises the omission via `spec.get`.

A service is **L1-conformant** if it emits proposals on `improve.proposals.<svc>`
with valid Proposal records and accepts triage decisions on
`improve.triage.<svc>`.

A service is **L2-conformant** if it additionally enforces the first-execution
human-in-loop gate for L2 proposals targeting itself, including refusal to
apply absent an explicit operator acceptance record.

A service is **L3-conformant** if it additionally honours content-addressed
TOFU per §4: storing hashes, refusing drift, and re-prompting on hash
change.

A service is **L4-conformant** if it additionally meets §7 rollback
requirements: pre-apply LKG verification, atomic apply window, and
revert_proposal_id emission.

The mesh as a whole is conformant at the lowest conformance level of any
participating service.

---

*§3.1 (tool-surface authority projection) added 2026-05-03 to make the
L0–L4 change-class gradient the canonical taxonomy for agent tool
surfaces as well as change proposals. Driver: the agentd ↔ mcp
unification plan (`_doc/2026-05-03-cosmix-agent-runtime-unification-
plan.md`) introduces an `Authority::L0/L2/L4` enum on the shared
`ToolProvider` trait; without a SPEC home the implementation would have
been a parallel taxonomy that future runtimes would have had to
reconcile. Pinning it here keeps every audit chain (constitution V.5)
joining on a single gradient.*
