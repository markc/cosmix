//! `SEARCH` / `UID SEARCH` — RFC 9051 §6.4.4 / §6.4.8.
//!
//! Scans the selected mailbox and emits `* SEARCH <msn|uid> ...` for
//! every message that satisfies the criteria expression. `UID SEARCH`
//! returns UIDs; bare `SEARCH` returns message sequence numbers.
//!
//! ## T13a scope
//!
//! Criteria implemented in this task:
//!
//! - `ALL`
//! - System flag predicates: `ANSWERED`, `UNANSWERED`, `DELETED`,
//!   `UNDELETED`, `DRAFT`, `UNDRAFT`, `FLAGGED`, `UNFLAGGED`, `SEEN`,
//!   `UNSEEN`
//! - `KEYWORD <name>`, `UNKEYWORD <name>` — user keywords, normalised to
//!   NFC + lowercase (the same fold STORE applies on write) and matched
//!   against per-membership tags. Per RFC 9051 §9 these take
//!   `flag-keyword`, a bare atom, so a `\`-prefixed argument is a syntax
//!   error rather than a system-flag search — unlike STORE, whose
//!   `flag-list` takes the wider `flag` production. Use the dedicated
//!   system keys (`SEEN`, `UNFLAGGED`, …) for the bits.
//! - `NEW` — always empty. RFC 9051 §6.4.4 defines `NEW = RECENT
//!   UNSEEN`; we don't track `\Recent` (no message carries it), so
//!   `RECENT` is always empty and `NEW` therefore must be too —
//!   collapsing `NEW` to plain `UNSEEN` would give clients false
//!   positives for any unread old mail.
//! - `OLD` — `NOT RECENT`. Since no message carries `\Recent`,
//!   every message is "old" → equivalent to `ALL`.
//! - `RECENT` — always empty (we don't track `\Recent`).
//! - `SMALLER <n>`, `LARGER <n>` — compares `size_bytes` to the literal.
//! - `UID <seq-set>` — restrict to UIDs in the given set.
//! - Bare `<seq-set>` — restrict to MSNs in the given set.
//! - `NOT <key>`, `OR <key1> <key2>`, `(<key1> <key2> ...)` —
//!   the standard combinators.
//!
//! Multiple top-level keys are AND-combined (RFC 9051 §6.4.4
//! "combining multiple search-keys with implicit AND").
//!
//! ## T13b scope (deferred)
//!
//! Header / date / body-text criteria — `FROM`, `TO`, `CC`, `BCC`,
//! `SUBJECT`, `HEADER name astring`, `BEFORE`, `ON`, `SINCE`,
//! `SENTBEFORE`, `SENTON`, `SENTSINCE`, `TEXT`, `BODY`. These require
//! parsing each message body and are deferred. They are rejected as
//! `BAD` rather than silently treated as `ALL` — silently dropping a
//! filter the client expects to honour would cause the wrong message
//! list to render (state-of-truth divergence with no IDLE-style
//! reconciliation channel for SEARCH).
//!
//! ## Substrate primitive
//!
//! `list_emails_in_mailbox` returns the full snapshot — `EmailHandle`
//! carries `uid`, `keywords`, `size_bytes`, `received_at`. T13a needs
//! no blob load. T13b (date / header / body) will add a per-row blob
//! fetch path mirroring `op::fetch`.
//!
//! ## Response shape
//!
//! ```text
//! * SEARCH 1 3 5 7
//! tag OK SEARCH completed
//! ```
//!
//! Empty match emits `* SEARCH\r\n` (no trailing space) per RFC 9051
//! §7.3.4.

use std::sync::Arc;

use cosmix_mds::{ContainerId, Flags};

use super::flags::{self, FlagToken};
use crate::imap::response::{Status, tagged};
use crate::imap::seq::SeqSet;
use crate::mailstore::{EmailHandle, ListOpts, MailStore, SortKey};

type AccountId = i32;

