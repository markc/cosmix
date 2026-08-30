//! MX-derived hostname resolution for mail-client autoconfig.
//!
//! webd serves the **MX target** of the requested domain as the
//! IMAPS/SMTPS `<hostname>` (the NS 3.0 model — see
//! `_doc/planned/maild-autoconfig.md` §Decision). Two deliberate
//! divergences from `cosmix-maild`'s delivery resolver
//! (`smtp/delivery.rs::resolve_mx`), driven by the autoconfig
//! §Security bar:
//!
//! 1. **No A-record fallback.** maild falls back to an A lookup on the
//!    bare domain when MX resolution fails (best-effort delivery).
//!    Autoconfig must *never* serve a guessed `<hostname>` — a DNS
//!    failure is a clean `404`/`503`, not a body.
//! 2. **Deterministic selection.** maild sorts by preference only and
//!    tries hosts in order. Autoconfig serves *one* hostname, so the
//!    choice must be stable: lowest preference, ties broken by the
//!    lexicographically-smallest normalized target — never resolver
//!    record order, which is non-deterministic.
//!
//! ## Operator-policy probe
//!
//! An MX record is *inbound-SMTP routing*, not a declaration that the
//! target also terminates IMAPS/SMTPS. Reusing it as the served
//! `<hostname>` is a Cosmix operator-policy requirement
//! (autoconfig.md §Decision); §Acceptance bullet 3 requires that policy
//! be *verified*, not assumed. So after MX selection `resolve` probes
//! the chosen host: a bounded, concurrent **implicit-TLS** handshake to
//! IMAPS:993 *and* SMTPS:465 (maild is implicit-TLS only — no STARTTLS).
//! A successful rustls handshake with `ServerName = host` proves, in
//! one step, both that the chain validates to a webpki root and that
//! the presented cert covers `host` (rustls verifies the name during
//! the handshake) — exactly the operator-policy assertion. A
//! mis-published inbound-only MX therefore fails the probe and is
//! served as `503`, never as a config that would silently fail in the
//! client. Unlike `cosmix-maild`'s delivery resolver, the probe **never
//! falls back to a placeholder `ServerName`**: for a verification probe
//! an unparseable name must fail closed, not validate some other host.
//!
//! `select_mx` / `normalize_host` are pure and unit-tested; the
//! network paths (`MxResolver`, `probe_host`) are thin async wrappers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hickory_resolver::{Resolver, TokioResolver};

/// Positive answers are stable for this long (MX rarely flips).
const POSITIVE_TTL: Duration = Duration::from_secs(300);
/// Negative answers (NXDOMAIN / no-MX) cached briefly so a public
/// endpoint cannot be turned into a DNS-amplification driver, while a
/// freshly-published MX still appears within a minute.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
/// Bound the per-request resolver wait so a slow upstream cannot wedge
/// the public endpoint (autoconfig.md §Security: "Bounded timeout").
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// When the single-flight gate map exceeds this, opportunistically drop
/// the idle (map-only) entries so it tracks *in-flight* hosts, not
/// every MX host ever observed. Generous so the sweep is rare.
const GATE_MAP_SOFT_CAP: usize = 256;
/// Single deadline covering *both* implicit-TLS probe handshakes (they
/// run concurrently, so worst-case cold-request latency is
/// `LOOKUP_TIMEOUT + PROBE_TIMEOUT`, not three serial waits).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// maild's fixed wire ports — IMAPS and SMTPS, both implicit-TLS
/// (autoconfig.md §Constraints: "993/465, no STARTTLS"). The probe
/// asserts the MX target terminates *both*.
const IMAPS_PORT: u16 = 993;
const SMTPS_PORT: u16 = 465;

