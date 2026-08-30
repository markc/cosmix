//! `ZoneStore` seam (`store.rs`).
//!
//! `StaticZoneStore` holds an `ArcSwap<ZoneSnapshot>` plus `reload()`
//! which runs the candidate path and **adopts-or-keeps-last-good**:
//! on any reject the previously-served `Arc<ZoneSnapshot>` is retained
//! (no panic, no exit, in-zone names keep resolving). v0 has no
//! cross-node merge — its "fold" is the trivial validate-then-adopt of
//! the hand-assembled file.
//!
//! **Only first boot with no last-good is a hard stop**
//! (`load_initial` → `Err(StoreInitError)`; the binary exits non-zero).

use crate::candidate::{RejectReason, adopt_candidate, build_candidate, validate_candidate};
use crate::model::ZoneSnapshot;
use crate::persistence::Persistence;
use crate::strict_data::parse_zones_file;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub trait ZoneStore: Send + Sync {
    fn snapshot(&self) -> Arc<ZoneSnapshot>;
}

/// First-boot failure (no last-known-good exists → hard stop). Distinct
/// from `RejectReason`, which on *reload* is recoverable (last-good).
#[derive(Debug)]
pub enum StoreInitError {
    Parse(String),
    Reject(RejectReason),
}

impl std::fmt::Display for StoreInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreInitError::Parse(m) => write!(f, "first-boot zones.mix parse failed: {m}"),
            StoreInitError::Reject(r) => write!(f, "first-boot zones.mix rejected: {r}"),
        }
    }
}

impl std::error::Error for StoreInitError {}

pub struct StaticZoneStore {
    current: ArcSwap<ZoneSnapshot>,
    persistence: Mutex<Box<dyn Persistence>>,
    zones_path: PathBuf,
}

impl StaticZoneStore {
    /// Load the initial snapshot. A first-boot failure (parse or
    /// reject) with no last-good is a **hard stop** — returns `Err`.
    pub fn load_initial(
        zones_path: PathBuf,
        mut persistence: Box<dyn Persistence>,
    ) -> Result<Self, StoreInitError> {
        let parsed =
            parse_zones_file(&zones_path).map_err(|e| StoreInitError::Parse(e.to_string()))?;
        let candidate =
            build_candidate(parsed, persistence.as_ref()).map_err(StoreInitError::Reject)?;
        validate_candidate(&candidate).map_err(StoreInitError::Reject)?;
        let snap = adopt_candidate(None, candidate, persistence.as_mut())
            .map_err(StoreInitError::Reject)?;
        Ok(Self {
            current: ArcSwap::new(snap),
            persistence: Mutex::new(persistence),
            zones_path,
        })
    }

    /// Re-read `zones.mix` and adopt-or-keep-last-good. On reject the
    /// previous snapshot is retained and the `RejectReason` returned
    /// (the caller logs it; the daemon keeps serving).
    pub fn reload(&self) -> Result<(), RejectReason> {
        let parsed = parse_zones_file(&self.zones_path)
            .map_err(|e| RejectReason::Malformed(e.to_string()))?;
        let mut guard = self.persistence.lock().unwrap_or_else(|p| p.into_inner());
        let candidate = build_candidate(parsed, &**guard)?;
        validate_candidate(&candidate)?;
        let prev = self.current.load_full();
        let next = adopt_candidate(Some(prev), candidate, &mut **guard)?;
        self.current.store(next);
        Ok(())
    }
}

impl ZoneStore for StaticZoneStore {
    fn snapshot(&self) -> Arc<ZoneSnapshot> {
        self.current.load_full()
    }
}
