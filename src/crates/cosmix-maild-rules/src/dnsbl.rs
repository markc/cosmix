//! DNSBL lookup — RFC 5782 reverse-IP query against a zone, with a
//! per-(ip,zone) cache for cacheable verdicts and a leader/follower
//! single-flight gate so a burst of mail from the same peer fans into
//! one upstream lookup *even when the upstream is failing*.
//!
//! Two layers stack:
//!
//! - [`AsyncDnsResolver`] is the low-level "do one A/TXT lookup"
//!   seam, mockable in tests with a counting fake.
//! - [`Dnsbl<R>`] composes cache + single-flight on top of any
//!   `AsyncDnsResolver` and implements the public [`DnsblLookup`]
//!   trait the rules engine consumes.
//!
//! The production path is `Dnsbl<HickoryResolver>` (gated behind the
//! `dnsbl` feature). Tests assert single-flight against
//! `Dnsbl<FakeResolver>` so the upstream-call counter is observable.
//!
//! Semantics per RFC 5782:
//!
//! - IPv4 `a.b.c.d` against `zone` → query `d.c.b.a.zone`, A-record.
//!   Any A response = [`DnsblResult::Listed`]; NXDOMAIN or NODATA =
//!   [`DnsblResult::NotListed`]; SERVFAIL / timeout / network error =
//!   [`DnsblResult::LookupFailed`].
//! - IPv6 → 32 hex nibbles in reverse order, dotted, then `.zone`.
//! - TXT (reason) is fetched only by the `explain()` path; `classify()`
//!   never touches TXT.
//!
//! Cache contract:
//!
//! - `Listed` and `NotListed` verdicts are cached for `cache_ttl` so
//!   subsequent messages from the same peer reuse the verdict.
//! - `LookupFailed` is **not cached** across messages — the plan's
//!   "retry next message" intent. Burst collapse for failures comes
//!   from the leader/follower single-flight, not from caching.
//! - The single-flight registry is in-flight only: leaders broadcast
//!   their verdict to all parked followers via a oneshot, then the
//!   entry is removed. There is no cross-message state held for
//!   `LookupFailed`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::oneshot;

/// Three-state membership result returned by [`DnsblLookup::is_listed`].
///
/// `LookupFailed` is *transient* and is never cached across messages —
/// a flaky DNSBL upstream must not pin a peer's verdict to "not listed"
/// for the negative TTL. Callers treat `LookupFailed` as
/// no-match-this-message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsblResult {
    Listed,
    NotListed,
    LookupFailed,
}

/// Public interface the rule engine uses. The implementor decides how
/// to cache, single-flight, and time-bound; the engine just asks
/// "is this peer IP listed in this zone."
#[async_trait]
pub trait DnsblLookup: Send + Sync {
    async fn is_listed(&self, ip: IpAddr, zone: &str) -> DnsblResult;

    /// Optional TXT reason for a listed entry. Lazy — only called by
    /// the `explain()` path. Returns `None` for not-listed / failed /
    /// no-TXT.
    async fn reason(&self, ip: IpAddr, zone: &str) -> Option<String>;
}

/// Lower-level "perform one A/TXT lookup" seam. `Dnsbl<R>` composes
/// cache + single-flight on top; tests replace `R` with a counting
/// fake to assert the single-flight collapse property.
#[async_trait]
pub trait AsyncDnsResolver: Send + Sync {
    /// `Some(true)` = at least one A record; `Some(false)` = NXDOMAIN
    /// or NODATA; `None` = transient failure (network, SERVFAIL,
    /// truncation, etc.).
    async fn lookup_a(&self, qname: &str) -> Option<bool>;

    /// First TXT record string, if any. Network failure → `None`.
    async fn lookup_txt(&self, qname: &str) -> Option<String>;
}

