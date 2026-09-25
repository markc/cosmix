//! Recovery files: unsaved text survives a daemon crash, restart or SIGTERM
//! (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §5 — the binding
//! contract, repeated here). Stage S froze the types; Stage E1c implemented
//! them: [`Writer`] (the single FIFO writer), [`Recovery`] (the shared handle),
//! [`ActorRec`] (the per-actor boundary state the actor hooks drive) and
//! [`restore`].
//!
//! # Where and when
//! Directory: `COSMIX_EDIT_RECOVERY_DIR`, else `$XDG_STATE_HOME/cosmix/edit/recovery`,
//! else `$HOME/.local/state/cosmix/edit/recovery`; dir `0700`, files `0600`.
//! `COSMIX_EDIT_RECOVERY=0` disables recovery (E0 behaviour, `volatile:true`).
//! Files exist only while a buffer is dirty. They are deleted ONLY when the
//! buffer becomes clean through a **durable** save, a clean reload, or an
//! explicit discard (`close force:true`) — meta first (the commit point), then
//! snap and log, then a directory fsync. An ordinary close never deletes them;
//! after a `durable:false` save they are retained and the next restore cleans
//! them up (content equal to disk). Undo never makes a buffer clean (dirty =
//! `saved_rev != Some(rev)`, revs only grow).
//!
//! # Files and generations
//! `<rid>.meta.json` ([`RecoveryMeta`]) names the current generation `gen`;
//! `<rid>.<gen>.snap` = one [`SnapHeader`] JSON line + exactly `bytes` bytes of
//! text covering every rev `<= header.rev`; `<rid>.<gen>.log` = one
//! [`LogRecord`] JSON line per text entry with rev `> header.rev`, append-only.
//! Nothing is truncated or rewritten in place.
//!
//! **Switch to generation g+1** (clean→dirty, compaction, repair, restore) —
//! frozen order, synchronous in the writer, never debounced:
//! 1. write `<rid>.<g+1>.snap.tmp`, fsync, rename to `<rid>.<g+1>.snap`;
//! 2. create empty `<rid>.<g+1>.log`, fsync;
//! 3. fsync the directory;
//! 4. write `meta.json.tmp` with `gen: g+1`, fsync, rename over `<rid>.meta.json`;
//! 5. fsync the directory (the switch is durable — ack [`RecSignal::switch_done`]);
//! 6. unlink `<rid>.<g>.snap` / `.log`, fsync the directory.
//!
//! # Generation hand-off: the actor owns the boundary (codex round-2 N4)
//! The actor is strictly serial, so in ONE step it sets `R = buffer.rev()`,
//! `gen += 1` and sends [`RecoveryMsg::Switch`]`{gen, rev: R, text}`; every
//! later text entry (rev > R) is sent as `Append{gen: g+1}`, every earlier one
//! was sent as `Append{gen: g}` BEFORE the Switch on the same FIFO. Hence
//! snap(g+1) holds every rev ≤ R and log(g+1) only revs > R — no record in two
//! generations or none. The single writer applies messages in FIFO order; an
//! `Append` for a gen other than the rid's current file gen is unreachable and
//! discarded (debug-asserted). `Append{g}` records before the Switch land in
//! log(g), retired with it.
//!
//! **Overflow** is detected by the actor at reservation time
//! ([`QueueBudget::try_reserve`]): the record is NOT sent, `needs_switch` is
//! set, health goes degraded (`ok:false`, `volatile:true`) and — if no switch
//! is outstanding — the actor issues a Switch at its current rev in the same
//! step (that snapshot contains the unsent record). With a switch outstanding
//! it waits for `SwitchDone` then switches again; meanwhile it sends no
//! Appends. Writer I/O failures and log-size compaction (`log bytes >
//! max(1 MiB, snap bytes)`) raise `needs_switch` through [`RecSignal`] (a
//! coalescing flag + `Notify`, never the actor inbox); the actor answers with a
//! Switch at once.
//!
//! **Durability**: the first unsynced Append arms a one-shot [`SYNC_MS`]
//! debounce, then fdatasync of every touched log. Healthy loss window ≤ 1 s on a
//! crash / heap-OOM abort, 0 on SIGTERM (drain + switches + sync within E0's
//! 10 s exit); degraded: since the last completed sync (visible as
//! `recovery_unsynced`).
//!
//! **Flush barrier** (`edit.recovery.flush`): a [`RecoveryMsg::Flush`] token per
//! actor stream; answered once every message ahead of it is processed, no rid
//! has `needs_switch` / an outstanding switch, and the sync completed.
//!
//! # Restore (at start, BEFORE registration and READY=1)
//! Scan `*.meta.json`; a malformed meta or snap quarantines every file of that
//! rid into `recovery/quarantine/` (never deleted). Replay the log: `h` checked,
//! revs exactly `snap.rev+1, +2, …`; a bad/torn record, a rev ≤ snap.rev or a
//! gap stops replay there — the tail is salvaged, the rest kept. Orphans are
//! swept: files of a non-current gen, `<rid>.*` with no meta, `*.tmp`. The buffer
//! is created as rev 0 = recovered text with `restored_dirty` (see the actor's
//! `dirty()`); a path buffer rebinds and compares with disk (content equal →
//! `base` = current identity, clean, files removed; identity equal → clean
//! disk, dirty buffer; identity different → `disk:"modified"`; missing →
//! `deleted`). Every restored dirty buffer immediately does a generation
//! switch at rev 0. Over `MAX_BUFFERS`/budget → left on disk as `skipped`.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use cosmix_edit_core::buffer::Eol;
use cosmix_edit_core::ot::Edit;
use cosmix_edit_core::text::Text;
use cosmix_edit_core::wire::{DiskState, RecoveredFrom, RecoveryFlushReply, RecoveryInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};

use crate::files::DiskIdentity;

/// Debounce between the first unsynced Append and the fdatasync.
pub const SYNC_MS: u64 = 1000;
/// Encoded bytes of Appends in flight to the writer (shared across actors).
pub const MAX_RECOVERY_QUEUE_BYTES: u64 = 8 * 1024 * 1024;
/// Compaction threshold floor: a log is compacted past `max(this, snap bytes)`.
pub const COMPACT_LOG_MIN_BYTES: u64 = 1024 * 1024;

pub const META_FORMAT: &str = "edit-recovery-v1";
pub const SNAP_FORMAT: &str = "edit-recovery-snap-v1";

/// The disk content a buffer's `saved_rev` corresponds to (E0 `DiskIdentity`,
/// serialisable; `blake3` as 64 lowercase hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub blake3: String,
}

