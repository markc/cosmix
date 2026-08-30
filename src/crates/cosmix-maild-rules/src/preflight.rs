//! DNSBL preflight — per-classify async I/O gathered into one bounded
//! step before the synchronous rule matcher loop runs.
//!
//! The rule engine's matcher trait is intentionally synchronous and
//! CPU-only (D1 in `_doc/planned/maild-rules-phase2.md`). Pushing
//! `async fn matches` across 12 unrelated matcher kinds would force
//! every consumer to await regardless of whether they perform I/O.
//! Instead, for the one matcher kind that needs DNS (`peer_ip_in_dnsbl`),
//! we run an *async preflight* over the loaded pack before the sync
//! matcher loop: collect every `(peer_ip, zone)` pair the pack cares
//! about, resolve them in parallel against the configured
//! [`crate::dnsbl::DnsblLookup`], and hand a [`DnsblPreflightResult`]
//! to the matcher loop. The synchronous matcher looks up its zones in
//! the result map and decides match/no-match without ever awaiting.
//!
//! Failure semantics: a [`crate::dnsbl::DnsblResult::LookupFailed`]
//! verdict (transient SERVFAIL, timeout, network error) is treated
//! as no-match for the rule. The matcher matches iff *any* configured
//! zone returned `Listed`. This matches the spec's "don't poison
//! classify on a flaky DNSBL" intent — false negatives during a DNS
//! brown-out are preferable to false positives that would junk
//! legitimate mail.
//!
//! Missing zones (preflight not run, or peer IP not provided): the
//! map returns [`DnsblResult::LookupFailed`] for every lookup, so
//! `peer_ip_in_dnsbl` matchers fall through to no-match. This is the
//! same shape as "DNS unreachable" and keeps the matcher trivial.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Arc;

use futures_util::future::join_all;

use crate::compiled::{CompiledRule, RuleMatcher};
use crate::dnsbl::{DnsblLookup, DnsblResult};
use crate::types::RuleId;

/// Per-classify DNSBL verdicts. Built by [`run`] before the sync
/// matcher loop; consulted by [`crate::rules::peer_ip_in_dnsbl::PeerIpInDnsbl::matches`].
///
/// Keys are the zone strings exactly as they appear in the rule
/// pack (case-preserved); a missing key implies the preflight was
/// not run for that zone (treat as `LookupFailed`).
#[derive(Debug, Default, Clone)]
pub struct DnsblPreflightResult {
    verdicts: HashMap<String, DnsblResult>,
}

impl DnsblPreflightResult {
    /// Empty result — every lookup returns `LookupFailed` (no-match).
    /// Used when the engine has no `DnsblLookup` configured or the
    /// pack carries no `peer_ip_in_dnsbl` rules.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Look up a single zone's verdict. Unknown zones — preflight
    /// didn't run for them — return `LookupFailed` so matchers
    /// treat the absence the same as a transient DNS failure.
    pub fn verdict_for(&self, zone: &str) -> DnsblResult {
        self.verdicts
            .get(zone)
            .copied()
            .unwrap_or(DnsblResult::LookupFailed)
    }

    /// Whether the result contains any verdict at all. Only used by
    /// tests that need to assert the engine actually ran a preflight.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.verdicts.is_empty()
    }
}

/// Walk a compiled rule set and collect the union of every zone any
/// *active* `peer_ip_in_dnsbl` matcher cares about. Rules where
/// `is_disabled` returns `true` are skipped — disabled rules cannot
/// match anyway, so issuing their lookups would waste a DNS round-trip
/// and (more importantly) leak the peer IP to a third-party DNSBL
/// for a verdict the engine will discard.
///
/// Deduplicated via `BTreeSet` so the preflight issues one lookup per
/// zone regardless of how many rules in the pack reference it.
pub(crate) fn pending_zones(
    rules: &[CompiledRule],
    is_disabled: impl Fn(&RuleId) -> bool,
) -> Vec<String> {
    let mut set = BTreeSet::new();
    for rule in rules {
        if is_disabled(&rule.id) {
            continue;
        }
        if let RuleMatcher::PeerIpInDnsbl(m) = &rule.matcher {
            for z in &m.zones {
                set.insert(z.clone());
            }
        }
    }
    set.into_iter().collect()
}

