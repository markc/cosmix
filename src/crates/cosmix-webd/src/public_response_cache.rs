//! Per-vhost cache of rendered ANONYMOUS public responses.
//!
//! The authed-request win is [`crate::current_session_epoch`]'s epoch cache;
//! this is the sibling for the *unauthenticated* content path — the blog
//! index, posts, pages, archives — where 100 concurrent anonymous readers
//! currently re-render + re-query per request. A cache hit here is one render
//! plus N replays.
//!
//! ## Why it is opt-in per route (`public_cache` capability), not automatic
//!
//! An anonymous Mix-rendered page is NOT automatically safe to cache. The
//! render pipeline injects a per-visitor `$CSRF` token and, on first contact,
//! sets a `cosmix_csrf` cookie; a route that embeds either is personalised and
//! must never be shared. Caching is therefore gated on an explicit
//! `public_cache` capability that declares a route's anonymous SSR is a pure
//! function of `(host, path, query)` — no cookie, `$CSRF`, `Authorization`,
//! `Accept*` or `User-Agent` variance. `serve_static` enforces the neutrality
//! by STRIPPING the incoming `Cookie` header and passing `csrf: None` for a
//! cacheable render, and refuses to STORE any response that carries
//! `Set-Cookie`, a `private`/`no-store` cache directive, or a non-200 status.
//!
//! ## Invalidation
//!
//! A per-vhost generation counter is bumped (and the map cleared) whenever a
//! non-safe HTTP method completes against the vhost (`host_router`). A render
//! that began under generation G is only stored if the generation is still G at
//! store time (a mutation that raced the render drops the would-be-stale entry).
//! A short TTL bounds any staleness the counter can't catch (a direct DB edit,
//! or — on a multi-node vhost — a mutation that landed on a different node).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body::Body as _; // `size_hint` on the response body
use tokio::sync::{Mutex, OwnedMutexGuard};

/// How long a stored entry may be served before a re-render. Short on purpose:
/// the generation counter makes same-node invalidation immediate, so the TTL
/// only bounds an out-of-band DB edit or a cross-node mutation.
pub(crate) const TTL: Duration = Duration::from_secs(5);

/// Largest body we will cache. A page over this renders normally every time
/// (never stored) — the cache is for the small hot content pages (a blog index
/// is ~50 KB), not bulk exports (e.g. an unbounded `?size=all`). Paired with
/// `MAX_ENTRIES` this HARD-bounds resident cache memory to 64 MiB/vhost with no
/// per-byte accounting (256 × 256 KiB).
pub(crate) const MAX_CACHED_BODY: usize = 256 * 1024;

/// Hard cap on distinct cached keys per vhost. A real content site has far
/// fewer live URLs; the cap bounds an ANONYMOUS key-enumeration fill (a fresh
/// `?x=<random>` or `/post/<random>` per request → a new key each time) into a
/// memory DoS. When the map is full, expired/unreferenced entries are reaped
/// (at most once per `REAP_COOLDOWN`); if it is STILL full, a NEW key is served
/// UNCACHED rather than stored, so the map can never exceed this many entries
/// and worst-case memory is `MAX_ENTRIES * MAX_CACHED_BODY` = 64 MiB.
pub(crate) const MAX_ENTRIES: usize = 256;

/// Minimum spacing between reap scans when the map is full: an attacker forcing
/// overflow can trigger at most one O(MAX_ENTRIES) scan per this interval;
/// between scans a new key is refused (served uncached) with no scan. Short
/// enough that legitimately-new content becomes cacheable within a beat.
const REAP_COOLDOWN: Duration = Duration::from_secs(1);

/// Response headers we refuse to replay from cache (recomputed per response or
/// visitor-specific). Everything else the handler set (Content-Type, a public
/// Cache-Control, etc.) is stored and replayed verbatim.
const SKIP_STORE_HEADERS: &[HeaderName] = &[header::SET_COOKIE];

/// Cache key. Host is included because a handler may vary on `$REQUEST.host`
/// (canonical URLs, `og:url`) and aliases share one `VhostState`/cache. The
/// `public_cache` contract forbids any OTHER request-derived variance, so
/// nothing else belongs in the key.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Key {
    pub host: String,
    pub path_and_query: String,
}

/// A stored rendered response plus the metadata to judge its freshness.
#[derive(Clone)]
pub(crate) struct Entry {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    stored_at: Instant,
    generation: u64,
}

