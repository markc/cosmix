//! `MOVE` / `UID MOVE` — RFC 6851 (extension layered on RFC 9051).
//!
//! Atomic per-item move: remove the membership of `item_id` in `src`
//! and add a membership in `dest`, in one `with_set_tx`. The
//! substrate primitive [`crate::mailstore::MailStore::move_message`]
//! returns `(src_uid, dest_uid)` so the wire response can emit
//! `* OK [COPYUID <uidvalidity> <src-set> <dest-set>]` (RFC 4315)
//! without a follow-up read.
//!
//! ## Wire shape
//!
//! ```text
//! <tag> MOVE <seqset> <dest-mailbox>
//! <tag> UID MOVE <uidset> <dest-mailbox>
//! ```
//!
//! ## Response shape (RFC 6851 §3.3)
//!
//! Successful MOVE emits:
//!
//! 1. One untagged `* OK [COPYUID <uidvalidity> <src-set> <dest-set>]`
//!    (RFC 4315 COPYUID) — only when at least one message moved.
//! 2. One `* <msn> EXPUNGE` per moved message **in descending MSN
//!    order** (RFC 9051 §7.4.1 — see [`super::expunge`]).
//! 3. The tagged `OK MOVE completed`.
//!
//! Non-atomic across items, same rationale as
//! [`super::copy`] — each `move_message` is its own `with_set_tx`.
//!
//! ## Error mapping
//!
//! | Failure                                | Response                                  |
//! |----------------------------------------|-------------------------------------------|
//! | grammar parse                          | `BAD <reason>`                            |
//! | dest mailbox does not exist            | `NO [TRYCREATE] mailbox does not exist`   |
//! | dest mailbox name unrepresentable      | `NO [SERVERBUG] mailbox not representable`|
//! | dest == src                            | `NO MOVE: src and dest must differ`       |
//! | mid-move substrate error               | `NO [SERVERBUG] MOVE failed` (partial)    |

use std::sync::Arc;

use cosmix_mds::ContainerId;
use cosmix_mds::types::Flags;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::copy::render_uid_set;
use crate::imap::op::list;
use crate::imap::response::{Status, tagged, wire_uidvalidity};
use crate::imap::seq::{Resolved, SeqSet, SequenceMap};
use crate::mailstore::{EmailHandle, ListOpts, MailStore, SortKey};

type AccountId = i32;

/// Whether the sequence set is interpreted as MSN (`MOVE`) or UID
/// (`UID MOVE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMode {
    Msn,
    Uid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MoveReq {
    set: SeqSet,
    dest: String,
    mode: MoveMode,
}

/// Parse `MOVE` / `UID MOVE` arguments per RFC 6851 §3.1.
///
/// Grammar after the verb is stripped:
///
/// ```text
/// args = sequence-set SP mailbox
/// ```
fn parse_args(args: &str, mode: MoveMode) -> Result<MoveReq, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err("MOVE requires sequence-set and destination mailbox".into());
    }
    let (seq_str, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => return Err("MOVE missing destination mailbox".into()),
    };
    let set = SeqSet::parse(seq_str).map_err(|e| format!("MOVE invalid sequence-set: {e}"))?;

    let (dest, rest_after) = list::take_mailbox_name(rest)?;
    if dest.is_empty() {
        return Err("MOVE requires a non-empty destination mailbox name".into());
    }
    let tail = rest_after.trim();
    if !tail.is_empty() {
        return Err(format!("MOVE: unexpected trailing argument {tail:?}"));
    }
    Ok(MoveReq { set, dest, mode })
}

/// Pre-validate so grammar failures bump the bad-command counter at
/// dispatch time.
pub fn parse_args_for_dispatch(args: &str, mode: MoveMode) -> Result<(), String> {
    parse_args(args, mode).map(|_| ())
}