/// Encode an IP + zone per RFC 5782 — reverse octets (v4) or nibbles
/// (v6), dot-separated, suffixed with the zone.
///
/// Examples:
///
/// - `(1.2.3.4, "zen.spamhaus.org")` → `"4.3.2.1.zen.spamhaus.org"`
/// - `(2001:db8::1, "blackhole.example")` →
///   `"1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.blackhole.example"`
pub fn encode_query(ip: IpAddr, zone: &str) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            format!("{d}.{c}.{b}.{a}.{zone}")
        }
        IpAddr::V6(v6) => {
            let mut buf = String::with_capacity(64 + zone.len() + 1);
            let segments = v6.segments();
            // 8 u16 segments × 4 hex nibbles = 32 nibbles total.
            let mut nibbles: [char; 32] = ['0'; 32];
            for (i, seg) in segments.iter().enumerate() {
                let s = format!("{seg:04x}");
                for (j, ch) in s.chars().enumerate() {
                    nibbles[i * 4 + j] = ch;
                }
            }
            for (i, ch) in nibbles.iter().rev().enumerate() {
                if i > 0 {
                    buf.push('.');
                }
                buf.push(*ch);
            }
            buf.push('.');
            buf.push_str(zone);
            buf
        }
    }
}

/// Hard cap on the verdict cache. When exceeded after an expired-entry
/// sweep, `cache_put` evicts the oldest entry before inserting. With
/// realistic SMTP peer-IP cardinality the cap is rarely reached; it
/// exists to bound memory under adversarial / scanner traffic.
const CACHE_MAX_ENTRIES: usize = 2048;

type Key = (IpAddr, String);

/// Cache + leader/follower single-flight wrapper. Generic in `R` so
/// production wires to [`HickoryResolver`] and tests wire to a
/// counting fake.
pub struct Dnsbl<R: AsyncDnsResolver> {
    resolver: R,
    timeout: Duration,
    /// Cache TTL for both `Listed` and `NotListed` verdicts. The
    /// public DNSBLs cap TTL on their A records, so even on a stale
    /// `Listed` we'd retry within a sane window; making positives
    /// and negatives share one knob keeps the cache shape trivial.
    /// (The plan reserves a `positive TTL = DNS TTL` refinement for
    /// later if operators ask; v2 keeps one knob.)
    cache_ttl: Duration,
    cache: Mutex<HashMap<Key, (DnsblResult, Instant)>>,
    /// In-flight registry. Present iff a leader is currently
    /// performing a lookup for that key. Followers push a oneshot
    /// sender so the leader can broadcast its verdict. The entry is
    /// removed by the leader when the lookup completes; there is no
    /// cross-message state held here.
    inflight: Mutex<HashMap<Key, Vec<oneshot::Sender<DnsblResult>>>>,
}

