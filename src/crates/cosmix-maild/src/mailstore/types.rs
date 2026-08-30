//! `MailStore`-facing record types.
//!
//! These shapes are the adapter's externally-visible row types
//! consumed by the JMAP / SMTP layers. They are intentionally narrower
//! than `cosmix-mds`'s native types — only the fields a mail-protocol
//! handler needs are surfaced — so callers cannot reach into MDS
//! internals and so the trait surface remains stable when MDS internal
//! types evolve.

use cosmix_mds::{BlobHash, ContainerAttrs, ContainerId, Flags, ItemId, Tags};

/// JMAP thread identifier (RFC 8621 §4.2). String-typed because the
/// `mail_threads.thread_id` column is `TEXT NOT NULL` (cosmix-mds v1.1
/// schema, see `cosmix-mds/src/schema/v1_1.sql` §6) and the value is
/// minted upstream — historically as a `Uuid` string by
/// `cosmix-maild::db::thread::find_or_create`, but the storage layer
/// is opaque to the format. Threads stay in MailStore per the v1.1
/// amendment §6; the JMAP layer above this surface emits the same
/// string verbatim as the `Email.threadId` JSON property.
///
/// The inner field is `pub` intentionally: this newtype is a
/// pass-through marker around an opaque-format string, not a
/// structural-invariant guard like [`ItemId`] (which wraps `Uuid`)
/// or [`ContainerId`] (likewise). Callers — JMAP handlers,
/// thread-id generators, tests — construct values directly from
/// upstream sources and round-trip them through the storage layer
/// unchanged. The DB binds via `?2` parameter, so SQL-injection
/// is not a concern. If a future invariant tightens the format
/// (e.g., enforced UUID-only), the field becomes `pub(crate)` and
/// adds a `try_from`/`new` constructor at that point.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadId(pub String);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// JMAP `Email/changes` (RFC 8621 §4.1.5) projection over the
/// MDS set-wide change stream.
///
/// `new_state` is the largest `set_change_seq` covered by this
/// response — the JMAP layer surfaces it as the new state token
/// the client persists for its next `Email/changes` call. When
/// no events have occurred since `since_state`, `new_state ==
/// since_state`.
///
/// `created` / `updated` / `destroyed` partition the unique
/// `ItemId`s touched in the window by their *net effect* at
/// `new_state` versus `since_state`:
///
/// * an item that did not exist at `since_state` but exists at
///   `new_state` → `created`;
/// * an item that existed at both, with at least one event in
///   the window → `updated`;
/// * an item that existed at `since_state` but does not exist
///   at `new_state` → `destroyed`;
/// * an item created and destroyed within the window is
///   *omitted entirely* (RFC 8621 §4.1.5: clients never observed
///   it, so it should not appear in any of the three arrays).
///
/// "Exists at `new_state`" means "has at least one membership
/// row" — the same predicate `EmailRecord::mailbox_ids` uses.
/// A move (`Removed` from src + `Added` to dest in the same
/// window) classifies as `updated`, not `created` + `destroyed`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailChangeSet {
    pub new_state: u64,
    pub created: Vec<ItemId>,
    pub updated: Vec<ItemId>,
    pub destroyed: Vec<ItemId>,
}

/// JMAP `Mailbox/changes` (RFC 8621 §2.6) projection over the
/// MDS account-wide container lifecycle stream
/// (`container_change_set`) paired with the set-wide
/// `set_change` tip for count-derived `updated` synthesis.
///
/// `new_state` is the paired state token `"<lifecycle_seq>:<set_change_seq>"`
/// the JMAP layer surfaces; clients persist it verbatim for their
/// next call. See `_doc/planned/mailbox-changes-substrate.md`
/// §"State token" for the pagination phasing — truncated pages
/// advance only the lifecycle component, final pages advance both
/// and synthesise count-derived `updated` entries from the
/// `set_change` window.
///
/// Precedence rules (substrate is authoritative; JMAP handler
/// surfaces verbatim):
///
/// * A container that appears as `CONTAINER_CREATED` and in the
///   set_change window → `created` only (the creation already
///   implies any same-window count change).
/// * A container that appears as `CONTAINER_DESTROYED` and in the
///   set_change window → `destroyed` only (no count signal can
///   apply to a destroyed mailbox).
/// * A container that appears with `CONTAINER_RENAMED` /
///   `CONTAINER_ATTRS_CHANGED` and in the set_change window →
///   one `updated` entry; `updated_properties` becomes `None`
///   ("any may have changed") because count is one of the
///   changed dimensions.
/// * A container present only in the set_change window (no
///   lifecycle row) → one `updated` entry from count synthesis;
///   `updated_properties` becomes `None`.
/// * A container created and destroyed within the same lifecycle
///   window is omitted entirely — RFC 8621 §2.6 says the client
///   never observed it.
///
/// `updated_properties` is `None` when any updated entry was
/// driven by count synthesis (in which case the client must
/// refresh all properties). A `Some(list)` value would be emitted
/// only when *every* updated entry had a known, identical
/// property-set; Phase 2 always returns `None` for simplicity
/// (matches the conservative end of RFC 8621 §2.6's `null`-or-list
/// contract). A later refinement can tighten this to the
/// intersection across entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxChangeSet {
    pub new_state: String,
    pub has_more_changes: bool,
    pub created: Vec<ContainerId>,
    pub updated: Vec<ContainerId>,
    pub destroyed: Vec<ContainerId>,
    pub updated_properties: Option<Vec<String>>,
}

