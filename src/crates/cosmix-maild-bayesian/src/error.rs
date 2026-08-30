//! Crate-wide error type. Frozen surface per spec §Error.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("rusqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("MIME parse error")]
    MimeParse,

    #[error("classifier untrained for account {account:?}")]
    Untrained { account: String },

    #[error("migration error: {0}")]
    Migration(String),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
