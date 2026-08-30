//! Read-only Bus surface (citizen build only) — plan §7.
//!
//! v0 exposes **no Bus writes** (writes = v1 gossip, out of scope —
//! plan §7). Exactly two read-only actions, and *only* these two:
//!
//! * `dnsd.zone.snapshot` — the assembled snapshot's **serial-excluded
//!   `config_hash`** (plan §3: daemon-owned `emitted_serial` is NOT
//!   hashed; the hand-set `configured_serial_floor` IS) plus the served
//!   zone-name set. Backs **§8 Layer 1 only** (config-drift parity:
//!   the §8 Mix script collects each node's `config_hash` and compares
//!   — string equality across the 4 hand-maintained files). §8 Layer 2
//!   is a separate real-wire `dig`-class probe, never this action.
//! * `dnsd.stats` — per-rcode response counts, esp. the `REFUSED`
//!   canary (see [`crate::stats`]).
//!
//! **Scope of `dnsd.zone.snapshot` (deliberate v0 line, not a gap):**
//! it returns `config_hash` (the parity primitive §8 Layer 1 actually
//! consumes) + the zone-name set + count — NOT every RR as JSON.
//! `ZoneSnapshot`/`ZoneAnswerState` are frozen P1 core types that
//! derive no `Serialize`; emitting the full record set would mean
//! either deriving `Serialize` onto frozen core (a core change v0
//! forbids — plan §1) or hand-mirroring the whole RR model for a
//! consumer that does not exist in v0. Full per-record export is v1
//! territory, the same boundary as writes/gossip.
//!
//! **Goal-(c)-preserving, retry-with-backoff:** if the broker is
//! unreachable at startup — or drops the connection mid-life — DNS
//! serving continues uninterrupted in its sibling task, and the Bus
//! surface keeps retrying until the broker accepts a fresh
//! registration. The shape (1s → 2s → ... → 60s, healthy-session
//! reset) mirrors `cosmix-webd::bus` verbatim so both citizens behave
//! the same against broker outages. See
//! [`_doc/planned/cosmix-dnsd-broker-reregister.md`] for the failure
//! mode this replaces (a single one-shot `connect_default` call that
//! left dnsd silently invisible to Bus after the first transient
//! noded-side error).

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_dns::ZoneStore;

use crate::stats::DnsStats;

/// rc=10 is the caller-error sentinel: `NodedClient::call` only surfaces
/// `rc >= 10` as an error, so a smaller rc would let a typo'd / a
/// write-shaped action return `Ok(...)` and mask the failure at the
/// caller. Same value + rationale as `cosmix-maild`'s Bus module so the
/// mesh-wide wire contract stays uniform.
const RC_CALLER_ERROR: u8 = 10;

/// Bus service name — SPEC-10 R6: the daemon name minus `cosmix-`
/// (`cosmix-dnsd` → `dnsd`). Must equal `citizen::BUS_SERVICE`; the
/// SPEC-10 legibility test already pins that constant to the sysusers
/// fragment, so this string is the single registration use of it.
const BUS_SERVICE: &str = "dnsd";

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A session is "healthy" once it has stayed up for this long. A
/// disconnect after a healthy session resets backoff to
/// [`INITIAL_BACKOFF`]; a disconnect before it does NOT reset — the
/// backoff continues growing from its prior value (capped at
/// [`MAX_BACKOFF`]) so a broker that accepts-then-drops can't drive
/// a tight reconnect/log loop.
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

