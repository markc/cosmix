# cosmix-maild-bayesian

`cosmix-maild-bayesian` is the per-account Bayesian classification library used as the third stage of `cosmix-maild`'s inbound DATA filter pipeline, after `cosmix-maild-rules` returns `Continue`. It tokenises RFC 5322 messages, scores them with a Robinson-Fisher classifier, stores training state in SQLite, and returns a routing verdict. In the `bus <- mix <- cos` dependency chain it is a cos-layer crate: it has no direct Mix dependency, while its optional `cosmix` feature activates direct dependencies on Bus client libraries.

## Synopsis

The Cargo package is `cosmix-maild-bayesian`. Rust code imports it as `cosmix_maild_bayesian`.

```rust
use std::sync::Arc;

use cosmix_maild_bayesian::{
    Classifier, ClassifierConfig, ClassifyContext, DefaultClassifier,
    storage::SqliteBackend,
};
use cosmix_maild_rules::AccountId;

let config = ClassifierConfig::default();
let storage = Arc::new(SqliteBackend::new(
    "/var/lib/example-mail/bayesian",
    None,
    config.cold_start_floor,
));
let classifier = DefaultClassifier::new(config, storage);

let account = AccountId::new("alpha");
let message = b"From: sender@example.com\r\nSubject: Status\r\n\r\nAll clear.\r\n";
let verdict = classifier.classify(&ClassifyContext {
    message,
    account: &account,
    rules_score: 0.0,
    matched_rules: &[],
    trusted: false,
}).await?;
```

## Classification

`Classifier` is the asynchronous service trait. It defines:

- `classify` to produce a `BayesianVerdict` for one message and account.
- `retrain` to apply a `Spam` or `Ham` label under a stable stamp identifier.
- `stats` to read per-account corpus statistics.

`DefaultClassifier` implements the trait over an injected `StorageBackend`.

Classification tokenises the message, applies `max_tokens_per_message`, fetches per-token ham and spam counts, applies Robinson correction, selects the most informative tokens, and combines their probabilities with Fisher's method.

An empty corpus returns `Ham` with score `0.5`. A non-empty corpus returns `Spam` when its score is greater than or equal to the effective threshold, otherwise `Ham`.

Cold-start mode applies while the combined ham and spam message count is below `cold_start_floor`. It changes the threshold without changing the score calculation.

When `rules_score_bias_k` is non-zero, the preceding rules score adjusts the classifier score in log-odds space.

`BayesianVerdict` contains the label, score, effective threshold, cold-start flag, and the selected token contributions. Each `TokenContribution` reports the token, corpus counts, corrected probability, and signed distance from `0.5`.

## Training

`RetrainRequest` carries a stamp identifier, account, raw message, and target `Label`.

The SQLite implementation records labels by stamp identifier. Repeating the same label is an idempotent no-op. Applying the opposite label reverses the prior token and message counts before adding the new label.

`DefaultClassifier::retrain` returns `RetrainOutcome::Applied` when storage changes and `RetrainOutcome::AlreadyLabeled` when `record_label` touches no rows. `RetrainOutcome::NoStamp` is part of the public enum but is not produced by this implementation.

## Public modules

| Module | Surface |
|---|---|
| `classifier` | `Classifier` and `DefaultClassifier` |
| `config` | `ClassifierConfig` |
| `error` | crate-wide `Error` and `Result` |
| `scoring` | `Params`, `CombineMode`, `Verdict`, and `RailHit`; scoring functions remain crate-visible |
| `storage` | storage traits plus SQLite and in-memory implementations |
| `tokenizer` | default and configurable RFC 5322 tokenisation |
| `types` | classification, verdict, retraining, contribution, and statistics types |

The crate root re-exports `Classifier`, `DefaultClassifier`, `ClassifierConfig`, `Error`, `Result`, and all public types from `types`.

## Classifier configuration

`ClassifierConfig` is a programmatic Rust configuration surface. The crate does not define a configuration file format.

| Field | Default | Meaning |
|---|---:|---|
| `spam_threshold` | `0.95` | Steady-state spam threshold |
| `cold_start_spam_threshold` | `0.85` | Threshold used during cold start |
| `cold_start_floor` | `100` | Combined labelled-message count required to leave cold start |
| `robinson_max_extreme_tokens` | `15` | Maximum informative tokens used by scoring |
| `smoothing_s` | `0.5` | Robinson smoothing strength |
| `smoothing_x` | `0.5` | Fixed unknown-token probability when the base-rate prior is off |
| `explanation_top_k` | `15` | Maximum token contributions returned |
| `max_tokens_per_message` | `200` | Maximum token count passed to scoring and retraining |
| `body_scan_bytes` | `1,048,576` | Reserved body-scan limit; the classifier does not currently read it |
| `rules_score_bias_k` | `0.0` | Rules-score log-odds multiplier; zero disables adjustment |
| `budget_ms` | `25` | Reserved classification budget; the classifier does not currently enforce it |
| `seed_from_default` | `true` | Reserved seed switch; `SqliteBackend` seeding is controlled by its constructor |
| `base_rate_prior` | `false` | Use a shrunk and clamped account spam rate as the Robinson centre |
| `base_rate_pseudocount` | `20.0` | Symmetric pseudo-message weight for the base-rate prior |
| `base_rate_min` | `0.2` | Lower clamp for the derived base rate |
| `base_rate_max` | `0.8` | Upper clamp for the derived base rate |

