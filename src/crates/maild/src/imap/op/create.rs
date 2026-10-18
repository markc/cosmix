//! `CREATE` — RFC 9051 §6.3.4.
//!
//! Phase 2 T5 v1: walk the hierarchical name top-down, creating
//! missing intermediate segments as we go. The wire grammar is one
//! mailbox name (atom or quoted-string); segments are split on the
//! advertised hierarchy delimiter `/`.
//!
//! Behaviour locked with Codex (T5 pre-impl consult):
//! * Every segment is pre-validated **before** the first storage
//!   mutation — non-empty, printable ASCII (0x20..=0x7e), no `&`
//!   (RFC 3501 modified-UTF-7 introducer; would render wrong on a
//!   strict `IMAP4rev1` client without UTF8=ACCEPT), not the reserved
//!   `__upload_staging__` name. This prevents the worst partial-walk
//!   outcome where `foo/bar/<bad>` leaves `foo/bar` orphaned.
//! * `CREATE INBOX` (case-insensitive) returns tagged `OK` if the
//!   account has a role-Inbox container — RFC 9051 mandates the
//!   idempotent-probe semantic. If the account is missing INBOX the
//!   handler returns `NO [SERVERBUG]`; that's an account-bootstrap
//!   invariant break, not something the client can fix.
//! * `CREATE foo/` (trailing delimiter) normalises to `foo` and is
//!   idempotent: if `foo` already exists, return `OK` (the client is
//!   announcing "ensure this parent path is present") rather than
//!   `NO [ALREADYEXISTS]`. Without the trailing delimiter, a
//!   pre-existing final segment is `NO [ALREADYEXISTS]`.
//! * Empty path segments (`foo//bar`, `/foo`) are rejected as
//!   `NO [CANNOT] empty mailbox path segment` after the trailing
//!   delimiter is stripped — they cannot be silently coalesced
//!   without lying about the name the client typed. A bare `foo/`
//!   is **not** in this set: it normalises to `foo` per the
//!   trailing-delimiter rule above.
//!
//! Walk atomicity is **not** transactional in v1. The storage
//! `create_mailbox` opens its own `with_set_tx` per call, so a walk
//! that creates `a` then fails on `a/b` (e.g. another session beat
//! us to creating `a/b` with a clashing storage row) leaves `a`
//! committed. This is user-visible but not corrupting: the client
//! can retry the same `CREATE a/b/c` and the walk will continue
//! through the already-existing prefix. A fully-atomic walk would
//! require a typed `SqliteSetTx::create_mailbox` (`with_set_tx`
//! cannot re-enter public `Mds` mutation methods per
//! `feedback_with_set_tx_no_reentry`); deferred to a future phase.
//!
//! Storage-race handling: if `create_mailbox` returns
//! `"mailbox already exists"` on an intermediate segment (another
//! session raced us in between resolve and create), we reload the
//! mailbox list and re-resolve the prefix to pick up the
//! now-existing id rather than continuing with a stale parent.
//!
//! SPECIAL-USE (RFC 6154 §5.1): the optional `(USE (\Drafts ...))`
//! parameter after the mailbox name designates one SPECIAL-USE
//! attribute for the *leaf* segment; any intermediate segments
//! created by the walk get `role = None`. Constraints:
//! * `\Inbox` is rejected (`NO [USEATTR]`) — the role-Inbox container
//!   is account-bootstrap-managed and cannot be (re)assigned by the
//!   client.
//! * Unknown SPECIAL-USE attributes are rejected `NO [USEATTR]`.
//! * Multiple attributes in one CREATE are rejected `NO [USEATTR]` —
//!   our storage model is single-`MailboxRole` per mailbox.
//! * Per-account role uniqueness is enforced *here*: storage today has
//!   no `(account, role)` unique index and `mailbox_by_role` returns
//!   the first match by iteration order, so allowing duplicate roles
//!   would be a user-visible ambiguity. CREATE with a USE role that
//!   another mailbox already carries → `NO [USEATTR]`.
//! * On the three idempotent CREATE paths (bare `INBOX`,
//!   `Found+trailing-slash`, race-reload `Found`) we additionally
//!   require the existing mailbox's role to match the requested USE;
//!   otherwise we'd return `OK` while silently ignoring the attribute
//!   the client asked for.
//! * The wire `USE` keyword and the SPECIAL-USE flag tokens are parsed
//!   ASCII-case-insensitively (RFC 9051 §1.2 says IMAP keywords are
//!   case-insensitive). `\drafts` and `\DRAFTS` both map to
//!   `MailboxRole::Drafts`.
//!
//! Role uniqueness on the CREATE path is enforced *atomically* by the
//! storage layer: `MailStore::create_mailbox` runs a
//! `SELECT … json_extract(attrs, '$.special_use')` probe inside the
//! same `with_set_tx` scope as the create. The per-set mutex
//! serialises concurrent CREATEs on the same account, so two peer
//! `CREATE foo (USE (\Drafts))` / `CREATE bar (USE (\Drafts))` calls
//! cannot both commit — the second to acquire the mutex sees the
//! first's row and returns `mailbox special-use already in use: …`,
//! which the handler maps to `NO [USEATTR]`. The snapshot pre-walk
//! scan in this file is the cheap fast-path that avoids leaving
//! intermediate folders behind when the leaf would conflict; the
//! storage-layer check is the authoritative barrier.
//!
//! Limitation: the barrier covers CREATE only. `update_mailbox` can
//! still introduce a duplicate-role state if two concurrent updates
//! re-assign the same `MailboxRole` to different mailboxes (the same
//! caller-side snapshot pattern JMAP `Mailbox/set update` documents).
//! Tightening the update path is a follow-up — the in-tx probe needs
//! a target-id exclusion to allow same-target idempotent updates, and
//! is symmetric work better tackled with its own review pass.

