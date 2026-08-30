//! `IDLE` — RFC 2177 (carried into RFC 9051 §6.3.13 as a normative
//! reference).
//!
//! The IMAP IDLE command is a bidirectional command that pins the
//! connection in "wait-for-server-push" state until the client sends
//! `DONE` (a single line, no tag). While idle, the server emits
//! untagged events as substrate changes arrive on the mailbox.
//!
//! ## Wire shape
//!
//! ```text
//! C: a01 IDLE
//! S: + idling
//! S: * 14 EXISTS                ; one new message
//! S: * 12 EXPUNGE               ; one deletion
//! S: * OK Still here            ; keepalive (every `idle_status_interval`)
//! C: DONE
//! S: a01 OK IDLE terminated
//! ```
//!
//! ## Translation discipline
//!
//! The IDLE handler maintains a local snapshot of the selected
//! mailbox's UIDs (ascending) at entry. Substrate
//! [`ContainerEvent`]s arrive via the per-mailbox notifier
//! ([`crate::mailstore::MailStore::subscribe_mailbox`]) and are
//! translated:
//!
//! * `ItemAdded { seq, .. }` — insert `seq` into the snapshot,
//!   emit `* <len> EXISTS`.
//! * `ItemRemoved { seq, .. }` — find `seq`'s 1-based position,
//!   remove, emit `* <position> EXPUNGE`. If the UID is not in our
//!   snapshot, drop silently — the entry-diff and Lagged-diff
//!   paths *guarantee* the local snapshot tracks the client's
//!   view at IDLE entry and after every resync, so an unknown UID
//!   here means the substrate is re-publishing an event whose
//!   removal we already accounted for (e.g. a queued event that
//!   fired between subscribe and entry-snapshot whose effect the
//!   diff already emitted).
//! * `FlagsChanged` / `ItemMoved` — skipped. `ItemMoved` is
//!   *additive* (the substrate also publishes the underlying
//!   `ItemAdded`/`ItemRemoved` on src/dest) so we still see the
//!   moved-into-this-mailbox / moved-out-of-this-mailbox shapes
//!   without parsing the correlation event. Flag updates are
//!   re-fetched by the client on the next NOOP/SELECT pass; an
//!   MSN-bearing `FETCH FLAGS` push would require an out-of-band
//!   item-id → MSN lookup that's not worth the substrate
//!   roundtrip for the Thunderbird MVP.
//!
//! ## Entry-race coherence
//!
//! At IDLE entry, the handler takes one fresh substrate snapshot
//! (`s2`) and diffs it against the *session's* view UID list
//! (`selected.view_uids`, populated at SELECT/EXAMINE and mutated in
//! lockstep with every EXPUNGE / MOVE-out the session has emitted).
//! The diff is emitted as untagged EXPUNGEs (descending MSN per
//! RFC 9051 §7.4.1) followed by a single `EXISTS` if the count grew.
//!
//! Using `selected.view_uids` (not a fresh pre-subscribe snapshot)
//! closes the post-SELECT / pre-IDLE expunge gap: another session
//! could have removed UIDs the client *was* told about at SELECT
//! time, between SELECT and IDLE. A pre-subscribe substrate snapshot
//! would already exclude those UIDs, so a snapshot-twice diff would
//! miss them. The session-tracked view is the only thing that knows
//! "what the client believes is there."
//!
//! After emitting the entry diff we update `selected.view_uids = s2`
//! so subsequent recv-loop events mutate the same vector the session
//! reads, and we run the recv loop against `&mut selected.view_uids`
//! directly — no further desync is possible because every emission
//! goes through the same vector.
//!
//! Events queued between `subscribe` and `s2` are reflected in `s2`
//! already and are drained idempotently against it: `ItemAdded`
//! dedups via `partition_point`; `ItemRemoved` for a UID already
//! gone is a no-op because the diff covered it.
//!
//! ## Lagged subscribers
//!
//! `broadcast::Receiver::recv` returns `RecvError::Lagged(n)` when
//! the subscriber falls more than `notifier::DEFAULT_CAPACITY`
//! events behind. We emit a `* OK [ALERT] notifier lagged …` line
//! (RFC 9051 §7.1 — `OK` with an `[ALERT]` response code is a
//! human-readable warning the client should surface), then run the
//! same diff-and-emit as entry: resnapshot the UID list, emit the
//! local-view → new-snapshot diff (descending EXPUNGEs + final
//! EXISTS), and replace the local view. `EXISTS` alone is
//! insufficient because the skipped events may have included
//! deletions the client must see to keep its MSN map valid.
//!
//! ## Channel close (container deleted)
//!
//! `RecvError::Closed` arrives when the mailbox container is
//! deleted out-of-band (rare; the DELETE handler emits
//! `notifier::drop_channel` for the doomed container). We emit
//! `* BYE` plus the tagged OK and return `IdleOutcome::Close`; the
//! session loop closes the connection so the client reconnects
//! against a fresh mailbox list.
//!
//! ## Protocol violations are terminal
//!
//! Any input on the IDLE stream that is not `DONE` (case-insensitive)
//! is a protocol violation — including non-UTF8 framing, line-too-
//! long, and "looks like an IMAP command." The handler emits a tagged
//! `BAD` and returns `IdleOutcome::Close`. We cannot resume normal
//! dispatch from inside IDLE for the LineTooLong / literal-shaped
//! cases without re-syncing stream framing first, and the simplest
//! defensible behavior across all bad-input shapes is to close. This
//! matches Cyrus / Dovecot's stream-sync posture for IDLE violations.
//!
//! ## Authenticated vs Selected
//!
//! Per RFC 2177 §3, IDLE is permitted in both AUTHENTICATED and
//! SELECTED states. In AUTHENTICATED-only (no mailbox selected),
//! there is nothing to subscribe to and we simply wait for `DONE`
//! with keepalive ticks. Emitting any untagged mailbox-state event
//! without a selection would be a protocol error.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cosmix_mds::types::{ContainerEvent, Seq};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::imap::codec::{self, ReadLineError};
use crate::imap::response::{Status, tagged};
use crate::imap::state::SelectedMailbox;
use crate::mailstore::{ListOpts, MailStore, SortKey};

