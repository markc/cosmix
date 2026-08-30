//! Crate-wide error type. Frozen surface per spec §Error.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("DNS lookup timed out for {0}")]
    DnsTimeout(String),

    #[error("DNS lookup error: {0}")]
    DnsError(String),

    #[error("failed to load signing key: {0}")]
    KeyLoad(String),

    #[error("malformed prior Authentication-Results: {0}")]
    MalformedPriorAuthResults(String),

    #[error("no DKIM signer configured for domain {0}")]
    NoSignerForDomain(String),

    #[error("upstream mail-auth error: {0}")]
    Upstream(String),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