impl<R: AsyncDnsResolver> Dnsbl<R> {
    /// Construct a wrapper. `timeout` bounds each individual lookup;
    /// `cache_ttl` is how long both `Listed` and `NotListed` verdicts
    /// stick. `LookupFailed` is never cached across messages; bursts
    /// of failures from the same peer collapse via single-flight.
    pub fn new(resolver: R, timeout: Duration, cache_ttl: Duration) -> Self {
        Self {
            resolver,
            timeout,
            cache_ttl,
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    fn cache_get(&self, key: &Key) -> Option<DnsblResult> {
        let cache = self.cache.lock().unwrap();
        let (verdict, at) = cache.get(key)?;
        (at.elapsed() < self.cache_ttl).then_some(*verdict)
    }

    /// Cache success / not-listed verdicts. `LookupFailed` is dropped
    /// on the floor — the no-cross-message-cache contract.
    ///
    /// Eviction discipline: on every put we sweep expired entries
    /// (cheap for small maps), then if still over `CACHE_MAX_ENTRIES`
    /// evict the oldest entry. This is the real hard cap; absent it,
    /// adversarial traffic with high unique-key cardinality would
    /// grow the cache without bound up to `cache_ttl`-worth of keys.
    fn cache_put(&self, key: &Key, verdict: DnsblResult) {
        if matches!(verdict, DnsblResult::LookupFailed) {
            return;
        }
        let mut cache = self.cache.lock().unwrap();
        let now = Instant::now();
        let cache_ttl = self.cache_ttl;
        cache.retain(|_, (_, at)| now.duration_since(*at) < cache_ttl);
        if cache.len() >= CACHE_MAX_ENTRIES
            && let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
        cache.insert(key.clone(), (verdict, now));
    }

    /// Attempt to register as the leader for `key`. Two outcomes:
    ///
    /// - `JoinResult::Leader(guard)` = we're the leader. Caller is
    ///   responsible for performing the lookup and calling
    ///   [`LeaderGuard::publish`] with the result; followers wake on
    ///   the broadcast verdict.
    /// - `JoinResult::Follower(rx)` = a leader is already in flight
    ///   for this key. Caller awaits `rx` and uses the broadcast
    ///   verdict, or treats a closed channel (`Err`) as
    ///   `LookupFailed` — which happens iff the leader's
    ///   `LeaderGuard` was dropped without publish (panic, task
    ///   cancellation), at which point Drop removes the in-flight
    ///   entry and the senders are released.
    fn join_or_lead(&self, key: Key) -> JoinResult<'_, R> {
        let mut map = self.inflight.lock().unwrap();
        if let Some(senders) = map.get_mut(&key) {
            let (tx, rx) = oneshot::channel();
            senders.push(tx);
            JoinResult::Follower(rx)
        } else {
            map.insert(key.clone(), Vec::new());
            JoinResult::Leader(LeaderGuard {
                dnsbl: self,
                key,
                armed: true,
            })
        }
    }
}

/// Leader / follower disambiguator for [`Dnsbl::join_or_lead`].
enum JoinResult<'a, R: AsyncDnsResolver> {
    Leader(LeaderGuard<'a, R>),
    Follower(oneshot::Receiver<DnsblResult>),
}

/// RAII guard the leader holds while performing its lookup.
///
/// On normal completion the leader calls [`Self::publish`] to remove
/// the in-flight entry and broadcast its verdict to every parked
/// follower. If the guard is dropped without publish (panic, task
/// cancellation, an early `?`-return after a future commit-2 outer
/// timeout fires), [`Drop`] removes the in-flight entry; the
/// `Vec<oneshot::Sender<_>>` is freed, every follower's `rx.await`
/// returns `Err`, and `is_listed`'s `unwrap_or(DnsblResult::LookupFailed)`
/// correctly converts the channel-closed to a transient-failure
/// verdict. No registry leak, no follower hang.
struct LeaderGuard<'a, R: AsyncDnsResolver> {
    dnsbl: &'a Dnsbl<R>,
    key: Key,
    /// Set to `false` by `publish`; when `false`, `Drop` skips its
    /// cleanup because the in-flight entry has already been removed
    /// and the senders consumed.
    armed: bool,
}

impl<R: AsyncDnsResolver> LeaderGuard<'_, R> {
    /// Remove the in-flight entry and broadcast `verdict` to every
    /// follower. Consumes the guard so `Drop` becomes a no-op.
    fn publish(mut self, verdict: DnsblResult) {
        let senders = {
            let mut map = self.dnsbl.inflight.lock().unwrap();
            map.remove(&self.key).unwrap_or_default()
        };
        self.armed = false;
        for tx in senders {
            // Receiver may have been dropped (caller went away); ignore.
            let _ = tx.send(verdict);
        }
    }
}

impl<R: AsyncDnsResolver> Drop for LeaderGuard<'_, R> {
    fn drop(&mut self) {
        if self.armed {
            // Leader was cancelled or panicked before publish.
            // Removing the entry drops the `Vec<oneshot::Sender<_>>`,
            // which closes every follower's receiver. Followers'
            // `rx.await` returns `Err`, which `is_listed` maps to
            // `LookupFailed`. The next message for this key will find
            // no in-flight entry and start a fresh leader — matching
            // the plan's "retry next message" contract for failures.
            let mut map = self.dnsbl.inflight.lock().unwrap();
            map.remove(&self.key);
        }
    }
}