/// `<rid>.meta.json` — the commit point naming the current generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMeta {
    pub format: String,
    pub rid: String,
    #[serde(rename = "gen")]
    pub generation: u64,
    pub path: Option<String>,
    pub opened_as: Option<String>,
    pub language: String,
    pub eol: cosmix_edit_core::buffer::Eol,
    pub bom: bool,
    pub base: Option<BaseIdentity>,
    pub epoch: String,
    pub buffer: String,
    pub created_ms: u64,
}

/// First line of `<rid>.<gen>.snap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapHeader {
    pub format: String,
    #[serde(rename = "gen")]
    pub generation: u64,
    /// The snap covers every rev `<= rev`.
    pub rev: u64,
    pub bytes: u64,
    /// 64 lowercase hex of blake3(text).
    pub blake3: String,
}

/// One line of `<rid>.<gen>.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub rev: u64,
    pub edits: Vec<Edit>,
    /// [`record_hash`] of this record in its generation.
    pub h: String,
}

/// First 16 lowercase hex of `blake3(gen ‖ "\n" ‖ rev ‖ "\n" ‖ compact JSON of edits)`.
pub fn record_hash(generation: u64, rev: u64, edits: &[Edit]) -> String {
    let body = serde_json::to_string(edits).unwrap_or_default();
    let mut h = blake3::Hasher::new();
    h.update(format!("{generation}\n{rev}\n").as_bytes());
    h.update(body.as_bytes());
    h.finalize().to_hex()[..16].to_string()
}

/// Actor → writer, one FIFO for the whole daemon (per-rid order = send order).
pub enum RecoveryMsg {
    /// Start generation `gen` whose snap covers every rev `<= rev`.
    Switch { rid: String, generation: u64, rev: u64, text: Arc<str>, meta: Box<RecoveryMeta> },
    /// One applied text entry (rev > the current gen's snap rev).
    Append { rid: String, generation: u64, rev: u64, edits: Vec<Edit> },
    /// Delete every file of `rid` (clean through a durable save / clean reload
    /// / explicit discard).
    Discard { rid: String },
    /// Barrier: answered when everything ahead of it is durable.
    Flush { reply: oneshot::Sender<FlushOutcome> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushOutcome {
    pub records: u64,
    pub bytes: u64,
    pub repairs: u64,
}

/// Per-actor generation bookkeeping (the boundary owner).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecState {
    /// Generation that new Appends go to.
    pub generation: u64,
    /// A Switch sent but not yet acknowledged durable.
    pub outstanding_switch: Option<u64>,
    /// A record was dropped / a write failed / the log needs compaction.
    pub needs_switch: bool,
}

/// Writer → actor signals: coalescing flags + a `Notify` (never the inbox).
#[derive(Default)]
pub struct RecSignal {
    needs_switch: AtomicBool,
    switch_done: AtomicU64,
    pub notify: Notify,
}

impl RecSignal {
    /// Ask the owning actor for a Switch at its current rev.
    pub fn request_switch(&self) {
        self.needs_switch.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Acknowledge that generation `gen`'s switch is durable (step 5).
    pub fn switch_done(&self, generation: u64) {
        self.switch_done.fetch_max(generation, Ordering::AcqRel);
        self.notify.notify_one();
    }

    /// Actor side: take the pending switch request, if any.
    pub fn take_switch_request(&self) -> bool {
        self.needs_switch.swap(false, Ordering::AcqRel)
    }

    /// Actor side: the highest generation acknowledged durable.
    pub fn durable_gen(&self) -> u64 {
        self.switch_done.load(Ordering::Acquire)
    }
}

/// The shared Append byte budget ([`MAX_RECOVERY_QUEUE_BYTES`]); reserved by
/// the actor before sending, released by the writer once written.
#[derive(Debug, Default)]
pub struct QueueBudget {
    used: AtomicU64,
}

impl QueueBudget {
    pub fn try_reserve(&self, n: u64) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |u| {
                u.checked_add(n).filter(|&t| t <= MAX_RECOVERY_QUEUE_BYTES)
            })
            .is_ok()
    }

