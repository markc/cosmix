//! Cross-owner collision + post-canonical duplicate detection
//! (`validate.rs`).
//!
//! Pure, owner-aware. Operates on the accepted (post-floor-gate)
//! bundles. Apex SOA/NS are config, not assertions, so they are not
//! grouped here. Withdrawn (tombstone) assertions suppress a record
//! (plan §3 b3) — they do not produce an answer, so they are excluded
//! from collision/dup detection and from the flattened RRset.

use crate::candidate::RejectReason;
use crate::canonical::Name;
use crate::model::{OwnerId, ZoneSource};
use crate::rr::{RData, RecordType};
use std::collections::{BTreeMap, BTreeSet};

/// One assembled RRset (a `(name, type)` group within a zone), tracking
/// which owners contributed for cross-owner-collision detection.
pub(crate) struct GroupInfo {
    pub ttl: u32,
    pub rdatas: BTreeSet<RData>,
    pub owner: OwnerId,
}

/// Group a zone's **non-withdrawn** assertions by `(name, type)`,
/// rejecting on cross-owner collision and post-canonical duplicate
/// RDATA. Multi-RDATA for one owner (2 MX, 2 A, 2 NS) is legal — it is
/// the same owner contributing several rdatas to one RRset.
pub(crate) fn group_zone_rrsets(
    zone: &ZoneSource,
) -> Result<BTreeMap<(Name, RecordType), GroupInfo>, RejectReason> {
    let mut groups: BTreeMap<(Name, RecordType), GroupInfo> = BTreeMap::new();

    for (owner, bundle) in &zone.bundles {
        for rec in &bundle.records {
            if rec.withdrawn {
                continue;
            }
            let key = (rec.name.clone(), rec.rr_type);
            match groups.get_mut(&key) {
                None => {
                    let mut rdatas = BTreeSet::new();
                    rdatas.insert(rec.rdata.clone());
                    groups.insert(
                        key,
                        GroupInfo {
                            ttl: rec.ttl,
                            rdatas,
                            owner: owner.clone(),
                        },
                    );
                }
                Some(g) => {
                    if &g.owner != owner {
                        return Err(RejectReason::CrossOwnerCollision {
                            zone: zone.name.name().as_str().to_string(),
                            name: rec.name.as_str().to_string(),
                            rr_type: rec.rr_type,
                            owner_a: g.owner.0.clone(),
                            owner_b: owner.0.clone(),
                        });
                    }
                    // Same owner: post-canonical duplicate RDATA?
                    if !g.rdatas.insert(rec.rdata.clone()) {
                        return Err(RejectReason::DuplicateRdata {
                            zone: zone.name.name().as_str().to_string(),
                            name: rec.name.as_str().to_string(),
                            rr_type: rec.rr_type,
                        });
                    }
                    // RRset shares one TTL; take the minimum.
                    g.ttl = g.ttl.min(rec.ttl);
                }
            }
        }
    }
    Ok(groups)
}
