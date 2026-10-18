//! `STORE` / `UID STORE` — RFC 9051 §6.4.6 / §6.4.8.
//!
//! Mutates per-message system flags on the selected mailbox.
//!
//! ## Wire shape
//!
//! ```text
//! <tag> STORE <seqset> [+|-]FLAGS[.SILENT] <flag-list>
//! <tag> UID STORE <uidset> [+|-]FLAGS[.SILENT] <flag-list>
//! ```
//!
//! `<flag-list>` is either a single system flag (`\Seen`) or a
//! parenthesised space-separated list (`(\Seen \Flagged)`). The leading
//! sigil controls the merge:
//!
//! * `FLAGS` — replace the flag set with exactly `<flag-list>`.
//! * `+FLAGS` — set the bits in `<flag-list>` (leave others alone).
//! * `-FLAGS` — clear the bits in `<flag-list>` (leave others alone).
//!
//! The `.SILENT` suffix suppresses the per-message `* N FETCH (FLAGS
//! (...))` untagged response that normally accompanies a successful
//! store; the tagged `OK` is still emitted.
//!
//! ## Substrate primitive
//!
//! Per-message writes go through
//! [`crate::mailstore::MailStore::set_keywords_in_mailbox`], the
//! per-mailbox sibling of `set_keywords`. This honours IMAP's
//! non-negotiable invariant (`_doc/maild/imap.md` §5/§6): `\Seen` set
//! on the INBOX membership of a multi-mailbox email must NOT propagate
//! to the Archive membership.
//!
//! ## Scope
//!
//! Phase 4 T10 ships the five RFC 9051 §2.3.2 system flags
//! (`\Seen`, `\Flagged`, `\Answered`, `\Draft`, `\Deleted`). User
//! keywords (`Tags` in the substrate) are deferred to a later task.
//! `UNCHANGEDSINCE` (CONDSTORE) is deferred to Phase 6.
//!
//! Unknown flag atoms are rejected as `BAD` rather than silently
//! ignored: silently dropping a flag the client believes it stored is
//! a state-of-truth divergence the IDLE FETCH echo cannot reconcile.

use std::collections::HashMap;
use std::sync::Arc;

use cosmix_mds::{ContainerId, Flags, ItemId, Tags};

use crate::imap::response::{Status, tagged};
use crate::imap::seq::{Resolved, SeqSet, SequenceMap};
use crate::mailstore::{EmailHandle, ListOpts, MailStore, SortKey};

type AccountId = i32;

/// Whether the sequence set is interpreted as MSN (`STORE`) or
/// UID (`UID STORE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    Msn,
    Uid,
}

/// How `<flag-list>` is merged with the current flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreOp {
    /// `FLAGS` — replace the entire (substrate-known) flag bitmap.
    Replace,
    /// `+FLAGS` — set the named bits, leave others.
    Add,
    /// `-FLAGS` — clear the named bits, leave others.
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoreReq {
    set: SeqSet,
    op: StoreOp,
    silent: bool,
    /// Bitmap of the system flags named in `<flag-list>`. Order doesn't
    /// matter; duplicates are folded.
    flags: Flags,
    /// User keywords (bare atoms) named in `<flag-list>`, normalised.
    tags: Tags,
    mode: StoreMode,
}

/// Parse `STORE` / `UID STORE` arguments per RFC 9051 §6.4.6.
///
/// Wire grammar (after the verb has been stripped):
///
/// ```text
/// args      = seq-set SP store-att-flags
/// store-att-flags = ("FLAGS" / "+FLAGS" / "-FLAGS") [".SILENT"]
///                   SP (flag-list / flag *(SP flag))
/// flag-list = "(" [flag *(SP flag)] ")"
/// flag      = "\Answered" / "\Flagged" / "\Deleted" / "\Seen" / "\Draft"
/// ```
fn parse_args(args: &str, mode: StoreMode) -> Result<StoreReq, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err("STORE requires sequence-set and flag-list".into());
    }

    // Split off the sequence-set (first whitespace-delimited token).
    let (seq_str, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => return Err("STORE missing flag-list".into()),
    };
    let set = SeqSet::parse(seq_str).map_err(|e| format!("STORE invalid sequence-set: {e}"))?;

    // Split off the store-att token: "FLAGS", "+FLAGS", "-FLAGS",
    // each optionally followed by ".SILENT".
    let (att_tok, flag_str) = match rest.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => return Err("STORE missing flag-list".into()),
    };
    let (op, silent) = parse_store_att(att_tok)?;

    if flag_str.is_empty() {
        return Err("STORE missing flag-list".into());
    }
    let (flags, tags) = parse_flag_list(flag_str)?;

    Ok(StoreReq {
        set,
        op,
        silent,
        flags,
        tags,
        mode,
    })
}