impl Entry {
    /// Reconstruct a fresh `Response` from the stored bytes (each hit gets its
    /// own body; the stored `Bytes` is a cheap refcount clone).
    fn to_response(&self) -> Response {
        let mut resp = Response::builder()
            .status(self.status)
            .body(Body::from(self.body.clone()))
            .expect("valid cached response");
        *resp.headers_mut() = self.headers.clone();
        resp
    }
}

type Slot = Arc<Mutex<Option<Entry>>>;

/// Map + reap bookkeeping, guarded together so the cooldown check and the reap
/// are one critical section (no separate lock, no ordering concern).
#[derive(Default)]
struct Inner {
    slots: HashMap<Key, Slot>,
    /// When the last full-map reap ran; `None` until the first. Bounds reap
    /// frequency under an overflow attack (see `REAP_COOLDOWN`).
    last_reap: Option<Instant>,
}

/// Per-vhost public-response cache. Cheap-default; lives on `VhostState`.
#[derive(Default)]
pub(crate) struct Cache {
    generation: AtomicU64,
    inner: Mutex<Inner>,
}

impl Cache {
    /// The current invalidation generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Bump the generation and drop every stored entry. Called after any
    /// non-safe method completes against the vhost — a superset invalidation
    /// (a POST to one path clears the whole vhost) that is correct and cheap
    /// for content-management writes.
    pub(crate) async fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let mut inner = self.inner.lock().await;
        inner.slots.clear();
        inner.last_reap = None;
    }

    /// Read the current generation and lock the single-flight slot for `key`,
    /// or `None` when the map is full of live entries and `key` is new (the
    /// caller then serves the response UNCACHED). The outer map lock is held
    /// only for the slot lookup, never across the render. The returned
    /// generation is captured BEFORE the render so a racing invalidation is
    /// detected at store time.
    pub(crate) async fn lock_slot(
        &self,
        key: Key,
    ) -> Option<(u64, OwnedMutexGuard<Option<Entry>>)> {
        // Read the generation first: if it advances during the render, store
        // will see the newer value and drop the (now-stale) entry.
        let generation = self.generation();
        let slot = {
            let mut inner = self.inner.lock().await;
            // An EXISTING key always resolves (a hit/refresh — no growth). A
            // NEW key on a full map triggers a reap of unreferenced stale
            // entries, but AT MOST once per `REAP_COOLDOWN` (an attacker forcing
            // overflow can't make every request pay the O(MAX_ENTRIES) scan);
            // if still full after (or when the cooldown skips) the reap, refuse
            // → the caller serves uncached. Bounds the map hard.
            if !inner.slots.contains_key(&key) && inner.slots.len() >= MAX_ENTRIES {
                let now = Instant::now();
                let due = inner
                    .last_reap
                    .is_none_or(|t| now.duration_since(t) >= REAP_COOLDOWN);
                if due {
                    inner.last_reap = Some(now);
                    inner.slots.retain(|_, s| {
                        // Keep a slot another request references (in-flight).
                        if Arc::strong_count(s) > 1 {
                            return true;
                        }
                        // Unreferenced: keep only a still-fresh entry; drop
                        // empty/expired. `try_lock` succeeds (strong_count == 1).
                        match s.try_lock() {
                            Ok(g) => g.as_ref().is_some_and(|e| e.stored_at.elapsed() <= TTL),
                            Err(_) => true,
                        }
                    });
                }
                if inner.slots.len() >= MAX_ENTRIES {
                    return None;
                }
            }
            inner
                .slots
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        Some((generation, slot.lock_owned().await))
    }

    /// If the guard holds a still-fresh entry (same generation, within TTL),
    /// return its replay response.
    pub(crate) fn hit(&self, guard: &OwnedMutexGuard<Option<Entry>>) -> Option<Response> {
        let entry = guard.as_ref()?;
        if entry.generation == self.generation() && entry.stored_at.elapsed() <= TTL {
            Some(entry.to_response())
        } else {
            None
        }
    }
}

