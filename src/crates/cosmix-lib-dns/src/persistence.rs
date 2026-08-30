//! `Persistence` boundary (`persistence.rs`).
//!
//! Two **infallible readers** (`per_owner_floor`, `zone_emitted_floor`
//! — a missing floor is `None` = "no prior state") and two **fallible
//! mutators** (`note_owner_serial`, `bump_zone_emitted_floor` — write +
//! `fsync` **before** `Ok`). A swallowed fsync failure silently
//! re-enables resurrection (the exact class plan §3 b4 prevents), so
//! the mutators return `Result` and `adopt_candidate` maps a failure to
//! `RejectReason::Persistence` + last-known-good.
//!
//! **Non-2PC across the two floors (prompt §3, MAJOR):** both floors
//! live in the one state file but each is independently monotone /
//! no-regress. A crash between the two writes recovers to a per-floor
//! value `≥` its last persisted value (never a torn *regression*). The
//! contract is "no floor regresses across restart/partition", **not** a
//! cross-floor 2-phase commit (a v1/residual-2 concern). A partial
//! advance is therefore expected and harmless.
//!
//! **Format is the implementer's unpinned choice EXCEPT a mandatory
//! leading format-version tag (IS P1 — forward-compat hygiene so P2 can
//! detect/migrate).** Hardened corruption-recovery (checksum /
//! backup-floor / regress-detection / refuse-on-corrupt) and path
//! policy are P2/residual-2.
//
// P2: residual-2 — format/recovery/path *hardening* (the leading
// version tag itself is P1).

use crate::canonical::Name;
use crate::model::{OwnerId, Serial, ZoneName};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A write/`fsync` failure on a mutating `Persistence` method. Carries
/// a human-readable cause; `adopt_candidate` wraps it into
/// `RejectReason::Persistence` (no new error type threads the public
/// API).
#[derive(Debug)]
pub struct PersistenceError(pub String);

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "persistence write/fsync failed: {}", self.0)
    }
}

impl std::error::Error for PersistenceError {}

pub trait Persistence: Send + Sync {
    fn per_owner_floor(&self, owner: &OwnerId) -> Option<Serial>;
    /// Monotone; MUST write + `fsync` before returning `Ok`. The
    /// "monotone" half is **enforced by the implementation** (the
    /// stored value never regresses below the current floor regardless
    /// of caller), not merely a caller obligation — see [`high_water`].
    fn note_owner_serial(&mut self, owner: &OwnerId, s: Serial) -> Result<(), PersistenceError>;
    fn zone_emitted_floor(&self, zone: &ZoneName) -> Option<Serial>;
    /// Monotone; MUST write + `fsync` before returning `Ok`. Identical
    /// durability + impl-enforced-monotonicity contract to
    /// `note_owner_serial` (neither weaker).
    fn bump_zone_emitted_floor(
        &mut self,
        zone: &ZoneName,
        s: Serial,
    ) -> Result<(), PersistenceError>;
}

/// The value a high-water floor takes after a mutator is asked to
/// record `s`: the RFC-1982 max of the existing floor and `s`. A
/// caller passing a stale or ambiguous (`Undefined`, the 2³¹ gap)
/// serial can therefore never *regress* a persisted floor — the
/// "monotone" word in the trait is an impl guarantee, not a caller
/// promise. `None` (no prior floor) → `s` (first observation).
fn high_water(existing: Option<Serial>, s: Serial) -> Serial {
    match existing {
        None => s,
        // `rfc1982_max` is `None` only on the 2³¹ `Undefined` gap;
        // fail-closed by keeping the existing floor (never regress,
        // never adopt an ambiguous value).
        Some(e) => e.rfc1982_max(s).unwrap_or(e),
    }
}

// ---------------------------------------------------------------------
// In-memory (tests) — with a fault-injection switch for the §9
// fallible-persistence test.
// ---------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryPersistence {
    owners: BTreeMap<OwnerId, Serial>,
    zones: BTreeMap<ZoneName, Serial>,
    /// When set, both mutators return `Err` without recording — models
    /// an injected fsync/write failure.
    fail_writes: bool,
}