type AccountId = i32;

/// Validate `IDLE`'s argument list — RFC 2177 §3 says the command
/// takes no arguments.
pub fn parse_args(args: &str) -> Result<(), String> {
    if !args.trim().is_empty() {
        return Err(format!("IDLE takes no arguments; got {:?}", args.trim()));
    }
    Ok(())
}

/// Snapshot the selected mailbox's current UIDs (ascending) for the
/// IDLE handler's local map. Factored out so tests can exercise the
/// translator directly without going through the full session.
async fn snapshot_uids(
    store: Arc<dyn MailStore>,
    account: AccountId,
    mailbox: cosmix_mds::ContainerId,
) -> Result<Vec<u64>> {
    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let store_for_list = store.clone();
    let handles = tokio::task::spawn_blocking(move || {
        store_for_list.list_emails_in_mailbox(account, mailbox, opts)
    })
    .await??;
    Ok(handles.into_iter().map(|h| h.uid).collect())
}

/// Translate one `ContainerEvent` against the local UID snapshot
/// into the matching untagged IMAP line. Returns `None` for events
/// we deliberately skip (`FlagsChanged`, `ItemMoved`, torn states).
pub fn translate_event(ev: &ContainerEvent, uids: &mut Vec<u64>) -> Option<String> {
    match ev {
        ContainerEvent::ItemAdded { seq, .. } => {
            let uid = uid_of(*seq);
            // Substrate publishes adds with strictly-increasing seq
            // per container, so the new UID is almost always the new
            // max. `partition_point` keeps us correct against torn or
            // out-of-order delivery without paying for a re-sort.
            let pos = uids.partition_point(|&u| u < uid);
            // Defensive: if this UID is already in the snapshot (a
            // double-fire across a Lagged-resync boundary, say), skip
            // the insert and the wire emission. Re-emitting EXISTS
            // for a UID the client already saw would inflate its
            // count by one.
            if uids.get(pos) == Some(&uid) {
                return None;
            }
            uids.insert(pos, uid);
            Some(format!("* {} EXISTS\r\n", uids.len()))
        }
        ContainerEvent::ItemRemoved { seq, .. } => {
            let uid = uid_of(*seq);
            match uids.binary_search(&uid) {
                Ok(idx) => {
                    uids.remove(idx);
                    Some(format!("* {} EXPUNGE\r\n", idx + 1))
                }
                Err(_) => None,
            }
        }
        // FlagsChanged: see module docs — would require an MSN
        // lookup we don't have cheaply. Client re-fetches on the
        // next NOOP/SELECT pass.
        ContainerEvent::FlagsChanged { .. } => None,
        // ItemMoved is additive (`ItemAdded`/`ItemRemoved` already
        // fire on the corresponding side); skip.
        ContainerEvent::ItemMoved { .. } => None,
    }
}

