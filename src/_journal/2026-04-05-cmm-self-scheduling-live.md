# 2026-04-05 — CMM goes autonomic: knowledge base now maintains itself

## Inflection point

The cosmix knowledge base crossed from **reactive** to **autonomic** today. Before: humans and agents pushed signals in (`docs_feedback`, `skills_refine`) and the index responded. After: the index observes itself, writes what it observes, and those observations re-enter the retrieval loop. The feedback horizon is now infinite.

## Context

Started the day absorbing patterns from a received "OpenClaw Memory Stack" giveaway — a layered memory architecture for AI assistants. The patterns were architecturally instructive even though the tooling was different. Six of them mapped cleanly onto cosmix's existing indexd + skills infrastructure:

1. Trust hierarchy — not all sources are equal
2. Temporal decay — journal entries shouldn't outrank fresh docs forever
3. Retrieval tracking — implicit negative signal from unliked retrievals
4. Staleness detection — prune what's not earning its place
5. Layer separation — enforce per-source metadata contracts
6. Supersede, don't mutate — preserve skill history

All six shipped in one afternoon, across 3 batches, all building cleanly. Then the conversation turned to the question: how does this *run itself*?

## What shipped

### Inner loop — OpenClaw pattern absorption (3 batches, all green)

**Batch 1 — trust + decay + validation** (`cosmix-mcp/src/main.rs`, `cosmix-lib-config/src/settings.rs`, `cosmix-indexd/src/main.rs`)
New `KnowledgeSettings` config section with 6 knobs. `context_search` applies trust weights per source (doc=0.08, graduated-skill=0.06, skill=0.03, journal=0.0) as distance bonuses, then re-sorts each source array. Journals additionally penalised by age (0.02/month) and filtered past 180 days. `handle_store` now validates required metadata per source type — unknown sources and missing fields rejected with descriptive errors.

**Batch 2 — retrieval tracking + staleness** (`cosmix-indexd/src/main.rs`)
Schema migration added `retrieval_count` + `last_retrieved` to chunks. `handle_search` fires a best-effort `mark_retrieved()` UPDATE on every returned ID. The ranking function now combines: `distance - (feedback_score * 0.05) + implicit_penalty + staleness_penalty`. Implicit penalty kicks in for chunks retrieved >3× without upvotes. Staleness penalty for chunks >90d old never retrieved, or >180d since last retrieval. No external date crate — wrote minimal Hinnant-style date math in ~20 lines.

**Batch 3 — supersede, don't mutate** (`cosmix-lib-skills/src/{types,indexd_client,loop_fns}.rs`, `cosmix-mcp/src/main.rs`)
`SkillDocument` gained `superseded_by: Option<i64>`. `refine_skill` now returns `(new_id, SkillDocument)` instead of `SkillDocument` — creates a new chunk, marks the old one as superseded, preserves history. Search filters exclude superseded entries automatically. Old versions remain queryable via `list_skills` for rollback or audit.

### Outer loop — CMM (Cosmix Memory Management) autonomous scheduler

**Design doc**: `src/_doc/2026-04-05-cmm-scheduling-architecture.md` — the core loop (observe → record to `_memory/` → index → retrieve → act → observe), the tier ladder (5m/15m/30m/60m/1440m), Phase 1/2/3 evolution plan, self-improvement closed loop gated by auto-apply/ask-once/human-review buckets.

**`_memory/` as a 4th source type**: distinct from `_doc/` (human design truth) and `_journal/` (human session notes). Machine-generated only, regenerable from SQLite + logs, lower trust weight (0.01). Required metadata: path + domain + date + generator + tier. Indexd validates it, MCP `index_workspace` walks it, `context_search` returns a `memory: [...]` array with same temporal decay as journals.

**New `stale` indexd action**: returns 3 buckets (never_retrieved_old, low_value, long_dormant) with configurable thresholds. Used by CMM's staleness-report job.

**New CLI subcommands**: `cosmix-skills-cli graduate-all` (sweeps all skills, graduates eligibles, emits markdown) and `cosmix-skills-cli staleness-report` (queries stale buckets, emits markdown). Both idempotent, both output-only.

**Script**: `src/scripts/cmm-tick.sh` — tier-dispatched bash with a `write_memory` helper that wraps job output with frontmatter (generator, tier, date) and drops to `_memory/YYYY-MM-DD-<job>.md`.

