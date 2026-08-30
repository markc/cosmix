//! `MailStore` — mail-aware adapter over `cosmix-mds`.
//!
//! v1.1 amendment §"Concrete trait deltas — MailStore" (Phase 8d):
//! every cross-layer write (MDS mutation + sidecar table row) goes
//! through `SqliteCasMds::with_set_tx` so the two writes commit
//! atomically. Side-effect publishes (Bayesian retrain, FTS rebuild)
//! ride on the outbox / projection tables and run out-of-band.
//!
//! See `_doc/2026-05-02-cosmix-mds-spec-v1_1-amendment.md` §1, §4b
//! and `_doc/2026-04-29-cosmix-mds-plan.md` §8d.
//!
//! Phase 8d.2 lands this module as a pure adapter; JMAP/SMTP wiring
//! arrives in subsequent phases (Phase A "JMAP migration onto MDS"
//! per the v1.1 amendment §5). Until then, dead-code warnings on the
//! public surface are expected.
#![allow(dead_code, unused_imports)]

pub mod expiry;
pub mod retrain;
mod search;
mod setid;
mod subject;
mod types;

pub use types::{
    AccountStat, EmailChangeSet, EmailEnvelope, EmailRecord, MailboxChangeSet, MailboxRecord,
    MailboxRights, MailboxRole, MailboxStat, MembershipUid, RetentionParams, RetentionSweepReport,
    ThreadId,
};

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use cosmix_mds::{
    BlobHash, ChangeKind, ContainerAttrs, ContainerEvent, ContainerId, Flags, ItemId, Mds, Seq,
    SeqRange, SetChangeToken, SetId, SqliteCasMds, Tags, blob, container::UPLOAD_STAGING_NAME,
};
use rusqlite::{OptionalExtension, params};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use search::build_search_projection;

pub use setid::{MAILD_SET_NAMESPACE, account_id_to_setid};

/// Local alias for clarity at call sites — accounts are identified by
/// their integer PK from the `accounts` table.
pub type AccountId = i32;

/// JMAP-visible blob identifier — one entry per `blob_refs` row.
///
/// Distinct from the MDS `BlobHash` (the BLAKE3 of the bytes) because
/// JMAP requires per-account aliases that can carry TTLs and outlive
/// (or pre-date) any specific `item.blob_hash` reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobId(pub Uuid);

impl BlobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BlobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Three-state result of [`MailStore::resolve_blob_ref`].
///
/// Carries enough state for callers to distinguish "alias never minted"
/// (legacy fallback OK during the Task-3.3a migration window) from
/// "alias minted but its CAS bytes are gone" (real `blobNotFound`,
/// MUST NOT fall back to legacy lookup). See the trait method
/// doc-comment for the full contract.
#[derive(Debug)]
pub enum BlobRefLookup {
    /// Alias row found and the underlying CAS blob exists.
    Found(BlobHash),
    /// Alias row absent. Caller may try a legacy lookup during the
    /// Task-3.3a migration window; after migration this is a real
    /// `blobNotFound`.
    NotFound,
    /// Alias row exists but its CAS bytes are gone (staging GC, core
    /// MDS GC, or operator surgery). Always a real `blobNotFound`;
    /// callers MUST NOT fall back to legacy storage.
    Dangling,
}

/// IMAP / RFC-6154 special-use attribute used to detect Junk-boundary
/// moves for the Bayesian retrain outbox.
const SPECIAL_USE_JUNK: &str = "\\Junk";

/// IMAP / RFC-6154 special-use attribute for the Trash mailbox. Used
/// (with `SPECIAL_USE_JUNK`) by the retention sweep to resolve the two
/// delete-bucket roles by attribute — never by name, so a localized or
/// renamed folder still binds and a config typo can't point retention at
/// the Inbox.
const SPECIAL_USE_TRASH: &str = "\\Trash";

/// Defensive ceilings the retention primitive clamps its (public,
/// caller-supplied) [`RetentionParams`] to before any arithmetic, so an
/// out-of-range value can't overflow the cutoff math or turn the SQLite
/// `LIMIT` negative. These mirror the `maild.retention` namespace bounds
/// (`crate::props::retention`); the primitive must not trust its caller.
/// ≈100-year day ceiling keeps `days * 86_400_000` far inside i64.
const RETENTION_MAX_DAYS: u64 = 36_500;
const RETENTION_MAX_DELETES_PER_SWEEP: u64 = 1_000_000;

/// IMAP / RFC-6154 special-use attribute identifying the per-account
/// inbox. Used by `MailStore::destroy_mailbox` with
/// [`DestroyPolicy::MoveToInbox`].
const SPECIAL_USE_INBOX: &str = "\\Inbox";

/// On-emails policy for `MailStore::destroy_mailbox`.
///
/// JMAP `Mailbox/set destroy` (RFC 8621 §2.5) defines two client-
/// observable shapes: reject if the mailbox holds messages (default,
/// no flag) or destroy emails along with the mailbox
/// (`onDestroyRemoveEmails: true`). [`DestroyPolicy::Reject`] and
/// [`DestroyPolicy::Delete`] map directly. [`DestroyPolicy::MoveToInbox`]
/// is a maild-side extension for operator workflows ("clean up this
/// mailbox without losing messages") and is not exposed to JMAP
/// clients in Phase A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestroyPolicy {
    /// Refuse to destroy if any item still has membership in this
    /// mailbox. The container itself is left untouched. Surfaced as
    /// `"mailbox not empty"`.
    Reject,
    /// Move every item from this mailbox to the per-account
    /// `\Inbox` container, then destroy the now-empty source. The
    /// inbox is identified by `attrs.special_use == "\\Inbox"`, not
    /// by container name — accounts whose inbox container was
    /// provisioned without that attribute will hit `"mailbox has no
    /// inbox"` even if a container literally named `Inbox` exists.
    /// Items already in inbox have their membership in the source
    /// dropped rather than re-added (avoiding a no-op `move_item`
    /// failure on duplicate destination). Errors with
    /// `"mailbox has no inbox"` if the account has no `\Inbox`
    /// special-use container, or `"mailbox conflicting policy"` if
    /// the source *is* the inbox.
    MoveToInbox,
    /// Drop the source membership for every item; items left with
    /// zero memberships have their cross-DB `blob_ref` released and
    /// their `item` row removed (mirrors
    /// [`cosmix_mds::SqliteSetTx::remove_membership`] cascade).
    /// Then destroy the now-empty container.
    Delete,
}

/// Sort key for `MailStore::list_emails_in_mailbox`.
///
/// Both keys sort ascending; descending order is a JMAP wire-layer
/// concern (`Email/query` flips the comparison at serialize time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Order by `item.received_at` (the delivery timestamp recorded on
    /// the underlying MDS item, not the per-membership `added_at`).
    /// Ties break on `seq` to keep the order deterministic.
    #[default]
    ReceivedAt,
    /// Order by per-membership `seq` — i.e. the order in which the
    /// item joined this container. Matches `mds::list_items`'s
    /// natural ORDER BY.
    Seq,
}

/// Pagination + sort options for `MailStore::list_emails_in_mailbox`.
///
/// `limit == 0` returns an empty `Vec` (no rows requested), matching
/// the JMAP `Email/query` `limit: 0` shape used for "count only"
/// probes once that surface arrives. `offset` past the end of the
/// result set returns an empty `Vec` rather than an error.
#[derive(Debug, Clone, Copy)]
pub struct ListOpts {
    pub limit: u32,
    pub offset: u32,
    pub sort_by: SortKey,
}

impl Default for ListOpts {
    fn default() -> Self {
        Self {
            limit: u32::MAX,
            offset: 0,
            sort_by: SortKey::default(),
        }
    }
}

/// Storage-layer projection of the JMAP `Email/query` filter shape
/// (RFC 8621 §4.4.1) — Phase A subset.
///
/// Phase A supports filters that fall out of `Mds::list_items` rows
/// (`received_at`, `size_bytes`) and `Mds::item_memberships` (union
/// keywords, membership-set predicates). Envelope substring filters
/// (`from`, `to`, `cc`, `subject`) and full-text (`text`, `body`)
/// require a `mail_envelopes` join (or FTS index) and arrive in
/// Task 3.6 alongside the JMAP `Email/query` handler. The JMAP layer
/// is responsible for translating per-method client args into this
/// shape and for surfacing `unsupportedFilter` for any condition the
/// backend does not yet support — `EmailQueryFilter` is intentionally
/// the *capability surface* of the storage adapter, not the wire
/// surface.
///
/// `before` is exclusive (`received_at < before`); `after` is
/// inclusive (`received_at >= after`); `min_size` / `max_size` are
/// both inclusive. `has_keyword` / `not_keyword` operate on the
/// **union** of `Flags` across an item's memberships, matching JMAP
/// `keywords` semantics (RFC 8621 §4.1.4); `has_tag` / `not_tag` are
/// the user-keyword (`Tags`) counterparts and operate on the union of
/// `Tags` across memberships the same way.
#[derive(Debug, Clone, Default)]
pub struct EmailQueryFilter {
    pub in_mailbox: Option<ContainerId>,
    pub in_mailbox_other_than: Option<Vec<ContainerId>>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub has_keyword: Option<Flags>,
    pub not_keyword: Option<Flags>,
    /// User-keyword (`Tags`) analogue of `has_keyword`: keep only items
    /// whose membership-union `Tags` contains this exact keyword name.
    /// Distinct from `has_keyword` (system-flag bitmask) because user
    /// keywords are arbitrary strings, not bits — RFC 8621 §4.1.4's
    /// `hasKeyword` covers both, so the JMAP layer routes a system
    /// keyword to `has_keyword` and a user keyword here. Matched on the
    /// union of `Tags` across memberships, mirroring the `has_keyword`
    /// union semantics. Name normalisation (case/charset) is the wire
    /// layer's job before this predicate is built.
    pub has_tag: Option<String>,
    /// User-keyword analogue of `not_keyword`: drop items whose
    /// membership-union `Tags` contains this keyword name.
    pub not_tag: Option<String>,
    /// Full-text `text` search (JMAP `Email/query` `filter.text`). Raw
    /// user needle, matched against the `mail_search` FTS5 projection
    /// (folded headers + subject + plain-text body + normalized addrs)
    /// via `Mds::search_items`. Set-wide; `query_emails` intersects the
    /// match set with the candidate items (so it ANDs with `in_mailbox`
    /// and the keyword predicates). A blank needle matches nothing.
    pub text: Option<String>,
}

/// Sort order for `MailStore::query_emails`.
///
/// Phase A only sorts by `received_at`. Other JMAP-specified sort
/// orders surface `unsupportedSort` at the JMAP handler layer
/// (RFC 8621 §4.4.2 + RFC 8620 §5.5). When ties on `received_at`
/// occur, the implementation tie-breaks deterministically on
/// `ItemId` so a paginated client sees stable cursor positions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmailSort {
    #[default]
    ReceivedAtDesc,
    ReceivedAtAsc,
    /// Phase B: sort by the RFC 5256 base subject (prefix-stripped,
    /// whitespace-collapsed, lowercased — see [`subject::normalize_subject`]),
    /// so "Re: Foo" orders next to "Foo".
    SubjectAsc,
    SubjectDesc,
    /// Phase B: sort by the From header's bare address, lowercased.
    FromAsc,
    FromDesc,
}

/// Storage-layer projection of a mailbox row, ready for JMAP
/// `Email/get` / `Email/query` slim-fetch flows.
///
/// **Plan deviation note.** The migration plan's Task 1.5 names this
/// field `blob_id: BlobId`, but the per-account persisted-alias
/// `blob_refs` row that minted JMAP `BlobId`s requires (and is the
/// subject of) Task 1.7's `create_email`. Reads must not write, so
/// the slim handle returns the underlying [`BlobHash`]; the JMAP
/// wire layer mints/looks up the `BlobId` alias at serialize time
/// once Task 1.7 lands. Substituting `BlobHash` here lets Phase 1.5
/// land cleanly without lazy-mint-on-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailHandle {
    pub id: ItemId,
    pub blob_hash: BlobHash,
    pub keywords: Flags,
    /// Per-membership user keywords (IMAP "Tags" / JMAP custom
    /// `keywords` minus the system flags) for this membership row.
    /// Sourced from the same `membership.tags` column the read path
    /// carries `flags` from. The IMAP wire layer renders these in
    /// `FETCH (FLAGS (…))` and matches them in SEARCH; empty for the
    /// common system-flags-only message.
    pub tags: Tags,
    pub received_at: i64,
    /// Per-membership sequence in the listed container. Surfaced so
    /// the JMAP wire layer can stable-sort or build a compound
    /// `state` token from it without re-querying.
    pub seq: Seq,
    /// IMAP UID for this membership — same numerical value as `seq`,
    /// widened to `u64` for the IMAP wire layer (RFC 9051 §2.3.1.1
    /// permits 32-bit UIDs but consumers commonly use 64-bit accumulators
    /// for monotonicity reasoning). Sourced from `membership.seq`; never
    /// recycles within a mailbox lifetime — see P6 / P7 in
    /// `_doc/planned/imap-runway.md`.
    pub uid: u64,
    /// IMAP per-message MODSEQ (RFC 7162 CONDSTORE). Sourced from
    /// `membership.change_seq`; bumped on every flag change to this
    /// specific membership row. NOT the source for per-mailbox
    /// HIGHESTMODSEQ — that's `MailboxRecord::highest_mod_seq`,
    /// derived from `container.change_seq`. Using
    /// `MAX(membership.change_seq)` over surviving rows would be
    /// non-monotonic across EXPUNGE because the row is removed before
    /// the container counter bumps (see
    /// `_doc/planned/imap-runway.md` §P6).
    pub mod_seq: u64,
    /// Size of the on-disk RFC 822 representation in bytes. Sourced
    /// from `ItemMeta::size_bytes` — the same CAS-blob length the
    /// store recorded at ingest. IMAP FETCH RFC822.SIZE (RFC 9051
    /// §6.4.5) surfaces this verbatim; JMAP `Email.size` (RFC 8621
    /// §4.1.1) reuses it. Carrying the size on the slim handle
    /// avoids a per-row `get_email` round-trip from the IMAP wire
    /// layer.
    pub size_bytes: u64,
}

/// MailStore behaviour required by the `cosmix-maild::jmap` adapter.
///
/// All methods are synchronous; tokio callers wrap in
/// `tokio::task::spawn_blocking` per the maild convention. Sync
/// matches `cosmix-mds::Mds` (which is also sync) and lets the
/// adapter chain MDS + sidecar writes inside one rusqlite
/// `Transaction` without crossing await points.
pub trait MailStore: Send + Sync {
    /// Stage an uploaded blob behind a JMAP `BlobId`.
    ///
    /// **Upload-staging path only.** v1.1 amendment §1 distinguishes two
    /// `blob_refs` shapes: upload-staging rows carry `temp_item_id` and
    /// a non-NULL `expires_at`, while persisted-email aliases carry
    /// neither (the underlying blob is pinned by `item.blob_hash` on an
    /// already-stored mail item). This method writes the staging shape;
    /// `expires_at` is required because the temp MDS item it provisions
    /// would otherwise pin CAS data indefinitely.
    ///
    /// Pre-condition: the bytes are already on disk via `Mds::put_blob`
    /// (caller's responsibility — `MailStore` does not re-hash). Effect:
    /// provisions the per-account `__upload_staging__` container if
    /// needed, creates a temp MDS item there carrying `hash`, and
    /// inserts a `blob_refs` row tying the JMAP `BlobId` to the temp
    /// item. All three writes commit in one `with_set_tx` scope.
    ///
    /// Persisted-alias minting (post-`Email/import`, when there's an
    /// owning mail item) is a Phase A surface and will arrive as a
    /// distinct method (`mint_persistent_blob_ref` or similar) when
    /// the JMAP migration onto MDS lands a caller for it.
    fn create_blob_ref(
        &self,
        account: AccountId,
        hash: BlobHash,
        size: u64,
        expires_at: i64,
    ) -> Result<BlobId>;

    /// Look up the underlying `BlobHash` for a JMAP `BlobId`.
    ///
    /// Three-state result, callers must distinguish each:
    ///   - `Ok(BlobRefLookup::Found(hash))` — alias row found AND
    ///     the underlying CAS blob still exists.
    ///   - `Ok(BlobRefLookup::NotFound)` — alias row absent (never
    ///     minted or expired and reaped). The `BlobId` is unknown
    ///     to this account. Callers may fall back to a legacy
    ///     lookup during the Task-3.3a migration window.
    ///   - `Ok(BlobRefLookup::Dangling)` — alias row exists but
    ///     the underlying CAS blob is missing (v1.1 §1 non-owning
    ///     aliases are subject to staging-item GC, core MDS GC, or
    ///     operator surgery). Callers MUST NOT fall back: the
    ///     client received this `BlobId` from us, so absence-of-
    ///     bytes is a real `blobNotFound` rather than "try legacy".
    ///   - `Err(_)` — internal failure (SQLite error, malformed
    ///     `blob_refs.blob_hash`). Propagate as a server error.
    fn resolve_blob_ref(&self, account: AccountId, blob_id: BlobId) -> Result<BlobRefLookup>;

    /// Move an item between containers within an account.
    ///
    /// Performs the MDS move (with v1.1 §3 keyword inheritance) and,
    /// if the move crosses the `\Junk` boundary, inserts a
    /// `mail_retrain_outbox` row keyed on `(stamp_id, label)` so the
    /// retrain worker can run record_label out-of-band. Both writes
    /// commit in one `with_set_tx` scope; retrain failure does NOT
    /// roll back the move (v1.1 §4b).
    ///
    /// Returns `(src_uid, dest_uid)` — the IMAP-shaped (uid,
    /// uid_validity) pairs for the source row (the seq it held just
    /// before removal) and the destination row (the freshly-allocated
    /// seq in `dest`). Both pairs are read inside the same
    /// `with_set_tx` that mutated the rows, so an IMAP MOVE / UID MOVE
    /// can emit `OK [COPYUID <uidvalidity> <src-set> <dest-set>]`
    /// (RFC 4315) without a follow-up read. JMAP callers that don't
    /// care about UIDPLUS can discard the tuple.
    fn move_message(
        &self,
        account: AccountId,
        item_id: ItemId,
        src: ContainerId,
        dest: ContainerId,
        flags: Flags,
        stamp_id: &str,
    ) -> Result<(MembershipUid, MembershipUid)>;

    /// IMAP COPY / UID COPY primitive (RFC 9051 §6.4.7 / RFC 4315
    /// §3 `COPYUID`). Add a new membership of `item_id` in `dest`
    /// **without** removing the existing membership in `src`. Mirrors
    /// [`Self::move_message`] in atomicity and junk-boundary retrain
    /// semantics — COPY into / out of the per-account `\Junk` container
    /// crosses the spam/ham boundary the same way MOVE does, so the
    /// retrain row is enqueued in the same `with_set_tx` scope.
    ///
    /// **Flag inheritance.** The new dest membership inherits the
    /// system flags + user tags from the `src` membership specifically
    /// (not from "any sibling"). RFC 9051 §6.4.7 requires the
    /// destination to preserve the source mailbox's flags, and the
    /// IMAP §3.2 per-mailbox flag model lets sibling memberships
    /// diverge — so source-specific inheritance is load-bearing.
    /// **An absent `(item_id, src)` membership is a hard error**, not
    /// a silent empty-flags fallback: the mailstore maps it to the
    /// same `email not found` surface a concurrent EXPUNGE would
    /// produce, so a snapshot/write race that drops `src`'s row
    /// between the IMAP snapshot and this call fails the COPY rather
    /// than minting a dest row with the wrong flags. Internaldate
    /// is preserved because the underlying item row is unchanged.
    ///
    /// Returns the dest [`MembershipUid`] for the
    /// `OK [COPYUID <uidvalidity> <src-set> <dest-set>]` response.
    /// The source UID is the caller's responsibility — the IMAP layer
    /// already resolved it from the SELECTED-state snapshot before
    /// calling this primitive. `uid_validity` on the dest is read in
    /// the same tx so a freshly-allocated seq can be paired with a
    /// snapshot-consistent validity for UIDPLUS.
    ///
    /// `stamp_id` is the `maild.verdict.stamp_id` (the per-account
    /// public Email id) used as the retrain-outbox row key, matching
    /// [`Self::move_message`]. The retrain outbox PK is
    /// `(stamp_id, label)`, so a non-unique stamp (e.g. a blank
    /// placeholder shared across items) silently drops all but the
    /// first row's retrain. IMAP COPY callers pass the
    /// `item_id.0.to_string()` since the substrate ItemId IS the
    /// per-delivery stamp on inbound mail (see
    /// `src/smtp/inbound.rs:386` — `email_uuid = item_id.0`).
    ///
    /// `read_only_src` SHOULD be `true` when the IMAP source mailbox
    /// is in EXAMINE mode (RFC 9051 §6.4.7 — COPY out of a read-only
    /// source is permitted, but side-channel mutations like the
    /// retrain-outbox enqueue would let an EXAMINE'd session train
    /// the classifier without the user actually moving mail). When
    /// `true`, the junk-boundary retrain row is suppressed; the COPY
    /// itself still proceeds because the substrate-level "side
    /// effect" is on the dest mailbox, not the source.
    ///
    /// Errors carry a message starting with one of:
    ///   * `"email not found: <id>"` — `item_id` is not in the
    ///     account's set, or the account is unprovisioned.
    ///   * `"mailbox not found: <id>"` — `dest` does not exist.
    ///   * `"item <id> already in container <cid>"` — the email is
    ///     already a member of `dest`. RFC 9051 §6.4.7 leaves the
    ///     behaviour implementation-defined; we refuse rather than
    ///     no-op so the IMAP layer can return `NO`.
    fn copy_message(
        &self,
        account: AccountId,
        item_id: ItemId,
        src: ContainerId,
        dest: ContainerId,
        stamp_id: &str,
        read_only_src: bool,
    ) -> Result<MembershipUid>;

    /// IMAP EXPUNGE / UID EXPUNGE primitive (RFC 9051 §6.4.3 /
    /// RFC 4315 §2.1). Atomically remove every supplied
    /// `(item_id, mailbox)` membership in **one** `with_set_tx`
    /// scope; returns the count of memberships actually removed.
    ///
    /// The caller (IMAP EXPUNGE handler) is responsible for:
    ///   * snapshotting the mailbox via [`Self::list_emails_in_mailbox`]
    ///     so it can compute MSNs for the `* N EXPUNGE` responses
    ///     (descending order per RFC 9051 §7.4.1 — the substrate is
    ///     order-agnostic);
    ///   * filtering by `\Deleted` (and, for UID EXPUNGE, by the
    ///     UID-set);
    ///   * passing the filtered `item_ids` here in any order.
    ///
    /// Snapshot-vs-removal TOCTOU is benign per RFC 9051 §7.4.1:
    /// after the caller selects `\Deleted` items, a concurrent
    /// `STORE -FLAGS \Deleted` between snapshot and call removes
    /// the flag but does not undo our removal — the expunge still
    /// fires and the EXPUNGE response on the wire is well-formed.
    ///
    /// Membership rows that no longer exist (concurrent EXPUNGE
    /// in another session, e.g. two IMAP clients each running
    /// EXPUNGE) are skipped silently and do not count toward the
    /// returned `removed` total. The IMAP layer uses the returned
    /// count only for telemetry — wire emission is driven by the
    /// caller's snapshot.
    ///
    /// **Precondition.** Empty `item_ids` short-circuits to `Ok(0)`
    /// *without* opening a tx — so `mailbox`'s existence is NOT
    /// validated in that case. Callers that need a "does this
    /// mailbox exist" probe must do so via [`Self::list_mailboxes`]
    /// or [`Self::list_emails_in_mailbox`] first. This shape mirrors
    /// the `add_memberships` empty-input short-circuit and is
    /// non-negotiable for the change-stream contract (empty fan-out
    /// must not allocate a `set_change` row).
    ///
    /// **Cross-DB blob cascade.** `remove_membership_in_tx` drops
    /// the cross-DB `blob_ref` (refcount -= 1) and deletes the
    /// `item` row when the last membership of an item goes; the
    /// orphan blob then becomes a GC candidate. This means an
    /// `EXPUNGE` on the only mailbox an item lives in eventually
    /// frees the bytes; multi-mailbox items are merely unfastened
    /// from `mailbox` and remain readable elsewhere.
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    fn expunge_memberships(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        item_ids: &[ItemId],
    ) -> Result<usize>;

    /// List all JMAP-visible mailboxes (containers) for an account.
    ///
    /// Reads only — no `with_set_tx` scope is needed. The adapter
    /// reads the container list and joins each row with
    /// `container_status` for `total_emails` / `unread_emails`. Each
    /// underlying MDS call takes its own short transaction; the
    /// resulting view is **eventually consistent**, not snapshot
    /// consistent. A concurrent `Mailbox/set destroy` between the
    /// list call and a per-row status call shows up as a row that
    /// has been removed from the response (the adapter swallows
    /// `ContainerNotFound` mid-iteration); a concurrent message
    /// store/delete shows up as a count one tick stale relative to
    /// its row's metadata. Both are acceptable for `Mailbox/get`,
    /// which the JMAP client refreshes via `Mailbox/changes`.
    ///
    /// Filters out the per-account `__upload_staging__` container
    /// (which `create_blob_ref` provisions as a non-mailbox temp
    /// landing zone for blob uploads) so JMAP `Mailbox/get` and
    /// `Mailbox/query` never expose it. The filter is by name today;
    /// `MailStore::create_mailbox` (Task 1.2) is responsible for
    /// rejecting that reserved name at the create boundary so a
    /// real mailbox can never collide with the staging container.
    ///
    /// Returns an empty vector if the account's set has not yet been
    /// provisioned via `ensure_account_set`. Account existence and
    /// authentication are caller preconditions — this method does
    /// not distinguish "valid empty account" from "bogus account id."
    fn list_mailboxes(&self, account: AccountId) -> Result<Vec<MailboxRecord>>;

    /// Return the first mailbox in `account` whose JMAP `role` matches
    /// `role`, or `None` if none does.
    ///
    /// **Match semantics.** RFC 8621 §2 forbids two mailboxes in the
    /// same account from sharing a role, so "first match" is the
    /// observed-unique match in any compliant set. If a non-compliant
    /// migration leaves duplicate roles (legacy `db::mailbox` did not
    /// enforce uniqueness), the iteration order of `list_mailboxes`
    /// determines the winner — there is no preferred tiebreaker here.
    ///
    /// **Default implementation** filters `list_mailboxes`. SQLite-
    /// backed implementations may override with a direct
    /// `WHERE special_use = ?` query if the linear scan ever shows up
    /// hot; the contract is identical either way.
    ///
    /// Errors propagate from `list_mailboxes`: an unprovisioned account
    /// returns `Ok(None)`, not `"mailbox not found"`.
    fn mailbox_by_role(
        &self,
        account: AccountId,
        role: MailboxRole,
    ) -> Result<Option<ContainerId>> {
        Ok(self
            .list_mailboxes(account)?
            .into_iter()
            .find(|m| m.role == Some(role))
            .map(|m| m.id))
    }

    /// Return the first mailbox in `account` whose name matches `name`
    /// (case-sensitive), or `None` if none does.
    ///
    /// Used by SMTP inbound filter routing: a Mix script returns a
    /// mailbox name and the deliver path resolves it to a `ContainerId`
    /// before calling `create_email`. Hierarchy collisions (`Foo` and
    /// `Sub/Foo`) are not resolved here — callers wanting nested-name
    /// disambiguation walk `list_mailboxes` themselves with the parent
    /// chain. RFC 8621 §2 makes per-account top-level names unique by
    /// construction, so this is sufficient for the SMTP filter use case.
    ///
    /// **Default implementation** filters `list_mailboxes`. Errors
    /// propagate from `list_mailboxes`; an unprovisioned account
    /// returns `Ok(None)`.
    fn mailbox_by_name(&self, account: AccountId, name: &str) -> Result<Option<ContainerId>> {
        Ok(self
            .list_mailboxes(account)?
            .into_iter()
            .find(|m| m.name == name)
            .map(|m| m.id))
    }

