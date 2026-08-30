//! Embedded (trusted) Mix HTTP handlers for webd — slice #3 of the
//! maild/webd trust-split ADR (`_decisions/2026-06-04-maild-webd-trust-split.md`).
//!
//! Generalises maild's proven inbound-filter embed
//! (`cosmix-maild/src/smtp/inbound.rs`) from SMTP to HTTP: an
//! operator-authored `.mix` script under a vhost's `www_dir` serves a
//! request, dispatched by the declarative `webd.handlers` SPEC-12
//! namespace ([`crate::handlers_namespace`]).
//!
//! ## Trust + sandbox
//!
//! Handler scripts are **trusted/operator-authored** (ADR: in-process
//! embed is trusted-only; untrusted multi-tenant code is slice #4's
//! out-of-process worker pool). Even so, the embed applies the
//! cosmix-lib-mix 0.14 self-protection knobs so a buggy handler can't
//! take the daemon down:
//!
//! * **Capability gate** — `CategoryAllowList::new(&[FsRead])` allows
//!   `Pure` + `FsRead` only; `FsWrite`/`Process`/`Env` are always denied,
//!   and `Network` is denied unless the matched route declares the `net`
//!   capability. This covers builtins *and* Mix's shell syntax (`sh "…"`,
//!   `$(…)` substitution, `… | cmd` pipes), which spawn `/bin/sh` and
//!   are gated as `Process` (requires `cosmix-lib-mix ≥ 0.14.1` — see
//!   `CapabilityPolicy::check_class`). No Bus handler is installed, so
//!   `send`/`emit` are inert.
//! * **Recursion cap** — `DEFAULT_RECURSION_LIMIT` (16). webd handlers
//!   run on tokio's ~2 MB blocking threads; the async call path burns
//!   tens of KB/level, so 16 is the reliably-safe default — do NOT
//!   raise it here (unlike the `mix` binary's 128 on its 8 MB main
//!   thread). A runaway recursion returns a clean error, not a SIGSEGV.
//! * **Wall-clock deadline** — 5 s, bounding loop-based runaways.
//! * **Collection-size caps** — bound the response a handler can build.
//!
//! ## `!Send` discipline
//!
//! The Mix evaluator and `Value` are `!Send` (`Value::Function`
//! carries an `Rc`). All `Value` construction and consumption happens
//! **inside** the `spawn_blocking` closure; only the plain
//! [`MixResponse`] (raw body bytes + a status code) crosses back to the async
//! caller. The parsed AST (`Vec<Stmt>`) *is* `Send + Sync` (plain data,
//! no `Rc`), so it is cached in an `Arc` on `NodeState` and shared.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use cosmix_mix::ast::Stmt;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{
    CapabilityClass, CapabilityPolicy, CategoryAllowList, DEFAULT_RECURSION_LIMIT, EvalLimits,
};

use crate::handlers_namespace::HandlerRow;

/// Capability policy for embedded handlers: `Pure` + `FsRead` (and
/// `Db` for a `db`-capable route), with `sleep` denied.
///
/// `sleep` is classed `Pure`, so the class gate alone would allow it —
/// but in webd's build (no `tokio-sleep` feature) it is a blocking
/// `std::thread::sleep` that the per-statement deadline cannot interrupt
/// mid-call, so a handler calling `sleep(3600)` would pin a
/// `spawn_blocking` thread far past the 5 s budget (a thread-pool
/// exhaustion vector). The capability check runs *before* the sleep, so
/// denying it returns an error immediately. A request handler has no
/// legitimate reason to sleep.
///
/// `Db` is granted only when the route opted into the `db` capability
/// (`capabilities = ["db"]`); likewise `Jmap` only when the route opted
/// into `jmap`, and `Network` only when it opted into `net` (outbound
/// http builtins for payment-gateway/registrar handlers). `FsWrite`/
/// `Process`/`Env` (and the shell syntax they gate, via
/// `check_class(Process)`) stay denied regardless — note `Jmap` is its
/// own class, distinct from `Network`, so a `jmap` route still cannot
/// make arbitrary outbound HTTP without its own `net` grant.
struct HandlerCapabilityPolicy {
    inner: CategoryAllowList,
}

impl HandlerCapabilityPolicy {
    fn new(wants_db: bool, wants_jmap: bool, wants_bus: bool, wants_net: bool) -> Self {
        let mut classes = vec![CapabilityClass::FsRead];
        if wants_db {
            classes.push(CapabilityClass::Db);
        }
        if wants_jmap {
            classes.push(CapabilityClass::Jmap);
        }
        // `Bus` (the delegated `bus_call` seam) is granted only when the route
        // declared `bus:<verb>` grants AND webd's admin/CSRF gate passed (the
        // caller injects no handler otherwise, so the class would be inert) —
        // see `run_with_limits`.
        if wants_bus {
            classes.push(CapabilityClass::Bus);
        }
        // `net` — outbound HTTP builtins (payment gateways, registrar APIs).
        // Per-route opt-in only; the 5s handler deadline still bounds calls.
        if wants_net {
            classes.push(CapabilityClass::Network);
        }
        Self {
            inner: CategoryAllowList::new(&classes),
        }
    }
}

impl CapabilityPolicy for HandlerCapabilityPolicy {
    fn check_builtin(&self, name: &str) -> Result<(), String> {
        if name == "sleep" {
            return Err(
                "sleep is not permitted in a handler (it would block past the deadline)".into(),
            );
        }
        self.inner.check_builtin(name)
    }

    fn check_class(&self, class: CapabilityClass) -> Result<(), String> {
        self.inner.check_class(class)
    }
}

/// Request-body read cap. A handler can't be forced to buffer an
/// unbounded body; the trusted path's DoS edge.
const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB
/// Per-request wall-clock budget for handler evaluation.
const HANDLER_TIME_LIMIT_SECS: u64 = 5;
/// Cap on any single list/map a handler builds.
const MAX_COLLECTION_LEN: usize = 100_000;
/// Cap on any single string a handler builds — bounds the response body.
const MAX_RESPONSE_STRING_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
/// Default content type for string / stdout responses.
const DEFAULT_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// Path → (mtime, parsed AST). Lives on `NodeState`; filled lazily and
/// invalidated by mtime change. `Arc<Vec<Stmt>>` is shareable because
/// the AST is `Send + Sync`.
/// Inner map of the [`AstCache`]: resolved script path → (mtime, AST).
type AstMap = HashMap<PathBuf, (SystemTime, Arc<Vec<Stmt>>)>;
pub type AstCache = Mutex<AstMap>;

/// Construct an empty AST cache.
pub fn new_ast_cache() -> Arc<AstCache> {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Lock the cache, recovering from poisoning (our critical sections
/// never panic, so a poisoned lock should never happen — but a request
/// handler must not turn one into a process abort).
fn lock_cache(cache: &AstCache) -> MutexGuard<'_, AstMap> {
    cache.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Compiled route table
// ---------------------------------------------------------------------------

/// How a stored `path_pattern` matches a request path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PathMatcher {
    /// Exact match: request path equals the pattern.
    Exact(String),
    /// Single trailing-glob: request path starts with this prefix
    /// (the pattern `"/blog/*"` becomes `Prefix("/blog/")`, `"/*"`
    /// becomes `Prefix("/")`).
    Prefix(String),
}

impl PathMatcher {
    fn compile(path_pattern: &str) -> Self {
        // Only a trailing `/*` is a glob (matching the namespace's
        // `validate_path_pattern`). Strip `/*` and re-add the `/` so
        // `/blog/*` → `Prefix("/blog/")` and `/*` → `Prefix("/")`.
        // Anything else — including a malformed `/blog*` that bypassed
        // validation (e.g. a row from an older daemon) — is treated as
        // `Exact`, which fails safe (it can only match the literal
        // string, never an unintended prefix).
        if let Some(prefix) = path_pattern.strip_suffix("/*") {
            PathMatcher::Prefix(format!("{prefix}/"))
        } else {
            PathMatcher::Exact(path_pattern.to_string())
        }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            PathMatcher::Exact(p) => path == p,
            PathMatcher::Prefix(pre) => path.starts_with(pre.as_str()),
        }
    }
}

