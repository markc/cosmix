//! Classifier configuration. Frozen surface per spec §ClassifierConfig.

#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Robinson-Fisher score above this → `Label::Spam`. Default 0.95.
    pub spam_threshold: f32,
    /// Lenient threshold applied while in cold-start mode. Default 0.85.
    pub cold_start_spam_threshold: f32,
    /// Minimum spam+ham messages before exiting cold-start. Default 100.
    pub cold_start_floor: u32,
    /// Tokens with extreme probabilities (>= this many) are weighted
    /// down per the Robinson smoothing constant. Default 15.
    pub robinson_max_extreme_tokens: u32,
    /// Smoothing constant `s` from Gary Robinson 2003. Default 0.5.
    pub smoothing_s: f32,
    /// `x` constant: prior probability of an unknown token being
    /// spam. Default 0.5 (no prior).
    pub smoothing_x: f32,
    /// Top-k tokens surfaced on `BayesianVerdict.contributions`.
    /// Default 15.
    pub explanation_top_k: u32,
    /// Post-tokenisation cap applied to classification and retraining. Default
    /// 50_000; 0 = unlimited. Header, flag, and other non-body/non-URL tokens
    /// are retained first, then `b:*` body tokens, then `u:*` URL tokens. This
    /// bounds URL-heavy messages without repeating the old default-200 defect,
    /// which discarded sender/header evidence before body evidence.
    pub max_tokens_per_message: u32,
    /// Body bytes scanned. Default 1_048_576 (1 MiB). Reserved, currently
    /// unused by the bayesian stage.
    pub body_scan_bytes: usize,
    /// Log-odds adjustment applied per unit of `rules_score`. v1
    /// default 0.0 (off); tuned against the mrn corpus in Phase 8.
    pub rules_score_bias_k: f32,
    /// Wall-clock budget per `classify` call. Default 25. Reserved, currently
    /// unused by the bayesian stage.
    pub budget_ms: u32,
    /// Whether to seed empty per-account databases from
    /// `default-bayesian.db`. Default true.
    pub seed_from_default: bool,

    // ---- Empirical-Bayes base-rate prior (EXPERIMENTAL, default OFF) ----
    //
    // `smoothing_x` is the Robinson centre: the probability an UNKNOWN token is
    // assigned, and the value thinly-observed tokens are pulled toward. It is
    // hardcoded at 0.5 — "an unseen word is a coin flip".
    //
    // But the corpus can answer that question. `raw_p` is already base-rate
    // normalised (`spam/total_spam` over `good/total_good`), so the prior enters
    // the score ONLY through this centre — which means 0.5 is a *claim*, and one
    // the data may contradict: a mailbox running 80% spam should not treat an
    // unseen word the same as one running 5%.
    //
    // When enabled, the centre becomes the observed spam base rate
    // (`spam_msgs / (spam_msgs + ham_msgs)`), shrunk toward 0.5 by
    // `base_rate_pseudocount` and clamped to `[base_rate_min, base_rate_max]`.
    //
    // Off by default and shadow-evaluated on mrn before it goes anywhere near a
    // real mailbox. See `~/.cmctl/_doc/2026-07-14-bayes-base-rate-prior.md` for the
    // rationale, the failure mode it is meant to attack (thin-corpus false
    // positives), and the failure mode it could CAUSE (a spam-heavy mailbox
    // scoring novel ham vocabulary as spam).
    /// Use the corpus's observed spam base rate as the Robinson centre instead
    /// of the fixed `smoothing_x`. Default **false** (v0.4.1 behaviour).
    pub base_rate_prior: bool,
    /// Shrinkage toward 0.5, in pseudo-messages (a Beta(k/2, k/2) prior).
    /// Without it, a corpus of 2 spam and 0 ham would set the centre to 1.0 and
    /// every unknown token would scream spam. Default 20.
    pub base_rate_pseudocount: f32,
    /// Lower clamp on the derived centre. Default 0.2.
    pub base_rate_min: f32,
    /// Upper clamp on the derived centre. A heavily-spammed mailbox
    /// (5000 spam / 50 ham) has a base rate of ~0.99; letting the centre go
    /// there would make every unseen word near-conclusive evidence of spam and
    /// bury ham with novel vocabulary. Default 0.8.
    pub base_rate_max: f32,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            spam_threshold: 0.95,
            cold_start_spam_threshold: 0.85,
            cold_start_floor: 100,
            robinson_max_extreme_tokens: 15,
            smoothing_s: 0.5,
            smoothing_x: 0.5,
            explanation_top_k: 15,
            max_tokens_per_message: 50_000,
            body_scan_bytes: 1_048_576,
            rules_score_bias_k: 0.0,
            budget_ms: 25,
            seed_from_default: true,
            // OFF: the golden-score test pins the classifier's default
            // behaviour, and this must not move it.
            base_rate_prior: false,
            base_rate_pseudocount: 20.0,
            base_rate_min: 0.2,
            base_rate_max: 0.8,
        }
    }
}