The base-rate prior is experimental and off by default. With the flag off, `smoothing_x` remains the Robinson centre. With the flag on, the observed spam rate is shrunk towards `0.5` and clamped to the configured interval. An inverted interval falls back to `0.5`.

## Tokenisation

`tokenize` uses `TokenizerConfig::default`. `tokenize_with_config` accepts an explicit configuration.

The default tokenizer emits namespaced tokens for subject words and bigrams, sender-related headers, the first Received header, plain-text and HTML body words, and normalised URLs. It strips HTML text, drops URL queries and fragments, sorts the result, and deduplicates tokens so each token records presence rather than frequency within one message.

Unparseable input falls back to raw-text tokenisation. Token lengths are measured in bytes. Defaults accept lengths from `3` through `40`, and extraction has a hard pre-deduplication limit of `50,000` tokens.

`TokenizerConfig` also exposes expanded recipient and Received-header coverage, sender TLD tokens, Unicode confusable folding, display-name brand mismatch tokens, and authentication-result tokens. These anti-evasion options are off by default. Enabling an option changes the token stream and therefore requires retraining existing corpora.

`TokenizerConfig::from_env` recognises `SPAMLITE_EXPANDED_HEADERS`, `SPAMLITE_TLD`, `SPAMLITE_FOLD`, `SPAMLITE_BRAND`, and `SPAMLITE_AUTH`. Values `1` and `true` enable the corresponding option.

## Storage

`StorageBackend` opens an `AccountConnection`. `AccountConnection` reads token counts, records labels, returns statistics, and returns total ham and spam message counts.

`SqliteBackend::new` accepts a base directory, an optional seed database path, and the cold-start floor. It stores each account at `<base>/<account>/bayes.db` and caches open connections.

For a new account, the backend first promotes an adjacent legacy `db.sqlite` through SQLite so committed WAL data is included. If no legacy database exists, it copies the configured seed when that file exists. Otherwise it creates an empty database.

The SQLite schema contains `tokens`, `meta`, and `labels` tables. It preserves the token and metadata layout used by the compatible source database and adds labels for idempotent retraining.

`SqliteAccountConnection::open_path` opens one database directly, bypassing the backend cache.

`InMemoryBackend` and `InMemoryConnection` represent an always-empty corpus. They are suitable for tests and downstream stubs; they do not retain training.

`AccountStats` reports message totals, approximate token totals, cold-start state, seed label, and model version. The SQLite implementation reports the same unique-token count in both token-total fields and currently returns no seed label.

## Cargo features

| Feature | Default | Effect |
|---|---:|---|
| `core` | Yes | Empty marker feature for the core library surface |
| `cosmix` | No | Activates optional `cosmix-lib-bus` and native `cosmix-lib-client` dependencies |

The current source has no `cfg(feature = ...)` sections. Enabling `cosmix` adds the optional dependencies but does not add Bus verbs or feature-gated Rust items.

## Dependencies

| Dependency group | Crates |
|---|---|
| Mail pipeline | `cosmix-maild-rules` |
| Message parsing | `mail-parser`, `nanohtml2text` |
| Storage and async | `rusqlite`, `tokio`, `async-trait` |
| Data and errors | `serde`, `serde_json`, `thiserror`, `chrono` |
| Diagnostics | `tracing` |
| Optional Bus integration | `cosmix-lib-bus`, `cosmix-lib-client` |

## Migration binary

The package builds `cosmix-maild-bayesian-migrate`.

```text
cosmix-maild-bayesian-migrate
```

The binary is currently a placeholder. It prints that the migration is not wired and exits with status `2`. It does not accept a usable migration command or modify a database.

## Errors

The crate error type covers storage failures, SQLite errors, MIME parsing, untrained accounts, migration failures, and internal failures.

## Compatibility and defaults

The scoring and tokenizer modules preserve the crate's earlier default behaviour while carrying later anti-evasion controls in a default-off state. Golden-score tests pin the default token stream and Robinson-Fisher results. A separate compatibility test can compare an externally provisioned compatible database when its helper binary is available.

The default classifier uses Fisher combination. `scoring::CombineMode::Geometric` and the abuse-TLD rail parameter types are public, but `DefaultClassifier` constructs its scoring parameters internally with Fisher mode and the rail disabled.

## See also

- [Cos overview](../overview.md)