use std::sync::Arc;

use cosmix_mds::ContainerId;
use cosmix_mds::container::UPLOAD_STAGING_NAME;

use crate::imap::mailbox_name::{HIERARCHY_DELIMITER, ResolveResult, resolve};
use crate::imap::op::list;
use crate::imap::response::{Status, tagged};
use crate::mailstore::{MailStore, MailboxRecord, MailboxRole, jmap_attrs_for_create};

type AccountId = i32;

/// One token of the `(USE (...))` argument list, after the leading
/// `(USE (` and trailing `))` have been peeled. We keep the raw
/// `\Flag` text here so the semantic check (known? Inbox-only? unique?)
/// can run after the wire grammar has validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseFlag(pub String);

/// Parsed CREATE wire arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub name: String,
    /// RFC 6154 §5.1 `(USE (\Drafts ...))` arguments. `None` means the
    /// client didn't send a USE clause at all; `Some(vec![])` means
    /// they sent `(USE ())`, which RFC 6154 allows but our handler
    /// treats as "no role" (same as `None`).
    pub use_flags: Option<Vec<UseFlag>>,
}

/// Parse `CREATE <mailbox-name> [(USE (\Flag...))]`. The mailbox-name
/// is a single atom or quoted-string; the optional USE clause is the
/// RFC 6154 §5.1 wire form. Trailing tokens after either are `BAD`.
pub fn parse_args(args: &str) -> Result<Args, String> {
    let s = args.trim_start();
    if s.is_empty() {
        return Err("CREATE requires a mailbox name".to_string());
    }
    let (name, rest) = list::take_mailbox_name(s)?;
    if name.is_empty() {
        return Err("CREATE requires a non-empty mailbox name".to_string());
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Ok(Args {
            name,
            use_flags: None,
        });
    }
    let (use_flags, tail) = parse_use_clause(rest)?;
    let tail = tail.trim();
    if !tail.is_empty() {
        return Err("CREATE trailing tokens after USE clause".to_string());
    }
    Ok(Args {
        name,
        use_flags: Some(use_flags),
    })
}

/// Parse the RFC 6154 §5.1 `(USE (\Flag...))` wire form, returning the
/// flag list and the unconsumed tail. Caller already trimmed leading
/// whitespace.
fn parse_use_clause(s: &str) -> Result<(Vec<UseFlag>, &str), String> {
    let s = s
        .strip_prefix('(')
        .ok_or_else(|| "CREATE: expected '(' starting parameter list".to_string())?
        .trim_start();
    let s = if s.len() >= 3 && s.as_bytes()[..3].eq_ignore_ascii_case(b"USE") {
        &s[3..]
    } else {
        return Err("CREATE: only the USE parameter is supported".to_string());
    };
    let s = s.trim_start();
    let s = s
        .strip_prefix('(')
        .ok_or_else(|| "CREATE USE: expected '(' starting flag list".to_string())?;
    let mut flags = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix(')') {
            rest = after;
            break;
        }
        let end = rest
            .find([' ', ')'])
            .ok_or_else(|| "CREATE USE: unterminated flag list".to_string())?;
        let flag = &rest[..end];
        if flag.is_empty() {
            return Err("CREATE USE: empty flag token".to_string());
        }
        if !flag.starts_with('\\') {
            return Err(format!("CREATE USE: flag {flag:?} must start with '\\\\'"));
        }
        flags.push(UseFlag(flag.to_string()));
        rest = &rest[end..];
    }
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix(')')
        .ok_or_else(|| "CREATE: expected ')' ending parameter list".to_string())?;
    Ok((flags, rest))
}

