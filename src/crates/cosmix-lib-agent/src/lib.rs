//! Agent loop + session management + tool registry for cosmix.
//!
//! Drives multi-turn conversations against an `LlmClient`, dispatching any
//! tool calls the model requests through a pluggable `ToolRegistry`. Sessions
//! persist their message history as JSONL under `$COSMIX_VAR/agent/sessions/`.

pub mod runner;
pub mod session;
pub mod tools;

pub use runner::{TurnOutcome, run_turn};
pub use session::{Session, SessionId, sessions_dir};
pub use tools::{BuiltinTools, Tool, ToolRegistry};

/// Hard ceiling on tool-use iterations within a single user message.
/// Protects against runaway loops if the model gets stuck re-calling tools.
pub const MAX_ITERATIONS: u32 = 50;

/// Default `max_tokens` per LLM call in the agent loop.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