/// One placement of an item in a mailbox, with the IMAP-shaped UID
/// triple needed for `OK [APPENDUID …]` (RFC 4315) on a future IMAP
/// `APPEND` and for the JMAP `Email/set create` membership-set
/// identification.
///
/// `container_id` is the MDS container the membership lives in. `uid`
/// is the membership row's `seq` value within that container —
/// monotonically increasing from 1, the IMAP UID. `uid_validity` is
/// the container's `seq_validity` value — the IMAP UIDVALIDITY that
/// pairs with `uid` for an unambiguous (mailbox, uid) → message
/// resolution across UIDVALIDITY rotations.
///
/// All three fields are read inside the same `with_set_tx` that
/// produced the membership row, eliminating the
/// follow-up-read-can-see-a-different-validity hazard for freshly-
/// inserted memberships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipUid {
    pub container_id: ContainerId,
    pub uid: u64,
    pub uid_validity: u64,
}

/// JMAP `Mailbox.role` (RFC 8621 §2.0). Mapped 1:1 from the IMAP
/// special-use attribute carried in [`ContainerAttrs::special_use`]
/// (RFC 6154 — `\All`, `\Archive`, `\Drafts`, `\Flagged`, `\Junk`,
/// `\Sent`, `\Trash`; RFC 8457 adds `\Important`; RFC 8621 §10.5.1
/// registers `\Inbox` as JMAP-only). Storage stores the IMAP
/// attribute literal; the adapter projects to this enum on read so
/// the JMAP layer can serialise a stable lower-case wire token
/// without re-parsing the backslash-prefixed string at every
/// handler.
///
/// Wire-token validity: RFC 8621 §2 requires `Mailbox.role` values
/// to come from the IANA "IMAP Mailbox Name Attributes" registry
/// (originally RFC 6154; extended by RFC 8457 and RFC 8621 §10.5),
/// converted to lowercase. Every variant here corresponds to one
/// of those registered attributes. The JMAP `isSubscribed` flag is
/// not modelled as a role variant here even though `\Subscribed`
/// is also a registered IMAP attribute (RFC 5258), because RFC
/// 8621 §2 carries subscription as a distinct `isSubscribed`
/// property; that bit lives on `MailboxRecord::is_subscribed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MailboxRole {
    Inbox,
    Archive,
    Drafts,
    Flagged,
    Important,
    Junk,
    Sent,
    Trash,
    All,
}

impl MailboxRole {
    /// Map an IMAP `\Special-Use` attribute (RFC 6154; RFC 8457
    /// adds `\Important`; RFC 8621 §10.5.1 adds `\Inbox`) to a
    /// JMAP role. Unknown attributes — including IMAP attributes
    /// JMAP doesn't expose via the `role` property — return `None`
    /// so they project as a `null` JMAP `role` rather than
    /// misclassifying.
    pub fn from_special_use(s: &str) -> Option<Self> {
        match s {
            "\\Inbox" => Some(Self::Inbox),
            "\\Archive" => Some(Self::Archive),
            "\\Drafts" => Some(Self::Drafts),
            "\\Flagged" => Some(Self::Flagged),
            "\\Important" => Some(Self::Important),
            "\\Junk" => Some(Self::Junk),
            "\\Sent" => Some(Self::Sent),
            "\\Trash" => Some(Self::Trash),
            "\\All" => Some(Self::All),
            _ => None,
        }
    }