/// Resolve a parsed `Option<Vec<UseFlag>>` into the role we should
/// assign to the leaf segment. `None` (or `Some(empty)`) → no role.
/// Multiple flags, unknown flags, and `\Inbox` are all `Err(message)`
/// for the caller to wrap in `NO [USEATTR]`.
fn role_from_use_flags(use_flags: &Option<Vec<UseFlag>>) -> Result<Option<MailboxRole>, String> {
    let Some(flags) = use_flags else {
        return Ok(None);
    };
    if flags.is_empty() {
        return Ok(None);
    }
    if flags.len() > 1 {
        return Err(format!(
            "only one SPECIAL-USE attribute supported per mailbox (got {})",
            flags.len()
        ));
    }
    let raw = &flags[0].0;
    let role = role_from_special_use_ci(raw)
        .ok_or_else(|| format!("unknown SPECIAL-USE attribute {raw:?}"))?;
    if role == MailboxRole::Inbox {
        return Err("\\Inbox cannot be assigned by CREATE".to_string());
    }
    Ok(Some(role))
}

/// ASCII-case-insensitive `MailboxRole::from_special_use`. RFC 9051 §1.2
/// mandates IMAP keywords are case-insensitive; clients in the wild
/// send `\drafts`, `\DRAFTS`, `\Drafts`. Storage canonicalises to the
/// title-case `\Drafts` form via `MailboxRole::as_special_use`, so this
/// lookup only needs to accept the variants on the wire.
fn role_from_special_use_ci(s: &str) -> Option<MailboxRole> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "\\inbox" => Some(MailboxRole::Inbox),
        "\\archive" => Some(MailboxRole::Archive),
        "\\drafts" => Some(MailboxRole::Drafts),
        "\\flagged" => Some(MailboxRole::Flagged),
        "\\important" => Some(MailboxRole::Important),
        "\\junk" => Some(MailboxRole::Junk),
        "\\sent" => Some(MailboxRole::Sent),
        "\\trash" => Some(MailboxRole::Trash),
        "\\all" => Some(MailboxRole::All),
        _ => None,
    }
}

/// Reject a path segment that the wire layer cannot round-trip
/// safely. See module-level docs for the rule list.
///
/// `pub(super)` so `op::rename` reuses the exact same validation —
/// the two verbs share the segment grammar.
pub(super) fn validate_segment(seg: &str) -> Result<(), String> {
    if seg.is_empty() {
        return Err("empty mailbox path segment".to_string());
    }
    for b in seg.bytes() {
        if !(0x20..=0x7e).contains(&b) {
            return Err(format!(
                "non-ASCII or control byte 0x{b:02x} in mailbox name (UTF8=ACCEPT not advertised)"
            ));
        }
        if b == b'&' {
            return Err("'&' is reserved as the IMAP4rev1 modified-UTF-7 introducer".to_string());
        }
    }
    if seg == UPLOAD_STAGING_NAME {
        return Err(format!("{seg:?} is a reserved mailbox name"));
    }
    Ok(())
}

/// Outcome of comparing `leaf_role` against an existing mailbox we're
/// about to return `OK` for through an idempotent CREATE path. The
/// caller maps `Mismatch` to `NO [USEATTR]`; `Match` and `NoRoleAsked`
/// both let the OK proceed.
enum LeafRoleCheck {
    /// Client didn't ask for any role, or the existing mailbox already
    /// carries the requested role.
    Ok,
    /// Client asked for a role; the existing mailbox carries a
    /// different (or no) role. Returning `OK` here would silently
    /// ignore the requested SPECIAL-USE attribute.
    Mismatch,
}