/// Why an MX could not be served. Maps directly to an HTTP status in
/// the handler so the §Security "clean failure, never a guessed body"
/// rule is enforced at one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MxError {
    /// NXDOMAIN, no MX RRset, or every candidate failed normalization.
    /// A definitive, cacheable "no" → HTTP 404.
    NoMx,
    /// Timeout / SERVFAIL / resolver I/O — indeterminate, not cached,
    /// retryable → HTTP 503.
    Transient,
    /// MX resolved, but the target failed the operator-policy probe (it
    /// did not terminate implicit TLS on 993 *and* 465 with a cert
    /// valid for that host). A mis-published inbound-only MX → HTTP 503,
    /// never a config that fails silently in the client. Briefly cached
    /// (see `probe_cache_put`): unlike `Transient`, re-probing on every
    /// request would turn the public endpoint into a TLS-handshake
    /// amplifier against the (operator-published) MX host.
    ProbeFailed,
}

/// One cache slot: when the answer was stored, and what it was. The
/// `Result` distinguishes a cached positive hostname from a cached
/// definitive `NoMx` (a `Transient` is never stored — see `cache_put`).
type CacheEntry = (Instant, Result<String, MxError>);

/// One probe-cache slot, keyed by **MX host** (not domain): N
/// allowlisted domains sharing one MX target must trigger at most one
/// probe per host per TTL, not one per domain.
type ProbeEntry = (Instant, Result<(), MxError>);