    pub fn release(&self, n: u64) {
        let _ = self.used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |u| Some(u.saturating_sub(n)));
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

// ── configuration ───────────────────────────────────────────────────────────

/// Subdirectory of the recovery directory that malformed rids move into.
pub const QUARANTINE_DIR: &str = "quarantine";
/// A meta file is tiny; anything larger is malformed.
const META_MAX_BYTES: u64 = 64 * 1024;
/// The snap header line is tiny; a snap with no `\n` in this prefix is malformed.
const SNAP_HEADER_MAX: usize = 4096;
/// A log is compacted past `max(1 MiB, snap bytes)`, so a healthy one stays
/// under this; restore reads at most this much and treats the rest as a tail.
const LOG_READ_MAX: u64 = 2 * cosmix_edit_core::limits::MAX_BUFFER_BYTES as u64 + 16 * 1024 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Whether recovery files are written, and where (plan §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryConfig {
    pub enabled: bool,
    pub dir: Option<PathBuf>,
}

impl RecoveryConfig {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::from_vars(
            var("COSMIX_EDIT_RECOVERY").as_deref(),
            var("COSMIX_EDIT_RECOVERY_DIR").as_deref(),
            var("XDG_STATE_HOME").as_deref(),
            var("HOME").as_deref(),
        )
    }

    /// `COSMIX_EDIT_RECOVERY=0` disables; the directory is
    /// `COSMIX_EDIT_RECOVERY_DIR`, else `$XDG_STATE_HOME/cosmix/edit/recovery`,
    /// else `$HOME/.local/state/cosmix/edit/recovery`. Relative XDG/HOME values
    /// are ignored (XDG base-directory rule); with no directory at all recovery
    /// is off.
    pub fn from_vars(switch: Option<&str>, dir: Option<&str>, xdg_state: Option<&str>, home: Option<&str>) -> Self {
        let absolute = |v: Option<&str>| v.map(PathBuf::from).filter(|p| p.is_absolute());
        let dir = dir
            .map(PathBuf::from)
            .or_else(|| absolute(xdg_state).map(|x| x.join("cosmix/edit/recovery")))
            .or_else(|| absolute(home).map(|h| h.join(".local/state/cosmix/edit/recovery")));
        Self { enabled: switch != Some("0") && dir.is_some(), dir }
    }
}

impl From<DiskIdentity> for BaseIdentity {
    fn from(d: DiskIdentity) -> Self {
        Self {
            dev: d.dev,
            ino: d.ino,
            size: d.size,
            mtime_ns: d.mtime_ns,
            blake3: blake3::Hash::from_bytes(d.blake3).to_hex().to_string(),
        }
    }
}

impl BaseIdentity {
    /// Back to the actor's identity (`None` when the hash is malformed).
    pub fn to_disk(&self) -> Option<DiskIdentity> {
        let hash = blake3::Hash::from_hex(&self.blake3).ok()?;
        Some(DiskIdentity { dev: self.dev, ino: self.ino, size: self.size, mtime_ns: self.mtime_ns, blake3: *hash.as_bytes() })
    }
}

/// Bytes an `Append` reserves against [`MAX_RECOVERY_QUEUE_BYTES`]: the
/// inserted text plus a fixed per-edit and per-record allowance. The actor
/// reserves and the writer releases this same number.
pub fn append_cost(edits: &[Edit]) -> u64 {
    let per_edit: usize = edits.iter().map(|e| e.insert.len() + 48).sum();
    (per_edit + 96) as u64
}

fn is_rid(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ── shared state ────────────────────────────────────────────────────────────

/// Daemon-wide counters (`edit.info.recovery`, `edit.recovery.flush`).
#[derive(Debug, Default)]
pub struct Stats {
    /// Log records written / their encoded bytes, since start.
    pub records: AtomicU64,
    pub bytes: AtomicU64,
    /// Switches that restored lost protection (overflow, write failure).
    pub repairs: AtomicU64,
    pub failures: AtomicU64,
    /// Records written but not yet fdatasync'd.
    pub unsynced: AtomicU64,
    pub restored: AtomicU64,
    pub quarantined: AtomicU64,
    pub skipped: AtomicU64,
    /// Rids with an actor-detected overflow awaiting its repair Switch.
    pub overflow_rids: AtomicU64,
    /// Appends that reached the writer for a generation it does not hold.
    pub stale_appends: AtomicU64,
}

/// State shared by the writer thread, the actors and the front door.
#[derive(Default)]
pub struct Shared {
    pub budget: QueueBudget,
    pub stats: Stats,
    links: Mutex<HashMap<String, Arc<RecLink>>>,
    /// Rids whose last write failed; cleared by that rid's next durable
    /// Switch (its snapshot covers everything the failure lost) or a Discard.
    failed: Mutex<HashSet<String>>,
    /// Rung when health (failed / overflowed rids) may have changed.
    pub health: Notify,
    halted: AtomicBool,
    dir_error: AtomicBool,
}

impl Shared {
    fn link(&self, rid: &str) -> Option<Arc<RecLink>> {
        self.links.lock().expect("recovery links").get(rid).cloned()
    }

    pub fn is_failed(&self, rid: &str) -> bool {
        self.failed.lock().expect("recovery failures").contains(rid)
    }

    pub fn failed_rids(&self) -> u64 {
        self.failed.lock().expect("recovery failures").len() as u64
    }

    fn set_failed(&self, rid: &str, failed: bool) {
        let changed = {
            let mut set = self.failed.lock().expect("recovery failures");
            if failed { set.insert(rid.to_string()) } else { set.remove(rid) }
        };
        if changed {
            self.health.notify_one();
        }
    }

    /// No failed rid, no overflow awaiting repair, a usable directory.
    pub fn healthy(&self) -> bool {
        self.failed_rids() == 0
            && self.stats.overflow_rids.load(Ordering::Acquire) == 0
            && !self.dir_error.load(Ordering::Acquire)
    }
}

/// One actor's link: the writer's [`RecSignal`], plus switch failures and
/// flush requests (both ring the same `Notify`; never the actor inbox).
#[derive(Default)]
pub struct RecLink {
    pub signal: RecSignal,
    /// Highest generation whose Switch failed (the actor clears its
    /// `outstanding_switch` and retries at the next text entry).
    failed: AtomicU64,
    flushes: Mutex<Vec<oneshot::Sender<FlushOutcome>>>,
}

impl RecLink {
    pub fn switch_failed(&self, generation: u64) {
        self.failed.fetch_max(generation, Ordering::AcqRel);
        self.signal.notify.notify_one();
    }

    pub fn failed_gen(&self) -> u64 {
        self.failed.load(Ordering::Acquire)
    }

    fn push_flush(&self, reply: oneshot::Sender<FlushOutcome>) {
        self.flushes.lock().expect("recovery flushes").push(reply);
        self.signal.notify.notify_one();
    }

    pub fn take_flushes(&self) -> Vec<oneshot::Sender<FlushOutcome>> {
        std::mem::take(&mut *self.flushes.lock().expect("recovery flushes"))
    }
}

/// The daemon's recovery handle: the FIFO into the writer thread plus the
/// shared state. Cloned (as an `Arc`) into the router and every actor.
pub struct Recovery {
    dir: PathBuf,
    tx: Mutex<Option<std_mpsc::Sender<RecoveryMsg>>>,
    pub shared: Arc<Shared>,
}

/// What restore may bring back (the daemon's own limits).
#[derive(Debug, Clone, Copy)]
pub struct RestoreCaps {
    pub max_buffers: usize,
    pub max_bytes: u64,
}

impl Recovery {
    /// Open `dir`, [`restore`] it (quarantine, salvage, sweep, new
    /// generations — all before the caller registers on the Bus), then start
    /// the writer thread. Blocking.
    pub fn start(dir: &Path, epoch: &str, caps: RestoreCaps) -> (Arc<Recovery>, Vec<RestoredBuffer>) {
        let mut writer = Writer::new(dir, Arc::new(Shared::default()));
        let restored = restore(&mut writer, epoch, caps);
        (Self::spawn(writer), restored)
    }

    /// Run `writer` on its own thread (blocking I/O, one FIFO).
    pub fn spawn(writer: Writer) -> Arc<Recovery> {
        let (tx, rx) = std_mpsc::channel();
        let dir = writer.dir.clone();
        let shared = writer.shared.clone();
        let spawned = std::thread::Builder::new().name("editd-recovery".into()).spawn(move || writer.run(rx));
        let tx = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::error!("cosmix-editd: cannot start the recovery writer: {e}; buffers are volatile");
                shared.dir_error.store(true, Ordering::Release);
                None
            }
        };
        Arc::new(Recovery { dir, tx: Mutex::new(tx), shared })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Enqueue for the writer; never blocks. `false` when the writer is gone.
    pub fn send(&self, msg: RecoveryMsg) -> bool {
        match self.tx.lock().expect("recovery sender").as_ref() {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    pub fn register(&self, rid: &str) -> Arc<RecLink> {
        let link = Arc::new(RecLink::default());
        self.shared.links.lock().expect("recovery links").insert(rid.to_string(), link.clone());
        link
    }

    pub fn unregister(&self, rid: &str) {
        self.shared.links.lock().expect("recovery links").remove(rid);
    }

    /// `edit.recovery.flush` (plan §5.1): one token per actor — each actor
    /// enqueues it once it has no Switch outstanding or wanted, so everything
    /// it owes is ahead of the token — plus one daemon-wide token. The writer
    /// answers a token after everything ahead of it is written and synced.
    /// `synced` is false when the writer is gone or a rid is still unprotected.
    pub async fn flush(&self) -> RecoveryFlushReply {
        let links: Vec<Arc<RecLink>> = self.shared.links.lock().expect("recovery links").values().cloned().collect();
        let mut waits = Vec::with_capacity(links.len());
        for link in links {
            let (tx, rx) = oneshot::channel();
            link.push_flush(tx);
            waits.push(rx);
        }
        let (tx, rx) = oneshot::channel();
        let sent = self.send(RecoveryMsg::Flush { reply: tx });
        // An actor that ended forwards (or drops) its token: either way it owes nothing more.
        for wait in waits {
            let _ = wait.await;
        }
        let answered = sent && rx.await.is_ok();
        let s = &self.shared.stats;
        RecoveryFlushReply {
            synced: answered && self.shared.healthy(),
            records: s.records.load(Ordering::Acquire),
            bytes: s.bytes.load(Ordering::Acquire),
            repairs: s.repairs.load(Ordering::Acquire),
        }
    }

    /// `edit.info.recovery`.
    pub fn info(&self) -> RecoveryInfo {
        let s = &self.shared.stats;
        let ok = self.shared.healthy();
        RecoveryInfo {
            enabled: true,
            ok,
            degraded: !ok,
            dir: Some(self.dir.display().to_string()),
            sync_ms: SYNC_MS,
            queue_bytes: self.shared.budget.used(),
            unsynced: s.unsynced.load(Ordering::Acquire),
            failures: s.failures.load(Ordering::Acquire),
            restored: s.restored.load(Ordering::Acquire),
            quarantined: s.quarantined.load(Ordering::Acquire),
            skipped: s.skipped.load(Ordering::Acquire),
        }
    }

    /// Test hook: the writer stops where it stands — nothing further is
    /// written or synced (a crash, as far as the files can tell).
    #[doc(hidden)]
    pub fn halt(&self) {
        self.shared.halted.store(true, Ordering::Release);
        self.tx.lock().expect("recovery sender").take();
    }
}

// ── the actor side ──────────────────────────────────────────────────────────

/// Per-actor recovery state: the generation-boundary owner (§5.1, codex N4).
/// The actor hooks (`rec_*` in `actor.rs`) drive it; the actor is serial, so
/// every decision here is a position in its own message stream.
pub struct ActorRec {
    pub recovery: Arc<Recovery>,
    pub link: Arc<RecLink>,
    pub rid: String,
    pub created_ms: u64,
    pub st: RecState,
    /// Files exist for this rid (or the Switch creating them is queued).
    pub active: bool,
    /// The last Switch failed: the next text entry retries it (no hot loop
    /// against a failing disk).
    pub held: bool,
    overflow: bool,
    /// The first Switch issued after the latest overflow; health returns once
    /// it is durable.
    pub repair_gen: Option<u64>,
    /// The outstanding Switch's text: part of the actor's leased footprint
    /// until the writer acknowledges it.
    pub switch_text: Option<Arc<str>>,
    /// The meta the newest Switch carried (`gen` 0): any change re-switches,
    /// so a restore never binds a stale path or base.
    pub written: Option<RecoveryMeta>,
}

impl ActorRec {
    pub fn new(recovery: Arc<Recovery>, rid: &str, created_ms: u64, st: RecState, active: bool) -> Self {
        let link = recovery.register(rid);
        Self {
            recovery,
            link,
            rid: rid.to_string(),
            created_ms,
            st,
            active,
            held: false,
            overflow: false,
            repair_gen: None,
            switch_text: None,
            written: None,
        }
    }

    pub fn overflow(&self) -> bool {
        self.overflow
    }

    /// A record was dropped at reservation (or a snapshot could not be
    /// leased): degraded until a later Switch is durable.
    pub fn set_overflow(&mut self, on: bool) {
        if on {
            // Only a Switch issued AFTER this overflow can repair it.
            self.repair_gen = None;
        }
        if on != self.overflow {
            self.overflow = on;
            let n = &self.recovery.shared.stats.overflow_rids;
            if on {
                n.fetch_add(1, Ordering::AcqRel);
            } else {
                let _ = n.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1)));
            }
            self.recovery.shared.health.notify_one();
        }
    }

