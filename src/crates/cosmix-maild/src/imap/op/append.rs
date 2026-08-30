//! `APPEND` — RFC 9051 §6.3.12 / RFC 4315 (UIDPLUS APPENDUID).
//!
//! Phase 4 T14 ships APPEND with a single trailing literal (sync or
//! non-sync per RFC 7888 `LITERAL+`). The codec ([`crate::imap::codec`])
//! has already consumed the literal body by the time this handler
//! runs; we only see the parsed command line plus `literal: Vec<u8>`.
//!
//! Wire grammar (after the verb is stripped):
//!
//! ```text
//! args        = mailbox [SP "(" flag-list ")"] [SP date-time]
//! mailbox     = atom / quoted-string
//! date-time   = '"' DD "-" Mmm "-" YYYY SP HH ":" MM ":" SS SP ("+"/"-") ZHHMM '"'
//! flag-list   = flag *(SP flag)
//! flag        = "\Answered" / "\Flagged" / "\Deleted" / "\Seen" / "\Draft"
//! ```
//!
//! The literal itself is stripped from the line by the codec, so the
//! grammar here is the head only.
//!
//! ## Output
//!
//! On success, RFC 4315 §3 mandates:
//!
//! ```text
//! <tag> OK [APPENDUID <uidvalidity> <uid>] APPEND completed
//! ```
//!
//! where `<uidvalidity>` and `<uid>` come from the [`MembershipUid`]
//! returned by [`crate::mailstore::MailStore::create_email`] — both
//! read inside the same `with_set_tx` that committed the membership,
//! so an APPENDUID for a freshly-inserted membership cannot diverge
//! from the actual stored row.
//!
//! ## Error mapping
//!
//! | Failure                                | Response                                  |
//! |----------------------------------------|-------------------------------------------|
//! | grammar parse                          | `BAD <reason>`                            |
//! | mailbox does not exist                 | `NO [TRYCREATE] mailbox does not exist`   |
//! | mailbox name unrepresentable           | `NO [SERVERBUG] mailbox not representable`|
//! | mail_parser rejects the literal        | `NO [PARSE] message body not parseable`   |
//! | substrate `blob not found`             | `NO [SERVERBUG] storage error`            |
//! | substrate other error                  | `NO [SERVERBUG] APPEND failed`            |
//!
//! `TRYCREATE` is RFC 9051 §7.1's hint that the client should retry
//! after `CREATE`-ing the mailbox; Thunderbird honours it on first-
//! send for an unseen Sent / Drafts.

use std::sync::Arc;

use cosmix_mds::{Flags, Tags};
use mail_parser::MessageParser;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged, wire_uidvalidity};
use crate::jmap::email::{build_envelope, extract_in_reply_to};
use crate::mailstore::MailStore;

type AccountId = i32;

/// Parsed APPEND head (everything before the literal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppendReq {
    mailbox: String,
    flags: Flags,
    /// User keywords (bare atoms) from the flag-list, normalised.
    tags: Tags,
    /// Parsed `INTERNALDATE` in unix-millis, or `None` if absent (the
    /// handler then defaults to "now").
    received_at_ms: Option<i64>,
}

/// Parse the APPEND argument head — mailbox, optional flag-list, and
/// optional date-time. The literal has already been stripped by the
/// codec, so `args` is everything between the verb and the (now
/// removed) literal marker. Trailing whitespace is tolerated.
fn parse_args(args: &str) -> Result<AppendReq, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("APPEND requires a mailbox name".to_string());
    }

    let (mailbox, mut rest) = list::take_mailbox_name(s)?;
    if mailbox.is_empty() {
        return Err("APPEND requires a non-empty mailbox name".to_string());
    }
    rest = rest.trim_start();

    // Optional flag-list.
    let mut flags = Flags(0);
    let mut tags = Tags::new();
    if rest.starts_with('(') {
        let close = rest
            .find(')')
            .ok_or_else(|| "APPEND flag-list missing closing paren".to_string())?;
        let inner = &rest[1..close];
        let (f, t) = parse_flag_list(inner)?;
        flags = f;
        tags = t;
        rest = rest[close + 1..].trim_start();
    }

    // Optional date-time (quoted string).
    let received_at_ms = if rest.starts_with('"') {
        let close = rest[1..]
            .find('"')
            .ok_or_else(|| "APPEND date-time missing closing quote".to_string())?;
        let dt = &rest[1..1 + close];
        let parsed = parse_internaldate(dt)?;
        rest = rest[1 + close + 1..].trim_start();
        Some(parsed)
    } else {
        None
    };

    if !rest.is_empty() {
        return Err(format!("APPEND: unexpected trailing argument {:?}", rest));
    }

    Ok(AppendReq {
        mailbox,
        flags,
        tags,
        received_at_ms,
    })
}

