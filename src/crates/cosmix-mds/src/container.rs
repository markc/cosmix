//! Per-set `data.sqlite` operations: container CRUD + status reads.
//!
//! Item / membership / change-log writes land in Phase 1b. This file
//! covers the container-lifecycle subset of the trait.

use crate::blob;
use crate::bus::{
    ContainerCreated, ContainerDeleted, ContainerRenamed, EventSink, ItemAdded, ItemCopied,
    ItemFlagged, ItemMoved, ItemRemoved, MdsEvent,
};
use crate::error::{Error, Result};
use crate::notifier::Notifier;
use crate::types::*;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

/// Reserved name for the per-account upload-staging container in
/// cosmix-maild (Phase 8d). MDS itself does not interpret this name —
/// reservation is documented here so a future caller cannot
/// accidentally collide it via normal mailbox creation. The leading +
/// trailing underscores are deliberately not legal in IMAP/JMAP
/// mailbox naming.
pub const UPLOAD_STAGING_NAME: &str = "__upload_staging__";

/// Per-`SqliteSetTx` event buffer (Phase 8d.1). Typed `SqliteSetTx::*`
/// methods append events here instead of calling
/// [`Notifier::publish`] / [`EventSink::emit`] directly. The buffer is
/// drained by [`crate::store::SqliteCasMds::with_set_tx`] **only after
/// the SQL commit succeeds**; on rollback or panic, the `SqliteSetTx`
/// (and this buffer) are dropped without replay.
///
/// **Why not eager publish.** Inside a `with_set_tx` closure, the SQL
/// transaction is still open. An eager `notifier.publish` would leak
/// "ghost events" to subscribers when the closure later returns
/// `Err(_)`, panics, or the commit itself fails — JMAP `Email/changes`
/// or Bus audit consumers acting on a ghost event would diverge from
/// on-disk state. Buffering preserves the "subscribers never see
/// events that later fail to land" rule from the v1 spec.
#[derive(Default)]
pub struct BufferedEvents {
    notifier: Vec<(SetId, ContainerId, ContainerEvent)>,
    /// Per-container broadcast channels to close in the post-commit
    /// drain. Mirrors the public `Mds::delete_container` ordering
    /// (close subscribers, then publish `ContainerDeleted`) but
    /// deferred so subscribers cannot observe a teardown that the tx
    /// later rolls back.
    drop_channels: Vec<(SetId, ContainerId)>,
    sink: Vec<MdsEvent>,
}

impl BufferedEvents {
    pub fn new() -> Self {
        Self::default()
    }

    fn push_notifier(&mut self, set: SetId, container: ContainerId, ev: ContainerEvent) {
        self.notifier.push((set, container, ev));
    }

    fn push_drop_channel(&mut self, set: SetId, container: ContainerId) {
        self.drop_channels.push((set, container));
    }

    fn push_sink(&mut self, ev: MdsEvent) {
        self.sink.push(ev);
    }

    /// Replay buffered events in original order. Called by
    /// `with_set_tx` *after* `tx.commit()` succeeds. Notifier events
    /// fire first (per-container channels), then any deferred
    /// channel-closes, then Bus events — same ordering as the
    /// existing eager-publish-after-commit paths (a deletion drops
    /// its channel before `ContainerDeleted` fires on Bus).
    pub fn drain_into(self, notifier: &Notifier, sink: &EventSink) {
        for (set, container, ev) in self.notifier {
            notifier.publish(&set, &container, ev);
        }
        for (set, container) in self.drop_channels {
            notifier.drop_channel(&set, &container);
        }
        for ev in self.sink {
            sink.emit(ev);
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn map_sql_err(prefix: &str, e: rusqlite::Error) -> Error {
    use rusqlite::ffi;

    match e {
        rusqlite::Error::SqliteFailure(ffi::Error { extended_code, .. }, msg) => {
            let detail = msg.unwrap_or_else(|| ffi::code_to_str(extended_code).to_owned());
            let code_suffix = match extended_code {
                ffi::SQLITE_BUSY => format!("SQLITE_BUSY, code {extended_code}"),
                ffi::SQLITE_BUSY_RECOVERY => {
                    format!("SQLITE_BUSY_RECOVERY, code {extended_code}")
                }
                ffi::SQLITE_BUSY_SNAPSHOT => {
                    format!("SQLITE_BUSY_SNAPSHOT, code {extended_code}")
                }
                ffi::SQLITE_BUSY_TIMEOUT => {
                    format!("SQLITE_BUSY_TIMEOUT, code {extended_code}")
                }
                _ => format!("code {extended_code}"),
            };
            Error::Other(format!("{prefix}: {detail} ({code_suffix})"))
        }
        _ => Error::Other(format!("{prefix}: {e}")),
    }
}

fn attrs_to_json(attrs: &ContainerAttrs) -> Result<String> {
    serde_json::to_string(attrs).map_err(|e| Error::Other(format!("encode container attrs: {e}")))
}

fn attrs_from_json(s: &str) -> Result<ContainerAttrs> {
    serde_json::from_str(s).map_err(|e| Error::Other(format!("decode container attrs: {e}")))
}

// The legacy non-tx public `create_container` / `rename_container` /
// `delete_container` helpers were removed in MCS-P1-C. The trait
// methods on `SqliteCasMds` now route through `with_set_tx`, which
// inherits the `container_change_set` lifecycle row written by the
// `_in_tx` variants below. Keeping a parallel non-tx surface would
// have been a permanent hole in the stream: any caller of the
// public trait that took the legacy path would silently bypass the
// rows JMAP `Mailbox/changes` is going to read in Phase 2.

pub fn list_containers(conn: &Connection) -> Result<Vec<ContainerInfo>> {
    let mut stmt = conn
        .prepare("SELECT id, parent_id, name, attrs FROM container ORDER BY id")
        .map_err(|e| map_sql_err("prepare list_containers", e))?;
    let rows = stmt
        .query_map([], |row| {
            let id_s: String = row.get(0)?;
            let parent_s: Option<String> = row.get::<_, Option<String>>(1)?;
            let name: String = row.get(2)?;
            let attrs_s: String = row.get(3)?;
            Ok((id_s, parent_s, name, attrs_s))
        })
        .map_err(|e| map_sql_err("query list_containers", e))?;
    let mut out = Vec::new();
    for r in rows {
        let (id_s, parent_s, name, attrs_s) =
            r.map_err(|e| map_sql_err("row list_containers", e))?;
        let id = ContainerId(parse_uuid(&id_s, "container.id")?);
        let parent = match parent_s {
            Some(s) => Some(ContainerId(parse_uuid(&s, "container.parent_id")?)),
            None => None,
        };
        let attrs = attrs_from_json(&attrs_s)?;
        out.push(ContainerInfo {
            id,
            parent,
            name,
            attrs,
        });
    }
    Ok(out)
}

pub fn container_status(conn: &Connection, id: &ContainerId) -> Result<ContainerStatus> {
    let row = conn
        .query_row(
            "SELECT exists_count, unread_count, seq_validity, next_seq, change_seq \
             FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| map_sql_err("container_status", e))?;
    let (exists, unread, validity, next_seq, change_seq) =
        row.ok_or_else(|| Error::ContainerNotFound(id.0.to_string()))?;
    Ok(ContainerStatus {
        exists: exists as u64,
        unread: unread as u64,
        seq_validity: validity as u64,
        next_seq: Seq(next_seq as u32),
        change_seq: ChangeToken(change_seq as u64),
    })
}

pub(crate) fn container_exists(conn: &Connection, id: &ContainerId) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("container_exists", e))?;
    Ok(n > 0)
}

fn parse_uuid(s: &str, field: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|e| Error::Other(format!("decode {field}={s:?}: {e}")))
}

// ---------------------------------------------------------------------------
// Delivery transactions (Phase 1b)
// ---------------------------------------------------------------------------
//
// All item-mutating operations run inside `BEGIN IMMEDIATE` so SQLite
// gives us write-priority and serializes against any concurrent writer.
// Per spec §Invariants 4 the multi-row mutations either all commit or
// none do.
//
// The cross-DB refcount writes (blobs.sqlite via ATTACH) are
// deliberately deferred to Phase 3. Phase 1b commits to the data.sqlite
// truth; the bidx is rebuildable from data.sqlite per invariant 8.

const CHANGE_KIND_ADDED: i64 = 0;
const CHANGE_KIND_METADATA: i64 = 1;
const CHANGE_KIND_REMOVED: i64 = 2;

fn unread_delta_of(flags: Flags) -> i64 {
    // Unread accounting reads `\Seen` (bit 0) only. Other allocated
    // system flag bits (`\Flagged`, `\Answered`, `\Draft`, `\Deleted`)
    // never contribute to the unread counter — see the regression
    // test `unread_delta_only_consults_seen_bit` below.
    if (flags.0 & Flags::SEEN) == 0 { 1 } else { 0 }
}

pub fn add_item(
    conn: &mut Connection,
    blobs_root: &Path,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    blob_hash: &BlobHash,
    memberships: &[Membership],
) -> Result<AddReport> {
    if memberships.is_empty() {
        return Err(Error::Other(
            "add_item: at least one membership required".into(),
        ));
    }
    let blob_hex = blob::hex(blob_hash);
    let size = blob::size(blobs_root, blob_hash)?;
    let item_id = ItemId(uuid::Uuid::now_v7());
    let now = now_ms();

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN add_item", e))?;

    tx.execute(
        "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![item_id.0.to_string(), blob_hex, size as i64, now],
    )
    .map_err(|e| map_sql_err("insert item", e))?;

    // Cross-DB write to blobs_db (ATTACH'd at open time): one
    // blob_ref per item, refcount += 1. Per-item, not per-membership
    // — copy/move inside the same set never touch refcount.
    crate::blob_index::add_blob_ref_in_tx(&tx, set, &item_id, blob_hash, size)?;

    let mut placements = Vec::with_capacity(memberships.len());
    for m in memberships {
        let (placement, _set_seq) =
            insert_membership(&tx, set, &item_id, &m.container, m.flags, &[], m.added_at)?;
        placements.push(placement);
    }

    tx.commit().map_err(|e| map_sql_err("commit add_item", e))?;

    // Spec §subscribe: publish *after* commit so observers never see
    // events that later fail to land.
    for p in &placements {
        notifier.publish(
            set,
            &p.container,
            ContainerEvent::ItemAdded {
                item_id,
                seq: p.seq,
                change_seq: p.change_seq,
            },
        );
    }

    // Bus `mds.item.added`: one event for the item, with parallel
    // container_ids/change_seqs arrays per placement (spec §Bus
    // event surface). The blob_hash is hex on the wire.
    let container_ids: Vec<ContainerId> = placements.iter().map(|p| p.container).collect();
    let change_seqs: Vec<ChangeToken> = placements.iter().map(|p| p.change_seq).collect();
    sink.emit(MdsEvent::ItemAdded(ItemAdded {
        set_id: *set,
        item_id,
        blob_hash: blob_hex.clone(),
        container_ids,
        change_seqs,
    }));

    Ok(AddReport {
        item_id,
        placements,
    })
}

pub fn copy_item(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    dest: &ContainerId,
    flags: Flags,
) -> Result<CopyReport> {
    let now = now_ms();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN copy_item", e))?;
    if !item_exists(&tx, item_id)? {
        return Err(Error::ItemNotFound(item_id.0.to_string()));
    }
    if !container_exists_tx(&tx, dest)? {
        return Err(Error::ContainerNotFound(dest.0.to_string()));
    }
    if membership_exists(&tx, item_id, dest)? {
        return Err(Error::Other(format!(
            "item {} already in container {}",
            item_id.0, dest.0
        )));
    }
    // v1.1 §3: a new membership inherits the existing flags+tags so the
    // JMAP "all memberships agree" invariant holds. The caller-supplied
    // `flags` arg is the fallback only when the item has no other
    // memberships (which can't happen for copy_item today — item_exists
    // implies at least one membership — but the fallback keeps the
    // code defensive against future zero-membership-item shapes).
    let (inh_flags, inh_tags) = match read_any_membership_keywords(&tx, item_id)? {
        Some(p) => p,
        None => (flags, Vec::new()),
    };
    let (placement, _set_seq) =
        insert_membership(&tx, set, item_id, dest, inh_flags, &inh_tags, now)?;
    tx.commit()
        .map_err(|e| map_sql_err("commit copy_item", e))?;
    notifier.publish(
        set,
        dest,
        ContainerEvent::ItemAdded {
            item_id: *item_id,
            seq: placement.seq,
            change_seq: placement.change_seq,
        },
    );
    sink.emit(MdsEvent::ItemCopied(ItemCopied {
        set_id: *set,
        item_id: *item_id,
        dest: *dest,
        seq_dest: placement.seq,
        change_seq_dest: placement.change_seq,
    }));
    Ok(CopyReport {
        seq_dest: placement.seq,
        change_seq_dest: placement.change_seq,
    })
}

/// Source and destination containers for a [`move_item`] call.
pub(crate) struct MovePath<'a> {
    pub src: &'a ContainerId,
    pub dest: &'a ContainerId,
}