    /// The Switch about to be issued restores lost protection.
    pub fn is_repair(&self) -> bool {
        self.overflow || self.held || self.recovery.shared.is_failed(&self.rid)
    }
}

impl Drop for ActorRec {
    fn drop(&mut self) {
        self.set_overflow(false);
        // Pending flush tokens go behind everything this actor sent.
        for reply in self.link.take_flushes() {
            self.recovery.send(RecoveryMsg::Flush { reply });
        }
        self.recovery.unregister(&self.rid);
    }
}

// ── the writer ──────────────────────────────────────────────────────────────

/// Crash-injection points (tests): the hook returning `true` at a point stops
/// the writer there, as a kill would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// After step N (1..=6) of a generation switch.
    SwitchStep(u8),
    /// After any message was handled.
    Message,
}

type FaultHook = Box<dyn FnMut(Fault) -> bool + Send>;

struct RidFiles {
    generation: u64,
    log: Option<File>,
    log_bytes: u64,
    snap_bytes: u64,
    unsynced: u64,
    compaction_asked: bool,
}

/// The single recovery writer: applies [`RecoveryMsg`]s in FIFO order with
/// blocking I/O on its own thread ([`Recovery::spawn`]); restore drives it
/// synchronously first. Tests drive it directly.
pub struct Writer {
    dir: PathBuf,
    shared: Arc<Shared>,
    files: HashMap<String, RidFiles>,
    logged: HashSet<String>,
    sync_due: Option<Instant>,
    fault: Option<FaultHook>,
    crashed: bool,
    strict: bool,
}

fn create_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn list_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

