//! `EXPUNGE` / `UID EXPUNGE` — RFC 9051 §6.4.3 / RFC 4315 §2.1.
//!
//! Permanently remove every message in the selected mailbox that has
//! the `\Deleted` flag set (UID EXPUNGE additionally intersects the
//! supplied UID set). The substrate primitive
//! [`crate::mailstore::MailStore::expunge_memberships`] performs the
//! removals atomically inside one `with_set_tx`; this handler is
//! responsible for snapshotting + filtering + emitting the
//! `* <msn> EXPUNGE` responses **in descending MSN order**
//! (RFC 9051 §7.4.1 — sending ascending MSNs would invalidate the
//! client's local map because each prior EXPUNGE renumbers later
//! messages).
//!
//! ## Wire shape
//!
//! ```text
//! <tag> EXPUNGE
//! <tag> UID EXPUNGE <uidset>
//! ```
//!
//! ## Filtering
//!
//! * `EXPUNGE`: every message with `\Deleted` set.
//! * `UID EXPUNGE`: every message with `\Deleted` set **and** UID in
//!   the supplied set. Out-of-range UIDs in the set are silently
//!   ignored per RFC 4315 §2.1 (the intersection with the mailbox
//!   produces the working set).
//!
//! On parse-success but zero matched messages, emit
//! `OK EXPUNGE completed` with no untagged `EXPUNGE` rows.

use std::sync::Arc;

use cosmix_mds::ContainerId;
use cosmix_mds::types::Flags;

use crate::imap::response::{Status, tagged};
use crate::imap::seq::{Resolved, SeqSet, SequenceMap};

// `Resolved` is used as the iterator element type in the UID filter
// closure; the explicit annotation keeps inference stable.
use crate::mailstore::{ListOpts, MailStore, SortKey};

type AccountId = i32;

/// EXPUNGE invocation mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpungeMode {
    /// Plain `EXPUNGE` — no arguments.
    Plain,
    /// `UID EXPUNGE <uidset>` — intersect `\Deleted` with the UID set.
    Uid(SeqSet),
}

/// Parse arguments for plain `EXPUNGE`. RFC 9051 §6.4.3 — no args.
pub fn parse_args_plain(args: &str) -> Result<(), String> {
    if !args.trim().is_empty() {
        return Err(format!("EXPUNGE takes no arguments; got {:?}", args.trim()));
    }
    Ok(())
}

/// Parse arguments for `UID EXPUNGE <uidset>`. RFC 4315 §2.1.
pub fn parse_args_uid(args: &str) -> Result<SeqSet, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err("UID EXPUNGE requires a UID set".into());
    }
    SeqSet::parse(trimmed).map_err(|e| format!("UID EXPUNGE invalid UID set: {e}"))
}

/// Pre-validate at dispatch time so grammar failures bump the
/// bad-command counter before the handler is spawned.
pub fn parse_args_for_dispatch(args: &str, mode_uid: bool) -> Result<(), String> {
    if mode_uid {
        parse_args_uid(args).map(|_| ())
    } else {
        parse_args_plain(args)
    }
}

/// Wire response plus the UIDs that were actually expunged. The
/// caller (`session.rs`) writes `wire` to the socket and removes
/// `expunged_uids` from the selected session's view UID list so a
/// subsequent IDLE entry does not re-emit the same EXPUNGEs as
/// catch-up.
pub struct ExpungeOutcome {
    pub wire: String,
    pub expunged_uids: Vec<u64>,
}

impl ExpungeOutcome {
    fn wire_only(wire: String) -> Self {
        Self {
            wire,
            expunged_uids: Vec::new(),
        }
    }
}

impl From<String> for ExpungeOutcome {
    fn from(wire: String) -> Self {
        Self::wire_only(wire)
    }
}