/// Resolve whether the existing mailbox at `id` already satisfies the
/// requested SPECIAL-USE attribute. Used at every idempotent CREATE
/// return point: bare `INBOX` (handled inline, since INBOX is rejected
/// for non-`\Inbox` roles unconditionally), `Found` + trailing slash,
/// and race-reload `Found`.
fn check_existing_leaf_role(
    records: &[MailboxRecord],
    id: ContainerId,
    leaf_role: Option<MailboxRole>,
) -> LeafRoleCheck {
    let Some(role) = leaf_role else {
        return LeafRoleCheck::Ok;
    };
    let existing = records.iter().find(|r| r.id == id);
    match existing.and_then(|r| r.role) {
        Some(existing_role) if existing_role == role => LeafRoleCheck::Ok,
        _ => LeafRoleCheck::Mismatch,
    }
}

/// Find any *other* mailbox already carrying `role`. Per-account role
/// uniqueness is enforced here — storage has no `(account, role)`
/// unique index and `mailbox_by_role` returns first-match, so allowing
/// a second holder would be a user-visible ambiguity.
///
/// `target_id` is the id of the mailbox we're idempotently re-OKing
/// (when applicable) — a record at that id is excluded from the scan,
/// so `CREATE Drafts/ (USE (\Drafts))` against a pre-existing `Drafts`
/// with `\Drafts` is not self-conflicting. Pass `None` from tail-create
/// paths (the target doesn't exist yet).
fn other_role_holder_exists(
    records: &[MailboxRecord],
    role: MailboxRole,
    target_id: Option<ContainerId>,
) -> bool {
    records
        .iter()
        .any(|r| r.role == Some(role) && Some(r.id) != target_id)
}

/// Fetch the mailbox record set for the account, mapping any
/// storage error to a pre-built tagged response.
async fn load_records(
    store: &Arc<dyn MailStore>,
    account_id: AccountId,
    tag: &str,
) -> Result<Vec<MailboxRecord>, String> {
    let s = store.clone();
    match tokio::task::spawn_blocking(move || s.list_mailboxes(account_id)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "CREATE list_mailboxes failed");
            Err(tagged(tag, Status::No, Some("SERVERBUG"), "create failed"))
        }
        Err(e) => {
            tracing::warn!(error = %e, "CREATE list_mailboxes spawn_blocking panicked");
            Err(tagged(tag, Status::No, Some("SERVERBUG"), "create failed"))
        }
    }
}

