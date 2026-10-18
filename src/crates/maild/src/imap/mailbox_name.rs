//! Mailbox-name encoder / decoder for IMAP Phase 2 (T3).
//!
//! Bridges the gap between [`crate::mailstore::MailboxRecord`] —
//! flat `name` + `parent: Option<ContainerId>` graph — and the
//! IMAP wire shape that LIST / LSUB / NAMESPACE / SELECT need.
//!
//! Pure module: no I/O, no MailStore reference, no `tokio`. Takes
//! `&[MailboxRecord]` slices and returns owned `String` /
//! structured rows. The wire handlers (`op/list.rs`, `op/lsub.rs`,
//! `op/select.rs` in T4) own the I/O; this module owns the naming
//! rules so they can be tested in isolation.
//!
//! # Design (locked Phase 2 T3a)
//!
//! * **Hierarchy delimiter**: `/`. Advertised in NAMESPACE.
//! * **INBOX**: the container whose `role == Some(MailboxRole::Inbox)`
//!   is exposed on the wire as the literal `INBOX` (uppercase),
//!   regardless of its stored `name` or `parent`. Decode matches
//!   `INBOX` case-insensitively to the role container.
//! * **Charset**: ASCII-only on the wire in v1. v1's advertised
//!   capability set does **not** include `UTF8=ACCEPT`, so any
//!   mailbox whose stored `name` contains a non-ASCII byte is
//!   *unrepresentable* over IMAP and is omitted from LIST / LSUB
//!   with a logged warning; SELECT / STATUS on such names is the
//!   wire handler's responsibility to reject with
//!   `NO [SERVERBUG] mailbox name not representable over IMAP`.
//! * **Internal delimiter conflict**: a stored segment containing
//!   the literal `/` is similarly *unrepresentable* — emitting it
//!   would lie about hierarchy. Same omission/`NO [SERVERBUG]`
//!   rule as non-ASCII.
//! * **CHILDREN**: derived from the input slice — a single O(N)
//!   parent-id-count pass attaches `\HasChildren` / `\HasNoChildren`
//!   per row.
//!
//! No CREATE/RENAME validation lives here yet — that is T5's job.

use std::collections::HashMap;

use cosmix_mds::ContainerId;

use crate::mailstore::{MailboxRecord, MailboxRole};

/// The fixed IMAP hierarchy delimiter advertised in NAMESPACE and
/// emitted between segments in LIST / LSUB wire names.
pub const HIERARCHY_DELIMITER: char = '/';

/// One mailbox row ready to be formatted into an IMAP LIST / LSUB
/// response. Carries the IMAP-attribute facts the handler needs so
/// the formatter never has to re-walk the record set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedMailbox {
    pub id: ContainerId,
    /// Wire-visible mailbox name. For the role-Inbox container this
    /// is always the literal `INBOX`; for every other container it
    /// is the slash-joined walk from the root of the tree.
    pub wire_name: String,
    /// Maps to `\HasChildren` / `\HasNoChildren`.
    pub has_children: bool,
    /// Maps to the SPECIAL-USE attribute on LIST (`\Inbox`,
    /// `\Archive`, …) when present. The role-Inbox container also
    /// carries `\Inbox`; the wire name `INBOX` is a separate
    /// per-RFC-9051 contract, not a substitute for the attribute.
    pub role: Option<MailboxRole>,
    /// Mirrored from `MailboxRecord::is_subscribed` so the LSUB
    /// handler does not need to re-look-up the record.
    pub is_subscribed: bool,
}