pub(crate) fn move_item(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    path: MovePath<'_>,
    flags: Flags,
) -> Result<MoveReport> {
    let MovePath { src, dest } = path;
    if src == dest {
        return Err(Error::Other("move_item: src and dest must differ".into()));
    }
    let now = now_ms();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN move_item", e))?;
    if !item_exists(&tx, item_id)? {
        return Err(Error::ItemNotFound(item_id.0.to_string()));
    }
    if !container_exists_tx(&tx, src)? {
        return Err(Error::ContainerNotFound(src.0.to_string()));
    }
    if !container_exists_tx(&tx, dest)? {
        return Err(Error::ContainerNotFound(dest.0.to_string()));
    }
    if !membership_exists(&tx, item_id, src)? {
        return Err(Error::Other(format!(
            "item {} not in source container {}",
            item_id.0, src.0
        )));
    }
    if membership_exists(&tx, item_id, dest)? {
        return Err(Error::Other(format!(
            "item {} already in destination container {}",
            item_id.0, dest.0
        )));
    }
    // v1.1 §3 inheritance: read src's (flags, tags) BEFORE
    // remove_membership_inner deletes the row, so dest can adopt the
    // exact keywords. Caller's `flags` arg is the fallback for the
    // (impossible-today) zero-membership case — see copy_item for the
    // same defensive pattern.
    let (inh_flags, inh_tags) = match read_membership_keywords(&tx, item_id, src)? {
        Some(p) => p,
        None => (flags, Vec::new()),
    };
    let removed = remove_membership_inner(&tx, set, item_id, src)?;
    let (added, set_seq_dest) =
        insert_membership(&tx, set, item_id, dest, inh_flags, &inh_tags, now)?;
    tx.commit()
        .map_err(|e| map_sql_err("commit move_item", e))?;
    notifier.publish(
        set,
        src,
        ContainerEvent::ItemRemoved {
            item_id: *item_id,
            seq: removed.old_seq,
            change_seq: removed.change_seq,
        },
    );
    notifier.publish(
        set,
        dest,
        ContainerEvent::ItemAdded {
            item_id: *item_id,
            seq: added.seq,
            change_seq: added.change_seq,
        },
    );
    // §4a: ItemMoved is *additive* to ItemRemoved/ItemAdded.
    // Delivered to subscribers of BOTH src and dest so JMAP / audit /
    // cross-mailbox consumers can correlate the pair atomically.
    // IMAP IDLE consumers ignore this and react to the per-container
    // events above as today.
    let moved_event = ContainerEvent::ItemMoved {
        item_id: *item_id,
        src: *src,
        dest: *dest,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
        set_change_seq_src: removed.set_change_seq,
        set_change_seq_dest: set_seq_dest,
    };
    notifier.publish(set, src, moved_event.clone());
    notifier.publish(set, dest, moved_event);
    // Bus `mds.item.moved` is a *single* event spanning src+dest.
    // The in-process notifier emits a pair (Removed+Added) because
    // it's container-keyed; Bus subscribers want the atomic move
    // semantics — see spec §Bus event surface, "deliberately
    // distinct" copy vs move note.
    sink.emit(MdsEvent::ItemMoved(ItemMoved {
        set_id: *set,
        item_id: *item_id,
        src: *src,
        dest: *dest,
        seq_dest: added.seq,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
    }));
    Ok(MoveReport {
        seq_src: removed.old_seq,
        seq_dest: added.seq,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
    })
}

pub fn remove_membership(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN remove_membership", e))?;
    if !membership_exists(&tx, item_id, container_id)? {
        return Err(Error::Other(format!(
            "membership ({}, {}) not present",
            item_id.0, container_id.0
        )));
    }
    let removed = remove_membership_inner(&tx, set, item_id, container_id)?;
    // If the item now has zero memberships, drop the cross-DB
    // blob_ref + refcount, then delete the item row so the orphan
    // blob becomes a GC candidate in Phase 3.
    let remaining: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM membership WHERE item_id = ?1",
            params![item_id.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("count remaining memberships", e))?;
    if remaining == 0 {
        let blob_hex: String = tx
            .query_row(
                "SELECT blob_hash FROM item WHERE id = ?1",
                params![item_id.0.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| map_sql_err("read blob_hash for orphan", e))?;
        let blob_hash = parse_blob_hash(&blob_hex)?;
        crate::blob_index::drop_blob_ref_in_tx(&tx, set, item_id, &blob_hash)?;
        tx.execute(
            "DELETE FROM item WHERE id = ?1",
            params![item_id.0.to_string()],
        )
        .map_err(|e| map_sql_err("delete orphan item", e))?;
    }
    tx.commit()
        .map_err(|e| map_sql_err("commit remove_membership", e))?;
    notifier.publish(
        set,
        container_id,
        ContainerEvent::ItemRemoved {
            item_id: *item_id,
            seq: removed.old_seq,
            change_seq: removed.change_seq,
        },
    );
    sink.emit(MdsEvent::ItemRemoved(ItemRemoved {
        set_id: *set,
        item_id: *item_id,
        container_id: *container_id,
        seq: removed.old_seq,
        change_seq: removed.change_seq,
    }));
    Ok(())
}

pub fn store_flags(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
    flags: Flags,
) -> Result<ChangeToken> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN store_flags", e))?;
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT flags, seq FROM membership WHERE item_id = ?1 AND container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read flags", e))?;
    let (old_flags, seq) = match row {
        Some(r) => r,
        None => {
            return Err(Error::Other(format!(
                "membership ({}, {}) not present",
                item_id.0, container_id.0
            )));
        }
    };
    let old_flags = Flags(old_flags as u32);
    let unread_delta = unread_delta_of(flags) - unread_delta_of(old_flags);
    let new_change_seq = bump_change_seq(&tx, container_id, unread_delta, 0)?;
    tx.execute(
        "UPDATE membership SET flags = ?1, change_seq = ?2 \
         WHERE item_id = ?3 AND container_id = ?4",
        params![
            flags.0 as i64,
            new_change_seq.0 as i64,
            item_id.0.to_string(),
            container_id.0.to_string(),
        ],
    )
    .map_err(|e| map_sql_err("update flags", e))?;
    insert_change_row(
        &tx,
        container_id,
        new_change_seq,
        CHANGE_KIND_METADATA,
        Seq(seq as u32),
        Some(item_id),
    )?;
    allocate_set_change(
        &tx,
        set,
        SetChangeRow {
            container_id,
            container_change_seq: new_change_seq,
            item_id,
            kind: ChangeKind::MetadataChanged,
            seq: Seq(seq as u32),
            changed_at: now_ms(),
        },
    )?;
    tx.commit()
        .map_err(|e| map_sql_err("commit store_flags", e))?;
    notifier.publish(
        set,
        container_id,
        ContainerEvent::FlagsChanged {
            item_id: *item_id,
            change_seq: new_change_seq,
        },
    );
    sink.emit(MdsEvent::ItemFlagged(ItemFlagged {
        set_id: *set,
        item_id: *item_id,
        container_id: *container_id,
        old_flags: old_flags.0,
        new_flags: flags.0,
        change_seq: new_change_seq,
    }));
    Ok(new_change_seq)
}

/// Encode `Tags` as a sorted JSON array string for the
/// `membership.tags` column, or `None` when the set is empty (matches
/// the legacy `tags = NULL` storage). BTreeSet iteration is
/// lexicographic, so two identical keyword sets serialise to the same
/// bytes — important for any future content-addressed comparison.
fn encode_tags_for_membership(tags: &Tags) -> Result<Option<String>> {
    if tags.is_empty() {
        return Ok(None);
    }
    let v: Vec<&String> = tags.iter().collect();
    Ok(Some(
        serde_json::to_string(&v).map_err(|e| Error::Other(format!("encode tags: {e}")))?,
    ))
}

/// Single-membership keyword write — IMAP STORE per-mailbox semantics.
/// Allocates one `set_change` row of kind `METADATA_CHANGED`.
///
/// **Not the JMAP fan-out path.** JMAP `Email/set keywords` must apply
/// to *all* current memberships in one transaction (spec §3 line 335);
/// per-membership commits would let pagination interleave a partial
/// update and silently desync `Email/changes` consumers. The
/// flags-only JMAP fan-out is [`store_item_flags_in_tx`] (Task 1.8;
/// rewrites `flags`, leaves `tags` alone); the older flags+tags
/// fan-out is [`store_item_keywords`] and is retained for callers
/// that wrote per-membership Tags before Task 1.8 split them out.
/// This function remains for direct IMAP STORE on a single
/// (item, mailbox) pair.
/// Flags + tags pair stored on a single membership row.
pub(crate) struct Keywords<'a> {
    pub flags: Flags,
    pub tags: &'a Tags,
}

pub(crate) fn store_membership_keywords(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
    keywords: Keywords<'_>,
) -> Result<ChangeToken> {
    let Keywords { flags, tags } = keywords;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN store_membership_keywords", e))?;
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT flags, seq FROM membership WHERE item_id = ?1 AND container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read flags+seq", e))?;
    let (old_flags, seq) = match row {
        Some(r) => r,
        None => {
            // Disambiguate container-missing from membership-missing,
            // mirroring `store_flags_in_tx`: a keyword write against a
            // mailbox that does not exist surfaces `ContainerNotFound`
            // (the `MailStore` wrapper maps it to "mailbox not found"),
            // while an existing mailbox with no row for this item
            // surfaces the membership-missing sentinel (mapped to
            // "email not found"). Without this split an unknown-mailbox
            // write would masquerade as a stale-UID error — the same
            // trap `store_flags_in_tx` guards against.
            if !container_exists_tx(&tx, container_id)? {
                return Err(Error::ContainerNotFound(container_id.0.to_string()));
            }
            return Err(Error::Other(format!(
                "membership ({}, {}) not present",
                item_id.0, container_id.0
            )));
        }
    };
    let old_flags = Flags(old_flags as u32);
    let unread_delta = unread_delta_of(flags) - unread_delta_of(old_flags);
    let new_change_seq = bump_change_seq(&tx, container_id, unread_delta, 0)?;
    let tags_json = encode_tags_for_membership(tags)?;
    tx.execute(
        "UPDATE membership SET flags = ?1, change_seq = ?2, tags = ?3 \
         WHERE item_id = ?4 AND container_id = ?5",
        params![
            flags.0 as i64,
            new_change_seq.0 as i64,
            tags_json,
            item_id.0.to_string(),
            container_id.0.to_string(),
        ],
    )
    .map_err(|e| map_sql_err("update flags+tags", e))?;
    insert_change_row(
        &tx,
        container_id,
        new_change_seq,
        CHANGE_KIND_METADATA,
        Seq(seq as u32),
        Some(item_id),
    )?;
    allocate_set_change(
        &tx,
        set,
        SetChangeRow {
            container_id,
            container_change_seq: new_change_seq,
            item_id,
            kind: ChangeKind::MetadataChanged,
            seq: Seq(seq as u32),
            changed_at: now_ms(),
        },
    )?;
    tx.commit()
        .map_err(|e| map_sql_err("commit store_membership_keywords", e))?;
    notifier.publish(
        set,
        container_id,
        ContainerEvent::FlagsChanged {
            item_id: *item_id,
            change_seq: new_change_seq,
        },
    );
    // Reuse the Bus `mds.item.flagged` event — keyword changes are a
    // strict superset of flag changes and consumers already know how
    // to react. Subscribers wanting the full keyword set re-fetch via
    // `item_memberships`. A dedicated keywords-changed Bus event can
    // land later if a real consumer asks for the diff in-band.
    sink.emit(MdsEvent::ItemFlagged(ItemFlagged {
        set_id: *set,
        item_id: *item_id,
        container_id: *container_id,
        old_flags: old_flags.0,
        new_flags: flags.0,
        change_seq: new_change_seq,
    }));
    Ok(new_change_seq)
}

/// JMAP `Email/set keywords` fan-out: apply `flags`+`tags` to **every**
/// current membership of `item` in one transaction. All affected rows
/// commit together or none do; each row gets its own `set_change`
/// `METADATA_CHANGED` entry, so `Email/changes` consumers see one
/// allocation per touched mailbox in `set_change_seq` order.
///
/// Returns one `(container_id, new_change_seq)` per affected
/// membership in deterministic order (`ORDER BY container_id`). An item
/// with no current memberships returns `Ok(vec![])` and writes nothing
/// — no transaction, no notifier traffic.
///
/// Use `store_membership_keywords` for IMAP STORE per-mailbox
/// semantics, where the caller intentionally diverges keywords across
/// mailboxes.
pub fn store_item_keywords(
    conn: &mut Connection,
    notifier: &Notifier,
    sink: &EventSink,
    set: &SetId,
    item_id: &ItemId,
    flags: Flags,
    tags: &Tags,
) -> Result<Vec<(ContainerId, ChangeToken)>> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_sql_err("BEGIN store_item_keywords", e))?;
    let rows: Vec<(ContainerId, Flags, Seq)> = {
        let mut stmt = tx
            .prepare(
                "SELECT container_id, flags, seq FROM membership \
                 WHERE item_id = ?1 ORDER BY container_id",
            )
            .map_err(|e| map_sql_err("prepare store_item_keywords scan", e))?;
        let mapped = stmt
            .query_map(params![item_id.0.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| map_sql_err("query store_item_keywords scan", e))?;
        let mut out = Vec::new();
        for row in mapped {
            let (cid_s, flags_i, seq_i) =
                row.map_err(|e| map_sql_err("row store_item_keywords scan", e))?;
            out.push((
                ContainerId(parse_uuid(&cid_s, "membership.container_id")?),
                Flags(flags_i as u32),
                Seq(seq_i as u32),
            ));
        }
        out
    };
    if rows.is_empty() {
        // No memberships → no work, no commit. Drop the empty IMMEDIATE
        // tx silently; SQLite reverts the reservation on drop.
        return Ok(Vec::new());
    }
    let tags_json = encode_tags_for_membership(tags)?;
    let now = now_ms();
    let mut updates: Vec<(ContainerId, Flags, Flags, ChangeToken)> = Vec::with_capacity(rows.len());
    for (container_id, old_flags, seq) in rows {
        let unread_delta = unread_delta_of(flags) - unread_delta_of(old_flags);
        let new_change_seq = bump_change_seq(&tx, &container_id, unread_delta, 0)?;
        tx.execute(
            "UPDATE membership SET flags = ?1, change_seq = ?2, tags = ?3 \
             WHERE item_id = ?4 AND container_id = ?5",
            params![
                flags.0 as i64,
                new_change_seq.0 as i64,
                tags_json,
                item_id.0.to_string(),
                container_id.0.to_string(),
            ],
        )
        .map_err(|e| map_sql_err("update flags+tags fan-out", e))?;
        insert_change_row(
            &tx,
            &container_id,
            new_change_seq,
            CHANGE_KIND_METADATA,
            seq,
            Some(item_id),
        )?;
        allocate_set_change(
            &tx,
            set,
            SetChangeRow {
                container_id: &container_id,
                container_change_seq: new_change_seq,
                item_id,
                kind: ChangeKind::MetadataChanged,
                seq,
                changed_at: now,
            },
        )?;
        updates.push((container_id, old_flags, flags, new_change_seq));
    }
    tx.commit()
        .map_err(|e| map_sql_err("commit store_item_keywords", e))?;
    let mut out = Vec::with_capacity(updates.len());
    for (container_id, old_flags, new_flags, new_change_seq) in updates {
        notifier.publish(
            set,
            &container_id,
            ContainerEvent::FlagsChanged {
                item_id: *item_id,
                change_seq: new_change_seq,
            },
        );
        sink.emit(MdsEvent::ItemFlagged(ItemFlagged {
            set_id: *set,
            item_id: *item_id,
            container_id,
            old_flags: old_flags.0,
            new_flags: new_flags.0,
            change_seq: new_change_seq,
        }));
        out.push((container_id, new_change_seq));
    }
    Ok(out)
}

