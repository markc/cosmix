//! Crate-wide error type. Frozen surface per spec §Error.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rule pack TOML parse error: {0}")]
    PackParse(String),

    #[error("rule pack file I/O: {0}")]
    PackIo(String),

    #[error("rule {id}: regex compile failed: {source}")]
    RegexCompile { id: String, source: regex::Error },

    #[error("rule {id}: unknown match.kind {kind:?}")]
    UnknownKind { id: String, kind: String },

    #[error("rule {id}: missing required field {field:?}")]
    MissingField { id: String, field: String },

    #[error("invalid rule configuration: {0}")]
    InvalidRule(String),

    #[error("MIME parse error")]
    MimeParse,

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
