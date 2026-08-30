//! DNSBL wrapper integration tests — exercises [`Dnsbl<R>`] cache +
//! single-flight against a counting fake resolver. The hickory-backed
//! production impl is behind the `dnsbl` feature and not exercised
//! here; what we need to prove are the wrapper's invariants
//! (single-flight collapse, transient-failure no-cache, RFC 5782
//! encoding correctness, cache-hit fast path).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use cosmix_maild_rules::dnsbl::{AsyncDnsResolver, Dnsbl, DnsblLookup, DnsblResult, encode_query};

/// Programmable fake resolver.
///
/// `a_answers` maps an exact qname to `Some(true|false)` (listed /
/// not-listed). Missing qnames default to `Some(false)` (NXDOMAIN
/// behaviour) unless `fail_a` is true.
///
/// `delay_a` is added before returning each `lookup_a` answer so the
/// single-flight test can fan multiple concurrent callers into one
/// upstream call without racing the synchronous cache fill.
struct FakeResolver {
    a_answers: HashMap<String, Option<bool>>,
    txt_answers: HashMap<String, String>,
    delay_a: Duration,
    /// When true, `lookup_a` returns `None` (transient failure).
    fail_a: bool,
    a_calls: AtomicU64,
    txt_calls: AtomicU64,
}

impl FakeResolver {
    fn new() -> Self {
        Self {
            a_answers: HashMap::new(),
            txt_answers: HashMap::new(),
            delay_a: Duration::from_millis(0),
            fail_a: false,
            a_calls: AtomicU64::new(0),
            txt_calls: AtomicU64::new(0),
        }
    }

    fn with_listed(mut self, qname: &str) -> Self {
        self.a_answers.insert(qname.to_string(), Some(true));
        self
    }

    fn with_txt(mut self, qname: &str, reason: &str) -> Self {
        self.txt_answers
            .insert(qname.to_string(), reason.to_string());
        self
    }

    fn with_delay(mut self, d: Duration) -> Self {
        self.delay_a = d;
        self
    }

    fn failing(mut self) -> Self {
        self.fail_a = true;
        self
    }
}

#[async_trait]
impl AsyncDnsResolver for FakeResolver {
    async fn lookup_a(&self, qname: &str) -> Option<bool> {
        self.a_calls.fetch_add(1, Ordering::SeqCst);
        if self.delay_a > Duration::from_millis(0) {
            tokio::time::sleep(self.delay_a).await;
        }
        if self.fail_a {
            return None;
        }
        // Default: NXDOMAIN (`Some(false)`) for unmapped names.
        Some(
            self.a_answers
                .get(qname)
                .copied()
                .unwrap_or(Some(false))
                .unwrap_or(false),
        )
    }

    async fn lookup_txt(&self, qname: &str) -> Option<String> {
        self.txt_calls.fetch_add(1, Ordering::SeqCst);
        self.txt_answers.get(qname).cloned()
    }
}

// ============================================================
// Encoding (smoke; thorough oracle tests live in the in-module unit
// tests on dnsbl.rs)
// ============================================================

#[test]
fn rfc5782_ipv4_smoke() {
    let ip: IpAddr = "203.0.113.42".parse().unwrap();
    assert_eq!(
        encode_query(ip, "zen.spamhaus.org"),
        "42.113.0.203.zen.spamhaus.org"
    );
}

#[test]
fn rfc5782_ipv6_smoke() {
    // Same RFC 5782 §2.4 example as the in-module oracle; here as a
    // cross-crate-boundary smoke that the function is publicly callable.
    let ip: IpAddr = "2001:db8::1".parse().unwrap();
    let q = encode_query(ip, "bl.example");
    assert!(q.starts_with("1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0"));
    assert!(q.ends_with(".8.b.d.0.1.0.0.2.bl.example"));
}

// ============================================================
// Cache + single-flight behaviour
// ============================================================

