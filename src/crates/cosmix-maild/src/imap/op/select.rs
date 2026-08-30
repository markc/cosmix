//! `SELECT` / `EXAMINE` — RFC 9051 §6.3.1 / §6.3.2.
//!
//! Phase 2 T4 v1: resolve the wire mailbox name through
//! [`crate::imap::mailbox_name::resolve`], read the matching
//! [`MailboxRecord`] off `MailStore::list_mailboxes`, and emit the
//! mandatory untagged response block:
//!
//! ```text
//! * FLAGS (\Answered \Flagged \Deleted \Seen \Draft)
//! * <N> EXISTS
//! * OK [PERMANENTFLAGS (\Answered \Flagged \Deleted \Seen \Draft)] Limited
//! * OK [UIDVALIDITY <n>] Ok
//! * OK [UIDNEXT <n>] Predicted next UID
//! * LIST () "/" "<wire-name>"
//! <tag> OK [READ-WRITE|READ-ONLY] SELECT|EXAMINE completed
//! ```
//!
//! Phase 2 omissions, each tied to a capability that is not yet
//! advertised:
//! * No `* N RECENT` — IMAP4rev2 removed RECENT (§7.3).
//! * No `* OK [HIGHESTMODSEQ ...]` — CONDSTORE is not in the v1
//!   capability subset; emitting the code without the capability
//!   would tell a strict client we implicitly support
//!   `ENABLE CONDSTORE` semantics (RFC 7162 §3.1.2.2).
//! * `PERMANENTFLAGS` includes `\*` (RFC 9051 §7.1) — the server
//!   accepts client-defined keyword *creation* via STORE/APPEND
//!   (normalised to NFC + lowercase; see `crate::keyword`). The five
//!   system flags are advertised alongside it. `FLAGS` lists only the
//!   system flags (it is the "defined flags" line; `\*` is a
//!   PERMANENTFLAGS-only signal).
//!
//! State transition. RFC 9051 §6.3.2 requires a successful or
//! *failed* SELECT/EXAMINE to first put the session back into
//! `Authenticated` — the previously selected mailbox is implicitly
//! closed before the new lookup runs. The session loop in
//! `imap::session` performs that transition before calling
//! [`handle`]; this module only reports what the post-lookup state
//! should be via [`SelectOutcome::selected`].
//!
//! Error mapping:
//! * NotFound → `NO [NONEXISTENT] mailbox does not exist`.
//! * Unrepresentable → `NO [SERVERBUG] mailbox name not
//!   representable over IMAP`.
//! * Storage error → `NO [SERVERBUG] select failed`.

use std::sync::Arc;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged, wire_uidvalidity};
use crate::imap::state::SelectedMailbox;
use crate::mailstore::MailStore;

type AccountId = i32;

/// IMAP4rev2 system flags. Order matches every other handler in
/// this crate so a wire transcript reads the same regardless of
/// which command emitted the list.
const SYSTEM_FLAGS: &str = "\\Answered \\Flagged \\Deleted \\Seen \\Draft";

/// Outcome of [`handle`]. `response` is the raw wire bytes to send;
/// `selected` is the new `Selected(_)` state on success, or `None`
/// to leave the session in `Authenticated`.
pub struct SelectOutcome {
    pub response: String,
    pub selected: Option<SelectedMailbox>,
}

impl SelectOutcome {
    fn fail(tag: &str, code: &str, text: &str) -> Self {
        Self {
            response: tagged(tag, Status::No, Some(code), text),
            selected: None,
        }
    }

    fn bad(tag: &str, text: &str) -> Self {
        Self {
            response: tagged(tag, Status::Bad, None, text),
            selected: None,
        }
    }
}