impl Writer {
    /// Create `dir` (`0700`) if needed. A directory that cannot be made or
    /// secured leaves recovery degraded (`ok:false`), never refuses to start.
    pub fn new(dir: &Path, shared: Arc<Shared>) -> Self {
        let made = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)));
        if let Err(e) = made {
            tracing::error!("cosmix-editd: recovery directory {}: {e}; buffers are volatile", dir.display());
            shared.dir_error.store(true, Ordering::Release);
        }
        Self {
            dir: dir.to_path_buf(),
            shared,
            files: HashMap::new(),
            logged: HashSet::new(),
            sync_due: None,
            fault: None,
            crashed: false,
            strict: true,
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Test hook: crash where `hook` returns true.
    #[doc(hidden)]
    pub fn set_fault(&mut self, hook: impl FnMut(Fault) -> bool + Send + 'static) {
        self.fault = Some(Box::new(hook));
    }

    /// Test hook: a stale-generation Append is debug-asserted unreachable;
    /// `false` lets a test exercise the discard path in a debug build.
    #[doc(hidden)]
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }

    pub fn crashed(&self) -> bool {
        self.crashed
    }

    /// The generation `rid`'s meta names on disk, as far as this writer knows.
    pub fn current_gen(&self, rid: &str) -> Option<u64> {
        self.files.get(rid).map(|f| f.generation)
    }

    /// Restore: `rid` has generation `generation` on disk already (so the
    /// next Switch retires its files).
    fn adopt(&mut self, rid: &str, generation: u64) {
        self.files.entry(rid.to_string()).or_insert(RidFiles {
            generation,
            log: None,
            log_bytes: 0,
            snap_bytes: 0,
            unsynced: 0,
            compaction_asked: false,
        });
    }

    fn fault(&mut self, point: Fault) -> bool {
        if !self.crashed
            && let Some(hook) = self.fault.as_mut()
            && hook(point)
        {
            self.crashed = true;
        }
        self.crashed
    }

    fn fsync_dir(&self) -> std::io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }

    /// Apply one message (FIFO order is the caller's).
    pub fn handle(&mut self, msg: RecoveryMsg) {
        if self.crashed {
            return;
        }
        match msg {
            RecoveryMsg::Switch { rid, generation, rev, text, meta } => self.switch(&rid, generation, rev, &text, &meta),
            RecoveryMsg::Append { rid, generation, rev, edits } => self.append(&rid, generation, rev, edits),
            RecoveryMsg::Discard { rid } => self.discard(&rid),
            RecoveryMsg::Flush { reply } => {
                self.sync();
                let s = &self.shared.stats;
                let _ = reply.send(FlushOutcome {
                    records: s.records.load(Ordering::Acquire),
                    bytes: s.bytes.load(Ordering::Acquire),
                    repairs: s.repairs.load(Ordering::Acquire),
                });
            }
        }
        self.fault(Fault::Message);
    }

    /// Record a failed write: counted, logged once per rid, the rid marked
    /// failed (degraded). A failed Switch is retried at the rid's next text
    /// entry; any other failure asks the actor for a repair Switch at once.
    fn fail(&mut self, rid: &str, switch: Option<u64>, what: &str, e: &std::io::Error) {
        self.shared.stats.failures.fetch_add(1, Ordering::AcqRel);
        self.shared.set_failed(rid, true);
        if self.logged.insert(rid.to_string()) {
            tracing::error!("cosmix-editd: recovery {what} for {rid} failed: {e}; retrying at the next record or repair");
        }
        if let Some(link) = self.shared.link(rid) {
            match switch {
                Some(generation) => link.switch_failed(generation),
                None => link.signal.request_switch(),
            }
        }
    }

    fn switch(&mut self, rid: &str, generation: u64, rev: u64, text: &Arc<str>, meta: &RecoveryMeta) {
        if let Err(e) = self.try_switch(rid, generation, rev, text, meta) {
            self.fail(rid, Some(generation), &format!("switch to generation {generation}"), &e);
        }
    }

    /// The frozen six steps (module docs). `Ok` also when a fault stopped it.
    fn try_switch(&mut self, rid: &str, generation: u64, rev: u64, text: &str, meta: &RecoveryMeta) -> std::io::Result<()> {
        let prev = self.files.get(rid).map(|f| f.generation).filter(|g| *g != generation);

        // 1. the snapshot, via a synced temp file
        let snap = self.dir.join(format!("{rid}.{generation}.snap"));
        let tmp = self.dir.join(format!("{rid}.{generation}.snap.tmp"));
        let header = SnapHeader {
            format: SNAP_FORMAT.into(),
            generation,
            rev,
            bytes: text.len() as u64,
            blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
        };
        {
            let mut f = create_private(&tmp)?;
            f.write_all(&to_json(&header)?)?;
            f.write_all(b"\n")?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &snap)?;
        if self.fault(Fault::SwitchStep(1)) {
            return Ok(());
        }

        // 2. an empty log
        let log_path = self.dir.join(format!("{rid}.{generation}.log"));
        let log = create_private(&log_path)?;
        log.sync_all()?;
        if self.fault(Fault::SwitchStep(2)) {
            return Ok(());
        }

        // 3.
        self.fsync_dir()?;
        if self.fault(Fault::SwitchStep(3)) {
            return Ok(());
        }

        // 4. the commit point
        let mut meta = meta.clone();
        meta.generation = generation;
        let meta_tmp = self.dir.join(format!("{rid}.meta.json.tmp"));
        {
            let mut f = create_private(&meta_tmp)?;
            f.write_all(&to_json(&meta)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&meta_tmp, self.dir.join(format!("{rid}.meta.json")))?;
        let retired = self.files.insert(
            rid.to_string(),
            RidFiles {
                generation,
                log: Some(log),
                log_bytes: 0,
                snap_bytes: text.len() as u64,
                unsynced: 0,
                compaction_asked: false,
            },
        );
        if let Some(old) = retired {
            // Records of the retired log are covered by the new snapshot.
            let _ = self.shared.stats.unsynced.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(old.unsynced))
            });
        }
        if self.fault(Fault::SwitchStep(4)) {
            return Ok(());
        }

        // 5. durable: acknowledge
        self.fsync_dir()?;
        self.shared.set_failed(rid, false);
        self.logged.remove(rid);
        if let Some(link) = self.shared.link(rid) {
            link.signal.switch_done(generation);
        }
        if self.fault(Fault::SwitchStep(5)) {
            return Ok(());
        }

        // 6. retire the old generation (a failure here only leaves orphans
        // for the restore sweep: the switch itself is complete)
        if let Some(old) = prev {
            let retire = remove_if_exists(&self.dir.join(format!("{rid}.{old}.snap")))
                .and_then(|()| remove_if_exists(&self.dir.join(format!("{rid}.{old}.log"))))
                .and_then(|()| self.fsync_dir());
            if let Err(e) = retire {
                tracing::warn!("cosmix-editd: retiring recovery generation {old} of {rid}: {e}");
            }
        }
        self.fault(Fault::SwitchStep(6));
        Ok(())
    }

    fn append(&mut self, rid: &str, generation: u64, rev: u64, edits: Vec<Edit>) {
        self.shared.budget.release(append_cost(&edits));
        let failed = self.shared.is_failed(rid);
        let current = self.files.get(rid).map(|f| f.generation);
        if current != Some(generation) {
            // Unreachable by construction (the actor switches before it
            // appends to a new generation) — except after a failed write,
            // whose repair Switch supersedes the record.
            if !failed {
                self.shared.stats.stale_appends.fetch_add(1, Ordering::AcqRel);
                tracing::error!("cosmix-editd: recovery Append for {rid} gen {generation} (on disk: {current:?}) discarded");
                debug_assert!(!self.strict, "stale recovery Append for {rid}: gen {generation}, on disk {current:?}");
            }
            return;
        }
        if failed {
            // Nothing is written after a failure (no gaps): the pending
            // repair Switch covers this record.
            return;
        }
        let record = LogRecord { rev, h: record_hash(generation, rev, &edits), edits };
        let mut line = match serde_json::to_vec(&record) {
            Ok(l) => l,
            Err(e) => {
                self.fail(rid, None, "encoding a record", &std::io::Error::other(e));
                return;
            }
        };
        line.push(b'\n');
        let Some(f) = self.files.get_mut(rid) else { return };
        let written = match f.log.as_mut() {
            Some(log) => log.write_all(&line),
            None => Err(std::io::Error::other("the log is not open")),
        };
        if let Err(e) = written {
            self.fail(rid, None, "append", &e);
            return;
        }
        f.log_bytes += line.len() as u64;
        f.unsynced += 1;
        let compact = !f.compaction_asked && f.log_bytes > COMPACT_LOG_MIN_BYTES.max(f.snap_bytes);
        if compact {
            f.compaction_asked = true;
        }
        let s = &self.shared.stats;
        s.records.fetch_add(1, Ordering::AcqRel);
        s.bytes.fetch_add(line.len() as u64, Ordering::AcqRel);
        s.unsynced.fetch_add(1, Ordering::AcqRel);
        if self.sync_due.is_none() {
            self.sync_due = Some(Instant::now() + Duration::from_millis(SYNC_MS));
        }
        if compact && let Some(link) = self.shared.link(rid) {
            link.signal.request_switch();
        }
    }

    /// Meta first (the commit point), then every other `<rid>.*`, then the
    /// directory. A failure deletes nothing further (a restore may bring the
    /// buffer back; never the reverse).
    fn discard(&mut self, rid: &str) {
        if let Some(f) = self.files.remove(rid) {
            let _ = self.shared.stats.unsynced.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(f.unsynced))
            });
        }
        let prefix = format!("{rid}.");
        let meta = format!("{rid}.meta.json");
        let removed = remove_if_exists(&self.dir.join(&meta))
            .and_then(|()| self.fsync_dir())
            .and_then(|()| list_names(&self.dir))
            .and_then(|names| {
                for name in names.iter().filter(|n| n.starts_with(&prefix)) {
                    remove_if_exists(&self.dir.join(name))?;
                }
                self.fsync_dir()
            });
        if let Err(e) = removed {
            self.shared.stats.failures.fetch_add(1, Ordering::AcqRel);
            tracing::error!("cosmix-editd: discarding recovery files of {rid}: {e}");
        }
        self.shared.set_failed(rid, false);
        self.logged.remove(rid);
    }

    /// fdatasync every log with unsynced records.
    pub fn sync(&mut self) {
        self.sync_due = None;
        let mut failures = Vec::new();
        for (rid, f) in self.files.iter_mut().filter(|(_, f)| f.unsynced > 0) {
            match f.log.as_ref().map(File::sync_data) {
                Some(Ok(())) | None => {
                    let n = std::mem::take(&mut f.unsynced);
                    let _ = self.shared.stats.unsynced.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                        Some(v.saturating_sub(n))
                    });
                }
                Some(Err(e)) => failures.push((rid.clone(), e)),
            }
        }
        for (rid, e) in failures {
            self.fail(&rid, None, "sync", &e);
        }
    }

    /// The writer thread: FIFO messages, plus the one-shot sync debounce
    /// armed by the first unsynced Append (a deadline on the receive, not a
    /// poll). Ends when every sender is gone, after a final sync.
    pub fn run(mut self, rx: std_mpsc::Receiver<RecoveryMsg>) {
        loop {
            let msg = match self.sync_due {
                Some(due) => {
                    let now = Instant::now();
                    if due <= now {
                        self.sync();
                        continue;
                    }
                    match rx.recv_timeout(due - now) {
                        Ok(m) => m,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {
                            self.sync();
                            continue;
                        }
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                },
            };
            if self.shared.halted.load(Ordering::Acquire) {
                self.crashed = true;
            }
            self.handle(msg);
        }
        if !self.crashed {
            self.sync();
        }
    }
}

