//! `COPY` / `UID COPY` — RFC 9051 §6.4.7 with RFC 4315 `COPYUID`.
//!
//! Adds a new membership in the destination mailbox for every message
//! in the sequence-set, **without** touching the source membership.
//! Returns `OK [COPYUID <uidvalidity> <src-uid-set> <dest-uid-set>]`
//! so well-behaved clients (Thunderbird, mutt) can update local maps
//! without re-listing the destination.
//!
//! ## Wire shape
//!
//! ```text
//! <tag> COPY <seqset> <dest-mailbox>
//! <tag> UID COPY <uidset> <dest-mailbox>
//! ```
//!
//! ## Substrate primitive
//!
//! Per-item writes go through
//! [`crate::mailstore::MailStore::copy_message`], which calls
//! `SqliteSetTx::add_membership_from` inside one tx per item. Unlike
//! `add_membership` (which reads `(flags, tags)` from any existing
//! membership via `ORDER BY container_id LIMIT 1`), the `_from`
//! variant reads from the **specific** source mailbox — RFC 9051
//! §6.4.7 mandates the destination inherit the source mailbox's
//! flags, and the IMAP §3.2 per-mailbox flag model permits sibling
//! memberships of the same item to carry different flags. An absent
//! `(item_id, src)` row at write time is a hard error (mapped to
//! `email not found`), not a fallback — a concurrent EXPUNGE/MOVE
//! that drops `src`'s membership between snapshot and tx must fail
//! the COPY rather than silently mint a dest row with empty flags.
//! Junk-boundary detection enqueues a retrain row when the copy
//! crosses the `\Junk` boundary, matching `move_message`; the
//! enqueue is suppressed when the source mailbox is EXAMINE'd
//! (read-only) so a read-only source has no side-channel effect on
//! the classifier.
//!
//! Each `copy_message` is its own `with_set_tx`. A multi-message
//! COPY is therefore NOT atomic across items — a power-loss between
//! item #3 and #4 of a 10-message COPY leaves the first three
//! visible in `dest`. This matches the existing STORE behaviour
//! (per-item writes) and is consistent with how Thunderbird reads
//! COPYUID after a partial failure (it re-runs `FETCH UID` on the
//! dest). A future task can collapse this into a single
//! `with_set_tx` once `add_membership` supports multi-item input.
//!
//! ## Error mapping
//!
//! | Failure                                | Response                                  |
//! |----------------------------------------|-------------------------------------------|
//! | grammar parse                          | `BAD <reason>`                            |
//! | dest mailbox does not exist            | `NO [TRYCREATE] mailbox does not exist`   |
//! | dest mailbox name unrepresentable      | `NO [SERVERBUG] mailbox not representable`|
//! | dest == src                            | `NO COPY: src and dest must differ`       |
//! | mid-copy substrate error               | `NO [SERVERBUG] COPY failed` (partial)    |
//!
//! On parse-success but zero resolved items (empty mailbox or a
//! sequence-set that hits no live messages), emit
//! `OK COPY completed` with no COPYUID code — RFC 4315 §3 says
//! `COPYUID` is OPTIONAL and the response code is omitted when no
//! messages were copied.

use std::sync::Arc;

use cosmix_mds::ContainerId;

use crate::imap::mailbox_name::{ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged, wire_uidvalidity};
use crate::imap::seq::{Resolved, SeqSet, SequenceMap};
use crate::mailstore::{EmailHandle, ListOpts, MailStore, SortKey};

type AccountId = i32;

/// Whether the sequence set is interpreted as MSN (`COPY`) or UID
/// (`UID COPY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    Msn,
    Uid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyReq {
    set: SeqSet,
    dest: String,
    mode: CopyMode,
}