    /// Create a new JMAP-visible mailbox (container) for an account.
    ///
    /// Bootstraps the per-account set on first call. `parent` must
    /// resolve to an existing container in the same set when `Some`;
    /// `None` creates a root-level mailbox.
    ///
    /// **Reserved-name rule.** The name `__upload_staging__` is
    /// reserved for `create_blob_ref`'s per-account upload-staging
    /// container (see [`UPLOAD_STAGING_NAME`]). Attempting to create
    /// a mailbox with that name fails — a real mailbox must never
    /// collide with the staging container, otherwise it would be
    /// silently filtered out of `list_mailboxes`. This closes the
    /// other half of the staging-filter contract documented on
    /// `list_mailboxes`.
    ///
    /// Errors carry a message starting with one of:
    /// `"reserved name"`, `"mailbox invalid name"`,
    /// `"mailbox parent not found"`, `"mailbox already exists"` so the
    /// JMAP `Mailbox/set create` handler can map them to the correct
    /// JMAP error types. Caller-side input validation (length, control
    /// characters, IMAP-naming rules) belongs in the JMAP/IMAP layer;
    /// this trait only enforces the storage-level invariants.
    fn create_mailbox(
        &self,
        account: AccountId,
        name: &str,
        parent: Option<ContainerId>,
        attrs: ContainerAttrs,
    ) -> Result<ContainerId>;

    /// Rename and/or reparent an existing mailbox.
    ///
    /// Atomic via `with_set_tx`. Inherits `create_mailbox`'s
    /// reserved-name and empty-name guards: renaming a mailbox to
    /// `__upload_staging__` is rejected so a legitimate user mailbox
    /// cannot be re-shaped into the staging container's slot, and
    /// renaming to `""` is rejected as an invalid name. Cycle and
    /// self-parent guards live in MDS (`rename_container_in_tx`).
    ///
    /// Errors carry a message starting with one of:
    /// `"reserved name"`, `"mailbox invalid name"`,
    /// `"mailbox not found"`, `"mailbox parent not found"`,
    /// `"mailbox cycle"`, `"mailbox already exists"` — same prefix
    /// vocabulary as `create_mailbox` plus `"mailbox cycle"` for the
    /// reparent-into-descendant case (and self-parent collapsed onto
    /// the same prefix because both are "this rename would break the
    /// tree").
    fn rename_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        new_name: &str,
        new_parent: Option<ContainerId>,
    ) -> Result<()>;

    /// Destroy a mailbox, choosing what to do with any items that
    /// still have membership in it via [`DestroyPolicy`].
    ///
    /// Atomic via `with_set_tx`: the per-item membership cleanup
    /// (move-to-inbox or remove-membership) and the final container
    /// delete commit as one transaction; on any failure the whole
    /// scope rolls back. Like `rename_mailbox`, this method does NOT
    /// bootstrap the per-account set — surfacing `SetNotFound` as
    /// `"mailbox not found"` rather than creating empty MDS state on
    /// an error path.
    ///
    /// Refuses to destroy the per-account `__upload_staging__`
    /// container by id (defensive: JMAP listings filter it, so a
    /// client should never legitimately hold its id).
    ///
    /// Refuses to destroy a mailbox that still has child mailboxes
    /// — `delete_container_in_tx`'s `ON DELETE RESTRICT` surfaces as
    /// `"mailbox has children"`. Clients destroying a subtree must
    /// recurse from the leaves up.
    ///
    /// Refuses to destroy a mailbox carrying any non-null
    /// `attrs.special_use` (the storage source for JMAP `role`,
    /// e.g. `\Inbox` / `\Sent` / `\Trash`) per RFC 8621 §2.5,
    /// surfaced as `"mailbox has role"`. The guard fires on the raw
    /// stored value, so unrecognized special-use strings — which
    /// `MailboxRole::from_special_use` would project as `role: null`
    /// on the wire — still trip it; the storage barrier is the
    /// authoritative one. The guard runs ahead of the policy branch
    /// so it applies under `Reject`, `Delete`, and `MoveToInbox`
    /// alike — `MoveToInbox` itself looks up `\Inbox` by
    /// `attrs.special_use`, so silently destroying it would corrupt
    /// subsequent destroys. Clients must clear `special_use` via
    /// `Mailbox/set update` first.
    ///
    /// Errors carry a message starting with one of:
    /// `"mailbox not found"`, `"mailbox not empty"`,
    /// `"mailbox has children"`, `"mailbox has no inbox"`,
    /// `"mailbox has role"`, `"mailbox conflicting policy"`,
    /// `"reserved container"`. The first two cover the JMAP-visible
    /// cases (`notFound`, `mailboxHasEmail`); `"mailbox has role"`
    /// maps to `forbidden`; the rest cover operator-visible
    /// preconditions.
    fn destroy_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        on_emails: DestroyPolicy,
    ) -> Result<()>;

    /// Apply a JMAP `Mailbox/set update` patch atomically: rename /
    /// reparent and/or update the JMAP-visible attrs (`role`,
    /// `sortOrder`, `isSubscribed`) in **one** `with_set_tx`. Each
    /// argument follows the JMAP patch-object semantic: `None` =
    /// "field not present in the patch, leave it alone", `Some(value)` =
    /// "set to this new value". `new_parent: Some(None)` moves the
    /// mailbox to the root. `role: Some(None)` clears the role (per
    /// RFC 8621 §2.5 "the value … may be null").
    ///
    /// Why a single combined method (vs separate `rename_mailbox` +
    /// `update_attrs` calls): two `with_set_tx` round-trips release
    /// the per-set mutex between them, so a concurrent
    /// `Mailbox/set destroy` against the same mailbox can land in the
    /// gap — the rename commits to disk but the attrs update sees
    /// `notFound`, leaving the on-disk state changed but the JMAP
    /// response reporting `notUpdated`. Bundling both into one
    /// `with_set_tx` closure makes the operation indivisible. (Codex
    /// review of Task 2.4 surfaced this; see `_doc/planned/jmap-mds-
    /// migration.md` Task 2.4 commit.)
    ///
    /// Like `rename_mailbox` / `destroy_mailbox` this method does NOT
    /// bootstrap the per-account set on an error path — `SetNotFound`
    /// surfaces as `"mailbox not found"`. A bogus `id` against an
    /// existing set surfaces as `"mailbox not found"` too.
    ///
    /// Role uniqueness: the JMAP layer is responsible for the
    /// per-account uniqueness check (RFC 8621 §2.5) before calling
    /// this method, mirroring `create_mailbox`. The storage layer has
    /// no `(account, role)` unique index — adding one would require a
    /// schema migration.
    ///
    /// All-`None` is a no-op (no tx is opened).
    ///
    /// Errors carry a message starting with one of (union of
    /// `rename_mailbox` and the attrs-update vocabulary):
    /// `"reserved name"`, `"mailbox invalid name"`,
    /// `"mailbox not found"`, `"mailbox parent not found"`,
    /// `"mailbox cycle"`, `"mailbox already exists"`. Other failures
    /// (decode/encode the attrs JSON, raw SQL) bubble up as anyhow
    /// errors.
    #[allow(clippy::too_many_arguments)] // JMAP patch surface: each arg maps to one optional field
    fn update_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        new_name: Option<&str>,
        new_parent: Option<Option<ContainerId>>,
        role: Option<Option<MailboxRole>>,
        sort_order: Option<u32>,
        is_subscribed: Option<bool>,
    ) -> Result<()>;

    /// List the emails in a mailbox as slim [`EmailHandle`] rows.
    ///
    /// Read-only; no `with_set_tx` scope. The default
    /// [`ListOpts::sort_by`] is [`SortKey::ReceivedAt`]; pagination
    /// (`limit` / `offset`) and sorting both happen in-memory after
    /// the underlying [`Mds::list_items`] returns the full container
    /// listing. This is fine for the current scale (single-user
    /// accounts, tens of thousands of emails per mailbox at the high
    /// end); a future Phase A optimization can push limit/offset into
    /// the SQL once `Email/query` filters land and the read path
    /// matters.
    ///
    /// Like `rename_mailbox` and `destroy_mailbox`, this method does
    /// NOT bootstrap the per-account set — `SetNotFound` (account
    /// has no MDS state at all) and `ContainerNotFound` (set exists
    /// but the container does not) both surface as
    /// `"mailbox not found"`. The existence check runs unconditionally
    /// (including for `limit == 0`), so a JMAP "count probe" over a
    /// missing mailbox sees `mailbox not found` rather than a
    /// successful-looking empty result. Returning empty for
    /// missing-mailbox would conflate "no emails" with "no such
    /// mailbox", which the JMAP `Email/query` `inMailbox` filter
    /// needs to distinguish.
    ///
    /// **Not snapshot-consistent.** A concurrent `destroy_mailbox`
    /// between the existence check and the per-row read will produce
    /// `Ok(vec![])` rather than an error (the destroy's cascade fires
    /// in between). Mirrors `list_mailboxes`'s eventual-consistency
    /// guarantee — callers needing stronger semantics serialize at a
    /// higher layer.
    ///
    /// `limit == 0` returns an empty `Vec` (caller asked for no
    /// rows) once the existence check has passed. `offset` past the
    /// end of the result also returns empty. See [`ListOpts`] for
    /// the full surface.
    fn list_emails_in_mailbox(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        opts: ListOpts,
    ) -> Result<Vec<EmailHandle>>;

    /// Query email item ids across mailboxes — JMAP `Email/query`
    /// storage surface (RFC 8621 §4.4) — Phase A.
    ///
    /// Read-only; no `with_set_tx` scope. **Eventually consistent**
    /// in the same sense as `list_emails_in_mailbox` and
    /// `list_mailboxes`: the candidate-gather + per-item membership
    /// reads are issued as separate short transactions, so a
    /// concurrent move/destroy mid-query may produce a slightly stale
    /// answer. JMAP clients refresh through `Email/changes`.
    ///
    /// Implementation:
    ///   1. Determine candidate containers — one container if
    ///      `filter.in_mailbox` is set (with an existence check so
    ///      `inMailbox: <missing>` surfaces `mailbox not found`),
    ///      otherwise every container in the account's set excluding
    ///      `__upload_staging__`.
    ///   2. Walk `Mds::list_items` per candidate container, deduping
    ///      by `ItemId` so an email appearing in N mailboxes is
    ///      counted once. `received_at` and `size_bytes` come from
    ///      the row directly.
    ///   3. For predicates that need cross-membership state
    ///      (`in_mailbox_other_than`, `has_keyword`, `not_keyword`),
    ///      issue one `item_memberships` call per surviving candidate
    ///      to compute the union flag-set and the membership set.
    ///      Items destroyed between the gather and the membership
    ///      read are dropped silently — they cannot satisfy any
    ///      predicate from a JMAP client's perspective.
    ///   4. Apply `EmailSort` and `opts.offset` / `opts.limit`. The
    ///      sort is total-ordered by tie-breaking on `ItemId` so
    ///      cursor pagination is stable. **`opts.sort_by` is ignored**
    ///      — `EmailSort` is the source of truth.
    ///
    /// Phase A scope cap: envelope filters (`from`/`to`/`cc`/
    /// `subject`) are not in `EmailQueryFilter` yet; they land with
    /// `Email/query` in Task 3.6 once a `mail_envelopes` join (or FTS
    /// projection) replaces this in-memory walk for the columns it
    /// can serve. This method is the prelim that lets Task 3.6 ship
    /// without re-shaping the storage surface.
    ///
    /// Errors carry a message starting with one of:
    ///   * `"mailbox not found"` — `filter.in_mailbox` references a
    ///     container that does not exist in the account's set, or the
    ///     account's set itself is unprovisioned **and** `in_mailbox`
    ///     was set. When `in_mailbox` is `None` and the set is
    ///     unprovisioned, returns `Ok(vec![])` (matches `list_mailboxes`).
    fn query_emails(
        &self,
        account: AccountId,
        filter: EmailQueryFilter,
        sort: EmailSort,
        opts: ListOpts,
    ) -> Result<Vec<ItemId>>;

    /// Read one email by item id, returning the JMAP `Email/get` row.
    ///
    /// Read-only; no `with_set_tx` scope. Reads the underlying
    /// `item` row (via `Mds::fetch_item_meta`), the per-item envelope
    /// sidecar (`mail_envelopes` from cosmix-mds v1.2 schema), and the
    /// item's memberships (via `Mds::item_memberships`) to assemble the
    /// merged JMAP shape — see [`EmailRecord`] for the per-field
    /// semantics and the rationale for *not* embedding the
    /// per-membership [`EmailHandle`].
    ///
    /// Like the other read-only `MailStore` methods, this does NOT
    /// bootstrap the per-account set on `SetNotFound` — surfacing
    /// `"email not found"` rather than creating empty MDS state on
    /// an error path.
    ///
    /// Errors carry a message starting with one of:
    ///   * `"email not found"` — the item doesn't exist (or its set
    ///     hasn't been provisioned yet). Maps to JMAP `notFound`.
    ///   * `"email envelope missing"` — the item exists but no
    ///     `mail_envelopes` sidecar row was ever written. Phase A
    ///     transient: items predating Task 1.7's `create_email` write
    ///     path will hit this; the JMAP layer maps it to a synthetic
    ///     "skeleton" envelope at serialise time once that path lands.
    fn get_email(&self, account: AccountId, id: ItemId) -> Result<EmailRecord>;

    /// Return every email record in the account whose stored blob
    /// hash equals `blob_hash`. Order is unspecified.
    ///
    /// Backs the JMAP `Email/import` idempotency contract (RFC 8621
    /// §4.6 SHOULD, promoted to MUST in
    /// `_doc/planned/jmap-mds-migration.md` §3.5b): the JMAP layer
    /// uses this primitive to decide whether to reuse an existing
    /// email id rather than allocate a new one when the same blob is
    /// re-imported into the same mailbox set. `find` rather than
    /// `get`-singular because two distinct imports may legitimately
    /// share a blob across different mailbox sets, and the caller
    /// performs membership-equality filtering.
    ///
    /// Returns `Ok(vec![])` for an unprovisioned account
    /// (`SetNotFound`) — same observable shape as "this account has
    /// never seen this blob."
    fn find_emails_by_blob_hash(
        &self,
        account: AccountId,
        blob_hash: &cosmix_mds::BlobHash,
    ) -> Result<Vec<EmailRecord>>;

    /// Read the raw RFC 5322 blob bytes for a known CAS hash.
    ///
    /// The CAS layer is account-agnostic by design (one byte sequence,
    /// one hash, regardless of which account ingested it). Callers MUST
    /// have obtained the hash from an account-scoped query
    /// ([`EmailHandle::blob_hash`] returned by
    /// [`list_emails_in_mailbox`], [`EmailRecord::blob_hash`] returned
    /// by [`get_email`], etc.) before invoking this method — passing a
    /// hash sourced from outside the substrate trusts the caller for
    /// authorization.
    ///
    /// Used by the IMAP wire layer for FETCH atoms that need the
    /// message body (`ENVELOPE`, `BODYSTRUCTURE`, `BODY[...]`,
    /// `RFC822*`). Returns the bytes verbatim — the same bytes
    /// originally passed to [`cosmix_mds::Mds::put_blob`], suitable
    /// for parsing via `mail_parser::MessageParser` without further
    /// transformation.
    fn read_blob(&self, hash: cosmix_mds::BlobHash) -> Result<Vec<u8>>;

    /// Land a raw RFC 5322 message in CAS and return its content hash.
    ///
    /// Trait-level forwarder for the IMAP APPEND path (Phase 4 T14),
    /// which holds an `Arc<dyn MailStore>` and cannot reach the
    /// concrete `mds()` accessor on `SqliteMailStore`. The default
    /// implementation is a thin wrapper around `Mds::put_blob`; impls
    /// override only if they need a different CAS surface. CAS is
    /// account-agnostic, so no `account` is required; the same bytes
    /// hash to the same handle regardless of caller.
    ///
    /// Used by APPEND to land the literal body before `create_email`
    /// can bind it to a fresh item row.
    ///
    /// **Orphan-blob tradeoff.** A successful `put_blob` materialises
    /// bytes in CAS *before* any `item`/`mailbox_membership` row
    /// references them. If the caller then fails between `put_blob`
    /// and a row-binding `create_email` (panic, error, peer disconnect),
    /// the bytes remain in CAS with refcount 0 and no row pointing at
    /// them — they are an orphan file, not an orphan row. The substrate
    /// GC pass walks `blob` rows with `refcount = 0`; orphan *files*
    /// without a row are reaped only by the rebuild path. SMTP inbound
    /// has the same shape, so APPEND follows the established pattern
    /// rather than introducing a staging mechanism. Operators may need
    /// a periodic CAS scrub if APPEND failure rates make this material.
    fn put_blob(&self, blob: &[u8]) -> Result<cosmix_mds::BlobHash>;

    /// Create an email item in `mailbox` with one initial membership.
    ///
    /// `blob` must already be present in CAS via `Mds::put_blob`; the
    /// size is read from CAS rather than taken as a parameter so the
    /// "unknown blob → error" contract is enforced unconditionally and
    /// the item row's `size_bytes` cannot drift from the actual bytes.
    /// `received_at` is the server-received timestamp (IMAP
    /// `INTERNALDATE` / JMAP `receivedAt`) in **milliseconds since the
    /// Unix epoch** — matching the underlying `item.received_at`
    /// column written by `cosmix_mds::container::add_staging_item_in_tx`
    /// via `now_ms()`. It can differ from `envelope.date`, which is the
    /// message's `Date:` header in unix **seconds** (RFC 5322 origination
    /// time, see `EmailEnvelope::date`). All writes — `item`, cross-DB
    /// `blob_refs` (refcount += 1), the initial `membership`, and the
    /// `mail_envelopes` sidecar row, plus the `mail_threads` membership
    /// row — commit inside one `with_set_tx` scope; the Bus
    /// `mds.item.added` event is buffered until commit.
    ///
    /// **Threading.** `in_reply_to` carries the parsed `In-Reply-To:` /
    /// `References:` Message-IDs (bare addr-spec strings, no angle
    /// brackets) extracted by the caller from the message headers. The
    /// implementation looks each reference up against
    /// `mail_envelopes.message_id` for items already in the account; a
    /// match reuses that item's `mail_threads.thread_id`. `in_reply_to`
    /// order is honoured (first match wins), and references that
    /// resolve to no known item are silently skipped. When no
    /// reference matches (or the slice is empty), the resolver falls
    /// back to RFC 5256 §2.1 base-subject equality (Phase 8e gate 4
    /// / JMAP §4.1.5) against `mail_envelopes.normalized_subject`
    /// scoped to the same account — see
    /// `find_or_new_thread_id_in_tx` for the precise lookup. Only when
    /// both passes miss is a fresh UUIDv4 thread id allocated. Empty
    /// normalized subjects deliberately skip the fallback so that
    /// blank-subject emails don't cluster.
    ///
    /// **Bus event surface — plan deviation.** The migration plan
    /// names a higher-level `mail.created` event; the implementation
    /// emits the existing `mds.item.added` Bus event only. A separate
    /// `mail.created` surface remains future work for the JMAP
    /// `Email/changes` migration (Phase 3); shipping it now would
    /// duplicate the mds-layer notification without a consumer.
    ///
    /// Returns `(ItemId, MembershipUid)` — the freshly minted item id
    /// plus the IMAP-shaped (uid, uid_validity) pair for the initial
    /// membership in `mailbox`. The uid pair is read inside the same
    /// `with_set_tx` that produced the membership row, so an IMAP
    /// APPEND can emit `OK [APPENDUID <uidvalidity> <uid>]` (RFC 4315)
    /// without a follow-up read. JMAP callers that don't care about
    /// UIDPLUS can discard the second tuple element.
    ///
    /// Errors carry a message starting with one of:
    ///   * `"blob not found"` — `blob` is not present in CAS. Maps to
    ///     JMAP `blobNotFound`.
    ///   * `"mailbox not found"` — the account's set is unprovisioned
    ///     or the container does not exist in the set.
    #[allow(clippy::too_many_arguments)]
    fn create_email(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        blob: BlobHash,
        envelope: EmailEnvelope,
        in_reply_to: &[String],
        initial_keywords: Flags,
        initial_tags: Tags,
        received_at: i64,
    ) -> Result<(ItemId, MembershipUid)>;

    /// Create an email item with N≥1 initial memberships in one
    /// `with_set_tx` scope. Multi-mailbox counterpart of
    /// [`MailStore::create_email`], used by JMAP `Email/set create`
    /// when `mailboxIds` carries more than one entry, and by JMAP
    /// `Email/import` (which always supplies an explicit list per
    /// RFC 8621 §4.5).
    ///
    /// `mailboxes` must be non-empty and contain unique container ids;
    /// duplicates surface as a typed error per the underlying mds
    /// `add_item_with_memberships_in_tx` prevalidation contract. Per
    /// the v1.1 §3 "all memberships agree on (flags, tags)" invariant,
    /// `initial_keywords` applies uniformly to every membership; JMAP
    /// `Email/set create` carries a single top-level `keywords` object
    /// so this matches the wire shape.
    ///
    /// `blob`, `envelope`, `initial_keywords`, and `received_at` carry
    /// the same semantics as [`MailStore::create_email`] — see that
    /// method for the unit conventions (received_at = unix-ms,
    /// envelope.date = unix-s) and the Bus event surface notes. The
    /// Bus event shape is the **aggregate** `mds.item.added` with
    /// parallel `container_ids[]` / `change_seqs[]` arrays (mirrors
    /// public `Mds::add_item`'s shape; one event covers all dests
    /// rather than one event per dest).
    ///
    /// Returns `(ItemId, Vec<MembershipUid>)`: the freshly minted item
    /// id plus, for each mailbox in `mailboxes` (in input order), the
    /// IMAP-shaped (uid, uid_validity) pair needed by a future IMAP
    /// `APPEND OK [APPENDUID …]` (RFC 4315) and by JMAP layers that
    /// want to persist a stable per-membership identity at create
    /// time without a follow-up read.
    ///
    /// Errors carry a message starting with one of:
    ///   * `"blob not found"` — `blob` is not present in CAS. Maps to
    ///     JMAP `blobNotFound`.
    ///   * `"mailbox not found: <id>"` — the named mailbox does not
    ///     exist in the account's set, or the account's set itself is
    ///     unprovisioned. Prevalidated **before** any `item` /
    ///     `blob_ref` / membership write, so an Err on this path leaves
    ///     no rows even if a caller catches it.
    ///   * `"mailboxes must be non-empty"` — `mailboxes.is_empty()`.
    ///     Prevalidated as above.
    ///   * `"duplicate mailbox in mailboxes slice: <id>"` — the same
    ///     container appeared twice. Prevalidated as above.
    #[allow(clippy::too_many_arguments)]
    fn create_email_with_memberships(
        &self,
        account: AccountId,
        mailboxes: &[ContainerId],
        blob: BlobHash,
        envelope: EmailEnvelope,
        in_reply_to: &[String],
        initial_keywords: Flags,
        initial_tags: Tags,
        received_at: i64,
    ) -> Result<(ItemId, Vec<MembershipUid>)>;

    /// Add N≥1 *post-create* memberships to an existing email,
    /// atomically. Used by JMAP `Email/set update` (Task 3.4) when
    /// `mailboxIds` adds new entries the email did not previously
    /// belong to.
    ///
    /// Each new membership inherits `(flags, tags)` from the email's
    /// existing memberships per the v1.1 §3 invariant, regardless of
    /// caller intent — JMAP `Email/set update` cannot set per-mailbox
    /// flags out of band (RFC 8621 §4.4.6). The Bus event shape is
    /// `mds.item.copied` per dest (not aggregate `item.added`),
    /// matching the user-visible "place an existing email in a new
    /// mailbox" semantic.
    ///
    /// `mailboxes` must be non-empty, contain unique container ids,
    /// and contain only mailboxes the email is **not** already in. A
    /// duplicate or already-existing membership surfaces as a typed
    /// error rather than being silently no-op'd, so the JMAP layer
    /// can distinguish "already in that mailbox" from "successfully
    /// added".
    ///
    /// Returns one `MembershipUid` per added mailbox (in input order)
    /// for the same UIDPLUS / JMAP membership-identity reasons as
    /// [`Self::create_email_with_memberships`].
    ///
    /// Errors carry a message starting with one of:
    ///   * `"email not found: <id>"` — `id` does not exist in the
    ///     account's set, or the set is unprovisioned.
    ///   * `"mailbox not found: <id>"` — a named mailbox does not
    ///     exist in the account's set.
    ///   * `"mailboxes must be non-empty"`.
    ///   * `"item <id> already in container <cid>"` — the email is
    ///     already a member of one of the named mailboxes. The first
    ///     such collision short-circuits; the tx rolls back so no
    ///     partial fan-out lands.
    fn add_memberships(
        &self,
        account: AccountId,
        id: ItemId,
        mailboxes: &[ContainerId],
    ) -> Result<Vec<MembershipUid>>;

    /// Apply `keywords` (system flags) to the email across **every**
    /// mailbox it currently belongs to, atomically. Mirrors JMAP
    /// `Email/set keywords` semantics: the keywords property is the
    /// per-item union, so a write must touch every membership in one
    /// transaction or none at all (RFC 8621 §4.4.6 / spec §3 line 335).
    ///
    /// `keywords` is the system flag set (`\Seen`, `\Flagged`,
    /// `\Draft`, `\Answered`); `tags` is the user-keyword set (JMAP
    /// custom `keywords` minus the system flags). Both are applied as
    /// the new whole-item value — a JMAP `keywords` object is the
    /// complete set, not a delta, so this **replaces** the keywords on
    /// every membership (the wire layer computes the merged set before
    /// calling). Routed to `Mds::store_item_keywords` (flags + tags in
    /// one atomic fan-out).
    ///
    /// Errors:
    ///   * `"email not found"` — `id` does not exist (or has zero
    ///     memberships, which the JMAP layer treats the same — there
    ///     is no observable email for the client to address).
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    ///
    /// **Bus event surface — plan deviation.** The migration plan
    /// names a higher-level `mail.keywords_changed` event; the
    /// implementation re-uses the existing `mds.item.flagged` Bus
    /// event (one per touched membership). A dedicated
    /// `mail.keywords_changed` surface remains future work for the
    /// JMAP `Email/changes` migration in Phase 3.
    fn set_keywords(
        &self,
        account: AccountId,
        id: ItemId,
        keywords: Flags,
        tags: Tags,
    ) -> Result<()>;

    /// IMAP STORE per-mailbox flags. Sibling of [`MailStore::set_keywords`]
    /// (JMAP shape, fans out across every membership of `id`) — this
    /// method mutates exactly **one** `(item, mailbox)` membership row
    /// and leaves any other memberships of `id` alone. Required for
    /// the IMAP non-negotiable that `\Seen` set on the INBOX
    /// membership of a multi-mailbox email must NOT propagate to the
    /// Archive membership (`_doc/maild/imap.md` §Non-negotiable
    /// invariants 5/6).
    ///
    /// `keywords` is the new system flag set (`\Seen`, `\Flagged`,
    /// `\Draft`, `\Answered`) and `tags` the new user-keyword set for
    /// **this membership only**. Both **replace** the prior values on
    /// the `(item, mailbox)` row (the IMAP wire layer computes the
    /// post-`+FLAGS`/`-FLAGS`/`FLAGS` keyword set before calling).
    /// Routed to `Mds::store_membership_keywords`, the per-mailbox
    /// flags + tags writer — see its doc-comment for the
    /// `ContainerNotFound`-vs-membership-missing disambiguation this
    /// method's error mapping relies on.
    ///
    /// **Bus event surface.** Re-uses the existing `mds.item.flagged`
    /// Bus event (one emission, scoped to the touched membership).
    /// The IMAP IDLE notifier sees a single `FlagsChanged` event on
    /// the corresponding `(account, mailbox)` channel; subscribers
    /// to sibling mailboxes are NOT woken.
    ///
    /// Errors:
    ///   * `"email not found"` — `id` has no membership in `mailbox`
    ///     (either the item was never in this mailbox, or it was
    ///     EXPUNGE'd between IMAP UID resolution and the STORE call).
    ///   * `"mailbox not found"` — the mailbox does not exist, or the
    ///     account's set is unprovisioned.
    fn set_keywords_in_mailbox(
        &self,
        account: AccountId,
        id: ItemId,
        mailbox: ContainerId,
        keywords: Flags,
        tags: Tags,
    ) -> Result<()>;

    /// Atomically OR `bits` into the per-membership flags for
    /// `(item, mailbox)` and return the resulting full flag set. The
    /// read-modify-write happens inside a single `with_set_tx` so
    /// concurrent flag updates from peer sessions do not get
    /// clobbered (which `set_keywords_in_mailbox` would do — it
    /// replaces, it doesn't merge).
    ///
    /// Introduced for the IMAP FETCH `\Seen` side-effect (RFC 9051
    /// §6.4.5): a non-PEEK `BODY[...]` / `RFC822` / `RFC822.TEXT`
    /// must mark the message `\Seen` without affecting any other
    /// flag the user (or a peer session) has set.
    ///
    /// Idempotent: if every bit in `bits` is already set, the write
    /// is skipped and the existing flags are returned unchanged
    /// (avoids spurious notifier wakeups).
    ///
    /// Errors mirror [`Self::set_keywords_in_mailbox`].
    fn add_flags_in_mailbox(
        &self,
        account: AccountId,
        id: ItemId,
        mailbox: ContainerId,
        bits: Flags,
    ) -> Result<Flags>;

    /// Move the email from its (currently single) mailbox to
    /// `to_mailbox`, atomically. JMAP `Email/set` migration entry
    /// point for the simple "change mailboxIds to a single mailbox"
    /// case — i.e. moves between mailboxes as opposed to membership
    /// set replacement across multiple mailboxes.
    ///
    /// **Non-retrain.** Junk-boundary retraining lives on
    /// [`MailStore::move_message`] (the IMAP-shaped move with an
    /// explicit `src` and a stamp id), which inserts a
    /// `mail_retrain_outbox` row in the same `with_set_tx` scope. The
    /// JMAP-shaped path here intentionally omits that hookup; the
    /// JMAP layer either calls `move_message` for filter-driven
    /// retrains or relies on a separate retrain trigger surface
    /// (Phase 3).
    ///
    /// **Single-membership only.** This method requires the email to
    /// currently belong to exactly one mailbox; moving an email
    /// belonging to multiple mailboxes is the JMAP "set mailboxIds"
    /// case and surfaces as a typed error. A future
    /// `MailStore::set_mailboxes` will handle the multi-mailbox
    /// replacement. An email currently in `to_mailbox` (and only
    /// there) is a no-op.
    ///
    /// Errors:
    ///   * `"email not found"` — `id` does not exist or has zero
    ///     current memberships (orphan window before GC).
    ///   * `"mailbox not found"` — the account's set is unprovisioned,
    ///     or `to_mailbox` does not exist in this account.
    ///   * `"ambiguous source"` — the email belongs to more than one
    ///     mailbox; use `MailStore::move_message` (with explicit
    ///     `src`) or wait for `set_mailboxes`.
    fn move_email(&self, account: AccountId, id: ItemId, to_mailbox: ContainerId) -> Result<()>;

    /// Delete the email from every mailbox it currently belongs to,
    /// atomically. JMAP `Email/set destroy` migration entry point.
    ///
    /// Iterates the email's current memberships and removes each.
    /// `SqliteSetTx::remove_membership` performs the cross-DB
    /// blob_ref drop (refcount -= 1) and `item` row delete when the
    /// last membership goes; the FK `mail_envelopes.item_id ON
    /// DELETE CASCADE` removes the envelope sidecar row in the same
    /// statement. The orphan blob becomes a GC candidate after the
    /// quiescence window — actual blob unlink is `cosmix-mds`'s
    /// `gc.rs` problem, not this method's.
    ///
    /// Errors:
    ///   * `"email not found"` — `id` does not exist or has zero
    ///     current memberships (orphan window before GC).
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    fn delete_email(&self, account: AccountId, id: ItemId) -> Result<()>;

    /// JMAP `Email/changes` (RFC 8621 §4.1.5) migration entry
    /// point. Returns the deltas between `since_state` and the
    /// current state of the account's set, with each touched
    /// `ItemId` partitioned into `created` / `updated` /
    /// `destroyed` by its net effect at the new state. See
    /// [`EmailChangeSet`] for the classification rules.
    ///
    /// `since_state` is interpreted as a `set_change_seq`. The
    /// returned `new_state` is the largest seq covered by this
    /// response; if no events have occurred, `new_state ==
    /// since_state`.
    ///
    /// The change-stream walk and the per-item "exists at
    /// new_state" classification queries run inside a single
    /// `with_set_tx` so a delete committed between them cannot
    /// leak as an event past `new_state`.
    ///
    /// **No `max_changes` cap.** This primitive returns *every*
    /// change since `since_state` and issues `O(distinct_items)`
    /// `item_memberships` reads in the classification loop. The
    /// JMAP protocol layer above is responsible for enforcing
    /// `Email/changes maxChanges` (RFC 8621 §4.1.5) and converting
    /// over-cap responses into `cannotCalculateChanges`.
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    ///   * `"since_state out of range"` — `since_state >
    ///     i64::MAX`; SQLite stores `set_change_seq` as a signed
    ///     64-bit integer and this primitive refuses to silently
    ///     wrap to a negative value at the SQL boundary.
    ///   * `"future since_state"` — `since_state` is greater than
    ///     the per-set head (`set_state.set_change_seq`). Per RFC
    ///     8620 §5.2 this is "not a valid state token"; the JMAP
    ///     handler maps the prefix to `cannotCalculateChanges`.
    ///   * `"stale since_state"` — `since_state` is strictly below
    ///     the retention floor (`set_state.set_change_floor`) raised
    ///     by `Mds::prune_changelog` (MCS Phase 3-A). Reads use
    ///     `seq > since`, so `since_state == floor` is still
    ///     answerable; only strictly-below tokens are rejected.
    ///     Handler maps the prefix to `cannotCalculateChanges`.
    fn email_changes(&self, account: AccountId, since_state: u64) -> Result<EmailChangeSet>;

    /// Read the per-set head `set_change_seq` — the JMAP `Email`-type
    /// state token observed *now* without walking any events. Returns
    /// `0` for an unprovisioned account, matching `email_changes`'s
    /// "no-history" baseline. Used by the `Email/set` and
    /// `Email/import` handlers to populate `oldState`/`newState` in
    /// the response without paying the `email_changes(0).new_state`
    /// full-history walk.
    fn email_state(&self, account: AccountId) -> Result<u64>;

    /// JMAP `Mailbox`-type state token observed *now*. Returns the paired
    /// `"<lifecycle_seq>:<set_change_seq>"` form documented in
    /// `_doc/planned/mailbox-changes-substrate.md` §"State token":
    ///
    ///   * `lifecycle_seq` is
    ///     `max(MAX(container_change_set_seq), set_state.container_change_set_floor)`
    ///     from the account-wide container lifecycle stream (the
    ///     AUTOINCREMENT monotone allocator), lifted against the
    ///     retention floor so that `keep_n=0`-style prunes which
    ///     empty the lifecycle table never cause the emitted token
    ///     to dip below the floor and turn round-tripped tokens
    ///     into stale rejections on the very next call.
    ///   * `set_change_seq` is the per-set head from
    ///     `set_state.set_change_seq` — the same value `email_state`
    ///     returns. The Mailbox token folds it in so a count-only
    ///     change (a new message landing in a folder) advances
    ///     `state` without forcing a fresh lifecycle event. The
    ///     allocator counter survives row deletes, so this half
    ///     does not need an analogous floor lift.
    ///
    /// Returns `"0:0"` for an unprovisioned account, matching
    /// `mailbox_changes`'s "no-history" baseline. Used by
    /// `Mailbox/get`, `Mailbox/query`, and `Mailbox/set`'s
    /// `oldState`/`newState` projection.
    fn mailbox_state(&self, account: AccountId) -> Result<String>;

    /// JMAP `Mailbox/changes` (RFC 8621 §2.6) backed by the
    /// `container_change_set` lifecycle stream paired with the
    /// `set_change` count stream. Returns the per-container
    /// classification described in `_doc/planned/mailbox-changes-substrate.md`
    /// §Design — Option B.
    ///
    /// `since_state` is the paired token `"<lifecycle>:<set_change>"`
    /// previously returned as `new_state`. Both halves are validated
    /// against the current per-set heads and the per-stream retention
    /// floors raised by `Mds::prune_changelog` (MCS Phase 3-A):
    ///
    ///   * Bad syntax → `"invalid sinceState"` (handler maps to
    ///     `invalidArguments`).
    ///   * Either half above its current head → `"future since_state"`
    ///     (handler maps to `cannotCalculateChanges`). The lifecycle
    ///     head is `max(MAX(container_change_set_seq), container_change_set_floor)`
    ///     — same lift as `mailbox_state`'s emitted token — so a
    ///     round-tripped `since == floor` after a `keep_n=0` prune
    ///     is not mis-classified as "future".
    ///   * Either half strictly below its retention floor →
    ///     `"stale since_state"` (handler maps to
    ///     `cannotCalculateChanges`). Predicate is strict `<`:
    ///     `since == floor` is still answerable because reads use
    ///     `seq > since`.
    ///
    /// Pagination phasing follows the plan: when the lifecycle slice
    /// is truncated by `max_changes`, `new_state` advances only the
    /// lifecycle half, count synthesis is suppressed for this page,
    /// and `has_more_changes = true`. On the final page (lifecycle
    /// drained), `new_state` advances both halves and the count
    /// stream contributes `updated` entries for containers touched
    /// by item churn within the window but not present in the
    /// lifecycle stream — see `MailboxChangeSet`'s doc for the
    /// precedence rules.
    ///
    /// `updated_properties` is always `None` in Phase 2 (RFC 8621
    /// §2.6's "null" projection — "any may have changed"); a later
    /// refinement may tighten this to the intersection across
    /// entries when every updated container was driven by a known
    /// lifecycle kind.
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned
    ///     and `since_state` was not `"0:0"`.
    ///   * `"invalid sinceState"` — token does not parse as
    ///     `<u64>:<u64>`.
    ///   * `"future since_state"` — either half is greater than the
    ///     current corresponding per-set head (lifecycle head lifted
    ///     against `container_change_set_floor`).
    ///   * `"stale since_state"` — either half is strictly below its
    ///     retention floor (`container_change_set_floor` or
    ///     `set_change_floor`). Handler maps the prefix to
    ///     `cannotCalculateChanges`.
    fn mailbox_changes(
        &self,
        account: AccountId,
        since_state: &str,
        max_changes: usize,
    ) -> Result<MailboxChangeSet>;

    /// JMAP `Thread/get` (RFC 8621 §4.2) migration entry point.
    /// Returns the `ItemId`s of every email currently associated
    /// with `thread_id` for `account`, ordered by RFC 5322 origination
    /// date (`mail_envelopes.date_ts`) ascending — JMAP `Thread.emailIds`
    /// MUST be in date order per §4.2.
    ///
    /// Read-only; the entire query runs in one `with_set_tx` so the
    /// `mail_threads` ↔ `mail_envelopes` ↔ `item` join is point-in-time
    /// consistent (a concurrent delete cannot return a thread member
    /// whose item row has already been reaped).
    ///
    /// **Behaviour change vs. legacy.** The pre-migration path
    /// `cosmix-maild::db::thread::get_by_ids` ordered thread members by
    /// `received_at` (delivery time) ASC. This method orders by
    /// `mail_envelopes.date_ts` (RFC 5322 origination) ASC — RFC 8621
    /// §4.2 specifies `Thread.emailIds` is sorted by the `Date:` header,
    /// not by delivery time. For threads where the two orders diverge
    /// (clock-skewed senders, imported mail, replies received before the
    /// original) the JMAP-visible `Thread.emailIds` order will change
    /// when the `Thread/get` handler migrates to this primitive in
    /// Phase 4.
    ///
    /// Filtering is by `(account_id, thread_id)` against the
    /// `mail_threads` sidecar; rows are joined to `mail_envelopes` for
    /// `date_ts` and to `item` for `received_at` as a deterministic
    /// fallback. Items predating Task 1.7's `create_email` write path
    /// will lack a `mail_envelopes` row — those sort by `received_at`
    /// (delivery time, in milliseconds) and after items with envelope
    /// rows whose `date_ts` is in the same window. The fallback exists
    /// so this method does not silently drop rows; once Phase A
    /// backfills envelopes uniformly, every row sorts by `date_ts`.
    /// Ties break on the `item_id` string for reproducibility.
    ///
    /// Returns an empty `Vec` when the thread has no members in this
    /// account (a thread referenced from a deleted email is observable
    /// as an empty `Thread` until the JMAP layer destroys it).
    /// The `JOIN item` is load-bearing for the `i.received_at` ordering
    /// fallback and the `i.id` tie-break. Since v1.8 it no longer
    /// *filters* any `mail_threads` row: `mail_threads.item_id` now
    /// carries `REFERENCES item(id) ON DELETE CASCADE`
    /// (`cosmix-mds/src/schema/v1_8.sql`), so dangling rows are reaped
    /// when the `item` is deleted rather than skipped at read time. The
    /// join therefore doubles as a post-v1.8 debug invariant — a
    /// `debug_assert` in the impl pins "the join drops nothing".
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    fn list_thread(&self, account: AccountId, thread_id: ThreadId) -> Result<Vec<ItemId>>;

    /// JMAP `Thread/get` no-`ids` paginated listing. Returns up to
    /// `limit` distinct `ThreadId`s currently observable in
    /// `mail_threads` for this account, skipping the first
    /// `position` of them, ordered by `thread_id` ASC for stable
    /// pagination. Threads whose every member has been reaped are
    /// not surfaced (since v1.8 the FK cascade reaps `mail_threads`
    /// rows on `item` delete, so a fully-reaped thread observably
    /// disappears; the `JOIN item` is kept as a debug invariant).
    ///
    /// RFC 8620 §5.1 allows `Thread/get` with `ids: null` to return
    /// every Thread up to `maxObjectsInGet`. Legacy `db::thread::
    /// query_ids` capped at LIMIT 100 with no error; this primitive
    /// preserves that contract — JMAP handlers cap `limit` to 100
    /// to match. There is no overflow signal; clients that need
    /// pagination must supply explicit `ids`.
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned.
    fn list_threads(&self, account: AccountId, position: u64, limit: u64) -> Result<Vec<ThreadId>>;

    /// IMAP IDLE notifier surface. Returns a
    /// `tokio::sync::broadcast::Receiver<ContainerEvent>` keyed by
    /// `(account, mailbox)`; events fire post-commit only, matching
    /// the `cosmix-mds::notifier` discipline (`BufferedEvents::
    /// drain_into` is invoked from `with_set_tx` after `tx.commit()`).
    ///
    /// The wrapper validates the mailbox exists *before* subscribing
    /// so that an unknown mailbox surfaces as `"mailbox not found"`
    /// rather than silently creating an idle channel that never
    /// publishes. (The underlying `notifier::subscribe` is lazy —
    /// it allocates a `broadcast::Sender` on first call regardless of
    /// whether the container row exists.) An unprovisioned account
    /// also surfaces as `"mailbox not found"` for parity with the
    /// other read-side methods.
    ///
    /// Capacity is `cosmix_mds::notifier::DEFAULT_CAPACITY` (256). A
    /// slow IMAP IDLE consumer that falls more than that many events
    /// behind receives a `RecvError::Lagged` and must resync (a
    /// fresh SELECT against the same mailbox suffices). This is
    /// consistent with the v1 mds notifier commitment ("wakes,
    /// doesn't deadlock"); sub-50ms p99 latency tuning is a v1.x
    /// concern, not a contract.
    ///
    /// Per-mailbox isolation: the underlying channel is keyed by
    /// `(SetId, ContainerId)`. A mutation in a sibling mailbox of the
    /// same account does NOT wake this subscriber — the receiver is
    /// strictly bound to the container the caller named.
    ///
    /// Errors:
    ///   * `"mailbox not found"` — the account's set is unprovisioned,
    ///     or the mailbox does not exist in this account.
    fn subscribe_mailbox(
        &self,
        account: AccountId,
        mailbox: ContainerId,
    ) -> Result<broadcast::Receiver<ContainerEvent>>;
}

