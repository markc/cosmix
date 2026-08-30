//! `cosmix-maild-rules` — deterministic rule engine, second stage of
//! cosmix-maild's inbound DATA filter pipeline.
//!
//! Runs **after** `cosmix-maild-auth`'s RFC verification and **before**
//! `cosmix-maild-bayesian`'s statistical classification. Produces a
//! `RuleVerdict` with three shapes — `HardAccept` (skip Bayesian,
//! deliver), `HardJunk` (skip Bayesian, junk), `Continue` (pass to
//! Bayesian with `score` and `matched_rules` as contextual features).
//!
//! See `_doc/2026-04-30-cosmix-maild-rules-doc.md`,
//! `_doc/2026-04-30-cosmix-maild-rules-spec.md` (frozen public API),
//! and `_doc/2026-04-30-cosmix-maild-rules-plan.md` (8-phase plan,
//! 3.5-day budget).
//!
//! Phase 2 commit 2b status: 13 active rule kinds — `peer_ip_in_dnsbl`
//! is now live behind an `Option<Arc<dyn DnsblLookup>>` on the engine
//! and an async preflight pass (see `crate::preflight`); `alignment`
//! remains a reserved no-match stub. v1.0 default pack, verdict shapes
//! (allowlist > blocklist > mail-auth-hard-fail > structural anomaly >
//! score breach > continue), shadow-mode downgrade, MIME views, sender
//! globs, and per-account overrides are wired. Hot reload + Bus
//! integration are partially wired (reload exists; per-rule stats Bus
//! verbs are commit 4 / commit 5 work).

mod compiled;
pub mod config;
pub mod dnsbl;
pub mod engine;
pub mod error;
pub mod glob_match;
pub mod loader;
mod mime;
pub mod preflight;
pub mod rules;
pub mod types;

pub use crate::config::EngineConfig;
pub use crate::engine::{DefaultRuleEngine, RuleEngine, RuleMatchHook};
pub use crate::error::{Error, Result};
pub use crate::types::*;

/// Embedded v1.0 default rule pack. Callers that want the shipping
/// pack without locating it on disk (the umbrella daemon, integration
/// tests) can pass this directly to `DefaultRuleEngine::with_pack_str`.
pub fn default_pack_str() -> &'static str {
    include_str!("../rules/default.conf.mix")
}
