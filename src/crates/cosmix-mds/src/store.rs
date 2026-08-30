//! `Mds` trait and the `SqliteCasMds` concrete implementation.
//!
//! Phase 1a covers container-set + container lifecycle. Item / blob
//! / change-notification / GC / verify methods still return
//! `unimplemented!()` and land in subsequent phases per
//! `_doc/2026-04-29-cosmix-mds-plan.md`.
//!
//! # Write-lock order
//!
//! Every in-process `blobs.sqlite` writer queues on one fair write
//! gate before asking SQLite for its file lock. The Rust lock order is:
//!
//! 1. per-set connection mutexes (multiple sets in sorted UUID order),
//! 2. `write_gate`,
//! 3. the direct `blobs.sqlite` connection mutex,
//! 4. the set-cache write lock.
//!
//! A path may skip locks it does not need, but never acquires an
//! earlier lock while holding a later one. Import has no per-set
//! connection yet, so it takes `write_gate` before the cache write
//! lock and holds both through finalisation. `create_set` takes only
//! the cache write lock; it does not write `blobs.sqlite`. SQLite's
//! busy timeout remains necessary for other processes and for other
//! `SqliteCasMds` instances, which do not share this in-process gate.

use crate::blob;
use crate::bus::{
    self, ChangelogPruned, EventSink, GcCompleted, MdsEvent, SetCreated, SetDeleted,
    VerifyCompleted, VerifyFailed,
};
use crate::container;
use crate::error::{Error, Result};
use crate::notifier::Notifier;
use crate::schema;
use crate::types::*;
use parking_lot::{FairMutex, FairMutexGuard, Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Maps a [`ChangelogStream`] to (table, seq column, floor column).
/// Single source of truth for the per-stream SQL bindings used by
/// `prune_changelog` and `changelog_floor`; pinned here so a future
/// stream addition lands in one place.
fn stream_columns(stream: ChangelogStream) -> (&'static str, &'static str, &'static str) {
    match stream {
        ChangelogStream::ContainerChangeSet => (
            "container_change_set",
            "container_change_set_seq",
            "container_change_set_floor",
        ),
        ChangelogStream::SetChange => ("set_change", "set_change_seq", "set_change_floor"),
    }
}

/// Default Pass-1 → Pass-2 quiescence wait for `gc()`.
///
/// Production default. Tests use `with_gc_quiescence` to drop this
/// to ~100ms — the wait *shape* is what's tested, not the wall-clock
/// value.
pub const DEFAULT_GC_QUIESCENCE: Duration = Duration::from_secs(60);

/// Lower bound for the env-var override. The wait exists to cover
/// in-flight delivery commit windows; setting it below this defeats
/// the protection. The in-process `with_gc_quiescence` setter has
/// no minimum because tests need to drop the wait far below this.
pub const MIN_ENV_GC_QUIESCENCE: Duration = Duration::from_secs(5);

/// Env var name for operator-facing override of the GC quiescence
/// wait. Integer seconds; values `< 5` are logged at warn level and
/// ignored (default applies). Read once at `SqliteCasMds::open`
/// time.
pub const ENV_GC_QUIESCENCE: &str = "COSMIX_MDS_GC_QUIESCENCE_SECS";

/// How long a writer waits for the in-process fair gate and for a
/// contended SQLite write lock.
///
/// Every per-set `data.sqlite` connection ATTACHes the box-wide
/// `blobs.sqlite`, so *all* sets serialize on that one file's write
/// lock. Writers within one store instance queue fairly before they
/// reach SQLite. SQLite's busy handler remains the fallback for other
/// processes and other store instances; it is a backoff-and-retry
/// mechanism with no fairness guarantee.
///
/// Five seconds is the production ceiling on purpose: a daemon that
/// blocks a delivery for longer should surface the contention rather
/// than hide it. Callers that knowingly permit longer transactions or
/// cross-instance waits can raise it explicitly via
/// [`SqliteCasMds::open_with_busy_timeout`].
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Mds: Send + Sync {
    // ---- Container-set lifecycle ----
    fn create_set(&self, set: &SetId) -> Result<()>;
    fn delete_set(&self, set: &SetId) -> Result<DeleteReport>;
    fn list_sets(&self) -> Result<Vec<SetId>>;

    // ---- Container lifecycle ----
    fn create_container(
        &self,
        set: &SetId,
        parent: Option<&ContainerId>,
        name: &str,
        attrs: ContainerAttrs,
    ) -> Result<ContainerId>;
    fn rename_container(
        &self,
        set: &SetId,
        id: &ContainerId,
        new_parent: Option<&ContainerId>,
        new_name: &str,
    ) -> Result<()>;
    fn delete_container(&self, set: &SetId, id: &ContainerId) -> Result<()>;
    fn list_containers(&self, set: &SetId) -> Result<Vec<ContainerInfo>>;
    fn container_status(&self, set: &SetId, id: &ContainerId) -> Result<ContainerStatus>;

    // ---- Blob-level ----
    fn put_blob(&self, bytes: &[u8]) -> Result<BlobHash>;
    fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>>;
    fn blob_size(&self, hash: &BlobHash) -> Result<u64>;
    fn blob_exists(&self, hash: &BlobHash) -> Result<bool>;

    // ---- Item ----
    fn add_item(
        &self,
        set: &SetId,
        blob: &BlobHash,
        memberships: &[Membership],
    ) -> Result<AddReport>;
    /// Add the item to `dest` as a new membership. v1.1: the new
    /// membership's `Flags` and per-membership `Tags` are inherited
    /// from any existing membership of the item, per spec §3 "all
    /// memberships agree" invariant. The `flags` argument is a
    /// fallback used only when the item has zero existing memberships
    /// (which `item_exists` precludes today, but the parameter remains
    /// for that edge case + signature stability).
    fn copy_item(
        &self,
        set: &SetId,
        id: &ItemId,
        dest: &ContainerId,
        flags: Flags,
    ) -> Result<CopyReport>;
    /// Move the item from `src` to `dest` atomically. v1.1: the new
    /// `dest` membership inherits `(Flags, Tags)` from the `src`
    /// membership about to be removed, per spec §3 "all memberships
    /// agree" invariant. The `flags` argument is the fallback for the
    /// (impossible today) zero-membership case; in normal operation
    /// it is ignored.
    fn move_item(
        &self,
        set: &SetId,
        id: &ItemId,
        src: &ContainerId,
        dest: &ContainerId,
        flags: Flags,
    ) -> Result<MoveReport>;
    fn remove_membership(&self, set: &SetId, id: &ItemId, container: &ContainerId) -> Result<()>;
    fn store_flags(
        &self,
        set: &SetId,
        id: &ItemId,
        container: &ContainerId,
        flags: Flags,
    ) -> Result<ChangeToken>;
    /// v1.1: write both system flags and per-membership user `Tags`
    /// in one row update on a **single** (item, container) membership.
    /// This is the **IMAP STORE per-mailbox** path: the caller is
    /// intentionally diverging keywords across mailboxes for one item.
    ///
    /// **Not for JMAP `Email/set keywords`** — that operation must
    /// apply to *all* current memberships in one transaction (spec §3
    /// line 335 "all rows commit atomically"). Per-membership commits
    /// would let a paginating `Email/changes` consumer interleave a
    /// partial update and silently desync. Use
    /// [`store_item_keywords`](Mds::store_item_keywords) for that.
    /// Use [`store_flags`](Mds::store_flags) when the caller does not
    /// touch tags.
    fn store_membership_keywords(
        &self,
        set: &SetId,
        id: &ItemId,
        container: &ContainerId,
        flags: Flags,
        tags: Tags,
    ) -> Result<ChangeToken>;
    /// v1.1: JMAP `Email/set keywords` fan-out — apply `flags`+`tags`
    /// to **every** current membership of `id` in one SQL transaction
    /// (spec §3 line 335). All affected memberships commit together or
    /// none do; each membership row gets its own `set_change`
    /// `METADATA_CHANGED` entry, in `set_change_seq` order matching
    /// `ORDER BY container_id`.
    ///
    /// Returns one `(ContainerId, ChangeToken)` per touched
    /// membership. An item with no current memberships returns
    /// `Ok(vec![])` and writes nothing — no transaction, no change
    /// stream allocation, no notifier traffic.
    fn store_item_keywords(
        &self,
        set: &SetId,
        id: &ItemId,
        flags: Flags,
        tags: Tags,
    ) -> Result<Vec<(ContainerId, ChangeToken)>>;
    /// v1.1: containers in which `id` currently has membership, with
    /// the per-membership system [`Flags`] and user [`Tags`]. Returns
    /// an empty `Vec` if the item exists but is fully unmoored, or if
    /// the item id is unknown.
    fn item_memberships(&self, set: &SetId, id: &ItemId)
    -> Result<Vec<(ContainerId, Flags, Tags)>>;
    fn fetch_item(&self, set: &SetId, id: &ItemId, container: &ContainerId) -> Result<ItemRecord>;
    fn fetch_item_meta(&self, set: &SetId, id: &ItemId) -> Result<ItemMeta>;
    /// v1.2: return every item id in the set sharing the given blob
    /// hash. Used by the JMAP `Email/import` idempotency contract:
    /// re-importing the same blob into the same mailbox MUST return
    /// the existing email id rather than allocate a new one. The
    /// caller filters by membership equality — this primitive only
    /// narrows by content hash. Order is unspecified.
    fn find_items_by_blob_hash(&self, set: &SetId, blob_hash: &BlobHash) -> Result<Vec<ItemId>>;
    /// v1.1+: full-text search across the `mail_search` FTS5 projection
    /// (folded headers, subject, plain-text body, normalized addresses)
    /// for the set. `needle` is raw user text — the implementation turns
    /// it into a safe FTS5 MATCH (every token quoted as a literal phrase,
    /// implicit AND). Returns the matching item ids in unspecified order;
    /// a blank / punctuation-only needle matches nothing. The caller
    /// intersects with its own container/keyword scope (this primitive is
    /// set-wide). Backs the JMAP `Email/query` `text` filter.
    fn search_items(&self, set: &SetId, needle: &str) -> Result<Vec<ItemId>>;
    fn list_items(
        &self,
        set: &SetId,
        container: &ContainerId,
        range: SeqRange,
    ) -> Result<Vec<ItemRecord>>;
    fn changes_since(
        &self,
        set: &SetId,
        container: &ContainerId,
        since: ChangeToken,
    ) -> Result<Vec<Change>>;
    /// v1.1: account-/set-wide changes since the given set-scoped
    /// token. Returns up to `limit` rows in `set_change_seq` order
    /// plus `Some(next)` cursor if the stream extends past the
    /// returned page; `None` when the caller is at the tip. Used by
    /// JMAP `Email/changes` and any consumer needing
    /// "what changed anywhere in this set?".
    fn changes_since_set(
        &self,
        set: &SetId,
        since: SetChangeToken,
        limit: usize,
    ) -> Result<(Vec<SetChange>, Option<SetChangeToken>)>;

    // ---- Change notification ----
    fn subscribe(
        &self,
        set: &SetId,
        container: &ContainerId,
    ) -> tokio::sync::broadcast::Receiver<ContainerEvent>;

    /// Subscribe to a container's change stream after verifying it
    /// exists, all while holding the same per-set Mutex that
    /// `delete_container` must acquire to run `drop_channel`. This is
    /// the race-free variant of [`subscribe`]: a concurrent delete
    /// cannot drop the channel between the existence check and the
    /// `Sender::subscribe` call, so subscribers never end up on a
    /// resurrected dead channel for a deleted mailbox. Returns
    /// [`Error::SetNotFound`] or [`Error::ContainerNotFound`] when
    /// either key is missing.
    fn subscribe_existing(
        &self,
        set: &SetId,
        container: &ContainerId,
    ) -> Result<tokio::sync::broadcast::Receiver<ContainerEvent>>;

    // ---- Operational ----
    fn rebuild_index(&self) -> Result<RebuildReport>;
    fn verify_blobs(&self, scope: VerifyScope) -> Result<VerifyReport>;
    fn gc(&self, dry_run: bool) -> Result<GcReport>;
    /// Trim the named per-set changelog stream to its newest `keep_n`
    /// entries, advancing the durable per-stream retention floor to the
    /// highest deleted seq. Changelog reads use `seq > since`, so after
    /// this returns, `mailbox_changes` / `Email/changes` callers whose
    /// `sinceState` is strictly below `PruneReport.new_floor` get the
    /// `cannotCalculateChanges` rejection path; callers with
    /// `sinceState >= new_floor` continue normally (a since-state equal
    /// to the floor still answers from the surviving `seq > floor`
    /// rows).
    ///
    /// Idempotent on no-op: if the stream has fewer than `keep_n` rows
    /// the floor is unchanged and the report is `{0, current_floor}`.
    /// `keep_n == 0` is the "prune everything" sweep — floor advances
    /// to the table's previous max seq. Emits exactly one
    /// `mds.changelog.pruned` Bus event on success (no-op included),
    /// post-commit, with the new floor.
    ///
    /// See `_doc/planned/mailbox-changes-substrate.md` MCS Phase 3.
    fn prune_changelog(
        &self,
        set: &SetId,
        stream: ChangelogStream,
        keep_n: u64,
    ) -> Result<PruneReport>;
    /// Read the current retention floor for `stream` in `set`. Used
    /// by the mailstore read path to detect stale `sinceState` tokens
    /// before paging the stream (returning `cannotCalculateChanges`
    /// rather than over-fetching). A fresh set returns `0` — no rows
    /// have been pruned, so any non-negative `sinceState` is
    /// answerable.
    fn changelog_floor(&self, set: &SetId, stream: ChangelogStream) -> Result<u64>;
    fn stats(&self) -> Result<MdsStats>;
    /// Per-set breakdown. The CLI's `stats --per-set` is the
    /// only consumer in v1; library callers that just want
    /// global counters should call `stats()`.
    fn stats_per_set(&self) -> Result<Vec<PerSetStats>>;
    /// Export `set` to a tar archive at `dest`. Lock discipline:
    /// the per-set Mutex is held for the entire export, so the
    /// `data.sqlite` snapshot and the referenced-blob enumeration
    /// see the same point-in-time state. `blobs.sqlite` is *not*
    /// touched — it is derivable on the receiving site via
    /// `rebuild-index`.
    fn export_set(&self, set: &SetId, dest: &Path) -> Result<ExportReport>;
    /// Import a per-set tar archive produced by `export_set`. The
    /// set id is taken from the tarball's manifest — it is *not* a
    /// caller parameter. Refuses if a set with the same UUID is
    /// already known. After staging + hash-verifying every CAS
    /// blob, the staged `data.sqlite` is rename-installed under
    /// `containers/<uuid>/` and the box-wide `blobs.sqlite` is
    /// updated atomically with one `blob_ref` per imported item.
    fn import_set(&self, tarball: &Path) -> Result<ImportReport>;
}

/// Production implementation: per-set `data.sqlite` + box-wide
/// `blobs.sqlite` + flat CAS dir.
pub struct SqliteCasMds {
    root: PathBuf,
    /// Per-set connection cache. `Arc<Mutex<Option<Connection>>>`:
    /// the `Option` lets `delete_set` drain the SQLite handle while
    /// still holding the per-set Mutex, so any other Arc holder
    /// who already cloned the cache entry but hasn't yet locked
    /// finds `None` instead of writing a phantom `blob_ref` for a
    /// torn-down set through the ATTACH'd `blobs_db`. The cache
    /// itself is an `RwLock` so reads (the common case) don't
    /// contend.
    sets: RwLock<HashMap<SetId, Arc<Mutex<Option<Connection>>>>>,
    /// Fair box-wide queue for every in-process `blobs.sqlite`
    /// writer. SQLite already permits only one such writer; making
    /// that serialization explicit prevents its non-FIFO busy
    /// handler from starving a retrying per-set connection.
    write_gate: FairMutex<()>,
    /// Box-wide blobs.sqlite. Opened eagerly in `open` so the
    /// schema is applied once at startup; used directly by
    /// `delete_set` for the cross-set blob_ref sweep.
    blobs: Arc<Mutex<Connection>>,
    /// In-process change-notification fanout. Channels are created
    /// lazily on first subscribe; events are published *after* the
    /// per-set transaction commits.
    notifier: Arc<Notifier>,
    /// Bus event sink. Default is [`EventSink::none`] (silent
    /// no-op); the `with_bus_events` builder swaps in a real
    /// broadcast channel under the `cosmix` feature. Same
    /// post-commit discipline as `notifier`.
    event_sink: EventSink,
    /// Pass-1 → Pass-2 quiescence wait for `gc()`. Set at
    /// construction from `DEFAULT_GC_QUIESCENCE` (or the
    /// `COSMIX_MDS_GC_QUIESCENCE_SECS` env override, validated
    /// `>= 5s`); the test path overrides via `with_gc_quiescence`.
    gc_quiescence: Duration,
    /// Write-lock wait applied to every connection this store owns,
    /// including sets opened later by `create_set`. Defaults to
    /// [`DEFAULT_BUSY_TIMEOUT`]; raised by
    /// [`open_with_busy_timeout`](Self::open_with_busy_timeout).
    busy_timeout: Duration,
}

impl SqliteCasMds {
    /// Open (or create) the mds root. Creates `containers/`,
    /// `blobs/`, `blobs/.tmp/`, applies the box-wide blobs.sqlite
    /// schema, and discovers any pre-existing sets.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_busy_timeout(root, DEFAULT_BUSY_TIMEOUT)
    }

    /// Like [`open`](Self::open), but with an explicit write-lock
    /// wait instead of [`DEFAULT_BUSY_TIMEOUT`].
    ///
    /// For callers that knowingly permit a single transaction or
    /// cross-process/cross-instance wait beyond the production
    /// ceiling. Not for normal daemon use: there, a long block is a
    /// signal, not something to absorb.
    pub fn open_with_busy_timeout(
        root: impl Into<PathBuf>,
        busy_timeout: Duration,
    ) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("containers"))?;
        std::fs::create_dir_all(root.join("blobs").join(".tmp"))?;

        let blobs_path = blobs_db_path(&root);
        let mut blobs_conn = Connection::open(&blobs_path)
            .map_err(|e| Error::Other(format!("open blobs.sqlite: {e}")))?;
        schema::apply_blobs_migrations(&mut blobs_conn)?;
        set_busy_timeout(&blobs_conn, busy_timeout)?;

        let mut sets: HashMap<SetId, Arc<Mutex<Option<Connection>>>> = HashMap::new();
        for set in discover_sets(&root)? {
            let conn = open_set_db(&root, &set, &blobs_path, busy_timeout)?;
            sets.insert(set, Arc::new(Mutex::new(Some(conn))));
        }

        Ok(Self {
            root,
            sets: RwLock::new(sets),
            write_gate: FairMutex::new(()),
            blobs: Arc::new(Mutex::new(blobs_conn)),
            notifier: Arc::new(Notifier::new()),
            event_sink: EventSink::none(),
            gc_quiescence: gc_quiescence_from_env(),
            busy_timeout,
        })
    }

    /// Write-lock wait every connection in this store is configured
    /// with. Public so a caller (or test) can assert the plumbing
    /// rather than infer it from timing.
    pub fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    /// Install an Bus event sink. Returns the broadcast receiver so
    /// callers can subscribe a publisher task (or, in tests, drain
    /// events directly). Capacity defaults to
    /// [`bus::DEFAULT_CAPACITY`]; bypass with [`with_event_sink`] if a
    /// pre-built sink is needed (e.g. shared with other emitters).
    ///
    /// [`with_event_sink`]: Self::with_event_sink
    pub fn with_bus_events(mut self) -> (Self, tokio::sync::broadcast::Receiver<MdsEvent>) {
        let (sink, rx) = EventSink::with_capacity(bus::DEFAULT_CAPACITY);
        self.event_sink = sink;
        (self, rx)
    }

    /// Install a pre-built [`EventSink`]. Used by tests that share a
    /// sink with an out-of-band assertion thread, or by call sites
    /// that have already wired their own broadcast channel.
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = sink;
        self
    }

    /// Read-only handle to the configured sink. Useful for tests
    /// that want to subscribe a second receiver after the store is
    /// constructed.
    pub fn event_sink(&self) -> &EventSink {
        &self.event_sink
    }

    /// Override the GC quiescence wait (Pass 1 → Pass 2). Intended
    /// for tests that need to drop the wait below the operator-facing
    /// 5-second floor; production wiring should use `open()` and the
    /// `COSMIX_MDS_GC_QUIESCENCE_SECS` env var. Documented as not for
    /// production use because it skips env-var validation.
    pub fn with_gc_quiescence(mut self, q: Duration) -> Self {
        self.gc_quiescence = q;
        self
    }

    /// Currently configured GC quiescence wait. Public mostly for
    /// tests asserting on the configuration plumbing.
    pub fn gc_quiescence(&self) -> Duration {
        self.gc_quiescence
    }

    fn set_conn(&self, set: &SetId) -> Result<Arc<Mutex<Option<Connection>>>> {
        if let Some(c) = self.sets.read().get(set) {
            return Ok(Arc::clone(c));
        }
        Err(Error::SetNotFound(set.0.to_string()))
    }

    /// Join the fair in-process writer queue, bounded by the same
    /// configured wait as SQLite's cross-process busy handler.
    fn lock_write_gate(&self) -> Result<FairMutexGuard<'_, ()>> {
        self.write_gate
            .try_lock_for(self.busy_timeout)
            .ok_or_else(|| {
                Error::Other(format!(
                    "timed out after {}ms waiting for in-process write gate",
                    self.busy_timeout.as_millis()
                ))
            })
    }

    /// Run a closure with mutable access to the per-set
    /// `Connection`. Returns `SetNotFound` if the set has no cache
    /// entry, or if a concurrent `delete_set` has already drained
    /// the inner `Option` (handled the same way in either case —
    /// the set is unusable from this caller's perspective).
    fn with_conn_mut<R, F>(&self, set: &SetId, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R>,
    {
        let arc = self.set_conn(set)?;
        let mut g = arc.lock();
        let c = g
            .as_mut()
            .ok_or_else(|| Error::SetNotFound(set.0.to_string()))?;
        // Global order: per-set connection, then fair write gate.
        // Hold both for the complete operation, including commit or
        // rollback. Never retry `f`: callers of `with_set_tx` may
        // perform non-database side effects inside an FnOnce closure.
        let _write_guard = self.lock_write_gate()?;
        f(c)
    }

    fn with_conn_ref<R, F>(&self, set: &SetId, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        let arc = self.set_conn(set)?;
        let g = arc.lock();
        let c = g
            .as_ref()
            .ok_or_else(|| Error::SetNotFound(set.0.to_string()))?;
        f(c)
    }

    fn blobs_root(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// Run `f` inside a single per-set `BEGIN IMMEDIATE` transaction
    /// against the `data.sqlite` connection (with `blobs.sqlite`
    /// already ATTACH'd as `blobs_db`). On `Ok`, the transaction is
    /// committed; on `Err` or panic, it rolls back via `Transaction`'s
    /// `Drop`.
    ///
    /// This is the load-bearing primitive for cross-layer atomicity:
    /// a single `with_set_tx` body can write to per-set tables (item,
    /// container, set_change, mail_threads, mail_search …) and to the
    /// box-wide `blobs_db.blob_ref` table in one indivisible commit,
    /// matching the v1.1 amendment's "every mutation is one tx" rule.
    ///
    /// **Escape-hatch discipline.** [`SqliteSetTx::tx`] /
    /// [`SqliteSetTx::tx_mut`] expose the raw `rusqlite::Transaction`
    /// so this primitive can ship before typed accessors exist.
    /// `8c`/`8d` will add typed methods (`item_memberships`,
    /// `store_membership_keywords`, …) as real callers arrive — those
    /// callers should prefer typed methods over the raw handle. New
    /// crate-internal mutations should still go through the existing
    /// `Mds` trait methods or the upcoming typed accessors; the raw
    /// handle exists for *cross-crate* sidecar writes
    /// (e.g. `cosmix-maild`'s `MailStore` adapter) that need to
    /// coordinate writes the `Mds` trait does not yet name.
    ///
    /// Lock discipline: acquires the per-set `Mutex` for the entire
    /// closure (so the IMMEDIATE tx never races a concurrent
    /// mutation through the same cache entry) and releases on
    /// commit / rollback.
    pub fn with_set_tx<R, F>(&self, set: &SetId, f: F) -> Result<R>
    where
        F: FnOnce(&mut SqliteSetTx<'_>) -> Result<R>,
    {
        // Capture references to the Notifier + EventSink up-front so
        // we can drain the per-tx event buffer *after* the commit
        // returns successfully — `with_conn_mut`'s closure cannot
        // borrow `&self` again from inside its body without grief, so
        // we lift the references through the closure boundary.
        let notifier = Arc::clone(&self.notifier);
        let event_sink = self.event_sink.clone();
        self.with_conn_mut(set, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| Error::Other(format!("begin immediate: {e}")))?;
            let mut handle = SqliteSetTx {
                set: *set,
                tx,
                events: container::BufferedEvents::new(),
            };
            // On panic in `f`, `handle` is dropped here, which drops
            // `tx` (rollback) and `events` (no replay). parking_lot's
            // Mutex (held by the enclosing `with_conn_mut`) does not
            // poison, so the per-set cache entry remains usable for
            // the next caller. On `Err` return, same outcome — the
            // events are silently discarded; subscribers cannot see
            // events from a transaction that did not commit.
            let r = f(&mut handle)?;
            // Partial-move out of `handle`: commit consumes `handle.tx`
            // (Transaction::commit takes self by value), then we drain
            // `handle.events` only after the commit succeeds. If
            // `commit()` itself fails, `handle.events` drops without
            // replay — same ghost-event prevention as a closure error.
            handle
                .tx
                .commit()
                .map_err(|e| Error::Other(format!("commit per-set tx: {e}")))?;
            handle.events.drain_into(&notifier, &event_sink);
            Ok(r)
        })
    }
}

