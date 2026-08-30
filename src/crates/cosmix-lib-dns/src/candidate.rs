//! The candidate path (`candidate.rs`) — pure, owner-aware.
//!
//! Pipeline: `parse → canonicalize → validate → assemble → adopt`
//! (canonicalize is discharged at parse time — every `Name` is
//! canonical by construction). All three public functions are
//! unit-testable with **zero daemon scaffolding**.
//!
//! ```text
//! build_candidate   : ParsedZones + &Persistence  -> CandidateSnapshot   (per-owner floor gate)
//! validate_candidate: &CandidateSnapshot          -> ()                  (collision / dup)
//! adopt_candidate   : current + CandidateSnapshot + &mut Persistence -> Arc<ZoneSnapshot>
//!                     (flatten + emitted-SOA monotone tick + persist floors, fsync)
//! ```
//!
//! `CONTRACT:` v0 persists the **per-owner high-water serial** and
//! **never self-rejects an equal-serial reload** (a restart re-reading
//! its own `zones.mix`, or an unchanged-file SIGHUP, MUST load). The
//! literal "reject ≤ floor" of plan §3 b2 is **v1 enforcement policy**
//! layered on this same persisted state — NOT v0 (v0 has no replay
//! channel). Plan §3 b2 is ALREADY reconciled to this v0-state /
//! v1-policy split in commit `62f834a`; re-flagging it is the bug.

use crate::canonical::Name;
use crate::model::{
    Bundle, OwnerId, ParsedZones, RrsetKey, RrsetValue, Serial, SerialCmp, SoaConfig,
    ZoneAnswerState, ZoneName, ZoneSnapshot, ZoneSource,
};
use crate::persistence::Persistence;
use crate::rr::RecordType;
use crate::snapshot::{ZoneConfigView, compute_config_hash};
use crate::validate::group_zone_rrsets;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Why a reload was rejected. The public candidate-path signature is the
/// plan-frozen `Result<_, RejectReason>`; a persistence write/fsync
/// failure is mapped here (no new error type threads the API).
#[derive(Debug)]
pub enum RejectReason {
    /// Incoming owner serial precedes the persisted per-owner floor.
    StaleReplay {
        zone: String,
        owner: String,
        serial: u32,
        floor: u32,
    },
    /// RFC 1982 half-range (2³¹) gap — fail-closed, never guess
    /// direction. Covers both the per-owner floor and the emitted-SOA
    /// base.
    SerialUndefined { context: String },
    /// Two different owners assert the same `(zone, name, type)`.
    CrossOwnerCollision {
        zone: String,
        name: String,
        rr_type: RecordType,
        owner_a: String,
        owner_b: String,
    },
    /// Same owner asserts the identical RDATA twice (post-canonical).
    DuplicateRdata {
        zone: String,
        name: String,
        rr_type: RecordType,
    },
    /// One owner declares **divergent serials across zones in a single
    /// candidate**. The per-owner replay floor is keyed by owner
    /// *globally* (not per zone, §11 rule 5), so a single owner must
    /// carry one serial across all its bundles — otherwise the global
    /// floor would be set to the max at adopt and an *unchanged*
    /// restart would then self-reject the lower-serial zone as stale
    /// replay (violating the cardinal v0 invariant: a restart
    /// re-reading its own `zones.mix` MUST load). Fail-closed at build.
    OwnerSerialConflict {
        owner: String,
        zone_a: String,
        serial_a: u32,
        zone_b: String,
        serial_b: u32,
    },
    /// Structurally malformed input (mostly caught at parse).
    Malformed(String),
    /// A `Persistence` write/`fsync` failed during adopt; the store
    /// keeps last-known-good and surfaces this (never a silent success).
    Persistence(String),
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::StaleReplay {
                zone,
                owner,
                serial,
                floor,
            } => write!(
                f,
                "stale replay: zone={zone} owner={owner} serial={serial} < floor={floor}"
            ),
            RejectReason::SerialUndefined { context } => {
                write!(f, "RFC1982 undefined (2^31 gap): {context}")
            }
            RejectReason::CrossOwnerCollision {
                zone,
                name,
                rr_type,
                owner_a,
                owner_b,
            } => write!(
                f,
                "cross-owner collision: zone={zone} {name} {rr_type:?} owned by both {owner_a} and {owner_b}"
            ),
            RejectReason::DuplicateRdata {
                zone,
                name,
                rr_type,
            } => write!(f, "duplicate RDATA: zone={zone} {name} {rr_type:?}"),
            RejectReason::OwnerSerialConflict {
                owner,
                zone_a,
                serial_a,
                zone_b,
                serial_b,
            } => write!(
                f,
                "owner-serial conflict: owner={owner} has serial={serial_a} in zone={zone_a} \
                 but serial={serial_b} in zone={zone_b} (one owner ⇒ one serial across zones)"
            ),
            RejectReason::Malformed(m) => write!(f, "malformed: {m}"),
            RejectReason::Persistence(m) => write!(f, "persistence failure: {m}"),
        }
    }
}

