//! Connection guard primitives — protocol-agnostic (TCP-level), so
//! they apply equally to HTTPS / SMTP / IMAP. All default-off; a
//! daemon opts in per listener via [`GuardPolicy`].
//!
//! Enforcement order in the accept driver is cheapest-rejection-first:
//! ACL → per-IP rate → connection caps. `strict_sni` is realised by
//! building the listener's [`SniCertResolver`] in strict mode (it is
//! enforced inside the TLS handshake, not here), so this module
//! carries it only as declared intent.
//!
//! [`SniCertResolver`]: crate::tls::sni::SniCertResolver

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

/// Soft cap on the per-IP rate-bucket map. Past this, idle (fully
/// refilled) buckets are swept; a distinct-IP flood that defeats the
/// sweep clears the map outright (rate state resets to lenient — the
/// connection caps still apply). Bounds the map's memory on a public
/// listener.
const RATE_MAP_SOFT_CAP: usize = 65_536;

/// New-connection rate limit: at most `max` admitted per `per` window
/// for a single source IP (token bucket, fractional refill).
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    pub max: u32,
    pub per: Duration,
}

/// A CIDR block (IPv4 or IPv6). `parse` accepts `"a.b.c.d/n"`,
/// `"addr/n"`, or a bare host address (implicit `/32` or `/128`).
#[derive(Debug, Clone)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr_s, prefix_s) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr_s
            .trim()
            .parse()
            .map_err(|_| format!("invalid IP address in CIDR {s:?}"))?;
        let max: u8 = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix_s {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .map_err(|_| format!("invalid prefix length in CIDR {s:?}"))?,
            None => max,
        };
        if prefix > max {
            return Err(format!("prefix /{prefix} too large for {addr}"));
        }
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => v4_prefix_match(net, ip, self.prefix),
            (IpAddr::V6(net), IpAddr::V6(ip)) => v6_prefix_match(net, ip, self.prefix),
            // v4 CIDR never contains a v6 address or vice versa.
            _ => false,
        }
    }
}

fn v4_prefix_match(net: Ipv4Addr, ip: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true; // 0.0.0.0/0 matches everything; avoids a 32-bit shift.
    }
    let mask = u32::MAX << (32 - prefix as u32);
    (u32::from(net) & mask) == (u32::from(ip) & mask)
}

fn v6_prefix_match(net: Ipv6Addr, ip: Ipv6Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let mask = u128::MAX << (128 - prefix as u32);
    (u128::from(net) & mask) == (u128::from(ip) & mask)
}

/// Peer-IP allow/deny list. Deny wins; an empty `allow` means
/// allow-all (deny still applies).
#[derive(Debug, Clone, Default)]
pub struct IpAcl {
    pub allow: Vec<Cidr>,
    pub deny: Vec<Cidr>,
}

impl IpAcl {
    pub fn permits(&self, ip: IpAddr) -> bool {
        if self.deny.iter().any(|c| c.contains(ip)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|c| c.contains(ip))
    }
}

/// Per-listener guard policy. Every field is optional / off by
/// default, so a bare `GuardPolicy::default()` admits everything (the
/// behaviour of an unguarded listener).
#[derive(Debug, Clone, Default)]
pub struct GuardPolicy {
    /// Per-source-IP new-connection rate limit.
    pub per_ip_rate: Option<RateLimit>,
    /// Listener-wide concurrent-connection ceiling.
    pub max_conns: Option<u32>,
    /// Per-source-IP concurrent-connection ceiling.
    pub max_conns_per_ip: Option<u32>,
    /// Reject no-SNI / unknown-SNI handshakes. Only meaningful for
    /// `TlsMode::Terminate`; realised by building the resolver in
    /// strict mode (see module docs).
    pub strict_sni: bool,
    /// Optional peer-IP allow/deny list.
    pub acl: Option<IpAcl>,
}