/// Wire response plus the source-side UIDs that were actually moved
/// out. The caller (`session.rs`) writes `wire` to the socket and
/// removes `expunged_uids` from the selected session's view UID list
/// so a subsequent IDLE entry does not re-emit the same EXPUNGEs as
/// catch-up.
pub struct MoveOutcome {
    pub wire: String,
    pub expunged_uids: Vec<u64>,
}

impl MoveOutcome {
    fn wire_only(wire: String) -> Self {
        Self {
            wire,
            expunged_uids: Vec::new(),
        }
    }
}

impl From<String> for MoveOutcome {
    fn from(wire: String) -> Self {
        Self::wire_only(wire)
    }
}

/// Execute MOVE / UID MOVE for a selected mailbox.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    src_container: ContainerId,
    store: Arc<dyn MailStore>,
    mode: MoveMode,
) -> MoveOutcome {
    let req = match parse_args(args, mode) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e).into(),
    };

    // Resolve destination mailbox against the live list.
    let store_for_list = store.clone();
    let records = match tokio::task::spawn_blocking(move || {
        store_for_list.list_mailboxes(account_id)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "MOVE list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed").into();
        }
        Err(e) => {
            tracing::warn!(error = %e, "MOVE list_mailboxes spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed").into();
        }
    };

    let dest_id = match resolve(&req.dest, &records) {
        ResolveResult::Found(id) => id,
        ResolveResult::NotFound => {
            return tagged(tag, Status::No, Some("TRYCREATE"), "mailbox does not exist").into();
        }
        ResolveResult::Unrepresentable => {
            return tagged(
                tag,
                Status::No,
                Some("SERVERBUG"),
                "mailbox name not representable over IMAP",
            )
            .into();
        }
    };

    if dest_id == src_container {
        return tagged(tag, Status::No, None, "MOVE: src and dest must differ").into();
    }

    // Snapshot the source mailbox so we can resolve MSN/UID → ItemId
    // and recover per-item flags for `move_message`.
    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let store_for_src = store.clone();
    let handles = match tokio::task::spawn_blocking(move || {
        store_for_src.list_emails_in_mailbox(account_id, src_container, opts)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "MOVE list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed").into();
        }
        Err(e) => {
            tracing::warn!(error = %e, "MOVE spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed").into();
        }
    };

    let map = SequenceMap::from_entries(handles.iter().map(|h| (h.uid, h.id)));
    let resolved: Vec<Resolved> = match req.mode {
        MoveMode::Msn => map.resolve_msn(&req.set),
        MoveMode::Uid => map.resolve_uid(&req.set, 0),
    };

    if resolved.is_empty() {
        return tagged(tag, Status::Ok, None, "MOVE completed").into();
    }

    let by_id: std::collections::HashMap<cosmix_mds::ItemId, &EmailHandle> =
        handles.iter().map(|h| (h.id, h)).collect();

    // Issue per-item moves. Substrate returns `(src_uid, dest_uid)`
    // pairs; we accumulate them for the COPYUID code and track which
    // MSNs to EXPUNGE (descending).
    //
    // **Snapshot/write race:** the source snapshot is read in its
    // own short tx before the per-item writes start; a concurrent
    // EXPUNGE / STORE / MOVE between snapshot and per-item move can
    // leave the resolved ItemId pointing at a row that's already
    // gone, surfacing as substrate `email not found` and
    // terminating the MOVE with `NO [SERVERBUG]`. Matches STORE's
    // existing snapshot-then-mutate pattern; avoids a mailbox-level
    // lock. The wire response is well-formed for whatever subset
    // committed before the failure.
    //
    // Per-item `stamp_id` is `item_id.0.to_string()` — for
    // inbound-delivered items the ItemId IS the per-delivery stamp
    // (see `src/smtp/inbound.rs:386`). The `mail_retrain_outbox`
    // PK is `(stamp_id, label)`, so a unique per-item stamp is
    // load-bearing on `INSERT OR IGNORE`.
    let mut src_uids: Vec<u64> = Vec::with_capacity(resolved.len());
    let mut dest_uids: Vec<u64> = Vec::with_capacity(resolved.len());
    let mut expunged_msns: Vec<u32> = Vec::with_capacity(resolved.len());
    let mut uid_validity: Option<u64> = None;

    for r in resolved.iter() {
        let h = match by_id.get(&r.item_id) {
            Some(h) => h,
            None => continue,
        };
        let store_for_move = store.clone();
        let item_id = r.item_id;
        let flags = Flags(h.keywords.0);
        let msn = r.msn;
        let stamp = item_id.0.to_string();
        let res = tokio::task::spawn_blocking(move || {
            store_for_move.move_message(account_id, item_id, src_container, dest_id, flags, &stamp)
        })
        .await;
        match res {
            Ok(Ok((src_uid, dest_uid))) => {
                if uid_validity.is_none() {
                    uid_validity = Some(dest_uid.uid_validity);
                }
                src_uids.push(src_uid.uid);
                dest_uids.push(dest_uid.uid);
                expunged_msns.push(msn);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "MOVE move_message failed");
                // Emit whatever partial-progress untagged responses
                // we've already accumulated so the client can
                // reconcile, then close with tagged NO.
                let mut out = String::new();
                if !dest_uids.is_empty() {
                    let validity = wire_uidvalidity(uid_validity.unwrap_or(0));
                    let src_set = render_uid_set(&src_uids);
                    let dest_set = render_uid_set(&dest_uids);
                    out.push_str(&format!(
                        "* OK [COPYUID {validity} {src_set} {dest_set}]\r\n"
                    ));
                    let mut sorted = expunged_msns.clone();
                    sorted.sort_by(|a, b| b.cmp(a));
                    for m in &sorted {
                        out.push_str(&format!("* {m} EXPUNGE\r\n"));
                    }
                }
                out.push_str(&tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed"));
                // Partial-progress: still report the UIDs that did
                // move so the session can prune `view_uids`.
                let mut expunged_uids = src_uids.clone();
                expunged_uids.sort_unstable();
                return MoveOutcome {
                    wire: out,
                    expunged_uids,
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "MOVE spawn_blocking panicked");
                return tagged(tag, Status::No, Some("SERVERBUG"), "MOVE failed").into();
            }
        }
    }

    if dest_uids.is_empty() {
        return tagged(tag, Status::Ok, None, "MOVE completed").into();
    }

    // Assemble the full response: COPYUID, then EXPUNGE rows in
    // descending MSN order, then tagged OK.
    let validity = wire_uidvalidity(uid_validity.unwrap_or(0));
    let src_set = render_uid_set(&src_uids);
    let dest_set = render_uid_set(&dest_uids);
    let mut out = String::new();
    out.push_str(&format!(
        "* OK [COPYUID {validity} {src_set} {dest_set}]\r\n"
    ));
    expunged_msns.sort_by(|a, b| b.cmp(a));
    for m in &expunged_msns {
        out.push_str(&format!("* {m} EXPUNGE\r\n"));
    }
    out.push_str(&tagged(tag, Status::Ok, None, "MOVE completed"));
    let mut expunged_uids = src_uids.clone();
    expunged_uids.sort_unstable();
    MoveOutcome {
        wire: out,
        expunged_uids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_msn_basic() {
        let r = parse_args("1:5 Archive", MoveMode::Msn).unwrap();
        assert_eq!(r.dest, "Archive");
        assert_eq!(r.mode, MoveMode::Msn);
    }

    #[test]
    fn parse_args_uid_quoted_dest() {
        let r = parse_args(r#"7,9:11 "Sent Items""#, MoveMode::Uid).unwrap();
        assert_eq!(r.dest, "Sent Items");
        assert_eq!(r.mode, MoveMode::Uid);
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("", MoveMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_missing_dest() {
        assert!(parse_args("1:5", MoveMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_bad_seq() {
        assert!(parse_args("0 INBOX", MoveMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_trailing_junk() {
        assert!(parse_args("1 INBOX trailing", MoveMode::Msn).is_err());
    }
}