/// SQLite-backed `MailStore` over `cosmix-mds::SqliteCasMds`.
pub struct SqliteMailStore {
    mds: Arc<SqliteCasMds>,
}

impl SqliteMailStore {
    pub fn new(mds: Arc<SqliteCasMds>) -> Self {
        Self { mds }
    }

    pub fn mds(&self) -> &Arc<SqliteCasMds> {
        &self.mds
    }

    /// Per-mailbox admin stats for one account: name, role, message
    /// count, unread count, and total bytes. Backs `maild.stats.*`
    /// (the doveadm `mailbox status` analog).
    ///
    /// Reuses the same `list_containers` + `container_status` reads as
    /// [`MailStore::list_mailboxes`], but projects `status.size_bytes`
    /// (already computed by `container_status`) which `MailboxRecord`
    /// does not carry. Read-only; the internal upload-staging container
    /// is filtered out, exactly as the JMAP mailbox listing does. A
    /// brand-new account with no set yet returns an empty vec.
    pub fn mailbox_stats(&self, account: AccountId) -> Result<Vec<MailboxStat>> {
        let set = account_id_to_setid(account);
        let containers = match self.mds.list_containers(&set) {
            Ok(v) => v,
            Err(cosmix_mds::Error::SetNotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!(e)),
        };
        let mut out = Vec::with_capacity(containers.len());
        for ci in containers {
            if ci.name == UPLOAD_STAGING_NAME {
                continue;
            }
            // Eventual consistency: a container destroyed between the
            // list and the status read drops from the listing rather
            // than failing the whole call (same contract as
            // `list_mailboxes`).
            let status = match self.mds.container_status(&set, &ci.id) {
                Ok(s) => s,
                Err(cosmix_mds::Error::ContainerNotFound(_)) => continue,
                Err(e) => return Err(anyhow!(e)),
            };
            // `container_status` gives counts but not bytes, so sum
            // `size_bytes` over the container's items. O(messages) per
            // mailbox — acceptable for an occasional admin read; a
            // cheaper MDS-level `SUM(size_bytes)` aggregate (shared with
            // the deferred IMAP STATUS=SIZE) can replace this later.
            let size_bytes: u64 = match self.mds.list_items(&set, &ci.id, SeqRange::All) {
                Ok(items) => items.iter().map(|r| r.meta.size_bytes).sum(),
                Err(cosmix_mds::Error::ContainerNotFound(_)) => continue,
                Err(e) => return Err(anyhow!(e)),
            };
            let role = ci
                .attrs
                .special_use
                .as_deref()
                .and_then(MailboxRole::from_special_use);
            out.push(MailboxStat {
                name: ci.name,
                role,
                total_emails: status.exists,
                unread_emails: status.unread,
                size_bytes,
            });
        }
        Ok(out)
    }

    /// Account-wide rollup for `maild.stats.account` (the doveadm
    /// `quota get` analog). Unlike summing [`Self::mailbox_stats`]
    /// per-folder counts, this **dedupes by `ItemId`** so a multi-homed
    /// message (filed in several mailboxes) counts **once** toward
    /// `message_count` / `total_bytes` — the physical-storage figure an
    /// operator expects from a quota view. `mailbox_count` excludes the
    /// internal upload-staging container. Empty for an unprovisioned
    /// account.
    pub fn account_stats(&self, account: AccountId) -> Result<AccountStat> {
        let set = account_id_to_setid(account);
        let containers = match self.mds.list_containers(&set) {
            Ok(v) => v,
            Err(cosmix_mds::Error::SetNotFound(_)) => return Ok(AccountStat::default()),
            Err(e) => return Err(anyhow!(e)),
        };
        // Dedupe items across all containers so a multi-homed message is
        // counted once (same ItemId-keyed dedupe as the size-spread
        // query elsewhere in this module).
        let mut sizes: std::collections::HashMap<ItemId, u64> = std::collections::HashMap::new();
        let mut mailbox_count = 0u64;
        for ci in containers {
            if ci.name == UPLOAD_STAGING_NAME {
                continue;
            }
            let items = match self.mds.list_items(&set, &ci.id, SeqRange::All) {
                Ok(v) => v,
                // Container destroyed mid-scan: don't count it (matches
                // the list_mailboxes snapshot-consistency contract).
                Err(cosmix_mds::Error::ContainerNotFound(_)) => continue,
                Err(e) => return Err(anyhow!(e)),
            };
            mailbox_count += 1;
            for r in items {
                sizes.entry(r.id).or_insert(r.meta.size_bytes);
            }
        }
        Ok(AccountStat {
            mailbox_count,
            message_count: sizes.len() as u64,
            total_bytes: sizes.values().copied().sum(),
        })
    }

    /// Retention sweep for ONE account: remove memberships in the
    /// account's Junk and Trash mailboxes whose per-membership `added_at`
    /// is older than the configured window (dovecot `savedbefore`
    /// semantics — keyed on when the message entered that folder, NOT its
    /// original receipt date), oldest-first, capped per sweep.
    ///
    /// `now_ms` is Unix-epoch milliseconds (same unit as membership
    /// `added_at`); a fake clock drives the unit tests. The cutoff for a
    /// folder is `now_ms - days * 86_400_000`; a `*_retention_days` of 0
    /// disables that folder.
    ///
    /// **Multi-homed safety:** removal is membership-scoped
    /// (`tx.remove_membership`), so a message also filed in another
    /// mailbox (e.g. Inbox) keeps its other memberships and only its
    /// Junk/Trash copy is dropped. The item row (and its CAS blob ref)
    /// are dropped by the MDS only when this was the item's LAST
    /// membership; in that case the maild-side FTS row is reaped in the
    /// same tx via [`reap_mail_search_if_item_gone`] (FTS5 can't
    /// FK-cascade, so this mirrors `expunge_memberships`/`delete_email`).
    /// The freed blob becomes GC-eligible but is not reclaimed here —
    /// the worker calls `mds.gc()` once post-sweep.
    ///
    /// Target mailboxes are resolved by **special-use attribute** only
    /// (`\Junk` / `\Trash`), never by name, so a renamed or localized
    /// folder still binds and retention can never point at the Inbox.
    ///
    /// Roles are resolved and aged memberships are selected+removed
    /// inside ONE `with_set_tx` so the candidate set can't shift between
    /// selection and removal. An unprovisioned account (no set) or an
    /// account missing a Junk/Trash folder is a clean no-op. When no
    /// folder is armed the call short-circuits without opening a tx.
    pub fn retention_sweep_account(
        &self,
        account: AccountId,
        params: &RetentionParams,
        now_ms: i64,
    ) -> Result<RetentionSweepReport> {
        // Nothing armed → no set enumeration, no tx (mirrors the
        // empty-input short-circuit elsewhere in this module).
        if params.junk_retention_days == 0 && params.trash_retention_days == 0 {
            return Ok(RetentionSweepReport::default());
        }
        let set = account_id_to_setid(account);
        let dry_run = params.dry_run;
        // Clamp the caller-supplied (public, untrusted) params to safe
        // ceilings BEFORE any arithmetic: an oversized `*_retention_days`
        // would overflow the `now_ms - days*86_400_000` cutoff (and a
        // wrap into a future cutoff would delete EVERYTHING), and an
        // oversized cap would overflow the `cap+1` probe / make the
        // SQLite `LIMIT` cast negative (an unbounded LIMIT silently
        // disabling the per-sweep guard). The ceilings mirror the
        // `maild.retention` namespace bounds.
        let junk_days = params.junk_retention_days.min(RETENTION_MAX_DAYS);
        let trash_days = params.trash_retention_days.min(RETENTION_MAX_DAYS);
        let cap = params
            .max_deletes_per_sweep
            .min(RETENTION_MAX_DELETES_PER_SWEEP);

        let result = self.mds.with_set_tx(&set, |tx| {
            let mut remaining = cap;
            let mut junk_removed = 0u64;
            let mut junk_capped = false;
            let mut trash_removed = 0u64;
            let mut trash_capped = false;

            if junk_days > 0 && remaining > 0 {
                // `saturating_sub` so an extreme (negative) `now_ms` from
                // an untrusted caller floors the cutoff rather than
                // underflowing into a future value that would delete the
                // wrong mail. `junk_days` is clamped ≤ RETENTION_MAX_DAYS,
                // so the multiply itself can't overflow.
                let cutoff = now_ms.saturating_sub((junk_days as i64) * 86_400_000);
                let (removed, capped) =
                    sweep_role_in_tx(tx, SPECIAL_USE_JUNK, cutoff, remaining, dry_run)?;
                junk_removed = removed;
                junk_capped = capped;
                // Dry-run "removes" nothing, so it must not consume the
                // cap budget meant for real deletions on the trash pass.
                if !dry_run {
                    remaining = remaining.saturating_sub(removed);
                }
            }
            if trash_days > 0 && remaining > 0 {
                let cutoff = now_ms.saturating_sub((trash_days as i64) * 86_400_000);
                let (removed, capped) =
                    sweep_role_in_tx(tx, SPECIAL_USE_TRASH, cutoff, remaining, dry_run)?;
                trash_removed = removed;
                trash_capped = capped;
            }
            Ok(RetentionSweepReport {
                junk_removed,
                trash_removed,
                junk_capped,
                trash_capped,
            })
        });
        match result {
            Ok(r) => Ok(r),
            // Account never provisioned a set → nothing to sweep.
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(RetentionSweepReport::default()),
            Err(e) => Err(anyhow!(e)),
        }
    }

    /// Idempotently ensure that the per-account MDS set exists.
    ///
    /// Maild bootstraps each account's set on first use; calling this
    /// twice for the same account is a no-op. Used by the JMAP /
    /// SMTP delivery paths before any other MailStore call against
    /// that account.
    pub fn ensure_account_set(&self, account: AccountId) -> Result<SetId> {
        let set = account_id_to_setid(account);
        let existing = self.mds.list_sets()?;
        if !existing.contains(&set) {
            self.mds.create_set(&set)?;
        }
        Ok(set)
    }