/// Collect a rendered response, store it into `guard` iff it is cacheable, and
/// return a reconstructed response either way (the body is consumed to buffer
/// it, so the return value is rebuilt from the buffered bytes).
///
/// `render_generation` is the generation captured at `lock_slot` time; the
/// entry is stored ONLY if the cache generation is still that value — a
/// mutation that raced this render bumped it, so the freshly-rendered bytes may
/// predate the write and must not be cached.
pub(crate) async fn store_and_respond(
    cache: &Cache,
    render_generation: u64,
    guard: &mut OwnedMutexGuard<Option<Entry>>,
    resp: Response,
) -> Response {
    let (parts, body) = resp.into_parts();
    // The cache must NEVER turn a successfully-rendered response into an error.
    // The Mix handler produces a fully-buffered body (bounded by the 4 MiB
    // response-string cap), but a `SharedBuf`/repeated-`print()` render can
    // aggregate more — so if the body's declared upper bound is ABSENT or above
    // our collect ceiling, serve it AS-IS, uncached, without buffering. Only a
    // body we can safely buffer is a caching candidate; `MAX_CACHED_BODY` then
    // gates STORING, never serving.
    const COLLECT_CEILING: u64 = 8 * 1024 * 1024;
    let within_ceiling = body
        .size_hint()
        .upper()
        .is_some_and(|u| u <= COLLECT_CEILING);
    if !within_ceiling {
        return Response::from_parts(parts, body);
    }
    let bytes = match axum::body::to_bytes(body, COLLECT_CEILING as usize).await {
        Ok(b) => b,
        Err(_) => {
            // Unreachable given the size-hint gate above (a Full body's hint is
            // exact). If a lying custom body impl ever reached here, a 500 is a
            // safer signal than silently serving a truncated page as 200.
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "response body collection exceeded its declared bound",
            )
                .into_response();
        }
    };

    let cacheable = parts.status == StatusCode::OK
        && bytes.len() <= MAX_CACHED_BODY
        && !parts.headers.contains_key(header::SET_COOKIE)
        // A `Vary` header is an explicit statement that the response depends on
        // request headers our key does NOT contain — refuse to shared-cache it.
        && !parts.headers.contains_key(header::VARY)
        && !cache_control_forbids_shared(&parts.headers)
        && cache.generation() == render_generation;

    if cacheable {
        let mut stored_headers = parts.headers.clone();
        for h in SKIP_STORE_HEADERS {
            stored_headers.remove(h);
        }
        **guard = Some(Entry {
            status: parts.status,
            headers: stored_headers,
            body: bytes.clone(),
            stored_at: Instant::now(),
            generation: render_generation,
        });
    }

    // Rebuild the response from the buffered bytes regardless of cacheability.
    Response::from_parts(parts, Body::from(bytes))
}

