//! `cosmix-maild-bayesian` — the async, per-account wrapper around the
//! [spamlite](https://github.com/markc/spamlite) engine used by the third stage
//! of `cosmix-maild`'s inbound DATA filter pipeline.
//!
//! ## Engine provenance
//!
//! Tokenisation, Robinson-Fisher scoring, the SQLite schema, and corpus
//! mutations come from spamlite pinned at
//! `7daaa1e6361c7a119479239e1cef06ecf09b1fec`. This crate owns the daemon-facing
//! policy and plumbing around that engine: cold-start threshold selection,
//! rules-score bias, the `f32` label decision, the per-message token cap,
//! contribution shaping, cold-start seeding and legacy promotion, async
//! per-account storage, and Bus integration.
//!
//! Runs **after** `cosmix-maild-rules` returns `Continue` and produces
//! a `BayesianVerdict` consumed by `cosmix-maild` for routing. Cold
//! starts seed from `default-bayesian.db` and use a lenient threshold
//! (0.85) until N=100 spam+ham samples are observed.
//!
//! The public verdict and retraining surface is frozen by convention in
//! `types.rs`.

pub mod classifier;
pub mod config;
pub mod error;
pub mod storage;
pub mod types;

pub use crate::classifier::{Classifier, DefaultClassifier};
pub use crate::config::ClassifierConfig;
pub use crate::error::{Error, Result};
pub use crate::types::*;