/// Async MX resolver with short positive/negative caches for both the
/// MX lookup and the operator-policy TLS probe.
pub struct MxResolver {
    resolver: TokioResolver,
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// Shared rustls client config (webpki roots, no client auth) for
    /// the implicit-TLS probe. Built once; `TlsConnector::from` clones
    /// the `Arc` per handshake.
    tls: Arc<rustls::ClientConfig>,
    /// Probe verdicts keyed by MX host. `Ok(())` = host terminates
    /// 993+465 with a valid cert; `Err(ProbeFailed)` = it does not.
    probe_cache: Mutex<HashMap<String, ProbeEntry>>,
    /// Per-host single-flight gate. Without it the negative cache only
    /// bounds *steady-state* amplification: a concurrent cold-cache
    /// burst for the same host would all miss (no entry until the first
    /// probe finishes) and each fire `probe_host` → N×2 outbound TLS
    /// handshakes inside the probe window. Serializing provers per host
    /// collapses any burst to one probe (the others wait, then reuse
    /// the now-cached verdict). Keyed per host so distinct hosts still
    /// probe in parallel. Async mutex: held across `probe_host().await`.
    /// Unlike `probe_cache` this is pure coordination state with no
    /// reason to outlive the probe, so `probe_gate` sweeps idle
    /// (map-only) entries past `GATE_MAP_SOFT_CAP` — its size tracks
    /// in-flight hosts, not every MX host ever observed.
    probe_gate: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl MxResolver {
    /// Build a system-config (`/etc/resolv.conf`) backed resolver.
    /// Mirrors `cosmix-maild` delivery worker construction so both
    /// crates use the identical hickory builder path.
    pub fn new() -> anyhow::Result<Self> {
        let resolver = Resolver::builder_tokio()
            .map_err(|e| anyhow::anyhow!("DNS resolver init failed: {e}"))?
            .build();
        Ok(Self {
            resolver,
            cache: Mutex::new(HashMap::new()),
            tls: build_client_config(),
            probe_cache: Mutex::new(HashMap::new()),
            probe_gate: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve `domain` to its deterministically-selected, normalized
    /// MX target, **verified** to terminate IMAPS:993 + SMTPS:465 with
    /// a cert valid for that host. Never falls back to an A record, a
    /// guessed name, or an unverified host: a bad MX is `NoMx`/`503`,
    /// not a body.
    pub async fn resolve(&self, domain: &str) -> Result<String, MxError> {
        let host = self.resolve_mx(domain).await?;
        self.verify_host(&host).await?;
        Ok(host)
    }

    /// Verify a fixed, node-configured internal mail host (e.g.
    /// `epsilon.example.com`, reached over WireGuard) terminates IMAPS:993 +
    /// SMTPS:465 with a valid cert — the same operator-policy probe as
    /// [`Self::resolve`], but for a configured host instead of the domain's
    /// public MX. This node deliberately exposes the mail client protocols
    /// only on the WG interface (the public FQDN serves :25 + :443 only), so
    /// autoconfig advertises the WG host. A down/misconfigured host still
    /// 503s rather than handing the client a dead config.
    pub async fn resolve_fixed(&self, host: &str) -> Result<String, MxError> {
        self.verify_host(host).await?;
        Ok(host.to_string())
    }

    /// MX lookup + selection step (cached). Split from `resolve` so the
    /// operator-policy probe composes cleanly on top of it.
    async fn resolve_mx(&self, domain: &str) -> Result<String, MxError> {
        if let Some(hit) = self.cache_get(domain) {
            return hit;
        }
        let result = self.resolve_uncached(domain).await;
        self.cache_put(domain, &result);
        result
    }

    /// Operator-policy probe step (cached by MX host). Returns `Ok(())`
    /// only when the host terminates both implicit-TLS ports with a
    /// cert that covers it; any failure → `Err(ProbeFailed)`.
    async fn verify_host(&self, host: &str) -> Result<(), MxError> {
        if let Some(hit) = self.probe_cache_get(host) {
            return hit;
        }
        // Single-flight per host so a concurrent cold-cache burst for
        // the same MX target collapses to one probe instead of N×2
        // outbound TLS handshakes (the MX-lookup path keeps its weaker,
        // pre-existing DNS stampede — own resolver, not a third party,
        // and out of scope for this change).
        let gate = self.probe_gate(host);
        let _held = gate.lock().await;
        // A prior holder of this gate may have just filled the cache —
        // re-check before probing so the burst genuinely coalesces.
        if let Some(hit) = self.probe_cache_get(host) {
            return hit;
        }
        let result = probe_host(&self.tls, host).await;
        self.probe_cache_put(host, &result);
        result
    }

    /// Per-host async mutex (created on first use). Holding it across
    /// the probe is what makes "one probe per host per TTL" true even
    /// under a concurrent burst, not just in steady state.
    fn probe_gate(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.probe_gate.lock().unwrap();
        // Opportunistic sweep. `strong_count == 1` ⇒ only the map holds
        // it: no prober and no parked waiter (a waiter always holds a
        // clone). Every clone is created here under this same std lock,
        // so the count is authoritative within this scope and dropping
        // a count-1 entry cannot strand anyone — a future user just
        // recreates it. Cheap and rare (gated on the soft cap).
        if map.len() > GATE_MAP_SOFT_CAP {
            map.retain(|_, g| Arc::strong_count(g) > 1);
        }
        map.entry(host.to_string()).or_default().clone()
    }

    async fn resolve_uncached(&self, domain: &str) -> Result<String, MxError> {
        let lookup = tokio::time::timeout(LOOKUP_TIMEOUT, self.resolver.mx_lookup(domain)).await;
        let mx = match lookup {
            Err(_) => return Err(MxError::Transient), // timed out
            Ok(Err(e)) => return Err(classify(&e)),
            Ok(Ok(mx)) => mx,
        };
        let records: Vec<(u16, String)> = mx
            .iter()
            .map(|r| (r.preference(), r.exchange().to_ascii()))
            .collect();
        // Empty RRset or all targets malformed → definitive 404.
        select_mx(&records).ok_or(MxError::NoMx)
    }

    fn cache_get(&self, domain: &str) -> Option<Result<String, MxError>> {
        let cache = self.cache.lock().unwrap();
        let (at, val) = cache.get(domain)?;
        let ttl = if val.is_ok() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        (at.elapsed() < ttl).then(|| val.clone())
    }

    fn cache_put(&self, domain: &str, val: &Result<String, MxError>) {
        // Transient failures are never cached — they must be retried,
        // and caching one would pin a public domain to 503 for the TTL.
        if matches!(val, Err(MxError::Transient)) {
            return;
        }
        self.cache
            .lock()
            .unwrap()
            .insert(domain.to_string(), (Instant::now(), val.clone()));
    }

    fn probe_cache_get(&self, host: &str) -> Option<Result<(), MxError>> {
        let cache = self.probe_cache.lock().unwrap();
        let (at, val) = cache.get(host)?;
        let ttl = if val.is_ok() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        (at.elapsed() < ttl).then(|| val.clone())
    }

    fn probe_cache_put(&self, host: &str, val: &Result<(), MxError>) {
        // Both outcomes are cached. A pass is stable for POSITIVE_TTL.
        // A fail is cached only for the short NEGATIVE_TTL — deliberately
        // *unlike* `Transient` (never cached): a probe is two full TLS
        // handshakes to an operator-published third-party host, so
        // re-probing on every request would make this public endpoint a
        // TLS-handshake amplifier. NEGATIVE_TTL bounds the *steady
        // state* to ~one probe per MX host per minute (the per-host
        // single-flight gate in `verify_host` bounds the concurrent
        // *burst*), while a fixed operator config or a recovered host
        // still reappears within the minute — the same
        // amplification/freshness trade the negative DNS cache already
        // makes. A momentary host-down blip costs at most NEGATIVE_TTL
        // of 503s, which is the accepted price of not being an amplifier.
        self.probe_cache
            .lock()
            .unwrap()
            .insert(host.to_string(), (Instant::now(), val.clone()));
    }
}

/// Build the shared rustls client config: webpki trust roots, no client
/// auth. Mirrors `cosmix-maild`'s delivery TLS builder path verbatim so
/// the workspace keeps a single rustls/webpki-roots configuration.
fn build_client_config() -> Arc<rustls::ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

/// Probe one implicit-TLS port: TCP connect, then a rustls handshake
/// with `ServerName = host`. Success proves the chain validates to a
/// webpki root *and* the cert covers `host`. Any failure (connect,
/// handshake, cert-name mismatch, untrusted chain, unparseable name) →
/// `ProbeFailed`. Errors are intentionally collapsed: the handler maps
/// them all to one 503; the specific cause is the operator's, not the
/// client's, to debug.
async fn probe_one(tls: &Arc<rustls::ClientConfig>, host: &str, port: u16) -> Result<(), MxError> {
    // A `normalize_host` FQDN always parses. We do NOT fall back to a
    // placeholder name the way the delivery resolver does — for a
    // *verification* probe an unparseable name must fail closed, never
    // validate a cert issued for some unrelated host.
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| MxError::ProbeFailed)?;
    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|_| MxError::ProbeFailed)?;
    let connector = tokio_rustls::TlsConnector::from(tls.clone());
    connector
        .connect(server_name, tcp)
        .await
        .map(|_| ())
        .map_err(|_| MxError::ProbeFailed)
}

/// Operator-policy probe: the host must terminate **both** implicit-TLS
/// ports with a cert valid for it. The two handshakes run concurrently
/// under one `PROBE_TIMEOUT`; `try_join!` short-circuits on the first
/// failure so a dead host fails fast, and the deadline is a single
/// window (not `2 ×`).
async fn probe_host(tls: &Arc<rustls::ClientConfig>, host: &str) -> Result<(), MxError> {
    let both = async {
        tokio::try_join!(
            probe_one(tls, host, IMAPS_PORT),
            probe_one(tls, host, SMTPS_PORT),
        )
    };
    match tokio::time::timeout(PROBE_TIMEOUT, both).await {
        Ok(Ok(((), ()))) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(MxError::ProbeFailed), // overall deadline hit
    }
}

/// Classify a hickory resolve error into "definitive no MX" (cacheable
/// 404) vs "transient" (uncached 503).
///
/// `ResolveError::is_no_records_found()` is **not** safe to use here:
/// hickory's `ProtoError::from_response` funnels SERVFAIL, REFUSED,
/// FORMERR, NOTIMP, NOTAUTH, BADVERS, … *all* into the single
/// `NoRecordsFound` variant (only distinguished by its inner
/// `response_code`), so that predicate is `true` for transient
/// server/transport faults too. Caching one of those as a permanent
/// absence would pin a public domain to 404 for the negative TTL on a
/// transient upstream blip. So this matches the inner `response_code`
/// and treats only a genuine negative answer as `NoMx`: `NXDomain`
/// (the name does not exist) or `NoError` (the name exists but has no
/// MX RRset). Every other code — and every non-`NoRecordsFound` proto
/// kind, plus internal Message/IO errors with no proto payload — is
/// `Transient`.
fn classify(e: &hickory_resolver::ResolveError) -> MxError {
    use hickory_resolver::proto::ProtoErrorKind;
    use hickory_resolver::proto::op::ResponseCode;
    match e.proto().map(|p| p.kind()) {
        Some(ProtoErrorKind::NoRecordsFound {
            response_code: ResponseCode::NXDomain | ResponseCode::NoError,
            ..
        }) => MxError::NoMx,
        _ => MxError::Transient,
    }
}

/// Deterministic MX selection: lowest preference wins; equal
/// preference is broken by the lexicographically-smallest *normalized*
/// target. Targets that fail `normalize_host` are discarded (a single
/// malformed MX must not shadow a valid lower-priority one). Returns
/// `None` when no candidate normalizes — the caller maps that to 404.
pub fn select_mx(records: &[(u16, String)]) -> Option<String> {
    records
        .iter()
        .filter_map(|(pref, ex)| normalize_host(ex).map(|h| (*pref, h)))
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, h)| h)
}