/// In-transaction handle for a single per-set `data.sqlite` mutation
/// scope. Constructed only by [`SqliteCasMds::with_set_tx`].
///
/// Phase 8b shipped this as the *transaction-scope primitive* — the
/// thing that proves cross-layer atomicity is reachable from outside
/// the crate. Phase 8d.1 added typed methods (`ensure_container_by_name`,
/// `add_staging_item`, `move_item`, `remove_membership`) for the
/// load-bearing MDS operations cosmix-maild's `MailStore` needs;
/// Phase 1 of the JMAP→MDS migration added `store_item_flags` for
/// `MailStore::set_keywords`. Sidecar tables continue to use raw
/// SQL through [`Self::tx`].
///
/// **Re-entry rule.** Inside a `with_set_tx` closure, do **not** call
/// the public [`Mds`] mutation methods (`add_item`, `move_item`,
/// `store_membership_keywords`, etc.). They re-enter the same per-set
/// `Mutex` and open their own transaction — either deadlocks or splits
/// the atomic boundary the closure exists to enforce. Use the typed
/// `SqliteSetTx::*` methods below, or raw SQL through [`Self::tx`].
///
/// **Event buffering.** Typed methods append events (notifier +
/// Bus `MdsEvent`) to an internal buffer rather than calling
/// `Notifier::publish` / `EventSink::emit` directly. The buffer is
/// drained by `with_set_tx` *only after* the SQL commit succeeds; on
/// rollback or panic the buffer is dropped without replay, so
/// subscribers cannot observe events from a transaction that did not
/// land. Raw-SQL callers using [`Self::tx`] do not get this
/// protection — they must publish their own events outside the
/// closure (or refrain from publishing on uncommitted state).
///
/// The wrapped `rusqlite::Transaction<'a>` borrows the per-set
/// `Connection` for the duration of the closure passed to
/// `with_set_tx`. The handle cannot outlive that borrow.
pub struct SqliteSetTx<'a> {
    set: SetId,
    tx: rusqlite::Transaction<'a>,
    events: container::BufferedEvents,
}