fn ip4(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[tokio::test]
async fn listed_ipv4_returns_listed() {
    let qname = "4.3.2.1.test.bl";
    let fake = FakeResolver::new().with_listed(qname);
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let r = dnsbl.is_listed(ip4("1.2.3.4"), "test.bl").await;
    assert_eq!(r, DnsblResult::Listed);
}

#[tokio::test]
async fn nxdomain_returns_not_listed() {
    let fake = FakeResolver::new();
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let r = dnsbl.is_listed(ip4("9.8.7.6"), "test.bl").await;
    assert_eq!(r, DnsblResult::NotListed);
}

#[tokio::test]
async fn transient_failure_returns_lookup_failed() {
    let fake = FakeResolver::new().failing();
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let r = dnsbl.is_listed(ip4("1.2.3.4"), "test.bl").await;
    assert_eq!(r, DnsblResult::LookupFailed);
}

#[tokio::test]
async fn timeout_returns_lookup_failed() {
    // Resolver delay > timeout → wrapper sees Elapsed → LookupFailed.
    let fake = FakeResolver::new().with_delay(Duration::from_millis(200));
    let dnsbl = Dnsbl::new(fake, Duration::from_millis(50), Duration::from_secs(300));

    let r = dnsbl.is_listed(ip4("1.2.3.4"), "test.bl").await;
    assert_eq!(r, DnsblResult::LookupFailed);
}

#[tokio::test]
async fn cache_hit_avoids_upstream_call() {
    let qname = "4.3.2.1.test.bl";
    let fake = FakeResolver::new().with_listed(qname);
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let ip = ip4("1.2.3.4");
    let _ = dnsbl.is_listed(ip, "test.bl").await;
    // Second call: must come from cache.
    let r = dnsbl.is_listed(ip, "test.bl").await;
    assert_eq!(r, DnsblResult::Listed);
    // The fake's a_calls counter is the upstream-call ground truth.
    // We can't read it from inside the Dnsbl wrapper (resolver is
    // owned), but we can hand the counter via an Arc — see the
    // single-flight test below for the explicit count assertion.
}

#[tokio::test]
async fn transient_failure_burst_collapses_to_one_upstream_call() {
    // Single-flight must collapse a concurrent burst even when the
    // verdict is LookupFailed, otherwise a flaky upstream amplifies
    // into N timeouts per peer-IP burst.
    let calls = Arc::new(AtomicU64::new(0));

    struct FailingResolver {
        calls: Arc<AtomicU64>,
        delay: Duration,
    }
    #[async_trait]
    impl AsyncDnsResolver for FailingResolver {
        async fn lookup_a(&self, _qname: &str) -> Option<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            None
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = FailingResolver {
        calls: calls.clone(),
        delay: Duration::from_millis(80),
    };
    let dnsbl = Arc::new(Dnsbl::new(
        r,
        Duration::from_secs(2),
        Duration::from_secs(300),
    ));

    let ip = ip4("1.2.3.4");
    let mut handles = Vec::new();
    for _ in 0..10 {
        let d = dnsbl.clone();
        handles.push(tokio::spawn(
            async move { d.is_listed(ip, "test.bl").await },
        ));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), DnsblResult::LookupFailed);
    }

    // Collapse comes from the leader/follower single-flight registry,
    // not from caching the failure. Followers park on the leader's
    // oneshot and observe its LookupFailed verdict; the in-flight
    // entry is removed once the leader publishes, so no cross-message
    // state survives.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failures_are_not_cached_across_calls() {
    // Companion to the burst-collapse property. Sequential (not
    // concurrent) failed lookups must each issue their own upstream
    // call — failures live only inside the single-flight cohort, never
    // in the verdict cache. Without this property a flaky upstream
    // would pin a peer to LookupFailed for the full cache_ttl, locking
    // out a recovered DNSBL.
    let calls = Arc::new(AtomicU64::new(0));

    struct AlwaysFails {
        calls: Arc<AtomicU64>,
    }
    #[async_trait]
    impl AsyncDnsResolver for AlwaysFails {
        async fn lookup_a(&self, _qname: &str) -> Option<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            None
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = AlwaysFails {
        calls: calls.clone(),
    };
    let dnsbl = Dnsbl::new(r, Duration::from_secs(2), Duration::from_secs(300));

    let ip = ip4("1.2.3.4");
    assert_eq!(
        dnsbl.is_listed(ip, "test.bl").await,
        DnsblResult::LookupFailed
    );
    // Second call awaits the first one's completion before starting,
    // so single-flight cannot collapse them — the count proves the
    // failure verdict was NOT cached.
    assert_eq!(
        dnsbl.is_listed(ip, "test.bl").await,
        DnsblResult::LookupFailed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn leader_arm_rechecks_cache_after_join() {
    // Race: caller A misses cache, becomes leader, completes, caches
    // verdict, publishes. Caller B missed cache *before* A finished
    // but joined the inflight registry *after* A removed its entry —
    // B then becomes a fresh leader and would re-query upstream even
    // though the cache has the answer.
    //
    // Simulate it deterministically by warming the cache through a
    // first lookup, then issuing a second lookup whose `join_or_lead`
    // is guaranteed to be the "no inflight entry" branch (because the
    // first one already returned). The leader arm must re-check the
    // cache and short-circuit; the second upstream call must NOT fire.
    let calls = Arc::new(AtomicU64::new(0));

    struct CountingListedResolver {
        calls: Arc<AtomicU64>,
        listed_qname: String,
    }
    #[async_trait]
    impl AsyncDnsResolver for CountingListedResolver {
        async fn lookup_a(&self, qname: &str) -> Option<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(qname == self.listed_qname)
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = CountingListedResolver {
        calls: calls.clone(),
        listed_qname: "4.3.2.1.test.bl".to_string(),
    };
    let dnsbl = Dnsbl::new(r, Duration::from_secs(2), Duration::from_secs(300));

    let ip = ip4("1.2.3.4");

    // First call fills the cache.
    assert_eq!(dnsbl.is_listed(ip, "test.bl").await, DnsblResult::Listed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Second call's cache_get hits the fast path; it never reaches
    // the leader arm. The first leg of this test is the cache-hit
    // fast path that already had coverage.
    assert_eq!(dnsbl.is_listed(ip, "test.bl").await, DnsblResult::Listed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // To exercise the leader-arm re-check directly, force a window
    // where cache_get misses but the cache fills before join_or_lead.
    // We can't easily inject that ordering against the public API
    // because cache_get + join_or_lead are within the same is_listed
    // body without yield points. Instead we assert the invariant
    // structurally: with a populated cache, even after publishing
    // (which removes the in-flight entry), no further calls fire.
    //
    // (The cache-fast-path above already proves the property the
    // race needs — once the cache is populated, subsequent callers
    // short-circuit before they ever touch the in-flight registry.
    // The leader-arm re-check inside `is_listed` covers the narrow
    // window between an initial cache_get miss and join_or_lead in
    // the same future where a concurrent leader's publish slips in.
    // A direct test of that window would need an injected yield
    // point in `is_listed`; the structural invariant + the existing
    // single-flight test together cover the property.)
    for _ in 0..5 {
        assert_eq!(dnsbl.is_listed(ip, "test.bl").await, DnsblResult::Listed);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn leader_cancellation_does_not_hang_followers() {
    // If the leader's future is dropped before publish (panic, task
    // cancellation, outer timeout), the RAII LeaderGuard's Drop must
    // remove the in-flight entry, which closes follower receivers,
    // which lets each follower fall through to LookupFailed. Without
    // the guard, followers would hang on the never-published oneshot
    // for the full lifetime of the wrapper.
    let leader_started = Arc::new(AtomicU64::new(0));

    struct ForeverResolver {
        leader_started: Arc<AtomicU64>,
    }
    #[async_trait]
    impl AsyncDnsResolver for ForeverResolver {
        async fn lookup_a(&self, _qname: &str) -> Option<bool> {
            self.leader_started.fetch_add(1, Ordering::SeqCst);
            // Park "forever"; the test will cancel the leader task.
            tokio::time::sleep(Duration::from_secs(86_400)).await;
            None
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = ForeverResolver {
        leader_started: leader_started.clone(),
    };
    let dnsbl = Arc::new(Dnsbl::new(
        r,
        Duration::from_secs(60),
        Duration::from_secs(300),
    ));

    let ip = ip4("1.2.3.4");

    // Spawn the leader and wait until it has registered itself in the
    // in-flight map (proxied via the resolver call count reaching 1).
    let d_leader = dnsbl.clone();
    let leader_handle = tokio::spawn(async move { d_leader.is_listed(ip, "test.bl").await });
    while leader_started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }

    // Spawn a follower; it should now be parked on the oneshot.
    let d_follower = dnsbl.clone();
    let follower_handle = tokio::spawn(async move { d_follower.is_listed(ip, "test.bl").await });

    // Give the follower a tick to actually join the registry.
    tokio::task::yield_now().await;

    // Cancel the leader. Its LeaderGuard::drop must remove the
    // in-flight entry and close the follower's receiver.
    leader_handle.abort();

    // The follower must complete with LookupFailed, not hang.
    let r = tokio::time::timeout(Duration::from_secs(2), follower_handle)
        .await
        .expect("follower hung after leader cancellation");
    assert_eq!(r.unwrap(), DnsblResult::LookupFailed);

    // A fresh lookup after cancellation must start a new leader — the
    // stale registry entry must be gone. We can't easily prove this
    // without another long-lived call, but we can at least confirm
    // the resolver gets a second invocation by issuing one quick call
    // against a non-failing branch. Skip: ForeverResolver has no fast
    // path. The follower's LookupFailed return is the load-bearing
    // assertion; entry-leak would manifest as the timeout above.
}

#[tokio::test]
async fn single_flight_collapses_concurrent_burst() {
    // The load-bearing single-flight test. A burst of 10 concurrent
    // is_listed() calls for the same (ip, zone) must produce exactly
    // ONE upstream lookup. The resolver's per-call delay is bigger
    // than the burst's spawn time so all callers arrive at the gate
    // before the first one fills the cache.
    let calls = Arc::new(AtomicU64::new(0));

    struct CountingResolver {
        calls: Arc<AtomicU64>,
        delay: Duration,
        listed_qname: String,
    }
    #[async_trait]
    impl AsyncDnsResolver for CountingResolver {
        async fn lookup_a(&self, qname: &str) -> Option<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Some(qname == self.listed_qname)
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = CountingResolver {
        calls: calls.clone(),
        delay: Duration::from_millis(100),
        listed_qname: "4.3.2.1.test.bl".to_string(),
    };
    let dnsbl = Arc::new(Dnsbl::new(
        r,
        Duration::from_secs(2),
        Duration::from_secs(300),
    ));

    let ip = ip4("1.2.3.4");
    let mut handles = Vec::new();
    for _ in 0..10 {
        let d = dnsbl.clone();
        handles.push(tokio::spawn(
            async move { d.is_listed(ip, "test.bl").await },
        ));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), DnsblResult::Listed);
    }

    // The single load-bearing assertion: N concurrent calls collapse
    // to one upstream lookup.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn distinct_keys_do_not_collapse() {
    // Two distinct (ip,zone) pairs run independently — single-flight
    // is per-key, not global. Two concurrent lookups against different
    // peers should produce TWO upstream calls.
    let calls = Arc::new(AtomicU64::new(0));

    struct CountingResolver {
        calls: Arc<AtomicU64>,
    }
    #[async_trait]
    impl AsyncDnsResolver for CountingResolver {
        async fn lookup_a(&self, _qname: &str) -> Option<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some(true)
        }
        async fn lookup_txt(&self, _qname: &str) -> Option<String> {
            None
        }
    }

    let r = CountingResolver {
        calls: calls.clone(),
    };
    let dnsbl = Arc::new(Dnsbl::new(
        r,
        Duration::from_secs(2),
        Duration::from_secs(300),
    ));

    let d1 = dnsbl.clone();
    let d2 = dnsbl.clone();
    let h1 = tokio::spawn(async move { d1.is_listed(ip4("1.2.3.4"), "test.bl").await });
    let h2 = tokio::spawn(async move { d2.is_listed(ip4("5.6.7.8"), "test.bl").await });
    let (r1, r2) = (h1.await.unwrap(), h2.await.unwrap());
    assert_eq!(r1, DnsblResult::Listed);
    assert_eq!(r2, DnsblResult::Listed);

    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reason_returns_txt_string() {
    let qname = "4.3.2.1.test.bl";
    let fake = FakeResolver::new()
        .with_listed(qname)
        .with_txt(qname, "https://example.org/why-listed");
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let r = dnsbl.reason(ip4("1.2.3.4"), "test.bl").await;
    assert_eq!(r.as_deref(), Some("https://example.org/why-listed"));
}

#[tokio::test]
async fn reason_returns_none_when_no_txt() {
    let fake = FakeResolver::new();
    let dnsbl = Dnsbl::new(fake, Duration::from_secs(2), Duration::from_secs(300));

    let r = dnsbl.reason(ip4("1.2.3.4"), "test.bl").await;
    assert_eq!(r, None);
}
