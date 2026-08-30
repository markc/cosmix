use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cosmix_maild_bayesian::{
    Classifier, ClassifierConfig, ClassifyContext, DefaultClassifier, Label, Result,
    storage::{AccountConnection, SqliteAccountConnection, StorageBackend},
};
use cosmix_maild_rules::AccountId;
use rusqlite::Connection;
use spamlite::scoring::{BaseRatePrior, CombineMode, Params};

struct TempDb {
    dir: PathBuf,
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bayes-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bayes.db");
        Self { dir, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct SingleConn(Arc<SqliteAccountConnection>);

#[async_trait]
impl StorageBackend for SingleConn {
    async fn open_account(&self, _account: &AccountId) -> Result<Arc<dyn AccountConnection>> {
        Ok(self.0.clone() as Arc<dyn AccountConnection>)
    }
}

// MUST mirror classifier.rs's Params mapping: this test proves the engine's
// maths and storage interop, not that the independently duplicated mapping is correct.
fn params(config: &ClassifierConfig, good: u64, spam: u64, threshold: f32) -> Params {
    let centre = spamlite::scoring::centre_from_base_rate(
        good,
        spam,
        config.smoothing_x as f64,
        &BaseRatePrior {
            enabled: config.base_rate_prior,
            pseudocount: config.base_rate_pseudocount as f64,
            min: config.base_rate_min as f64,
            max: config.base_rate_max as f64,
        },
    );
    Params {
        strength: config.smoothing_s as f64,
        unknown_prob: centre,
        max_interesting: config.robinson_max_extreme_tokens as usize,
        threshold: threshold as f64,
        good_bias: 1.0,
        min_word_count: 0,
        combine_mode: CombineMode::Fisher,
        new_word_score: centre,
        min_distance: 0.0,
        min_array_size: 0,
        train_max_reps: 1,
        rail: false,
        ..Params::default()
    }
}

fn fixed_tokens(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_string()).collect()
}

#[tokio::test]
async fn spamlite_written_db_classifies_with_identical_engine_math() {
    let temp = TempDb::new("engine-to-maild");
    let engine = spamlite::storage::Database::open(&temp.path).unwrap();
    let spam = fixed_tokens(&["h:subject:buy", "h:subject:now", "b:viagra", "b:discount"]);
    let ham = fixed_tokens(&[
        "h:subject:meeting",
        "h:subject:tomorrow",
        "b:agenda",
        "b:discuss",
    ]);
    for _ in 0..12 {
        engine.train_message(&spam, true).unwrap();
    }
    for _ in 0..9 {
        engine.train_message(&ham, false).unwrap();
    }
    drop(engine);

    let maild = Arc::new(SqliteAccountConnection::open_path(&temp.path, 0).unwrap());
    let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(maild));
    let config = ClassifierConfig {
        cold_start_floor: 0,
        spam_threshold: 0.5,
        ..ClassifierConfig::default()
    };
    let classifier = DefaultClassifier::new(config.clone(), backend);
    let account = AccountId::new("interop");
    let message = b"From: sender@example.com\r\nSubject: buy now\r\n\r\nviagra discount\r\n";
    let verdict = classifier
        .classify(&ClassifyContext {
            message,
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        })
        .await
        .unwrap();

    let engine = spamlite::storage::Database::open(&temp.path).unwrap();
    let tokens = spamlite::tokenizer::tokenize(message);
    // Mirror maild's priority cap for this short stream (which is unchanged).
    assert!(tokens.len() <= config.max_tokens_per_message as usize);
    let known = engine.lookup_tokens(&tokens).unwrap();
    let good = engine.total_good().unwrap();
    let spam_total = engine.total_spam().unwrap();
    let expected = spamlite::scoring::classify_tokens(
        &tokens,
        &known,
        good,
        spam_total,
        &params(&config, good, spam_total, verdict.threshold),
    );

    assert_eq!(verdict.score.to_bits(), (expected.score as f32).to_bits());
    assert_eq!(
        verdict.label,
        if expected.score as f32 >= verdict.threshold {
            Label::Spam
        } else {
            Label::Ham
        }
    );

    let counts: Vec<(u64, u64)> = tokens
        .iter()
        .map(|word| known.get(word).copied().unwrap_or((0, 0)))
        .collect();
    let expected_indices: Vec<usize> = expected
        .scored
        .as_ref()
        .map(|scored| scored.selected.as_slice())
        .unwrap_or(&[])
        .iter()
        .copied()
        .filter(|&index| counts[index] != (0, 0))
        .take(config.explanation_top_k as usize)
        .collect();
    assert_eq!(verdict.contributions.len(), expected_indices.len());
    for (contribution, index) in verdict.contributions.iter().zip(expected_indices) {
        assert_eq!(contribution.token, tokens[index]);
        assert_eq!(
            contribution.probability.to_bits(),
            (expected.fws[index] as f32).to_bits()
        );
    }
}