impl<'a> SqliteSetTx<'a> {
    /// The `SetId` this transaction is scoped to. Useful for sidecar
    /// callers that need to thread the set id into their own SQL
    /// without re-deriving it from external context.
    pub fn set(&self) -> &SetId {
        &self.set
    }

    /// Borrow the underlying `rusqlite::Transaction`. Sufficient for
    /// `execute`, `query_row`, `prepare`, `prepare_cached`, and other
    /// methods that take `&self`.
    ///
    /// **Escape hatch — see the type-level "Re-entry rule" doc.**
    /// Sidecar code in other crates uses this to run sidecar table
    /// writes inside the same indivisible commit as the upstream MDS
    /// mutation.
    pub fn tx(&self) -> &rusqlite::Transaction<'a> {
        &self.tx
    }

    /// Mutable borrow for the rare `&mut self` operation
    /// (e.g. `transaction.savepoint()`). Prefer [`Self::tx`] for the
    /// common path; reach for this only when the rusqlite API
    /// demands `&mut Transaction`.
    pub fn tx_mut(&mut self) -> &mut rusqlite::Transaction<'a> {
        &mut self.tx
    }

    /// Idempotent `(parent, name)` → `ContainerId` resolver. Returns
    /// the existing container if one is present under the given
    /// parent with that name; otherwise inserts a new one and buffers
    /// a `ContainerCreated` Bus event for post-commit replay.
    ///
    /// Used by maild's `MailStore` to provision its per-account
    /// reserved [`crate::container::UPLOAD_STAGING_NAME`] container
    /// the first time an account stages an upload, without needing a
    /// separate "ensure provisioned" code path.
    pub fn ensure_container_by_name(
        &mut self,
        parent: Option<&ContainerId>,
        name: &str,
        attrs: &ContainerAttrs,
    ) -> Result<ContainerId> {
        container::ensure_container_by_name_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            parent,
            name,
            attrs,
        )
    }

    /// Strict create — non-idempotent counterpart of
    /// [`Self::ensure_container_by_name`]. Fails if a sibling with the
    /// same `(parent, name)` already exists. Used by maild's
    /// `MailStore::create_mailbox` so JMAP `Mailbox/set create` can
    /// surface a name collision rather than aliasing onto an existing
    /// container.
    ///
    /// See [`container::create_container_named_in_tx`] for the full
    /// error contract.
    pub fn create_container_named(
        &mut self,
        parent: Option<&ContainerId>,
        name: &str,
        attrs: &ContainerAttrs,
    ) -> Result<ContainerId> {
        container::create_container_named_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            parent,
            name,
            attrs,
        )
    }

    /// Rename / reparent a container inside this transaction. Used by
    /// maild's `MailStore::rename_mailbox`. Buffers the
    /// `ContainerRenamed` event for post-commit replay; on rollback
    /// the buffer is dropped without publishing. See
    /// [`container::rename_container_in_tx`] for the full error
    /// contract.
    pub fn rename_container(
        &mut self,
        id: &ContainerId,
        new_parent: Option<&ContainerId>,
        new_name: &str,
    ) -> Result<()> {
        container::rename_container_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            id,
            new_parent,
            new_name,
        )
    }

    /// Delete a container inside this transaction. Used by maild's
    /// `MailStore::destroy_mailbox`. Buffers the per-container
    /// channel-close and `ContainerDeleted` Bus event for post-commit
    /// replay; on rollback both are dropped without effect. See
    /// [`container::delete_container_in_tx`] for the full error
    /// contract — including the cascade rule (child containers must
    /// be deleted first; surviving memberships are dropped via
    /// `ON DELETE CASCADE` but item rows orphan, so callers wanting
    /// orphan cleanup must enumerate and remove memberships first).
    pub fn delete_container(&mut self, id: &ContainerId) -> Result<()> {
        container::delete_container_in_tx(&self.tx, &mut self.events, &self.set, id)
    }

    /// Patch a container's JMAP-visible attrs (`role`, `sortOrder`,
    /// `isSubscribed`) inside this tx and, if anything changed,
    /// allocate one `CONTAINER_ATTRS_CHANGED` row in
    /// `container_change_set`. Returns the JMAP-named subset of
    /// `["role", "sortOrder", "isSubscribed"]` that actually
    /// differed; an empty list means the patch was a no-op (no SQL
    /// UPDATE, no lifecycle row).
    ///
    /// This is the **single substrate write surface** for container
    /// attrs mutation. maild's `MailStore::update_mailbox` was
    /// previously a raw `UPDATE container SET attrs = ?` via the
    /// `tx()` escape hatch; MCS-P1-D cuts it over to this method so
    /// the `container_change_set` invariant ("every container
    /// mutation produces a row") holds end-to-end. See the
    /// trait-boundary discipline note in
    /// `_doc/planned/mailbox-changes-substrate.md`
    /// §"Trait surface change".
    ///
    /// See [`container::update_container_attrs_in_tx`] for the full
    /// error contract and the diff-scope rationale (why only the
    /// JMAP triple, why `extra` keys outside `"jmap_sort_order"` do
    /// not participate in the diff).
    pub fn update_container_attrs(
        &mut self,
        id: &ContainerId,
        new_attrs: &ContainerAttrs,
    ) -> Result<UpdatedContainerProps> {
        container::update_container_attrs_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            id,
            new_attrs,
        )
    }

    /// Stage one CAS-deduplicated item with a **single** membership in
    /// `container_id`. The blob bytes must already be on disk via
    /// [`Mds::put_blob`]; this writes only the per-set rows + the
    /// cross-DB blob_ref refcount inside the caller's tx.
    ///
    /// **Single-membership/staging only.** For brand-new items with
    /// N≥1 destinations use [`Self::add_item_with_memberships`]
    /// instead; do **not** combine `add_staging_item` with
    /// [`Self::add_membership`] to assemble a multi-mailbox create —
    /// that publishes a single-element Bus `ItemAdded` followed by N
    /// `ItemCopied` events, which is the wrong shape for a creation.
    ///
    /// Returns the freshly allocated `ItemId` and the initial
    /// membership's [`Placement`] (the seq + change_seq the membership
    /// row received). Buffers `ItemAdded` notifier + Bus events for
    /// post-commit replay.
    pub fn add_staging_item(
        &mut self,
        container_id: &ContainerId,
        blob_hash: &BlobHash,
        blob_size: u64,
        flags: Flags,
        tags: &[String],
    ) -> Result<(ItemId, Placement)> {
        container::add_staging_item_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            container_id,
            blob_hash,
            blob_size,
            flags,
            tags,
        )
    }

    /// Stage a CAS-deduplicated item with N≥1 memberships. The
    /// multi-membership counterpart of [`Self::add_staging_item`]:
    /// inserts the item row + cross-DB blob_ref + N membership rows in
    /// one shot, emits ONE aggregate Bus `ItemAdded` event with
    /// parallel `container_ids[]` / `change_seqs[]` arrays plus one
    /// per-dest notifier `ItemAdded` event.
    ///
    /// Used by maild's `MailStore::create_email_with_memberships` so a
    /// multi-mailbox JMAP `Email/set create` (or `Email/import`) lands
    /// in a single `with_set_tx` and produces the same downstream-Bus
    /// shape as the legacy public `add_item` path.
    ///
    /// `memberships` carries `(container_id, flags)` per dest. Per the
    /// v1.1 §3 "all memberships agree" invariant the caller is
    /// responsible for passing the same `flags` for every entry; the
    /// JMAP `keywords` object is fresh-item-wide so this is the
    /// expected shape.
    ///
    /// Errors:
    ///   * `Error::Other("add_item_with_memberships: at least one
    ///     membership required")` on empty `memberships`.
    ///   * `Error::Other("add_item_with_memberships: duplicate
    ///     container in memberships slice: <id>")` if the same
    ///     container appears twice in the slice.
    ///   * [`Error::ContainerNotFound`] on a missing dest.
    ///
    /// All three checks are prevalidated **before** any item /
    /// blob_ref / membership writes execute. Storage-layer errors
    /// after prevalidation (e.g. `add_blob_ref_in_tx` raising on a
    /// blobs-DB failure, or `insert_membership` returning a SQL error
    /// for a reason not covered by prevalidation) can still surface
    /// post-write — those rely on the caller letting the `Err`
    /// propagate so `with_set_tx` rolls back. The prevalidation
    /// guarantee covers exactly the three errors listed above.
    pub fn add_item_with_memberships(
        &mut self,
        blob_hash: &BlobHash,
        blob_size: u64,
        memberships: &[(ContainerId, Flags)],
        tags: &[String],
    ) -> Result<(ItemId, Vec<Placement>)> {
        container::add_item_with_memberships_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            blob_hash,
            blob_size,
            memberships,
            tags,
        )
    }

    /// Add a new membership for an *existing* item (post-create path),
    /// inheriting `(flags, tags)` from the item's current memberships
    /// per the v1.1 §3 "all memberships agree" invariant.
    /// Caller-supplied `flags` is the fallback only when no other
    /// membership currently exists.
    ///
    /// Used by maild's `MailStore::add_memberships` to apply Task 3.4
    /// "added mailbox" deltas in JMAP `Email/set update`. For
    /// brand-new items use [`Self::add_item_with_memberships`]
    /// instead — that path emits an aggregate `ItemAdded` Bus event
    /// (correct for creation), whereas this path emits `ItemCopied`
    /// (correct for "membership added to existing item").
    ///
    /// Returns the new membership's [`Placement`]. Buffers `ItemAdded`
    /// (notifier on dest) + `ItemCopied` (Bus) events for post-commit
    /// replay; on rollback both are dropped. See
    /// [`container::add_membership_in_tx`] for the full error contract:
    /// `ItemNotFound`, `ContainerNotFound`, and a `"item ... already in
    /// container ..."` `Error::Other` on duplicate.
    pub fn add_membership(
        &mut self,
        item_id: &ItemId,
        container_id: &ContainerId,
        flags: Flags,
    ) -> Result<Placement> {
        container::add_membership_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            item_id,
            container_id,
            flags,
        )
    }

    /// IMAP COPY shape: like [`Self::add_membership`] but inherits
    /// `(flags, tags)` from a specific `src` container instead of any
    /// existing membership. Required because the IMAP §3.2 per-mailbox
    /// flag model lets sibling memberships of the same item diverge,
    /// so RFC 9051 §6.4.7 "preserve flags" needs source-specific
    /// inheritance rather than `ORDER BY container_id LIMIT 1`.
    ///
    /// **Source membership is required**: if `(item_id, src)` has no
    /// row this returns an `"item ... not in source container ..."`
    /// error rather than falling back to an empty-flags membership.
    /// The IMAP dispatch always names the selected source mailbox; a
    /// concurrent EXPUNGE/MOVE that drops `src`'s membership between
    /// snapshot and COPY must fail the call, not silently mint a
    /// dest membership with the wrong flags. Same `ItemAdded` +
    /// `ItemCopied` event shape as [`Self::add_membership`].
    pub fn add_membership_from(
        &mut self,
        item_id: &ItemId,
        src: &ContainerId,
        dest: &ContainerId,
    ) -> Result<Placement> {
        container::add_membership_from_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            item_id,
            src,
            dest,
        )
    }

    /// Read `container.seq_validity` (the IMAP `UIDVALIDITY` analogue)
    /// for `container_id` inside this transaction.
    ///
    /// Sidecar callers (maild) need this to bind a stable
    /// `(uid, uid_validity)` pair to a freshly-inserted membership in
    /// the same tx that allocated the seq, without a follow-up read
    /// after `with_set_tx` returns. Errors with
    /// [`Error::ContainerNotFound`] if the container has been deleted.
    pub fn container_seq_validity(&self, container_id: &ContainerId) -> Result<u64> {
        container::container_seq_validity_in_tx(&self.tx, container_id)
    }

    /// Atomic move with v1.1 §3 keyword inheritance. Mirrors
    /// [`Mds::move_item`]'s invariants and event shape (dual
    /// `ItemMoved` notifier publish on src+dest, Bus
    /// `mds.item.moved`) but the SQL runs in the caller's tx and
    /// events are deferred to the post-commit drain.
    ///
    /// Used by maild's `MailStore::move_message` so the move + the
    /// junk-boundary retrain-outbox row land atomically.
    pub fn move_item(
        &mut self,
        item_id: &ItemId,
        src: &ContainerId,
        dest: &ContainerId,
        flags: Flags,
    ) -> Result<MoveReport> {
        container::move_item_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            item_id,
            src,
            dest,
            flags,
        )
    }

    /// Remove the (item, container) membership inside the caller's
    /// tx. If the item now has zero memberships, drop its cross-DB
    /// `blob_ref` (refcount -= 1) and delete the `item` row so the
    /// orphan blob becomes a GC candidate. Mirrors
    /// [`Mds::remove_membership`]'s cascade exactly; events go to the
    /// buffer.
    ///
    /// Used by maild's upload-staging expiry worker to drop staged
    /// items whose TTL has elapsed without finalisation.
    pub fn remove_membership(
        &mut self,
        item_id: &ItemId,
        container_id: &ContainerId,
    ) -> Result<()> {
        container::remove_membership_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            item_id,
            container_id,
        )
    }

    /// JMAP `Email/set keywords` flags-only fan-out: apply `flags` to
    /// every current membership of `item_id` in this tx, preserving
    /// each row's `tags`. Returns `(container_id, new_change_seq)` per
    /// touched membership in `ORDER BY container_id`; an item with no
    /// memberships returns `Ok(vec![])` and writes nothing. Buffers
    /// `FlagsChanged` notifier + `mds.item.flagged` Bus events.
    ///
    /// Used by maild's `MailStore::set_keywords` so the per-mailbox
    /// flag updates and any sidecar writes (Phase A: none; Phase 3:
    /// `mail.keywords_changed`) land atomically.
    pub fn store_item_flags(
        &mut self,
        item_id: &ItemId,
        flags: Flags,
    ) -> Result<Vec<(ContainerId, ChangeToken)>> {
        container::store_item_flags_in_tx(&self.tx, &mut self.events, &self.set, item_id, flags)
    }

    /// IMAP STORE per-mailbox flags-only write, in-tx variant.
    /// Mutates exactly the `(item_id, container_id)` membership row;
    /// sibling memberships of the same item in other containers are
    /// untouched. Returns the new per-membership `change_seq`.
    ///
    /// Used by maild's `MailStore::set_keywords_in_mailbox` (the
    /// IMAP-shaped sibling of `MailStore::set_keywords`) so IMAP
    /// STORE writes and any sidecar updates land in one tx. The peer
    /// `store_item_flags` is the JMAP fan-out path; this primitive is
    /// its inverse — it MUST NOT walk siblings or RFC 8621 §4.4.6's
    /// JMAP-keywords-are-union contract breaks. See
    /// `_doc/planned/imap-runway.md` §P1 for the architectural
    /// rationale.
    ///
    /// `Error::Other("membership (item, container) not present")` if
    /// the row does not exist (IMAP STORE against a UID whose
    /// membership has been EXPUNGE'd between client-side resolve and
    /// this call).
    pub fn store_flags(
        &mut self,
        item_id: &ItemId,
        container_id: &ContainerId,
        flags: Flags,
    ) -> Result<ChangeToken> {
        container::store_flags_in_tx(
            &self.tx,
            &mut self.events,
            &self.set,
            item_id,
            container_id,
            flags,
        )
    }

    /// Read the current memberships of `item_id` inside the open tx.
    /// Mirrors [`Mds::item_memberships`] but stays inside the
    /// already-acquired per-set lock — calling the public
    /// `Mds::item_memberships` from within a `with_set_tx` closure
    /// would deadlock by re-entering the same per-set `Mutex`.
    /// Returns `(container_id, flags, tags)` rows ordered by
    /// `container_id`; an unknown or fully-unmoored item returns an
    /// empty `Vec`.
    pub fn item_memberships(&self, item_id: &ItemId) -> Result<Vec<(ContainerId, Flags, Tags)>> {
        // `Transaction` derefs to `Connection`, which is what the
        // free-function signature wants.
        container::item_memberships(&self.tx, item_id)
    }

    /// Read up to `limit` rows from the set-wide `set_change` stream
    /// inside the open tx. Mirrors [`Mds::changes_since_set`] but
    /// stays inside the already-acquired per-set lock — calling the
    /// public `Mds::changes_since_set` from within a `with_set_tx`
    /// closure would deadlock by re-entering the same per-set
    /// `Mutex`. Returns `(rows, next_cursor)` where `next_cursor` is
    /// `Some(token)` if the stream extends past the returned page,
    /// `None` when the caller has reached the tip.
    ///
    /// Used by maild's `MailStore::email_changes` so the change-stream
    /// read and the per-item `item_memberships` "exists at new_state"
    /// classification queries land in the same point-in-time view.
    /// Without this, a delete committed between the two reads would
    /// leak as a destroyed event past `new_state`, violating the
    /// JMAP `Email/changes` snapshot-at-`new_state` property.
    pub fn changes_since_set(
        &self,
        since: SetChangeToken,
        limit: usize,
    ) -> Result<(Vec<SetChange>, Option<SetChangeToken>)> {
        container::changes_since_set(&self.tx, since, limit)
    }
}