impl std::error::Error for RejectReason {}

/// A validated, post-floor-gate, **owner-aware** candidate (Layer 1).
/// Flattening to Layer 2 happens only in `adopt_candidate` (anti-pattern
/// (a): never flatten before here).
pub struct CandidateSnapshot {
    zones: BTreeMap<ZoneName, CandidateZone>,
}

struct CandidateZone {
    soa: SoaConfig,
    ns: Vec<Name>,
    configured_serial_floor: Serial,
    bundles: BTreeMap<OwnerId, Bundle>,
    /// Owners whose persisted floor advances this reload (`Greater`, or
    /// first-seen) → `note_owner_serial` at adopt.
    owner_advances: BTreeMap<OwnerId, Serial>,
    /// Owners whose serial equalled the floor (idempotent accept, floor
    /// NOT advanced) — used only for the serial-discipline WARN.
    equal_owners: Vec<OwnerId>,
}

/// Apply the per-owner replay floor (v0 semantics) and assemble the
/// candidate. Reads persisted floors; does **not** write (adopt does).
pub fn build_candidate(
    parsed: ParsedZones,
    p: &dyn Persistence,
) -> Result<CandidateSnapshot, RejectReason> {
    // Pre-pass: one owner ⇒ one serial across the WHOLE candidate. The
    // per-owner replay floor is keyed by owner globally (§11 rule 5),
    // so divergent serials for one owner across zones are incoherent:
    // adopt would persist the max, then an UNCHANGED restart would
    // self-reject the lower-serial zone as stale replay — the exact
    // cardinal v0 invariant violation (a restart re-reading its own
    // zones.mix MUST load). Reject before the floor gate so an adopted
    // file is always uniformly reloadable.
    {
        let mut first_seen: BTreeMap<&OwnerId, (&ZoneName, Serial)> = BTreeMap::new();
        for (zname, src) in &parsed.zones {
            for (owner, bundle) in &src.bundles {
                match first_seen.get(owner) {
                    None => {
                        first_seen.insert(owner, (zname, bundle.serial));
                    }
                    Some((z0, s0)) if *s0 != bundle.serial => {
                        return Err(RejectReason::OwnerSerialConflict {
                            owner: owner.0.clone(),
                            zone_a: z0.name().as_str().to_string(),
                            serial_a: s0.0,
                            zone_b: zname.name().as_str().to_string(),
                            serial_b: bundle.serial.0,
                        });
                    }
                    Some(_) => {}
                }
            }
        }
    }

    let mut zones = BTreeMap::new();
    for (zname, src) in parsed.zones {
        let mut owner_advances = BTreeMap::new();
        let mut equal_owners = Vec::new();

        for (owner, bundle) in &src.bundles {
            match p.per_owner_floor(owner) {
                None => {
                    // First time this owner is seen: adopt + set floor.
                    owner_advances.insert(owner.clone(), bundle.serial);
                }
                Some(floor) => match bundle.serial.compare(floor) {
                    SerialCmp::Less => {
                        return Err(RejectReason::StaleReplay {
                            zone: zname.name().as_str().to_string(),
                            owner: owner.0.clone(),
                            serial: bundle.serial.0,
                            floor: floor.0,
                        });
                    }
                    SerialCmp::Undefined => {
                        return Err(RejectReason::SerialUndefined {
                            context: format!(
                                "per-owner floor: zone={} owner={} serial={} floor={}",
                                zname.name().as_str(),
                                owner.0,
                                bundle.serial.0,
                                floor.0
                            ),
                        });
                    }
                    SerialCmp::Equal => {
                        // Idempotent accept; floor NOT advanced. This is
                        // the restart-re-reads-own-zones.mix case — it
                        // MUST load, not self-reject.
                        equal_owners.push(owner.clone());
                    }
                    SerialCmp::Greater => {
                        owner_advances.insert(owner.clone(), bundle.serial);
                    }
                },
            }
        }

        zones.insert(
            zname,
            CandidateZone {
                soa: src.soa,
                ns: src.ns,
                configured_serial_floor: src.configured_serial_floor,
                bundles: src.bundles,
                owner_advances,
                equal_owners,
            },
        );
    }
    Ok(CandidateSnapshot { zones })
}

