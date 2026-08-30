---
title: CMM proposal pipeline — schema v2 and lifecycle decisions (A.3)
status: decided
date: 2026-05-10
supersedes_in_part: _plan/2026-04-20-skills-self-updating.md (sibling-pipeline shape)
historical_origin: git show "$(git rev-list -1 HEAD -- _plan/2026-05-10-cmm-phase3-priority.md)^:_plan/2026-05-10-cmm-phase3-priority.md"
review: /review-loop arch (Codex thread 019e1132-fdf1-71c1-95b0-9b3b269fedde)
---

# CMM proposal pipeline — schema v2 and lifecycle decisions

Originally settled Sub-phase A.3. The code-side proposal pipeline was retired
in `020c9732`; this ADR is dormant historical design. Drafted from a Codex
architecture pass on the original question set.

## Why this decision needs to land first

`_memory/proposals/` does not exist on disk yet — the workspace is
clippy-clean (baseline 5dbb449), so the existing observer
`cmm_cargo_clippy.mix` finds zero whitelist hits and writes nothing.
The v1 schema embedded in that script (`kind: clippy_<lint_short>`,
`tier:`, `lint:`, `target_files:`) is **uncommitted in practice** —
no historical artifacts to migrate. This is the cheapest moment to
fix the schema. Once Sub-phase A.2's smoke test runs and we have
real proposals on disk, every change becomes a migration.

A second pipeline is also pending: `_plan/2026-04-20-skills-self-updating.md`
proposes a sibling layout (`proposals/skills/`, sibling applier,
different field names — `applicable: auto` vs `gating:`). Resolving
that conflict now keeps the skill plan from re-deriving the lifecycle
contract from scratch.

## Decisions

### D1 — `applied/` retention: append-only

`_memory/proposals/applied/` stays append-only. No date-partition
prune, no index-then-delete sweep.

**Why.** Volume is currently zero; rotation pressure is hypothetical.
Disk markdown is the lifecycle authority of truth (see D-substrate
below); deleting it shifts authority to indexd, which is
retrieval-shaped, not human- or agent-editable lifecycle state.

**How to apply.** Observers and applier write/move proposals to
`applied/` without a TTL. Re-evaluate when (a) a single year of daily
runs accumulates >2k entries, or (b) a tooling step starts taking
noticeable wall-clock walking the directory. Until then, the
append-only ledger is the audit trail.

**Blocked alternative.** The "(c) hybrid — index-then-delete after
N days" option is rejected: it makes indexd authoritative for
historical lifecycle state, which weakens legibility and
modifiability for an agent diagnosing the pipeline.

### D2 — frontmatter schema v2

Drop the v1 ad-hoc fields. v2 has a stable kind-agnostic spine plus a
`kind_data:` block for kind-specific payload.

```yaml
schema_version: 2
id: <stable id — see D-identity>
kind: <hierarchical, dotted — see kind taxonomy>
status: proposed | accepted | rejected | applied | failed | blocked
gating: auto | ask | human
confidence: 0.0–1.0
observer: <script or daemon name>
created: <ISO8601>
source:
  chunk_id: <indexd id, optional>
  git_sha: <short, optional>
targets:
  files: [...]      # for code-shaped kinds
  skills: [...]     # for skill-shaped kinds
  config: [...]     # for tuning-shaped kinds
kind_data:
  # kind-specific freeform; the kind handler is the schema authority
  # for what lives here
block_reason: <string, optional>   # set by dispatcher when status: blocked
reviewed_by: <human handle, optional>  # required when gating: human (D3)
```

**Field rationale.**

- `schema_version: 2` — explicit. The next redesign won't be implicit.
- `kind` is **hierarchical** (dotted), not a flat enum. Concrete
  values defined below. Flat enums age badly once self-analysis (Sub-
  phase B) lands with open-ended kinds.
- `status` adds `applied | failed | blocked` to v1's
  `proposed | accepted | rejected`. `failed` = handler ran and didn't
  succeed (vs `blocked` = handler refused to run, e.g. unknown kind).
- `gating` (not `applicable`) is the lifecycle-authority field. Sub-
  phase B's `auto | ask | human` semantics apply across all kinds.
- `targets` namespaces the "what does this proposal touch" field by
  shape. Clippy proposals carry `targets.files`; skill proposals
  carry `targets.skills`; tuning proposals carry `targets.config`.
- `kind_data` is the only place kind-specific fields live. The kind
  handler owns its schema. The kind-agnostic spine stays clean.