    /// Phase 8e gate 3 — rebuild the `mail_search` FTS5 projection for
    /// one account from the underlying `mail_envelopes` rows and the
    /// CAS blob bodies. Idempotent at the query layer: running twice
    /// in succession yields the same MATCH results for every probe
    /// (we do not promise byte-identical FTS5 shadow-table storage —
    /// `contentless_delete=1` doclist residue is implementation
    /// detail).
    ///
    /// Use when the projection is corrupt (FTS5 columns desynced from
    /// envelope storage) or absent (administrative wipe / failed
    /// migration). Acceptance per
    /// `src/_tasks/2026-05-04-maild-phase-8e.spec.mix:32` —
    /// "mail_search rebuild reproduces the projection bit-for-bit
    /// from a corrupted/missing baseline."
    ///
    /// Concurrency: the entire rebuild runs in one `with_set_tx`, so
    /// no partial state is ever visible. A `create_email` arriving
    /// concurrently serialises behind the per-set mutex: either it
    /// commits before the rebuild snapshot (the new row is included
    /// in the rebuild) or it waits and re-INSERTs its `mail_search`
    /// row after the rebuild's transaction commits (the new row lands
    /// on top of the freshly-rebuilt baseline). Either ordering yields
    /// a correct projection.
    ///
    /// Empty-account semantics: an account with no MDS set (never
    /// stored a message) returns `RebuildReport { 0, 0 }`. We
    /// explicitly do NOT bootstrap the set here — rebuild is a
    /// read-rewrite of existing state, not an initialisation path.
    ///
    /// Blob-load failures are tolerated per `[[gate-2 policy]]`: a
    /// missing CAS blob produces an envelope-only row (empty
    /// `body_text`) and is counted in `blob_load_failures`. This
    /// matches `create_email`'s behaviour at the original write site
    /// (`mailstore::mod.rs` around the gate-2 INSERT) so a rebuild
    /// cannot regress search recall below what create_email achieved.
    pub fn rebuild_search_index(&self, account: AccountId) -> Result<RebuildReport> {
        let set = account_id_to_setid(account);

        // Skip cleanly when the account has no MDS set at all. Calling
        // `with_set_tx` on a missing set would surface `SetNotFound`;
        // for a rebuild verb that's not an error — there is simply
        // nothing to project.
        let sets = self
            .mds
            .list_sets()
            .map_err(|e| anyhow::anyhow!("rebuild_search_index list_sets: {e}"))?;
        if !sets.contains(&set) {
            return Ok(RebuildReport {
                items_rebuilt: 0,
                blob_load_failures: 0,
            });
        }

        let mds = Arc::clone(&self.mds);
        let report = self
            .mds
            .with_set_tx(&set, |tx| {
                // Step 1: snapshot every (envelope, blob_hash) tuple
                // in the set. Bounded by the set's item count; a
                // separate prepared statement keeps SQLite from
                // re-parsing the JOIN per row.
                type EnvelopeRow = (
                    String,         // item_id
                    String,         // blob_hash (hex)
                    String,         // from_addr
                    String,         // to_addrs JSON
                    String,         // cc_addrs JSON
                    String,         // bcc_addrs JSON
                    String,         // reply_to_addrs JSON
                    String,         // subject
                    i64,            // date_ts
                    Option<String>, // message_id
                );
                let rows: Vec<EnvelopeRow> = {
                    let mut stmt = tx
                        .tx()
                        .prepare(
                            // ORDER BY e.item_id pins a deterministic
                            // insertion order so the FTS5 rowid
                            // sequence is the same on every rebuild;
                            // without it the JOIN's row order is
                            // unspecified. The acceptance contract is
                            // bit-identical MATCH results, not
                            // byte-identical shadow-table storage —
                            // FTS5 delete/insert leaves doclist
                            // residue that we do not promise to
                            // reproduce.
                            "SELECT e.item_id, i.blob_hash, e.from_addr, \
                                    e.to_addrs, e.cc_addrs, e.bcc_addrs, \
                                    e.reply_to_addrs, e.subject, e.date_ts, \
                                    e.message_id \
                             FROM mail_envelopes e \
                             JOIN item i ON i.id = e.item_id \
                             ORDER BY e.item_id",
                        )
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index prepare: {e}"
                            ))
                        })?;
                    let iter = stmt
                        .query_map([], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                                r.get::<_, String>(5)?,
                                r.get::<_, String>(6)?,
                                r.get::<_, String>(7)?,
                                r.get::<_, i64>(8)?,
                                r.get::<_, Option<String>>(9)?,
                            ))
                        })
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index query_map: {e}"
                            ))
                        })?;
                    iter.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "rebuild_search_index collect: {e}"
                        ))
                    })?
                };

                // Step 2: wipe the entire `mail_search` projection in
                // this set. The per-set DB is scoped to a single
                // account in the current one-set-per-account layout
                // (`account_id_to_setid` is a pure function of the
                // account id), so the bare DELETE is both correct and
                // necessary for repair: a `WHERE account_id = ?1`
                // predicate trusts the `account_id` column we are
                // about to rebuild — if a corrupt row carries the
                // wrong account_id, the WHERE would skip it and the
                // rebuild would leave stale FTS5 rows behind. Wiping
                // the whole table inside this set tx is the only way
                // to guarantee "bit-for-bit reproduction from a
                // corrupted baseline" per
                // `src/_tasks/2026-05-04-maild-phase-8e.spec.mix:32`.
                // A future multi-account-per-set layout would need a
                // different scoping mechanism (item_id set membership,
                // not the in-row account_id) and is a re-tuning
                // trigger, not a correctness regression today.
                tx.tx()
                    .execute("DELETE FROM mail_search", [])
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "rebuild_search_index delete: {e}"
                        ))
                    })?;

                // Step 3: re-INSERT one row per envelope, projecting
                // through the same `build_search_projection` that
                // create_email uses so the result is byte-identical
                // to a fresh-write baseline.
                let mut items_rebuilt: u64 = 0;
                let mut blob_load_failures: u64 = 0;
                for (
                    item_id_str,
                    blob_hex,
                    from,
                    to_json,
                    cc_json,
                    bcc_json,
                    reply_to_json,
                    subject,
                    date_ts,
                    message_id,
                ) in rows
                {
                    let to: Vec<String> =
                        serde_json::from_str(&to_json).map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index parse to_addrs for {item_id_str}: {e}"
                            ))
                        })?;
                    let cc: Vec<String> =
                        serde_json::from_str(&cc_json).map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index parse cc_addrs for {item_id_str}: {e}"
                            ))
                        })?;
                    let bcc: Vec<String> =
                        serde_json::from_str(&bcc_json).map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index parse bcc_addrs for {item_id_str}: {e}"
                            ))
                        })?;
                    let reply_to: Vec<String> =
                        serde_json::from_str(&reply_to_json).map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index parse reply_to_addrs for {item_id_str}: {e}"
                            ))
                        })?;
                    let envelope = EmailEnvelope {
                        from,
                        to,
                        cc,
                        bcc,
                        reply_to,
                        subject,
                        date: date_ts,
                        message_id,
                    };
                    let blob_hash = blob::from_hex(&blob_hex).ok_or_else(|| {
                        cosmix_mds::Error::Other(format!(
                            "rebuild_search_index: item {item_id_str} has \
                             invalid blob_hash hex ({blob_hex:?})"
                        ))
                    })?;
                    let raw_body = match mds.get_blob(&blob_hash) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(
                                item_id = %item_id_str,
                                blob = %blob_hex,
                                err = %e,
                                "rebuild_search_index: blob load failed, indexing envelope only"
                            );
                            blob_load_failures += 1;
                            Vec::new()
                        }
                    };
                    let projection = build_search_projection(&envelope, &raw_body);
                    let inserted = tx
                        .tx()
                        .execute(
                            "INSERT INTO mail_search \
                             (item_id, account_id, headers, subject, body_text, normalized_addrs) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                item_id_str,
                                account,
                                projection.headers,
                                projection.subject,
                                projection.body_text,
                                projection.normalized_addrs,
                            ],
                        )
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "rebuild_search_index insert {item_id_str}: {e}"
                            ))
                        })?;
                    if inserted != 1 {
                        return Err(cosmix_mds::Error::Other(format!(
                            "rebuild_search_index insert {item_id_str}: expected 1 row, got {inserted}"
                        )));
                    }
                    items_rebuilt += 1;
                }

                Ok(RebuildReport {
                    items_rebuilt,
                    blob_load_failures,
                })
            })
            .map_err(|e| anyhow::anyhow!("rebuild_search_index: {e}"))?;

        Ok(report)
    }
}

/// Result of [`SqliteMailStore::rebuild_search_index`]. `items_rebuilt`
/// counts every `mail_search` row written; `blob_load_failures` counts
/// items whose CAS blob couldn't be loaded — those items still got an
/// envelope-only row (empty `body_text`) inserted so they remain
/// addressable by header/subject search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildReport {
    pub items_rebuilt: u64,
    pub blob_load_failures: u64,
}

/// Resolve the `mail_threads.thread_id` for a freshly-created email.
///
/// Two-pass JMAP §4.1.5 / RFC 5256 §2.1 lookup:
///
/// 1. Walks `in_reply_to` (parsed `In-Reply-To:` / `References:`
///    Message-IDs, bare addr-spec strings without angle brackets) in
///    input order; the first reference whose
///    `mail_envelopes.message_id` matches an existing item in this
///    account hands back that item's `mail_threads.thread_id`.
/// 2. On miss, normalizes `subject` per RFC 5256 §2.1 (see
///    `subject::normalize_subject`) and looks the result up against
///    `mail_envelopes.normalized_subject` joined to `mail_threads` on
///    the same account. The first match wins. Empty normalized
///    subjects deliberately skip this pass so blank-subject emails
///    don't cluster into one giant thread.
/// 3. Only when both passes miss is a fresh UUIDv4 thread id
///    allocated.
///
/// Runs inside the caller's `with_set_tx` callback; reads via
/// `tx.tx()` against the same per-set Rusqlite transaction so the
/// match (or lack thereof) is read-consistent with the
/// `mail_envelopes` row this same callback is about to (or just did)
/// insert.
/// Parse the JMAP `Mailbox`-type paired state token
/// `"<lifecycle_seq>:<set_change_seq>"`. Both halves are unsigned
/// 64-bit decimals; rejects empty halves, non-decimal characters,
/// missing colon, leading `+`/`-`, and any non-canonical encoding so
/// a future change to the on-wire form (e.g. base32) trips a clean
/// error instead of silently aliasing two distinct tokens.
fn parse_paired_token(s: &str) -> Option<(u64, u64)> {
    fn parse_half(part: &str) -> Option<u64> {
        if part.is_empty() {
            return None;
        }
        // Reject signs: `u64::from_str` already rejects `-`, but a
        // leading `+` parses on some platforms via different paths.
        if part.starts_with('+') || part.starts_with('-') {
            return None;
        }
        // Reject non-canonical leading zeros — `"01"` and `"1"` must
        // not alias, or a client that round-trips a state token through
        // its own normalisation would see two distinct tokens map to
        // the same head and miss the `cannotCalculateChanges` signal
        // when a future encoding (e.g. base32) lands.
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        part.parse::<u64>().ok()
    }
    let (l, r) = s.split_once(':')?;
    let lifecycle = parse_half(l)?;
    let set_change = parse_half(r)?;
    Some((lifecycle, set_change))
}

fn find_or_new_thread_id_in_tx(
    tx: &cosmix_mds::SqliteSetTx<'_>,
    account: AccountId,
    in_reply_to: &[String],
    subject: &str,
) -> std::result::Result<String, cosmix_mds::Error> {
    // Step 1 (JMAP §4.1.5): References / In-Reply-To match. Each
    // reference is a raw RFC 5322 Message-ID; first match wins.
    for r in in_reply_to {
        let row: Option<String> = tx
            .tx()
            .query_row(
                "SELECT mt.thread_id \
                 FROM mail_threads mt \
                 JOIN mail_envelopes e ON e.item_id = mt.item_id \
                 WHERE mt.account_id = ?1 AND e.message_id = ?2 \
                 LIMIT 1",
                params![account, r],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                cosmix_mds::Error::Other(format!("find_or_new_thread_id_in_tx lookup: {e}"))
            })?;
        if let Some(tid) = row {
            return Ok(tid);
        }
    }
    // Step 2 (Phase 8e gate 4): RFC 5256 §2.1 base-subject fallback.
    // A reply with no References/In-Reply-To (common for mobile MUAs
    // and Outlook) still clusters with the original when its
    // normalized subject matches an existing thread in the same
    // account. Skip when normalization yields empty — otherwise
    // every empty-subject email would falsely cluster.
    //
    // Multiple existing threads can share a normalized subject (e.g.
    // two unrelated "weekly status" threads months apart). The
    // `ORDER BY e.date_ts DESC, e.message_id DESC, e.item_id DESC
    // LIMIT 1` clause picks the most recent matching thread
    // deterministically. The key chain is layered so the final
    // total-order tie-break is `item_id`, which is the per-set
    // PRIMARY KEY of `mail_envelopes` — always present and unique —
    // even if `message_id` is NULL or two envelopes share the same
    // (date_ts, message_id). This matches the "cluster with the
    // most recent same-subject thread" intuition every mainstream
    // MUA implements; without the ordering the selected thread was
    // arbitrary and would flap between SQLite page-order changes.
    //
    // The composite index `mail_envelopes_normsubj_idx
    // (normalized_subject, date_ts, message_id, item_id)` covers the
    // WHERE filter and the ORDER BY chain so the planner can walk a
    // bounded index range in reverse and skip the TEMP B-TREE sort.
    // SQLite cannot infer the one-set-per-account invariant from the
    // schema, so this is the observed plan today (under the current
    // `mailstore::setid` layout EXPLAIN QUERY PLAN drives the loop
    // from `mail_envelopes`), not a planner-level guarantee. If the
    // invariant ever relaxes — or statistics shift enough that the
    // planner picks `mail_threads(account_id, thread_id)` first — the
    // fallback to a TEMP sort is legal: a re-tuning trigger, not a
    // correctness regression (see `cosmix-mds/src/schema/v1_7.sql`
    // for the full rationale).
    let normalized = subject::normalize_subject(subject);
    if !normalized.is_empty() {
        let row: Option<String> = tx
            .tx()
            .query_row(
                "SELECT mt.thread_id \
                 FROM mail_threads mt \
                 JOIN mail_envelopes e ON e.item_id = mt.item_id \
                 WHERE mt.account_id = ?1 \
                   AND e.normalized_subject = ?2 \
                 ORDER BY e.date_ts DESC, e.message_id DESC, e.item_id DESC \
                 LIMIT 1",
                params![account, &normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                cosmix_mds::Error::Other(format!("find_or_new_thread_id_in_tx subject lookup: {e}"))
            })?;
        if let Some(tid) = row {
            return Ok(tid);
        }
    }
    Ok(Uuid::new_v4().to_string())
}

impl MailStore for SqliteMailStore {
    fn create_blob_ref(
        &self,
        account: AccountId,
        hash: BlobHash,
        size: u64,
        expires_at: i64,
    ) -> Result<BlobId> {
        let set = account_id_to_setid(account);
        let blob_id = BlobId::new();
        let now = chrono::Utc::now().timestamp();
        let staging_attrs = ContainerAttrs {
            special_use: None,
            subscribed: false,
            extra: serde_json::json!({}),
        };

        self.mds
            .with_set_tx(&set, |tx| {
                let staging =
                    tx.ensure_container_by_name(None, UPLOAD_STAGING_NAME, &staging_attrs)?;

                let (item_id, _placement) =
                    tx.add_staging_item(&staging, &hash, size, Flags(0), &[])?;

                let inserted = tx
                    .tx()
                    .execute(
                        "INSERT INTO blob_refs \
                         (blob_id, account_id, blob_hash, created_at, expires_at, temp_item_id) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            blob_id.0.to_string(),
                            account,
                            blob::hex(&hash),
                            now,
                            expires_at,
                            item_id.0.to_string(),
                        ],
                    )
                    .map_err(|e| cosmix_mds::Error::Other(format!("insert blob_refs row: {e}")))?;

                if inserted != 1 {
                    return Err(cosmix_mds::Error::Other(format!(
                        "insert blob_refs row: expected 1 row, got {inserted}"
                    )));
                }
                Ok(())
            })
            .map_err(|e| anyhow!(e))?;