/// Read `COSMIX_MDS_GC_QUIESCENCE_SECS`, validate `>= 5s`, fall back
/// to `DEFAULT_GC_QUIESCENCE` on absent / unparseable / out-of-range
/// values. Out-of-range or unparseable values warn so the operator
/// notices the misconfiguration; absence is silent (default applies).
fn gc_quiescence_from_env() -> Duration {
    let raw = match std::env::var(ENV_GC_QUIESCENCE) {
        Ok(v) => v,
        Err(_) => return DEFAULT_GC_QUIESCENCE,
    };
    let trimmed = raw.trim();
    let secs: u64 = match trimmed.parse() {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(
                target: "cosmix_mds",
                "{ENV_GC_QUIESCENCE}={raw:?} unparseable as integer seconds; \
                 falling back to {default}s default",
                default = DEFAULT_GC_QUIESCENCE.as_secs(),
            );
            return DEFAULT_GC_QUIESCENCE;
        }
    };
    let candidate = Duration::from_secs(secs);
    if candidate < MIN_ENV_GC_QUIESCENCE {
        tracing::warn!(
            target: "cosmix_mds",
            "{ENV_GC_QUIESCENCE}={secs}s is below the {min}s floor; falling back \
             to {default}s default",
            min = MIN_ENV_GC_QUIESCENCE.as_secs(),
            default = DEFAULT_GC_QUIESCENCE.as_secs(),
        );
        return DEFAULT_GC_QUIESCENCE;
    }
    candidate
}

