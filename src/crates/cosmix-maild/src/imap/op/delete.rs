//! `DELETE` — RFC 9051 §6.3.5.
//!
//! Phase 2 T5 v1: resolve the wire mailbox name and call
//! `MailStore::destroy_mailbox` with [`DestroyPolicy::Reject`]. The
//! locked Phase 2 design refuses to auto-purge messages — STORE and
//! EXPUNGE ship in Phase 4, so a non-empty mailbox cannot be
//! deleted yet through this verb.
//!
//! Special cases:
//! * `DELETE INBOX` is rejected up-front with `NO [CANNOT] INBOX
//!   cannot be deleted` for the clearer wire message; the storage
//!   layer would also refuse via the `"mailbox has role"` guard, but
//!   the IMAP-side message is more accurate (it's INBOX, not
//!   "mailbox with role").
//!
//! Error mapping (storage error prefix → wire response):
//! * `"mailbox not found"` → `NO [NONEXISTENT] mailbox does not exist`
//!   (race against a concurrent destroy).
//! * `"mailbox not empty"` → `NO [CANNOT] mailbox has messages`.
//! * `"mailbox has children"` → `NO [CANNOT] mailbox has child mailboxes`.
//! * `"mailbox has role"` → `NO [CANNOT] cannot delete mailbox with
//!   a special-use role`.
//! * `"reserved container"` / `"mailbox conflicting policy"` →
//!   `NO [SERVERBUG]` with logging; neither should be reachable via
//!   wire under the `Reject` policy.

use std::sync::Arc;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged};
use crate::mailstore::{DestroyPolicy, MailStore};

type AccountId = i32;

/// Parse `DELETE <mailbox-name>` — single atom or quoted-string.
pub fn parse_args(args: &str) -> Result<String, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("DELETE requires a mailbox name".to_string());
    }
    let (name, tail) = list::take_mailbox_name(s)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err("DELETE trailing tokens after mailbox name".to_string());
    }
    if name.is_empty() {
        return Err("DELETE requires a non-empty mailbox name".to_string());
    }
    Ok(name)
}

/// Execute DELETE for an authenticated session.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
) -> String {
    let name = match parse_args(args) {
        Ok(n) => n,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };

    // INBOX is locked out at the IMAP layer for a clearer wire message
    // than the storage `"mailbox has role"` guard would produce.
    if name.eq_ignore_ascii_case("INBOX") {
        return tagged(tag, Status::No, Some("CANNOT"), "INBOX cannot be deleted");
    }

    let s = store.clone();
    let records = match tokio::task::spawn_blocking(move || s.list_mailboxes(account_id)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "DELETE list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "delete failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "DELETE spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "delete failed");
        }
    };

    let id = match resolve(&name, &records) {
        ResolveResult::Found(id) => id,
        ResolveResult::NotFound => {
            return tagged(
                tag,
                Status::No,
                Some("NONEXISTENT"),
                "mailbox does not exist",
            );
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

    let s2 = store.clone();
    let destroy = tokio::task::spawn_blocking(move || {
        s2.destroy_mailbox(account_id, id, DestroyPolicy::Reject)
    })
    .await;
    let destroy = match destroy {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "DELETE destroy_mailbox spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "delete failed");
        }
    };
    match destroy {
        Ok(()) => tagged(tag, Status::Ok, None, "DELETE completed"),
        Err(e) => map_destroy_error(tag, &e.to_string()),
    }
}

fn map_destroy_error(tag: &str, msg: &str) -> String {
    if msg.starts_with("mailbox not found") {
        return tagged(
            tag,
            Status::No,
            Some("NONEXISTENT"),
            "mailbox does not exist",
        );
    }
    if msg.starts_with("mailbox not empty") {
        return tagged(tag, Status::No, Some("CANNOT"), "mailbox has messages");
    }
    if msg.starts_with("mailbox has children") {
        return tagged(
            tag,
            Status::No,
            Some("CANNOT"),
            "mailbox has child mailboxes",
        );
    }
    if msg.starts_with("mailbox has role") {
        return tagged(
            tag,
            Status::No,
            Some("CANNOT"),
            "cannot delete mailbox with a special-use role",
        );
    }
    tracing::warn!(error = %msg, "DELETE unexpected storage error");
    tagged(tag, Status::No, Some("SERVERBUG"), "delete failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_atom() {
        assert_eq!(parse_args("Archive").unwrap(), "Archive");
    }

    #[test]
    fn parse_args_quoted() {
        assert_eq!(parse_args(r#""Archive 2025""#).unwrap(), "Archive 2025");
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("").is_err());
    }

    #[test]
    fn parse_args_rejects_trailing_tokens() {
        let e = parse_args("Archive extra").unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }
}