**Systemd user services** — the user has systemd, not cron:
- `cosmix-indexd.service` — keeps indexd running, auto-restarts on failure
- `cmm-tick@.service` — templated service taking tier as instance name
- 5 timers: `cmm-tick-{5m,15m,30m,60m,1440m}.timer` with staggered OnBootSec to avoid startup stampede
- `/etc/tmpfiles.d/cosmix.conf` — persists `/run/cosmix/` across reboots

## Pre-existing bug fixed along the way

The initial `execute_batch` for indexd's schema tried to create a unique index on `content_hash` before the migration that adds the column. On DBs that predated the `content_hash` column (i.e. any real deployment), indexd failed to start with "no such column: content_hash". Split the INDEX creation out to run AFTER migrations. This unblocked indexd startup on the live DB with 602 existing chunks.

## Verification

All 5 timers listed in `systemctl --user list-timers`. Manually triggered `cmm-tick@30m.service` through systemd → produced a real graduation report identifying 2 skills below threshold with explicit gaps (`conf+0.40, uses+4, successes+3`). The 60m tier produced a real staleness report across all 3 buckets (all empty — DB is only 2 days old).

```
$ systemctl --user list-timers 'cmm-tick*'
NEXT                             LEFT     LAST                         PASSED  UNIT
Sun 2026-04-05 07:07:23 AEST     4min 56s Sun 2026-04-05 07:02:23 AEST 3s ago  cmm-tick-5m.timer
Sun 2026-04-05 07:17:23 AEST     14min    Sun 2026-04-05 07:02:23 AEST 3s ago  cmm-tick-15m.timer
Sun 2026-04-05 07:32:23 AEST     29min    Sun 2026-04-05 07:02:23 AEST 3s ago  cmm-tick-30m.timer
Sun 2026-04-05 08:00:00 AEST     57min    Sun 2026-04-05 07:02:23 AEST 3s ago  cmm-tick-60m.timer
Mon 2026-04-06 03:00:00 AEST     19h      -                                 -  cmm-tick-1440m.timer
```

## Why this is an inflection point

Three properties crossed thresholds simultaneously:

1. **The loop closes without human input.** Previously every feedback signal required a Claude invocation. Now the system generates signals about itself at 5 cadences without anyone being present.

2. **Observations are first-class.** `_memory/` chunks are indexed alongside human-authored content. Next time someone asks "what's the state of the knowledge base?" the answer surfaces from the index itself — not computed, retrieved.

3. **History is preserved, not overwritten.** The supersede-don't-delete pattern means skill evolution is visible. Combined with `_memory/` dailies, the system accumulates a recursive operational history.

Phase 2 (noded-emitted ticks + Mix handlers) and Phase 3 (task registry + LLM proposal gateway) are documented and queued. The 5m/15m tiers are wired but empty — placeholders for the next expansion.

## Files touched

- `src/crates/cosmix-lib-config/src/settings.rs` — `KnowledgeSettings` struct + defaults
- `src/crates/cosmix-indexd/src/main.rs` — 6 OpenClaw patterns, `stale` action, `_memory/` validation, migration fix
- `src/crates/cosmix-mcp/src/main.rs` — trust weighting, journal decay, memory source, `_memory/` walking
- `src/crates/cosmix-lib-skills/src/types.rs` — `superseded_by` field
- `src/crates/cosmix-lib-skills/src/indexd_client.rs` — `supersede_skill`, `stale`, response types
- `src/crates/cosmix-lib-skills/src/loop_fns.rs` — refine returns `(new_id, doc)`
- `src/crates/cosmix-lib-skills/src/cli.rs` — `graduate-all`, `staleness-report` subcommands
- `src/_doc/2026-04-05-cmm-scheduling-architecture.md` — new design doc
- `src/scripts/cmm-tick.sh` — new CMM dispatcher
- `src/_memory/.gitkeep` — new directory
- `~/.config/systemd/user/cosmix-indexd.service` + 5 timers + templated service
- `/etc/tmpfiles.d/cosmix.conf` (sudo)

## Deleted

- `src/crates/cosmix-maild/migrations/` — orphaned PostgreSQL migration files (pre-SQLite pivot)
