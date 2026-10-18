//! `RENAME` — RFC 9051 §6.3.6.
//!
//! Phase 2 T5 v1: resolve the old wire name, validate every segment
//! of the new name, look up the destination parent (must already
//! exist), and call `MailStore::rename_mailbox`.
//!
//! Locked Phase 2 decisions (with Codex sign-off):
//! * **INBOX rename is rejected** with `NO [CANNOT] INBOX cannot be
//!   renamed`. RFC 9051 §6.3.6 actually requires INBOX-rename to
//!   move messages from INBOX into the new mailbox and leave INBOX
//!   empty — that behaviour depends on STORE / MOVE landing in
//!   Phase 4, so v1 refuses outright rather than implementing it
//!   half-way.
//! * **Destination parent must exist.** Unlike `CREATE`, RENAME does
//!   **not** auto-create intermediate segments. RFC's auto-create
//!   language is on CREATE only; RENAME silently materialising the
//!   parent tree would mask client mistakes and could leave orphans
//!   if the final rename then fails on cycle/already-exists/role.
//!   A missing parent path surfaces as `NO [NONEXISTENT] parent
//!   mailbox does not exist`.
//! * **Pre-resolve the destination.** If the full new wire name
//!   already resolves to a different container, return
//!   `NO [ALREADYEXISTS]` up-front so the client sees the same
//!   diagnostic whether the conflict is detected here or by the
//!   storage layer.
//!
//! Segment validation mirrors `CREATE` — non-empty, printable ASCII,
//! no `&` (RFC 3501 modified-UTF-7 introducer), not the reserved
//! `__upload_staging__` name. Renaming **to** literal `INBOX` (any
//! case) is also rejected: it would shadow the role-Inbox container
//! on the wire.

use std::sync::Arc;

use cosmix_mds::ContainerId;

use crate::imap::mailbox_name::{HIERARCHY_DELIMITER, ResolveResult, resolve};
use crate::imap::op::create::validate_segment;
use crate::imap::op::list;
use crate::imap::response::{Status, tagged};
use crate::mailstore::MailStore;

type AccountId = i32;

/// Parsed `RENAME` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub old_name: String,
    pub new_name: String,
}

/// Parse `RENAME <old> <new>` — two atoms or quoted-strings.
pub fn parse_args(args: &str) -> Result<Args, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("RENAME requires <old> <new> mailbox names".to_string());
    }
    let (old_name, rest) = list::take_mailbox_name(s)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Err("RENAME requires a new mailbox name".to_string());
    }
    let (new_name, tail) = list::take_mailbox_name(rest)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err("RENAME trailing tokens after new mailbox name".to_string());
    }
    if old_name.is_empty() || new_name.is_empty() {
        return Err("RENAME requires non-empty mailbox names".to_string());
    }
    Ok(Args { old_name, new_name })
}