/// Execute CREATE for an authenticated session.
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
    let raw_name = parsed.name;
    let leaf_role = match role_from_use_flags(&parsed.use_flags) {
        Ok(r) => r,
        Err(e) => return tagged(tag, Status::No, Some("USEATTR"), &e),
    };

    // RFC 9051 §6.3.4: a trailing delimiter announces "ensure this
    // hierarchy parent exists." Strip it, but remember the flag so a
    // pre-existing final segment is `OK` (the client is asserting
    // existence, not requesting a fresh creation).
    let (trimmed, had_trailing_slash) = match raw_name.strip_suffix(HIERARCHY_DELIMITER) {
        Some(stripped) => (stripped.to_string(), true),
        None => (raw_name.clone(), false),
    };
    if trimmed.is_empty() {
        return tagged(tag, Status::No, Some("CANNOT"), "empty mailbox name");
    }

    let segments: Vec<&str> = trimmed.split(HIERARCHY_DELIMITER).collect();
    for seg in &segments {
        if let Err(e) = validate_segment(seg) {
            return tagged(tag, Status::No, Some("CANNOT"), &e);
        }
    }

    let mut records = match load_records(&store, account_id, tag).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // If the first segment is the reserved `INBOX` wire name we MUST
    // anchor the walk on the role-Inbox container, never on a non-role
    // storage row literally named "INBOX": the latter would shadow
    // INBOX on the wire (the encoder would omit it as a shadow root,
    // per `OmissionReason::InboxNameCollision`), so the client gets `OK` for
    // a mailbox tree it then cannot see in LIST/SELECT. Without a
    // role-Inbox the account is in a broken state; refuse rather than
    // synthesising a non-role replacement.
    let starts_with_inbox = segments
        .first()
        .is_some_and(|s| s.eq_ignore_ascii_case("INBOX"));
    let inbox_id = records
        .iter()
        .find(|r| r.role == Some(MailboxRole::Inbox))
        .map(|r| r.id);
    if starts_with_inbox && inbox_id.is_none() {
        tracing::error!("CREATE INBOX[/...]: account has no role-Inbox container");
        return tagged(tag, Status::No, Some("SERVERBUG"), "INBOX unavailable");
    }
    // `CREATE INBOX` is the RFC 9051 idempotent existence probe. If the
    // client also supplied a non-empty `(USE (...))`, they're trying to
    // (re)assign a SPECIAL-USE attribute to INBOX — `\Inbox` is already
    // rejected at parse, and any other role here would silently no-op
    // because INBOX is bootstrap-managed. Reject `NO [USEATTR]`.
    if segments.len() == 1 && starts_with_inbox {
        if leaf_role.is_some() {
            return tagged(
                tag,
                Status::No,
                Some("USEATTR"),
                "INBOX SPECIAL-USE attribute is not client-assignable",
            );
        }
        return tagged(tag, Status::Ok, None, "CREATE completed");
    }

    // Walk segments top-down, resolving each prefix against the
    // current snapshot. On `Found`, descend into that container as
    // the parent. On `NotFound`, switch to the creation tail loop.
    //
    // When the path starts with `INBOX`, seed the walk with the
    // resolved role-Inbox id and skip segment 0 entirely — segment 0
    // is the reserved wire alias and cannot itself be created or
    // re-resolved. Skipping it also closes the
    // "tail-create synthesises a non-role INBOX" hazard a literal
    // walk would expose if the encoder ever omits the row.
    let start_index = if starts_with_inbox { 1 } else { 0 };
    let mut parent: Option<ContainerId> = inbox_id.filter(|_| starts_with_inbox);
    for i in start_index..segments.len() {
        let _seg = segments[i];
        let is_last = i == segments.len() - 1;
        let prefix: String = segments[..=i].join(&HIERARCHY_DELIMITER.to_string());
        match resolve(&prefix, &records) {
            ResolveResult::Found(id) => {
                if is_last {
                    if !had_trailing_slash {
                        return tagged(
                            tag,
                            Status::No,
                            Some("ALREADYEXISTS"),
                            "mailbox already exists",
                        );
                    }
                    // Idempotent existence probe. If the client also
                    // sent USE, the existing mailbox MUST already
                    // carry that role — otherwise we'd return OK and
                    // silently ignore the requested SPECIAL-USE.
                    if let LeafRoleCheck::Mismatch =
                        check_existing_leaf_role(&records, id, leaf_role)
                    {
                        return tagged(
                            tag,
                            Status::No,
                            Some("USEATTR"),
                            "mailbox exists with a different SPECIAL-USE attribute",
                        );
                    }
                    // Even if the target itself satisfies USE, refuse
                    // to OK an idempotent probe when *another* mailbox
                    // already holds the same role — the snapshot
                    // contains a pre-existing duplicate (e.g. from
                    // JMAP's documented non-atomic role snapshot path
                    // or a prior TOCTOU race) and re-OKing would
                    // perpetuate the ambiguity.
                    if let Some(role) = leaf_role
                        && other_role_holder_exists(&records, role, Some(id))
                    {
                        return tagged(
                            tag,
                            Status::No,
                            Some("USEATTR"),
                            "another mailbox already has that SPECIAL-USE attribute",
                        );
                    }
                    return tagged(tag, Status::Ok, None, "CREATE completed");
                }
                parent = Some(id);
            }
            ResolveResult::Unrepresentable => {
                return tagged(
                    tag,
                    Status::No,
                    Some("SERVERBUG"),
                    "mailbox name not representable over IMAP",
                );
            }
            ResolveResult::NotFound => {
                // Tail-create: build segments[i..] under `parent`. The
                // SPECIAL-USE role (if any) lands on the leaf segment
                // only; intermediates always get `None` so the encoder
                // doesn't accidentally tag a synthesised parent as
                // `\Drafts` etc.
                //
                // Pre-create role-uniqueness check: storage has no
                // `(account, role)` unique index, so we reject here if
                // any existing mailbox already carries `leaf_role`.
                // The target doesn't exist yet (`NotFound`), so the
                // exclusion-id is `None`.
                if let Some(role) = leaf_role
                    && other_role_holder_exists(&records, role, None)
                {
                    return tagged(
                        tag,
                        Status::No,
                        Some("USEATTR"),
                        "another mailbox already has that SPECIAL-USE attribute",
                    );
                }
                let mut current_parent = parent;
                for j in i..segments.len() {
                    let is_last_segment = j == segments.len() - 1;
                    let seg_owned = segments[j].to_string();
                    let role_for_seg = if is_last_segment { leaf_role } else { None };
                    let attrs = jmap_attrs_for_create(role_for_seg, 0, false);
                    let store_c = store.clone();
                    let cp = current_parent;
                    let res = tokio::task::spawn_blocking(move || {
                        store_c.create_mailbox(account_id, &seg_owned, cp, attrs)
                    })
                    .await;
                    let create_res = match res {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "CREATE spawn_blocking panicked");
                            return tagged(tag, Status::No, Some("SERVERBUG"), "create failed");
                        }
                    };
                    let is_last_j = j == segments.len() - 1;
                    match create_res {
                        Ok(new_id) => {
                            current_parent = Some(new_id);
                            if is_last_j {
                                return tagged(tag, Status::Ok, None, "CREATE completed");
                            }
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.starts_with("mailbox already exists") {
                                // Race: another session created this segment.
                                // For the final segment without trailing slash
                                // that's still ALREADYEXISTS — same answer the
                                // pre-walk resolve would have returned.
                                if is_last_j && !had_trailing_slash {
                                    return tagged(
                                        tag,
                                        Status::No,
                                        Some("ALREADYEXISTS"),
                                        "mailbox already exists",
                                    );
                                }
                                // Otherwise reload + re-resolve to discover the
                                // raced-in id rather than continuing with a
                                // stale parent.
                                records = match load_records(&store, account_id, tag).await {
                                    Ok(r) => r,
                                    Err(resp) => return resp,
                                };
                                let prefix_j: String =
                                    segments[..=j].join(&HIERARCHY_DELIMITER.to_string());
                                match resolve(&prefix_j, &records) {
                                    ResolveResult::Found(id) => {
                                        current_parent = Some(id);
                                        if is_last_j {
                                            // Trailing-slash idempotence wins,
                                            // but only if the raced-in mailbox
                                            // already carries the requested
                                            // SPECIAL-USE (if any). Otherwise
                                            // we'd return OK while ignoring
                                            // the USE the client asked for.
                                            if let LeafRoleCheck::Mismatch =
                                                check_existing_leaf_role(&records, id, leaf_role)
                                            {
                                                return tagged(
                                                    tag,
                                                    Status::No,
                                                    Some("USEATTR"),
                                                    "mailbox exists with a different SPECIAL-USE attribute",
                                                );
                                            }
                                            // Same belt-and-braces guard as
                                            // the non-race idempotent path:
                                            // refuse to perpetuate a
                                            // pre-existing duplicate-role
                                            // snapshot.
                                            if let Some(role) = leaf_role
                                                && other_role_holder_exists(
                                                    &records,
                                                    role,
                                                    Some(id),
                                                )
                                            {
                                                return tagged(
                                                    tag,
                                                    Status::No,
                                                    Some("USEATTR"),
                                                    "another mailbox already has that SPECIAL-USE attribute",
                                                );
                                            }
                                            return tagged(
                                                tag,
                                                Status::Ok,
                                                None,
                                                "CREATE completed",
                                            );
                                        }
                                    }
                                    ResolveResult::NotFound => {
                                        // Storage reported a conflict on this
                                        // segment, then it vanished before our
                                        // reload landed (create-then-destroy
                                        // race). Retry the create once, but
                                        // only if the parent chain itself is
                                        // still consistent — a missing parent
                                        // means the chain broke under us and
                                        // we should not synthesise around it.
                                        let parent_ok = match current_parent {
                                            None => true,
                                            Some(pid) => records.iter().any(|r| r.id == pid),
                                        };
                                        if !parent_ok {
                                            tracing::error!(
                                                prefix = %prefix_j,
                                                ?current_parent,
                                                "CREATE race: parent chain broken after reload"
                                            );
                                            return tagged(
                                                tag,
                                                Status::No,
                                                Some("SERVERBUG"),
                                                "create failed",
                                            );
                                        }
                                        let seg_retry = segments[j].to_string();
                                        let role_retry = if is_last_j { leaf_role } else { None };
                                        // Re-check role uniqueness on the
                                        // reloaded snapshot before retrying
                                        // — another session may have raced
                                        // in a holder we didn't see on the
                                        // first pass.
                                        if let Some(role) = role_retry
                                            && other_role_holder_exists(&records, role, None)
                                        {
                                            return tagged(
                                                tag,
                                                Status::No,
                                                Some("USEATTR"),
                                                "another mailbox already has that SPECIAL-USE attribute",
                                            );
                                        }
                                        let attrs_retry =
                                            jmap_attrs_for_create(role_retry, 0, false);
                                        let store_r = store.clone();
                                        let cp_retry = current_parent;
                                        let retry = tokio::task::spawn_blocking(move || {
                                            store_r.create_mailbox(
                                                account_id,
                                                &seg_retry,
                                                cp_retry,
                                                attrs_retry,
                                            )
                                        })
                                        .await;
                                        match retry {
                                            Ok(Ok(new_id)) => {
                                                current_parent = Some(new_id);
                                                if is_last_j {
                                                    return tagged(
                                                        tag,
                                                        Status::Ok,
                                                        None,
                                                        "CREATE completed",
                                                    );
                                                }
                                            }
                                            Ok(Err(e2)) => {
                                                tracing::warn!(
                                                    error = %e2,
                                                    prefix = %prefix_j,
                                                    "CREATE race retry: storage rejected on second attempt"
                                                );
                                                return tagged(
                                                    tag,
                                                    Status::No,
                                                    Some("SERVERBUG"),
                                                    "create failed",
                                                );
                                            }
                                            Err(e2) => {
                                                tracing::warn!(error = %e2, "CREATE retry spawn_blocking panicked");
                                                return tagged(
                                                    tag,
                                                    Status::No,
                                                    Some("SERVERBUG"),
                                                    "create failed",
                                                );
                                            }
                                        }
                                    }
                                    ResolveResult::Unrepresentable => {
                                        tracing::error!(
                                            prefix = %prefix_j,
                                            "CREATE race: prefix unrepresentable after reload"
                                        );
                                        return tagged(
                                            tag,
                                            Status::No,
                                            Some("SERVERBUG"),
                                            "create failed",
                                        );
                                    }
                                }
                            } else if msg.starts_with("mailbox special-use already in use") {
                                // Storage-layer SPECIAL-USE uniqueness
                                // barrier closed the TOCTOU window
                                // between the pre-walk snapshot scan
                                // and the in-tx commit (peer CREATE
                                // landed first). Maps cleanly to
                                // NO [USEATTR].
                                return tagged(
                                    tag,
                                    Status::No,
                                    Some("USEATTR"),
                                    "another mailbox already has that SPECIAL-USE attribute",
                                );
                            } else if msg.starts_with("reserved name")
                                || msg.starts_with("mailbox invalid name")
                            {
                                return tagged(tag, Status::No, Some("CANNOT"), &msg);
                            } else if msg.starts_with("mailbox parent not found") {
                                tracing::error!(
                                    error = %msg,
                                    "CREATE walk: parent not found despite top-down construction"
                                );
                                return tagged(tag, Status::No, Some("SERVERBUG"), "create failed");
                            } else {
                                tracing::warn!(error = %msg, "CREATE storage error");
                                return tagged(tag, Status::No, Some("SERVERBUG"), "create failed");
                            }
                        }
                    }
                }
                // The inner loop always returns on the last segment.
                tracing::error!("CREATE walk tail fell through without returning");
                return tagged(tag, Status::No, Some("SERVERBUG"), "create failed");
            }
        }
    }
    tracing::error!("CREATE walk fell through without returning");
    tagged(tag, Status::No, Some("SERVERBUG"), "create failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_atom() {
        let a = parse_args("Archive").unwrap();
        assert_eq!(a.name, "Archive");
        assert!(a.use_flags.is_none());
    }

    #[test]
    fn parse_args_quoted_with_space() {
        let a = parse_args(r#""Archive/2025 plans""#).unwrap();
        assert_eq!(a.name, "Archive/2025 plans");
        assert!(a.use_flags.is_none());
    }

    #[test]
    fn parse_args_with_single_use_flag() {
        let a = parse_args(r"Drafts (USE (\Drafts))").unwrap();
        assert_eq!(a.name, "Drafts");
        let flags = a.use_flags.expect("USE present");
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, r"\Drafts");
    }

    #[test]
    fn parse_args_with_empty_use_list() {
        let a = parse_args("Archive (USE ())").unwrap();
        assert_eq!(a.name, "Archive");
        assert_eq!(a.use_flags.as_deref(), Some(&[][..]));
    }

    #[test]
    fn parse_args_with_multiple_use_flags() {
        let a = parse_args(r"Foo (USE (\Drafts \Sent))").unwrap();
        assert_eq!(a.use_flags.as_ref().map(|v| v.len()), Some(2));
    }

    #[test]
    fn parse_args_rejects_non_use_parameter() {
        let e = parse_args(r"Foo (RETURN (\Drafts))").unwrap_err();
        assert!(e.contains("USE"), "{e}");
    }

    #[test]
    fn parse_args_rejects_unbalanced_paren() {
        assert!(parse_args(r"Foo (USE (\Drafts").is_err());
        assert!(parse_args(r"Foo (USE (\Drafts)").is_err());
    }

    #[test]
    fn role_from_use_flags_known_flag() {
        let f = Some(vec![UseFlag(r"\Drafts".to_string())]);
        assert_eq!(role_from_use_flags(&f).unwrap(), Some(MailboxRole::Drafts));
    }

    #[test]
    fn role_from_use_flags_rejects_inbox() {
        let f = Some(vec![UseFlag(r"\Inbox".to_string())]);
        let e = role_from_use_flags(&f).unwrap_err();
        assert!(e.contains("Inbox"), "{e}");
    }

    #[test]
    fn role_from_use_flags_rejects_unknown() {
        let f = Some(vec![UseFlag(r"\Bogus".to_string())]);
        assert!(role_from_use_flags(&f).is_err());
    }

    #[test]
    fn parse_args_use_keyword_is_case_insensitive() {
        // RFC 9051 §1.2 — IMAP keywords are case-insensitive.
        let a = parse_args(r"Drafts (use (\Drafts))").unwrap();
        assert_eq!(a.use_flags.as_ref().map(|v| v.len()), Some(1));
        let a = parse_args(r"Drafts (UsE (\Drafts))").unwrap();
        assert_eq!(a.use_flags.as_ref().map(|v| v.len()), Some(1));
    }

    #[test]
    fn role_from_use_flags_flag_is_case_insensitive() {
        let f = Some(vec![UseFlag(r"\drafts".to_string())]);
        assert_eq!(role_from_use_flags(&f).unwrap(), Some(MailboxRole::Drafts));
        let f = Some(vec![UseFlag(r"\DRAFTS".to_string())]);
        assert_eq!(role_from_use_flags(&f).unwrap(), Some(MailboxRole::Drafts));
        let f = Some(vec![UseFlag(r"\InBoX".to_string())]);
        // Still rejected — `\Inbox` is the role we forbid CREATE from
        // assigning, regardless of input casing.
        assert!(role_from_use_flags(&f).is_err());
    }

    #[test]
    fn role_from_use_flags_rejects_multiple() {
        let f = Some(vec![
            UseFlag(r"\Drafts".to_string()),
            UseFlag(r"\Sent".to_string()),
        ]);
        let e = role_from_use_flags(&f).unwrap_err();
        assert!(e.contains("only one"), "{e}");
    }

    #[test]
    fn parse_args_rejects_empty() {
        assert!(parse_args("").is_err());
    }

    #[test]
    fn parse_args_rejects_empty_quoted() {
        assert!(parse_args(r#""""#).is_err());
    }

    #[test]
    fn parse_args_rejects_trailing_tokens() {
        // After the mailbox name, only an RFC 6154 `(USE (...))`
        // clause is accepted; a bare unparenthesised token is `BAD`.
        let e = parse_args("Archive extra").unwrap_err();
        assert!(
            e.contains("USE") || e.contains("'('") || e.contains("trailing"),
            "{e}"
        );
    }

    #[test]
    fn parse_args_rejects_trailing_after_use_clause() {
        let e = parse_args(r"Foo (USE (\Drafts)) leftover").unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn validate_segment_allows_printable_ascii() {
        assert!(validate_segment("Archive").is_ok());
        assert!(validate_segment("Folder 1").is_ok());
        assert!(validate_segment("inbox").is_ok());
    }

    #[test]
    fn validate_segment_rejects_empty() {
        assert!(validate_segment("").is_err());
    }

    #[test]
    fn validate_segment_rejects_non_ascii() {
        let e = validate_segment("Caf\u{00e9}").unwrap_err();
        assert!(e.contains("non-ASCII"), "{e}");
    }

    #[test]
    fn validate_segment_rejects_ampersand() {
        let e = validate_segment("R&D").unwrap_err();
        assert!(e.contains("UTF-7"), "{e}");
    }

    #[test]
    fn validate_segment_rejects_reserved() {
        let e = validate_segment(UPLOAD_STAGING_NAME).unwrap_err();
        assert!(e.contains("reserved"), "{e}");
    }

    #[test]
    fn validate_segment_rejects_control_byte() {
        let e = validate_segment("foo\nbar").unwrap_err();
        assert!(e.contains("non-ASCII"), "{e}");
    }
}