**Body.** Below the frontmatter, the markdown body must include a
self-contained rationale snapshot. Indexd pruning is allowed to
dangle `source.chunk_id`; the proposal must remain readable without
indexd. (If indexd is down, agents must still triage.)

### D2.1 — kind taxonomy v1

Hierarchical, three top-level families. Add families when a real
fourth observer demands one.

```
code.clippy.<lint>      # cmm_cargo_clippy
code.<other-tool>.<rule>  # reserved for future static-analysis observers
skill.graduate          # cmm_skill_proposals (planned)
skill.archive           # cmm_skill_proposals (planned)
skill.merge             # cmm_skill_proposals (planned)
analysis.tuning         # cmm_self_analysis (Sub-phase B), parameter ±20%
analysis.structural     # cmm_self_analysis (Sub-phase B), new observer/kind
analysis.docs           # cmm_self_analysis (Sub-phase B), doc/CLAUDE.md edits
```

`code.*` defaults to `gating: ask` (human triage required). `skill.*`
defaults to `gating: auto` for graduate/archive (per the skills plan
trigger thresholds), `ask` for merge. `analysis.*` follows Sub-phase
B's bucket mapping (`tuning: auto`, `structural: ask`, `docs: human`).

Defaults are observer-set. Handlers may refuse stricter→looser
overrides at validate-time.

### D2.2 — proposal identity

Identity is hash-discriminated; the hash input must include `kind`
and normalized `targets` so the same body string under two kinds
doesn't collide. The current scheme (`<date>-clippy-<lint>-<8hex>.md`,
hash of observation text only) is collision-prone for that reason.

v2:

- **Hash input:** `blake3(kind || "\n" || normalized_targets_json || "\n" || body_text)`,
  truncated to 8 hex chars.
- **Filename:** `<date>-<kind-with-dashes>[-<target_slug>]-<8hex>.md`
  — `target_slug` is **optional**, included only when `kind` alone
  doesn't disambiguate within a date+hash (e.g. when one observer
  emits multiple proposals per tick that share a kind). Each kind
  family chooses what its slug is; the slug is for human-readability
  while scanning, the hash is for identity.
- **Per-family slug rules:**
  - `code.clippy.<lint>` — slug omitted. The lint is already in
    `kind`; the file basename is in `targets.files[0]` and the body.
  - `skill.<verb>` — slug = the `skill_id` (numeric).
  - `analysis.<sub>` — slug = a short config-key or analysis-tag.
- **Examples:**
  ```
  2026-05-11-code-clippy-needless_borrow-a1b2c3d4.md
  2026-05-11-skill-graduate-47-9f8e7d6c.md
  2026-05-11-analysis-tuning-retrieval_top_k-7e6d5c4b.md
  ```
- **`id:` field:** the filename stem.

### D3 — multi-applier dispatch: hybrid (one entry point + handler registry)

Reject both pure-grow (single-script monolith) and pure-split (sibling
appliers per kind with their own directories).

**Shape.**

```
_bin/cmm_apply.mix              # entry point — scans, groups, dispatches
_bin/handlers/code_clippy.mix   # one file per handler family
_bin/handlers/skill.mix
_bin/handlers/analysis.mix      # arrives in Sub-phase B
_bin/cmm_propose_lib.mix        # shared write-side helper (per A.1)
```

`cmm_apply.mix` walks `_memory/proposals/` (single directory — no
per-kind subdirs), filters proposals by **eligibility** (see below),
groups by `kind`, looks up the handler whose `kind_pattern` matches,
and dispatches. Each handler is a separate Mix file with a fixed
entry-point signature.

**Eligibility** is the join of `status` and `gating`, not just
`status: accepted`:

| `gating` | Eligible when |
|---|---|
| `auto` | `status: proposed` (no human triage required) |
| `ask` | `status: accepted` (human moved status forward) |
| `human` | `status: accepted` AND a `reviewed_by:` field naming a human (per Sub-phase B.2) |

`applied`, `failed`, `rejected`, `blocked` are terminal for the
dispatcher — proposals in those states are never re-applied.
Re-emission of the same proposal id is a no-op (same hash → same
filename → `exists()` skip), so terminal states are durable.

This is the load-bearing reason `gating` lives in the schema spine
rather than per-kind: the dispatcher must read it without knowing
the kind.

**Why hybrid, not pure-split.** Pure-split paired with separate
directories loses the unified pending queue —
`cmm_proposals_summary.mix` would have to walk per-kind subdirs and
each apply tier would have its own scheduling concerns. The summary
job, the triage email, the retention sweep, and the learn-back path
all want one queue.