    /// JMAP wire token for `Mailbox.role`. Lower-case per RFC 8621 §2.
    pub fn as_jmap_str(&self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Archive => "archive",
            Self::Drafts => "drafts",
            Self::Flagged => "flagged",
            Self::Important => "important",
            Self::Junk => "junk",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::All => "all",
        }
    }

    /// Inverse of [`Self::as_jmap_str`]. Map a JMAP `Mailbox.role` wire
    /// token to its variant; unknown tokens return `None` so the JMAP
    /// `Mailbox/set` create handler can reject them as
    /// `invalidProperties`.
    pub fn from_jmap_str(s: &str) -> Option<Self> {
        match s {
            "inbox" => Some(Self::Inbox),
            "archive" => Some(Self::Archive),
            "drafts" => Some(Self::Drafts),
            "flagged" => Some(Self::Flagged),
            "important" => Some(Self::Important),
            "junk" => Some(Self::Junk),
            "sent" => Some(Self::Sent),
            "trash" => Some(Self::Trash),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// IMAP `\Special-Use` attribute (RFC 6154; RFC 8457 / RFC 8621
    /// §10.5 extensions) for this role. Inverse of
    /// [`Self::from_special_use`] — used by the JMAP `Mailbox/set`
    /// create path to encode `role` into [`ContainerAttrs::special_use`].
    pub fn as_special_use(&self) -> &'static str {
        match self {
            Self::Inbox => "\\Inbox",
            Self::Archive => "\\Archive",
            Self::Drafts => "\\Drafts",
            Self::Flagged => "\\Flagged",
            Self::Important => "\\Important",
            Self::Junk => "\\Junk",
            Self::Sent => "\\Sent",
            Self::Trash => "\\Trash",
            Self::All => "\\All",
        }
    }
}

/// JMAP `Mailbox.myRights` (RFC 8621 §2.0). Per-account ACL mask the
/// JMAP client uses to gate UI affordances. Cosmix is a personal
/// substrate today (single-tenant accounts, no shared mailboxes), so
/// the canonical projection is [`MailboxRights::full`]; the struct
/// exists to (a) keep the JMAP wire shape honest and (b) leave room
/// for shared-mailbox ACLs later without re-shaping the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxRights {
    pub may_read_items: bool,
    pub may_add_items: bool,
    pub may_remove_items: bool,
    pub may_set_seen: bool,
    pub may_set_keywords: bool,
    pub may_create_child: bool,
    pub may_rename: bool,
    pub may_delete: bool,
    pub may_submit: bool,
}

impl MailboxRights {
    /// Owner-of-mailbox rights — every bit set. Default for the
    /// personal-substrate, single-tenant-account case.
    pub fn full() -> Self {
        Self {
            may_read_items: true,
            may_add_items: true,
            may_remove_items: true,
            may_set_seen: true,
            may_set_keywords: true,
            may_create_child: true,
            may_rename: true,
            may_delete: true,
            may_submit: true,
        }
    }
}

impl Default for MailboxRights {
    fn default() -> Self {
        Self::full()
    }
}