fn to_json<T: Serialize>(value: &T) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(std::io::Error::other)
}

// ── restore ─────────────────────────────────────────────────────────────────

/// One buffer brought back by [`restore`], ready for the router to seed.
#[derive(Debug, Clone)]
pub struct RestoredBuffer {
    /// Pre-assigned `b<N>_<epoch>` (the router seeds these first, in order).
    pub bid: String,
    pub rid: String,
    pub text: String,
    /// `None` for scratch, and for a second rid naming an already-claimed path.
    pub path: Option<PathBuf>,
    pub opened_as: Option<String>,
    pub language: String,
    pub eol: Eol,
    pub bom: bool,
    pub base: Option<DiskIdentity>,
    pub disk: DiskState,
    /// Content equal to disk: restored clean, its files already discarded.
    pub clean: bool,
    pub created_ms: u64,
    pub from: RecoveredFrom,
    /// The rid's newest generation: on disk unless `clean` (then its files
    /// are gone, and the next Switch still counts up from here).
    pub generation: u64,
    /// The restore Switch failed: the old generation still holds the text,
    /// and the actor must switch before anything else is written.
    pub needs_switch: bool,
    /// Log records dropped from a bad or torn tail.
    pub dropped_records: u64,
}

struct LoadedRid {
    rid: String,
    meta: RecoveryMeta,
    text: String,
    rev: u64,
    time_ms: u64,
    dropped: u64,
}