/// One compiled handler row.
#[derive(Clone, Debug)]
struct CompiledHandler {
    /// The route's id (the `webd.handlers` primary key) — carried so the
    /// delegated-Bus envelope can attribute a call to its route.
    route_id: String,
    /// Uppercase method, or `"ANY"`.
    method: String,
    matcher: PathMatcher,
    /// Path relative to the vhost `www_dir` (re-resolved + traversal-
    /// guarded per request against the live `www_dir`).
    handler_ref: String,
    /// The route opted into the `db` capability (`capabilities` contains
    /// `"db"`) → the per-vhost DB is injected and `Db` is allowed.
    wants_db: bool,
    /// The route opted into the `jmap` capability (`capabilities`
    /// contains `"jmap"`) → the vhost's maild upstream is injected and
    /// `Jmap` is allowed.
    wants_jmap: bool,
    /// The route opted into the `public_read` capability → on an ANONYMOUS
    /// request (no session, no dev_session) the vhost's `public_read_*` content
    /// credential may back `jmap()`. ONLY routes that declare this can consume
    /// the public credential, so the credential is bounded to audited routes
    /// (not vhost-wide across every `jmap` handler).
    wants_public_read: bool,
    /// The route opted into `public_cache` AND is a GET with no delegated-Bus
    /// grants → its ANONYMOUS render may be cached + replayed. Computed
    /// fail-closed in `from_rows`: a stored `public_cache` grant on a non-GET
    /// row, or one carrying `bus:` verbs, is IGNORED (logged), never cached.
    wants_public_cache: bool,
    /// The route opted into the `net` capability (`capabilities` contains
    /// `"net"`) → `CapabilityClass::Network` builtins (http_get/http_post/
    /// http_request/dns_lookup) are allowed. For handlers that talk to
    /// external HTTP APIs (payment gateways, registrars) — grant per audited
    /// route, never vhost-wide.
    wants_net: bool,
    /// The EXACT delegated-Bus verbs the route may call via `bus_call`, parsed
    /// from its `bus:<verb>` capability grants (vtoken C2). Non-empty ⇒ the
    /// `Bus` capability is granted and the `WebdBusCallHandler` is injected,
    /// bounded to this set. Shared via `Arc` (cheap per-request clone).
    bus_verbs: Arc<BTreeSet<String>>,
    /// The Bus service the `bus_verbs` route to (`bus-svc:<service>` capability,
    /// else derived from the verbs' shared prefix). Empty ⇔ `bus_verbs` empty.
    bus_service: String,
    /// The exact database (schema) names this route may access via the injected
    /// connection: `main` when it holds the plain `db` cap, plus one entry per
    /// `db-schema:<name>` grant. The `WebdDbHandler` authorizer denies any schema
    /// outside this set, so a `db-schema`-only route can't reach `main` and a
    /// co-tenant `db` route can't reach an aux schema. Shared via `Arc`.
    allowed_dbs: Arc<BTreeSet<String>>,
    /// A `wake:<verb>` grant: an authority-free accelerator wake webd fires
    /// ITSELF, best-effort, after this route returns a non-error (`< 400`)
    /// response for an authenticated session — `(verb, service)`,
    /// service derived from the verb prefix. Decoupled from `bus_service` so a
    /// route pinned to (e.g.) `filesd-fs` for its real work can still nudge a
    /// different daemon's queue drain. `None` unless a valid grant was parsed.
    wake: Option<(String, String)>,
}

impl CompiledHandler {
    fn method_matches(&self, request_method: &str) -> bool {
        self.method == "ANY" || self.method == request_method
    }
}

/// A matched route, returned guard-free from [`HandlerTable::lookup`]
/// (cloned so no `ArcSwap` load guard is held across the handler await).
#[derive(Clone, Debug)]
pub struct MatchedHandler {
    /// The matched route's id (for the delegated-Bus envelope's `route_id`).
    pub route_id: String,
    /// Script path relative to the vhost `www_dir`.
    pub handler_ref: String,
    /// Whether the route requested the `db` capability.
    pub wants_db: bool,
    /// Whether the route requested the `jmap` capability.
    pub wants_jmap: bool,
    /// Whether the route opted into the `public_read` content credential
    /// (anonymous Basic-read fallback as the vhost's dedicated content account).
    pub wants_public_read: bool,
    /// Whether the route's anonymous render may be cached (`public_cache`,
    /// GET-only, bus-free — computed fail-closed in `from_rows`).
    pub wants_public_cache: bool,
    /// Whether the route requested the `net` capability (outbound HTTP builtins).
    pub wants_net: bool,
    /// The exact delegated-Bus verbs this route may call (`bus:<verb>` grants).
    /// Non-empty ⇒ the `Bus` capability + `WebdBusCallHandler` are wired.
    pub bus_verbs: Arc<BTreeSet<String>>,
    /// The Bus service those verbs route to (e.g. `maild`, `filesd-notes`).
    pub bus_service: String,
    /// Database (schema) names this route may access — `main` (with `db`) plus
    /// each `db-schema:<name>` grant; passed to `WebdDbHandler`'s authorizer.
    pub allowed_dbs: Arc<BTreeSet<String>>,
    /// A validated `wake:<verb>` grant `(verb, service)` — an authority-free
    /// accelerator wake webd fires itself, best-effort, after a non-error
    /// response for an authenticated session. `None`
    /// unless the route carried a valid grant.
    pub wake: Option<(String, String)>,
}

/// Per-vhost compiled handler table, hot-swapped behind an `ArcSwap` on
/// `NodeState`.
#[derive(Clone, Debug, Default)]
pub struct HandlerTable {
    by_vhost: HashMap<String, Vec<CompiledHandler>>,
}

impl HandlerTable {
    /// Build from a `webd.handlers` namespace snapshot. Skips disabled
    /// rows and non-`mix` kinds; de-duplicates the `(method,
    /// path_pattern)` dispatch tuple per vhost (warn + first-wins,
    /// since storage only enforces `route_id` uniqueness); sorts each
    /// vhost's list most-specific-first (exact before glob; concrete
    /// method before `ANY`; longer prefix before shorter) so a linear
    /// first-match scan is deterministic.
    pub fn from_rows(rows: Vec<HandlerRow>) -> Self {
        let mut by_vhost: HashMap<String, Vec<CompiledHandler>> = HashMap::new();
        // Per-vhost seen (method, path_pattern) for de-dup.
        let mut seen: HashMap<String, std::collections::HashSet<(String, String)>> = HashMap::new();

        for row in rows {
            if !row.enabled {
                continue;
            }
            if row.handler_kind != "mix" {
                tracing::debug!(
                    target: "webd::mix_handler",
                    route_id = %row.route_id,
                    kind = %row.handler_kind,
                    "skipping non-mix handler kind (not wired in slice #3)",
                );
                continue;
            }
            let method = row.method.to_ascii_uppercase();
            let dedup_key = (method.clone(), row.path_pattern.clone());
            let vhost_seen = seen.entry(row.vhost_fqdn.clone()).or_default();
            if !vhost_seen.insert(dedup_key) {
                tracing::warn!(
                    target: "webd::mix_handler",
                    route_id = %row.route_id,
                    vhost = %row.vhost_fqdn,
                    method = %method,
                    path_pattern = %row.path_pattern,
                    "duplicate (method, path_pattern) handler tuple — keeping the \
                     first-seen row, ignoring this one. Storage enforces only route_id \
                     uniqueness; resolve the duplicate to silence this warning.",
                );
                continue;
            }
            let has_plain_db = row.capabilities.iter().any(|c| c == "db");
            let wants_jmap = row.capabilities.iter().any(|c| c == "jmap");
            let wants_public_read = row.capabilities.iter().any(|c| c == "public_read");
            let wants_net = row.capabilities.iter().any(|c| c == "net");
            // `db-schema:<name>` grants: aux schemas this route may reach. A
            // stored row could carry a malformed name (pre-validation / tooling
            // bug); filter to valid identifiers, mirroring the bus-grant guard.
            let mut allowed_dbs: BTreeSet<String> = row
                .capabilities
                .iter()
                .filter_map(|c| c.strip_prefix("db-schema:"))
                .filter(|s| {
                    let ok = crate::is_valid_schema_name(s);
                    if !ok {
                        tracing::warn!(
                            target: "webd::mix_handler",
                            route_id = %row.route_id,
                            schema = %s,
                            "ignoring an invalid db-schema grant from a stored handler row",
                        );
                    }
                    ok
                })
                .map(str::to_string)
                .collect();
            // `main` is reachable ONLY with the plain `db` capability. A route
            // holding just `db-schema:X` gets the connection (to reach X) but is
            // authorizer-denied `main`, so it cannot touch CMS/session state.
            if has_plain_db {
                allowed_dbs.insert("main".to_string());
            }
            // Either the plain db cap or an aux-schema grant needs the injected
            // connection.
            let wants_db = has_plain_db || !allowed_dbs.is_empty();
            // Parse the route's exact `bus:<verb>` grants into its verb set.
            // `before_set` validates new writes, but a STORED row could carry a
            // non-delegated-safe `bus:` grant (a pre-allowlist row, or a tooling
            // bug). Filter those out here so a bad persisted grant is inert +
            // noisy (defence-in-depth; the handler re-checks at call time too).
            let bus_verbs: BTreeSet<String> = row
                .capabilities
                .iter()
                .filter_map(|c| c.strip_prefix("bus:"))
                .filter(|v| {
                    let safe = crate::bus_call_handler::is_delegated_safe_bus_verb(v);
                    if !safe {
                        tracing::warn!(
                            target: "webd::bus",
                            route_id = %row.route_id,
                            verb = %v,
                            "ignoring a non-delegated-safe bus grant from a stored handler row",
                        );
                    }
                    safe
                })
                .map(str::to_string)
                .collect();
            // The target Bus service: explicit `bus-svc:<service>` wins, else
            // derive from the verbs' shared prefix (maild compat). If verbs are
            // granted but the service is unresolved (ambiguous prefixes, no
            // explicit), DROP the grants — fail closed rather than send to a
            // guessed/wrong service.
            let explicit_svc = row
                .capabilities
                .iter()
                .find_map(|c| c.strip_prefix("bus-svc:"))
                .map(str::to_string);
            let (bus_verbs, bus_service) = match crate::bus_call_handler::resolve_target_service(
                explicit_svc.as_deref(),
                &bus_verbs,
            ) {
                _ if bus_verbs.is_empty() => (bus_verbs, String::new()),
                Some(svc) => (bus_verbs, svc),
                None => {
                    tracing::warn!(
                        target: "webd::bus",
                        route_id = %row.route_id,
                        "bus grants present but target service unresolved (set bus-svc:<service>); dropping bus capability",
                    );
                    (BTreeSet::new(), String::new())
                }
            };
            // `public_cache` is honoured ONLY on a GET route with NO delegated
            // Bus grants — the response-cache contract is anonymous, side-
            // effect-free reads. A stored grant that violates that (a non-GET
            // row, or one that also carries `bus:` verbs) is ignored + logged,
            // never cached — fail closed against a mis-authored/legacy row.
            let declares_public_cache = row.capabilities.iter().any(|c| c == "public_cache");
            let wants_public_cache =
                declares_public_cache && method == "GET" && bus_verbs.is_empty();
            if declares_public_cache && !wants_public_cache {
                tracing::warn!(
                    target: "webd::mix_handler",
                    route_id = %row.route_id,
                    method = %method,
                    "ignoring public_cache grant on a non-GET or bus-bearing route",
                );
            }
            // `wake:<verb>` — an accelerator wake webd fires itself after a
            // non-error response, independent of `bus-svc:`. Fail closed: only a
            // valid accelerator verb whose service resolves is kept; a bad/legacy
            // grant is dropped and logged.
            let wake_grants: Vec<(String, String)> = row
                .capabilities
                .iter()
                .filter(|c| c.starts_with("wake:"))
                .filter_map(|c| {
                    let parsed = crate::bus_call_handler::parse_wake_capability(c);
                    if parsed.is_none() {
                        tracing::warn!(
                            target: "webd::bus",
                            route_id = %row.route_id,
                            cap = %c,
                            "ignoring an invalid wake grant (not an argument-free accelerator wake verb)",
                        );
                    }
                    parsed
                })
                .collect();
            // A route needs at most one wake. More than one is a mis-authored
            // route: keep the first but say so, rather than silently drop the rest.
            if wake_grants.len() > 1 {
                tracing::warn!(
                    target: "webd::bus",
                    route_id = %row.route_id,
                    count = wake_grants.len(),
                    "route carries multiple wake grants; firing only the first",
                );
            }
            let wake = wake_grants.into_iter().next();
            by_vhost
                .entry(row.vhost_fqdn.clone())
                .or_default()
                .push(CompiledHandler {
                    route_id: row.route_id,
                    method,
                    matcher: PathMatcher::compile(&row.path_pattern),
                    handler_ref: row.handler_ref,
                    wants_db,
                    wants_jmap,
                    wants_public_read,
                    wants_public_cache,
                    wants_net,
                    bus_verbs: Arc::new(bus_verbs),
                    bus_service,
                    allowed_dbs: Arc::new(allowed_dbs),
                    wake,
                });
        }

        for handlers in by_vhost.values_mut() {
            handlers.sort_by_key(specificity_key);
        }

        HandlerTable { by_vhost }
    }