/// JMAP `Email/set keywords` flags-only fan-out, in-tx variant.
/// Apply `flags` to **every** current membership of `item_id`, leaving
/// each row's `tags` column untouched. Bumps each container's
/// `change_seq`, writes a `METADATA_CHANGED` change row, and allocates
/// one `set_change` row per touched membership. Buffers
/// `FlagsChanged` notifier + `mds.item.flagged` Bus events into
/// `events` for post-commit replay.
///
/// Returns one `(container_id, new_change_seq)` per touched
/// membership in deterministic order (`ORDER BY container_id`). An
/// item with no current memberships returns `Ok(vec![])` and writes
/// nothing.
///
/// **Why flags-only.** `MailStore::set_keywords` (Phase 1 Task 1.8) is
/// the JMAP `Email/set keywords` migration entry point; for the
/// initial maild surface we only model system flags, leaving
/// user-defined keyword Tags for a follow-on. The peer
/// `store_item_keywords` (no `_in_tx`) also rewrites the `tags`
/// column; this primitive deliberately omits `tags = ?` from the
/// UPDATE so per-membership Tags are preserved.
pub fn store_item_flags_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    flags: Flags,
) -> Result<Vec<(ContainerId, ChangeToken)>> {
    let rows: Vec<(ContainerId, Flags, Seq)> = {
        let mut stmt = tx
            .prepare(
                "SELECT container_id, flags, seq FROM membership \
                 WHERE item_id = ?1 ORDER BY container_id",
            )
            .map_err(|e| map_sql_err("prepare store_item_flags scan", e))?;
        let mapped = stmt
            .query_map(params![item_id.0.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| map_sql_err("query store_item_flags scan", e))?;
        let mut out = Vec::new();
        for row in mapped {
            let (cid_s, flags_i, seq_i) =
                row.map_err(|e| map_sql_err("row store_item_flags scan", e))?;
            out.push((
                ContainerId(parse_uuid(&cid_s, "membership.container_id")?),
                Flags(flags_i as u32),
                Seq(seq_i as u32),
            ));
        }
        out
    };
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let now = now_ms();
    let mut out = Vec::with_capacity(rows.len());
    for (container_id, old_flags, seq) in rows {
        let unread_delta = unread_delta_of(flags) - unread_delta_of(old_flags);
        let new_change_seq = bump_change_seq(tx, &container_id, unread_delta, 0)?;
        tx.execute(
            "UPDATE membership SET flags = ?1, change_seq = ?2 \
             WHERE item_id = ?3 AND container_id = ?4",
            params![
                flags.0 as i64,
                new_change_seq.0 as i64,
                item_id.0.to_string(),
                container_id.0.to_string(),
            ],
        )
        .map_err(|e| map_sql_err("update flags fan-out", e))?;
        insert_change_row(
            tx,
            &container_id,
            new_change_seq,
            CHANGE_KIND_METADATA,
            seq,
            Some(item_id),
        )?;
        allocate_set_change(
            tx,
            set,
            SetChangeRow {
                container_id: &container_id,
                container_change_seq: new_change_seq,
                item_id,
                kind: ChangeKind::MetadataChanged,
                seq,
                changed_at: now,
            },
        )?;
        events.push_notifier(
            *set,
            container_id,
            ContainerEvent::FlagsChanged {
                item_id: *item_id,
                change_seq: new_change_seq,
            },
        );
        events.push_sink(MdsEvent::ItemFlagged(ItemFlagged {
            set_id: *set,
            item_id: *item_id,
            container_id,
            old_flags: old_flags.0,
            new_flags: flags.0,
            change_seq: new_change_seq,
        }));
        out.push((container_id, new_change_seq));
    }
    Ok(out)
}

/// IMAP STORE per-mailbox flags-only write, in-tx variant. Mutates
/// exactly one `(item_id, container_id)` membership row. Returns the
/// `change_seq` allocated to the bump.
///
/// **Per-mailbox isolation.** This is the substrate primitive behind
/// the IMAP non-negotiable invariant that `\Seen` set on the INBOX
/// membership of a multi-mailbox email must NOT propagate to the
/// Archive membership (`_doc/maild/imap.md` §Non-negotiable
/// invariants 5/6). The peer `store_item_flags_in_tx` is the JMAP
/// shape that explicitly fans out to every membership; this
/// primitive is its inverse and must NOT walk siblings.
///
/// `tags` is left untouched (mirrors the non-tx `store_flags` and
/// `store_item_flags_in_tx`). Errors as `Error::Other("membership
/// (item, container) not present")` when the membership row does
/// not exist — IMAP STORE against a UID that no longer has the
/// caller's resolution-time membership lands here, and the
/// `MailStore` wrapper translates it to the user-visible "email
/// not found" sentinel.
///
/// Buffers one `FlagsChanged` notifier event + one `mds.item.flagged`
/// Bus event for post-commit replay; the caller's `with_set_tx`
/// drains the buffer only after `tx.commit()`.
pub fn store_flags_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
    flags: Flags,
) -> Result<ChangeToken> {
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT flags, seq FROM membership WHERE item_id = ?1 AND container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read flags", e))?;
    let (old_flags, seq) = match row {
        Some(r) => r,
        None => {
            // Disambiguate: container missing vs membership missing.
            // Without this branch the unknown-mailbox case collapses to
            // the membership-missing sentinel, which `MailStore`
            // translates to "email not found" — masking an IMAP UID
            // resolution-race as a stale-UID error. Surface
            // `ContainerNotFound` so the wrapper layer can map it to
            // the "mailbox not found" wire shape every other
            // mailbox-parametric verb already uses.
            if !container_exists_tx(tx, container_id)? {
                return Err(Error::ContainerNotFound(container_id.0.to_string()));
            }
            return Err(Error::Other(format!(
                "membership ({}, {}) not present",
                item_id.0, container_id.0
            )));
        }
    };
    let old_flags = Flags(old_flags as u32);
    let unread_delta = unread_delta_of(flags) - unread_delta_of(old_flags);
    let new_change_seq = bump_change_seq(tx, container_id, unread_delta, 0)?;
    tx.execute(
        "UPDATE membership SET flags = ?1, change_seq = ?2 \
         WHERE item_id = ?3 AND container_id = ?4",
        params![
            flags.0 as i64,
            new_change_seq.0 as i64,
            item_id.0.to_string(),
            container_id.0.to_string(),
        ],
    )
    .map_err(|e| map_sql_err("update flags", e))?;
    insert_change_row(
        tx,
        container_id,
        new_change_seq,
        CHANGE_KIND_METADATA,
        Seq(seq as u32),
        Some(item_id),
    )?;
    allocate_set_change(
        tx,
        set,
        SetChangeRow {
            container_id,
            container_change_seq: new_change_seq,
            item_id,
            kind: ChangeKind::MetadataChanged,
            seq: Seq(seq as u32),
            changed_at: now_ms(),
        },
    )?;
    events.push_notifier(
        *set,
        *container_id,
        ContainerEvent::FlagsChanged {
            item_id: *item_id,
            change_seq: new_change_seq,
        },
    );
    events.push_sink(MdsEvent::ItemFlagged(ItemFlagged {
        set_id: *set,
        item_id: *item_id,
        container_id: *container_id,
        old_flags: old_flags.0,
        new_flags: flags.0,
        change_seq: new_change_seq,
    }));
    Ok(new_change_seq)
}

/// Containers in which `item` currently has membership, with the
/// per-membership system [`Flags`] and user [`Tags`]. Returns an empty
/// `Vec` if the item exists but is fully unmoored (zero current
/// memberships) or if the item id is unknown — callers that need to
/// distinguish must consult [`fetch_item_meta`] separately.
pub fn item_memberships(
    conn: &Connection,
    item_id: &ItemId,
) -> Result<Vec<(ContainerId, Flags, Tags)>> {
    let mut stmt = conn
        .prepare(
            "SELECT container_id, flags, tags FROM membership \
             WHERE item_id = ?1 ORDER BY container_id",
        )
        .map_err(|e| map_sql_err("prepare item_memberships", e))?;
    let rows = stmt
        .query_map(params![item_id.0.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| map_sql_err("query item_memberships", e))?;
    let mut out = Vec::new();
    for row in rows {
        let (cid_s, flags_i, tags_json) =
            row.map_err(|e| map_sql_err("row item_memberships", e))?;
        let cid = ContainerId(parse_uuid(&cid_s, "membership.container_id")?);
        let flags = Flags(flags_i as u32);
        let tags: Tags = tags_from_json(tags_json.as_deref())?.into();
        out.push((cid, flags, tags));
    }
    Ok(out)
}

/// Build a safe FTS5 MATCH expression from a raw user needle.
///
/// Each whitespace-separated token is wrapped as an FTS5 *string literal*
/// (`"…"`, with any embedded `"` doubled) and the tokens are joined with a
/// space — FTS5's implicit AND. Quoting every token defuses the FTS5 query
/// grammar: bareword operators (`AND`/`OR`/`NOT`/`NEAR`), column filters
/// (`col:`), prefix stars (`*`), and punctuation (`-`, `(`, `)`, `^`) all
/// become literal phrase content instead of syntax, so a hostile or
/// accidental needle can never error the query or reach an unintended
/// operator.
///
/// Tokens with no alphanumeric character are dropped: the FTS5 tokeniser
/// would yield an empty phrase for pure punctuation, which is unsafe to
/// embed. If every token drops (or the needle is blank) the result is the
/// empty string — [`search_items`] treats that as "match nothing".
fn build_fts_query(needle: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for tok in needle.split_whitespace() {
        if !tok.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }
        parts.push(format!("\"{}\"", tok.replace('"', "\"\"")));
    }
    parts.join(" ")
}

/// Full-text search across the `mail_search` FTS5 projection (headers,
/// subject, body_text, normalized_addrs) of the per-set database. Returns
/// the matching item ids (order unspecified). `needle` is raw user text,
/// turned into a safe FTS5 MATCH via [`build_fts_query`]; a blank or
/// punctuation-only needle matches nothing (empty Vec) rather than erroring.
pub fn search_items(conn: &Connection, needle: &str) -> Result<Vec<ItemId>> {
    let query = build_fts_query(needle);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare("SELECT item_id FROM mail_search WHERE mail_search MATCH ?1")
        .map_err(|e| map_sql_err("prepare search_items", e))?;
    let rows = stmt
        .query_map(params![query], |r| r.get::<_, String>(0))
        .map_err(|e| map_sql_err("query search_items", e))?;
    let mut out = Vec::new();
    for row in rows {
        let item_s = row.map_err(|e| map_sql_err("row search_items", e))?;
        out.push(ItemId(parse_uuid(&item_s, "mail_search.item_id")?));
    }
    Ok(out)
}

