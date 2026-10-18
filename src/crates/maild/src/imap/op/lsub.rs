//! `LSUB` — RFC 9051 §6.3.10 (preserved for legacy clients).
//!
//! Same pipeline as [`crate::imap::op::list`] but filtered to rows
//! whose [`crate::mailstore::MailboxRecord::is_subscribed`] flag is
//! set, and emitted with the `LSUB` response keyword instead of
//! `LIST`. The v1 capability subset advertises subscription state
//! exclusively via LSUB — LIST-EXTENDED is not in the subset, so
//! the alternative `LIST ... RETURN (SUBSCRIBED)` path is not yet
//! a wire option.
//!
//! Argument grammar is identical to LIST; we reuse
//! [`crate::imap::op::list::parse_args`] verbatim rather than
//! re-parse here.

use std::sync::Arc;

use crate::imap::mailbox_name::{encode_all, matches_pattern, render_mailbox_line};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged};
use crate::mailstore::MailStore;

type AccountId = i32;

/// Execute LSUB for an authenticated session.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
) -> String {
    let parsed = match list::parse_args(args) {
        Ok(p) => p,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };
    if !parsed.reference.is_empty() {
        return tagged(
            tag,
            Status::Bad,
            None,
            "non-empty LSUB reference not supported in this phase",
        );
    }
    if !list::pattern_is_ascii_clean(&parsed.pattern) {
        return tagged(
            tag,
            Status::Bad,
            None,
            "non-ASCII or control bytes in LSUB pattern (UTF8=ACCEPT not advertised)",
        );
    }
    // Empty pattern: per RFC 9051 LSUB returns no rows (the
    // delimiter probe lives on LIST). Emit just the tagged OK.
    if parsed.pattern.is_empty() {
        return tagged(tag, Status::Ok, None, "LSUB completed");
    }
    let records = match tokio::task::spawn_blocking(move || store.list_mailboxes(account_id)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "LSUB list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "lsub failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "LSUB spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "lsub failed");
        }
    };
    let set = encode_all(&records);
    for om in &set.omitted {
        tracing::warn!(
            mailbox_id = ?om.id,
            stored_name = %om.stored_name,
            reason = ?om.reason,
            "mailbox omitted from LSUB: not representable over IMAP"
        );
    }
    let mut out = String::new();
    for enc in &set.mailboxes {
        if !enc.is_subscribed {
            continue;
        }
        if !matches_pattern(&enc.wire_name, &parsed.pattern) {
            continue;
        }
        out.push_str(&render_mailbox_line("LSUB", enc));
    }
    out.push_str(&tagged(tag, Status::Ok, None, "LSUB completed"));
    out
}