/// Whether the response emits message sequence numbers (`SEARCH`) or
/// UIDs (`UID SEARCH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Msn,
    Uid,
}

/// Criteria AST. `And` is the implicit combinator for a top-level
/// list of keys; `Or` and `Not` are the explicit RFC combinators.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Crit {
    /// Always true.
    All,
    /// Always false. Produced by `RECENT` (we never carry `\Recent`)
    /// and `NEW` (RFC 9051 §6.4.4 `NEW = RECENT UNSEEN`, which is
    /// empty whenever `RECENT` is empty).
    None,
    /// All named bits in `flags` must be set on the message.
    FlagsSet(Flags),
    /// None of the named bits in `flags` may be set on the message.
    FlagsClear(Flags),
    /// The normalised user keyword must be present in the message's tags.
    TagSet(String),
    /// `size_bytes < limit`.
    Smaller(u64),
    /// `size_bytes > limit`.
    Larger(u64),
    /// `msn` is a member of `set`.
    MsnInSet(SeqSet),
    /// `uid` is a member of `set`.
    UidInSet(SeqSet),
    /// All sub-keys must match.
    And(Vec<Crit>),
    /// Either sub-key matches.
    Or(Box<Crit>, Box<Crit>),
    /// The sub-key must not match.
    Not(Box<Crit>),
}

/// Parsed `SEARCH` / `UID SEARCH` request — the criteria expression
/// plus whether the response emits MSNs or UIDs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchReq {
    crit: Crit,
    mode: SearchMode,
}

/// Parse the argument portion of `SEARCH` / `UID SEARCH`.
fn parse_args(args: &str, mode: SearchMode) -> Result<SearchReq, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err("SEARCH requires at least one criterion".into());
    }
    let tokens = tokenize(trimmed)?;
    let mut idx = 0;
    if idx + 1 < tokens.len() && tokens[idx].eq_ignore_ascii_case("CHARSET") {
        idx += 2;
    }
    if idx < tokens.len() && tokens[idx].eq_ignore_ascii_case("RETURN") {
        return Err("SEARCH RETURN (ESEARCH) not implemented in this phase".into());
    }
    let mut keys: Vec<Crit> = Vec::new();
    while idx < tokens.len() {
        let (k, next) = parse_key(&tokens, idx)?;
        keys.push(k);
        idx = next;
    }
    if keys.is_empty() {
        return Err("SEARCH requires at least one criterion".into());
    }
    let crit = if keys.len() == 1 {
        keys.into_iter().next().unwrap()
    } else {
        Crit::And(keys)
    };
    Ok(SearchReq { crit, mode })
}

/// Pre-validate `args` so grammar failures can bump the abuse-cap
/// counter at the dispatch site.
pub fn parse_args_for_dispatch(args: &str, mode: SearchMode) -> Result<(), String> {
    parse_args(args, mode).map(|_| ())
}