/// Account/set-wide changes since the given set-scoped token. Returns
/// up to `limit` rows in `set_change_seq` order plus an opaque
/// `Some(next)` cursor when there is at least one row beyond the
/// returned page (caller passes `next` back as `since`); `None` when
/// the caller has reached the tip.
///
/// `limit == 0` returns `(vec![], None)` — explicit empty page; never
/// short-circuits the cursor decision.
pub fn changes_since_set(
    conn: &Connection,
    since: SetChangeToken,
    limit: usize,
) -> Result<(Vec<SetChange>, Option<SetChangeToken>)> {
    if limit == 0 {
        return Ok((Vec::new(), None));
    }
    // `LIMIT N+1` so we know whether there is a next page without a
    // second COUNT round-trip. The N+1th row is dropped from the
    // returned page; its predecessor — i.e. row `N` — becomes the
    // cursor (`since` is exclusive: caller's next call asks for
    // `set_change_seq > N`).
    let probe = (limit as i64).saturating_add(1);
    let mut stmt = conn
        .prepare(
            "SELECT set_change_seq, container_id, container_change_seq, item_id, kind, seq, changed_at \
             FROM set_change WHERE set_change_seq > ?1 \
             ORDER BY set_change_seq ASC LIMIT ?2",
        )
        .map_err(|e| map_sql_err("prepare changes_since_set", e))?;
    let rows = stmt
        .query_map(params![since.0 as i64, probe], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .map_err(|e| map_sql_err("query changes_since_set", e))?;
    let mut collected: Vec<SetChange> = Vec::new();
    for row in rows {
        let (sc, cid_s, ccs, item_s, kind_s, seq_i, ts) =
            row.map_err(|e| map_sql_err("row changes_since_set", e))?;
        collected.push(SetChange {
            set_change_seq: SetChangeToken(sc as u64),
            container_id: ContainerId(parse_uuid(&cid_s, "set_change.container_id")?),
            container_change_seq: ChangeToken(ccs as u64),
            item_id: ItemId(parse_uuid(&item_s, "set_change.item_id")?),
            kind: change_kind_from_sql(&kind_s)?,
            seq: Seq(seq_i as u32),
            changed_at: ts,
        });
    }
    let next = if collected.len() > limit {
        // Drop the probe row; cursor is the last row we *return*.
        collected.truncate(limit);
        collected.last().map(|sc| sc.set_change_seq)
    } else {
        None
    };
    Ok((collected, next))
}

// ---------------------------------------------------------------------------
// Read paths (Phase 2)
// ---------------------------------------------------------------------------

/// Return every item id in the set whose `blob_hash` equals `target`.
///
/// Used by the JMAP `Email/import` idempotency check (RFC 8621 §4.6
/// SHOULD; the cosmix-maild migration plan §3.5b promotes it to MUST):
/// re-importing the same blob into the same mailbox MUST return the
/// existing email id, not allocate a new one. The caller is
/// responsible for filtering by membership-set match — this primitive
/// only narrows by content hash. Order is unspecified.
pub fn find_items_by_blob_hash(conn: &Connection, target: &BlobHash) -> Result<Vec<ItemId>> {
    let hex = blob::hex(target);
    let mut stmt = conn
        .prepare("SELECT id FROM item WHERE blob_hash = ?1")
        .map_err(|e| map_sql_err("find_items_by_blob_hash:prepare", e))?;
    let rows = stmt
        .query_map(params![hex], |r| r.get::<_, String>(0))
        .map_err(|e| map_sql_err("find_items_by_blob_hash:query", e))?;
    let mut out = Vec::new();
    for row in rows {
        let id_str = row.map_err(|e| map_sql_err("find_items_by_blob_hash:row", e))?;
        let id = id_str
            .parse::<uuid::Uuid>()
            .map_err(|e| Error::Other(format!("find_items_by_blob_hash: bad id {id_str}: {e}")))?;
        out.push(ItemId(id));
    }
    Ok(out)
}

pub fn fetch_item_meta(conn: &Connection, item_id: &ItemId) -> Result<ItemMeta> {
    let row = conn
        .query_row(
            "SELECT blob_hash, size_bytes, received_at, cache_blob, cache_version \
             FROM item WHERE id = ?1",
            params![item_id.0.to_string()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| map_sql_err("fetch_item_meta", e))?;
    let (blob_hex, size, received_at, cache_blob, cache_version) =
        row.ok_or_else(|| Error::ItemNotFound(item_id.0.to_string()))?;
    Ok(ItemMeta {
        blob_hash: parse_blob_hash(&blob_hex)?,
        size_bytes: size as u64,
        received_at,
        cache_blob,
        cache_version,
    })
}

pub fn fetch_item(
    conn: &Connection,
    item_id: &ItemId,
    container_id: &ContainerId,
) -> Result<ItemRecord> {
    let row = conn
        .query_row(
            "SELECT i.blob_hash, i.size_bytes, i.received_at, i.cache_blob, i.cache_version, \
                    m.seq, m.change_seq, m.flags, m.tags, m.added_at \
             FROM item i \
             JOIN membership m ON m.item_id = i.id \
             WHERE i.id = ?1 AND m.container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|e| map_sql_err("fetch_item", e))?;
    let row = row.ok_or_else(|| Error::ItemNotFound(item_id.0.to_string()))?;
    let (
        blob_hex,
        size,
        received_at,
        cache_blob,
        cache_version,
        seq,
        cs,
        flags,
        tags_json,
        added_at,
    ) = row;
    Ok(ItemRecord {
        id: *item_id,
        meta: ItemMeta {
            blob_hash: parse_blob_hash(&blob_hex)?,
            size_bytes: size as u64,
            received_at,
            cache_blob,
            cache_version,
        },
        seq: Seq(seq as u32),
        change_seq: ChangeToken(cs as u64),
        flags: Flags(flags as u32),
        tags: tags_from_json(tags_json.as_deref())?,
        added_at,
    })
}

pub fn list_items(
    conn: &Connection,
    container_id: &ContainerId,
    range: SeqRange,
) -> Result<Vec<ItemRecord>> {
    let (lo, hi) = match range {
        SeqRange::All => (1u64, u32::MAX as u64),
        SeqRange::From(s) => (s.0 as u64, u32::MAX as u64),
        SeqRange::Range(a, b) => {
            if a.0 > b.0 {
                return Err(Error::Other(format!("list_items: range {} > {}", a.0, b.0)));
            }
            (a.0 as u64, b.0 as u64)
        }
    };
    let mut stmt = conn
        .prepare(
            "SELECT i.id, i.blob_hash, i.size_bytes, i.received_at, i.cache_blob, i.cache_version, \
                    m.seq, m.change_seq, m.flags, m.tags, m.added_at \
             FROM membership m \
             JOIN item i ON i.id = m.item_id \
             WHERE m.container_id = ?1 AND m.seq BETWEEN ?2 AND ?3 \
             ORDER BY m.seq",
        )
        .map_err(|e| map_sql_err("prepare list_items", e))?;
    let rows = stmt
        .query_map(
            params![container_id.0.to_string(), lo as i64, hi as i64],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<Vec<u8>>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, i64>(10)?,
                ))
            },
        )
        .map_err(|e| map_sql_err("query list_items", e))?;
    let mut out = Vec::new();
    for row in rows {
        let (
            id_str,
            blob_hex,
            size,
            received_at,
            cache_blob,
            cache_version,
            seq,
            cs,
            flags,
            tags_json,
            added_at,
        ) = row.map_err(|e| map_sql_err("row list_items", e))?;
        let id = ItemId(uuid::Uuid::parse_str(&id_str).map_err(|e| {
            Error::Other(format!("list_items: invalid item.id uuid {id_str:?}: {e}"))
        })?);
        out.push(ItemRecord {
            id,
            meta: ItemMeta {
                blob_hash: parse_blob_hash(&blob_hex)?,
                size_bytes: size as u64,
                received_at,
                cache_blob,
                cache_version,
            },
            seq: Seq(seq as u32),
            change_seq: ChangeToken(cs as u64),
            flags: Flags(flags as u32),
            tags: tags_from_json(tags_json.as_deref())?,
            added_at,
        });
    }
    Ok(out)
}

pub fn changes_since(
    conn: &Connection,
    container_id: &ContainerId,
    since: ChangeToken,
) -> Result<Vec<Change>> {
    let mut stmt = conn
        .prepare(
            "SELECT change_seq, kind, seq, item_id \
             FROM container_change \
             WHERE container_id = ?1 AND change_seq > ?2 \
             ORDER BY change_seq",
        )
        .map_err(|e| map_sql_err("prepare changes_since", e))?;
    let rows = stmt
        .query_map(params![container_id.0.to_string(), since.0 as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| map_sql_err("query changes_since", e))?;
    let mut out = Vec::new();
    for row in rows {
        let (cs, kind, seq, item_s) = row.map_err(|e| map_sql_err("row changes_since", e))?;
        let kind = match kind {
            CHANGE_KIND_ADDED => ChangeKind::Added,
            CHANGE_KIND_METADATA => ChangeKind::MetadataChanged,
            CHANGE_KIND_REMOVED => ChangeKind::Removed,
            other => {
                return Err(Error::Other(format!(
                    "container_change.kind {other} unknown"
                )));
            }
        };
        let item_id = match item_s {
            Some(s) => Some(ItemId(parse_uuid(&s, "container_change.item_id")?)),
            None => None,
        };
        out.push(Change {
            change_seq: ChangeToken(cs as u64),
            kind,
            seq: Seq(seq as u32),
            item_id,
        });
    }
    Ok(out)
}

fn parse_blob_hash(s: &str) -> Result<BlobHash> {
    if s.len() != 64 {
        return Err(Error::Other(format!("blob_hash {s:?}: wrong length")));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let byte_str = &s[i * 2..i * 2 + 2];
        out[i] = u8::from_str_radix(byte_str, 16)
            .map_err(|e| Error::Other(format!("blob_hash {s:?}: {e}")))?;
    }
    Ok(BlobHash(out))
}

fn tags_from_json(s: Option<&str>) -> Result<Vec<String>> {
    match s {
        None => Ok(Vec::new()),
        Some(s) => serde_json::from_str(s).map_err(|e| Error::Other(format!("decode tags: {e}"))),
    }
}

// ---- internal helpers ----

/// Read `(flags, tags)` from a specific (item, container) membership.
/// Returns `Ok(None)` if the membership doesn't exist; the caller
/// decides whether to error or fall back. Used by `move_item` to
/// inherit src-membership keywords onto the dest row per spec §3
/// "all memberships agree" invariant.
fn read_membership_keywords(
    tx: &Transaction<'_>,
    item_id: &ItemId,
    container_id: &ContainerId,
) -> Result<Option<(Flags, Vec<String>)>> {
    let row: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT flags, tags FROM membership \
             WHERE item_id = ?1 AND container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read_membership_keywords", e))?;
    match row {
        None => Ok(None),
        Some((f, tj)) => Ok(Some((Flags(f as u32), tags_from_json(tj.as_deref())?))),
    }
}

/// Read any one current membership's `(flags, tags)` for the item.
/// Used by `copy_item`: by the JMAP "all memberships agree" invariant,
/// any existing membership has the keywords the dest row should
/// inherit. `ORDER BY container_id` makes the choice deterministic for
/// debuggability.
fn read_any_membership_keywords(
    tx: &Transaction<'_>,
    item_id: &ItemId,
) -> Result<Option<(Flags, Vec<String>)>> {
    let row: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT flags, tags FROM membership \
             WHERE item_id = ?1 ORDER BY container_id LIMIT 1",
            params![item_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read_any_membership_keywords", e))?;
    match row {
        None => Ok(None),
        Some((f, tj)) => Ok(Some((Flags(f as u32), tags_from_json(tj.as_deref())?))),
    }
}

fn container_exists_tx(tx: &Transaction<'_>, id: &ContainerId) -> Result<bool> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("container_exists_tx", e))?;
    Ok(n > 0)
}

fn item_exists(tx: &Transaction<'_>, id: &ItemId) -> Result<bool> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM item WHERE id = ?1",
            params![id.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("item_exists", e))?;
    Ok(n > 0)
}

fn membership_exists(tx: &Transaction<'_>, item: &ItemId, container: &ContainerId) -> Result<bool> {
    let n: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM membership WHERE item_id = ?1 AND container_id = ?2",
            params![item.0.to_string(), container.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("membership_exists", e))?;
    Ok(n > 0)
}

/// Bump the container's change_seq (and optionally next_seq /
/// exists_count / unread_count) atomically and return the new
/// `change_seq`. `unread_delta` is the signed change to
/// unread_count; `exists_delta` is the signed change to
/// exists_count and is also used to decide whether to bump
/// next_seq (Some(+1) means "we're allocating a new seq", which we
/// signal via `allocate_seq=true`).
fn bump_change_seq(
    tx: &Transaction<'_>,
    container_id: &ContainerId,
    unread_delta: i64,
    exists_delta: i64,
) -> Result<ChangeToken> {
    let cs: Option<i64> = tx
        .query_row(
            "UPDATE container SET \
                change_seq    = change_seq + 1, \
                exists_count  = exists_count + ?1, \
                unread_count  = unread_count + ?2 \
             WHERE id = ?3 \
             RETURNING change_seq",
            params![exists_delta, unread_delta, container_id.0.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| map_sql_err("bump_change_seq", e))?;
    let cs = cs.ok_or_else(|| Error::ContainerNotFound(container_id.0.to_string()))?;
    Ok(ChangeToken(cs as u64))
}

/// Allocate a fresh `seq` and bump `change_seq` and `exists_count`
/// in one atomic UPDATE, returning both. Caller is responsible for
/// the corresponding membership / changelog inserts inside the same
/// transaction.
fn allocate_seq_and_change(
    tx: &Transaction<'_>,
    container_id: &ContainerId,
    unread_delta: i64,
) -> Result<(Seq, ChangeToken)> {
    let row: Option<(i64, i64)> = tx
        .query_row(
            "UPDATE container SET \
                next_seq      = next_seq + 1, \
                change_seq    = change_seq + 1, \
                exists_count  = exists_count + 1, \
                unread_count  = unread_count + ?1 \
             WHERE id = ?2 \
             RETURNING (next_seq - 1) AS allocated_seq, change_seq",
            params![unread_delta, container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("allocate_seq", e))?;
    let (seq, change_seq) =
        row.ok_or_else(|| Error::ContainerNotFound(container_id.0.to_string()))?;
    Ok((Seq(seq as u32), ChangeToken(change_seq as u64)))
}

/// SQL TEXT representation of a [`ChangeKind`] for the `set_change`
/// table's `kind` column. The `set_change.kind CHECK` constraint
/// (see `schema/v1_1.sql`) admits exactly these three literals; any
/// future variant addition must update both ends.
fn change_kind_to_sql(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "ADDED",
        ChangeKind::MetadataChanged => "METADATA_CHANGED",
        ChangeKind::Removed => "REMOVED",
    }
}

/// Inverse of [`change_kind_to_sql`]. Anything else is a schema /
/// data corruption surface — the `CHECK` constraint should make it
/// unreachable in practice, but we surface it cleanly rather than
/// silently coerce.
fn change_kind_from_sql(s: &str) -> Result<ChangeKind> {
    match s {
        "ADDED" => Ok(ChangeKind::Added),
        "METADATA_CHANGED" => Ok(ChangeKind::MetadataChanged),
        "REMOVED" => Ok(ChangeKind::Removed),
        other => Err(Error::Other(format!(
            "set_change.kind {other:?} not in (ADDED, METADATA_CHANGED, REMOVED)"
        ))),
    }
}

/// Allocate the next `set_change_seq` for `set` and write one
/// `set_change` row inside the caller's transaction. This is the
/// **single point of truth** for set-wide change-stream writes —
/// every mutation path (`add_item`, `copy_item`, `move_item`,
/// `remove_membership`, `store_flags`, `store_membership_keywords`)
/// goes through this so `Mds::changes_since_set` can guarantee
/// "every successful mutation appears in the stream."
///
/// The allocator pattern matches `tests/set_change_allocator.rs`'s
/// concurrent guard: defensive `INSERT OR IGNORE` (the open-time
/// `seed_set_state` already ran, but a future opener that forgot
/// would still self-heal here) followed by `UPDATE … RETURNING`
/// inside an `IMMEDIATE` transaction.
struct SetChangeRow<'a> {
    container_id: &'a ContainerId,
    container_change_seq: ChangeToken,
    item_id: &'a ItemId,
    kind: ChangeKind,
    seq: Seq,
    changed_at: i64,
}

fn allocate_set_change(
    tx: &Transaction<'_>,
    set: &SetId,
    row: SetChangeRow<'_>,
) -> Result<SetChangeToken> {
    let SetChangeRow {
        container_id,
        container_change_seq,
        item_id,
        kind,
        seq,
        changed_at,
    } = row;
    let set_id_str = set.0.to_string();
    tx.execute(
        "INSERT OR IGNORE INTO set_state (set_id, set_change_seq) VALUES (?1, 0);",
        params![set_id_str],
    )
    .map_err(|e| map_sql_err("set_state self-heal", e))?;
    let new_seq: i64 = tx
        .query_row(
            "UPDATE set_state SET set_change_seq = set_change_seq + 1 \
             WHERE set_id = ?1 RETURNING set_change_seq;",
            params![set_id_str],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("allocate set_change_seq", e))?;
    tx.execute(
        "INSERT INTO set_change \
         (set_change_seq, container_id, container_change_seq, item_id, kind, seq, changed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
        params![
            new_seq,
            container_id.0.to_string(),
            container_change_seq.0 as i64,
            item_id.0.to_string(),
            change_kind_to_sql(kind),
            seq.0 as i64,
            changed_at,
        ],
    )
    .map_err(|e| map_sql_err("insert set_change", e))?;
    Ok(SetChangeToken(new_seq as u64))
}

// Wired into the lifecycle paths (create/rename/delete) below and
// `update_container_attrs` in MCS-P1-D.
fn container_change_set_kind_to_sql(k: ContainerChangeSetKind) -> &'static str {
    match k {
        ContainerChangeSetKind::Created => "CONTAINER_CREATED",
        ContainerChangeSetKind::Renamed => "CONTAINER_RENAMED",
        ContainerChangeSetKind::Destroyed => "CONTAINER_DESTROYED",
        ContainerChangeSetKind::AttrsChanged => "CONTAINER_ATTRS_CHANGED",
    }
}

/// Inverse of [`container_change_set_kind_to_sql`]. Anything else is
/// a schema / data corruption surface — the `CHECK` constraint in
/// `schema/v1_4.sql` should make it unreachable in practice, but we
/// surface it cleanly rather than silently coerce.
// No production caller until Phase 2 wires the read side (legacy
// `db::changelog` is the JMAP source until then). Allowed to stay
// dead-code-warned-off; the test module exercises both round-trip
// and rejection so the symbol can't bit-rot in the meantime.
#[allow(dead_code)]
fn container_change_set_kind_from_sql(s: &str) -> Result<ContainerChangeSetKind> {
    match s {
        "CONTAINER_CREATED" => Ok(ContainerChangeSetKind::Created),
        "CONTAINER_RENAMED" => Ok(ContainerChangeSetKind::Renamed),
        "CONTAINER_DESTROYED" => Ok(ContainerChangeSetKind::Destroyed),
        "CONTAINER_ATTRS_CHANGED" => Ok(ContainerChangeSetKind::AttrsChanged),
        other => Err(Error::Other(format!(
            "container_change_set.kind {other:?} not in (CONTAINER_CREATED, \
             CONTAINER_RENAMED, CONTAINER_DESTROYED, CONTAINER_ATTRS_CHANGED)"
        ))),
    }
}

/// Allocate the next `container_change_set_seq` (AUTOINCREMENT, so
/// strictly monotonic) and write one `container_change_set` row
/// inside the caller's transaction. This is the **single point of
/// truth** for account-wide container-lifecycle-stream writes —
/// every lifecycle mutation (`create_container`, `rename_container`,
/// `delete_container`, `update_container_attrs`) will go through
/// this so that JMAP `Mailbox/changes` (Phase 2) can guarantee
/// "every container CRUD appears in the stream."
///
/// `payload` is the pre-serialized JSON body (the per-kind shape
/// documented in `schema/v1_4.sql`); this helper does not validate
/// shape — the caller is responsible for emitting the right keys.
/// The schema constrains `payload TEXT NOT NULL`, which only rejects
/// SQL `NULL`; an empty string `""` is technically accepted at the
/// SQL layer. Callers are expected to always pass a JSON object
/// (the small per-kind payload builders in MCS-P1-C/D are the
/// single validation point for this); a bare `""` here would
/// surface only at the Phase-2 read-side JSON decode.
/// Walk `id`'s parent chain inside the caller's tx and render the
/// container's full nested name as a `/`-joined path snapshot for
/// the `container_change_set` payload. Substrate-level: no
/// IMAP-INBOX or JMAP-role special-casing — that's the consumer's
/// job in Phase 2 / the IMAP NOTIFY wire layer.
///
/// This is called **at write time** by lifecycle paths so the
/// snapshot reflects the path *as it was* when the event happened;
/// ancestor renames after the fact must not corrupt a replayed
/// IMAP NOTIFY `MailboxName` event (RFC 5465 §4.1.3).
/// See `_doc/planned/mailbox-changes-substrate.md` §Design.
///
/// Errors:
/// - `Error::ContainerNotFound(id)` if the root container row is
///   missing — surfaces the same as the lifecycle path's own
///   existence check, so callers can prefer this fn over a
///   separate `container_exists_tx` probe.
/// - `Error::Other` if an ancestor row is missing mid-walk
///   (broken FK chain), or if a parent cycle is detected
///   (`child → A → B → A`). The normal CRUD paths block cycles
///   at rename via `would_form_cycle_tx`, so a cycle hit here
///   means the on-disk graph has been corrupted out-of-band —
///   a substrate-level surface worth surfacing loudly. The
///   cycle guard is what prevents this fn from spinning the
///   per-set tx forever on a corrupt store.
fn render_full_path_in_tx(tx: &Transaction<'_>, id: &ContainerId) -> Result<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut visited: std::collections::HashSet<ContainerId> = std::collections::HashSet::new();
    let mut cur = Some(*id);
    let mut first = true;
    while let Some(c) = cur {
        if !visited.insert(c) {
            return Err(Error::Other(format!(
                "render_full_path_in_tx: parent cycle detected at {} walking from {} (broken on-disk container graph)",
                c.0, id.0
            )));
        }
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT name, parent_id FROM container WHERE id = ?1",
                params![c.0.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|e| map_sql_err("read container for path snapshot", e))?;
        let (name, parent) = match row {
            Some(t) => t,
            None if first => return Err(Error::ContainerNotFound(c.0.to_string())),
            None => {
                return Err(Error::Other(format!(
                    "render_full_path_in_tx: ancestor {} missing (broken FK chain from {})",
                    c.0, id.0
                )));
            }
        };
        segments.push(name);
        cur = match parent {
            Some(s) => Some(ContainerId(parse_uuid(&s, "container.parent_id")?)),
            None => None,
        };
        first = false;
    }
    segments.reverse();
    Ok(segments.join("/"))
}

fn allocate_container_change_set(
    tx: &Transaction<'_>,
    container_id: &ContainerId,
    kind: ContainerChangeSetKind,
    payload: &str,
    changed_at: i64,
) -> Result<ContainerChangeSetToken> {
    let new_seq: i64 = tx
        .query_row(
            "INSERT INTO container_change_set \
             (container_id, kind, payload, changed_at) \
             VALUES (?1, ?2, ?3, ?4) RETURNING container_change_set_seq;",
            params![
                container_id.0.to_string(),
                container_change_set_kind_to_sql(kind),
                payload,
                changed_at,
            ],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("insert container_change_set", e))?;
    Ok(ContainerChangeSetToken(new_seq as u64))
}

fn insert_change_row(
    tx: &Transaction<'_>,
    container_id: &ContainerId,
    change_seq: ChangeToken,
    kind: i64,
    seq: Seq,
    item_id: Option<&ItemId>,
) -> Result<()> {
    let item_str = item_id.map(|i| i.0.to_string());
    tx.execute(
        "INSERT INTO container_change \
         (container_id, change_seq, kind, seq, item_id, changed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            container_id.0.to_string(),
            change_seq.0 as i64,
            kind,
            seq.0 as i64,
            item_str,
            now_ms(),
        ],
    )
    .map_err(|e| map_sql_err("insert container_change", e))?;
    Ok(())
}