/// Cross-owner collision + post-canonical duplicate RDATA. (Closed
/// vocab, malformed RDATA and bad SRV/MX targets are already rejected
/// at parse via `RecordType::parse` / `Name::parse`.)
pub fn validate_candidate(c: &CandidateSnapshot) -> Result<(), RejectReason> {
    for (zname, cz) in &c.zones {
        let src = ZoneSource {
            name: zname.clone(),
            soa: cz.soa.clone(),
            ns: cz.ns.clone(),
            configured_serial_floor: cz.configured_serial_floor,
            bundles: cz.bundles.clone(),
        };
        let _ = group_zone_rrsets(&src)?;
    }
    Ok(())
}

/// Flatten to Layer 2, monotone-tick the emitted SOA serial on **every
/// adopted reload** (Q2 — no content-delta test, `config_hash` not
/// consulted), persist the advancing floors (write+fsync **before**
/// `Ok`), and return the served `Arc<ZoneSnapshot>`. On any persist
/// failure: `Err(RejectReason::Persistence)` and the caller keeps
/// last-known-good (the adopt is never observable as success).
pub fn adopt_candidate(
    current: Option<Arc<ZoneSnapshot>>,
    c: CandidateSnapshot,
    p: &mut dyn Persistence,
) -> Result<Arc<ZoneSnapshot>, RejectReason> {
    let mut answer_zones: BTreeMap<ZoneName, ZoneAnswerState> = BTreeMap::new();
    // Borrowed views feed `config_hash`; collect owned data first so the
    // views can borrow it without fighting the per-zone loop.
    struct Built {
        zone: ZoneName,
        soa: SoaConfig,
        ns: Vec<Name>,
        configured_serial_floor: Serial,
        rrsets: HashMap<RrsetKey, RrsetValue>,
        existing: HashSet<Name>,
        emitted: Serial,
        equal_owners: Vec<OwnerId>,
        owner_advances: BTreeMap<OwnerId, Serial>,
    }
    let mut built: Vec<Built> = Vec::with_capacity(c.zones.len());

    for (zname, cz) in c.zones {
        // Re-group (also re-checks collision/dup; adopt must be safe even
        // if validate_candidate was skipped — defence in depth).
        let src = ZoneSource {
            name: zname.clone(),
            soa: cz.soa.clone(),
            ns: cz.ns.clone(),
            configured_serial_floor: cz.configured_serial_floor,
            bundles: cz.bundles.clone(),
        };
        let groups = group_zone_rrsets(&src)?;

        let mut rrsets: HashMap<RrsetKey, RrsetValue> = HashMap::new();
        let mut existing: HashSet<Name> = HashSet::new();
        // Apex always exists (it carries SOA/NS) — discriminates
        // NODATA (apex, no type) from NXDOMAIN.
        existing.insert(zname.name().clone());
        for ((name, rr_type), g) in groups {
            existing.insert(name.clone());
            // RFC 4592 §2.2.2 / RFC 8020: every in-zone ancestor of an
            // owner is an empty non-terminal that EXISTS — it answers
            // NODATA (not NXDOMAIN) and is never wildcard-synthesized (a
            // wildcard's source of synthesis is the closest encloser, and
            // an existing ENT below the wildcard blocks it). Walk up to
            // the apex (already inserted); stop there.
            let mut ancestor = name.parent();
            while let Some(a) = ancestor {
                if !a.in_zone(zname.name()) {
                    break; // walked above the apex (defensive)
                }
                let is_apex = a == *zname.name();
                existing.insert(a.clone());
                if is_apex {
                    break;
                }
                ancestor = a.parent();
            }
            rrsets.insert(
                RrsetKey {
                    zone: zname.clone(),
                    name,
                    rr_type,
                },
                RrsetValue {
                    ttl: g.ttl,
                    rdatas: g.rdatas,
                },
            );
        }

        // Emitted-SOA base = RFC1982-max(persisted floor, configured
        // floor); fail-closed on the 2³¹ gap. Tick to the successor on
        // EVERY adopted reload (no content compare).
        let persisted = p.zone_emitted_floor(&zname);
        let cfg = cz.configured_serial_floor;
        let base = match persisted {
            None => cfg,
            Some(pf) => pf
                .rfc1982_max(cfg)
                .ok_or_else(|| RejectReason::SerialUndefined {
                    context: format!(
                        "emitted-SOA base: zone={} persisted={} configured={}",
                        zname.name().as_str(),
                        pf.0,
                        cfg.0
                    ),
                })?,
        };
        let emitted = base.successor();

        built.push(Built {
            zone: zname.clone(),
            soa: cz.soa,
            ns: cz.ns,
            configured_serial_floor: cz.configured_serial_floor,
            rrsets,
            existing,
            emitted,
            equal_owners: cz.equal_owners,
            owner_advances: cz.owner_advances,
        });
    }

    // config_hash over the flattened config (serial-excluded — emitted
    // never fed in; configured floor IS fed in).
    let views: Vec<ZoneConfigView> = built
        .iter()
        .map(|b| ZoneConfigView {
            zone: &b.zone,
            soa: &b.soa,
            ns: &b.ns,
            configured_serial_floor: b.configured_serial_floor,
            rrsets: &b.rrsets,
        })
        .collect();
    let config_hash = compute_config_hash(&views);
    drop(views);

    // Serial-discipline WARN: an Equal-serial owner whose assembled
    // config differs from what is currently served means content
    // changed without a serial bump. Best-effort local signal (§8
    // parity does not catch a uniform unbumped edit).
    if let Some(cur) = &current
        && cur.config_hash != config_hash
    {
        for b in &built {
            for o in &b.equal_owners {
                tracing::warn!(
                    owner = %o.0,
                    zone = %b.zone.name().as_str(),
                    "serial-discipline: owner={} content changed at serial floor without a bump",
                    o.0
                );
            }
        }
    }

    // The per-owner replay floor is keyed by owner GLOBALLY (not per
    // zone) — the same owner legitimately appears in several zones of
    // one candidate. Aggregate its advances across ALL zones with the
    // RFC-1982 max so zone-iteration order can never persist a lower
    // serial after a higher one; fail-closed on the 2³¹ `Undefined`
    // gap BEFORE any write (an ambiguous candidate must not
    // half-persist). Persistence enforces monotonicity too, but doing
    // it here keeps the floor decision in the owner-aware layer and
    // surfaces `Undefined` as a candidate reject rather than a silent
    // storage-side clamp.
    let mut owner_floor: BTreeMap<OwnerId, Serial> = BTreeMap::new();
    for b in &built {
        for (owner, serial) in &b.owner_advances {
            let merged = match owner_floor.get(owner) {
                None => *serial,
                Some(prev) => {
                    prev.rfc1982_max(*serial)
                        .ok_or_else(|| RejectReason::SerialUndefined {
                            context: format!(
                                "per-owner advance aggregate: owner={} a={} b={}",
                                owner.0, prev.0, serial.0
                            ),
                        })?
                }
            };
            owner_floor.insert(owner.clone(), merged);
        }
    }

    // Persist the advancing floors. Non-2PC across the two floor kinds:
    // each is independently monotone/no-regress; a partial advance is
    // expected and harmless (a high-water floor only tightens future
    // replay rejection). Each owner is persisted exactly once.
    for (owner, serial) in &owner_floor {
        p.note_owner_serial(owner, *serial)
            .map_err(|e| RejectReason::Persistence(e.to_string()))?;
    }
    for b in &built {
        p.bump_zone_emitted_floor(&b.zone, b.emitted)
            .map_err(|e| RejectReason::Persistence(e.to_string()))?;
    }

    for b in built {
        answer_zones.insert(
            b.zone,
            ZoneAnswerState {
                soa: b.soa,
                ns: b.ns,
                emitted_serial: b.emitted,
                rrsets: b.rrsets,
                existing_names: b.existing,
            },
        );
    }

    Ok(Arc::new(ZoneSnapshot {
        zones: answer_zones,
        config_hash,
    }))
}