/// Pre-validate so grammar failures bump the bad-command counter at
/// dispatch time. The handler re-parses on its own path; the second
/// parse is pure.
pub fn parse_args_for_dispatch(args: &str) -> Result<(), String> {
    parse_args(args).map(|_| ())
}

/// Parse the inner of an APPEND `<flag-list>` (parens already stripped by the
/// caller) into system flags + user keywords, via the shared [`super::flags`]
/// implementation STORE / FETCH also use.
fn parse_flag_list(s: &str) -> Result<(Flags, Tags), String> {
    super::flags::parse_flag_list_inner(s).map_err(|e| format!("APPEND: {e}"))
}

/// Parse an IMAP `date-time` — `"DD-Mmm-YYYY HH:MM:SS +ZZZZ"` — into
/// unix milliseconds. Returns `Err` on any structural deviation;
/// callers map to `BAD`. Mirrors the `chrono::DateTime::parse_from_str`
/// shape used elsewhere in the crate.
fn parse_internaldate(s: &str) -> Result<i64, String> {
    // `%d-%b-%Y %H:%M:%S %z` accepts e.g. `01-Jan-2026 10:00:00 +0000`.
    // chrono is case-insensitive on month names.
    let dt = chrono::DateTime::parse_from_str(s, "%d-%b-%Y %H:%M:%S %z")
        .map_err(|e| format!("APPEND: invalid date-time {s:?}: {e}"))?;
    Ok(dt.timestamp_millis())
}