/// Any `Cache-Control` value forbids a SHARED cache storing this response:
/// `private`, `no-store`, or `no-cache`. Iterates ALL header instances (a
/// response may carry several) so `public` followed by `private` still fails.
fn cache_control_forbids_shared(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|cc| {
            let cc = cc.to_ascii_lowercase();
            cc.contains("private") || cc.contains("no-store") || cc.contains("no-cache")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> Key {
        Key {
            host: "example.test".into(),
            path_and_query: path.into(),
        }
    }

    fn resp(status: StatusCode, body: &str, headers: &[(HeaderName, &str)]) -> Response {
        let mut b = Response::builder().status(status);
        for (k, v) in headers {
            b = b.header(k, *v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    /// Store a response into the slot for `path` under `generation`, returning the
    /// held guard so a test can assert what (if anything) was stored.
    async fn store_into(
        cache: &Cache,
        path: &str,
        generation: u64,
        r: Response,
    ) -> OwnedMutexGuard<Option<Entry>> {
        let (_g, mut guard) = cache.lock_slot(key(path)).await.unwrap();
        let _ = store_and_respond(cache, generation, &mut guard, r).await;
        guard
    }

    #[tokio::test]
    async fn stores_and_hits_a_plain_200() {
        let cache = Cache::default();
        let (generation, mut guard) = cache.lock_slot(key("/")).await.unwrap();
        let out = store_and_respond(
            &cache,
            generation,
            &mut guard,
            resp(
                StatusCode::OK,
                "<p>hi</p>",
                &[(header::CONTENT_TYPE, "text/html")],
            ),
        )
        .await;
        assert_eq!(out.status(), StatusCode::OK);
        let hit = cache.hit(&guard).expect("a fresh 200 is a hit");
        assert_eq!(hit.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn does_not_store_non_200() {
        let cache = Cache::default();
        let guard = store_into(&cache, "/", 0, resp(StatusCode::NOT_FOUND, "nope", &[])).await;
        assert!(guard.is_none(), "a 404 is never stored");
    }

    #[tokio::test]
    async fn does_not_store_set_cookie_response() {
        let cache = Cache::default();
        let guard = store_into(
            &cache,
            "/",
            0,
            resp(StatusCode::OK, "x", &[(header::SET_COOKIE, "a=b")]),
        )
        .await;
        assert!(
            guard.is_none(),
            "a Set-Cookie response must never be cached"
        );
    }

    #[tokio::test]
    async fn does_not_store_private_cache_control() {
        let cache = Cache::default();
        for cc in ["private", "no-store", "no-cache, max-age=0"] {
            let guard = store_into(
                &cache,
                "/",
                0,
                resp(StatusCode::OK, "x", &[(header::CACHE_CONTROL, cc)]),
            )
            .await;
            assert!(
                guard.is_none(),
                "Cache-Control: {cc} must not be shared-cached"
            );
        }
    }

    #[tokio::test]
    async fn does_not_store_vary_response() {
        let cache = Cache::default();
        let guard = store_into(
            &cache,
            "/",
            0,
            resp(StatusCode::OK, "x", &[(header::VARY, "Accept-Language")]),
        )
        .await;
        assert!(
            guard.is_none(),
            "a Vary response declares unkeyed variance and must not be shared-cached"
        );
    }

    #[tokio::test]
    async fn does_not_store_if_generation_advanced_during_render() {
        let cache = Cache::default();
        let (generation, mut guard) = cache.lock_slot(key("/")).await.unwrap(); // generation 0
        cache.invalidate().await; // a mutation raced the render: generation -> 1
        let _ = store_and_respond(
            &cache,
            generation,
            &mut guard,
            resp(StatusCode::OK, "stale", &[]),
        )
        .await;
        assert!(
            guard.is_none(),
            "a render that predates a mutation is not stored"
        );
    }

    #[tokio::test]
    async fn oversized_body_is_served_in_full_but_not_stored() {
        let cache = Cache::default();
        let big = "a".repeat(MAX_CACHED_BODY + 10);
        let (generation, mut guard) = cache.lock_slot(key("/big")).await.unwrap();
        let out = store_and_respond(
            &cache,
            generation,
            &mut guard,
            resp(StatusCode::OK, &big, &[]),
        )
        .await;
        assert!(guard.is_none(), "a body over the cache limit is not stored");
        assert_eq!(
            out.status(),
            StatusCode::OK,
            "but it is still served, not 500'd"
        );
        let served = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            served.len(),
            MAX_CACHED_BODY + 10,
            "served in full, untruncated"
        );
    }

    #[tokio::test]
    async fn invalidate_turns_a_prior_hit_into_a_miss() {
        let cache = Cache::default();
        {
            let (generation, mut guard) = cache.lock_slot(key("/")).await.unwrap();
            store_and_respond(
                &cache,
                generation,
                &mut guard,
                resp(StatusCode::OK, "v1", &[]),
            )
            .await;
            assert!(
                cache.hit(&guard).is_some(),
                "stored entry hits before invalidate"
            );
        }
        cache.invalidate().await;
        let (_g, guard) = cache.lock_slot(key("/")).await.unwrap();
        assert!(
            cache.hit(&guard).is_none(),
            "post-invalidate lookup is a miss (map cleared + generation bumped)"
        );
    }

    #[tokio::test]
    async fn full_map_refuses_new_keys_but_still_serves_existing() {
        let cache = Cache::default();
        // Fill the map to the cap with live (fresh) entries.
        for i in 0..MAX_ENTRIES {
            let (g, mut guard) = cache.lock_slot(key(&format!("/p{i}"))).await.unwrap();
            store_and_respond(&cache, g, &mut guard, resp(StatusCode::OK, "x", &[])).await;
        }
        assert_eq!(cache.inner.lock().await.slots.len(), MAX_ENTRIES);

        // A NEW key is refused (map full of live entries) → caller serves uncached.
        assert!(
            cache.lock_slot(key("/overflow")).await.is_none(),
            "a new key on a full live map is refused (no unbounded growth)"
        );
        // An EXISTING key still resolves (real content stays cached).
        assert!(
            cache.lock_slot(key("/p0")).await.is_some(),
            "an existing key always resolves even when the map is full"
        );
        // The map never exceeded the cap.
        assert!(cache.inner.lock().await.slots.len() <= MAX_ENTRIES);
    }

    #[tokio::test]
    async fn distinct_keys_do_not_collide() {
        let cache = Cache::default();
        let (g1, mut s1) = cache.lock_slot(key("/a")).await.unwrap();
        store_and_respond(&cache, g1, &mut s1, resp(StatusCode::OK, "A", &[])).await;
        drop(s1);
        let (_g2, s2) = cache.lock_slot(key("/b")).await.unwrap();
        assert!(
            cache.hit(&s2).is_none(),
            "a different path is a separate slot"
        );
    }
}
