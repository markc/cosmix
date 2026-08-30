//! `LIST` — RFC 9051 §6.3.9.
//!
//! Phase 2 T3 v1: walk the per-account MailboxRecord set,
//! encode names via [`crate::imap::mailbox_name`], filter by the
//! requested pattern, emit one `* LIST (attrs) "/" "wire-name"`
//! response per match plus a final tagged OK.
//!
//! The v1 advertised capability set does NOT include LIST-EXTENDED,
//! LIST-STATUS, or UTF8=ACCEPT, so:
//! * Only positional `reference-name` and `mailbox-name` are parsed.
//!   `RETURN (...)` and `SELECT (...)` parameters are rejected as
//!   `BAD` to keep the wire grammar honest.
//! * `\Subscribed` is not emitted on LIST responses; subscription
//!   state surfaces through `LSUB` only.
//! * Mailbox names that the encoder marks unrepresentable
//!   (non-ASCII, contain `/`, broken parent chain) are omitted from
//!   the result with a `tracing::warn!`; SELECT / STATUS on those
//!   names emits `NO [SERVERBUG] mailbox name not representable
//!   over IMAP` (T4).
//!
//! Argument grammar this handler accepts:
//! ```text
//! LIST <ref-name> <mailbox-name-pattern>
//! ```
//! where each token is an atom or quoted-string (literals are
//! already rejected at the codec layer). The reference name is
//! ignored in v1 unless non-empty, in which case it returns
//! `BAD non-empty LIST reference not supported in this phase`.

use std::sync::Arc;

use crate::imap::mailbox_name::{encode_all, matches_pattern, render_mailbox_line};
use crate::imap::response::{Status, tagged};
use crate::mailstore::MailStore;

type AccountId = i32;

/// Parsed LIST/LSUB positional arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub reference: String,
    pub pattern: String,
}

/// Parse the `reference mailbox-pattern` argument pair shared by
/// `LIST` and `LSUB`. Exposed at module scope so `op::lsub` can
/// reuse the exact same wire grammar without duplicating the
/// parser.
pub fn parse_args(args: &str) -> Result<Args, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("LIST requires reference and mailbox pattern".to_string());
    }
    let (reference, rest) = take_mailbox_token(s)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Err("LIST requires a mailbox pattern".to_string());
    }
    let (pattern, tail) = take_mailbox_token(rest)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err(
            "LIST extension parameters (RETURN/SELECT) not supported in this phase".to_string(),
        );
    }
    Ok(Args { reference, pattern })
}

/// Like [`take_mailbox_token`] but applies the stricter
/// `mailbox = astring` grammar (RFC 9051 §9). When the token is
/// quoted, defers to [`take_mailbox_token`]. When the token is an
/// unquoted atom, rejects IMAP `atom-specials` — `(` `)` `{` `"` `\`
/// `]` `%` `*` and any control byte — so a glued extension-parameter
/// block like `SELECT INBOX(CONDSTORE)` does not sneak through the
/// space-only tokeniser as one giant atom.
///
/// LIST / LSUB intentionally do *not* use this entry point because
/// their pattern argument legitimately contains the list-wildcards
/// `%` and `*` unquoted; they keep using [`take_mailbox_token`].
pub fn take_mailbox_name(s: &str) -> Result<(String, &str), String> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err("missing mailbox name".to_string());
    }
    if bytes[0] == b'"' {
        return take_mailbox_token(s);
    }
    let end = bytes.iter().position(|b| *b == b' ').unwrap_or(bytes.len());
    let atom = &s[..end];
    for b in atom.bytes() {
        let is_special = matches!(b, b'(' | b')' | b'{' | b'"' | b'\\' | b']' | b'%' | b'*')
            || b < 0x20
            || b == 0x7f;
        if is_special {
            return Err(format!(
                "mailbox name contains IMAP atom-special byte 0x{b:02x}; quote the name"
            ));
        }
    }
    Ok((atom.to_string(), &s[end..]))
}