fn insert_membership(
    tx: &Transaction<'_>,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
    flags: Flags,
    tags: &[String],
    added_at: i64,
) -> Result<(Placement, SetChangeToken)> {
    if !container_exists_tx(tx, container_id)? {
        return Err(Error::ContainerNotFound(container_id.0.to_string()));
    }
    let unread_delta = unread_delta_of(flags);
    let (seq, change_seq) = allocate_seq_and_change(tx, container_id, unread_delta)?;
    let tags_json = if tags.is_empty() {
        None
    } else {
        // Canonicalize to a sorted, deduplicated JSON array so the stored
        // `membership.tags` always has the set semantics every read /
        // keyword-write path assumes (matching `encode_tags_for_membership`'s
        // output). A raw `&[String]` create-path caller therefore cannot
        // persist duplicate or out-of-order keywords.
        let canonical: std::collections::BTreeSet<&str> = tags.iter().map(String::as_str).collect();
        Some(
            serde_json::to_string(&canonical)
                .map_err(|e| Error::Other(format!("encode tags: {e}")))?,
        )
    };
    tx.execute(
        "INSERT INTO membership \
         (item_id, container_id, seq, change_seq, flags, tags, added_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            item_id.0.to_string(),
            container_id.0.to_string(),
            seq.0 as i64,
            change_seq.0 as i64,
            flags.0 as i64,
            tags_json,
            added_at,
        ],
    )
    .map_err(|e| map_sql_err("insert membership", e))?;
    insert_change_row(
        tx,
        container_id,
        change_seq,
        CHANGE_KIND_ADDED,
        seq,
        Some(item_id),
    )?;
    let set_change_seq = allocate_set_change(
        tx,
        set,
        SetChangeRow {
            container_id,
            container_change_seq: change_seq,
            item_id,
            kind: ChangeKind::Added,
            seq,
            changed_at: added_at,
        },
    )?;
    Ok((
        Placement {
            container: *container_id,
            seq,
            change_seq,
        },
        set_change_seq,
    ))
}

/// What `remove_membership_inner` reports back so callers can both
/// build their report struct and emit the right notifier event.
struct RemovalInfo {
    old_seq: Seq,
    change_seq: ChangeToken,
    set_change_seq: SetChangeToken,
}

/// Remove the (item, container) row, decrement counters, write a
/// removed-changelog entry, and allocate a `set_change` row.
/// Returns the seq the row had before removal, the container's new
/// change_seq, and the set-wide change_seq.
fn remove_membership_inner(
    tx: &Transaction<'_>,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
) -> Result<RemovalInfo> {
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT seq, flags FROM membership \
             WHERE item_id = ?1 AND container_id = ?2",
            params![item_id.0.to_string(), container_id.0.to_string()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read membership for remove", e))?;
    let (seq, old_flags) = match row {
        Some(r) => r,
        None => {
            return Err(Error::Other(format!(
                "membership ({}, {}) not present",
                item_id.0, container_id.0
            )));
        }
    };
    let unread_delta = -unread_delta_of(Flags(old_flags as u32));
    tx.execute(
        "DELETE FROM membership WHERE item_id = ?1 AND container_id = ?2",
        params![item_id.0.to_string(), container_id.0.to_string()],
    )
    .map_err(|e| map_sql_err("delete membership", e))?;
    let new_change = bump_change_seq(tx, container_id, unread_delta, -1)?;
    insert_change_row(
        tx,
        container_id,
        new_change,
        CHANGE_KIND_REMOVED,
        Seq(seq as u32),
        Some(item_id),
    )?;
    let set_change_seq = allocate_set_change(
        tx,
        set,
        SetChangeRow {
            container_id,
            container_change_seq: new_change,
            item_id,
            kind: ChangeKind::Removed,
            seq: Seq(seq as u32),
            changed_at: now_ms(),
        },
    )?;
    Ok(RemovalInfo {
        old_seq: Seq(seq as u32),
        change_seq: new_change,
        set_change_seq,
    })
}

// ---------------------------------------------------------------------------
// Phase 8d.1 — typed `SqliteSetTx::*` helpers
// ---------------------------------------------------------------------------
//
// These helpers run inside an existing per-set IMMEDIATE transaction
// owned by `SqliteCasMds::with_set_tx`. They differ from the public
// `Mds::*` paths above in two ways:
//
//   1. They never open their own transaction (the caller already
//      did) and never re-enter the per-set `Mutex` (the caller
//      already holds it via `with_conn_mut`). Calling a public
//      `Mds::*` mutation method from inside a `with_set_tx` closure
//      would either deadlock or split the atomic boundary.
//
//   2. They append events to a `BufferedEvents` rather than calling
//      `Notifier::publish` / `EventSink::emit` directly — so a
//      rolled-back closure cannot leak ghost events. The buffer is
//      drained by `with_set_tx` after the SQL commit succeeds.
//
// The lower-level helpers (`insert_membership`, `remove_membership_inner`,
// `bump_change_seq`, `allocate_set_change`, …) are tx-taking and shared
// with the public `Mds::*` paths; only the orchestration layer
// (transaction lifecycle + event dispatch) differs.

/// Idempotent lookup-then-create by `(parent, name)`. Returns the
/// existing `ContainerId` if a sibling with that name already exists,
/// otherwise inserts a new container row and buffers a
/// `ContainerCreated` Bus event.
///
/// Used by maild's `MailStore` to provision the per-account
/// upload-staging container exactly once, even if multiple uploads
/// race on a fresh account.
pub fn ensure_container_by_name_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    parent: Option<&ContainerId>,
    name: &str,
    attrs: &ContainerAttrs,
) -> Result<ContainerId> {
    if name.is_empty() {
        return Err(Error::Other("container name must be non-empty".into()));
    }
    // Lookup first. SQLite NULL handling: equality `parent_id = NULL`
    // never matches, so split the query on parent presence.
    let existing: Option<String> = match parent {
        None => tx
            .query_row(
                "SELECT id FROM container WHERE parent_id IS NULL AND name = ?1",
                params![name],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| map_sql_err("ensure_container_by_name lookup (root)", e))?,
        Some(p) => tx
            .query_row(
                "SELECT id FROM container WHERE parent_id = ?1 AND name = ?2",
                params![p.0.to_string(), name],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| map_sql_err("ensure_container_by_name lookup (child)", e))?,
    };
    if let Some(s) = existing {
        return Ok(ContainerId(parse_uuid(&s, "container.id")?));
    }
    if let Some(p) = parent
        && !container_exists_tx(tx, p)?
    {
        return Err(Error::ContainerNotFound(p.0.to_string()));
    }
    let id = ContainerId(uuid::Uuid::now_v7());
    let attrs_json = attrs_to_json(attrs)?;
    let parent_str = parent.map(|p| p.0.to_string());
    let now = now_ms();
    let res = tx.execute(
        "INSERT INTO container \
         (id, parent_id, name, attrs, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.0.to_string(), parent_str, name, attrs_json, now],
    );
    match res {
        Ok(_) => {
            // Only the create branch allocates a lifecycle row.
            // The race-catch branch below sees a row that some
            // other writer already produced — *they* are responsible
            // for their own CONTAINER_CREATED (their tx had its own
            // events buffer). Emitting one here would mean two
            // rows for one logical create.
            let full_path = render_full_path_in_tx(tx, &id)?;
            let payload = serde_json::json!({
                "name": name,
                "parent_id": parent.map(|p| p.0.to_string()),
                "full_path": full_path,
            })
            .to_string();
            allocate_container_change_set(tx, &id, ContainerChangeSetKind::Created, &payload, now)?;
            events.push_sink(MdsEvent::ContainerCreated(ContainerCreated {
                set_id: *set,
                container_id: id,
                parent_id: parent.copied(),
                name: name.to_string(),
            }));
            Ok(id)
        }
        // Race against a concurrent ensure_container_by_name in another
        // SetTx: SQLite's UNIQUE(parent_id, name) constraint fires.
        // The concurrent writer's BEGIN IMMEDIATE serialises us, so by
        // the time we see ConstraintViolation here the row exists —
        // re-read to return its id rather than surfacing a spurious
        // error. parent_id NULL again needs the IS-NULL form.
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let s: Option<String> = match parent {
                None => tx
                    .query_row(
                        "SELECT id FROM container WHERE parent_id IS NULL AND name = ?1",
                        params![name],
                        |r| r.get::<_, String>(0),
                    )
                    .optional(),
                Some(p) => tx
                    .query_row(
                        "SELECT id FROM container WHERE parent_id = ?1 AND name = ?2",
                        params![p.0.to_string(), name],
                        |r| r.get::<_, String>(0),
                    )
                    .optional(),
            }
            .map_err(|e| map_sql_err("ensure_container_by_name re-read", e))?;
            match s {
                Some(s) => Ok(ContainerId(parse_uuid(&s, "container.id")?)),
                None => Err(Error::Other(format!(
                    "ensure_container_by_name: constraint violation but no row visible for name {name:?}"
                ))),
            }
        }
        Err(e) => Err(map_sql_err("insert container", e)),
    }
}

