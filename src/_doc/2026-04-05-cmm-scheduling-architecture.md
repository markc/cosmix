# CMM — Cosmix Memory Management Scheduler

Autonomous background maintenance and self-improvement layer for the cosmix knowledge base.

## Motivation

The inner loop of knowledge self-improvement was built on 2026-04-05 as part of the OpenClaw memory-stack absorption:
- retrieval tracking, feedback scoring, implicit negative signals
- skill supersession (history preserved), graduation to CLAUDE.md
- trust hierarchy (doc > graduated > skill > journal > memory)

That inner loop is **reactive** — it responds to signals pushed in by humans and agents. What's missing is the **autonomic** outer loop that observes the knowledge base's own behaviour, records what it observes, and tunes itself over time.

## Core loop

```
observe → record to _memory/ → index → retrieve → act → observe
```

Every CMM task writes a markdown summary to `_memory/`. Those summaries are indexed like any other source type, meaning the system's observations about itself are retrievable via `context_search`. Over time, the system accumulates a recursive history of its own hygiene.

## `_memory/` as a 4th source type

| Dir | Source | Trust | Who writes | Regenerable? |
|---|---|---|---|---|
| `_doc/` | `doc` | highest | humans (design truth) | no |
| `_journal/` | `journal` | medium | humans (session notes) | no |
| `_memory/` | `memory` | low | **CMM only** (observations, reports, proposals) | **yes** — rebuild from SQLite + logs |

`_memory/` chunks carry `source = "memory"` in indexd. Trust weight defaults to `0.01` (slightly above journal baseline, below skills). Required metadata: `path`, `domain`, `date`, `generator` (which CMM task produced it), `tier` (which scheduling tier triggered it).