impl InMemoryPersistence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct one whose mutators always fail (injected fsync/write
    /// failure) — the §9 fallible-persistence test driver.
    pub fn failing() -> Self {
        Self {
            fail_writes: true,
            ..Self::default()
        }
    }

    pub fn set_fail_writes(&mut self, fail: bool) {
        self.fail_writes = fail;
    }
}

impl Persistence for InMemoryPersistence {
    fn per_owner_floor(&self, owner: &OwnerId) -> Option<Serial> {
        self.owners.get(owner).copied()
    }

    fn note_owner_serial(&mut self, owner: &OwnerId, s: Serial) -> Result<(), PersistenceError> {
        if self.fail_writes {
            return Err(PersistenceError("injected in-memory failure".into()));
        }
        let next = high_water(self.owners.get(owner).copied(), s);
        self.owners.insert(owner.clone(), next);
        Ok(())
    }

    fn zone_emitted_floor(&self, zone: &ZoneName) -> Option<Serial> {
        self.zones.get(zone).copied()
    }

    fn bump_zone_emitted_floor(
        &mut self,
        zone: &ZoneName,
        s: Serial,
    ) -> Result<(), PersistenceError> {
        if self.fail_writes {
            return Err(PersistenceError("injected in-memory failure".into()));
        }
        let next = high_water(self.zones.get(zone).copied(), s);
        self.zones.insert(zone.clone(), next);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Minimal fsynced file persistence (standalone binary).
// ---------------------------------------------------------------------

/// Mandatory leading format-version tag. The *rest* of the format is
/// unpinned (P2/residual-2); this single line is P1 so P2 can
/// detect/migrate rather than reverse-engineer or silently discard.
const STATE_VERSION_LINE: &str = "cosmix-dnsd-state v1";

/// Outcome of opening a state file at boot.
pub enum StateLoad {
    /// Parsed cleanly (or genuinely absent → empty floors).
    Ok(FilePersistence),
    /// Existed but was unreadable/unparseable/wrong-version. The caller
    /// **logs loudly, treats both floors as absent, keeps serving**
    /// (availability-first; safe in v0 — no replay channel, no
    /// zone-transfer secondaries). NOT a hard stop.
    //
    // P2: residual-2 — hardened state-file corruption-recovery
    // (v0 = log-loud + treat-as-absent + keep serving).
    Corrupt {
        store: FilePersistence,
        detail: String,
    },
}

/// Whole-file-rewrite, atomic (`write tmp → fsync → rename → fsync
/// dir`), monotone. Each mutator persists durably before returning.
pub struct FilePersistence {
    path: PathBuf,
    owners: BTreeMap<OwnerId, Serial>,
    zones: BTreeMap<ZoneName, Serial>,
}

impl FilePersistence {
    /// Open (or initialize) the state file. A genuinely-absent file is
    /// `StateLoad::Ok` with empty floors (same path as first run). An
    /// existing-but-corrupt file is `StateLoad::Corrupt` with empty
    /// floors — the caller decides logging; both floors are absent.
    pub fn open(path: &Path) -> StateLoad {
        let empty = FilePersistence {
            path: path.to_path_buf(),
            owners: BTreeMap::new(),
            zones: BTreeMap::new(),
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return StateLoad::Ok(empty);
            }
            Err(e) => {
                return StateLoad::Corrupt {
                    store: empty,
                    detail: format!("read {}: {e}", path.display()),
                };
            }
        };
        match parse_state(&raw) {
            Ok((owners, zones)) => StateLoad::Ok(FilePersistence {
                path: path.to_path_buf(),
                owners,
                zones,
            }),
            Err(detail) => StateLoad::Corrupt {
                store: empty,
                detail,
            },
        }
    }

    fn flush(&self) -> Result<(), PersistenceError> {
        let mut body = String::new();
        body.push_str(STATE_VERSION_LINE);
        body.push('\n');
        for (o, s) in &self.owners {
            body.push_str(&format!("owner\t{}\t{}\n", o.0, s.0));
        }
        for (z, s) in &self.zones {
            body.push_str(&format!("zone\t{}\t{}\n", z.name().as_str(), s.0));
        }

        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let tmp = self.path.with_extension("tmp");

        let map_err = |e: std::io::Error, what: &str| PersistenceError(format!("{what}: {e}"));

        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| map_err(e, "create tmp"))?;
            f.write_all(body.as_bytes())
                .map_err(|e| map_err(e, "write tmp"))?;
            f.sync_all().map_err(|e| map_err(e, "fsync tmp"))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| map_err(e, "rename"))?;
        // Best-effort dir fsync so the rename is durable; a dir that
        // cannot be opened (rare) is not fatal to the floor contract
        // because the renamed file is already fsynced.
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

impl Persistence for FilePersistence {
    fn per_owner_floor(&self, owner: &OwnerId) -> Option<Serial> {
        self.owners.get(owner).copied()
    }

