# OpenClaw Analysis: What Cosmix Should Steal

*2026-03-14 — Distilled from OpenClaw deep dive research brief*

## Context

OpenClaw is a Node.js AI agent framework that hit 250k GitHub stars in 60 days. Austrian developer Peter Steinberger built it as a persistent daemon that connects LLMs to local tools and messaging apps. Several Rust alternatives emerged (ZeroClaw, IronClaw, Moltis) addressing its Node.js bloat and security gaps.

## Already Have

| OpenClaw Pattern | Cosmix Equivalent | Status |
|---|---|---|
| Gateway daemon | cosmix-daemon | Done |
| ReAct agent loop | `agent/loop_engine.rs` | Done |
| Tool approval/sandboxing | `safety/` + `approval.rs` (3-tier) | Done |
| Memory with hybrid search | `memory/` (pgvector + FTS + RRF) | Done |
| Skills/extensions | `extensions/` (TOML manifest + Lua) | Done |
| MCP integration | `mcp/` (JSON-RPC stdio + HTTP) | Done |
| Cron/heartbeat | `agent/scheduler.rs` + `routines/` | Done |
| Local LLM (round-robin) | Ollama across 3 nodes | Done |

## Three Things Worth Stealing

### 1. Pre-Compaction Memory Flush

Before context compaction, OpenClaw runs a silent agentic turn asking the model to write durable notes. This prevents information loss when long sessions get compacted.

**Implementation:** Before compacting, prompt the model with "write anything important from this session to memory before I forget it." Single function addition to `compaction.rs`.

### 2. Lazy Skill Loading

OpenClaw only injects skill *metadata* (name + one-line description) into the system prompt. The model reads full SKILL.md on demand when it decides a skill is relevant. This keeps the base prompt lean regardless of how many skills are installed.

**Implementation:** Extension registry sends only name + description to context assembly. Add a `read_skill` tool that loads the full Lua script + manifest on demand. Three-tier precedence: workspace > user-global > system-bundled.

### 3. Secrets Isolation (IronClaw Pattern)

Secrets never touch LLM context. Model reasons with placeholder references like `SecretRef("db_password")`, which resolve only at the execution boundary after model reasoning is complete.

**Implementation:** `SecretRef` type in tool argument schema. Safety layer resolves refs to real values only during tool execution, after the model has finished its reasoning turn.

## What to Skip

- **ClawHub marketplace** — security nightmare. Signed local extensions are better.
- **WASM sandboxing** — premature. Lua sandbox + safety policy is sufficient until untrusted third-party extensions exist.
- **Knowledge graph overlay** — pgvector + RRF hybrid search is already better than OpenClaw's SQLite-vec. Add graph later only if search quality actually degrades.

## Architectural Validation

The ARexx port model + AMP three-reader format + Rust+Lua stack is architecturally superior to OpenClaw's TypeScript. The 394MB Node.js idle footprint vs a single Rust binary tells the story. OpenClaw proved the UX demand; Cosmix has the better architecture.
