//! Public types — frozen surface per spec §Public API.

use cosmix_maild_rules::{AccountId, RuleId};

// ---- Classify-time context ----

/// Inputs the classifier needs for one classification call.
pub struct ClassifyContext<'a> {
    pub message: &'a [u8],
    pub account: &'a AccountId,
    /// Rule engine output, propagated so the classifier can apply
    /// `rules_score_bias_k` log-odds adjustment.
    pub rules_score: f32,
    /// Rules that matched in the previous stage; preserved on the
    /// classification stamp regardless of Bayesian outcome.
    pub matched_rules: &'a [RuleId],
    /// Used to bypass token caps for trusted senders. v1: always
    /// false; reserved for future allowlist coupling.
    pub trusted: bool,
}

// ---- Verdict ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Label {
    Spam,
    Ham,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BayesianVerdict {
    pub label: Label,
    /// Robinson-Fisher score in [0, 1]. Higher = more spam-like.
    pub score: f32,
    /// Effective threshold actually applied (post cold-start
    /// adjustment if any).
    pub threshold: f32,
    /// True when the per-account corpus was below the cold-start
    /// floor (N < 100 spam+ham combined).
    pub cold_start: bool,
    /// Top contributing tokens, descending by |contribution|. Capped
    /// per `ClassifierConfig.explanation_top_k`. v1 default 15.
    pub contributions: Vec<TokenContribution>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenContribution {
    pub token: String,
    pub spam_count: u32,
    pub ham_count: u32,
    pub probability: f32,
    pub contribution: f32,
}

// ---- Retrain surface ----

#[derive(Debug, Clone)]
pub struct RetrainRequest<'a> {
    /// Stable identifier (e.g. classification stamp UUID) so duplicate
    /// labels are idempotent.
    pub stamp_id: &'a str,
    pub account: &'a AccountId,
    pub message: &'a [u8],
    pub label: Label,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrainOutcome {
    /// Tokens were added to the per-account corpus.
    Applied,
    /// No prior classification stamp matched; ignored.
    NoStamp,
    /// Already labeled with the same label; idempotent no-op.
    AlreadyLabeled,
}

// ---- Stats ----

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AccountStats {
    pub spam_messages: u32,
    pub ham_messages: u32,
    pub spam_tokens: u64,
    pub ham_tokens: u64,
    /// True until `spam_messages + ham_messages >= 100`.
    pub cold_start: bool,
    /// Path or label of the seed corpus; `None` once the account has
    /// trained past the seeded baseline.
    pub seeded_from: Option<String>,
    pub model_version: u32,
}