/// One mailbox row that cannot be represented over IMAP in v1.
/// Wire handlers should `tracing::warn!` on these and either omit
/// (LIST / LSUB) or reject (SELECT / STATUS — T4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedMailbox {
    pub id: ContainerId,
    pub stored_name: String,
    pub reason: OmissionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmissionReason {
    /// Stored `name` contains a byte outside printable ASCII.
    NonAsciiName,
    /// Stored `name` contains the hierarchy delimiter literally.
    NameContainsDelimiter,
    /// Stored `name` contains a byte the active wire-charset reserves
    /// for an escape introducer. Concretely: `&` is the modified-UTF-7
    /// introducer in RFC 3501 (IMAP4rev1). Because we advertise both
    /// `IMAP4rev1` and `IMAP4rev2` from one wire and have no UTF-7
    /// encoder yet, emitting `R&D` literally would render as the
    /// IMAP4rev1 modified-UTF-7 sequence `R<encoded>D` on a strict
    /// `rev1` client. The Phase 2 stance is to omit; users rename or
    /// wait for UTF8=ACCEPT (T6+) to round-trip arbitrary names.
    NameContainsCharsetReserved,
    /// `parent` chain points outside the record set or forms a
    /// cycle. `list_mailboxes` is expected not to surface either
    /// of these, but the encoder refuses to emit a partial name
    /// rather than silently truncating the chain.
    BrokenParentChain,
    /// A second row claims the role-`Inbox` slot, or a non-role row
    /// whose wire path (case-insensitively) equals `INBOX`. Either
    /// would put two rows under the IMAP-reserved `INBOX` wire name
    /// and corrupt every client that caches the mailbox tree by name.
    /// Storage is supposed to prevent both shapes; the encoder
    /// surfaces the inconsistency loudly rather than emitting it.
    InboxNameCollision,
}

/// Output of [`encode_all`] — the two-bucket split between rows
/// that can ride on the IMAP wire and rows that cannot.
#[derive(Debug, Clone, Default)]
pub struct EncodedSet {
    pub mailboxes: Vec<EncodedMailbox>,
    pub omitted: Vec<OmittedMailbox>,
}

/// Encode every record in `records` for the IMAP wire.
///
/// Behaviour:
/// * The role-Inbox container (`role == Some(MailboxRole::Inbox)`)
///   becomes `wire_name = "INBOX"`. If two records claim the role,
///   the first one seen wins; subsequent inbox-tagged records are
///   omitted with [`OmissionReason::InboxNameCollision`] to surface
///   the inconsistency at a separate diagnostic from the parent-chain
///   variant. Storage is supposed to prevent the duplicate.
/// * Every other record walks its `parent` chain through the
///   record set and joins segments with [`HIERARCHY_DELIMITER`].
///   ASCII / delimiter / charset-reserved / chain validity is
///   checked at each step; the first failure removes the row to
///   `omitted`. The walk also rejects a non-role row whose resulting
///   wire path is case-insensitively equal to `INBOX` so the
///   IMAP-reserved literal cannot be shadowed by an unrelated row.
/// * `has_children` is computed in a single sweep over `parent`
///   ids before the per-row walk, so the cost is O(N).
pub fn encode_all(records: &[MailboxRecord]) -> EncodedSet {
    let by_id: HashMap<ContainerId, &MailboxRecord> = records.iter().map(|r| (r.id, r)).collect();

    let mut child_count: HashMap<ContainerId, usize> = HashMap::new();
    for r in records {
        if let Some(p) = r.parent {
            *child_count.entry(p).or_insert(0) += 1;
        }
    }

    let mut inbox_id: Option<ContainerId> = None;
    for r in records {
        if r.role == Some(MailboxRole::Inbox) {
            inbox_id = Some(r.id);
            break;
        }
    }

    let mut out = EncodedSet::default();
    for r in records {
        if Some(r.id) == inbox_id {
            out.mailboxes.push(EncodedMailbox {
                id: r.id,
                wire_name: "INBOX".to_string(),
                has_children: child_count.contains_key(&r.id),
                role: r.role,
                is_subscribed: r.is_subscribed,
            });
            continue;
        }

        // Duplicate-inbox-role guard: any second role-Inbox row is
        // a broken state from the storage layer's perspective. Drop
        // it loudly rather than emitting a second "INBOX" on the
        // wire (which would corrupt every client that caches the
        // mailbox tree by name).
        if r.role == Some(MailboxRole::Inbox) {
            out.omitted.push(OmittedMailbox {
                id: r.id,
                stored_name: r.name.clone(),
                reason: OmissionReason::InboxNameCollision,
            });
            continue;
        }

        match walk_name(r, &by_id, inbox_id) {
            Ok(WalkOutcome {
                wire_name,
                root_kind,
            }) => {
                // Non-role row whose wire path occupies the reserved
                // INBOX namespace *without* being a descendant of
                // the role-Inbox container. RFC 9051 reserves `INBOX`
                // as a special name; allowing any non-role row whose
                // top-level segment is `inbox`/`Inbox`/etc would
                // split client caches and shadow the real INBOX. A
                // legitimate descendant of the role-Inbox container
                // gets `RootKind::RoleInbox` here, so its
                // `INBOX/<child>` wire path is preserved.
                if root_kind == RootKind::NonRole && first_segment_is_inbox(&wire_name) {
                    out.omitted.push(OmittedMailbox {
                        id: r.id,
                        stored_name: r.name.clone(),
                        reason: OmissionReason::InboxNameCollision,
                    });
                    continue;
                }
                out.mailboxes.push(EncodedMailbox {
                    id: r.id,
                    wire_name,
                    has_children: child_count.contains_key(&r.id),
                    role: r.role,
                    is_subscribed: r.is_subscribed,
                })
            }
            Err(reason) => out.omitted.push(OmittedMailbox {
                id: r.id,
                stored_name: r.name.clone(),
                reason,
            }),
        }
    }
    out
}