fn tokenize(s: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '(' | ')' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            }
            '"' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                let mut q = String::new();
                let mut closed = false;
                while let Some(quoted) = chars.next() {
                    match quoted {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some(escaped) => q.push(escaped),
                            None => break,
                        },
                        _ => q.push(quoted),
                    }
                }
                if !closed {
                    return Err("SEARCH unterminated quoted string".into());
                }
                out.push(q);
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn parse_key(tokens: &[String], idx: usize) -> Result<(Crit, usize), String> {
    if idx >= tokens.len() {
        return Err("SEARCH unexpected end of expression".into());
    }
    let tok = &tokens[idx];
    let upper = tok.to_ascii_uppercase();
    match upper.as_str() {
        "(" => {
            let mut keys: Vec<Crit> = Vec::new();
            let mut cur = idx + 1;
            loop {
                if cur >= tokens.len() {
                    return Err("SEARCH missing closing paren".into());
                }
                if tokens[cur] == ")" {
                    if keys.is_empty() {
                        return Err("SEARCH empty parenthesised list".into());
                    }
                    let combined = if keys.len() == 1 {
                        keys.into_iter().next().unwrap()
                    } else {
                        Crit::And(keys)
                    };
                    return Ok((combined, cur + 1));
                }
                let (k, next) = parse_key(tokens, cur)?;
                keys.push(k);
                cur = next;
            }
        }
        ")" => Err("SEARCH stray closing paren".into()),
        "ALL" => Ok((Crit::All, idx + 1)),
        "ANSWERED" => Ok((Crit::FlagsSet(Flags(Flags::ANSWERED)), idx + 1)),
        "UNANSWERED" => Ok((Crit::FlagsClear(Flags(Flags::ANSWERED)), idx + 1)),
        "DELETED" => Ok((Crit::FlagsSet(Flags(Flags::DELETED)), idx + 1)),
        "UNDELETED" => Ok((Crit::FlagsClear(Flags(Flags::DELETED)), idx + 1)),
        "DRAFT" => Ok((Crit::FlagsSet(Flags(Flags::DRAFT)), idx + 1)),
        "UNDRAFT" => Ok((Crit::FlagsClear(Flags(Flags::DRAFT)), idx + 1)),
        "FLAGGED" => Ok((Crit::FlagsSet(Flags(Flags::FLAGGED)), idx + 1)),
        "UNFLAGGED" => Ok((Crit::FlagsClear(Flags(Flags::FLAGGED)), idx + 1)),
        "SEEN" => Ok((Crit::FlagsSet(Flags(Flags::SEEN)), idx + 1)),
        "UNSEEN" => Ok((Crit::FlagsClear(Flags(Flags::SEEN)), idx + 1)),
        // `NEW = RECENT UNSEEN`; with no `\Recent` tracking, RECENT
        // is always empty so NEW must also be empty. Mapping NEW to
        // plain UNSEEN would emit false positives for unread old
        // mail, divergent from a client's expected semantics.
        "NEW" => Ok((Crit::None, idx + 1)),
        "OLD" => Ok((Crit::All, idx + 1)),
        "RECENT" => Ok((Crit::None, idx + 1)),
        "KEYWORD" => {
            let name = tokens
                .get(idx + 1)
                .ok_or_else(|| "SEARCH KEYWORD missing argument".to_string())?;
            Ok((parse_keyword(name, false)?, idx + 2))
        }
        "UNKEYWORD" => {
            let name = tokens
                .get(idx + 1)
                .ok_or_else(|| "SEARCH UNKEYWORD missing argument".to_string())?;
            Ok((parse_keyword(name, true)?, idx + 2))
        }
        "SMALLER" => {
            let n = parse_number(tokens, idx + 1, "SMALLER")?;
            Ok((Crit::Smaller(n), idx + 2))
        }
        "LARGER" => {
            let n = parse_number(tokens, idx + 1, "LARGER")?;
            Ok((Crit::Larger(n), idx + 2))
        }
        "UID" => {
            let set_tok = tokens
                .get(idx + 1)
                .ok_or_else(|| "SEARCH UID missing sequence-set".to_string())?;
            let set = SeqSet::parse(set_tok)
                .map_err(|e| format!("SEARCH UID invalid sequence-set: {e}"))?;
            Ok((Crit::UidInSet(set), idx + 2))
        }
        "NOT" => {
            let (k, next) = parse_key(tokens, idx + 1)?;
            Ok((Crit::Not(Box::new(k)), next))
        }
        "OR" => {
            let (k1, n1) = parse_key(tokens, idx + 1)?;
            let (k2, n2) = parse_key(tokens, n1)?;
            Ok((Crit::Or(Box::new(k1), Box::new(k2)), n2))
        }
        "FROM" | "TO" | "CC" | "BCC" | "SUBJECT" | "HEADER" | "TEXT" | "BODY" | "BEFORE" | "ON"
        | "SINCE" | "SENTBEFORE" | "SENTON" | "SENTSINCE" => Err(format!(
            "SEARCH {upper} criterion deferred (T13b) — try ALL, flag predicates, UID, NOT, OR, SMALLER, LARGER"
        )),
        _ => match SeqSet::parse(tok) {
            Ok(set) => Ok((Crit::MsnInSet(set), idx + 1)),
            Err(_) => Err(format!("SEARCH unknown criterion {tok:?}")),
        },
    }
}