    fn note_owner_serial(&mut self, owner: &OwnerId, s: Serial) -> Result<(), PersistenceError> {
        let next = high_water(self.owners.get(owner).copied(), s);
        let prev = self.owners.insert(owner.clone(), next);
        if let Err(e) = self.flush() {
            // Roll the in-memory map back so a failed persist is never
            // observable as success on a later read.
            match prev {
                Some(p) => {
                    self.owners.insert(owner.clone(), p);
                }
                None => {
                    self.owners.remove(owner);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    fn zone_emitted_floor(&self, zone: &ZoneName) -> Option<Serial> {
        self.zones.get(zone).copied()
    }

    fn bump_zone_emitted_floor(
        &mut self,
        zone: &ZoneName,
        s: Serial,
    ) -> Result<(), PersistenceError> {
        let next = high_water(self.zones.get(zone).copied(), s);
        let prev = self.zones.insert(zone.clone(), next);
        if let Err(e) = self.flush() {
            match prev {
                Some(p) => {
                    self.zones.insert(zone.clone(), p);
                }
                None => {
                    self.zones.remove(zone);
                }
            }
            return Err(e);
        }
        Ok(())
    }
}

/// The two persisted floor maps: per-owner replay high-water and
/// per-zone emitted-SOA high-water (§3 — distinct floors, one file).
type ParsedState = (BTreeMap<OwnerId, Serial>, BTreeMap<ZoneName, Serial>);

fn parse_state(raw: &str) -> Result<ParsedState, String> {
    let mut lines = raw.lines();
    match lines.next() {
        Some(l) if l == STATE_VERSION_LINE => {}
        Some(other) => {
            return Err(format!(
                "unexpected version line {other:?} (expected {STATE_VERSION_LINE:?})"
            ));
        }
        None => return Err("empty state file".into()),
    }
    let mut owners = BTreeMap::new();
    let mut zones = BTreeMap::new();
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut it = line.split('\t');
        let kind = it.next().unwrap_or("");
        let key = it
            .next()
            .ok_or_else(|| format!("line {}: missing key", i + 2))?;
        let val = it
            .next()
            .ok_or_else(|| format!("line {}: missing value", i + 2))?;
        let serial: u32 = val
            .parse()
            .map_err(|_| format!("line {}: bad serial {val:?}", i + 2))?;
        match kind {
            "owner" => {
                owners.insert(OwnerId(key.to_string()), Serial(serial));
            }
            "zone" => {
                let n = Name::parse(key)
                    .map_err(|e| format!("line {}: bad zone {key:?}: {e}", i + 2))?;
                zones.insert(ZoneName(n), Serial(serial));
            }
            other => return Err(format!("line {}: unknown kind {other:?}", i + 2)),
        }
    }
    Ok((owners, zones))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn file_round_trip_no_regress_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state");
        let owner = OwnerId("alpha".into());
        let zone = ZoneName(Name::parse("bus").unwrap());

        match FilePersistence::open(&path) {
            StateLoad::Ok(mut p) => {
                p.note_owner_serial(&owner, Serial(7)).unwrap();
                p.bump_zone_emitted_floor(&zone, Serial(42)).unwrap();
            }
            StateLoad::Corrupt { .. } => panic!("fresh file must be Ok"),
        }
        // Reopen: floors survive (no regress across restart).
        match FilePersistence::open(&path) {
            StateLoad::Ok(p) => {
                assert_eq!(p.per_owner_floor(&owner), Some(Serial(7)));
                assert_eq!(p.zone_emitted_floor(&zone), Some(Serial(42)));
            }
            StateLoad::Corrupt { detail, .. } => panic!("reopen corrupt: {detail}"),
        }
    }

    #[test]
    fn corrupt_state_is_treated_as_absent_not_a_hard_stop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::write(&path, b"garbage not a version line\n").unwrap();
        match FilePersistence::open(&path) {
            StateLoad::Corrupt { store, detail } => {
                assert!(detail.contains("version line"), "detail: {detail}");
                // Both floors absent — same path as first run.
                assert_eq!(store.per_owner_floor(&OwnerId("x".into())), None);
            }
            StateLoad::Ok(_) => panic!("garbage must be Corrupt"),
        }
    }

    #[test]
    fn mutators_enforce_monotonicity_regardless_of_caller() {
        // A direct caller passing a stale serial must NOT regress the
        // stored floor — "monotone" is an impl guarantee (Codex MAJOR).
        let owner = OwnerId("alpha".into());
        let zone = ZoneName(Name::parse("bus").unwrap());

        let mut m = InMemoryPersistence::new();
        m.note_owner_serial(&owner, Serial(7)).unwrap();
        m.note_owner_serial(&owner, Serial(3)).unwrap(); // stale → ignored
        assert_eq!(
            m.per_owner_floor(&owner),
            Some(Serial(7)),
            "no regress (in-mem)"
        );
        m.note_owner_serial(&owner, Serial(7)).unwrap(); // equal → idempotent
        assert_eq!(m.per_owner_floor(&owner), Some(Serial(7)));
        m.note_owner_serial(&owner, Serial(9)).unwrap(); // greater → advances
        assert_eq!(m.per_owner_floor(&owner), Some(Serial(9)));
        m.bump_zone_emitted_floor(&zone, Serial(42)).unwrap();
        m.bump_zone_emitted_floor(&zone, Serial(10)).unwrap(); // stale
        assert_eq!(
            m.zone_emitted_floor(&zone),
            Some(Serial(42)),
            "zone floor no regress"
        );

        let dir = tempdir().unwrap();
        let path = dir.path().join("state");
        if let StateLoad::Ok(mut f) = FilePersistence::open(&path) {
            f.note_owner_serial(&owner, Serial(7)).unwrap();
            f.note_owner_serial(&owner, Serial(3)).unwrap(); // stale → ignored
            assert_eq!(
                f.per_owner_floor(&owner),
                Some(Serial(7)),
                "no regress (file)"
            );
        } else {
            panic!("fresh file must be Ok");
        }
        // Survives reopen at the high-water, not the last value written.
        match FilePersistence::open(&path) {
            StateLoad::Ok(f) => {
                assert_eq!(f.per_owner_floor(&owner), Some(Serial(7)));
            }
            StateLoad::Corrupt { detail, .. } => panic!("reopen corrupt: {detail}"),
        }
    }

    #[test]
    fn missing_file_is_ok_with_empty_floors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        match FilePersistence::open(&path) {
            StateLoad::Ok(p) => {
                assert_eq!(
                    p.zone_emitted_floor(&ZoneName(Name::parse("bus").unwrap())),
                    None
                );
            }
            StateLoad::Corrupt { .. } => panic!("absent file is first-run Ok, not corrupt"),
        }
    }
}