/// True if the wire path's first segment is `INBOX` case-insensitively.
/// Used together with [`RootKind`] to catch non-role rows whose top
/// segment would shadow the IMAP-reserved INBOX namespace.
fn first_segment_is_inbox(wire_path: &str) -> bool {
    let first = wire_path
        .split_once(HIERARCHY_DELIMITER)
        .map(|(head, _)| head)
        .unwrap_or(wire_path);
    first.eq_ignore_ascii_case("INBOX")
}

/// What terminated the parent-chain walk: did we hit the role-Inbox
/// container (in which case the first wire segment is the literal
/// `INBOX` legitimately), or did we run off a non-role root?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    RoleInbox,
    NonRole,
}

#[derive(Debug, Clone)]
struct WalkOutcome {
    wire_name: String,
    root_kind: RootKind,
}

/// Walk the parent chain of `r`, validating each segment, and
/// return the slash-joined wire name. The role-Inbox container is
/// rendered as the literal `INBOX` when it appears anywhere in the
/// chain — so a child whose parent is the inbox produces e.g.
/// `INBOX/Subfolder` even if the stored name of the inbox container
/// is "Posteingang" or similar.
fn walk_name(
    leaf: &MailboxRecord,
    by_id: &HashMap<ContainerId, &MailboxRecord>,
    inbox_id: Option<ContainerId>,
) -> Result<WalkOutcome, OmissionReason> {
    let mut segments: Vec<&str> = Vec::new();
    let mut seen: std::collections::HashSet<ContainerId> = std::collections::HashSet::new();
    let mut cur = leaf;
    let root_kind;
    loop {
        if !seen.insert(cur.id) {
            return Err(OmissionReason::BrokenParentChain);
        }
        if Some(cur.id) == inbox_id {
            segments.push("INBOX");
            root_kind = RootKind::RoleInbox;
            break;
        }
        if !is_ascii_printable(&cur.name) {
            return Err(OmissionReason::NonAsciiName);
        }
        if cur.name.contains(HIERARCHY_DELIMITER) {
            return Err(OmissionReason::NameContainsDelimiter);
        }
        if cur.name.contains('&') {
            return Err(OmissionReason::NameContainsCharsetReserved);
        }
        segments.push(&cur.name);
        match cur.parent {
            None => {
                root_kind = RootKind::NonRole;
                break;
            }
            Some(pid) => match by_id.get(&pid) {
                Some(p) => cur = p,
                None => return Err(OmissionReason::BrokenParentChain),
            },
        }
    }
    segments.reverse();
    Ok(WalkOutcome {
        wire_name: segments.join(&HIERARCHY_DELIMITER.to_string()),
        root_kind,
    })
}

