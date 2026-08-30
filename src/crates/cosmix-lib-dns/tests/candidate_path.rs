//! §9 tier-2: candidate path + per-owner floor (v0 semantics) +
//! emitted-serial monotone-tick + fallible persistence + tombstone.
//!
//! The load-bearing invariants this whole P1 exists to pin. The
//! monotone-tick (Q2) and the non-2PC "no-regress not no-advance"
//! persistence tests are the most review-sensitive — written precisely.

mod common;

use common::adopt;
use cosmix_dns::{
    FilePersistence, InMemoryPersistence, Name, OwnerId, Persistence, PersistenceError, RecordType,
    RejectReason, RrsetKey, Serial, SerialCmp, StateLoad, StaticZoneStore, StoreInitError,
    ZoneName, ZoneSnapshot, ZoneStore,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn bus_min(alpha_serial: u32, a_ip: &str) -> String {
    format!(
        r#"zones: {{
  "bus": {{
    soa: {{ primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 }},
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      {{ owner: "alpha", serial: {alpha_serial}, records: [
          {{ name: "alpha.bus", type: "A", ttl: 300, data: "{a_ip}" }}
      ] }}
    ]
  }}
}}
"#
    )
}

fn bus_zone() -> ZoneName {
    ZoneName(Name::parse("bus").unwrap())
}
fn alpha() -> OwnerId {
    OwnerId("alpha".to_string())
}

/// RFC-1982 monotone "did not regress" (Serial is deliberately `!Ord`):
/// `after` precedes `before` (or the 2³¹ gap) ⇒ regress. `None→…` and
/// `None→None` are fine; `Some→None` is a regress.
fn no_regress(before: Option<Serial>, after: Option<Serial>) -> bool {
    match (before, after) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(b), Some(a)) => matches!(a.compare(b), SerialCmp::Equal | SerialCmp::Greater),
    }
}

fn a_rrset(snap: &ZoneSnapshot) -> &cosmix_dns::RrsetValue {
    let z = snap.zones.get(&bus_zone()).expect("bus present");
    z.rrsets
        .get(&RrsetKey {
            zone: bus_zone(),
            name: Name::parse("alpha.bus").unwrap(),
            rr_type: RecordType::A,
        })
        .expect("alpha.bus A rrset")
}

// ── valid → snapshot; structurally bad → reject ──────────────────────

#[test]
fn valid_candidate_produces_snapshot() {
    let mut p = InMemoryPersistence::new();
    let snap = adopt(common::ZONES_SRC, &mut p, None).expect("valid adopt");
    assert!(snap.zones.contains_key(&bus_zone()));
    assert!(
        snap.zones
            .contains_key(&ZoneName(Name::parse("example.com").unwrap()))
    );
}

#[test]
fn cross_owner_collision_rejected() {
    let src = r#"zones: {
  "bus": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 1, records: [
          { name: "x.bus", type: "A", ttl: 300, data: "1.1.1.1" } ] },
      { owner: "gamma", serial: 1, records: [
          { name: "x.bus", type: "A", ttl: 300, data: "2.2.2.2" } ] }
    ]
  }
}
"#;
    let mut p = InMemoryPersistence::new();
    let e = adopt(src, &mut p, None).unwrap_err();
    assert!(
        matches!(e, RejectReason::CrossOwnerCollision { .. }),
        "got {e:?}"
    );
}

#[test]
fn post_canonical_duplicate_rdata_rejected() {
    let src = r#"zones: {
  "bus": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 1, records: [
          { name: "x.bus", type: "A", ttl: 300, data: "1.1.1.1" },
          { name: "x.bus", type: "A", ttl: 300, data: "1.1.1.1" } ] }
    ]
  }
}
"#;
    let mut p = InMemoryPersistence::new();
    let e = adopt(src, &mut p, None).unwrap_err();
    assert!(
        matches!(e, RejectReason::DuplicateRdata { .. }),
        "got {e:?}"
    );
}

#[test]
fn malformed_rdata_rejected() {
    let mut p = InMemoryPersistence::new();
    let e = adopt(&bus_min(1, "999.1.1.1"), &mut p, None).unwrap_err();
    assert!(matches!(e, RejectReason::Malformed(_)), "got {e:?}");
}