/// Execute RENAME for an authenticated session.
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

    // INBOX rename is deferred to Phase 4.
    if parsed.old_name.eq_ignore_ascii_case("INBOX") {
        return tagged(tag, Status::No, Some("CANNOT"), "INBOX cannot be renamed");
    }

    // A trailing delimiter on the destination is meaningless for
    // RENAME (no parent-ensure semantic exists here) — reject rather
    // than silently strip and risk a "what did you really mean"
    // ambiguity with the auto-create-parents stance.
    let new_name = parsed.new_name.as_str();
    if new_name.ends_with(HIERARCHY_DELIMITER) {
        return tagged(
            tag,
            Status::No,
            Some("CANNOT"),
            "RENAME destination must not end with the hierarchy delimiter",
        );
    }

    let new_segments: Vec<&str> = new_name.split(HIERARCHY_DELIMITER).collect();
    for seg in &new_segments {
        if let Err(e) = validate_segment(seg) {
            return tagged(tag, Status::No, Some("CANNOT"), &e);
        }
    }
    // Reject renaming TO literal INBOX — it would shadow the
    // role-Inbox container on the wire.
    if new_segments.len() == 1 && new_segments[0].eq_ignore_ascii_case("INBOX") {
        return tagged(
            tag,
            Status::No,
            Some("CANNOT"),
            "cannot rename to reserved name INBOX",
        );
    }

    let s = store.clone();
    let records = match tokio::task::spawn_blocking(move || s.list_mailboxes(account_id)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "RENAME list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "rename failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "RENAME spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "rename failed");
        }
    };

    // Resolve the source.
    let old_id = match resolve(&parsed.old_name, &records) {
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

    // Pre-resolve the full destination — duplicate name fast-fail.
    match resolve(new_name, &records) {
        ResolveResult::Found(existing) if existing != old_id => {
            return tagged(
                tag,
                Status::No,
                Some("ALREADYEXISTS"),
                "target mailbox already exists",
            );
        }
        ResolveResult::Unrepresentable => {
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                "destination mailbox name not representable over IMAP",
            );
        }
        _ => {}
    }

    // Resolve the destination parent. Empty parent path = root.
    let (parent_segments, final_segment) = match new_segments.split_last() {
        Some((last, rest)) => (rest, *last),
        None => {
            // split() on a non-empty string yields at least one entry,
            // so this branch is unreachable; guarded for the type-system.
            return tagged(tag, Status::No, Some("SERVERBUG"), "rename failed");
        }
    };
    let new_parent: Option<ContainerId> = if parent_segments.is_empty() {
        None
    } else {
        let parent_path = parent_segments.join(&HIERARCHY_DELIMITER.to_string());
        match resolve(&parent_path, &records) {
            ResolveResult::Found(id) => Some(id),
            ResolveResult::NotFound => {
                return tagged(
                    tag,
                    Status::No,
                    Some("NONEXISTENT"),
                    "parent mailbox does not exist",
                );
            }
            ResolveResult::Unrepresentable => {
                return tagged(
                    tag,
                    Status::No,
                    Some("SERVERBUG"),
                    "parent mailbox name not representable over IMAP",
                );
            }
        }
    };

    let final_owned = final_segment.to_string();
    let s2 = store.clone();
    let rename = tokio::task::spawn_blocking(move || {
        s2.rename_mailbox(account_id, old_id, &final_owned, new_parent)
    })
    .await;
    let rename = match rename {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "RENAME spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "rename failed");
        }
    };
    match rename {
        Ok(()) => tagged(tag, Status::Ok, None, "RENAME completed"),
        Err(e) => map_rename_error(tag, &e.to_string()),
    }
}

fn map_rename_error(tag: &str, msg: &str) -> String {
    if msg.starts_with("mailbox not found") {
        return tagged(
            tag,
            Status::No,
            Some("NONEXISTENT"),
            "mailbox does not exist",
        );
    }
    if msg.starts_with("mailbox already exists") {
        return tagged(
            tag,
            Status::No,
            Some("ALREADYEXISTS"),
            "target mailbox already exists",
        );
    }
    if msg.starts_with("mailbox parent not found") {
        return tagged(
            tag,
            Status::No,
            Some("NONEXISTENT"),
            "parent mailbox does not exist",
        );
    }
    if msg.starts_with("mailbox cycle") {
        return tagged(
            tag,
            Status::No,
            Some("CANNOT"),
            "rename would create a hierarchy cycle",
        );
    }
    if msg.starts_with("reserved name") || msg.starts_with("mailbox invalid name") {
        return tagged(tag, Status::No, Some("CANNOT"), msg);
    }
    tracing::warn!(error = %msg, "RENAME unexpected storage error");
    tagged(tag, Status::No, Some("SERVERBUG"), "rename failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_two_atoms() {
        let p = parse_args("OldName NewName").unwrap();
        assert_eq!(p.old_name, "OldName");
        assert_eq!(p.new_name, "NewName");
    }

    #[test]
    fn parse_args_quoted_with_spaces() {
        let p = parse_args(r#""Old Name" "New Name""#).unwrap();
        assert_eq!(p.old_name, "Old Name");
        assert_eq!(p.new_name, "New Name");
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("").is_err());
    }

    #[test]
    fn parse_args_rejects_one_arg() {
        let e = parse_args("OnlyOne").unwrap_err();
        assert!(e.contains("new mailbox"), "{e}");
    }

    #[test]
    fn parse_args_rejects_trailing_tokens() {
        let e = parse_args("a b c").unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }
}
