# ZeroClaw Analysis: What Cosmix Should Steal

*2026-03-14 — Deep dive into ZeroClaw codebase (168k lines Rust, 228 files)*

## Context

ZeroClaw is a Rust rewrite of OpenClaw — single binary, <5MB RAM, SQLite + vector search. It's a multi-channel chatbot framework (WhatsApp, Telegram, Discord, etc.) with agent capabilities. Different problem space from Cosmix, but several production-grade patterns worth adopting.

## What Cosmix Already Does Better

| Area | ZeroClaw | Cosmix | Winner |
|------|----------|--------|--------|
| Vector search | SQLite + BLOBs, manual cosine sim | PostgreSQL + pgvector, native indexing | Cosmix |
| Memory search | Hybrid BM25 + vector (SQLite FTS5) | Hybrid FTS + vector + RRF fusion | Cosmix |
| IPC/orchestration | JSON WebSocket, no structured protocol | AMP wire format, three-reader principle | Cosmix |
| Tool sandboxing | Docker/Firejail/Bubblewrap (heavy) | Lua sandbox + safety policy (lightweight) | Cosmix |
| Mesh networking | None (single-node) | WebSocket mesh over WireGuard, 3 nodes | Cosmix |
| Desktop integration | None | AT-SPI2, D-Bus, Wayland, cosmix-port | Cosmix |
| Scripting | None (hardcoded tools) | Lua hot-reload, ARexx-style ports | Cosmix |

## Tier 1: Implemented

### 1. LLM Response Cache

Cache LLM responses by SHA-256 of (model + system_prompt + user_prompt) with TTL + LRU eviction. Saves 10+ seconds on repeated queries with slow local models.

### 2. Query Classification for Model Routing

Route queries to different models based on keyword/length rules. Short code questions → fast small model, reasoning tasks → larger model. Replaces load-based round-robin with intelligence-based routing.

### 3. Cost/Token Tracking with Budget Enforcement

Per-model token usage with daily/monthly budgets and pre-flight budget checks. Prevents runaway agent loops from burning API quota.

### 4. Credential Leak Detection

Regex patterns (Stripe keys, AWS creds, PEM blocks, JWTs) + Shannon entropy analysis for unknown token formats. Catches leaked credentials in LLM-generated content.

### 5. Prompt Injection Guard

Detects six attack categories: system override, role confusion, tool injection, secret extraction, command injection, jailbreak. Post-inference check before tool execution.

## Tier 2: Deferred

### SOP Workflow Engine

Event-driven workflows with multi-source triggers, approval gates with timeout escalation, concurrency limits, cooldown throttling. Wait until Phase 8 agent work needs it.

### Encrypted Secret Store

ChaCha20-Poly1305 encrypted secrets, never in LLM context. Cosmix needs this but it's a separate effort (SecretRef pattern from OpenClaw analysis).

## What to Avoid

- 18 channel integrations (WhatsApp/Telegram/Discord) — AMP mesh is the right abstraction
- 15 LLM provider abstractions — Ollama + Claude fallback is sufficient
- SkillForge marketplace — supply chain attack surface
- 9,295-line config schema — over-engineered
- Hardware peripherals (STM32/Arduino) — not Cosmix's niche
- 168k lines total — Cosmix should stay lean