fn parse_number(tokens: &[String], idx: usize, key: &str) -> Result<u64, String> {
    let tok = tokens
        .get(idx)
        .ok_or_else(|| format!("SEARCH {key} missing number"))?;
    tok.parse::<u64>()
        .map_err(|_| format!("SEARCH {key} expected number, got {tok:?}"))
}

/// Parse the argument of `KEYWORD` / `UNKEYWORD`.
///
/// RFC 9051 §9 spells these `"KEYWORD" SP flag-keyword` and `"UNKEYWORD" SP
/// flag-keyword`, and `flag-keyword = "$MDNSent" / "$Forwarded" / "$Junk" /
/// "$NotJunk" / "$Phishing" / atom` — a bare atom that **cannot** begin with
/// `\`. That is deliberately narrower than STORE, whose `flag-list` takes the
/// full `flag` production (system `\`-atoms *and* keywords). So SEARCH must
/// NOT reuse STORE's [`flags::parse_flag_token`] discriminator wholesale: a
/// `\`-prefixed argument here is a syntax error, not a system-flag search.
///
/// Nothing is lost by rejecting it — every system flag has a dedicated search
/// key in both polarities (`SEEN`/`UNSEEN`, `FLAGGED`/`UNFLAGGED`,
/// `ANSWERED`/`UNANSWERED`, `DELETED`/`UNDELETED`, `DRAFT`/`UNDRAFT`), which is
/// precisely why the grammar reserves `flag-keyword` for keywords.
///
/// (§6.4.4's prose renders the placeholder as `KEYWORD <flag>`, an editorial
/// carry-over from RFC 3501 that contradicts its own §9 ABNF. The formal
/// syntax governs; the prose gloss "the specified **keyword** flag" agrees
/// with it in substance.)
///
/// The normaliser is still shared with STORE — the same NFC + lowercase fold
/// on both the write and read paths is what makes a stored tag findable.
fn parse_keyword(tok: &str, negated: bool) -> Result<Crit, String> {
    if tok.starts_with('\\') {
        return Err(format!(
            "SEARCH KEYWORD/UNKEYWORD takes a keyword, not a system flag, got {tok:?} \
             (RFC 9051 §9 flag-keyword) — use SEEN/UNSEEN, FLAGGED/UNFLAGGED, \
             ANSWERED/UNANSWERED, DELETED/UNDELETED or DRAFT/UNDRAFT"
        ));
    }
    let parsed =
        flags::parse_flag_token(tok).map_err(|e| format!("SEARCH invalid keyword {tok:?}: {e}"))?;
    Ok(match (parsed, negated) {
        // Unreachable: `parse_flag_token` only yields `System` for a
        // `\`-prefixed token, and those are rejected above. Kept total rather
        // than `unreachable!()` so a future change to the discriminator can
        // never turn a parse into a panic on a client-supplied string.
        (FlagToken::System(bit), false) => Crit::FlagsSet(Flags(bit)),
        (FlagToken::System(bit), true) => Crit::FlagsClear(Flags(bit)),
        (FlagToken::User(name), false) => Crit::TagSet(name),
        (FlagToken::User(name), true) => Crit::Not(Box::new(Crit::TagSet(name))),
    })
}