fn uid_of(seq: Seq) -> u64 {
    seq.0 as u64
}

/// Compute the EXPUNGE+EXISTS lines a client needs to transition
/// from `old` (its current view) to `new` (the substrate truth).
///
/// EXPUNGEs are emitted in **descending old-MSN order** so each
/// `* N EXPUNGE` line remains valid against the client's
/// pre-decrement view — RFC 9051 §7.4.1's "the client decrements
/// every subsequent message number by one" invariant only holds
/// when the server emits in descending order. After all EXPUNGEs
/// the function emits one `* <new.len()> EXISTS` line if `new` has
/// any UIDs the client did not previously see.
///
/// This is the load-bearing coherence primitive for both IDLE
/// entry (snapshot-twice diff) and Lagged-subscriber resync. A
/// plain `* N EXISTS` is *not* sufficient: if the skipped/raced
/// events include deletions the client must see the EXPUNGEs to
/// keep its UID→MSN map valid.
fn diff_lines(old: &[u64], new: &[u64]) -> Vec<String> {
    let new_set: std::collections::BTreeSet<u64> = new.iter().copied().collect();
    let old_set: std::collections::BTreeSet<u64> = old.iter().copied().collect();
    let mut out = Vec::new();

    // Walk `old` from right to left; for each UID not in `new`,
    // emit `* (idx+1) EXPUNGE` using the 1-based position in
    // `old`. Because we walk descending and the client decrements
    // after each, the emitted MSN sequence stays valid.
    for (i, uid) in old.iter().enumerate().rev() {
        if !new_set.contains(uid) {
            out.push(format!("* {} EXPUNGE\r\n", i + 1));
        }
    }

    // EXISTS only if `new` has UIDs `old` didn't, OR if
    // the count actually grew. The "additions only" check is
    // important: a pure-EXPUNGE diff should not also emit a
    // smaller EXISTS — the client already decrements its count
    // per EXPUNGE.
    let has_additions = new.iter().any(|u| !old_set.contains(u));
    if has_additions {
        out.push(format!("* {} EXISTS\r\n", new.len()));
    }

    out
}

