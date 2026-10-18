//! SASL mechanism parsers (Phase 1: PLAIN, LOGIN; OAUTHBEARER stub).
//!
//! Phase 1 supports two mechanism families:
//!
//! - **PLAIN** (RFC 4616) — `base64(authzid \0 authcid \0 password)`.
//!   The authzid is ignored: maild does not implement proxy
//!   authentication. authcid must equal the account email.
//! - **LOGIN** (legacy two-step) — two base64-encoded continuations
//!   carrying username then password.
//!
//! SASL-IR (RFC 4959) is supported: the AUTHENTICATE command may
//! carry the initial response inline (`AUTHENTICATE PLAIN <b64>`).
//!
//! `OAUTHBEARER` is parsed only as far as needed to emit a clean
//! `NO [AUTHENTICATIONFAILED] oauthbearer not configured` — the
//! provider integration lands in a later phase.

use anyhow::{Result, anyhow};
use base64::Engine;

/// Mechanism identifier as advertised in `AUTH=...` capability tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Plain,
    Login,
    OAuthBearer,
}

impl Mechanism {
    /// Parse the bare mechanism word (ASCII, case-insensitive).
    pub fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_uppercase().as_str() {
            "PLAIN" => Some(Self::Plain),
            "LOGIN" => Some(Self::Login),
            "OAUTHBEARER" => Some(Self::OAuthBearer),
            _ => None,
        }
    }
}

/// Successful PLAIN decode result.
#[derive(Debug, Clone)]
pub struct PlainCreds {
    /// Authentication identity (the login email).
    pub authcid: String,
    /// Cleartext password as supplied by the client.
    pub password: String,
}

/// Parse a SASL PLAIN response body.
///
/// `b64` is the base64 token from either an SASL-IR (sent on the
/// AUTHENTICATE line) or a continuation response. The decoded buffer
/// is exactly three NUL-separated fields: `authzid \0 authcid \0
/// password`. Per RFC 4616 §2 authzid may be empty; if present and
/// non-empty it is rejected here because maild does not implement
/// SASL proxy auth.
pub fn parse_plain(b64: &str) -> Result<PlainCreds> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| anyhow!("malformed base64"))?;
    let mut parts = bytes.split(|b| *b == 0);
    let authzid = parts.next().ok_or_else(|| anyhow!("malformed PLAIN"))?;
    let authcid = parts.next().ok_or_else(|| anyhow!("malformed PLAIN"))?;
    let password = parts.next().ok_or_else(|| anyhow!("malformed PLAIN"))?;
    if parts.next().is_some() {
        return Err(anyhow!("extra fields in PLAIN"));
    }
    if !authzid.is_empty() && authzid != authcid {
        // RFC 4616 allows authzid == authcid as equivalent to empty;
        // anything else is proxy auth, which we refuse.
        return Err(anyhow!("authzid not supported"));
    }
    let authcid = std::str::from_utf8(authcid)
        .map_err(|_| anyhow!("non-UTF8 authcid"))?
        .to_string();
    let password = std::str::from_utf8(password)
        .map_err(|_| anyhow!("non-UTF8 password"))?
        .to_string();
    if authcid.is_empty() {
        return Err(anyhow!("empty authcid"));
    }
    Ok(PlainCreds { authcid, password })
}

/// Decode a single base64 token from a LOGIN-mechanism continuation
/// (one for username, one for password). Returns the UTF-8 string.
pub fn decode_login_field(b64: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| anyhow!("malformed base64"))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("non-UTF8 field"))
}

/// Encode a server challenge to base64 for the `+` continuation.
/// Phase 1 uses empty challenges (just `+ \r\n`); this helper exists
/// so the OAUTHBEARER stub and future GSSAPI/SCRAM mechanisms can
/// emit non-empty challenges without re-importing the engine.
pub fn encode_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn plain_basic() {
        let raw = b"\x00alice@example.com\x00hunter2";
        let creds = parse_plain(&b64(raw)).unwrap();
        assert_eq!(creds.authcid, "alice@example.com");
        assert_eq!(creds.password, "hunter2");
    }

    #[test]
    fn plain_empty_authzid_ok() {
        let raw = b"\x00bob\x00pw";
        assert!(parse_plain(&b64(raw)).is_ok());
    }

    #[test]
    fn plain_authzid_equal_authcid_ok() {
        let raw = b"bob\x00bob\x00pw";
        assert!(parse_plain(&b64(raw)).is_ok());
    }

    #[test]
    fn plain_rejects_proxy_authzid() {
        let raw = b"proxy\x00bob\x00pw";
        assert!(parse_plain(&b64(raw)).is_err());
    }

    #[test]
    fn plain_rejects_empty_authcid() {
        let raw = b"\x00\x00pw";
        assert!(parse_plain(&b64(raw)).is_err());
    }

    #[test]
    fn plain_rejects_extra_fields() {
        let raw = b"\x00bob\x00pw\x00extra";
        assert!(parse_plain(&b64(raw)).is_err());
    }

    #[test]
    fn plain_rejects_bad_base64() {
        assert!(parse_plain("!!! not base64 !!!").is_err());
    }

    #[test]
    fn login_field_decode() {
        let user = decode_login_field(&b64(b"alice@example.com")).unwrap();
        assert_eq!(user, "alice@example.com");
    }

    #[test]
    fn mechanism_parse_is_case_insensitive() {
        assert_eq!(Mechanism::parse("plain"), Some(Mechanism::Plain));
        assert_eq!(Mechanism::parse("LOGIN"), Some(Mechanism::Login));
        assert_eq!(
            Mechanism::parse("OAuthBearer"),
            Some(Mechanism::OAuthBearer)
        );
        assert_eq!(Mechanism::parse("CRAM-MD5"), None);
    }
}