#[tokio::test]
async fn maild_written_db_is_visible_through_spamlite_database() {
    let temp = TempDb::new("maild-to-engine");
    let maild = SqliteAccountConnection::open_path(&temp.path, 0).unwrap();
    let shared = fixed_tokens(&["b:shared", "b:offer"]);
    let spam_only = fixed_tokens(&["b:spam-only"]);

    maild
        .record_label("spam-1", &shared, Label::Spam, 0)
        .await
        .unwrap();
    maild
        .record_label("ham-1", &shared, Label::Ham, 0)
        .await
        .unwrap();
    maild
        .record_label("spam-2", &spam_only, Label::Spam, 0)
        .await
        .unwrap();

    let engine = spamlite::storage::Database::open(&temp.path).unwrap();
    let counts = engine.counts().unwrap();
    assert_eq!((counts.total_good, counts.total_spam), (1, 2));
    assert_eq!(counts.unique_tokens, 3);
    let seen = engine.lookup_tokens(&[shared, spam_only].concat()).unwrap();
    assert_eq!(seen["b:shared"], (1, 1));
    assert_eq!(seen["b:offer"], (1, 1));
    assert_eq!(seen["b:spam-only"], (0, 1));
}

#[tokio::test]
async fn label_decision_stays_on_mailds_f32_boundary() {
    let temp = TempDb::new("f32-boundary");
    let maild = Arc::new(SqliteAccountConnection::open_path(&temp.path, 0).unwrap());
    maild
        .record_label("ham", &fixed_tokens(&["b:signal"]), Label::Ham, 0)
        .await
        .unwrap();
    maild
        .record_label("spam", &fixed_tokens(&["b:dummy"]), Label::Spam, 0)
        .await
        .unwrap();

    let threshold = (1.0_f64 / 6.0) as f32;
    let config = ClassifierConfig {
        cold_start_floor: 0,
        spam_threshold: threshold,
        robinson_max_extreme_tokens: 1,
        ..ClassifierConfig::default()
    };
    let backend: Arc<dyn StorageBackend> = Arc::new(SingleConn(Arc::clone(&maild)));
    let classifier = DefaultClassifier::new(config.clone(), backend);
    let account = AccountId::new("boundary");
    let message = b"Subject: signal\r\n\r\nsignal\r\n";

    let engine = spamlite::storage::Database::open(&temp.path).unwrap();
    let tokens = spamlite::tokenizer::tokenize(message);
    // Mirror maild's priority cap for this short stream (which is unchanged).
    assert!(tokens.len() <= config.max_tokens_per_message as usize);
    let known = engine.lookup_tokens(&tokens).unwrap();
    let good = engine.total_good().unwrap();
    let spam = engine.total_spam().unwrap();
    let classified = spamlite::scoring::classify_tokens(
        &tokens,
        &known,
        good,
        spam,
        &params(&config, good, spam, threshold),
    );

    // No absolute f64 bit pin here: the score passes through ln/exp, whose
    // last-bit rounding is not guaranteed identical across libm
    // implementations. The tolerance + side-of-threshold + f32-cast
    // assertions below are what pin the behaviour this test exists for.
    assert!((classified.score - threshold as f64).abs() < 1e-7);
    assert!(classified.score < threshold as f64);
    assert_eq!(classified.verdict, spamlite::scoring::Verdict::Good);

    let verdict = classifier
        .classify(&ClassifyContext {
            message,
            account: &account,
            rules_score: 0.0,
            matched_rules: &[],
            trusted: false,
        })
        .await
        .unwrap();
    let expected_label = if classified.score as f32 >= threshold {
        Label::Spam
    } else {
        Label::Ham
    };
    assert_eq!(expected_label, Label::Spam);
    assert_eq!(verdict.label, expected_label);
    assert_eq!(verdict.score.to_bits(), (classified.score as f32).to_bits());
}

#[test]
fn relabel_only_reverses_tokens_in_the_current_tokenizer_stream() {
    let temp = TempDb::new("reversal-scope");
    let conn = Connection::open(&temp.path).unwrap();
    spamlite::storage::schema::init(&conn).unwrap();
    let frozen = fixed_tokens(&["b:style", "b:nbsp", "b:helvetica", "b:meeting", "b:agenda"]);
    let current = fixed_tokens(&["b:meeting", "b:agenda"]);

    let tx = spamlite::storage::ops::begin_immediate(&conn).unwrap();
    spamlite::storage::ops::train(&tx, &frozen, true).unwrap();
    tx.commit().unwrap();
    let tx = spamlite::storage::ops::begin_immediate(&conn).unwrap();
    let result = spamlite::storage::ops::relabel(&tx, &current, true, false).unwrap();
    tx.commit().unwrap();

    assert_eq!(result.stranded, 0);
    let seen: HashMap<String, (u64, u64)> =
        spamlite::storage::ops::lookup_tokens(&conn, &frozen).unwrap();
    for markup in ["b:style", "b:nbsp", "b:helvetica"] {
        assert_eq!(seen[markup], (0, 1), "{markup} must remain spam-side");
    }
    for content in ["b:meeting", "b:agenda"] {
        assert_eq!(seen[content], (1, 0), "{content} must move classes");
    }
}