#[test]
fn first_boot_bad_zones_is_hard_stop() {
    let dir = tempfile::tempdir().unwrap();
    let zpath = dir.path().join("zones.mix");
    std::fs::write(&zpath, "this is not valid mix data {{{").unwrap();
    let StateLoad::Ok(p) = FilePersistence::open(&dir.path().join("state")) else {
        panic!("fresh state must be Ok");
    };
    let r = StaticZoneStore::load_initial(zpath, Box::new(p));
    assert!(matches!(
        r,
        Err(StoreInitError::Parse(_) | StoreInitError::Reject(_))
    ));
}

// ── per-owner replay floor (v0 semantics) ────────────────────────────

#[test]
fn floor_less_is_rejected() {
    let mut p = InMemoryPersistence::new();
    adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("seed floor 5");
    let e = adopt(&bus_min(4, "10.0.0.2"), &mut p, None).unwrap_err();
    assert!(matches!(e, RejectReason::StaleReplay { .. }), "got {e:?}");
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(5)),
        "floor unchanged"
    );
}

#[test]
fn floor_undefined_half_range_is_rejected_fail_closed() {
    let mut p = InMemoryPersistence::new();
    adopt(&bus_min(1, "10.0.0.1"), &mut p, None).expect("seed floor 1");
    // 1 and 1+2^31 are exactly 2^31 apart → Undefined → reject.
    let e = adopt(&bus_min(2_147_483_649, "10.0.0.2"), &mut p, None).unwrap_err();
    assert!(
        matches!(e, RejectReason::SerialUndefined { .. }),
        "got {e:?}"
    );
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(1)),
        "floor unchanged"
    );
}

#[test]
fn floor_equal_is_idempotent_accept_floor_not_advanced() {
    let mut p = InMemoryPersistence::new();
    adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("seed");
    // Restart re-reading its own unchanged zones.mix MUST load, not
    // self-reject; per-owner floor stays put.
    let snap = adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("equal-serial reload loads");
    assert_eq!(p.per_owner_floor(&alpha()), Some(Serial(5)));
    assert!(snap.zones.contains_key(&bus_zone()));
}

#[test]
fn floor_equal_edited_records_without_bump_still_resolves_floor_unchanged() {
    let mut p = InMemoryPersistence::new();
    let s1 = adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("seed");
    let s2 = adopt(&bus_min(5, "10.0.0.9"), &mut p, Some(s1)).expect("edited-no-bump still loads");
    // Content changed at the same serial — accepted, served, floor put.
    let rd: Vec<_> = a_rrset(&s2).rdatas.iter().cloned().collect();
    assert_eq!(rd, vec![cosmix_dns::RData::A("10.0.0.9".parse().unwrap())]);
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(5)),
        "floor not advanced"
    );
}

#[test]
fn floor_greater_adopts_and_advances_and_persists() {
    let mut p = InMemoryPersistence::new();
    let s1 = adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("seed");
    adopt(&bus_min(6, "10.0.0.2"), &mut p, Some(s1)).expect("greater adopts");
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(6)),
        "floor advanced + persisted"
    );
}

/// One owner (`alpha`) appearing in TWO zones. The per-owner replay
/// floor is keyed by owner *globally* (§11 rule 5), so the owner must
/// carry **one** serial across all its bundles. Divergent serials are
/// rejected at build (`OwnerSerialConflict`); a uniform serial adopts
/// and — the cardinal v0 invariant — reloads unchanged without
/// self-rejecting. (Codex round-2 caught that aggregating to the max
/// only masked this: adopt floor 9, then the serial-4 zone self-bricks
/// on an unchanged restart.)
fn two_zone_same_owner(bus_serial: u32, example_serial: u32) -> String {
    format!(
        r#"zones: {{
  "bus": {{
    soa: {{ primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 }},
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      {{ owner: "alpha", serial: {bus_serial}, records: [
          {{ name: "alpha.bus", type: "A", ttl: 300, data: "192.0.2.5" }}
      ] }}
    ]
  }},
  "example.com": {{
    soa: {{ primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 }},
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      {{ owner: "alpha", serial: {example_serial}, records: [
          {{ name: "alpha.example.com", type: "A", ttl: 300, data: "192.0.2.5" }}
      ] }}
    ]
  }}
}}
"#
    )
}