fn set_dir(root: &Path, set: &SetId) -> PathBuf {
    root.join("containers").join(set.0.to_string())
}

fn set_db_path(root: &Path, set: &SetId) -> PathBuf {
    set_dir(root, set).join("data.sqlite")
}

fn blobs_db_path(root: &Path) -> PathBuf {
    root.join("blobs.sqlite")
}

fn discover_sets(root: &Path) -> Result<Vec<SetId>> {
    let mut out = Vec::new();
    let containers = root.join("containers");
    if !containers.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&containers)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let s = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        match uuid::Uuid::parse_str(s) {
            Ok(u) => {
                if entry.path().join("data.sqlite").exists() {
                    out.push(SetId(u));
                }
            }
            Err(_) => {
                tracing::warn!(target: "cosmix_mds", "skipping non-UUID dir under containers/: {s}");
            }
        }
    }
    Ok(out)
}

fn open_set_db(
    root: &Path,
    set: &SetId,
    blobs_path: &Path,
    busy_timeout: Duration,
) -> Result<Connection> {
    let path = set_db_path(root, set);
    let mut conn = Connection::open(&path)
        .map_err(|e| Error::Other(format!("open {}: {e}", path.display())))?;
    schema::apply_data_migrations(&mut conn)?;
    schema::seed_set_state(&conn, set)?;
    attach_blobs_db(&conn, blobs_path)?;
    // After ATTACH, not before: `busy_timeout` is a connection-wide
    // setting, so this one call covers waits on the ATTACHed
    // `blobs.sqlite` as well as on this set's own `data.sqlite`.
    set_busy_timeout(&conn, busy_timeout)?;
    Ok(conn)
}

/// Override the connection's write-lock wait, replacing the default
/// `schema::set_pragmas` installed at migration time.
///
/// `PRAGMA busy_timeout` is per-connection and settable at any point
/// in its life, which is what lets the store configure it here rather
/// than threading it through every migration entry point.
pub(crate) fn set_busy_timeout(conn: &Connection, busy_timeout: Duration) -> Result<()> {
    let ms = i64::try_from(busy_timeout.as_millis()).unwrap_or(i64::MAX);
    conn.pragma_update(None, "busy_timeout", ms)
        .map_err(|e| Error::Other(format!("set busy_timeout={ms}ms: {e}")))?;
    Ok(())
}

/// ATTACH the box-wide `blobs.sqlite` to a per-set connection as
/// `blobs_db` so delivery transactions can co-write blob refcount /
/// blob_ref rows in the same atomic unit. The path is one we
/// constructed ourselves from `root`, not user input — but ATTACH
/// does not allow parameter binding for the path, so the literal is
/// embedded directly with single-quote escaping for safety.
fn attach_blobs_db(conn: &Connection, blobs_path: &Path) -> Result<()> {
    let s = blobs_path.to_str().ok_or_else(|| {
        Error::Other(format!(
            "blobs.sqlite path {} is not valid UTF-8",
            blobs_path.display()
        ))
    })?;
    let escaped = s.replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS blobs_db;"))
        .map_err(|e| Error::Other(format!("ATTACH blobs.sqlite as blobs_db: {e}")))?;
    Ok(())
}

impl Mds for SqliteCasMds {
    fn create_set(&self, set: &SetId) -> Result<()> {
        // Lifecycle write: the cache write lock is held across the
        // existence check, mkdir, db open, and insert. This serializes
        // create_set against itself for the same `set`, and against
        // `delete_set`'s final cache removal — `delete_set` keeps the
        // (drained) cache entry alive until *after* rmdir, so a
        // racing same-UUID create_set will see the entry under this
        // lock and reject with "already exists" rather than mkdir
        // into a directory that delete_set is about to recursively
        // remove.
        let mut cache = self.sets.write();
        if cache.contains_key(set) {
            return Err(Error::SetAlreadyExists { set: *set });
        }
        let dir = set_dir(&self.root, set);
        if dir.exists() {
            // Cache and disk disagree — discover_sets at open() time
            // should have populated the cache for any UUID dir. This
            // is the safety net for the edge case where someone
            // manually mkdir'd a UUID-named directory under
            // containers/ between open() and now.
            return Err(Error::SetAlreadyExists { set: *set });
        }
        std::fs::create_dir_all(&dir)?;
        let conn = open_set_db(
            &self.root,
            set,
            &blobs_db_path(&self.root),
            self.busy_timeout,
        )?;
        cache.insert(*set, Arc::new(Mutex::new(Some(conn))));
        // Bus event: emitted post-commit (cache write is the commit
        // for set lifecycle since data.sqlite open is idempotent).
        self.event_sink
            .emit(MdsEvent::SetCreated(SetCreated { set_id: *set }));
        Ok(())
    }

    fn delete_set(&self, set: &SetId) -> Result<DeleteReport> {
        let started = std::time::Instant::now();

        // Snapshot the Arc out of the cache without removing the
        // entry. The drained cache entry stays in place as a
        // tombstone until rmdir completes; `create_set` checks the
        // cache under its write lock and will reject a same-UUID
        // re-create that races with this delete instead of mkdir'ing
        // into a directory we are about to `remove_dir_all` below.
        let arc_opt = self.sets.read().get(set).cloned();

        if let Some(arc) = arc_opt {
            let mut g = arc.lock();
            // Take the Connection out and drop it now (closes the
            // SQLite handle and any WAL/SHM mappings). The Option
            // is left as None so any other Arc holder who finally
            // acquires this Mutex sees None and returns
            // SetNotFound from `with_conn_mut`/`with_conn_ref`.
            let _closed = g.take();

            // Apply blobs.sqlite tally + blob_ref sweep, then rmdir
            // — all while the cache entry is still present (acting
            // as the same-UUID tombstone) and `g` is still locked
            // (so no in-flight delivery resumes).
            let blob_count_unreffed = {
                let _write_guard = self.lock_write_gate()?;
                let mut bg = self.blobs.lock();
                crate::blob_index::delete_set_blobs(&mut bg, set)?
            };

            let dir = set_dir(&self.root, set);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }

            // Now drop the cache entry. Held under the cache write
            // lock so any concurrent `create_set` either sees the
            // entry (rejects) or sees it gone (succeeds with a
            // fresh mkdir). Per-set Mutex is dropped after, so
            // any other Arc holder that was queued sees `None` on
            // their final lock and returns SetNotFound.
            self.sets.write().remove(set);

            drop(g);
            let duration_ms = started.elapsed().as_millis() as u64;
            // Bus event: emitted after the cache entry is gone so a
            // subscriber cannot observe SetDeleted while the set is
            // still resolvable.
            self.event_sink.emit(MdsEvent::SetDeleted(SetDeleted {
                set_id: *set,
                blob_count_unreffed,
                duration_ms,
            }));
            return Ok(DeleteReport {
                blob_count_unreffed,
                duration_ms,
            });
        }

