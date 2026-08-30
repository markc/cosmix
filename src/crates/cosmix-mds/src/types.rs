//! Public types for the `Mds` trait surface.
//!
//! Definitions are taken verbatim from
//! `_doc/2026-04-29-cosmix-mds-spec.md` §Types. Per the spec's stability
//! commitment, `Flags`, `ChangeToken`, `Change`, `ContainerEvent`,
//! `Membership`, and the four `*Report` types are frozen across v1.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

// ---- Identifiers ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SetId(pub uuid::Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerId(pub uuid::Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId(pub uuid::Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobHash(pub [u8; 32]);

// ---- Sequencing ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Seq(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChangeToken(pub u64);

/// Opaque token identifying a position in the *set-wide* change
/// stream (`set_change.set_change_seq`). Distinct from the
/// per-container [`ChangeToken`] because the underlying counters are
/// allocated by different mechanisms (`container.change_seq` vs
/// `set_state.set_change_seq`); a `SetChangeToken` and a
/// `ChangeToken` with the same numeric value are unrelated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SetChangeToken(pub u64);

/// Opaque token identifying a position in the account-wide container
/// lifecycle stream (`container_change_set.container_change_set_seq`).
/// Distinct from [`ChangeToken`] (per-container item-scoped) and
/// [`SetChangeToken`] (set-wide item-scoped) because all three streams
/// are allocated by different mechanisms; tokens are not
/// interchangeable even if their numeric values collide.
///
/// `container_change_set_seq` is AUTOINCREMENT-allocated, so the
/// sequence is strictly monotonic but may have gaps after retention
/// sweeps. JMAP `Mailbox/changes` paging is `seq > ?`, gaps don't
/// matter — see `schema/v1_4.sql` and
/// `_doc/planned/mailbox-changes-substrate.md` §Design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContainerChangeSetToken(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeqRange {
    All,
    From(Seq),
    Range(Seq, Seq),
}

// ---- Flags ----

/// Per-membership system flag bitmap. Each allocated bit corresponds
/// to one IMAP system flag (RFC 9051 §2.3.2) and its JMAP keyword
/// counterpart (RFC 8621 §4.1.4, the "system keywords" set):
///
/// | Bit | IMAP flag | JMAP keyword |
/// |-----|-----------|--------------|
/// | 0   | `\Seen`     | `$seen`      |
/// | 1   | `\Flagged`  | `$flagged`   |
/// | 2   | `\Answered` | `$answered`  |
/// | 3   | `\Draft`    | `$draft`     |
/// | 4   | `\Deleted`  | (no JMAP equiv — IMAP EXPUNGE-only marker) |
///
/// Bits 5..31 are reserved. Unread accounting (see
/// `container::unread_delta_of`) reads bit 0 only, so allocating the
/// upper bits is non-breaking for derived counters. `\Recent` is
/// deliberately absent — RFC 9051 §2.3.2 forbids STORE from setting
/// it and the substrate does not surface it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Flags(pub u32);

impl Flags {
    /// `\Seen` / `$seen`. Set when the message has been read.
    pub const SEEN: u32 = 0x01;
    /// `\Flagged` / `$flagged`. User-marked as important.
    pub const FLAGGED: u32 = 0x02;
    /// `\Answered` / `$answered`. Reply has been sent.
    pub const ANSWERED: u32 = 0x04;
    /// `\Draft` / `$draft`. Message is a draft composition.
    pub const DRAFT: u32 = 0x08;
    /// `\Deleted`. Marked for EXPUNGE; no JMAP equivalent.
    pub const DELETED: u32 = 0x10;

    /// Mask of all currently-allocated system flag bits. Anything
    /// outside this mask is reserved; the substrate accepts it on the
    /// wire (`Flags(pub u32)` is transparent) but the IMAP and JMAP
    /// projection layers drop unmapped bits silently.
    pub const ALL_ALLOCATED: u32 =
        Self::SEEN | Self::FLAGGED | Self::ANSWERED | Self::DRAFT | Self::DELETED;
}

// ---- Tags ----

/// Per-membership user keywords (the JMAP `keywords` set minus the
/// system flags `\Seen` / `\Flagged` / `\Draft` / `\Answered` /
/// `\Junk`, which live in [`Flags`]).
///
/// `BTreeSet` semantics: lexicographically ordered, no duplicates.
/// Matches the `membership.tags` JSON storage discipline (sorted on
/// write, set-equality on read) and gives JMAP `keywords` consumers
/// a deterministic encoding for diffing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tags(pub BTreeSet<String>);

impl Tags {
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn contains(&self, t: &str) -> bool {
        self.0.contains(t)
    }
    pub fn insert(&mut self, t: String) -> bool {
        self.0.insert(t)
    }
    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, String> {
        self.0.iter()
    }
}

