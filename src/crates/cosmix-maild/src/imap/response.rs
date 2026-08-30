//! IMAP response builders.
//!
//! Phase 1 needs only tagged status responses, untagged
//! `* CAPABILITY` / `* OK` / `* BYE`, and SASL continuations. Per
//! `_doc/maild/imap.md` §"Error mappings" we map authentication
//! refusal to `NO [AUTHENTICATIONFAILED]` (the doc's correction over
//! the historical `BAD`) and never expose internal error text.
//!
//! All strings emitted by this module are guaranteed not to contain
//! a bare CR or LF. Callers feed pre-sanitised metadata only (capability
//! list, hostname); user-controlled values would need additional
//! quoting that Phase 1 does not yet have a caller for.

use std::fmt::Write;

/// Tagged response status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bad,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::No => "NO",
            Status::Bad => "BAD",
        }
    }
}

/// Build a tagged response: `<tag> <STATUS> [<code>] <text>\r\n`.
/// `code` is the bracketed response-code (e.g. `CAPABILITY ...`,
/// `AUTHENTICATIONFAILED`, `CLIENTBUG`) without surrounding brackets;
/// pass `None` for none.
pub fn tagged(tag: &str, status: Status, code: Option<&str>, text: &str) -> String {
    let mut s = String::with_capacity(16 + text.len());
    let _ = write!(&mut s, "{tag} {} ", status.as_str());
    if let Some(c) = code {
        let _ = write!(&mut s, "[{c}] ");
    }
    s.push_str(text);
    s.push_str("\r\n");
    s
}

/// `* OK <text>\r\n` greeting / advisory.
pub fn untagged_ok(text: &str) -> String {
    format!("* OK {text}\r\n")
}

/// `* OK [<code>] <text>\r\n` — greeting with response code, used to
/// embed the unsolicited `CAPABILITY` list in the banner.
pub fn untagged_ok_with_code(code: &str, text: &str) -> String {
    format!("* OK [{code}] {text}\r\n")
}

/// `* BYE <text>\r\n` — sent before closing a connection.
pub fn untagged_bye(text: &str) -> String {
    format!("* BYE {text}\r\n")
}

/// `* CAPABILITY <body>\r\n` — emitted unsolicited in the greeting and
/// after successful LOGIN/AUTHENTICATE.
pub fn untagged_capability(caps_body: &str) -> String {
    format!("* CAPABILITY {caps_body}\r\n")
}

/// SASL continuation request: `+ <base64-challenge>\r\n`. An empty
/// challenge (the typical Phase 1 case) renders as `+ \r\n`.
pub fn continuation(challenge_b64: &str) -> String {
    format!("+ {challenge_b64}\r\n")
}

/// Render a mailbox's `seq_validity` as an IMAP `UIDVALIDITY` value.
///
/// RFC 9051 §2.3.1.1 (and RFC 3501) define UIDVALIDITY as an
/// `nz-number` — a **32-bit** unsigned integer. The mailstore's
/// `seq_validity` is an epoch-**milliseconds** stamp (e.g.
/// `1783857640000`), which overflows u32 and made every strict client
/// choke: dovecot's imapc parses the code as uint32, fails, and reports
/// "Opening mailbox failed: UIDVALIDITY not received" (found
/// 2026-08-25 trying to dsync gw's mailboxes into gco). Thunderbird
/// only survived because it is lenient.
///
/// Mapping: values that fit in u32 pass through untouched (unit tests
/// use small stamps); anything larger is taken as milliseconds and
/// reduced to seconds, which is monotonic, stable for a given mailbox,
/// and fits until 2106. It is a one-time cache invalidation for clients
/// that stored the old 64-bit value — the correct thing to happen when
/// UIDVALIDITY changes. Every IMAP wire site (SELECT/EXAMINE, STATUS,
/// APPENDUID, COPYUID) MUST go through this; the storage value itself
/// is untouched (it is also the Bus-visible `seq_validity`).
pub fn wire_uidvalidity(seq_validity: u64) -> u32 {
    if seq_validity <= u32::MAX as u64 {
        seq_validity as u32
    } else {
        // Milliseconds → seconds. 2^32 ms is only ~49 days after the
        // epoch, so any real epoch-ms stamp lands here.
        (seq_validity / 1000).min(u32::MAX as u64) as u32
    }
}

#[cfg(test)]
mod uidvalidity_tests {
    use super::wire_uidvalidity;

    #[test]
    fn small_values_pass_through() {
        assert_eq!(wire_uidvalidity(1), 1);
        assert_eq!(wire_uidvalidity(u32::MAX as u64), u32::MAX);
    }

    #[test]
    fn epoch_millis_become_seconds() {
        // The exact value gco served on 2026-08-25.
        assert_eq!(wire_uidvalidity(1_783_857_640_000), 1_783_857_640);
    }

    #[test]
    fn never_exceeds_u32_and_is_monotonic() {
        assert_eq!(wire_uidvalidity(u64::MAX), u32::MAX);
        assert!(wire_uidvalidity(1_783_857_640_000) < wire_uidvalidity(1_783_857_641_000));
    }
}