**Why hybrid, not pure-grow.** A monolith dispatcher means every new
kind edits the same script, the test surface conflates kinds, and the
clean-tree precondition for clippy bleeds into kinds that don't need
it.

### D3.1 — handler contract

Each handler file exports a record with these fields. Today only the
v1 single-kind clippy applier (`cmm_apply_proposals.mix`) exists; the
v2 `code.clippy` handler is the planned replacement. The contract is
forward-defined so skill and analysis handlers conform without
re-deriving it. v1 stays in place until A.2's smoke test validates
the v2 path end-to-end, then is deleted.

For the v1 cut, only `kind_pattern`, `apply`, and `verify` are
strictly required; the rest have safe defaults (`gating_supported:
[ask]`, `can_batch: false`, `preconditions: clean_tree`,
`commit_policy: none`, `failure_policy: leave_proposed`,
`learn_back: noop`). Skill and analysis rows below are
forward-defined target configurations, not required scaffolding for
the first ship.

| Field | Type | Purpose |
|---|---|---|
| `kind_pattern` | string (glob: `code.clippy.*`) | Which kinds this handler claims |
| `gating_supported` | list of `auto`/`ask`/`human` | Permitted gating values |
| `can_batch` | bool | Whether multiple proposals apply in one tick |
| `preconditions` | function → ok/err | Clean-tree, tool availability, branch policy |
| `validate` | function(proposal) → ok/err | Reject malformed/stale before applying |
| `apply` | function(proposals) → result | Perform the change |
| `verify` | function → ok/err | Domain-specific post-check (e.g. `cargo check`) |
| `commit_policy` | `none` / `per_proposal` / `batch` | Commit shape |
| `failure_policy` | `leave_proposed` / `mark_failed` / `move_rejected` | What to do on apply failure |
| `learn_back` | function(result) → unit | Record outcome (refine skills, update memory, etc.) |

`code.clippy` settings: `gating_supported: [ask]`, `can_batch: true`
(group by `lint`), `preconditions: clean_tree`, `verify: cargo check`,
`commit_policy: batch`, `failure_policy: leave_proposed`,
`learn_back: noop` (today; later: track lint acceptance rate).

`skill.graduate` (planned): `gating_supported: [auto, ask]`,
`can_batch: false` (each graduate is independent), `preconditions:
none`, `verify: cosmix-skills-cli list confirms`,
`commit_policy: per_proposal` (just the proposal markdown move),
`failure_policy: mark_failed`, `learn_back: skills_refine`.

### D3.2 — per-tick apply cap

Borrow from the skills plan: per-handler cap = 10 applies per tick.
Bounds blast radius if an observer over-emits.

## Cross-cutting decisions

### D-substrate — authority of truth

| Concern | Authority |
|---|---|
| Pending lifecycle state (status, gating, kind, targets) | `_memory/proposals/**/*.md` on disk |
| Observation evidence, retrieval, supersession scoring | indexd |
| Applied audit trail | `_memory/proposals/applied/` on disk |

Agents asking "what's pending?" read disk first. indexd is for
retrieving the *evidence behind* a proposal, not its lifecycle state.

### D-failure-modes

Concrete behavior for the four breakage cases surfaced in Codex
review:

1. **Unknown `kind`.** `cmm_apply.mix` fails closed: sets
   `status: blocked` and `block_reason: no_registered_handler` (both
   v2 schema fields per D2). Summary surfaces it under the blocked
   tally, not silently skipped. A later observer run that re-emits
   the same proposal id is a no-op (same hash → same filename →
   `exists()` skip).
2. **Pruned `source.chunk_id`.** Proposal markdown body must contain
   the rationale snapshot; indexd pointer becomes informational only.
3. **Same content hash, different kinds.** Identity includes `kind`
   (D2.2). No collision.
4. **Auto + human-triaged ask at same tick.** `cmm_apply.mix`
   acquires a directory-level lock at startup (`flock` on
   `_memory/proposals/.lock`, created on first run), and each
   handler's `validate()` re-reads the proposal status immediately
   before `apply()`. Stale status → skip and re-queue. The lock is
   the dispatcher's responsibility, not the handler's; handlers
   assume single-writer execution.