`.gitignore` rule: `_memory/` should generally NOT be committed (it's a derived cache). Exception: the user may opt to commit `_memory/` for historical review.

## Tier ladder

| Tier | Cadence | Task kinds |
|---|---|---|
| **1m** | liveness | health pings only — indexd/hub/claud alive? restart if dead |
| **5m** | incremental | autoindex recently-modified `_doc/`+`_journal/`; embed cache stats |
| **15m** | lightweight maintenance | retrieval heat map; log rotation; circuit-breaker drift |
| **30m** | analytic | skill graduation sweep; confidence recalibration |
| **60m** | housekeeping + digest | dedup near-duplicates; orphan blob scan; hourly retrieval digest |
| **1440m** | heavy + meta | VACUUM; WAL checkpoint; supersession compaction; CLAUDE.md audit; **LLM self-analysis pass** |
| **10080m (weekly)** | evolution | semantic cluster analysis; skill merge proposals; CLAUDE.md promotion proposals |

Tiers share a single clock — every tier is a multiple of 1m so no drift between them.

## Job catalog

Each job is an AMP command (called via `wsamp`) that produces:
1. A structured response (logged)
2. A markdown report written to `_memory/YYYY-MM-DD-<job-name>.md`

### Bootstrap jobs (Phase 1)

| Job | Tier | What it does |
|---|---|---|
| `cmm.graduation_sweep` | 30m | List all skills via `indexd.list`, check each against graduation thresholds, auto-graduate matches |
| `cmm.staleness_report` | 60m | Enumerate chunks with retrieval_count>3 & feedback_score≤0; chunks never retrieved & >90d old; write report |

### Phase 2+3 jobs

| Job | Tier | What it does |
|---|---|---|
| `cmm.health_ping` | 1m | Call `hub_ping`; restart dead services via noded |
| `cmm.autoindex` | 5m | Walk `_doc/`+`_journal/`, re-index files modified since last tick |
| `cmm.heat_map` | 15m | Aggregate retrieval_count vs feedback_score distribution; flag anomalies |
| `cmm.confidence_recalibrate` | 30m | Compare stored confidence to actual success_count/use_count; nudge skills with drift >0.2 |
| `cmm.dedup_sweep` | 60m | Find chunk pairs with cosine similarity >0.97; propose merges |
| `cmm.orphan_blobs` | 60m | Find blob files with no chunk reference |
| `cmm.vacuum` | 1440m | SQLite VACUUM + integrity_check + WAL checkpoint |
| `cmm.supersession_compact` | 1440m | In chains >5 versions, keep first + latest + branch points |
| `cmm.claude_md_audit` | 1440m | Verify graduated-skill rules in CLAUDE.md still match indexd state |
| `cmm.self_analysis` | 1440m | **LLM pass** — read last 24h of `_memory/` + retrieval logs; emit proposals |
| `cmm.cluster_analysis` | 10080m | Semantic clustering of skills; propose merges |
| `cmm.promotion_proposals` | 10080m | Identify skills worth CLAUDE.md promotion (beyond auto-graduation) |

## Scheduling phases

### Phase 1 — Bootstrap via cron + bash

Simplest possible start:
- A single bash dispatcher at `src/scripts/cmm-tick.sh`
- Cron entries on 5 tiers (skip 1m — nothing needs it yet)
- Jobs call existing AMP commands via `wsamp`; no new Rust
- Output written to `_memory/YYYY-MM-DD-<job>.md`

Prerequisites:
- `_memory/` accepted as a source type by indexd (small change to `validate_store_entry`)
- `cosmix-mcp`'s `find_content_dirs` walks `_memory/` alongside `_doc/`+`_journal/`
- `cosmix-mcp`'s `context_search` applies trust weight to `memory` results

Graduate to Phase 2 when: cron drift becomes annoying, or when observability beyond `tail _memory/` is needed.

### Phase 2 — Noded tick emitter

Add a `schedd` subsystem to `cosmix-noded`:
- One tokio timer wheel, single clock
- Emits `tick.1m`, `tick.5m`, …, `tick.10080m` events onto AMP
- Scripts subscribe via Mix `address tick.5m` (native Mix syntax)
- Ticks visible in AMP log; zero drift

Benefits: lifecycle managed by noded; mesh peers can see each other's ticks.

Graduate to Phase 3 when: >5 tasks active, or when proposals from `cmm.self_analysis` become a regular occurrence.

### Phase 3 — Registry + proposals gateway

- AMP commands: `cmm.register`, `cmm.list_tasks`, `cmm.run_now <name>`, `cmm.disable <name>`, `cmm.history <name>`
- Tasks self-register at startup
- Proposals from `cmm.self_analysis` are routed into 3 buckets:
  - **Auto-apply**: parameter tuning within ±20% of current value (logged to `_memory/`)
  - **Ask-once**: structural changes (new skill extraction, chunk pruning) — surfaced next time user talks to the agent
  - **Human-review**: CLAUDE.md edits, graduation threshold changes — require explicit approval
- Every proposal + decision + outcome → `_memory/` → indexed → recallable

This closes the loop: CMM observes → proposes → applies → observes the effect of its application.

## Self-improvement closed loop

The daily `cmm.self_analysis` job is where emergent behaviour lives:

1. **Collect**: pull last 24h from `_memory/` + retrieval_count/feedback_score distributions + graduation events
2. **Analyze**: feed to claud (Haiku): *"Here's 24h of operational data. What's miscalibrated? What patterns are emerging?"*
3. **Propose**: LLM emits structured JSON proposals:
   ```json
   {"kind": "tune_config", "key": "knowledge.trust_weight_journal", "from": 0.0, "to": 0.02, "reason": "..."}
   {"kind": "extract_skill", "cluster_ids": [...], "reason": "5 journals mention same workflow"}
   {"kind": "prune_chunks", "ids": [...], "reason": "never retrieved, >180d old"}
   ```
4. **Gate**: route through the 3 buckets above
5. **Record**: every proposal + decision + outcome → `_memory/`

Over weeks, this converges on calibrated parameters without constant human tuning.

## First concrete step (Phase 1 MVP)

1. Accept `_memory/` as a source type in indexd + MCP (small Rust change)
2. Create `src/scripts/cmm-tick.sh` dispatcher
3. Implement two bootstrap jobs: `cmm.graduation_sweep` (30m) and `cmm.staleness_report` (60m)
4. Install `cmm-tick` as a symlink in `~/.local/bin/`
5. Add 2 cron entries
6. Wait a day, inspect `_memory/`

If bootstrap proves valuable → expand job catalog, eventually migrate to Phase 2.

## Open questions

- **Should `_memory/` be per-domain or global?** Proposal: per-domain, like `_doc/` and `_journal/` already are.
- **Trust weight for auto-journals?** Starting at 0.01 (just above journal baseline). Tune based on observation.
- **How does this interact with Claude Code's memory system?** They're separate — Claude Code memory is in `~/.claude/projects/.../memory/`, CMM memory is in the workspace. No overlap.
- **Mesh awareness?** Defer. Phase 1 is single-node. Phase 2/3 can address cross-node scheduling.
- **Do we need a 1-minute tier at all?** Probably not. Skip until a real 1m use case appears.