    /// Look up the first matching handler for `(vhost, method, path)`.
    /// `None` falls through to static-file serving.
    pub fn lookup(&self, vhost: &str, method: &str, path: &str) -> Option<MatchedHandler> {
        let handlers = self.by_vhost.get(vhost)?;
        for h in handlers {
            if h.method_matches(method) && h.matcher.matches(path) {
                return Some(MatchedHandler {
                    route_id: h.route_id.clone(),
                    handler_ref: h.handler_ref.clone(),
                    wants_db: h.wants_db,
                    wants_jmap: h.wants_jmap,
                    wants_public_read: h.wants_public_read,
                    wants_public_cache: h.wants_public_cache,
                    wants_net: h.wants_net,
                    bus_verbs: h.bus_verbs.clone(),
                    bus_service: h.bus_service.clone(),
                    allowed_dbs: h.allowed_dbs.clone(),
                    wake: h.wake.clone(),
                });
            }
        }
        None
    }

    /// Total compiled-route count across all vhosts (for startup logging).
    pub fn route_count(&self) -> usize {
        self.by_vhost.values().map(Vec::len).sum()
    }
}

/// Sort key: most-specific first. Exact (0) before Prefix (1); within a
/// class, concrete method (0) before `ANY` (1); among prefixes, longer
/// prefix sorts first (`usize::MAX - len`).
fn specificity_key(h: &CompiledHandler) -> (u8, u8, usize) {
    let method_rank = if h.method == "ANY" { 1 } else { 0 };
    match &h.matcher {
        PathMatcher::Exact(_) => (0, method_rank, 0),
        PathMatcher::Prefix(p) => (1, method_rank, usize::MAX - p.len()),
    }
}

// ---------------------------------------------------------------------------
// Request dispatch / embed
// ---------------------------------------------------------------------------

/// Plain, `Send` response data produced inside the blocking closure and
/// turned into an `axum` `Response` by the async caller.
struct MixResponse {
    status: u16,
    /// `None` → default `text/html` if the body is non-empty, else no
    /// content-type header.
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    /// Raw response bytes. A string return is its UTF-8 bytes; a
    /// `Value::Bytes` return (e.g. `base64_decode` of a stored image)
    /// passes through verbatim so handlers can serve binary (images,
    /// downloads) losslessly — NOT `from_utf8_lossy`, which corrupts it.
    body: Vec<u8>,
}

/// Inputs to wire the `jmap` seam for one handler request. `None` is
/// passed to [`run_with_caps`] when the route didn't opt into `jmap`, OR
/// when it did but the vhost has no `jmap_upstream` configured — in the
/// latter case the `Jmap` capability is still granted (via `wants_jmap`)
/// but no handler is injected, so `jmap()` errors cleanly, mirroring the
/// `db`-capable-but-no-db-connection path.
pub struct JmapCaps {
    /// The vhost's `jmap_upstream` base URL.
    pub upstream: String,
    /// `Bearer <maild-token>` unsealed from the session cookie (Phase 2),
    /// or `None` when the request carries no valid session.
    pub auth: Option<String>,
}

/// Inputs to wire the delegated `bus_call` seam for one handler request
/// (vtoken C2). Built ONLY when the route declared `bus:<verb>` grants AND
/// webd's Rust admin/CSRF gate passed (the caller passes `None` otherwise — a
/// gate failure means no `Bus` capability + no handler). Every field is `Send`
/// — the `Rc<WebdBusCallHandler>` is built INSIDE the blocking closure (`Rc` is
/// `!Send`), and the broker send is bridged onto `main_handle` (the broker WS
/// lives on the main runtime, the eval on an isolated one).
pub struct BusInjection {
    pub broker: crate::bus::subscribe_granter::SharedBrokerHandle,
    pub main_handle: tokio::runtime::Handle,
    pub bus_verbs: Arc<BTreeSet<String>>,
    pub inputs: crate::bus_call_handler::DelegationInputs,
    /// The Bus service the granted verbs route to (from the matched route).
    pub service: String,
    /// The caller has no cookie-backed session (an internal-listener dev_session)
    /// and may therefore reach ONLY an argument-free accelerator wake. See
    /// `bus_call_handler::is_accelerator_wake_verb`.
    pub accelerator_only: bool,
}

/// Run a `mix`-kind handler with the full capability set a matched route
/// requested: the per-vhost DB (`db` + `wants_db`) and/or the maild JMAP
/// upstream (`jmap` + `wants_jmap`), and/or the delegated `bus_call` seam
/// (`bus`, when the admin/CSRF gate passed). The `wants_*` flags widen the
/// capability profile; the injected handles (`db` / `jmap` / `bus`) supply the
/// host authority. A `wants_*` with no matching handle grants the
/// capability but errors cleanly when the script reaches for it.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_caps(
    cache: &Arc<AstCache>,
    www_dir: &Path,
    handler_ref: &str,
    db: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    wants_db: bool,
    // ATTACHed aux-schema grants (`db-schema:<name>`) that scope the injected
    // DB handler's authorizer; empty for the vast majority of routes.
    allowed_dbs: Arc<BTreeSet<String>>,
    jmap: Option<JmapCaps>,
    wants_jmap: bool,
    // The authenticated maild email from the sealed session, if any. Injected
    // to the handler as `$SESSION` (identity, not a secret — never the token).
    session_email: Option<String>,
    // The session identity kind ("maild" | "customer"), if signed in. Injected
    // as `$SESSION.kind`; Mix caps a "customer" session at rank-1.
    session_kind: Option<String>,
    // The double-submit CSRF token for this request, injected to the handler as
    // `$CSRF` (the value the caller mirrored into the readable `cosmix_csrf`
    // cookie). Lets a Mix-rendered form (e.g. the login modal) embed a hidden
    // `csrf` field that matches the cookie — CSRF generation stays Rust-owned;
    // the handler only consumes the token.
    csrf: Option<String>,
    // The delegated `bus_call` injection, or `None` (route didn't grant `bus:*`
    // OR the admin/CSRF gate failed). `Some` ⇒ `Bus` capability + handler wired.
    bus: Option<BusInjection>,
    // The route opted into the `net` capability → outbound HTTP builtins allowed.
    wants_net: bool,
    req: Request,
) -> Response {
    run_with_limits(
        cache,
        www_dir,
        handler_ref,
        req,
        default_limits(),
        db,
        wants_db,
        allowed_dbs,
        jmap,
        wants_jmap,
        session_email,
        session_kind,
        csrf,
        bus,
        wants_net,
    )
    .await
}

