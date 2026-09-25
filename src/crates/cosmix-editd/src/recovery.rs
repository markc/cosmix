//! Recovery files: unsaved text survives a daemon crash, restart or SIGTERM
//! (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §5 — the binding
//! contract, repeated here). Stage S freezes the types; Stage E1c implements.
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cosmix_edit_core::ot::Edit;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};

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
