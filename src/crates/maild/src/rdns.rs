//! Cached reverse-DNS (PTR) lookups for mail logging.
//!
//! ## Why this module exists
//!
//! `maillog::client()` renders the postfix `name[ip]` client clause, but
//! maild historically hardcoded `unknown[ip]`. On the mesh, dnsd serves
//! the WG reverse zone (`2.0.192.in-addr.arpa`), so a connecting node
//! *can* be named (`alpha.example.com[192.0.2.5]`) — which is what an
//! operator grepping the mail log actually wants to see.
//!
//! ## Design constraints
//!
//! - **Never block the accept path.** The only lookup happens inside
//!   [`resolve`], an async call bounded by [`LOOKUP_TIMEOUT`], awaited
//!   once per connection *before* the connect line is logged. Every
//!   other log line reads the cache synchronously via [`cached`].
//! - **Bounded memory.** The cache holds at most [`MAX_CACHE`] entries;
//!   positive hits live [`POSITIVE_TTL`], misses [`NEGATIVE_TTL`] (so a
//!   PTR-less scanner costs one lookup per IP per minute, not per
//!   connection). On overflow, expired entries are swept; if the cache
//!   is still full, it is cleared outright — crude, but the worst case
//!   is extra lookups, never growth.
//! - **The PTR target is attacker-controlled** (whoever holds the
//!   reverse zone for the connecting IP writes it). Names are validated
//!   against a strict hostname charset ([`valid_hostname`]) and
//!   anything else degrades to `unknown` — no reliance on downstream
//!   escaping. The name is PTR-asserted only (no forward confirmation);
//!   it is used for *logging*, never for authorization, and the
//!   bracketed IP in `name[ip]` remains the ground truth.
//!
//! Public (non-mesh) client IPs typically stay `unknown`: the system
//! resolver on a mesh node is dnsd, which is authoritative-only. That
//! matches the prior behavior exactly — this module only *adds* names
//! where the resolver actually has them.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hickory_resolver::{Resolver, TokioResolver};

/// Hard wall-clock bound on a single PTR lookup. dnsd answers over WG
/// in ~1 ms; 1 s covers a slow upstream without holding the SMTP
/// greeting hostage.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a successfully resolved name is served from cache.
const POSITIVE_TTL: Duration = Duration::from_secs(300);
/// How long a miss (NXDOMAIN, timeout, invalid name) is remembered.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
/// Cache capacity. 1024 distinct client IPs per TTL window is far
/// beyond mesh reality; the cap exists for public :25 scanners.
const MAX_CACHE: usize = 1024;
/// RFC 1035 total hostname length bound.
const MAX_NAME: usize = 253;

struct Entry {
    /// `Some(name)` = validated PTR name; `None` = negative entry.
    name: Option<String>,
    expires: Instant,
}

fn cache() -> &'static Mutex<HashMap<IpAddr, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<IpAddr, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lazily built shared resolver (system config, same construction as
/// the delivery worker). `None` if the system resolver config is
/// unusable — every lookup then negative-caches and logs stay
/// `unknown`, which is the pre-rdns behavior.
fn resolver() -> Option<&'static TokioResolver> {
    static RESOLVER: OnceLock<Option<TokioResolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| match Resolver::builder_tokio().map(|b| b.build()) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "rdns: no system resolver — client names stay unknown");
                None
            }
        })
        .as_ref()
}