/// Parse the `FLAGS` / `+FLAGS` / `-FLAGS` token, optionally followed
/// by `.SILENT`. Case-insensitive per RFC 9051 §6.4.6.
fn parse_store_att(tok: &str) -> Result<(StoreOp, bool), String> {
    let (op_tok, silent) = if let Some(prefix) = strip_silent_suffix(tok) {
        (prefix, true)
    } else {
        (tok, false)
    };
    let op = if op_tok.eq_ignore_ascii_case("FLAGS") {
        StoreOp::Replace
    } else if op_tok.eq_ignore_ascii_case("+FLAGS") {
        StoreOp::Add
    } else if op_tok.eq_ignore_ascii_case("-FLAGS") {
        StoreOp::Remove
    } else {
        return Err(format!(
            "STORE: expected FLAGS/+FLAGS/-FLAGS, got {op_tok:?}"
        ));
    };
    Ok((op, silent))
}

/// Return the prefix before `.SILENT` if present (case-insensitive).
fn strip_silent_suffix(tok: &str) -> Option<&str> {
    let lower = tok.to_ascii_lowercase();
    if let Some(idx) = lower.find(".silent")
        && idx + ".silent".len() == lower.len()
    {
        return Some(&tok[..idx]);
    }
    None
}

/// Parse a flag-list: either a single bare atom or a parenthesised list,
/// splitting system flags from user keywords (RFC 9051 §2.3.2). Empty list
/// `()` is a well-formed no-op (`(Flags(0), Tags::new())`). Atom parsing +
/// keyword normalisation is the shared [`super::flags`] implementation that
/// STORE / APPEND / FETCH all use.
fn parse_flag_list(s: &str) -> Result<(Flags, Tags), String> {
    let inner = if let Some(rest) = s.strip_prefix('(') {
        let rest = rest
            .strip_suffix(')')
            .ok_or_else(|| format!("STORE flag-list missing closing paren: {s:?}"))?;
        rest.trim()
    } else {
        // Bare single flag; whole `s` is one token.
        if s.contains(')') {
            return Err(format!("STORE flag-list stray paren: {s:?}"));
        }
        s.trim()
    };
    super::flags::parse_flag_list_inner(inner).map_err(|e| format!("STORE: {e}"))
}

/// Compute the new flag bitmap from the current state + the operation.
fn apply_op(current: Flags, op: StoreOp, named: Flags) -> Flags {
    match op {
        // Replace only touches the allocated bits — preserve any
        // unallocated bits the substrate may have set. For now
        // `ALL_ALLOCATED` is the entire system-flag surface so this
        // collapses to `named`.
        StoreOp::Replace => Flags((current.0 & !Flags::ALL_ALLOCATED) | named.0),
        StoreOp::Add => Flags(current.0 | named.0),
        StoreOp::Remove => Flags(current.0 & !named.0),
    }
}

/// Compute the new user-keyword set from the current state + the operation —
/// the `Tags` counterpart of [`apply_op`], with the same `FLAGS` /
/// `+FLAGS` / `-FLAGS` semantics.
fn apply_op_tags(current: &Tags, op: StoreOp, named: &Tags) -> Tags {
    match op {
        StoreOp::Replace => named.clone(),
        StoreOp::Add => {
            let mut out = current.clone();
            for t in named.iter() {
                out.insert(t.clone());
            }
            out
        }
        StoreOp::Remove => {
            let mut out = current.clone();
            for t in named.iter() {
                out.0.remove(t);
            }
            out
        }
    }
}

