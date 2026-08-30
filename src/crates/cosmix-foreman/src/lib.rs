//! cosmix-foreman — the Cosmix build-orchestration harness (Phase 0).
//!
//! Foreman hires, briefs, supervises, and accounts for three coding agents —
//! Claude Code, Codex CLI, and GLM (via Z.ai's Anthropic-compatible endpoint)
//! — through one `Executor` trait with three subprocess drivers. The ledger
//! (SQLite, WAL) records tasks, atomic claims, per-session runs with token
//! usage and cost, findings, and the full normalized event stream.
//!
//! Design: `~/.cmctl/_plan/2026-08-17-cosmix-harness-plan.md`. This crate is
//! the Phase-0 slice — one task, one worktree, one driver, budget caps,
//! everything accounted. The MCP server (rmcp), refinery, policy gate, and
//! governor build on these types in Phase 1+.

/// Shared fixture writer for unit tests; integration tests include the same
/// file verbatim via tests/support/mod.rs. See `src/fixture.rs`.
#[cfg(test)]
mod fixture;
#[cfg(test)]
mod fixture_tests;

pub mod agent_sessions;
pub mod attachment_harm;
pub mod clock;
pub mod clone_lock;
pub mod config;
pub mod driver;
pub mod executor;
pub mod gc;
pub mod governor;
pub mod ladder;
pub mod ledger;
pub mod lowering;
pub mod manifest;
pub mod mcp;
pub mod policy;
pub mod procutil;
pub mod provenance;
pub mod refinery;
pub mod remote_git;
pub mod remote_job;
pub mod replay;
pub mod review;
pub mod runner;
pub mod sandbox;
pub mod scratch;
pub mod state;
pub mod target_dir;
pub mod unit_health;
pub mod verify;
pub mod wake;