#[test]
fn divergent_per_owner_serials_across_zones_rejected_at_build() {
    // bus=9, example.com=4 for the same owner → incoherent (one owner
    // ⇒ one global floor). Rejected BEFORE the floor gate so no
    // partial state is persisted, in either zone-iteration order.
    let mut p = InMemoryPersistence::new();
    let e = adopt(&two_zone_same_owner(9, 4), &mut p, None).unwrap_err();
    assert!(
        matches!(e, RejectReason::OwnerSerialConflict { .. }),
        "got {e:?}"
    );
    let mut p2 = InMemoryPersistence::new();
    let e2 = adopt(&two_zone_same_owner(3, 8), &mut p2, None).unwrap_err();
    assert!(
        matches!(e2, RejectReason::OwnerSerialConflict { .. }),
        "got {e2:?}"
    );
    // Nothing persisted on a rejected candidate.
    assert_eq!(
        p.per_owner_floor(&alpha()),
        None,
        "no floor written on reject"
    );
}

#[test]
fn uniform_serial_owner_across_zones_adopts_and_reloads_unchanged() {
    // The cardinal v0 invariant Codex's round-2 finding demands: an
    // owner spanning multiple zones at ONE serial adopts, sets a
    // single global floor, and an UNCHANGED restart (same serial,
    // Equal per-owner) re-loads without self-rejecting either zone.
    let mut p = InMemoryPersistence::new();
    let s1 = adopt(&two_zone_same_owner(7, 7), &mut p, None).expect("uniform adopt");
    assert!(s1.zones.contains_key(&bus_zone()));
    assert!(
        s1.zones
            .contains_key(&ZoneName(Name::parse("example.com").unwrap()))
    );
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(7)),
        "one owner ⇒ one global floor"
    );
    // Unchanged restart: same file, Equal per-owner → MUST load.
    let s2 =
        adopt(&two_zone_same_owner(7, 7), &mut p, Some(s1)).expect("unchanged restart reloads");
    assert!(s2.zones.contains_key(&bus_zone()));
    assert!(
        s2.zones
            .contains_key(&ZoneName(Name::parse("example.com").unwrap()))
    );
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(7)),
        "Equal: floor not advanced"
    );
}

// ── emitted-SOA serial: monotone tick on EVERY adopted reload (Q2) ────

#[test]
fn emitted_serial_ticks_on_every_adopted_reload_including_unchanged() {
    let mut p = InMemoryPersistence::new();
    let s1 = adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("adopt 1");
    let e1 = s1.zones[&bus_zone()].emitted_serial;
    // base = max(persisted=None→cfg=1, cfg=1) = 1 → successor = 2.
    assert_eq!(e1, Serial(2));
    assert!(e1.0 >= 1, "emitted >= configured_serial_floor");
    assert_eq!(p.zone_emitted_floor(&bus_zone()), Some(Serial(2)));

    // Unchanged-content reload (Equal per-owner) STILL ticks the
    // emitted serial — the deliberate opposite of a content-delta
    // model (pins the Q2 decision). Per-owner floor stays at 5.
    let s2 = adopt(&bus_min(5, "10.0.0.1"), &mut p, Some(s1)).expect("adopt 2");
    let e2 = s2.zones[&bus_zone()].emitted_serial;
    assert_eq!(e2, Serial(3), "every adopted reload bumps");
    assert_eq!(
        p.per_owner_floor(&alpha()),
        Some(Serial(5)),
        "per-owner floor decoupled"
    );
    assert_eq!(p.zone_emitted_floor(&bus_zone()), Some(Serial(3)));

    // A rejected reload does NOT bump and does NOT regress.
    let _ = adopt(&bus_min(4, "10.0.0.1"), &mut p, Some(s2)).unwrap_err();
    assert_eq!(
        p.zone_emitted_floor(&bus_zone()),
        Some(Serial(3)),
        "rejected reload no bump"
    );
}

#[test]
fn emitted_serial_survives_persistence_round_trip_no_regress() {
    let dir = tempfile::tempdir().unwrap();
    let spath = dir.path().join("state");
    {
        let StateLoad::Ok(mut fp) = FilePersistence::open(&spath) else {
            panic!("fresh state Ok");
        };
        let s = adopt(&bus_min(5, "10.0.0.1"), &mut fp, None).expect("adopt");
        assert_eq!(s.zones[&bus_zone()].emitted_serial, Serial(2));
    }
    // Restart/partition simulation: reopen, the floor must not regress.
    let StateLoad::Ok(mut fp2) = FilePersistence::open(&spath) else {
        panic!("reopen Ok");
    };
    assert_eq!(
        fp2.zone_emitted_floor(&bus_zone()),
        Some(Serial(2)),
        "no regress across restart"
    );
    let s2 = adopt(&bus_min(5, "10.0.0.1"), &mut fp2, None).expect("adopt post-restart");
    assert_eq!(
        s2.zones[&bus_zone()].emitted_serial,
        Serial(3),
        "monotone across restart"
    );
}