/// Execute APPEND for an authenticated session.
///
/// The codec has already consumed the literal body, so `body` carries
/// the exact N bytes that landed on the wire. The mailbox resolution
/// runs against the freshest `list_mailboxes` snapshot — APPEND has
/// no SELECTED-state pre-condition, just AUTHENTICATED.
pub async fn handle(
    tag: &str,
    args: &str,
    body: Vec<u8>,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
) -> String {
    let req = match parse_args(args) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };

    // Parse the message body in foreground — `mail_parser` is CPU-
    // bound, but typical messages are < 1 MiB. spawn_blocking would
    // pay a worker hop for sub-millisecond work; SMTP inbound uses
    // foreground parsing for the same reason.
    let parser = MessageParser::default();
    let parsed = match parser.parse(&body) {
        Some(m) => m,
        None => return tagged(tag, Status::No, Some("PARSE"), "message body not parseable"),
    };
    let envelope = build_envelope(&parsed);
    let in_reply_to = extract_in_reply_to(&parsed);
    drop(parsed); // `parsed` borrows `body`; drop the borrow before move.

    let received_at_ms = req
        .received_at_ms
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    // Resolve mailbox via wire-name → ContainerId, on the freshest
    // list_mailboxes snapshot.
    let store_for_list = store.clone();
    let records = match tokio::task::spawn_blocking(move || {
        store_for_list.list_mailboxes(account_id)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "APPEND list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "APPEND failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "APPEND list_mailboxes spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "APPEND failed");
        }
    };

    let container_id = match resolve(&req.mailbox, &records) {
        ResolveResult::Found(id) => id,
        ResolveResult::NotFound => {
            return tagged(tag, Status::No, Some("TRYCREATE"), "mailbox does not exist");
        }
        ResolveResult::Unrepresentable => {
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                "mailbox name not representable over IMAP",
            );
        }
    };

    // Land bytes in CAS + create the email row inside one
    // spawn_blocking, mirroring smtp::inbound's local-delivery shape.
    let store_for_create = store.clone();
    let body_for_create = body;
    let flags = req.flags;
    let tags = req.tags;
    let create_outcome = tokio::task::spawn_blocking(move || {
        let blob_hash = store_for_create.put_blob(&body_for_create)?;
        store_for_create.create_email(
            account_id,
            container_id,
            blob_hash,
            envelope,
            &in_reply_to,
            flags,
            // APPEND user keywords from the flag-list, persisted with the
            // new item in the same create transaction.
            tags,
            received_at_ms,
        )
    })
    .await;

    let membership = match create_outcome {
        Ok(Ok((_item, uid))) => uid,
        Ok(Err(e)) => {
            let msg = e.to_string();
            // Substrate-level `mailbox not found` after we resolved
            // would be a TOCTOU race against a DELETE in another
            // session — surface as TRYCREATE rather than SERVERBUG so
            // the client retries with CREATE.
            if msg.starts_with("mailbox not found") {
                return tagged(tag, Status::No, Some("TRYCREATE"), "mailbox does not exist");
            }
            tracing::warn!(error = %msg, "APPEND create_email failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "APPEND failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "APPEND spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "APPEND failed");
        }
    };

    let code = format!(
        "APPENDUID {} {}",
        wire_uidvalidity(membership.uid_validity),
        membership.uid
    );
    tagged(tag, Status::Ok, Some(&code), "APPEND completed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_minimal() {
        let r = parse_args("INBOX").unwrap();
        assert_eq!(r.mailbox, "INBOX");
        assert_eq!(r.flags.0, 0);
        assert!(r.received_at_ms.is_none());
    }

    #[test]
    fn parse_args_quoted_mailbox() {
        let r = parse_args(r#""Sent Items""#).unwrap();
        assert_eq!(r.mailbox, "Sent Items");
    }

    #[test]
    fn parse_args_with_flags() {
        let r = parse_args(r"INBOX (\Seen \Draft)").unwrap();
        assert_eq!(r.flags.0, Flags::SEEN | Flags::DRAFT);
        assert!(r.received_at_ms.is_none());
    }

    #[test]
    fn parse_args_with_date_time() {
        let r = parse_args(r#"INBOX "01-Jan-2026 10:00:00 +0000""#).unwrap();
        assert!(r.received_at_ms.is_some());
    }

    #[test]
    fn parse_args_with_flags_and_date_time() {
        let r = parse_args(r#"INBOX (\Seen) "01-Jan-2026 10:00:00 +1000""#).unwrap();
        assert_eq!(r.flags.0, Flags::SEEN);
        assert!(r.received_at_ms.is_some());
    }

    #[test]
    fn parse_args_empty_flag_list_is_zero() {
        let r = parse_args("INBOX ()").unwrap();
        assert_eq!(r.flags.0, 0);
    }

    #[test]
    fn parse_args_case_insensitive_flag_names() {
        let r = parse_args(r"INBOX (\seen \DRAFT)").unwrap();
        assert_eq!(r.flags.0, Flags::SEEN | Flags::DRAFT);
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("").is_err());
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let e = parse_args(r"INBOX (\Bogus)").unwrap_err();
        assert!(e.contains("unknown system flag"), "{e}");
    }

    #[test]
    fn parse_args_rejects_trailing_junk() {
        let e = parse_args(r"INBOX trailing").unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unclosed_flag_list() {
        let e = parse_args(r"INBOX (\Seen").unwrap_err();
        assert!(e.contains("closing paren"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unclosed_date_time() {
        let e = parse_args(r#"INBOX "01-Jan-2026"#).unwrap_err();
        assert!(e.contains("closing quote"), "{e}");
    }

    #[test]
    fn parse_args_rejects_bad_date_time() {
        let e = parse_args(r#"INBOX "not-a-date""#).unwrap_err();
        assert!(e.contains("invalid date-time"), "{e}");
    }
}