/// Default per-request evaluator limits.
fn default_limits() -> EvalLimits {
    EvalLimits {
        recursion_limit: DEFAULT_RECURSION_LIMIT,
        time_limit: Some(Duration::from_secs(HANDLER_TIME_LIMIT_SECS)),
        max_list_len: Some(MAX_COLLECTION_LEN),
        max_map_len: Some(MAX_COLLECTION_LEN),
        max_string_len: Some(MAX_RESPONSE_STRING_BYTES),
    }
}

/// Core embed, parameterised by `limits` (tests pass a short deadline)
/// and the optional per-vhost `db` connection + `wants_db` capability,
/// plus the optional `jmap` upstream + `wants_jmap` capability.
#[allow(clippy::too_many_arguments)]
async fn run_with_limits(
    cache: &Arc<AstCache>,
    www_dir: &Path,
    handler_ref: &str,
    req: Request,
    limits: EvalLimits,
    db: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    wants_db: bool,
    allowed_dbs: Arc<BTreeSet<String>>,
    jmap: Option<JmapCaps>,
    wants_jmap: bool,
    session_email: Option<String>,
    session_kind: Option<String>,
    csrf: Option<String>,
    bus: Option<BusInjection>,
    wants_net: bool,
) -> Response {
    // Resolve + traversal-guard the script path against the live
    // www_dir (defence in depth: before_set validated the stored
    // handler_ref, this re-checks the resolved absolute path including
    // symlink escapes).
    let script_path = match resolve_script_path(www_dir, handler_ref) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "webd::mix_handler",
                www_dir = %www_dir.display(),
                handler_ref,
                error = %e,
                "handler script path rejected",
            );
            return server_error();
        }
    };

    // Capture request metadata + body BEFORE entering the blocking
    // closure (the body read is async). All captured values are `Send`.
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let headers_map = collect_headers(&parts.headers);

    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds handler limit",
            )
                .into_response();
        }
    };
    // `BODY` is a UTF-8 string (lossy): a binary body is mangled. v1
    // handlers are text/form/JSON oriented; a future `Value::Bytes`
    // global could carry raw bytes if a binary-upload handler appears.
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    // `$SIGNALS` — the parsed Datastar signal store. Computed HERE (before
    // the blocking closure) from `Send` inputs into a `Send` serde_json
    // value; the Mix `Value` (non-`Send`, holds `Rc`) is built inside the
    // closure. Reads `body_str` but does not consume it — `$BODY` is still
    // injected verbatim below.
    let signals_json = parse_datastar_signals(&method, &query, &body_str, &parts.headers);

    let cache = cache.clone();
    let script_for_log = script_path.clone();
    // `Bus` is granted iff the caller built an injection (route granted `bus:*`
    // AND the admin/CSRF gate passed). Computed before the `move` closure
    // consumes `bus`.
    let wants_bus = bus.is_some();
    let outcome = tokio::task::spawn_blocking(move || -> Result<MixResponse, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("current-thread runtime build: {e}"))?;
        rt.block_on(async move {
            let ast = get_or_parse(&cache, &script_path)?;

            let stdout = SharedBuf::new();
            let stderr = SharedBuf::new();
            let mut eval =
                Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
            // Set the script's resolved path so `include "lib.mix"`
            // resolves relative to the handler (under www_dir), not
            // webd's CWD — lets handlers share a layout/partials lib.
            // `include` is FsRead-gated (cosmix-lib-mix ≥ 0.15.3), which
            // the handler profile grants; a Pure-only profile denies it.
            eval.set_file(script_path.to_string_lossy());
            eval.set_limits(limits);
            // Pure + FsRead (+ Db when wants_db), minus `sleep` (see
            // HandlerCapabilityPolicy). Shell syntax (sh/$()/pipe) is
            // Process-gated by the policy's check_class (cosmix-lib-mix
            // ≥ 0.14.1). No Bus handler → send/emit inert.
            eval.set_capability_policy(Rc::new(HandlerCapabilityPolicy::new(
                wants_db, wants_jmap, wants_bus, wants_net,
            )));
            // Inject the per-vhost DB seam when present. The
            // capability gate above independently controls whether the
            // script may reach it; a `db`-capable route with no vhost db
            // gets no handler → `db_query` errors cleanly.
            if let Some(conn) = db {
                eval.set_db_handler(Rc::new(crate::db::WebdDbHandler::new(conn, allowed_dbs)));
            }
            // Inject the JMAP seam when present (same independent-gate
            // discipline as the DB seam): a `jmap`-capable route whose
            // vhost has no `jmap_upstream` gets no handler → `jmap`
            // errors cleanly. The reqwest client is built HERE, on this
            // request's current-thread runtime, and is first used on it —
            // never the shared `node.http_client` (which is bound to the
            // main runtime and used by the `/jmap` proxy; reusing a
            // pooled client across runtimes risks runtime-affinity
            // failures). `danger_accept_invalid_certs(true)` mirrors
            // `node.http_client` so maild's internal TLS cert is trusted.
            if let Some(jc) = jmap {
                match reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                {
                    Ok(client) => {
                        eval.set_jmap_handler(Rc::new(crate::jmap_handler::WebdJmapHandler::new(
                            client,
                            jc.upstream,
                            jc.auth,
                        )));
                    }
                    Err(e) => {
                        // No handler injected → `jmap()` errors cleanly.
                        tracing::warn!(
                            target: "webd::mix_handler",
                            error = %e,
                            "failed to build jmap client; jmap() will error in this handler",
                        );
                    }
                }
            }

            // Inject the delegated `bus_call` seam when the caller's admin/CSRF
            // gate passed. The `Rc` is built HERE (it is `!Send`, so it can't
            // cross the spawn_blocking boundary); the handler bridges the broker
            // send onto `main_handle`. `Bus` is granted via `wants_bus` above.
            if let Some(ac) = bus {
                eval.set_bus_call_handler(Rc::new(
                    crate::bus_call_handler::WebdBusCallHandler::new(
                        ac.broker,
                        ac.main_handle,
                        ac.bus_verbs,
                        ac.inputs,
                        ac.service,
                        ac.accelerator_only,
                    ),
                ));
            }

            eval.set_global("METHOD", Value::String(method));
            eval.set_global("PATH", Value::String(path));
            eval.set_global("QUERY", Value::String(query));
            eval.set_global("HOST", Value::String(host));
            eval.set_global("BODY", Value::String(body_str));
            // `to_value` of a BTreeMap<String,String> is a `Value::Map`
            // of strings (no Rc) — built here, inside the closure, so
            // no `Value` ever crosses the spawn_blocking boundary.
            let headers_value = cosmix_mix::to_value(&headers_map).unwrap_or(Value::Nil);
            eval.set_global("HEADERS", headers_value);
            // `$SESSION` — the unified-login identity from the sealed cookie.
            // `{authenticated, email, kind}`; never the token. Handlers read
            // this instead of a per-vhost CMS session; the per-vhost grants map
            // `email` → role. `kind` is "maild" (default) or "customer" — Mix
            // current_user() caps a "customer" session at rank-1 regardless of
            // the users table, the escalation guard for the billing portal.
            let session_value = cosmix_mix::to_value(&serde_json::json!({
                "authenticated": session_email.is_some(),
                "email": session_email.clone().unwrap_or_default(),
                "kind": session_kind.clone().unwrap_or_else(|| "maild".to_string()),
            }))
            .unwrap_or(Value::Nil);
            eval.set_global("SESSION", session_value);
            // `$CSRF` — the per-request double-submit token (matches the readable
            // `cosmix_csrf` cookie the caller set). A handler embeds it as a hidden
            // form field so a same-origin POST to a Rust `/auth/*` endpoint passes
            // the double-submit check. Empty string when the route carried none.
            eval.set_global("CSRF", Value::String(csrf.unwrap_or_default()));
            // `$SIGNALS` — Datastar signal store (parsed pre-closure). Built
            // into a Mix `Value` here, inside the closure, like HEADERS.
            let signals_value = cosmix_mix::to_value(&signals_json).unwrap_or(Value::Nil);
            eval.set_global("SIGNALS", signals_value);

            match eval.execute(&ast[..]).await {
                Ok(val) => Ok(map_outcome(val, &stdout)),
                Err(e) => Err(format!("{e}")),
            }
        })
    })
    .await;

    match outcome {
        Ok(Ok(resp)) => build_response(resp),
        Ok(Err(mix_err)) => {
            // All Mix errors (runtime, recursion-cap, deadline,
            // collection-cap) map to 500 with a generic body; the detail
            // goes to the log, never to the client.
            tracing::warn!(
                target: "webd::mix_handler",
                script = %script_for_log.display(),
                error = %mix_err,
                "mix handler failed",
            );
            server_error()
        }
        Err(join_err) => {
            tracing::error!(
                target: "webd::mix_handler",
                script = %script_for_log.display(),
                error = %join_err,
                "mix handler task panicked",
            );
            server_error()
        }
    }
}