fn is_ascii_printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Result of [`resolve`]: either the [`ContainerId`] the wire name
/// referred to, or the reason resolution failed. The latter shapes
/// the IMAP error the wire handler emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveResult {
    Found(ContainerId),
    /// No record matched. Wire handler should emit
    /// `NO [NONEXISTENT] ...`.
    NotFound,
    /// A record matched on stored name but could not be encoded
    /// (non-ASCII, contains delimiter, …). Wire handler should
    /// emit `NO [SERVERBUG] mailbox name not representable over
    /// IMAP`.
    Unrepresentable,
}

/// Resolve a client-sent wire name to a [`ContainerId`].
///
/// * `INBOX` (any case) → the role-Inbox container, if any.
/// * Anything else → an exact match against the wire name produced
///   by [`encode_all`].
///
/// Wire-name matches use byte equality (after the INBOX case-fold
/// step). This deliberately does **not** accept partial paths or
/// case-fold non-INBOX names — that would conflict with the
/// non-negotiable "UIDVALIDITY identifies a mailbox" invariant.
pub fn resolve(wire_name: &str, records: &[MailboxRecord]) -> ResolveResult {
    if wire_name.eq_ignore_ascii_case("INBOX") {
        for r in records {
            if r.role == Some(MailboxRole::Inbox) {
                return ResolveResult::Found(r.id);
            }
        }
        return ResolveResult::NotFound;
    }
    let set = encode_all(records);
    for enc in &set.mailboxes {
        if enc.wire_name == wire_name {
            return ResolveResult::Found(enc.id);
        }
    }
    for om in &set.omitted {
        if om.stored_name == wire_name {
            return ResolveResult::Unrepresentable;
        }
    }
    ResolveResult::NotFound
}

/// RFC 9051 §6.3.9 wildcard match.
///
/// * `*` — matches zero or more of any byte, including the
///   hierarchy delimiter.
/// * `%` — matches zero or more of any byte **except** the
///   hierarchy delimiter (single-level wildcard).
/// * Every other byte matches itself exactly.
///
/// **INBOX case-fold.** Per RFC 9051 §5.1, `INBOX` is reserved and
/// matched case-insensitively. If the pattern's first 5 ASCII bytes
/// (case-insensitively) spell `INBOX` *and* the byte after them is
/// either absent or the hierarchy delimiter, those 5 bytes are
/// pre-folded to uppercase before matching. So `LIST "" inbox` and
/// `LIST "" inbox/%` both reach the role-Inbox row even though wire
/// names are case-sensitive in general.
///
/// Implementation is a small recursive matcher; the patterns IMAP
/// clients emit are tiny in practice (`*`, `%`, `INBOX*`,
/// `INBOX/%`) and `MailboxRecord` counts are O(10s), so the
/// asymptotic cost is irrelevant.
pub fn matches_pattern(wire_name: &str, pattern: &str) -> bool {
    let folded = fold_inbox_prefix(pattern);
    let nbytes = wire_name.as_bytes();
    let pbytes = folded.as_bytes();
    match_at(nbytes, 0, pbytes, 0)
}