/// Run the preflight: for every `zone` in `zones`, ask `lookup` whether
/// `peer_ip` is listed, in parallel. Returns a [`DnsblPreflightResult`]
/// keyed by zone string.
///
/// Concurrency: `futures_util::future::join_all` fans the lookups out
/// so a pack with N DNSBL zones sees ~max-latency-of-one-zone
/// wall-clock, not N × max-latency. The `Dnsbl<R>` wrapper already
/// does single-flight across concurrent classifications for the
/// same `(ip, zone)` pair.
pub async fn run(
    lookup: &Arc<dyn DnsblLookup>,
    peer_ip: IpAddr,
    zones: &[String],
) -> DnsblPreflightResult {
    if zones.is_empty() {
        return DnsblPreflightResult::empty();
    }
    let futures = zones.iter().map(|z| {
        let zone = z.clone();
        let lookup = Arc::clone(lookup);
        async move {
            let verdict = lookup.is_listed(peer_ip, &zone).await;
            (zone, verdict)
        }
    });
    let verdicts: HashMap<String, DnsblResult> = join_all(futures).await.into_iter().collect();
    DnsblPreflightResult { verdicts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RuleId;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingLookup {
        listed_zones: Mutex<HashMap<String, bool>>,
        calls: AtomicU64,
    }

    impl CountingLookup {
        fn new(listed: &[(&str, bool)]) -> Self {
            let mut m = HashMap::new();
            for (z, v) in listed {
                m.insert((*z).to_string(), *v);
            }
            Self {
                listed_zones: Mutex::new(m),
                calls: AtomicU64::new(0),
            }
        }
    }

    #[async_trait]
    impl DnsblLookup for CountingLookup {
        async fn is_listed(&self, _ip: IpAddr, zone: &str) -> DnsblResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.listed_zones.lock().unwrap().get(zone) {
                Some(true) => DnsblResult::Listed,
                Some(false) => DnsblResult::NotListed,
                None => DnsblResult::LookupFailed,
            }
        }
        async fn reason(&self, _ip: IpAddr, _zone: &str) -> Option<String> {
            None
        }
    }

    fn dnsbl_rule(id: &str, zones: &[&str]) -> CompiledRule {
        CompiledRule {
            id: RuleId::from(id),
            description: format!("dnsbl rule {id}"),
            weight: 5,
            matcher: RuleMatcher::PeerIpInDnsbl(crate::rules::peer_ip_in_dnsbl::PeerIpInDnsbl {
                zones: zones.iter().map(|s| (*s).to_string()).collect(),
            }),
        }
    }

    #[test]
    fn pending_zones_deduplicates_across_rules() {
        let rules = vec![
            dnsbl_rule("a", &["zen.spamhaus.org", "b.barracudacentral.org"]),
            dnsbl_rule("b", &["zen.spamhaus.org", "dnsbl.sorbs.net"]),
        ];
        let zones = pending_zones(&rules, |_| false);
        assert_eq!(zones.len(), 3);
        assert!(zones.contains(&"zen.spamhaus.org".to_string()));
        assert!(zones.contains(&"b.barracudacentral.org".to_string()));
        assert!(zones.contains(&"dnsbl.sorbs.net".to_string()));
    }

    #[test]
    fn pending_zones_empty_for_pack_without_dnsbl_rules() {
        assert!(pending_zones(&[], |_| false).is_empty());
    }

    #[test]
    fn pending_zones_excludes_disabled_rules() {
        let rules = vec![
            dnsbl_rule("active", &["zen.spamhaus.org"]),
            dnsbl_rule("disabled", &["b.barracudacentral.org", "dnsbl.sorbs.net"]),
        ];
        let disabled = RuleId::from("disabled");
        let zones = pending_zones(&rules, |id| id == &disabled);
        assert_eq!(zones, vec!["zen.spamhaus.org".to_string()]);
    }

    #[tokio::test]
    async fn run_returns_verdict_per_zone() {
        let lookup: Arc<dyn DnsblLookup> = Arc::new(CountingLookup::new(&[
            ("listed.test", true),
            ("clean.test", false),
        ]));
        let zones = vec![
            "listed.test".into(),
            "clean.test".into(),
            "broken.test".into(),
        ];
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let result = run(&lookup, ip, &zones).await;
        assert_eq!(result.verdict_for("listed.test"), DnsblResult::Listed);
        assert_eq!(result.verdict_for("clean.test"), DnsblResult::NotListed);
        assert_eq!(result.verdict_for("broken.test"), DnsblResult::LookupFailed);
        // Unknown zone — preflight didn't run for it.
        assert_eq!(
            result.verdict_for("not-asked.test"),
            DnsblResult::LookupFailed
        );
    }

    #[tokio::test]
    async fn run_with_empty_zones_does_not_call_lookup() {
        let counting = Arc::new(CountingLookup::new(&[]));
        let lookup: Arc<dyn DnsblLookup> = counting.clone();
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let result = run(&lookup, ip, &[]).await;
        assert!(result.is_empty());
        assert_eq!(counting.calls.load(Ordering::SeqCst), 0);
    }
}