/// Fractional token bucket. Exposed (crate-internal) so the rate
/// logic is unit-tested with synthetic `Instant`s rather than wall
/// clock.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(max: u32, now: Instant) -> Self {
        Self {
            tokens: max as f64,
            last: now,
        }
    }

    /// Refill for elapsed time, then try to spend one token.
    fn try_take(&mut self, rl: &RateLimit, now: Instant) -> bool {
        let per = rl.per.as_secs_f64().max(f64::MIN_POSITIVE);
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed / per * rl.max as f64).min(rl.max as f64);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Runtime guard state for one listener (shared across its bind
/// addresses). Holds the live connection counters and rate buckets.
///
/// The [`GuardPolicy`] sits behind an [`ArcSwap`] so a listener's
/// limits can be hot-swapped at runtime ([`swap_guard`]) — e.g. an
/// operator tightening `max_conns` via the `webd.listeners` namespace.
/// Only the *policy* swaps; the live counters (`global`, `per_ip`,
/// `buckets`) are separate fields, so a swap never resets the active
/// connection count or the rate buckets — it only changes the limits
/// the next [`admit`](Self::admit) is judged against.
///
/// [`swap_guard`]: Self::swap_guard
#[derive(Debug)]
pub struct Guards {
    policy: ArcSwap<GuardPolicy>,
    global: Arc<AtomicU32>,
    per_ip: Arc<Mutex<HashMap<IpAddr, u32>>>,
    buckets: Arc<Mutex<HashMap<IpAddr, TokenBucket>>>,
}

impl Guards {
    pub fn new(policy: GuardPolicy) -> Self {
        Self {
            policy: ArcSwap::from_pointee(policy),
            global: Arc::new(AtomicU32::new(0)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Hot-swap this listener's guard policy. Wait-free for the accept
    /// loop's concurrent [`admit`](Self::admit) calls (an in-flight
    /// admit completes against the policy in force when it started; the
    /// next admit sees the new one). The live connection counters and
    /// rate buckets are untouched, so changing `max_conns` 100→50 does
    /// not zero the active count and changing `per_ip_rate` does not
    /// refill anyone's bucket.
    pub fn swap_guard(&self, policy: GuardPolicy) {
        self.policy.store(Arc::new(policy));
    }

    /// Live count of admitted-and-not-yet-dropped connections. Used
    /// as the drain gauge on listener disable.
    pub fn active(&self) -> u32 {
        self.global.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn rate_bucket_len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }

    /// Admit a connection from `ip` (uses the wall clock for rate).
    /// Returns a [`ConnPermit`] whose `Drop` releases the slot, or
    /// `None` if any guard rejected it.
    pub fn admit(&self, ip: IpAddr) -> Option<ConnPermit> {
        self.admit_at(ip, Instant::now())
    }

    /// `admit` with an explicit `now` for deterministic tests.
    pub fn admit_at(&self, ip: IpAddr, now: Instant) -> Option<ConnPermit> {
        // One wait-free snapshot of the policy for the whole decision —
        // a concurrent `swap_guard` mid-admit can't tear it, and this
        // admit is judged consistently against the limits in force when
        // it began.
        let policy = self.policy.load();

        // 1. ACL — cheapest, no locks beyond the scan.
        if let Some(acl) = &policy.acl
            && !acl.permits(ip)
        {
            return None;
        }

        // 2. Per-IP rate.
        if let Some(rl) = &policy.per_ip_rate {
            let mut buckets = self.buckets.lock().expect("guard buckets poisoned");
            let allowed = buckets
                .entry(ip)
                .or_insert_with(|| TokenBucket::new(rl.max, now))
                .try_take(rl, now);
            // Bound memory: a fully-refilled bucket is indistinguishable
            // from a fresh one, so idle IPs are safe to drop. Sweep when
            // the map grows past the soft cap; if a distinct-IP flood
            // defeats the sweep, clear outright.
            if buckets.len() > RATE_MAP_SOFT_CAP {
                let max = rl.max as f64;
                buckets.retain(|_, b| b.tokens < max);
                if buckets.len() > RATE_MAP_SOFT_CAP {
                    buckets.clear();
                }
            }
            if !allowed {
                return None;
            }
        }

        // 3. Global concurrency cap. Always count active conns (drain
        //    gauge); reserve-then-rollback keeps the cap race-free.
        let prev = self.global.fetch_add(1, Ordering::AcqRel);
        if let Some(cap) = policy.max_conns
            && prev >= cap
        {
            self.global.fetch_sub(1, Ordering::AcqRel);
            return None;
        }

        // 4. Per-IP concurrency. ALWAYS track the per-IP active count
        //    (mirroring the always-on global counter), and enforce the
        //    ceiling only when the *current* policy sets one. Tracking
        //    unconditionally is what keeps the count correct across a
        //    `swap_guard` that introduces a per-IP cap: connections
        //    admitted before the cap existed are already counted, so
        //    the new cap can't be defeated by pre-swap connections. The
        //    map self-bounds — an entry is removed when its count hits
        //    zero on the last permit drop (no post-close residue, so no
        //    sweep needed unlike the rate buckets).
        {
            let mut per_ip = self.per_ip.lock().expect("guard per_ip poisoned");
            let current = per_ip.get(&ip).copied().unwrap_or(0);
            if let Some(cap) = policy.max_conns_per_ip
                && current >= cap
            {
                drop(per_ip);
                self.global.fetch_sub(1, Ordering::AcqRel);
                return None;
            }
            per_ip.insert(ip, current + 1);
        }

        Some(ConnPermit {
            global: self.global.clone(),
            per_ip: self.per_ip.clone(),
            ip,
        })
    }
}

/// RAII permit returned by [`Guards::admit`]. Releasing it (drop)
/// decrements the connection counters, so an in-flight connection
/// closing — for any reason — restores its slot with no bookkeeping
/// at the call site.
#[derive(Debug)]
pub struct ConnPermit {
    global: Arc<AtomicU32>,
    per_ip: Arc<Mutex<HashMap<IpAddr, u32>>>,
    ip: IpAddr,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.global.fetch_sub(1, Ordering::AcqRel);
        // Per-IP is tracked for every admitted connection (see
        // `admit_at` step 4), so every permit decrements it.
        let mut per_ip = self.per_ip.lock().expect("guard per_ip poisoned");
        if let Some(c) = per_ip.get_mut(&self.ip) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                per_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_v4_contains() {
        let c = Cidr::parse("192.168.2.0/24").unwrap();
        assert!(c.contains(ip("192.168.2.10")));
        assert!(!c.contains(ip("192.168.3.10")));
        assert!(!c.contains(ip("::1")));
    }

    #[test]
    fn cidr_bare_host_is_full_prefix() {
        let c = Cidr::parse("10.0.0.5").unwrap();
        assert!(c.contains(ip("10.0.0.5")));
        assert!(!c.contains(ip("10.0.0.6")));
    }

    #[test]
    fn cidr_v6_and_zero_prefix() {
        let c = Cidr::parse("2403:580a::/32").unwrap();
        assert!(c.contains(ip("2403:580a:5e99::1")));
        assert!(!c.contains(ip("2001:db8::1")));
        let any = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(any.contains(ip("8.8.8.8")));
    }

    #[test]
    fn cidr_rejects_garbage() {
        assert!(Cidr::parse("nope").is_err());
        assert!(Cidr::parse("10.0.0.0/40").is_err());
        assert!(Cidr::parse("10.0.0.0/x").is_err());
    }

    #[test]
    fn acl_deny_wins_over_allow() {
        let acl = IpAcl {
            allow: vec![Cidr::parse("10.0.0.0/8").unwrap()],
            deny: vec![Cidr::parse("10.0.0.5/32").unwrap()],
        };
        assert!(acl.permits(ip("10.1.2.3")));
        assert!(!acl.permits(ip("10.0.0.5")));
        // Not in allow → denied.
        assert!(!acl.permits(ip("192.168.1.1")));
    }

    #[test]
    fn acl_empty_allow_is_allow_all() {
        let acl = IpAcl {
            allow: vec![],
            deny: vec![Cidr::parse("1.2.3.4/32").unwrap()],
        };
        assert!(acl.permits(ip("9.9.9.9")));
        assert!(!acl.permits(ip("1.2.3.4")));
    }

    #[test]
    fn token_bucket_exhausts_then_refills() {
        let t0 = Instant::now();
        let rl = RateLimit {
            max: 2,
            per: Duration::from_secs(1),
        };
        let mut b = TokenBucket::new(rl.max, t0);
        assert!(b.try_take(&rl, t0)); // 2 -> 1
        assert!(b.try_take(&rl, t0)); // 1 -> 0
        assert!(!b.try_take(&rl, t0)); // empty
        // Half a window later: +1 token.
        let t1 = t0 + Duration::from_millis(500);
        assert!(b.try_take(&rl, t1));
        assert!(!b.try_take(&rl, t1));
    }

    #[test]
    fn admit_unguarded_always_admits_and_counts() {
        let g = Guards::new(GuardPolicy::default());
        let p1 = g.admit(ip("1.1.1.1")).unwrap();
        let p2 = g.admit(ip("1.1.1.1")).unwrap();
        assert_eq!(g.active(), 2);
        drop(p1);
        assert_eq!(g.active(), 1);
        drop(p2);
        assert_eq!(g.active(), 0);
    }

    #[test]
    fn admit_global_cap_rejects_over_limit() {
        let g = Guards::new(GuardPolicy {
            max_conns: Some(1),
            ..Default::default()
        });
        let _p = g.admit(ip("1.1.1.1")).unwrap();
        assert!(g.admit(ip("2.2.2.2")).is_none());
        assert_eq!(g.active(), 1);
    }

    #[test]
    fn admit_per_ip_cap_isolates_sources() {
        let g = Guards::new(GuardPolicy {
            max_conns_per_ip: Some(1),
            ..Default::default()
        });
        let _a = g.admit(ip("1.1.1.1")).unwrap();
        assert!(g.admit(ip("1.1.1.1")).is_none()); // same IP at cap
        let _b = g.admit(ip("2.2.2.2")).unwrap(); // different IP ok
        assert_eq!(g.active(), 2);
    }

    #[test]
    fn admit_acl_denies_before_counting() {
        let g = Guards::new(GuardPolicy {
            acl: Some(IpAcl {
                allow: vec![],
                deny: vec![Cidr::parse("9.9.9.9/32").unwrap()],
            }),
            ..Default::default()
        });
        assert!(g.admit(ip("9.9.9.9")).is_none());
        assert_eq!(g.active(), 0); // rejected before the global increment
    }

    #[test]
    fn rate_bucket_map_stays_bounded_under_distinct_ip_flood() {
        let now = Instant::now();
        let g = Guards::new(GuardPolicy {
            per_ip_rate: Some(RateLimit {
                max: 10,
                per: Duration::from_secs(1),
            }),
            ..Default::default()
        });
        // Feed well past the soft cap with distinct source IPs; the
        // map must not grow without bound.
        for i in 0..(super::RATE_MAP_SOFT_CAP as u32 + 5_000) {
            let _ = g.admit_at(IpAddr::V4(Ipv4Addr::from(i)), now);
        }
        assert!(
            g.rate_bucket_len() <= super::RATE_MAP_SOFT_CAP,
            "rate bucket map unbounded: {}",
            g.rate_bucket_len()
        );
    }

    #[test]
    fn admit_rate_limit_rejects_burst() {
        let now = Instant::now();
        let g = Guards::new(GuardPolicy {
            per_ip_rate: Some(RateLimit {
                max: 1,
                per: Duration::from_secs(1),
            }),
            ..Default::default()
        });
        assert!(g.admit_at(ip("5.5.5.5"), now).is_some());
        assert!(g.admit_at(ip("5.5.5.5"), now).is_none()); // bucket empty
        // A different IP has its own bucket.
        assert!(g.admit_at(ip("6.6.6.6"), now).is_some());
    }

    #[test]
    fn swap_preserves_active_count() {
        // Tightening the cap mid-flight must not touch the live count —
        // the swap only changes what the NEXT admit is judged against.
        let g = Guards::new(GuardPolicy {
            max_conns: Some(10),
            ..Default::default()
        });
        let _p1 = g.admit(ip("1.1.1.1")).unwrap();
        let _p2 = g.admit(ip("1.1.1.1")).unwrap();
        assert_eq!(g.active(), 2);
        g.swap_guard(GuardPolicy {
            max_conns: Some(1),
            ..Default::default()
        });
        // The two in-flight permits still count; the swap didn't reset.
        assert_eq!(g.active(), 2);
    }

    #[test]
    fn swap_tightens_cap_rejects_next() {
        // With the global cap already exceeded by live conns, the next
        // admit under the tightened policy is rejected.
        let g = Guards::new(GuardPolicy {
            max_conns: Some(10),
            ..Default::default()
        });
        let _p1 = g.admit(ip("1.1.1.1")).unwrap();
        let _p2 = g.admit(ip("2.2.2.2")).unwrap();
        g.swap_guard(GuardPolicy {
            max_conns: Some(1),
            ..Default::default()
        });
        // active()==2 >= cap 1 → reject, and the rejected attempt must
        // not leak the global counter.
        assert!(g.admit(ip("3.3.3.3")).is_none());
        assert_eq!(g.active(), 2);
    }

    #[test]
    fn swap_introducing_per_ip_cap_counts_preexisting_conns() {
        // Per-IP counts are tracked even with no per-IP cap, so a swap
        // that INTRODUCES one cannot be defeated by connections that
        // were admitted before the cap existed.
        let g = Guards::new(GuardPolicy::default());
        let _a = g.admit(ip("1.1.1.1")).unwrap();
        let _b = g.admit(ip("1.1.1.1")).unwrap(); // 2 from this IP, uncapped
        g.swap_guard(GuardPolicy {
            max_conns_per_ip: Some(1),
            ..Default::default()
        });
        // The 2 pre-swap connections are counted, so the IP is over the
        // new cap of 1 → the next admit from it is rejected.
        assert!(g.admit(ip("1.1.1.1")).is_none());
        // A fresh IP with no history is admitted.
        let _c = g.admit(ip("2.2.2.2")).unwrap();
    }

    #[test]
    fn swap_loosens_rate_keeps_bucket_state() {
        // A drained bucket stays drained across a swap that only raises
        // the rate — the bucket refills against the NEW rate over time,
        // it is not reset to full by the swap itself.
        let now = Instant::now();
        let g = Guards::new(GuardPolicy {
            per_ip_rate: Some(RateLimit {
                max: 1,
                per: Duration::from_secs(1),
            }),
            ..Default::default()
        });
        assert!(g.admit_at(ip("5.5.5.5"), now).is_some()); // 1 -> 0
        assert!(g.admit_at(ip("5.5.5.5"), now).is_none()); // empty
        // Raise the rate; the existing bucket is preserved (still ~0
        // tokens at `now`), so an immediate retry at the same instant
        // is still rejected — no free refill from the swap.
        g.swap_guard(GuardPolicy {
            per_ip_rate: Some(RateLimit {
                max: 100,
                per: Duration::from_secs(1),
            }),
            ..Default::default()
        });
        assert!(g.admit_at(ip("5.5.5.5"), now).is_none());
        // A full window later it refills against the new (larger) rate.
        let later = now + Duration::from_secs(1);
        assert!(g.admit_at(ip("5.5.5.5"), later).is_some());
    }
}