/// Connect to the broker and run the read-only dispatch loop. Spawn
/// once at startup with `tokio::spawn(bus::run(...))`. Holds shared
/// `Arc`s to the live `ZoneStore` (for `config_hash`) and the
/// `DnsStats` the serve tasks increment.
///
/// **Retry-with-backoff governs both initial connect and mid-life
/// disconnect** (replacing the prior one-shot behaviour described in
/// [`_doc/planned/cosmix-dnsd-broker-reregister.md`]). On
/// `incoming_async` stream end we log and fall back into the connect
/// loop rather than exiting `run`; the daemon never gives up
/// re-registering as long as `bus::run` is alive. DNS serving runs
/// in a sibling task and is unaffected by broker availability —
/// goal-(c) invariant preserved.
///
/// Backoff state is preserved across reconnects. It is only reset to
/// [`INITIAL_BACKOFF`] after a session lasted at least
/// [`HEALTHY_SESSION_THRESHOLD`] — a broker that accepts registration
/// and immediately drops us therefore keeps growing the delay rather
/// than driving a tight reconnect/log loop.
pub async fn run(store: Arc<dyn ZoneStore>, stats: Arc<DnsStats>) {
    // Built ONCE so started_at is the true process start and survives the
    // reconnect loop (re-sent on every register). Version-discovery contract.
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    let mut delay = INITIAL_BACKOFF;
    loop {
        let client = connect_with_backoff(&mut delay, &prov).await;
        let session_started = Instant::now();
        let mut rx = match client.incoming_async().await {
            Some(rx) => rx,
            None => {
                tracing::error!(
                    service = BUS_SERVICE,
                    "incoming_async returned None — receiver already taken; \
                     closing client and reconnecting"
                );
                // `Drop` does not abort the reader or close the
                // socket; `close().await` does, so the broker reaps
                // the registered name immediately rather than holding
                // an orphan connection until the next inbound frame.
                client.close().await;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        while let Some(cmd) = rx.recv().await {
            let (rc, body) = dispatch(&cmd, &store, &stats);
            if let Err(e) = client.respond(&cmd, rc, &body).await {
                tracing::warn!(
                    error = %e,
                    command = %cmd.command,
                    "Bus response send failed"
                );
            }
        }
        let session_lifetime = session_started.elapsed();
        if session_lifetime >= HEALTHY_SESSION_THRESHOLD {
            delay = INITIAL_BACKOFF;
        } else {
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        tracing::warn!(
            service = BUS_SERVICE,
            session_lifetime_seconds = session_lifetime.as_secs(),
            next_retry_in_seconds = delay.as_secs(),
            "Bus incoming stream ended; reconnecting with backoff \
             (DNS serving continues — goal-(c))"
        );
        client.close().await;
        tokio::time::sleep(delay).await;
    }
}

/// Loop until `NodedClient::connect_default` succeeds, advancing the
/// caller's `delay` (1s → 60s exponential, cap at 60s) on each
/// failure. The caller owns the backoff state so it can preserve it
/// across mid-life disconnects — see [`run`] for the session-lifetime
/// heuristic that decides when to reset.
async fn connect_with_backoff(
    delay: &mut Duration,
    prov: &cosmix_bus::RegisterProvenance,
) -> NodedClient {
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(
            BUS_SERVICE,
            prov.clone(),
        )
        .await
        {
            Ok(c) => {
                tracing::info!(service = BUS_SERVICE, "registered as Bus service");
                return c;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_seconds = delay.as_secs(),
                    "broker not available; Bus surface offline until reconnect succeeds \
                     (DNS serving continues — goal-(c))"
                );
                tokio::time::sleep(*delay).await;
                *delay = (*delay * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pure request→(rc, body) router. Read-only by construction: there is
/// no mutation arm — anything that is not one of the two read actions
/// is a caller error (this includes any future write-shaped
/// `dnsd.zone.*` verb, which v0 must reject, not silently accept).
/// Split out from [`run`] so it is unit-testable without a broker.
fn dispatch(
    cmd: &IncomingCommand,
    store: &Arc<dyn ZoneStore>,
    stats: &Arc<DnsStats>,
) -> (u8, String) {
    match cmd.command.as_str() {
        "dnsd.zone.snapshot" => (0, zone_snapshot_body(store)),
        "dnsd.stats" => (0, stats.snapshot_json().to_string()),
        other => (
            RC_CALLER_ERROR,
            serde_json::json!({
                "error": "unknown or non-read-only action",
                "action": other,
                "read_only_actions": ["dnsd.zone.snapshot", "dnsd.stats"],
            })
            .to_string(),
        ),
    }
}

/// `dnsd.zone.snapshot` body. `config_hash` is serialised as a **decimal
/// string**, not a JSON number: it is a `u64` whose range exceeds the
/// IEEE-754 / JSON safe-integer ceiling (2^53), and a number would be
/// silently rounded by some consumers. §8 Layer 1 only needs *exact
/// equality* of the hash across nodes, for which lossless string
/// compare is precisely right.
fn zone_snapshot_body(store: &Arc<dyn ZoneStore>) -> String {
    let snap = store.snapshot();
    let zones: Vec<&str> = snap.zones.keys().map(|z| z.0.as_str()).collect();
    serde_json::json!({
        "config_hash": snap.config_hash.0.to_string(),
        "zone_count": zones.len(),
        "zones": zones,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_dns::ZoneSnapshot;
    use cosmix_dns::model::ZoneName;
    use cosmix_dns::snapshot::SnapshotHash;
    use std::collections::BTreeMap;

    struct FixedStore(Arc<ZoneSnapshot>);
    impl ZoneStore for FixedStore {
        fn snapshot(&self) -> Arc<ZoneSnapshot> {
            Arc::clone(&self.0)
        }
    }

    // `ZoneAnswerState` has no `Default` (a served zone always has a
    // real apex SOA/NS — model.rs). `dnsd.zone.snapshot` only reads
    // `snap.zones.keys()` + `config_hash`, never the answer state, so
    // a minimal-but-real apex with empty rrsets is sufficient and
    // honest about the type's invariant.
    fn answer_state() -> cosmix_dns::ZoneAnswerState {
        use cosmix_dns::canonical::Name;
        cosmix_dns::ZoneAnswerState {
            soa: cosmix_dns::SoaConfig {
                primary: Name::parse("ns.example.").unwrap(),
                mbox: Name::parse("hostmaster.example.").unwrap(),
                ttl: 3600,
                minimum: 300,
            },
            ns: vec![Name::parse("ns.example.").unwrap()],
            emitted_serial: cosmix_dns::Serial(1),
            rrsets: Default::default(),
            existing_names: Default::default(),
        }
    }

    fn store_with(hash: u64, zone_names: &[&str]) -> Arc<dyn ZoneStore> {
        use cosmix_dns::canonical::Name;
        let mut zones = BTreeMap::new();
        for n in zone_names {
            zones.insert(ZoneName(Name::parse(n).unwrap()), answer_state());
        }
        Arc::new(FixedStore(Arc::new(ZoneSnapshot {
            zones,
            config_hash: SnapshotHash(hash),
        })))
    }

    fn cmd(command: &str) -> IncomingCommand {
        IncomingCommand {
            from: String::new(),
            command: command.to_string(),
            id: None,
            args: serde_json::Value::Null,
            body: String::new(),
            headers: Default::default(),
        }
    }

    #[test]
    fn zone_snapshot_returns_hash_as_string_and_sorted_zone_names() {
        let store = store_with(u64::MAX, &["b.example.", "a.example."]);
        let stats = Arc::new(DnsStats::new());
        let (rc, body) = dispatch(&cmd("dnsd.zone.snapshot"), &store, &stats);
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // u64::MAX round-trips losslessly only as a string.
        assert_eq!(v["config_hash"], u64::MAX.to_string());
        assert_eq!(v["zone_count"], 2);
        // BTreeMap key order ⇒ deterministic, sorted across nodes (a
        // §8 Layer-1 precondition: the name set must not be order-noisy).
        assert_eq!(v["zones"][0], "a.example.");
        assert_eq!(v["zones"][1], "b.example.");
    }

    #[test]
    fn stats_action_returns_the_rcode_counters() {
        let store = store_with(0, &[]);
        let stats = Arc::new(DnsStats::new());
        let (rc, body) = dispatch(&cmd("dnsd.stats"), &store, &stats);
        assert_eq!(rc, 0);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["refused"], 0);
        assert_eq!(v["total"], 0);
    }

    #[test]
    fn unknown_and_write_shaped_actions_are_rejected_read_only() {
        let store = store_with(0, &[]);
        let stats = Arc::new(DnsStats::new());
        // A plausible future *write* verb must be rejected by v0, not
        // silently accepted (read-only-by-construction guard).
        for bad in [
            "dnsd.zone.set",
            "dnsd.zone.adopt",
            "dnsd.reload",
            "dnsd.bogus",
            "maild.rules.stats",
        ] {
            let (rc, body) = dispatch(&cmd(bad), &store, &stats);
            assert_eq!(rc, RC_CALLER_ERROR, "{bad} must be a caller error");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["action"], bad);
        }
    }
}