// ── fallible persistence (MAJOR) — non-2PC, no-regress NOT no-advance ─

/// A `Persistence` wrapper whose mutators fail when the shared flag is
/// set — lets a `StaticZoneStore` keep last-known-good through an
/// injected fsync/write failure on `reload()`.
struct Toggle {
    inner: InMemoryPersistence,
    fail: Arc<AtomicBool>,
}
impl Persistence for Toggle {
    fn per_owner_floor(&self, o: &OwnerId) -> Option<Serial> {
        self.inner.per_owner_floor(o)
    }
    fn note_owner_serial(&mut self, o: &OwnerId, s: Serial) -> Result<(), PersistenceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PersistenceError("injected".into()));
        }
        self.inner.note_owner_serial(o, s)
    }
    fn zone_emitted_floor(&self, z: &ZoneName) -> Option<Serial> {
        self.inner.zone_emitted_floor(z)
    }
    fn bump_zone_emitted_floor(&mut self, z: &ZoneName, s: Serial) -> Result<(), PersistenceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PersistenceError("injected".into()));
        }
        self.inner.bump_zone_emitted_floor(z, s)
    }
}

#[test]
fn fallible_persistence_rejects_and_keeps_last_known_good() {
    let mut p = InMemoryPersistence::new();
    let good = adopt(&bus_min(5, "10.0.0.1"), &mut p, None).expect("good adopt");
    let owner_floor_before = p.per_owner_floor(&alpha());
    let zone_floor_before = p.zone_emitted_floor(&bus_zone());

    // Inject failure, attempt a Greater reload.
    p.set_fail_writes(true);
    let e = adopt(&bus_min(6, "10.0.0.2"), &mut p, Some(good.clone())).unwrap_err();
    assert!(matches!(e, RejectReason::Persistence(_)), "got {e:?}");

    // No floor REGRESSES (we deliberately do NOT assert "no advance":
    // the §3 non-2PC model permits a partial high-water advance).
    let of = p.per_owner_floor(&alpha());
    let zf = p.zone_emitted_floor(&bus_zone());
    assert!(
        no_regress(owner_floor_before, of),
        "per-owner floor must not regress"
    );
    assert!(
        no_regress(zone_floor_before, zf),
        "emitted floor must not regress"
    );
}

#[test]
fn static_store_reload_failure_keeps_serving_last_good() {
    let dir = tempfile::tempdir().unwrap();
    let zpath = dir.path().join("zones.mix");
    std::fs::write(&zpath, bus_min(5, "10.0.0.1")).unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let p = Toggle {
        inner: InMemoryPersistence::new(),
        fail: Arc::clone(&fail),
    };
    let store = StaticZoneStore::load_initial(zpath.clone(), Box::new(p)).expect("load_initial");
    let before = store.snapshot();

    // Rewrite to a Greater serial, inject persistence failure, reload.
    std::fs::write(&zpath, bus_min(6, "10.0.0.2")).unwrap();
    fail.store(true, Ordering::SeqCst);
    let r = store.reload();
    assert!(matches!(r, Err(RejectReason::Persistence(_))), "got {r:?}");

    // Last-known-good still served (in-zone names still resolve): the
    // served Arc is the prior one (adopt NOT visible as success).
    let after = store.snapshot();
    assert!(
        Arc::ptr_eq(&before, &after),
        "served snapshot unchanged on failed reload"
    );
}

// ── tombstone ────────────────────────────────────────────────────────

#[test]
fn withdrawn_is_representable_and_suppressed_from_answers() {
    // Representable: the parsed Layer-1 keeps the tombstone bit.
    let parsed = common::parse(common::ZONES_SRC);
    let bus = parsed.zones.get(&bus_zone()).expect("bus source");
    let has_tombstone = bus.bundles.values().any(|b| {
        b.records
            .iter()
            .any(|r| r.name.as_str() == "old.bus." && r.withdrawn)
    });
    assert!(has_tombstone, "withdrawn record must survive into Layer 1");

    // Suppressed: it is neither an existing name nor an RRset.
    let mut p = InMemoryPersistence::new();
    let snap = adopt(common::ZONES_SRC, &mut p, None).expect("adopt");
    let z = &snap.zones[&bus_zone()];
    let old = Name::parse("old.bus").unwrap();
    assert!(
        !z.existing_names.contains(&old),
        "tombstoned name absent → NXDOMAIN"
    );
    assert!(
        !z.rrsets.keys().any(|k| k.name == old),
        "tombstoned record produces no RRset"
    );
}