/// Strict create — fails if a sibling with the same `(parent, name)`
/// already exists. The non-idempotent counterpart of
/// [`ensure_container_by_name_in_tx`]: maild's `MailStore::create_mailbox`
/// must surface a duplicate-name collision to the JMAP layer rather
/// than silently aliasing onto the existing container.
///
/// Errors:
/// - `Error::Other("container name must be non-empty")` for empty name.
/// - `Error::ContainerNotFound(parent)` if `parent` is `Some` but does
///   not exist in this set.
/// - `Error::ContainerAlreadyExists { parent, name }` if a sibling
///   already has the requested name.
///
/// Buffers a `ContainerCreated` Bus event for post-commit replay, per
/// the typed-helper discipline (no eager publish from inside a
/// transaction).
pub fn create_container_named_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    parent: Option<&ContainerId>,
    name: &str,
    attrs: &ContainerAttrs,
) -> Result<ContainerId> {
    if name.is_empty() {
        return Err(Error::Other("container name must be non-empty".into()));
    }
    if let Some(p) = parent
        && !container_exists_tx(tx, p)?
    {
        return Err(Error::ContainerNotFound(p.0.to_string()));
    }
    let id = ContainerId(uuid::Uuid::now_v7());
    let attrs_json = attrs_to_json(attrs)?;
    let parent_str = parent.map(|p| p.0.to_string());
    let now = now_ms();
    let res = tx.execute(
        "INSERT INTO container \
         (id, parent_id, name, attrs, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.0.to_string(), parent_str, name, attrs_json, now],
    );
    match res {
        Ok(_) => {
            // Render path against the just-inserted row; failure here
            // is a substrate-corruption surface (we INSERTed and then
            // can't walk our own row), not a user error — bubble it.
            let full_path = render_full_path_in_tx(tx, &id)?;
            let payload = serde_json::json!({
                "name": name,
                "parent_id": parent.map(|p| p.0.to_string()),
                "full_path": full_path,
            })
            .to_string();
            allocate_container_change_set(tx, &id, ContainerChangeSetKind::Created, &payload, now)?;
            events.push_sink(MdsEvent::ContainerCreated(ContainerCreated {
                set_id: *set,
                container_id: id,
                parent_id: parent.copied(),
                name: name.to_string(),
            }));
            Ok(id)
        }
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(Error::ContainerAlreadyExists {
                parent: parent.map(|p| p.0.to_string()),
                name: name.to_string(),
            })
        }
        Err(e) => Err(map_sql_err("insert container", e)),
    }
}

/// Walk `candidate_parent`'s ancestors inside an open `Transaction`;
/// returns `true` if `id` appears in the chain. Tx-scoped twin of
/// [`would_form_cycle`] — kept so cycle detection runs against the
/// same isolated snapshot as the rename's existence checks.
fn would_form_cycle_tx(
    tx: &Transaction<'_>,
    id: &ContainerId,
    candidate_parent: &ContainerId,
) -> Result<bool> {
    let mut current = Some(*candidate_parent);
    while let Some(c) = current {
        if &c == id {
            return Ok(true);
        }
        let next: Option<String> = tx
            .query_row(
                "SELECT parent_id FROM container WHERE id = ?1",
                params![c.0.to_string()],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| map_sql_err("walk parents", e))?
            .flatten();
        current = match next {
            Some(s) => Some(ContainerId(parse_uuid(&s, "container.parent_id")?)),
            None => None,
        };
    }
    Ok(false)
}

/// Rename / reparent a container inside the caller's per-set tx.
/// Mirrors [`rename_container`]'s invariants but buffers the
/// `ContainerRenamed` event for post-commit replay.
///
/// Errors:
/// - `Error::Other("container name must be non-empty")` for empty name.
/// - `Error::ContainerNotFound(id)` if the source does not exist.
/// - `Error::ContainerNotFound(p)` if `new_parent` is `Some` but does
///   not exist in this set.
/// - `Error::Other("container cannot be its own parent")` if
///   `new_parent == Some(id)`.
/// - `Error::Other("rename would form a parent cycle")` if the new
///   parent is a descendant of `id`.
/// - `Error::ContainerAlreadyExists { parent, name }` if a sibling
///   already has `new_name` under `new_parent`.
pub fn rename_container_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    id: &ContainerId,
    new_parent: Option<&ContainerId>,
    new_name: &str,
) -> Result<()> {
    if new_name.is_empty() {
        return Err(Error::Other("container name must be non-empty".into()));
    }
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT name, parent_id FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|e| map_sql_err("read container name + parent", e))?;
    let (old_name, old_parent_str) = match row {
        Some(t) => t,
        None => return Err(Error::ContainerNotFound(id.0.to_string())),
    };
    if let Some(p) = new_parent {
        if !container_exists_tx(tx, p)? {
            return Err(Error::ContainerNotFound(p.0.to_string()));
        }
        if p == id {
            return Err(Error::Other("container cannot be its own parent".into()));
        }
        if would_form_cycle_tx(tx, id, p)? {
            return Err(Error::Other("rename would form a parent cycle".into()));
        }
    }
    let new_parent_str = new_parent.map(|p| p.0.to_string());
    // Snapshot the pre-update path before the UPDATE rewrites it.
    let old_full_path = render_full_path_in_tx(tx, id)?;
    let res = tx.execute(
        "UPDATE container SET parent_id = ?1, name = ?2 WHERE id = ?3",
        params![new_parent_str, new_name, id.0.to_string()],
    );
    match res {
        Ok(_) => {
            // Discriminate name-only / parent-only / both. A no-op
            // rename (caller passed the same name + same parent)
            // skips the lifecycle row entirely — the JMAP/IMAP
            // `Mailbox/changes` consumer should not see a phantom
            // event. The legacy `ContainerRenamed` Bus event is
            // left untouched on that branch (out of scope here).
            let name_changed = old_name != new_name;
            let parent_changed = old_parent_str != new_parent_str;
            if name_changed || parent_changed {
                let new_full_path = render_full_path_in_tx(tx, id)?;
                let mut changed_props: Vec<&'static str> = Vec::new();
                if name_changed {
                    changed_props.push("name");
                }
                if parent_changed {
                    changed_props.push("parentId");
                }
                let payload = serde_json::json!({
                    "old_name": old_name,
                    "new_name": new_name,
                    "old_parent_id": old_parent_str,
                    "new_parent_id": new_parent_str,
                    "old_full_path": old_full_path,
                    "new_full_path": new_full_path,
                    "changed_props": changed_props,
                })
                .to_string();
                allocate_container_change_set(
                    tx,
                    id,
                    ContainerChangeSetKind::Renamed,
                    &payload,
                    now_ms(),
                )?;
            }
            events.push_sink(MdsEvent::ContainerRenamed(ContainerRenamed {
                set_id: *set,
                container_id: *id,
                old_name,
                new_name: new_name.to_string(),
            }));
            Ok(())
        }
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(Error::ContainerAlreadyExists {
                parent: new_parent.map(|p| p.0.to_string()),
                name: new_name.to_string(),
            })
        }
        Err(e) => Err(map_sql_err("rename container", e)),
    }
}

/// Delete a container inside the caller's tx. Used by maild's
/// `MailStore::destroy_mailbox`. Mirrors the public
/// [`delete_container`]'s SQL surface — `ON DELETE RESTRICT` on
/// `container.parent_id` rejects the delete if any child container
/// remains, surfaced as `Error::Other("container has child
/// containers; delete them first")`. `ON DELETE CASCADE` on
/// `membership.container_id` drops the per-container memberships,
/// leaving any items that were *only* in this container as orphans
/// in the `item` table — callers that need orphan cleanup must
/// enumerate items first and call [`remove_membership_in_tx`] to
/// drop blob_refs and item rows. Buffers the per-container channel
/// drop *and* the `ContainerDeleted` Bus event for post-commit
/// replay; on rollback both are dropped without effect.
pub fn delete_container_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    id: &ContainerId,
) -> Result<()> {
    // Snapshot name + full_path BEFORE the DELETE — the row is
    // gone afterwards and `render_full_path_in_tx` would surface
    // ContainerNotFound. `render_full_path_in_tx` already returns
    // ContainerNotFound if the row is missing, so it doubles as
    // the existence check the legacy path performed via
    // `container_exists_tx`.
    let old_name: String = tx
        .query_row(
            "SELECT name FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| map_sql_err("read container name for destroy snapshot", e))?
        .ok_or_else(|| Error::ContainerNotFound(id.0.to_string()))?;
    let old_full_path = render_full_path_in_tx(tx, id)?;
    let res = tx.execute(
        "DELETE FROM container WHERE id = ?1",
        params![id.0.to_string()],
    );
    match res {
        Ok(_) => {
            let payload = serde_json::json!({
                "name": old_name,
                "full_path": old_full_path,
            })
            .to_string();
            allocate_container_change_set(
                tx,
                id,
                ContainerChangeSetKind::Destroyed,
                &payload,
                now_ms(),
            )?;
            events.push_drop_channel(*set, *id);
            events.push_sink(MdsEvent::ContainerDeleted(ContainerDeleted {
                set_id: *set,
                container_id: *id,
            }));
            Ok(())
        }
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(Error::Other(
                "container has child containers; delete them first".into(),
            ))
        }
        Err(e) => Err(map_sql_err("delete container", e)),
    }
}

/// Substrate convention for the `ContainerAttrs.extra` key carrying
/// JMAP's `Mailbox.sortOrder` (RFC 8621 §2.4). Hardcoded here because
/// `update_container_attrs_in_tx` is the diff authority — it has to
/// know which extra-key to compare. Maild's `MailStore` writes
/// through the same key (`mailstore/mod.rs`'s `JMAP_SORT_ORDER_KEY`);
/// the two must agree. A future commit may extract this to a single
/// shared `pub const` re-exported from `cosmix-mds`; for MCS-P1 the
/// duplication is acceptable and well-documented.
const JMAP_SORT_ORDER_EXTRA_KEY: &str = "jmap_sort_order";