/// Parse `COPY` / `UID COPY` arguments per RFC 9051 §6.4.7.
///
/// Grammar after the verb is stripped:
///
/// ```text
/// args = sequence-set SP mailbox
/// ```
fn parse_args(args: &str, mode: CopyMode) -> Result<CopyReq, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err("COPY requires sequence-set and destination mailbox".into());
    }
    let (seq_str, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => return Err("COPY missing destination mailbox".into()),
    };
    let set = SeqSet::parse(seq_str).map_err(|e| format!("COPY invalid sequence-set: {e}"))?;

    let (dest, rest_after) = list::take_mailbox_name(rest)?;
    if dest.is_empty() {
        return Err("COPY requires a non-empty destination mailbox name".into());
    }
    let tail = rest_after.trim();
    if !tail.is_empty() {
        return Err(format!("COPY: unexpected trailing argument {tail:?}"));
    }
    Ok(CopyReq { set, dest, mode })
}

/// Pre-validate so grammar failures bump the bad-command counter at
/// dispatch time. The handler re-parses on its own path; the second
/// parse is pure.
pub fn parse_args_for_dispatch(args: &str, mode: CopyMode) -> Result<(), String> {
    parse_args(args, mode).map(|_| ())
}

/// Execute COPY / UID COPY for a selected mailbox.
///
/// `read_only_src` is `true` when the source mailbox is EXAMINE'd.
/// COPY is permitted under EXAMINE (RFC 9051 §6.4.7 — the
/// destination is the mutating party), but the read-only flag
/// suppresses the substrate's junk-boundary retrain-outbox enqueue
/// so an EXAMINE'd session can't side-channel the classifier.
///
/// Per-item `stamp_id` is `item_id.0.to_string()` — for substrate
/// items minted by inbound delivery, the ItemId IS the per-delivery
/// stamp the verdict topic publishes (see `src/smtp/inbound.rs:386`).
/// The `mail_retrain_outbox` PK is `(stamp_id, label)`, so a unique
/// stamp per item is load-bearing: a shared placeholder would
/// silently drop retrains on `INSERT OR IGNORE`.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    src_container: ContainerId,
    store: Arc<dyn MailStore>,
    mode: CopyMode,
    read_only_src: bool,
) -> String {
    let req = match parse_args(args, mode) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
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
            tracing::warn!(error = %e, "COPY list_mailboxes failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "COPY list_mailboxes spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
        }
    };

    let dest_id = match resolve(&req.dest, &records) {
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

    if dest_id == src_container {
        // RFC 9051 §6.4.7 doesn't forbid copying a message into the
        // same mailbox, but the substrate refuses (membership uniqueness)
        // and the wire-result would be an `item already in container`
        // surface for every row. Cheaper to flag at the IMAP layer.
        return tagged(tag, Status::No, None, "COPY: src and dest must differ");
    }

    // Snapshot the source mailbox so we can resolve MSN/UID → ItemId.
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
            tracing::warn!(error = %e, "COPY list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "COPY spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
        }
    };

    let map = SequenceMap::from_entries(handles.iter().map(|h| (h.uid, h.id)));
    let resolved: Vec<Resolved> = match req.mode {
        CopyMode::Msn => map.resolve_msn(&req.set),
        CopyMode::Uid => map.resolve_uid(&req.set, 0),
    };

    // Empty resolution: OK with no COPYUID per RFC 4315 §3.
    if resolved.is_empty() {
        return tagged(tag, Status::Ok, None, "COPY completed");
    }

    // Issue per-item copies. Substrate returns the dest MembershipUid
    // (uid + uid_validity); we accumulate (src_uid, dest_uid) for
    // the COPYUID code.
    //
    // **Snapshot/write race:** the source snapshot is read in its
    // own short tx before the per-item writes start; a concurrent
    // EXPUNGE / STORE between snapshot and the per-item copy_message
    // can leave the resolved ItemId pointing at a row that's already
    // gone, which surfaces as a substrate `email not found` error
    // and terminates the COPY with `NO [SERVERBUG]`. This matches
    // STORE's existing snapshot-then-mutate behaviour and avoids
    // taking a mailbox-level lock; the wire response is well-formed
    // either way.
    //
    // The junk-boundary retrain row uses `INSERT OR IGNORE` against
    // a `(stamp_id, label)` PK. We pass `item_id.0.to_string()` as
    // the stamp because for inbound-delivered items, the ItemId IS
    // the per-delivery stamp the verdict topic publishes — see
    // `src/smtp/inbound.rs:386` (`email_uuid = item_id.0`). This
    // gives each row a per-item unique stamp so multi-message COPY
    // into Junk does not silently drop later retrains.
    let mut src_uids: Vec<u64> = Vec::with_capacity(resolved.len());
    let mut dest_uids: Vec<u64> = Vec::with_capacity(resolved.len());
    let mut uid_validity: Option<u64> = None;

    let by_id: std::collections::HashMap<cosmix_mds::ItemId, &EmailHandle> =
        handles.iter().map(|h| (h.id, h)).collect();

    for r in resolved.iter() {
        let src_uid = match by_id.get(&r.item_id) {
            Some(h) => h.uid,
            None => continue,
        };
        let store_for_copy = store.clone();
        let item_id = r.item_id;
        let stamp = item_id.0.to_string();
        let res = tokio::task::spawn_blocking(move || {
            store_for_copy.copy_message(
                account_id,
                item_id,
                src_container,
                dest_id,
                &stamp,
                read_only_src,
            )
        })
        .await;
        match res {
            Ok(Ok(uid)) => {
                if uid_validity.is_none() {
                    uid_validity = Some(uid.uid_validity);
                }
                src_uids.push(src_uid);
                dest_uids.push(uid.uid);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "COPY copy_message failed");
                return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "COPY spawn_blocking panicked");
                return tagged(tag, Status::No, Some("SERVERBUG"), "COPY failed");
            }
        }
    }

    if dest_uids.is_empty() {
        return tagged(tag, Status::Ok, None, "COPY completed");
    }

    // Render `COPYUID <uidvalidity> <src-set> <dest-set>` per RFC 4315.
    let validity = wire_uidvalidity(uid_validity.unwrap_or(0));
    let src_set = render_uid_set(&src_uids);
    let dest_set = render_uid_set(&dest_uids);
    let code = format!("COPYUID {validity} {src_set} {dest_set}");
    tagged(tag, Status::Ok, Some(&code), "COPY completed")
}