/// Parse one atom or quoted-string from the head of `s`. Returns
/// `(value, remainder)`. Public so `op::lsub` reuses it.
pub fn take_mailbox_token(s: &str) -> Result<(String, &str), String> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err("missing argument".to_string());
    }
    if bytes[0] == b'"' {
        let mut out = String::new();
        let mut i = 1;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                i += 1;
                if i >= bytes.len() {
                    return Err("unterminated quoted-string escape".to_string());
                }
                let nc = bytes[i];
                if nc != b'"' && nc != b'\\' {
                    return Err("invalid quoted-string escape".to_string());
                }
                out.push(nc as char);
                i += 1;
            } else if c == b'"' {
                return Ok((out, &s[i + 1..]));
            } else {
                out.push(c as char);
                i += 1;
            }
        }
        Err("unterminated quoted-string".to_string())
    } else {
        let end = bytes.iter().position(|b| *b == b' ').unwrap_or(bytes.len());
        Ok((s[..end].to_string(), &s[end..]))
    }
}

/// Reject patterns containing non-printable-ASCII or control bytes.
/// The Phase 2 wire is ASCII-only (no UTF8=ACCEPT), so a pattern
/// with non-ASCII or control bytes can never match a wire name the
/// encoder will emit. Returning `BAD` instead of silently producing
/// an empty result set keeps the protocol honest with clients that
/// might otherwise mis-frame their next command. Wildcards `*` and
/// `%` are printable so they fall through the guard.
pub fn pattern_is_ascii_clean(pattern: &str) -> bool {
    pattern.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Execute LIST for an authenticated session.
///
/// Wraps the synchronous `MailStore::list_mailboxes` call in
/// `spawn_blocking` so the per-connection async task does not park
/// a tokio worker on the SQLite read. Mirrors the JMAP
/// `Mailbox/get` / `Mailbox/query` shape.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
) -> String {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };
    if !parsed.reference.is_empty() {
        return tagged(
            tag,
            Status::Bad,
            None,
            "non-empty LIST reference not supported in this phase",
        );
    }
    if !pattern_is_ascii_clean(&parsed.pattern) {
        return tagged(
            tag,
            Status::Bad,
            None,
            "non-ASCII or control bytes in LIST pattern (UTF8=ACCEPT not advertised)",
        );
    }
    // Hierarchy-delimiter probe: `LIST "" ""` per RFC 9051 §6.3.9
    // returns one row carrying the root delimiter and `\Noselect`.
    if parsed.pattern.is_empty() {
        let mut out = "* LIST (\\Noselect) \"/\" \"\"\r\n".to_string();
        out.push_str(&tagged(tag, Status::Ok, None, "LIST completed"));
        return out;
    }
    let records = match tokio::task::spawn_blocking(move || store.list_mailboxes(account_id)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "LIST list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "list failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "LIST spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "list failed");
        }
    };
    let set = encode_all(&records);
    for om in &set.omitted {
        tracing::warn!(
            mailbox_id = ?om.id,
            stored_name = %om.stored_name,
            reason = ?om.reason,
            "mailbox omitted from LIST: not representable over IMAP"
        );
    }
    let mut out = String::new();
    for enc in &set.mailboxes {
        if matches_pattern(&enc.wire_name, &parsed.pattern) {
            out.push_str(&render_mailbox_line("LIST", enc));
        }
    }
    out.push_str(&tagged(tag, Status::Ok, None, "LIST completed"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_empty_reference_with_star_pattern() {
        let p = parse_args(r#""" *"#).unwrap();
        assert_eq!(p.reference, "");
        assert_eq!(p.pattern, "*");
    }

    #[test]
    fn parse_args_quoted_pattern_with_spaces_is_one_token() {
        let p = parse_args(r#""" "INBOX/With Space""#).unwrap();
        assert_eq!(p.reference, "");
        assert_eq!(p.pattern, "INBOX/With Space");
    }

    #[test]
    fn parse_args_atom_reference_atom_pattern() {
        let p = parse_args("\"\" INBOX").unwrap();
        assert_eq!(p.reference, "");
        assert_eq!(p.pattern, "INBOX");
    }

    #[test]
    fn parse_args_handles_quoted_escapes() {
        let p = parse_args(r#""" "a\"b\\c""#).unwrap();
        assert_eq!(p.pattern, "a\"b\\c");
    }

    #[test]
    fn parse_args_rejects_missing_pattern() {
        let e = parse_args(r#""""#).unwrap_err();
        assert!(e.contains("mailbox pattern"), "{e}");
    }

    #[test]
    fn parse_args_rejects_trailing_extension_parameters() {
        let e = parse_args(r#""" * RETURN (SUBSCRIBED)"#).unwrap_err();
        assert!(e.contains("not supported"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unterminated_quoted_string() {
        let e = parse_args(r#""" "INBOX"#).unwrap_err();
        assert!(e.contains("unterminated"), "{e}");
    }
}