#[async_trait]
impl<R: AsyncDnsResolver + 'static> DnsblLookup for Dnsbl<R> {
    async fn is_listed(&self, ip: IpAddr, zone: &str) -> DnsblResult {
        let key: Key = (ip, zone.to_string());

        // Fast path: cache hit. Note `LookupFailed` is never present
        // here — failures are not cached across messages.
        if let Some(verdict) = self.cache_get(&key) {
            return verdict;
        }

        // Either become the leader (RAII guard ensures cleanup even
        // on cancellation/panic) or park as a follower (oneshot
        // receiver; a closed channel maps to LookupFailed).
        match self.join_or_lead(key.clone()) {
            JoinResult::Follower(rx) => {
                // Leader dropping without publish closes the senders,
                // which closes our receiver — treat as LookupFailed so
                // we never block forever and the caller falls through
                // to the per-message "no DNSBL verdict" branch.
                rx.await.unwrap_or(DnsblResult::LookupFailed)
            }
            JoinResult::Leader(guard) => {
                // Re-check the cache before starting the upstream
                // lookup. Race: a previous leader may have completed
                // and cached its verdict in the window between our
                // initial cache miss and our `join_or_lead`. Without
                // this check we'd issue a redundant upstream call
                // even though the cache now has the answer. Publish
                // the cached verdict to drain any followers that
                // joined behind us.
                if let Some(verdict) = self.cache_get(&key) {
                    guard.publish(verdict);
                    return verdict;
                }

                let qname = encode_query(ip, zone);
                let verdict = match tokio::time::timeout(
                    self.timeout,
                    self.resolver.lookup_a(&qname),
                )
                .await
                {
                    Err(_) => DnsblResult::LookupFailed,
                    Ok(None) => DnsblResult::LookupFailed,
                    Ok(Some(true)) => DnsblResult::Listed,
                    Ok(Some(false)) => DnsblResult::NotListed,
                };

                // Cache Listed/NotListed only; failures don't persist
                // beyond the in-flight cohort.
                self.cache_put(&key, verdict);

                // Consume the guard: remove the in-flight entry and
                // broadcast to followers.
                guard.publish(verdict);

                verdict
            }
        }
    }

    async fn reason(&self, ip: IpAddr, zone: &str) -> Option<String> {
        let qname = encode_query(ip, zone);
        tokio::time::timeout(self.timeout, self.resolver.lookup_txt(&qname))
            .await
            .unwrap_or_default()
    }
}

// ============================================================
// Production resolver: hickory-resolver
// ============================================================

/// Hickory-backed [`AsyncDnsResolver`]. System resolver by default
/// (reads `/etc/resolv.conf`); custom upstreams land in Phase 2
/// commit 2 once the `EngineConfig` substrate amendment carries the
/// `dnsbl_resolver_addresses` field.
#[cfg(feature = "dnsbl")]
pub struct HickoryResolver {
    inner: hickory_resolver::TokioResolver,
}

#[cfg(feature = "dnsbl")]
impl HickoryResolver {
    /// Build from `/etc/resolv.conf`. Mirrors the construction path
    /// used by `cosmix-maild::smtp::delivery` and `cosmix-webd::mxresolve`
    /// so the workspace shares one hickory builder shape.
    pub fn from_system() -> crate::error::Result<Self> {
        let inner = hickory_resolver::Resolver::builder_tokio()
            .map_err(|e| crate::error::Error::Internal(format!("DNSBL resolver init failed: {e}")))?
            .build();
        Ok(Self { inner })
    }

    /// Build from an explicit set of resolver `ip:port` socket addrs
    /// — typically populated from the `dnsbl_resolver_addresses` field
    /// of `EngineConfig` (substrate amendment v0.2.0). Empty slice
    /// returns the system-resolver shape so callers do not need to
    /// branch on emptiness. Mirrors the multi-protocol pattern in
    /// `cosmix-maild-auth::resolver` — UDP first, TCP fallback for
    /// responses that exceed UDP MTU.
    ///
    /// The transport enum is `proto::xfer::Protocol`, NOT the
    /// `config::ProtocolConfig` that `cosmix-maild-auth` uses: this crate
    /// pins hickory-resolver 0.25 (see its manifest — deliberately, to keep
    /// one 0.25.x in the workspace graph), while maild-auth reaches
    /// hickory 0.26-alpha through `mail_auth`'s re-export, and 0.26 renamed
    /// and moved that type. Copying maild-auth's import here is what rotted
    /// this function; it did not compile between then and 2026-08-20
    /// because `dnsbl` is an optional feature and nothing built it.
    pub fn with_resolvers(addrs: &[std::net::SocketAddr]) -> crate::error::Result<Self> {
        use hickory_resolver::config::{NameServerConfig, NameServerConfigGroup, ResolverConfig};
        use hickory_resolver::proto::xfer::Protocol;
        if addrs.is_empty() {
            return Self::from_system();
        }
        let mut servers: Vec<NameServerConfig> = Vec::with_capacity(addrs.len() * 2);
        for addr in addrs {
            servers.push(NameServerConfig::new(*addr, Protocol::Udp));
            servers.push(NameServerConfig::new(*addr, Protocol::Tcp));
        }
        let group = NameServerConfigGroup::from(servers);
        let config = ResolverConfig::from_parts(None, vec![], group);
        let inner = hickory_resolver::Resolver::builder_with_config(
            config,
            hickory_resolver::name_server::TokioConnectionProvider::default(),
        )
        .build();
        Ok(Self { inner })
    }
}

