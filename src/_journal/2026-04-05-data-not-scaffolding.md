# 2026-04-05 — data, not scaffolding: letting CMM earn its next phase

## Context

Hours after shipping CMM Phase 1 (live systemd timers, `_memory/` accumulating), the natural instinct was to start Phase 2. Resisting that. Writing down why, and what to do instead.

## Principle

Phase 2 and Phase 3 are not calendar-driven — they are trigger-driven. Each phase should earn its complexity from observed need. Right now CMM has 2 jobs and <1 day of history. The need is data, not more scaffolding. Scaffolding without load teaches nothing.

## Triggers, made explicit

**Phase 2 (noded tick emitter + Mix handlers):** start when
- writing a CMM job in bash feels wrong because you're already in Mix for something else, OR
- mesh peers need visibility into each other's ticks

**Phase 3 (task registry + LLM self-analysis):** start when ALL of
- ≥5 active CMM jobs (currently 2),
- ≥1 week of `_memory/` accumulation,
- real skill refinements happening (supersession chains exist, graduations have occurred naturally).

## How to accumulate data well

1. **Use `context_search` during real work.** It's the only way `retrieval_count` and `feedback_score` acquire signal. Report feedback via `docs_feedback` when chunks help or mislead.

2. **Refine skills when used.** `skills_refine` with accurate success+notes. Without refinement traffic, no skill ever graduates — and "nothing graduates" becomes noise not data.

3. **Don't manually trigger CMM jobs to 'check'.** The cadence *is* the experiment. Re-running `cmm-tick@30m.service` at 3:05 to see what changed corrupts the observational baseline.

4. **Don't edit `_memory/` files.** They're regenerable by definition. Manual edits turn observations into fabrications.

5. **Resist premature tuning.** `trust_weight_doc=0.08`, `feedback_score * 0.05`, `staleness 90d`, `graduation confidence=0.9` — all guesses. Give them 2+ weeks before touching. The first misfires are the most informative.

6. **Capture friction in real journals.** When you wish CMM had flagged something, write it as a normal `_journal/` entry. That's where Phase 1.5 job ideas should come from — observed lack, not speculation.

## Phase 1.5 — cheap expansion within bash+systemd

Before jumping to Phase 2 (noded+Mix), fill out the tiers with more bash jobs. Same architecture, just more signal.

| Tier | Candidate job | Closes gap |
|---|---|---|
| 5m | `autoindex-recent` — walk content dirs for mtime > last_tick, re-index via MCP | stale index after edits |
| 15m | `retrieval-heatmap` — top-10 retrieved chunks, correlate with feedback | pattern visibility |
| 60m | `hourly-digest` — summarize last hour of `_memory/` as one line | LLM training substrate |
| 1440m | `vacuum-and-checkpoint` — `PRAGMA wal_checkpoint; VACUUM` via indexd | routine DB hygiene |

Each ~30-50 lines of bash. Add them when friction demands, not preemptively.

## Success signals (2 weeks out)

- `_memory/` has ≥50 entries across tiers
- at least one skill has naturally graduated (not forced)
- at least one chunk has non-zero feedback_score
- staleness report shows real candidates you'd genuinely not miss

When those conditions hold, Phase 2 and Phase 3 have real material to chew on. Until then, patience is the highest-leverage operation.