        Ok(blob_id)
    }

    fn resolve_blob_ref(&self, account: AccountId, blob_id: BlobId) -> Result<BlobRefLookup> {
        let set = account_id_to_setid(account);
        // Four-state classification per the trait doc-comment:
        //   * `Ok(BlobRefLookup::NotFound)` → alias row absent.
        //     Callers may try a legacy lookup during 3.3a migration.
        //   * `Ok(BlobRefLookup::Found(hash))` → resolved, CAS present.
        //   * `Ok(BlobRefLookup::Dangling)` → alias row exists but
        //     the CAS bytes are gone. A real `blobNotFound` — callers
        //     MUST NOT fall back to legacy storage.
        //   * `Err(_)` → internal failure (SQLite, hex parse).
        //     Propagate as a server error.
        // An account whose set has not yet been provisioned cannot
        // possibly have a `blob_refs` row — surface as `NotFound`
        // (rather than letting `SetNotFound` from `with_set_tx`
        // escape as `Err`) so the /download UUID branch can fall
        // through to the legacy `db::blob` lookup. Without this
        // remap, a fresh account with only legacy-uploaded blobs
        // would 500 instead of resolving via the legacy fallback.
        let result = self.mds.with_set_tx(&set, |tx| {
            let row = tx.tx().query_row(
                "SELECT blob_hash FROM blob_refs \
                 WHERE blob_id = ?1 AND account_id = ?2",
                params![blob_id.0.to_string(), account],
                |r| r.get::<_, String>(0),
            );
            let blob_hex: String = match row {
                Ok(s) => s,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Ok(BlobRefLookup::NotFound);
                }
                Err(other) => {
                    return Err(cosmix_mds::Error::Other(format!(
                        "lookup blob_ref {blob_id}: {other}"
                    )));
                }
            };

            let hash = blob::from_hex(&blob_hex).ok_or_else(|| {
                cosmix_mds::Error::Other(format!(
                    "blob_refs.blob_hash {blob_hex:?} is not 64-char hex"
                ))
            })?;

            // Verify the underlying CAS blob still exists. blob_refs
            // are non-owning aliases — staging item GC, core MDS GC,
            // or operator surgery can leave the alias dangling. The
            // ATTACH'd `blobs_db` is in the same tx, so the check is
            // consistent with the read above. Dangling surfaces as
            // a distinct variant (NOT collapsed into NotFound) so
            // callers do not silently fall back to legacy storage
            // when the alias was minted but its bytes are gone.
            let alive: i64 = tx
                .tx()
                .query_row(
                    "SELECT COUNT(*) FROM blobs_db.blob WHERE hash = ?1",
                    params![&blob_hex],
                    |r| r.get(0),
                )
                .map_err(|e| {
                    cosmix_mds::Error::Other(format!("check CAS blob existence for {blob_id}: {e}"))
                })?;
            if alive == 0 {
                return Ok(BlobRefLookup::Dangling);
            }
            Ok(BlobRefLookup::Found(hash))
        });
        match result {
            Ok(lookup) => Ok(lookup),
            // `with_set_tx` returns `SetNotFound` when the per-account
            // set has not been bootstrapped yet (an account that has
            // never round-tripped through `ensure_account_set`).
            // Surface as `NotFound` so a legacy /download UUID can
            // fall through to `db::blob` rather than 500.
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(BlobRefLookup::NotFound),
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn create_mailbox(
        &self,
        account: AccountId,
        name: &str,
        parent: Option<ContainerId>,
        attrs: ContainerAttrs,
    ) -> Result<ContainerId> {
        // Name-shape guards run BEFORE `ensure_account_set` so a probe
        // with a reserved or empty name on a brand-new account cannot
        // bootstrap the per-account set as a side effect — the only
        // observable outcome of an invalid call must be the rejection.
        // (Required by Codex review of Tasks 1.1 / 1.2.)
        if name == UPLOAD_STAGING_NAME {
            return Err(anyhow!(
                "reserved name {name:?}: collides with upload-staging container"
            ));
        }
        if name.is_empty() {
            return Err(anyhow!("mailbox invalid name: must be non-empty"));
        }
        let set = self.ensure_account_set(account)?;
        // Plan §1.2 mandates `with_set_tx` for the write so future
        // sidecar additions (e.g. JMAP-side mailbox metadata) land
        // atomically with the MDS row, and so events are buffered for
        // post-commit replay rather than published on rollback.
        //
        // SPECIAL-USE / `MailboxRole` uniqueness barrier: storage has
        // no `(account, role)` unique index, but each per-set sqlite
        // contains exactly one account's containers and `with_set_tx`
        // serialises mutations on that set via the per-set mutex. A
        // SELECT-inside-tx against `json_extract(attrs, '$.special_use')`
        // therefore sees every committed peer and closes the TOCTOU
        // window between two concurrent `CREATE foo (USE (\Drafts))`
        // calls on different IMAP/JMAP connections.
        //
        // The `tx.tx()` escape hatch is used here because the role-
        // uniqueness invariant is a maild-level concept (`MailboxRole`),
        // not a generic mds primitive — keeping it out of the typed
        // `SqliteSetTx` surface avoids leaking IMAP/JMAP semantics into
        // the storage crate.
        let role_for_check = attrs.special_use.clone();
        match self.mds.with_set_tx(&set, |tx| {
            if let Some(ref su) = role_for_check {
                let n: i64 = tx
                    .tx()
                    .query_row(
                        "SELECT COUNT(*) FROM container \
                         WHERE json_extract(attrs, '$.special_use') = ?1",
                        [su],
                        |r| r.get(0),
                    )
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "special_use uniqueness probe failed: {e}"
                        ))
                    })?;
                if n > 0 {
                    return Err(cosmix_mds::Error::Other(format!(
                        "mailbox special-use already in use: {su}"
                    )));
                }
            }
            tx.create_container_named(parent.as_ref(), name, &attrs)
        }) {
            Ok(id) => Ok(id),
            Err(cosmix_mds::Error::ContainerNotFound(p)) => {
                Err(anyhow!("mailbox parent not found: {p}"))
            }
            Err(cosmix_mds::Error::ContainerAlreadyExists { .. }) => {
                Err(anyhow!("mailbox already exists: {name}"))
            }
            Err(cosmix_mds::Error::Other(msg))
                if msg.starts_with("mailbox special-use already in use") =>
            {
                Err(anyhow!(msg))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn rename_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        new_name: &str,
        new_parent: Option<ContainerId>,
    ) -> Result<()> {
        // Same name-shape guards as `create_mailbox`. Reserved-name on
        // rename is the symmetric guard for the staging-filter
        // contract: a mailbox renamed to `__upload_staging__` would
        // silently disappear from `list_mailboxes` (which filters that
        // name to hide `create_blob_ref`'s per-account staging
        // container), so the rename must be rejected up-front.
        if new_name == UPLOAD_STAGING_NAME {
            return Err(anyhow!(
                "reserved name {new_name:?}: collides with upload-staging container"
            ));
        }
        if new_name.is_empty() {
            return Err(anyhow!("mailbox invalid name: must be non-empty"));
        }
        // Unlike `create_mailbox`, rename must NOT bootstrap the
        // per-account set: a failed rename on an unprovisioned account
        // would otherwise create empty MDS state as a side effect of
        // an error path. Derive the set deterministically and surface
        // `SetNotFound` as `"mailbox not found"` — the rename target
        // can't exist if the set doesn't.
        let set = account_id_to_setid(account);
        match self.mds.with_set_tx(&set, |tx| {
            tx.rename_container(&id, new_parent.as_ref(), new_name)
        }) {
            Ok(()) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = id.0))
            }
            // `rename_container_in_tx` surfaces both "source missing"
            // and "new_parent missing" as `ContainerNotFound`. The
            // missing id distinguishes them. Source-id wins on
            // ambiguity (e.g. `id == new_parent` with both bogus):
            // the source check runs first inside MDS, so a
            // ContainerNotFound matching the source id is always the
            // correct classification, even if `new_parent.is_some()`
            // and would also match.
            Err(cosmix_mds::Error::ContainerNotFound(missing)) => {
                if missing == id.0.to_string() {
                    Err(anyhow!("mailbox not found: {missing}"))
                } else if let Some(p) = new_parent
                    && missing == p.0.to_string()
                {
                    Err(anyhow!("mailbox parent not found: {missing}"))
                } else {
                    Err(anyhow!("mailbox not found: {missing}"))
                }
            }
            Err(cosmix_mds::Error::ContainerAlreadyExists { .. }) => {
                Err(anyhow!("mailbox already exists: {new_name}"))
            }
            // Cycle detection lives in MDS (cycle + self-parent both
            // collapse onto this prefix from the trait doc).
            Err(cosmix_mds::Error::Other(s))
                if s.contains("would form a parent cycle")
                    || s.contains("cannot be its own parent") =>
            {
                Err(anyhow!("mailbox cycle: {s}"))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn destroy_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        on_emails: DestroyPolicy,
    ) -> Result<()> {
        // Like `rename_mailbox`, derive deterministically — never
        // bootstrap on an error path. SetNotFound maps to "mailbox not
        // found" because the destroy target cannot exist if its set
        // doesn't.
        let set = account_id_to_setid(account);
        match self.mds.with_set_tx(&set, |tx| {
            // Confirm the container exists and capture its name plus
            // the JMAP role (stored as `attrs.special_use`) in one read.
            // Cheap row read keeps the staging-container guard and the
            // role-protection guard at the same boundary, and returns
            // ContainerNotFound immediately on a bogus id rather than
            // waiting for the trailing `delete_container` to surface it.
            let row: Option<(String, Option<String>)> = tx
                .tx()
                .query_row(
                    "SELECT name, json_extract(attrs, '$.special_use') \
                     FROM container WHERE id = ?1",
                    params![id.0.to_string()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .optional()
                .map_err(|e| cosmix_mds::Error::Other(format!("read container row: {e}")))?;
            let (name, special_use) = match row {
                Some(t) => t,
                None => {
                    return Err(cosmix_mds::Error::ContainerNotFound(id.0.to_string()));
                }
            };
            // Symmetric guard with create/rename: the staging container
            // is internal infrastructure for `create_blob_ref`. JMAP
            // never exposes its id (filtered by `list_mailboxes`), so
            // any caller arriving with this id is a programming bug
            // upstream — refusing here keeps `create_blob_ref` viable.
            if name == UPLOAD_STAGING_NAME {
                return Err(cosmix_mds::Error::Other(format!(
                    "reserved container {UPLOAD_STAGING_NAME:?}: \
                     refusing to destroy the upload-staging container"
                )));
            }
            // RFC 8621 §2.5 role protection: refuse destroying any
            // mailbox with a non-null `attrs.special_use`. The guard
            // fires on the raw stored value, including unrecognized
            // special-use strings that `MailboxRole::from_special_use`
            // would project as `role: null` on the wire — the storage
            // barrier is authoritative. Holds for both `Reject` (an
            // empty role mailbox would otherwise pass the count check)
            // and `onDestroyRemoveEmails: true` (which would otherwise
            // reach `Delete` / `MoveToInbox`). \Inbox / \Sent / \Trash
            // etc. are operationally load-bearing — `MoveToInbox`
            // itself looks up `\Inbox` by `attrs.special_use`, so
            // destroying it here would silently corrupt subsequent
            // destroys on other mailboxes. Clients clear `special_use`
            // via `Mailbox/set update` first if they genuinely want
            // to destroy.
            if let Some(special_use_value) = special_use.as_deref() {
                return Err(cosmix_mds::Error::Other(format!(
                    "mailbox has role: refusing to destroy mailbox \
                     with attrs.special_use {special_use_value:?}; \
                     clear special_use via Mailbox/set update first"
                )));
            }
            match on_emails {
                DestroyPolicy::Reject => {
                    // Authoritative live count over `membership`, not
                    // the denormalized `container.exists_count`. The
                    // counter is maintained by `allocate_seq_and_change`
                    // / `bump_change_seq` and is correct in practice,
                    // but as a *gating* predicate for a destructive
                    // action we want truth from the indexed table that
                    // the cascade actually keys off.
                    let n: i64 = tx
                        .tx()
                        .query_row(
                            "SELECT COUNT(*) FROM membership WHERE container_id = ?1",
                            params![id.0.to_string()],
                            |r| r.get(0),
                        )
                        .map_err(|e| cosmix_mds::Error::Other(format!("count memberships: {e}")))?;
                    if n > 0 {
                        return Err(cosmix_mds::Error::Other(format!(
                            "mailbox not empty: {n} item(s) remain"
                        )));
                    }
                }
                DestroyPolicy::MoveToInbox => {
                    let inbox = lookup_special_use_container(tx.tx(), SPECIAL_USE_INBOX)?
                        .ok_or_else(|| {
                            cosmix_mds::Error::Other(
                                "mailbox has no inbox: account has no \
                                 \\Inbox special-use container"
                                    .into(),
                            )
                        })?;
                    if inbox == id {
                        return Err(cosmix_mds::Error::Other(
                            "mailbox conflicting policy: cannot move-to-inbox \
                             when destroying inbox itself"
                                .into(),
                        ));
                    }
                    let item_ids = collect_items_in_container(tx.tx(), &id)?;
                    for item in item_ids {
                        // Multi-membership case: the same item may be
                        // in both the source and the inbox already.
                        // `move_item_in_tx` rejects "already in
                        // destination", so fall back to dropping just
                        // the source membership for that case.
                        let already_in_inbox = membership_exists_tx(tx.tx(), &item, &inbox)?;
                        if already_in_inbox {
                            tx.remove_membership(&item, &id)?;
                            // Phase 8e gate 2: the source-membership
                            // drop may have been the item's last,
                            // dropping the item row too. Reap FTS.
                            reap_mail_search_if_item_gone(tx.tx(), &item)?;
                        } else {
                            tx.move_item(&item, &id, &inbox, Flags(0))?;
                        }
                    }
                }
                DestroyPolicy::Delete => {
                    let item_ids = collect_items_in_container(tx.tx(), &id)?;
                    for item in item_ids {
                        // `remove_membership` drops the cross-DB
                        // blob_ref and the item row when this was the
                        // last membership — items with other
                        // memberships survive in those mailboxes.
                        tx.remove_membership(&item, &id)?;
                        // Phase 8e gate 2: reap FTS for any item
                        // whose last membership was just removed.
                        // No-op when the item survives elsewhere.
                        reap_mail_search_if_item_gone(tx.tx(), &item)?;
                    }
                }
            }
            // Final delete. Children-still-present surfaces as
            // Error::Other("container has child containers; ...") via
            // the cascade rule; we map it at the trait boundary below.
            tx.delete_container(&id)?;
            Ok(())
        }) {
            Ok(()) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = id.0))
            }
            Err(cosmix_mds::Error::ContainerNotFound(missing)) => {
                Err(anyhow!("mailbox not found: {missing}"))
            }
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("container has child containers") => {
                Err(anyhow!("mailbox has children: {s}"))
            }
            // Pre-shaped messages from inside the closure carry the
            // intended trait-prefix vocabulary already; pass through
            // verbatim so JMAP error mapping sees a single form.
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("mailbox not empty")
                    || s.starts_with("mailbox has no inbox")
                    || s.starts_with("mailbox conflicting policy")
                    || s.starts_with("mailbox has role")
                    || s.starts_with("reserved container") =>
            {
                Err(anyhow!(s))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    #[allow(clippy::too_many_arguments)] // matches trait surface
    fn update_mailbox(
        &self,
        account: AccountId,
        id: ContainerId,
        new_name: Option<&str>,
        new_parent: Option<Option<ContainerId>>,
        role: Option<Option<MailboxRole>>,
        sort_order: Option<u32>,
        is_subscribed: Option<bool>,
    ) -> Result<()> {
        // Name-shape guards run BEFORE the tx so a probe with a reserved
        // or empty rename target can't even open a per-set tx (mirrors
        // `create_mailbox` / `rename_mailbox`).
        if let Some(n) = new_name {
            if n == UPLOAD_STAGING_NAME {
                return Err(anyhow!(
                    "reserved name {n:?}: collides with upload-staging container"
                ));
            }
            if n.is_empty() {
                return Err(anyhow!("mailbox invalid name: must be non-empty"));
            }
        }
        // Like `rename_mailbox`/`destroy_mailbox`: derive deterministically,
        // never bootstrap empty MDS state on an error path.
        let set = account_id_to_setid(account);
        let attrs_change_requested =
            role.is_some() || sort_order.is_some() || is_subscribed.is_some();
        let rename_requested = new_name.is_some() || new_parent.is_some();
        // No-op if the patch supplied nothing — skip the tx so a
        // `Mailbox/set update` with no recognised fields doesn't pay
        // for a redundant per-set round-trip. Documented all-None
        // contract.
        if !rename_requested && !attrs_change_requested {
            return Ok(());
        }
        match self.mds.with_set_tx(&set, |tx| {
            let id_str = id.0.to_string();
            // Single read covers both the existence check (used to
            // produce ContainerNotFound on a bogus id before any
            // mutation runs) and the current name + parent the rename
            // helper needs when the patch only supplies one of them.
            // The attrs blob is fetched in the same row so the impl
            // doesn't pay for a second SELECT in the typical mixed
            // patch case.
            let row: Option<(String, Option<String>, String)> = tx
                .tx()
                .query_row(
                    "SELECT name, parent_id, attrs FROM container WHERE id = ?1",
                    rusqlite::params![id_str],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| cosmix_mds::Error::Other(format!("read container row: {e}")))?;
            let (cur_name, cur_parent_str, cur_attrs_raw) = match row {
                Some(t) => t,
                None => return Err(cosmix_mds::Error::ContainerNotFound(id_str)),
            };

            // Rename-or-reparent first so the typed
            // `tx.rename_container` runs its cycle / collision /
            // already-exists checks before the attrs UPDATE — no point
            // re-encoding attrs if the rename will fail.
            if rename_requested {
                let target_name = new_name.unwrap_or(cur_name.as_str());
                let target_parent: Option<ContainerId> = match new_parent {
                    Some(opt) => opt,
                    None => match cur_parent_str.as_deref() {
                        Some(s) => Some(ContainerId(uuid::Uuid::parse_str(s).map_err(|e| {
                            cosmix_mds::Error::Other(format!("decode container parent: {e}"))
                        })?)),
                        None => None,
                    },
                };
                tx.rename_container(&id, target_parent.as_ref(), target_name)?;
            }

            // Attrs update. Decode the snapshot we already SELECTed,
            // patch the requested fields, then hand the full new
            // attrs blob to the substrate's typed
            // `update_container_attrs` so the diff + UPDATE +
            // `container_change_set` row all happen atomically
            // through the single substrate write surface (MCS-P1-D
            // cut over the legacy raw UPDATE on this site —
            // bypassing the typed method would silently defeat the
            // JMAP `Mailbox/changes` lifecycle-stream invariant; see
            // `_doc/planned/mailbox-changes-substrate.md` §"Trait
            // surface change").
            if attrs_change_requested {
                let mut attrs: ContainerAttrs =
                    serde_json::from_str(&cur_attrs_raw).map_err(|e| {
                        cosmix_mds::Error::Other(format!("decode container attrs: {e}"))
                    })?;
                if let Some(r_opt) = role {
                    attrs.special_use = r_opt.map(|r| r.as_special_use().to_string());
                }
                if let Some(so) = sort_order {
                    let mut extra_map = match attrs.extra {
                        serde_json::Value::Object(m) => m,
                        _ => serde_json::Map::new(),
                    };
                    if so == 0 {
                        extra_map.remove(JMAP_SORT_ORDER_KEY);
                    } else {
                        extra_map.insert(JMAP_SORT_ORDER_KEY.to_string(), so.into());
                    }
                    attrs.extra = serde_json::Value::Object(extra_map);
                }
                if let Some(sub) = is_subscribed {
                    attrs.subscribed = sub;
                }
                // No-op patches (e.g. JMAP `Mailbox/set update` that
                // resends the current role) collapse cleanly:
                // `update_container_attrs` returns empty
                // `changed_props`, no SQL UPDATE runs, and no
                // lifecycle row is written. The maild caller does
                // not need to discriminate — the legacy code
                // unconditionally UPDATE'd even on byte-identical
                // attrs, which the substrate now suppresses.
                tx.update_container_attrs(&id, &attrs)?;
            }
            Ok(())
        }) {
            Ok(()) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = id.0))
            }
            Err(cosmix_mds::Error::ContainerNotFound(missing)) => {
                // `rename_container_in_tx` surfaces both source-missing
                // and parent-missing as ContainerNotFound. Disambiguate
                // by id — same disambiguation `rename_mailbox` uses.
                if missing == id.0.to_string() {
                    Err(anyhow!("mailbox not found: {missing}"))
                } else if let Some(Some(p)) = new_parent
                    && missing == p.0.to_string()
                {
                    Err(anyhow!("mailbox parent not found: {missing}"))
                } else {
                    Err(anyhow!("mailbox not found: {missing}"))
                }
            }
            Err(cosmix_mds::Error::ContainerAlreadyExists { name, .. }) => {
                Err(anyhow!("mailbox already exists: {name}"))
            }
            Err(cosmix_mds::Error::Other(s))
                if s.contains("would form a parent cycle")
                    || s.contains("cannot be its own parent") =>
            {
                Err(anyhow!("mailbox cycle: {s}"))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn list_mailboxes(&self, account: AccountId) -> Result<Vec<MailboxRecord>> {
        let set = account_id_to_setid(account);
        // Brand-new accounts have no set yet — map `SetNotFound` to
        // `Ok(vec![])` so the JMAP `Mailbox/get` handler does not have
        // to special-case it. One read, no global O(N) set scan.
        let containers = match self.mds.list_containers(&set) {
            Ok(v) => v,
            Err(cosmix_mds::Error::SetNotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!(e)),
        };
        let mut out = Vec::with_capacity(containers.len());
        for ci in containers {
            // The per-account upload-staging container is an internal
            // landing zone for `create_blob_ref`; JMAP must never
            // expose it as a mailbox.
            if ci.name == UPLOAD_STAGING_NAME {
                continue;
            }
            // Eventual consistency: if the container is destroyed
            // between `list_containers` and `container_status`, drop
            // the row rather than fail the whole listing — see trait
            // doc for the snapshot-consistency contract.
            let status = match self.mds.container_status(&set, &ci.id) {
                Ok(s) => s,
                Err(cosmix_mds::Error::ContainerNotFound(_)) => continue,
                Err(e) => return Err(anyhow!(e)),
            };
            let role = ci
                .attrs
                .special_use
                .as_deref()
                .and_then(MailboxRole::from_special_use);
            let sort_order = jmap_sort_order_from_attrs(&ci.attrs);
            let is_subscribed = ci.attrs.subscribed;
            let my_rights = jmap_my_rights_from_attrs(&ci.attrs);
            out.push(MailboxRecord {
                id: ci.id,
                name: ci.name,
                parent: ci.parent,
                attrs: ci.attrs,
                total_emails: status.exists,
                unread_emails: status.unread,
                role,
                sort_order,
                is_subscribed,
                my_rights,
                uid_validity: status.seq_validity,
                // `container.change_seq` is the IMAP HIGHESTMODSEQ
                // source. See P6 in `_doc/planned/imap-runway.md` for
                // why `MAX(membership.change_seq)` is wrong.
                highest_mod_seq: status.change_seq.0,
                // `container.next_seq` is the IMAP UIDNEXT source.
                // Read inside the same `container_status` call above
                // so it's snapshot-consistent with EXISTS / HIGHESTMODSEQ
                // for the same container (no second mds round-trip).
                uid_next: u64::from(status.next_seq.0),
            });
        }
        Ok(out)
    }

    fn move_message(
        &self,
        account: AccountId,
        item_id: ItemId,
        src: ContainerId,
        dest: ContainerId,
        flags: Flags,
        stamp_id: &str,
    ) -> Result<(MembershipUid, MembershipUid)> {
        let set = account_id_to_setid(account);
        let stamp_id = stamp_id.to_string();
        self.mds
            .with_set_tx(&set, |tx| {
                let report = tx.move_item(&item_id, &src, &dest, flags)?;

                // Junk-boundary detection. Look up the per-account
                // `\Junk` container by `attrs.special_use` inside the
                // same tx so the retrain enqueue is consistent with
                // any concurrent container metadata edit.
                //
                // `INSERT OR REPLACE` (not `OR IGNORE`): the
                // `retrain.rs` drain worker is deferred, so the
                // outbox must record the *latest* drag for a
                // `(stamp, label)`, not the first. With `OR IGNORE`,
                // an In→Junk that follows a still-undrained
                // In→Junk/Junk→In pair would be silently dropped and
                // the worker — replaying rows in `rowid` order into
                // `record_label`'s per-stamp latest-wins state —
                // would converge to the wrong final corpus label.
                // `OR REPLACE` mints a fresh rowid for the re-drag so
                // it sorts after the intervening opposite-label row;
                // a same-label re-drag still collapses (idempotent in
                // `record_label`).
                let junk_id = lookup_special_use_container(tx.tx(), SPECIAL_USE_JUNK)?;
                if let Some(junk) = junk_id {
                    let from_junk = src == junk;
                    let to_junk = dest == junk;
                    if from_junk ^ to_junk {
                        let label = if to_junk { "junk" } else { "ham" };
                        let now = chrono::Utc::now().timestamp();
                        tx.tx()
                            .execute(
                                "INSERT OR REPLACE INTO mail_retrain_outbox \
                                 (stamp_id, account_id, item_id, label, attempts, last_error, created_at) \
                                 VALUES (?1, ?2, ?3, ?4, 0, NULL, ?5)",
                                params![
                                    stamp_id,
                                    account,
                                    item_id.0.to_string(),
                                    label,
                                    now,
                                ],
                            )
                            .map_err(|e| {
                                cosmix_mds::Error::Other(format!(
                                    "insert mail_retrain_outbox row: {e}"
                                ))
                            })?;
                    }
                }

                // Read each container's seq_validity in the same tx
                // that produced the move, so the returned
                // (uid, uid_validity) pairs are read-consistent with
                // the source row's pre-removal seq and the dest row's
                // post-insert seq — surfaces UIDPLUS COPYUID/MOVE
                // return data without a second mds round-trip.
                let src_uid_validity = tx.container_seq_validity(&src)?;
                let dest_uid_validity = tx.container_seq_validity(&dest)?;
                let src_uid = MembershipUid {
                    container_id: src,
                    uid: report.seq_src.0 as u64,
                    uid_validity: src_uid_validity,
                };
                let dest_uid = MembershipUid {
                    container_id: dest,
                    uid: report.seq_dest.0 as u64,
                    uid_validity: dest_uid_validity,
                };
                Ok((src_uid, dest_uid))
            })
            .map_err(|e| anyhow!(e))
    }

    fn copy_message(
        &self,
        account: AccountId,
        item_id: ItemId,
        src: ContainerId,
        dest: ContainerId,
        stamp_id: &str,
        read_only_src: bool,
    ) -> Result<MembershipUid> {
        if src == dest {
            return Err(anyhow!("copy_message: src and dest must differ"));
        }
        let set = account_id_to_setid(account);
        let stamp_id = stamp_id.to_string();
        let result = self.mds.with_set_tx(&set, |tx| {
            // `add_membership_from` inherits keywords + tags from the
            // `src` membership specifically — RFC 9051 §6.4.7 wants
            // the destination to preserve the source mailbox's flags,
            // and the IMAP §3.2 per-mailbox flag model lets sibling
            // memberships diverge, so source-specific inheritance is
            // load-bearing here. An absent `(item_id, src)` membership
            // is a hard error, not a silent Flags(0) fallback: a
            // concurrent EXPUNGE/MOVE that snapshot-dropped `src`'s
            // row between IMAP snapshot and this tx must fail the
            // COPY, not mint a dest membership with the wrong flags.
            let placement = tx
                .add_membership_from(&item_id, &src, &dest)
                .map_err(|e| match e {
                    cosmix_mds::Error::ItemNotFound(_) => cosmix_mds::Error::Other(format!(
                        "email not found: {id}",
                        id = item_id.0
                    )),
                    cosmix_mds::Error::ContainerNotFound(c) => {
                        cosmix_mds::Error::Other(format!("mailbox not found: {c}"))
                    }
                    cosmix_mds::Error::Other(ref s)
                        if s.starts_with("item ") && s.contains("not in source container ") =>
                    {
                        cosmix_mds::Error::Other(format!(
                            "email not found: {id}",
                            id = item_id.0
                        ))
                    }
                    other => other,
                })?;

            // Junk-boundary detection — copying TO Junk trains spam,
            // FROM Junk trains ham. Same shape as `move_message` so
            // a COPY into Junk has identical retraining semantics
            // to a MOVE into Junk. Look up the per-account `\Junk`
            // container inside the same tx so the enqueue is
            // consistent with concurrent metadata edits.
            //
            // `read_only_src` (EXAMINE'd source) suppresses the
            // enqueue: a read-only source mailbox must not have a
            // side-channel effect on the classifier.
            //
            // `INSERT OR REPLACE` (not `OR IGNORE`), matching
            // `move_message`: the `retrain.rs` drain worker is
            // deferred, so the outbox must record the *latest* drag
            // for a `(stamp, label)`. `OR REPLACE` mints a fresh
            // rowid so a re-drag sorts after any intervening
            // opposite-label row, and the worker — replaying rows in
            // `rowid` order into `record_label`'s per-stamp
            // latest-wins state — converges to the correct final
            // corpus label. A same-label re-drag still collapses
            // (idempotent in `record_label`).
            let junk_id = if read_only_src {
                None
            } else {
                lookup_special_use_container(tx.tx(), SPECIAL_USE_JUNK)?
            };
            if let Some(junk) = junk_id {
                let from_junk = src == junk;
                let to_junk = dest == junk;
                if from_junk ^ to_junk {
                    let label = if to_junk { "junk" } else { "ham" };
                    let now = chrono::Utc::now().timestamp();
                    tx.tx()
                        .execute(
                            "INSERT OR REPLACE INTO mail_retrain_outbox \
                             (stamp_id, account_id, item_id, label, attempts, last_error, created_at) \
                             VALUES (?1, ?2, ?3, ?4, 0, NULL, ?5)",
                            params![
                                stamp_id,
                                account,
                                item_id.0.to_string(),
                                label,
                                now,
                            ],
                        )
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "insert mail_retrain_outbox row: {e}"
                            ))
                        })?;
                }
            }

            let uid_validity = tx.container_seq_validity(&dest)?;
            Ok(MembershipUid {
                container_id: dest,
                uid: placement.seq.0 as u64,
                uid_validity,
            })
        });

        match result {
            Ok(uid) => Ok(uid),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("email not found: {id}", id = item_id.0))
            }
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("email not found")
                    || s.starts_with("mailbox not found")
                    || s.starts_with("item ") =>
            {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn expunge_memberships(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        item_ids: &[ItemId],
    ) -> Result<usize> {
        if item_ids.is_empty() {
            // Empty input MUST NOT open a tx — non-negotiable for
            // the change-stream contract (SPEC: empty fan-out must
            // not allocate a set_change row). Mirrors
            // `add_memberships`'s short-circuit for the same reason.
            return Ok(0);
        }
        let item_ids: Vec<ItemId> = item_ids.to_vec();
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            let mut removed = 0usize;
            for id in &item_ids {
                match tx.remove_membership(id, &mailbox) {
                    Ok(()) => {
                        removed += 1;
                        // Phase 8e gate 2: if the just-removed
                        // membership was the item's last, the
                        // underlying `item` row is gone; reap its
                        // FTS row in the same tx. No-op when the
                        // item still has memberships elsewhere.
                        reap_mail_search_if_item_gone(tx.tx(), id)?;
                    }
                    // Concurrent EXPUNGE in another session: the
                    // membership is already gone. The MDS layer
                    // surfaces this as `Error::Other` with a
                    // "membership ... not present" string rather
                    // than a typed `MembershipNotFound`, so we
                    // match the prefix. Skip silently — the
                    // caller's wire emission is driven by the
                    // pre-snapshot, not this count.
                    Err(cosmix_mds::Error::ItemNotFound(_)) => continue,
                    Err(cosmix_mds::Error::Other(ref s))
                        if s.starts_with("membership ") && s.contains("not present") =>
                    {
                        continue;
                    }
                    Err(cosmix_mds::Error::ContainerNotFound(c)) => {
                        return Err(cosmix_mds::Error::Other(format!("mailbox not found: {c}")));
                    }
                    Err(other) => return Err(other),
                }
            }
            Ok(removed)
        });

        match result {
            Ok(n) => Ok(n),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("mailbox not found") => {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn list_emails_in_mailbox(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        opts: ListOpts,
    ) -> Result<Vec<EmailHandle>> {
        // Pure-derivation: never bootstrap. SetNotFound below maps to
        // "mailbox not found" (account has no MDS state, hence no
        // such mailbox).
        let set = account_id_to_setid(account);

        // Pre-check existence so a missing mailbox surfaces as a
        // distinct error: `Mds::list_items` returns an empty Vec on
        // an unknown container_id (the JOIN simply has no matching
        // rows), and we want JMAP's `inMailbox` filter to be able to
        // tell "empty" from "no such mailbox". This runs even for
        // `limit == 0` so a "count probe" over a missing mailbox
        // surfaces the error rather than masquerading as zero rows.
        match self.mds.container_status(&set, &mailbox) {
            Ok(_) => {}
            Err(cosmix_mds::Error::SetNotFound(_))
            | Err(cosmix_mds::Error::ContainerNotFound(_)) => {
                return Err(anyhow!("mailbox not found: {id}", id = mailbox.0));
            }
            Err(e) => return Err(anyhow!(e)),
        }

        // Caller asked for zero rows after the mailbox is confirmed
        // to exist. Skip the read.
        if opts.limit == 0 {
            return Ok(Vec::new());
        }

        let items = self
            .mds
            .list_items(&set, &mailbox, SeqRange::All)
            .map_err(|e| anyhow!(e))?;

        let mut handles: Vec<EmailHandle> = items
            .into_iter()
            .map(|r| EmailHandle {
                id: r.id,
                blob_hash: r.meta.blob_hash,
                keywords: r.flags,
                // `ItemRecord.tags` is the per-membership user-keyword
                // set already decoded from `membership.tags` by
                // `list_items`; `Tags: From<Vec<String>>` sorts + dedups.
                tags: r.tags.into(),
                received_at: r.meta.received_at,
                seq: r.seq,
                // `r.seq` is `Seq(u32)`; widen to u64 for the IMAP
                // wire layer's UID surface.
                uid: r.seq.0 as u64,
                // Per-membership MODSEQ comes directly from
                // `membership.change_seq` projected into
                // `ItemRecord.change_seq`. No extra round-trip.
                mod_seq: r.change_seq.0,
                size_bytes: r.meta.size_bytes,
            })
            .collect();

        match opts.sort_by {
            // Tie-break on `seq` to keep ordering deterministic when
            // multiple items share a `received_at` (e.g. SMTP burst
            // delivery into the same mailbox in one second).
            SortKey::ReceivedAt => {
                handles.sort_by(|a, b| {
                    a.received_at
                        .cmp(&b.received_at)
                        .then(a.seq.0.cmp(&b.seq.0))
                });
            }
            // `Mds::list_items` already orders by `m.seq`, so this is
            // a no-op in practice. Doing it explicitly keeps the
            // contract independent of the underlying order.
            SortKey::Seq => handles.sort_by_key(|a| a.seq.0),
        }

        let start = opts.offset as usize;
        if start >= handles.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(opts.limit as usize).min(handles.len());
        Ok(handles[start..end].to_vec())
    }

    fn query_emails(
        &self,
        account: AccountId,
        filter: EmailQueryFilter,
        sort: EmailSort,
        opts: ListOpts,
    ) -> Result<Vec<ItemId>> {
        let set = account_id_to_setid(account);

        // Step 1: candidate containers. `in_mailbox: Some(X)` validates
        // X exists (so `Email/query inMailbox: <unknown>` surfaces a
        // distinct error). `in_mailbox: None` walks every container in
        // the account's set, excluding `__upload_staging__` per the
        // same rule `list_mailboxes` enforces.
        let candidate_containers: Vec<ContainerId> = if let Some(mb) = filter.in_mailbox {
            match self.mds.container_status(&set, &mb) {
                Ok(_) => vec![mb],
                Err(cosmix_mds::Error::SetNotFound(_))
                | Err(cosmix_mds::Error::ContainerNotFound(_)) => {
                    return Err(anyhow!("mailbox not found: {id}", id = mb.0));
                }
                Err(e) => return Err(anyhow!(e)),
            }
        } else {
            match self.mds.list_containers(&set) {
                Ok(cs) => cs
                    .into_iter()
                    .filter(|c| c.name != UPLOAD_STAGING_NAME)
                    .map(|c| c.id)
                    .collect(),
                // Unprovisioned account + no `in_mailbox` filter is the
                // moral equivalent of "no mailboxes at all" — return
                // empty rather than error. JMAP `Email/query` over a
                // brand-new account should give `[]`, not blow up.
                Err(cosmix_mds::Error::SetNotFound(_)) => return Ok(Vec::new()),
                Err(e) => return Err(anyhow!(e)),
            }
        };

        // Caller asked for zero rows. The mailbox-existence pre-check
        // above has already run for the `in_mailbox: Some(_)` case.
        if opts.limit == 0 {
            return Ok(Vec::new());
        }

        // Step 2: gather candidate item ids with received_at +
        // size_bytes from `list_items`. Dedupe by ItemId — an email
        // with N memberships shows up N times across container walks
        // but its item-level metadata is invariant across them.
        use std::collections::{HashMap, HashSet};
        let mut candidates: HashMap<ItemId, (i64, u64)> = HashMap::new();
        for cid in &candidate_containers {
            let rows = match self.mds.list_items(&set, cid, SeqRange::All) {
                Ok(v) => v,
                // Walk-all branch: a container destroyed mid-iteration
                // simply contributes nothing. `in_mailbox: Some(_)` was
                // already validated above, so this can only fire on
                // the unfiltered path.
                Err(cosmix_mds::Error::ContainerNotFound(_)) => continue,
                Err(e) => return Err(anyhow!(e)),
            };
            for r in rows {
                candidates
                    .entry(r.id)
                    .or_insert((r.meta.received_at, r.meta.size_bytes));
            }
        }

        // Step 3: filter. Time + size predicates use only per-row
        // metadata; membership predicates need a follow-up
        // `item_memberships` read (one per surviving candidate after
        // the cheap filters).
        let other_set: HashSet<ContainerId> = filter
            .in_mailbox_other_than
            .as_ref()
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let need_memberships = filter.in_mailbox_other_than.is_some()
            || filter.has_keyword.is_some()
            || filter.not_keyword.is_some()
            || filter.has_tag.is_some()
            || filter.not_tag.is_some();

        // `text` full-text predicate: resolve the set-wide FTS match set ONCE
        // (a single `mail_search` MATCH), then intersect it per-candidate below
        // — so `text` ANDs with `in_mailbox` and the keyword predicates. A blank
        // / punctuation-only needle yields an empty set → no rows survive.
        let text_matches: Option<HashSet<ItemId>> = match filter.text.as_deref() {
            Some(needle) => Some(self.mds.search_items(&set, needle)?.into_iter().collect()),
            None => None,
        };

        let mut survivors: Vec<(ItemId, i64)> = Vec::with_capacity(candidates.len());
        for (id, (received_at, size_bytes)) in candidates {
            if let Some(b) = filter.before
                && received_at >= b
            {
                continue;
            }
            if let Some(a) = filter.after
                && received_at < a
            {
                continue;
            }
            if let Some(min) = filter.min_size
                && size_bytes < min
            {
                continue;
            }
            if let Some(max) = filter.max_size
                && size_bytes > max
            {
                continue;
            }
            // `text` predicate: drop any candidate not in the FTS match set.
            if let Some(tm) = &text_matches
                && !tm.contains(&id)
            {
                continue;
            }

            if need_memberships {
                let memberships = match self.mds.item_memberships(&set, &id) {
                    Ok(v) => v,
                    // Item destroyed between the candidate gather and
                    // here — drop silently. `ItemNotFound` is not
                    // emitted by `item_memberships` (it returns an
                    // empty Vec for unknown ids per the trait doc),
                    // so this branch handles the SetNotFound shape and
                    // any future tightening of the contract.
                    Err(cosmix_mds::Error::SetNotFound(_))
                    | Err(cosmix_mds::Error::ItemNotFound(_)) => continue,
                    Err(e) => return Err(anyhow!(e)),
                };

                // An item with zero memberships is either destroyed
                // mid-walk (returns an empty Vec per the trait doc) or
                // unmoored. Either way it cannot satisfy a membership-
                // sensitive filter — in particular `not_keyword` would
                // otherwise leak ghost rows since `0 & mask != 0` is
                // always false.
                if memberships.is_empty() {
                    continue;
                }

                if filter.in_mailbox_other_than.is_some() {
                    // RFC 8621 §4.4.1: "the email has at least one
                    // mailbox not in the list". An item with all
                    // memberships inside `other_set` fails the filter.
                    // An item with zero memberships also fails (it is
                    // not "in" any mailbox other than the listed ones
                    // because it is not in any mailbox at all).
                    let has_other = memberships.iter().any(|(c, _, _)| !other_set.contains(c));
                    if !has_other {
                        continue;
                    }
                }

                if filter.has_keyword.is_some() || filter.not_keyword.is_some() {
                    let union: u32 = memberships
                        .iter()
                        .map(|(_, f, _)| f.0)
                        .fold(0u32, |a, b| a | b);
                    if let Some(hk) = filter.has_keyword
                        && union & hk.0 == 0
                    {
                        continue;
                    }
                    if let Some(nk) = filter.not_keyword
                        && union & nk.0 != 0
                    {
                        continue;
                    }
                }

                if filter.has_tag.is_some() || filter.not_tag.is_some() {
                    // User-keyword predicate: post-filter against the
                    // union of per-membership `Tags` (no mds pushdown
                    // for tags — the membership rows are already in
                    // hand). Mirrors the `has_keyword`/`not_keyword`
                    // union semantics above. An item with zero
                    // memberships was already dropped, so a `not_tag`
                    // here cannot leak a ghost row.
                    let tag_match =
                        |needle: &str| memberships.iter().any(|(_, _, t)| t.contains(needle));
                    if let Some(ht) = filter.has_tag.as_deref()
                        && !tag_match(ht)
                    {
                        continue;
                    }
                    if let Some(nt) = filter.not_tag.as_deref()
                        && tag_match(nt)
                    {
                        continue;
                    }
                }
            }

            survivors.push((id, received_at));
        }

        // Step 4: sort + paginate. Tie-break on ItemId so paginated
        // cursor positions are stable when multiple items share a
        // received_at (e.g. SMTP burst delivery in the same second).
        // The tiebreak mirrors the primary direction so that a client
        // flipping `EmailSort` sees a fully reversed list within a
        // tied group, not a partially-reversed one.
        match sort {
            EmailSort::ReceivedAtDesc => {
                survivors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.0.cmp(&a.0.0)));
            }
            EmailSort::ReceivedAtAsc => {
                survivors.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.0.cmp(&b.0.0)));
            }
            EmailSort::SubjectAsc
            | EmailSort::SubjectDesc
            | EmailSort::FromAsc
            | EmailSort::FromDesc => {
                // Envelope-keyed sort (Phase B): pull the per-item sort key
                // (RFC base subject, or the lowercased From address) from the
                // `mail_envelopes` sidecar for the WHOLE filtered set — JMAP
                // sorts before paginating. One prepared statement, reused per
                // survivor. A missing envelope row (an item mid-delete) keys to
                // the empty string rather than failing the whole query.
                //
                // Cost: O(survivors) indexed lookups on `mail_envelopes`
                // (item_id is the row key), only when the client actually sorts
                // by subject/from (the default receivedAt path stays key-free).
                // This scales with the RESULT set, deliberately preferred over a
                // single `SELECT … FROM mail_envelopes` that would load the whole
                // account's envelopes (O(total), worse when survivors << total)
                // or an `IN (…)` batch that would need chunking around SQLite's
                // bound-parameter cap. If a pathological large-result subject/
                // from sort ever becomes hot, batch the lookups then.
                let want_subject = matches!(sort, EmailSort::SubjectAsc | EmailSort::SubjectDesc);
                let mut keys: HashMap<ItemId, String> = HashMap::with_capacity(survivors.len());
                self.mds
                    .with_set_tx(&set, |tx| {
                        let mut stmt = tx
                            .tx()
                            .prepare(
                                "SELECT subject, from_addr \
                                 FROM mail_envelopes WHERE item_id = ?1",
                            )
                            .map_err(|e| {
                                cosmix_mds::Error::Other(format!("prepare envelope sort: {e}"))
                            })?;
                        for (id, _) in &survivors {
                            let row = stmt
                                .query_row(params![id.0.to_string()], |r| {
                                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                                })
                                .optional()
                                .map_err(|e| {
                                    cosmix_mds::Error::Other(format!("envelope sort read: {e}"))
                                })?;
                            let key = match row {
                                Some((subj, from_addr)) => {
                                    if want_subject {
                                        subject::normalize_subject(&subj)
                                    } else {
                                        from_addr.to_lowercase()
                                    }
                                }
                                None => String::new(),
                            };
                            keys.insert(*id, key);
                        }
                        Ok(())
                    })
                    .map_err(|e| anyhow!(e))?;
                let blank = String::new();
                let ascending = matches!(sort, EmailSort::SubjectAsc | EmailSort::FromAsc);
                survivors.sort_by(|a, b| {
                    let ka = keys.get(&a.0).unwrap_or(&blank);
                    let kb = keys.get(&b.0).unwrap_or(&blank);
                    // Tiebreak within an equal key: newest received first,
                    // then ItemId. Reverse the whole ordering for descending
                    // so a direction flip fully reverses the list.
                    let ord = ka
                        .cmp(kb)
                        .then_with(|| b.1.cmp(&a.1))
                        .then_with(|| a.0.0.cmp(&b.0.0));
                    if ascending { ord } else { ord.reverse() }
                });
            }
        }

        let start = opts.offset as usize;
        if start >= survivors.len() {
            return Ok(Vec::new());
        }
        let end = start
            .saturating_add(opts.limit as usize)
            .min(survivors.len());
        Ok(survivors[start..end].iter().map(|(id, _)| *id).collect())
    }

    fn get_email(&self, account: AccountId, id: ItemId) -> Result<EmailRecord> {
        // Pure-derivation: never bootstrap. Hitting `SetNotFound`
        // means the account has no MDS state at all, which is the
        // same observable shape (from JMAP's view) as "no such item":
        // an `Email/get` with this id finds nothing. Both surface as
        // `"email not found"`.
        let set = account_id_to_setid(account);

        // Pull item-level fields outside the envelope sidecar tx so a
        // missing item surfaces `"email not found"` even if mds and
        // mailstore-side reads ever drift (e.g. an envelope row
        // orphaned by a manual surgery scenario; the FK ON DELETE
        // CASCADE prevents this in normal operation but defensive
        // ordering is cheap).
        let meta = match self.mds.fetch_item_meta(&set, &id) {
            Ok(m) => m,
            Err(cosmix_mds::Error::SetNotFound(_)) | Err(cosmix_mds::Error::ItemNotFound(_)) => {
                return Err(anyhow!("email not found: {id}", id = id.0));
            }
            Err(e) => return Err(anyhow!(e)),
        };

        let memberships = self
            .mds
            .item_memberships(&set, &id)
            .map_err(|e| anyhow!(e))?;
        // Union flags AND user keywords (`Tags`) across memberships
        // per RFC 8621 §4.1.4 ("`keywords` is the union of the
        // per-mailbox keywords"). `Flags` is a bit-set u32 wrapper
        // (OR = union); `Tags` is a string set (insert = union). The
        // JMAP layer merges the two into one wire `keywords` object.
        let mut keywords_acc: u32 = 0;
        let mut tags_acc = Tags::new();
        let mut mailbox_ids: Vec<ContainerId> = Vec::with_capacity(memberships.len());
        for (cid, flags, tags) in memberships {
            keywords_acc |= flags.0;
            for t in tags.0 {
                tags_acc.insert(t);
            }
            mailbox_ids.push(cid);
        }

        let envelope =
            self.mds
                .with_set_tx(&set, |tx| {
                    let row = tx
                        .tx()
                        .query_row(
                            "SELECT from_addr, to_addrs, cc_addrs, bcc_addrs, \
                                reply_to_addrs, subject, date_ts, message_id \
                         FROM mail_envelopes WHERE item_id = ?1",
                            params![id.0.to_string()],
                            |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, String>(3)?,
                                    r.get::<_, String>(4)?,
                                    r.get::<_, String>(5)?,
                                    r.get::<_, i64>(6)?,
                                    r.get::<_, Option<String>>(7)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!("read mail_envelopes: {e}"))
                        })?;
                    let (
                        from,
                        to_json,
                        cc_json,
                        bcc_json,
                        reply_to_json,
                        subject,
                        date_ts,
                        message_id,
                    ) = match row {
                        Some(t) => t,
                        None => {
                            // Pre-shaped sentinel — the trait-level mapper
                            // below recognises this prefix and forwards it
                            // to the JMAP layer untouched.
                            return Err(cosmix_mds::Error::Other(format!(
                                "email envelope missing: {id}",
                                id = id.0
                            )));
                        }
                    };
                    let parse_addr_array =
                        |label: &str, json: &str| -> Result<Vec<String>, cosmix_mds::Error> {
                            serde_json::from_str(json).map_err(|e| {
                                cosmix_mds::Error::Other(format!(
                                    "mail_envelopes.{label} is not a JSON array: {e}"
                                ))
                            })
                        };
                    let to = parse_addr_array("to_addrs", &to_json)?;
                    let cc = parse_addr_array("cc_addrs", &cc_json)?;
                    let bcc = parse_addr_array("bcc_addrs", &bcc_json)?;
                    let reply_to = parse_addr_array("reply_to_addrs", &reply_to_json)?;
                    Ok(EmailEnvelope {
                        from,
                        to,
                        cc,
                        bcc,
                        reply_to,
                        subject,
                        date: date_ts,
                        message_id,
                    })
                })
                .map_err(|e| match e {
                    cosmix_mds::Error::Other(s) if s.starts_with("email envelope missing") => {
                        anyhow!(s)
                    }
                    other => anyhow!(other),
                })?;

        Ok(EmailRecord {
            id,
            blob_hash: meta.blob_hash,
            size_bytes: meta.size_bytes,
            received_at: meta.received_at,
            envelope,
            keywords: Flags(keywords_acc),
            tags: tags_acc,
            mailbox_ids,
        })
    }

    fn read_blob(&self, hash: cosmix_mds::BlobHash) -> Result<Vec<u8>> {
        self.mds.get_blob(&hash).map_err(|e| anyhow!(e))
    }

    fn put_blob(&self, blob: &[u8]) -> Result<cosmix_mds::BlobHash> {
        self.mds.put_blob(blob).map_err(|e| anyhow!(e))
    }

    fn find_emails_by_blob_hash(
        &self,
        account: AccountId,
        blob_hash: &cosmix_mds::BlobHash,
    ) -> Result<Vec<EmailRecord>> {
        let set = account_id_to_setid(account);
        let ids = match self.mds.find_items_by_blob_hash(&set, blob_hash) {
            Ok(v) => v,
            Err(cosmix_mds::Error::SetNotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!(e)),
        };
        // `get_email` re-issues the per-item read inside its own
        // with_set_tx. Filter the two pre-shaped sentinels that mean
        // "this candidate isn't a usable Email/import target right
        // now":
        //
        //   * `"email not found"` — concurrent destroy between the
        //     blob-hash index lookup and the per-item fetch.
        //
        //   * `"email envelope missing"` — the item has the matching
        //     blob_hash but no `mail_envelopes` sidecar row. This
        //     happens for staging-container items (SMTP inbound
        //     deposits the body in CAS first, the envelope is
        //     written when the message is filed) or for items
        //     created before Task 1.7's create_email path landed.
        //     Either way the row is not a JMAP-visible Email and
        //     must not be returned as an idempotency match —
        //     otherwise the import would fail with the same
        //     sentinel surfaced as a JMAP error.
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            match self.get_email(account, id) {
                Ok(rec) => out.push(rec),
                Err(e) => {
                    let s = e.to_string();
                    if s.starts_with("email not found") || s.starts_with("email envelope missing") {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    fn create_email(
        &self,
        account: AccountId,
        mailbox: ContainerId,
        blob: BlobHash,
        envelope: EmailEnvelope,
        in_reply_to: &[String],
        initial_keywords: Flags,
        initial_tags: Tags,
        received_at: i64,
    ) -> Result<(ItemId, MembershipUid)> {
        let set = account_id_to_setid(account);
        // Initial user keywords applied to the new membership atomically
        // with the create (mds `insert_membership` writes the tags column
        // in the same tx). Empty for the common no-user-keyword create.
        let initial_tags: Vec<String> = initial_tags.into();
        let to_json = serde_json::to_string(&envelope.to)
            .map_err(|e| anyhow!("serialize envelope.to: {e}"))?;
        let cc_json = serde_json::to_string(&envelope.cc)
            .map_err(|e| anyhow!("serialize envelope.cc: {e}"))?;
        let bcc_json = serde_json::to_string(&envelope.bcc)
            .map_err(|e| anyhow!("serialize envelope.bcc: {e}"))?;
        let reply_to_json = serde_json::to_string(&envelope.reply_to)
            .map_err(|e| anyhow!("serialize envelope.reply_to: {e}"))?;
        let in_reply_to: Vec<String> = in_reply_to.to_vec();

        let mds = &self.mds;
        let result = mds.with_set_tx(&set, |tx| {
            // `mds.blob_size` and `mds.blob_exists` (below) are pure
            // `fs::metadata` / `Path::exists` calls — they do **not**
            // re-enter the per-set `Mutex` that `with_set_tx` holds, so
            // calling them inside the closure is safe per the re-entry
            // rule (which only forbids `Mds` *mutation* methods).
            // Capture the blob size INSIDE the closure so a GC Pass 2
            // that started after our entry is serialized against
            // `add_staging_item`'s blobs_db UPSERT below.
            // `add_staging_item` takes the size as input — the
            // primitive itself does not stat the file.
            let blob_size = mds.blob_size(&blob).map_err(|e| match e {
                cosmix_mds::Error::BlobNotFound(_) => {
                    cosmix_mds::Error::BlobNotFound(blob::hex(&blob))
                }
                other => other,
            })?;

            let (item_id, placement) =
                tx.add_staging_item(&mailbox, &blob, blob_size, initial_keywords, &initial_tags)
                    .map_err(|e| match e {
                        cosmix_mds::Error::ContainerNotFound(_) => cosmix_mds::Error::Other(
                            format!("mailbox not found: {mailbox}", mailbox = mailbox.0),
                        ),
                        other => other,
                    })?;

            // Defensive re-stat after `add_staging_item`. If a GC Pass
            // 2 ran between our entry and `add_blob_ref_in_tx`, it
            // would have deleted the CAS file and the `blob` row;
            // `add_blob_ref_in_tx`'s UPSERT then re-created the row
            // (with our `blob_size`) but cannot re-create the file.
            // Catch that here — the per-set tx's blobs_db writes are
            // not yet committed, so returning an error rolls back
            // every write atomically: the re-inserted `blob` row, the
            // bumped `refcount`, the `blob_ref`, the `item`, and the
            // initial `membership`. The blob row going back to absent
            // is the load-bearing piece — GC's next pass then sees no
            // row at all and the dangling row scenario does not arise.
            // Any *future* GC Pass 2 sees refcount ≥ 1 from our
            // committed UPSERT and skips.
            if !mds.blob_exists(&blob)? {
                return Err(cosmix_mds::Error::BlobNotFound(blob::hex(&blob)));
            }

            // Override the auto-assigned `received_at` (now_ms) with
            // the caller-supplied timestamp. Matches IMAP `INTERNALDATE`
            // / JMAP `receivedAt` semantics — the caller knows when the
            // server received this message; mds doesn't.
            let updated = tx
                .tx()
                .execute(
                    "UPDATE item SET received_at = ?1 WHERE id = ?2",
                    params![received_at, item_id.0.to_string()],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("update item.received_at: {e}")))?;
            if updated != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "update item.received_at: expected 1 row, got {updated}"
                )));
            }

            let normalized_subject = subject::normalize_subject(&envelope.subject);
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_envelopes \
                     (item_id, from_addr, to_addrs, cc_addrs, bcc_addrs, \
                      reply_to_addrs, subject, date_ts, message_id, \
                      normalized_subject) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        item_id.0.to_string(),
                        envelope.from,
                        to_json,
                        cc_json,
                        bcc_json,
                        reply_to_json,
                        envelope.subject,
                        envelope.date,
                        envelope.message_id,
                        normalized_subject,
                    ],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_envelopes: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_envelopes: expected 1 row, got {inserted}"
                )));
            }

            let thread_id =
                find_or_new_thread_id_in_tx(tx, account, &in_reply_to, &envelope.subject)?;
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_threads (item_id, account_id, thread_id) \
                     VALUES (?1, ?2, ?3)",
                    params![item_id.0.to_string(), account, thread_id],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_threads: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_threads: expected 1 row, got {inserted}"
                )));
            }

            // mail_search FTS5 projection — Phase 8e gate 2. Synchronous
            // write inside this `with_set_tx` so the row commits
            // atomically with `item` / `mail_envelopes` / `mail_threads`.
            // `mds.get_blob` is a `fs::read` against the CAS root and
            // does not re-enter the per-set mutex; if it fails (GC race
            // squeezed between `blob_exists` above and here, or a
            // transient fs error) we fall back to indexing the envelope-
            // only columns rather than dropping the search row entirely.
            // A missing FTS row would make the message permanently
            // un-findable until a `mail_search rebuild` admin command
            // runs — far worse than a row that lacks `body_text`.
            // Acceptance bar (`_tasks/2026-05-04-maild-phase-8e.spec.mix`)
            // calls out FTS overhead measurement as a follow-up before
            // declaring 8e done; not bench-gating here keeps the
            // synchronous-write semantics the spec demands.
            let raw_body = mds.get_blob(&blob).unwrap_or_else(|e| {
                warn!(
                    item_id = %item_id.0,
                    blob = %blob::hex(&blob),
                    err = %e,
                    "mail_search: blob load failed inside with_set_tx; indexing envelope only"
                );
                Vec::new()
            });
            let projection = build_search_projection(&envelope, &raw_body);
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_search \
                     (item_id, account_id, headers, subject, body_text, normalized_addrs) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        item_id.0.to_string(),
                        account,
                        projection.headers,
                        projection.subject,
                        projection.body_text,
                        projection.normalized_addrs,
                    ],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_search: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_search: expected 1 row, got {inserted}"
                )));
            }

            // Read the destination container's seq_validity in the
            // same tx that produced the membership row, so the returned
            // (uid, uid_validity) pair is read-consistent with the
            // freshly-inserted membership.seq — surfaces UIDPLUS
            // APPENDUID without a second mds round-trip.
            let uid_validity = tx.container_seq_validity(&mailbox)?;
            let uid = MembershipUid {
                container_id: mailbox,
                uid: placement.seq.0 as u64,
                uid_validity,
            };
            Ok((item_id, uid))
        });

        match result {
            Ok(pair) => Ok(pair),
            Err(cosmix_mds::Error::BlobNotFound(hex)) => Err(anyhow!("blob not found: {hex}")),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("mailbox not found: {mailbox}", mailbox = mailbox.0))
            }
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("mailbox not found") => {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn create_email_with_memberships(
        &self,
        account: AccountId,
        mailboxes: &[ContainerId],
        blob: BlobHash,
        envelope: EmailEnvelope,
        in_reply_to: &[String],
        initial_keywords: Flags,
        initial_tags: Tags,
        received_at: i64,
    ) -> Result<(ItemId, Vec<MembershipUid>)> {
        // Uniform initial user keywords for every membership of the
        // fresh item (the v1.1 §3 "all memberships agree" invariant),
        // applied atomically with the create. Empty for the common case.
        let initial_tags: Vec<String> = initial_tags.into();
        // Caller-side prevalidation for shape errors that have a
        // friendlier surface than the underlying mds primitives'
        // `Error::Other("add_item_with_memberships: …")` formatting.
        // `add_item_with_memberships_in_tx` re-validates these inside
        // the tx; this layer just reshapes the messages so JMAP
        // handlers don't have to grep for "add_item_with_memberships:"
        // sentinels. Both layers reject before any write.
        if mailboxes.is_empty() {
            return Err(anyhow!("mailboxes must be non-empty"));
        }
        {
            let mut seen: std::collections::HashSet<ContainerId> =
                std::collections::HashSet::with_capacity(mailboxes.len());
            for cid in mailboxes.iter() {
                if !seen.insert(*cid) {
                    return Err(anyhow!(
                        "duplicate mailbox in mailboxes slice: {cid}",
                        cid = cid.0
                    ));
                }
            }
        }

        let set = account_id_to_setid(account);
        let to_json = serde_json::to_string(&envelope.to)
            .map_err(|e| anyhow!("serialize envelope.to: {e}"))?;
        let cc_json = serde_json::to_string(&envelope.cc)
            .map_err(|e| anyhow!("serialize envelope.cc: {e}"))?;
        let bcc_json = serde_json::to_string(&envelope.bcc)
            .map_err(|e| anyhow!("serialize envelope.bcc: {e}"))?;
        let reply_to_json = serde_json::to_string(&envelope.reply_to)
            .map_err(|e| anyhow!("serialize envelope.reply_to: {e}"))?;
        let memberships: Vec<(ContainerId, Flags)> = mailboxes
            .iter()
            .map(|cid| (*cid, initial_keywords))
            .collect();
        let in_reply_to: Vec<String> = in_reply_to.to_vec();

        let mds = &self.mds;
        let result = mds.with_set_tx(&set, |tx| {
            // Mirror create_email's GC Pass 2 race-handling: capture
            // blob_size inside the closure (so a Pass 2 that started
            // after our entry serializes against the blobs_db UPSERT
            // below), then re-stat after the writes to catch the case
            // where Pass 2 ran in the gap.
            let blob_size = mds.blob_size(&blob).map_err(|e| match e {
                cosmix_mds::Error::BlobNotFound(_) => {
                    cosmix_mds::Error::BlobNotFound(blob::hex(&blob))
                }
                other => other,
            })?;

            let (item_id, placements) = tx
                .add_item_with_memberships(&blob, blob_size, &memberships, &initial_tags)
                .map_err(|e| match e {
                    cosmix_mds::Error::ContainerNotFound(cid) => {
                        cosmix_mds::Error::Other(format!("mailbox not found: {cid}"))
                    }
                    other => other,
                })?;

            // Defensive re-stat after the writes. See create_email
            // doc-comment for the GC Pass 2 race rationale.
            if !mds.blob_exists(&blob)? {
                return Err(cosmix_mds::Error::BlobNotFound(blob::hex(&blob)));
            }

            // Override the auto-assigned `received_at` (now_ms) with
            // the caller-supplied timestamp, matching create_email.
            let updated = tx
                .tx()
                .execute(
                    "UPDATE item SET received_at = ?1 WHERE id = ?2",
                    params![received_at, item_id.0.to_string()],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("update item.received_at: {e}")))?;
            if updated != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "update item.received_at: expected 1 row, got {updated}"
                )));
            }

            // Single envelope sidecar row per item, regardless of
            // membership count. The envelope is item-scoped (RFC 5322
            // headers don't differ per mailbox).
            let normalized_subject = subject::normalize_subject(&envelope.subject);
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_envelopes \
                     (item_id, from_addr, to_addrs, cc_addrs, bcc_addrs, \
                      reply_to_addrs, subject, date_ts, message_id, \
                      normalized_subject) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        item_id.0.to_string(),
                        envelope.from,
                        to_json,
                        cc_json,
                        bcc_json,
                        reply_to_json,
                        envelope.subject,
                        envelope.date,
                        envelope.message_id,
                        normalized_subject,
                    ],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_envelopes: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_envelopes: expected 1 row, got {inserted}"
                )));
            }

            let thread_id =
                find_or_new_thread_id_in_tx(tx, account, &in_reply_to, &envelope.subject)?;
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_threads (item_id, account_id, thread_id) \
                     VALUES (?1, ?2, ?3)",
                    params![item_id.0.to_string(), account, thread_id],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_threads: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_threads: expected 1 row, got {inserted}"
                )));
            }

            // mail_search FTS5 projection — Phase 8e gate 2. Single row
            // per item regardless of membership count; the projection
            // is item-scoped (envelope + body bytes), not membership-
            // scoped. See `create_email` above for the GC-race
            // fallback rationale; same applies here.
            let raw_body = mds.get_blob(&blob).unwrap_or_else(|e| {
                warn!(
                    item_id = %item_id.0,
                    blob = %blob::hex(&blob),
                    err = %e,
                    "mail_search: blob load failed inside with_set_tx; indexing envelope only"
                );
                Vec::new()
            });
            let projection = build_search_projection(&envelope, &raw_body);
            let inserted = tx
                .tx()
                .execute(
                    "INSERT INTO mail_search \
                     (item_id, account_id, headers, subject, body_text, normalized_addrs) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        item_id.0.to_string(),
                        account,
                        projection.headers,
                        projection.subject,
                        projection.body_text,
                        projection.normalized_addrs,
                    ],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("insert mail_search: {e}")))?;
            if inserted != 1 {
                return Err(cosmix_mds::Error::Other(format!(
                    "insert mail_search: expected 1 row, got {inserted}"
                )));
            }

            // Read each dest container's seq_validity in the same tx
            // that produced the membership rows, so the returned
            // (uid, uid_validity) pair is read-consistent with the
            // freshly-inserted membership.seq.
            let mut uids: Vec<MembershipUid> = Vec::with_capacity(placements.len());
            for p in placements.iter() {
                let uid_validity = tx.container_seq_validity(&p.container)?;
                uids.push(MembershipUid {
                    container_id: p.container,
                    uid: p.seq.0 as u64,
                    uid_validity,
                });
            }

            Ok((item_id, uids))
        });

        match result {
            Ok(pair) => Ok(pair),
            Err(cosmix_mds::Error::BlobNotFound(hex)) => Err(anyhow!("blob not found: {hex}")),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                // Surface the first mailbox in the slice as the
                // representative not-found id, matching create_email's
                // single-mailbox surface and giving the JMAP layer a
                // concrete id to echo. We've already prevalidated
                // non-empty above so [0] is safe.
                Err(anyhow!("mailbox not found: {cid}", cid = mailboxes[0].0))
            }
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("mailbox not found") => {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn add_memberships(
        &self,
        account: AccountId,
        id: ItemId,
        mailboxes: &[ContainerId],
    ) -> Result<Vec<MembershipUid>> {
        if mailboxes.is_empty() {
            return Err(anyhow!("mailboxes must be non-empty"));
        }
        {
            let mut seen: std::collections::HashSet<ContainerId> =
                std::collections::HashSet::with_capacity(mailboxes.len());
            for cid in mailboxes.iter() {
                if !seen.insert(*cid) {
                    return Err(anyhow!(
                        "duplicate mailbox in mailboxes slice: {cid}",
                        cid = cid.0
                    ));
                }
            }
        }

        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            // Per-membership inheritance happens inside
            // `add_membership_in_tx`; the caller-supplied flags is
            // only used as the fallback when the item has zero
            // existing memberships. JMAP `Email/set update` adding
            // mailboxIds is the post-create path, so the item is
            // expected to already have ≥1 membership and the fallback
            // is dead in that flow. We pass `Flags(0)` to make that
            // explicit — if the inheritance read returns None, the
            // result is a deeply-unmoored item being re-anchored,
            // which is a bug a JMAP handler shouldn't cover up by
            // injecting flags it didn't see in the wire request.
            let mut uids: Vec<MembershipUid> = Vec::with_capacity(mailboxes.len());
            for cid in mailboxes.iter() {
                let placement = tx.add_membership(&id, cid, Flags(0)).map_err(|e| match e {
                    cosmix_mds::Error::ItemNotFound(_) => {
                        cosmix_mds::Error::Other(format!("email not found: {id}", id = id.0))
                    }
                    cosmix_mds::Error::ContainerNotFound(c) => {
                        cosmix_mds::Error::Other(format!("mailbox not found: {c}"))
                    }
                    other => other,
                })?;
                let uid_validity = tx.container_seq_validity(cid)?;
                uids.push(MembershipUid {
                    container_id: *cid,
                    uid: placement.seq.0 as u64,
                    uid_validity,
                });
            }
            Ok(uids)
        });

        match result {
            Ok(uids) => Ok(uids),
            Err(cosmix_mds::Error::SetNotFound(_)) => {
                Err(anyhow!("email not found: {id}", id = id.0))
            }
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("email not found")
                    || s.starts_with("mailbox not found")
                    || s.starts_with("item ")
                    || s.starts_with("add_item_with_memberships:") =>
            {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn set_keywords(
        &self,
        account: AccountId,
        id: ItemId,
        keywords: Flags,
        tags: Tags,
    ) -> Result<()> {
        let set = account_id_to_setid(account);
        // Whole-item fan-out: `store_item_keywords` applies `keywords`
        // (system flags) + `tags` (user keywords) to **every** current
        // membership of `id` in one atomic transaction — the JMAP
        // `Email/set keywords` contract (RFC 8621 §4.4.6). Unlike the
        // prior flags-only `store_item_flags`, this also writes `tags`,
        // so it carries the full keyword object the wire layer
        // computed. An empty result vec means the item has no
        // memberships (unknown id, or the zero-membership orphan
        // window): JMAP has no addressable target, so we surface a
        // single `"email not found"` sentinel — *stricter* than
        // `get_email`, matching the previous contract.
        match self.mds.store_item_keywords(&set, &id, keywords, tags) {
            Ok(touched) if touched.is_empty() => Err(anyhow!("email not found: {id}", id = id.0)),
            Ok(_) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn set_keywords_in_mailbox(
        &self,
        account: AccountId,
        id: ItemId,
        mailbox: ContainerId,
        keywords: Flags,
        tags: Tags,
    ) -> Result<()> {
        let set = account_id_to_setid(account);
        // Per-mailbox write: `store_membership_keywords` mutates the
        // (item, mailbox) row only — sibling memberships of `id` in
        // other mailboxes are untouched (the load-bearing IMAP STORE
        // isolation; see P1 in `_doc/planned/imap-runway.md`). It
        // writes `keywords` (system flags) + `tags` (user keywords) on
        // that one row, and — like the flags-only `store_flags` — it
        // disambiguates two failures: `ContainerNotFound` when the
        // named mailbox does not exist (caller typo, deleted between
        // resolution and STORE) and `Error::Other("membership ... not
        // present")` when the mailbox exists but the (item, mailbox)
        // row does not (item never in this mailbox, or removed by a
        // peer between UID lookup and STORE). The wrapper maps the
        // first to "mailbox not found" — matching every other
        // mailbox-parametric MailStore verb — and the second to
        // "email not found", so existing error-prefix matchers keep
        // working.
        match self
            .mds
            .store_membership_keywords(&set, &id, &mailbox, keywords, tags)
        {
            Ok(_) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            Err(cosmix_mds::Error::ContainerNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = mailbox.0))
            }
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("membership ") => {
                Err(anyhow!("email not found: {id}", id = id.0))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn add_flags_in_mailbox(
        &self,
        account: AccountId,
        id: ItemId,
        mailbox: ContainerId,
        bits: Flags,
    ) -> Result<Flags> {
        let set = account_id_to_setid(account);
        let result: std::result::Result<Flags, cosmix_mds::Error> =
            self.mds.with_set_tx(&set, |tx| {
                // Read-modify-write under the per-set transaction so
                // any peer session's intervening STORE is honoured
                // rather than clobbered.
                let memberships = tx.item_memberships(&id)?;
                let cur = memberships
                    .iter()
                    .find(|(m, _, _)| *m == mailbox)
                    .map(|(_, f, _)| *f)
                    .ok_or_else(|| {
                        cosmix_mds::Error::Other(format!(
                            "membership {id}/{mb} not present",
                            id = id.0,
                            mb = mailbox.0
                        ))
                    })?;
                let merged = Flags(cur.0 | bits.0);
                if merged == cur {
                    // Idempotent no-op — skip the notifier wakeup.
                    return Ok(cur);
                }
                tx.store_flags(&id, &mailbox, merged)?;
                Ok(merged)
            });

        match result {
            Ok(f) => Ok(f),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            Err(cosmix_mds::Error::ContainerNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = mailbox.0))
            }
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("membership ") => {
                Err(anyhow!("email not found: {id}", id = id.0))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn move_email(&self, account: AccountId, id: ItemId, to_mailbox: ContainerId) -> Result<()> {
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            let memberships = tx.item_memberships(&id)?;
            match memberships.len() {
                0 => Err(cosmix_mds::Error::Other(format!(
                    "email not found: {id}",
                    id = id.0
                ))),
                1 => {
                    let (src, _flags, _tags) = &memberships[0];
                    if *src == to_mailbox {
                        // Already in the destination, no work to do.
                        // No-op return preserves idempotency for
                        // JMAP clients that re-send the same set.
                        return Ok(());
                    }
                    // `move_item_in_tx` inherits the per-membership
                    // (Flags, Tags) from the existing src row; the
                    // `Flags(0)` arg here is the documented fallback
                    // for the impossible "src has no row at this
                    // point" case and is otherwise ignored.
                    tx.move_item(&id, src, &to_mailbox, Flags(0))?;
                    Ok(())
                }
                n => Err(cosmix_mds::Error::Other(format!(
                    "ambiguous source: email {id} belongs to {n} \
                     mailboxes; multi-mailbox set replacement is not \
                     yet supported by move_email — use move_message \
                     with an explicit src",
                    id = id.0
                ))),
            }
        });

        match result {
            Ok(()) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            // `move_item_in_tx` validates both src and dest. Under
            // the per-set lock held by `with_set_tx`, src came from
            // `item_memberships` *in the same tx*, so it cannot be
            // missing — barring DB-level corruption. We still echo
            // the offending UUID from the error rather than
            // hard-coding `to_mailbox.0`, so an unexpected src miss
            // surfaces with the correct id rather than a misleading
            // one.
            Err(cosmix_mds::Error::ContainerNotFound(missing)) => {
                Err(anyhow!("mailbox not found: {missing}"))
            }
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("email not found") || s.starts_with("ambiguous source") =>
            {
                Err(anyhow!(s))
            }
            // The remaining `move_item_in_tx` errors are unreachable
            // from this path and fall through opaquely as db-
            // inconsistency signals if they ever surface:
            //   * `Error::Other("move_item: src and dest must
            //     differ")` — short-circuited by the `*src ==
            //     to_mailbox` no-op check above.
            //   * `Error::Other("item ... not in source
            //     container")` and `Error::ItemNotFound(...)` — src
            //     and item id both came from `item_memberships` in
            //     the same tx, so they exist.
            //   * `Error::Other("item ... already in destination
            //     container")` — would imply the email already had
            //     ≥2 memberships including `to_mailbox`, caught by
            //     the ambiguous-source branch.
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn delete_email(&self, account: AccountId, id: ItemId) -> Result<()> {
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            let memberships = tx.item_memberships(&id)?;
            if memberships.is_empty() {
                return Err(cosmix_mds::Error::Other(format!(
                    "email not found: {id}",
                    id = id.0
                )));
            }
            // Iterate every membership; the last `remove_membership`
            // call cascades `item` and `mail_envelopes` deletes and
            // drops the cross-DB `blob_ref`. Order is the
            // `item_memberships` order (BY container_id) — not
            // semantically load-bearing, but stable across runs.
            for (cid, _flags, _tags) in memberships {
                tx.remove_membership(&id, &cid)?;
            }
            // mail_search FTS5 reap — Phase 8e gate 2. The FTS5
            // virtual table cannot carry a foreign key (FK constraints
            // are unsupported on `content=''` tables), so we delete
            // by `item_id` in the same `with_set_tx` scope as the
            // `mail_envelopes` cascade. `_doc/planned/maild-sidecar-
            // reap.md` Option C is the long-term shape for ALL
            // sidecars; here we follow the FTS-must-be-manual carve-
            // out. Tolerates zero affected rows: an item that never
            // had an FTS projection (legacy item written before this
            // commit, or `mail_search rebuild` not yet run) still
            // deletes cleanly.
            tx.tx()
                .execute(
                    "DELETE FROM mail_search WHERE item_id = ?1",
                    params![id.0.to_string()],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("delete mail_search: {e}")))?;
            Ok(())
        });

        match result {
            Ok(()) => Ok(()),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            // Wontfix here: prefix match against an `Error::Other`
            // we ourselves constructed inside the closure above
            // follows the Phase 1 convention (set_keywords,
            // move_email). A typed `MailStoreError` variant is
            // the right fix and is deferred to a Phase 1 cleanup
            // pass once all adapter methods land.
            Err(cosmix_mds::Error::Other(s)) if s.starts_with("email not found") => Err(anyhow!(s)),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn email_changes(&self, account: AccountId, since_state: u64) -> Result<EmailChangeSet> {
        // `cosmix-mds::container::changes_since_set` casts the
        // token `as i64` at the SQL boundary. A `since_state`
        // above `i64::MAX` would silently wrap to negative and
        // return the full set_change table — fail fast instead.
        // Real seqs are allocated monotonically from 0 and will
        // not approach this bound for any plausible workload.
        if since_state > i64::MAX as u64 {
            return Err(anyhow!(
                "since_state out of range: {since_state} > i64::MAX"
            ));
        }
        let set = account_id_to_setid(account);
        let since = SetChangeToken(since_state);

        // Page limit for `changes_since_set`. `u32::MAX as usize`
        // (4_294_967_295) is the largest value that round-trips
        // through SQLite's i64 `LIMIT` parameter without sign-extension
        // surprises. Phase 1 callers don't paginate at the JMAP layer
        // — `Email/changes` `maxChanges` is enforced by the protocol
        // handler above this — so this method always returns the full
        // delta since `since_state`. The internal page loop below stays
        // for correctness if the row count ever exceeds this cap.
        const PAGE_LIMIT: usize = u32::MAX as usize;

        let result = self.mds.with_set_tx(
            &set,
            |tx| -> std::result::Result<EmailChangeSet, cosmix_mds::Error> {
                // RFC 8620 §5.2: a `sinceState` that is not a valid
                // prior state MUST yield `cannotCalculateChanges`.
                // Read the per-set head (`set_state.set_change_seq`,
                // self-healed to 0 on the first allocation) and
                // reject any `since_state` above it. Without this
                // guard, a future token returns an empty page and
                // `unwrap_or(since_state)` echoes it back as
                // `new_state` — subsequent real changes whose seq
                // is `<= since_state` would then be permanently
                // skipped for that client. The JMAP handler maps
                // the `"future since_state"` prefix to
                // `cannotCalculateChanges`. `since_state == head`
                // is valid (no changes since the last sync).
                // Read head and retention floor in one shot. The
                // floor (MCS Phase 3-A `set_change_floor`) advances
                // whenever `prune_changelog` deletes from `set_change`;
                // a since_state below it can no longer be answered from
                // the surviving rows, so the storage layer surfaces
                // `"stale since_state"` and the JMAP handler maps it to
                // `cannotCalculateChanges` (RFC 8620 §5.2). Reading
                // both inside the same `with_set_tx` snapshot keeps the
                // head/floor check race-free against a concurrent prune.
                let (head, floor): (u64, u64) = tx
                    .tx()
                    .query_row(
                        "SELECT set_change_seq, set_change_floor \
                         FROM set_state WHERE set_id = ?1",
                        params![set.0.to_string()],
                        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
                    )
                    .optional()
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!("read set_state head/floor: {e}"))
                    })?
                    .unwrap_or((0, 0));
                if since_state > head {
                    return Err(cosmix_mds::Error::Other(format!(
                        "future since_state: {since_state} > current set_change_seq {head}"
                    )));
                }
                // `since == floor` is still answerable from `seq >
                // since` rows, so the predicate is strict `<`.
                if since_state < floor {
                    return Err(cosmix_mds::Error::Other(format!(
                        "stale since_state: {since_state} < set_change_floor {floor}"
                    )));
                }

                // Resolve the per-account `__upload_staging__`
                // container id (if it has been provisioned). Items
                // currently sitting in the staging zone — and any
                // `Added`/`Removed` events targeting it — are
                // internal to `create_blob_ref` / GC and must not
                // surface as JMAP `Email/changes` deltas. Without
                // this filter, an in-flight upload would appear as
                // a phantom `created` (or be omitted as
                // create+destroy if GC runs in-window), neither of
                // which a JMAP client should ever observe. Mirrors
                // the same filter `list_mailboxes` applies by name
                // at line ~978.
                let staging_id: Option<ContainerId> = tx
                    .tx()
                    .query_row(
                        "SELECT id FROM container WHERE parent_id IS NULL AND name = ?1",
                        params![UPLOAD_STAGING_NAME],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!("staging container lookup: {e}"))
                    })?
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .map(ContainerId);

                // Drain the entire change stream since `since` in
                // one tx so every classification read sees the
                // same point-in-time set state.
                let mut all_changes: Vec<cosmix_mds::SetChange> = Vec::new();
                let mut cursor = since;
                loop {
                    let (page, next) = tx.changes_since_set(cursor, PAGE_LIMIT)?;
                    all_changes.extend(page);
                    match next {
                        Some(t) => cursor = t,
                        None => break,
                    }
                }

                // `new_state` is the highest set_change_seq the
                // caller is being asked to advance past — including
                // staging-container events. Filtering staging out
                // of `touched_order` does not require lowering
                // `new_state`: the JMAP client just tracks "I have
                // seen up to seq N" regardless of which events
                // landed in that range.
                let new_state = all_changes
                    .iter()
                    .map(|c| c.set_change_seq.0)
                    .max()
                    .unwrap_or(since_state);

                // Walk events in seq-ASC order (already sorted by
                // changes_since_set) and capture the *first*
                // non-staging event kind per item — that
                // determines whether the item existed at
                // `since_state` from the JMAP-visible perspective.
                use std::collections::{HashMap, HashSet};
                let mut first_kind: HashMap<ItemId, ChangeKind> = HashMap::new();
                let mut touched_order: Vec<ItemId> = Vec::new();
                let mut touched_set: HashSet<ItemId> = HashSet::new();
                for c in &all_changes {
                    if Some(c.container_id) == staging_id {
                        continue;
                    }
                    first_kind.entry(c.item_id).or_insert(c.kind);
                    if touched_set.insert(c.item_id) {
                        touched_order.push(c.item_id);
                    }
                }

                let mut created = Vec::new();
                let mut updated = Vec::new();
                let mut destroyed = Vec::new();

                for id in touched_order {
                    // Exists at `new_state` iff the item still has
                    // any non-staging membership row at the snapshot
                    // point. An item whose only remaining membership
                    // is in staging has not yet "appeared" as an
                    // email per JMAP — treat it as not-yet-existing.
                    let memberships = tx.item_memberships(&id)?;
                    let exists_now = memberships
                        .iter()
                        .any(|(cid, _, _)| Some(*cid) != staging_id);
                    let existed_at_since = matches!(
                        first_kind[&id],
                        ChangeKind::MetadataChanged | ChangeKind::Removed
                    );

                    match (existed_at_since, exists_now) {
                        (false, true) => created.push(id),
                        (true, false) => destroyed.push(id),
                        (true, true) => updated.push(id),
                        // Created and destroyed inside the window —
                        // omit per RFC 8621 §4.1.5: the client never
                        // observed it, so reporting either kind would
                        // be a phantom event.
                        (false, false) => {}
                    }
                }

                Ok(EmailChangeSet {
                    new_state,
                    created,
                    updated,
                    destroyed,
                })
            },
        );

        match result {
            Ok(cs) => Ok(cs),
            // Unprovisioned account: mirror `email_state`'s zero-token
            // baseline so a client that calls `Email/changes` against
            // an empty account with `sinceState="0"` gets an empty
            // delta instead of `cannotCalculateChanges`. Any
            // `since_state > 0` against an unprovisioned set is still
            // an error — there's no valid prior state token they
            // could have observed.
            Err(cosmix_mds::Error::SetNotFound(_)) if since_state == 0 => Ok(EmailChangeSet {
                new_state: 0,
                created: Vec::new(),
                updated: Vec::new(),
                destroyed: Vec::new(),
            }),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "future since_state: {since_state} > current set_change_seq 0"
            )),
            // Pre-shaped message from inside the closure carries
            // the trait-prefix vocabulary already; unwrap the
            // Error::Other variant so the Display form is the bare
            // `"future since_state: ..."` (or `"stale since_state:
            // ..."` for the MCS Phase 3-B retention-floor reject) and
            // the JMAP handler's `starts_with` match fires cleanly.
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("future since_state") || s.starts_with("stale since_state") =>
            {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn email_state(&self, account: AccountId) -> Result<u64> {
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            let head: Option<i64> = tx
                .tx()
                .query_row(
                    "SELECT set_change_seq FROM set_state WHERE set_id = ?1",
                    params![set.0.to_string()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| cosmix_mds::Error::Other(format!("read set_state head: {e}")))?;
            Ok(head.map(|v| v as u64).unwrap_or(0))
        });

        match result {
            Ok(seq) => Ok(seq),
            // Unprovisioned account: matches email_changes' "no
            // history" semantics — return 0 so JMAP oldState / newState
            // converge to the empty-account zero token rather than an
            // error. The JMAP handler does not need to distinguish
            // "valid empty account" from "unprovisioned account."
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(0),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn mailbox_state(&self, account: AccountId) -> Result<String> {
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(&set, |tx| {
            let lifecycle_max: Option<i64> = tx
                .tx()
                .query_row(
                    "SELECT MAX(container_change_set_seq) FROM container_change_set",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| {
                    cosmix_mds::Error::Other(format!("read container_change_set head: {e}"))
                })?
                .flatten();
            // Pull the lifecycle floor alongside the set_change head;
            // after `prune_changelog(ContainerChangeSet, 0)` deletes
            // every lifecycle row, `MAX(container_change_set_seq)`
            // collapses to NULL while `container_change_set_floor`
            // retains the previous max. The state token must remain
            // monotonic — emitting `0` after a full prune would issue
            // a token below the floor and the next
            // `Mailbox/changes(sinceState=…)` would reject it as
            // stale. Lift the lifecycle half to `max(MAX, floor)` so
            // the issued token always sits at or above the floor.
            let row: Option<(i64, i64)> = tx
                .tx()
                .query_row(
                    "SELECT set_change_seq, container_change_set_floor \
                     FROM set_state WHERE set_id = ?1",
                    params![set.0.to_string()],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(|e| {
                    cosmix_mds::Error::Other(format!("read set_state head/lifecycle-floor: {e}"))
                })?;
            let (set_change, lifecycle_floor) = row.unwrap_or((0, 0));
            let lifecycle = lifecycle_max
                .map(|v| v as u64)
                .unwrap_or(0)
                .max(lifecycle_floor as u64);
            Ok((lifecycle, set_change as u64))
        });

        match result {
            Ok((l, s)) => Ok(format!("{l}:{s}")),
            // Unprovisioned account: mirror email_state's zero-baseline.
            // Both halves collapse to 0, giving a "0:0" token that
            // mailbox_changes accepts as a valid prior state.
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok("0:0".to_string()),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn mailbox_changes(
        &self,
        account: AccountId,
        since_state: &str,
        max_changes: usize,
    ) -> Result<MailboxChangeSet> {
        // Parse the paired token before opening any tx so a malformed
        // string surfaces as `"invalid sinceState"` without touching
        // the database. RFC 8620 §1.3: state tokens are opaque
        // strings on the wire — the substrate is free to define the
        // internal grammar, but parse strictness defends against
        // clients echoing back garbage from a different
        // implementation.
        let (since_lifecycle, since_set_change) = parse_paired_token(since_state)
            .ok_or_else(|| anyhow!("invalid sinceState: {since_state:?}"))?;

        // i64::MAX guard mirrors email_changes — `container_change_set_seq`
        // and `set_state.set_change_seq` are stored as signed 64-bit
        // integers and a `> i64::MAX` token would silently wrap to a
        // negative value at the SQL boundary.
        if since_lifecycle > i64::MAX as u64 || since_set_change > i64::MAX as u64 {
            return Err(anyhow!(
                "invalid sinceState: {since_state:?} (component exceeds i64::MAX)"
            ));
        }

        let set = account_id_to_setid(account);
        // Cap defensively at i64::MAX as usize so the `LIMIT max+1`
        // SQL bind below cannot wrap. `max_changes == 0` is honoured
        // as a real zero-page probe (see post-head short-circuit
        // below) — no silent coercion to 1, which would otherwise
        // emit one un-asked-for row and break the storage-layer
        // contract.
        let max = max_changes.min(i64::MAX as usize);

        let result = self
            .mds
            .with_set_tx(&set, |tx| -> std::result::Result<MailboxChangeSet, cosmix_mds::Error> {
                // Heads first. Inside `with_set_tx` the per-set
                // mutex serialises writers, so no new lifecycle or
                // set_change rows can appear between this read and
                // the page query below.
                let lifecycle_max: u64 = tx
                    .tx()
                    .query_row(
                        "SELECT MAX(container_change_set_seq) FROM container_change_set",
                        [],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "read container_change_set head: {e}"
                        ))
                    })?
                    .map(|v| v as u64)
                    .unwrap_or(0);
                // Both heads + both retention floors come from
                // `set_state` (lifecycle head is the MAX on the
                // `container_change_set` table itself, read above).
                // The two floors (MCS Phase 3-A) advance when
                // `prune_changelog` deletes from either stream; a
                // paired token whose lifecycle half is below the
                // lifecycle floor or whose set_change half is below
                // the set_change floor can no longer be answered
                // from the surviving rows. Read both halves in one
                // SELECT so the head/floor check is consistent
                // against a concurrent prune.
                let (curr_set_change, lifecycle_floor, set_change_floor): (u64, u64, u64) =
                    tx.tx()
                        .query_row(
                            "SELECT set_change_seq, container_change_set_floor, \
                                    set_change_floor \
                             FROM set_state WHERE set_id = ?1",
                            params![set.0.to_string()],
                            |r| {
                                Ok((
                                    r.get::<_, i64>(0)? as u64,
                                    r.get::<_, i64>(1)? as u64,
                                    r.get::<_, i64>(2)? as u64,
                                ))
                            },
                        )
                        .optional()
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!(
                                "read set_state head/floors: {e}"
                            ))
                        })?
                        .unwrap_or((0, 0, 0));

                // `prune_changelog(ContainerChangeSet, 0)` deletes every
                // lifecycle row, dropping `MAX(container_change_set_seq)`
                // back to 0 while leaving the floor at the previous max.
                // The lifecycle head must monotonically reflect "highest
                // seq that has ever been observed", so lift the head to
                // `max(MAX, floor)` — otherwise a token at the floor
                // would be rejected as "future" before the stale check
                // can apply and the floor-equality contract would break.
                // The `set_change_seq` allocator counter in `set_state`
                // outlives row deletes and needs no such correction.
                let curr_lifecycle: u64 = lifecycle_max.max(lifecycle_floor);

                // RFC 8620 §5.2: a sinceState that is not a valid
                // prior state MUST yield cannotCalculateChanges. The
                // paired token has two heads; either being in the
                // future fails the same way. `since == head` is
                // valid (no changes since the last sync).
                if since_lifecycle > curr_lifecycle {
                    return Err(cosmix_mds::Error::Other(format!(
                        "future since_state: lifecycle {since_lifecycle} > current {curr_lifecycle}"
                    )));
                }
                if since_set_change > curr_set_change {
                    return Err(cosmix_mds::Error::Other(format!(
                        "future since_state: set_change {since_set_change} > current {curr_set_change}"
                    )));
                }
                // Either half being strictly below its retention
                // floor invalidates the paired token. `since ==
                // floor` is still answerable from `seq > since`
                // rows, so the predicate is strict `<`.
                if since_lifecycle < lifecycle_floor {
                    return Err(cosmix_mds::Error::Other(format!(
                        "stale since_state: lifecycle {since_lifecycle} < \
                         container_change_set_floor {lifecycle_floor}"
                    )));
                }
                if since_set_change < set_change_floor {
                    return Err(cosmix_mds::Error::Other(format!(
                        "stale since_state: set_change {since_set_change} < \
                         set_change_floor {set_change_floor}"
                    )));
                }

                // `max_changes == 0` is a "have any events happened
                // since the given state" probe. Return an empty page
                // that pins `new_state` at the request token and
                // signals `has_more_changes` iff either head has
                // moved. JMAP rejects `maxChanges == 0` upstream per
                // RFC 8620 §5.2 PositiveInt; this branch defends the
                // trait against internal callers (and a future IMAP
                // path) that legitimately want a zero-cost peek.
                if max == 0 {
                    let has_more =
                        since_lifecycle < curr_lifecycle || since_set_change < curr_set_change;
                    return Ok(MailboxChangeSet {
                        new_state: format!("{}:{}", since_lifecycle, since_set_change),
                        has_more_changes: has_more,
                        created: Vec::new(),
                        updated: Vec::new(),
                        destroyed: Vec::new(),
                        updated_properties: None,
                    });
                }

                // Staging container id (if provisioned). Same filter
                // `list_mailboxes` applies — lifecycle and count-stream
                // events targeting `__upload_staging__` are internal
                // and must not surface as JMAP `Mailbox/changes`
                // deltas.
                let staging_id: Option<ContainerId> = tx
                    .tx()
                    .query_row(
                        "SELECT id FROM container WHERE parent_id IS NULL AND name = ?1",
                        params![UPLOAD_STAGING_NAME],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!("staging container lookup: {e}"))
                    })?
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .map(ContainerId);

                // Page the lifecycle stream `since_lifecycle <
                // container_change_set_seq <= curr_lifecycle`, LIMIT
                // `max + 1` to detect truncation. ASC order is
                // load-bearing: precedence rules within a container's
                // events depend on seeing CREATED before DESTROYED
                // in the same window.
                let mut stmt = tx
                    .tx()
                    .prepare(
                        "SELECT container_change_set_seq, container_id, kind \
                         FROM container_change_set \
                         WHERE container_change_set_seq > ?1 \
                           AND container_change_set_seq <= ?2 \
                         ORDER BY container_change_set_seq ASC \
                         LIMIT ?3",
                    )
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "prepare lifecycle page query: {e}"
                        ))
                    })?;
                let page_limit_with_probe = (max as i64).saturating_add(1);
                let lifecycle_rows = stmt
                    .query_map(
                        params![
                            since_lifecycle as i64,
                            curr_lifecycle as i64,
                            page_limit_with_probe,
                        ],
                        |r| {
                            let seq: i64 = r.get(0)?;
                            let cid: String = r.get(1)?;
                            let kind: String = r.get(2)?;
                            Ok((seq as u64, cid, kind))
                        },
                    )
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "query lifecycle page: {e}"
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| {
                        cosmix_mds::Error::Other(format!(
                            "scan lifecycle page rows: {e}"
                        ))
                    })?;
                drop(stmt);

                let truncated = lifecycle_rows.len() > max;
                let lifecycle_slice: Vec<(u64, String, String)> = if truncated {
                    // Drop the probe row; the highest seq we will
                    // actually ack to the client is the last one in
                    // the kept slice.
                    lifecycle_rows.into_iter().take(max).collect()
                } else {
                    lifecycle_rows
                };

                // Per-container lifecycle classification. Order of
                // insertion mirrors row order so JMAP-visible
                // `created`/`updated`/`destroyed` listings keep a
                // stable seq-ascending order.
                use std::collections::{HashMap, HashSet};
                #[derive(Default)]
                struct Status {
                    created: bool,
                    destroyed: bool,
                    prop_updated: bool,
                }
                let mut status: HashMap<ContainerId, Status> = HashMap::new();
                let mut first_seen_order: Vec<ContainerId> = Vec::new();
                let mut seen: HashSet<ContainerId> = HashSet::new();
                let mut highest_seen_seq: u64 = since_lifecycle;
                for (seq, cid_str, kind_str) in &lifecycle_slice {
                    highest_seen_seq = (*seq).max(highest_seen_seq);
                    let Ok(uuid) = Uuid::parse_str(cid_str) else {
                        continue;
                    };
                    let cid = ContainerId(uuid);
                    if Some(cid) == staging_id {
                        continue;
                    }
                    if seen.insert(cid) {
                        first_seen_order.push(cid);
                    }
                    let s = status.entry(cid).or_default();
                    match kind_str.as_str() {
                        "CONTAINER_CREATED" => s.created = true,
                        "CONTAINER_DESTROYED" => s.destroyed = true,
                        "CONTAINER_RENAMED" | "CONTAINER_ATTRS_CHANGED" => {
                            s.prop_updated = true;
                        }
                        // Unknown kinds are tolerated (forward
                        // compatibility) but contribute no
                        // classification signal.
                        _ => {}
                    }
                }

                let mut created: Vec<ContainerId> = Vec::new();
                let mut updated: Vec<ContainerId> = Vec::new();
                let mut destroyed: Vec<ContainerId> = Vec::new();
                let mut lifecycle_classified: HashSet<ContainerId> = HashSet::new();

                for cid in &first_seen_order {
                    let s = &status[cid];
                    lifecycle_classified.insert(*cid);
                    match (s.created, s.destroyed, s.prop_updated) {
                        // Created and destroyed within the window —
                        // RFC 8621 §2.6: client never observed it,
                        // omit from all three arrays.
                        (true, true, _) => {}
                        (true, false, _) => created.push(*cid),
                        (false, true, _) => destroyed.push(*cid),
                        (false, false, true) => updated.push(*cid),
                        // No signal — defensive, should not occur
                        // (kind text passed the CHECK constraint on
                        // INSERT).
                        (false, false, false) => {}
                    }
                }

                // Count synthesis runs only on the final page
                // (lifecycle stream drained). Truncated pages defer
                // it to the follow-up call once the lifecycle
                // cursor catches up — preserves the substrate
                // invariant that `new_state`'s set_change half is
                // either `since_set_change` (no count signal seen)
                // or `curr_set_change` (every count signal in window
                // has been classified).
                let new_set_change = if truncated {
                    since_set_change
                } else {
                    // Drain the set_change stream in pages — mirror
                    // email_changes' loop so a huge backlog still
                    // walks correctly.
                    const PAGE_LIMIT: usize = u32::MAX as usize;
                    let mut cursor = SetChangeToken(since_set_change);
                    let mut updated_extras: Vec<ContainerId> = Vec::new();
                    let mut count_seen: HashSet<ContainerId> = HashSet::new();
                    loop {
                        let (page, next) = tx.changes_since_set(cursor, PAGE_LIMIT)?;
                        for c in &page {
                            let cid = c.container_id;
                            if Some(cid) == staging_id {
                                continue;
                            }
                            if lifecycle_classified.contains(&cid) {
                                // Lifecycle classification wins per
                                // the plan's precedence rules. The
                                // count-signal-implies-updated-properties=null
                                // semantic is folded into the
                                // Phase-2 blanket `None` below, so
                                // no extra bookkeeping needed.
                                continue;
                            }
                            if count_seen.insert(cid) {
                                updated_extras.push(cid);
                            }
                        }
                        match next {
                            Some(t) => cursor = t,
                            None => break,
                        }
                    }
                    // Verify the staging container itself never
                    // surfaces in lifecycle output even if the
                    // staging-id lookup raced its first creation
                    // — defense-in-depth.
                    debug_assert!(staging_id
                        .map(|sid| !created.contains(&sid)
                            && !updated.contains(&sid)
                            && !destroyed.contains(&sid)
                            && !updated_extras.contains(&sid))
                        .unwrap_or(true));
                    updated.extend(updated_extras);
                    curr_set_change
                };

                let new_lifecycle = if truncated {
                    highest_seen_seq
                } else {
                    curr_lifecycle
                };

                Ok(MailboxChangeSet {
                    new_state: format!("{new_lifecycle}:{new_set_change}"),
                    has_more_changes: truncated,
                    created,
                    updated,
                    destroyed,
                    // RFC 8621 §2.6 null projection: clients refresh
                    // all properties for every `updated` id. Phase 2
                    // takes the conservative end of the null-or-list
                    // contract uniformly; a refinement to emit the
                    // intersection across entries is deferred.
                    updated_properties: None,
                })
            });

        match result {
            Ok(cs) => Ok(cs),
            // Unprovisioned account: mirror email_changes' zero-baseline
            // — `since_state == "0:0"` against an empty account is a
            // valid first-sync call and returns an empty delta. Any
            // non-zero half against an unprovisioned set is a
            // future-state error.
            Err(cosmix_mds::Error::SetNotFound(_))
                if since_lifecycle == 0 && since_set_change == 0 =>
            {
                Ok(MailboxChangeSet {
                    new_state: "0:0".to_string(),
                    has_more_changes: false,
                    created: Vec::new(),
                    updated: Vec::new(),
                    destroyed: Vec::new(),
                    updated_properties: None,
                })
            }
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "future since_state: {since_state:?} against unprovisioned account"
            )),
            // Pre-shaped "future since_state" / "stale since_state" /
            // "invalid sinceState" messages from inside the closure
            // carry the trait-prefix vocabulary already; unwrap
            // Error::Other so the JMAP handler's prefix match fires
            // cleanly. "stale since_state" is the MCS Phase 3-B
            // retention-floor reject — a paired-token half is strictly
            // below its `set_state` floor column after a prune.
            Err(cosmix_mds::Error::Other(s))
                if s.starts_with("future since_state")
                    || s.starts_with("stale since_state")
                    || s.starts_with("invalid sinceState") =>
            {
                Err(anyhow!(s))
            }
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn list_thread(&self, account: AccountId, thread_id: ThreadId) -> Result<Vec<ItemId>> {
        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(
            &set,
            |tx| -> std::result::Result<Vec<ItemId>, cosmix_mds::Error> {
                // LEFT JOIN `mail_envelopes` so items predating Task 1.7's
                // create_email (no envelope row) still surface. The
                // `CASE WHEN e.date_ts IS NULL THEN 1 ELSE 0 END` guard
                // partitions the result into two ordered buckets — bucket 0
                // for envelope-bearing rows (sort by `date_ts`), bucket 1
                // for bare items (sort by `received_at`). The two columns
                // are never compared against each other, so the unit
                // mismatch between `date_ts` (seconds) and `received_at`
                // (milliseconds) is irrelevant to correctness. Tie-break on
                // `item.id` (TEXT) to keep the order deterministic across
                // identical timestamps within a bucket. The INNER JOIN on
                // `item` is load-bearing for the `i.received_at` ordering
                // and the `i.id` tie-break. As of v1.8 it no longer
                // *filters* any row: `mail_threads.item_id` carries
                // `REFERENCES item(id) ON DELETE CASCADE`, so dangling rows
                // are reaped on item delete rather than skipped here. The
                // `#[cfg(debug_assertions)]` probe below pins that
                // invariant — the joined result must equal the raw count.
                let mut stmt = tx
                    .tx()
                    .prepare(
                        "SELECT mt.item_id \
                     FROM mail_threads mt \
                     JOIN item i ON i.id = mt.item_id \
                     LEFT JOIN mail_envelopes e ON e.item_id = mt.item_id \
                     WHERE mt.account_id = ?1 AND mt.thread_id = ?2 \
                     ORDER BY \
                         CASE WHEN e.date_ts IS NULL THEN 1 ELSE 0 END ASC, \
                         e.date_ts ASC, \
                         i.received_at ASC, \
                         i.id ASC",
                    )
                    .map_err(|e| cosmix_mds::Error::Other(format!("prepare list_thread: {e}")))?;
                let rows = stmt
                    .query_map(params![account, thread_id.0], |r| r.get::<_, String>(0))
                    .map_err(|e| cosmix_mds::Error::Other(format!("query list_thread: {e}")))?;
                let mut out = Vec::new();
                for row in rows {
                    let s =
                        row.map_err(|e| cosmix_mds::Error::Other(format!("row list_thread: {e}")))?;
                    let uuid = Uuid::parse_str(&s).map_err(|e| {
                        cosmix_mds::Error::Other(format!("invalid item_id in mail_threads: {e}"))
                    })?;
                    out.push(ItemId(uuid));
                }
                #[cfg(debug_assertions)]
                {
                    // Post-v1.8 invariant: the FK cascade reaps dangling
                    // `mail_threads` rows on item delete, so the `JOIN item`
                    // above must drop nothing — the raw row count for this
                    // (account, thread) equals the joined result length.
                    let raw: i64 = tx
                        .tx()
                        .query_row(
                            "SELECT COUNT(*) FROM mail_threads \
                         WHERE account_id = ?1 AND thread_id = ?2",
                            params![account, thread_id.0],
                            |r| r.get(0),
                        )
                        .map_err(|e| {
                            cosmix_mds::Error::Other(format!("count mail_threads: {e}"))
                        })?;
                    debug_assert_eq!(
                        raw as usize,
                        out.len(),
                        "mail_threads dangling rows after v1.8 cascade"
                    );
                }
                Ok(out)
            },
        );

        match result {
            Ok(items) => Ok(items),
            Err(cosmix_mds::Error::SetNotFound(_)) => Err(anyhow!(
                "mailbox not found: account {account} unprovisioned"
            )),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn list_threads(&self, account: AccountId, position: u64, limit: u64) -> Result<Vec<ThreadId>> {
        // sqlite LIMIT/OFFSET cast is i64; saturate to keep the
        // query well-formed for any plausible u64. Real callers
        // pass <= 100 (Thread/get cap).
        let position = position.min(i64::MAX as u64) as i64;
        let limit = limit.min(i64::MAX as u64) as i64;

        let set = account_id_to_setid(account);
        let result = self.mds.with_set_tx(
            &set,
            |tx| -> std::result::Result<Vec<ThreadId>, cosmix_mds::Error> {
                // INNER JOIN on `item` — same discipline as `list_thread`.
                // As of v1.8 the FK cascade reaps dangling `mail_threads`
                // rows on item delete, so this join no longer filters; it
                // stays as a post-v1.8 debug invariant and to keep the two
                // listing methods structurally identical. DISTINCT collapses
                // multi-member threads to one row each. ORDER BY
                // thread_id keeps pagination stable across calls.
                let mut stmt = tx
                    .tx()
                    .prepare(
                        "SELECT DISTINCT mt.thread_id \
                     FROM mail_threads mt \
                     JOIN item i ON i.id = mt.item_id \
                     WHERE mt.account_id = ?1 \
                     ORDER BY mt.thread_id ASC \
                     LIMIT ?2 OFFSET ?3",
                    )
                    .map_err(|e| cosmix_mds::Error::Other(format!("prepare list_threads: {e}")))?;
                let rows = stmt
                    .query_map(params![account, limit, position], |r| r.get::<_, String>(0))
                    .map_err(|e| cosmix_mds::Error::Other(format!("query list_threads: {e}")))?;
                let mut out = Vec::new();
                for row in rows {
                    let s = row
                        .map_err(|e| cosmix_mds::Error::Other(format!("row list_threads: {e}")))?;
                    out.push(ThreadId(s));
                }
                Ok(out)
            },
        );

        match result {
            Ok(ids) => Ok(ids),
            // Mirror `email_state`/`email_changes(0)`'s zero-baseline
            // for unprovisioned accounts: an empty thread list is the
            // right answer for an account that has never received
            // mail, not an error the client never asked about.
            Err(cosmix_mds::Error::SetNotFound(_)) => Ok(Vec::new()),
            Err(other) => Err(anyhow!(other)),
        }
    }

    fn subscribe_mailbox(
        &self,
        account: AccountId,
        mailbox: ContainerId,
    ) -> Result<broadcast::Receiver<ContainerEvent>> {
        let set = account_id_to_setid(account);
        // `subscribe_existing` holds the per-set Mutex across the
        // container-exists check and the notifier subscribe, so a
        // concurrent `delete_container` cannot drop the real channel
        // between our verification and `Sender::subscribe` and leave
        // us listening on a resurrected, never-published one. The
        // plain `Mds::subscribe` lazily allocates a `Sender` for any
        // (set, container) key, which would silently hand IMAP IDLE
        // a dead channel on either a typo or a delete-after-status
        // race. Maps both `SetNotFound` and `ContainerNotFound` to
        // the same `"mailbox not found"` sentinel used by every
        // other mailbox-parametric MailStore method.
        match self.mds.subscribe_existing(&set, &mailbox) {
            Ok(rx) => Ok(rx),
            Err(cosmix_mds::Error::SetNotFound(_))
            | Err(cosmix_mds::Error::ContainerNotFound(_)) => {
                Err(anyhow!("mailbox not found: {id}", id = mailbox.0))
            }
            Err(e) => Err(anyhow!(e)),
        }
    }
}

/// Well-known key in [`ContainerAttrs::extra`] carrying JMAP
/// `Mailbox.sortOrder`. Stored as a JSON number (any integer type);
/// adapter narrows to `u32` on read, saturating non-finite or
/// negative values to 0 — JMAP `UnsignedInt` (RFC 8621 §2) is
/// 53-bit unsigned, so the saturation never truncates valid data
/// and `MailboxRecord::sort_order` keeps the stable wire range.
const JMAP_SORT_ORDER_KEY: &str = "jmap_sort_order";

/// Well-known key in [`ContainerAttrs::extra`] carrying JMAP
/// `Mailbox.myRights`. Stored as a JSON object whose field names
/// match the snake-case [`MailboxRights`] field names. A row whose
/// `extra` does not contain this key (the v1 / v1.1 / v1.2 default)
/// projects as [`MailboxRights::full`] — owner-of-mailbox semantics
/// for the personal-substrate case.
const JMAP_MY_RIGHTS_KEY: &str = "jmap_my_rights";

/// Build a [`ContainerAttrs`] payload for the JMAP `Mailbox/set`
/// create path. Encapsulates the on-disk encoding (special-use
/// attribute string, `extra.jmap_sort_order`) so JMAP-layer callers
/// don't have to know the well-known keys directly. Symmetric writer
/// to [`jmap_sort_order_from_attrs`] / [`MailboxRole::from_special_use`]
/// — every value emitted here round-trips through `list_mailboxes`'s
/// reader path.
///
/// `my_rights` is intentionally not exposed: every mailbox in the
/// personal-substrate model is owner-of-mailbox, and writing the
/// `jmap_my_rights` key would just bloat `attrs.extra` with the
/// reader's default. Shared-mailbox ACLs land separately and will
/// extend this builder rather than the JMAP layer.
pub(crate) fn jmap_attrs_for_create(
    role: Option<MailboxRole>,
    sort_order: u32,
    is_subscribed: bool,
) -> ContainerAttrs {
    let mut extra_map = serde_json::Map::new();
    if sort_order != 0 {
        extra_map.insert(JMAP_SORT_ORDER_KEY.to_string(), sort_order.into());
    }
    let extra = serde_json::Value::Object(extra_map);
    ContainerAttrs {
        special_use: role.map(|r| r.as_special_use().to_string()),
        subscribed: is_subscribed,
        extra,
    }
}

/// Read JMAP `Mailbox.sortOrder` from `attrs.extra`. Default 0 when
/// the key is absent or the value is not a non-negative integer that
/// fits in `u32`. Saturating-to-zero rather than erroring keeps a
/// row with a bogus `extra` payload visible in `Mailbox/get` instead
/// of silently dropping it from the listing.
///
/// JSON floats (`7.0`) are treated as malformed and default to 0:
/// every writer here mints the field as `serde_json::json!(<u32>)`
/// (a JSON integer), so a float on the wire signals a foreign
/// rewrite and the safer projection is the documented default.
pub(crate) fn jmap_sort_order_from_attrs(attrs: &ContainerAttrs) -> u32 {
    attrs
        .extra
        .as_object()
        .and_then(|m| m.get(JMAP_SORT_ORDER_KEY))
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Read JMAP `Mailbox.myRights` from `attrs.extra`. The on-disk
/// schema mirrors [`MailboxRights`]: each per-bit field is an
/// optional bool whose absence means "default to the full-rights
/// value." A missing or malformed `jmap_my_rights` object falls
/// through to [`MailboxRights::full`] entirely. The bit-by-bit
/// fallback is deliberate: a forward-compatibility guard for a row
/// rewritten by a tool that knows about a subset of the rights
/// fields. Keeping unknown bits at the full default mirrors the
/// "absent key = default" rule used elsewhere on `attrs.extra`.
fn jmap_my_rights_from_attrs(attrs: &ContainerAttrs) -> MailboxRights {
    let full = MailboxRights::full();
    let Some(obj) = attrs
        .extra
        .as_object()
        .and_then(|m| m.get(JMAP_MY_RIGHTS_KEY))
        .and_then(|v| v.as_object())
    else {
        return full;
    };
    let bit = |name: &str, fallback: bool| -> bool {
        obj.get(name).and_then(|v| v.as_bool()).unwrap_or(fallback)
    };
    MailboxRights {
        may_read_items: bit("may_read_items", full.may_read_items),
        may_add_items: bit("may_add_items", full.may_add_items),
        may_remove_items: bit("may_remove_items", full.may_remove_items),
        may_set_seen: bit("may_set_seen", full.may_set_seen),
        may_set_keywords: bit("may_set_keywords", full.may_set_keywords),
        may_create_child: bit("may_create_child", full.may_create_child),
        may_rename: bit("may_rename", full.may_rename),
        may_delete: bit("may_delete", full.may_delete),
        may_submit: bit("may_submit", full.may_submit),
    }
}

/// Remove aged memberships from the container with `special_use` (one
/// retention delete-bucket — `\Junk` or `\Trash`), inside the caller's
/// set transaction. Returns `(removed, capped)` where `removed` is the
/// count actually removed (or, in `dry_run`, that WOULD be removed) and
/// `capped` means more aged memberships were eligible than `cap` allowed.
///
/// The candidate query is deterministically ordered
/// (`ORDER BY added_at ASC, item_id`) so "oldest up to the cap" is
/// well-defined, and it probes `cap + 1` rows so the `capped` flag is
/// computed without a second count query. Selection and removal share
/// this one tx (the candidate set cannot shift between them). Each
/// removal reaps the FTS row if it orphaned the item; a concurrent
/// removal (membership already gone) is skipped, mirroring
/// `expunge_memberships`.
fn sweep_role_in_tx(
    tx: &mut cosmix_mds::SqliteSetTx<'_>,
    special_use: &str,
    cutoff_ms: i64,
    cap: u64,
    dry_run: bool,
) -> std::result::Result<(u64, bool), cosmix_mds::Error> {
    if cap == 0 {
        return Ok((0, false));
    }
    let container = match lookup_special_use_container(tx.tx(), special_use)? {
        Some(c) => c,
        // Account has no such folder configured — clean no-op.
        None => return Ok((0, false)),
    };
    // Probe one past the cap so we can report `capped` without a second
    // COUNT query.
    let probe_limit = cap.saturating_add(1);
    let aged = collect_aged_items_in_container(tx.tx(), &container, cutoff_ms, probe_limit)?;
    let capped = aged.len() as u64 > cap;
    let to_process: Vec<ItemId> = aged.into_iter().take(cap as usize).collect();
    if to_process.is_empty() {
        return Ok((0, false));
    }
    if dry_run {
        // Count what WOULD be removed; mutate nothing (no membership
        // removal → no set_change allocated, so a dry-run sweep is a
        // pure read).
        return Ok((to_process.len() as u64, capped));
    }
    let mut removed = 0u64;
    for id in &to_process {
        match tx.remove_membership(id, &container) {
            Ok(()) => {
                removed += 1;
                // If that was the item's last membership, the `item` row
                // is gone; reap its FTS row in the same tx (no-op when
                // the item still has memberships elsewhere — the
                // multi-homed case).
                reap_mail_search_if_item_gone(tx.tx(), id)?;
            }
            // Concurrent removal (another session expunged it first):
            // the membership is already gone. Skip — mirrors
            // `expunge_memberships`'s tolerance of the same race.
            Err(cosmix_mds::Error::ItemNotFound(_)) => continue,
            Err(cosmix_mds::Error::Other(ref s))
                if s.starts_with("membership ") && s.contains("not present") =>
            {
                continue;
            }
            Err(other) => return Err(other),
        }
    }
    Ok((removed, capped))
}

/// Enumerate item ids membership-bound to `container` whose `added_at`
/// is strictly older than `cutoff_ms`, oldest first, capped at `limit`.
/// Backs the retention sweep ([`sweep_role_in_tx`]); the deterministic
/// `ORDER BY added_at ASC, item_id` makes "the oldest N" well-defined,
/// and the `(container_id, added_at)` index (`idx_mbr_container_added_at`)
/// serves the predicate + order.
fn collect_aged_items_in_container(
    tx: &rusqlite::Transaction<'_>,
    container: &ContainerId,
    cutoff_ms: i64,
    limit: u64,
) -> std::result::Result<Vec<ItemId>, cosmix_mds::Error> {
    let mut stmt = tx
        .prepare(
            "SELECT item_id FROM membership \
             WHERE container_id = ?1 AND added_at < ?2 \
             ORDER BY added_at ASC, item_id LIMIT ?3",
        )
        .map_err(|e| {
            cosmix_mds::Error::Other(format!("prepare collect_aged_items_in_container: {e}"))
        })?;
    let rows = stmt
        .query_map(
            params![container.0.to_string(), cutoff_ms, limit as i64],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| {
            cosmix_mds::Error::Other(format!("query collect_aged_items_in_container: {e}"))
        })?;
    let mut out = Vec::new();
    for r in rows {
        let s = r.map_err(|e| {
            cosmix_mds::Error::Other(format!("row collect_aged_items_in_container: {e}"))
        })?;
        let uuid = Uuid::parse_str(&s).map_err(|e| {
            cosmix_mds::Error::Other(format!("membership.item_id {s:?} is not a UUID: {e}"))
        })?;
        out.push(ItemId(uuid));
    }
    Ok(out)
}

/// Enumerate the items currently membership-bound to `container` in
/// the caller's transaction. Used by `destroy_mailbox`'s
/// `MoveToInbox` and `Delete` policies. Order is unspecified — the
/// callers iterate to apply per-item membership operations and are
/// not order-sensitive.
fn collect_items_in_container(
    tx: &rusqlite::Transaction<'_>,
    container: &ContainerId,
) -> std::result::Result<Vec<ItemId>, cosmix_mds::Error> {
    let mut stmt = tx
        .prepare("SELECT item_id FROM membership WHERE container_id = ?1")
        .map_err(|e| {
            cosmix_mds::Error::Other(format!("prepare collect_items_in_container: {e}"))
        })?;
    let rows = stmt
        .query_map(params![container.0.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| cosmix_mds::Error::Other(format!("query collect_items_in_container: {e}")))?;
    let mut out = Vec::new();
    for r in rows {
        let s = r.map_err(|e| {
            cosmix_mds::Error::Other(format!("row collect_items_in_container: {e}"))
        })?;
        let uuid = Uuid::parse_str(&s).map_err(|e| {
            cosmix_mds::Error::Other(format!("membership.item_id {s:?} is not a UUID: {e}"))
        })?;
        out.push(ItemId(uuid));
    }
    Ok(out)
}

/// Cheap membership existence check inside the caller's tx. Used by
/// `destroy_mailbox`'s `MoveToInbox` to detect items that are already
/// in the destination inbox so it can fall back to removing just the
/// source membership rather than calling `move_item` (which rejects
/// duplicate destination memberships).
fn membership_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    item: &ItemId,
    container: &ContainerId,
) -> std::result::Result<bool, cosmix_mds::Error> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM membership \
             WHERE item_id = ?1 AND container_id = ?2",
            params![item.0.to_string(), container.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| cosmix_mds::Error::Other(format!("membership_exists_tx: {e}")))?;
    Ok(n > 0)
}

/// Phase 8e gate-2 mail_search reap for paths whose `remove_membership`
/// call MAY drop the underlying item row (last-membership case) or
/// MAY leave it (other memberships remain). FTS5 virtual tables
/// cannot carry an FK cascade, so the maild adapter reaps explicitly
/// inside the same `with_set_tx` scope.
///
/// The `NOT EXISTS (SELECT 1 FROM item WHERE id = ?)` guard makes
/// this idempotent: if the item still has memberships in other
/// mailboxes, its FTS row stays; if the item row is gone, the FTS
/// row is dropped. Tolerates the legacy "no FTS row yet" case
/// (gate 3 rebuild not yet run) — a no-op DELETE returns Ok(0).
fn reap_mail_search_if_item_gone(
    tx: &rusqlite::Transaction<'_>,
    item: &ItemId,
) -> std::result::Result<(), cosmix_mds::Error> {
    let item_str = item.0.to_string();
    tx.execute(
        "DELETE FROM mail_search \
         WHERE item_id = ?1 \
           AND NOT EXISTS (SELECT 1 FROM item WHERE id = ?1)",
        params![item_str],
    )
    .map_err(|e| cosmix_mds::Error::Other(format!("reap mail_search: {e}")))?;
    Ok(())
}

/// Find the container whose `attrs.special_use` JSON field matches
/// `special_use`. Returns `Ok(None)` if no such container exists in
/// this set (e.g. the account has no `\Junk` mailbox configured).
fn lookup_special_use_container(
    tx: &rusqlite::Transaction<'_>,
    special_use: &str,
) -> std::result::Result<Option<ContainerId>, cosmix_mds::Error> {
    let row = tx.query_row(
        "SELECT id FROM container \
         WHERE json_extract(attrs, '$.special_use') = ?1 \
         LIMIT 1",
        params![special_use],
        |r| r.get::<_, String>(0),
    );
    let id_str = match row {
        Ok(s) => s,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => {
            return Err(cosmix_mds::Error::Other(format!(
                "lookup special_use {special_use:?}: {e}"
            )));
        }
    };
    let uuid = Uuid::parse_str(&id_str).map_err(|e| {
        cosmix_mds::Error::Other(format!("container.id {id_str:?} is not a UUID: {e}"))
    })?;
    Ok(Some(ContainerId(uuid)))
}
