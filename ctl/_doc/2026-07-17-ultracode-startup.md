# Ultracode workflow method — session startup

Loaded into every cmctl session via `@` from `CLAUDE.md`. This is the working
method: **orchestrate non-trivial work as ultracode workflows, with codex doing
the bulk work at stages and Claude-family agents authoring, verifying, and
synthesizing.** Distilled from Theo's findings
(`_doc/2026-07-17-ultracode-workflows-driving-codex.md` — read it for the full
reasoning and sources); this file is the operational subset.

## Latest and best, always

Both frontier engines ride the **best currently-available model, regardless of
what it is at the time** — this method must survive 6–12 months of model churn:

- **Frontier Claude** = the strongest Claude available to the session
  (2026-07-17: fable‑5; when it leaves the MAX plan — expected within a week —
  fall back to opus‑4.8 or whatever 5.x succeeds it, without waiting for doc
  updates).
- **Codex** = the strongest model in `~/.codex/config.toml` (2026-07-17:
  `gpt-5.6-sol`, high reasoning). Check the config, don't assume.

Persistent docs use the **role names** ("frontier Claude", "codex"); hardcoded
model ids appear only as dated "currently X" annotations. When either vendor
ships a better model, update the config/session and the annotations — the
method doesn't change.

## The core principle

**Workflows are code, so they actually end.** A workflow script fixes the
phases before execution; a phase may fan out wide, but the run always
terminates when the script returns. Free-running agent swarms don't — and burn
roughly 4× the tokens for comparable output. Deterministic orchestration is
the token-efficiency and quality win. Ultracode isn't a reasoning level; it's
the opt-in that tells the session to work this way (and it pins xhigh, not
max — max costs ~2× the tokens of xhigh for a 4–10% bench gain, so max is
reserved for rare single-agent hard problems, never fan-outs).

## The arc for any non-trivial task

1. **Scout inline.** Build the work-list cheaply in the main session (list the
   files, scope the diff, find the sites). You need the shape of the
   *orchestration step*, not the whole task, before writing the script.
2. **Frontier Claude authors the workflow script** — `meta` with named phases,
   a **schema per phase** (typed outputs are what let plain code route work),
   a common prompt prefix, and per-`agent()` model + effort chosen per stage.
3. **Bulk/mechanical stages go to codex** via wrapper agents (mechanics
   below). Implementation, transforms, sweeps, cold diff-reading — anything
   with a clear spec and no need for live session state.
4. **Verification stages stay Claude-family** — they can drive our harness
   (tests, ABP calls, props stores, e2e against real data). Route stage
   outputs with plain code: filter on structured fields ("needs follow-up",
   "confirmed") to decide what flows to the next phase.
5. **Adversarial review is a workflow phase**, not a ritual afterwards: a
   `codex:review` stage reads the diff cold with a targeted hunt list and
   returns findings (severity + file:line + concrete failure scenario) through
   a schema; a fix phase consumes them; a **re-review phase confirms** them
   (see Convergence below). Reject wrong findings with primary-source
   evidence.
6. **Ship**: convergence reached, version-bump gate, commit AND push
   immediately, journal if the work warrants it.

Trial-run the loop on ~3 items end-to-end before fanning out over many.
`log()` anything dropped — silent truncation reads as full coverage.

## Convergence — no partial fixes (reinstated 2026-07-17, proven method)

Every substantive change — code or normative doc, workflow-orchestrated or
inline — converges through a continuous review loop before it ships:

1. **Cold review** of the actual diff, TWO arms in parallel (standing rule,
   Mark 2026-08-15 — "add the GLM-5.3 review arm to ALL decisions"):
   - a `codex:review` workflow phase or an inline fresh-thread codex pass, and
   - a **ZCode/GLM arm**: `zcode --cwd <repo> --mode plan -p "<same review
     prompt>"` (read-only; explicit `timeout` — headless zcode has no built-in
     cap). Different model family, fails differently; costs nothing against
     either the Claude or the codex budget.
   Both arms get the standing prompts in `CODEX.md` § "Review-stage prompts" —
   including the assumption question: *what is this change relying on that is
   not enforced anywhere?* Merge + dedup findings before the fix pass; every
   finding from either arm is verified against source before acceptance (GLM
   is an additional detector, not a gate authority — a finding neither arm can
   evidence is rejected with the evidence).
2. **Fix by severity**: BLOCKER/MAJOR before commit; MINOR/NIT fixed or
   explicitly `Wontfix: <reason>`.
3. **Re-review the fixes** — the load-bearing step. Ask explicitly: *fully
   fixed / partially fixed (what residual remains) / unaddressed*, per finding.
   This is what refuses the partial fix whose symptom is gone but whose root
   is one indirection deep (shared on-disk state, a fallback branch quietly
   preserving old behavior).
4. **Iterate until convergence**: "no issues found", or every finding
   dispositioned. Inline: reply rounds on the same codex MCP thread. In a
   workflow: code the review→fix→re-verify loop into the script with a bounded
   round count (2–3; escalate to Mark if still contested after that).