/// Uppercase the leading `INBOX` segment of a LIST pattern, if any.
/// Only fires when the 5-byte prefix matches `INBOX` ASCII-case-
/// insensitively *and* the boundary after it is end-of-pattern or
/// the hierarchy delimiter — so `inboxes` (with a trailing `es`) is
/// left untouched and treated as an unrelated user-named mailbox.
fn fold_inbox_prefix(pattern: &str) -> std::borrow::Cow<'_, str> {
    let bytes = pattern.as_bytes();
    if bytes.len() < 5 {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let head_is_inbox = bytes[..5].eq_ignore_ascii_case(b"INBOX");
    if !head_is_inbox {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let boundary_ok = bytes.len() == 5 || bytes[5] as char == HIERARCHY_DELIMITER;
    if !boundary_ok {
        return std::borrow::Cow::Borrowed(pattern);
    }
    if &bytes[..5] == b"INBOX" {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let mut out = String::with_capacity(pattern.len());
    out.push_str("INBOX");
    out.push_str(&pattern[5..]);
    std::borrow::Cow::Owned(out)
}

fn match_at(name: &[u8], ni: usize, pat: &[u8], pi: usize) -> bool {
    if pi == pat.len() {
        return ni == name.len();
    }
    match pat[pi] {
        b'*' => {
            // Try matching `*` against 0..=remaining bytes of name.
            for j in ni..=name.len() {
                if match_at(name, j, pat, pi + 1) {
                    return true;
                }
            }
            false
        }
        b'%' => {
            // `%` may consume any byte except the delimiter.
            for j in ni..=name.len() {
                if match_at(name, j, pat, pi + 1) {
                    return true;
                }
                if j < name.len() && name[j] as char == HIERARCHY_DELIMITER {
                    break;
                }
            }
            false
        }
        b => {
            if ni < name.len() && name[ni] == b {
                match_at(name, ni + 1, pat, pi + 1)
            } else {
                false
            }
        }
    }
}

/// Quote a wire-name for a LIST / LSUB response.
///
/// `encode_all` guarantees wire names contain only printable ASCII
/// and no hierarchy delimiter inside a segment, so the only bytes
/// that need escaping are `"` and `\` per the IMAP quoted-string
/// production (RFC 9051 §4.3). We always emit a quoted-string
/// (never an atom) so the client never has to second-guess token
/// boundaries when a name contains a space or other atom-illegal
/// byte.
pub fn quote_mailbox(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Format one row of a LIST or LSUB response.
///
/// `keyword` is `LIST` or `LSUB`. The terminating CRLF is included.
/// SPECIAL-USE and CHILDREN attributes are emitted from
/// [`format_attrs`]; `\Subscribed` is intentionally NOT included
/// (LIST-EXTENDED is not in the v1 capability subset — LSUB carries
/// subscription via its own response, not via a LIST attribute).
pub fn render_mailbox_line(keyword: &str, enc: &EncodedMailbox) -> String {
    format!(
        "* {} ({}) \"{}\" {}\r\n",
        keyword,
        format_attrs(enc),
        HIERARCHY_DELIMITER,
        quote_mailbox(&enc.wire_name),
    )
}

/// Format the LIST/LSUB attribute list for one encoded row.
///
/// Emits `\Inbox` / `\Archive` / … from SPECIAL-USE plus
/// `\HasChildren` / `\HasNoChildren` from CHILDREN. The v1
/// capability subset does not include LIST-EXTENDED, so
/// `\Subscribed` is **not** emitted here — LSUB carries
/// subscription state via its own response.
///
/// Returns the attribute list as a space-separated string with no
/// surrounding parens — the caller wraps it in the `* LIST (...)`
/// envelope alongside the delimiter and mailbox name.
pub fn format_attrs(enc: &EncodedMailbox) -> String {
    let mut parts: Vec<&'static str> = Vec::with_capacity(2);
    if let Some(role) = enc.role {
        parts.push(role.as_special_use());
    }
    parts.push(if enc.has_children {
        "\\HasChildren"
    } else {
        "\\HasNoChildren"
    });
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mds::{ContainerAttrs, ContainerId};
    use uuid::Uuid;

    use crate::mailstore::MailboxRights;

    fn mk(
        name: &str,
        parent: Option<ContainerId>,
        role: Option<MailboxRole>,
        subscribed: bool,
    ) -> MailboxRecord {
        MailboxRecord {
            id: ContainerId(Uuid::new_v4()),
            name: name.to_string(),
            parent,
            attrs: ContainerAttrs {
                special_use: role.map(|r| r.as_special_use().to_string()),
                subscribed,
                extra: serde_json::Value::Null,
            },
            total_emails: 0,
            unread_emails: 0,
            role,
            sort_order: 0,
            is_subscribed: subscribed,
            my_rights: MailboxRights::full(),
            uid_validity: 1,
            highest_mod_seq: 0,
            uid_next: 1,
        }
    }

    #[test]
    fn inbox_role_renders_as_uppercase_inbox_regardless_of_stored_name() {
        let inbox = mk("Posteingang", None, Some(MailboxRole::Inbox), true);
        let set = encode_all(std::slice::from_ref(&inbox));
        assert_eq!(set.mailboxes.len(), 1);
        assert_eq!(set.mailboxes[0].wire_name, "INBOX");
        assert_eq!(set.mailboxes[0].role, Some(MailboxRole::Inbox));
        assert!(set.omitted.is_empty());
    }

    #[test]
    fn flat_mailboxes_render_as_their_stored_name() {
        let archive = mk("Archive", None, Some(MailboxRole::Archive), false);
        let custom = mk("Receipts", None, None, false);
        let set = encode_all(&[archive, custom]);
        let names: Vec<_> = set.mailboxes.iter().map(|m| m.wire_name.as_str()).collect();
        assert!(names.contains(&"Archive"));
        assert!(names.contains(&"Receipts"));
    }

    #[test]
    fn nested_mailboxes_compose_with_slash() {
        let inbox = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        let inbox_id = inbox.id;
        let child = mk("Receipts", Some(inbox_id), None, false);
        let grandchild = mk("2024", Some(child.id), None, false);
        let child_id = child.id;
        let grand_id = grandchild.id;
        let set = encode_all(&[inbox, child, grandchild]);

        let by_id: HashMap<_, _> = set
            .mailboxes
            .iter()
            .map(|m| (m.id, m.wire_name.clone()))
            .collect();
        assert_eq!(by_id[&inbox_id], "INBOX");
        assert_eq!(by_id[&child_id], "INBOX/Receipts");
        assert_eq!(by_id[&grand_id], "INBOX/Receipts/2024");
    }

    #[test]
    fn has_children_attribute_is_computed_per_row() {
        let inbox = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        let inbox_id = inbox.id;
        let leaf = mk("Receipts", Some(inbox_id), None, false);
        let set = encode_all(&[inbox, leaf]);
        for m in &set.mailboxes {
            if m.wire_name == "INBOX" {
                assert!(m.has_children);
            } else {
                assert!(!m.has_children);
            }
        }
    }

    #[test]
    fn non_ascii_name_is_omitted_with_reason() {
        let r = mk("Heizölrückstoßabdämpfung", None, None, false);
        let id = r.id;
        let set = encode_all(&[r]);
        assert!(set.mailboxes.is_empty());
        assert_eq!(set.omitted.len(), 1);
        assert_eq!(set.omitted[0].id, id);
        assert_eq!(set.omitted[0].reason, OmissionReason::NonAsciiName);
    }

    #[test]
    fn name_containing_delimiter_is_omitted_with_reason() {
        let r = mk("a/b", None, None, false);
        let set = encode_all(&[r]);
        assert!(set.mailboxes.is_empty());
        assert_eq!(set.omitted.len(), 1);
        assert_eq!(set.omitted[0].reason, OmissionReason::NameContainsDelimiter);
    }

    #[test]
    fn broken_parent_chain_is_omitted_with_reason() {
        // `child.parent` points to an id that is not in the slice.
        let phantom = ContainerId(Uuid::new_v4());
        let child = mk("Orphan", Some(phantom), None, false);
        let set = encode_all(&[child]);
        assert!(set.mailboxes.is_empty());
        assert_eq!(set.omitted.len(), 1);
        assert_eq!(set.omitted[0].reason, OmissionReason::BrokenParentChain);
    }

    #[test]
    fn parent_cycle_is_omitted_not_infinite_loop() {
        // Two records each pointing at each other.
        let a_id = ContainerId(Uuid::new_v4());
        let b_id = ContainerId(Uuid::new_v4());
        let mut a = mk("A", Some(b_id), None, false);
        let mut b = mk("B", Some(a_id), None, false);
        a.id = a_id;
        b.id = b_id;
        let set = encode_all(&[a, b]);
        assert!(set.mailboxes.is_empty());
        assert_eq!(set.omitted.len(), 2);
        for om in &set.omitted {
            assert_eq!(om.reason, OmissionReason::BrokenParentChain);
        }
    }

    #[test]
    fn duplicate_inbox_role_keeps_first_omits_rest() {
        let first = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        let second = mk("Other", None, Some(MailboxRole::Inbox), false);
        let first_id = first.id;
        let set = encode_all(&[first, second]);
        assert_eq!(set.mailboxes.len(), 1);
        assert_eq!(set.mailboxes[0].id, first_id);
        assert_eq!(set.mailboxes[0].wire_name, "INBOX");
        assert_eq!(set.omitted.len(), 1);
        assert_eq!(set.omitted[0].reason, OmissionReason::InboxNameCollision);
    }

    #[test]
    fn non_role_inbox_subtree_descendants_also_omitted() {
        // Role-INBOX plus a non-role root literally named "inbox"
        // with a child "Sub". The child walks to wire path
        // `inbox/Sub`, which does NOT equal "INBOX" but still
        // occupies the reserved INBOX namespace. Both shadow root
        // and child must be omitted with InboxNameCollision.
        let role_inbox = mk("RealInbox", None, Some(MailboxRole::Inbox), true);
        let shadow_root = mk("inbox", None, None, false);
        let shadow_root_id = shadow_root.id;
        let shadow_child = mk("Sub", Some(shadow_root_id), None, false);
        let shadow_child_id = shadow_child.id;
        let set = encode_all(&[role_inbox, shadow_root, shadow_child]);
        // Only the role-INBOX row makes it through.
        assert_eq!(set.mailboxes.len(), 1);
        assert_eq!(set.mailboxes[0].wire_name, "INBOX");
        assert_eq!(set.omitted.len(), 2);
        let ids: std::collections::HashSet<_> = set.omitted.iter().map(|o| o.id).collect();
        assert!(ids.contains(&shadow_root_id));
        assert!(ids.contains(&shadow_child_id));
        for om in &set.omitted {
            assert_eq!(om.reason, OmissionReason::InboxNameCollision);
        }
    }

    #[test]
    fn non_role_inbox_named_row_omitted_with_collision_reason() {
        // A non-role row stored literally as "INBOX" (or "inbox")
        // must not shadow the role-Inbox wire name.
        let role_inbox = mk("RealInbox", None, Some(MailboxRole::Inbox), true);
        let shadow_upper = mk("INBOX", None, None, false);
        let shadow_lower = mk("inbox", None, None, false);
        let shadow_upper_id = shadow_upper.id;
        let shadow_lower_id = shadow_lower.id;
        let set = encode_all(&[role_inbox, shadow_upper, shadow_lower]);
        assert_eq!(set.mailboxes.len(), 1);
        assert_eq!(set.mailboxes[0].wire_name, "INBOX");
        assert_eq!(set.omitted.len(), 2);
        for om in &set.omitted {
            assert_eq!(om.reason, OmissionReason::InboxNameCollision);
            assert!(om.id == shadow_upper_id || om.id == shadow_lower_id);
        }
    }

    #[test]
    fn ampersand_in_name_omitted_as_charset_reserved() {
        let r = mk("R&D", None, None, false);
        let set = encode_all(&[r]);
        assert!(set.mailboxes.is_empty());
        assert_eq!(set.omitted.len(), 1);
        assert_eq!(
            set.omitted[0].reason,
            OmissionReason::NameContainsCharsetReserved
        );
    }

    #[test]
    fn pattern_lowercase_inbox_matches_role_inbox_row() {
        // RFC 9051 §5.1: INBOX is case-insensitive.
        assert!(matches_pattern("INBOX", "inbox"));
        assert!(matches_pattern("INBOX", "Inbox"));
        assert!(matches_pattern("INBOX", "iNbOx"));
    }

    #[test]
    fn pattern_lowercase_inbox_children_match() {
        assert!(matches_pattern("INBOX/Receipts", "inbox/*"));
        assert!(matches_pattern("INBOX/Receipts", "INBOX/*"));
        assert!(matches_pattern("INBOX/Receipts", "inbox/%"));
        assert!(!matches_pattern("INBOX/Sub/Leaf", "inbox/%"));
    }

    #[test]
    fn pattern_lowercase_inboxes_does_not_case_fold() {
        // The fold only fires at end-of-pattern or hierarchy
        // delimiter boundary, so an unrelated user-named mailbox
        // (e.g. literal `inboxes`) stays case-sensitive.
        assert!(!matches_pattern("INBOX", "inboxes"));
        assert!(!matches_pattern("Inboxes", "inboxes"));
    }

    #[test]
    fn resolve_inbox_is_case_insensitive() {
        let inbox = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        let inbox_id = inbox.id;
        let recs = [inbox];
        assert_eq!(resolve("INBOX", &recs), ResolveResult::Found(inbox_id));
        assert_eq!(resolve("inbox", &recs), ResolveResult::Found(inbox_id));
        assert_eq!(resolve("InBoX", &recs), ResolveResult::Found(inbox_id));
    }

    #[test]
    fn resolve_non_inbox_is_case_sensitive() {
        let inbox = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        let other = mk("Receipts", None, None, false);
        let other_id = other.id;
        let recs = [inbox, other];
        assert_eq!(resolve("Receipts", &recs), ResolveResult::Found(other_id));
        assert_eq!(resolve("receipts", &recs), ResolveResult::NotFound);
        assert_eq!(resolve("RECEIPTS", &recs), ResolveResult::NotFound);
    }

    #[test]
    fn resolve_nonexistent_returns_not_found() {
        let inbox = mk("Inbox", None, Some(MailboxRole::Inbox), true);
        assert_eq!(resolve("DoesNotExist", &[inbox]), ResolveResult::NotFound);
    }

    #[test]
    fn resolve_unrepresentable_name_surfaces_as_unrepresentable() {
        let r = mk("a/b", None, None, false);
        let recs = [r];
        assert_eq!(resolve("a/b", &recs), ResolveResult::Unrepresentable);
    }

    #[test]
    fn pattern_star_matches_everything() {
        assert!(matches_pattern("INBOX", "*"));
        assert!(matches_pattern("INBOX/Receipts", "*"));
        assert!(matches_pattern("", "*"));
        assert!(matches_pattern("INBOX/Receipts/2024", "*"));
    }

    #[test]
    fn pattern_percent_stops_at_delimiter() {
        assert!(matches_pattern("INBOX", "%"));
        assert!(!matches_pattern("INBOX/Receipts", "%"));
        assert!(matches_pattern("INBOX/Receipts", "INBOX/%"));
        assert!(!matches_pattern("INBOX/Receipts/2024", "INBOX/%"));
    }

    #[test]
    fn pattern_literal_matches_exact() {
        assert!(matches_pattern("INBOX", "INBOX"));
        assert!(!matches_pattern("INBOX", "INBOXX"));
        assert!(!matches_pattern("INBOXX", "INBOX"));
    }

    #[test]
    fn pattern_prefix_star_matches_subtree() {
        assert!(matches_pattern("INBOX/Receipts", "INBOX/*"));
        assert!(matches_pattern("INBOX/Receipts/2024", "INBOX/*"));
        assert!(!matches_pattern("OtherFolder", "INBOX/*"));
    }

    #[test]
    fn format_attrs_emits_special_use_and_children() {
        let inbox_with_kids = EncodedMailbox {
            id: ContainerId(Uuid::new_v4()),
            wire_name: "INBOX".into(),
            has_children: true,
            role: Some(MailboxRole::Inbox),
            is_subscribed: true,
        };
        assert_eq!(format_attrs(&inbox_with_kids), "\\Inbox \\HasChildren");

        let leaf = EncodedMailbox {
            id: ContainerId(Uuid::new_v4()),
            wire_name: "Receipts".into(),
            has_children: false,
            role: None,
            is_subscribed: false,
        };
        assert_eq!(format_attrs(&leaf), "\\HasNoChildren");

        let sent = EncodedMailbox {
            id: ContainerId(Uuid::new_v4()),
            wire_name: "Sent".into(),
            has_children: false,
            role: Some(MailboxRole::Sent),
            is_subscribed: false,
        };
        assert_eq!(format_attrs(&sent), "\\Sent \\HasNoChildren");
    }
}