fn read_capped(path: &Path, cap: u64, over_is_error: bool) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if over_is_error && len > cap {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{len} bytes is over {cap}")));
    }
    let mut out = Vec::with_capacity(len.min(cap) as usize);
    file.take(cap).read_to_end(&mut out)?;
    Ok(out)
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One log line: the next rev, its `h`, and edits that apply.
fn replay_line(text: &mut Text, generation: u64, expect_rev: u64, line: &[u8]) -> Result<(), String> {
    let record: LogRecord = serde_json::from_slice(line).map_err(|e| format!("unparsable record: {e}"))?;
    if record.rev != expect_rev {
        return Err(format!("rev {} where {expect_rev} was due", record.rev));
    }
    if record.h != record_hash(generation, record.rev, &record.edits) {
        return Err(format!("rev {}: hash mismatch", record.rev));
    }
    let prepared = text.prepare(record.edits).map_err(|e| format!("rev {}: {e}", record.rev))?;
    text.commit(prepared);
    Ok(())
}

/// Meta + snap must be intact (else the caller quarantines); the log replays
/// until its first bad or torn record (salvage).
fn load_rid(dir: &Path, rid: &str) -> Result<LoadedRid, String> {
    let meta_path = dir.join(format!("{rid}.meta.json"));
    let raw = read_capped(&meta_path, META_MAX_BYTES, true).map_err(|e| format!("meta: {e}"))?;
    let meta: RecoveryMeta = serde_json::from_slice(&raw).map_err(|e| format!("meta: {e}"))?;
    if meta.format != META_FORMAT {
        return Err(format!("meta: format {:?}", meta.format));
    }
    if meta.rid != rid {
        return Err(format!("meta: names rid {:?}", meta.rid));
    }
    let generation = meta.generation;
    let snap_path = dir.join(format!("{rid}.{generation}.snap"));
    let cap = cosmix_edit_core::limits::MAX_BUFFER_BYTES as u64 + SNAP_HEADER_MAX as u64;
    let raw = read_capped(&snap_path, cap, true).map_err(|e| format!("snap: {e}"))?;
    let nl = raw
        .iter()
        .take(SNAP_HEADER_MAX)
        .position(|&b| b == b'\n')
        .ok_or_else(|| "snap: no header line".to_string())?;
    let header: SnapHeader = serde_json::from_slice(&raw[..nl]).map_err(|e| format!("snap header: {e}"))?;
    let body = &raw[nl + 1..];
    if header.format != SNAP_FORMAT || header.generation != generation {
        return Err(format!("snap header: format {:?} gen {} (meta gen {generation})", header.format, header.generation));
    }
    if body.len() as u64 != header.bytes {
        return Err(format!("snap: {} bytes where the header says {}", body.len(), header.bytes));
    }
    if blake3::hash(body).to_hex().as_str() != header.blake3 {
        return Err("snap: blake3 mismatch".into());
    }
    let snap_text = std::str::from_utf8(body).map_err(|e| format!("snap: not UTF-8: {e}"))?;
    let mut text = Text::from_text(snap_text).map_err(|e| format!("snap: {e}"))?;

    let log_path = dir.join(format!("{rid}.{generation}.log"));
    let log = match read_capped(&log_path, LOG_READ_MAX, false) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("cosmix-editd: recovery log of {rid} unreadable ({e}); restoring the snapshot alone");
            Vec::new()
        }
    };
    let mut rev = header.rev;
    let mut dropped = 0u64;
    let mut rest: &[u8] = &log;
    while !rest.is_empty() {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            dropped += 1; // a torn final record
            break;
        };
        if let Err(why) = replay_line(&mut text, generation, rev + 1, &rest[..end]) {
            let after = &rest[end + 1..];
            let complete = after.iter().filter(|&&b| b == b'\n').count() as u64;
            dropped += 1 + complete + u64::from(!after.is_empty() && !after.ends_with(b"\n"));
            tracing::warn!("cosmix-editd: recovery log of {rid} stops at rev {} ({why})", rev + 1);
            break;
        }
        rev += 1;
        rest = &rest[end + 1..];
    }
    if dropped > 0 {
        tracing::warn!("cosmix-editd: salvaged {rid} to rev {rev}; {dropped} log record(s) dropped");
    }
    let mut out = String::with_capacity(text.len());
    text.read(0..text.len(), &mut out);
    let time_ms = [&meta_path, &snap_path, &log_path].iter().map(|p| mtime_ms(p)).max().unwrap_or(0);
    Ok(LoadedRid { rid: rid.to_string(), meta, text: out, rev, time_ms, dropped })
}