/// Parse the argument of `SELECT` / `EXAMINE`.
///
/// Phase 2 accepts a single atom or quoted-string. Trailing
/// extension parameters (e.g., `(CONDSTORE)` from RFC 7162 or the
/// QRESYNC parenthesised list from RFC 7162 §3.2.5) are rejected as
/// `BAD` — neither capability is advertised in the v1 subset, so a
/// strict-grammar reject is honest rather than silently ignoring
/// what the client asked for.
pub fn parse_args(args: &str) -> Result<String, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("SELECT requires a mailbox name".to_string());
    }
    let (name, tail) = list::take_mailbox_name(s)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err(
            "SELECT/EXAMINE parameters (CONDSTORE/QRESYNC) not supported in this phase".to_string(),
        );
    }
    if name.is_empty() {
        return Err("SELECT requires a non-empty mailbox name".to_string());
    }
    Ok(name)
}

/// Execute `SELECT` (when `read_only == false`) or `EXAMINE`
/// (when `read_only == true`) for an authenticated session.
///
/// The session loop is responsible for the "implicit close on
/// every SELECT/EXAMINE" transition — see module-level docs.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
    read_only: bool,
) -> SelectOutcome {
    let name = match parse_args(args) {
        Ok(n) => n,
        Err(e) => return SelectOutcome::bad(tag, &e),
    };

    let store_for_list = store.clone();
    let records = match tokio::task::spawn_blocking(move || {
        store_for_list.list_mailboxes(account_id)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "SELECT list_mailboxes failed");
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "SELECT spawn_blocking panicked");
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
    };

    let resolved_id = match resolve(&name, &records) {
        ResolveResult::Found(id) => id,
        ResolveResult::NotFound => {
            return SelectOutcome::fail(tag, "NONEXISTENT", "mailbox does not exist");
        }
        ResolveResult::Unrepresentable => {
            return SelectOutcome::fail(
                tag,
                "SERVERBUG",
                "mailbox name not representable over IMAP",
            );
        }
    };

    // `resolve` is a pure function over `records`; the matching record
    // must still be in the slice we just pulled from the store.
    let rec = match records.iter().find(|r| r.id == resolved_id) {
        Some(r) => r,
        None => {
            tracing::error!(
                resolved_id = ?resolved_id,
                "SELECT resolve returned id absent from list_mailboxes (storage race?)"
            );
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
    };

    // Re-derive the canonical wire name from the encoded set so the
    // `SelectedMailbox.name` field and the mandatory `* LIST` response
    // both carry the case-folded `INBOX` rather than whatever spelling
    // the client used. If the encoder omits the record after `resolve`
    // returned `Found` — an encoder/resolve drift that this code does
    // not silently paper over — fail with NO [SERVERBUG] so the
    // operator sees the inconsistency rather than the wire emitting
    // an unrepresentable stored name as if it were valid.
    let wire_name = match wire_name_for(resolved_id, &records) {
        Some(n) => n,
        None => {
            tracing::error!(
                resolved_id = ?resolved_id,
                "SELECT could not re-encode wire name for resolved record (encoder/resolve drift)"
            );
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
    };

    let uid_validity = rec.uid_validity;
    let uid_next = rec.uid_next;
    let highest_mod_seq = rec.highest_mod_seq;

    // Snapshot the UID list at SELECT/EXAMINE time. This becomes
    // the session's record of "what the client knows about this
    // mailbox" — kept in lockstep with subsequent EXPUNGE / MOVE-
    // out emissions, and used by IDLE to compute the catch-up
    // diff for changes that occurred between SELECT and IDLE.
    //
    // EXISTS on the wire and `SelectedMailbox.exists` BOTH derive
    // from `view_uids.len()` — not from `rec.total_emails`. The
    // record's count was sampled during `list_mailboxes`, a
    // *separate* substrate read from the UID list below; a
    // concurrent APPEND or EXPUNGE between those reads would make
    // the EXISTS count disagree with `view_uids`, and the IDLE
    // entry-diff would never reconcile that initial skew. Using a
    // single snapshot for both keeps "what the client was told"
    // and "what the session believes the client was told"
    // coherent.
    let store_for_uids = store.clone();
    let view_uids = match tokio::task::spawn_blocking(move || {
        store_for_uids.list_emails_in_mailbox(
            account_id,
            resolved_id,
            crate::mailstore::ListOpts {
                sort_by: crate::mailstore::SortKey::Seq,
                limit: u32::MAX,
                offset: 0,
            },
        )
    })
    .await
    {
        Ok(Ok(handles)) => handles.into_iter().map(|h| h.uid).collect::<Vec<u64>>(),
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                "SELECT view_uids snapshot failed",
            );
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "SELECT view_uids spawn_blocking panicked");
            return SelectOutcome::fail(tag, "SERVERBUG", "select failed");
        }
    };
    let exists = view_uids.len() as u64;

    let mut out = String::new();
    out.push_str(&format!("* FLAGS ({SYSTEM_FLAGS})\r\n"));
    out.push_str(&format!("* {exists} EXISTS\r\n"));
    // `\*` (RFC 9051 §7.1) advertises that STORE/APPEND may create new
    // client-defined keywords — distinct from the `* FLAGS` line, which lists
    // only the defined system flags.
    out.push_str(&format!(
        "* OK [PERMANENTFLAGS ({SYSTEM_FLAGS} \\*)] Limited\r\n"
    ));
    // 32-bit on the wire (RFC 9051 nz-number); the stored value is epoch-ms.
    let wire_validity = wire_uidvalidity(uid_validity);
    out.push_str(&format!("* OK [UIDVALIDITY {wire_validity}] Ok\r\n"));
    out.push_str(&format!("* OK [UIDNEXT {uid_next}] Predicted next UID\r\n"));
    out.push_str(&format!(
        "* LIST () \"/\" {}\r\n",
        crate::imap::mailbox_name::quote_mailbox(&wire_name),
    ));

    let (code, text) = if read_only {
        ("READ-ONLY", "EXAMINE completed")
    } else {
        ("READ-WRITE", "SELECT completed")
    };
    out.push_str(&tagged(tag, Status::Ok, Some(code), text));

    let selected = SelectedMailbox {
        container_id: resolved_id,
        name: wire_name,
        uid_validity,
        uid_next,
        exists,
        highest_mod_seq,
        read_only,
        view_uids,
    };

    SelectOutcome {
        response: out,
        selected: Some(selected),
    }
}