`block_reason:` (D2) is set only when `status: blocked` (e.g.
unknown kind, validate refused). Apply-time failures take the
handler's `failure_policy` path: `mark_failed` sets `status:
failed` plus a handler-defined `failure_reason:` field in
`kind_data`; `move_rejected` sets `status: rejected`;
`leave_proposed` keeps the proposal in the queue for the next tick.

### D-helper-location

`cmm_propose_lib.mix` lives in `_bin/` for now (not a
`cosmix-lib-mix` builtin, not a daemon ABP action).

**Why.** The schema is unproven across more than one real producer.
Per `project_mix_builtin_gaps.md`'s 3-occurrences promotion rule, we
promote to a builtin when three observers have converged on the same
API. Sub-phase A.2 (smoke test) gives us producer #1 (clippy);
Sub-phase A.1 grows producers #2–3 (memory_hygiene, doc_freshness).
Promote to `cosmix-lib-mix` only after all three converge.

A daemon/ABP boundary is rejected at this stage: it adds operational
coupling before the lifecycle contract is stable. Sub-phase B can
emit proposals from `claude --print` output by piping into a Mix
wrapper that calls `cmm_propose_lib`; no daemon needed.

## Consequences for the 2026-04-20 skills-self-updating plan

> **Plan retired 2026-07-23** (superseded by this decision; full text in git
> history). The section below is kept as the record of what this schema
> decision changed relative to that draft — the consequences are moot as
> rewrites but stand as the ratified taxonomy.

The skills plan (status: draft) needs schema-and-shape rewrites in
its §2, §3, §4, §5 before it reaches `status: ready`. The
substantive content (kind triggers, threshold calibration, phasing)
is unaffected; only the schema and layout shift.

**§2 Proposal kinds — taxonomy rename.** Underscored kinds become
dotted: `skill_graduate` → `skill.graduate`, `skill_archive` →
`skill.archive`, `skill_merge` → `skill.merge`. (D2.1.)

**§4 Proposal markdown shape — full schema rewrite.** Replace the
current §4 example with the v2 spine. Concretely:

- Add `schema_version: 2` and `id: <stem>`.
- Drop `tier:` (cosmetic; not in v2).
- Drop top-level `applicable:` → use spine `gating:` (D2).
- Move top-level `skill_id:`, `skill_name:`, `domain:`,
  `usage_count:`, `success_rate:` into `kind_data:`.
- Add `targets.skills: [<skill_id>]` so the dispatcher can find
  the target without reading `kind_data`.
- Drop top-level `confidence:` if duplicated elsewhere; keep one
  copy in the spine.

**§3 Observer — directory.** Drop `_memory/proposals/skills/`;
proposals go to the unified `_memory/proposals/` directory, with
`kind: skill.*` discriminating.

**§3 Observer — filename.** Update from
`<date>-<kind>-<skill_id>-<blake3_8>.md` to v2's
`<date>-<kind-with-dashes>-<target_slug>-<8hex>.md` (D2.2). For
skills, `target_slug` is the `skill_id`.

**§5 Applier — replaced by v2 handler.** Drop
`cmm_apply_skill_proposals.mix` as a sibling script. Replace with
`_bin/handlers/skill.mix` per D3, conforming to D3.1's handler
contract. The skills plan's apply sequence redistributes:

- **Dispatcher (`cmm_apply.mix`) owns:** scanning the unified
  proposals directory, eligibility filtering (gating × status from
  D3), grouping by `kind`, kind→handler lookup, the per-handler
  per-tick cap of 10 applies, post-apply move to `applied/`.
- **Handler (`_bin/handlers/skill.mix`) owns:** the kind-specific
  steps — running `cosmix-skills-cli` for the matching verb,
  per-proposal verify (`cosmix-skills-cli list` confirms the status
  change), `learn_back` via `skills_refine`. The handler does not
  scan and does not dispatch.

## Out of scope (deferred to A.4 or Sub-phase B)

- **Substrate documentation** of the pipeline as a whole. A.4 writes
  `_doc/cmm/proposal-pipeline.md`, which is the agent-facing
  contract derived from this decision record.
- **`cmm_propose()` helper signature.** A.1 finalizes the Mix-level
  API; the schema above is the on-disk contract it must produce.
- **Self-analysis kind-data fields.** Sub-phase B.0's spike chooses
  the invocation mechanism; B.1 finalizes `analysis.*` kind_data.
- **Indexing applied proposals into indexd.** Useful for cross-time
  retrieval but not lifecycle-load-bearing. Defer until volume
  justifies.

## Validation

- Codex thread `019e1132-fdf1-71c1-95b0-9b3b269fedde` covers Q1 / Q2 /
  Q3 plus six derived architecture questions; recommendations
  encoded above with Codex's blocked-options preserved verbatim.
- Schema v2 has zero on-disk artifacts to migrate (verified:
  `_memory/proposals/` does not exist).
- Handler contract is forward-defined for two kinds beyond clippy
  without committing their applier code (skill, analysis).