/// Derive the **handler-root** — the directory a vhost's `handler_ref`
/// scripts resolve against — from its static docroot (`www_dir`).
///
/// NS 4.0 layout (`ns4-server-class-spec.md` §4.1): the docroot is
/// `…/web/app/public` and handler scripts live one level up in
/// `…/web/app/`, *above* the served directory, so handler source is not
/// under the `ServeDir` static root — no request path (incl. encoded
/// `..` / trailing-slash tricks) can reach it. (Caveat: `ServeDir`
/// follows filesystem symlinks, so deployment policy must forbid a
/// symlink from `public/` back into `web/app` or `web/data`; per-domain
/// ownership §3.3 is what keeps an untrusted tenant from planting one.)
/// We encode the split with
/// the `public`-basename convention: when the docroot's last component
/// is `public`, the handler-root is its parent; otherwise (legacy
/// layouts where handlers sit under the docroot) the handler-root is the
/// docroot itself — preserving prior behaviour.
pub(crate) fn derive_handler_root(www_dir: &Path) -> PathBuf {
    if www_dir.file_name().and_then(|n| n.to_str()) == Some("public") {
        match www_dir.parent() {
            // `Path::parent()` returns `Some("")` for a bare `public`;
            // an empty parent is no useful root, so fall back to self.
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => www_dir.to_path_buf(),
        }
    } else {
        www_dir.to_path_buf()
    }
}

/// Resolve `handler_ref` against the handler-root and reject anything
/// that escapes it. Requires the script to exist (a route pointing at a
/// missing file is a config error → 500). Canonicalising both sides
/// also blocks symlink escapes. (Parameter named `www_dir` for the
/// legacy layout where handler-root == docroot; callers pass the
/// `derive_handler_root` result in NS 4.0.)
fn resolve_script_path(www_dir: &Path, handler_ref: &str) -> Result<PathBuf, String> {
    if handler_ref.starts_with('/')
        || Path::new(handler_ref).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "handler_ref {handler_ref:?} is not a safe relative path"
        ));
    }
    let canon_www = www_dir
        .canonicalize()
        .map_err(|e| format!("canonicalize www_dir {}: {e}", www_dir.display()))?;
    let joined = canon_www.join(handler_ref);
    let canon_script = joined
        .canonicalize()
        .map_err(|e| format!("canonicalize script {}: {e}", joined.display()))?;
    if !canon_script.starts_with(&canon_www) {
        return Err(format!(
            "handler_ref {handler_ref:?} escapes www_dir {}",
            canon_www.display()
        ));
    }
    Ok(canon_script)
}

/// Get the cached AST for `path`, re-parsing if the file's mtime
/// changed. Concurrent fills both parse and both insert (last wins);
/// every caller gets a valid `Arc`.
///
/// Invalidation is mtime-based: on a filesystem with coarse (1 s) mtime
/// granularity, a sub-second edit that doesn't bump the second field
/// would serve the stale AST until the next change. Acceptable for
/// operator-authored handlers — and no worse than maild, which re-reads
/// from disk per message but parses afresh each time.
fn get_or_parse(cache: &AstCache, path: &Path) -> Result<Arc<Vec<Stmt>>, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    {
        let guard = lock_cache(cache);
        if let Some((cached_mtime, ast)) = guard.get(path)
            && *cached_mtime == mtime
        {
            return Ok(ast.clone());
        }
    }
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut lexer = Lexer::new(&source);
    let tokens = lexer.tokenize().map_err(|e| format!("{e}"))?;
    let mut parser = Parser::new(tokens, &source);
    let stmts = parser.parse_program().map_err(|e| format!("{e}"))?;
    let ast = Arc::new(stmts);
    lock_cache(cache).insert(path.to_path_buf(), (mtime, ast.clone()));
    Ok(ast)
}

/// Collect headers into a `BTreeMap<name, value>`; duplicate header
/// names are joined HTTP-canonically with `", "`. Names are already
/// lowercase (hyper normalises). Non-UTF-8 values render as empty.
fn collect_headers(h: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    let mut out: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (name, value) in h.iter() {
        let v = value.to_str().unwrap_or("").to_string();
        out.entry(name.as_str().to_string())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(&v);
            })
            .or_insert(v);
    }
    out
}

/// Parse the Datastar signal store from the request into a `serde_json::Value`
/// (a `Send` value, so it can cross the `spawn_blocking` boundary that a Mix
/// `Value` cannot). Mirrors the Datastar transport contract: a request
/// carrying `Datastar-Request: true` sends its signals as the `datastar`
/// query param on GET, or as the JSON body otherwise (data-star.dev/reference
/// /actions). The result is injected as the `$SIGNALS` handler global.
///
/// Lenient by construction — every non-Datastar request, missing param, or
/// malformed/oversize/non-object payload yields an empty object `{}`. A
/// handler that needs strictness inspects `$SIGNALS` itself. The parse NEVER
/// touches `$BODY`: on a Datastar POST it *reads* `body` but the caller still
/// injects the same `body` string as `$BODY`, so an ordinary JSON/form POST
/// handler is unaffected (and a non-Datastar POST gets `$SIGNALS = {}`).
fn parse_datastar_signals(
    method: &str,
    query: &str,
    body: &str,
    headers: &HeaderMap,
) -> serde_json::Value {
    let empty = || serde_json::Value::Object(serde_json::Map::new());

    // Gate on the header so an ordinary form/API request is untouched.
    let is_datastar = headers
        .get("datastar-request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !is_datastar {
        return empty();
    }

    // GET/HEAD carry signals in the `datastar=<url-encoded-json>` query
    // param; every other method carries them as the JSON body.
    let raw: Option<String> =
        if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
            // The body is capped by axum::body::to_bytes, but the query string
            // is not bounded HERE (only by upstream request-line limits we
            // don't control). `form_urlencoded` would percent-DECODE and own
            // the matching value before any size check, so bound the work with
            // a cheap borrowed length check up front: percent-decoding never
            // grows a string, so a query ≤ MAX_BODY_BYTES yields a decoded
            // `datastar` value ≤ MAX_BODY_BYTES.
            if query.len() > MAX_BODY_BYTES {
                return empty();
            }
            form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "datastar")
                .map(|(_, v)| v.into_owned())
        } else {
            Some(body.to_string())
        };
    let Some(raw) = raw else {
        return empty();
    };

    // Bounded parse: the body is already capped at MAX_BODY_BYTES (and the GET
    // query was capped above); this guards the POST/PUT/… body path and is a
    // no-op belt-and-suspenders for GET — never an unbounded JSON parse.
    if raw.len() > MAX_BODY_BYTES {
        return empty();
    }

    // A signal store is a JSON object; a bare array/number/string/null is not
    // a valid `$SIGNALS` map, so fall back to `{}` rather than inject a
    // non-map global (keeps `$SIGNALS` a uniform map for handlers).
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => empty(),
    }
}

/// Map an evaluator outcome (`Ok(Value)` + captured stdout) to a
/// [`MixResponse`] per ADR §8. Runs inside the blocking closure — takes
/// `Value` by value, returns `Send` data.
fn map_outcome(mut val: Value, stdout: &SharedBuf) -> MixResponse {
    // `Value` implements `Drop` (mix ≥ 0.16.3, stack-safe nested drop),
    // so inner data can't be moved out by value — match a `&mut` binding
    // and `mem::take` the body string (zero-copy; no clone of the
    // potentially-MiB response body).
    match &mut val {
        // A non-empty string return IS the body (stdout ignored, like
        // maild's filter return).
        Value::String(s) => MixResponse {
            status: 200,
            content_type: Some(DEFAULT_CONTENT_TYPE.into()),
            headers: Vec::new(),
            body: std::mem::take(s).into_bytes(),
        },
        // A map return is a typed response: { status?, headers?, body? }.
        Value::Map(m) => {
            let status = m
                .get("status")
                .and_then(Value::to_number)
                .filter(|n| (100.0..=599.0).contains(n))
                .map(|n| n as u16)
                .unwrap_or(200);
            // Zero-copy body: when the return Map is uniquely owned (the common
            // case — a handler returns a fresh { status, body } literal),
            // mem::take the body String out instead of cloning a potentially-MiB
            // response (a 1000-row datatable SSE partial). Map is Rc<IndexMap>
            // (CoW): Rc::get_mut yields Some only at refcount 1, so a shared map
            // (rare — `return $global_map`) falls back to the clone, never worse.
            let body: Vec<u8> = if let Some(map) = std::rc::Rc::get_mut(m) {
                match map.get_mut("body") {
                    Some(Value::String(s)) => std::mem::take(s).into_bytes(),
                    Some(Value::Bytes(b)) => b.to_vec(),
                    _ => Vec::new(),
                }
            } else {
                match m.get("body") {
                    Some(Value::String(s)) => s.clone().into_bytes(),
                    // Raw bytes pass through verbatim — lets a handler serve
                    // binary (e.g. `body: base64_decode($row["data"])` for a
                    // stored image) without UTF-8 corruption.
                    Some(Value::Bytes(b)) => b.to_vec(),
                    _ => Vec::new(),
                }
            };
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut content_type: Option<String> = None;
            if let Some(Value::Map(hm)) = m.get("headers") {
                for (k, v) in hm.iter() {
                    let val = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => format_number(*n),
                        Value::Bool(b) => b.to_string(),
                        // Skip non-scalar header values rather than
                        // emitting a debug rendering.
                        _ => continue,
                    };
                    if k.eq_ignore_ascii_case("content-type") {
                        content_type = Some(val);
                    } else {
                        headers.push((k.clone(), val));
                    }
                }
            }
            MixResponse {
                status,
                content_type,
                headers,
                body,
            }
        }
        // Anything else (nil, number, bool, list, …): use captured
        // stdout as the body if present, else 204.
        _ => {
            let out = stdout.to_string_lossy();
            if out.trim().is_empty() {
                MixResponse {
                    status: 204,
                    content_type: None,
                    headers: Vec::new(),
                    body: Vec::new(),
                }
            } else {
                MixResponse {
                    status: 200,
                    content_type: Some(DEFAULT_CONTENT_TYPE.into()),
                    headers: Vec::new(),
                    body: out.into_bytes(),
                }
            }
        }
    }
}