/// Evaluate a criterion against a single message. `exists` and
/// `largest_uid` are the mailbox-wide bounds needed to resolve `*`
/// inside `MsnInSet` / `UidInSet` per RFC 9051 §9.
fn matches(crit: &Crit, msn: u32, handle: &EmailHandle, exists: u32, largest_uid: u64) -> bool {
    match crit {
        Crit::All => true,
        Crit::None => false,
        Crit::FlagsSet(f) => (handle.keywords.0 & f.0) == f.0,
        Crit::FlagsClear(f) => (handle.keywords.0 & f.0) == 0,
        Crit::TagSet(name) => handle.tags.contains(name),
        Crit::Smaller(n) => handle.size_bytes < *n,
        Crit::Larger(n) => handle.size_bytes > *n,
        Crit::MsnInSet(set) => set.contains_msn(msn, exists),
        Crit::UidInSet(set) => set.contains_uid(handle.uid, largest_uid),
        Crit::And(ks) => ks
            .iter()
            .all(|k| matches(k, msn, handle, exists, largest_uid)),
        Crit::Or(a, b) => {
            matches(a, msn, handle, exists, largest_uid)
                || matches(b, msn, handle, exists, largest_uid)
        }
        Crit::Not(k) => !matches(k, msn, handle, exists, largest_uid),
    }
}

pub async fn handle(
    tag: &str,
    args: &str,
    account_id: AccountId,
    container: ContainerId,
    store: Arc<dyn MailStore>,
    mode: SearchMode,
) -> String {
    let req = match parse_args(args, mode) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::Bad, None, &e),
    };

    let opts = ListOpts {
        sort_by: SortKey::Seq,
        limit: u32::MAX,
        offset: 0,
    };
    let store_for_list = store.clone();
    let mut handles = match tokio::task::spawn_blocking(move || {
        store_for_list.list_emails_in_mailbox(account_id, container, opts)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "SEARCH list_emails_in_mailbox failed");
            return tagged(tag, Status::No, Some("SERVERBUG"), "search failed");
        }
        Err(e) => {
            tracing::warn!(error = %e, "SEARCH spawn_blocking panicked");
            return tagged(tag, Status::No, Some("SERVERBUG"), "search failed");
        }
    };

    // SortKey::Seq returns ascending-by-seq, which equals
    // ascending-by-UID for cosmix mailstore membership (Seq is the
    // u32 width of UID). Defensive sort + dedup here keeps MSN
    // assignment correct even if the storage backend ever returns
    // unsorted rows; matches the invariants the `SequenceMap`
    // constructor enforces (RFC 9051 §2.3.1 — UIDs strictly increase
    // by message position in a mailbox snapshot).
    handles.sort_by_key(|h| h.uid);
    handles.dedup_by_key(|h| h.uid);

    let exists = u32::try_from(handles.len()).unwrap_or(u32::MAX);
    let largest_uid = handles.last().map(|h| h.uid).unwrap_or(0);

    let mut hits: Vec<(u32, u64)> = Vec::with_capacity(handles.len());
    for (idx, handle) in handles.iter().enumerate() {
        let msn = (idx + 1) as u32;
        if matches(&req.crit, msn, handle, exists, largest_uid) {
            hits.push((msn, handle.uid));
        }
    }

    let mut out = String::new();
    out.push_str(&render_search_response(&hits, req.mode));
    let text = if req.mode == SearchMode::Uid {
        "UID SEARCH completed"
    } else {
        "SEARCH completed"
    };
    out.push_str(&tagged(tag, Status::Ok, None, text));
    out
}