/// One row of `MailStore::list_mailboxes`.
///
/// `total_emails` / `unread_emails` come from `Mds::container_status`,
/// which already maintains running counts inside MDS (see SPEC 08 §3
/// "ContainerStatus is a precomputed projection"). They are surfaced
/// as `u64` to match the underlying type — narrowing to JMAP's
/// `UnsignedInt` (53-bit per RFC 8621 §2) is the response-shape
/// layer's job, not the storage adapter's. Silently saturating in
/// the adapter would lie about corrupted state; per Codex review
/// 019dffa6.
///
/// `role` / `sort_order` / `is_subscribed` / `my_rights` are the
/// RFC 8621 §2.0 JMAP projection over the underlying
/// [`cosmix_mds::ContainerAttrs`]:
///
/// * `role` derives from `attrs.special_use` via [`MailboxRole::from_special_use`].
/// * `is_subscribed` mirrors `attrs.subscribed` 1:1.
/// * `sort_order` and `my_rights` ride on `attrs.extra` — an opaque
///   `serde_json::Value` carried verbatim through MDS — under the
///   well-known keys `"jmap_sort_order"` (number) and
///   `"jmap_my_rights"` (object, schema matches [`MailboxRights`]).
///   Pre-existing rows with no `extra` entries default to
///   `sort_order = 0` and `my_rights = MailboxRights::full()`. The
///   defaults are populated in code, not via SQL backfill — `attrs`
///   is opaque JSON, and a missing key is the canonical "default"
///   signal across the v1 / v1.1 / v1.2 schema versions.
#[derive(Debug, Clone)]
pub struct MailboxRecord {
    pub id: ContainerId,
    pub name: String,
    pub parent: Option<ContainerId>,
    pub attrs: ContainerAttrs,
    pub total_emails: u64,
    pub unread_emails: u64,
    /// JMAP `Mailbox.role` projection of `attrs.special_use`. `None`
    /// means the mailbox has no recognised role.
    pub role: Option<MailboxRole>,
    /// JMAP `Mailbox.sortOrder`. Defaults to 0 for rows whose
    /// `attrs.extra` does not carry `"jmap_sort_order"`.
    pub sort_order: u32,
    /// JMAP `Mailbox.isSubscribed` — passes `attrs.subscribed`
    /// through unchanged.
    pub is_subscribed: bool,
    /// JMAP `Mailbox.myRights`. Defaults to [`MailboxRights::full`]
    /// for rows whose `attrs.extra` does not carry `"jmap_my_rights"`.
    pub my_rights: MailboxRights,
    /// IMAP UIDVALIDITY for this mailbox (RFC 9051 §2.3.1.1). Sourced
    /// from `container.seq_validity` — allocated once at container
    /// creation, never mutated except via the explicit "rebuild" path.
    /// Stable for the life of the mailbox; a UIDVALIDITY change is
    /// the IMAP signal to a client that it must full-resync. See
    /// `_doc/planned/imap-runway.md` §P6.
    pub uid_validity: u64,
    /// IMAP HIGHESTMODSEQ for this mailbox (RFC 7162 CONDSTORE).
    /// Sourced from `container.change_seq` — a per-container counter
    /// bumped on every membership add / remove / flag change.
    /// **Not** `MAX(membership.change_seq)`: per-membership max is
    /// non-monotonic across EXPUNGE because rows are deleted before
    /// the container counter bumps. See `_doc/planned/imap-runway.md`
    /// §P6 for the regression this guards against.
    pub highest_mod_seq: u64,
    /// IMAP UIDNEXT for this mailbox (RFC 9051 §7.3.2). Sourced from
    /// `container.next_seq` — the seq the next inserted membership
    /// will allocate. Monotonically non-decreasing for the life of
    /// the mailbox; never reused after EXPUNGE. Widened from MDS's
    /// `Seq(u32)` to `u64` to match the rest of the IMAP-shaped
    /// surface on this record. Required by the SELECT/EXAMINE/STATUS
    /// handlers in IMAP Phase 2 (`_doc/maild/imap.md` §"Phase 2").
    pub uid_next: u64,
}

/// Per-mailbox admin stats, returned by
/// [`crate::mailstore::SqliteMailStore::mailbox_stats`]. A lean
/// projection for the `maild.stats.*` Bus surface (doveadm `mailbox
/// status` analog) — counts + bytes, none of the JMAP/IMAP wire fields
/// `MailboxRecord` carries.
#[derive(Debug, Clone)]
pub struct MailboxStat {
    pub name: String,
    pub role: Option<MailboxRole>,
    pub total_emails: u64,
    pub unread_emails: u64,
    pub size_bytes: u64,
}

/// Account-wide rollup, returned by
/// [`crate::mailstore::SqliteMailStore::account_stats`]. `message_count`
/// and `total_bytes` are **distinct** (a multi-homed message counts
/// once), unlike a naive sum of [`MailboxStat`] folder counts.
#[derive(Debug, Clone, Default)]
pub struct AccountStat {
    pub mailbox_count: u64,
    pub message_count: u64,
    pub total_bytes: u64,
}

/// Effective retention parameters for one
/// [`crate::mailstore::SqliteMailStore::retention_sweep_account`] pass.
/// A mailstore-local mirror of the `maild.retention` substrate config so
/// the storage primitive does not depend on the props layer (the worker
/// bridges the two). A `*_retention_days` of 0 disables that folder.
#[derive(Debug, Clone)]
pub struct RetentionParams {
    pub junk_retention_days: u64,
    pub trash_retention_days: u64,
    /// Per-account ceiling on membership removals this sweep, shared
    /// across Junk then Trash. The remainder bleeds down on later ticks.
    pub max_deletes_per_sweep: u64,
    /// When true, count what WOULD be removed and remove nothing.
    pub dry_run: bool,
}