/// Run the IDLE loop until the client sends `DONE`, EOF, or a
/// substrate event triggers BYE.
///
/// Wire contract emitted by this function:
///   * `+ idling\r\n` continuation right after argument validation.
///   * Zero or more untagged `EXISTS` / `EXPUNGE` lines as events
///     arrive.
///   * Untagged `* OK Still here\r\n` keepalive every
///     `keepalive_interval`.
///   * On clean DONE: tagged `<tag> OK IDLE terminated`.
///   * On bad line in place of DONE: tagged `<tag> BAD expected DONE`.
///   * On channel close: untagged `* BYE mailbox channel closed`
///     then return (the caller flips state to Logout).
///
/// `selected` is `Some` when the session is in `Selected(_)`; in
/// `Authenticated`-only, IDLE is still valid (RFC 2177 §3) — we
/// just don't subscribe to anything and emit no mailbox-state
/// events.
#[allow(clippy::too_many_arguments)]
pub async fn handle<R, W>(
    tag: &str,
    args: &str,
    selected: Option<&mut SelectedMailbox>,
    account_id: AccountId,
    store: Arc<dyn MailStore>,
    reader: &mut BufReader<R>,
    writer: &mut W,
    keepalive_interval: Duration,
) -> Result<IdleOutcome>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if let Err(e) = parse_args(args) {
        let resp = tagged(tag, Status::Bad, None, &e);
        writer.write_all(resp.as_bytes()).await?;
        writer.flush().await?;
        return Ok(IdleOutcome::Close);
    }

    // `sub` carries the broadcast receiver only; the per-session UID
    // view lives on `selected.view_uids` so EXPUNGE/MOVE-out
    // mutations made *outside* IDLE in earlier dispatches are
    // visible here, and so the recv-loop's emissions update the
    // very same vector the next IDLE entry will diff against.
    let mut sub: Option<broadcast::Receiver<ContainerEvent>> = None;
    let mailbox = selected.as_ref().map(|sm| sm.container_id);
    let mut selected = selected;

    if let Some(mb) = mailbox {
        // Subscribe first, then snapshot. Any event delivered
        // between subscribe and snapshot is reflected in `s2` and
        // also queued on `rx`; the recv loop drains the queued
        // copies idempotently (ItemAdded dedups, ItemRemoved on a
        // vanished UID is a no-op). Events delivered between
        // `selected.view_uids` (= what the session believes the
        // client has been told) and `subscribe` are covered by the
        // diff against `s2` below.
        let rx = match store.subscribe_mailbox(account_id, mb) {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(error = %e, "IDLE subscribe_mailbox failed");
                let resp = tagged(tag, Status::No, Some("SERVERBUG"), "IDLE subscribe failed");
                writer.write_all(resp.as_bytes()).await?;
                writer.flush().await?;
                return Ok(IdleOutcome::Done);
            }
        };
        let s2 = match snapshot_uids(store.clone(), account_id, mb).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "IDLE post-subscribe snapshot_uids failed");
                let resp = tagged(tag, Status::No, Some("SERVERBUG"), "IDLE snapshot failed");
                writer.write_all(resp.as_bytes()).await?;
                writer.flush().await?;
                return Ok(IdleOutcome::Done);
            }
        };
        sub = Some(rx);
        // Continuation must precede any untagged response while
        // IDLE is "starting up" — RFC 2177 §3 wire shape. After
        // `+ idling` the server is free to emit untagged updates
        // including the entry-diff.
        writer.write_all(b"+ idling\r\n").await?;
        writer.flush().await?;
        if let Some(sm) = selected.as_deref_mut() {
            for line in diff_lines(&sm.view_uids, &s2) {
                writer.write_all(line.as_bytes()).await?;
            }
            // Realign the session view to substrate truth at
            // recv-loop start so subsequent translate_event calls
            // mutate the same vector the next IDLE entry will diff.
            sm.view_uids = s2;
        }
        writer.flush().await?;
    } else {
        // Authenticated-state IDLE: no mailbox, no subscription.
        // Per RFC 2177 §3 we still emit the continuation and
        // keepalive ticks; emitting any untagged mailbox-state
        // event in this state would be a protocol error, which
        // we trivially achieve by not having a subscription.
        writer.write_all(b"+ idling\r\n").await?;
        writer.flush().await?;
    }

    let mut keepalive_timer = tokio::time::interval(keepalive_interval);
    // `interval` fires immediately on first tick — drop it so the
    // very first keepalive is one full interval into the IDLE.
    keepalive_timer.tick().await;

    loop {
        tokio::select! {
            biased;

            // 1. DONE / bad line from the client. Highest priority so
            // a client racing DONE against an event doesn't wait on a
            // notifier wake.
            line_res = codec::read_line(reader) => {
                match line_res {
                    Ok(line) => {
                        // Strip trailing CRLF before classifying. Non-UTF8
                        // input is treated as a framing violation rather
                        // than coerced to empty, so the BAD message points
                        // at the real cause and the bad_commands counter
                        // still increments.
                        let s = match std::str::from_utf8(&line) {
                            Ok(s) => s.trim_end_matches('\r').trim(),
                            Err(_) => {
                                let resp = tagged(
                                    tag,
                                    Status::Bad,
                                    None,
                                    "non-UTF8 input during IDLE",
                                );
                                writer.write_all(resp.as_bytes()).await?;
                                writer.flush().await?;
                                return Ok(IdleOutcome::Close);
                            }
                        };
                        if s.eq_ignore_ascii_case("DONE") {
                            let resp = tagged(tag, Status::Ok, None, "IDLE terminated");
                            writer.write_all(resp.as_bytes()).await?;
                            writer.flush().await?;
                            return Ok(IdleOutcome::Done);
                        }
                        // Per RFC 2177 §3 the only legal input during
                        // IDLE is `DONE`. Anything else is a protocol
                        // violation; emit BAD and close the session.
                        // Resuming dispatch from inside IDLE on a
                        // literal-shaped or otherwise malformed line
                        // is unsafe (stream framing may already be
                        // out of sync), so the conservative posture
                        // matches Cyrus / Dovecot — terminate.
                        let resp = tagged(tag, Status::Bad, None, "expected DONE");
                        writer.write_all(resp.as_bytes()).await?;
                        writer.flush().await?;
                        return Ok(IdleOutcome::Close);
                    }
                    Err(ReadLineError::Eof) => return Ok(IdleOutcome::Close),
                    Err(ReadLineError::LineTooLong) => {
                        let resp = tagged(tag, Status::Bad, None, "line too long");
                        writer.write_all(resp.as_bytes()).await?;
                        writer.flush().await?;
                        return Ok(IdleOutcome::Close);
                    }
                    Err(ReadLineError::Io(e)) => return Err(anyhow::Error::new(e)),
                }
            }

            // 2. Notifier event — only polled when we have a
            // subscription (Selected state). The guard suppresses
            // this branch in Authenticated-only IDLE so we don't
            // poll a `pending` future forever needlessly.
            event = recv_event(&mut sub), if sub.is_some() => {
                match event {
                    Ok(ev) => {
                        if let Some(sm) = selected.as_deref_mut()
                            && let Some(line) = translate_event(&ev, &mut sm.view_uids)
                        {
                            writer.write_all(line.as_bytes()).await?;
                            writer.flush().await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let alert = format!(
                            "* OK [ALERT] notifier lagged {n} events; resyncing\r\n"
                        );
                        writer.write_all(alert.as_bytes()).await?;
                        writer.flush().await?;
                        if let Some(mb) = mailbox {
                            match snapshot_uids(store.clone(), account_id, mb).await {
                                Ok(new_uids) => {
                                    if let Some(sm) = selected.as_deref_mut() {
                                        // Diff against the session
                                        // view so the client sees
                                        // EXPUNGEs for vanished UIDs
                                        // (descending MSN, RFC 9051
                                        // §7.4.1) plus a final EXISTS
                                        // only if new UIDs were
                                        // added. A bare EXISTS would
                                        // leave the client's MSN map
                                        // invalid when the skipped
                                        // events included deletions.
                                        for line in diff_lines(&sm.view_uids, &new_uids) {
                                            writer.write_all(line.as_bytes()).await?;
                                        }
                                        writer.flush().await?;
                                        sm.view_uids = new_uids;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "IDLE Lagged resync snapshot_uids failed"
                                    );
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Mailbox container was deleted out-of-band.
                        // Emit BYE + the tagged OK so the client sees
                        // a clean termination of the IDLE; return
                        // Close so the session loop closes the
                        // connection. The selected mailbox no longer
                        // exists; continuing the session against a
                        // stale `SelectedMailbox` would only spawn
                        // errors.
                        writer
                            .write_all(b"* BYE mailbox channel closed\r\n")
                            .await?;
                        let resp = tagged(tag, Status::Ok, None, "IDLE terminated");
                        writer.write_all(resp.as_bytes()).await?;
                        writer.flush().await?;
                        return Ok(IdleOutcome::Close);
                    }
                }
            }

            // 3. Keepalive — emit `* OK Still here\r\n` so clients
            // and intermediate NATs/firewalls see traffic and don't
            // reap the connection.
            _ = keepalive_timer.tick() => {
                writer.write_all(b"* OK Still here\r\n").await?;
                writer.flush().await?;
            }
        }
    }
}

async fn recv_event(
    sub: &mut Option<broadcast::Receiver<ContainerEvent>>,
) -> std::result::Result<ContainerEvent, broadcast::error::RecvError> {
    match sub.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// How the IDLE handler exited.
///
/// * `Done` — clean DONE; the session loop resumes normal command
///   dispatch.
/// * `Close` — terminal: the underlying reader hit EOF, the mailbox
///   channel closed (BYE already emitted), or the client sent a
///   non-DONE line / triggered a framing violation while idling
///   (tagged BAD already emitted). In all cases the session caller
///   should return; resuming dispatch on a possibly-desynced stream
///   is not safe and matches Cyrus/Dovecot's IDLE-violation posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    Done,
    Close,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mds::types::{ChangeToken, ItemId};
    use uuid::Uuid;

    fn item_added(uid: u32) -> ContainerEvent {
        ContainerEvent::ItemAdded {
            item_id: ItemId(Uuid::nil()),
            seq: Seq(uid),
            change_seq: ChangeToken(0),
        }
    }

    fn item_removed(uid: u32) -> ContainerEvent {
        ContainerEvent::ItemRemoved {
            item_id: ItemId(Uuid::nil()),
            seq: Seq(uid),
            change_seq: ChangeToken(0),
        }
    }

    #[test]
    fn parse_args_accepts_empty() {
        assert!(parse_args("").is_ok());
        assert!(parse_args("   ").is_ok());
    }

    #[test]
    fn parse_args_rejects_anything_else() {
        assert!(parse_args("foo").is_err());
        assert!(parse_args("1:5").is_err());
    }

    #[test]
    fn translate_item_added_emits_exists_with_new_len() {
        let mut uids = vec![1, 2, 5];
        let line = translate_event(&item_added(6), &mut uids).unwrap();
        assert_eq!(line, "* 4 EXISTS\r\n");
        assert_eq!(uids, vec![1, 2, 5, 6]);
    }

    #[test]
    fn translate_item_added_inserts_in_order() {
        // Out-of-order delivery is rare but `partition_point` keeps
        // us correct against it.
        let mut uids = vec![1, 5, 9];
        let line = translate_event(&item_added(3), &mut uids).unwrap();
        assert_eq!(line, "* 4 EXISTS\r\n");
        assert_eq!(uids, vec![1, 3, 5, 9]);
    }

    #[test]
    fn translate_item_added_skips_duplicate_uid() {
        let mut uids = vec![1, 2, 5];
        let line = translate_event(&item_added(2), &mut uids);
        assert!(line.is_none(), "duplicate UID must not re-emit EXISTS");
        assert_eq!(uids, vec![1, 2, 5]);
    }

    #[test]
    fn translate_item_removed_emits_expunge_with_1based_msn() {
        let mut uids = vec![10, 20, 30, 40];
        let line = translate_event(&item_removed(20), &mut uids).unwrap();
        // UID 20 is at index 1 (zero-based) → MSN 2 (one-based).
        assert_eq!(line, "* 2 EXPUNGE\r\n");
        assert_eq!(uids, vec![10, 30, 40]);
    }

    #[test]
    fn translate_item_removed_for_unknown_uid_drops_event() {
        let mut uids = vec![10, 20, 30];
        let line = translate_event(&item_removed(99), &mut uids);
        assert!(line.is_none());
        assert_eq!(uids, vec![10, 20, 30]);
    }

    #[test]
    fn translate_flags_changed_is_skipped_for_mvp() {
        let mut uids = vec![1, 2, 3];
        let ev = ContainerEvent::FlagsChanged {
            item_id: ItemId(Uuid::nil()),
            change_seq: ChangeToken(0),
        };
        assert!(translate_event(&ev, &mut uids).is_none());
        assert_eq!(uids, vec![1, 2, 3]);
    }

    #[test]
    fn diff_lines_empty_diff_is_empty() {
        let lines = diff_lines(&[1, 2, 3], &[1, 2, 3]);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn diff_lines_pure_addition_emits_only_exists() {
        let lines = diff_lines(&[1, 2], &[1, 2, 3, 4]);
        assert_eq!(lines, vec!["* 4 EXISTS\r\n".to_string()]);
    }

    #[test]
    fn diff_lines_pure_removal_emits_descending_expunge_no_exists() {
        // RFC 9051 §7.4.1: descending MSN order. UID 1 at MSN 1,
        // UID 3 at MSN 3, UID 5 at MSN 5. Both removed.
        let lines = diff_lines(&[1, 2, 3, 4, 5], &[2, 4]);
        assert_eq!(
            lines,
            vec![
                "* 5 EXPUNGE\r\n".to_string(),
                "* 3 EXPUNGE\r\n".to_string(),
                "* 1 EXPUNGE\r\n".to_string(),
            ]
        );
    }

    #[test]
    fn diff_lines_mixed_emits_expunges_then_exists() {
        // [1,2,3] → [2,3,4]: UID 1 removed (MSN 1), UID 4 added.
        // Expect `* 1 EXPUNGE` then `* 3 EXISTS`.
        let lines = diff_lines(&[1, 2, 3], &[2, 3, 4]);
        assert_eq!(
            lines,
            vec!["* 1 EXPUNGE\r\n".to_string(), "* 3 EXISTS\r\n".to_string(),]
        );
    }

    #[test]
    fn diff_lines_full_replacement_emits_all_expunges_then_exists() {
        let lines = diff_lines(&[1, 2, 3], &[10, 20]);
        assert_eq!(
            lines,
            vec![
                "* 3 EXPUNGE\r\n".to_string(),
                "* 2 EXPUNGE\r\n".to_string(),
                "* 1 EXPUNGE\r\n".to_string(),
                "* 2 EXISTS\r\n".to_string(),
            ]
        );
    }

    #[test]
    fn diff_lines_empty_to_empty_is_empty() {
        let lines = diff_lines(&[], &[]);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn diff_lines_empty_to_populated_emits_only_exists() {
        let lines = diff_lines(&[], &[1, 2, 3]);
        assert_eq!(lines, vec!["* 3 EXISTS\r\n".to_string()]);
    }

    #[test]
    fn diff_lines_populated_to_empty_emits_descending_expunges_only() {
        let lines = diff_lines(&[1, 2, 3], &[]);
        assert_eq!(
            lines,
            vec![
                "* 3 EXPUNGE\r\n".to_string(),
                "* 2 EXPUNGE\r\n".to_string(),
                "* 1 EXPUNGE\r\n".to_string(),
            ]
        );
    }

    #[test]
    fn descending_expunges_keep_msn_valid_against_pre_snapshot() {
        // RFC 9051 §7.4.1 — a server emitting multiple EXPUNGE
        // events must send them in descending MSN order so each
        // emitted MSN remains valid against the client's local map
        // *before* the prior EXPUNGE applied. The translator only
        // emits one EXPUNGE per event, but for a chain (UID 30,
        // then UID 20 from the substrate) the MSN math has to come
        // out so the client sees `* 3 EXPUNGE` then `* 2 EXPUNGE`
        // — not `* 3` then `* 3`. This is a smoke check that the
        // local-snapshot decrement keeps subsequent MSNs aligned
        // with where the client's view sits.
        let mut uids = vec![10, 20, 30, 40];
        let l1 = translate_event(&item_removed(30), &mut uids).unwrap();
        let l2 = translate_event(&item_removed(20), &mut uids).unwrap();
        assert_eq!(l1, "* 3 EXPUNGE\r\n");
        assert_eq!(l2, "* 2 EXPUNGE\r\n");
        assert_eq!(uids, vec![10, 40]);
    }
}
