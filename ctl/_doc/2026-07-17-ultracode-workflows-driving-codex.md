# Ultracode workflows as the codex driver — lessons from Theo's harness deep-dive

**Sources:** Theo (t3.gg), two companion videos, distilled from auto-caption
transcripts (model names normalized: "5-6 Soul/Terra" = GPT‑5.6 Soul/Terra
variants; "pie" = the Pi harness):

- "I can't believe they released this", 2026-07-14,
  https://www.youtube.com/watch?v=t8hfOyF4ehw (25:58) — what Ultra/ultracode
  actually are, Codex sub-agent V1/V2 internals, workflow-file anatomy.
- "I need you to hear me out (it's REALLY good)", 2026-07-16,
  https://www.youtube.com/watch?v=Noo0NWD0gHU (30:48) — GPT‑5.6 inside Claude
  Code, the Codex system-prompt teardown.

**What this doc is:** a reference guide capturing Theo's findings and mapping them
onto our existing fable‑5 + codex (gpt‑5.5) setup, so any session can follow the
same principle: **let the Claude Code harness orchestrate via Workflow (ultracode),
and put codex at the stages where it does the actual work.** It records practice,
not new architecture; the normative model-picking rules stay in
`~/.claude/CLAUDE.md` and `~/.cmctl/CLAUDE.md`.

---

## 1. Theo's core findings

Theo's experiment: run GPT‑5.6 *as the driving model inside Claude Code* (via
CLIProxyAPI + his Codex subscription) instead of inside Codex's own harness. Results
were markedly better — and the reasons generalize.

### 1.1 Workflows beat free-running agent swarms

- Codex sub-agents V1: flat, simple — a handful of tools (spawn/send/wait/close/
  resume), each sub-agent a separate thread with its own scoped context, results
  summarized by the parent. Codex V2 (unfinished, but force-routed for the
  Soul/Terra models via the models-cache JSON regardless of your config): a
  path-named task tree (`/root/research/tests`) with mailboxes — typed messages
  between agents, follow-up tasks to idle helpers, list/interrupt — results
  routing only to the direct parent, **no depth limit** (infinitely nestable, 4
  concurrent slots by default), and **the full main-thread history copied to
  every sub-agent by default**. Theo's verdict: context pollution plus a massive
  cost increase (and the copy filters out tool calls, so it busts the prompt
  cache it was presumably meant to preserve) — "noisier … and it burns way, way,
  way more tokens". He endorsed a viewer (JKF)'s take that "it sounds like too
  many people designed sub-agents V2".
- Claude Code Workflow inverts the design: **the model writes the orchestration
  code up front** — one JS file, executed top to bottom, with phases, per-stage
  prompts, and per-stage sub-agents. Codex ships orchestration code in the harness
  and hopes the model calls it right; Claude Code lets the model author the
  orchestration for the task at hand.
- **"Since workflows are code, they actually end."** Codex Ultra "can just go
  forever"; a workflow terminates when the script returns. Theo reports seeing
  **the same or better output quality at roughly a quarter of the token usage**
  for the same task (his observation on his own limits, not a controlled
  benchmark). Deterministic control flow is the token-efficiency win, not a nicety.
- Anatomy of his real workflow files (from the 07-14 walkthrough): a `meta`
  block (name, description, phases), a **schema per phase** for typed outputs, a
  common prompt prefix appended programmatically, mixed models per `agent()`
  call (Soul/Terra/Fable each at their own effort), and stage-routing decided in
  plain code — e.g. filter on a structured response field ("needs follow-up",
  "should keep researching") to decide what flows to the next phase. Simple runs
  were two phases (review → synthesize); real ones went five (research → verify
  → synthesize → critique → finalize), including some with 12+ phases.
- Why this shape wins, in his framing: tool-call-driven sub-agents make the LLM
  itself carry all the orchestration context turn by turn; fully hardcoded agent
  rosters (opencode-style) are too rigid. Workflows are "hardcoded on the fly" —
  the structure is fixed before execution, so a phase may fan out to 72
  sub-agents if the work demands it, "but it will eventually get to the end,
  always". His conclusion on Codex's Ultra: "OpenAI copied the wrong parts" —
  the UX (a fake reasoning level) rather than the implementation (workflows).
- His verdict: "The workflows in Claude Code are the best implementation I have
  seen of orchestrating sub-agents on real day-to-day work … nothing else comes
  close." Other harnesses (Pi, opencode, oh-my-pi) don't solve orchestration at
  all — that's the feature that matters, not terminal UX.

### 1.2 The harness/system-prompt matters more than the model

The bulk of the video is a teardown of the Codex system prompt, and the lessons are
portable:

- **Over-prescription makes capable models worse.** Codex's front-end
  "constitution" (~25% of the prompt: cards at 8px radius or less, Lucide icons
  whenever one exists, limits on specific palettes, no landing pages unless
  absolutely required, a ban on visible instructional text) is why every
  Codex-designed page looks the same and why empty states are bad. Strong
  instruction-followers obey arbitrary global rules *over* the user's design
  system, domain convention, and the actual request. The same GPT‑5.6 produced
  clearly better designs under Claude Code's prompt — which contains **no design
  guidance at all** (the word "frontend" appears only in "verify UI changes in a
  browser" and a memory example).
- Behavioral tics trace straight to prompt lines: the 30-second timer fixation
  ("provide user updates every 30 seconds"), premature building during planning
  ("do not stop at a proposal, implement the fix"), refusal to stop ("continue
  until solved").
- **Write prompts, skills, and standing instructions by hand.** "Prompts should be
  by hand, especially global ones and skills." Theo's read on the Codex prompt is
  that it "feels more thrown together than crafted" — his fix is a from-scratch
  hand-written rewrite. Keep them minimal, general, and tone-consistent;
  when a model misbehaves systematically, read the prompt before blaming the model
  (our `feedback_fix_the_process_not_the_code` memory says the same thing from the
  other direction).

### 1.3 Steering a mixed-model fleet needs a standing vocabulary

- The workflow habit doesn't happen by itself: Theo appends a standing instruction
  ("please use workflows for this task") so vague prompts orchestrate instead of
  running inline.
- He also plans to give the harness a **model roster** — what "Soul", "Terra",
  "Fable" mean and when to use which — in his system prompt or global AGENTS.md,
  so "analyze this codebase with a workflow using Soul, Terra and Fable on high
  reasoning" just works from a vague ask.
- Foreign models in the Claude harness have small rough edges: GPT‑5.6 loses track
  of task state slightly more, fumbles Claude-Code-specific formatting (markdown
  numbering, link rendering), and doesn't report live token usage. None were
  blocking; output quality was the surprise, not the friction.

### 1.4 Ultra / ultracode are skills, not reasoning levels — and effort economics

The 07-14 video's central correction:

- **Ultracode (Claude Code) and Ultra (Codex) are system-prompt appends** — a
  skill-style "please use more sub-agents" toggle — not reasoning levels, despite
  both being presented on the effort slider. Selecting ultracode in `/effort`
  sets the model to **xhigh, not max**, with ultracode as a separate state.
- That xhigh default is the deliberate, correct call: in Theo's numbers, **max
  costs roughly 2× the token burn of xhigh/high for a 4–10% bench improvement**
  — "I don't think max makes sense almost ever."
- Codex's Ultra gets this wrong twice: the root *and every spawned sub-agent*
  run at max, V2 offers no per-sub-agent reasoning control, and there's no depth
  limit — "nearly infinitely recursive maxes". Theo blew a 5-hour usage limit
  in 20 minutes on a single Ultra run (on fast mode, and with V2's default
  4-concurrent-slot cap raised — which he called a big part of the burn rate),
  then again within the next 40 minutes: twice in under an hour.
- Claude Code workflows expose exactly the lever Codex lacks: **per-`agent()`
  model and effort**. That, plus code-bounded termination, is the whole
  cost-control story.
- His advice to people scared of falling behind: defaults are fine; the current
  Ultra implementation is bad, but the vendors iterate fast — don't use Ultra,
  use workflows.

---

## 2. Mapping onto our setup

We already run the **complementary topology** to Theo's: he swapped the harness
model (GPT‑5.6 *is* the driver); we keep fable‑5 as the driver and reach codex
(gpt‑5.5) as a worker/reviewer. Theo's findings strengthen our pattern and sharpen
one part of it: **codex belongs at workflow stages, not just at review time.**

The equivalences, piece by piece:

| Theo's setup | Ours |
|---|---|
| Standing "use workflows" instruction | `ultracode` keyword / session toggle (explicit opt-in per Workflow policy) |
| Model roster (his proposed AGENTS.md append) | Model table + rubric in `~/.claude/CLAUDE.md` |
| GPT‑5.6 driving via CLIProxyAPI | fable‑5 drives; codex reached via MCP (in-session) or `codex exec` (in workflows) |
| Sub-agents per workflow stage | `agent()` calls; gpt‑5.5 stages via the wrapper-agent pattern |
| "Workflows actually end" | script returns + `budget` guard |

### 2.1 The recipe: fable authors the workflow, codex does the work

1. **Opt in.** Say `ultracode` (or ask for a workflow) — Workflow is explicit-opt-in
   here, which is our version of Theo's standing instruction with a safety catch.
2. **Fable writes the orchestration script** — phases, schemas, verification
   stages. (Theo found GPT‑5.6 handles workflows fine too; fable gets the
   author seat here because our rubric puts it on architecture/orchestration —
   intelligence > taste > cost — and the script is cheap relative to the work.)
3. **Bulk/mechanical stages go to codex** via the wrapper-agent pattern from
   `~/.claude/CLAUDE.md`: a thin wrapper (`model: 'sonnet', effort: 'low'`) writes
   a **self-contained** codex prompt, runs `codex exec` via Bash, returns the
   report through `schema`. Label it `gpt-5.5:<stage>` — the UI shows the
   wrapper's Claude model, so the label is the only signal.
4. **Adversarial review stays codex** — either as a workflow stage (wrapper agent
   reviewing another stage's diff) or post-workflow via `/codex-review` on the MCP.
5. **Verification stages stay Claude-family** when they need harness context
   (running our tools, reading props stores, ABP calls) — codex prompts must be
   self-contained, so anything needing live session state is a poor codex stage.
6. **Set effort per stage, and don't max the fan-out.** Per-`agent()`
   model/effort is the cost lever Codex's V2 lacks (§1.4) — but note it only
   governs **Claude-family stages and the wrappers themselves** (`effort:
   'low'` on the Sonnet wrapper, always). The codex model's reasoning effort is
   a separate knob, set on the `codex exec` invocation itself (config.toml
   default or an explicit `-c` override) — decide it per stage too. Either way,
   never blanket-max a wide fan-out: max costs roughly 2× xhigh for a 4–10%
   gain, multiplied by every agent in the phase.

Known mechanics (each has a backing memory — they bite):

- `codex exec` **wedges on stdin**: always `</dev/null`, plus a watchdog; it can
  exceed Bash's 10-min default, so set `timeout` or background + poll
  (`feedback_codex_exec_stdin_wedge`).
- Parallel codex *implementation* stages need `isolation: 'worktree'` — they
  mutate files and will collide otherwise.
- `budget.spent()` counts **Claude tokens only**; codex work is invisible to it.
  A budget guard on a codex-heavy loop only meters the thin Sonnet wrappers, so
  it badly under-counts real spend — use a count/round cap as the primary bound
  and keep the budget guard as a backstop.
- Codex stages are **stateless about the working tree**: the wrapper prompt must
  say what changed, what's staged, and what is already verified, or codex
  re-litigates settled ground (graduated codex-drill skill, same lesson).

### 2.2 Why this division, restated through Theo's evidence

- **Termination and token efficiency:** the workflow script is the thing that
  ends. In Theo's observation, free-running codex ("Ultra"-style) used roughly
  4× the tokens for comparable output. Wrapping codex calls inside a
  deterministic script gives gpt‑5.5's near-free bulk capacity *and* a hard stop.
- **Task-prompt quality is ours to control — the harness baggage is not.**
  `codex exec` still runs the Codex CLI, so the Codex system prompt (the one
  Theo tore apart) rides along under every wrapper stage; only Theo's
  model-swap route actually escapes it. What the wrapper buys is a
  hand-written, self-contained *task* prompt plus deterministic orchestration
  around the call — and Theo's crash-out got the worst front-end prescriptions
  removed for the 5.6-era prompt, so the baggage is shrinking. If codex stages
  show a system-prompt-shaped tic (30s-timer updates, premature building,
  refusing to stop), suspect the Codex prompt before the task prompt.
- **Design/taste work never routes to raw gpt‑5.5** (taste ≥ 7 rule stands).
  Theo's page comparison is direct evidence: model + prompt determine design
  quality, and gpt‑5.x under a design-prescriptive prompt produced slop. Our rule
  already encodes this; keep it.

### 2.3 The arm we deliberately don't run (yet)

Theo's actual configuration — gpt‑5.x as the **driving** model in Claude Code via
CLIProxyAPI — is worth knowing about but is not our pattern:

- **Buys:** the codex model's cost profile on the *whole* session, not just
  stages (Theo ran GPT‑5.6 Soul via his Codex subscription; our equivalent
  would be gpt‑5.5); escaping the Codex system prompt entirely; a first-hand
  check of Theo's quality claims.
- **Costs:** loses fable‑5 as orchestrator (the tie-break for anything that ships
  is intelligence > taste > cost, and orchestration/architecture is exactly where
  fable earns its seat); adds a proxy moving part + auth layer; foreign-model
  formatting/statefulness rough edges land on every turn instead of inside
  contained stages.
- **When to revisit:** if codex-stage volume grows to where wrapper overhead
  dominates, or for a disposable experiment session. It's a reversible experiment
  (`git revert`-class), not a one-way door.

---

## 3. Working patterns

### 3.1 Codex-heavy feature workflow (the codex-drill shape, workflow-ified)

```
Design (inline, BEFORE the workflow) — codex design consult via the MCP in the
    main session (binding unless technically wrong). Workflow agents have no
    MCP session, so inside the workflow codex is always a `codex exec` wrapper.
phase('Implement') — pipeline over modules: gpt-5.5:impl wrapper stages,
                     worktree isolation if parallel, schema-forced reports
phase('Verify')    — Claude-family agents run tests/e2e against real data
                     (codex can't drive our harness tools)
phase('Review')    — gpt-5.5:review wrapper reads the diff cold; findings with
                     severity + file:line + concrete failure scenario
phase('Fix')       — fix real findings; REJECT wrong ones with primary-source
                     evidence; iterate to SHIP verdict
```

Keep one codex MCP thread across the *main-session* rounds of the arc — design
consult before the workflow, `/codex-review` and fix-round replies after it —
where accumulated design context makes reviews sharper (graduated skill, 5
uses). The in-workflow `codex exec` wrapper stages are stateless and inherit
none of that thread's context: everything they need goes in the wrapper prompt.

### 3.2 Bulk migration / sweep

Scout inline first (build the work-list cheaply), then one workflow:
`pipeline(items, transformStage, verifyStage)` — stage 1 a `gpt-5.5:transform`
wrapper, stage 2 a Claude verify agent; each item flows through both with no
barrier. Trial-run ~3 items end to end
before fanning out (`feedback_trial_run_before_fanout`). `log()` anything dropped —
no silent caps.

### 3.3 Understand-a-codebase (Theo's own demo case)

Parallel readers per subsystem, mixed models on high effort, structured map out.
This is the "vague ask orchestrates properly" case the standing vocabulary exists
for — Theo demoed exactly this against the Codex codebase.

### 3.4 Checklist for any codex-in-workflow stage

- [ ] Wrapper agent `model: 'sonnet', effort: 'low'`, label `gpt-5.5:<stage>`
- [ ] Codex prompt fully self-contained (paths, diff summary, what's already verified)
- [ ] `codex exec … </dev/null`, watchdog, explicit `timeout`
- [ ] `schema` on the wrapper so the report comes back structured
- [ ] `isolation: 'worktree'` if the stage writes files in parallel
- [ ] Codex-heavy loops bounded by count/rounds (budget only meters the Claude
      wrappers — keep it as a backstop, not the primary limit)
- [ ] Effort set per stage — wrapper stays `effort: 'low'`; codex reasoning
      effort set on the `codex exec` invocation; high tiers reserved for
      verify/judge; no blanket max across a fan-out
- [ ] Taste-sensitive output (UI, copy, public API shape) reviewed by fable/opus
      before ship

---

## 4. Prompt-hygiene rules this video reinforces

1. **Hand-write global prompts, skills, and standing instructions.** No
   agent-generated system prompts. Adjust incrementally as you learn.
2. **Minimal and general beats prescriptive.** Style constitutions, fixed pixel
   values, mandated libraries, and cadence rules ("update every 30s") become
   compulsions in strong instruction-followers. State goals and constraints the
   code can't express; let the model bring capability.
3. **Behavioral bugs are prompt bugs first.** A model that always does some weird
   specific thing was probably told to. Read the operative prompt/skill before
   working around the behavior — same class as our fix-the-generator rule.
4. **Keep the model vocabulary current.** Our CLAUDE.md model table is the
   steering surface that makes "use a workflow with codex doing the heavy lifting"
   work from a one-line ask. When the fleet changes (new codex model, new Claude
   tier), update the table promptly — stale vocabulary is steering rot.
   (Recommendation; the binding rules live in `~/.claude/CLAUDE.md` itself.)

---

*Distilled 2026-07-17 from yt-dlp auto-caption transcripts (VTT cleaned by a Mix
script). The companion 07-14 video (Ultra/ultracode semantics, Codex V1/V2
internals, workflow anatomy, effort economics) folded in the same day.*

*Update (2026-07-17, later same day, Mark-directed): this doc's references to
"gpt‑5.5" as our codex model were already stale when written — `~/.codex/config.toml`
runs `gpt-5.6-sol` at high reasoning. Operative rule now: both engines ride the
**latest and best available model** (role names "frontier Claude" / "codex" in
docs; wrapper labels `codex:<stage>` not `gpt-5.5:<stage>`), and the
review→fix→re-review **convergence loop is mandatory** for every substantive
change. The operative method file is `_doc/2026-07-17-ultracode-startup.md`;
this doc stays as the source distillation.*