/// Normalize an MX exchange into a served `<hostname>`: strip the root
/// dot, lowercase, and validate it is a syntactically well-formed
/// ASCII FQDN (LDH labels, ≤63 each, ≤253 total, ≥2 labels). hickory's
/// `to_ascii()` already produced the IDNA A-label form, so this only
/// has to reject what is still malformed — guaranteeing a bad MX never
/// reaches the XML serializer as a broken `<hostname>`.
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() || h.len() > 253 || !h.contains('.') {
        return None;
    }
    let ok = h.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    ok.then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowest_preference_wins() {
        let r = vec![
            (20, "backup.example.com.".to_string()),
            (10, "primary.example.com.".to_string()),
        ];
        assert_eq!(select_mx(&r).as_deref(), Some("primary.example.com"));
    }

    #[test]
    fn equal_preference_breaks_lexicographically() {
        let r = vec![
            (10, "zeta.example.com.".to_string()),
            (10, "alpha.example.com.".to_string()),
            (10, "mu.example.com.".to_string()),
        ];
        // Deterministic regardless of RRset order.
        assert_eq!(select_mx(&r).as_deref(), Some("alpha.example.com"));
    }

    #[test]
    fn malformed_target_is_skipped_not_selected() {
        let r = vec![
            (5, "not_a_host".to_string()), // bare label, underscore
            (10, "valid.example.com.".to_string()),
        ];
        // The preference-5 entry is invalid; the valid 10 is served
        // rather than letting a malformed top entry win or 404.
        assert_eq!(select_mx(&r).as_deref(), Some("valid.example.com"));
    }

    #[test]
    fn empty_rrset_is_none() {
        assert_eq!(select_mx(&[]), None);
    }

    #[test]
    fn normalize_strips_dot_and_lowercases() {
        assert_eq!(
            normalize_host("Mail.Example.ORG.").as_deref(),
            Some("mail.example.org")
        );
    }

    #[test]
    fn normalize_rejects_bare_label_and_bad_chars() {
        assert_eq!(normalize_host("localhost"), None); // no dot → not a FQDN
        assert_eq!(normalize_host("a..b.com"), None); // empty label
        assert_eq!(normalize_host("-bad.example.com"), None); // leading hyphen
        assert_eq!(normalize_host("under_score.example.com"), None);
        assert_eq!(normalize_host(""), None);
    }

    /// A free, in-process TCP port: bind ephemeral, read it, drop the
    /// listener. Nothing rebinds it within the test process, so a
    /// connect there is a deterministic, instant ECONNREFUSED — no
    /// network, no `PROBE_TIMEOUT` wait, no flake.
    fn refused_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    }

    /// Idempotent across the test binary: first call installs the ring
    /// provider, later calls get `Err` (already set) which we ignore.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[tokio::test]
    async fn probe_one_fails_closed_on_connection_refused() {
        ensure_crypto_provider();
        let tls = build_client_config();
        // Connection refused must surface as ProbeFailed (a config the
        // client could silently fail on must never be served), and it
        // must do so without waiting the timeout — refused is instant.
        let r = probe_one(&tls, "127.0.0.1", refused_port()).await;
        assert_eq!(r, Err(MxError::ProbeFailed));
    }

    #[tokio::test]
    async fn concurrent_verify_for_same_host_is_consistent_and_cached_once() {
        ensure_crypto_provider();
        let r = MxResolver::new().expect("resolver init");
        // A burst of concurrent verifications for one refused host: all
        // must agree (ProbeFailed) and the single-flight gate must leave
        // exactly one cached verdict — i.e. they coalesced rather than
        // each racing in its own probe + cache write.
        let calls = (0..8).map(|_| r.verify_host("127.0.0.1"));
        let outcomes = futures_util::future::join_all(calls).await;
        assert!(outcomes.iter().all(|o| *o == Err(MxError::ProbeFailed)));
        assert_eq!(r.probe_cache.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn probe_gate_sweeps_idle_entries_past_soft_cap() {
        ensure_crypto_provider();
        let r = MxResolver::new().expect("resolver init");
        {
            let mut m = r.probe_gate.lock().unwrap();
            for i in 0..(GATE_MAP_SOFT_CAP + 50) {
                m.insert(format!("idle-{i}.example.com"), Arc::default());
            }
            assert!(m.len() > GATE_MAP_SOFT_CAP);
        }
        // Next acquisition triggers the sweep: every seeded entry is
        // map-only (strong_count == 1) so all are dropped; only the
        // freshly requested host's gate survives — growth is bounded to
        // in-flight hosts, not every MX host ever seen.
        let _g = r.probe_gate("live.example.com");
        let m = r.probe_gate.lock().unwrap();
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("live.example.com"));
    }

    #[tokio::test]
    async fn probe_host_fails_when_a_port_is_unreachable() {
        ensure_crypto_provider();
        let tls = build_client_config();
        // 993/465 on loopback are unbound in the test sandbox, so the
        // try_join short-circuits on the first ECONNREFUSED — exercises
        // the real two-port composition fast (refused, not the 5s
        // deadline) and proves a non-terminating MX → ProbeFailed.
        let r = probe_host(&tls, "127.0.0.1").await;
        assert_eq!(r, Err(MxError::ProbeFailed));
    }

    #[test]
    fn classify_only_caches_genuine_negative_answers() {
        use hickory_resolver::proto::ProtoError;
        use hickory_resolver::proto::op::ResponseCode;
        let mk = |rc: ResponseCode| {
            // hickory wraps *every* error response code in the single
            // NoRecordsFound variant; only the inner response_code
            // distinguishes them.
            let pe = ProtoError::nx_error(Box::default(), None, None, None, rc, false, None);
            let re: hickory_resolver::ResolveError = pe.into();
            classify(&re)
        };
        // Genuine "no MX" → cacheable 404.
        assert_eq!(mk(ResponseCode::NXDomain), MxError::NoMx);
        assert_eq!(mk(ResponseCode::NoError), MxError::NoMx);
        // Transient server/transport faults funnelled through the same
        // variant → uncached 503, never a 404 pinned for the TTL.
        assert_eq!(mk(ResponseCode::ServFail), MxError::Transient);
        assert_eq!(mk(ResponseCode::Refused), MxError::Transient);
        assert_eq!(mk(ResponseCode::FormErr), MxError::Transient);
    }
}