/// Read the JMAP sort-order out of an attrs `extra` blob. Absent /
/// non-object / non-integer / out-of-u32-range values all collapse to
/// `0`, matching maild's `jmap_sort_order_from_attrs` so the diff
/// can't observe a phantom change when both sides read absence as
/// the same "zero" sentinel.
fn jmap_sort_order_from_extra(extra: &serde_json::Value) -> u32 {
    extra
        .as_object()
        .and_then(|m| m.get(JMAP_SORT_ORDER_EXTRA_KEY))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

/// Patch a container's attrs blob in-place inside the caller's tx
/// and, if any JMAP-visible property actually changed, allocate one
/// `container_change_set` row (`CONTAINER_ATTRS_CHANGED`) describing
/// the diff. Returns the list of properties that changed; an empty
/// list means the patch was a no-op (no UPDATE, no row).
///
/// This is the **only** substrate write surface for container-attrs
/// mutation. The legacy raw `UPDATE container SET attrs = ?`
/// previously executed from maild's `MailStore::update_mailbox`
/// bypassed this discipline; MCS-P1-D cuts that escape hatch over
/// to call this function so the lifecycle stream guarantee
/// ("every container mutation produces a row") holds end-to-end.
///
/// Diff scope is the JMAP-visible triple `{role, sortOrder,
/// isSubscribed}` (RFC 8621 §2.4):
/// - `role` ←→ `ContainerAttrs.special_use` (Option<String>;
///   `None` vs `Some("\\Inbox")` etc.).
/// - `sortOrder` ←→ `ContainerAttrs.extra["jmap_sort_order"]`
///   decoded via [`jmap_sort_order_from_extra`] (absent ≡ 0).
/// - `isSubscribed` ←→ `ContainerAttrs.subscribed`.
///
/// Fields outside that triple (any other `extra` keys, future
/// substrate-private fields) do NOT participate in the diff. A
/// caller that flips ONLY such fields gets `changed_props == []`,
/// no lifecycle row, and no UPDATE — the new attrs blob is *not*
/// persisted on that call. Those fields ride along only when at
/// least one JMAP-visible property also changed: the UPDATE that
/// fires for the JMAP diff writes the whole blob (including
/// untouched extras) atomically. This is deliberate: the lifecycle
/// stream is the JMAP `Mailbox/changes` substrate, and emitting a
/// row for a property no JMAP consumer observes would generate
/// noise the cursor cannot interpret. A future kind
/// (`CONTAINER_EXTRA_CHANGED` or similar) can land additively if a
/// non-JMAP consumer ever needs to mutate extras in isolation.
///
/// Payload shape per `_doc/planned/mailbox-changes-substrate.md`:
/// `{changed_props, old_values, new_values}`. Each `*_values` is
/// scoped to the changed keys only — a consumer that sees
/// `changed_props == ["role"]` will find only `role` in
/// `old_values` / `new_values`, never a phantom `sortOrder` /
/// `isSubscribed` carrying the same value.
///
/// `events` / `set` are accepted for signature symmetry with the
/// sibling `*_container_in_tx` helpers (and so a future
/// `MdsEvent::ContainerAttrsChanged` Bus buffered-event can slot in
/// without churning every caller); MCS-P1-D does not push any Bus
/// event yet — the lifecycle row alone satisfies the spec's
/// "every mutation produces a row" guarantee.
///
/// Errors:
/// - `Error::ContainerNotFound(id)` if `id` is not in this set.
/// - `Error::Other(...)` if the stored attrs blob fails to decode
///   (substrate corruption) or the new attrs fail to encode.
pub fn update_container_attrs_in_tx(
    tx: &Transaction<'_>,
    _events: &mut BufferedEvents,
    _set: &SetId,
    id: &ContainerId,
    new_attrs: &ContainerAttrs,
) -> Result<UpdatedContainerProps> {
    let cur_raw: Option<String> = tx
        .query_row(
            "SELECT attrs FROM container WHERE id = ?1",
            params![id.0.to_string()],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| map_sql_err("read container attrs for update", e))?;
    let cur_raw = cur_raw.ok_or_else(|| Error::ContainerNotFound(id.0.to_string()))?;
    let old_attrs = attrs_from_json(&cur_raw)?;

    let mut changed_props: Vec<&'static str> = Vec::new();
    if old_attrs.special_use != new_attrs.special_use {
        changed_props.push("role");
    }
    let old_sort = jmap_sort_order_from_extra(&old_attrs.extra);
    let new_sort = jmap_sort_order_from_extra(&new_attrs.extra);
    if old_sort != new_sort {
        changed_props.push("sortOrder");
    }
    if old_attrs.subscribed != new_attrs.subscribed {
        changed_props.push("isSubscribed");
    }

    if changed_props.is_empty() {
        // No JMAP-visible property changed — skip the UPDATE and the
        // lifecycle row entirely. Returning empty `changed_props` is
        // the documented no-op signal; the caller's tx still commits
        // (any other writes it batched are preserved). See the
        // function-level doc on why this scope is deliberately
        // narrower than "any byte of attrs changed".
        return Ok(UpdatedContainerProps { changed_props });
    }

    let encoded = attrs_to_json(new_attrs)?;
    let n = tx
        .execute(
            "UPDATE container SET attrs = ?1 WHERE id = ?2",
            params![encoded, id.0.to_string()],
        )
        .map_err(|e| map_sql_err("update container attrs", e))?;
    if n == 0 {
        // The pre-UPDATE SELECT above already confirmed the row
        // exists inside this same tx; `n == 0` means the row has
        // been deleted under us without the per-set mutex —
        // substrate corruption. Surface loudly rather than silently
        // succeed.
        return Err(Error::ContainerNotFound(id.0.to_string()));
    }

    let mut old_values = serde_json::Map::with_capacity(changed_props.len());
    let mut new_values = serde_json::Map::with_capacity(changed_props.len());
    for prop in &changed_props {
        match *prop {
            "role" => {
                old_values.insert(
                    "role".into(),
                    match &old_attrs.special_use {
                        Some(s) => serde_json::Value::String(s.clone()),
                        None => serde_json::Value::Null,
                    },
                );
                new_values.insert(
                    "role".into(),
                    match &new_attrs.special_use {
                        Some(s) => serde_json::Value::String(s.clone()),
                        None => serde_json::Value::Null,
                    },
                );
            }
            "sortOrder" => {
                old_values.insert("sortOrder".into(), serde_json::Value::from(old_sort));
                new_values.insert("sortOrder".into(), serde_json::Value::from(new_sort));
            }
            "isSubscribed" => {
                old_values.insert(
                    "isSubscribed".into(),
                    serde_json::Value::Bool(old_attrs.subscribed),
                );
                new_values.insert(
                    "isSubscribed".into(),
                    serde_json::Value::Bool(new_attrs.subscribed),
                );
            }
            // The `changed_props` array is built immediately above
            // from a closed match on the same three string
            // literals — any other value here is a substrate bug,
            // not a runtime input.
            other => unreachable!("changed_props leaked unknown key {other:?}"),
        }
    }
    let payload = serde_json::json!({
        "changed_props": changed_props,
        "old_values": serde_json::Value::Object(old_values),
        "new_values": serde_json::Value::Object(new_values),
    })
    .to_string();
    allocate_container_change_set(
        tx,
        id,
        ContainerChangeSetKind::AttrsChanged,
        &payload,
        now_ms(),
    )?;
    Ok(UpdatedContainerProps { changed_props })
}

/// Stage one CAS-deduplicated item with a **single** membership in
/// `container_id`. The blob bytes must already be on disk via
/// `Mds::put_blob`; this writes only the `item` row, the cross-DB
/// `blob_ref` (refcount += 1), and the `membership` row, all inside
/// the caller's tx. `blob_size` is supplied by the caller (maild has
/// it from the staged-upload path) so we don't re-read the CAS dir.
///
/// **Single-membership/staging only.** This emits a per-container
/// `ItemAdded` notifier event AND a single-element-arrays Bus
/// `MdsEvent::ItemAdded` covering only `container_id`. It MUST NOT be
/// paired with [`add_membership_in_tx`] to construct a brand-new
/// multi-mailbox item: that would publish one single-element
/// `ItemAdded` followed by N `ItemCopied` events, which is the wrong
/// shape for a creation (downstream consumers expect ONE aggregate
/// `ItemAdded` with parallel `container_ids[]` arrays for a fresh
/// item, matching public `add_item`'s shape). For brand-new items
/// with N≥1 destinations use [`add_item_with_memberships_in_tx`]
/// instead. `add_membership_in_tx`'s `ItemCopied` shape is correct
/// only for adding a placement to an *already-existing* item (e.g.
/// JMAP `Email/set update` adding a mailbox).
///
/// Returns the freshly allocated `ItemId` and the initial membership's
/// `Placement` (the seq + change_seq the membership row received). The
/// `Placement` is also embedded in the buffered `ItemAdded` event for
/// any observer that cares.
#[allow(clippy::too_many_arguments)]
pub fn add_staging_item_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    container_id: &ContainerId,
    blob_hash: &BlobHash,
    blob_size: u64,
    flags: Flags,
    tags: &[String],
) -> Result<(ItemId, Placement)> {
    let blob_hex = blob::hex(blob_hash);
    let item_id = ItemId(uuid::Uuid::now_v7());
    let now = now_ms();

    tx.execute(
        "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![item_id.0.to_string(), blob_hex, blob_size as i64, now],
    )
    .map_err(|e| map_sql_err("insert staging item", e))?;

    crate::blob_index::add_blob_ref_in_tx(tx, set, &item_id, blob_hash, blob_size)?;

    let (placement, _set_seq) =
        insert_membership(tx, set, &item_id, container_id, flags, tags, now)?;

    events.push_notifier(
        *set,
        *container_id,
        ContainerEvent::ItemAdded {
            item_id,
            seq: placement.seq,
            change_seq: placement.change_seq,
        },
    );
    events.push_sink(MdsEvent::ItemAdded(ItemAdded {
        set_id: *set,
        item_id,
        blob_hash: blob_hex,
        container_ids: vec![*container_id],
        change_seqs: vec![placement.change_seq],
    }));

    Ok((item_id, placement))
}

/// Stage one CAS-deduplicated item with N≥1 memberships in a single
/// caller's tx — the multi-membership counterpart of
/// [`add_staging_item_in_tx`] and the in-tx counterpart of public
/// [`add_item`]. Mirrors `add_item`'s aggregate Bus shape: ONE
/// `ItemAdded` Bus event with parallel `container_ids[]` /
/// `change_seqs[]` arrays, plus one per-container notifier `ItemAdded`
/// event per dest. This is the primitive maild's `MailStore` calls for
/// JMAP `Email/set create` / `Email/import` so a multi-mailbox create
/// produces the same downstream-Bus shape as the legacy `add_item`
/// path.
///
/// `memberships` is a slice of `(container_id, flags)`; per the v1.1 §3
/// "all memberships agree" invariant the per-membership flags arg is
/// effectively a fresh-item-wide value (JMAP `Email/set create` carries
/// a single top-level `keywords` object; differing per-mailbox flags
/// would violate the invariant and is the caller's responsibility to
/// not request).
///
/// Errors:
///   * `Error::Other("add_item_with_memberships: at least one
///      membership required")` if the slice is empty.
///   * `Error::Other("add_item_with_memberships: duplicate container
///     in memberships slice: <id>")` if the same container appears
///     twice in the slice. Prevalidated **before any writes** so a
///     caller catching this error inside `with_set_tx` cannot commit
///     a half-inserted item.
///   * `Error::ContainerNotFound` if any dest container is missing.
///     Prevalidated **before any writes** for the same reason.
///
/// Prevalidation note: `memberships` is fully checked (non-empty,
/// unique containers, all containers exist) **before** the `item` /
/// `blob_ref` / membership row inserts run. This guarantees that if
/// the helper returns one of the **prevalidation** errors above (the
/// three listed under "Errors:") no rows were touched — a caller that
/// catches such an error inside `with_set_tx` and returns `Ok(())`
/// will commit a clean tx, not a partial item.
///
/// Storage-layer errors raised *after* prevalidation succeeds (e.g.
/// `add_blob_ref_in_tx` failing on a blobs-DB I/O error, or
/// `insert_membership` returning a SQL error for a reason not covered
/// by prevalidation) can still occur post-write. Those rely on the
/// caller letting the `Err` propagate so `with_set_tx` rolls back the
/// tx; swallowing them inside `with_set_tx` would commit a partial
/// item. The prevalidation guarantee covers exactly the three errors
/// listed above.
///
/// Returns the freshly allocated `ItemId` and the placement vector in
/// the same order as `memberships`. Buffers events for post-commit
/// replay; on rollback all buffered events drop.
pub fn add_item_with_memberships_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    blob_hash: &BlobHash,
    blob_size: u64,
    memberships: &[(ContainerId, Flags)],
    tags: &[String],
) -> Result<(ItemId, Vec<Placement>)> {
    // ---- Prevalidation: must run before ANY write so that a caller
    // catching Err inside with_set_tx cannot commit a partial item.
    if memberships.is_empty() {
        return Err(Error::Other(
            "add_item_with_memberships: at least one membership required".into(),
        ));
    }
    {
        let mut seen: std::collections::HashSet<ContainerId> =
            std::collections::HashSet::with_capacity(memberships.len());
        for (cid, _flags) in memberships.iter() {
            if !seen.insert(*cid) {
                return Err(Error::Other(format!(
                    "add_item_with_memberships: duplicate container in memberships slice: {}",
                    cid.0
                )));
            }
        }
    }
    for (cid, _flags) in memberships.iter() {
        if !container_exists_tx(tx, cid)? {
            return Err(Error::ContainerNotFound(cid.0.to_string()));
        }
    }
    // ---- Prevalidation done; from here on out we mutate.
    let blob_hex = blob::hex(blob_hash);
    let item_id = ItemId(uuid::Uuid::now_v7());
    let now = now_ms();

    tx.execute(
        "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![item_id.0.to_string(), blob_hex, blob_size as i64, now],
    )
    .map_err(|e| map_sql_err("insert item", e))?;

    crate::blob_index::add_blob_ref_in_tx(tx, set, &item_id, blob_hash, blob_size)?;

    // Per the v1.1 §3 "all memberships agree on (flags, tags)"
    // invariant, a fresh item carries one uniform tag set across every
    // destination membership — so `tags` is applied to each insert.
    let mut placements: Vec<Placement> = Vec::with_capacity(memberships.len());
    for (container_id, flags) in memberships.iter() {
        let (placement, _set_seq) =
            insert_membership(tx, set, &item_id, container_id, *flags, tags, now)?;
        placements.push(placement);
    }

    // Per-container notifier events, one per dest. Mirrors public
    // `add_item`'s notifier fan-out.
    for p in placements.iter() {
        events.push_notifier(
            *set,
            p.container,
            ContainerEvent::ItemAdded {
                item_id,
                seq: p.seq,
                change_seq: p.change_seq,
            },
        );
    }

    // ONE aggregate Bus event for the whole creation, parallel arrays
    // matching public `add_item`. Downstream Bus consumers reconstruct
    // "new item landed in these mailboxes" from this single event.
    let container_ids: Vec<ContainerId> = placements.iter().map(|p| p.container).collect();
    let change_seqs: Vec<ChangeToken> = placements.iter().map(|p| p.change_seq).collect();
    events.push_sink(MdsEvent::ItemAdded(ItemAdded {
        set_id: *set,
        item_id,
        blob_hash: blob_hex,
        container_ids,
        change_seqs,
    }));

    Ok((item_id, placements))
}

/// Add a new membership to an *existing* item inside the caller's tx —
/// the in-tx counterpart of [`copy_item`]. This is the **post-create**
/// path: the item already exists with ≥1 membership, and we are adding
/// a new placement (e.g. JMAP `Email/set update` adding a mailbox to
/// `mailboxIds`).
///
/// For brand-new items use [`add_item_with_memberships_in_tx`] instead;
/// it emits a single aggregate Bus `ItemAdded` event covering all dest
/// containers, which is the shape downstream consumers expect for
/// creation. `add_membership_in_tx` deliberately emits Bus `ItemCopied`
/// — the user-visible operation IS a copy from the existing
/// memberships into a new one.
///
/// Validates that both the item and the destination container exist
/// and that no membership is already present, then inserts a new
/// membership row whose `(flags, tags)` are *inherited* from any
/// existing membership of the item per the v1.1 §3 "all memberships
/// agree" invariant. The caller-supplied `flags` is the fallback only
/// when the item has zero existing memberships (a "deeply unmoored
/// item being re-anchored" case).
///
/// Returns the new membership's [`Placement`]. Buffers the per-dest
/// `ItemAdded` notifier event and an `ItemCopied` Bus sink event for
/// post-commit replay; on rollback both are dropped.
pub fn add_membership_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
    flags: Flags,
) -> Result<Placement> {
    let now = now_ms();
    if !item_exists(tx, item_id)? {
        return Err(Error::ItemNotFound(item_id.0.to_string()));
    }
    if !container_exists_tx(tx, container_id)? {
        return Err(Error::ContainerNotFound(container_id.0.to_string()));
    }
    if membership_exists(tx, item_id, container_id)? {
        return Err(Error::Other(format!(
            "item {} already in container {}",
            item_id.0, container_id.0
        )));
    }
    let (inh_flags, inh_tags) = match read_any_membership_keywords(tx, item_id)? {
        Some(p) => p,
        None => (flags, Vec::new()),
    };
    let (placement, _set_seq) =
        insert_membership(tx, set, item_id, container_id, inh_flags, &inh_tags, now)?;

    events.push_notifier(
        *set,
        *container_id,
        ContainerEvent::ItemAdded {
            item_id: *item_id,
            seq: placement.seq,
            change_seq: placement.change_seq,
        },
    );
    events.push_sink(MdsEvent::ItemCopied(ItemCopied {
        set_id: *set,
        item_id: *item_id,
        dest: *container_id,
        seq_dest: placement.seq,
        change_seq_dest: placement.change_seq,
    }));

    Ok(placement)
}