/// Pre-validate `<args>` before invoking [`handle`] so grammar failures
/// can bump the session's `bad_commands` abuse-cap counter. Returns
/// `Ok(())` when [`parse_args`] would succeed.
pub fn parse_args_for_dispatch(args: &str, mode: StoreMode) -> Result<(), String> {
    parse_args(args, mode).map(|_| ())
}

/// Execute STORE / UID STORE for a selected mailbox.
///
/// Caller has already validated `Selected(_)` state, resolved the
/// container, parsed via [`parse_args`] (used by `session::dispatch`
/// for the abuse-cap pre-check), and gated the verb against the
/// session state machine.
pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    container: ContainerId,
    store: Arc<dyn MailStore>,
    mode: StoreMode,
) -> String {
    let req = match parse_args(args, mode) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };

    // Snapshot the mailbox so we can resolve msn/uid → ItemId AND so
    // each touched message's current keywords are read once. Same
    // approach FETCH uses (see `op::fetch::handle`).
    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let store_for_list = store.clone();
    let handles = match tokio::task::spawn_blocking(move || {
        store_for_list.list_emails_in_mailbox(account_id, container, opts)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "STORE list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "store failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "STORE spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "store failed");
        }
    };

    let by_id: HashMap<ItemId, EmailHandle> = handles.iter().map(|h| (h.id, h.clone())).collect();
    let map = SequenceMap::from_entries(handles.iter().map(|h| (h.uid, h.id)));

    // Resolve sequence set. UID STORE `*` / `<n>:*` on an empty
    // mailbox: see fetch.rs for the matching sentinel rationale.
    let resolved: Vec<Resolved> = match req.mode {
        StoreMode::Msn => map.resolve_msn(&req.set),
        StoreMode::Uid => map.resolve_uid(&req.set, 0),
    };

    let mut out = String::new();
    for r in &resolved {
        let cur_handle = match by_id.get(&r.item_id) {
            Some(h) => h,
            None => continue, // Shouldn't happen — resolver only returns snapshot ids.
        };
        let new_flags = apply_op(cur_handle.keywords, req.op, req.flags);
        let new_tags = apply_op_tags(&cur_handle.tags, req.op, &req.tags);

        // The flag-list parser caps the *incoming* keyword count, but a
        // `+FLAGS` union with the membership's existing tags can still push the
        // persisted set over the cap. Re-check the final set so a sequence of
        // `+FLAGS` commands can't bloat the row; reject the STORE with NO
        // rather than silently truncating.
        if new_tags.len() > crate::keyword::MAX_KEYWORDS_PER_MESSAGE {
            out.push_str(&tagged(
                tag,
                Status::No,
                None,
                "STORE: too many keywords on message",
            ));
            return out;
        }

        // Skip the substrate write if NEITHER flags nor user keywords
        // change. This also avoids a redundant `mds.item.flagged`
        // notifier emission that would wake IDLE listeners for no
        // observable delta. A tags-only change must NOT be skipped.
        if new_flags == cur_handle.keywords && new_tags == cur_handle.tags {
            if !req.silent {
                out.push_str(&render_fetch_flags_row(
                    r.msn,
                    cur_handle.uid,
                    new_flags,
                    &new_tags,
                    req.mode,
                ));
            }
            continue;
        }

        let store_for_set = store.clone();
        let item_id = r.item_id;
        // `set_keywords_in_mailbox` REPLACES both the flags and the tags
        // column with the post-STORE values computed above (`apply_op` /
        // `apply_op_tags` already folded the +/-/replace op against the
        // membership's current keywords), so per-mailbox isolation holds and
        // a JMAP-created user keyword is preserved unless this STORE changed it.
        let write_tags = new_tags.clone();
        let write_res = tokio::task::spawn_blocking(move || {
            store_for_set
                .set_keywords_in_mailbox(account_id, item_id, container, new_flags, write_tags)
        })
        .await;

        match write_res {
            Ok(Ok(())) => {
                if !req.silent {
                    out.push_str(&render_fetch_flags_row(
                        r.msn,
                        cur_handle.uid,
                        new_flags,
                        &new_tags,
                        req.mode,
                    ));
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "STORE set_keywords_in_mailbox failed");
                // Per RFC 9051 §6.4.6 a partial STORE failure is
                // reported as tagged NO; emit whatever FETCH rows
                // already succeeded so the client can reconcile.
                out.push_str(&tagged(tag, Status::No, None, "store failed"));
                return out;
            }
            Err(e) => {
                tracing::warn!(error = %e, "STORE spawn_blocking panicked");
                out.push_str(&tagged(tag, Status::No, Some("SERVERBUG"), "store failed"));
                return out;
            }
        }
    }

    let text = if req.mode == StoreMode::Uid {
        "UID STORE completed"
    } else {
        "STORE completed"
    };
    out.push_str(&tagged(tag, Status::Ok, None, text));
    out
}