Nothing substantive ships on round one's output alone — round-two catches are
the norm, not the exception.

## When inline (no workflow), the loop still applies

Small-to-medium changes that don't warrant a workflow still get steps 1–4 via
the codex MCP on the current feature-arc thread. The retired apparatus was the
skill zoo, not the discipline.

## MCP vs `codex exec` — which route, and why

Same codex model both ways; the choice is **context handling, not quality**:

- **MCP (`codex` + `codex-reply`)** keeps a **persistent thread** — a follow-up
  round continues the same warm context, so review/fix rounds don't re-ingest
  the code. The token-efficient route for **iterative work on one feature-arc**
  (design → implement → review → fix). Fails by **idle-aborting after ~30 min
  of silence** — but the *work* completes; the *reporting* aborts, so verify
  the tree and never discard. Goes stale after a mid-session `codex login`
  (`/mcp` reconnect).
- **`codex exec`** is a **stateless cold one-shot** — re-pays context every
  call, so worse for tight loops, but it's the **only** route inside a
  workflow/agent (no MCP session there) and the more robust choice for
  **fan-out** (`isolation: 'worktree'`), bulk, and any single run past ~30 min
  (background + poll sidesteps the MCP idle-abort entirely).

**Rule:** inline single-arc multi-round → MCP; fan-out / bulk / in-workflow /
>30-min run → `codex exec` backgrounded. This is exactly the split the method
already encodes — consults and inline review rounds on the MCP, workflow stages
on `codex exec`.

## Codex-in-workflow mechanics (every codex stage)

Workflow agents have no MCP session, so codex inside a workflow is always a
thin wrapper agent running the CLI:

- Wrapper: `model: 'sonnet', effort: 'low'`, label **`codex:<stage>`** (the
  UI shows the wrapper's model; the label is the only signal).
- The codex prompt must be **fully self-contained** — paths, what changed,
  what's already verified. Wrapper stages are stateless and inherit nothing.
- Canonical invocation (no permission prompts, no stdin wedge):

  ```sh
  codex exec -C <workdir> "$prompt" </dev/null
  ```

  No-approval policy is set globally in `~/.codex/config.toml`
  (`sandbox_mode = "danger-full-access"`, `approval_policy = "never"`,
  2026-07-18) — no per-invocation flags needed; codex never requests
  approvals mid-run (there's no one to answer inside a wrapper agent).
  `</dev/null` always (it wedges on stdin); explicit `timeout` or
  background + poll (it can exceed Bash's 10-min default), watchdog.
- `schema` on the wrapper so the report comes back structured.
- `isolation: 'worktree'` when parallel stages write files.
- **Count/round caps, not `budget`**, as the primary bound on codex-heavy
  loops — `budget` only meters the thin Claude wrappers.
- Codex's reasoning effort is its own knob, set on the `codex exec`
  invocation (config default or `-c` override), decided per stage like any
  other effort choice.

Design consults and follow-up rounds with codex happen **inline in the main
session** via the codex MCP (one thread per feature arc keeps its context
sharp); the MCP goes stale after a mid-session `codex login` → `/mcp`
reconnect. **Omit `sandbox`/`approval-policy` params on MCP calls** — they
override the config.toml no-approval policy for the whole thread and bring
permission prompts back; inheriting the config is always the no-prompt path.

## Model + effort selection per stage

From the global rubric (intelligence > taste > cost for anything that ships):

| stage kind | engine | effort |
|---|---|---|
| orchestration script, architecture, synthesis | frontier Claude (inherit) | inherit |
| bulk implementation, transforms, sweeps | codex wrapper | wrapper low; codex effort per difficulty |
| cold adversarial review | codex wrapper + ZCode/GLM arm (both, always) | wrapper low |
| verify/judge (hardest) | frontier Claude / step-down | high–xhigh |
| mechanical glue, formatting | haiku/sonnet | low |
| user-facing taste (UI, copy, public API shape) | frontier Claude / step-down only | — |

Never blanket-max a fan-out; the cost multiplies by every agent in the phase.

## Prompt hygiene (Theo's hardest-won lesson)

- **Hand-write global prompts, skills, and standing instructions.** Never
  agent-generate them. Minimal and general beats prescriptive — style
  constitutions, fixed pixel values, mandated cadences become compulsions in
  strong instruction-followers and make output *worse*.
- **Behavioral bugs are prompt bugs first.** A model that always does some
  weird specific thing was probably told to; read the operative prompt/skill
  before working around the behavior. Recurring finding class → fix the
  generator (prompt/harness/spec), not each instance.
- **Keep the model vocabulary current.** The global CLAUDE.md model table is
  what makes "workflow this, codex does the heavy lifting" work from a
  one-line ask; update it when the fleet changes.

## When NOT to workflow

Conversational turns, trivial mechanical edits, single-fact lookups, anything
a single inline agent finishes faster than the script takes to write. The
method is for work with genuine fan-out, phases, or verification structure —
not a tax on every keystroke.