/// Render a Mix number for a header value: whole values as integers.
fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Build the `axum` `Response` from a [`MixResponse`]. Invalid header
/// names/values are skipped (debug-logged); a structurally-impossible
/// response falls back to 500.
fn build_response(r: MixResponse) -> Response {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(status);

    let effective_ct = r
        .content_type
        .or_else(|| (!r.body.is_empty()).then(|| DEFAULT_CONTENT_TYPE.to_string()));
    if let Some(ct) = effective_ct
        && let Ok(value) = HeaderValue::from_str(&ct)
    {
        builder = builder.header(header::CONTENT_TYPE, value);
    }

    for (k, v) in r.headers {
        match (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            (Ok(name), Ok(value)) => builder = builder.header(name, value),
            _ => tracing::debug!(
                target: "webd::mix_handler",
                header = %k,
                "skipping invalid response header from mix handler",
            ),
        }
    }

    builder
        .body(Body::from(r.body))
        .unwrap_or_else(|_| server_error())
}

/// Generic 500 — never leaks Mix internals to the client.
fn server_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::io::Write;
    use tempfile::TempDir;

    /// Write `body` to `<dir>/<name>` and return the dir's path.
    fn write_script(dir: &TempDir, name: &str, body: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    fn get_req(path: &str) -> Request {
        Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    /// Ergonomic wrappers over [`run_with_caps`] for the read-only and
    /// db-only cases the tests exercise (production funnels everything
    /// through `run_with_caps`).
    async fn run(
        cache: &Arc<AstCache>,
        www_dir: &Path,
        handler_ref: &str,
        req: Request,
    ) -> Response {
        run_with_caps(
            cache,
            www_dir,
            handler_ref,
            None,
            false,
            Arc::new(BTreeSet::new()),
            None,
            false,
            None,
            None,
            None,
            None,
            false,
            req,
        )
        .await
    }

    async fn run_with_db(
        cache: &Arc<AstCache>,
        www_dir: &Path,
        handler_ref: &str,
        db: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
        wants_db: bool,
        req: Request,
    ) -> Response {
        // A plain `db` route: main allowed, no aux schemas.
        let allowed: BTreeSet<String> = if wants_db {
            BTreeSet::from(["main".to_string()])
        } else {
            BTreeSet::new()
        };
        run_with_caps(
            cache,
            www_dir,
            handler_ref,
            db,
            wants_db,
            Arc::new(allowed),
            None,
            false,
            None,
            None,
            None,
            None,
            false,
            req,
        )
        .await
    }

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    /// `derive_handler_root`: a `…/web/app/public` docroot yields the
    /// `…/web/app` handler-root (handlers live above the served dir);
    /// any other docroot is its own handler-root (legacy layout).
    #[test]
    fn handler_root_is_parent_of_public_docroot() {
        assert_eq!(
            derive_handler_root(Path::new("/srv/x.example/web/app/public")),
            PathBuf::from("/srv/x.example/web/app")
        );
        // Legacy layout: handlers sit under the docroot → unchanged.
        assert_eq!(
            derive_handler_root(Path::new("/var/lib/cosmix/www-markc")),
            PathBuf::from("/var/lib/cosmix/www-markc")
        );
        // A bare `public` with no parent falls back to itself.
        assert_eq!(
            derive_handler_root(Path::new("public")),
            PathBuf::from("public")
        );
    }

    /// A `Value::Bytes` body (e.g. `base64_decode` of a stored image) must
    /// reach the client VERBATIM — not via `from_utf8_lossy`, which would
    /// corrupt any non-UTF-8 byte. Regression guard for binary serving
    /// (media.mix). base64 "iVD/AA==" decodes to [0x89,0x50,0xFF,0x00];
    /// the 0xFF is invalid UTF-8, so a lossy path would mangle it.
    #[tokio::test]
    async fn bytes_body_served_verbatim() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "b.mix",
            "{ status: 200, headers: { \"Content-Type\": \"image/png\" }, body: base64_decode(\"iVD/AA==\") }",
        );
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "b.mix", get_req("/b")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(body_bytes(resp).await, vec![0x89u8, 0x50, 0xFF, 0x00]);
    }

    #[tokio::test]
    async fn print_becomes_html_200() {
        let dir = TempDir::new().unwrap();
        write_script(&dir, "hello.mix", "print(\"<h1>hi</h1>\")");
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "hello.mix", get_req("/hello")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            DEFAULT_CONTENT_TYPE
        );
        assert!(body_string(resp).await.contains("<h1>hi</h1>"));
    }

    #[tokio::test]
    async fn string_return_is_html_200() {
        let dir = TempDir::new().unwrap();
        write_script(&dir, "s.mix", "\"<p>x</p>\"");
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "s.mix", get_req("/s")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "<p>x</p>");
    }

    // --- $SIGNALS injection (Part A.3) ---

    fn ds_get_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("datastar-request", HeaderValue::from_static("true"));
        h
    }

    #[test]
    fn signals_get_query_param_parsed() {
        // datastar={"count":5}
        let v = parse_datastar_signals(
            "GET",
            "datastar=%7B%22count%22%3A5%7D",
            "",
            &ds_get_headers(),
        );
        assert_eq!(v, serde_json::json!({"count": 5}));
    }

    #[test]
    fn signals_post_json_body_parsed() {
        let v = parse_datastar_signals("POST", "", r#"{"open":true}"#, &ds_get_headers());
        assert_eq!(v, serde_json::json!({"open": true}));
    }

    #[test]
    fn signals_malformed_json_is_empty_object() {
        let v = parse_datastar_signals("POST", "", "{not json", &ds_get_headers());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn signals_non_object_json_is_empty_object() {
        // A bare array is valid JSON but not a signal store → {}.
        let v = parse_datastar_signals("POST", "", "[1,2,3]", &ds_get_headers());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn signals_oversize_payload_is_empty_object() {
        let big = format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_BODY_BYTES + 1));
        let v = parse_datastar_signals("POST", "", &big, &ds_get_headers());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn signals_oversize_get_query_is_empty_object() {
        // A query over MAX_BODY_BYTES is rejected BEFORE form_urlencoded
        // decodes/allocates the value (resource-exhaustion guard).
        let q = format!("datastar={}", "a".repeat(MAX_BODY_BYTES + 1));
        let v = parse_datastar_signals("GET", &q, "", &ds_get_headers());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn signals_non_datastar_request_is_empty_object() {
        // No Datastar-Request header → a plain POST with a JSON body still
        // gets `$SIGNALS = {}` (zero behaviour change for existing handlers).
        let v = parse_datastar_signals("POST", "", r#"{"x":1}"#, &HeaderMap::new());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn signals_datastar_get_without_param_is_empty_object() {
        let v = parse_datastar_signals("GET", "foo=bar", "", &ds_get_headers());
        assert_eq!(v, serde_json::json!({}));
    }

    /// End-to-end: a Datastar POST's signals reach `$SIGNALS` AND `$BODY`
    /// is preserved verbatim (the parse must not consume the body).
    #[tokio::test]
    async fn signals_reach_handler_and_body_is_preserved() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "echo.mix",
            "{ status: 200, headers: { \"Content-Type\": \"application/json\" }, \
             body: json_encode({ sig: $SIGNALS, body: $BODY }) }",
        );
        let cache = new_ast_cache();
        let req = Request::builder()
            .method("POST")
            .uri("/echo")
            .header("datastar-request", "true")
            .body(Body::from(r#"{"count":7}"#))
            .unwrap();
        let resp = run(&cache, dir.path(), "echo.mix", req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(parsed["sig"], serde_json::json!({"count": 7}));
        // `$BODY` untouched — an ordinary JSON/form POST handler still sees it.
        assert_eq!(parsed["body"], serde_json::json!(r#"{"count":7}"#));
    }

    /// D5 acceptance gate: a handler that sets `Content-Type:
    /// text/event-stream` and builds its body with the `ds_*` builtins has
    /// BOTH passed through unmodified (no webd response-path change for
    /// single-response outbound SSE). Also proves the `datastar` feature is
    /// wired into webd's Mix lib (else `ds_sse` would error).
    #[tokio::test]
    async fn datastar_sse_response_passes_through_unmodified() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "sse.mix",
            "{ status: 200, headers: { \"Content-Type\": \"text/event-stream\" }, \
             body: ds_sse([ds_patch_elements(\"<p>hi</p>\", {selector: \"#content\", mode: \"inner\"})]) }",
        );
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "sse.mix", get_req("/sse")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            body_string(resp).await,
            "event: datastar-patch-elements\n\
             data: selector #content\n\
             data: mode inner\n\
             data: elements <p>hi</p>\n\n"
        );
    }

    /// A non-Datastar POST (no header) leaves `$SIGNALS` an empty map while
    /// `$BODY` still carries the JSON — the existing-handler regression case.
    #[tokio::test]
    async fn non_datastar_post_signals_empty_body_intact() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "echo.mix",
            "{ status: 200, headers: { \"Content-Type\": \"application/json\" }, \
             body: json_encode({ sig: $SIGNALS, body: $BODY }) }",
        );
        let cache = new_ast_cache();
        let req = Request::builder()
            .method("POST")
            .uri("/echo")
            .body(Body::from(r#"{"count":7}"#))
            .unwrap();
        let resp = run(&cache, dir.path(), "echo.mix", req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(parsed["sig"], serde_json::json!({}));
        assert_eq!(parsed["body"], serde_json::json!(r#"{"count":7}"#));
    }

    #[tokio::test]
    async fn typed_map_response() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "m.mix",
            "{ status: 404, headers: { \"X-Test\": \"yes\" }, body: \"nope\" }",
        );
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "m.mix", get_req("/m")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.headers().get("x-test").unwrap(), "yes");
        assert_eq!(body_string(resp).await, "nope");
    }

    #[tokio::test]
    async fn capability_denied_write_file_500() {
        let dir = TempDir::new().unwrap();
        write_script(&dir, "w.mix", "write_file(\"/tmp/should-not.txt\", \"x\")");
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "w.mix", get_req("/w")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!std::path::Path::new("/tmp/should-not.txt").exists());
    }

    /// SECURITY: Mix's AST-level shell constructs (`sh`, `$()`, pipes)
    /// must not run `/bin/sh` from inside a handler — the embed claims a
    /// Pure+FsRead sandbox with Process denied. Each test writes a
    /// marker via a shell side-effect; the marker must NOT appear.
    #[tokio::test]
    async fn sh_keyword_cannot_run_processes() {
        let www = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let marker = out.path().join("escaped_sh");
        write_script(
            &www,
            "sh.mix",
            &format!("sh \"touch '{}'\"", marker.display()),
        );
        let cache = new_ast_cache();
        let _ = run(&cache, www.path(), "sh.mix", get_req("/sh")).await;
        assert!(
            !marker.exists(),
            "SANDBOX ESCAPE: `sh` keyword spawned /bin/sh despite Process-denied policy"
        );
    }

    #[tokio::test]
    async fn command_substitution_cannot_run_processes() {
        let www = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let marker = out.path().join("escaped_subst");
        write_script(
            &www,
            "subst.mix",
            &format!("$x = $(touch '{}')", marker.display()),
        );
        let cache = new_ast_cache();
        let _ = run(&cache, www.path(), "subst.mix", get_req("/subst")).await;
        assert!(
            !marker.exists(),
            "SANDBOX ESCAPE: $() command substitution spawned /bin/sh despite Process-denied policy"
        );
    }

    #[tokio::test]
    async fn pipe_to_shell_cannot_run_processes() {
        let www = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        let marker = out.path().join("escaped_pipe");
        write_script(
            &www,
            "pipe.mix",
            &format!("\"x\" | tee '{}'", marker.display()),
        );
        let cache = new_ast_cache();
        let _ = run(&cache, www.path(), "pipe.mix", get_req("/pipe")).await;
        assert!(
            !marker.exists(),
            "SANDBOX ESCAPE: pipe-to-external spawned /bin/sh despite Process-denied policy"
        );
    }

    #[tokio::test]
    async fn sleep_is_denied_and_does_not_block() {
        let www = TempDir::new().unwrap();
        write_script(&www, "sleep.mix", "sleep(30)");
        let cache = new_ast_cache();
        // Must return promptly (denied before the blocking sleep), NOT
        // hang for 30s. The timeout is the real assertion of the fix.
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            run(&cache, www.path(), "sleep.mix", get_req("/sleep")),
        )
        .await
        .expect("denied sleep must not block the handler thread");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn runaway_recursion_500_not_crash() {
        let dir = TempDir::new().unwrap();
        write_script(
            &dir,
            "r.mix",
            "function rec($n)\n  return rec($n + 1)\nend\nrec(0)",
        );
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "r.mix", get_req("/r")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn deadline_exceeded_500() {
        let dir = TempDir::new().unwrap();
        write_script(&dir, "loop.mix", "while true\n  $x = 1\nend");
        let cache = new_ast_cache();
        // Short deadline keeps the test fast.
        let limits = EvalLimits {
            time_limit: Some(Duration::from_millis(200)),
            ..default_limits()
        };
        let resp = run_with_limits(
            &cache,
            dir.path(),
            "loop.mix",
            get_req("/loop"),
            limits,
            None,
            false,
            Arc::new(BTreeSet::new()),
            None,
            false,
            None,
            None,
            None,
            None,
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn missing_script_500() {
        let dir = TempDir::new().unwrap();
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "nope.mix", get_req("/x")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn ast_cache_reuses_until_mtime_changes() {
        let dir = TempDir::new().unwrap();
        write_script(&dir, "c.mix", "print(\"v1\")");
        let path = dir.path().join("c.mix").canonicalize().unwrap();
        let cache: AstCache = Mutex::new(HashMap::new());

        let a = get_or_parse(&cache, &path).unwrap();
        let b = get_or_parse(&cache, &path).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same-mtime calls reuse the cached AST");
        assert_eq!(lock_cache(&cache).len(), 1);

        // Force the cached mtime stale → next call must re-parse and
        // replace, deterministically exercising the invalidation branch
        // without relying on filesystem mtime granularity.
        lock_cache(&cache).insert(path.clone(), (SystemTime::UNIX_EPOCH, Arc::new(Vec::new())));
        let c = get_or_parse(&cache, &path).unwrap();
        assert!(
            !c.is_empty(),
            "stale entry re-parsed to the real (non-empty) AST"
        );
        let (cached_mtime, _) = lock_cache(&cache).get(&path).cloned().unwrap();
        assert_ne!(
            cached_mtime,
            SystemTime::UNIX_EPOCH,
            "cached mtime refreshed"
        );
    }

    /// Build a HandlerRow with no extra capabilities (the common case).
    fn row(
        route_id: &str,
        vhost: &str,
        method: &str,
        path: &str,
        href: &str,
        enabled: bool,
    ) -> HandlerRow {
        HandlerRow {
            route_id: route_id.into(),
            vhost_fqdn: vhost.into(),
            method: method.into(),
            path_pattern: path.into(),
            handler_kind: "mix".into(),
            handler_ref: href.into(),
            enabled,
            capabilities: Vec::new(),
        }
    }

    /// Matched handler_ref for an assertion (lookup now returns a
    /// MatchedHandler; tests care about the script ref).
    fn matched_ref(t: &HandlerTable, v: &str, m: &str, p: &str) -> Option<String> {
        t.lookup(v, m, p).map(|h| h.handler_ref)
    }

    #[test]
    fn table_dispatch_specificity_and_fallthrough() {
        let rows = vec![
            row(
                "blog-glob",
                "example.com",
                "GET",
                "/blog/*",
                "blog.mix",
                true,
            ),
            row(
                "blog-exact",
                "example.com",
                "GET",
                "/blog/index",
                "index.mix",
                true,
            ),
            row("disabled", "example.com", "GET", "/off", "off.mix", false),
        ];
        let table = HandlerTable::from_rows(rows);
        // Exact beats the glob for /blog/index.
        assert_eq!(
            matched_ref(&table, "example.com", "GET", "/blog/index").as_deref(),
            Some("index.mix")
        );
        // Glob catches other /blog/ paths.
        assert_eq!(
            matched_ref(&table, "example.com", "GET", "/blog/2026/post").as_deref(),
            Some("blog.mix")
        );
        // Disabled row is absent.
        assert!(table.lookup("example.com", "GET", "/off").is_none());
        // Unmatched path / unknown vhost → fall through.
        assert!(table.lookup("example.com", "GET", "/other").is_none());
        assert!(table.lookup("other.com", "GET", "/blog/index").is_none());
        // Method mismatch → fall through.
        assert!(table.lookup("example.com", "POST", "/blog/index").is_none());
        assert_eq!(table.route_count(), 2);
    }

    #[test]
    fn any_method_matches_but_concrete_wins() {
        let rows = vec![
            row("any", "v", "ANY", "/p", "any.mix", true),
            row("get", "v", "GET", "/p", "get.mix", true),
        ];
        let table = HandlerTable::from_rows(rows);
        // Concrete GET beats ANY for a GET request.
        assert_eq!(
            matched_ref(&table, "v", "GET", "/p").as_deref(),
            Some("get.mix")
        );
        // POST has no concrete handler → ANY catches it.
        assert_eq!(
            matched_ref(&table, "v", "POST", "/p").as_deref(),
            Some("any.mix")
        );
    }

    #[test]
    fn capabilities_db_token_sets_wants_db() {
        let mut r = row("cms", "v", "GET", "/admin", "admin.mix", true);
        r.capabilities = vec!["db".into()];
        let table = HandlerTable::from_rows(vec![r]);
        let m = table.lookup("v", "GET", "/admin").expect("matched");
        assert!(m.wants_db, "a 'db' capability token sets wants_db");
        // A plain route does not.
        let plain = HandlerTable::from_rows(vec![row("p", "v", "GET", "/x", "x.mix", true)]);
        assert!(!plain.lookup("v", "GET", "/x").unwrap().wants_db);
    }

    #[test]
    fn capabilities_jmap_token_sets_wants_jmap() {
        let mut r = row("pim", "v", "GET", "/pim/contacts", "contacts.mix", true);
        r.capabilities = vec!["jmap".into()];
        let table = HandlerTable::from_rows(vec![r]);
        let m = table.lookup("v", "GET", "/pim/contacts").expect("matched");
        assert!(m.wants_jmap, "a 'jmap' capability token sets wants_jmap");
        assert!(!m.wants_db, "jmap alone does not set wants_db");
        // A plain route does not.
        let plain = HandlerTable::from_rows(vec![row("p", "v", "GET", "/x", "x.mix", true)]);
        assert!(!plain.lookup("v", "GET", "/x").unwrap().wants_jmap);
    }

    #[test]
    fn capabilities_db_and_jmap_both_set() {
        let mut r = row("both", "v", "GET", "/both", "both.mix", true);
        r.capabilities = vec!["db".into(), "jmap".into()];
        let table = HandlerTable::from_rows(vec![r]);
        let m = table.lookup("v", "GET", "/both").expect("matched");
        assert!(
            m.wants_db && m.wants_jmap,
            "both capability tokens set both flags"
        );
    }

    #[test]
    fn wake_capability_parses_into_matched_handler() {
        // A route carrying a `wake:` grant (here db-only, like /ssh/host-tool/run)
        // resolves it to (verb, derived-service).
        let mut r = row(
            "wakey",
            "v",
            "POST",
            "/ssh/host-tool/run",
            "ssh_tools.mix",
            true,
        );
        r.capabilities = vec!["db".into(), "wake:sshm.wake".into()];
        let table = HandlerTable::from_rows(vec![r]);
        let m = table
            .lookup("v", "POST", "/ssh/host-tool/run")
            .expect("matched");
        assert_eq!(
            m.wake,
            Some(("sshm.wake".to_string(), "sshm".to_string())),
            "a valid wake: grant parses to (verb, own-derived-service)"
        );

        // A bad persisted wake grant is dropped by from_rows (defence-in-depth,
        // beyond the store-time is_known_capability gate).
        let mut bad = row("badwake", "v", "POST", "/x", "x.mix", true);
        bad.capabilities = vec!["db".into(), "wake:evil.wake".into()];
        let table = HandlerTable::from_rows(vec![bad]);
        assert_eq!(
            table.lookup("v", "POST", "/x").unwrap().wake,
            None,
            "a non-accelerator wake grant is dropped, not parsed"
        );

        // No grant → None.
        let plain = HandlerTable::from_rows(vec![row("p", "v", "GET", "/y", "y.mix", true)]);
        assert_eq!(plain.lookup("v", "GET", "/y").unwrap().wake, None);
    }

    #[test]
    fn public_cache_is_get_only_and_bus_free_fail_closed() {
        // A plain GET route with `public_cache` → cacheable.
        let mut get = row("cacheable", "v", "GET", "/blog", "blog.mix", true);
        get.capabilities = vec!["public_cache".into()];
        let t = HandlerTable::from_rows(vec![get]);
        assert!(
            t.lookup("v", "GET", "/blog").unwrap().wants_public_cache,
            "public_cache on a GET route is honoured"
        );

        // Same grant on a POST route → IGNORED (a mutation is never cached).
        let mut post = row("post", "v", "POST", "/blog", "blog.mix", true);
        post.capabilities = vec!["public_cache".into()];
        let t = HandlerTable::from_rows(vec![post]);
        assert!(
            !t.lookup("v", "POST", "/blog").unwrap().wants_public_cache,
            "public_cache on a non-GET route is ignored (fail closed)"
        );

        // Grant on a GET route that ALSO delegates a (real, delegated-safe) Bus
        // verb → IGNORED: a cached render must be side-effect free.
        let mut ampy = row("ampy", "v", "GET", "/blog", "blog.mix", true);
        ampy.capabilities = vec!["public_cache".into(), "bus:filesd.read".into()];
        let t = HandlerTable::from_rows(vec![ampy]);
        let m = t.lookup("v", "GET", "/blog").unwrap();
        assert!(
            !m.bus_verbs.is_empty(),
            "the bus grant is live (guards the next assert)"
        );
        assert!(
            !m.wants_public_cache,
            "public_cache on an bus-bearing route is ignored (fail closed)"
        );

        // A route WITHOUT the grant is never cacheable.
        let plain =
            HandlerTable::from_rows(vec![row("plain", "v", "GET", "/blog", "blog.mix", true)]);
        assert!(
            !plain
                .lookup("v", "GET", "/blog")
                .unwrap()
                .wants_public_cache
        );
    }

    #[test]
    fn malformed_glob_pattern_does_not_overmatch() {
        // A row that somehow bypassed `validate_path_pattern` with
        // "/blog*" must NOT compile to an over-broad "/blog" prefix.
        let table =
            HandlerTable::from_rows(vec![row("bad", "v", "GET", "/blog*", "bad.mix", true)]);
        assert!(table.lookup("v", "GET", "/blogXYZ").is_none());
        assert!(table.lookup("v", "GET", "/blog/2026").is_none());
        // Only the literal string matches (Exact — fails safe).
        assert_eq!(
            matched_ref(&table, "v", "GET", "/blog*").as_deref(),
            Some("bad.mix")
        );
    }

    #[tokio::test]
    async fn traversal_at_dispatch_500() {
        // Even if a bad handler_ref reached the table, dispatch-time
        // resolution must reject the escape.
        let dir = TempDir::new().unwrap();
        let cache = new_ast_cache();
        let resp = run(&cache, dir.path(), "../escape.mix", get_req("/x")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn mem_db() -> Arc<tokio::sync::Mutex<rusqlite::Connection>> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        Arc::new(tokio::sync::Mutex::new(conn))
    }

    #[tokio::test]
    async fn db_route_writes_then_reads_200() {
        let www = TempDir::new().unwrap();
        // A db-capable handler: create a table, insert a row, read it
        // back, and render it. Exercises db_exec (DDL + DML) + db_query.
        write_script(
            &www,
            "cms.mix",
            "db_exec(\"CREATE TABLE IF NOT EXISTS p (id INTEGER PRIMARY KEY, t TEXT)\", [])\n\
             db_exec(\"INSERT INTO p (t) VALUES (?1)\", [\"hello-cms\"])\n\
             $rows = db_query(\"SELECT t FROM p\", [])\n\
             print(\"<h1>\" .. $rows[0][\"t\"] .. \"</h1>\")",
        );
        let cache = new_ast_cache();
        let resp = run_with_db(
            &cache,
            www.path(),
            "cms.mix",
            Some(mem_db()),
            true,
            get_req("/admin"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("<h1>hello-cms</h1>"));
    }

    #[tokio::test]
    async fn db_query_denied_without_db_capability_500() {
        let www = TempDir::new().unwrap();
        write_script(&www, "q.mix", "db_query(\"SELECT 1\", [])");
        let cache = new_ast_cache();
        // wants_db = false (the default `run` path) → the policy denies
        // the Db class and no handler is injected → 500.
        let resp = run(&cache, www.path(), "q.mix", get_req("/q")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn db_capable_route_without_vhost_db_500() {
        let www = TempDir::new().unwrap();
        write_script(&www, "q.mix", "db_query(\"SELECT 1\", [])");
        let cache = new_ast_cache();
        // wants_db = true but db = None (vhost has no CMS db) → the
        // builtin errors "database not available" → 500 (not a panic).
        let resp = run_with_db(&cache, www.path(), "q.mix", None, true, get_req("/q")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn handler_can_include_sibling_lib() {
        // `include "lib.mix"` must resolve relative to the handler (under
        // www_dir), not webd's CWD — proves the embed sets the script's
        // current_file. Enables shared layout/partial libs.
        let www = TempDir::new().unwrap();
        write_script(
            &www,
            "lib.mix",
            "function greet($n)\n  return \"<h1>hi \" .. $n .. \"</h1>\"\nend",
        );
        write_script(
            &www,
            "page.mix",
            "include \"lib.mix\"\nreturn greet(\"world\")",
        );
        let cache = new_ast_cache();
        let resp = run(&cache, www.path(), "page.mix", get_req("/p")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("<h1>hi world</h1>"));
    }
}