impl From<Vec<String>> for Tags {
    fn from(v: Vec<String>) -> Self {
        Self(v.into_iter().collect())
    }
}

impl From<Tags> for Vec<String> {
    fn from(t: Tags) -> Vec<String> {
        t.0.into_iter().collect()
    }
}

impl FromIterator<String> for Tags {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

// ---- Membership and reports ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub container: ContainerId,
    pub flags: Flags,
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placement {
    pub container: ContainerId,
    pub seq: Seq,
    pub change_seq: ChangeToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddReport {
    pub item_id: ItemId,
    pub placements: Vec<Placement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyReport {
    pub seq_dest: Seq,
    pub change_seq_dest: ChangeToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveReport {
    /// The source membership's `seq` (IMAP UID in `src`), captured
    /// just before the row was removed. Surfaces UIDPLUS-shaped
    /// COPYUID/MOVE return data without a second mds round-trip.
    pub seq_src: Seq,
    pub seq_dest: Seq,
    pub change_seq_src: ChangeToken,
    pub change_seq_dest: ChangeToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteReport {
    pub blob_count_unreffed: u64,
    pub duration_ms: u64,
}

// ---- Container metadata ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerAttrs {
    pub special_use: Option<String>,
    pub subscribed: bool,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: ContainerId,
    pub parent: Option<ContainerId>,
    pub name: String,
    pub attrs: ContainerAttrs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStatus {
    pub exists: u64,
    pub unread: u64,
    pub seq_validity: u64,
    pub next_seq: Seq,
    pub change_seq: ChangeToken,
}

// ---- Item shapes ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemMeta {
    pub blob_hash: BlobHash,
    pub size_bytes: u64,
    pub received_at: i64,
    pub cache_blob: Option<Vec<u8>>,
    pub cache_version: Option<String>,
}

/// Per-membership view of an item joined with its container row.
///
/// **Not persisted.** `ItemRecord` is constructed fresh from SQL on
/// every `fetch_item` / `list_items` call; the `Serialize` /
/// `Deserialize` derives exist for in-process payloads (Bus events,
/// CLI JSON output) and *not* for on-disk caches or wire snapshots.
/// Adding fields is therefore live-code-safe; it does NOT need a
/// version bump or migration the way a persisted shape would.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemRecord {
    /// Stable item identity. `list_items` populates this from the
    /// joined `item.id` (the SELECT already retrieved it); `fetch_item`
    /// populates it from the caller-supplied `item_id` since the join
    /// is keyed on it. JMAP-style listings need this to act on
    /// returned rows; omitting it in the original shape was a latent
    /// silent drop.
    pub id: ItemId,
    pub meta: ItemMeta,
    pub seq: Seq,
    pub change_seq: ChangeToken,
    pub flags: Flags,
    pub tags: Vec<String>,
    pub added_at: i64,
}

// ---- Changelog and events ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    MetadataChanged,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub change_seq: ChangeToken,
    pub kind: ChangeKind,
    pub seq: Seq,
    pub item_id: Option<ItemId>,
}

/// One row from the set-wide `set_change` stream. Carries *both*
/// tokens: `set_change_seq` for JMAP `Email/changes` pagination and
/// `container_change_seq` for IMAP/MODSEQ context when the consumer
/// needs to cross-reference with the per-container view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetChange {
    pub set_change_seq: SetChangeToken,
    pub container_id: ContainerId,
    pub container_change_seq: ChangeToken,
    pub item_id: ItemId,
    pub kind: ChangeKind,
    pub seq: Seq,
    pub changed_at: i64,
}

/// Kind discriminator for the `container_change_set` lifecycle
/// stream (account-wide container CRUD signal that powers JMAP
/// `Mailbox/changes` post-Phase-2).
///
/// `Renamed` covers any path-affecting mutation — name change,
/// parent change, or both. There is no separate `Reparented`: a
/// combined rename-and-reparent is one atomic SQL UPDATE in
/// `rename_container_in_tx`, and the substrate stream mirrors that
/// atomicity. The payload's `changed_props` array discriminates —
/// `["name"]`, `["parentId"]`, or `["name", "parentId"]`.
///
/// `AttrsChanged` covers JMAP-visible container properties
/// (`role`, `sortOrder`, `isSubscribed`); the writer is expected
/// to suppress no-op diffs (empty `changed_props`).
///
/// Wire encoding (text, not integer-coded — these events are rare,
/// the storage overhead is negligible, and a malformed write
/// surfaces at INSERT-time via the `CHECK` constraint rather than
/// at a read-side decode six months later) lives in
/// `schema/v1_4.sql`'s `kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContainerChangeSetKind {
    Created,
    Renamed,
    Destroyed,
    AttrsChanged,
}