        // No cached entry. Either the set was never opened, or a
        // prior delete_set partially completed (rare; e.g. crash
        // between blobs sweep and rmdir). Tolerate the latter: if
        // the dir is still on disk we still try to clean it up so
        // a re-attempt makes progress.
        if !set_dir(&self.root, set).exists() {
            return Err(Error::SetNotFound(set.0.to_string()));
        }
        let blob_count_unreffed = {
            let _write_guard = self.lock_write_gate()?;
            let mut bg = self.blobs.lock();
            crate::blob_index::delete_set_blobs(&mut bg, set)?
        };
        let dir = set_dir(&self.root, set);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        let duration_ms = started.elapsed().as_millis() as u64;
        self.event_sink.emit(MdsEvent::SetDeleted(SetDeleted {
            set_id: *set,
            blob_count_unreffed,
            duration_ms,
        }));
        Ok(DeleteReport {
            blob_count_unreffed,
            duration_ms,
        })
    }

    fn list_sets(&self) -> Result<Vec<SetId>> {
        Ok(self.sets.read().keys().copied().collect())
    }

    // The legacy public `Mds` container CRUD methods route through
    // `with_set_tx` so they pick up the typed SetTx instrumentation
    // (notifier channel drop, Bus event buffering, and the
    // `container_change_set` lifecycle row from MCS-P1-C). Direct-
    // connection variants used to live in `container.rs` but became
    // a permanent hole the moment Phase 1 wired the lifecycle stream:
    // any non-maild caller of the trait would silently bypass the
    // stream that JMAP `Mailbox/changes` reads in Phase 2. The
    // typed SetTx path is the single write surface.
    fn create_container(
        &self,
        set: &SetId,
        parent: Option<&ContainerId>,
        name: &str,
        attrs: ContainerAttrs,
    ) -> Result<ContainerId> {
        self.with_set_tx(set, |h| h.create_container_named(parent, name, &attrs))
    }

    fn rename_container(
        &self,
        set: &SetId,
        id: &ContainerId,
        new_parent: Option<&ContainerId>,
        new_name: &str,
    ) -> Result<()> {
        self.with_set_tx(set, |h| h.rename_container(id, new_parent, new_name))
    }

    fn delete_container(&self, set: &SetId, id: &ContainerId) -> Result<()> {
        self.with_set_tx(set, |h| h.delete_container(id))
    }

    fn list_containers(&self, set: &SetId) -> Result<Vec<ContainerInfo>> {
        self.with_conn_ref(set, container::list_containers)
    }

    fn container_status(&self, set: &SetId, id: &ContainerId) -> Result<ContainerStatus> {
        self.with_conn_ref(set, |c| container::container_status(c, id))
    }

    // ---- Blob CAS (Phase 1b) ----
    fn put_blob(&self, bytes: &[u8]) -> Result<BlobHash> {
        blob::put(&self.blobs_root(), bytes)
    }
    fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>> {
        blob::get(&self.blobs_root(), hash)
    }
    fn blob_size(&self, hash: &BlobHash) -> Result<u64> {
        blob::size(&self.blobs_root(), hash)
    }
    fn blob_exists(&self, hash: &BlobHash) -> Result<bool> {
        blob::exists(&self.blobs_root(), hash)
    }

    // ---- Per-set delivery transactions (Phase 1b) ----
    fn add_item(
        &self,
        set: &SetId,
        blob_hash: &BlobHash,
        memberships: &[Membership],
    ) -> Result<AddReport> {
        let blobs_root = self.blobs_root();
        self.with_conn_mut(set, |c| {
            container::add_item(
                c,
                &blobs_root,
                &self.notifier,
                &self.event_sink,
                set,
                blob_hash,
                memberships,
            )
        })
    }
    fn copy_item(
        &self,
        set: &SetId,
        id: &ItemId,
        dest: &ContainerId,
        flags: Flags,
    ) -> Result<CopyReport> {
        self.with_conn_mut(set, |c| {
            container::copy_item(c, &self.notifier, &self.event_sink, set, id, dest, flags)
        })
    }
    fn move_item(
        &self,
        set: &SetId,
        id: &ItemId,
        src: &ContainerId,
        dest: &ContainerId,
        flags: Flags,
    ) -> Result<MoveReport> {
        self.with_conn_mut(set, |c| {
            container::move_item(
                c,
                &self.notifier,
                &self.event_sink,
                set,
                id,
                container::MovePath { src, dest },
                flags,
            )
        })
    }
    fn remove_membership(
        &self,
        set: &SetId,
        id: &ItemId,
        container_id: &ContainerId,
    ) -> Result<()> {
        self.with_conn_mut(set, |c| {
            container::remove_membership(c, &self.notifier, &self.event_sink, set, id, container_id)
        })
    }
    fn store_flags(
        &self,
        set: &SetId,
        id: &ItemId,
        container_id: &ContainerId,
        flags: Flags,
    ) -> Result<ChangeToken> {
        self.with_conn_mut(set, |c| {
            container::store_flags(
                c,
                &self.notifier,
                &self.event_sink,
                set,
                id,
                container_id,
                flags,
            )
        })
    }

    fn store_membership_keywords(
        &self,
        set: &SetId,
        id: &ItemId,
        container_id: &ContainerId,
        flags: Flags,
        tags: Tags,
    ) -> Result<ChangeToken> {
        self.with_conn_mut(set, |c| {
            container::store_membership_keywords(
                c,
                &self.notifier,
                &self.event_sink,
                set,
                id,
                container_id,
                container::Keywords { flags, tags: &tags },
            )
        })
    }
    fn store_item_keywords(
        &self,
        set: &SetId,
        id: &ItemId,
        flags: Flags,
        tags: Tags,
    ) -> Result<Vec<(ContainerId, ChangeToken)>> {
        self.with_conn_mut(set, |c| {
            container::store_item_keywords(
                c,
                &self.notifier,
                &self.event_sink,
                set,
                id,
                flags,
                &tags,
            )
        })
    }
    fn item_memberships(
        &self,
        set: &SetId,
        id: &ItemId,
    ) -> Result<Vec<(ContainerId, Flags, Tags)>> {
        self.with_conn_ref(set, |c| container::item_memberships(c, id))
    }

    fn fetch_item(
        &self,
        set: &SetId,
        id: &ItemId,
        container_id: &ContainerId,
    ) -> Result<ItemRecord> {
        self.with_conn_ref(set, |c| container::fetch_item(c, id, container_id))
    }
    fn fetch_item_meta(&self, set: &SetId, id: &ItemId) -> Result<ItemMeta> {
        self.with_conn_ref(set, |c| container::fetch_item_meta(c, id))
    }
    fn find_items_by_blob_hash(&self, set: &SetId, blob_hash: &BlobHash) -> Result<Vec<ItemId>> {
        self.with_conn_ref(set, |c| container::find_items_by_blob_hash(c, blob_hash))
    }
    fn search_items(&self, set: &SetId, needle: &str) -> Result<Vec<ItemId>> {
        self.with_conn_ref(set, |c| container::search_items(c, needle))
    }
    fn list_items(
        &self,
        set: &SetId,
        container_id: &ContainerId,
        range: SeqRange,
    ) -> Result<Vec<ItemRecord>> {
        self.with_conn_ref(set, |c| container::list_items(c, container_id, range))
    }
    fn changes_since(
        &self,
        set: &SetId,
        container_id: &ContainerId,
        since: ChangeToken,
    ) -> Result<Vec<Change>> {
        self.with_conn_ref(set, |c| container::changes_since(c, container_id, since))
    }
    fn changes_since_set(
        &self,
        set: &SetId,
        since: SetChangeToken,
        limit: usize,
    ) -> Result<(Vec<SetChange>, Option<SetChangeToken>)> {
        self.with_conn_ref(set, |c| container::changes_since_set(c, since, limit))
    }

    fn subscribe(
        &self,
        set: &SetId,
        container: &ContainerId,
    ) -> tokio::sync::broadcast::Receiver<ContainerEvent> {
        self.notifier.subscribe(set, container)
    }

    fn subscribe_existing(
        &self,
        set: &SetId,
        container: &ContainerId,
    ) -> Result<tokio::sync::broadcast::Receiver<ContainerEvent>> {
        // Hold the per-set Mutex across the existence check and the
        // notifier subscribe. Every path that drops a container's
        // broadcast channel (`Mds::delete_container`, the `with_set_tx`
        // commit drain) first deletes the `container` row under this
        // same per-set Mutex, then calls `notifier.drop_channel`.
        // So any `subscribe_existing` whose `container_exists` check
        // observes a row is guaranteed to subscribe before *that*
        // row's `drop_channel` runs (it will see a future delete's
        // channel close through its receiver, which is correct IDLE
        // behavior); and any `subscribe_existing` that races a delete
        // past the row's removal sees `false` here and bails with
        // `ContainerNotFound`. No lock-order inversion: this path is
        // set→notifier, and the notifier's own Mutex is only ever
        // acquired with no set Mutex held (subscribe / publish /
        // drop_channel).
        self.with_conn_ref(set, |c| {
            if !container::container_exists(c, container)? {
                return Err(Error::ContainerNotFound(container.0.to_string()));
            }
            Ok(self.notifier.subscribe(set, container))
        })
    }

    fn rebuild_index(&self) -> Result<RebuildReport> {
        // Lock discipline (rebuild_index is the only operation in
        // the crate that holds the *superset* set+blobs):
        //
        //   1. Snapshot the cache, sort by SetId UUID.
        //   2. Acquire every per-set Mutex in that order.
        //   3. Acquire `write_gate`.
        //   4. Then acquire `self.blobs`.
        //
        // Soundness depends on two crate-wide invariants:
        //
        //   a) No path takes `write_gate` or `self.blobs` and then a
        //      per-set Mutex. `delete_set` and ordinary mutations take
        //      set→gate; `gc` and `verify_blobs` take gate→blobs but
        //      acquire no set lock while either is held.
        //
        //   b) Every concurrent caller holds at most one set lock
        //      at a time. With sorted-UUID acquisition here, that
        //      gives a global ordering that no other path can
        //      contradict — they each can only ever wait for a
        //      single set we already hold or will acquire later.
        //
        // Holding the union of locks freezes delivery for the
        // duration of the rebuild, which is required because
        // delivery transactions write `blob`/`blob_ref` rows
        // through the per-set ATTACH'd `blobs_db`, and this
        // function wipes and reconstructs those tables.
        let started = std::time::Instant::now();
        let blobs_root = self.blobs_root();

        let mut entries: Vec<(SetId, Arc<Mutex<Option<Connection>>>)> = self
            .sets
            .read()
            .iter()
            .map(|(s, c)| (*s, Arc::clone(c)))
            .collect();
        entries.sort_by_key(|(s, _)| s.0);

        // `lock_arc` keeps the Arc alive inside the guard, so the
        // guards remain valid across the rebuild even though we
        // also hold them via `entries`. Sequential acquisition in
        // sorted order avoids per-set ordering issues.
        let guards: Vec<parking_lot::ArcMutexGuard<parking_lot::RawMutex, Option<Connection>>> =
            entries.iter().map(|(_, arc)| arc.lock_arc()).collect();

        // Pair set id with borrowed Connection. Sets whose option
        // was drained by a `delete_set` that completed between the
        // cache snapshot and our lock acquisition are skipped —
        // there's nothing to scan and the cache entry will be
        // pruned on the next `delete_set` write.
        let pairs: Vec<(SetId, &Connection)> = entries
            .iter()
            .zip(guards.iter())
            .filter_map(|((set, _), g)| g.as_ref().map(|c| (*set, c)))
            .collect();

        let _write_guard = self.lock_write_gate()?;
        let mut bg = self.blobs.lock();
        crate::rebuild::rebuild_blobs_index(&mut bg, &blobs_root, &pairs, started)
    }
    fn verify_blobs(&self, scope: VerifyScope) -> Result<VerifyReport> {
        // Lock-order discipline: per-set Mutex first, then
        // self.blobs (matches `delete_set`). Resolve the candidate
        // hash list *before* taking blobs.lock() so Container scope
        // — which acquires per-set Mutexes via `with_conn_ref` —
        // never runs underneath the blobs lock. The earlier
        // implementation passed a closure into `verify::run` after
        // taking `self.blobs`, which inverted the order and
        // deadlocked against a concurrent `delete_set`.
        let blobs_root = self.blobs_root();

        let hashes: Vec<String> = match &scope {
            VerifyScope::Full | VerifyScope::Since(_) => {
                let bg = self.blobs.lock();
                crate::verify::list_for_scope(&bg, &scope)?
            }
            VerifyScope::Container(cid) => {
                let set_ids: Vec<SetId> = self.sets.read().keys().cloned().collect();
                let mut acc: Vec<String> = Vec::new();
                for set in &set_ids {
                    let r =
                        self.with_conn_ref(set, |c| crate::verify::container_hashes_in_set(c, cid));
                    match r {
                        Ok(mut v) => acc.append(&mut v),
                        // Set drained mid-iteration (concurrent
                        // delete_set): treat as empty.
                        Err(Error::SetNotFound(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
                acc.sort();
                acc.dedup();
                acc
            }
        };

        let scope_tag = match &scope {
            VerifyScope::Full => "full",
            VerifyScope::Since(_) => "since",
            VerifyScope::Container(_) => "container",
        };
        let report = crate::verify::run(
            &blobs_root,
            &hashes,
            |hash, verified_at, status| {
                // Verification spends most of its time reading and
                // hashing CAS files. Queue each short ledger upsert
                // separately so ordinary writers get fair turns
                // between blobs instead of waiting for a full sweep.
                let _write_guard = self.lock_write_gate()?;
                let bg = self.blobs.lock();
                crate::verify::upsert_verify(&bg, hash, verified_at, status)
            },
            |mismatch| {
                // Per-blob mismatch event. `verify::run` invokes this
                // *after* the corresponding `blob_verify` ledger
                // upsert completes, so the event fires post-commit
                // per spec. Spec payload is `{hash, status}` only —
                // no set_id because the same corrupt blob may be
                // referenced by zero, one, or several sets.
                self.event_sink.emit(MdsEvent::VerifyFailed(VerifyFailed {
                    hash: mismatch.hash.clone(),
                    status: mismatch.status,
                }));
            },
        )?;
        // `mds.verify.completed` semantics: the sweep finished as an
        // operation. Mismatches > 0 still counts as completed — the
        // counter is reported in the payload so subscribers can
        // notice. A real failure (Err return) skips this emission.
        self.event_sink
            .emit(MdsEvent::VerifyCompleted(VerifyCompleted {
                scope: scope_tag.to_string(),
                blobs_checked: report.blobs_checked,
                mismatches: report.mismatches,
                duration_ms: report.duration.as_millis() as u64,
            }));
        Ok(report)
    }
    fn gc(&self, dry_run: bool) -> Result<GcReport> {
        // Two-pass GC orchestration. The blobs.sqlite lock is held
        // *only* across each individual database operation — never
        // across the quiescence wait — so an ongoing `delete_set` is
        // not blocked by a 60-second sleep. See plan rev 4 § "Two-
        // pass GC contract" for the full contract.
        let blobs_root = self.blobs_root();
        let started = std::time::Instant::now();
        let mut report = GcReport::default();

        let pass1 = {
            let bg = self.blobs.lock();
            crate::blob_index::gc_pass1(&bg)?
        };
        report.candidates_pass1 = pass1.candidates.len() as u64;
        report.pending_rows_observed = pass1.pending_rows_observed;

        if pass1.candidates.is_empty() {
            report.duration = started.elapsed();
            self.event_sink.emit(MdsEvent::GcCompleted(GcCompleted {
                blobs_deleted: report.blobs_deleted,
                bytes_freed: report.bytes_freed,
                duration_ms: report.duration.as_millis() as u64,
            }));
            return Ok(report);
        }

        if !self.gc_quiescence.is_zero() {
            std::thread::sleep(self.gc_quiescence);
        }

        {
            let _write_guard = self.lock_write_gate()?;
            let mut bg = self.blobs.lock();
            crate::blob_index::gc_pass2_apply(
                &mut bg,
                &blobs_root,
                &pass1.candidates,
                dry_run,
                &mut report,
            )?;
        }

        report.duration = started.elapsed();
        // Bus `mds.gc.completed`: emitted whether dry-run or live —
        // a "completed" event on a dry-run reports zero deletions
        // but the operator still cares that the sweep ran. The spec
        // does not gate on dry-run, so neither does emission.
        self.event_sink.emit(MdsEvent::GcCompleted(GcCompleted {
            blobs_deleted: report.blobs_deleted,
            bytes_freed: report.bytes_freed,
            duration_ms: report.duration.as_millis() as u64,
        }));
        Ok(report)
    }
    fn prune_changelog(
        &self,
        set: &SetId,
        stream: ChangelogStream,
        keep_n: u64,
    ) -> Result<PruneReport> {
        // SQLite parameter binding caps at i64. Saturating clamp is
        // correct semantics: keep_n > i64::MAX is indistinguishable
        // from "keep everything currently in the table" for any
        // realistic stream.
        let keep_clamped: i64 = keep_n.min(i64::MAX as u64) as i64;
        let (table, seq_col, floor_col) = stream_columns(stream);

        // Compute floor + delete + bump set_state in a single
        // IMMEDIATE transaction. Holding the per-set Mutex for the
        // duration matches the discipline of `with_set_tx` / `gc` and
        // avoids racing a concurrent allocator that bumps
        // `set_change_seq` between the OFFSET query and the DELETE.
        let report = self.with_conn_mut(set, |conn| -> Result<PruneReport> {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| Error::Other(format!("begin prune tx: {e}")))?;

            // Defensive: a pre-v1.5 caller path could hit this if the
            // singleton row was somehow never seeded. `seed_set_state`
            // is idempotent and matches the schema's NOT NULL defaults.
            tx.execute(
                "INSERT OR IGNORE INTO set_state (set_id, set_change_seq) VALUES (?1, 0);",
                [set.0.to_string()],
            )
            .map_err(|e| Error::Other(format!("ensure set_state row: {e}")))?;

            let current_floor: i64 = tx
                .query_row(
                    &format!("SELECT {floor_col} FROM set_state WHERE set_id = ?1;"),
                    [set.0.to_string()],
                    |r| r.get(0),
                )
                .map_err(|e| Error::Other(format!("read {floor_col}: {e}")))?;

            // The (keep_n+1)th newest seq is the highest seq we want
            // to delete; rows with seq <= that go, rows above stay.
            // OFFSET 0 with keep_n = 0 returns the table's top row,
            // which is exactly the "prune everything" cutoff.
            // `.optional()` converts `QueryReturnedNoRows` to `None`
            // (the legitimate "keep_n >= row_count" no-op signal) while
            // propagating any other SQLite error — a bare `.ok()` would
            // silently turn schema corruption or I/O errors into a
            // successful no-op + spurious Bus event.
            let cutoff_opt: Option<i64> = tx
                .query_row(
                    &format!(
                        "SELECT {seq_col} FROM {table} \
                         ORDER BY {seq_col} DESC LIMIT 1 OFFSET ?1;"
                    ),
                    [keep_clamped],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| Error::Other(format!("read cutoff from {table}: {e}")))?;

            let (rows_removed, new_floor) = if let Some(cutoff) = cutoff_opt {
                let n = tx
                    .execute(
                        &format!("DELETE FROM {table} WHERE {seq_col} <= ?1;"),
                        [cutoff],
                    )
                    .map_err(|e| Error::Other(format!("DELETE from {table}: {e}")))?;
                if cutoff > current_floor {
                    tx.execute(
                        &format!("UPDATE set_state SET {floor_col} = ?1 WHERE set_id = ?2;"),
                        rusqlite::params![cutoff, set.0.to_string()],
                    )
                    .map_err(|e| Error::Other(format!("bump {floor_col}: {e}")))?;
                }
                (n as u64, cutoff.max(current_floor))
            } else {
                // Table shorter than keep_n; nothing to prune. Floor
                // is unchanged and rows_removed is zero — the no-op
                // shape contracted in the trait doc-comment.
                (0u64, current_floor)
            };

            tx.commit()
                .map_err(|e| Error::Other(format!("commit prune tx: {e}")))?;
            Ok(PruneReport {
                rows_removed,
                new_floor: new_floor as u64,
            })
        })?;

        // Post-commit emission — same discipline as `gc()` and
        // `delete_set()`. Always emits, including the no-op case, so
        // operators tailing `mds.changelog.pruned` see every invocation
        // (otherwise a stuck retention loop looks the same as a healthy
        // up-to-date stream).
        self.event_sink
            .emit(MdsEvent::ChangelogPruned(ChangelogPruned {
                set_id: *set,
                stream: stream.as_str().to_string(),
                rows_removed: report.rows_removed,
                new_floor: report.new_floor,
            }));

        Ok(report)
    }
    fn changelog_floor(&self, set: &SetId, stream: ChangelogStream) -> Result<u64> {
        let (_, _, floor_col) = stream_columns(stream);
        self.with_conn_ref(set, |conn| {
            // A set whose set_state row hasn't been materialised yet
            // (theoretically possible if a caller bypassed
            // `seed_set_state`) is reported as floor=0 — the "no
            // prune has run" baseline matches the schema default and
            // avoids surfacing a transient SetNotFound here for a
            // read-only probe.
            // `.optional()` keeps the "set_state row missing → floor=0"
            // baseline while propagating real SQLite errors. A bare
            // `.ok()` would mask schema corruption as a healthy floor.
            let f: Option<i64> = conn
                .query_row(
                    &format!("SELECT {floor_col} FROM set_state WHERE set_id = ?1;"),
                    [set.0.to_string()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| Error::Other(format!("read {floor_col}: {e}")))?;
            Ok(f.unwrap_or(0) as u64)
        })
    }
    fn stats(&self) -> Result<MdsStats> {
        // Lock-order discipline: per-set conn first, then blobs.
        // Same as verify_blobs(Container) — `delete_set` takes set
        // mutex then blobs mutex, so any compound here would risk
        // inversion if it took blobs first. We sidestep by reading
        // each set serially (releasing the lock between iterations)
        // and only acquiring blobs.lock() once at the end.
        let set_ids: Vec<SetId> = self.sets.read().keys().cloned().collect();
        let mut container_count: u64 = 0;
        let mut item_count: u64 = 0;
        let mut logical_bytes: u64 = 0;
        let mut visible_sets: u64 = 0;
        for set in &set_ids {
            let r = self.with_conn_ref(set, query_set_counts);
            match r {
                Ok((cc, ic, lb)) => {
                    container_count += cc;
                    item_count += ic;
                    logical_bytes = logical_bytes.saturating_add(lb);
                    visible_sets += 1;
                }
                // Concurrent delete_set drained the cache entry while
                // we were iterating — the set legitimately no longer
                // exists for this snapshot.
                Err(Error::SetNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let bg = self.blobs.lock();
        let blob_count: i64 = bg
            .query_row("SELECT COUNT(*) FROM blob", [], |r| r.get(0))
            .map_err(|e| Error::Other(format!("count blob: {e}")))?;
        let total_bytes: i64 = bg
            .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM blob", [], |r| {
                r.get(0)
            })
            .map_err(|e| Error::Other(format!("sum blob.size_bytes: {e}")))?;
        let total_bytes = total_bytes as u64;
        // Empty-store dedup_ratio is 1.0 — there's nothing to
        // deduplicate, so the "no compression" baseline is the
        // honest answer (vs 0.0 which would imply infinite
        // compression).
        let dedup_ratio = if total_bytes == 0 {
            1.0
        } else {
            logical_bytes as f64 / total_bytes as f64
        };
        Ok(MdsStats {
            set_count: visible_sets,
            container_count,
            item_count,
            blob_count: blob_count as u64,
            total_bytes,
            dedup_ratio,
        })
    }

    fn export_set(&self, set: &SetId, dest: &Path) -> Result<ExportReport> {
        // Per-set Mutex held for the duration so the data.sqlite
        // VACUUM INTO snapshot and the distinct-hash enumeration see
        // the same quiescent state. blobs.sqlite is never touched —
        // exports carry data.sqlite + CAS, the importing site
        // rebuilds the box-wide index.
        let blobs_root = self.blobs_root();
        self.with_conn_ref(set, |c| {
            crate::export::export_set(c, &blobs_root, *set, dest)
        })
    }

    fn import_set(&self, tarball: &Path) -> Result<ImportReport> {
        // Two-phase import. Phase 1 (`stage`) is lock-free: it
        // streams the tarball, hash-verifies each blob, places CAS
        // bytes (atomic per-file via `blob::put`), and lands
        // `data.sqlite` under `<root>/.import-staging/<uuid>/`. CAS
        // placement is content-addressed so it doesn't need the
        // cache lock; staging is keyed on the manifest's set UUID
        // so concurrent imports of *different* sets don't conflict.
        //
        // Phase 2 (write gate + cache write lock + finalize) mirrors
        // `create_set`'s discipline: if no set with this UUID is
        // visible (cache + disk), we install `data.sqlite` and
        // ingest blob_ref rows in a single per-set transaction
        // through the ATTACH'd `blobs_db`. The gate is deliberately
        // acquired before the cache write lock, preserving the
        // module-level order against `delete_set` (per-set → gate →
        // blobs → cache_write). We do not take `self.blobs` here;
        // the ATTACH'd connection performs the indexed write.
        let started = std::time::Instant::now();
        let staged = crate::import::stage(&self.root, tarball)?;

        let _write_guard = match self.lock_write_gate() {
            Ok(guard) => guard,
            Err(e) => {
                staged.cleanup_staging();
                return Err(e);
            }
        };
        let mut cache = self.sets.write();
        if cache.contains_key(&staged.set_id) {
            staged.cleanup_staging();
            return Err(Error::SetAlreadyExists { set: staged.set_id });
        }
        if set_dir(&self.root, &staged.set_id).exists() {
            staged.cleanup_staging();
            return Err(Error::SetAlreadyExists { set: staged.set_id });
        }

        match crate::import::finalize(&self.root, &staged, started, self.busy_timeout) {
            Ok((report, conn)) => {
                cache.insert(staged.set_id, Arc::new(Mutex::new(Some(conn))));
                Ok(report)
            }
            Err(e) => {
                staged.cleanup_staging();
                Err(e)
            }
        }
    }

    fn stats_per_set(&self) -> Result<Vec<PerSetStats>> {
        let set_ids: Vec<SetId> = self.sets.read().keys().cloned().collect();
        let mut out: Vec<PerSetStats> = Vec::with_capacity(set_ids.len());
        for set in set_ids {
            let r = self.with_conn_ref(&set, query_per_set);
            match r {
                Ok((cc, ic, bc, tb)) => out.push(PerSetStats {
                    set_id: set,
                    container_count: cc,
                    item_count: ic,
                    blob_count: bc,
                    total_bytes: tb,
                }),
                Err(Error::SetNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        // Stable order so `--json` output and the human table are
        // reproducible across invocations on the same root.
        out.sort_by_key(|s| s.set_id.0);
        Ok(out)
    }
}

/// Counts pulled from one set's `data.sqlite` for the global
/// `stats()` aggregation: (containers, items, logical_bytes).
/// SQLite stores integers as i64; we cast at the boundary because
/// the public type is u64 (counts and sizes are non-negative).
fn query_set_counts(c: &Connection) -> Result<(u64, u64, u64)> {
    let cc: i64 = c
        .query_row("SELECT COUNT(*) FROM container", [], |r| r.get(0))
        .map_err(|e| Error::Other(format!("count container: {e}")))?;
    let ic: i64 = c
        .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
        .map_err(|e| Error::Other(format!("count item: {e}")))?;
    let lb: i64 = c
        .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM item", [], |r| {
            r.get(0)
        })
        .map_err(|e| Error::Other(format!("sum item.size_bytes: {e}")))?;
    Ok((cc as u64, ic as u64, lb as u64))
}

/// Counts pulled from one set's `data.sqlite` for the per-set
/// breakdown: (containers, items, distinct_blobs, logical_bytes).
fn query_per_set(c: &Connection) -> Result<(u64, u64, u64, u64)> {
    let cc: i64 = c
        .query_row("SELECT COUNT(*) FROM container", [], |r| r.get(0))
        .map_err(|e| Error::Other(format!("count container: {e}")))?;
    let ic: i64 = c
        .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
        .map_err(|e| Error::Other(format!("count item: {e}")))?;
    let bc: i64 = c
        .query_row("SELECT COUNT(DISTINCT blob_hash) FROM item", [], |r| {
            r.get(0)
        })
        .map_err(|e| Error::Other(format!("count distinct blob_hash: {e}")))?;
    let tb: i64 = c
        .query_row("SELECT COALESCE(SUM(size_bytes), 0) FROM item", [], |r| {
            r.get(0)
        })
        .map_err(|e| Error::Other(format!("sum item.size_bytes: {e}")))?;
    Ok((cc as u64, ic as u64, bc as u64, tb as u64))
}

#[cfg(test)]
mod busy_timeout_tests {
    //! The write-lock wait has to reach the *connections*, not just
    //! sit in a struct field. These live in-crate because the only
    //! way to prove it is to read `PRAGMA busy_timeout` back off a
    //! connection the store owns privately.

    use super::*;
    use tempfile::TempDir;

    /// Read the wait SQLite is actually configured with, in ms.
    fn effective_ms(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("read back busy_timeout")
    }

    fn set_conn_ms(mds: &SqliteCasMds, set: &SetId) -> i64 {
        let cell = mds.set_conn(set).expect("set is in the cache");
        let guard = cell.lock();
        effective_ms(guard.as_ref().expect("connection not drained"))
    }

    #[test]
    fn plain_open_leaves_the_production_ceiling_in_place() {
        let root = TempDir::new().unwrap();
        let mds = SqliteCasMds::open(root.path()).unwrap();

        assert_eq!(mds.busy_timeout(), DEFAULT_BUSY_TIMEOUT);
        assert_eq!(
            effective_ms(&mds.blobs.lock()),
            DEFAULT_BUSY_TIMEOUT.as_millis() as i64,
            "blobs.sqlite must keep the 5s default when nobody asked for more"
        );
    }

    /// The override has to survive `apply_data_migrations`'
    /// `set_pragmas`, which installs the 5s default on every fresh
    /// connection — so it is applied after, and this is what proves
    /// the ordering.
    #[test]
    fn override_reaches_blobs_and_every_set_connection() {
        let wait = Duration::from_millis(37_000);
        let root = TempDir::new().unwrap();

        // Set created through `create_set` on a live store: the
        // lazily-opened path, distinct from the discovery path below.
        let created = SetId(uuid::Uuid::now_v7());
        {
            let mds = SqliteCasMds::open_with_busy_timeout(root.path(), wait).unwrap();
            assert_eq!(mds.busy_timeout(), wait);
            assert_eq!(effective_ms(&mds.blobs.lock()), 37_000);
            mds.create_set(&created).unwrap();
            assert_eq!(
                set_conn_ms(&mds, &created),
                37_000,
                "create_set must open the new connection with the store's wait"
            );
        }

        // Reopen: `created` now comes back via `discover_sets`, the
        // other call site of `open_set_db`.
        let mds = SqliteCasMds::open_with_busy_timeout(root.path(), wait).unwrap();
        assert_eq!(
            set_conn_ms(&mds, &created),
            37_000,
            "a set discovered at open() must get the wait too"
        );

        // And a plain reopen must not inherit the previous run's
        // override from anywhere — the wait is per-connection, set at
        // open time, never persisted in the file.
        let plain = SqliteCasMds::open(root.path()).unwrap();
        assert_eq!(
            set_conn_ms(&plain, &created),
            DEFAULT_BUSY_TIMEOUT.as_millis() as i64,
            "busy_timeout must not persist into the database file"
        );
    }

    #[test]
    fn override_reaches_imported_set_connection() {
        let source_root = TempDir::new().unwrap();
        let source = SqliteCasMds::open(source_root.path()).unwrap();
        let imported = SetId(uuid::Uuid::now_v7());
        source.create_set(&imported).unwrap();
        let tarball = source_root.path().join("set.tar");
        source.export_set(&imported, &tarball).unwrap();

        let wait = Duration::from_millis(37_000);
        let destination_root = TempDir::new().unwrap();
        let destination =
            SqliteCasMds::open_with_busy_timeout(destination_root.path(), wait).unwrap();
        destination.import_set(&tarball).unwrap();

        assert_eq!(
            set_conn_ms(&destination, &imported),
            37_000,
            "an imported set's cached connection must get the store's wait"
        );
    }
}