/// Render `* <msn> FETCH (FLAGS (...) [UID <n>])` per RFC 9051 §6.4.6.
/// UID STORE MUST include `UID` in the echo response so the client can
/// correlate without an `EXISTS` snapshot — same rule as UID FETCH.
fn render_fetch_flags_row(
    msn: u32,
    uid: u64,
    flags: Flags,
    tags: &Tags,
    mode: StoreMode,
) -> String {
    let flags_str = super::flags::render_keywords(flags, tags);
    match mode {
        StoreMode::Uid => format!("* {msn} FETCH (FLAGS {flags_str} UID {uid})\r\n"),
        StoreMode::Msn => format!("* {msn} FETCH (FLAGS {flags_str})\r\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_replace_single_flag_msn() {
        let r = parse_args("1:5 FLAGS \\Seen", StoreMode::Msn).unwrap();
        assert_eq!(r.op, StoreOp::Replace);
        assert!(!r.silent);
        assert_eq!(r.flags, Flags(Flags::SEEN));
        assert_eq!(r.mode, StoreMode::Msn);
    }

    #[test]
    fn parse_args_plus_flags_silent_list() {
        let r = parse_args("1 +FLAGS.SILENT (\\Seen \\Flagged)", StoreMode::Msn).unwrap();
        assert_eq!(r.op, StoreOp::Add);
        assert!(r.silent);
        assert_eq!(r.flags, Flags(Flags::SEEN | Flags::FLAGGED));
    }

    #[test]
    fn parse_args_minus_flags() {
        let r = parse_args("1:* -FLAGS (\\Deleted)", StoreMode::Uid).unwrap();
        assert_eq!(r.op, StoreOp::Remove);
        assert_eq!(r.flags, Flags(Flags::DELETED));
        assert_eq!(r.mode, StoreMode::Uid);
    }

    #[test]
    fn parse_args_case_insensitive_atts_and_atoms() {
        let r = parse_args("1 flags.SILENT (\\seen \\FLAGGED)", StoreMode::Msn).unwrap();
        assert_eq!(r.op, StoreOp::Replace);
        assert!(r.silent);
        assert_eq!(r.flags, Flags(Flags::SEEN | Flags::FLAGGED));
    }

    #[test]
    fn parse_args_all_system_flags() {
        let r = parse_args(
            "1 FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)",
            StoreMode::Msn,
        )
        .unwrap();
        assert_eq!(r.flags, Flags(Flags::ALL_ALLOCATED));
    }

    #[test]
    fn parse_args_empty_paren_list_is_replace_to_zero() {
        let r = parse_args("1 FLAGS ()", StoreMode::Msn).unwrap();
        assert_eq!(r.op, StoreOp::Replace);
        assert_eq!(r.flags, Flags(0));
    }

    #[test]
    fn parse_args_rejects_empty() {
        let e = parse_args("", StoreMode::Msn).unwrap_err();
        assert!(e.contains("sequence-set"), "{e}");
    }

    #[test]
    fn parse_args_rejects_missing_flag_list() {
        let e = parse_args("1 FLAGS", StoreMode::Msn).unwrap_err();
        assert!(e.contains("flag-list"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unknown_att() {
        let e = parse_args("1 KEYWORDS \\Seen", StoreMode::Msn).unwrap_err();
        assert!(e.contains("FLAGS"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let e = parse_args("1 FLAGS \\Recent", StoreMode::Msn).unwrap_err();
        assert!(e.contains("\\Recent") || e.contains("Recent"), "{e}");
    }

    #[test]
    fn parse_args_accepts_user_and_registry_keywords_rejects_system_dollar() {
        // Bare atoms are user keywords (normalised into `tags`), leaving `flags`
        // untouched — including registry `$`-keywords like Thunderbird's tags.
        let r = parse_args("1 FLAGS (\\Seen Important $label2)", StoreMode::Msn).unwrap();
        assert_eq!(r.flags, Flags(Flags::SEEN));
        assert!(r.tags.contains("important"));
        assert!(r.tags.contains("$label2"));
        // Only a bare SYSTEM `$`-name is reserved → rejected.
        let e = parse_args("1 FLAGS $seen", StoreMode::Msn).unwrap_err();
        assert!(e.contains("$seen"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unclosed_paren() {
        let e = parse_args("1 FLAGS (\\Seen", StoreMode::Msn).unwrap_err();
        assert!(e.contains("closing paren"), "{e}");
    }

    #[test]
    fn parse_args_rejects_bad_sequence_set() {
        let e = parse_args("abc FLAGS \\Seen", StoreMode::Msn).unwrap_err();
        assert!(e.contains("sequence-set"), "{e}");
    }

    // ---- apply_op ----

    #[test]
    fn apply_op_replace_clears_other_allocated_bits() {
        let cur = Flags(Flags::SEEN | Flags::FLAGGED);
        let out = apply_op(cur, StoreOp::Replace, Flags(Flags::ANSWERED));
        assert_eq!(out, Flags(Flags::ANSWERED));
    }

    #[test]
    fn apply_op_add_preserves_existing() {
        let cur = Flags(Flags::SEEN);
        let out = apply_op(cur, StoreOp::Add, Flags(Flags::FLAGGED));
        assert_eq!(out, Flags(Flags::SEEN | Flags::FLAGGED));
    }

    #[test]
    fn apply_op_remove_leaves_unmentioned_alone() {
        let cur = Flags(Flags::SEEN | Flags::FLAGGED | Flags::DRAFT);
        let out = apply_op(cur, StoreOp::Remove, Flags(Flags::FLAGGED));
        assert_eq!(out, Flags(Flags::SEEN | Flags::DRAFT));
    }

    #[test]
    fn apply_op_replace_preserves_unallocated_bits_for_forward_compat() {
        // Bit 31 not allocated to any system flag — Replace should not
        // touch it. Guards against a future user-keyword bit getting
        // accidentally cleared by a `STORE FLAGS (...)` system-flag
        // mutation.
        let opaque = 1u32 << 31;
        let cur = Flags(Flags::SEEN | opaque);
        let out = apply_op(cur, StoreOp::Replace, Flags(Flags::FLAGGED));
        assert_eq!(out.0, Flags::FLAGGED | opaque);
    }

    // ---- apply_op_tags ----

    #[test]
    fn apply_op_tags_replace_add_remove() {
        let cur = Tags::from(vec!["work".to_string(), "old".to_string()]);
        // Replace = the named set verbatim.
        assert_eq!(
            apply_op_tags(&cur, StoreOp::Replace, &Tags::from(vec!["new".to_string()])),
            Tags::from(vec!["new".to_string()])
        );
        // Add = union.
        assert_eq!(
            apply_op_tags(&cur, StoreOp::Add, &Tags::from(vec!["fresh".to_string()])),
            Tags::from(vec![
                "work".to_string(),
                "old".to_string(),
                "fresh".to_string()
            ])
        );
        // Remove = difference.
        assert_eq!(
            apply_op_tags(&cur, StoreOp::Remove, &Tags::from(vec!["old".to_string()])),
            Tags::from(vec!["work".to_string()])
        );
    }

    // ---- render_fetch_flags_row ----

    #[test]
    fn render_fetch_flags_row_uid_mode_includes_uid_atom() {
        let row = render_fetch_flags_row(3, 42, Flags(Flags::SEEN), &Tags::new(), StoreMode::Uid);
        assert_eq!(row, "* 3 FETCH (FLAGS (\\Seen) UID 42)\r\n");
    }

    #[test]
    fn render_fetch_flags_row_renders_user_keywords() {
        let tags = Tags::from(vec!["important".to_string()]);
        let row = render_fetch_flags_row(3, 42, Flags(Flags::SEEN), &tags, StoreMode::Msn);
        assert_eq!(row, "* 3 FETCH (FLAGS (\\Seen important))\r\n");
    }
}