/// Per-account retention sweep outcome, returned by
/// [`crate::mailstore::SqliteMailStore::retention_sweep_account`]. In
/// dry-run the `*_removed` counts are what WOULD have been removed. A
/// `*_capped` flag means more aged memberships were eligible than the
/// per-sweep cap allowed — the remainder is left for the next tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionSweepReport {
    pub junk_removed: u64,
    pub trash_removed: u64,
    pub junk_capped: bool,
    pub trash_capped: bool,
}

impl RetentionSweepReport {
    /// Total memberships removed (or, in dry-run, that would be removed).
    pub fn total_removed(&self) -> u64 {
        self.junk_removed + self.trash_removed
    }
}

/// JMAP `Email/get` envelope projection.
///
/// One row per item in `mail_envelopes` (cosmix-mds v1.2 + v1.3
/// columns, see `cosmix-mds/src/schema/v1_2.sql` §A and
/// `cosmix-mds/src/schema/v1_3.sql`). The fields are RFC 5322 header
/// values frozen at delivery time — they are not re-derived from
/// `item.blob_hash` on read.
///
/// Strings are stored verbatim (the writer is responsible for any MIME
/// decoding); `to`/`cc`/`bcc`/`reply_to` are JSON-encoded arrays of
/// addr-specs; `date` is the RFC 5322 origination time as unix-seconds
/// (which is distinct from `received_at` on the underlying MDS item —
/// the latter is delivery time, the former is the sender's stamp).
///
/// `cc`/`bcc`/`reply_to` were added in v1.3 to close the substrate
/// gap that previously forced JMAP `EmailSubmission/set` auto-detect
/// (RFC 8621 §7.1.2) to fall back to the legacy `db::email` table for
/// CC recovery. Items written before v1.3 carry empty arrays for
/// these fields (the migration backfills `'[]'` defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailEnvelope {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub subject: String,
    pub date: i64,
    pub message_id: Option<String>,
}

/// Full JMAP `Email/get` row returned by `MailStore::get_email`.
///
/// **Plan deviation note.** The migration plan's Task 1.6 spec
/// describes `EmailRecord = EmailHandle + envelope metadata + body
/// blob hash`. `EmailHandle` (Task 1.5) is per-membership (carries a
/// container `seq` and the keywords for one specific membership);
/// `get_email` does not take a container, so embedding `EmailHandle`
/// directly would require picking an arbitrary membership and would
/// misrepresent the JMAP `keywords` semantics (RFC 8621 §4.1.4 — the
/// `keywords` JMAP property is the **union** of per-mailbox flags, not
/// any one membership's flags).
///
/// Instead, `EmailRecord` carries the item-level identity and the
/// merged JMAP shape: `keywords` is the OR of every membership's
/// flags, and `mailbox_ids` enumerates every container the item
/// belongs to (driving JMAP `mailboxIds`). The `blob_hash` is the
/// underlying CAS hash; per the same Task 1.5 deviation, the
/// per-account `BlobId` alias is minted by Task 1.7's `create_email`
/// and looked up at JMAP serialise time, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailRecord {
    pub id: ItemId,
    pub blob_hash: BlobHash,
    pub size_bytes: u64,
    pub received_at: i64,
    pub envelope: EmailEnvelope,
    /// Union of `Flags` across every membership of this item, matching
    /// JMAP `Email.keywords` semantics (RFC 8621 §4.1.4).
    pub keywords: Flags,
    /// Union of user keywords (`Tags`) across every membership, the
    /// `Tags` analogue of `keywords`. JMAP `Email.keywords` is the
    /// per-item union of the per-mailbox keyword sets (RFC 8621
    /// §4.1.4), so the JMAP layer unions these with the system flags
    /// when projecting the wire `keywords` object. Empty for the
    /// common system-flags-only message.
    pub tags: Tags,
    /// Every container the item is currently a member of. Empty if
    /// the item exists in `item` but no membership rows survive — a
    /// transient state the JMAP layer treats as "not yet visible".
    pub mailbox_ids: Vec<ContainerId>,
}
