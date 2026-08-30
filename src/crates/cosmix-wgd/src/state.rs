//! Shared snapshot between the reconcile loop (writer) and the Bus surface
//! (readers). One `std::sync::Mutex` guards a single latest `Snapshot`; the
//! Bus dispatch is synchronous and never holds the lock across an await.

use std::sync::{Arc, Mutex};

use crate::derive::IntendedPeerSet;
use crate::reconcile::DriftReport;

/// The latest reconcile result. Rebuilt wholesale each tick; readers see either
/// the previous snapshot or the new one, never a torn mix.
#[derive(Clone)]
pub struct Snapshot {
    /// The kernel interface the reconcile ran against.
    pub iface: String,
    /// The intended peer set derived from the verified inventory this tick.
    pub intended: IntendedPeerSet,
    /// The dry-run reconcile against live kernel state — `Ok(report)`, or
    /// `Err(reason)` when the live read failed (e.g. the lab interface is not
    /// up). A live-read failure is not fatal: the intended set still serves.
    pub live: Result<DriftReport, String>,
    /// When this snapshot was produced (unix seconds, injected — no clock in
    /// the pure core).
    pub refreshed_at_unix: u64,
}

/// Handle shared by the reconcile loop and the Bus task.
pub type Shared = Arc<Mutex<Option<Snapshot>>>;

/// A fresh, empty shared handle (no reconcile has run yet).
pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(None))
}