/// Look up the encoded wire name for an already-resolved
/// [`ContainerId`]. Returns `None` if the encoder omitted the
/// matching row — `resolve` succeeded but `encode_all` can omit
/// rows whose stored name is non-ASCII / contains the delimiter,
/// in which case `resolve` would have returned `Unrepresentable`
/// upstream.
fn wire_name_for(
    id: cosmix_mds::ContainerId,
    records: &[crate::mailstore::MailboxRecord],
) -> Option<String> {
    let set = crate::imap::mailbox_name::encode_all(records);
    set.mailboxes
        .into_iter()
        .find(|enc| enc.id == id)
        .map(|enc| enc.wire_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_accepts_atom() {
        assert_eq!(parse_args("INBOX").unwrap(), "INBOX");
    }

    #[test]
    fn parse_args_accepts_quoted() {
        assert_eq!(parse_args(r#""INBOX""#).unwrap(), "INBOX");
    }

    #[test]
    fn parse_args_accepts_quoted_with_space() {
        assert_eq!(
            parse_args(r#""Archive/2025 plans""#).unwrap(),
            "Archive/2025 plans"
        );
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("").is_err());
    }

    #[test]
    fn parse_args_rejects_empty_quoted_string() {
        let e = parse_args(r#""""#).unwrap_err();
        assert!(e.contains("non-empty"), "{e}");
    }

    #[test]
    fn parse_args_rejects_trailing_extension_parameter() {
        let e = parse_args("INBOX (CONDSTORE)").unwrap_err();
        assert!(e.contains("not supported"), "{e}");
    }
}