/// IMAP COPY shape: like [`add_membership_in_tx`] but inherits the
/// dest membership's `(flags, tags)` from a **specific** source
/// container instead of `ORDER BY container_id LIMIT 1`.
///
/// `add_membership_in_tx` picks any current membership for inheritance,
/// which is fine for JMAP fan-out (the v1.1 §3 "all memberships agree"
/// invariant guarantees they're identical) but wrong for IMAP COPY:
/// IMAP STORE writes per-membership rows (the IMAP §3.2 model deviates
/// from JMAP's union-of-keywords contract), so sibling memberships of
/// the same item can carry different flags. RFC 9051 §6.4.7 requires
/// the COPY destination to inherit from the source mailbox, not from
/// an arbitrary sibling.
///
/// **Source membership is load-bearing, not informational.** If
/// `(item_id, src)` has no row, this returns an error and does NOT
/// fall back to `flags`. Falling back would let a TOCTOU race —
/// snapshot sees the item in `src`, a concurrent EXPUNGE/MOVE removes
/// `src`'s membership, COPY survives because the item still exists
/// via a sibling — silently mint a dest membership with the
/// caller-supplied flags (typically `Flags(0)`) and zero tags,
/// dropping the user's intended source-mailbox flags. The IMAP
/// dispatch always passes the selected source mailbox; an absent
/// `src` membership at this point is a substantive failure.
///
/// Errors: `ItemNotFound`, `ContainerNotFound` (dest), `"item ...
/// already in container ..."` on duplicate dest membership,
/// `"item ... not in source container ..."` when `(item_id, src)`
/// has no membership row. Buffers the same `ItemAdded` notifier +
/// `ItemCopied` Bus events for post-commit replay.
pub fn add_membership_from_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    src: &ContainerId,
    dest: &ContainerId,
) -> Result<Placement> {
    let now = now_ms();
    if !item_exists(tx, item_id)? {
        return Err(Error::ItemNotFound(item_id.0.to_string()));
    }
    if !container_exists_tx(tx, dest)? {
        return Err(Error::ContainerNotFound(dest.0.to_string()));
    }
    if membership_exists(tx, item_id, dest)? {
        return Err(Error::Other(format!(
            "item {} already in container {}",
            item_id.0, dest.0
        )));
    }
    let (inh_flags, inh_tags) = match read_membership_keywords(tx, item_id, src)? {
        Some(p) => p,
        None => {
            return Err(Error::Other(format!(
                "item {} not in source container {}",
                item_id.0, src.0
            )));
        }
    };
    let (placement, _set_seq) =
        insert_membership(tx, set, item_id, dest, inh_flags, &inh_tags, now)?;

    events.push_notifier(
        *set,
        *dest,
        ContainerEvent::ItemAdded {
            item_id: *item_id,
            seq: placement.seq,
            change_seq: placement.change_seq,
        },
    );
    events.push_sink(MdsEvent::ItemCopied(ItemCopied {
        set_id: *set,
        item_id: *item_id,
        dest: *dest,
        seq_dest: placement.seq,
        change_seq_dest: placement.change_seq,
    }));

    Ok(placement)
}

/// Read the dest container's `seq_validity` value inside the caller's
/// tx. Used by maild's `MailStore` to bind the IMAP `UIDVALIDITY` (=
/// `seq_validity`) for a freshly-inserted membership in the same tx
/// that produced the membership's seq, eliminating the
/// follow-up-read-can-see-a-different-validity hazard.
///
/// Errors with [`Error::ContainerNotFound`] if the row is absent.
pub fn container_seq_validity_in_tx(
    tx: &Transaction<'_>,
    container_id: &ContainerId,
) -> Result<u64> {
    let v: Option<i64> = tx
        .query_row(
            "SELECT seq_validity FROM container WHERE id = ?1",
            params![container_id.0.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| map_sql_err("container_seq_validity_in_tx", e))?;
    match v {
        Some(v) => Ok(v as u64),
        None => Err(Error::ContainerNotFound(container_id.0.to_string())),
    }
}

/// Atomic move within a SetTx, with v1.1 §3 keyword inheritance. Mirrors
/// the public `move_item` mutation path's invariants and event shape
/// — including the dual `ItemMoved` notifier publish on src+dest and
/// the Bus `mds.item.moved` event — but defers all event emission to
/// the post-commit drain.
pub fn move_item_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    src: &ContainerId,
    dest: &ContainerId,
    flags: Flags,
) -> Result<MoveReport> {
    if src == dest {
        return Err(Error::Other("move_item: src and dest must differ".into()));
    }
    let now = now_ms();
    if !item_exists(tx, item_id)? {
        return Err(Error::ItemNotFound(item_id.0.to_string()));
    }
    if !container_exists_tx(tx, src)? {
        return Err(Error::ContainerNotFound(src.0.to_string()));
    }
    if !container_exists_tx(tx, dest)? {
        return Err(Error::ContainerNotFound(dest.0.to_string()));
    }
    if !membership_exists(tx, item_id, src)? {
        return Err(Error::Other(format!(
            "item {} not in source container {}",
            item_id.0, src.0
        )));
    }
    if membership_exists(tx, item_id, dest)? {
        return Err(Error::Other(format!(
            "item {} already in destination container {}",
            item_id.0, dest.0
        )));
    }
    let (inh_flags, inh_tags) = match read_membership_keywords(tx, item_id, src)? {
        Some(p) => p,
        None => (flags, Vec::new()),
    };
    let removed = remove_membership_inner(tx, set, item_id, src)?;
    let (added, set_seq_dest) =
        insert_membership(tx, set, item_id, dest, inh_flags, &inh_tags, now)?;

    events.push_notifier(
        *set,
        *src,
        ContainerEvent::ItemRemoved {
            item_id: *item_id,
            seq: removed.old_seq,
            change_seq: removed.change_seq,
        },
    );
    events.push_notifier(
        *set,
        *dest,
        ContainerEvent::ItemAdded {
            item_id: *item_id,
            seq: added.seq,
            change_seq: added.change_seq,
        },
    );
    let moved_event = ContainerEvent::ItemMoved {
        item_id: *item_id,
        src: *src,
        dest: *dest,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
        set_change_seq_src: removed.set_change_seq,
        set_change_seq_dest: set_seq_dest,
    };
    events.push_notifier(*set, *src, moved_event.clone());
    events.push_notifier(*set, *dest, moved_event);
    events.push_sink(MdsEvent::ItemMoved(ItemMoved {
        set_id: *set,
        item_id: *item_id,
        src: *src,
        dest: *dest,
        seq_dest: added.seq,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
    }));
    Ok(MoveReport {
        seq_src: removed.old_seq,
        seq_dest: added.seq,
        change_seq_src: removed.change_seq,
        change_seq_dest: added.change_seq,
    })
}

/// Remove the (item, container) membership inside the caller's tx. If
/// the item now has zero memberships, drop its cross-DB `blob_ref`
/// (refcount -= 1) and delete the `item` row so the orphan blob
/// becomes a GC candidate. Mirrors the public `remove_membership`'s
/// cascade exactly — but the tx is the caller's, and events go to the
/// buffer.
///
/// This is the building block the maild expiry worker uses to drop
/// staged items whose TTL has elapsed without finalisation.
pub fn remove_membership_in_tx(
    tx: &Transaction<'_>,
    events: &mut BufferedEvents,
    set: &SetId,
    item_id: &ItemId,
    container_id: &ContainerId,
) -> Result<()> {
    if !membership_exists(tx, item_id, container_id)? {
        return Err(Error::Other(format!(
            "membership ({}, {}) not present",
            item_id.0, container_id.0
        )));
    }
    let removed = remove_membership_inner(tx, set, item_id, container_id)?;
    let remaining: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM membership WHERE item_id = ?1",
            params![item_id.0.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| map_sql_err("count remaining memberships", e))?;
    if remaining == 0 {
        let blob_hex: String = tx
            .query_row(
                "SELECT blob_hash FROM item WHERE id = ?1",
                params![item_id.0.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| map_sql_err("read blob_hash for orphan", e))?;
        let blob_hash = parse_blob_hash(&blob_hex)?;
        crate::blob_index::drop_blob_ref_in_tx(tx, set, item_id, &blob_hash)?;
        tx.execute(
            "DELETE FROM item WHERE id = ?1",
            params![item_id.0.to_string()],
        )
        .map_err(|e| map_sql_err("delete orphan item", e))?;
    }
    events.push_notifier(
        *set,
        *container_id,
        ContainerEvent::ItemRemoved {
            item_id: *item_id,
            seq: removed.old_seq,
            change_seq: removed.change_seq,
        },
    );
    events.push_sink(MdsEvent::ItemRemoved(ItemRemoved {
        set_id: *set,
        item_id: *item_id,
        container_id: *container_id,
        seq: removed.old_seq,
        change_seq: removed.change_seq,
    }));
    Ok(())
}

#[cfg(test)]
mod container_change_set_tests {
    use super::*;
    use crate::schema;
    use uuid::Uuid;

    #[test]
    fn sqlite_failure_mapping_names_busy_snapshot() {
        let sqlite = rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY_SNAPSHOT);
        let mapped = map_sql_err(
            "BEGIN add_item",
            rusqlite::Error::SqliteFailure(sqlite, Some("database is locked".to_owned())),
        );

        assert_eq!(
            mapped.to_string(),
            "other: BEGIN add_item: database is locked (SQLITE_BUSY_SNAPSHOT, code 517)"
        );
    }

    /// Round-trip every kind through the SQL encoder/decoder. Pins
    /// the wire strings against the `CHECK` constraint in
    /// `schema/v1_4.sql`; any drift (typo, renamed variant) surfaces
    /// here, not at INSERT time in some downstream caller.
    #[test]
    fn kind_sql_round_trip() {
        for k in [
            ContainerChangeSetKind::Created,
            ContainerChangeSetKind::Renamed,
            ContainerChangeSetKind::Destroyed,
            ContainerChangeSetKind::AttrsChanged,
        ] {
            let s = container_change_set_kind_to_sql(k);
            let back = container_change_set_kind_from_sql(s).unwrap();
            assert_eq!(back, k, "{s:?} did not round-trip");
        }
    }

    #[test]
    fn fts_query_quotes_tokens_and_neutralises_operators() {
        // Every token is wrapped as a literal FTS5 phrase, joined by space
        // (implicit AND). Bareword operators + punctuation become literal
        // content, never syntax.
        assert_eq!(build_fts_query("invoice"), r#""invoice""#);
        assert_eq!(
            build_fts_query("quarterly invoice"),
            r#""quarterly" "invoice""#
        );
        assert_eq!(build_fts_query("a AND b"), r#""a" "AND" "b""#);
        assert_eq!(
            build_fts_query("alice@example.com"),
            r#""alice@example.com""#
        );
        // An embedded double-quote is DOUBLED (FTS5 string-literal escape) —
        // the injection guard. Input `say "hi"` → tokens `say`, `"hi"` →
        // `"say"` + `"""hi"""`.
        assert_eq!(build_fts_query("say \"hi\""), "\"say\" \"\"\"hi\"\"\"");
    }

    #[test]
    fn fts_query_drops_punctuation_only_and_blank() {
        // Pure-punctuation tokens (no alphanumeric) are dropped — an empty
        // FTS5 phrase is unsafe. A blank / all-punctuation needle → "".
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("   "), "");
        assert_eq!(build_fts_query("- ( ) ^"), "");
        // Mixed: only the alphanumeric token survives.
        assert_eq!(build_fts_query("- report"), r#""report""#);
    }

    #[test]
    fn kind_from_sql_rejects_unknown() {
        let err = container_change_set_kind_from_sql("CONTAINER_REPARENTED").unwrap_err();
        let msg = format!("{err}");
        // The diagnostic exists for a future maintainer reading a log
        // line. Pin every allowed variant by name so a sloppy rewrite
        // (e.g. dropping CONTAINER_ATTRS_CHANGED from the message but
        // leaving the match arm) is caught here, not in production.
        for expected in [
            "CONTAINER_REPARENTED",
            "CONTAINER_CREATED",
            "CONTAINER_RENAMED",
            "CONTAINER_DESTROYED",
            "CONTAINER_ATTRS_CHANGED",
        ] {
            assert!(
                msg.contains(expected),
                "expected diagnostic to mention {expected}, got: {msg}"
            );
        }
    }

    /// `allocate_container_change_set` writes one row and the row
    /// reads back with the fields the caller passed in. The
    /// AUTOINCREMENT contract is pinned independently in
    /// `schema::tests`; here we only check the helper's column
    /// wiring + that two sequential allocations are monotonic
    /// (which proves the AUTOINCREMENT path is wired, not e.g.
    /// hardcoded to 1 by accident).
    #[test]
    fn allocate_writes_row_and_returns_token() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::apply_data_migrations(&mut conn).unwrap();
        let cid = ContainerId(Uuid::new_v4());
        let payload = r#"{"name":"INBOX","full_path":"INBOX"}"#;
        let changed_at: i64 = 1_700_000_000;

        let tok = {
            let tx = conn.transaction().unwrap();
            let tok = allocate_container_change_set(
                &tx,
                &cid,
                ContainerChangeSetKind::Created,
                payload,
                changed_at,
            )
            .unwrap();
            tx.commit().unwrap();
            tok
        };

        let (got_cid, got_kind, got_payload, got_at): (String, String, String, i64) = conn
            .query_row(
                "SELECT container_id, kind, payload, changed_at \
                 FROM container_change_set WHERE container_change_set_seq = ?1;",
                params![tok.0 as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(got_cid, cid.0.to_string());
        assert_eq!(got_kind, "CONTAINER_CREATED");
        assert_eq!(got_payload, payload);
        assert_eq!(got_at, changed_at);

        let tok2 = {
            let tx = conn.transaction().unwrap();
            let tok = allocate_container_change_set(
                &tx,
                &cid,
                ContainerChangeSetKind::Destroyed,
                r#"{"name":"INBOX","full_path":"INBOX"}"#,
                changed_at + 1,
            )
            .unwrap();
            tx.commit().unwrap();
            tok
        };
        assert!(
            tok2.0 > tok.0,
            "expected monotonic seq, got {} -> {}",
            tok.0,
            tok2.0
        );
    }

    /// Rollback of the outer transaction must drop the row — the
    /// allocator inherits the caller's tx, so a failed write
    /// upstream cannot leave a phantom lifecycle row behind. This
    /// pins the contract that lets MCS-P1-C wire the helper inside
    /// `with_set_tx` without a separate rollback story.
    #[test]
    fn allocate_rolls_back_with_outer_tx() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::apply_data_migrations(&mut conn).unwrap();
        let cid = ContainerId(Uuid::new_v4());
        {
            let tx = conn.transaction().unwrap();
            allocate_container_change_set(
                &tx,
                &cid,
                ContainerChangeSetKind::Created,
                r#"{"name":"INBOX","full_path":"INBOX"}"#,
                1,
            )
            .unwrap();
            // No commit — drops at end of scope.
        }
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM container_change_set;", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "rollback should drop the lifecycle row");
    }
}