/// One row from the account-wide container lifecycle stream
/// (`container_change_set`). `payload` is the raw JSON string from
/// the row — per-kind shape is documented in `schema/v1_4.sql` and
/// `_doc/planned/mailbox-changes-substrate.md` §Design. The
/// substrate ships writes only in Phase 1; consumers (JMAP read
/// path) cut over in Phase 2 and parse `payload` at that time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerChangeSetRow {
    pub container_change_set_seq: ContainerChangeSetToken,
    pub container_id: ContainerId,
    pub kind: ContainerChangeSetKind,
    pub payload: String,
    pub changed_at: i64,
}

/// Return value of `SqliteSetTx::update_container_attrs`. Carries the
/// JMAP-named subset of `["role", "sortOrder", "isSubscribed"]` that
/// actually changed between the stored attrs and the supplied patch.
/// Empty when the patch was a no-op — in that case no
/// `container_change_set` row is written and the SQL UPDATE is also
/// skipped (the per-set tx still commits cleanly, just without
/// touching the container row).
///
/// Strings are `&'static str` because they are drawn from a closed
/// set defined by the substrate itself (the spec's documented
/// `changed_props` vocabulary) — not from user input, not from the
/// caller's patch. The substrate is authoritative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatedContainerProps {
    pub changed_props: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerEvent {
    ItemAdded {
        item_id: ItemId,
        seq: Seq,
        change_seq: ChangeToken,
    },
    FlagsChanged {
        item_id: ItemId,
        change_seq: ChangeToken,
    },
    ItemRemoved {
        item_id: ItemId,
        seq: Seq,
        change_seq: ChangeToken,
    },
    /// Atomic-move correlation event. Delivered to subscribers of
    /// **both** `src` and `dest` containers, in addition to the
    /// per-container `ItemRemoved` (src) and `ItemAdded` (dest)
    /// events — `ItemMoved` is *additive*, not a replacement, so
    /// IMAP IDLE consumers can keep reacting to ItemRemoved/ItemAdded
    /// while JMAP / audit / cross-mailbox consumers get the richer
    /// paired shape with the four tokens needed to reconcile both
    /// the per-container and set-wide change streams.
    ItemMoved {
        item_id: ItemId,
        src: ContainerId,
        dest: ContainerId,
        change_seq_src: ChangeToken,
        change_seq_dest: ChangeToken,
        set_change_seq_src: SetChangeToken,
        set_change_seq_dest: SetChangeToken,
    },
}

// ---- Operational reports ----

#[derive(Debug, Clone)]
pub enum VerifyScope {
    Full,
    Since(std::time::SystemTime),
    Container(ContainerId),
}

/// Outcome of a `Mds::verify_blobs` sweep.
///
/// `mismatches` is the total of `mismatches_hash + mismatches_missing`
/// — kept as a single counter so simple consumers don't have to add,
/// and because the operator format `mismatches: 2 (1 hash-mismatch,
/// 1 missing)` reads naturally with both totals available.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub blobs_checked: u64,
    pub mismatches: u64,
    /// Blob whose on-disk bytes hashed to a different value
    /// (`blob_verify.status = 1`). True bitrot.
    pub mismatches_hash: u64,
    /// Blob whose CAS file was absent at verify time
    /// (`blob_verify.status = 2`). Usually a torn delivery,
    /// premature GC, or backup/restore mishap.
    pub mismatches_missing: u64,
    pub duration: std::time::Duration,
}

/// Outcome of a `Mds::export_set` call.
///
/// Covers a single set; bytes_written is the size of the produced
/// tarball, useful for the operator one-liner. Fields are stable:
/// the export wire shape is part of the operator surface.
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub set_id: SetId,
    pub item_count: u64,
    pub blob_count: u64,
    pub bytes_written: u64,
    pub duration: std::time::Duration,
}

/// Outcome of a `Mds::import_set` call.
///
/// Mirrors `ExportReport` shape. `bytes_read` is the size of the
/// source tarball, surfaced for the operator one-liner. `set_id` is
/// pulled from the manifest, not chosen by the caller — the import
/// command takes only `<TARBALL_PATH>`.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub set_id: SetId,
    pub item_count: u64,
    pub blob_count: u64,
    pub bytes_read: u64,
    pub duration: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub sets_scanned: u64,
    pub items_indexed: u64,
    pub blobs_indexed: u64,
    pub orphan_blobs_found: u64,
    pub duration: std::time::Duration,
}