/// Render a list of UIDs as an RFC 9051 `sequence-set` — coalesce
/// contiguous runs `a:b`, comma-join otherwise. Caller guarantees the
/// input is non-empty.
pub(crate) fn render_uid_set(uids: &[u64]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            end = sorted[i + 1];
            i += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_msn_basic() {
        let r = parse_args("1:5 Archive", CopyMode::Msn).unwrap();
        assert_eq!(r.dest, "Archive");
        assert_eq!(r.mode, CopyMode::Msn);
    }

    #[test]
    fn parse_args_uid_quoted_dest() {
        let r = parse_args(r#"7,9:11 "Sent Items""#, CopyMode::Uid).unwrap();
        assert_eq!(r.dest, "Sent Items");
        assert_eq!(r.mode, CopyMode::Uid);
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("", CopyMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_missing_dest() {
        assert!(parse_args("1:5", CopyMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_bad_seq() {
        assert!(parse_args("0 INBOX", CopyMode::Msn).is_err());
    }

    #[test]
    fn parse_args_rejects_trailing_junk() {
        assert!(parse_args("1 INBOX trailing", CopyMode::Msn).is_err());
    }

    #[test]
    fn render_uid_set_single() {
        assert_eq!(render_uid_set(&[5]), "5");
    }

    #[test]
    fn render_uid_set_run() {
        assert_eq!(render_uid_set(&[3, 4, 5, 6]), "3:6");
    }

    #[test]
    fn render_uid_set_mixed() {
        assert_eq!(render_uid_set(&[1, 2, 5, 8, 9, 10]), "1:2,5,8:10");
    }

    #[test]
    fn render_uid_set_unsorted_dedup() {
        assert_eq!(render_uid_set(&[7, 3, 5, 3, 4]), "3:5,7");
    }
}
