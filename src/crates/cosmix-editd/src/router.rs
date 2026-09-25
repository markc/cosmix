//! The router: global verbs, buffer table, and every cross-buffer invariant
//! (plan §4.8, D10).
//!
//! # Contract: refusal precedence (frozen; plan §4.1)
//! The first failing check wins:
//! 1. unknown verb → UNKNOWN_VERB;
//! 2. argument shape (serde into the `wire` request type; both `expect_rev`
//!    and `base_rev` → `both_cas`) → INVALID_ARGUMENT;
//! 3. mutating verb without a `broker_origin` stamp → INVALID_ARGUMENT `unstamped`;
//! 4. mesh lock (`COSMIX_MESH_OPEN=0`, mutating verb, `broker_origin != local`) → FORBIDDEN `mesh_locked`;
//! 5. buffer lookup: wrong epoch → NOT_FOUND `epoch_mismatch`; unknown → `unknown_buffer`;
//! 6. inbox full → RESOURCE_LIMIT `busy`;
//! 7. (in the actor) op_id duplicate → the cached original reply, `duplicate: true`;
//! 8. CAS (`stale_rev`, `history_trimmed`, …);
//! 9. position resolution / validation;
//! 10. limits and byte budget;
//! 11. I/O.
//!
//! # Contract: path table (frozen)
//! The router processes its inbox strictly in order and is the ONLY writer of
//! [`PathSlot`]s. Actors ask over a oneshot; nothing is a lock held across I/O.
//! - `edit.open P`: `Bound` → reopen (add holder); `Loading`/`Closing` → park
//!   in `waiters`; absent → insert `Loading`, take a byte lease for the stat'd
//!   size, spawn the actor, whose FIRST job is the load (`spawn_blocking`).
//!   `Loaded` → `Bound` and every waiter gets the same buffer; `Failed(refusal)`
//!   → slot removed, lease released, every waiter gets the same refusal.
//!   `Closing` waiters re-run as fresh opens once the close completes.
//! - save-as `P`: `ReserveSaveAs(P)`: absent → `SaveAs`, anything else →
//!   CONFLICT `path_open`. After the rename `CommitSaveAs(old, P)` removes
//!   `old`, binds `P`, moves the watch — one inbox step. Any failure →
//!   `ReleaseSaveAs(P)`. An open of `P` while `SaveAs` → CONFLICT `path_open`.
//! - `edit.close`: `Closing`, forward to the actor; the actor stops, releases
//!   its lease, reports `Closed`; the slot is removed and waiters served.
//! - Scratch buffers have no slot. `MAX_BUFFERS` is checked/incremented when a
//!   slot or scratch buffer is created, decremented on `Closed`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use cosmix_edit_core::wire::{BufferId, Refusal};
use tokio::sync::oneshot;

/// The reply channel of a parked `edit.open`.
pub type OpenWaiter = oneshot::Sender<Result<BufferId, Refusal>>;

pub enum PathSlot {
    Loading { bid: BufferId, waiters: Vec<OpenWaiter> },
    Bound { bid: BufferId },
    SaveAs { bid: BufferId },
    Closing { bid: BufferId, waiters: Vec<OpenWaiter> },
}

/// Canonical path → slot. Router-owned.
pub type PathTable = HashMap<PathBuf, PathSlot>;

/// The aggregate byte budget (`MAX_TOTAL_BYTES`: text + retained log text +
/// snapshots). Created by the router, shared with actors. `try_lease` is an
/// atomic `fetch_update` with a checked add, so two actors can never both
/// pass a check only one fits.
///
/// Leases are taken in phase 1 of a transaction (peak growth of text + the log
/// text the entry adds), at open (file size) and at snapshot creation; released
/// on shrinkage (`peak - final` after phase 2), log trimming, snapshot release
/// and actor exit. A refused lease → RESOURCE_LIMIT `budget`, nothing changed.
#[derive(Debug)]
pub struct Budget {
    cap: u64,
    used: AtomicU64,
}

impl Budget {
    pub fn new(cap: u64) -> Self {
        Self { cap, used: AtomicU64::new(0) }
    }

    /// Reserve `n` bytes, or `false` with nothing reserved.
    pub fn try_lease(&self, n: u64) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|total| *total <= self.cap)
            })
            .is_ok()
    }

    /// Return `n` previously leased bytes.
    pub fn release(&self, n: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| Some(used.saturating_sub(n)));
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// Messages actors send the router (answers come back on the oneshot).
pub enum ToRouter {
    Loaded { path: PathBuf, bid: BufferId },
    Failed { path: PathBuf, refusal: Refusal },
    ReserveSaveAs { bid: BufferId, path: PathBuf, reply: oneshot::Sender<Result<(), Refusal>> },
    CommitSaveAs { bid: BufferId, old: Option<PathBuf>, new: PathBuf, reply: oneshot::Sender<()> },
    ReleaseSaveAs { path: PathBuf },
    Closed { bid: BufferId, path: Option<PathBuf> },
}

/// Router state. Stage S: fields frozen, behaviour E0b.
pub struct Router {
    pub epoch: String,
    pub paths: PathTable,
    pub budget: std::sync::Arc<Budget>,
    pub buffer_count: usize,
    pub next_buffer: u64,
    pub mesh_open: bool,
}

impl Router {
    /// `b<N>_<epoch>`.
    pub fn buffer_id(&mut self) -> BufferId {
        self.next_buffer += 1;
        format!("b{}_{}", self.next_buffer, self.epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_admits_only_what_fits() {
        let b = Budget::new(100);
        assert!(b.try_lease(60));
        assert!(!b.try_lease(41));
        assert!(b.try_lease(40));
        b.release(50);
        assert_eq!(b.used(), 50);
        assert!(!b.try_lease(u64::MAX));
    }
}