/// Execute EXPUNGE / UID EXPUNGE for a selected mailbox.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    mailbox: ContainerId,
    store: Arc<dyn MailStore>,
    mode_uid: bool,
) -> ExpungeOutcome {
    let uid_filter: Option<SeqSet> = if mode_uid {
        match parse_args_uid(args) {
            Ok(s) => Some(s),
            Err(e) => return tagged(tag, Status::Bad, None, &e).into(),
        }
    } else {
        if let Err(e) = parse_args_plain(args) {
            return tagged(tag, Status::Bad, None, &e).into();
        }
        None
    };

    // Snapshot the mailbox so we can compute MSNs and read \Deleted.
    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let store_for_list = store.clone();
    let handles = match tokio::task::spawn_blocking(move || {
        store_for_list.list_emails_in_mailbox(account_id, mailbox, opts)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "EXPUNGE list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "EXPUNGE failed").into();
        }
        Err(e) => {
            tracing::warn!(error = %e, "EXPUNGE spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "EXPUNGE failed").into();
        }
    };

    // For UID EXPUNGE, restrict to UIDs in the supplied set. Use
    // `SequenceMap::resolve_uid` for the canonical wildcard/range
    // semantics (handles `*`, `1:*`, etc.); collect the matched UIDs
    // into a set.
    let uid_allowed: Option<std::collections::HashSet<u64>> = if let Some(s) = &uid_filter {
        let map = SequenceMap::from_entries(handles.iter().map(|h| (h.uid, h.id)));
        Some(
            map.resolve_uid(s, 0)
                .into_iter()
                .map(|r: Resolved| r.uid)
                .collect(),
        )
    } else {
        None
    };

    // Filter by \Deleted (+ optional UID set), keep (msn, item_id).
    // `list_emails_in_mailbox(SortKey::Seq)` returns handles ordered
    // ascending by `seq` (which equals UID), so the 1-based index is
    // the MSN.
    //
    // **Snapshot/write race:** the snapshot's MSNs are computed
    // against `handles` as it was at list-time; a concurrent
    // EXPUNGE / STORE / MOVE between snapshot and substrate write
    // can leave the wire MSNs slightly off relative to the real
    // mailbox state at write time. The substrate's `with_set_tx`
    // skips already-removed memberships silently
    // (`expunge_memberships` matches the "membership not present"
    // shape — see `mailstore/mod.rs`), so the write is safe; the
    // wire emission is well-formed against the pre-snapshot which
    // is what RFC 9051 §7.4.1 expects clients to apply. Matches
    // STORE/COPY/MOVE's snapshot-then-mutate pattern.
    // `(msn, item_id, uid)` triple — the UID is reported back to the
    // session loop so it can prune the per-connection `view_uids`
    // exactly in lockstep with the EXPUNGE lines we emit.
    let mut targets: Vec<(u32, cosmix_mds::ItemId, u64)> = Vec::new();
    for (idx, h) in handles.iter().enumerate() {
        if (h.keywords.0 & Flags::DELETED) == 0 {
            continue;
        }
        if let Some(allowed) = &uid_allowed
            && !allowed.contains(&h.uid)
        {
            continue;
        }
        let msn = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        targets.push((msn, h.id, h.uid));
    }

    if targets.is_empty() {
        let text = if mode_uid {
            "UID EXPUNGE completed"
        } else {
            "EXPUNGE completed"
        };
        return tagged(tag, Status::Ok, None, text).into();
    }

    // Run the substrate write — atomically removes every matched
    // membership in one `with_set_tx`.
    let item_ids: Vec<cosmix_mds::ItemId> = targets.iter().map(|(_, id, _)| *id).collect();
    let store_for_expunge = store.clone();
    let mb = mailbox;
    let ids_owned = item_ids.clone();
    let res = tokio::task::spawn_blocking(move || {
        store_for_expunge.expunge_memberships(account_id, mb, &ids_owned)
    })
    .await;
    match res {
        Ok(Ok(_n)) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "EXPUNGE expunge_memberships failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "EXPUNGE failed").into();
        }
        Err(e) => {
            tracing::warn!(error = %e, "EXPUNGE spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "EXPUNGE failed").into();
        }
    }

    // Emit untagged `* <msn> EXPUNGE` rows in **descending** MSN
    // order (RFC 9051 §7.4.1). Each row decrements the client's
    // local MSN map for messages above it; sending descending lets
    // every emitted MSN remain valid against the original map at the
    // moment the client applies it.
    let mut out = String::new();
    let mut sorted = targets;
    sorted.sort_by_key(|t| std::cmp::Reverse(t.0));
    for (msn, _, _) in &sorted {
        out.push_str(&format!("* {msn} EXPUNGE\r\n"));
    }
    // UID list is what the session uses to prune `view_uids`. Order
    // does not matter for the prune; emit ascending for ergonomics.
    let mut expunged_uids: Vec<u64> = sorted.iter().map(|(_, _, uid)| *uid).collect();
    expunged_uids.sort_unstable();

    let text = if mode_uid {
        "UID EXPUNGE completed"
    } else {
        "EXPUNGE completed"
    };
    out.push_str(&tagged(tag, Status::Ok, None, text));
    ExpungeOutcome {
        wire: out,
        expunged_uids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_rejects_args() {
        assert!(parse_args_plain("1:5").is_err());
    }

    #[test]
    fn parse_plain_accepts_blank() {
        assert!(parse_args_plain("").is_ok());
        assert!(parse_args_plain("   ").is_ok());
    }

    #[test]
    fn parse_uid_rejects_empty() {
        assert!(parse_args_uid("").is_err());
    }

    #[test]
    fn parse_uid_accepts_set() {
        assert!(parse_args_uid("1:5,7").is_ok());
    }

    #[test]
    fn parse_uid_rejects_bad_seq() {
        assert!(parse_args_uid("0").is_err());
    }
}