/// Strict charset gate for an untrusted PTR name destined for the log.
///
/// Accepts only ASCII letters/digits/hyphen in dot-separated labels
/// (1..=63 chars each, no leading/trailing hyphen), total length
/// <= [`MAX_NAME`]. Everything else — control chars, Unicode, spaces,
/// empty labels — is rejected and the IP logs as `unknown`.
fn valid_hostname(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_NAME {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Sweep expired entries; if still at capacity, drop everything.
/// Caller holds the lock.
fn make_room(map: &mut HashMap<IpAddr, Entry>, now: Instant) {
    if map.len() < MAX_CACHE {
        return;
    }
    map.retain(|_, e| e.expires > now);
    if map.len() >= MAX_CACHE {
        map.clear();
    }
}

fn insert(ip: IpAddr, name: Option<String>, ttl: Duration) {
    let now = Instant::now();
    let mut map = cache().lock().expect("rdns cache poisoned");
    make_room(&mut map, now);
    map.insert(
        ip,
        Entry {
            name,
            expires: now + ttl,
        },
    );
}

/// Cache-only, non-blocking read: the validated PTR name for `ip`, if
/// a positive entry is live. This is what `maillog::client()` calls on
/// every log line — it never triggers network I/O.
pub fn cached(ip: IpAddr) -> Option<String> {
    let map = cache().lock().expect("rdns cache poisoned");
    map.get(&ip)
        .filter(|e| e.expires > Instant::now())
        .and_then(|e| e.name.clone())
}

/// Cache state for `ip`: `None` = no live entry, `Some(name)` = live
/// entry (positive or negative).
fn cached_entry(ip: IpAddr) -> Option<Option<String>> {
    let map = cache().lock().expect("rdns cache poisoned");
    map.get(&ip)
        .filter(|e| e.expires > Instant::now())
        .map(|e| e.name.clone())
}

/// Resolve `ip` to a validated PTR name, consulting and populating the
/// cache. Bounded by [`LOOKUP_TIMEOUT`]; a miss/timeout/invalid name is
/// negative-cached. Awaited once per SMTP connection before the
/// connect line so the name is available to every subsequent
/// `maillog` line via [`cached`].
pub async fn resolve(ip: IpAddr) -> Option<String> {
    if let Some(entry) = cached_entry(ip) {
        return entry;
    }
    let resolved = match resolver() {
        Some(r) => match tokio::time::timeout(LOOKUP_TIMEOUT, r.reverse_lookup(ip)).await {
            Ok(Ok(ptr)) => ptr.iter().next().map(|p| {
                let mut name = p.0.to_utf8();
                if name.ends_with('.') {
                    name.pop();
                }
                name.make_ascii_lowercase();
                name
            }),
            // NXDOMAIN / SERVFAIL / REFUSED or wall-clock timeout.
            Ok(Err(_)) | Err(_) => None,
        },
        None => None,
    };
    let validated = resolved.filter(|n| valid_hostname(n));
    let ttl = if validated.is_some() {
        POSITIVE_TTL
    } else {
        NEGATIVE_TTL
    };
    insert(ip, validated.clone(), ttl);
    validated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_accepts_normal_fqdn() {
        assert!(valid_hostname("alpha.example.com"));
        assert!(valid_hostname("a-1.example.com"));
        assert!(valid_hostname("localhost"));
    }

    #[test]
    fn hostname_rejects_injection_and_junk() {
        // Log-injection attempts.
        assert!(!valid_hostname("evil\r\nimap-login: Login: user=<admin>"));
        assert!(!valid_hostname("a b"));
        assert!(!valid_hostname("ansi\u{1b}[31m"));
        // Unicode (Trojan-Source class) — ASCII only.
        assert!(!valid_hostname("еxample.com")); // Cyrillic е
        // Structural junk.
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("."));
        assert!(!valid_hostname("a..b"));
        assert!(!valid_hostname(".lead"));
        assert!(!valid_hostname("trail."));
        assert!(!valid_hostname("-lead.example.com"));
        assert!(!valid_hostname("trail-.example.com"));
        assert!(!valid_hostname(&"x".repeat(MAX_NAME + 1)));
        let long_label = format!("{}.com", "y".repeat(64));
        assert!(!valid_hostname(&long_label));
    }

    // Single test for all cache mechanics: the cache is a process-wide
    // global, so separate #[test] fns would race each other's inserts
    // and clears across the parallel test threads.
    #[test]
    fn cache_positive_negative_expiry_and_overflow() {
        let ip: IpAddr = "192.0.2.77".parse().unwrap();
        // Positive entry serves from cache.
        insert(ip, Some("alpha.example.org".into()), POSITIVE_TTL);
        assert_eq!(cached(ip).as_deref(), Some("alpha.example.org"));
        assert_eq!(
            cached_entry(ip),
            Some(Some("alpha.example.org".to_string()))
        );
        // Negative entry: a live entry, but no name.
        let ip2: IpAddr = "192.0.2.78".parse().unwrap();
        insert(ip2, None, NEGATIVE_TTL);
        assert_eq!(cached(ip2), None);
        assert_eq!(cached_entry(ip2), Some(None));
        // Expired entry behaves as absent.
        let ip3: IpAddr = "192.0.2.79".parse().unwrap();
        insert(ip3, Some("gone.example.org".into()), Duration::ZERO);
        assert_eq!(cached(ip3), None);
        assert_eq!(cached_entry(ip3), None);

        // Overflow: fill past capacity with never-expiring entries; the
        // map must never exceed MAX_CACHE (sweep, then clear).
        for i in 0..(MAX_CACHE + 8) {
            let ip = IpAddr::from([10u8, 99, (i / 256) as u8, (i % 256) as u8]);
            insert(ip, None, Duration::from_secs(3600));
        }
        let len = cache().lock().unwrap().len();
        assert!(len <= MAX_CACHE, "cache grew past cap: {len}");
    }
}