fn render_search_response(hits: &[(u32, u64)], mode: SearchMode) -> String {
    if hits.is_empty() {
        return "* SEARCH\r\n".to_string();
    }
    let mut s = String::from("* SEARCH");
    for &(msn, uid) in hits {
        s.push(' ');
        match mode {
            SearchMode::Msn => s.push_str(&msn.to_string()),
            SearchMode::Uid => s.push_str(&uid.to_string()),
        }
    }
    s.push_str("\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mds::{BlobHash, ItemId, Seq, Tags};
    use uuid::Uuid;

    fn iid(seed: u64) -> ItemId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&seed.to_be_bytes());
        ItemId(Uuid::from_bytes(bytes))
    }

    fn make_handle(uid: u64, flags: u32, size: u64) -> EmailHandle {
        EmailHandle {
            id: iid(uid),
            blob_hash: BlobHash([0u8; 32]),
            keywords: Flags(flags),
            tags: Tags::new(),
            received_at: 0,
            seq: Seq(uid as u32),
            uid,
            mod_seq: 0,
            size_bytes: size,
        }
    }

    /// Test-only matcher with msn-bound and uid-bound defaulted from
    /// the handle so the property tests stay readable. Real callers
    /// pass the actual mailbox-wide bounds (see `handle()`).
    fn m(crit: &Crit, msn: u32, handle: &EmailHandle) -> bool {
        matches(crit, msn, handle, msn.max(1), handle.uid.max(1))
    }

    #[test]
    fn parse_all() {
        let r = parse_args("ALL", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::All);
    }

    #[test]
    fn parse_flag_predicates() {
        let r = parse_args("SEEN", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::FlagsSet(Flags(Flags::SEEN)));
        let r = parse_args("UNSEEN", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::FlagsClear(Flags(Flags::SEEN)));
        let r = parse_args("FLAGGED UNDELETED", SearchMode::Msn).unwrap();
        assert_eq!(
            r.crit,
            Crit::And(vec![
                Crit::FlagsSet(Flags(Flags::FLAGGED)),
                Crit::FlagsClear(Flags(Flags::DELETED)),
            ])
        );
    }

    #[test]
    fn parse_smaller_larger() {
        let r = parse_args("SMALLER 1024", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::Smaller(1024));
        let r = parse_args("LARGER 4096", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::Larger(4096));
    }

    #[test]
    fn parse_uid_and_seqset() {
        let r = parse_args("UID 1:10", SearchMode::Uid).unwrap();
        assert!(matches!(r.crit, Crit::UidInSet(_)));
        let r = parse_args("1:3", SearchMode::Msn).unwrap();
        assert!(matches!(r.crit, Crit::MsnInSet(_)));
    }

    #[test]
    fn parse_not_or() {
        let r = parse_args("NOT SEEN", SearchMode::Msn).unwrap();
        assert!(matches!(r.crit, Crit::Not(_)));
        let r = parse_args("OR SEEN FLAGGED", SearchMode::Msn).unwrap();
        assert!(matches!(r.crit, Crit::Or(_, _)));
    }

    #[test]
    fn parse_parenthesised_and() {
        let r = parse_args("(SEEN FLAGGED)", SearchMode::Msn).unwrap();
        match r.crit {
            Crit::And(ks) => assert_eq!(ks.len(), 2),
            _ => panic!("expected And"),
        }
    }

    /// RFC 9051 §9: `KEYWORD`/`UNKEYWORD` take `flag-keyword`, which is a bare
    /// atom — a `\`-prefixed system flag is a syntax error here, not a
    /// system-flag search. The dedicated keys cover that case.
    #[test]
    fn parse_keyword_rejects_system_flag_syntax() {
        for arg in ["KEYWORD \\Seen", "UNKEYWORD \\Flagged", "KEYWORD \\Nope"] {
            let e = parse_args(arg, SearchMode::Msn).unwrap_err();
            assert!(e.contains("not a system flag"), "{arg}: {e}");
            assert!(e.contains("SEEN/UNSEEN"), "{arg}: {e}");
        }

        // The dedicated search keys are what a client uses instead, and they
        // still resolve to the system bits.
        let r = parse_args("SEEN", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::FlagsSet(Flags(Flags::SEEN)));
        let r = parse_args("UNFLAGGED", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::FlagsClear(Flags(Flags::FLAGGED)));

        // A bare name is a user keyword even when it collides with a system
        // flag's name — `Flagged` the keyword is not `\Flagged` the bit.
        let r = parse_args("UNKEYWORD Flagged", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::Not(Box::new(Crit::TagSet("flagged".into()))));
    }

    #[test]
    fn parse_keyword_and_unkeyword_user_keyword() {
        let r = parse_args("KEYWORD Important", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::TagSet("important".into()));
        let r = parse_args("UNKEYWORD Important", SearchMode::Msn).unwrap();
        assert_eq!(
            r.crit,
            Crit::Not(Box::new(Crit::TagSet("important".into())))
        );
    }

    #[test]
    fn parse_keyword_case_folds_user_keyword() {
        for raw in ["ProjectX", "projectx", "PROJECTX"] {
            let r = parse_args(&format!("KEYWORD {raw}"), SearchMode::Msn).unwrap();
            assert_eq!(r.crit, Crit::TagSet("projectx".into()), "{raw}");
        }
    }

    #[test]
    fn parse_keyword_nfc_folds_user_keyword() {
        let precomposed = parse_args("KEYWORD café", SearchMode::Msn).unwrap();
        let decomposed = parse_args("KEYWORD cafe\u{301}", SearchMode::Msn).unwrap();
        assert_eq!(precomposed.crit, Crit::TagSet("café".into()));
        assert_eq!(decomposed.crit, precomposed.crit);
    }

    #[test]
    fn parse_keyword_accepts_registry_dollar_keyword() {
        let r = parse_args("KEYWORD $Junk", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::TagSet("$junk".into()));
    }

    #[test]
    fn parse_keyword_rejects_reserved_system_dollar_name() {
        let e = parse_args("KEYWORD $Seen", SearchMode::Msn).unwrap_err();
        assert!(e.contains("$Seen"), "{e}");
        assert!(e.contains("reserved system name"), "{e}");
    }

    #[test]
    fn parse_recent_old_new_aliases() {
        // NEW = RECENT UNSEEN; we don't track \Recent so NEW must be
        // empty too. Mapping NEW to plain UNSEEN would emit unread
        // *old* mail as `NEW`, which is wrong.
        let r = parse_args("NEW", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::None);
        let r = parse_args("OLD", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::All);
        let r = parse_args("RECENT", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::None);
    }

    #[test]
    fn parse_charset_prefix_is_ignored() {
        let r = parse_args("CHARSET UTF-8 SEEN", SearchMode::Msn).unwrap();
        assert_eq!(r.crit, Crit::FlagsSet(Flags(Flags::SEEN)));
    }

    #[test]
    fn parse_deferred_header_criterion_returns_bad() {
        let e = parse_args("SUBJECT hello", SearchMode::Msn).unwrap_err();
        assert!(e.contains("deferred"), "{e}");
    }

    #[test]
    fn parse_return_esearch_rejected() {
        let e = parse_args("RETURN (ALL) SEEN", SearchMode::Msn).unwrap_err();
        assert!(e.contains("ESEARCH"), "{e}");
    }

    #[test]
    fn parse_empty_args_rejected() {
        let e = parse_args("", SearchMode::Msn).unwrap_err();
        assert!(e.contains("at least one"), "{e}");
        let e = parse_args("   ", SearchMode::Msn).unwrap_err();
        assert!(e.contains("at least one"), "{e}");
    }

    #[test]
    fn parse_unterminated_paren_rejected() {
        let e = parse_args("(SEEN FLAGGED", SearchMode::Msn).unwrap_err();
        assert!(e.contains("closing paren"), "{e}");
    }

    #[test]
    fn matches_all_is_unconditional() {
        let h = make_handle(1, 0, 100);
        assert!(m(&Crit::All, 1, &h));
    }

    #[test]
    fn matches_flags_set_requires_all_named_bits() {
        let seen_flagged = make_handle(1, Flags::SEEN | Flags::FLAGGED, 100);
        let just_seen = make_handle(2, Flags::SEEN, 100);
        let both = Crit::FlagsSet(Flags(Flags::SEEN | Flags::FLAGGED));
        assert!(m(&both, 1, &seen_flagged));
        assert!(!m(&both, 2, &just_seen));
    }

    #[test]
    fn matches_flags_clear_rejects_when_any_named_bit_is_set() {
        let seen = make_handle(1, Flags::SEEN, 100);
        let none = make_handle(2, 0, 100);
        let unseen = Crit::FlagsClear(Flags(Flags::SEEN));
        assert!(!m(&unseen, 1, &seen));
        assert!(m(&unseen, 2, &none));
    }

    #[test]
    fn matches_keyword_and_unkeyword_against_tags() {
        let mut tagged = make_handle(1, 0, 100);
        tagged.tags.insert("projectx".into());
        let mut other = make_handle(2, 0, 100);
        other.tags.insert("other".into());
        let empty = make_handle(3, 0, 100);

        let keyword = parse_args("KEYWORD ProjectX", SearchMode::Msn)
            .unwrap()
            .crit;
        let unkeyword = parse_args("UNKEYWORD ProjectX", SearchMode::Msn)
            .unwrap()
            .crit;

        assert!(m(&keyword, 1, &tagged));
        assert!(!m(&keyword, 2, &other));
        assert!(!m(&keyword, 3, &empty));
        assert!(!m(&unkeyword, 1, &tagged));
        assert!(m(&unkeyword, 2, &other));
        assert!(m(&unkeyword, 3, &empty));
    }

    #[test]
    fn matches_smaller_larger_comparisons() {
        let h = make_handle(1, 0, 1000);
        assert!(m(&Crit::Smaller(2000), 1, &h));
        assert!(!m(&Crit::Smaller(1000), 1, &h));
        assert!(m(&Crit::Larger(500), 1, &h));
        assert!(!m(&Crit::Larger(1000), 1, &h));
    }

    #[test]
    fn matches_not_or_and_combinators() {
        let h = make_handle(1, Flags::SEEN, 100);
        let not_seen = Crit::Not(Box::new(Crit::FlagsSet(Flags(Flags::SEEN))));
        assert!(!m(&not_seen, 1, &h));
        let or = Crit::Or(
            Box::new(Crit::FlagsSet(Flags(Flags::FLAGGED))),
            Box::new(Crit::FlagsSet(Flags(Flags::SEEN))),
        );
        assert!(m(&or, 1, &h));
        let and = Crit::And(vec![Crit::FlagsSet(Flags(Flags::SEEN)), Crit::Smaller(200)]);
        assert!(m(&and, 1, &h));
    }

    #[test]
    fn matches_msn_in_set_respects_exists() {
        let h = make_handle(1, 0, 100);
        let set = SeqSet::parse("2:*").unwrap();
        let crit = Crit::MsnInSet(set);
        // exists=3, msn=2 → in set
        assert!(matches(&crit, 2, &h, 3, 1));
        // exists=3, msn=1 → not in set (range is 2..=3)
        assert!(!matches(&crit, 1, &h, 3, 1));
    }

    #[test]
    fn matches_uid_in_set_respects_largest_uid() {
        let h = make_handle(30, 0, 100);
        // RFC 9051 §6.4.8: `3291:*` covers the largest uid even if
        // out of nominal range.
        let set = SeqSet::parse("3291:*").unwrap();
        let crit = Crit::UidInSet(set);
        assert!(matches(&crit, 3, &h, 3, 30));
    }

    #[test]
    fn render_empty_emits_bare_search() {
        assert_eq!(render_search_response(&[], SearchMode::Msn), "* SEARCH\r\n");
        assert_eq!(render_search_response(&[], SearchMode::Uid), "* SEARCH\r\n");
    }

    #[test]
    fn render_emits_msn_for_search_mode() {
        let hits = [(1u32, 100u64), (3, 102), (5, 104)];
        assert_eq!(
            render_search_response(&hits, SearchMode::Msn),
            "* SEARCH 1 3 5\r\n"
        );
    }

    #[test]
    fn render_emits_uid_for_uid_search_mode() {
        let hits = [(1u32, 100u64), (3, 102), (5, 104)];
        assert_eq!(
            render_search_response(&hits, SearchMode::Uid),
            "* SEARCH 100 102 104\r\n"
        );
    }
}