#[cfg(feature = "dnsbl")]
#[async_trait]
impl AsyncDnsResolver for HickoryResolver {
    async fn lookup_a(&self, qname: &str) -> Option<bool> {
        match self.inner.ipv4_lookup(qname).await {
            Ok(_) => Some(true),
            Err(e) if is_genuine_negative(&e) => Some(false),
            Err(_) => None,
        }
    }

    async fn lookup_txt(&self, qname: &str) -> Option<String> {
        let lookup = self.inner.txt_lookup(qname).await.ok()?;
        lookup.iter().next().map(|t| t.to_string())
    }
}

/// Discriminate a genuine DNS negative answer from a transient error.
///
/// `ResolveError::is_no_records_found()` is **not** safe here: hickory
/// funnels SERVFAIL, REFUSED, FORMERR, NOTIMP, NOTAUTH, BADVERS, … all
/// into `ProtoErrorKind::NoRecordsFound`, distinguished only by the
/// inner `response_code`. Treating those as "not listed" would cache a
/// transient blip as a peer's permanent verdict.
///
/// Same shape as `cosmix-webd::mxresolve::classify`; we want one
/// workspace-wide reading of "DNS said no, definitely" vs "DNS broke."
#[cfg(feature = "dnsbl")]
fn is_genuine_negative(e: &hickory_resolver::ResolveError) -> bool {
    use hickory_resolver::proto::ProtoErrorKind;
    use hickory_resolver::proto::op::ResponseCode;
    matches!(
        e.proto().map(|p| p.kind()),
        Some(ProtoErrorKind::NoRecordsFound {
            response_code: ResponseCode::NXDomain | ResponseCode::NoError,
            ..
        })
    )
}

/// Convenience type alias for the production wiring.
#[cfg(feature = "dnsbl")]
pub type HickoryDnsbl = Dnsbl<HickoryResolver>;

// ============================================================
// Unit tests (pure encode_query checks)
// ============================================================

#[cfg(test)]
mod encode_tests {
    use super::*;

    #[test]
    fn ipv4_reverse_order() {
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(
            encode_query(ip, "zen.spamhaus.org"),
            "4.3.2.1.zen.spamhaus.org"
        );
    }

    #[test]
    fn ipv4_handles_zeros() {
        let ip: IpAddr = "0.0.0.10".parse().unwrap();
        assert_eq!(encode_query(ip, "dnsbl.test"), "10.0.0.0.dnsbl.test");
    }

    #[test]
    fn ipv6_loopback_full_oracle() {
        // ::1 → all 32 nibbles, last is 1.
        let ip: IpAddr = "::1".parse().unwrap();
        let q = encode_query(ip, "bl.example.org");
        let expected =
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.bl.example.org";
        assert_eq!(q, expected);
    }

    #[test]
    fn ipv6_rfc5782_example() {
        // RFC 5782 §2.4 example pattern: 2001:db8::1
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        let q = encode_query(ip, "bl.example");
        let expected = "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.bl.example";
        assert_eq!(q, expected);
    }

    #[test]
    fn ipv6_nibble_count_is_32() {
        // Any v6 address must produce exactly 32 nibbles (32 labels of
        // length 1) before the zone — i.e. 31 dots between nibbles plus
        // the trailing dot before the zone = 32 dots total. The zone
        // contributes its own dots; the test fixes the zone to a
        // dotless label so we can count.
        let ip: IpAddr = "fe80::1234:5678".parse().unwrap();
        let q = encode_query(ip, "zone");
        // 32 nibble chars + 31 inter-nibble dots + 1 zone dot + "zone"
        // = 32 + 31 + 1 + 4 = 68.
        assert_eq!(q.len(), 32 + 31 + 1 + "zone".len());
        let prefix = q.strip_suffix(".zone").unwrap();
        let nibble_chars: Vec<char> = prefix.split('.').flat_map(|s| s.chars()).collect();
        assert_eq!(nibble_chars.len(), 32);
        for ch in nibble_chars {
            assert!(ch.is_ascii_hexdigit(), "non-hex nibble: {ch:?}");
        }
    }
}
