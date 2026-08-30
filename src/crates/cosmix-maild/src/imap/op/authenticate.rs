//! `AUTHENTICATE <mechanism> [<initial-response>]` — RFC 9051 §6.2.2.
//!
//! Phase 1 parses the AUTHENTICATE command line and surfaces the
//! mechanism + optional SASL-IR initial response. The session loop
//! drives the continuation exchange (one `+ \r\n` per missing
//! field) — keeping the I/O out of this module makes it cheap to
//! unit-test argument parsing.

use anyhow::{Result, anyhow};

use crate::imap::sasl::Mechanism;

#[derive(Debug, Clone)]
pub struct AuthenticateArgs {
    pub mechanism: Mechanism,
    /// SASL-IR initial response (RFC 4959). `None` means the client
    /// sent only the mechanism word; the server replies `+ \r\n` and
    /// reads the response on the next line.
    pub initial_response: Option<String>,
}

/// Parse the argument tail of `AUTHENTICATE`.
pub fn parse_args(args: &str) -> Result<AuthenticateArgs> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("missing mechanism"));
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let mech_word = parts.next().ok_or_else(|| anyhow!("missing mechanism"))?;
    let mechanism =
        Mechanism::parse(mech_word).ok_or_else(|| anyhow!("unsupported mechanism: {mech_word}"))?;
    let initial_response = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // A standalone `=` means "empty initial response" per RFC 4959
    // §4 — distinguishes "no IR" from "IR is the empty string".
    let initial_response = match initial_response.as_deref() {
        Some("=") => Some(String::new()),
        _ => initial_response,
    };
    Ok(AuthenticateArgs {
        mechanism,
        initial_response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_no_ir() {
        let a = parse_args("PLAIN").unwrap();
        assert_eq!(a.mechanism, Mechanism::Plain);
        assert!(a.initial_response.is_none());
    }

    #[test]
    fn parses_plain_with_ir() {
        let a = parse_args("PLAIN AGFsaWNlAGh1bnRlcjI=").unwrap();
        assert_eq!(a.mechanism, Mechanism::Plain);
        assert_eq!(a.initial_response.as_deref(), Some("AGFsaWNlAGh1bnRlcjI="));
    }

    #[test]
    fn parses_empty_ir_marker() {
        let a = parse_args("PLAIN =").unwrap();
        assert_eq!(a.initial_response.as_deref(), Some(""));
    }

    #[test]
    fn parses_login() {
        assert_eq!(parse_args("LOGIN").unwrap().mechanism, Mechanism::Login);
    }

    #[test]
    fn rejects_unknown_mechanism() {
        assert!(parse_args("CRAM-MD5").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_args("").is_err());
    }
}
