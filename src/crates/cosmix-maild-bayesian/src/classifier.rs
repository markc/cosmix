//! Async per-account policy wrapper around spamlite's Robinson-Fisher engine.
//!
//! Score formula:
//!   * raw_p = spam / (spam + good_bias * good)
//!   * fw    = (s * x + n * raw_p) / (s + n)               (Robinson)
//!   * H_spam = -2 * Σ ln(1 - fw)                           (Fisher)
//!   * H_ham  = -2 * Σ ln(fw)
//!   * score  = (1 + (1 - χ²cdf(H_spam, 2n)) - (1 - χ²cdf(H_ham, 2n))) / 2
//!
//! Cold-start: when `total_spam + total_ham < cold_start_floor`, the
//! lenient `cold_start_spam_threshold` is used. The score itself is
//! computed identically — only the threshold differs.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use cosmix_maild_rules::AccountId;

use crate::config::ClassifierConfig;
use crate::error::Result;
use crate::storage::tokens::apply_token_cap;
use crate::storage::{AccountConnection, StorageBackend};
use crate::types::{
    AccountStats, BayesianVerdict, ClassifyContext, Label, RetrainOutcome, RetrainRequest,
    TokenContribution,
};

#[async_trait]
pub trait Classifier: Send + Sync {
    async fn classify(&self, ctx: &ClassifyContext<'_>) -> Result<BayesianVerdict>;

    /// Apply a label to a previously classified message. Idempotent
    /// per `(account, stamp_id)`.
    async fn retrain(&self, req: &RetrainRequest<'_>) -> Result<RetrainOutcome>;

    async fn stats(&self, account: &AccountId) -> Result<AccountStats>;

    async fn reset_account(&self, account: &AccountId) -> Result<()>;

    async fn snapshot_account(&self, account: &AccountId) -> Result<Option<PathBuf>>;
}

pub struct DefaultClassifier {
    config: ClassifierConfig,
    storage: Arc<dyn StorageBackend>,
}

impl DefaultClassifier {
    pub fn new(config: ClassifierConfig, storage: Arc<dyn StorageBackend>) -> Self {
        Self { config, storage }
    }

    /// Open the account connection used by the live classifier. Administrative
    /// rebuilds use the same cached connection for replay, snapshot and swap.
    pub async fn open_account_connection(
        &self,
        account: &AccountId,
    ) -> Result<Arc<dyn AccountConnection>> {
        self.storage.open_account(account).await
    }

    /// Train one message into the supplied connection using exactly the same
    /// tokenisation and cap policy as live retraining.
    pub async fn train_into(
        &self,
        conn: &dyn AccountConnection,
        req: &RetrainRequest<'_>,
    ) -> Result<RetrainOutcome> {
        let tokens = spamlite::tokenizer::tokenize_for_training(
            req.message,
            &spamlite::tokenizer::TokenizerConfig::default(),
        );
        // `None` = the stamp already carried this label; `Some(n)` = a label
        // row was written or flipped, even when `n == 0` (a message that
        // tokenises to nothing still counts as one message of its class).
        match conn
            .record_label(
                req.stamp_id,
                &tokens,
                req.label,
                self.config.max_tokens_per_message,
            )
            .await?
        {
            None => Ok(RetrainOutcome::AlreadyLabeled),
            Some(_) => Ok(RetrainOutcome::Applied),
        }
    }

    /// Remove one message from a supplied rebuild connection using the same
    /// tokenisation policy as training. Storage reapplies the cap recorded for
    /// the stamp before reversing the corpus counts.
    pub async fn forget_from(
        &self,
        conn: &dyn AccountConnection,
        stamp_id: &str,
        message: &[u8],
    ) -> Result<Option<Label>> {
        let tokens = spamlite::tokenizer::tokenize_for_training(
            message,
            &spamlite::tokenizer::TokenizerConfig::default(),
        );
        conn.forget_label(stamp_id, &tokens).await
    }
}

#[async_trait]
impl Classifier for DefaultClassifier {
    async fn classify(&self, ctx: &ClassifyContext<'_>) -> Result<BayesianVerdict> {
        let conn = self.storage.open_account(ctx.account).await?;
        let mut tokens = spamlite::tokenizer::tokenize(ctx.message);
        apply_token_cap(&mut tokens, self.config.max_tokens_per_message);

        let (total_good_raw, total_spam_raw) = conn.totals().await?;
        let cold_start = total_good_raw + total_spam_raw < self.config.cold_start_floor as u64;
        let threshold = if cold_start {
            self.config.cold_start_spam_threshold
        } else {
            self.config.spam_threshold
        };

        // Empty corpus → uninformative score.
        if total_good_raw < 1 && total_spam_raw < 1 {
            return Ok(BayesianVerdict {
                label: Label::Ham,
                score: 0.5,
                threshold,
                cold_start: true,
                contributions: Vec::new(),
            });
        }

        let known: std::collections::HashMap<String, (u64, u64)> = conn
            .token_counts(&tokens)
            .await?
            .into_iter()
            .map(|(word, good, spam)| (word, (good, spam)))
            .collect();

        // Map maild's `ClassifierConfig` onto spamlite's `Params`. maild
        // deliberately diverges from spamlite's own defaults (strength 0.5 not
        // 1.0; max_interesting 15 not 150), so we pass OUR values rather than
        // taking `Params::default()` — adopting spamlite's defaults here would
        // silently re-tune the filter under the guise of a version bump.
        //
        // The rest are the values the previous inline implementation hardcoded:
        // good_bias 1.0, min_word_count 0 (gating disabled), Fisher, and
        // min_distance/min_array_size 0 (selection = plain top-N by |fw-0.5|).
        // `rail: false` keeps the engine's abuse-TLD hard-rail off; turning it on
        // is a separate config decision, not a side effect of this integration.
        let unknown_prob = spamlite::scoring::centre_from_base_rate(
            total_good_raw,
            total_spam_raw,
            self.config.smoothing_x as f64,
            &spamlite::scoring::BaseRatePrior {
                enabled: self.config.base_rate_prior,
                pseudocount: self.config.base_rate_pseudocount as f64,
                min: self.config.base_rate_min as f64,
                max: self.config.base_rate_max as f64,
            },
        );

        let params = spamlite::scoring::Params {
            strength: self.config.smoothing_s as f64,
            unknown_prob,
            max_interesting: self.config.robinson_max_extreme_tokens as usize,
            threshold: threshold as f64,
            good_bias: 1.0,
            min_word_count: 0,
            combine_mode: spamlite::scoring::CombineMode::Fisher,
            new_word_score: unknown_prob,
            min_distance: 0.0,
            min_array_size: 0,
            train_max_reps: 1,
            rail: false,
            ..spamlite::scoring::Params::default()
        };

        // spamlite gives tokens absent from `known` `new_word_score` directly.
        // maild previously routed them through score_token(0, 0); these are
        // bit-equal at maild's defaults because strength is the power-of-two 0.5
        // and new_word_score == unknown_prob. The goldens pin that equivalence.
        let counts: Vec<(u64, u64)> = tokens
            .iter()
            .map(|word| known.get(word).copied().unwrap_or((0, 0)))
            .collect();
        let classified = spamlite::scoring::classify_tokens(
            &tokens,
            &known,
            total_good_raw,
            total_spam_raw,
            &params,
        );

        // The former inline engine returned no score when it selected no
        // evidence. Preserve maild's policy: rules bias and the threshold must
        // not turn an evidence-free message into spam.
        if classified.scored.is_none() {
            return Ok(BayesianVerdict {
                label: Label::Ham,
                score: 0.5,
                threshold,
                cold_start,
                contributions: Vec::new(),
            });
        }

        let mut score = classified.score;

        // maild-only: optional log-odds nudge from the rules stage. spamlite has
        // no rules stage, so this has no upstream counterpart. Default k=0 (off).
        if self.config.rules_score_bias_k != 0.0 {
            score = apply_log_odds_bias(score, ctx.rules_score, self.config.rules_score_bias_k);
        }

        let label = if score as f32 >= threshold {
            Label::Spam
        } else {
            Label::Ham
        };

        // Contributions follow the same selected indices the score was built
        // from. Unknown tokens (good=spam=0) carry no evidence and are dropped.
        let top_k = self.config.explanation_top_k as usize;
        let contributions: Vec<TokenContribution> = classified
            .scored
            .as_ref()
            .map(|scored| scored.selected.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|&&i| counts[i] != (0, 0))
            .take(top_k)
            .map(|&i| TokenContribution {
                token: tokens[i].clone(),
                spam_count: counts[i].1 as u32,
                ham_count: counts[i].0 as u32,
                probability: classified.fws[i] as f32,
                contribution: (classified.fws[i] - 0.5) as f32,
            })
            .collect();

        Ok(BayesianVerdict {
            label,
            score: score as f32,
            threshold,
            cold_start,
            contributions,
        })
    }

    async fn retrain(&self, req: &RetrainRequest<'_>) -> Result<RetrainOutcome> {
        let conn = self.storage.open_account(req.account).await?;
        self.train_into(conn.as_ref(), req).await
    }

    async fn stats(&self, account: &AccountId) -> Result<AccountStats> {
        let conn = self.storage.open_account(account).await?;
        conn.stats().await
    }

    async fn reset_account(&self, account: &AccountId) -> Result<()> {
        let conn = self.storage.open_account(account).await?;
        conn.reset().await
    }

    async fn snapshot_account(&self, account: &AccountId) -> Result<Option<PathBuf>> {
        let conn = self.storage.open_account(account).await?;
        conn.snapshot().await
    }
}

/// Apply a log-odds nudge from the rules-stage score:
///   logit(score') = logit(score) + k * rules_score
fn apply_log_odds_bias(score: f64, rules_score: f32, k: f32) -> f64 {
    let p = score.clamp(1e-6, 1.0 - 1e-6);
    let logit = (p / (1.0 - p)).ln();
    let adjusted = logit + (k as f64) * (rules_score as f64);
    1.0 / (1.0 + (-adjusted).exp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{AccountConnection, SqliteAccountConnection};
    use std::path::Path;

    async fn seeded_classifier_with(config: ClassifierConfig) -> (DefaultClassifier, AccountId) {
        // Build a classifier whose backend wraps a single in-memory
        // SQLite connection. We bypass the on-disk SqliteBackend so
        // tests stay hermetic.
        struct SingleConn(Arc<SqliteAccountConnection>);
        #[async_trait]
        impl StorageBackend for SingleConn {
            async fn open_account(
                &self,
                _account: &AccountId,
            ) -> Result<Arc<dyn crate::storage::AccountConnection>> {
                Ok(self.0.clone() as Arc<dyn crate::storage::AccountConnection>)
            }
        }

        let conn = SqliteAccountConnection::open_path(Path::new(":memory:"), 100).unwrap();
        let conn = Arc::new(conn);
        // Train 10 ham + 10 spam messages.
        for _ in 0..10 {
            conn.record_label(
                &format!("h{}", uuid_like()),
                &[
                    "h:subject:meeting".into(),
                    "h:subject:tomorrow".into(),
                    "b:agenda".into(),
                    "b:discuss".into(),
                ],
                Label::Ham,
                0,
            )
            .await
            .unwrap();
        }
        for _ in 0..10 {
            conn.record_label(
                &format!("s{}", uuid_like()),
                &[
                    "h:subject:buy".into(),
                    "h:subject:now".into(),
                    "b:viagra".into(),
                    "b:discount".into(),
                ],
                Label::Spam,
                0,
            )
            .await
            .unwrap();
        }

        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(conn));
        let classifier = DefaultClassifier::new(config, backend);
        (classifier, AccountId::new("t"))
    }

    /// Regression for the old alphabetical 200-token cap that dropped sender
    /// evidence. A long message from a strongly-ham sender must remain Ham and
    /// retain that sender contribution both uncapped and at cap 200.
    #[tokio::test]
    async fn long_message_keeps_sender_tokens_with_or_without_cap() {
        async fn build(cap: u32) -> (DefaultClassifier, AccountId, Vec<u8>) {
            let (classifier, account) = seeded_classifier_with(ClassifierConfig {
                cold_start_floor: 0,
                spam_threshold: 0.5,
                max_tokens_per_message: cap,
                ..ClassifierConfig::default()
            })
            .await;
            // Sender strongly ham; the message's body words are all UNKNOWN except
            // the seeded spam vocabulary, so the sender token is the decisive ham
            // evidence — exactly what the cap used to cut.
            let conn = classifier.storage.open_account(&account).await.unwrap();
            for i in 0..40 {
                conn.record_label(
                    &format!("from{i}"),
                    &[
                        "h:from:owner@example.test".into(),
                        "h:from:example.test".into(),
                    ],
                    Label::Ham,
                    0,
                )
                .await
                .unwrap();
            }
            // 260 distinct (unknown) body words so the stream is well past 200
            // tokens, plus ONE seeded spam word: the only evidence in the body
            // leans spam, the sender tokens are the only ham evidence.
            let mut body = String::from("discount ");
            for i in 0..260 {
                body.push_str(&format!("word{i:03} "));
            }
            let msg = format!(
                "From: owner@example.test\r\nTo: c@d.test\r\nSubject: hello\r\n\r\n{body}\r\n"
            )
            .into_bytes();
            (classifier, account, msg)
        }

        async fn run(cap: u32) -> BayesianVerdict {
            let (classifier, account, msg) = build(cap).await;
            classifier
                .classify(&ClassifyContext {
                    message: &msg,
                    account: &account,
                    rules_score: 0.0,
                    matched_rules: &[],
                    trusted: false,
                })
                .await
                .unwrap()
        }

        let uncapped = run(0).await;
        assert_eq!(
            uncapped.label,
            Label::Ham,
            "uncapped: sender evidence must win, got {uncapped:?}"
        );
        assert!(
            uncapped
                .contributions
                .iter()
                .any(|c| c.token == "h:from:owner@example.test"),
            "uncapped: sender token must be among contributions: {:?}",
            uncapped
                .contributions
                .iter()
                .map(|c| &c.token)
                .collect::<Vec<_>>()
        );

        let capped = run(200).await;
        assert_eq!(
            capped.label,
            Label::Ham,
            "cap=200: sender evidence must win, got {capped:?}"
        );
        assert!(
            capped
                .contributions
                .iter()
                .any(|c| c.token == "h:from:owner@example.test"),
            "cap=200: sender token must be among contributions: {:?}",
            capped
                .contributions
                .iter()
                .map(|c| &c.token)
                .collect::<Vec<_>>()
        );
    }

    async fn seeded_classifier() -> (DefaultClassifier, AccountId) {
        // Pretend we're past cold-start; pin spam_threshold to spamlite's
        // default for direct comparison.
        seeded_classifier_with(ClassifierConfig {
            cold_start_floor: 0,
            spam_threshold: 0.5,
            ..ClassifierConfig::default()
        })
        .await
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!("stamp-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    #[tokio::test]
    async fn classify_spammy_message() {
        let (cls, account) = seeded_classifier().await;
        let msg = b"From: x@evil\r\nSubject: BUY NOW\r\n\r\nviagra discount\r\n";
        let ctx = ClassifyContext {
            message: msg,
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v = cls.classify(&ctx).await.unwrap();
        assert_eq!(v.label, Label::Spam, "expected Spam, got {v:?}");
        assert!(v.score > 0.7, "score was {}", v.score);
    }

    #[tokio::test]
    async fn classify_hammy_message() {
        let (cls, account) = seeded_classifier().await;
        let msg = b"From: c@work\r\nSubject: meeting tomorrow\r\n\r\nagenda discuss\r\n";
        let ctx = ClassifyContext {
            message: msg,
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v = cls.classify(&ctx).await.unwrap();
        assert_eq!(v.label, Label::Ham, "expected Ham, got {v:?}");
        assert!(v.score < 0.3, "score was {}", v.score);
    }

    #[tokio::test]
    async fn empty_corpus_returns_uninformative_half() {
        struct Empty;
        #[async_trait]
        impl StorageBackend for Empty {
            async fn open_account(
                &self,
                _account: &AccountId,
            ) -> Result<Arc<dyn crate::storage::AccountConnection>> {
                Ok(Arc::new(SqliteAccountConnection::open_path(
                    Path::new(":memory:"),
                    100,
                )?))
            }
        }
        let cls = DefaultClassifier::new(ClassifierConfig::default(), Arc::new(Empty));
        let account = AccountId::new("t");
        let ctx = ClassifyContext {
            message: b"From: x\r\nSubject: hi\r\n\r\nbody\r\n",
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v = cls.classify(&ctx).await.unwrap();
        assert!(v.cold_start);
        assert!((v.score - 0.5).abs() < 0.01, "{}", v.score);
    }

    #[tokio::test]
    async fn empty_token_stream_ignores_rules_bias_and_threshold() {
        let (cls, account) = seeded_classifier_with(ClassifierConfig {
            cold_start_floor: 0,
            spam_threshold: 0.5,
            rules_score_bias_k: 2.0,
            ..ClassifierConfig::default()
        })
        .await;
        let ctx = ClassifyContext {
            message: b"",
            account: &account,
            rules_score: 5.0,
            matched_rules: &[],
            trusted: false,
        };

        let verdict = cls.classify(&ctx).await.unwrap();
        assert_eq!(verdict.label, Label::Ham);
        assert_eq!(verdict.score, 0.5);
        assert!(verdict.contributions.is_empty());
    }

    #[tokio::test]
    async fn zero_extreme_token_limit_returns_evidence_free_ham() {
        let (cls, account) = seeded_classifier_with(ClassifierConfig {
            cold_start_floor: 0,
            spam_threshold: 0.5,
            robinson_max_extreme_tokens: 0,
            ..ClassifierConfig::default()
        })
        .await;
        let ctx = ClassifyContext {
            message: b"Subject: buy now\r\n\r\nviagra discount\r\n",
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };

        let verdict = cls.classify(&ctx).await.unwrap();
        assert_eq!(verdict.label, Label::Ham);
        assert_eq!(verdict.score, 0.5);
        assert!(verdict.contributions.is_empty());
    }
}
#[cfg(test)]
mod base_rate_prior_tests {
    //! End-to-end coverage for maild's mapping onto spamlite's base-rate helper.

    use super::*;

    /// A deliberately SPAM-HEAVY corpus (450 spam / 50 ham = 0.9 raw base rate),
    /// which is where the prior has something to say. A balanced corpus would
    /// leave the centre at 0.5 and demonstrate nothing.
    async fn spam_heavy_classifier(on: bool) -> (DefaultClassifier, AccountId) {
        use crate::storage::{AccountConnection, SqliteAccountConnection};
        use std::path::Path;

        struct SingleConn(Arc<SqliteAccountConnection>);
        #[async_trait]
        impl StorageBackend for SingleConn {
            async fn open_account(&self, _a: &AccountId) -> Result<Arc<dyn AccountConnection>> {
                Ok(self.0.clone() as Arc<dyn AccountConnection>)
            }
        }

        let conn =
            Arc::new(SqliteAccountConnection::open_path(Path::new(":memory:"), 100).unwrap());
        for i in 0..450 {
            conn.record_label(&format!("s{i}"), &["b:viagra".into()], Label::Spam, 0)
                .await
                .unwrap();
        }
        for i in 0..50 {
            conn.record_label(&format!("h{i}"), &["b:agenda".into()], Label::Ham, 0)
                .await
                .unwrap();
        }
        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(conn));
        let config = ClassifierConfig {
            base_rate_prior: on,
            cold_start_floor: 0,
            spam_threshold: 0.5,
            ..ClassifierConfig::default()
        };
        (
            DefaultClassifier::new(config, backend),
            AccountId::new("br"),
        )
    }

    #[tokio::test]
    async fn enabled_prior_raises_unknown_vocabulary_on_a_spam_heavy_corpus() {
        // End-to-end through the real classifier, and the clearest statement of
        // what this flag DOES: a message of entirely unseen words scores exactly
        // the Robinson centre, because there is no other evidence.
        //
        //   off → 0.5   ("an unseen word is a coin flip")
        //   on  → ~0.8  (clamped from this corpus's 0.9 observed spam rate)
        //
        // This is also, honestly, the flag's RISK in one line: novel ham
        // vocabulary arriving at a spam-heavy mailbox now starts life
        // spam-leaning. Whether that trade pays needs corpus evaluation, which is
        // why this ships off.
        let msg = b"Subject: zzqq\r\n\r\nnevertrained vocabulary here\r\n";

        let (off, acct) = spam_heavy_classifier(false).await;
        let ctx = ClassifyContext {
            message: msg,
            account: &acct,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v_off = off.classify(&ctx).await.unwrap();
        assert!(
            (v_off.score - 0.5).abs() < 1e-6,
            "off: unknown-only message must score the fixed 0.5 centre, got {}",
            v_off.score
        );

        let (on, acct2) = spam_heavy_classifier(true).await;
        let ctx2 = ClassifyContext {
            message: msg,
            account: &acct2,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v_on = on.classify(&ctx2).await.unwrap();
        assert!(
            v_on.score > 0.75,
            "on: the corpus's own spam rate should lift it well above 0.5, got {}",
            v_on.score
        );
    }

    #[tokio::test]
    async fn enabled_prior_does_not_override_real_evidence() {
        // The guard rail that makes the whole idea defensible: the prior only
        // fills the vacuum where evidence is ABSENT. A message made of strongly
        // ham-trained tokens must still come out ham, even on a 90%-spam corpus.
        // If this ever fails, the prior is overriding data and the flag is wrong.
        let (on, acct) = spam_heavy_classifier(true).await;
        let msg = b"Subject: agenda\r\n\r\nagenda agenda agenda\r\n";
        let ctx = ClassifyContext {
            message: msg,
            account: &acct,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        };
        let v = on.classify(&ctx).await.unwrap();
        assert_eq!(
            v.label,
            Label::Ham,
            "trained ham evidence must still beat the prior (score {})",
            v.score
        );
    }
}