/// Move every file of `rid` into `quarantine/` (never deleted).
fn quarantine(dir: &Path, rid: &str, names: &[String]) {
    let qdir = dir.join(QUARANTINE_DIR);
    if let Err(e) = std::fs::create_dir_all(&qdir)
        .and_then(|()| std::fs::set_permissions(&qdir, std::fs::Permissions::from_mode(0o700)))
    {
        tracing::error!("cosmix-editd: cannot make {}: {e}; leaving {rid} in place", qdir.display());
        return;
    }
    let prefix = format!("{rid}.");
    for name in names.iter().filter(|n| n.starts_with(&prefix)) {
        let mut dest = qdir.join(name);
        let mut n = 1;
        while dest.exists() {
            dest = qdir.join(format!("{name}.{n}"));
            n += 1;
        }
        if let Err(e) = std::fs::rename(dir.join(name), &dest) {
            tracing::error!("cosmix-editd: quarantining {name}: {e}");
        }
    }
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// The disk half of §5.2(4): `(base, disk, content_equal)`.
fn compare_disk(path: &Path, text: &str, meta_base: Option<&BaseIdentity>) -> (Option<DiskIdentity>, DiskState, bool) {
    let recorded = meta_base.and_then(BaseIdentity::to_disk);
    match crate::files::read_bounded(path) {
        Ok((stat, bytes)) => {
            let id = DiskIdentity::from_parts(stat, &bytes);
            let body = bytes.strip_prefix(BOM).unwrap_or(&bytes);
            if body == text.as_bytes() {
                (Some(id), DiskState::Clean, true)
            } else if recorded == Some(id) {
                (recorded, DiskState::Clean, false)
            } else {
                (recorded, DiskState::Modified, false)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (recorded, if recorded.is_some() { DiskState::Deleted } else { DiskState::None }, false)
        }
        Err(_) => (recorded, DiskState::Modified, false),
    }
}

/// §5.2, driven through `writer` BEFORE the daemon registers: sweep orphans
/// (`*.tmp`, meta-less rids, non-current generations), quarantine malformed
/// meta/snap, salvage log tails, rebind paths against the disk, discard
/// content-equal rids and give every other restored buffer a fresh
/// generation at rev 0. Newest first: over `caps` the rest stay on disk
/// (`skipped`), and a path named by two rids binds the newer.
pub fn restore(writer: &mut Writer, epoch: &str, caps: RestoreCaps) -> Vec<RestoredBuffer> {
    let dir = writer.dir.clone();
    let stats = writer.shared.clone();
    let stats = &stats.stats;
    let names = match list_names(&dir) {
        Ok(n) => n,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::error!("cosmix-editd: reading recovery directory {}: {e}", dir.display());
            }
            return Vec::new();
        }
    };
    let sweep = |name: &str| {
        if let Err(e) = remove_if_exists(&dir.join(name)) {
            tracing::warn!("cosmix-editd: sweeping recovery file {name}: {e}");
        }
    };
    let mut metas = Vec::new();
    let mut data: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for name in &names {
        if name.ends_with(".tmp") {
            sweep(name);
            continue;
        }
        let Some((rid, rest)) = name.split_once('.') else { continue };
        if !is_rid(rid) {
            continue;
        }
        if rest == "meta.json" {
            metas.push(rid.to_string());
        } else if let Some((g, kind)) = rest.split_once('.')
            && (kind == "snap" || kind == "log")
            && let Ok(g) = g.parse::<u64>()
        {
            data.entry(rid.to_string()).or_default().push((name.clone(), g));
        }
    }
    for (rid, files) in &data {
        if !metas.contains(rid) {
            for (name, _) in files {
                sweep(name);
            }
        }
    }
    let live: Vec<String> = list_names(&dir).unwrap_or_default();
    let mut loaded = Vec::new();
    for rid in metas {
        match load_rid(&dir, &rid) {
            Ok(l) => {
                for (name, g) in data.get(&rid).into_iter().flatten() {
                    if *g != l.meta.generation {
                        sweep(name);
                    }
                }
                loaded.push(l);
            }
            Err(why) => {
                tracing::error!("cosmix-editd: recovery files of {rid} are malformed ({why}); quarantined");
                quarantine(&dir, &rid, &live);
                stats.quarantined.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    loaded.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then_with(|| a.rid.cmp(&b.rid)));

    let mut out = Vec::new();
    let mut claimed = HashSet::new();
    let mut bytes = 0u64;
    for l in loaded {
        if out.len() >= caps.max_buffers || bytes + l.text.len() as u64 > caps.max_bytes {
            tracing::warn!("cosmix-editd: recovery of {} deferred: over the buffer or byte limit", l.rid);
            stats.skipped.fetch_add(1, Ordering::AcqRel);
            continue;
        }
        bytes += l.text.len() as u64;
        let bid = format!("b{}_{epoch}", out.len() + 1);
        let mut path = l.meta.path.as_ref().map(PathBuf::from);
        let mut opened_as = l.meta.opened_as.clone();
        if let Some(p) = &path
            && !claimed.insert(p.clone())
        {
            tracing::warn!("cosmix-editd: {} names {}, already restored from a newer rid; restoring it unbound", l.rid, p.display());
            path = None;
            opened_as = None;
        }
        let (base, disk, clean) = match &path {
            None => (None, DiskState::None, false),
            Some(p) => compare_disk(p, &l.text, l.meta.base.as_ref()),
        };
        let mut generation = l.meta.generation;
        let mut needs_switch = false;
        writer.adopt(&l.rid, l.meta.generation);
        if clean {
            writer.handle(RecoveryMsg::Discard { rid: l.rid.clone() });
        } else {
            generation = l.meta.generation + 1;
            let meta = RecoveryMeta {
                format: META_FORMAT.into(),
                rid: l.rid.clone(),
                generation,
                path: path.as_ref().map(|p| p.display().to_string()),
                opened_as: opened_as.clone(),
                language: l.meta.language.clone(),
                eol: l.meta.eol,
                bom: l.meta.bom,
                base: base.map(BaseIdentity::from),
                epoch: epoch.to_string(),
                buffer: bid.clone(),
                created_ms: l.meta.created_ms,
            };
            writer.handle(RecoveryMsg::Switch {
                rid: l.rid.clone(),
                generation,
                rev: 0,
                text: Arc::from(l.text.as_str()),
                meta: Box::new(meta),
            });
            needs_switch = writer.current_gen(&l.rid) != Some(generation) || writer.shared.is_failed(&l.rid);
        }
        stats.restored.fetch_add(1, Ordering::AcqRel);
        tracing::info!(
            "cosmix-editd: restored {} as {bid} ({}, rev {} of epoch {}{})",
            l.rid,
            path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "scratch".into()),
            l.rev,
            l.meta.epoch,
            if clean { ", equal to disk" } else { "" }
        );
        out.push(RestoredBuffer {
            bid,
            rid: l.rid,
            path,
            opened_as,
            language: l.meta.language,
            eol: l.meta.eol,
            bom: l.meta.bom,
            base,
            disk,
            clean,
            created_ms: l.meta.created_ms,
            from: RecoveredFrom { epoch: l.meta.epoch, rev: l.rev, time_ms: l.time_ms },
            generation,
            needs_switch,
            dropped_records: l.dropped,
            text: l.text,
        });
    }
    out
}

/// A fresh meta's `created_ms`.
pub fn created_now() -> u64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_hash_is_generation_and_rev_bound() {
        let e = vec![Edit { offset: 1204, delete: 0, insert: "a".into() }];
        let h = record_hash(7, 58, &e);
        assert_eq!(h.len(), 16);
        assert_ne!(h, record_hash(8, 58, &e));
        assert_ne!(h, record_hash(7, 59, &e));
    }

    #[test]
    fn queue_budget_admits_only_what_fits() {
        let b = QueueBudget::default();
        assert!(b.try_reserve(MAX_RECOVERY_QUEUE_BYTES - 1));
        assert!(!b.try_reserve(2));
        b.release(MAX_RECOVERY_QUEUE_BYTES - 1);
        assert!(b.try_reserve(2));
        assert_eq!(b.used(), 2);
    }

    #[test]
    fn signal_coalesces() {
        let s = RecSignal::default();
        s.request_switch();
        s.request_switch();
        assert!(s.take_switch_request());
        assert!(!s.take_switch_request());
        s.switch_done(3);
        s.switch_done(2);
        assert_eq!(s.durable_gen(), 3);
    }
}