/// Outcome of a `Mds::gc` sweep.
///
/// Marked `#[non_exhaustive]` because Phase 3 added five new fields
/// over the spec's original three and future tuning may add more.
/// Construction is therefore deliberately non-public for callers
/// outside this crate; consumers should match on the fields they
/// care about and ignore the rest.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct GcReport {
    /// Pass 2-confirmed unlinks (file removed AND `blob` row deleted).
    pub blobs_deleted: u64,
    /// Sum of `size_bytes` across `blobs_deleted`.
    pub bytes_freed: u64,
    pub duration: std::time::Duration,
    /// Number of `refcount=0` rows enumerated in Pass 1, before the
    /// quiescence wait and Pass 2 re-check filtered any out.
    pub candidates_pass1: u64,
    /// Pass 1 candidates whose refcount was `>0` again at Pass 2 —
    /// a delivery transaction raced and re-took the blob during
    /// the quiescence wait. The "thank god the wait was there"
    /// signal.
    pub skipped_re_referenced: u64,
    /// Pass 1 candidates whose `last_seen` advanced between
    /// enumeration and Pass 2 (the blob was touched during the
    /// quiescence wait but refcount didn't move). Conservative skip;
    /// a future sweep collects it if still unreferenced.
    pub skipped_re_touched: u64,
    /// Dangling `blob` rows whose on-disk file was already missing
    /// at Pass 2 — post-crash recovery from a process death between
    /// `unlink` and the row delete. Counted as cleanup, not as a
    /// blob deleted by this sweep.
    pub orphan_rows_swept: u64,
    /// Sanity check: `refcount_pending` is dormant in v1 (see plan
    /// rev 4 — no producer in v1). Non-zero means a future
    /// maintainer wired a producer without lifting the dormancy
    /// flag, or there's a bug. Logged at warn level by `gc()` when
    /// non-zero; surfaced here so an operator can notice without
    /// scraping logs.
    pub pending_rows_observed: u64,
}

/// Retention-prune target streams. Both are per-set (`data.sqlite`)
/// changelog streams used by the mailbox-changes substrate; see
/// `schema/v1_4.sql` (container_change_set) and `schema/v1_1.sql`
/// (set_change). The legacy per-container `container_change` table
/// (v1) is intentionally not represented here — its retention story
/// is outside MCS Phase 3 scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangelogStream {
    /// `container_change_set` (v1.4) — account-wide container
    /// lifecycle stream. Powers the lifecycle half of the JMAP
    /// `Mailbox/changes` paired token.
    ContainerChangeSet,
    /// `set_change` (v1.1) — set-wide item-event stream. Powers JMAP
    /// `Email/changes` and the count half of the JMAP
    /// `Mailbox/changes` paired token.
    SetChange,
}

impl ChangelogStream {
    /// Operator-readable name; matches the spec / Bus wire form for
    /// `mds.changelog.pruned.stream`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangelogStream::ContainerChangeSet => "container_change_set",
            ChangelogStream::SetChange => "set_change",
        }
    }
}

impl std::fmt::Display for ChangelogStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of one `prune_changelog` invocation.
///
/// `rows_removed` is the count of rows the DELETE actually touched;
/// `new_floor` is the post-sweep retention watermark (highest seq
/// that was deleted, i.e. the largest seq no longer in the stream).
/// On a no-op (table shorter than `keep_n`) `rows_removed` is zero
/// and `new_floor` is the unchanged current floor — which may be
/// non-zero from a prior prune.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub rows_removed: u64,
    pub new_floor: u64,
}

#[derive(Debug, Clone)]
pub struct MdsStats {
    pub set_count: u64,
    pub container_count: u64,
    pub item_count: u64,
    pub blob_count: u64,
    pub total_bytes: u64,
    pub dedup_ratio: f64,
}

/// Per-set breakdown returned by `Mds::stats_per_set`.
///
/// Field semantics:
/// - `container_count` / `item_count` come from this set's
///   `data.sqlite` (no cross-set roll-up).
/// - `blob_count` is the number of *distinct* blob hashes
///   referenced by `item` rows in this set. Cross-set sharing
///   is not deducted: a blob shared with another set still
///   contributes 1 here. This matches the operator intuition
///   of "how many distinct blobs would I have to read to
///   reconstruct this set."
/// - `total_bytes` is the *logical* size of items in the set
///   (`SUM(item.size_bytes)`). Logical (not physical-distinct)
///   keeps the column meaningful even when a single blob is
///   referenced by multiple items in the set, and matches the
///   "if I had to copy this set out, this is how much I'd
///   write" intuition.
#[derive(Debug, Clone)]
pub struct PerSetStats {
    pub set_id: SetId,
    pub container_count: u64,
    pub item_count: u64,
    pub blob_count: u64,
    pub total_bytes: u64,
}
