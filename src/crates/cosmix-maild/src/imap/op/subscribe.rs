//! `SUBSCRIBE` / `UNSUBSCRIBE` — RFC 9051 §6.3.7 / §6.3.8.
//!
//! Phase 2 T5 v1: resolve the wire mailbox name and call
//! `MailStore::update_mailbox` with `is_subscribed = Some(true|false)`.
//! All other patch fields are `None`, so the update is a single-row
//! attribute toggle.
//!
//! INBOX is allowed to be subscribed and unsubscribed: clients
//! occasionally toggle it, and there is nothing storage-side that
//! makes the role-Inbox container special for the subscription flag.

use std::sync::Arc;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged};
use crate::mailstore::MailStore;

type AccountId = i32;

/// Parse a single mailbox-name argument. Shared by `SUBSCRIBE` and
/// `UNSUBSCRIBE` — same wire grammar.
pub fn parse_args(args: &str) -> Result<String, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("requires a mailbox name".to_string());
    }
    let (name, tail) = list::take_mailbox_name(s)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err("trailing tokens after mailbox name".to_string());
    }
    if name.is_empty() {
        return Err("requires a non-empty mailbox name".to_string());
    }
    Ok(name)
}

/// Execute `SUBSCRIBE` (when `subscribe == true`) or `UNSUBSCRIBE`
/// (when `subscribe == false`) for an authenticated session.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
    subscribe: bool,
) -> String {
    let verb = if subscribe {
        "SUBSCRIBE"
    } else {
        "UNSUBSCRIBE"
    };
    let name = match parse_args(args) {
        Ok(n) => n,
        Err(e) => return tagged(tag, Status::Bad, None, &format!("{verb} {e}")),
    };

    let s = store.clone();
    let records = match tokio::task::spawn_blocking(move || s.list_mailboxes(account_id)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(verb = verb, error = %e, "list_mailboxes failed");
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                &format!("{verb} failed"),
            );
        }
        Err(e) => {
            tracing::warn!(verb = verb, error = %e, "spawn_blocking panicked");
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                &format!("{verb} failed"),
            );
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
    let res = tokio::task::spawn_blocking(move || {
        s2.update_mailbox(account_id, id, None, None, None, None, Some(subscribe))
    })
    .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(verb = verb, error = %e, "update_mailbox spawn_blocking panicked");
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                &format!("{verb} failed"),
            );
        }
    };
    match res {
        Ok(()) => tagged(tag, Status::Ok, None, &format!("{verb} completed")),
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("mailbox not found") {
                return tagged(
                    tag,
                    Status::No,
                    Some("NONEXISTENT"),
                    "mailbox does not exist",
                );
            }
            tracing::warn!(verb = verb, error = %msg, "unexpected storage error");
            tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                &format!("{verb} failed"),
            )
        }
    }
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
        let e = parse_args("a b").unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }
}
