#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod acme_provisioner;
mod bus;
mod bus_call_handler;
mod db;
mod file_share;
mod handlers_namespace;
mod jmap_handler;
mod listeners_bootstrap;
mod listeners_namespace;
mod listeners_reaction;
mod media;
mod mix_handler;
mod mxresolve;
mod portal_auth;
mod public_response_cache;
mod session;
mod stats;
mod tls_status;
mod vhost_directory;
mod vhosts_bootstrap;
mod vhosts_namespace;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use clap::{Parser, Subcommand};
use cosmix_config::acme_policy::{AcmeVhostPlan, ResolvedWebdAcme};
use cosmix_config::node::{ResolvedWebdListener, TlsIdentityConfig};
use cosmix_daemon::listen::{
    AcceptedStream, ConnCtx, ConnHandler, ListenerSet, ListenerSpec, ListenerTls, TlsMode,
};
use cosmix_daemon::tls::sni::SniCertResolver;
use futures_util::{SinkExt, StreamExt};
use pulldown_cmark::{Options, Parser as MdParser};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tower::util::ServiceExt;
use tower_http::services::ServeDir;
use tracing::info;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `--version` line with git sha + build time (version-discovery
/// contract). `COSMIX_*` set by build.rs → `cosmix_buildinfo::emit()`.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("COSMIX_GIT_SHA"),
    ", built ",
    env!("COSMIX_BUILD_TIME"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "cosmix-webd",
    version = VERSION,
    about = "Lightweight web server for cosmix WASM apps + CMS API"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mint a self-signed EC P-256 cert (rcgen) for an internal/`.localhost`
    /// vhost that can never get a Let's Encrypt cert. Replaces the NS5
    /// firstboot `openssl req` shell-out. Args: <fqdn> <cert-pem> <key-pem>.
    Mkcert {
        /// FQDN used for the CN and DNS SAN (a 127.0.0.1 IP SAN is also added)
        fqdn: String,
        /// Output path for the certificate PEM (fullchain)
        cert: PathBuf,
        /// Output path for the private-key PEM
        key: PathBuf,
    },
    /// Start the web server
    Serve {
        /// Listen address override (default: derived from node.conf.mix)
        #[arg(long)]
        listen: Option<String>,

        /// Plain-HTTP listener address, e.g. `0.0.0.0:80`. Enables a
        /// second listener serving the NS-3.0 `301 → https` shape.
        /// Unset (and unset in node.conf.mix) means no :80 listener — the
        /// WG-only bind posture is preserved. Overrides node.conf.mix's
        /// `[webd] http_listen`.
        #[arg(long)]
        http_listen: Option<String>,

        /// Directory containing WASM apps (default: from node.conf.mix or /var/lib/cosmix/www)
        #[arg(long)]
        www_dir: Option<PathBuf>,

        /// Path to the SQLite database (default: $COSMIX_VAR/web.db)
        #[arg(long)]
        db_path: Option<PathBuf>,

        /// Upstream JMAP server override (default: derived from node.conf.mix)
        #[arg(long)]
        jmap_upstream: Option<String>,

        /// Upstream broker WebSocket URL override (default: derived from node.conf.mix)
        #[arg(long)]
        noded_ws: Option<String>,

        /// Directory of markdown files to serve at /docs/
        #[arg(long)]
        docs_dir: Option<PathBuf>,

        /// DEV MODE: serve this directory as a static site on a loopback-only
        /// listener (default `127.0.0.1:8080`). Short-circuits the entire
        /// production path — no `node.conf.mix`, no database, no Bus broker, no
        /// ACME/TLS, no vhost registration. No embedded **Mix handlers** and
        /// no CMS API (`/api/posts`, `/jmap`, `/ws`): the synthesized
        /// `localhost` vhost has an empty handler table and `db = None`, so
        /// those require a registered vhost + database. Files are served from
        /// the folder (plus webd's built-in `/docs` markdown viewer and
        /// `/assets/`). A zero-config local preview, fenced to loopback so the
        /// no-auth posture can't be reached off-box. With `--static-dir` set,
        /// `--listen` (if given) must RESOLVE to a loopback address; all other
        /// serve flags are ignored.
        #[arg(long)]
        static_dir: Option<PathBuf>,

        /// TLS certificate file (PEM). Enables HTTPS when set.
        #[arg(long)]
        tls_cert: Option<PathBuf>,

        /// TLS private key file (PEM)
        #[arg(long)]
        tls_key: Option<PathBuf>,
    },
    /// Initialise the SQLite database
    Init {
        /// Path to the SQLite database (default: $COSMIX_VAR/web.db)
        #[arg(long)]
        db_path: Option<PathBuf>,
    },
    /// Per-vhost SPEC 12 namespace (`webd.vhosts`)
    Vhost {
        #[command(subcommand)]
        action: VhostAction,
    },
    /// ACME (Let's Encrypt) lifecycle verbs for substrate-managed
    /// vhosts. Wraps `webd.acme.renew` (narrow `webd.acme.renew:webd.vhosts`
    /// cap; bypasses cooldown + 30-day renewal-window gates but NOT
    /// disabled/apex-policy gates) and `webd.acme.status`.
    Acme {
        #[command(subcommand)]
        action: AcmeAction,
    },
    /// Read-only inspection of the per-vhost route map
    /// (`webd.routes.list`). The verb's wire shape returns a sorted
    /// list of primaries with alias + capability booleans
    /// (`has_cms` / `has_jmap` / `has_ws` / `has_docs`); see
    /// `bus/routes.rs` for the snapshot semantics.
    Routes {
        #[command(subcommand)]
        action: RoutesAction,
    },
    /// Read-only per-vhost response-class counters (`webd.stats`).
    /// Leaf verb — no sub-action. See `bus/stats.rs`.
    Stats,
    /// Read-only TLS posture: manual-PEM identity hostnames, ACME
    /// plan summary, and per-vhost issuance state. Wraps
    /// `webd.tls.status` (see `bus/tls.rs` + `tls_status.rs` for
    /// the snapshot shape).
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
    /// Read-only autoconfig admission allowlist
    /// (`webd.autoconfig.served_domains`). A `Host:` not in this
    /// set is `404`ed before any DNS lookup — see
    /// `maild-autoconfig.md §Security` and `bus/autoconfig.rs`.
    Autoconfig {
        #[command(subcommand)]
        action: AutoconfigAction,
    },
}

#[derive(Subcommand)]
enum RoutesAction {
    /// Snapshot the current vhost map.
    List,
}

#[derive(Subcommand)]
enum TlsAction {
    /// Show manual-PEM identity hostnames + ACME plan + per-vhost
    /// issuance state.
    Status,
    /// Live-reload manual-PEM certs without a restart (`webd.tls.reload`).
    /// Re-reads + re-validates the configured cert files and atomically
    /// swaps each listener's resolver; a bad cert returns rc=10 and keeps
    /// the prior cert serving. Manual-PEM nodes only — ACME-managed nodes
    /// use `cosmix-webd acme renew`. Operator-gated (the listeners
    /// operator tier): the caller must be in `[webd.listeners] operators`,
    /// so a bare anonymous CLI invocation is denied unless `""` is an
    /// operator — drive it from an identity in that allowlist.
    Reload,
}

#[derive(Subcommand)]
enum AutoconfigAction {
    /// List the served-mail-domains autoconfig allowlist.
    ServedDomains,
}

#[derive(Subcommand)]
enum VhostAction {
    /// Create a `webd.vhosts` row through the daemon's `webd.vhost.add`
    /// Bus verb. The verb stamps `source = "bus.runtime"` and performs
    /// the tombstone-aware OCC anchor (`vhost.remove` → `vhost.add` is
    /// a valid flow). Required: `fqdn` + `www_dir`. This CLI rejects
    /// ACME companion fields without a provider; the daemon rejects
    /// provider without its required companions.
    Add {
        /// Primary FQDN of the vhost (substrate row key)
        fqdn: String,
        /// Static-file root for this vhost
        www_dir: String,
        /// ACME provider name (e.g. `letsencrypt`, `letsencrypt-staging`).
        /// Triggers an ACME plan instead of manual TLS.
        #[arg(long)]
        acme_provider: Option<String>,
        /// ACME challenge type (e.g. `http-01`).
        #[arg(long)]
        acme_challenge: Option<String>,
        /// ACME contact email for Let's Encrypt registration.
        #[arg(long)]
        acme_contact_email: Option<String>,
        /// Manual TLS certificate PEM path (mutually exclusive with ACME).
        #[arg(long)]
        tls_cert_path: Option<String>,
        /// Manual TLS private-key PEM path (mutually exclusive with ACME).
        #[arg(long)]
        tls_key_path: Option<String>,
        /// Disable on creation (defaults to enabled).
        #[arg(long)]
        disabled: bool,
    },
    /// List every `webd.vhosts` row (FQDN + enabled + TLS source)
    List,
    /// Show the full `webd.vhosts` row for a single FQDN
    Show {
        /// Primary FQDN of the vhost (substrate row key)
        fqdn: String,
    },
    /// Remove a `webd.vhosts` row. Idempotent: a missing row is
    /// reported but not treated as an error.
    Remove {
        /// Primary FQDN of the vhost (substrate row key)
        fqdn: String,
    },
}

#[derive(Subcommand)]
enum AcmeAction {
    /// Force-renew the ACME plan for a vhost. Bypasses cooldown +
    /// 30-day renewal-window gates; does NOT bypass disabled / apex-
    /// policy gates (those are policy, not timing). Returns synchronously
    /// `{ok, fqdn, state:"pending"}`; poll `acme status <fqdn>` (or
    /// watch `webd.props.audit.watch`) for the outcome.
    Renew {
        /// Primary FQDN of the vhost
        fqdn: String,
    },
    /// Show ACME provisioner state + derived status for a vhost.
    /// In the Phase 1 auth-policy stance (`vhosts_namespace.rs:489-517`)
    /// every WG peer holds `props.read:webd.vhosts:secrets`, so
    /// `last_error` is currently returned verbatim; this will narrow
    /// once the policy graduates from "every WG peer" to a tighter
    /// scope.
    Status {
        /// Primary FQDN of the vhost
        fqdn: String,
    },
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// A cached per-account session epoch and when this node loaded it.
pub(crate) struct CachedSessionEpoch {
    pub(crate) epoch: i64,
    pub(crate) loaded_at: std::time::Instant,
}

/// Per-account single-flight slot: `None` until the first load populates it.
pub(crate) type SessionEpochSlot = Arc<tokio::sync::Mutex<Option<CachedSessionEpoch>>>;

/// How long this node trusts a cached session epoch before re-reading the
/// per-vhost DB. Revocation is IN-PROCESS (the `webd.session.revoke` verb
/// updates the very same slot synchronously — see `bus::session_verbs`), so
/// the normal revoke path has ZERO staleness; this TTL only bounds an
/// unsupported out-of-band DB edit of `session_epochs`.
const SESSION_EPOCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-vhost session-epoch cache. The outer lock guards only slot
/// fetch/insert; each per-email slot single-flights cold/expired reads and is
/// the lock the revoke verb takes to publish a freshly-bumped epoch. Keyed by
/// email WITHIN a `VhostState`, so two vhosts sharing an email have separate
/// caches (per-vhost DB ⇒ per-vhost epoch).
#[derive(Default)]
pub(crate) struct SessionEpochCache {
    slots: tokio::sync::Mutex<HashMap<String, SessionEpochSlot>>,
}

impl SessionEpochCache {
    /// Fetch (or create) the single-flight slot for `email`. The outer map
    /// lock is held only for the lookup, never across the DB read.
    pub(crate) async fn slot(&self, email: &str) -> SessionEpochSlot {
        let mut slots = self.slots.lock().await;
        slots
            .entry(email.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    }

    /// Opportunistically drop slots that NO request currently references
    /// (`strong_count == 1` — only the map holds them, checked while the map
    /// lock is held so no task can clone concurrently) AND that hold no
    /// still-fresh epoch. Called only from the privileged, rare
    /// `webd.session.revoke` path (never the hot request path), so repeated
    /// revokes of distinct emails cannot grow a vhost's cache without bound.
    /// A referenced or still-fresh slot is always kept, so this can never
    /// evict an in-flight lookup or a hot account's cached epoch.
    pub(crate) async fn prune_unreferenced_stale(&self) {
        let mut slots = self.slots.lock().await;
        slots.retain(|_, slot| {
            // Another task still holds this Arc (an in-flight lookup) → keep.
            if Arc::strong_count(slot) > 1 {
                return true;
            }
            // Unreferenced: keep only a still-fresh epoch (a hot account we'd
            // otherwise have to re-read); drop empty or expired. `try_lock`
            // succeeds here — strong_count is 1, so no one else holds it.
            match slot.try_lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_some_and(|e| e.loaded_at.elapsed() <= SESSION_EPOCH_CACHE_TTL),
                Err(_) => true,
            }
        });
    }
}

#[cfg(test)]
mod session_epoch_cache_tests {
    use super::*;

    #[tokio::test]
    async fn slot_returns_stable_arc_per_email() {
        let cache = SessionEpochCache::default();
        let a1 = cache.slot("x@example.test").await;
        let a2 = cache.slot("x@example.test").await;
        assert!(Arc::ptr_eq(&a1, &a2), "same email → same slot Arc");
        let b = cache.slot("y@example.test").await;
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "different email → different slot Arc"
        );
    }

    #[tokio::test]
    async fn prune_drops_unreferenced_empty_slot() {
        let cache = SessionEpochCache::default();
        // Create the slot, then drop the returned Arc: only the map holds it,
        // and it is empty (never populated) — the drop-through pattern that
        // would otherwise accumulate on repeated revokes of distinct emails.
        drop(cache.slot("gone@example.test").await);
        assert_eq!(cache.slots.lock().await.len(), 1);
        cache.prune_unreferenced_stale().await;
        assert!(
            cache.slots.lock().await.is_empty(),
            "an unreferenced empty slot is pruned"
        );
    }

    #[tokio::test]
    async fn prune_keeps_referenced_slot() {
        let cache = SessionEpochCache::default();
        // Hold the Arc — models an in-flight lookup mid-DB-read.
        let _held = cache.slot("live@example.test").await;
        cache.prune_unreferenced_stale().await;
        assert_eq!(
            cache.slots.lock().await.len(),
            1,
            "a referenced slot is kept even while empty"
        );
    }

    #[tokio::test]
    async fn prune_keeps_fresh_epoch_slot() {
        let cache = SessionEpochCache::default();
        {
            let slot = cache.slot("fresh@example.test").await;
            *slot.lock().await = Some(CachedSessionEpoch {
                epoch: 7,
                loaded_at: std::time::Instant::now(),
            });
        } // drop the local Arc → unreferenced, but holds a still-fresh epoch
        cache.prune_unreferenced_stale().await;
        let slot = cache.slot("fresh@example.test").await;
        assert_eq!(
            slot.lock().await.as_ref().map(|e| e.epoch),
            Some(7),
            "a fresh unreferenced epoch is kept (not re-read)"
        );
    }
}

/// Per-vhost runtime state. One per resolved `[[webd.vhost]]` row,
/// plus one for the legacy top-level config if present. `Arc`-cloned
/// into each request via the `host_router` middleware's `Extension`.
///
/// `Debug` is hand-implemented (below), not derived, so the
/// `dev_session_password` secret is redacted and can never reach a log even
/// if a future site `?`-formats a whole `VhostState`.
pub(crate) struct VhostState {
    /// The primary FQDN for this vhost (`config.host`, lowercased).
    /// Host routing is keyed by `NodeState::vhosts`, not by this
    /// field; Bus read verbs (`webd.routes.list`, `webd.stats`)
    /// read it as the per-vhost primary key.
    fqdn: String,
    /// Per-vhost CMS SQLite. `None` ⇒ the `/api/posts*` routes 404
    /// for this vhost. `Arc` so the same connection can be shared into
    /// an embedded Mix handler's [`db::WebdDbHandler`] (`db_query`/
    /// `db_exec`) without a second connection — one serialized handle
    /// per vhost, as before.
    db: Option<Arc<Mutex<Connection>>>,
    /// Static-file root for this vhost.
    www_dir: PathBuf,
    /// Per-vhost JMAP upstream URL. `None` ⇒ `/jmap` routes 404.
    jmap_upstream: Option<String>,
    /// Per-vhost broker WebSocket URL. `None` ⇒ `/ws` routes 404.
    noded_ws: Option<String>,
    /// Per-vhost markdown documentation directory. `None` ⇒ `/docs`
    /// routes 404.
    docs_dir: Option<PathBuf>,
    /// DEV-ONLY auto-session identity, from `[[webd.vhost]] dev_session_*`.
    /// `Some` ⇒ on a non-external listener, a request with no valid
    /// `cosmix_session` cookie is treated as this maild identity (Basic auth
    /// to maild for `jmap()`). `None` for every non-dev vhost. The HARD gate
    /// is the per-request `!scope.external` check in `serve_static`; as
    /// defense-in-depth `resolve_node_state` additionally refuses at startup
    /// to bind a dev_session vhost reachable on an external or non-internal-
    /// bind listener. `dev_session_password` is redacted by the manual `Debug`.
    dev_session_email: Option<String>,
    dev_session_password: Option<String>,
    /// PUBLIC read-only content credential, from `[[webd.vhost]] public_read_*`.
    /// `Some` ⇒ an ANONYMOUS request (no session cookie, no dev_session) has its
    /// `jmap()` seam authenticate to maild via HTTP Basic as this identity — but
    /// it grants NO session (no `$SESSION`, no admin/manage). For a dedicated
    /// content account on a public `:443` listener. NOT internal-gated (the
    /// inverse of dev_session). `public_read_password` is redacted in `Debug`.
    public_read_email: Option<String>,
    public_read_password: Option<String>,
    /// SYSTEM transactional-sender credential, from `[[webd.vhost]]
    /// system_sender_*`. `Some` ⇒ webd can send transactional mail (2FA codes,
    /// registration confirm/approval links) FROM this account with no user
    /// session (the pre-auth path). The account is a dedicated maild account on
    /// this vhost's mail domain; maild's RFC 6409 §6.1 check pins the `From` to
    /// this address. Consumed ONLY by `send_system_mail`.
    /// `system_sender_password` is redacted in `Debug`.
    system_sender_email: Option<String>,
    system_sender_password: Option<String>,
    /// Email-2FA operator BREAK-GLASS, from `[[webd.vhost]]
    /// mfa_break_glass` (default `false`). The enrollment lookup fails
    /// CLOSED on an indeterminate read (broker down/timeout); `true`
    /// lets those logins proceed password-only — loudly logged per
    /// login — during a confirmed broker outage window. See
    /// [`account_requires_mfa`] / `WebdVhostConfig::mfa_break_glass`.
    mfa_break_glass: bool,
    /// Per-vhost response-class counters, populated by the
    /// `record_response_stats` middleware on every per-vhost
    /// response and read by `webd.stats`. Aliases share this `Arc`
    /// by construction (one `WebdStats` per `VhostState`, one
    /// `VhostState` per primary FQDN).
    stats: Arc<stats::WebdStats>,
    /// Per-account session-epoch cache for this vhost's CMS DB. Lets the hot
    /// authed-request epoch check ([`current_session_epoch`]) skip the
    /// per-vhost DB lock on a cache hit; the in-process `webd.session.revoke`
    /// verb updates the matching slot so a revocation is visible with zero
    /// staleness. Aliases share this by construction (one per `VhostState`).
    session_epoch_cache: SessionEpochCache,
    /// Per-vhost cache of rendered ANONYMOUS public responses, for routes that
    /// opted into the `public_cache` capability. Inert until a route declares
    /// it; a cache hit skips the whole render+DB path for the blog/content
    /// pages. Invalidated on any non-safe method to this vhost (`host_router`).
    /// Aliases share it (one per `VhostState`); see [`public_response_cache`].
    public_response_cache: public_response_cache::Cache,
}

impl std::fmt::Debug for VhostState {
    // Hand-rolled (not derived) so `dev_session_password` can NEVER render in
    // cleartext, even if a future log site `?`-formats a whole `VhostState`.
    // `db` shows presence only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostState")
            .field("fqdn", &self.fqdn)
            .field("db", &self.db.is_some())
            .field("www_dir", &self.www_dir)
            .field("jmap_upstream", &self.jmap_upstream)
            .field("noded_ws", &self.noded_ws)
            .field("docs_dir", &self.docs_dir)
            .field("dev_session_email", &self.dev_session_email)
            .field(
                "dev_session_password",
                &self.dev_session_password.as_ref().map(|_| "<redacted>"),
            )
            .field("public_read_email", &self.public_read_email)
            .field(
                "public_read_password",
                &self.public_read_password.as_ref().map(|_| "<redacted>"),
            )
            .field("system_sender_email", &self.system_sender_email)
            .field(
                "system_sender_password",
                &self.system_sender_password.as_ref().map(|_| "<redacted>"),
            )
            .field("mfa_break_glass", &self.mfa_break_glass)
            .field("stats", &self.stats)
            .finish()
    }
}

/// Per-listener request scope, injected as an axum `Extension` by the
/// listener wiring in `main()` (P2-C2). Carries the listener's vhost
/// allowlist so [`host_router`] can 404 a Host that belongs to a
/// *different* listener — defense-in-depth behind the kernel-level
/// socket isolation (a vhost served only on the WG interface must not
/// answer on the public socket even though the daemon knows the
/// Host). Cheap to clone — both `id` and `allowed_hosts` are `Arc`.
#[derive(Clone)]
struct ListenerScope {
    /// The owning [`ResolvedWebdListener`] id (log / debug aid).
    #[allow(dead_code)]
    id: Arc<str>,
    /// The vhost FQDNs this listener is allowed to serve (the
    /// `ResolvedWebdListener::hosts` allowlist, lowercased like the
    /// host-router keys). A single-listener (back-compat) node has
    /// every vhost here, so the check is a no-op.
    allowed_hosts: Arc<HashSet<String>>,
    /// Whether the owning listener is `external` (public-facing). A vhost's
    /// dev auto-session is NEVER honoured on an external listener (the hard
    /// per-request gate). An absent `ListenerScope` (single-listener back-
    /// compat node) is treated as external → fail-closed (no dev session).
    external: bool,
}

impl ListenerScope {
    fn allows(&self, host: &str) -> bool {
        self.allowed_hosts.contains(host)
    }
}

/// Is a listener `bind` address safe for a dev auto-session — i.e. NOT
/// reachable from the public internet? True only for a loopback, RFC1918 /
/// ULA, or link-local IP. An unspecified bind (`0.0.0.0` / `::`, which listens
/// on every interface incl. public) and any globally-routable address are
/// `false`, as is an unparseable bind (fail-closed). The startup dev_session
/// safety gate uses this to tie trust to the kernel-observable bind rather
/// than the operator's `external` flag alone.
fn bind_is_internal(bind: &str) -> bool {
    use std::net::IpAddr;
    let Ok(addr) = bind.parse::<std::net::SocketAddr>() else {
        return false;
    };
    match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

#[cfg(test)]
mod schema_and_path_tests {
    use super::{canonical_db_key, is_valid_schema_name};

    #[test]
    fn schema_name_injection_guard() {
        for ok in ["sshm", "aux1", "_x", "a_b_c"] {
            assert!(is_valid_schema_name(ok), "{ok:?} should be valid");
        }
        for bad in [
            "main",
            "temp",
            "",
            "1x",
            "Sshm",
            "a b",
            "a;drop",
            "a.b",
            "a\"b",
            &"x".repeat(33),
        ] {
            assert!(!is_valid_schema_name(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn canonical_db_key_aliases_collapse() {
        // Two spellings of the same not-yet-existing file under a real dir must
        // produce the same key (so the uniqueness check catches the alias) —
        // including a `..` in the non-existent tail.
        let base = std::env::temp_dir();
        let a = base.join("cosmix_dbkey_test/db.sqlite");
        let b = base.join("cosmix_dbkey_test/missing/../db.sqlite");
        std::fs::create_dir_all(base.join("cosmix_dbkey_test")).unwrap();
        assert_eq!(canonical_db_key(&a), canonical_db_key(&b));
        std::fs::remove_dir_all(base.join("cosmix_dbkey_test")).ok();
    }
}

#[cfg(test)]
mod dev_session_gate_tests {
    use super::bind_is_internal;

    #[test]
    fn internal_binds_are_safe() {
        // loopback / RFC1918 / ULA / link-local → dev_session permitted.
        assert!(bind_is_internal("127.0.0.1:443"));
        assert!(bind_is_internal("172.31.0.1:443")); // RFC1918 172.16/12
        assert!(bind_is_internal("10.0.0.1:443"));
        assert!(bind_is_internal("192.168.1.1:443"));
        assert!(bind_is_internal("169.254.0.1:443")); // IPv4 link-local
        assert!(bind_is_internal("[::1]:443")); // IPv6 loopback
        assert!(bind_is_internal("[fd00::1]:443")); // IPv6 ULA
        assert!(bind_is_internal("[fe80::1]:443")); // IPv6 link-local
    }

    #[test]
    fn public_and_unspecified_binds_are_unsafe() {
        // A public address, OR an unspecified bind (listens on every interface
        // incl. the public one), OR an unparseable string → fail-closed.
        assert!(!bind_is_internal("203.0.113.5:443")); // public (TEST-NET-3)
        assert!(!bind_is_internal("8.8.8.8:443"));
        assert!(!bind_is_internal("0.0.0.0:443")); // unspecified → treated public
        assert!(!bind_is_internal("[::]:443")); // IPv6 unspecified
        assert!(!bind_is_internal("[2606:4700::1]:443")); // public IPv6
        assert!(!bind_is_internal("not-an-addr")); // unparseable → fail-closed
    }
}

/// Per-listener [`ConnHandler`] adapter (P2-C2). The shared
/// `cosmix_daemon::listen::ListenerSet` owns bind + accept + guard +
/// TLS termination; this adapter is the post-accept half — it runs
/// the (already per-listener `ListenerScope`-layered) axum app over
/// the accepted stream via hyper. One instance per resolved listener,
/// each holding its own scoped `app` clone.
///
/// Replaces the old free-standing `serve_tls` / `serve_plain`
/// primary-listener loop. `serve_connection_with_upgrades` (rather
/// than the bare `serve_connection` the old `serve_tls` used) so a
/// `wss://` WebSocket upgrade works over the TLS path too — the plain
/// primary already had upgrades via `axum::serve`, and unifying both
/// surfaces under one handler makes the behaviour consistent.
struct WebdConnHandler {
    app: Router,
}

#[async_trait::async_trait]
impl ConnHandler for WebdConnHandler {
    async fn handle(&self, stream: AcceptedStream, _ctx: ConnCtx) {
        let svc = hyper_util::service::TowerToHyperService::new(self.app.clone());
        let builder =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
        match stream {
            AcceptedStream::Tcp(s) => {
                let io = hyper_util::rt::TokioIo::new(s);
                let _ = builder.serve_connection_with_upgrades(io, svc).await;
            }
            AcceptedStream::Tls(s) => {
                let io = hyper_util::rt::TokioIo::new(*s);
                let _ = builder.serve_connection_with_upgrades(io, svc).await;
            }
            // `AcceptedStream` is `#[non_exhaustive]`; webd only ever
            // sees `Tcp` (Plain listener) or `Tls` (Terminate). A new
            // variant should fail loudly rather than silently drop.
            other => tracing::error!(
                stream = ?std::mem::discriminant(&other),
                "webd handler: unexpected AcceptedStream variant — dropping connection"
            ),
        }
    }
}

/// Partition a flat, validated TLS identity list into per-listener
/// buckets keyed by listener id (P2-C2). Each identity's `server_name`
/// (a vhost host) is routed to the listener whose `hosts` allowlist
/// names it; `synthesize_listeners` has already proven every served
/// host belongs to exactly one listener, so a `server_name` with no
/// owning listener is a wiring bug — logged and skipped rather than
/// silently mis-served.
///
/// Each bucket is fed verbatim to `SniCertResolver::from_config`,
/// which applies its own "first identity becomes the default /
/// no-SNI fallback when none is flagged" rule per bucket — so the
/// single-bucket back-compat case is byte-identical to the old
/// flat-list resolver, and a multi-listener split gives each
/// listener its own default without leaking the others' certs.
pub(crate) fn partition_identities_by_listener(
    identities: &[TlsIdentityConfig],
    listeners: &[ResolvedWebdListener],
) -> HashMap<String, Vec<TlsIdentityConfig>> {
    let mut host_to_listener: HashMap<&str, &str> = HashMap::new();
    for l in listeners {
        for h in &l.hosts {
            host_to_listener.insert(h.as_str(), l.id.as_str());
        }
    }
    let mut by_listener: HashMap<String, Vec<TlsIdentityConfig>> = HashMap::new();
    for ident in identities {
        match host_to_listener.get(ident.server_name.as_str()) {
            Some(lid) => by_listener
                .entry((*lid).to_string())
                .or_default()
                .push(ident.clone()),
            None => tracing::warn!(
                server_name = %ident.server_name,
                "TLS identity for a host claimed by no listener — skipped \
                 (synthesize_listeners should have covered it)"
            ),
        }
    }
    by_listener
}

/// Node-wide state shared across every vhost. Created once at
/// startup and cloned into each request via axum `State`. Carries
/// only the genuinely node-scoped concerns: the autoconfig
/// admission set + resolver, the HTTP client (single
/// `reqwest::Client` per node), and the host-routing map.
struct NodeState {
    /// Hot-swappable host-routing snapshot (C3b). Carries every
    /// host-derived view — lookup by Host, plain-HTTP admit set,
    /// per-primary group with aliases — behind a single `ArcSwap`
    /// so the provisioner (C4) and the C5 ergonomic verbs can
    /// publish a new directory atomically. Every reader
    /// (`host_router` HTTPS dispatch, `plain_http_host_admit`
    /// redirect admit, `webd.routes.list`, `webd.stats`) takes a
    /// `load()` guard and reads through the snapshot — no parallel
    /// arc-swaps for `allowed` / `primaries`. See
    /// [`vhost_directory::VhostDirectory`] for the view shape and
    /// [`vhost_directory::from_namespace_rows`] for the C3e
    /// startup-time adapter (sources from the post-bootstrap
    /// `webd.vhosts` namespace snapshot, with the resolved runtime
    /// map threaded in for per-row wiring recovery).
    ///
    /// The outer `Arc` lets future consumers (provisioner, C5
    /// ergonomic verbs) hold a handle independent of the
    /// `Arc<NodeState>` lifetime.
    vhosts: Arc<ArcSwap<vhost_directory::VhostDirectory>>,
    /// HTTP client used by the JMAP reverse proxy and the autoconfig
    /// MX probe.
    http_client: reqwest::Client,
    /// Cached maild Bearer tokens for the `dev_session` / `public_read`
    /// jmap() SERVICE identities (perf). The cookie-session jmap() path
    /// already sends a Bearer; only these two seams sent HTTP Basic, which
    /// made maild re-run bcrypt cost-12 (~480ms) on EVERY jmap() call (3
    /// per SSR page → ~1.4s). Mint a maild Bearer ONCE per
    /// `(fqdn, upstream, service-email, purpose)` via `/auth/tokens/issue`
    /// and reuse it (`SERVICE_JMAP_TOKEN_TTL` = the requested ~1h minus a
    /// re-mint margin). The outer mutex is held only to fetch/insert the
    /// per-key slot; the inner per-key mutex single-flights the mint so a
    /// cold start does ONE bcrypt, not one per concurrent request. A rotated
    /// credential / revocation is honoured within that ~1h service-token TTL
    /// (or on webd restart). See [`service_jmap_bearer_auth`].
    service_jmap_tokens: Arc<ServiceJmapTokenMap>,
    /// P0c — per-email failed-login throttle (brute-force / credential-stuffing
    /// bound the connection-level `per_ip_rate` guard can't see). Ephemeral
    /// runtime state, shared across runtimes via the `Arc`. See [`login_throttle`].
    login_throttle: Arc<login_throttle::ThrottleMap>,
    /// P3 — email-2FA in-flight second factors, keyed by an opaque pending-id
    /// (the HttpOnly `cosmix_login_pending` cookie). Holds the live maild bearer
    /// and the emitted code's hash until the second factor passes. Ephemeral
    /// runtime state, shared across runtimes via the `Arc`. See [`login_pending`].
    login_pending: Arc<login_pending::PendingMap>,
    /// SSR PIM Phase 2 — per-node AEAD sealer for stateless session
    /// cookies. The `/auth/login` handler seals a maild bearer token into
    /// the `cosmix_session` cookie; `serve_static`'s `jmap()` seam
    /// unseals it to authorise SSR page navigations. Loaded (or
    /// generated) from `/var/lib/cosmix/webd/session.key` on the serving
    /// node; an ephemeral key on the bootstrap / test fixtures (which
    /// never serve login traffic). See [`session`].
    session: Arc<session::SessionSealer>,
    /// Mail domains this node serves autoconfig for, lowercased. The
    /// **security gate**: a `Host`-derived domain not in this set is
    /// `404`ed *before* any DNS lookup (maild-autoconfig.md §Security).
    /// Empty ⇒ autoconfig effectively disabled and `mx` is `None`.
    served_mail_domains: HashSet<String>,
    /// Internal mail host to advertise + probe in autoconfig instead of the
    /// domain's public MX (this node exposes 993/465 only on WG; the public
    /// FQDN is :25 + :443 only). `None` ⇒ legacy public-MX behaviour.
    autoconfig_mail_host: Option<String>,
    /// Lazily constructed only when `served_mail_domains` is non-empty,
    /// so a default (WG-only) node builds no resolver and issues no
    /// outbound DNS (`feedback_wg_only_binding`).
    mx: Option<mxresolve::MxResolver>,
    /// Pending HTTP-01 key authorisations, keyed by token. Populated
    /// by the Commit 5 `AcmeProvisioner` ahead of each
    /// order-finalisation `respond_to_challenge`, cleared as soon as
    /// the order is `valid`. Lookup happens on the plain-HTTP :80
    /// listener (`acme_challenge_serve`), so the map must be cheap
    /// to read concurrently; `RwLock` over a `HashMap` is the right
    /// shape (writes are rare — one per challenge — and reads are
    /// hot during validation polling). C4 only allocates and threads
    /// the map; the populating mutator lands with the provisioner.
    acme_challenges: Arc<RwLock<HashMap<String, String>>>,
    /// Backs `webd.tls.status`. Seeded from the resolved manual-PEM
    /// identities before the ACME branch so manual-PEM-only and
    /// HTTP-only deployments still expose the verb (the snapshot's
    /// `acme` field is `None`). When ACME exists, the
    /// `AcmeProvisioner` holds the matching sender and overwrites the
    /// snapshot after `startup_pass` and after every state-mutating
    /// branch of `run_forever`. See [`tls_status`].
    tls_status_rx: tokio::sync::watch::Receiver<tls_status::TlsStatusSnapshot>,
    /// SPEC-12 property router (Phase 3). Owns the `SqliteStore` for
    /// the `vhosts` namespace — declarative source-of-truth, no
    /// longer a runtime mirror. The `webd.props.*` family in
    /// [`bus::run`] dispatches against this router; the per-runtime
    /// fan-out dispatcher published over `webd.props.records.changed`
    /// also iterates `props_router.iter_runtimes()`. See
    /// [`vhosts_namespace`].
    ///
    /// `Arc` so `bus::run` can both dispatch and iterate runtimes
    /// to spawn dispatchers without awkward ownership.
    props_router: Arc<cosmix_props::PropsRouter>,
    /// SPEC-12 §15.5 subscribe-granter handle. Built before the Bus
    /// broker connect; `bus::run` calls `install_client` once the
    /// first `NodedClient::connect_default` succeeds. Until then any
    /// `webd.props.watch` returns `rc=10 grant_failed` (the typed
    /// degrade — see [`bus::subscribe_granter`]).
    props_subscribe_granter: Arc<bus::subscribe_granter::NodedSubscribeGranter>,
    /// Shared refreshable handle to the `NodedClient`. The granter
    /// and the [`bus::props_publisher::NodedPropsPublisher`] both
    /// load from this cell on every `grant()` / `publish()` call;
    /// `bus::run` swaps in the new client on every successful
    /// reconnect and stores `None` while a reconnect is in flight,
    /// so the watch + publish surfaces survive broker drops without
    /// re-spawning dispatchers (the dispatcher tasks themselves are
    /// daemon-lifetime — they own the projection cursor and would
    /// double-publish if re-spawned).
    ///
    /// Same `Arc<ArcSwapOption<NodedClient>>` cell that the granter
    /// holds internally — sharing avoids the "publisher pinned to
    /// the first client" failure mode Codex caught against the
    /// pre-rework C1b draft.
    broker_handle: bus::subscribe_granter::SharedBrokerHandle,
    /// SPEC-12 runtime handle for the `webd.vhosts` namespace. C5
    /// ergonomic Bus verbs ([`bus::vhost_verbs`]) read/write through
    /// this directly (not through the props router) because they
    /// stamp daemon-owned fields (`source = "bus_runtime"`) via
    /// `Runtime::set_with_origin(.., WriteOrigin::backend())` — a
    /// surface the props router does not expose. The runtime is the
    /// same instance also held by the provisioner and the props
    /// router; they all observe the same `before_set`/`after_set`
    /// hooks and the same OCC discipline.
    ///
    /// `Option<_>` so the bootstrap-only `NodeState` (pre-ACME
    /// :80 listener) and the middleware-test `synth_node` can be
    /// constructed without registering a real namespace. `None` ⇒
    /// the C5 verbs return `rc=10 webd.vhosts runtime not attached`
    /// rather than panicking.
    vhosts_runtime: Option<Arc<cosmix_props::Runtime>>,
    /// SPEC-12 runtime handle for the `webd.listeners` namespace (P3).
    /// The `webd.listener.{enable,disable,status}` ergonomic verbs
    /// ([`bus::listener_verbs`]) read/write through it (props.set the
    /// `enabled` field; the reaction loop applies it). `None` on the
    /// bootstrap / test fixtures ⇒ the verbs return `rc=10`.
    listeners_runtime: Option<Arc<cosmix_props::Runtime>>,
    /// L0 operator allowlist (`[webd.listeners] operators`) — the
    /// `listener_verbs` resolve the operator-tier write cap against
    /// this same list the namespace's AuthPolicy uses. Empty on
    /// fixtures (no operator ⇒ no write).
    listeners_operators: Vec<String>,
    /// Per-FQDN lock map shared with the ACME provisioner (C4b). The
    /// C5 `vhost.add` verb acquires the same lock identity the
    /// provisioner's `VhostRemoved` arm holds, so a `vhost.add`
    /// racing a cleanup serialises on the same outer mutex →
    /// inner-Arc lookup → inner mutex ladder. See
    /// [`acme_provisioner::FqdnLockMap`] and the spec's
    /// "Per-fqdn serialization lock" section.
    ///
    /// `Option<_>` for the same bootstrap/test-fixture reason as
    /// `vhosts_runtime`. `None` ⇒ C5 returns `rc=10`.
    vhost_key_locks: Option<acme_provisioner::FqdnLockMap>,
    /// Notify channel into the ACME provisioner's sweep loop. C5's
    /// `webd.acme.renew` verb calls `notify_one` here so the next
    /// sweep ticks immediately after queueing into
    /// [`Self::acme_force_renew_queue`]. The provisioner reads
    /// this same `Notify` in its `run_forever` loop's `select!`.
    ///
    /// `Option<_>` so no-ACME boots (no provisioner constructed at
    /// all) can leave it `None`; `webd.acme.renew` against such a
    /// node returns `rc=10 no ACME provisioner attached`. Captured
    /// via [`acme_provisioner::AcmeProvisioner::notify_handle`]
    /// *before* `tokio::spawn(provisioner.run_forever())` consumes
    /// the provisioner.
    acme_notify: Option<Arc<tokio::sync::Notify>>,
    /// Force-renew queue shared with the ACME provisioner. The C5
    /// `webd.acme.renew` verb inserts a fqdn here before calling
    /// `notify_one` on [`Self::acme_notify`]; the provisioner's
    /// `tick_once` drains this set at the top of each sweep and, for
    /// any plan whose fqdn appears in the drained snapshot, bypasses
    /// both the renewal-window gate and the per-vhost cooldown gate
    /// for that tick. Without this handle the verb would only kick
    /// the loop and any cert outside the 30-day window would silently
    /// remain unrenewed despite the operator's explicit request.
    ///
    /// `Option<_>` mirrors `acme_notify` — None on no-ACME boots.
    /// Captured via
    /// [`acme_provisioner::AcmeProvisioner::force_renew_handle`]
    /// alongside `notify_handle`, also before the provisioner moves
    /// into `tokio::spawn`.
    acme_force_renew_queue: Option<Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>>,
    /// Hot-swappable compiled `webd.handlers` route table (slice #3 of
    /// the maild/webd trust-split). `serve_static` consults this before
    /// static-file serving; a daemon-lifetime task rebuilds it from a
    /// fresh namespace snapshot on every `webd.handlers` change (the
    /// namespace hooks fire a dedicated `Notify`). Empty on
    /// bootstrap/test fixtures — those never serve embedded handlers.
    /// Same `Arc<ArcSwap<_>>` hot-swap shape as [`Self::vhosts`].
    handlers: Arc<ArcSwap<mix_handler::HandlerTable>>,
    /// Shared AST cache for embedded Mix handlers, keyed by resolved
    /// script path + mtime. Filled lazily by [`mix_handler::run`] and
    /// invalidated when a handler script's mtime changes.
    handler_ast_cache: Arc<mix_handler::AstCache>,
    /// B2 — `webd.tls.reload` live manual-PEM reload surface. `Some`
    /// only on a manual-PEM-capable node (no ACME provisioner + at least
    /// one TLS listener); `None` on ACME-managed nodes (the provisioner
    /// owns the resolvers — refresh via `webd.acme.renew`) and HTTP-only
    /// nodes (no TLS). The verb returns a helpful `rc=10` when `None`.
    /// See [`bus::tls`].
    tls_reload: Option<bus::tls::TlsReloadState>,
}

/// Which Basic seam a cached service token belongs to (keeps the dev and
/// public-read identities from ever sharing a cache slot).
#[derive(Clone, Hash, PartialEq, Eq)]
enum ServiceJmapTokenPurpose {
    DevSession,
    PublicRead,
    SystemSender,
}

/// Cache key for a service jmap Bearer: the vhost, the maild upstream, the
/// service account, and which seam minted it.
#[derive(Clone, Hash, PartialEq, Eq)]
struct ServiceJmapTokenKey {
    fqdn: String,
    upstream: String,
    email: String,
    purpose: ServiceJmapTokenPurpose,
}

/// A cached service Bearer + when this node should re-mint it.
struct CachedServiceJmapToken {
    token: String,
    expires_at: std::time::Instant,
}

/// Per-key single-flight slot: `None` until the first mint populates it.
type ServiceJmapTokenSlot = Arc<tokio::sync::Mutex<Option<CachedServiceJmapToken>>>;
/// The service-token map behind the `NodeState.service_jmap_tokens` outer lock.
type ServiceJmapTokenMap = tokio::sync::Mutex<HashMap<ServiceJmapTokenKey, ServiceJmapTokenSlot>>;

/// Lifetime webd REQUESTS for a service Bearer from maild's
/// `/auth/tokens/issue` (the `ttl_secs` body field). Short on purpose: a
/// minted Bearer is account-scoped, so it survives a password change — a
/// short TTL is what actually bounds the revocation tail for a rotated
/// dev_session/public_read credential (to ~1h). The cost is one re-mint
/// (one bcrypt) per hour per `(fqdn, upstream, service-email, purpose)`;
/// page traffic still rides the cached Bearer.
const SERVICE_JMAP_TOKEN_TTL_SECS: u64 = 3600;
/// Re-mint this long BEFORE the requested TTL elapses, so a cached Bearer is
/// never presented in its final moments.
const SERVICE_JMAP_TOKEN_CACHE_MARGIN_SECS: u64 = 300;
/// How long this node trusts a cached service Bearer: the TTL webd requested
/// minus the re-mint margin. Derived from the value webd itself sends, NOT a
/// mirror of any maild-side constant — so the two can never silently diverge,
/// and an older maild that ignores `ttl_secs` (mints 30 days) is harmless:
/// webd still re-mints hourly. No SQLite-datetime parsing of `expires_at`;
/// auth correctness stays with maild when the Bearer is presented.
const SERVICE_JMAP_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(
    SERVICE_JMAP_TOKEN_TTL_SECS - SERVICE_JMAP_TOKEN_CACHE_MARGIN_SECS,
);

/// Mint-once-and-cache a maild Bearer for a `dev_session` / `public_read`
/// SERVICE identity, so maild stops running bcrypt on every jmap() call.
/// Returns `Some("Bearer <tok>")`, or `None` on a mint failure — in which
/// case the caller leaves that seam's auth `None`, so a jmap() call gets a
/// maild 401 and the handler renders its login/redirect, exactly as a bad
/// Basic credential did (fail-closed). The per-key inner mutex single-
/// flights the mint: a cold-start burst for one identity does ONE
/// bcrypt-bearing `/auth/tokens/issue`, then every later call is a cheap
/// cache read + a Bearer maild verifies via SHA-256 + indexed lookup.
async fn service_jmap_bearer_auth(
    node: &NodeState,
    key: ServiceJmapTokenKey,
    password: &str,
) -> Option<String> {
    // Outer lock ONLY to fetch/insert the per-key slot, then drop it (never
    // held across the mint .await).
    let slot = {
        let mut map = node.service_jmap_tokens.lock().await;
        map.entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    };
    let mut guard = slot.lock().await;
    if let Some(cached) = guard.as_ref()
        && cached.expires_at > std::time::Instant::now()
    {
        return Some(format!("Bearer {}", cached.token));
    }
    // Stale/absent → mint. Mirrors the login mint (the one bcrypt cost is
    // amortised over `SERVICE_JMAP_TOKEN_TTL`).
    let issue_url = format!("{}/auth/tokens/issue", key.upstream.trim_end_matches('/'));
    let issued = node
        .http_client
        .post(&issue_url)
        .basic_auth(&key.email, Some(password))
        .json(&serde_json::json!({
            "label": "webd-service",
            "ttl_secs": SERVICE_JMAP_TOKEN_TTL_SECS,
        }))
        .send()
        .await;
    let token = match issued {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_string)),
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), email = %key.email, "service jmap token issue: non-success");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, email = %key.email, "service jmap token issue: request failed");
            None
        }
    };
    let token = token?;
    *guard = Some(CachedServiceJmapToken {
        token: token.clone(),
        expires_at: std::time::Instant::now() + SERVICE_JMAP_TOKEN_TTL,
    });
    Some(format!("Bearer {token}"))
}

/// P0c — the lockout expiry for `email` if it is currently rate-limited, else
/// None. Prunes the accessed key if its window has fully elapsed.
async fn login_lockout(node: &NodeState, email: &str) -> Option<std::time::Instant> {
    let now = std::time::Instant::now();
    let k = login_throttle::key(email);
    let mut map = node.login_throttle.lock().await;
    match map.get(&k) {
        Some(w) if w.is_stale(now) => {
            map.remove(&k);
            None
        }
        Some(w) => w.locked_until(now),
        None => None,
    }
}

/// P0c — record a failed login for `email` (engaging a lockout at the threshold).
/// Sweeps stale windows first if the map is at its soft cap, so a high-cardinality
/// email spray can't grow it without bound.
async fn login_record_failure(node: &NodeState, email: &str) {
    let now = std::time::Instant::now();
    let k = login_throttle::key(email);
    let mut map = node.login_throttle.lock().await;
    if !login_throttle::record_bounded(&mut map, k, now, login_throttle::MAP_CAP) {
        // Extreme: every one of MAP_CAP slots is actively locked, so there is no
        // non-locked entry to evict for this new email — fail open for it (the
        // connection per-IP guard remains the bound on the spray itself).
        tracing::warn!(
            "login throttle map saturated with locked entries; a new email is untracked this round"
        );
    }
}

/// P0c — clear an email's failure window after a successful login.
async fn login_clear_failures(node: &NodeState, email: &str) {
    let k = login_throttle::key(email);
    node.login_throttle.lock().await.remove(&k);
}

/// Atomically RESERVE one authentication attempt against `email`'s shared
/// failure window. Returns `false` if the email is ALREADY locked (the caller
/// refuses the guess WITHOUT checking it), else records the attempt (engaging
/// the lockout at the threshold) and returns `true`.
///
/// The whole check-then-record runs in ONE throttle-lock critical section, so
/// it is the serialization point that bounds TOTAL OTP guesses to
/// `login_throttle::MAX_FAILURES` per window — even when many verifies race
/// concurrently against many pre-minted pending challenges for the same email
/// (a check-lockout-then-record-later split races: every racer reads "not
/// locked" before any records, yielding >cap guesses — the Codex finding). A
/// correct credential clears the window afterwards (`login_clear_failures`),
/// so a reserved attempt on the eventual success costs nothing.
async fn login_try_consume_attempt(node: &NodeState, email: &str) -> bool {
    let now = std::time::Instant::now();
    let k = login_throttle::key(email);
    let mut map = node.login_throttle.lock().await;
    if let Some(w) = map.get(&k)
        && w.locked_until(now).is_some()
    {
        return false;
    }
    // Record (reserve) this attempt; engages the lockout once the count reaches
    // the threshold. `record_bounded` returns false ONLY in the extreme where
    // the map is saturated entirely with locked entries (no slot for a new
    // email) — refuse the guess in that case (fail CLOSED for callers using
    // atomic reservation: an untracked attempt would bypass the per-window
    // cap). Normal operation always tracks (insert/evict), so legit users are
    // unaffected outside a 50k-locked-email spray.
    login_throttle::record_bounded(&mut map, k, now, login_throttle::MAP_CAP)
}

/// P0a — transactional **system** mail-send: the capability behind email-2FA
/// codes and registration confirm/approval links. It sends FROM a vhost's
/// dedicated `system_sender` account (`noreply@<domain>`) with **no user
/// session**, so it works pre-auth (at 2FA time the login is not yet sealed; at
/// registration there is no account yet).
///
/// First wired by P3 (email-2FA at `POST /auth/login`, via `begin_mfa_challenge`);
/// P4 (registration confirm/approval) is the next caller. Plans:
/// `_plan/2026-06-29-p0a-webd-mail-send.md`, `_plan/2026-06-29-p3-email-2fa.md`.
mod system_mail {
    use super::{
        NodeState, ServiceJmapTokenKey, ServiceJmapTokenPurpose, VhostState,
        service_jmap_bearer_auth,
    };

    /// JMAP `using` set for the system send path — core + mail + submission (the
    /// only methods this path calls). A subset of the Mix seam's full union.
    const SYSTEM_MAIL_JMAP_USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:mail",
        "urn:ietf:params:jmap:submission",
    ];

    /// `Ok(())` ⇒ maild accepted the message for delivery. `Err(msg)` is a
    /// human-readable reason (the system-send path fails CLOSED — e.g. a 2FA
    /// login must not seal a session if the code could not be sent).
    pub(crate) type SystemMailResult = Result<(), String>;

    /// Send a plain-text transactional email FROM `vhost`'s configured
    /// `system_sender` account to `to`, with no user session.
    ///
    /// Mechanism mirrors the shipped SSR compose path (`h_pim_mail.mix`) but off
    /// the system identity, in Rust: mint+cache a maild Bearer for the system
    /// account, then over webd's own HTTP client to the vhost's `jmap_upstream`:
    /// `Identity/get` + `Mailbox/get` → build the RFC822 → blob upload →
    /// `Email/set` create in Sent (Inbox fallback, warned) → `EmailSubmission/set`.
    /// maild's RFC 6409 §6.1 check authorises the envelope because `From` == the
    /// system account.
    ///
    /// Uses direct `serde_json` over `node.http_client` (NOT `WebdJmapHandler`,
    /// the Mix-seam client): this runs in an axum handler on the MAIN runtime —
    /// the runtime `node.http_client` is bound to, exactly like
    /// `service_jmap_bearer_auth` which posts to maild the same way — and talks to
    /// our OWN maild with tiny trusted responses, so the seam client's per-request
    /// runtime + untrusted-response capping is neither available nor needed here.
    ///
    /// CALLER CONTRACT: this is an outbound-mail trigger. Do NOT call it from a
    /// public, unauthenticated path until rate-limiting (P0c) gates the caller, or
    /// it is a spam/abuse cannon.
    pub(crate) async fn send_system_mail(
        node: &NodeState,
        vhost: &VhostState,
        to: &str,
        subject: &str,
        body: &str,
    ) -> SystemMailResult {
        let upstream = vhost
            .jmap_upstream
            .as_deref()
            .ok_or("system mail: vhost has no jmap_upstream")?
            .trim_end_matches('/')
            .to_string();
        let (email, password) = match (
            vhost.system_sender_email.as_ref(),
            vhost.system_sender_password.as_ref(),
        ) {
            (Some(e), Some(p)) => (e.clone(), p.clone()),
            _ => return Err("system mail: vhost has no system_sender credential".into()),
        };

        // 1. Bearer for the system account (mint+cache, same infra as public_read).
        let auth = service_jmap_bearer_auth(
            node,
            ServiceJmapTokenKey {
                fqdn: vhost.fqdn.clone(),
                upstream: upstream.clone(),
                email: email.clone(),
                purpose: ServiceJmapTokenPurpose::SystemSender,
            },
            &password,
        )
        .await
        .ok_or("system mail: could not mint a system-sender token")?;

        // 2. Resolve the sender identity + the Sent mailbox (one batched request).
        let bootstrap = jmap_request(
            node,
            &upstream,
            &auth,
            serde_json::json!([["Identity/get", {}, "i"], ["Mailbox/get", {}, "b"]]),
        )
        .await?;
        let identity_id = parse_identity_id(&bootstrap)
            .ok_or("system mail: the system_sender account has no JMAP identity")?;
        // Store the outbound copy in Sent. A missing Sent on the DEDICATED
        // system_sender account signals misprovisioning — but transactional
        // delivery must not hinge on the sent-copy folder, so we still send and
        // fall back to Inbox. The fallback is WARNED (never silent, per the cold
        // review) so the account can be repaired.
        let sent_id = match parse_mailbox_id_by_role(&bootstrap, "sent") {
            Some(id) => id,
            None => {
                tracing::warn!(
                    account = %email,
                    "system_sender account has no Sent mailbox; storing the sent copy in Inbox \
                     (repair the account's role mailboxes)"
                );
                parse_mailbox_id_by_role(&bootstrap, "inbox")
                    .ok_or("system mail: the system_sender account has no Sent or Inbox mailbox")?
            }
        };

        // 3. Build + upload the RFC822 blob. Date is stamped here (kept out of the
        //    pure builder so the builder stays deterministic/testable).
        let date = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc2822)
            .unwrap_or_default();
        let rfc822 = build_transactional_rfc822(&email, to, subject, &date, body);
        let blob_id = jmap_upload(node, &upstream, &auth, rfc822.as_bytes()).await?;

        // 4. Create the Email in the resolved mailbox (Sent, or the warned Inbox
        //    fallback above). maild has NO creation-references, so we read the real
        //    id back before submitting — never a `#m` back-reference.
        let mut mailbox_ids = serde_json::Map::new();
        mailbox_ids.insert(sent_id, serde_json::Value::Bool(true));
        let created = jmap_request(
            node,
            &upstream,
            &auth,
            serde_json::json!([[
                "Email/set",
                {
                    "create": { "m": {
                        "mailboxIds": serde_json::Value::Object(mailbox_ids),
                        "keywords": { "$seen": true },
                        "blobId": blob_id,
                    }}
                },
                "c"
            ]]),
        )
        .await?;
        let email_id = parse_created_id(&created, "Email/set", "m")
            .ok_or("system mail: maild did not create the message")?;

        // 5. Submit for delivery. The envelope is derived from the To/From headers
        //    (no Bcc here); the §6.1 check passes because From == the system account.
        let submitted = jmap_request(
            node,
            &upstream,
            &auth,
            serde_json::json!([[
                "EmailSubmission/set",
                { "create": { "s": { "emailId": email_id, "identityId": identity_id } } },
                "s"
            ]]),
        )
        .await?;
        if !jmap_create_succeeded(&submitted, "EmailSubmission/set", "s") {
            return Err("system mail: maild saved the message but did not submit it".into());
        }
        Ok(())
    }

    /// POST a JMAP method-call batch to `upstream/jmap` as the system identity and
    /// return the parsed `methodResponses` array. `auth` is `Bearer <tok>`.
    /// Uncapped `resp.json()` is deliberate — same as `service_jmap_bearer_auth`,
    /// this is our own maild returning a tiny transactional response.
    async fn jmap_request(
        node: &NodeState,
        upstream: &str,
        auth: &str,
        method_calls: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{upstream}/jmap");
        let body = serde_json::json!({
            "using": SYSTEM_MAIL_JMAP_USING,
            "methodCalls": method_calls,
        });
        let resp = node
            .http_client
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("system mail: jmap request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "system mail: jmap returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("system mail: jmap returned malformed JSON: {e}"))?;
        json.get("methodResponses")
            .cloned()
            .ok_or_else(|| "system mail: jmap response missing methodResponses".to_string())
    }

    /// Upload the raw RFC822 to maild's blob endpoint as the system identity and
    /// return the minted `blobId`. maild derives the account from the Bearer and
    /// ignores the path accountId, so the `_` placeholder is correct (mirrors
    /// `WebdJmapHandler::upload`).
    async fn jmap_upload(
        node: &NodeState,
        upstream: &str,
        auth: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
        let url = format!("{upstream}/jmap/upload/_");
        let resp = node
            .http_client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "message/rfc822")
            .header(reqwest::header::AUTHORIZATION, auth)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| format!("system mail: blob upload failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "system mail: blob upload returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("system mail: upload returned malformed JSON: {e}"))?;
        json.get("blobId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "system mail: upload response missing a blobId".to_string())
    }

    /// Strip CR/LF and other C0 controls (keeping TAB, a legal folding char) from
    /// an RFC822 header VALUE, so a caller-supplied address/subject can't inject
    /// extra headers. Header-injection defence; mirrors `m_hdr_clean` in the Mix
    /// compose path.
    fn header_sanitize(s: &str) -> String {
        s.chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect()
    }

    /// Build a minimal RFC822 transactional message (plain UTF-8 text). Header
    /// values are CR/LF-stripped; the body's bare LFs are normalised to CRLF.
    /// Plain text (not HTML) on purpose: a 2FA code or a bare URL needs no markup,
    /// and plain text has no escaping/injection surface. `date` is the
    /// pre-formatted RFC 5322 date (passed in to keep this pure/testable).
    fn build_transactional_rfc822(
        from: &str,
        to: &str,
        subject: &str,
        date: &str,
        body: &str,
    ) -> String {
        const CRLF: &str = "\r\n";
        let body_norm = body
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', CRLF);
        let mut msg = String::new();
        msg.push_str(&format!("From: {}{CRLF}", header_sanitize(from)));
        msg.push_str(&format!("To: {}{CRLF}", header_sanitize(to)));
        msg.push_str(&format!("Subject: {}{CRLF}", header_sanitize(subject)));
        msg.push_str(&format!("Date: {}{CRLF}", header_sanitize(date)));
        msg.push_str(&format!("MIME-Version: 1.0{CRLF}"));
        msg.push_str(&format!("Content-Type: text/plain; charset=utf-8{CRLF}"));
        msg.push_str("Content-Transfer-Encoding: 8bit");
        msg.push_str(CRLF);
        msg.push_str(CRLF);
        msg.push_str(&body_norm);
        msg.push_str(CRLF);
        msg
    }

    /// The result payload of the FIRST methodResponse whose method is `method`, in
    /// a JMAP `methodResponses` array (each entry a `[method, payload, callId]`
    /// triple). A method-level `["error", …]` triple therefore yields `None` for
    /// the expected method — surfaced by the caller as a generic create failure.
    fn jmap_method_result<'a>(
        responses: &'a serde_json::Value,
        method: &str,
    ) -> Option<&'a serde_json::Value> {
        responses.as_array()?.iter().find_map(|triple| {
            let t = triple.as_array()?;
            (t.len() == 3 && t[0].as_str() == Some(method)).then(|| &t[1])
        })
    }

    /// The first JMAP Identity's id from an `Identity/get` result.
    fn parse_identity_id(responses: &serde_json::Value) -> Option<String> {
        let list = jmap_method_result(responses, "Identity/get")?
            .get("list")?
            .as_array()?;
        list.first()?.get("id")?.as_str().map(str::to_string)
    }

    /// The id of the mailbox with JMAP `role` from a `Mailbox/get` result, if any.
    fn parse_mailbox_id_by_role(responses: &serde_json::Value, role: &str) -> Option<String> {
        let list = jmap_method_result(responses, "Mailbox/get")?
            .get("list")?
            .as_array()?;
        list.iter().find_map(|m| {
            (m.get("role")?.as_str()? == role)
                .then(|| m.get("id")?.as_str().map(str::to_string))
                .flatten()
        })
    }

    /// The created object's id for `key` in a `<Foo>/set` result
    /// (`created.<key>.id`), or `None` if the create failed (maild reports a
    /// failed create under `notCreated`, never `created`).
    fn parse_created_id(responses: &serde_json::Value, method: &str, key: &str) -> Option<String> {
        jmap_method_result(responses, method)?
            .get("created")?
            .get(key)?
            .get("id")?
            .as_str()
            .map(str::to_string)
    }

    /// Whether a `<Foo>/set` create succeeded for `key`: `created.<key>` is
    /// present and non-null. Used for `EmailSubmission/set`, whose created object
    /// we don't need an id from — only confirmation it was queued.
    fn jmap_create_succeeded(responses: &serde_json::Value, method: &str, key: &str) -> bool {
        jmap_method_result(responses, method)
            .and_then(|p| p.get("created"))
            .and_then(|c| c.get(key))
            .is_some_and(|v| !v.is_null())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rfc822_has_headers_crlf_and_body() {
            let msg = build_transactional_rfc822(
                "noreply@example.org",
                "user@example.com",
                "Your code",
                "Tue, 01 Jul 2003 10:52:37 +0000",
                "Code: 123456\nUse it soon.",
            );
            assert!(msg.starts_with("From: noreply@example.org\r\n"), "{msg}");
            assert!(msg.contains("To: user@example.com\r\n"));
            assert!(msg.contains("Subject: Your code\r\n"));
            assert!(msg.contains("Date: Tue, 01 Jul 2003 10:52:37 +0000\r\n"));
            assert!(msg.contains("Content-Type: text/plain; charset=utf-8\r\n"));
            // Header/body separator + CRLF-normalised body.
            assert!(msg.contains("8bit\r\n\r\nCode: 123456\r\nUse it soon.\r\n"));
            // No bare LF anywhere (all normalised to CRLF).
            assert!(
                !msg.replace("\r\n", "").contains('\n'),
                "bare LF present: {msg:?}"
            );
        }

        #[test]
        fn rfc822_strips_header_injection() {
            // A To value carrying CRLF + a forged header must be flattened so the
            // forged "Bcc:" cannot become a real header line.
            let msg = build_transactional_rfc822(
                "noreply@example.org",
                "victim@example.com\r\nBcc: evil@example.net",
                "Hi\r\nX-Evil: 1",
                "Tue, 01 Jul 2003 10:52:37 +0000",
                "body",
            );
            // Exactly one CRLF after the To/Subject values (the injected CRLF is gone).
            assert!(
                msg.contains("To: victim@example.comBcc: evil@example.net\r\n"),
                "{msg}"
            );
            assert!(msg.contains("Subject: HiX-Evil: 1\r\n"), "{msg}");
            // No real injected header line.
            assert!(!msg.contains("\r\nBcc: evil@example.net"));
        }

        fn responses() -> serde_json::Value {
            serde_json::json!([
                ["Identity/get", { "list": [{ "id": "id-1", "email": "noreply@example.org" }] }, "i"],
                ["Mailbox/get", { "list": [
                    { "id": "mb-inbox", "role": "inbox" },
                    { "id": "mb-sent", "role": "sent" }
                ] }, "b"]
            ])
        }

        #[test]
        fn parses_identity_and_mailbox_roles() {
            let r = responses();
            assert_eq!(parse_identity_id(&r).as_deref(), Some("id-1"));
            assert_eq!(
                parse_mailbox_id_by_role(&r, "sent").as_deref(),
                Some("mb-sent")
            );
            assert_eq!(
                parse_mailbox_id_by_role(&r, "inbox").as_deref(),
                Some("mb-inbox")
            );
        }

        #[test]
        fn missing_role_is_none() {
            // No Sent present → the by-role lookup returns None. The orchestrator
            // then WARNS and falls back to Inbox; it never silently picks Inbox.
            let r = serde_json::json!([
                ["Mailbox/get", { "list": [{ "id": "mb-inbox", "role": "inbox" }] }, "b"]
            ]);
            assert_eq!(parse_mailbox_id_by_role(&r, "sent"), None);
            assert_eq!(
                parse_mailbox_id_by_role(&r, "inbox").as_deref(),
                Some("mb-inbox")
            );
        }

        #[test]
        fn parses_created_id_and_detects_success() {
            let created = serde_json::json!([
                ["Email/set", { "created": { "m": { "id": "e-9" } } }, "c"]
            ]);
            assert_eq!(
                parse_created_id(&created, "Email/set", "m").as_deref(),
                Some("e-9")
            );
            assert!(jmap_create_succeeded(&created, "Email/set", "m"));

            // A notCreated result → no id, not succeeded.
            let failed = serde_json::json!([
                ["Email/set", { "notCreated": { "m": { "type": "invalidProperties" } } }, "c"]
            ]);
            assert_eq!(parse_created_id(&failed, "Email/set", "m"), None);
            assert!(!jmap_create_succeeded(&failed, "Email/set", "m"));

            // A method-level error triple → no match for the expected method.
            let errored = serde_json::json!([["error", { "type": "serverFail" }, "c"]]);
            assert_eq!(parse_created_id(&errored, "Email/set", "m"), None);
            assert!(!jmap_create_succeeded(&errored, "Email/set", "m"));
        }
    }
}

/// P0c — per-email failed-login throttle. The connection-level `per_ip_rate`
/// guard caps per-IP request rate but can't see the ACCOUNT dimension, so a slow
/// attacker under that cap could still grind one account's password. After
/// `MAX_FAILURES` failed logins for an email within `WINDOW`, that email is
/// locked for `WINDOW`; the lockout also caps the residual bcrypt timing oracle
/// to a few probes per window. Tradeoff: a temporary per-account lockout is
/// itself a bounded, auto-recovering DoS lever — accepted, and the connection
/// per-IP guard bounds how fast one source can drive it. Wired into `login_post`;
/// reusable by the future registration handler (P4).
/// Email-2FA pending-login store (P3). After email+password validate, an
/// account that opted into 2FA gets a short numeric code emailed; the live
/// maild bearer + the code's SHA-256 hash are parked HERE, keyed by a random
/// opaque id handed to the browser (an HttpOnly cookie). The verify step looks
/// the id up, constant-time-compares the code hash, and only then seals the
/// real session — so the bearer never reaches the browser until the second
/// factor passes (D-P3-A: server-side pending map). Mirrors `login_throttle`:
/// in-memory, TTL-pruned, hard-bounded. Node-bound for the ~10-min window,
/// which is fine — sealed sessions are already per-node-key bound.
mod login_pending {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    /// Code lifetime AND the pending cookie's Max-Age.
    pub(crate) const TTL: Duration = Duration::from_secs(600);
    /// Human-readable TTL for the email body.
    pub(crate) const TTL_MINS: u64 = 10;
    /// Incorrect-code attempts before the pending row is burned (single sign-in
    /// challenge; the per-email `login_throttle` already bounds password tries).
    pub(crate) const MAX_ATTEMPTS: u32 = 5;
    /// Hard cap on the live map (same generous bound as the throttle map). Every
    /// row costs a successful password auth for a 2FA account to create, so this
    /// is unreachable in practice — it's a memory backstop, not a rate limit.
    pub(crate) const MAP_CAP: usize = 50_000;
    /// Emitted code length (digits).
    pub(crate) const CODE_DIGITS: u32 = 6;

    pub(crate) type PendingMap = tokio::sync::Mutex<HashMap<String, PendingLogin>>;

    /// One in-flight second factor. `code_hash` is SHA-256(code) — the plaintext
    /// code is never stored.
    pub(crate) struct PendingLogin {
        pub(crate) email: String,
        pub(crate) maild_token: String,
        pub(crate) code_hash: [u8; 32],
        pub(crate) expires_at: Instant,
        pub(crate) attempts: u32,
    }

    impl PendingLogin {
        pub(crate) fn is_expired(&self, now: Instant) -> bool {
            self.expires_at <= now
        }
    }

    /// The result of verifying a submitted code against a pending row.
    pub(crate) enum Verdict {
        /// Correct code — the row is consumed (single-use); carries the sealed-
        /// session inputs (the byte-exact email + the live maild bearer).
        Verified { email: String, maild_token: String },
        /// Wrong code, attempts remain — stay on the code step.
        BadCode,
        /// Wrong code, attempt cap reached — the row is burned; start over.
        TooManyAttempts,
        /// No such id, or it expired — start over.
        NotFound,
    }

    /// A uniform `CODE_DIGITS`-digit numeric code (leading zeros kept).
    /// Rejection-sampled from OS randomness to avoid modulo bias.
    pub(crate) fn generate_code() -> String {
        let modulus: u32 = 10u32.pow(CODE_DIGITS);
        // Largest multiple of `modulus` that fits in u32; reject above it so the
        // remainder is uniform over 0..modulus.
        let limit = (u32::MAX / modulus) * modulus;
        loop {
            let mut b = [0u8; 4];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
            let n = u32::from_le_bytes(b);
            if n < limit {
                return format!("{:0width$}", n % modulus, width = CODE_DIGITS as usize);
            }
        }
    }

    /// SHA-256 of a code's bytes.
    pub(crate) fn hash_code(code: &str) -> [u8; 32] {
        let d = Sha256::digest(code.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&d);
        out
    }

    /// Constant-time 32-byte equality (no early-exit timing oracle on the code).
    pub(crate) fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    /// Insert `entry` under `id`, enforcing `cap` as a HARD bound. When full,
    /// first sweeps expired rows; if still full, evicts the SOONEST-to-expire
    /// row. Returns `false` only if the map is full with nothing evictable (the
    /// caller then fails the login closed). `id` is a fresh 192-bit token, so a
    /// key collision never happens — `map.len()` never exceeds `cap`.
    pub(crate) fn insert_bounded(
        map: &mut HashMap<String, PendingLogin>,
        id: String,
        entry: PendingLogin,
        now: Instant,
        cap: usize,
    ) -> bool {
        if map.len() >= cap && !map.contains_key(&id) {
            map.retain(|_, e| !e.is_expired(now));
            if map.len() >= cap {
                let victim = map
                    .iter()
                    .min_by_key(|(_, e)| e.expires_at)
                    .map(|(k, _)| k.clone());
                match victim {
                    Some(v) => {
                        map.remove(&v);
                    }
                    None => return false,
                }
            }
        }
        map.insert(id, entry);
        true
    }

    /// Look up `id`, verify `code` at `now`, applying single-use + attempt-cap
    /// semantics. Expired/absent → `NotFound` (row removed). Correct → `Verified`
    /// (row removed). Wrong → `attempts++`, `BadCode` (kept) until the cap, then
    /// `TooManyAttempts` (row removed). An attempt is counted before the compare,
    /// so a correct guess on the final allowed try still verifies.
    pub(crate) fn verify(
        map: &mut HashMap<String, PendingLogin>,
        id: &str,
        code: &str,
        now: Instant,
    ) -> Verdict {
        match map.get_mut(id) {
            None => Verdict::NotFound,
            Some(e) if e.is_expired(now) => {
                map.remove(id);
                Verdict::NotFound
            }
            Some(e) => {
                e.attempts += 1;
                if ct_eq(&hash_code(code), &e.code_hash) {
                    let e = map.remove(id).expect("entry just matched");
                    Verdict::Verified {
                        email: e.email,
                        maild_token: e.maild_token,
                    }
                } else if e.attempts >= MAX_ATTEMPTS {
                    map.remove(id);
                    Verdict::TooManyAttempts
                } else {
                    Verdict::BadCode
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(code: &str, expires_at: Instant) -> PendingLogin {
            PendingLogin {
                email: "u@example.test".into(),
                maild_token: "tok-abc".into(),
                code_hash: hash_code(code),
                expires_at,
                attempts: 0,
            }
        }

        #[test]
        fn generate_code_is_six_uniform_digits() {
            for _ in 0..2000 {
                let c = generate_code();
                assert_eq!(c.len(), 6);
                assert!(c.chars().all(|ch| ch.is_ascii_digit()));
            }
        }

        #[test]
        fn verify_happy_path_is_single_use() {
            let mut m = HashMap::new();
            let now = Instant::now();
            insert_bounded(
                &mut m,
                "id1".into(),
                entry("123456", now + TTL),
                now,
                MAP_CAP,
            );
            match verify(&mut m, "id1", "123456", now) {
                Verdict::Verified { email, maild_token } => {
                    assert_eq!(email, "u@example.test");
                    assert_eq!(maild_token, "tok-abc");
                }
                _ => panic!("expected Verified"),
            }
            // Single-use: the row is consumed.
            assert!(matches!(
                verify(&mut m, "id1", "123456", now),
                Verdict::NotFound
            ));
        }

        #[test]
        fn verify_bad_code_then_cap_burns_row() {
            let mut m = HashMap::new();
            let now = Instant::now();
            insert_bounded(
                &mut m,
                "id1".into(),
                entry("000000", now + TTL),
                now,
                MAP_CAP,
            );
            for _ in 0..(MAX_ATTEMPTS - 1) {
                assert!(matches!(
                    verify(&mut m, "id1", "999999", now),
                    Verdict::BadCode
                ));
            }
            // The MAX_ATTEMPTS-th wrong try burns the row.
            assert!(matches!(
                verify(&mut m, "id1", "999999", now),
                Verdict::TooManyAttempts
            ));
            // Even the correct code can't revive a burned row.
            assert!(matches!(
                verify(&mut m, "id1", "000000", now),
                Verdict::NotFound
            ));
        }

        #[test]
        fn verify_expired_is_not_found_and_pruned() {
            let mut m = HashMap::new();
            let now = Instant::now();
            // expires_at == now → expired (is_expired is `<= now`).
            insert_bounded(&mut m, "id1".into(), entry("123456", now), now, MAP_CAP);
            assert!(matches!(
                verify(&mut m, "id1", "123456", now),
                Verdict::NotFound
            ));
            assert!(m.is_empty(), "expired row pruned on access");
        }

        #[test]
        fn ct_eq_matches_only_identical_hashes() {
            assert!(ct_eq(&hash_code("123456"), &hash_code("123456")));
            assert!(!ct_eq(&hash_code("123456"), &hash_code("123457")));
        }
    }
}

mod login_throttle {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// Failed logins for one email within `WINDOW` before a lockout engages.
    pub(crate) const MAX_FAILURES: u32 = 5;
    /// The failure-counting window AND the lockout duration.
    pub(crate) const WINDOW: Duration = Duration::from_secs(900);
    /// Hard cap on the live map. On overflow [`record_bounded`] sweeps elapsed
    /// windows, then evicts the LEAST-PROGRESSED non-locked entry (never a locked
    /// one), failing open only if every entry is locked — so the map never
    /// exceeds this size and an account accumulating failures is preferentially
    /// preserved under a spray.
    pub(crate) const MAP_CAP: usize = 50_000;

    pub(crate) type ThrottleMap = tokio::sync::Mutex<HashMap<String, FailureWindow>>;

    /// One email's failure window. Constructed via [`FailureWindow::new`]; all
    /// time decisions take an explicit `now` so the state machine is pure-testable.
    pub(crate) struct FailureWindow {
        count: u32,
        window_start: Instant,
        locked_until: Option<Instant>,
    }

    impl FailureWindow {
        pub(crate) fn new(now: Instant) -> Self {
            Self {
                count: 0,
                window_start: now,
                locked_until: None,
            }
        }

        /// The lockout expiry if this email is currently locked at `now`, else None.
        pub(crate) fn locked_until(&self, now: Instant) -> Option<Instant> {
            self.locked_until.filter(|t| *t > now)
        }

        /// Record a failed attempt at `now`. A fresh window starts when the prior
        /// one has fully elapsed (or on the first failure); a lockout engages once
        /// the count reaches the threshold.
        pub(crate) fn record_failure(&mut self, now: Instant) {
            if self.count == 0 || now.saturating_duration_since(self.window_start) >= WINDOW {
                self.window_start = now;
                self.count = 1;
                self.locked_until = None;
            } else {
                self.count += 1;
            }
            if self.count >= MAX_FAILURES {
                self.locked_until = Some(now + WINDOW);
            }
        }

        /// Safe to prune at `now`: not locked and the counting window has elapsed.
        pub(crate) fn is_stale(&self, now: Instant) -> bool {
            self.locked_until(now).is_none()
                && now.saturating_duration_since(self.window_start) >= WINDOW
        }
    }

    /// Normalised throttle key for an email — case-insensitive so case variants
    /// share a window (maild auth itself stays case-sensitive; this is only the
    /// throttle key, and an attacker must not bypass the lock by varying case).
    pub(crate) fn key(email: &str) -> String {
        email.trim().to_ascii_lowercase()
    }

    /// Record a failure for `k` in `map` at `now`, enforcing `cap` as a HARD
    /// bound. An EXISTING key is always updated (so a lockout can engage/extend).
    /// A NEW key, when the map is full, first sweeps elapsed windows; if still
    /// full it makes room by evicting the LEAST-PROGRESSED non-locked entry —
    /// lowest failure count, oldest window broken first — so a single-failure
    /// spray entry is reclaimed before an account that has accumulated failures.
    /// A locked entry is NEVER evicted. Only in the extreme case where EVERY
    /// entry is locked does it FAIL OPEN (returns `false`, tracks nothing). So
    /// `map.len()` never exceeds `cap`. Returns whether the failure was tracked.
    ///
    /// Least-progressed eviction is self-protecting for a real victim: each of
    /// its failures both advances its count AND lifts it above the count-1 spray
    /// floor, so it keeps climbing toward lockout. Residual (inherent to ANY
    /// fixed-memory per-email throttle — no policy escapes it): under a sustained
    /// high-cardinality spray a specific victim's window can still be churned out,
    /// and an all-locked map fails open. The connection-level `per_ip_rate` guard
    /// is the volumetric backstop that bounds the spray itself.
    pub(crate) fn record_bounded(
        map: &mut HashMap<String, FailureWindow>,
        k: String,
        now: Instant,
        cap: usize,
    ) -> bool {
        if map.len() >= cap && !map.contains_key(&k) {
            map.retain(|_, w| !w.is_stale(now));
            if map.len() >= cap {
                // Evict the least-progressed NON-locked entry (lowest count, then
                // oldest window); never a locked one. `count`/`window_start` are
                // visible here — same module as `FailureWindow`.
                let evictable = map
                    .iter()
                    .filter(|(_, w)| w.locked_until(now).is_none())
                    .min_by_key(|(_, w)| (w.count, w.window_start))
                    .map(|(key, _)| key.clone());
                match evictable {
                    Some(victim) => {
                        map.remove(&victim);
                    }
                    None => return false, // every entry locked → fail open
                }
            }
        }
        map.entry(k)
            .or_insert_with(|| FailureWindow::new(now))
            .record_failure(now);
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn locks_after_threshold_and_recovers_after_window() {
            let now = Instant::now();
            let mut w = FailureWindow::new(now);
            for _ in 0..MAX_FAILURES - 1 {
                w.record_failure(now);
            }
            assert!(
                w.locked_until(now).is_none(),
                "below threshold is not locked"
            );
            w.record_failure(now);
            assert!(w.locked_until(now).is_some(), "threshold engages a lockout");

            let after = now + WINDOW + Duration::from_secs(1);
            assert!(
                w.locked_until(after).is_none(),
                "lockout expires after WINDOW"
            );
            assert!(w.is_stale(after), "an elapsed, unlocked window is prunable");
        }

        #[test]
        fn window_resets_after_elapse() {
            let now = Instant::now();
            let mut w = FailureWindow::new(now);
            w.record_failure(now); // count 1
            let later = now + WINDOW + Duration::from_secs(1);
            w.record_failure(later); // window elapsed → fresh window, count 1
            assert!(
                w.locked_until(later).is_none(),
                "a single failure in a new window does not lock"
            );
        }

        #[test]
        fn key_is_trimmed_and_case_folded() {
            assert_eq!(key("  User@Example.COM "), "user@example.com");
        }

        #[test]
        fn record_bounded_is_a_hard_cap() {
            let now = Instant::now();
            let mut map = HashMap::new();
            // Fill to cap with distinct FRESH (non-locked) emails.
            for i in 0..3 {
                assert!(record_bounded(&mut map, format!("a{i}@x"), now, 3));
            }
            assert_eq!(map.len(), 3);
            // A NEW email at saturation evicts a non-locked entry and IS tracked;
            // size stays capped (the cap is a hard bound).
            assert!(record_bounded(&mut map, "new@x".into(), now, 3));
            assert_eq!(map.len(), 3);
            assert!(
                map.contains_key("new@x"),
                "a new account is still trackable under a spray"
            );
            // An EXISTING email is always updatable despite saturation.
            assert!(record_bounded(&mut map, "new@x".into(), now, 3));
            assert_eq!(map.len(), 3);
            // An elapsed window is swept to reclaim room.
            let later = now + WINDOW + Duration::from_secs(1);
            assert!(record_bounded(&mut map, "fresh@x".into(), later, 3));
            assert!(map.len() <= 3);
        }

        #[test]
        fn record_bounded_evicts_least_progressed_first() {
            let now = Instant::now();
            let mut map = HashMap::new();
            // Two count-1 spray entries + one progressed (count-3) victim.
            assert!(record_bounded(&mut map, "spray1@x".into(), now, 3));
            assert!(record_bounded(&mut map, "spray2@x".into(), now, 3));
            let mut victim = FailureWindow::new(now);
            for _ in 0..3 {
                victim.record_failure(now);
            }
            map.insert("victim@x".into(), victim);
            assert_eq!(map.len(), 3);
            // A new email evicts a least-progressed (count-1) entry, NOT the
            // progressed victim — so the victim keeps climbing toward lockout.
            assert!(record_bounded(&mut map, "new@x".into(), now, 3));
            assert_eq!(map.len(), 3);
            assert!(
                map.contains_key("victim@x"),
                "the progressed victim survives a count-1 spray"
            );
        }

        #[test]
        fn record_bounded_never_evicts_a_locked_entry() {
            let now = Instant::now();
            let mut map = HashMap::new();
            // Fill the map with LOCKED entries only.
            for i in 0..3 {
                let mut w = FailureWindow::new(now);
                for _ in 0..MAX_FAILURES {
                    w.record_failure(now);
                }
                assert!(w.locked_until(now).is_some());
                map.insert(format!("L{i}@x"), w);
            }
            assert_eq!(map.len(), 3);
            // No non-locked slot to reclaim → fail open; no locked entry evicted.
            assert!(!record_bounded(&mut map, "victim@x".into(), now, 3));
            assert_eq!(map.len(), 3);
            assert!(!map.contains_key("victim@x"));
            for i in 0..3 {
                assert!(
                    map.contains_key(&format!("L{i}@x")),
                    "locked accounts are preserved"
                );
            }
        }
    }
}

impl NodeState {
    /// Resolve a normalised lowercase Host through the current
    /// [`vhost_directory::VhostDirectory`] snapshot. Used by
    /// `host_router` on every HTTPS dispatch; the `load()` guard
    /// drops at the end of the expression so a publish in flight
    /// does not extend the request-path snapshot lifetime.
    fn vhost_for_host(&self, host: &str) -> Option<Arc<VhostState>> {
        self.vhosts.load().vhost_for_host(host)
    }
}

// ---------------------------------------------------------------------------
// Post model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Post {
    id: i64,
    slug: String,
    title: String,
    content: String,
    published: bool,
    created: String,
    updated: String,
}

#[derive(Debug, Deserialize)]
struct CreatePost {
    slug: String,
    title: String,
    content: String,
    #[serde(default)]
    published: bool,
}

#[derive(Debug, Deserialize)]
struct UpdatePost {
    slug: Option<String>,
    title: Option<String>,
    content: Option<String>,
    published: Option<bool>,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT UNIQUE NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    created TEXT NOT NULL DEFAULT (datetime('now')),
    updated TEXT NOT NULL DEFAULT (datetime('now'))
);
-- session_epochs — webd-owned cookie-path revocation counters (see
-- session.rs module doc). One row per account email; a missing row is
-- epoch 0. Bumped by the `webd.session.revoke` Bus verb; the value
-- current at login is sealed into the cookie and re-checked on every
-- cookie-authorized request. Deliberately SEPARATE from the Mix-owned
-- `users` table so identities without a CMS staff row are revocable too.
CREATE TABLE IF NOT EXISTS session_epochs (
    email TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL DEFAULT 0
);
"#;

/// Validate an aux-database schema identifier: `[a-z_][a-z0-9_]{0,31}`, and
/// never a reserved SQLite schema name. This is interpolated into
/// `ATTACH DATABASE ?1 AS <name>` (SQLite cannot bind a schema name), and is
/// the same predicate the `db-schema:<name>` route capability is checked
/// against, so it must be proven safe against injection.
pub(crate) fn is_valid_schema_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 || name == "main" || name == "temp" {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A canonical identity key for a database file that may not exist yet.
///
/// Canonicalize the deepest EXISTING prefix of the raw path first — that resolves
/// every real symlink (including a `symlink/..` sequence) correctly — then apply
/// only the non-existent remainder LEXICALLY (`.`/`..` popping is sound there
/// because a non-existent tail can contain no symlinks). Two paths naming the
/// same file via `.`/`..`/a symlinked ancestor collapse to the same key, so the
/// per-vhost uniqueness check can't be aliased around.
fn canonical_db_key(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let comps: Vec<Component> = path.components().collect();
    // Largest prefix (by component count) that canonicalizes = the deepest
    // existing part; walk down from the full path so symlinks are resolved.
    for k in (1..=comps.len()).rev() {
        let mut prefix = PathBuf::new();
        for c in &comps[..k] {
            prefix.push(c.as_os_str());
        }
        if let Ok(base) = prefix.canonicalize() {
            let mut key = base;
            for c in &comps[k..] {
                match c {
                    Component::ParentDir => {
                        key.pop();
                    }
                    Component::CurDir => {}
                    other => key.push(other.as_os_str()),
                }
            }
            return key;
        }
    }
    path.to_path_buf() // nothing canonicalizes; use raw
}

/// Open a vhost SQLite connection and ATTACH any auxiliary databases.
///
/// `aux` is a list of `(schema_name, path)`. Each aux database is created
/// (parents included) if absent and opened WAL + `synchronous=NORMAL`. A bad
/// aux row (invalid name, duplicate name, un-creatable path, failed ATTACH)
/// returns `Err` — callers fail the affected vhost soft, never the node. The
/// aux path is bound as a parameter; only the pre-validated schema name is
/// interpolated.
fn open_db(path: &std::path::Path, aux: &[(String, std::path::PathBuf)]) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // busy_timeout mirrors the props-substrate connection: the vhost
    // connection is shared across concurrent request tasks, so a brief lock
    // wait must retry rather than fail the request.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )?;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (name, aux_path) in aux {
        if !is_valid_schema_name(name) {
            anyhow::bail!("invalid aux_db schema name {name:?}");
        }
        if !seen.insert(name.as_str()) {
            anyhow::bail!("duplicate aux_db schema name {name:?}");
        }
        if let Some(parent) = aux_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating aux_db parent dir {}", parent.display()))?;
        }
        let aux_str = aux_path.to_str().ok_or_else(|| {
            anyhow::anyhow!("aux_db path {} is not valid UTF-8", aux_path.display())
        })?;
        // Schema name is validated above; path is bound.
        conn.execute(&format!("ATTACH DATABASE ?1 AS \"{name}\""), [aux_str])
            .with_context(|| format!("attaching aux_db {name} at {aux_str}"))?;
        conn.execute_batch(&format!(
            "PRAGMA \"{name}\".journal_mode=WAL; PRAGMA \"{name}\".synchronous=NORMAL;"
        ))
        .with_context(|| format!("configuring aux_db {name}"))?;
    }
    Ok(conn)
}

fn init_db(path: &std::path::Path) -> Result<()> {
    let conn = open_db(path, &[])?;
    conn.execute_batch(SCHEMA)?;
    info!("database initialised at {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

/// Authorise a CMS mutation (`/api/posts*` POST/PUT/DELETE). Returns
/// `None` when the request is an authorised admin mutation, or
/// `Some(rejection)` to short-circuit: `401` when no/invalid session
/// cookie, `403` on CSRF mismatch. `Option<Response>` rather than
/// `Result<(), Response>` keeps the large `Response` out of a `Result`
/// Err variant.
///
/// CSRF uses the double-submit pattern: the request must carry an
/// `X-CSRF-Token` header equal to the session-bound token (the JSON API
/// has no form field to carry it, unlike `logout_post`). The browser
/// sends the `cosmix_session` cookie automatically, so the header — which
/// a cross-origin attacker cannot read or set to the victim's token — is
/// the CSRF defence. Compared in constant time via `session::csrf_eq`.
/// CMS role rank from the unified maild session via the per-vhost `users`
/// table (login-unification RBAC). `None` = no valid session; otherwise
/// `(email, rank, csrf)` with user=1, author=2, admin=3 (an authenticated
/// email with no `users` row is rank 1 — signed in, no CMS staff role).
/// Async: takes the per-vhost db lock to read the role.
async fn cms_session_role(
    node: &NodeState,
    vhost: &VhostState,
    headers: &HeaderMap,
) -> Option<(String, i64, String)> {
    let payload = session::cookie_value(headers, session::SESSION_COOKIE)
        .and_then(|c| node.session.unseal(&c, &vhost.fqdn, session::now_secs()))?;
    // A billing-portal (kind="customer") session carries NO CMS/admin authority,
    // EVER — even if the customer's billing email collides with a users-table
    // admin/author grant. This is the Rust half of the escalation guard: the
    // webd-native admin gates (require_post_admin, has_admin_session,
    // build_bus_injection) all funnel through here, so refusing a customer
    // session a role here caps them regardless of any Mix-side check (Codex
    // BLOCKER — the kind cap must hold in Rust, not only in Mix). Admin
    // authority requires a maild-account session.
    if payload.kind != "maild" {
        return None;
    }
    let email = payload.email.clone();
    let db_mtx = vhost.db.as_ref()?;
    let (role, live_epoch) = {
        let db = db_mtx.lock().await;
        tokio::task::block_in_place(|| {
            let role = db
                .query_row(
                    "SELECT role FROM users WHERE username = ?1",
                    rusqlite::params![email],
                    |r| r.get::<_, String>(0),
                )
                .unwrap_or_else(|_| "user".to_string());
            (role, query_session_epoch(&db, &email))
        })
    };
    // Cookie-path revocation (2026-07 audit): a payload sealed under an
    // older epoch is DEAD regardless of its TTL — same outcome as no
    // cookie at all (no oracle distinguishing "revoked" from "expired").
    if payload.epoch != live_epoch {
        return None;
    }
    let rank = match role.as_str() {
        "admin" => 3,
        "author" => 2,
        _ => 1,
    };
    Some((email, rank, payload.csrf))
}

/// Live session epoch for `email` in an open per-vhost DB (sync — call
/// with the DB lock held, inside `block_in_place`). No row ⇒ 0 (the
/// virgin state every pre-epoch cookie sealed as). A real query error ⇒
/// -1, which no sealed cookie can carry (seals are ≥ 0) — the epoch
/// gate then fails CLOSED rather than quietly reverting to
/// pre-revocation behavior.
fn query_session_epoch(db: &Connection, email: &str) -> i64 {
    match db.query_row(
        "SELECT epoch FROM session_epochs WHERE email = ?1",
        rusqlite::params![email],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(e) => e,
        Err(rusqlite::Error::QueryReturnedNoRows) => 0,
        Err(e) => {
            tracing::warn!(error = %e, "session_epochs read failed; failing closed");
            -1
        }
    }
}

/// Async wrapper over [`query_session_epoch`] for the seal/inject paths:
/// takes the vhost DB lock. A vhost with NO CMS DB has no epoch store —
/// every account is epoch 0 there (its cookie authority is maild-backed
/// only, so the bearer path is the revocation lever).
///
/// The query runs INLINE (no `block_in_place`): a single-row primary-key
/// SELECT is microseconds, and `block_in_place` panics on a
/// current-thread runtime (e.g. `#[tokio::test]`), which this path — the
/// hot per-request seal/inject check — must never risk.
async fn current_session_epoch(vhost: &VhostState, email: &str) -> i64 {
    let Some(db_mtx) = vhost.db.as_ref() else {
        return 0;
    };

    // Per-email single-flight slot: a fresh cache hit skips the per-vhost DB
    // lock entirely (the concurrency win — 100 authed requests no longer
    // serialize on that lock); a miss single-flights the read so a cold burst
    // for one account does ONE SELECT. Lock order is ALWAYS slot → DB (the
    // revoke verb takes the same order); never hold the DB lock across a slot
    // await.
    let slot = vhost.session_epoch_cache.slot(email).await;
    let mut cached = slot.lock().await;
    if let Some(entry) = cached.as_ref()
        && entry.loaded_at.elapsed() <= SESSION_EPOCH_CACHE_TTL
    {
        return entry.epoch;
    }

    let epoch = {
        let db = db_mtx.lock().await;
        query_session_epoch(&db, email)
    };

    // NEVER cache the fail-closed sentinel (-1): a transient SQLite error must
    // fail THIS request closed without pinning -1 into the slot and locking
    // the account out for the whole TTL. Only a real epoch (≥ 0) is cached.
    if epoch >= 0 {
        *cached = Some(CachedSessionEpoch {
            epoch,
            loaded_at: std::time::Instant::now(),
        });
    } else {
        *cached = None;
    }
    epoch
}

/// Authorise a CMS mutation (`/api/posts*`): a valid session AND a CMS
/// `author`+ role (the unified RBAC — NOT any maild session) AND a matching
/// CSRF header. `None` = authorised; `Some(rejection)` short-circuits.
async fn require_post_admin(
    node: &NodeState,
    vhost: &VhostState,
    headers: &HeaderMap,
) -> Option<Response> {
    let (_, rank, csrf) = match cms_session_role(node, vhost, headers).await {
        Some(x) => x,
        None => return Some(StatusCode::UNAUTHORIZED.into_response()),
    };
    let header_token = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !session::csrf_eq(&csrf, header_token) {
        return Some(StatusCode::FORBIDDEN.into_response());
    }
    if rank < 2 {
        return Some(StatusCode::FORBIDDEN.into_response());
    }
    None
}

/// Whether the request carries a CMS `author`+ session (for read methods
/// that widen visibility to unpublished drafts). No CSRF (safe method).
async fn has_admin_session(node: &NodeState, vhost: &VhostState, headers: &HeaderMap) -> bool {
    matches!(cms_session_role(node, vhost, headers).await, Some((_, rank, _)) if rank >= 2)
}

/// Parse an `Origin`/`Referer` into `(scheme, host, port?)`. Port-AWARE
/// (cookies are host-scoped, NOT port-scoped, so the bus gate must reject a
/// same-host-DIFFERENT-port origin — a hostile HTTPS service on another port of
/// the same host).
fn bus_parse_origin(v: &str) -> Option<(&str, &str, Option<&str>)> {
    let (scheme, rest) = v.split_once("://")?;
    let authority = rest.split('/').next()?; // host[:port]
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if scheme.is_empty() || host.is_empty() {
        None
    } else {
        Some((scheme, host, port))
    }
}

/// Whether `v` is a same-origin `https` URL for `fqdn` with the default port
/// (absent or `443`).
fn bus_origin_matches(v: &str, fqdn: &str) -> bool {
    bus_parse_origin(v).is_some_and(|(scheme, host, port)| {
        scheme.eq_ignore_ascii_case("https")
            && host.eq_ignore_ascii_case(fqdn)
            && matches!(port, None | Some("443"))
    })
}

/// Whether a matched Mix-handler request must present a strict same-origin
/// Origin/Referer (the handler CSRF gate, Codex D8 #1). True iff the method is
/// NOT a safe method (anything but GET/HEAD/OPTIONS — so an `ANY`-method route
/// can't mutate through an unlisted verb) AND the caller is authenticated (a
/// session cookie OR an ambient internal `dev_session` — CSRF only matters when
/// there is authority to ride). Applies to EVERY authenticated mutating handler
/// request, bus routes included: the delegated-bus gate only withholds the
/// `bus_call` capability, but the Mix handler still runs and can commit a `db`
/// / `jmap` mutation, so same-origin must be enforced here for bus routes too.
fn handler_post_needs_csrf(method: &axum::http::Method, authenticated: bool) -> bool {
    let safe = matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    !safe && authenticated
}

/// Whether a request carries a **present, cross-origin** `Origin`/`Referer` —
/// the actual CSRF attack signal for the general handler gate. `Origin` is
/// authoritative when present (`Referer` is the fallback only when `Origin` is
/// absent). Returns `true` ONLY when a header is present AND fails the
/// scheme/host/port check; **both absent → `false`** (NOT an attack).
///
/// This is the key difference from `bus_post_same_origin` (which fails closed on
/// both-absent). Under `SameSite=Lax`, the sole exploitable CSRF vector is a
/// same-SITE cross-ORIGIN request (a sibling subdomain — cross-SITE POSTs get no
/// cookie), and a browser ALWAYS emits `Origin` on such a POST (there is no
/// browser API to send a credentialed cross-origin POST without it). So a POST
/// with NEITHER header is provably not the attack — it is a non-browser client
/// (an `X-*-Secret`-authed worker, an operator CLI) or a rare privacy browser,
/// and rejecting it is a false positive. Fleet evidence (2026-07-13): the
/// both-absent fail-closed rule 403-looped the sshm + provisiond drain workers on
/// internal `dev_session` vhosts and fired once on the my.renta.net portal. The
/// delegated-bus path keeps `bus_post_same_origin` (fail-closed) — it is reached
/// only by admin browser forms, which always send `Origin`, so its stricter rule
/// never false-positives and stays as defence-in-depth on token mutation.
fn handler_origin_is_cross_site(headers: &HeaderMap, fqdn: &str) -> bool {
    // A PRESENT header (even non-UTF-8) is authoritative: a real browser
    // serialises `Origin` as ASCII, so garbage bytes are never a legitimate
    // same-origin request — treat present-but-unparseable as a mismatch (reject),
    // never as "absent" (which would allow it through).
    if let Some(o) = headers.get(axum::http::header::ORIGIN) {
        return o.to_str().ok().is_none_or(|s| !bus_origin_matches(s, fqdn));
    }
    if let Some(r) = headers.get(axum::http::header::REFERER) {
        return r.to_str().ok().is_none_or(|s| !bus_origin_matches(s, fqdn));
    }
    false
}

/// STRICT same-origin check for the POST-mutating delegated-bus path. `Origin`
/// is AUTHORITATIVE when present (a present-but-wrong Origin fails); `Referer`
/// is the fallback only when `Origin` is absent; BOTH absent FAILS. Required
/// because `SameSite=Lax` does NOT block a same-site cross-ORIGIN POST (a
/// sibling `*.vhost` serving an auto-submitting form), so the session cookie
/// alone is not a sufficient CSRF defence for a delegated TOKEN MUTATION.
/// (Codex C2 end-to-end finding.)
fn bus_post_same_origin(headers: &HeaderMap, fqdn: &str) -> bool {
    if let Some(o) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        return bus_origin_matches(o, fqdn);
    }
    if let Some(r) = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        return bus_origin_matches(r, fqdn);
    }
    false
}

/// Minimum gap between two accelerator wakes for the SAME vhost. A wake starts a
/// systemd oneshot, so an unthrottled caller could spam unit starts; Codex asked for
/// this bound explicitly. Dropping a wake costs only latency — the backstop timer
/// still drains the queue — so throttling is always safe.
///
/// This is a FLOOR against pathological spam, NOT the primary control: the enqueue
/// path already caps a caller at 6 runs/minute per identity, so real wake volume is
/// tiny. Keep it well below human click cadence — at 500ms it dropped the wake for a
/// second run enqueued moments after the first, stranding that run until the backstop
/// timer (up to a minute later), which is exactly the latency the wake exists to avoid.
const WAKE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Per-vhost last-accelerator-wake clock. Returns true (and records now) when a wake
/// is allowed. Deliberately tiny and self-contained: the accelerator path is the only
/// caller, and a coarse global lock is cheap at wake frequency.
fn wake_rate_limit_ok(fqdn: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = map.lock() else {
        // A poisoned lock must not become a way to bypass the limiter.
        return false;
    };
    let now = Instant::now();
    match guard.get(fqdn) {
        Some(prev) if now.duration_since(*prev) < WAKE_MIN_INTERVAL => false,
        _ => {
            guard.insert(fqdn.to_string(), now);
            true
        }
    }
}

/// Build the delegated-`bus_call` injection for a matched route that granted
/// `bus:<verb>` capabilities — ONLY if webd's Rust admin + CSRF gate passes
/// (vtoken C2). Returns `None` (⇒ no `Bus` capability, no handler — `bus_call`
/// raises "not available") on ANY gate failure, logging the reason; the route's
/// own Mix `require_role` is page-level defence on top. The gate is THE
/// security boundary for `bus_call`.
async fn build_bus_injection(
    node: &NodeState,
    vhost: &VhostState,
    headers: &HeaderMap,
    method: &str,
    matched: &mix_handler::MatchedHandler,
    dev_ambient: bool,
) -> Option<mix_handler::BusInjection> {
    // vtoken management is ADMIN-only (rank 3), stricter than the author≥2 the
    // posts API uses.
    let (email, rank, csrf) = match cms_session_role(node, vhost, headers).await {
        Some(x) => x,
        None => {
            // ACCELERATOR EXCEPTION (Codex 019f5bc3). A dev_session vhost on an
            // INTERNAL listener authenticates with no cookie, so `cms_session_role`
            // (cookie-only) finds nothing and every delegated verb is refused —
            // which silently kills the `*.wake` accelerators on every dev rig, and
            // with them the whole point of an interactive spinner.
            //
            // A wake is not authority: it takes no args, names no target, and can
            // select no work. It only nudges a daemon to drain a queue it claims
            // through its OWN authenticated seam — and the run was already
            // authorised, rate-limited and COMMITTED by webd's admin gate before
            // the wake is even sent. So a dev_session may call an argument-free
            // accelerator wake, and NOTHING else: this branch does not inherit the
            // delegated-safe allowlist, `accelerator_only` re-checks the exact verb
            // + empty args in the handler, and every OTHER verb still requires a
            // real cookie-backed session.
            //
            // Losing a wake can only cost latency, never a run (the durable queue +
            // lease expiry + backstop timer are the correctness mechanism), so
            // failing closed here is always safe.
            let all_accelerators = !matched.bus_verbs.is_empty()
                && matched
                    .bus_verbs
                    .iter()
                    .all(|v| bus_call_handler::is_accelerator_wake_verb(v));
            let dev_email = vhost.dev_session_email.clone();
            if dev_ambient && all_accelerators && dev_email.is_some() {
                if !wake_rate_limit_ok(&vhost.fqdn) {
                    tracing::warn!(
                        target: "webd::bus", route_id = %matched.route_id, method,
                        "bus_call gate: accelerator wake rate-limited; the backstop timer will pick the work up"
                    );
                    return None;
                }
                let actor = dev_email.unwrap_or_default();
                tracing::info!(
                    target: "webd::bus", route_id = %matched.route_id, actor = %actor,
                    vhost = %vhost.fqdn, method,
                    "bus_call gate: dev_session accelerator wake permitted (argument-free, no target)"
                );
                return Some(mix_handler::BusInjection {
                    broker: node.broker_handle.clone(),
                    main_handle: tokio::runtime::Handle::current(),
                    bus_verbs: matched.bus_verbs.clone(),
                    inputs: bus_call_handler::DelegationInputs {
                        actor,
                        vhost: vhost.fqdn.clone(),
                        route_id: matched.route_id.clone(),
                        request_id: session::new_csrf_token(),
                    },
                    service: matched.bus_service.clone(),
                    accelerator_only: true,
                });
            }
            tracing::warn!(
                target: "webd::bus", route_id = %matched.route_id, method,
                "bus_call gate: no valid session; no delegated handler injected"
            );
            return None;
        }
    };
    if rank < 3 {
        tracing::warn!(
            target: "webd::bus", route_id = %matched.route_id, method, rank,
            "bus_call gate: caller is not an admin; no delegated handler injected"
        );
        return None;
    }
    // Verb-aware CSRF (Codex C2 ruling C): an explicit `x-csrf-token` is
    // required only when a MUTATING verb is reachable by a NON-`POST` method (a
    // Lax-cookie GET, or a fetch/JSON method). An SSR plain-form `POST` relies
    // on the SameSite session cookie + this admin gate, like every existing webd
    // SSR admin handler. See `bus_route_requires_csrf`.
    let has_mutating = matched
        .bus_verbs
        .iter()
        .any(|v| !bus_call_handler::is_read_only_bus_verb(v));
    if bus_call_handler::bus_route_requires_csrf(&matched.bus_verbs, method) {
        // A mutating verb reachable by a NON-POST method → explicit token.
        let header_token = headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !session::csrf_eq(&csrf, header_token) {
            tracing::warn!(
                target: "webd::bus", route_id = %matched.route_id, method,
                "bus_call gate: CSRF token mismatch; no delegated handler injected"
            );
            return None;
        }
    } else if method == "POST" && has_mutating {
        // POST + mutating relies on SameSite, BUT `SameSite=Lax` does not block a
        // same-site cross-ORIGIN POST — require a STRICT same-origin Origin/Referer
        // so a sibling `*.vhost` form can't ride the host cookie to mutate tokens.
        if !bus_post_same_origin(headers, &vhost.fqdn) {
            tracing::warn!(
                target: "webd::bus", route_id = %matched.route_id, method,
                "bus_call gate: POST-mutating without a same-origin Origin/Referer; no delegated handler injected"
            );
            return None;
        }
    }
    let request_id = session::new_csrf_token();
    tracing::info!(
        target: "webd::bus", route_id = %matched.route_id, actor = %email,
        vhost = %vhost.fqdn, method, request_id = %request_id,
        "bus_call gate passed; injecting delegated handler"
    );
    Some(mix_handler::BusInjection {
        broker: node.broker_handle.clone(),
        main_handle: tokio::runtime::Handle::current(),
        bus_verbs: matched.bus_verbs.clone(),
        inputs: bus_call_handler::DelegationInputs {
            actor: email,
            vhost: vhost.fqdn.clone(),
            route_id: matched.route_id.clone(),
            request_id,
        },
        service: matched.bus_service.clone(),
        accelerator_only: false,
    })
}

#[cfg(test)]
mod bus_gate_tests {
    use super::*;
    use axum::http::header::{ORIGIN, REFERER};

    #[test]
    fn bus_post_same_origin_is_strict() {
        let fq = "shop.example.org";
        let mk = |h: axum::http::HeaderName, v: &str| {
            let mut m = HeaderMap::new();
            m.insert(h, v.parse().unwrap());
            m
        };
        // Same-origin Origin (default port) / explicit :443 / Referer-fallback pass.
        assert!(bus_post_same_origin(
            &mk(ORIGIN, "https://shop.example.org"),
            fq
        ));
        assert!(bus_post_same_origin(
            &mk(ORIGIN, "https://shop.example.org:443"),
            fq
        ));
        assert!(bus_post_same_origin(
            &mk(REFERER, "https://shop.example.org/admin/vtokens"),
            fq
        ));
        // Same HOST, DIFFERENT port → rejected (cookies are host-scoped).
        assert!(!bus_post_same_origin(
            &mk(ORIGIN, "https://shop.example.org:8443"),
            fq
        ));
        // Sibling same-SITE cross-ORIGIN, foreign, non-https, both-missing → rejected.
        assert!(!bus_post_same_origin(
            &mk(ORIGIN, "https://blog.example.org"),
            fq
        ));
        assert!(!bus_post_same_origin(&mk(ORIGIN, "https://evil.test"), fq));
        assert!(!bus_post_same_origin(
            &mk(ORIGIN, "http://shop.example.org"),
            fq
        ));
        assert!(!bus_post_same_origin(&HeaderMap::new(), fq));
        // Origin is AUTHORITATIVE: a wrong Origin fails even with a right Referer.
        let mut m = HeaderMap::new();
        m.insert(ORIGIN, "https://evil.test".parse().unwrap());
        m.insert(REFERER, "https://shop.example.org/x".parse().unwrap());
        assert!(!bus_post_same_origin(&m, fq));
    }

    #[test]
    fn handler_post_csrf_predicate() {
        use axum::http::Method;
        // Any non-safe method + authenticated → gate applies (bus or not).
        assert!(handler_post_needs_csrf(&Method::POST, true));
        assert!(handler_post_needs_csrf(&Method::PUT, true));
        assert!(handler_post_needs_csrf(&Method::DELETE, true));
        assert!(handler_post_needs_csrf(&Method::PATCH, true));
        // An `ANY`-route method the old enumeration missed is still gated.
        assert!(handler_post_needs_csrf(
            &Method::from_bytes(b"PROPPATCH").unwrap(),
            true
        ));
        // Safe methods are never gated.
        assert!(!handler_post_needs_csrf(&Method::GET, true));
        assert!(!handler_post_needs_csrf(&Method::HEAD, true));
        assert!(!handler_post_needs_csrf(&Method::OPTIONS, true));
        // Genuinely anonymous request: no authority to ride, no CSRF risk.
        assert!(!handler_post_needs_csrf(&Method::POST, false));
    }

    #[test]
    fn handler_origin_is_cross_site_only_flags_present_mismatch() {
        let fq = "my.renta.net";
        let mk = |h: axum::http::HeaderName, v: &str| {
            let mut m = HeaderMap::new();
            m.insert(h, v.parse().unwrap());
            m
        };
        // Same-origin (default port / explicit :443 / Referer) → NOT cross-site.
        assert!(!handler_origin_is_cross_site(
            &mk(ORIGIN, "https://my.renta.net"),
            fq
        ));
        assert!(!handler_origin_is_cross_site(
            &mk(ORIGIN, "https://my.renta.net:443"),
            fq
        ));
        assert!(!handler_origin_is_cross_site(
            &mk(REFERER, "https://my.renta.net/portal/profile"),
            fq
        ));
        // Present + cross-origin/scheme/port → IS cross-site (the real attack).
        assert!(handler_origin_is_cross_site(
            &mk(ORIGIN, "https://evil.renta.net"),
            fq
        )); // sibling subdomain
        assert!(handler_origin_is_cross_site(
            &mk(ORIGIN, "https://evil.test"),
            fq
        ));
        assert!(handler_origin_is_cross_site(
            &mk(ORIGIN, "http://my.renta.net"),
            fq
        )); // non-https
        assert!(handler_origin_is_cross_site(
            &mk(ORIGIN, "https://my.renta.net:8443"),
            fq
        )); // wrong port
        // Origin AUTHORITATIVE: a wrong Origin flags even with a right Referer.
        let mut m = HeaderMap::new();
        m.insert(ORIGIN, "https://evil.test".parse().unwrap());
        m.insert(REFERER, "https://my.renta.net/x".parse().unwrap());
        assert!(handler_origin_is_cross_site(&m, fq));
        // BOTH ABSENT → NOT cross-site (the correction): a header-less POST is a
        // non-browser client (worker / CLI), never a browser CSRF vector. This is
        // what unbreaks the sshm + provisiond workers and the my.renta.net portal.
        assert!(!handler_origin_is_cross_site(&HeaderMap::new(), fq));
        // PRESENT but non-UTF-8 Origin → cross-site (reject), not "absent".
        let mut ng = HeaderMap::new();
        ng.insert(
            ORIGIN,
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(handler_origin_is_cross_site(&ng, fq));
    }
}

async fn list_posts(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(db_mtx) = &vhost.db else {
        let body = serde_json::json!({ "error": "posts not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    // Anonymous callers see only published posts; an admin session sees
    // drafts too. Without this, unpublished content leaks to the public.
    let admin = has_admin_session(&node, &vhost, &headers).await;
    let db = db_mtx.lock().await;
    let posts = tokio::task::block_in_place(|| {
        let sql = if admin {
            "SELECT id, slug, title, content, published, created, updated FROM posts ORDER BY created DESC"
        } else {
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE published = 1 ORDER BY created DESC"
        };
        let mut stmt = db.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(Post {
                id: row.get(0)?,
                slug: row.get(1)?,
                title: row.get(2)?,
                content: row.get(3)?,
                published: row.get::<_, i64>(4)? != 0,
                created: row.get(5)?,
                updated: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Json(posts).into_response())
}

async fn get_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(db_mtx) = &vhost.db else {
        let body = serde_json::json!({ "error": "posts not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    let admin = has_admin_session(&node, &vhost, &headers).await;
    let db = db_mtx.lock().await;
    let result = tokio::task::block_in_place(|| {
        db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )
    });
    match result {
        // A draft (published = false) is invisible to anonymous callers —
        // 404 (not 403), so its existence isn't disclosed.
        Ok(post) if !post.published && !admin => {
            let body = serde_json::json!({ "error": "not found" });
            Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
        Ok(post) => Ok(Json(post).into_response()),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            let body = serde_json::json!({ "error": "not found" });
            Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
        Err(e) => Err(AppError(e.into())),
    }
}

async fn create_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Json(input): Json<CreatePost>,
) -> Result<Response, AppError> {
    if let Some(resp) = require_post_admin(&node, &vhost, &headers).await {
        return Ok(resp);
    }
    let Some(db_mtx) = &vhost.db else {
        let body = serde_json::json!({ "error": "posts not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    let db = db_mtx.lock().await;
    let post = tokio::task::block_in_place(|| -> Result<Post> {
        db.execute(
            "INSERT INTO posts (slug, title, content, published) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                input.slug,
                input.title,
                input.content,
                input.published as i64
            ],
        )?;
        let id = db.last_insert_rowid();
        db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )
        .map_err(Into::into)
    })?;
    Ok((StatusCode::CREATED, Json(post)).into_response())
}

async fn update_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<UpdatePost>,
) -> Result<Response, AppError> {
    if let Some(resp) = require_post_admin(&node, &vhost, &headers).await {
        return Ok(resp);
    }
    let Some(db_mtx) = &vhost.db else {
        let body = serde_json::json!({ "error": "posts not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    let db = db_mtx.lock().await;
    let result = tokio::task::block_in_place(|| -> Result<Option<Post>> {
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref slug) = input.slug {
            sets.push("slug = ?");
            params.push(Box::new(slug.clone()));
        }
        if let Some(ref title) = input.title {
            sets.push("title = ?");
            params.push(Box::new(title.clone()));
        }
        if let Some(ref content) = input.content {
            sets.push("content = ?");
            params.push(Box::new(content.clone()));
        }
        if let Some(published) = input.published {
            sets.push("published = ?");
            params.push(Box::new(published as i64));
        }

        if sets.is_empty() {
            // Nothing to update — just return the existing post
            let post = db
                .query_row(
                    "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(Post {
                            id: row.get(0)?,
                            slug: row.get(1)?,
                            title: row.get(2)?,
                            content: row.get(3)?,
                            published: row.get::<_, i64>(4)? != 0,
                            created: row.get(5)?,
                            updated: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            return Ok(post);
        }

        sets.push("updated = datetime('now')");
        params.push(Box::new(id));

        let sql = format!("UPDATE posts SET {} WHERE id = ?", sets.join(", "));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let changed = db.execute(&sql, param_refs.as_slice())?;

        if changed == 0 {
            return Ok(None);
        }

        let post = db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )?;
        Ok(Some(post))
    })?;

    match result {
        Some(post) => Ok(Json(post).into_response()),
        None => {
            let body = serde_json::json!({ "error": "not found" });
            Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
    }
}

async fn delete_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if let Some(resp) = require_post_admin(&node, &vhost, &headers).await {
        return Ok(resp);
    }
    let Some(db_mtx) = &vhost.db else {
        let body = serde_json::json!({ "error": "posts not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    let db = db_mtx.lock().await;
    let changed =
        tokio::task::block_in_place(|| db.execute("DELETE FROM posts WHERE id = ?1", [id]))?;
    if changed == 0 {
        let body = serde_json::json!({ "error": "not found" });
        Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

// ---------------------------------------------------------------------------
// JMAP reverse proxy
// ---------------------------------------------------------------------------

/// Hard cap on a BUFFERED JMAP JSON request (method calls / session). The
/// blob/eventsource endpoints bypass this — they pass through unbuffered (see
/// [`proxy_jmap`]).
const JMAP_JSON_CAP: usize = 10 * 1024 * 1024;
/// Hard cap on a STREAMED blob upload — bounded but generous (attachments). The
/// body is streamed (never fully held in RAM) but still byte-counted, so a
/// hostile client can't push unbounded bytes through to maild.
const JMAP_BLOB_UPLOAD_CAP: u64 = 64 * 1024 * 1024;

/// RFC 7230 §6.1 hop-by-hop headers — a proxy must not forward them. Chiefly
/// `transfer-encoding`: reqwest de-chunks the upstream and axum re-frames the
/// (streamed) body, so re-emitting the upstream's framing header is wrong.
fn is_hop_by_hop(name: &axum::http::HeaderName) -> bool {
    use axum::http::header;
    name == header::CONNECTION
        || name == header::TRANSFER_ENCODING
        || name == "keep-alive"
        || name == "proxy-authenticate"
        || name == "proxy-authorization"
        || name == header::TE
        || name == header::TRAILER
        || name == header::UPGRADE
}

/// The header names a message nominates as hop-by-hop via its own `Connection`
/// header (RFC 7230 §6.1: those listed in `Connection` must also not be
/// forwarded). Returns the lowercased token set; `HeaderName::as_str()` is
/// always lowercase, so membership checks line up.
fn connection_nominated(headers: &HeaderMap) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for v in headers.get_all(axum::http::header::CONNECTION) {
        if let Ok(s) = v.to_str() {
            for tok in s.split(',') {
                let t = tok.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    set.insert(t);
                }
            }
        }
    }
    set
}

/// Reject a proxied JMAP path that carries dot-segments or percent-encoded
/// separators/dots. `proxy_jmap` classifies the route by raw-path PREFIX but
/// then joins the raw path into an absolute URL the `url` crate NORMALIZES — so
/// without this guard `/jmap/blob/../../x` would be classified as a blob route
/// yet rewritten to a different upstream path (a path-confusion / SSRF-shaped
/// escape). axum does not normalize the request path, so we must.
fn jmap_path_is_safe(path: &str) -> bool {
    if path.contains("%2e") || path.contains("%2E") || path.contains("%2f") || path.contains("%2F")
    {
        return false;
    }
    !path.split('/').any(|seg| seg == "." || seg == "..")
}

async fn jmap_proxy(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    req: axum::extract::Request,
) -> Result<Response, AppError> {
    let Some(upstream_base) = vhost.jmap_upstream.clone() else {
        let body = serde_json::json!({ "error": "jmap not configured for this host" });
        return Ok((StatusCode::NOT_FOUND, Json(body)).into_response());
    };
    proxy_jmap(&node.http_client, &upstream_base, req, JMAP_BLOB_UPLOAD_CAP).await
}

/// Reverse-proxy a JMAP request to the vhost's maild upstream.
///
/// JSON method calls (`POST /jmap`, `/.well-known/jmap`) are **buffered** under
/// [`JMAP_JSON_CAP`] (no evaluator caps apply here, so the byte cap is the only
/// bound). The blob + eventsource endpoints **pass through unbuffered**:
/// - `POST /jmap/upload/{accountId}` — the REQUEST body STREAMS to maild
///   (byte-capped by [`JMAP_BLOB_UPLOAD_CAP`], never fully buffered), so a large
///   attachment doesn't sit in webd's heap.
/// - `GET /jmap/blob/{blobId}` — the RESPONSE body STREAMS to the browser (a
///   large download isn't buffered + isn't JSON-parsed).
/// - `GET /jmap/eventsource` — the RESPONSE STREAMS; it never ends, so the old
///   `resp.bytes()` buffer-the-whole-body path would hang/OOM (the bug this
///   fixes).
///
/// Auth: the client's `Authorization` header is forwarded verbatim; `Host`,
/// `Cookie` (webd's sealed session cookie must never cross to maild), and
/// hop-by-hop headers are dropped. Client-disconnect cancellation is automatic:
/// when the browser drops, axum drops the streamed `Body`, the reqwest stream
/// is dropped, and the upstream connection is cancelled.
async fn proxy_jmap(
    client: &reqwest::Client,
    upstream_base: &str,
    req: axum::extract::Request,
    upload_cap: u64,
) -> Result<Response, AppError> {
    let path = req.uri().path().to_string();
    // Reject dot-segments / encoded separators BEFORE classifying or joining —
    // the URL join normalizes them, which would desync the route classification
    // from the upstream path actually hit.
    if !jmap_path_is_safe(&path) {
        return Ok((StatusCode::BAD_REQUEST, "bad jmap path").into_response());
    }
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let method = req.method().clone();
    let headers = req.headers().clone();

    // Exact endpoint + method classification (maild's routes). A near-miss
    // prefix like `/jmap/uploadX` or `/jmap/eventsourceX` must NOT inherit
    // streamed behaviour — it falls through to the buffered JSON path (and
    // maild 404s it), as it did before this change.
    use axum::http::Method;
    let is_upload = method == Method::POST && path.starts_with("/jmap/upload/");
    let stream_response =
        method == Method::GET && (path.starts_with("/jmap/blob/") || path == "/jmap/eventsource");

    // Headers the request itself nominates hop-by-hop via `Connection`.
    let req_conn_skip = connection_nominated(&headers);

    let upstream_url = format!("{upstream_base}{path}{query}");
    let mut upstream_req = client.request(method, &upstream_url);
    for (name, value) in &headers {
        if name == axum::http::header::HOST
            || name == axum::http::header::COOKIE
            || is_hop_by_hop(name)
            || req_conn_skip.contains(name.as_str())
        {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }

    // REQUEST body: stream a blob upload (byte-capped); buffer + cap everything
    // else (JSON method calls / session).
    if is_upload {
        let mut total: u64 = 0;
        let capped = req.into_body().into_data_stream().map(move |res| {
            let chunk = res.map_err(|e| std::io::Error::other(format!("jmap upload read: {e}")))?;
            total += chunk.len() as u64;
            if total > upload_cap {
                return Err(std::io::Error::other(format!(
                    "jmap upload exceeds {upload_cap} bytes"
                )));
            }
            Ok::<_, std::io::Error>(chunk)
        });
        upstream_req = upstream_req.body(reqwest::Body::wrap_stream(capped));
    } else {
        let body = axum::body::to_bytes(req.into_body(), JMAP_JSON_CAP).await?;
        upstream_req = upstream_req.body(body);
    }

    let resp = upstream_req.send().await?;
    let status = StatusCode::from_u16(resp.status().as_u16())?;
    let resp_headers = resp.headers().clone();
    let resp_conn_skip = connection_nominated(&resp_headers);

    // RESPONSE body: stream blob download + eventsource; buffer JSON.
    let mut response = if stream_response {
        (status, Body::from_stream(resp.bytes_stream())).into_response()
    } else {
        (status, resp.bytes().await?).into_response()
    };
    for (name, value) in &resp_headers {
        if is_hop_by_hop(name) || resp_conn_skip.contains(name.as_str()) {
            continue;
        }
        // A streamed body is re-framed by axum/hyper; copying the upstream
        // `Content-Length` onto an unknown-length stream risks a mismatch (a
        // mid-stream upstream error would then truncate against a fixed CL,
        // hanging the client). Let the streamed path frame itself (chunked).
        if stream_response && name == axum::http::header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().insert(name, value.clone());
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// WebSocket proxy to broker (for WASM apps)
// ---------------------------------------------------------------------------

async fn ws_proxy_handler(
    ws: WebSocketUpgrade,
    Extension(vhost): Extension<Arc<VhostState>>,
) -> Response {
    let Some(noded_url) = vhost.noded_ws.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.on_upgrade(move |browser_ws| ws_proxy(browser_ws, noded_url))
        .into_response()
}

async fn ws_proxy(browser_ws: WebSocket, noded_url: String) {
    // Connect to the upstream broker
    let noded_conn = match tokio_tungstenite::connect_async(&noded_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to connect to broker for WS proxy");
            return;
        }
    };

    let (mut browser_sink, mut browser_stream) = browser_ws.split();
    let (mut noded_sink, mut noded_stream) = noded_conn.split();

    // Browser → Broker
    let browser_to_noded = async {
        while let Some(Ok(msg)) = browser_stream.next().await {
            let tung_msg = match msg {
                AxumMessage::Text(t) => TungMessage::Text(t.to_string().into()),
                AxumMessage::Binary(b) => TungMessage::Binary(b),
                AxumMessage::Close(_) => break,
                AxumMessage::Ping(p) => TungMessage::Ping(p),
                AxumMessage::Pong(p) => TungMessage::Pong(p),
            };
            if noded_sink.send(tung_msg).await.is_err() {
                break;
            }
        }
    };

    // Broker → Browser
    let noded_to_browser = async {
        while let Some(Ok(msg)) = noded_stream.next().await {
            let axum_msg = match msg {
                TungMessage::Text(t) => AxumMessage::Text(t.to_string().into()),
                TungMessage::Binary(b) => AxumMessage::Binary(b),
                TungMessage::Close(_) => break,
                TungMessage::Ping(p) => AxumMessage::Ping(p),
                TungMessage::Pong(p) => AxumMessage::Pong(p),
                _ => continue,
            };
            if browser_sink.send(axum_msg).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = browser_to_noded => {}
        _ = noded_to_browser => {}
    }
}

// ---------------------------------------------------------------------------
// Markdown docs handler
// ---------------------------------------------------------------------------

/// Build a sidebar navigation from the docs directory structure.
fn build_sidebar(docs_dir: &StdPath, current_path: &str) -> String {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(docs_dir) {
        let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        dirs.sort_by_key(|e| e.file_name());

        for entry in &dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if path.is_dir() && !name.starts_with('.') {
                // Section title: strip numeric prefix "00-getting-started" -> "Getting Started"
                let title = name
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
                    .replace(['-', '_'], " ");
                let title = title
                    .split_whitespace()
                    .map(|w| {
                        let mut c = w.chars();
                        match c.next() {
                            None => String::new(),
                            Some(f) => f.to_uppercase().to_string() + c.as_str(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                let mut pages = Vec::new();
                if let Ok(files) = std::fs::read_dir(&path) {
                    let mut files: Vec<_> = files.filter_map(|e| e.ok()).collect();
                    files.sort_by_key(|e| e.file_name());
                    for file in &files {
                        let fname = file.file_name().to_string_lossy().to_string();
                        if fname.ends_with(".md") {
                            let slug = fname.trim_end_matches(".md");
                            let href = format!("/docs/{name}/{slug}");
                            let page_title = if slug == "index" {
                                "Overview".to_string()
                            } else {
                                slug.replace(['-', '_'], " ")
                            };
                            pages.push((href, page_title));
                        }
                    }
                }
                if !pages.is_empty() {
                    sections.push((title, pages));
                }
            }
        }
    }

    let mut html = String::from("<nav class=\"sidebar\">\n<h2><a href=\"/docs\">Docs</a></h2>\n");
    for (title, pages) in &sections {
        html.push_str(&format!(
            "<details{}>\n<summary>{title}</summary>\n<ul>\n",
            if pages.iter().any(|(href, _)| href.trim_end_matches("/index")
                == format!("/docs/{}", current_path.split('/').next().unwrap_or("")))
            {
                " open"
            } else {
                ""
            }
        ));
        for (href, page_title) in pages {
            let active = if current_path == href.trim_start_matches("/docs/") {
                " class=\"active\""
            } else {
                ""
            };
            html.push_str(&format!(
                "<li{active}><a href=\"{href}\">{page_title}</a></li>\n"
            ));
        }
        html.push_str("</ul>\n</details>\n");
    }
    html.push_str("</nav>\n");
    html
}

/// Resolve a docs path to its raw markdown content.
fn resolve_markdown_path(docs_dir: &StdPath, rel_path: &str) -> Option<String> {
    let candidates = [
        docs_dir.join(format!("{rel_path}.md")),
        docs_dir.join(rel_path).join("index.md"),
        docs_dir.join(rel_path),
    ];

    let file_path = candidates.iter().find(|p| p.is_file())?;

    // Security: ensure resolved path is under docs_dir
    let canonical = file_path.canonicalize().ok()?;
    let docs_canonical = docs_dir.canonicalize().ok()?;
    if !canonical.starts_with(&docs_canonical) {
        return None;
    }

    std::fs::read_to_string(&canonical).ok()
}

/// Render a markdown file to a full HTML page.
fn render_markdown(docs_dir: &StdPath, rel_path: &str) -> Option<String> {
    let content = resolve_markdown_path(docs_dir, rel_path)?;

    // Parse markdown
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = MdParser::new_ext(&content, opts);
    let mut body_html = String::new();
    pulldown_cmark::html::push_html(&mut body_html, parser);

    // Convert <img> tags pointing to .mp4/.webm to <video> tags
    let video_re =
        regex_lite::Regex::new(r#"<img src="([^"]+\.(?:mp4|webm|mov))" alt="([^"]*)"(?: /)?>"#)
            .unwrap();
    body_html = video_re.replace_all(&body_html, |caps: &regex_lite::Captures| {
        let src = &caps[1];
        let alt = &caps[2];
        format!(r#"<video src="{src}" alt="{alt}" controls muted autoplay loop style="max-width:100%;border-radius:0.5rem;margin:1rem 0"></video>"#)
    }).to_string();

    // Extract title from first <h1>
    let title = content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches("# "))
        .unwrap_or("Docs");

    let sidebar = build_sidebar(docs_dir, rel_path);

    Some(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{
    --bg: #1a1a2e; --fg: #e0e0e0; --sidebar-bg: #16213e; --accent: #0f3460;
    --link: #6cb4ee; --code-bg: #0d1117; --border: #2a2a4a;
    --active-bg: #0f3460; --hover-bg: #1a1a3e;
  }}
  @media (prefers-color-scheme: light) {{
    :root {{
      --bg: #fff; --fg: #1a1a1a; --sidebar-bg: #f5f5f5; --accent: #e8e8e8;
      --link: #0366d6; --code-bg: #f6f8fa; --border: #d0d0d0;
      --active-bg: #e2e8f0; --hover-bg: #edf2f7;
    }}
  }}
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg); color: var(--fg); line-height: 1.6;
    display: flex; min-height: 100vh;
  }}
  .sidebar {{
    width: 16rem; min-height: 100vh; padding: 1.5rem 1rem;
    background: var(--sidebar-bg); border-right: 1px solid var(--border);
    overflow-y: auto; flex-shrink: 0; position: sticky; top: 0;
    max-height: 100vh;
  }}
  .sidebar h2 {{ margin-bottom: 1rem; font-size: 1.25rem; }}
  .sidebar h2 a {{ color: var(--fg); text-decoration: none; }}
  .sidebar details {{ margin-bottom: 0.25rem; }}
  .sidebar summary {{
    cursor: pointer; padding: 0.3rem 0.5rem; font-weight: 600;
    font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.05em;
    color: var(--fg); opacity: 0.7;
  }}
  .sidebar ul {{ list-style: none; padding-left: 0.5rem; }}
  .sidebar li {{ margin: 0.1rem 0; }}
  .sidebar li a {{
    display: block; padding: 0.2rem 0.5rem; color: var(--link);
    text-decoration: none; font-size: 0.85rem; border-radius: 0.25rem;
  }}
  .sidebar li a:hover {{ background: var(--hover-bg); }}
  .sidebar li.active a {{ background: var(--active-bg); font-weight: 600; }}
  .content {{
    flex: 1; max-width: 52rem; padding: 2rem 3rem; min-width: 0;
  }}
  .content h1 {{ font-size: 2rem; margin-bottom: 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }}
  .content h2 {{ font-size: 1.5rem; margin: 2rem 0 0.75rem; }}
  .content h3 {{ font-size: 1.2rem; margin: 1.5rem 0 0.5rem; }}
  .content p {{ margin: 0.75rem 0; }}
  .content a {{ color: var(--link); }}
  .content img {{ max-width: 100%; border-radius: 0.5rem; margin: 1rem 0; }}
  .content ul, .content ol {{ margin: 0.75rem 0; padding-left: 1.5rem; }}
  .content li {{ margin: 0.25rem 0; }}
  .content table {{ border-collapse: collapse; width: 100%; margin: 1rem 0; }}
  .content th, .content td {{ border: 1px solid var(--border); padding: 0.5rem 0.75rem; text-align: left; }}
  .content th {{ background: var(--sidebar-bg); }}
  .content blockquote {{
    border-left: 3px solid var(--link); padding: 0.5rem 1rem;
    margin: 1rem 0; background: var(--code-bg); border-radius: 0 0.25rem 0.25rem 0;
  }}
  .content pre {{
    background: var(--code-bg); padding: 1rem; border-radius: 0.5rem;
    overflow-x: auto; margin: 1rem 0; border: 1px solid var(--border);
    font-size: 0.875rem; line-height: 1.5;
  }}
  .content code {{
    font-family: "JetBrains Mono", "Fira Code", "Cascadia Code", monospace;
    font-size: 0.875em;
  }}
  .content :not(pre) > code {{
    background: var(--code-bg); padding: 0.15rem 0.35rem; border-radius: 0.25rem;
  }}
  @media (max-width: 768px) {{
    body {{ flex-direction: column; }}
    .sidebar {{ width: 100%; max-height: none; position: static; border-right: none; border-bottom: 1px solid var(--border); }}
    .content {{ padding: 1.5rem; }}
  }}
</style>
</head>
<body>
{sidebar}
<main class="content">
{body_html}
</main>
</body>
</html>"#
    ))
}

async fn serve_docs(
    Extension(vhost): Extension<Arc<VhostState>>,
    Path(path): Path<String>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let docs_dir = match &vhost.docs_dir {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "docs not configured").into_response(),
    };

    let path = path.trim_end_matches('/');

    // ?format=md returns raw markdown
    if query.get("format").map(|v| v.as_str()) == Some("md") {
        return match resolve_markdown_path(docs_dir, path) {
            Some(content) => (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/markdown; charset=utf-8",
                )],
                content,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "page not found").into_response(),
        };
    }

    match render_markdown(docs_dir, path) {
        Some(html) => Html(html).into_response(),
        None => (StatusCode::NOT_FOUND, "page not found").into_response(),
    }
}

async fn serve_docs_index(Extension(vhost): Extension<Arc<VhostState>>) -> Response {
    let docs_dir = match &vhost.docs_dir {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "docs not configured").into_response(),
    };

    match render_markdown(docs_dir, "index") {
        Some(html) => Html(html).into_response(),
        None => {
            // No index.md — generate a directory listing
            let sidebar = build_sidebar(docs_dir, "");
            let html = format!(
                r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Docs</title></head>
<body style="display:flex">{sidebar}<main style="padding:2rem"><h1>Documentation</h1>
<p>Select a section from the sidebar.</p></main></body></html>"#
            );
            Html(html).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Mail-client autoconfig / autodiscover (NS 3.0 model, Phase 1)
//
// Ports the production NS 3.0 `autodiscover.php` (Appendix A of
// `_doc/planned/maild-autoconfig.md`) to a native axum handler. The PHP
// is safe only behind its nginx vhost envelope; this standalone handler
// adds the controls that envelope provided: a served-domain allowlist
// gate *before* any DNS, and XML-escaping of every interpolated value.
// ---------------------------------------------------------------------------

/// Escape the five XML metacharacters for both element-text and
/// attribute-value contexts. Defense-in-depth: `domain`/`mhost` are
/// already LDH-validated by `mxresolve::normalize_host`, but the
/// allowlist + escaping pair is the mandated security improvement over
/// the raw-interpolating PHP reference (maild-autoconfig.md §Security).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Derive the served mail domain from the `Host` header per the NS 3.0
/// rule, returning the validated, allowlisted, lowercased domain.
///
/// `Host` is attacker-controlled (`feedback_bus_wire_trust_boundary`).
/// **No DNS is performed here** — the allowlist gate runs first so a
/// hostile `Host` can never drive an outbound lookup. Every failure
/// path collapses to the same `None` ⇒ a uniform `404`, so the
/// endpoint cannot be used to probe which domains exist.
fn served_domain_from_host(headers: &HeaderMap, allow: &HashSet<String>) -> Option<String> {
    let raw = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    // Strict Host parse (lifted to `cosmix_daemon::http_host` in
    // webd-vhosts Phase 1 commit 1). Failures collapse to `None` so
    // the existing allowlist gate below produces the canonical `404`.
    let host = cosmix_daemon::http_host::parse_request_host(raw)?;
    // Strip a single leading `autoconfig.` / `autodiscover.` label,
    // matching the NS 3.0 `str_replace(['autoconfig.','autodiscover.'])`
    // intent. Deliberately **not** `mail.` (the reference does not
    // strip it; maild-autoconfig.md §webd-changes).
    let domain = host
        .strip_prefix("autoconfig.")
        .or_else(|| host.strip_prefix("autodiscover."))
        .unwrap_or(&host);
    // Syntactic validation (LDH, ≥2 labels, ≤253) reusing the
    // resolver's own rule so a Host can never become a malformed
    // `<hostname>` or query.
    let domain = mxresolve::normalize_host(domain)?;
    // Allowlist gate — BEFORE any DNS (the SSRF / DNS-amplification
    // close; maild-autoconfig.md §Security).
    allow.contains(&domain).then_some(domain)
}

/// Render the NS 3.0 `clientConfig` v1.1 body, Cosmix-rebranded
/// (`displayName`/`displayShortName`/`<documentation>`) and with every
/// interpolated value XML-escaped. Structure and the
/// port/socket/auth/`%EMAILADDRESS%` constants are verbatim from
/// Appendix A `autoconfig()`; no trailing newline, matching the PHP
/// heredoc.
fn render_autoconfig_xml(domain: &str, mhost: &str) -> String {
    let d = xml_escape(domain);
    let m = xml_escape(mhost);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="{d}">
    <domain>{d}</domain>
    <displayName>Cosmix Mail</displayName>
    <displayShortName>Cosmix</displayShortName>
    <incomingServer type="imap">
      <hostname>{m}</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <authentication>password-cleartext</authentication>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>{m}</hostname>
      <port>465</port>
      <socketType>SSL</socketType>
      <authentication>password-cleartext</authentication>
      <username>%EMAILADDRESS%</username>
    </outgoingServer>
    <documentation url="https://{d}/docs/">
      <descr lang="en">Cosmix Mail client setup</descr>
    </documentation>
  </emailProvider>
</clientConfig>"#
    )
}

/// `GET /mail/config-v1.1.xml` and its `.well-known` alias — the
/// Mozilla (Thunderbird) autoconfig endpoint. The `?emailaddress=`
/// query param is **deliberately ignored**: the response is
/// domain-level and byte-identical regardless of whether any mailbox
/// exists (no account enumeration; maild-autoconfig.md §Security).
async fn mozilla_autoconfig(State(node): State<Arc<NodeState>>, headers: HeaderMap) -> Response {
    let domain = match served_domain_from_host(&headers, &node.served_mail_domains) {
        Some(d) => d,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    // Unreachable when the allowlist is non-empty (then `mx` is always
    // `Some`); kept as a defensive invariant rather than an unwrap.
    let resolver = match &node.mx {
        Some(r) => r,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    // Advertise + probe the node's configured internal mail host (WG-reachable
    // 993/465) when set, else resolve + probe the domain's public MX. Both
    // paths run the same IMAPS/SMTPS+cert operator-policy probe.
    let mhost_result = match &node.autoconfig_mail_host {
        Some(host) => resolver.resolve_fixed(host).await,
        None => resolver.resolve(&domain).await,
    };
    let mhost = match mhost_result {
        Ok(h) => h,
        // Definitive "no MX" → 404. Transient resolver failure and a
        // failed operator-policy probe (MX target does not terminate
        // IMAPS:993 + SMTPS:465 with a valid cert) → 503: never a
        // partial, guessed, or unverified `<hostname>` body that would
        // silently fail in the client.
        Err(mxresolve::MxError::NoMx) => return StatusCode::NOT_FOUND.into_response(),
        Err(mxresolve::MxError::Transient | mxresolve::MxError::ProbeFailed) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/xml; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
        ],
        render_autoconfig_xml(&domain, &mhost),
    )
        .into_response()
}

/// `POST /autodiscover/autodiscover.xml` — route reserved in Phase 1,
/// body deferred to Phase 2 (maild-autoconfig.md §Phase 2). A probing
/// Outlook client must see a clean `404` so it falls through its own
/// discovery chain rather than parsing a half-built envelope.
async fn outlook_autodiscover_reserved() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// The three NS 3.0 autoconfig routes, shared by the TLS (`:443`) app
/// and the plain-HTTP (`:80`) listener — plain-HTTP autoconfig is not
/// optional, the `autoconfig.<domain>` host is frequently
/// cert-uncovered (maild-autoconfig.md §Constraints, wildcard depth).
fn autoconfig_routes() -> Router<Arc<NodeState>> {
    Router::new()
        .route(
            "/mail/config-v1.1.xml",
            axum::routing::get(mozilla_autoconfig),
        )
        .route(
            "/.well-known/autoconfig/mail/config-v1.1.xml",
            axum::routing::get(mozilla_autoconfig),
        )
        .route(
            "/autodiscover/autodiscover.xml",
            axum::routing::post(outlook_autodiscover_reserved),
        )
}

// ---------------------------------------------------------------------------
// Host routing
// ---------------------------------------------------------------------------

/// Dispatch by Host. Reads the request's `Host` header through the
/// shared `parse_request_host`, looks up the vhost in
/// `NodeState::vhosts`, swaps the matched `Arc<VhostState>` into
/// request extensions, and forwards to the per-vhost router.
///
/// Unknown Host → `404` with no body (no enumeration side-channel).
/// Missing/malformed Host → `400`, mirroring `redirect_to_https`.
///
/// When a [`ListenerScope`] extension is present (every per-interface
/// listener in `main()` injects one), a Host that resolves to a vhost
/// *not on this listener's allowlist* is also `404`ed — the HTTP-layer
/// half of the per-interface isolation (the socket bind is the
/// kernel-level half). `scope` is `Option` so the bootstrap :80
/// redirect router and the middleware tests — which don't layer a
/// scope — behave as the single-listener all-hosts case (no extra
/// restriction).
async fn host_router(
    State(node): State<Arc<NodeState>>,
    scope: Option<Extension<ListenerScope>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let raw = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let Some(host) = cosmix_daemon::http_host::parse_request_host(raw) else {
        return (StatusCode::BAD_REQUEST, "missing or invalid Host header").into_response();
    };
    // Per-listener allowlist: a vhost served only on another interface
    // must not answer here even though the node knows the Host. Same
    // 404 as an unknown Host — no cross-listener enumeration channel.
    if let Some(Extension(scope)) = &scope
        && !scope.allows(&host)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(vhost) = node.vhost_for_host(&host) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    req.extensions_mut().insert(vhost);
    next.run(req).await
}

/// Stats-recording middleware for the per-vhost router. Pulls the
/// `Arc<VhostState>` that `host_router` injected, lets the request
/// through, and increments the per-vhost response-class bucket on the
/// way back out. Layered *inside* `host_router` (closer to the
/// handler) so the `Extension` is guaranteed populated before this
/// runs. Autoconfig and the redirect router both sit sideways from
/// the per-vhost branch and are intentionally not counted — they
/// have no vhost binding.
async fn record_response_stats(
    Extension(vhost): Extension<Arc<VhostState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // A non-safe method against this vhost may have mutated content — drop the
    // public-response cache so anonymous readers can't be served pre-write
    // bytes. Computed BEFORE `req` is moved; invalidation runs regardless of
    // the response status (a handler can commit a DB write and then fail while
    // rendering its response). Safe methods (GET/HEAD/OPTIONS) never invalidate.
    let mutates_public_cache = !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let resp = next.run(req).await;
    vhost.stats.record(resp.status().as_u16());
    if mutates_public_cache {
        vhost.public_response_cache.invalidate().await;
    }
    resp
}

/// Plain-HTTP (:80) admission gate. Same Host parse + allowlist
/// shape as `host_router`, but rejects with `400` rather than `404`
/// so an attacker-Host on plain HTTP never produces a `301 →
/// https://<attacker-host>/` redirect. The allowed set is the
/// `admit_plain_http` view of the current
/// [`vhost_directory::VhostDirectory`] snapshot — the same source of
/// truth the HTTPS dispatcher reads, so the two listeners stay in
/// lock-step across `ArcSwap` publishes.
///
/// Pre-C3b this took a `State<Arc<HashSet<String>>>` snapshotted once
/// at router-build time, which silently rejected any vhost added
/// after startup (rev-2 BLOCKER fix in
/// `_doc/planned/webd-vhosts-phase3.md`). The middleware now lifts
/// the admit set off `NodeState::vhosts` per request — `load()` is
/// wait-free.
async fn plain_http_host_admit(
    State(node): State<Arc<NodeState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let raw = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let Some(host) = cosmix_daemon::http_host::parse_request_host(raw) else {
        return (StatusCode::BAD_REQUEST, "missing or invalid Host header").into_response();
    };
    if !node.vhosts.load().admit_plain_http.contains(&host) {
        return (StatusCode::BAD_REQUEST, "unknown host").into_response();
    }
    next.run(req).await
}

/// Per-vhost static-file fallback. The per-vhost router cannot bake
/// `www_dir` into a `fallback_service(ServeDir::new(...))` at build
/// time because each vhost has its own document root, so this
/// handler constructs a `ServeDir` per request from the
/// `Arc<VhostState>` extension that `host_router` injected.
async fn serve_static(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    // The per-listener scope (layered by the listener wiring in explicit-
    // listener mode). Absent on a single-listener back-compat node → the
    // dev auto-session below fails closed (treated as external).
    scope: Option<Extension<ListenerScope>>,
    mut req: axum::extract::Request,
) -> Response {
    // Slice #3 — consult the `webd.handlers` route table before
    // static-file serving. The hard-coded routes (`/api/posts`, `/jmap`,
    // …) already won via axum's matcher; `webd.handlers` governs the
    // static space. Bind the lookup result before branching so the
    // `ArcSwap` load guard isn't held across the handler's `.await`.
    let matched = node
        .handlers
        .load()
        .lookup(&vhost.fqdn, req.method().as_str(), req.uri().path());
    if let Some(m) = matched {
        // Handler scripts resolve against the handler-root (NS 4.0:
        // `…/web/app`, one level above the `…/web/app/public` docroot),
        // NOT the docroot — so handler source is not under the `ServeDir`
        // static root below and can't be fetched raw via any request path
        // (symlinks from public/ aside; see derive_handler_root).
        let handler_root = mix_handler::derive_handler_root(&vhost.www_dir);
        // CSRF defence for state-changing SSR handler requests (Codex D8 #1,
        // cosmix-cloud oneshot WS5; both-absent corrected 2026-07-13 after a live
        // fleet regression). `SameSite=Lax` on the session cookie does NOT block a
        // same-SITE cross-ORIGIN POST — a sibling `*.registrable` vhost (or any
        // co-hosted subdomain) can auto-submit a form to this vhost and ride the
        // victim's authority to mutate mail folders, send mail, save notes, etc.
        // Only `/auth/*`, `/api/posts`, and the delegated `bus_call` gate checked
        // a token before; every OTHER mutating handler request relied on Lax alone.
        //
        // Reject iff: an AUTHENTICATED mutating request carries a PRESENT,
        // cross-origin `Origin`/`Referer` (`handler_origin_is_cross_site`). We do
        // NOT reject on BOTH headers absent: under Lax, the attack is always a
        // same-site cross-origin browser POST, which ALWAYS emits `Origin`, so a
        // header-less POST is a non-browser client (an `X-*-Secret`-authed worker,
        // an operator CLI) — not a CSRF vector, and rejecting it broke the sshm +
        // provisiond workers live on 2026-07-13. See `handler_origin_is_cross_site`.
        //
        // "Authenticated" = a session cookie OR an ambient internal `dev_session`
        // (grants authority WITHOUT a cookie, so a browser cross-origin POST to a
        // dev vhost must still be caught — and it will be, because it carries an
        // Origin). Bus routes are gated here too: their own gate only withholds
        // `bus_call`, but the handler still runs and can commit a `db`/`jmap`
        // mutation. Safe methods (GET/HEAD/OPTIONS) and anonymous requests are
        // never gated.
        {
            let has_cookie =
                session::cookie_value(req.headers(), session::SESSION_COOKIE).is_some();
            // Ambient internal dev_session: an internal (non-external) listener
            // + a vhost declaring dev_session_* authenticates with no cookie
            // (mirrors the `on_internal_listener` gate below).
            let dev_ambient = vhost.dev_session_email.is_some()
                && scope
                    .as_ref()
                    .map(|Extension(s)| !s.external)
                    .unwrap_or(false);
            if handler_post_needs_csrf(req.method(), has_cookie || dev_ambient)
                && handler_origin_is_cross_site(req.headers(), &vhost.fqdn)
            {
                tracing::warn!(
                    target: "webd::csrf", route_id = %m.route_id,
                    method = %req.method(),
                    "handler CSRF gate: authenticated mutating request with a \
                     cross-origin Origin/Referer — rejected"
                );
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        // Public-response cache (opt-in `public_cache` routes): a GET from a
        // fully ANONYMOUS visitor to a cache-eligible route may be served from
        // (or stored into) the per-vhost response cache — one render, N
        // replays. The gate is deliberately strict: any session cookie, any
        // Authorization, a dev_session vhost, or delegated-Bus grants make the
        // request non-anonymous or side-effecting and disqualify it. The route
        // capability is GET-only + bus-free by construction (`from_rows`).
        let public_cache_candidate = m.wants_public_cache
            && req.method() == axum::http::Method::GET
            && vhost.dev_session_email.is_none()
            && session::cookie_value(req.headers(), session::SESSION_COOKIE).is_none()
            && !req
                .headers()
                .contains_key(axum::http::header::AUTHORIZATION)
            && m.bus_verbs.is_empty();
        let mut pcache_slot: Option<
            tokio::sync::OwnedMutexGuard<Option<public_response_cache::Entry>>,
        > = None;
        let mut pcache_generation = 0u64;
        if public_cache_candidate {
            // Normalise the Host ONCE — it is both the cache key's host and the
            // only request header the render is allowed to see (below).
            let host = req
                .headers()
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|h| h.trim().to_ascii_lowercase())
                .unwrap_or_else(|| vhost.fqdn.clone());
            let key = public_response_cache::Key {
                host: host.clone(),
                path_and_query: req
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_else(|| req.uri().path().to_string()),
            };
            // `None` ⇒ the per-vhost cache is full of live entries and this is
            // a new key → serve UNCACHED (render normally below, no strip, no
            // store). Bounds an anonymous key-enumeration fill.
            if let Some((generation, guard)) = vhost.public_response_cache.lock_slot(key).await {
                if let Some(hit) = vhost.public_response_cache.hit(&guard) {
                    return hit;
                }
                pcache_generation = generation;
                pcache_slot = Some(guard);
                // ENFORCE the (host, path, query)-pure contract mechanically: a
                // cacheable render sees ONLY the normalised Host. Every other
                // request header — Cookie, Authorization, and crucially
                // attacker-influenceable ones a handler might reflect
                // (`X-Forwarded-Host`, `Accept-Language`, `User-Agent`,
                // forwarded identity) — is dropped, so the render cannot vary on
                // (or be poisoned via) an UNKEYED header, and the Host the
                // handler sees matches the key exactly. The route's
                // method/path/query come from the URI, not headers, so nothing
                // legitimate is lost.
                let mut clean = axum::http::HeaderMap::new();
                if let Ok(hv) = axum::http::HeaderValue::from_str(&host) {
                    clean.insert(axum::http::header::HOST, hv);
                }
                *req.headers_mut() = clean;
                // A GET body is legal HTTP and Mix exposes it as `$BODY` (and
                // derives `$SIGNALS` from it) — it is NOT part of the cache key,
                // so canonicalise it to empty too, or an attacker could poison
                // the cached bytes via a bodied GET reflected into the render.
                *req.body_mut() = axum::body::Body::empty();
            }
        }
        // Build the capability handles the matched route asked for. Each
        // `wants_*` flag widens the sandbox; the handle supplies the
        // authority. An absent handle (vhost has no CMS db / no
        // `jmap_upstream`) still grants the capability but the builtin
        // then errors cleanly (→ 500) — never a silent fall-through.
        //
        // `db`: the per-vhost connection, scoped to this tenant.
        let db = if m.wants_db { vhost.db.clone() } else { None };
        // `jmap`: the vhost's maild upstream + the bearer token unsealed
        // from the session cookie (Phase 2). The SSR `jmap()` seam
        // authorises from the sealed `cosmix_session` cookie ONLY — never
        // a forwarded `Authorization` header (the `/jmap` reverse-proxy
        // keeps that client-side path; a same-site browser POST must not
        // gain maild authority just by riding the cookie). A page
        // navigation carries no `Authorization`; the cookie is the
        // authority. Absent / invalid / expired / vhost-mismatched cookie
        // → `auth = None` → maild 401 → `jmap()` errors, which the handler
        // renders as a login redirect. The HTTP client is built inside the
        // handler's own runtime (see `run_with_limits`), not shared from
        // `node.http_client`.
        // Unseal the session cookie ONCE: it yields both the user identity
        // (→ `$SESSION` for every handler) and the maild bearer token (→ the
        // jmap() auth, jmap-capable routes only). Absent/invalid/expired/
        // vhost-mismatched → None (handler sees `$SESSION.authenticated=false`).
        let mut session_payload = session::cookie_value(req.headers(), session::SESSION_COOKIE)
            .and_then(|c| node.session.unseal(&c, &vhost.fqdn, session::now_secs()));
        // Cookie-path revocation (2026-07 audit): the Mix `$SESSION` identity
        // is cookie authority too, so an epoch-stale payload must vanish HERE
        // as well, not only in `cms_session_role` — otherwise a revoked
        // cookie would keep an authenticated `$SESSION` (and its sealed
        // maild bearer) alive on every Mix-rendered page.
        if let Some(p) = session_payload.as_ref()
            && p.epoch != current_session_epoch(&vhost, &p.email).await
        {
            session_payload = None;
        }
        let mut session_email = session_payload.as_ref().map(|p| p.email.clone());
        // Identity kind for $SESSION (→ Mix current_user caps a "customer"
        // session at rank-1 regardless of the users table). Derived from the
        // real cookie payload; a dev_session (below) is always a maild identity.
        let session_kind = session_payload.as_ref().map(|p| p.kind.clone());
        // DEV-ONLY auto-session (sealed WG dev boxes). When this vhost declares a
        // `dev_session` identity AND no real cookie session is present AND this
        // request arrived on a NON-external listener, synthesise that identity:
        // `$SESSION.authenticated` becomes true and `jmap()` authenticates to
        // maild via HTTP Basic. The internal-listener check is the HARD gate — an
        // absent `ListenerScope` (single-listener back-compat node) is treated as
        // external, so the bypass fails closed. A real cookie session always wins
        // (we only fill in when `session_email` is None). Startup validation in
        // `resolve_node_state` independently refuses a dev_session vhost that is
        // bound to any external listener, so this can never fire publicly.
        let on_internal_listener = scope
            .as_ref()
            .map(|Extension(s)| !s.external)
            .unwrap_or(false);
        let mut dev_jmap_auth: Option<String> = None;
        if session_email.is_none()
            && on_internal_listener
            && let (Some(email), Some(pw)) = (
                vhost.dev_session_email.as_ref(),
                vhost.dev_session_password.as_ref(),
            )
        {
            session_email = Some(email.clone());
            // PERF: mint+cache a maild Bearer ONCE instead of HTTP Basic on
            // every jmap() call (Basic → maild bcrypt cost-12 ~480ms PER
            // call; Bearer verifies in sub-ms). Only when this route wants
            // jmap and the vhost has an upstream to mint against; on a mint
            // failure `dev_jmap_auth` stays None (→ 401 → login, as before).
            if m.wants_jmap
                && let Some(upstream) = vhost.jmap_upstream.as_deref()
            {
                dev_jmap_auth = service_jmap_bearer_auth(
                    &node,
                    ServiceJmapTokenKey {
                        fqdn: vhost.fqdn.clone(),
                        upstream: upstream.trim_end_matches('/').to_string(),
                        email: email.clone(),
                        purpose: ServiceJmapTokenPurpose::DevSession,
                    },
                    pw,
                )
                .await;
            }
        }
        // PUBLIC read-only content credential. When this vhost declares
        // `public_read_*` AND no session cookie AND no dev_session applied,
        // supply it as the `jmap()` Basic credential for ANONYMOUS reads. UNLIKE
        // dev_session it is NOT listener-gated (this is the public `:443` path)
        // and — critically — it grants NO session: `session_email` stays None, so
        // `$SESSION.authenticated` is false and `require_role("admin")` still
        // denies (no manage, no mutation). It authenticates as a dedicated
        // content account holding only public content, so the read is safe by
        // construction; the handler's redaction allowlist + live predicate are
        // defence in depth on top of the account boundary.
        let mut public_read_jmap_auth: Option<String> = None;
        if m.wants_jmap
            && m.wants_public_read
            && session_email.is_none()
            && dev_jmap_auth.is_none()
            && let Some(upstream) = vhost.jmap_upstream.as_deref()
            && let (Some(email), Some(pw)) = (
                vhost.public_read_email.as_ref(),
                vhost.public_read_password.as_ref(),
            )
        {
            // PERF: Bearer-once instead of per-call Basic (see dev_session
            // above). public_read is a PRODUCTION-facing anonymous surface,
            // so this removes the bcrypt cost from real public reads too.
            public_read_jmap_auth = service_jmap_bearer_auth(
                &node,
                ServiceJmapTokenKey {
                    fqdn: vhost.fqdn.clone(),
                    upstream: upstream.trim_end_matches('/').to_string(),
                    email: email.clone(),
                    purpose: ServiceJmapTokenPurpose::PublicRead,
                },
                pw,
            )
            .await;
        }
        let jmap = if m.wants_jmap {
            // Cookie-derived Bearer wins; the dev Basic auth is the fallback
            // (only ever `Some` on an internal listener, per the gate above).
            // A `kind="customer"` session carries NO maild authority — its
            // `maild_token` is empty by construction, so never derive a Bearer
            // from it (a logged-in customer then has no jmap identity, which is
            // correct: the portal never calls jmap()).
            let auth = session_payload
                .as_ref()
                .filter(|p| p.kind == "maild")
                .map(|p| format!("Bearer {}", p.maild_token))
                .or(dev_jmap_auth)
                .or(public_read_jmap_auth);
            vhost
                .jmap_upstream
                .clone()
                .map(|upstream| mix_handler::JmapCaps { upstream, auth })
        } else {
            None
        };
        // `bus`: the delegated `bus_call` seam. Only routes that granted
        // `bus:<verb>` get it, and ONLY after the Rust admin + CSRF gate
        // passes. Built from `req` borrows BEFORE `req` is moved below.
        let bus = if m.bus_verbs.is_empty() {
            None
        } else {
            // `dev_ambient`: a dev_session vhost on an INTERNAL listener authenticates
            // with no cookie. It buys ONLY the argument-free accelerator wake (see
            // build_bus_injection); every other delegated verb still needs a real
            // cookie-backed session. resolve_node_state independently refuses a
            // dev_session vhost bound to an external listener, so this cannot fire
            // publicly.
            let dev_ambient = on_internal_listener && vhost.dev_session_email.is_some();
            build_bus_injection(
                &node,
                &vhost,
                req.headers(),
                req.method().as_str(),
                &m,
                dev_ambient,
            )
            .await
        };
        // The double-submit CSRF token to inject as `$CSRF` and mirror into the
        // readable `cosmix_csrf` cookie, so a Mix-rendered form (the login modal, the
        // logout form, any future form) can embed a hidden field that matches what Rust
        // will check. CSRF *generation* stays Rust-owned (OsRng); the handler only
        // *consumes* the value, and the double-submit *check* stays in the Rust
        // `/auth/*` endpoints.
        //
        // For an AUTHENTICATED request the authoritative token is the one sealed in the
        // session — `POST /auth/logout` validates against `SessionPayload.csrf` — so
        // prefer it. That keeps `$CSRF` and the readable cookie from drifting out of
        // sync with what Rust checks, and heals a missing/stale/tampered readable cookie
        // back to the sealed value. For an anonymous request, reuse the request's
        // readable token when present (stable across navigations), else mint one.
        let req_csrf = session::cookie_value(req.headers(), session::CSRF_COOKIE);
        // A render we WILL cache is CSRF-NEUTRAL: `$CSRF` is `nil` and no readable
        // csrf cookie is set, so the cached bytes are identical for every
        // anonymous visitor (an embedded per-visitor token would leak across the
        // shared cache). Gated on actually holding a cache slot — a candidate the
        // cache refused (map full) renders normally + uncached, so it keeps its
        // csrf. For every other request, mint/reuse as before.
        let csrf: Option<String> = if pcache_slot.is_some() {
            None
        } else {
            Some(
                session_payload
                    .as_ref()
                    .map(|p| p.csrf.clone())
                    .or_else(|| req_csrf.clone())
                    .unwrap_or_else(session::new_csrf_token),
            )
        };
        // Set the readable cookie only when the token exists AND doesn't already
        // carry exactly this value (anon first contact, or an authenticated
        // request whose readable cookie is absent/stale). Never for a cache
        // candidate (csrf is None → no Set-Cookie → the response stays cacheable).
        let csrf_needs_cookie = csrf
            .as_deref()
            .is_some_and(|c| req_csrf.as_deref() != Some(c));
        // Capture the session identity BEFORE `session_email` is moved into the
        // handler. `Some` ⇔ an authenticated session (a real cookie session, or a
        // dev_session's ambient operator identity on the internal listener); `None`
        // ⇔ anonymous. The wake fires ONLY when this is `Some` — an anonymous POST
        // to a wake-granted route gets a 303 login redirect, which is also `< 400`,
        // and must never be able to nudge the drain.
        let wake_actor = session_email.clone();
        let mut resp = mix_handler::run_with_caps(
            &node.handler_ast_cache,
            &handler_root,
            &m.handler_ref,
            db,
            m.wants_db,
            m.allowed_dbs.clone(),
            jmap,
            m.wants_jmap,
            session_email,
            session_kind,
            csrf.clone(),
            bus,
            m.wants_net,
            req,
        )
        .await;
        // `wake:<verb>` — webd fires the accelerator wake ITSELF, best-effort,
        // after the handler returned a NON-ERROR status (`< 400`). This
        // deliberately includes 3xx: the sshm enqueue handlers confirm success
        // with a 303 `redirect_flash` (Post/Redirect/Get), so a 2xx-only gate
        // would miss every panel button. The wake is authority-free and the
        // drain is idempotent, so a wake fired on a non-enqueueing action (a
        // reindex, a validation-flash redirect) is a cheap empty-drain no-op —
        // acceptable imprecision for keeping the code decoupled from the
        // handler. Detached + short-deadline (see `fire_wake_after_response`):
        // never blocks or fails this response; a dropped wake costs only latency
        // (the target daemon's backstop timer recovers the work). Rate-limited
        // per vhost, exactly like the dev_session accelerator path.
        if let Some((wake_verb, wake_service)) = m.wake.clone() {
            // Only an AUTHENTICATED session's non-error response nudges the drain.
            // The `wake_actor.is_some()` gate excludes an anonymous caller whose 303
            // login redirect would otherwise pass the `< 400` check.
            if let Some(actor) = wake_actor.clone()
                && resp.status().as_u16() < 400
            {
                if wake_rate_limit_ok(&vhost.fqdn) {
                    bus_call_handler::fire_wake_after_response(
                        node.broker_handle.clone(),
                        &tokio::runtime::Handle::current(),
                        wake_service,
                        wake_verb,
                        bus_call_handler::DelegationInputs {
                            actor,
                            vhost: vhost.fqdn.clone(),
                            route_id: m.route_id.clone(),
                            request_id: session::new_csrf_token(),
                        },
                    );
                } else {
                    tracing::warn!(
                        target: "webd::bus", route_id = %m.route_id, verb = %wake_verb,
                        "post-response wake rate-limited; the backstop timer will pick the work up"
                    );
                }
            }
        }
        // Mirror the token into the readable cookie when needed, and ONLY on an HTML
        // document response. Gating on `text/html` keeps the Set-Cookie off cacheable
        // non-HTML handler responses (notably `/media/*`, which sets `Cache-Control:
        // public, max-age=…`) — a shared cache must never store and replay one visitor's
        // csrf cookie to another. The HTML pages reached here are per-user dynamic SSR
        // (no `public` cache directive), and every form that consumes the token lives on
        // an HTML page, so nothing is lost. `append` (not `insert`) so a Set-Cookie the
        // handler itself returned (e.g. a flash cookie) survives alongside this one.
        let resp_is_html = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/html"));
        if csrf_needs_cookie
            && resp_is_html
            && let Some(csrf_val) = csrf.as_ref()
            && let Ok(hv) = axum::http::HeaderValue::from_str(&csrf_set_cookie(
                csrf_val,
                session::SESSION_TTL_SECS,
            ))
        {
            resp.headers_mut()
                .append(axum::http::header::SET_COOKIE, hv);
        }
        // Cache-candidate render: buffer + (conditionally) store the response,
        // returning it either way. `store_and_respond` enforces the store-time
        // guards (status 200, no Set-Cookie, no private/no-store directive,
        // body ≤ cap, generation unchanged since the render began).
        if let Some(mut guard) = pcache_slot {
            return public_response_cache::store_and_respond(
                &vhost.public_response_cache,
                pcache_generation,
                &mut guard,
                resp,
            )
            .await;
        }
        return resp;
    }
    let serve = ServeDir::new(&vhost.www_dir);
    match serve.oneshot(req).await {
        Ok(mut resp) => {
            // Defence-in-depth on every static asset (CSS/JS and the
            // `/img/*` uploaded images): stop the browser second-guessing
            // the declared content-type. Uploads are already MIME-validated
            // by magic bytes at write time (media.rs).
            resp.headers_mut().insert(
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                axum::http::HeaderValue::from_static("nosniff"),
            );
            resp.into_response()
        }
        Err(infallible) => match infallible {},
    }
}

// ---------------------------------------------------------------------------
// SSR PIM Phase 2 — session login / logout (cookie-sealed maild token)
// ---------------------------------------------------------------------------

/// Path to the per-node session sealing key. Overridable via
/// `COSMIX_WEBD_SESSION_KEY_PATH` (dev/test); defaults to the canonical
/// `/var/lib/cosmix/webd/session.key` (D2.6).
fn session_key_path() -> PathBuf {
    std::env::var_os("COSMIX_WEBD_SESSION_KEY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/cosmix/webd/session.key"))
}

/// `Set-Cookie` for the sealed session (HttpOnly — JS can't read it).
fn session_set_cookie(value: &str, max_age: i64) -> String {
    format!(
        "{}={value}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age}",
        session::SESSION_COOKIE
    )
}

/// `Set-Cookie` for the readable CSRF companion (NOT HttpOnly — the
/// double-submit value a form/header must echo).
fn csrf_set_cookie(value: &str, max_age: i64) -> String {
    format!(
        "{}={value}; Secure; SameSite=Lax; Path=/; Max-Age={max_age}",
        session::CSRF_COOKIE
    )
}

/// Expire a cookie (`Max-Age=0`).
fn clear_cookie(name: &str, http_only: bool) -> String {
    let ho = if http_only { "; HttpOnly" } else { "" };
    format!("{name}=; Secure; SameSite=Lax; Path=/{ho}; Max-Age=0")
}

/// A 303 redirect with zero or more `Set-Cookie` headers.
fn redirect_with_cookies(location: &str, cookies: &[String]) -> Response {
    let mut b = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(axum::http::header::LOCATION, location);
    for c in cookies {
        b = b.header(axum::http::header::SET_COOKIE, c);
    }
    b.body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Validate a post-login `next` redirect target: must be a same-origin
/// absolute path (`/…`), never a scheme-relative or backslash-smuggled
/// host (`//host`, `/\host`, `/%5chost` once decoded) nor an absolute
/// URL (open-redirect guard). A backslash anywhere is rejected because
/// browsers/proxies normalise `\` → `/`, so `/\evil` would escape origin.
/// Control chars (incl. CR/LF header-injection) are rejected. Anything
/// failing the checks collapses to `/`.
fn safe_next(next: &str) -> String {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        // second byte must not be `/` or `\` (covers `/\host`).
        && next.as_bytes().get(1).is_none_or(|b| *b != b'/' && *b != b'\\')
        && !next.contains('\\')
        && !next.chars().any(|c| c.is_control());
    if ok {
        next.to_string()
    } else {
        "/".to_string()
    }
}

/// Build the `/login` Mix-page URL carrying an optional error code and the
/// post-login `next` target (both percent-encoded). The login UI is now the
/// Mix-rendered `/login` page + chrome modal (`h_login.mix`); the Rust `/auth/*`
/// endpoints redirect here (PRG) instead of re-rendering an inline form. The
/// handler renders an inline error banner from `err` and threads `next` into the
/// form's hidden field — `safe_next` re-validates it on the eventual POST.
fn login_page_location(err: Option<&str>, next: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    if let Some(e) = err {
        q.append_pair("err", e);
    }
    // Only carry a non-default `next` so the common URL stays clean (`/login`).
    if next != "/" {
        q.append_pair("next", next);
    }
    let qs = q.finish();
    if qs.is_empty() {
        "/login".to_string()
    } else {
        format!("/login?{qs}")
    }
}

/// Build the `/login?step=verify` code-entry URL (P3 email-2FA step 2),
/// carrying an optional error code and the post-login `next`. `h_login.mix`
/// renders the code form when it sees `step=verify`; the form POSTs to
/// `/auth/login/verify`. Mirrors [`login_page_location`]'s percent-encoding.
fn login_verify_location(err: Option<&str>, next: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("step", "verify");
    if let Some(e) = err {
        q.append_pair("err", e);
    }
    if next != "/" {
        q.append_pair("next", next);
    }
    format!("/login?{}", q.finish())
}

/// `Set-Cookie` for the HttpOnly email-2FA pending-login id. Short-lived
/// (`max_age` = the code TTL); carries ONLY the opaque map key.
fn login_pending_set_cookie(id: &str, max_age: i64) -> String {
    format!(
        "{}={id}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age}",
        session::LOGIN_PENDING_COOKIE
    )
}

/// Expire the pending-login cookie (on success, exhaustion, or expiry).
fn clear_pending_cookie() -> String {
    clear_cookie(session::LOGIN_PENDING_COOKIE, true)
}

/// Login form fields (`application/x-www-form-urlencoded`).
#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
    csrf: String,
    #[serde(default)]
    next: String,
}

/// CSRF-bearing form for logout (and any future state-changing POST).
#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

/// `GET /auth/login` — legacy entry point. The login UI is now the Mix-rendered
/// `/login` page + chrome modal, so this mints the pre-auth CSRF cookie and 303s
/// there (PRG), carrying `next`, so existing links/bookmarks still work. 404 on a
/// vhost with no maild upstream.
async fn login_get(
    Extension(vhost): Extension<Arc<VhostState>>,
    req: axum::extract::Request,
) -> Response {
    if vhost.jmap_upstream.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let next = req
        .uri()
        .query()
        .and_then(|q| url_query_value(q, "next"))
        .map(|n| safe_next(&n))
        .unwrap_or_else(|| "/".to_string());
    let csrf = session::new_csrf_token();
    redirect_with_cookies(
        &login_page_location(None, &next),
        &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
    )
}

/// `POST /auth/login` — verify the pre-auth CSRF double-submit, exchange
/// email+password for a maild bearer token (`/auth/tokens/issue`), seal
/// it into the session cookie, and redirect to `next`. On a bad CSRF,
/// lockout, upstream failure, or bad credentials it 303s back to the Mix
/// `/login?err=…` page (PRG), refreshing the CSRF cookie so the retry's
/// double-submit matches.
async fn login_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(upstream) = vhost.jmap_upstream.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let next = safe_next(&form.next);

    // Pre-auth double-submit: the readable CSRF cookie set by GET /login
    // must equal the submitted hidden field. A missing/mismatched cookie
    // means a cross-site POST or a stale form → re-render with a fresh
    // token rather than authenticate.
    let cookie_csrf = session::cookie_value(&headers, session::CSRF_COOKIE);
    if !cookie_csrf
        .as_deref()
        .map(|c| session::csrf_eq(c, &form.csrf))
        .unwrap_or(false)
    {
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("expired"), &next),
            &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
        );
    }

    // P0c: per-email failed-login lockout (layered on the connection-level
    // per_ip_rate guard). A locked email is rejected HERE without hitting maild,
    // so the lockout actually bounds the attempt rate and the bcrypt cost. The
    // response is the same shape as the other inline errors — no account-existence
    // signal (the key is the SUBMITTED email, locked regardless of existence).
    if login_lockout(&node, &form.email).await.is_some() {
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("locked"), &next),
            &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
        );
    }

    // Exchange password → maild bearer token (Slice 1). `node.http_client`
    // trusts maild's internal cert (danger_accept_invalid_certs) and runs
    // on the main runtime (this handler does, unlike the jmap() seam).
    let issue_url = format!("{}/auth/tokens/issue", upstream.trim_end_matches('/'));
    let issued = node
        .http_client
        .post(&issue_url)
        .basic_auth(&form.email, Some(&form.password))
        .json(&serde_json::json!({ "label": "webd-session" }))
        .send()
        .await;

    // Track a genuine 401 (bad credentials) so ONLY that records a throttle
    // failure. A maild TRANSPORT error 502s below (no record); a non-401 maild
    // RESPONSE (5xx/429) records nothing either, and — pre-existing behaviour,
    // unchanged here — still falls through to the generic invalid-credentials
    // reply. So a maild fault can never lock a legitimate user out.
    let mut auth_rejected = false;
    let token = match issued {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => v.get("token").and_then(|t| t.as_str()).map(str::to_string),
            Err(e) => {
                tracing::error!(error = %e, "login: maild issue returned malformed JSON");
                None
            }
        },
        Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
            auth_rejected = true;
            None
        }
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "login: maild issue non-success");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "login: maild issue request failed");
            // Treat an upstream/transport failure distinctly from bad creds so the
            // landing page shows "temporarily unavailable", not a misleading
            // "invalid credentials". Refresh the CSRF cookie so the retry's
            // double-submit matches the token the `/login` page will embed.
            let retry_csrf = session::new_csrf_token();
            return redirect_with_cookies(
                &login_page_location(Some("unavailable"), &next),
                &[csrf_set_cookie(&retry_csrf, session::SESSION_TTL_SECS)],
            );
        }
    };

    let Some(maild_token) = token else {
        // Record a throttle failure ONLY on a real credential rejection (401).
        if auth_rejected {
            login_record_failure(&node, &form.email).await;
        }
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("invalid"), &next),
            &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
        );
    };

    // Email-2FA (P3): if the account opted in, divert to a second factor BEFORE
    // sealing — and BEFORE clearing the failure window. 2FA-off (the common case)
    // falls through to clear + seal inline exactly as before. P3.4 (2026-07
    // audit): an INDETERMINATE lookup fails CLOSED — refuse the login rather
    // than silently skipping a user's second factor — unless the vhost's
    // explicit `mfa_break_glass` flag is set (audited operator action for a
    // confirmed broker outage; logged loudly per bypassed login).
    match account_requires_mfa(&node, &form.email).await {
        MfaDecision::On => {
            // Do NOT clear the per-email failure window here. The OTP step records
            // its own failures into that SAME window, and only the SECOND factor
            // clears it (see `login_verify_post`). Clearing on password success
            // would let a password-holder reset the OTP-failure counter between
            // guesses by re-submitting the (correct) password — defeating the
            // attempt cap across freshly-minted challenges (the Codex-flagged
            // brute-force). The top-of-handler `login_lockout` check then also
            // blocks re-login once the window locks.
            return begin_mfa_challenge(&node, &vhost, &form.email, maild_token, &next).await;
        }
        MfaDecision::Off => {}
        MfaDecision::Unavailable if vhost.mfa_break_glass => {
            tracing::warn!(
                vhost = %vhost.fqdn,
                "2fa: BREAK-GLASS ACTIVE — indeterminate enrollment lookup allowed \
                 through password-only by [[webd.vhost]] mfa_break_glass; remove the \
                 flag as soon as the broker is healthy"
            );
        }
        MfaDecision::Unavailable => {
            // Fail closed. Best-effort revoke the bearer maild just issued —
            // it was never disclosed to the browser, but an unused live token
            // shouldn't linger for its 30-day TTL. NOT a throttle failure
            // (the password was correct; this is an infrastructure fault).
            let revoke_url = format!("{}/auth/tokens/revoke", upstream.trim_end_matches('/'));
            if let Err(e) = node
                .http_client
                .post(&revoke_url)
                .bearer_auth(&maild_token)
                .send()
                .await
            {
                tracing::warn!(error = %e, "2fa fail-closed: best-effort token revoke failed");
            }
            let csrf = session::new_csrf_token();
            return redirect_with_cookies(
                &login_page_location(Some("unavailable"), &next),
                &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
            );
        }
    }

    // 2FA off — single factor complete; clear any accumulated failure window.
    login_clear_failures(&node, &form.email).await;

    // Seal the token into a fresh session bound to this vhost, with a new
    // post-auth CSRF mirrored into the readable cookie.
    let now = session::now_secs();
    let new_csrf = session::new_csrf_token();
    let payload = session::SessionPayload {
        vhost: vhost.fqdn.clone(),
        maild_token,
        // The login credential IS the identity — seal it EXACTLY as maild
        // authenticated it (same string passed to basic_auth above). NOT
        // lowercased: maild account lookup is case-sensitive, so normalizing
        // here would let `Admin@x` seal as `admin@x` and collide with a
        // different principal's grant (Codex BLOCKER). Seal == authenticated.
        email: form.email.clone(),
        iat: now,
        exp: now + session::SESSION_TTL_SECS,
        csrf: new_csrf.clone(),
        // Seal the account's LIVE epoch — a later `webd.session.revoke`
        // bump kills this cookie on every cookie-authorized surface.
        // `.max(0)`: a read error at seal time seals 0, never the -1
        // error sentinel — so a DB that errors on BOTH seal and check
        // can't accidentally produce a matching (-1 == -1) pair; sealed
        // epochs are always ≥ 0 and a -1 live read rejects everything.
        epoch: current_session_epoch(&vhost, &form.email).await.max(0),
        kind: "maild".to_string(),
        customer_id: 0,
    };
    match node.session.seal(&payload) {
        Ok(sealed) => redirect_with_cookies(
            &next,
            &[
                session_set_cookie(&sealed, session::SESSION_TTL_SECS),
                csrf_set_cookie(&new_csrf, session::SESSION_TTL_SECS),
            ],
        ),
        Err(e) => {
            tracing::error!(error = %e, "login: session seal failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /auth/logout` — verify the CSRF sealed in the session, revoke
/// the maild token (best-effort), and clear both cookies. Idempotent: no
/// valid session ⇒ just clear + redirect. A valid session with a bad
/// CSRF ⇒ 403 (don't let a cross-site POST force a logout).
async fn logout_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let now = session::now_secs();
    if let Some(sealed) = session::cookie_value(&headers, session::SESSION_COOKIE)
        && let Some(payload) = node.session.unseal(&sealed, &vhost.fqdn, now)
    {
        if !session::csrf_eq(&payload.csrf, &form.csrf) {
            return StatusCode::FORBIDDEN.into_response();
        }
        if let Some(upstream) = &vhost.jmap_upstream {
            let url = format!("{}/auth/tokens/revoke", upstream.trim_end_matches('/'));
            // Best-effort: a failed revoke still clears the cookie. maild
            // remains the authority; a stale token expires at its TTL.
            if let Err(e) = node
                .http_client
                .post(&url)
                .bearer_auth(&payload.maild_token)
                .send()
                .await
            {
                tracing::warn!(error = %e, "logout: maild revoke failed (cookie still cleared)");
            }
        }
    }
    redirect_with_cookies(
        "/login",
        &[
            clear_cookie(session::SESSION_COOKIE, true),
            clear_cookie(session::CSRF_COOKIE, false),
        ],
    )
}

/// Whether an account opted into email-2FA. P3.4 (2026-07 audit): the lookup
/// fails CLOSED — the cut is *determinate vs indeterminate*, not ok vs error.
/// A determinate answer decides (`On`, `Off`, or a clean `not_found` row ⇒
/// `Off` — maild positively stating no enrollment exists, the normal case for
/// a remote-maild vhost). An INDETERMINATE state (no broker, transport error,
/// timeout, any other daemon error) is `Unavailable` and the login is refused:
/// an attacker who can induce broker unavailability must not be able to skip a
/// user's second factor. The availability cost (broker = login SPOF for the
/// vhost) is met operationally — monitoring, and the explicit audited
/// `mfa_break_glass` vhost flag for a confirmed outage window — never by
/// silently degrading to password-only.
enum MfaDecision {
    On,
    Off,
    /// Enrollment could not be determined — fail closed (or break-glass).
    Unavailable,
}

/// Read the account's `mfa_enabled` from the LOCAL maild over Bus
/// (`maild.accounts.props.get`, namespace+key headers — the same wire path
/// maild's own CLI uses). Runs on the main runtime (this is a native axum
/// handler, like `service_jmap_bearer_auth`'s direct client use), so the broker
/// `NodedClient` is called directly. Wrapped in a short timeout so a wedged
/// broker can't stall login for the client's 60s internal ceiling. NB: targets
/// the local maild — a vhost whose accounts live on a REMOTE maild reads a
/// clean `not_found` here, which is determinate (⇒ `Off`); only genuinely
/// indeterminate states return [`MfaDecision::Unavailable`].
async fn account_requires_mfa(node: &NodeState, email: &str) -> MfaDecision {
    let Some(client) = node.broker_handle.load_full() else {
        tracing::warn!("2fa: broker unavailable; enrollment indeterminate (fail-closed)");
        return MfaDecision::Unavailable;
    };
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("namespace".to_string(), "accounts".to_string());
    headers.insert("key".to_string(), email.to_string());
    let fut = client.call_with_headers_raw("maild", "maild.props.get", &headers, "");
    match tokio::time::timeout(std::time::Duration::from_secs(3), fut).await {
        Ok(Ok((0, body, _))) => {
            // Strict parse (Codex review catch): only a WELL-FORMED reply is
            // determinate. A body that doesn't parse or lacks the `fields`
            // object is indeterminate ⇒ fail closed — `unwrap_or(false)`
            // here would quietly turn "malformed answer" into "2FA off".
            // Within a well-formed row, an absent/null `mfa_enabled` is a
            // real answer (account never enrolled) ⇒ Off.
            let fields = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    // `fields` must be a real OBJECT — `Value::get` on a
                    // string/array/number returns None and would otherwise
                    // read as a determinate "absent flag" ⇒ Off.
                    v.get("fields")
                        .and_then(|f| f.as_object().cloned())
                        .map(serde_json::Value::Object)
                });
            match fields {
                Some(f) => match f.get("mfa_enabled") {
                    Some(serde_json::Value::Bool(true)) => MfaDecision::On,
                    Some(serde_json::Value::Bool(false)) | Some(serde_json::Value::Null) | None => {
                        MfaDecision::Off
                    }
                    Some(_) => {
                        tracing::warn!(
                            "2fa: mfa_enabled has a non-bool shape; indeterminate (fail-closed)"
                        );
                        MfaDecision::Unavailable
                    }
                },
                None => {
                    tracing::warn!(
                        "2fa: maild.accounts.props.get rc=0 body malformed; indeterminate (fail-closed)"
                    );
                    MfaDecision::Unavailable
                }
            }
        }
        Ok(Ok((rc, body, _))) => {
            // maild ANSWERED with an error. A clean `not_found` is a
            // determinate "no enrollment row" (remote-maild vhost / legacy
            // account) ⇒ 2FA-off. Anything else (daemon-side storage fault,
            // unexpected shape) is indeterminate ⇒ fail closed.
            let not_found = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .is_some_and(|v| v.get("error_code").and_then(|e| e.as_str()) == Some("not_found"));
            if not_found {
                MfaDecision::Off
            } else {
                tracing::warn!(
                    rc,
                    "2fa: maild.accounts.props.get daemon error; indeterminate (fail-closed)"
                );
                MfaDecision::Unavailable
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "2fa: maild.accounts.props.get failed; indeterminate (fail-closed)");
            MfaDecision::Unavailable
        }
        Err(_) => {
            tracing::warn!("2fa: maild.accounts.props.get timed out; indeterminate (fail-closed)");
            MfaDecision::Unavailable
        }
    }
}

/// Step 1 of email-2FA: email a fresh code, park the pending-login state, and
/// 303 to the code-entry step. FAILS CLOSED on a send error or a saturated map
/// — never seals, never leaves a usable pending row, so a code that couldn't be
/// delivered can't authorise a session. The browser receives only the opaque
/// pending-id (HttpOnly); the live `maild_token` stays server-side.
async fn begin_mfa_challenge(
    node: &NodeState,
    vhost: &VhostState,
    email: &str,
    maild_token: String,
    next: &str,
) -> Response {
    let code = login_pending::generate_code();
    let subject = "Your sign-in code";
    let body = format!(
        "Your sign-in code for {host} is:\n\n    {code}\n\nIt expires in {mins} minutes. \
         If you didn't try to sign in, you can ignore this email.\n",
        host = vhost.fqdn,
        mins = login_pending::TTL_MINS,
    );
    // Send FIRST. On any send error, refuse the login — do NOT seal, do NOT
    // create a pending row (fail closed; the plan's load-bearing invariant).
    if let Err(e) = system_mail::send_system_mail(node, vhost, email, subject, &body).await {
        tracing::error!(error = %e, "2fa: could not email the sign-in code; refusing the login");
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("unavailable"), next),
            &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
        );
    }
    let now = std::time::Instant::now();
    let entry = login_pending::PendingLogin {
        // Byte-exact, as authenticated — NOT lowercased (mirrors the seal path:
        // maild account lookup is case-sensitive).
        email: email.to_string(),
        maild_token,
        code_hash: login_pending::hash_code(&code),
        expires_at: now + login_pending::TTL,
        attempts: 0,
    };
    let id = session::new_pending_id();
    {
        let mut map = node.login_pending.lock().await;
        if !login_pending::insert_bounded(&mut map, id.clone(), entry, now, login_pending::MAP_CAP)
        {
            // Map saturated with live, non-expired pendings (≈unreachable —
            // each costs a successful password auth). Fail closed.
            tracing::warn!("2fa: pending-login map saturated; refusing the login");
            let csrf = session::new_csrf_token();
            return redirect_with_cookies(
                &login_page_location(Some("unavailable"), next),
                &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
            );
        }
    }
    // 303 to the code step; set the HttpOnly pending cookie (id only).
    redirect_with_cookies(
        &login_verify_location(None, next),
        &[login_pending_set_cookie(
            &id,
            login_pending::TTL.as_secs() as i64,
        )],
    )
}

/// Code-entry form fields for `POST /auth/login/verify`.
#[derive(Deserialize)]
struct VerifyForm {
    code: String,
    csrf: String,
    #[serde(default)]
    next: String,
}

/// Step 2 of email-2FA: verify the emailed code, then seal the real session.
/// CSRF double-submit (as `/auth/login`) → load the pending row by its cookie
/// id → constant-time-verify the code (single-use, attempt-capped) → seal +
/// set the session/CSRF cookies + clear the pending cookie. A bad code keeps
/// the user on the code step (attempts remain); an exhausted/expired/missing
/// pending sends them back to step 1. A pending id is NEVER itself a session.
async fn login_verify_post(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<VerifyForm>,
) -> Response {
    let next = safe_next(&form.next);

    // Pre-auth double-submit CSRF — same contract as `login_post`. A mismatch
    // re-renders the code step with a fresh token (the pending row is untouched,
    // so a stale form just retries).
    let cookie_csrf = session::cookie_value(&headers, session::CSRF_COOKIE);
    if !cookie_csrf
        .as_deref()
        .map(|c| session::csrf_eq(c, &form.csrf))
        .unwrap_or(false)
    {
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_verify_location(Some("expired"), &next),
            &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
        );
    }

    // No pending cookie → nothing to verify; start over at step 1.
    let Some(pending_id) = session::cookie_value(&headers, session::LOGIN_PENDING_COOKIE) else {
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("expired"), &next),
            &[
                csrf_set_cookie(&csrf, session::SESSION_TTL_SECS),
                clear_pending_cookie(),
            ],
        );
    };

    let now = std::time::Instant::now();

    // Peek the pending row's email (without consuming) for the email-level
    // lockout gate below. Absent/expired → start over at step 1.
    let email = {
        let map = node.login_pending.lock().await;
        map.get(&pending_id)
            .filter(|e| !e.is_expired(now))
            .map(|e| e.email.clone())
    };
    let Some(email) = email else {
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("expired"), &next),
            &[
                csrf_set_cookie(&csrf, session::SESSION_TTL_SECS),
                clear_pending_cookie(),
            ],
        );
    };

    // Atomically RESERVE this attempt against the per-email failure window
    // BEFORE comparing the code. This single critical section is what bounds
    // TOTAL OTP guesses to MAX_FAILURES per window across freshly-minted
    // challenges AND concurrent verifies — the per-pending 5-attempt cap alone
    // wouldn't (a password-holder can mint a new challenge with a fresh cap),
    // and a check-then-record split would let concurrent racers each pass the
    // check before any records. Refused (already locked) → burn the pending and
    // go back to step 1. (begin_mfa_challenge can't even run for a locked email
    // — the top-of-login_post lockout check blocks step 1 first.)
    if !login_try_consume_attempt(&node, &email).await {
        node.login_pending.lock().await.remove(&pending_id);
        let csrf = session::new_csrf_token();
        return redirect_with_cookies(
            &login_page_location(Some("locked"), &next),
            &[
                csrf_set_cookie(&csrf, session::SESSION_TTL_SECS),
                clear_pending_cookie(),
            ],
        );
    }

    let verdict = {
        let mut map = node.login_pending.lock().await;
        login_pending::verify(&mut map, &pending_id, &form.code, now)
    };

    match verdict {
        login_pending::Verdict::Verified { email, maild_token } => {
            // Second factor passed — NOW clear the failure window deferred from
            // login_post (password + OTP failures share it).
            login_clear_failures(&node, &email).await;
            // Seal exactly as the non-2FA path does — fresh iat/exp/csrf, the
            // vhost from the request, the byte-exact email + live bearer carried
            // from step 1.
            let now = session::now_secs();
            let new_csrf = session::new_csrf_token();
            // `.max(0)` mirrors the login_post seal — see the comment there.
            let epoch = current_session_epoch(&vhost, &email).await.max(0);
            let payload = session::SessionPayload {
                vhost: vhost.fqdn.clone(),
                maild_token,
                email,
                iat: now,
                exp: now + session::SESSION_TTL_SECS,
                csrf: new_csrf.clone(),
                epoch,
                kind: "maild".to_string(),
                customer_id: 0,
            };
            match node.session.seal(&payload) {
                Ok(sealed) => redirect_with_cookies(
                    &next,
                    &[
                        session_set_cookie(&sealed, session::SESSION_TTL_SECS),
                        csrf_set_cookie(&new_csrf, session::SESSION_TTL_SECS),
                        clear_pending_cookie(),
                    ],
                ),
                Err(e) => {
                    tracing::error!(error = %e, "2fa verify: session seal failed");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        login_pending::Verdict::BadCode => {
            // The attempt was already counted by the atomic reservation above
            // (do NOT double-record). Stay on the code step; the pending cookie
            // persists (per-pending attempts may remain).
            let csrf = session::new_csrf_token();
            redirect_with_cookies(
                &login_verify_location(Some("code"), &next),
                &[csrf_set_cookie(&csrf, session::SESSION_TTL_SECS)],
            )
        }
        login_pending::Verdict::TooManyAttempts => {
            // Attempt already counted by the reservation. Burned — back to step
            // 1, clear the dead pending cookie.
            let csrf = session::new_csrf_token();
            redirect_with_cookies(
                &login_page_location(Some("locked"), &next),
                &[
                    csrf_set_cookie(&csrf, session::SESSION_TTL_SECS),
                    clear_pending_cookie(),
                ],
            )
        }
        login_pending::Verdict::NotFound => {
            // Expired or unknown id — back to step 1, clear the stale cookie.
            let csrf = session::new_csrf_token();
            redirect_with_cookies(
                &login_page_location(Some("expired"), &next),
                &[
                    csrf_set_cookie(&csrf, session::SESSION_TTL_SECS),
                    clear_pending_cookie(),
                ],
            )
        }
    }
}

/// Extract a single value from a `x-www-form-urlencoded` query string
/// (percent-decoded). Minimal — only what `login_get`'s `?next=` needs.
fn url_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Minimal percent-decode for the `next` query param (handles `%XX` and
/// `+`-as-space). Invalid escapes pass through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Per-vhost `/assets/{*path}` handler: serves the vhost's
/// `docs_dir/assets/` if present, 404 otherwise. Same dynamic
/// `ServeDir::new` shape as `serve_static` (each vhost has its own
/// docs_dir).
async fn serve_assets(
    Extension(vhost): Extension<Arc<VhostState>>,
    Path(rest): Path<String>,
    mut req: axum::extract::Request,
) -> Response {
    let Some(docs_dir) = &vhost.docs_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let assets_root = docs_dir.join("assets");
    if !assets_root.is_dir() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // ServeDir matches against the request URI, not the captured
    // path. Rewrite the URI so ServeDir sees the path relative to
    // the assets root (`/<rest>`) rather than `/assets/<rest>`.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let query = path_and_query
        .find('?')
        .map(|i| &path_and_query[i..])
        .unwrap_or("");
    let rewritten = format!("/{rest}{query}");
    if let Ok(uri) = rewritten.parse::<axum::http::Uri>() {
        *req.uri_mut() = uri;
    }
    let serve = ServeDir::new(&assets_root);
    match serve.oneshot(req).await {
        Ok(mut resp) => {
            // Defence-in-depth: `/assets/*` (the chrome CSS/JS) bypasses serve_static, so
            // add the same `nosniff` here — stop the browser second-guessing the declared
            // content-type. (serve_static's comment claims to cover "CSS/JS"; docs assets
            // actually route through here, so this is where they get it.)
            resp.headers_mut().insert(
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                axum::http::HeaderValue::from_static("nosniff"),
            );
            // `/assets/*` are the un-hashed shared chrome (base/site/cms/datatable .css
            // + base/site .js). Without a Cache-Control header browsers heuristic-cache
            // them off `Last-Modified`, so a deploy only showed up after a force-reload
            // (we used to paper over it with a `?v=` query). `no-cache` = "may cache, but
            // always revalidate before use": ServeDir emits `Last-Modified` and answers
            // `If-Modified-Since` with a cheap `304`, so a normal reload picks up a deploy
            // with one conditional round-trip and no re-download when unchanged. No fixed
            // `max-age` because the filenames are not content-hashed (Last-Modified is the
            // validator; deploys advance asset mtimes via `install`). If filenames ever
            // become content-hashed, switch to `public, max-age=31536000, immutable`.
            resp.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            );
            resp.into_response()
        }
        Err(infallible) => match infallible {},
    }
}

// ---------------------------------------------------------------------------
// Startup resolution (vhosts + legacy collapse → NodeState + identities)
// ---------------------------------------------------------------------------

/// Per-vhost / legacy inputs for `resolve_node_state`. The legacy
/// fields fold in the CLI overrides already (CLI > node.conf.mix >
/// hard-coded default), so this helper does *not* reach into the
/// `Cli` again.
struct ResolveInputs<'a> {
    webd: &'a cosmix_config::node::WebdConfig,
    /// Effective legacy cert/key after CLI override application.
    /// Either both `Some` or both `None`; an asymmetric pair is the
    /// caller's responsibility to reject (matches the pre-vhost
    /// behaviour where the listener was plain HTTP unless both were
    /// set).
    legacy_tls_cert: Option<PathBuf>,
    legacy_tls_key: Option<PathBuf>,
    /// Legacy top-level `www_dir` (after CLI override). Used as the
    /// document root for the legacy `Arc<VhostState>` when the
    /// legacy collapse fires.
    legacy_www_dir: PathBuf,
    /// Legacy CMS SQLite path. Always opened (preserves the pre-Phase-1
    /// behaviour: the single `web.db` is the legacy vhost's `db`).
    legacy_db_path: PathBuf,
    /// Legacy JMAP upstream + noded WS URL (after CLI override).
    legacy_jmap_upstream: String,
    legacy_noded_ws: String,
    /// Legacy docs directory (after CLI override).
    legacy_docs_dir: Option<PathBuf>,
    /// Wall-clock used for LE-chain `notBefore`/`notAfter`. Production
    /// passes `UnixTime::now()`; tests pin a fixed instant.
    now: rustls::pki_types::UnixTime,
}

/// Resolve `[[webd.vhost]]` rows + the legacy top-level pair into
/// the runtime `(vhosts_map, identities)` pair. The legacy fields
/// have already been folded against the CLI overrides; this helper
/// owns LDH validation, `validate_le_chain`, ACME config resolution
/// (via `cosmix_config::acme_policy::resolve_webd_acme`) and the
/// C3 pending-issuance guard, duplicate-host detection, and
/// per-vhost SQLite open.
///
/// On success every entry in `identities` has passed
/// `validate_le_chain` (or is absent because the legacy pair was
/// plain HTTP). The returned `HashMap` keys are lowercase Hosts;
/// aliases share the same `Arc<VhostState>` as their primary.
/// Output of `resolve_node_state`: the runtime host-router map +
/// the TLS identity list, plus the per-vhost ACME plans and the
/// (single, node-level) Subscriber-Agreement proof produced by
/// `cosmix_config::acme_policy::resolve_webd_acme`. At C3, ACME
/// plans are resolved far enough to run all config gates (LDH,
/// mutex-with-manual-TLS, DNS-01 / Phase 4, http_listen, ToS,
/// contact-email shape, LDH-normalised fqdn/aliases), but any
/// non-empty `acme_plans` is then deliberately refused by the
/// pending-issuance guard at the end of `resolve_node_state` —
/// `ResolvedNodeState` is therefore never observed with a
/// non-empty `acme_plans` at this commit. Commit 5 lifts the
/// guard (provisioner + pending-issuance SNI strict mode) so
/// ACME vhosts can resolve, register in `vhosts`, and have their
/// identities spliced into `identities` via the atomic resolver
/// swap.
#[derive(Debug)]
struct ResolvedNodeState {
    vhosts: HashMap<String, Arc<VhostState>>,
    identities: Vec<TlsIdentityConfig>,
    #[allow(dead_code)] // wired in Commit 5 (AcmeProvisioner consumer).
    acme_plans: Vec<AcmeVhostPlan>,
    #[allow(dead_code)] // wired in Commit 5 (ToS proof passed to instant-acme account create).
    acme_tos: Option<cosmix_config::AcmeTosAcceptance>,
    /// B1 fail-soft — `[[webd.vhost]]` rows whose per-vhost startup
    /// validation (www_dir existence, manual-PEM cert/key pair,
    /// `validate_le_chain`, per-vhost SQLite open) failed. These are
    /// **skipped** from `vhosts`/`identities`/`acme_plans` rather than
    /// aborting the whole resolve, so one bad vhost cert no longer takes
    /// down every listener (incl. healthy mesh). The set is threaded to
    /// `from_namespace_rows` (relaxes its config-row-must-be-in-runtime-map
    /// invariant for a known-disabled host) and `synthesize_listeners`
    /// (skips a disabled host named in a listener allowlist instead of
    /// rejecting it as unknown). The bootstrap row is left intact so the
    /// vhost resurrects on the next restart once the operator repairs it.
    disabled_vhosts: Vec<DisabledVhost>,
}

/// One `[[webd.vhost]]` row dropped by B1 fail-soft. `names` is the
/// full primary-plus-aliases set so a listener allowlist that references
/// any of them is pruned (not rejected); `host` (= `names[0]`) and
/// `reason` are kept for operator-facing logging and `webd.tls.status`.
#[derive(Debug, Clone)]
struct DisabledVhost {
    host: String,
    names: Vec<String>,
    reason: String,
}

fn resolve_node_state(inputs: ResolveInputs<'_>) -> Result<ResolvedNodeState> {
    let mut vhosts: HashMap<String, Arc<VhostState>> = HashMap::new();
    let mut identities: Vec<TlsIdentityConfig> = Vec::new();
    // B1 fail-soft — rows whose per-vhost validation failed. Populated
    // in the row loop below; the healthy subset still resolves.
    let mut disabled_vhosts: Vec<DisabledVhost> = Vec::new();

    // --- ACME pre-resolve ---
    //
    // Run the lib-config ACME resolver first so the typed gate errors
    // (mutex with manual TLS, DNS-01 / Phase 4, http_listen, ToS) fire
    // before any LDH / fs / cert work. The returned plans carry the
    // raw row strings; webd normalises `fqdn` / `aliases` against
    // `parse_request_host` further down so the provisioner sees the
    // same shape the runtime host-router does. lib-config deliberately
    // does not depend on `cosmix_daemon::http_host`.
    let ResolvedWebdAcme {
        tos_acceptance: acme_tos,
        plans: mut acme_plans,
    } = cosmix_config::acme_policy::resolve_webd_acme(inputs.webd)
        .map_err(|e| anyhow!("[webd] ACME config: {e}"))?;

    // --- Pre-pass: detect duplicate hosts BEFORE any I/O. ---
    //
    // Without this pass, a duplicate host between rows (or between a
    // row and the legacy collapse) is only caught after the second
    // row's `cms_db_path` parent has been `mkdir -p`'d, its SQLite
    // file opened, and `SCHEMA` executed against it — leaving disk
    // residue on a config error. Hoist the name-collision check to
    // run on the pure-config inputs, before any filesystem mutation.
    // The downstream insert-with-collision-check is kept as
    // defence-in-depth; it should now be unreachable on a valid
    // config.
    {
        let mut planned: HashMap<String, String> = HashMap::new();
        let mut record = |name: String, origin: String| -> Result<()> {
            if let Some(prev) = planned.insert(name.clone(), origin.clone()) {
                return Err(anyhow!(
                    "duplicate host {:?} in webd config — first declared by {} and \
                     re-declared by {}. Pick one config slot per FQDN.",
                    name,
                    prev,
                    origin
                ));
            }
            Ok(())
        };
        for (idx, row) in inputs.webd.vhost.iter().enumerate() {
            let Some(host) = cosmix_daemon::http_host::parse_request_host(&row.host) else {
                // Bad LDH; main loop will emit the precise error.
                continue;
            };
            record(host.clone(), format!("[[webd.vhost]] #{idx} host"))?;
            for alias in &row.aliases {
                let Some(a) = cosmix_daemon::http_host::parse_request_host(alias) else {
                    continue;
                };
                record(a, format!("[[webd.vhost]] #{idx} ({host}) alias"))?;
            }
        }
        let legacy_active = (inputs.legacy_tls_cert.is_some() && inputs.legacy_tls_key.is_some())
            || !inputs.webd.tls_server_name.is_empty();
        if legacy_active {
            for raw in &inputs.webd.tls_server_name {
                let Some(n) = cosmix_daemon::http_host::parse_request_host(raw) else {
                    continue;
                };
                record(n, "legacy [webd] tls_server_name".into())?;
            }
        }
    }

    // --- [[webd.vhost]] rows ---
    for (idx, row) in inputs.webd.vhost.iter().enumerate() {
        // LDH-validate primary + aliases via the same parser the
        // host_router middleware uses (case-folded, port stripped,
        // CR/LF/IPv6 rejected). A mismatch between the configured
        // host's accepted shape and what host_router will accept at
        // request time would silently 404 every request, so we share
        // exactly one normalisation function across both sites.
        let host = cosmix_daemon::http_host::parse_request_host(&row.host).ok_or_else(|| {
            anyhow!(
                "[[webd.vhost]] #{idx} host={:?} is not a valid LDH hostname",
                row.host
            )
        })?;
        let mut all_names: Vec<String> = vec![host.clone()];
        for alias in &row.aliases {
            let a = cosmix_daemon::http_host::parse_request_host(alias).ok_or_else(|| {
                anyhow!(
                    "[[webd.vhost]] #{idx} ({}): alias {:?} is not a valid LDH hostname",
                    host,
                    alias
                )
            })?;
            all_names.push(a);
        }

        // B1 fail-soft — the per-vhost validation that can fail for
        // *operational* (not config-structure) reasons is bundled into
        // one fallible closure: www_dir existence, the manual-PEM
        // cert/key pair, `validate_le_chain`, and the per-vhost SQLite
        // open. A failure here used to be a hard `?` that aborted the
        // whole resolve — one bad cert took down every listener,
        // healthy mesh included. Now the row is skipped and recorded;
        // the healthy subset still serves. Config-*structure* errors
        // (bad LDH host/alias above, duplicate host) stay fail-hard:
        // they can't be served correctly under any cert and bootstrap
        // would reject them too.
        //
        // Two issuance modes inside the closure:
        //   1. Manual-PEM (Phase 1): `tls_cert` + `tls_key` both
        //      required, both go through `validate_le_chain` (leaf
        //      chains to ISRG, intermediate SPKIs allowlisted, SANs
        //      cover every name in `all_names`).
        //   2. ACME (Phase 2): `acme = {...}` is set; manual-PEM is
        //      forbidden (lib-config resolver rejected the combo). No
        //      cert exists yet, so no validator runs and no TLS
        //      identity is emitted; the vhost still registers so HTTP /
        //      CMS surfaces work pre-issuance.
        type PreparedVhost = (
            PathBuf,
            Option<(PathBuf, PathBuf)>,
            Option<Arc<Mutex<Connection>>>,
        );
        let prepared: Result<PreparedVhost> = (|| {
            let www_dir = PathBuf::from(&row.www_dir);
            if !www_dir.is_dir() {
                return Err(anyhow!(
                    "[[webd.vhost]] #{idx} ({}): www_dir {:?} is not a directory",
                    host,
                    www_dir
                ));
            }

            // SAFETY GATE (startup, defense-in-depth). The HARD gate is
            // `serve_static`'s per-request `!scope.external` check; this belt
            // additionally refuses to even BIND a `dev_session` vhost reachable
            // on an unsafe listener, so a fat-fingered public dev_session fails
            // loudly at boot. Rules:
            //   * keyed on EITHER field (a superset of the per-request both-
            //     required predicate); an incomplete pair is always operator
            //     error, refused outright;
            //   * "unsafe" = an `external` (operator-declared public) listener OR
            //     a listener whose bind IP is not internal (loopback / RFC1918 /
            //     ULA / link-local) — so a listener mislabelled `external = false`
            //     on a public or `0.0.0.0` bind is still refused (ties trust to
            //     the kernel-observable bind, not just the operator flag);
            //   * `enabled` is intentionally NOT required — the L1 listener kill
            //     switch can re-enable a config-disabled listener at runtime.
            if row.dev_session_email.is_some() != row.dev_session_password.is_some() {
                return Err(anyhow!(
                    "[[webd.vhost]] #{idx} ({host}): dev_session is incomplete — set BOTH \
                     dev_session_email and dev_session_password, or neither."
                ));
            }
            // public_read pairing (both or neither). No listener-scope gate: unlike
            // dev_session, public_read is MEANT for public listeners, and it grants no
            // session (anonymous Basic-read only), so a public bind is its purpose, not
            // a footgun. The auth arm fires only when no cookie/dev applies.
            if row.public_read_email.is_some() != row.public_read_password.is_some() {
                return Err(anyhow!(
                    "[[webd.vhost]] #{idx} ({host}): public_read is incomplete — set BOTH \
                     public_read_email and public_read_password, or neither."
                ));
            }
            // system_sender pairing (both or neither). Like public_read it is not
            // listener-gated (transactional mail is a production surface); it grants
            // no session and is consumed only by `send_system_mail`.
            if row.system_sender_email.is_some() != row.system_sender_password.is_some() {
                return Err(anyhow!(
                    "[[webd.vhost]] #{idx} ({host}): system_sender is incomplete — set BOTH \
                     system_sender_email and system_sender_password, or neither."
                ));
            }
            if row.dev_session_email.is_some() {
                // Implicit single-listener mode (no `[[webd.listener]]` array)
                // lays NO per-listener ListenerScope, so the per-request gate
                // fails closed (dev_session is inert there) AND the implicit bind
                // is the public default — refuse rather than start a dev_session
                // vhost that silently won't work and isn't on a vetted internal
                // listener. dev_session therefore REQUIRES explicit listeners.
                let on_unsafe = inputs.webd.listener.is_empty()
                    || inputs.webd.listener.iter().any(|l| {
                        (l.external || !bind_is_internal(&l.bind))
                            && all_names
                                .iter()
                                .any(|n| l.vhosts.iter().any(|v| v.eq_ignore_ascii_case(n)))
                    });
                if on_unsafe {
                    return Err(anyhow!(
                        "[[webd.vhost]] #{idx} ({host}): dev_session_* is set but this vhost is not \
                         served exclusively on an internal listener — dev auto-session requires an \
                         explicit [[webd.listener]] with `external = false` AND a loopback/RFC1918/\
                         ULA/link-local bind. Remove dev_session_* or move the vhost to such a listener."
                    ));
                }
            }

            let cert_paths: Option<(PathBuf, PathBuf)> = if row.acme.is_some() {
                None
            } else {
                match (&row.tls_cert, &row.tls_key) {
                    (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
                    _ => {
                        return Err(anyhow!(
                            "[[webd.vhost]] #{idx} ({host}): tls_cert and tls_key are both \
                             required (manual-PEM mode) when no acme = {{...}} block is set"
                        ));
                    }
                }
            };

            // A vhost served EXCLUSIVELY on internal listeners (explicit
            // [[webd.listener]] with external = false AND a loopback/RFC1918/
            // ULA/link-local bind) is not publicly reachable, so it does not
            // need a public-CA (Let's Encrypt) chain — a self-signed / internal-CA
            // cert is accepted there, mirroring maild's *.bus exemption. This uses
            // the same internal-listener test as the dev_session gate above.
            // External or implicit-single-listener vhosts still go through
            // validate_le_chain (fail-closed to the public-CA requirement).
            let serving_internal_only = {
                let serving: Vec<_> = inputs
                    .webd
                    .listener
                    .iter()
                    .filter(|l| {
                        all_names
                            .iter()
                            .any(|n| l.vhosts.iter().any(|v| v.eq_ignore_ascii_case(n)))
                    })
                    .collect();
                !serving.is_empty()
                    && serving
                        .iter()
                        .all(|l| !l.external && bind_is_internal(&l.bind))
            };

            if let Some((cert_path, _key_path)) = cert_paths.as_ref() {
                let chain_pem = std::fs::read(cert_path).with_context(|| {
                    format!(
                        "[[webd.vhost]] #{idx} ({host}): reading tls_cert from {}",
                        cert_path.display()
                    )
                })?;
                if !serving_internal_only {
                    let expected_refs: Vec<&str> = all_names.iter().map(String::as_str).collect();
                    cosmix_daemon::tls::le_validator::validate_le_chain(
                        &chain_pem,
                        &expected_refs,
                        inputs.now,
                    )
                    .with_context(|| {
                        format!(
                            "[[webd.vhost]] #{idx} ({host}): validate_le_chain failed for {}",
                            cert_path.display()
                        )
                    })?;
                }
            }

            // Resolve auxiliary databases (ATTACHed onto the cms connection).
            // Each path must be absolute and distinct from cms_db_path and from
            // every other aux path, so a mis-config can't alias two schemas to
            // one file. Distinctness compares a CANONICAL key (deepest existing
            // ancestor canonicalized + remainder) so `.`/`..`/symlink aliases
            // can't defeat it. (Node-wide cross-vhost path uniqueness is a
            // documented follow-up; only one vhost uses aux_dbs today.)
            let mut aux: Vec<(String, PathBuf)> = Vec::new();
            {
                let mut seen_keys: std::collections::HashSet<PathBuf> =
                    std::collections::HashSet::new();
                if let Some(cp) = &row.cms_db_path {
                    seen_keys.insert(canonical_db_key(&PathBuf::from(cp)));
                }
                for a in &row.aux_dbs {
                    let apath = PathBuf::from(&a.path);
                    if !apath.is_absolute() {
                        anyhow::bail!(
                            "[[webd.vhost]] #{idx} ({host}): aux_db {} path {} is not absolute",
                            a.name,
                            a.path
                        );
                    }
                    if !seen_keys.insert(canonical_db_key(&apath)) {
                        anyhow::bail!(
                            "[[webd.vhost]] #{idx} ({host}): aux_db {} path {} duplicates another db on this vhost",
                            a.name,
                            a.path
                        );
                    }
                    aux.push((a.name.clone(), apath));
                }
            }
            if !aux.is_empty() && row.cms_db_path.is_none() {
                anyhow::bail!(
                    "[[webd.vhost]] #{idx} ({host}): aux_dbs set but cms_db_path is absent (no connection to attach onto)"
                );
            }

            // Per-vhost CMS SQLite — only opened if cms_db_path is set.
            let db = match &row.cms_db_path {
                Some(p) => {
                    let path = PathBuf::from(p);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!(
                                "[[webd.vhost]] #{idx} ({}): creating cms_db_path parent {}",
                                host,
                                parent.display()
                            )
                        })?;
                    }
                    let conn = open_db(&path, &aux).with_context(|| {
                        format!(
                            "[[webd.vhost]] #{idx} ({}): opening cms_db_path {}",
                            host,
                            path.display()
                        )
                    })?;
                    conn.execute_batch(SCHEMA)?;
                    Some(Arc::new(Mutex::new(conn)))
                }
                None => None,
            };

            Ok((www_dir, cert_paths, db))
        })();

        let (www_dir, cert_paths, db) = match prepared {
            Ok(v) => v,
            Err(reason) => {
                let reason = format!("{reason:#}");
                tracing::warn!(
                    target: "webd::resolve",
                    idx,
                    host = %host,
                    reason = %reason,
                    "[[webd.vhost]] skipped (fail-soft): the healthy vhost subset \
                     still serves; this vhost stays disabled until the operator \
                     repairs it and restarts webd",
                );
                // Drop a matching ACME plan so the provisioner doesn't
                // try to service a vhost that won't serve (e.g. an ACME
                // row that failed on www_dir / CMS-DB, not on a cert).
                acme_plans.retain(|p| p.vhost_index != idx);
                disabled_vhosts.push(DisabledVhost {
                    host: host.clone(),
                    names: all_names.clone(),
                    reason,
                });
                continue;
            }
        };

        let docs_dir = row.docs_dir.as_ref().map(PathBuf::from);
        let vhost_state = Arc::new(VhostState {
            fqdn: host.clone(),
            db,
            www_dir,
            jmap_upstream: row.jmap_upstream.clone(),
            noded_ws: row.noded_ws.clone(),
            docs_dir,
            dev_session_email: row.dev_session_email.clone(),
            dev_session_password: row.dev_session_password.clone(),
            public_read_email: row.public_read_email.clone(),
            public_read_password: row.public_read_password.clone(),
            system_sender_email: row.system_sender_email.clone(),
            system_sender_password: row.system_sender_password.clone(),
            mfa_break_glass: row.mfa_break_glass,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        if row.mfa_break_glass {
            // Loud at startup AND per bypassed login — a break-glass left
            // set after the outage is exactly the silent-fallback failure
            // mode the fail-closed design exists to prevent.
            tracing::warn!(
                host = %host,
                "[[webd.vhost]] mfa_break_glass = true — email-2FA lookups that are \
                 INDETERMINATE (broker down/timeout) will proceed password-only on \
                 this vhost. REMOVE the flag once the broker is healthy."
            );
        }

        // Register the shared Arc under primary + every alias.
        // Duplicate-host detection: any name already in the map (from
        // an earlier row, an alias of an earlier row, or this row's
        // primary colliding with this row's own alias) is a startup
        // error. Mirrors `SniCertResolver::from_config`'s
        // duplicate-`server_name` rejection.
        //
        // TLS identities are only emitted for manual-PEM rows; ACME
        // rows get their identity injected post-issuance by Commit 5's
        // provisioner via the atomic resolver swap.
        for name in &all_names {
            if vhosts.insert(name.clone(), vhost_state.clone()).is_some() {
                return Err(anyhow!(
                    "duplicate host {:?} — already registered by an earlier [[webd.vhost]] \
                     row or alias",
                    name
                ));
            }
            if let Some((cert_path, key_path)) = cert_paths.as_ref() {
                identities.push(TlsIdentityConfig {
                    server_name: name.clone(),
                    cert: cert_path.to_string_lossy().into_owned(),
                    key: key_path.to_string_lossy().into_owned(),
                    default: false,
                    no_sni_fallback: false,
                });
            }
        }

        // Normalise the ACME plan's fqdn/aliases in-place against
        // `parse_request_host` so the provisioner sees the same shape
        // the runtime host-router does. lib-config deliberately does
        // not depend on `cosmix_daemon::http_host`, so the plans land
        // here with the raw row strings.
        if row.acme.is_some()
            && let Some(plan) = acme_plans.iter_mut().find(|p| p.vhost_index == idx)
        {
            plan.fqdn = host.clone();
            plan.aliases = all_names.iter().skip(1).cloned().collect();
        }
    }

    // --- Legacy top-level pair collapse ---
    let legacy_tls = match (
        inputs.legacy_tls_cert.as_ref(),
        inputs.legacy_tls_key.as_ref(),
    ) {
        (Some(c), Some(k)) => Some((c.clone(), k.clone())),
        (None, None) => None,
        _ => {
            return Err(anyhow!(
                "legacy [webd] tls_cert and tls_key must both be set or both be absent"
            ));
        }
    };

    // Build the legacy `Arc<VhostState>` if either the legacy TLS
    // pair OR a non-empty tls_server_name is configured. The legacy
    // SQLite is always opened (matches the pre-Phase-1 single-DB
    // behaviour) so a plain-HTTP-with-tls_server_name deployment
    // still gets `/api/posts` working under the legacy host keys.
    let legacy_active = legacy_tls.is_some() || !inputs.webd.tls_server_name.is_empty();
    if legacy_active {
        // Both arms of legacy_active require tls_server_name to be
        // non-empty: the TLS arm needs it as the validator's SAN-list,
        // and the plain-HTTP arm needs it as the host-routing key-set.
        if inputs.webd.tls_server_name.is_empty() {
            return Err(anyhow!(
                "legacy [webd] tls_cert/tls_key set but tls_server_name is empty — \
                 declare the web FQDN(s) the legacy cert is issued for explicitly. \
                 Note: tls_server_name is the *web identity*, independent of \
                 served_mail_domains (the autoconfig admission list)"
            ));
        }

        // LDH-validate every tls_server_name entry through the same
        // parser host_router uses.
        let mut legacy_names: Vec<String> = Vec::new();
        for raw in &inputs.webd.tls_server_name {
            let n = cosmix_daemon::http_host::parse_request_host(raw).ok_or_else(|| {
                anyhow!(
                    "legacy [webd] tls_server_name entry {:?} is not a valid LDH hostname",
                    raw
                )
            })?;
            legacy_names.push(n);
        }

        // Validate the legacy chain if a cert is configured.
        if let Some((ref cert_path, _)) = legacy_tls {
            let chain_pem = std::fs::read(cert_path).with_context(|| {
                format!(
                    "legacy [webd] tls_cert: reading PEM from {}",
                    cert_path.display()
                )
            })?;
            let expected_refs: Vec<&str> = legacy_names.iter().map(String::as_str).collect();
            cosmix_daemon::tls::le_validator::validate_le_chain(
                &chain_pem,
                &expected_refs,
                inputs.now,
            )
            .with_context(|| {
                format!(
                    "legacy [webd] tls_cert: validate_le_chain failed for {}",
                    cert_path.display()
                )
            })?;
        }

        // Open the legacy SQLite unconditionally — preserves the
        // pre-vhost contract that `web.db` is always available.
        if let Some(parent) = inputs.legacy_db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = open_db(&inputs.legacy_db_path, &[])?;
        conn.execute_batch(SCHEMA)?;

        let legacy_state = Arc::new(VhostState {
            // Use the first tls_server_name entry as the informational
            // fqdn (matches what the legacy operator would describe as
            // "the primary identity").
            fqdn: legacy_names[0].clone(),
            db: Some(Arc::new(Mutex::new(conn))),
            www_dir: inputs.legacy_www_dir.clone(),
            jmap_upstream: Some(inputs.legacy_jmap_upstream.clone()),
            noded_ws: Some(inputs.legacy_noded_ws.clone()),
            docs_dir: inputs.legacy_docs_dir.clone(),
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });

        // Register the shared Arc under every tls_server_name entry.
        // First entry promotes to default + no_sni_fallback (matches
        // maild's Phase 1 collapse comment in tls.rs:22-26).
        for (i, name) in legacy_names.iter().enumerate() {
            if vhosts.insert(name.clone(), legacy_state.clone()).is_some() {
                return Err(anyhow!(
                    "duplicate host {:?} — legacy tls_server_name entry collides with a \
                     [[webd.vhost]] row's host/alias. Pick one config slot per FQDN.",
                    name
                ));
            }
            if let Some((ref cert_path, ref key_path)) = legacy_tls {
                identities.push(TlsIdentityConfig {
                    server_name: name.clone(),
                    cert: cert_path.to_string_lossy().into_owned(),
                    key: key_path.to_string_lossy().into_owned(),
                    default: i == 0,
                    no_sni_fallback: i == 0,
                });
            }
        }
    }

    if vhosts.is_empty() {
        // B1 fail-soft serves the *healthy* subset — but if every
        // configured vhost was disabled (or none were declared) there
        // is nothing to route, so we still refuse to start rather than
        // bind a server with an empty key-set. Name the disabled count
        // so the operator sees "all my certs are bad", not "I forgot to
        // declare a vhost".
        if !disabled_vhosts.is_empty() {
            let detail = disabled_vhosts
                .iter()
                .map(|d| format!("{} ({})", d.host, d.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow!(
                "webd resolved {} [[webd.vhost]] row(s) but every one failed per-vhost \
                 validation — refusing to start with an empty host key-set. Repair at \
                 least one vhost and restart. Disabled: {detail}",
                disabled_vhosts.len()
            ));
        }
        return Err(anyhow!(
            "webd has no [[webd.vhost]] rows and no legacy [webd] tls_server_name — \
             refusing to start. Declare at least one vhost or one tls_server_name \
             entry so Host routing has a registered key-set."
        ));
    }

    // C3 ACME-vhost rejection guard lifted by P2-C5d1. ACME plans
    // now flow through to `main()`, which binds the :80 HTTP-01
    // listener *before* calling `AcmeProvisioner::startup_pass`,
    // then splices the issued identities into the rustls resolver
    // and binds :443 with the full identity set. The "ACME-only
    // configs silently fall through to serve_plain" and "mixed
    // configs misroute via non-strict SNI" failure modes the
    // original guard defended against are now closed by the
    // listener-flow restructure in `main()`: ACME-only nodes get
    // a non-empty `identities` after the startup pass, and the
    // mixed-config SNI denylist will land with C5d2.

    Ok(ResolvedNodeState {
        vhosts,
        identities,
        acme_plans,
        acme_tos,
        disabled_vhosts,
    })
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Per-vhost router: every route that depends on `VhostState`.
/// Gated by `host_router` so an unknown Host returns `404` before
/// these handlers ever execute, and a known Host receives its
/// `Arc<VhostState>` via the `Extension` injected by the
/// middleware.
fn build_per_vhost_router(node: Arc<NodeState>) -> Router {
    Router::new()
        .route(
            "/api/posts",
            axum::routing::get(list_posts).post(create_post),
        )
        .route(
            "/api/posts/{id}",
            axum::routing::get(get_post)
                .put(update_post)
                .delete(delete_post),
        )
        .route("/jmap", axum::routing::any(jmap_proxy))
        .route("/jmap/{*rest}", axum::routing::any(jmap_proxy))
        // JMAP Session resource: maild serves it at `GET /.well-known/jmap`,
        // and a same-origin browser client needs it for accountId discovery.
        // Without this route the path falls through to `serve_static` (404),
        // so the proxied app could never resolve its account. `jmap_proxy`
        // still 404s vhosts with no `jmap_upstream`, so this is inert for
        // non-JMAP vhosts.
        .route("/.well-known/jmap", axum::routing::any(jmap_proxy))
        // SSR PIM Phase 2 — session login/logout. Under `/auth/` (not a
        // bare `/login`) to avoid shadowing a tenant's own login page, and
        // mirroring maild's `/auth/tokens/*`. 404 on a vhost with no maild
        // upstream. login_get also accepts `?next=/path` (open-redirect-
        // guarded).
        .route(
            "/auth/login",
            axum::routing::get(login_get).post(login_post),
        )
        // Email-2FA step 2 (P3): the code-entry POST. Same `/auth/` prefix +
        // maild-upstream gating story as `/auth/login`.
        .route("/auth/login/verify", axum::routing::post(login_verify_post))
        .route("/auth/logout", axum::routing::post(logout_post))
        // Customer billing-portal auth (Codex ruling A). POST-only crypto
        // endpoints under a distinct `/portal/auth/` prefix so they DON'T
        // shadow the Mix-rendered GET forms at /portal/login etc. (an axum
        // method-specific route claims the path — a GET would 405, not fall
        // through to the Mix handler). Inert on a vhost with no `customers`
        // table (fail-closed to invalid).
        .route(
            "/portal/auth/login",
            axum::routing::post(portal_auth::portal_login_post),
        )
        .route(
            "/portal/auth/set-password",
            axum::routing::post(portal_auth::portal_set_password_post),
        )
        .route(
            "/portal/auth/change-password",
            axum::routing::post(portal_auth::portal_change_password_post),
        )
        // CMS media library — filesystem-backed image storage. The byte
        // write must be native (Mix handlers are Pure+FsRead, FsWrite
        // denied). These win over the `/admin/media` Mix gallery handler
        // (distinct paths) and need a raised body limit for the base64
        // image payload. Auth is the unified maild session + author+ role
        // (cms_author, see media.rs).
        .route(
            "/admin/media/upload",
            axum::routing::post(media::media_upload).layer(axum::extract::DefaultBodyLimit::max(
                media::UPLOAD_BODY_LIMIT,
            )),
        )
        .route(
            "/admin/media/delete",
            axum::routing::post(media::media_delete),
        )
        .route("/ws", axum::routing::get(ws_proxy_handler))
        .route("/docs", axum::routing::get(serve_docs_index))
        .route("/docs/", axum::routing::get(serve_docs_index))
        .route("/docs/{*path}", axum::routing::get(serve_docs))
        .route("/assets/{*rest}", axum::routing::get(serve_assets))
        .fallback(serve_static)
        // Inner layer: records the response status against the per-vhost
        // counters. The `Extension<Arc<VhostState>>` it pulls is injected
        // by `host_router` (the outer layer below), so this only runs on
        // requests that already matched a known vhost — the layering
        // order is load-bearing.
        .layer(axum::middleware::from_fn(record_response_stats))
        .layer(axum::middleware::from_fn_with_state(
            node.clone(),
            host_router,
        ))
        .with_state(node)
}

/// Compose the HTTPS app: autoconfig branch (no host_router gate —
/// it has its own served-mail-domain admission) merged with the
/// host-routed per-vhost branch. `.layer` inside
/// `build_per_vhost_router` scopes the middleware to the per-vhost
/// branch only; merging the autoconfig branch sideways keeps it
/// out of the layer stack. The acceptance test
/// `autoconfig_path_bypasses_admit` is the regression fence.
fn build_router(node: Arc<NodeState>) -> Router {
    let autoconfig = autoconfig_routes().with_state(node.clone());
    let per_vhost = build_per_vhost_router(node);
    Router::new().merge(autoconfig).merge(per_vhost)
}

/// RFC 8555 §8.3 token shape validator. ACME HTTP-01 tokens are
/// base64url **without padding** of ≥128-bit random material. The
/// alphabet is `[A-Za-z0-9_-]`; the syntactic length floor is
/// `ceil(128 / 6) = 22` chars; the upper bound here is generous
/// (LE itself emits 43-char tokens) and protects the lookup from
/// pathological-length probes. Validating shape before the map
/// lookup keeps the route from being a free oracle for tokens of
/// any length / character class.
fn is_valid_acme_token(t: &str) -> bool {
    let len = t.len();
    (22..=128).contains(&len)
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `GET /.well-known/acme-challenge/{*tail}` handler — the
/// wildcard catches every URL with a non-empty path segment
/// under the ACME prefix (`/foo`, `/foo/bar`), and the bare
/// prefix + trailing-slash siblings dispatched to
/// `acme_challenge_prefix_404` cover the remaining two shapes
/// (`/.well-known/acme-challenge` and
/// `/.well-known/acme-challenge/`) that axum's `{*tail}` does
/// not match. Together they guarantee every URL under the ACME
/// prefix returns the uniform 404 from these handlers instead
/// of leaking into the redirect / admit fallback (which would
/// respond 301 / 400 and break the "no probe oracle" property).
///
/// Returns the key authorisation (RFC 8555 §8.1) as
/// `application/octet-stream` on a known token (matching the
/// content type used in the RFC 8555 §8.3 example), `404` on an
/// unknown or invalid-shape token. The handler never returns
/// `400` — a uniform `404` for "multi-segment", "valid-shape
/// unknown token", and "invalid-shape token" keeps the surface
/// predictable and matches the precedent of every other webd
/// 404-on-unknown path; it also denies a probe oracle that could
/// distinguish the rejection reasons.
///
/// Plumbed via `Extension(map)` rather than `State` so the
/// challenge map can ride alongside the redirect/autoconfig
/// routers (which are stateful on `Arc<NodeState>`) without
/// changing their state type.
async fn acme_challenge_serve(
    Extension(map): Extension<Arc<RwLock<HashMap<String, String>>>>,
    tail: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Response {
    // axum percent-decodes the wildcard parameter before extraction;
    // an invalid-UTF-8 percent escape (e.g. `%FF`) causes the
    // extractor to reject with `400`, which would otherwise become
    // a probe oracle distinguishing "extractor rejected" from
    // "uniform 404." Catch the rejection here and collapse it to
    // 404 so every malformed input returns the same status.
    let Ok(Path(tail)) = tail else {
        return (StatusCode::NOT_FOUND, "").into_response();
    };
    // The wildcard captures everything after `acme-challenge/`,
    // including embedded slashes (and percent-decoded `%2F` which
    // axum has already turned back into `/`). A legitimate token
    // is exactly one non-empty path segment with no slash; anything
    // else (multi-segment, empty, dot-segments) is a 404.
    if tail.is_empty() || tail.contains('/') || !is_valid_acme_token(&tail) {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let map = map.read().await;
    match map.get(&tail) {
        Some(keyauth) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            keyauth.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

/// `GET /.well-known/acme-challenge` (no trailing path) — uniform
/// 404 so a bare-prefix probe does not leak into the redirect /
/// admit fallback. Sibling of `acme_challenge_serve`.
async fn acme_challenge_prefix_404() -> Response {
    (StatusCode::NOT_FOUND, "").into_response()
}

/// NS-3.0 `00-default-http` router for the plain-HTTP (:80) listener:
/// the ACME HTTP-01 challenge route is carved out **above** the
/// mail-client autoconfig routes and the catch-all
/// `301 → https://$host$request_uri` fallback. The challenge route
/// must precede both the autoconfig admission and the redirect's
/// `plain_http_host_admit` middleware: RFC 8555 §8.3 requires the
/// server to serve the key authorisation under the requested host
/// regardless of any per-host admission policy (the validator
/// connects to the authoritative `A`/`AAAA` of the FQDN being
/// validated, and the Host header is whatever LE's validator sends —
/// often the ACME-pending FQDN that isn't in `NodeState::vhosts`
/// yet because the provisioner runs *before* the resolver swap).
///
/// Plain-HTTP autoconfig is **not** optional: the
/// `autoconfig.<domain>` host is frequently cert-uncovered (a DNS
/// wildcard matches one label only — maild-autoconfig.md §Constraints),
/// so the `http://` attempt and the `.well-known` HTTPS path are the
/// reliable ones.
fn build_http_redirect_router(node: Arc<NodeState>) -> Router {
    // ACME HTTP-01 challenge branch — runs above everything else
    // and bypasses both admission gates. Extension-plumbed so the
    // route can share the redirect router's per-host shape without
    // its admit layer.
    //
    // Three sibling routes together cover every URL shape under
    // the ACME prefix without leaking into the redirect fallback:
    //   * `{*tail}` — single-or-multi-segment after the trailing
    //     slash, dispatched to `acme_challenge_serve` which
    //     applies the shape gate and `tail.contains('/')` reject.
    //   * bare `/.well-known/acme-challenge` (no trailing slash)
    //     and `/.well-known/acme-challenge/` (trailing slash,
    //     empty tail) — neither matches axum's non-empty `{*tail}`
    //     wildcard, so each gets an explicit 404 sibling. Without
    //     these the trailing-slash probe leaks into the redirect
    //     branch as a 301 (Codex R2 finding, verified by test).
    let challenges = node.acme_challenges.clone();
    let acme = Router::new()
        .route(
            "/.well-known/acme-challenge/{*tail}",
            axum::routing::get(acme_challenge_serve),
        )
        // Bare-prefix and trailing-slash siblings. axum's `{*tail}`
        // wildcard requires a non-empty segment, so without these
        // two routes a probe for `/.well-known/acme-challenge` or
        // `/.well-known/acme-challenge/` would miss the ACME branch
        // and leak into the redirect fallback (301) — exactly the
        // probe-oracle vector the uniform-404 policy is designed
        // to close.
        .route(
            "/.well-known/acme-challenge",
            axum::routing::get(acme_challenge_prefix_404),
        )
        .route(
            "/.well-known/acme-challenge/",
            axum::routing::get(acme_challenge_prefix_404),
        )
        .layer(Extension(challenges));
    // Autoconfig branch — bypasses the redirect admit, runs its
    // own admission (the served-mail-domain gate inside
    // `mozilla_autoconfig`).
    let autoconfig = autoconfig_routes().with_state(node.clone());
    // Redirect branch — every request that does *not* match an
    // autoconfig route lands here. `route_layer` panics in axum 0.8
    // on a fallback-only router, so `.layer` is the right tool;
    // branch-then-merge isolation keeps the layer scoped to the
    // redirect branch and out of the autoconfig branch.
    //
    // C3b: the admit set is no longer a router-build-time snapshot
    // — `plain_http_host_admit` reads `NodeState::vhosts.load()
    // .admit_plain_http` per request so vhosts added after startup
    // (via the C4/C5 provisioner publish path) become routable
    // immediately. Pass the `Arc<NodeState>` as State; the
    // middleware lifts the admit set off the current directory
    // snapshot.
    let redirect =
        Router::new()
            .fallback(redirect_to_https)
            .layer(axum::middleware::from_fn_with_state(
                node.clone(),
                plain_http_host_admit,
            ));
    Router::new().merge(acme).merge(autoconfig).merge(redirect)
}

/// `301 → https://$host$request_uri`, mirroring the NS 3.0 nginx
/// `return 301 https://$host$request_uri`.
///
/// The `Host` header is attacker-controlled (`feedback_bus_wire_trust_boundary`,
/// maild-autoconfig.md §Security). The NS-3.0 reference emits
/// `https://$host$request_uri`; nginx `$host` is the host **without
/// port, lowercased**, and maild-autoconfig.md makes Appendix A
/// normative for wire behaviour. So this strips a single optional
/// numeric port, lowercases, and rejects anything that is not a bare
/// hostname (extra colons, non-numeric port, IPv6 literal, CR/LF) —
/// matching `$host` exactly and closing the response-header-injection
/// vector a raw reflect would open. An absent or malformed host is a
/// clean `400`, never a malformed redirect.
async fn redirect_to_https(req: axum::extract::Request) -> Response {
    let raw_host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    // Strict Host parse (the canonical implementation of these rules
    // now lives in `cosmix_daemon::http_host`; this site was its
    // origin before webd-vhosts Phase 1 commit 1 lifted it).
    let host = match cosmix_daemon::http_host::parse_request_host(raw_host) {
        Some(h) => h,
        None => return (StatusCode::BAD_REQUEST, "missing or invalid Host header").into_response(),
    };
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let location = format!("https://{host}{path_and_query}");
    (
        StatusCode::MOVED_PERMANENTLY,
        [(axum::http::header::LOCATION, location)],
    )
        .into_response()
}

/// Serve `app` over plain HTTP on `listen` until the listener errors.
/// `kind` is a short label for the startup log line.
async fn serve_plain(listen: &str, app: Router, kind: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("cosmix-web listening on {listen} ({kind})");
    axum::serve(listener, app).await?;
    Ok(())
}

/// DEV-MODE static server — `cosmix-webd serve --static-dir <dir>`.
///
/// Serves `static_dir` as a plain static site on a **loopback-only**
/// listener, behind a minimal synthesized `localhost` vhost. No database,
/// no Bus broker, no ACME/TLS, no `node.conf.mix` — a zero-config local
/// preview (the substrate-native replacement for a throwaway `php -S`).
///
/// **No embedded Mix, no CMS.** The synthesized vhost has `db = None` and
/// the node's handler table is empty, so [`serve_static`] never finds a
/// `webd.handlers` match (embedded Mix handlers unreachable) and the
/// `/api/posts`, `/jmap`, `/ws` routes 404 by construction (their per-vhost
/// fields are `None`). The folder is served as static files; `docs_dir` is
/// set to the folder so the built-in `/docs` markdown viewer and `/assets/`
/// also resolve under it (the markdown path canonicalises reads under
/// `docs_dir`). Embedded Mix handlers + the CMS API need a registered vhost
/// + database and are intentionally unreachable here.
///
/// Fenced to the loopback interface so the no-auth / no-isolation dev
/// posture cannot be reached off-box: `--listen` (if given) must resolve to
/// a loopback host, otherwise the daemon refuses to start.
async fn run_static_dev_server(static_dir: PathBuf, cli_listen: Option<String>) -> Result<()> {
    let www_dir = static_dir.canonicalize().with_context(|| {
        format!("--static-dir {static_dir:?} does not exist or is not readable")
    })?;
    if !www_dir.is_dir() {
        anyhow::bail!("--static-dir {www_dir:?} is not a directory");
    }

    // Fence to loopback. Default to a conventional dev port; an explicit
    // --listen is resolved and every resolved address must be loopback. We
    // bind the resolved IPv4 loopback SocketAddr, never the raw string.
    let listen_arg = cli_listen.unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let addr = resolve_dev_listen(&listen_arg)?;

    // Minimal synthesized localhost vhost — static root only, everything
    // else off. `docs_dir = www_dir` so `/docs/*` and the dedicated
    // `/assets/*` route also map under the served folder. Primary fqdn
    // "localhost"; alias "127.0.0.1" plus the actual bound IPv4 loopback
    // (e.g. a `127.x.y.z` given via --listen) so a direct-IP `Host` header
    // routes too. Dedup so `VhostDirectory::build` doesn't see a repeat.
    let bound_ip = addr.ip().to_string();
    let mut aliases = vec!["127.0.0.1".to_string()];
    if bound_ip != "127.0.0.1" {
        aliases.push(bound_ip);
    }
    let vhost = Arc::new(VhostState {
        fqdn: "localhost".to_string(),
        db: None,
        www_dir: www_dir.clone(),
        jmap_upstream: None,
        noded_ws: None,
        docs_dir: Some(www_dir.clone()),
        dev_session_email: None,
        dev_session_password: None,
        public_read_email: None,
        public_read_password: None,
        system_sender_email: None,
        system_sender_password: None,
        mfa_break_glass: false,
        stats: Arc::new(stats::WebdStats::new()),
        session_epoch_cache: SessionEpochCache::default(),
        public_response_cache: public_response_cache::Cache::default(),
    });
    let directory =
        vhost_directory::VhostDirectory::build(vec![vhost_directory::VhostDirectoryEntry {
            state: vhost,
            aliases,
        }])
        .context("building dev-mode vhost directory")?;

    // Minimal NodeState — broker / props / ACME / handlers all empty or
    // unattached (mirrors the bootstrap / test-fixture shape). The empty
    // `handlers` table is what makes this static-only.
    let (_tls_status_tx, tls_status_rx) =
        tokio::sync::watch::channel(tls_status::TlsStatusSnapshot::default());
    let node = Arc::new(NodeState {
        service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        login_throttle: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        login_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        vhosts: Arc::new(ArcSwap::from(Arc::new(directory))),
        http_client: reqwest::Client::builder()
            .build()
            .context("building dev-mode HTTP client")?,
        session: Arc::new(session::SessionSealer::ephemeral()),
        served_mail_domains: HashSet::new(),
        autoconfig_mail_host: None,
        mx: None,
        acme_challenges: Arc::new(RwLock::new(HashMap::new())),
        tls_status_rx,
        props_router: Arc::new(cosmix_props::PropsRouter::new("webd")),
        props_subscribe_granter: Arc::new(bus::subscribe_granter::NodedSubscribeGranter::new(
            bus::subscribe_granter::new_broker_handle(),
        )),
        broker_handle: bus::subscribe_granter::new_broker_handle(),
        vhosts_runtime: None,
        listeners_runtime: None,
        listeners_operators: Vec::new(),
        vhost_key_locks: None,
        acme_notify: None,
        acme_force_renew_queue: None,
        handlers: Arc::new(ArcSwap::from(
            Arc::new(mix_handler::HandlerTable::default()),
        )),
        handler_ast_cache: mix_handler::new_ast_cache(),
        tls_reload: None,
    });

    let listen = addr.to_string();
    info!(
        "DEV static server: {} → http://{}/ (loopback only; no DB / Mix \
         handlers / CMS API / TLS)",
        www_dir.display(),
        listen
    );
    // cosmix_log routes to journald (daemon mode), so a stdout banner the
    // interactive user can't miss — this is a foreground dev tool.
    println!("cosmix-webd dev server");
    println!("  serving : {}", www_dir.display());
    println!("  url     : http://{listen}/");
    println!(
        "  mode    : static files + webd's /docs markdown viewer; no Mix \
         handlers / CMS API / DB / TLS (loopback only)"
    );
    println!("  stop    : Ctrl-C");
    serve_plain(&listen, build_router(node), "static-dev").await
}

/// Resolve + validate the `--static-dir` dev-mode listen address and return
/// the IPv4 loopback `SocketAddr` to bind.
///
/// RESOLVES the string (so a `localhost` that `/etc/hosts` maps off-box
/// cannot slip a non-loopback bind past the check) and requires **every**
/// resolved address to be loopback — we then bind the resolved `SocketAddr`,
/// never the raw string. An IPv6-only resolution is refused with a pointer to
/// the v4 form, because the per-vhost host-router can't match an IPv6 literal
/// `Host` header (`[::1]:port`), so a browser hitting an `[::1]` URL would
/// 400 (`cosmix_daemon::http_host::parse_request_host`).
fn resolve_dev_listen(listen: &str) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = listen
        .to_socket_addrs()
        .with_context(|| {
            format!(
                "--static-dir dev mode: cannot parse/resolve listen {listen:?} \
                 (use 127.0.0.1:PORT or localhost:PORT)"
            )
        })?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("--static-dir dev mode: {listen:?} resolved to no socket address");
    }
    // Every resolution must be loopback — closes the "localhost → non-loopback
    // in /etc/hosts" hole. We bind a resolved address, never the raw string.
    for a in &addrs {
        if !a.ip().is_loopback() {
            anyhow::bail!(
                "--static-dir dev mode binds loopback only; {listen:?} resolves \
                 to non-loopback {}",
                a.ip()
            );
        }
    }
    // Prefer IPv4 loopback so the host-router can match the browser's `Host`.
    if let Some(v4) = addrs.iter().find(|a| a.is_ipv4()) {
        return Ok(*v4);
    }
    anyhow::bail!(
        "--static-dir dev mode: {listen:?} resolves only to IPv6 loopback, but the \
         host router can't match an IPv6 Host header — use 127.0.0.1:PORT or \
         localhost:PORT"
    )
}

// HTTPS termination + per-handshake hot-swap now live in the shared
// `cosmix_daemon::listen::ListenerSet` (`TlsMode::Terminate` +
// `ListenerTls`); `main()` builds one listener per resolved interface
// and plugs a `WebdConnHandler` for the post-accept hyper serve. The
// old free-standing `serve_tls` was removed in P2-C2.

// ---------------------------------------------------------------------------
// Rusqlite optional helper (query_row that returns Option)
// ---------------------------------------------------------------------------

trait QueryRowOptional {
    fn optional(self) -> Result<Option<Post>, rusqlite::Error>;
}

impl QueryRowOptional for Result<Post, rusqlite::Error> {
    fn optional(self) -> Result<Option<Post>, rusqlite::Error> {
        match self {
            Ok(post) => Ok(Some(post)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// `cosmix-webd vhost` CLI subcommands
// ---------------------------------------------------------------------------

/// Run a CLI `vhost` subcommand as a transient Bus client.
/// Mirrors `cosmix-maild`'s `run_domain_cli` (commit `6dc28ab`):
/// connects via the broker, calls the `webd.props.*` surface for the
/// `vhosts` namespace, and renders structured-error bodies into
/// operator-readable messages.
///
/// `webd.vhosts` runs with `require_version = true`
/// (`vhosts_namespace.rs::spec`), so `Remove` follows the pre-get +
/// `if_version` shape the substrate requires:
/// substrate enforces `if_version` BEFORE row existence
/// (`cosmix-lib-props-store/src/runtime.rs`), so a naive
/// delete-without-`if_version` fails for both existing AND missing
/// rows.
async fn run_vhost_cli(action: VhostAction) -> Result<()> {
    use std::collections::BTreeMap;
    const NAMESPACE: &str = "vhosts";

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow!(
                "webd daemon not reachable via broker ({e}). \
                 `cosmix-webd vhost` requires the daemon to be \
                 running — `systemctl start cosmix-webd` (or equivalent) first."
            )
        })?;

    match action {
        VhostAction::Add {
            fqdn,
            www_dir,
            acme_provider,
            acme_challenge,
            acme_contact_email,
            tls_cert_path,
            tls_key_path,
            disabled,
        } => {
            // ACME trio prevalidation. The namespace hook
            // (`vhosts_namespace.rs:819+`) only enforces companion
            // fields when `acme_provider` is present, so
            // `--acme-challenge` or `--acme-contact-email` without a
            // provider would silently persist as dead columns. Reject
            // here so the operator sees the mistake immediately
            // instead of after a successful `add` followed by a
            // confused `acme status`.
            let acme_partial = acme_challenge.is_some() || acme_contact_email.is_some();
            if acme_partial && acme_provider.is_none() {
                return Err(anyhow!(
                    "--acme-challenge / --acme-contact-email require \
                     --acme-provider; either set all three or none."
                ));
            }
            // Also refuse ACME + manual TLS in the same row — the
            // daemon's TLS-resolver only honours one source per vhost.
            if acme_provider.is_some() && (tls_cert_path.is_some() || tls_key_path.is_some()) {
                return Err(anyhow!(
                    "--acme-provider is mutually exclusive with \
                     --tls-cert-path / --tls-key-path; pick one TLS source."
                ));
            }
            // Manual-TLS pair must be set together. The namespace hook
            // requires `tls_key_path` once `tls_cert_path` is present
            // but does NOT enforce the reverse — a lone `--tls-key-path`
            // would persist as a dead column on a disabled-staged row.
            // Reject both half-pairs symmetrically here.
            if tls_cert_path.is_some() && tls_key_path.is_none() {
                return Err(anyhow!(
                    "--tls-cert-path requires --tls-key-path; pass both PEM paths."
                ));
            }
            if tls_key_path.is_some() && tls_cert_path.is_none() {
                return Err(anyhow!(
                    "--tls-key-path requires --tls-cert-path; pass both PEM paths."
                ));
            }
            // `webd.vhost.add` reads its parameters as flat HEADER
            // kwargs (see `bus/vhost_verbs.rs::vhost_add`), not a
            // JSON body. It stamps `source = "bus.runtime"` daemon-
            // side and performs the tombstone-aware OCC anchor so a
            // prior `remove` doesn't trap the FQDN.
            let mut headers = BTreeMap::new();
            headers.insert("fqdn".to_string(), fqdn.clone());
            headers.insert("www_dir".to_string(), www_dir);
            if let Some(p) = acme_provider {
                headers.insert("acme.provider".to_string(), p);
            }
            if let Some(c) = acme_challenge {
                headers.insert("acme.challenge".to_string(), c);
            }
            if let Some(e) = acme_contact_email {
                headers.insert("acme.contact_email".to_string(), e);
            }
            if let Some(c) = tls_cert_path {
                headers.insert("tls.cert_path".to_string(), c);
            }
            if let Some(k) = tls_key_path {
                headers.insert("tls.key_path".to_string(), k);
            }
            if disabled {
                headers.insert("enabled".to_string(), "false".to_string());
            }
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("webd", "webd.vhost.add", &headers, "")
                .await?;
            if rc == 0 {
                println!("Added vhost {fqdn}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                // `vhost.add` errors carry either `error` (legacy) or
                // `message` (substrate envelope on policy-cap failures).
                let msg = parsed
                    .get("error")
                    .and_then(|v| v.as_str())
                    .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow!("add failed (rc={rc}): {msg}"));
            }
        }
        VhostAction::List => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("webd", "webd.props.list", &headers, "")
                .await?;
            if rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                return Err(anyhow!("list failed (rc={rc}): {msg}"));
            }
            let resp: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
            let records = resp
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if records.is_empty() {
                println!("No vhosts.");
            } else {
                // TLS column summarises the per-vhost TLS posture:
                //   acme:<provider>  — ACME plan present
                //   manual           — `tls_cert_path` set, no ACME
                //   none             — plain-HTTP only (no TLS material)
                // The summary collapses the schema's `acme_provider` +
                // `tls_cert_path` pair into a single human-readable
                // column; the full row is one `vhost show <fqdn>` away.
                println!("{:<40} {:<8} TLS", "FQDN", "Enabled");
                println!("{}", "-".repeat(70));
                for r in records {
                    let fqdn = r.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    let fields = r.get("fields");
                    let enabled = fields
                        .and_then(|f| f.get("enabled"))
                        .and_then(|v| v.as_bool())
                        .map(|b| if b { "yes" } else { "no" })
                        .unwrap_or("?");
                    let acme = fields
                        .and_then(|f| f.get("acme_provider"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty());
                    let manual_cert = fields
                        .and_then(|f| f.get("tls_cert_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .is_some();
                    let tls = match (acme, manual_cert) {
                        (Some(p), _) => format!("acme:{p}"),
                        (None, true) => "manual".to_string(),
                        (None, false) => "none".to_string(),
                    };
                    println!("{fqdn:<40} {enabled:<8} {tls}");
                }
            }
        }
        VhostAction::Show { fqdn } => {
            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("webd", "webd.props.get", &headers, "")
                .await?;
            if rc == 0 {
                let resp: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Vhost {fqdn} not found");
                } else {
                    let msg = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if body_str.is_empty() {
                                "unknown error"
                            } else {
                                &body_str
                            }
                        });
                    return Err(anyhow!("show failed (rc={rc}): {msg}"));
                }
            }
        }
        VhostAction::Remove { fqdn } => {
            // Pre-get to capture the current `version` (or prove
            // absence via `not_found`). See module-level docstring on
            // why the require_version=true delete contract forces
            // this dance.
            let mut get_headers = BTreeMap::new();
            get_headers.insert("namespace".to_string(), NAMESPACE.to_string());
            get_headers.insert("key".to_string(), fqdn.clone());
            let (get_rc, get_body, _get_err) = client
                .call_with_headers_raw("webd", "webd.props.get", &get_headers, "")
                .await?;
            if get_rc != 0 {
                let parsed: serde_json::Value =
                    serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                if code == Some("not_found") {
                    println!("Vhost {fqdn} not found");
                    return Ok(());
                }
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if get_body.is_empty() {
                            "unknown error"
                        } else {
                            &get_body
                        }
                    });
                return Err(anyhow!("remove failed during pre-get (rc={get_rc}): {msg}"));
            }
            let get_resp: serde_json::Value =
                serde_json::from_str(&get_body).unwrap_or(serde_json::Value::Null);
            // Substrate's `Version(pub u64)` (`record.rs:21`) is
            // unsigned; mirror the wire type with `as_u64`.
            let version = get_resp
                .get("version")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    anyhow!("remove failed: pre-get response missing `version` field")
                })?;

            let mut headers = BTreeMap::new();
            headers.insert("namespace".to_string(), NAMESPACE.to_string());
            headers.insert("key".to_string(), fqdn.clone());
            headers.insert("if_version".to_string(), version.to_string());
            let (rc, body_str, _err_hdr) = client
                .call_with_headers_raw("webd", "webd.props.delete", &headers, "")
                .await?;
            if rc == 0 {
                println!("Removed vhost {fqdn}");
            } else {
                let parsed: serde_json::Value =
                    serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
                let code = parsed.get("error_code").and_then(|v| v.as_str());
                let msg = parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        if body_str.is_empty() {
                            "unknown error"
                        } else {
                            &body_str
                        }
                    });
                if code == Some("version_mismatch") {
                    return Err(anyhow!(
                        "remove failed: row changed concurrently between pre-get \
                         and delete ({msg}). Retry the command."
                    ));
                }
                if code == Some("not_found") {
                    // TOCTOU: pre-get saw the row, but it vanished
                    // before delete — treat as idempotent success.
                    println!("Vhost {fqdn} not found");
                    return Ok(());
                }
                return Err(anyhow!("remove failed (rc={rc}): {msg}"));
            }
        }
    }
    Ok(())
}

/// Run a CLI `acme` subcommand as a transient Bus client. Each arm
/// passes `fqdn` as a header kwarg (matching `bus/vhost_verbs.rs`
/// `acme_renew` / `acme_status` signatures). Errors fall back through
/// `error` → `message` → raw body for forward compatibility with
/// either envelope shape.
async fn run_acme_cli(action: AcmeAction) -> Result<()> {
    use std::collections::BTreeMap;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow!(
                "webd daemon not reachable via broker ({e}). \
                 `cosmix-webd acme` requires the daemon to be \
                 running — `systemctl start cosmix-webd` (or equivalent) first."
            )
        })?;

    let (verb, fqdn) = match action {
        AcmeAction::Renew { fqdn } => ("webd.acme.renew", fqdn),
        AcmeAction::Status { fqdn } => ("webd.acme.status", fqdn),
    };
    let mut headers = BTreeMap::new();
    headers.insert("fqdn".to_string(), fqdn.clone());

    let (rc, body_str, _err_hdr) = client
        .call_with_headers_raw("webd", verb, &headers, "")
        .await?;
    if rc == 0 {
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(resp) => {
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            }
            Err(_) => println!("{body_str}"),
        }
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
        let msg = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| {
                if body_str.is_empty() {
                    "unknown error"
                } else {
                    &body_str
                }
            });
        return Err(anyhow!("{verb} failed (rc={rc}): {msg}"));
    }
    Ok(())
}

/// Fire a parameter-less read-only Bus verb as a transient anonymous
/// client and pretty-print the JSON body. Shared by the `routes
/// list`, `stats`, `tls status`, and `autoconfig served-domains` CLI
/// subcommands — every one of those verbs takes no headers and no
/// body and produces a JSON object that's the operator-facing
/// snapshot in its entirety. Error envelope handling falls back
/// through `error` → `message` → raw body for forward compatibility
/// with either substrate or DKIM-style error shapes.
async fn run_readonly_verb_cli(verb: &'static str) -> Result<()> {
    use std::collections::BTreeMap;

    let client = cosmix_config::client_helpers::connect_anonymous_default()
        .await
        .map_err(|e| {
            anyhow!(
                "webd daemon not reachable via broker ({e}). \
                 read-only inspection verbs require the daemon to be \
                 running — `systemctl start cosmix-webd` (or equivalent) first."
            )
        })?;

    let headers: BTreeMap<String, String> = BTreeMap::new();
    let (rc, body_str, _err_hdr) = client
        .call_with_headers_raw("webd", verb, &headers, "")
        .await?;
    if rc == 0 {
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(resp) => {
                let pretty =
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| body_str.clone());
                println!("{pretty}");
            }
            Err(_) => println!("{body_str}"),
        }
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).unwrap_or(serde_json::Value::Null);
        let msg = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| {
                if body_str.is_empty() {
                    "unknown error"
                } else {
                    &body_str
                }
            });
        return Err(anyhow!("{verb} failed (rc={rc}): {msg}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Held for the whole of `main` — must outlive the process (drop
    // flushes). Renamed from `_log` because the Serve branch reads it
    // to attach the live `webd.log` watcher.
    let log_handle = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-webd").with_stats(false),
    )
    .expect("logging init failed");

    let cli = Cli::parse();

    match cli.command {
        Command::Mkcert { fqdn, cert, key } => {
            cosmix_daemon::selfcert::write_self_signed(&fqdn, &cert, &key)?;
        }
        Command::Init { db_path } => {
            let db_path = db_path.unwrap_or_else(|| {
                cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var).join("web.db")
            });
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            init_db(&db_path)?;
        }
        Command::Vhost { action } => {
            run_vhost_cli(action).await?;
        }
        Command::Acme { action } => {
            run_acme_cli(action).await?;
        }
        Command::Routes { action } => {
            let RoutesAction::List = action;
            run_readonly_verb_cli("webd.routes.list").await?;
        }
        Command::Stats => {
            run_readonly_verb_cli("webd.stats").await?;
        }
        Command::Tls { action } => match action {
            TlsAction::Status => {
                run_readonly_verb_cli("webd.tls.status").await?;
            }
            TlsAction::Reload => {
                // `webd.tls.reload` is parameter-less and returns a JSON
                // reload report (or a `{"error": ...}` envelope with
                // rc=10 on a bad cert / non-manual-PEM node). The
                // param-less verb helper applies cleanly — empty body,
                // JSON response, error-envelope handling.
                run_readonly_verb_cli("webd.tls.reload").await?;
            }
        },
        Command::Autoconfig { action } => {
            let AutoconfigAction::ServedDomains = action;
            run_readonly_verb_cli("webd.autoconfig.served_domains").await?;
        }
        Command::Serve {
            listen: cli_listen,
            http_listen: cli_http_listen,
            www_dir: cli_www_dir,
            db_path,
            jmap_upstream: cli_jmap_upstream,
            noded_ws: cli_noded_ws,
            docs_dir,
            static_dir,
            tls_cert: cli_tls_cert,
            tls_key: cli_tls_key,
        } => {
            // DEV MODE: `--static-dir` short-circuits the whole production
            // serve path (no node.conf.mix / DB / broker / ACME / TLS) and
            // serves a plain static folder on a loopback-only listener.
            if let Some(static_dir) = static_dir {
                return run_static_dev_server(static_dir, cli_listen).await;
            }

            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("Failed to install rustls crypto provider");

            let var_dir = cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var);
            let db_path = db_path.unwrap_or_else(|| var_dir.join("web.db"));

            // Resolve config: CLI args override node.conf.mix
            let node_cfg = cosmix_config::node::load_node_config()?;
            let listen = cli_listen
                .or_else(|| node_cfg.as_ref().map(|c| c.web_listen()))
                .unwrap_or_else(|| "0.0.0.0:443".into());
            let www_dir = cli_www_dir.unwrap_or_else(|| {
                node_cfg
                    .as_ref()
                    .map(|c| PathBuf::from(&c.webd.www_dir))
                    .unwrap_or_else(|| var_dir.join("www"))
            });
            let jmap_upstream = cli_jmap_upstream
                .or_else(|| node_cfg.as_ref().map(|c| c.jmap_upstream()))
                .unwrap_or_else(|| "https://127.0.0.1:8443".into());
            let noded_ws = cli_noded_ws
                .or_else(|| node_cfg.as_ref().map(|c| c.noded_url()))
                .unwrap_or_else(|| "ws://192.0.2.5:4200/ws".into());
            let tls_cert = cli_tls_cert.or_else(|| {
                node_cfg
                    .as_ref()
                    .and_then(|c| c.webd.tls_cert.as_ref().map(PathBuf::from))
            });
            let tls_key = cli_tls_key.or_else(|| {
                node_cfg
                    .as_ref()
                    .and_then(|c| c.webd.tls_key.as_ref().map(PathBuf::from))
            });
            // Opt-in only: CLI > node.conf.mix; absent on both = no :80
            // listener (WG-only posture preserved).
            let http_listen = cli_http_listen
                .or_else(|| node_cfg.as_ref().and_then(|c| c.webd.http_listen.clone()));

            // Served-domain allowlist (the autoconfig security gate).
            // Trimmed + lowercased so a Host comparison is exact.
            let served_mail_domains: HashSet<String> = node_cfg
                .as_ref()
                .map(|c| {
                    c.webd
                        .served_mail_domains
                        .iter()
                        .map(|d| d.trim().to_ascii_lowercase())
                        .filter(|d| !d.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            // No allowlist ⇒ autoconfig disabled ⇒ build no resolver
            // and issue no outbound DNS (`feedback_wg_only_binding`).
            let mx = if served_mail_domains.is_empty() {
                None
            } else {
                info!(
                    "mail-client autoconfig enabled for {} domain(s)",
                    served_mail_domains.len()
                );
                Some(mxresolve::MxResolver::new()?)
            };
            // Optional internal mail host for autoconfig (advertise + probe the
            // WG-reachable host instead of the public MX). Trimmed/lowercased;
            // empty → None (legacy public-MX behaviour).
            let autoconfig_mail_host = node_cfg
                .as_ref()
                .and_then(|c| c.webd.autoconfig_mail_host.clone())
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty());

            if let Some(ref d) = docs_dir {
                info!("serving markdown docs from {}", d.display());
            }

            // Resolve vhost rows + legacy collapse into the runtime
            // state. `validate_le_chain` runs here, once per chain, on
            // the startup path — never on the handshake path.
            let webd_cfg_default = cosmix_config::node::WebdConfig::default();
            let webd_cfg = node_cfg
                .as_ref()
                .map(|c| &c.webd)
                .unwrap_or(&webd_cfg_default);
            // Single clock read for the whole startup sequence: the
            // manual-PEM validator (in resolve_node_state), the ACME
            // startup_pass validator (which takes UnixTime), and the
            // OffsetDateTime that drives state-dir stamping all
            // derive from `now_odt`. Two separate `_::now()` calls
            // can land microseconds apart and silently turn an
            // about-to-expire chain into "still valid" on one path
            // and "already expired" on the other — the contract on
            // `startup_pass` already promises one clock; honour it.
            let now_odt = time::OffsetDateTime::now_utc();
            let now_secs = u64::try_from(now_odt.unix_timestamp()).map_err(|_| {
                anyhow!(
                    "[webd] system clock reports {} (pre-Unix-epoch) — refusing \
                     to bootstrap with a clock the rest of the validator stack \
                     cannot model. Set the clock forward via NTP or systemd-\
                     timesyncd and restart.",
                    now_odt.unix_timestamp()
                )
            })?;
            let now = rustls::pki_types::UnixTime::since_unix_epoch(
                std::time::Duration::from_secs(now_secs),
            );
            let inputs = ResolveInputs {
                webd: webd_cfg,
                legacy_tls_cert: tls_cert.clone(),
                legacy_tls_key: tls_key.clone(),
                legacy_www_dir: www_dir.clone(),
                legacy_db_path: db_path.clone(),
                legacy_jmap_upstream: jmap_upstream.clone(),
                legacy_noded_ws: noded_ws.clone(),
                legacy_docs_dir: docs_dir.clone(),
                now,
            };
            let ResolvedNodeState {
                vhosts,
                mut identities,
                acme_plans,
                acme_tos,
                disabled_vhosts,
            } = resolve_node_state(inputs).context("resolving webd vhost configuration")?;

            // B1 fail-soft — the set of every dropped vhost name
            // (primary + aliases). Threaded into `from_namespace_rows`
            // (tolerate the disabled config_bootstrap row), the
            // `AcmeProvisioner` (same, on republish), and
            // `synthesize_listeners` (skip a disabled host named in a
            // listener allowlist). Empty in the all-healthy common case,
            // so the strict pre-B1 behaviour is unchanged.
            let disabled_hosts: HashSet<String> = disabled_vhosts
                .iter()
                .flat_map(|d| d.names.iter().cloned())
                .collect();
            if !disabled_vhosts.is_empty() {
                tracing::warn!(
                    target: "webd::resolve",
                    disabled = disabled_vhosts.len(),
                    hosts = %disabled_vhosts
                        .iter()
                        .map(|d| d.host.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    "webd starting fail-soft: serving the healthy vhost subset; \
                     disabled vhosts stay down until repaired + restart",
                );
            }

            // Shared challenge map: the C4 :80 route reads from
            // here, the C5d1 provisioner writes via
            // `ProvisionerSolver`. Built before `NodeState` so the
            // :80 listener and the provisioner see the same Arc.
            let acme_challenges: Arc<RwLock<HashMap<String, String>>> =
                Arc::new(RwLock::new(HashMap::new()));

            // `webd.tls.status` watch-channel seed — carries the
            // resolved manual-PEM hostnames so manual-PEM-only and
            // HTTP-only deployments still expose the verb (`acme:
            // None`). When ACME exists, the sender is moved into
            // `AcmeProvisioner::new` below; the provisioner
            // overwrites the snapshot once after `startup_pass` then
            // again on every state-mutating branch of `run_forever`.
            // Manual-PEM-only deployments drop the sender at end of
            // scope; receivers keep returning the seed snapshot.
            //
            // Hoisted above the pre-ACME :80 bootstrap `NodeState`
            // (line ~2230) so both that bootstrap node and the main
            // node hold a valid receiver — only the main node is
            // consumed by Bus, but `NodeState::tls_status_rx` is
            // non-`Option` by design (a missing receiver would mean
            // `webd.tls.status` had nothing to return on a manual-
            // PEM-only deployment).
            let (tls_status_tx, tls_status_rx) =
                tokio::sync::watch::channel(tls_status::TlsStatusSnapshot::initial(&identities));
            // The sender has exactly one owner downstream: the ACME
            // provisioner (when ACME plans exist) takes it via
            // `.take()` below; otherwise it stays here and `main` hands
            // it to the B2 `TlsReloadState` so `webd.tls.reload` can
            // republish `tls.status` after a manual swap. The `Option`
            // makes that "moved into the provisioner XOR into the reload
            // state" handoff explicit to the borrow checker.
            let mut tls_status_tx: Option<tokio::sync::watch::Sender<_>> = Some(tls_status_tx);

            // P2-C5d1: if any ACME plan is configured, we MUST
            // bring up the :80 HTTP-01 listener BEFORE running the
            // provisioner's startup pass — order authorisations
            // resolve by HTTP-fetching `/.well-known/acme-challenge/
            // <token>` against the configured FQDN, which only
            // works once :80 is accepting connections backed by
            // the shared challenge map. The resolver in C3 already
            // guarantees `http_listen` is `Some` whenever any ACME
            // plan is present, so the existence check below is a
            // belt-and-braces invariant — a regression would
            // surface as a hard error rather than a silent timeout.
            //
            // The TcpListener::bind is performed *synchronously*
            // before the spawn (Codex C5d1 finding): if the bind
            // fails (port busy, permission denied, address in use)
            // the operator sees a deterministic startup error
            // instead of the ACME authorisation timing out 30s
            // later with no hint that :80 never came up. The
            // spawned task then runs `axum::serve` over the
            // already-listening socket.
            let _http_listen_task: Option<tokio::task::JoinHandle<Result<()>>> =
                if !acme_plans.is_empty() {
                    let http_listen = http_listen.clone().ok_or_else(|| {
                        anyhow!(
                            "[webd] ACME plans resolved but http_listen is unset — \
                         the resolver should have rejected this earlier"
                        )
                    })?;
                    // Build the redirect router against a temporary
                    // `NodeState` whose only populated fields are the
                    // shared challenge map and an empty vhost set —
                    // the :80 router only reads `acme_challenges` and
                    // the Host header. `vhosts` is intentionally empty
                    // here: the host-routed redirect path 301s any
                    // valid Host to `https://<host>/...` and the ACME
                    // challenge path bypasses host routing.
                    //
                    // C3b: an empty `VhostDirectory` matches the prior
                    // empty-HashMap behaviour — `plain_http_host_admit`
                    // sees an empty `admit_plain_http` and rejects every
                    // non-ACME-challenge Host with 400. Exactly what the
                    // pre-ACME bootstrap listener wants.
                    let bootstrap_node = Arc::new(NodeState {
                        service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                        login_throttle: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                        login_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                        vhosts: Arc::new(ArcSwap::from(Arc::new(
                            vhost_directory::VhostDirectory::empty(),
                        ))),
                        http_client: reqwest::Client::builder()
                            .danger_accept_invalid_certs(true)
                            .build()?,
                        // Pre-ACME :80 listener never serves login traffic →
                        // an ephemeral sealer is fine (and avoids touching the
                        // key file before the real node owns it).
                        session: Arc::new(session::SessionSealer::ephemeral()),
                        served_mail_domains: HashSet::new(),
                        autoconfig_mail_host: None,
                        mx: None,
                        acme_challenges: acme_challenges.clone(),
                        tls_status_rx: tls_status_rx.clone(),
                        // Bootstrap node serves only the pre-ACME HTTP-01
                        // redirect listener; the Bus loop is not spawned
                        // against it. An empty `PropsRouter` (no namespace
                        // registered) suffices — `webd.props.*` against
                        // this node would return `not_found` rather than
                        // dispatching, but no caller ever reaches it.
                        props_router: Arc::new(cosmix_props::PropsRouter::new("webd")),
                        props_subscribe_granter: Arc::new(
                            bus::subscribe_granter::NodedSubscribeGranter::new(
                                bus::subscribe_granter::new_broker_handle(),
                            ),
                        ),
                        // Bootstrap node never reaches `bus::run`, so the
                        // broker handle stays unattached — publishes
                        // against it would no-op even if a dispatcher
                        // were spawned (none is).
                        broker_handle: bus::subscribe_granter::new_broker_handle(),
                        // Bootstrap node serves only the pre-ACME HTTP-01
                        // redirect; C5 verbs (which need the runtime,
                        // the lock map, and the notify) never dispatch
                        // here. Leave all three `None` — C5 returns
                        // `rc=10` instead of panicking.
                        vhosts_runtime: None,
                        listeners_runtime: None,
                        listeners_operators: Vec::new(),
                        vhost_key_locks: None,
                        acme_notify: None,
                        acme_force_renew_queue: None,
                        // Bootstrap node serves only the pre-ACME redirect
                        // listener — no embedded handlers.
                        handlers: Arc::new(ArcSwap::from(Arc::new(
                            mix_handler::HandlerTable::default(),
                        ))),
                        handler_ast_cache: mix_handler::new_ast_cache(),
                        // Pre-ACME bootstrap node serves no TLS — no reload.
                        tls_reload: None,
                    });
                    let redirect = build_http_redirect_router(bootstrap_node);
                    let listener = tokio::net::TcpListener::bind(&http_listen)
                        .await
                        .with_context(|| {
                            format!(
                                "binding pre-ACME HTTP-01 listener on {http_listen} \
                             — refusing to enter the ACME order phase without :80"
                            )
                        })?;
                    info!("cosmix-web listening on {http_listen} (HTTP redirect (pre-ACME))");
                    Some(tokio::spawn(async move {
                        axum::serve(listener, redirect).await?;
                        Ok(())
                    }))
                } else {
                    None
                };

            // Run the provisioner's startup pass (reconcile +
            // classify + issue + validate + promote) before any
            // :443 listener exists. Identities issued here are
            // spliced into the runtime resolver below; a fresh-
            // vhost order failure is a hard startup error.
            //
            // Provisioner survives past the startup pass into
            // `acme_provisioner_opt` so the `run_forever` supervision
            // loop can be spawned after the per-listener `ListenerTls`
            // handles are built — they are attached via
            // `attach_tls_listeners` so each successful renewal can
            // `swap` a freshly-built `SniCertResolver` into the right
            // listener without taking the accept loop down.
            let mut acme_provisioner_opt: Option<acme_provisioner::AcmeProvisioner> = None;
            if !acme_plans.is_empty() {
                let acme_dir = cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
                    .join("webd")
                    .join("acme");
                std::fs::create_dir_all(&acme_dir)
                    .with_context(|| format!("creating ACME state root {}", acme_dir.display()))?;
                let mut provisioner = acme_provisioner::AcmeProvisioner::new(
                    acme_dir,
                    acme_plans,
                    acme_tos,
                    acme_challenges.clone(),
                    // `base_identities` — the manual-PEM identities
                    // resolved so far (legacy top-level pair + any
                    // per-vhost `tls_cert` / `tls_key` rows). The
                    // provisioner clones this floor into every
                    // rebuilt `ServerConfig`, so manual-PEM SNI keeps
                    // resolving after an ACME-only renewal swap.
                    identities.clone(),
                    acme_provisioner::DEFAULT_RENEWAL_TICK,
                    // Move the sender into the provisioner; the no-ACME
                    // path keeps it (in the Option) for the reload state.
                    tls_status_tx.take(),
                )
                .context("constructing AcmeProvisioner")?;
                // 6h default matches the renewal tick — see
                // `acme_provisioner::reconcile_on_startup` rationale.
                // `now_odt` / `now` are the single-source clock read
                // from above (`let now_odt = …` earlier in this fn).
                let acme_identities = provisioner
                    .startup_pass(now_odt, now, std::time::Duration::from_secs(6 * 60 * 60))
                    .await
                    .context("ACME startup pass failed — refusing to bind TLS listener")?;
                identities.extend(acme_identities);
                // Initial publish covers the carrying-servable and
                // freshly-issued paths inside `startup_pass`; the
                // run-loop publishes thereafter at every mutation.
                provisioner.publish_tls_status();
                acme_provisioner_opt = Some(provisioner);
            }

            let tls_pair_present = !identities.is_empty();

            // SPEC-12 vhosts namespace (Phase 3). Declarative
            // source-of-truth, persisted in `web.db` alongside the CMS
            // posts table. A *second* `rusqlite::Connection` (matching
            // the maild two-connection pattern) is opened here for the
            // substrate — the legacy CMS connection lives elsewhere
            // and is unaffected. PRAGMAs match the substrate's
            // expectations: WAL journal mode for concurrent readers,
            // `busy_timeout=5000` to absorb the brief contention
            // window from the dispatcher fan-out task, and
            // `foreign_keys=ON` (defensive — substrate tables don't
            // declare FKs today but the bootstrap-upsert C3 path
            // expects them honoured).
            //
            // C1 lands the register call only; no row seeding —
            // `webd.props.list namespace=vhosts` returns empty until
            // C3 wires the bootstrap-upsert path from `vhosts`. The
            // ergonomic `webd.routes.list` continues to project the
            // runtime vhost map verbatim.
            let props_conn = rusqlite::Connection::open(&db_path).with_context(|| {
                format!(
                    "opening substrate sqlite connection on {} for webd.vhosts \
                     namespace",
                    db_path.display(),
                )
            })?;
            props_conn
                .execute_batch(
                    "PRAGMA journal_mode=WAL; \
                     PRAGMA foreign_keys=ON; \
                     PRAGMA busy_timeout=5000;",
                )
                .context("applying PRAGMAs to webd substrate connection")?;
            let props_store = Arc::new(
                cosmix_props::sqlite::SqliteStore::new("webd", props_conn)
                    .context("constructing webd SqliteStore")?,
            );
            let mut props_router_inner = cosmix_props::PropsRouter::new("webd");
            // C4a — provisioner-event receiver. On nodes that boot
            // *with* an ACME plan (`acme_provisioner_opt.is_some()`),
            // the receiver is taken below and handed to
            // `AcmeProvisioner::attach_ns_events` so the
            // `run_forever` select! arm consumes `VhostSet` events
            // for fresh issuance. On nodes that boot *without* an
            // ACME plan (manual-PEM only, or no-TLS), the receiver
            // stays parked here for the daemon lifetime — the hooks
            // still fire `try_send` and would return `Closed` if we
            // dropped the receiver. Keeping it bound makes
            // `try_send` return `Full` (never `Closed`) under normal
            // operation; events accumulate up to
            // `PROVISIONER_EVENT_CHANNEL_CAPACITY` and then get
            // dropped + WARN-logged.
            //
            // No-ACME limitation: a no-ACME boot cannot process
            // runtime ACME adds (an operator setting `acme_*` on a
            // vhost via `props.set` or `vhost.add` while the daemon
            // runs without any pre-existing ACME plan). The
            // provisioner only exists when
            // `acme_plans.is_empty() == false` at construction; we
            // don't construct it lazily because the LE account
            // credentials and ToS proof are read at construction
            // and the no-ACME path never produces them.
            //
            // Codex C4a rev-3 MINOR fix: the acceptable recovery
            // shape is operator-side and specifically scoped:
            // restart webd after adding the **first** ACME row to
            // `node.conf.mix`'s `acme_plans` array. A namespace-only
            // add (via `props.set` or the C5 `vhost.add` verb)
            // does **not** populate `node.conf.mix` and therefore does
            // **not** flip the boot-time `acme_provisioner_opt`
            // gate. Closing that loop — provisioner construction
            // observing namespace-resident ACME rows so a node
            // can graduate from no-ACME to ACME without a
            // `node.conf.mix` edit — is a later-phase substrate fix,
            // not C4a's scope.
            let (vhosts_runtime, vhosts_provisioner_events_rx) =
                vhosts_namespace::register_vhosts_namespace(&mut props_router_inner, &props_store)
                    .context("registering webd.vhosts substrate namespace")?;

            // Slice #3 — register the `webd.handlers` namespace on the
            // same router + store (must happen before the router is
            // frozen into the `Arc` below). `handlers_reload` is the
            // dedicated `Notify` the hooks fire on every handler
            // change; the rebuild task spawned after `NodeState`
            // construction waits on it. (A dedicated Notify rather than
            // the runtime's `events_signal` — the latter is consumed by
            // the props fan-out dispatcher and `notify_one` wakes only
            // one waiter.)
            let (handlers_runtime, handlers_reload) =
                handlers_namespace::register_handlers_namespace(
                    &mut props_router_inner,
                    &props_store,
                )
                .context("registering webd.handlers substrate namespace")?;

            // P3 — register the `webd.listeners` kill-switch + guard
            // namespace (before the router freeze, like the others).
            // The L0 `[webd.listeners] operators` allowlist seeds the
            // operator-tier write AuthPolicy; read/describe stay open.
            // The returned events_rx is moved into the reaction loop
            // (spawned after the listener set is built).
            let listeners_operators: Vec<String> = webd_cfg.listeners.operators.clone();
            let (listeners_runtime, listeners_events_rx) =
                listeners_namespace::register_listeners_namespace(
                    &mut props_router_inner,
                    &props_store,
                    listeners_operators.clone(),
                )
                .context("registering webd.listeners substrate namespace")?;

            // SPEC 12 reserved `webd.log` namespace — live `EnvFilter`
            // swap driven by `props.set webd.log { level: "debug" }`.
            // Registered before the router freeze, like the others; the
            // returned runtime is handed to `cosmix_log_props::attach_props`
            // once serving begins (the watcher spawns a task on the live
            // tokio runtime).
            let log_runtime =
                cosmix_log_props::register_log_namespace(&mut props_router_inner, &props_store)
                    .context("registering webd.log substrate namespace")?;

            // C3d — materialise the `[[webd.vhost]]` config block into
            // the `webd.vhosts` substrate namespace. Runs *after*
            // namespace registration (the hook must be live so the
            // backend-origin writes flow through `before_set`
            // validation) and *before* the initial directory publish
            // below — so the C3e directory build sourced from
            // namespace state observes the post-bootstrap rows.
            //
            // The plan's nominal 5-step startup ordering (Phase 2
            // crash recovery → bootstrap → C4b orphan archive →
            // directory publish from namespace → first provisioner
            // tick) is partially realised at this commit: step 1
            // happens inside `acme_provisioner.startup_pass()` above
            // (which also classifies/issues/promotes from
            // `acme_plans`, not just crash-recovers), step 3 is C4b
            // territory and the helper does not exist yet. Step 4 is
            // implemented below via `snapshot_rows` +
            // `from_namespace_rows` — the directory is now sourced
            // from the post-bootstrap namespace state, so
            // `bus_runtime` rows survive a daemon restart with no
            // matching `[[webd.vhost]]` config block.
            //
            // `ts_ms`: single millisecond stamp threaded into every
            // substrate write in this bootstrap pass, derived from
            // the same `now_odt` clock read the validator stack uses
            // above so the substrate's audit log can be cross-
            // correlated with the ACME startup pass.
            let bootstrap_ts_ms: i64 = i64::try_from(now_odt.unix_timestamp_nanos() / 1_000_000)
                .map_err(|_| {
                    anyhow!(
                        "[webd] system clock millisecond stamp overflows i64 \
                         (unix_timestamp_nanos / 1_000_000 out of range) — \
                         refusing to bootstrap webd.vhosts namespace"
                    )
                })?;
            // One-shot AMP→Bus rename migrations for durable rows a
            // pre-rename daemon wrote: `webd.vhosts` rows stamped
            // `source = "amp_runtime"` and `webd.handlers` capability
            // grants spelled `amp:`/`amp-svc:`. Must run before the
            // bootstrap upsert / directory build / handler-table build
            // — the old spellings now fail hook validation and are
            // invisible to the renamed readers. No-ops on a clean
            // store.
            let migrated_vhosts =
                vhosts_bootstrap::migrate_amp_runtime_rows(&vhosts_runtime, bootstrap_ts_ms)
                    .await
                    .context("[webd] amp→bus migration of webd.vhosts rows")?;
            let migrated_handlers =
                handlers_namespace::migrate_amp_capability_rows(&handlers_runtime, bootstrap_ts_ms)
                    .await
                    .context("[webd] amp→bus migration of webd.handlers rows")?;
            if migrated_vhosts + migrated_handlers > 0 {
                tracing::info!(
                    target: "webd::main",
                    migrated_vhosts,
                    migrated_handlers,
                    "amp→bus rename migration rewrote legacy durable rows",
                );
            }
            vhosts_bootstrap::bootstrap_upsert_from_config(
                &vhosts_runtime,
                webd_cfg,
                bootstrap_ts_ms,
            )
            .await
            .context("[webd] vhosts namespace bootstrap from [[webd.vhost]] config block")?;
            // Shared broker handle: built empty, refreshed by
            // `bus::run` on every successful connect, read by the
            // granter on every `grant()` and by the publisher on
            // every `publish()`. The `ArcSwapOption` shape means
            // reconnect-aware watch + publish without re-spawning
            // dispatchers — see [`NodeState::broker_handle`] for the
            // full rationale.
            let broker_handle = bus::subscribe_granter::new_broker_handle();
            let props_subscribe_granter = Arc::new(
                bus::subscribe_granter::NodedSubscribeGranter::new(broker_handle.clone()),
            );
            props_router_inner.set_granter(props_subscribe_granter.clone());
            let props_router = Arc::new(props_router_inner);

            // C3e: build the initial host-routing directory from the
            // post-bootstrap `webd.vhosts` namespace snapshot. The
            // resolved runtime map (`vhosts`) is still threaded in to
            // recover per-vhost runtime wiring — CMS db, JMAP
            // upstream, noded WS, docs dir, alias grouping — for
            // primaries that came in via the `[[webd.vhost]]` config
            // block. Rows that exist only in the namespace
            // (`source = "bus_runtime"`) get a minimal `VhostState`
            // synthesised from the row's `fqdn`/`www_dir`/`aliases`
            // alone, which is what lets `bus_runtime`-origin vhosts
            // survive a restart with no matching config block.
            let vhosts_namespace_rows = vhosts_namespace::snapshot_rows(&vhosts_runtime)
                .await
                .context("snapshotting webd.vhosts namespace for initial directory build")?;
            // C4b — startup orphan-scan. Spec startup order
            // (`_doc/planned/webd-vhosts-phase3.md` § "Startup
            // ordering"):
            //   1. Phase 2 per-fqdn crash recovery (`startup_pass`)
            //   2. C3 bootstrap upsert (`bootstrap_upsert_from_config`)
            //   3. **C4b whole-set orphan archive** (HERE) — derives
            //      `known_fqdns` from the POST-BOOTSTRAP namespace
            //      snapshot so runtime `bus_runtime` rows that have
            //      no matching entry in `acme_plans` are preserved.
            //   4. Initial directory publish (below)
            //   5. First provisioner tick (run-loop)
            //
            // Codex C4b rev-1 BLOCKER fix: prior to this revision the
            // scan ran BEFORE bootstrap and used `acme_plans` for
            // `known_fqdns`, which would archive the on-disk acme
            // dir of any runtime-added (namespace-only) ACME row on
            // every restart.
            //
            // Fail-soft + no-acme-safe: a non-existent `acme_dir`
            // returns Ok(0); the scan therefore runs unconditionally
            // (no `if !acme_plans.is_empty()` gate) and the helper
            // short-circuits when no on-disk state exists.
            {
                let acme_dir = cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
                    .join("webd")
                    .join("acme");
                let known_fqdns: std::collections::HashSet<String> = vhosts_namespace_rows
                    .iter()
                    .map(|r| r.fqdn.clone())
                    .collect();
                match acme_provisioner::archive_orphan_acme_dirs(&acme_dir, &known_fqdns, now_odt) {
                    Ok(n) if n > 0 => tracing::info!(
                        archived = n,
                        "ACME startup orphan-scan: archived unknown fqdn directories"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        error = %e,
                        "ACME startup orphan-scan failed — bind proceeds, orphans \
                         remain on disk for the next restart to retry"
                    ),
                }
            }
            let initial_directory = vhost_directory::from_namespace_rows(
                &vhosts_namespace_rows,
                &vhosts,
                &disabled_hosts,
            )
            .context("building initial VhostDirectory from post-bootstrap webd.vhosts namespace")?;
            let vhost_directory_handle: Arc<ArcSwap<vhost_directory::VhostDirectory>> =
                Arc::new(ArcSwap::from(Arc::new(initial_directory)));

            // P2-C2: resolve the per-interface listener set. Sourced
            // from the *namespace-backed* directory (`by_host` keys),
            // NOT the config-only `vhosts` map — the namespace is the
            // declarative source of truth and may carry hosts the raw
            // `[[webd.vhost]]` rows don't (a runtime `vhost.add`
            // persisted in a prior session), and `NodeState::vhosts`
            // serves exactly this set. Using the config-only map would
            // leave such a host served by `host_router` but absent
            // from every listener allowlist (a 404) and unmapped for
            // ACME renewal.
            //
            // An explicit `[[webd.listener]]` array is validated by
            // `synthesize_listeners` (unique ids/binds, every served
            // host on exactly one ENABLED listener, no
            // wildcard+specific clash) — so a namespace host missing
            // from the listener allowlists fails loudly at startup
            // rather than silently 404ing. An empty array — or a
            // pure-CLI boot with no node config — collapses to a
            // single implicit `wg` listener at the resolved `listen`
            // address serving every host, byte-identical to the pre-P2
            // single-bind behaviour and still honouring a `--listen`
            // CLI override.
            let has_explicit_listeners = node_cfg
                .as_ref()
                .is_some_and(|c| !c.webd.listener.is_empty());
            let all_hosts: Vec<String> = vhost_directory_handle
                .load()
                .by_host
                .keys()
                .cloned()
                .collect();
            let resolved_listeners: Vec<ResolvedWebdListener> = if has_explicit_listeners {
                node_cfg
                    .as_ref()
                    .expect("has_explicit_listeners implies node_cfg is Some")
                    .synthesize_listeners(&all_hosts, &disabled_hosts)
                    .context("resolving [[webd.listener]] array")?
            } else {
                vec![ResolvedWebdListener {
                    id: "wg".to_string(),
                    bind: listen.clone(),
                    external: false,
                    enabled: true,
                    hosts: all_hosts.clone(),
                }]
            };

            // P3 — seed the `webd.listeners` namespace from the
            // resolved set (upsert-if-absent: config seeds `enabled` +
            // the daemon-owned `external` flag once; thereafter L1
            // wins), then snapshot it. The snapshot is L1-authoritative
            // for each listener's `enabled` + guard policy below, so a
            // listener an operator killed in a prior run stays killed
            // across this restart. Runs after `resolved_listeners` (it
            // needs the ids/binds/external) and before the ListenerSet
            // build (which reads the snapshot).
            listeners_bootstrap::bootstrap_upsert_from_config(
                &listeners_runtime,
                &resolved_listeners,
                bootstrap_ts_ms,
            )
            .await
            .context("[webd] listeners namespace bootstrap from [[webd.listener]] config")?;
            let listener_rows: HashMap<String, listeners_namespace::ListenerRow> =
                listeners_namespace::snapshot_rows(&listeners_runtime)
                    .await
                    .context("snapshotting webd.listeners namespace for the listener set")?
                    .into_iter()
                    .map(|r| (r.id.clone(), r))
                    .collect();

            // Map every served host to its owning listener — the
            // partition key for both the startup resolver split and
            // the provisioner's per-listener renewal republish.
            let fqdn_to_listener: HashMap<String, String> = resolved_listeners
                .iter()
                .flat_map(|l| l.hosts.iter().map(move |h| (h.clone(), l.id.clone())))
                .collect();

            // P2-C2: split the validated identity list into
            // per-listener buckets and build one `ListenerTls` per
            // cert-bearing listener. Each bucket's `SniCertResolver`
            // carries only that listener's vhosts' certs, so a
            // foreign-SNI handshake on the public interface can never
            // be served a mesh vhost's cert (the handshake half of the
            // per-interface isolation). A listener with no certs
            // (all-plain vhosts) gets no `ListenerTls` and binds plain
            // HTTP — the same fallthrough the old single `serve_plain`
            // primary used. The `ListenerTls` handles are cloned into
            // both the `ListenerSet` (the accept loop) and the ACME
            // provisioner (the renewal swap); cloning shares the inner
            // resolver slot, so a renewal swap is seen by the live
            // accept loop.
            let mut listener_buckets =
                partition_identities_by_listener(&identities, &resolved_listeners);
            let mut tls_listeners: HashMap<String, ListenerTls> = HashMap::new();
            for l in &resolved_listeners {
                let bucket = listener_buckets.remove(&l.id).unwrap_or_default();
                if bucket.is_empty() {
                    continue;
                }
                // strict_sni is L1-owned (the `webd.listeners` row),
                // seeded from config + tunable at runtime; a strict
                // resolver rejects no-SNI / unknown-SNI handshakes at
                // the TLS layer (the handshake half of public-listener
                // hardening). Renewals rebuild the resolver via the
                // provisioner; a later strict_sni flip applies on the
                // next renewal (documented).
                let strict_sni = listener_rows.get(&l.id).is_some_and(|r| r.strict_sni);
                let resolver = SniCertResolver::from_config(&bucket, strict_sni)
                    .with_context(|| format!("building TLS resolver for listener {:?}", l.id))?;
                tls_listeners.insert(l.id.clone(), ListenerTls::new(Some(Arc::new(resolver))));
            }

            // C5 — capture the ACME provisioner's notify-into-sweep
            // handle BEFORE the match arm below consumes
            // `acme_provisioner_opt` into the tokio task. The
            // `webd.acme.renew` verb's `notify_one` runs against this
            // handle to bypass the renewal-window gate. `None` on
            // no-ACME boots (no provisioner constructed) — the verb
            // returns `rc=10 no ACME provisioner attached` in that
            // case.
            let acme_notify_handle: Option<Arc<tokio::sync::Notify>> =
                acme_provisioner_opt.as_ref().map(|p| p.notify_handle());
            // C5 BLOCKER 1 fix — capture the force-renew queue handle
            // BEFORE `tokio::spawn` below consumes the provisioner.
            // The verb cannot reach the queue any other way once the
            // provisioner has moved into the runtime.
            let acme_force_renew_handle: Option<
                Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
            > = acme_provisioner_opt
                .as_ref()
                .map(|p| p.force_renew_handle());
            // C4b — shared per-FQDN coordination lock map, lifted to
            // before NodeState construction so the C5 ergonomic verbs
            // can acquire the same lock identity the provisioner's
            // `VhostRemoved` arm holds. Constructed unconditionally
            // so the C5 verbs work even on no-ACME boots (they still
            // need to serialise operator-driven add/remove). The
            // match arm below clones this same handle into the
            // provisioner via `attach_key_locks`.
            let webd_key_locks: acme_provisioner::FqdnLockMap =
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

            // Slice #3 — build the initial embedded-Mix handler table
            // from the persisted `webd.handlers` namespace (empty until
            // an operator adds rows). Hot-swapped by the reload task
            // spawned just after `NodeState`.
            let initial_handler_table = mix_handler::HandlerTable::from_rows(
                handlers_namespace::snapshot_rows(&handlers_runtime)
                    .await
                    .context("snapshotting webd.handlers namespace for initial route table")?,
            );
            let handlers: Arc<ArcSwap<mix_handler::HandlerTable>> =
                Arc::new(ArcSwap::from(Arc::new(initial_handler_table)));
            let handler_ast_cache = mix_handler::new_ast_cache();

            // B2 — build the `webd.tls.reload` surface. Only a
            // manual-PEM-capable node gets one: no ACME provisioner (it
            // would own the per-listener resolvers and re-merge ACME on
            // every republish, so an independent manual swap would race
            // it and drop ACME certs — those nodes use `webd.acme.renew`)
            // AND at least one cert-bearing listener. On such a node the
            // `tls_status_tx` was never moved into a provisioner, so it
            // is still `Some` here and the reload can republish status.
            // `identities` is the pure manual-PEM set on a no-ACME node
            // (the ACME-extend at `startup_pass` only runs when a
            // provisioner exists).
            let tls_reload = if acme_provisioner_opt.is_none() && !tls_listeners.is_empty() {
                Some(bus::tls::TlsReloadState::new(
                    tls_listeners.clone(),
                    identities.clone(),
                    resolved_listeners.clone(),
                    // Live strict_sni source — the reload snapshots this
                    // at call time rather than freezing the startup value.
                    listeners_runtime.clone(),
                    tls_status_tx.take(),
                ))
            } else {
                None
            };

            // SSR PIM Phase 2 — load (or first-run generate) the per-node
            // session sealing key. A wrong-length/unreadable key file is a
            // hard startup error (fail loud), not a silent regen.
            let session = Arc::new(
                session::SessionSealer::load_or_generate(&session_key_path())
                    .context("loading webd session sealing key")?,
            );
            let node = Arc::new(NodeState {
                service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                login_throttle: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                login_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                vhosts: vhost_directory_handle,
                http_client: reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()?,
                session,
                served_mail_domains,
                autoconfig_mail_host,
                mx,
                acme_challenges,
                tls_status_rx,
                props_router,
                props_subscribe_granter,
                broker_handle,
                vhosts_runtime: Some(vhosts_runtime.clone()),
                listeners_runtime: Some(listeners_runtime.clone()),
                listeners_operators: listeners_operators.clone(),
                vhost_key_locks: Some(webd_key_locks.clone()),
                acme_notify: acme_notify_handle,
                acme_force_renew_queue: acme_force_renew_handle,
                handlers,
                handler_ast_cache,
                tls_reload,
            });

            // Slice #3 — webd.handlers reload task. The namespace hooks
            // fire `handlers_reload.notify_one()` on every set/delete;
            // rebuild the compiled table from a fresh snapshot and
            // hot-swap it. `Notify` is coalescing and the namespace is
            // the durable source of truth, so a missed wakeup is
            // impossible as long as we loop straight back to
            // `notified()`. A snapshot failure keeps the previous table.
            {
                let table_handle = node.handlers.clone();
                let runtime = handlers_runtime.clone();
                let reload = handlers_reload.clone();
                tokio::spawn(async move {
                    loop {
                        reload.notified().await;
                        match handlers_namespace::snapshot_rows(&runtime).await {
                            Ok(rows) => {
                                let table = mix_handler::HandlerTable::from_rows(rows);
                                tracing::info!(
                                    target: "webd::mix_handler",
                                    routes = table.route_count(),
                                    "rebuilt webd.handlers route table",
                                );
                                table_handle.store(Arc::new(table));
                            }
                            Err(e) => tracing::warn!(
                                target: "webd::mix_handler",
                                error = %e,
                                "webd.handlers reload: snapshot failed; keeping previous table",
                            ),
                        }
                    }
                });
            }

            // P2-C5d2: spawn the provisioner's renewal supervision
            // loop. Only spawns when both an ACME provisioner exists
            // *and* a `TlsConfigHandle` exists — without the handle
            // there's nothing to publish into, and without an ACME
            // plan there's nothing to renew.
            //
            // The provisioner is moved into the tokio task; the task
            // outlives the request lifecycle. `run_forever` never
            // returns `Ok(())` — renewal failures are swallowed into
            // backoff state — so a `JoinError` from the task is the
            // only "loop died" signal we can observe. The task
            // handle joins alongside the listener so a panic surfaces
            // as a top-level error rather than silently dropping the
            // renewal loop while the daemon keeps serving stale
            // certs.
            // C4a: pre-bind the namespace event receiver as
            // `Option<_>` so the no-ACME branch (no provisioner,
            // no spawn) can leave it `Some(_)` for the parked-
            // receiver binding below. We use `Option::take` so the
            // match arm consumes it only on the ACME-plan path.
            //
            // Codex C4a review BLOCKER fix (rev 2): the parked
            // binding MUST live at the outer (main) scope — not
            // inside a nested `let acme_renewal_task = { … }`
            // block — otherwise it drops at end of block and the
            // channel closes before serving. On no-ACME boots the
            // hook would then see `Closed` instead of `Full` and
            // the after_set/after_delete sites would log a hard
            // failure on every namespace mutation.
            let mut vhosts_provisioner_events_rx_opt = Some(vhosts_provisioner_events_rx);
            // C4b/C5 — `webd_key_locks` is constructed above (before
            // NodeState) so the C5 ergonomic verbs can share the same
            // lock map. The match arm here just clones it into the
            // provisioner via `attach_key_locks`.
            let acme_renewal_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> =
                match acme_provisioner_opt {
                    // An ACME plan always issues at least one cert at
                    // startup_pass, so a provisioner implies a
                    // cert-bearing (Terminate) listener — i.e.
                    // `tls_listeners` is non-empty. The guard keeps the
                    // old "no TLS surface ⇒ don't spawn renewal" shape.
                    Some(mut provisioner) if !tls_listeners.is_empty() => {
                        provisioner
                            .attach_tls_listeners(tls_listeners.clone(), fqdn_to_listener.clone());
                        let events_rx = vhosts_provisioner_events_rx_opt
                            .take()
                            .expect("vhosts_provisioner_events_rx_opt is Some on first match arm");
                        provisioner.attach_ns_events(events_rx, vhosts_runtime.clone());
                        // C4b — wire the arc-swap + shared lock map so
                        // `VhostRemoved` cleanup can publish before
                        // mutating and so C5 `vhost.add` can serialise
                        // with the cleanup arm.
                        provisioner.attach_node_vhosts(
                            node.vhosts.clone(),
                            vhosts.clone(),
                            disabled_hosts.clone(),
                        );
                        provisioner.attach_key_locks(webd_key_locks.clone());
                        Some(tokio::spawn(async move { provisioner.run_forever().await }))
                    }
                    _ => None,
                };
            // Park the receiver for the daemon lifetime when the
            // ACME branch didn't take it (no-ACME boot). The hooks'
            // `try_send` then returns `Full` (never `Closed`) when
            // the channel saturates. Outer-scope binding is load-
            // bearing — see the comment above.
            let _vhosts_provisioner_events_rx_parked = vhosts_provisioner_events_rx_opt;

            let app = build_router(node.clone());

            // Bus citizen surface — fire-and-forget background task.
            // `bus::run` never returns (retry-with-backoff covers both
            // initial connect failure and mid-life disconnect). The
            // handle is intentionally dropped: tokio keeps the task
            // alive for the daemon lifetime and process exit reclaims
            // it. Per the plan's goal-(c)-equivalent invariant: Bus
            // unavailability never blocks request serving or ACME
            // renewal, which run as sibling futures via `try_join!`
            // below.
            tokio::spawn(bus::run(node.clone()));

            // Live `webd.log` watcher — swaps the EnvFilter on every
            // `props.set webd.log {...}`. Spawns a task on the live
            // tokio runtime; `log_handle` was created in `main`.
            cosmix_log_props::attach_props(&log_handle, log_runtime)
                .await
                .context("attaching webd.log live-reload watcher")?;

            // P2-C2: build the per-interface listener set. One
            // `ListenerSpec` per resolved listener; each gets its own
            // `app` clone layered with a `ListenerScope` (the vhost
            // allowlist `host_router` enforces) wrapped in a
            // `WebdConnHandler` for the post-accept hyper serve. A
            // cert-bearing listener runs `TlsMode::Terminate` with its
            // `ListenerTls` (shared with the provisioner so renewals
            // hot-swap the live resolver); a plain listener binds HTTP
            // — the same fallthrough the old single `serve_plain`
            // primary used. `start_all` binds every enabled listener
            // and returns; the accept loops live on as tasks the set
            // owns, so the set must stay in scope for the daemon
            // lifetime (dropping it stops them).
            let mut listener_builder = ListenerSet::builder();
            for l in &resolved_listeners {
                let bind: SocketAddr = l.bind.parse().with_context(|| {
                    format!(
                        "listener {:?}: bind {:?} is not a valid ip:port",
                        l.id, l.bind
                    )
                })?;
                // Only an *explicit* listener array gets a per-listener
                // allowlist. The implicit single `wg` listener layers
                // no scope so `host_router` serves every registered
                // vhost — including ones a runtime `vhost.add` stores
                // into `NodeState::vhosts` after startup, which a
                // static startup allowlist could never name. (A
                // multi-listener node, by contrast, partitions hosts to
                // interfaces by config; a runtime add there has no
                // listener assignment until config names it — an
                // accepted limitation of the explicit-partition mode.)
                let app_for_listener = if has_explicit_listeners {
                    let scope = ListenerScope {
                        id: Arc::from(l.id.as_str()),
                        allowed_hosts: Arc::new(l.hosts.iter().cloned().collect()),
                        external: l.external,
                    };
                    app.clone().layer(Extension(scope))
                } else {
                    app.clone()
                };
                let handler = Arc::new(WebdConnHandler {
                    app: app_for_listener,
                });
                let tls = tls_listeners.get(&l.id).cloned();
                let tls_mode = if tls.is_some() {
                    TlsMode::Terminate
                } else {
                    TlsMode::Plain
                };
                // L1-authoritative: `enabled` + the guard policy come
                // from the `webd.listeners` row (config-seeded, then
                // operator-owned), NOT raw config — so a listener an
                // operator killed in a prior run stays unbound across
                // this restart, and its guards are in force from the
                // first connection. The reaction loop hot-swaps both at
                // runtime.
                let row = listener_rows.get(&l.id);
                let enabled = row.map(|r| r.enabled).unwrap_or(l.enabled);
                let guard = row
                    .map(listeners_reaction::guard_from_row)
                    .unwrap_or_default();
                let spec = ListenerSpec::new(l.id.clone(), vec![bind])
                    .with_external(l.external)
                    .with_enabled(enabled)
                    .with_tls_mode(tls_mode)
                    .with_guard(guard);
                listener_builder.add(spec, handler, tls);
            }
            let listener_set = listener_builder.build();
            listener_set
                .start_all()
                .await
                .context("starting webd listener set")?;

            // `start_all` is best-effort (it only errors when *every*
            // enabled listener fails to bind), so a partial bind — e.g.
            // a public listener whose port is already taken while the
            // WG listener comes up — would otherwise leave webd
            // silently half-serving. Preserve the old single-bind
            // hard-fail: every listener that config says is enabled
            // MUST be bound, or refuse to start so the operator sees
            // the failure. (The Phase-3 kill switch toggles listeners
            // at runtime through a different path; this is the startup
            // contract.)
            {
                let running: HashSet<String> = listener_set
                    .control()
                    .status()
                    .into_iter()
                    .filter(|s| s.running)
                    .map(|s| s.id)
                    .collect();
                // "enabled" here is the L1 truth (a listener killed in
                // a prior run is legitimately not bound — not a failure).
                let mut down: Vec<&str> = resolved_listeners
                    .iter()
                    .filter(|l| {
                        let enabled = listener_rows
                            .get(&l.id)
                            .map(|r| r.enabled)
                            .unwrap_or(l.enabled);
                        enabled && !running.contains(&l.id)
                    })
                    .map(|l| l.id.as_str())
                    .collect();
                if !down.is_empty() {
                    down.sort_unstable();
                    anyhow::bail!(
                        "webd: enabled listener(s) failed to bind: {} — refusing to \
                         start partially (check the bind addresses are free and the \
                         interfaces are up)",
                        down.join(", ")
                    );
                }
            }

            // P3 — spawn the listeners reaction loop (daemon lifetime,
            // fire-and-forget). It owns a `ListenerSetControl` clone +
            // the namespace events receiver, applies `enabled`/guard
            // changes to the live set, and writes back observed state.
            // Not joined into `try_join!` — a writeback error must never
            // tear down serving (same posture as the Bus citizen task).
            tokio::spawn(listeners_reaction::run(
                listener_set.control(),
                listeners_runtime.clone(),
                listeners_events_rx,
            ));

            // Renewal task wrapper that flattens
            // `Result<Result<(), anyhow::Error>, JoinError>` into
            // `Result<(), anyhow::Error>` so `try_join!` can mix it
            // alongside the listener futures. `None` collapses to
            // an immediate `Ok(())` so a no-ACME node still hits the
            // same join site without a branch.
            let renewal_fut = async move {
                match acme_renewal_task {
                    Some(h) => match h.await {
                        Ok(r) => r,
                        Err(je) => Err(anyhow::anyhow!(
                            "ACME renewal task did not complete cleanly: {je}"
                        )),
                    },
                    None => Ok(()),
                }
            };

            // Optional secondary plain-HTTP listener (NS-3.0
            // `00-default-http` shape). Opt-in only — absent unless an
            // edge node explicitly set it (see WebdConfig::http_listen).
            // When ACME plans were configured, the pre-ACME :80
            // listener spawned above is aborted and awaited below
            // (its tokio task drops its TcpListener at cancel-point,
            // freeing the port) before the production redirect
            // router rebinds — both share the same
            // `acme_challenges` Arc on the NodeState the production
            // router carries, so a renewal mid-runtime keeps
            // publishing tokens through the same map.
            if let Some(http_listen) = http_listen {
                if !tls_pair_present {
                    tracing::warn!(
                        "http_listen={http_listen} set but no TLS identities resolved — \
                         the :80 listener will 301 to https:// with no \
                         https endpoint to receive it"
                    );
                }
                // Stop the pre-ACME bootstrap :80 task (if any)
                // so the production redirect router can claim the
                // port. `abort()` cancels the spawned `serve_plain`
                // task at the next await; the listener socket drops
                // and frees the bind before we re-bind below.
                if let Some(task) = _http_listen_task {
                    task.abort();
                    let _ = task.await;
                }
                let redirect = build_http_redirect_router(node.clone());
                // The :80 redirect serve runs forever; `renewal_fut`
                // runs forever for an ACME node and resolves `Ok(())`
                // immediately otherwise. Either way `try_join!` blocks
                // on the never-ending :80 serve, keeping `listener_set`
                // (and its accept loops) alive for the daemon lifetime.
                tokio::try_join!(
                    serve_plain(&http_listen, redirect, "HTTP redirect"),
                    renewal_fut,
                )?;
            } else {
                // No :80 listener. `renewal_fut` runs forever for an
                // ACME node; for a no-ACME node it resolves `Ok(())`
                // immediately, so a `pending` guard keeps the started
                // `listener_set` serving for the daemon lifetime.
                tokio::try_join!(renewal_fut, std::future::pending::<Result<()>>())?;
            }
            // `listener_set` is dropped here only on process teardown
            // (the joins above never complete); naming it keeps the
            // accept loops alive until then.
            drop(listener_set);
        }
    }

    Ok(())
}

#[cfg(test)]
mod autoconfig_tests {
    use super::*;

    fn host_map(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::HOST, v.parse().unwrap());
        h
    }

    fn allow(domains: &[&str]) -> HashSet<String> {
        domains.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn strips_autoconfig_and_autodiscover_prefix_only() {
        let a = allow(&["example.org"]);
        // bare, autoconfig., autodiscover. all map to the same domain
        assert_eq!(
            served_domain_from_host(&host_map("example.org"), &a).as_deref(),
            Some("example.org")
        );
        assert_eq!(
            served_domain_from_host(&host_map("autoconfig.example.org"), &a).as_deref(),
            Some("example.org")
        );
        assert_eq!(
            served_domain_from_host(&host_map("autodiscover.example.org"), &a).as_deref(),
            Some("example.org")
        );
    }

    #[test]
    fn does_not_strip_mail_prefix() {
        // `mail.` is NOT stripped (NS 3.0 reference does not); the
        // stripped domain `mail.example.org` is not in the allowlist.
        let a = allow(&["example.org"]);
        assert_eq!(
            served_domain_from_host(&host_map("mail.example.org"), &a),
            None
        );
    }

    #[test]
    fn allowlist_gate_rejects_unknown_domain() {
        let a = allow(&["example.org"]);
        // Not in the set → None (→ 404) with no DNS performed.
        assert_eq!(
            served_domain_from_host(&host_map("evil.example.com"), &a),
            None
        );
        // Empty allowlist rejects everything.
        assert_eq!(
            served_domain_from_host(&host_map("example.org"), &HashSet::new()),
            None
        );
    }

    #[test]
    fn strips_port_and_lowercases_but_rejects_garbage_host() {
        let a = allow(&["example.org"]);
        assert_eq!(
            served_domain_from_host(&host_map("Example.ORG:80"), &a).as_deref(),
            Some("example.org")
        );
        // IPv6 / multi-colon / non-numeric port all fail uniformly.
        assert_eq!(
            served_domain_from_host(&host_map("example.org:80:443"), &a),
            None
        );
        assert_eq!(
            served_domain_from_host(&host_map("example.org:notaport"), &a),
            None
        );
    }

    #[test]
    fn xml_escape_covers_all_five_metacharacters() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn rendered_xml_has_expected_structure_and_no_trailing_newline() {
        let xml = render_autoconfig_xml("example.org", "mail.example.org");
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains("<emailProvider id=\"example.org\">"));
        assert!(xml.contains("<domain>example.org</domain>"));
        assert!(xml.contains("<displayName>Cosmix Mail</displayName>"));
        assert!(xml.contains("<displayShortName>Cosmix</displayShortName>"));
        assert!(xml.contains("<hostname>mail.example.org</hostname>"));
        assert!(xml.contains("<port>993</port>"));
        assert!(xml.contains("<port>465</port>"));
        assert!(xml.contains("<authentication>password-cleartext</authentication>"));
        assert!(xml.contains("<username>%EMAILADDRESS%</username>"));
        assert!(xml.contains("url=\"https://example.org/docs/\""));
        // Heredoc fidelity: ends exactly at the closing tag.
        assert!(xml.ends_with("</clientConfig>"));
        assert!(!xml.ends_with('\n'));
    }

    #[test]
    fn hostile_values_cannot_break_out_of_the_document() {
        // Even though normalize_host would reject these upstream, the
        // serializer itself must neutralise XML metacharacters
        // (defense-in-depth; maild-autoconfig.md §Security explicit
        // XML-injection test).
        let xml = render_autoconfig_xml(r#""><script>x</script>"#, "a\"><injected>b");
        assert!(!xml.contains("<script>"));
        assert!(!xml.contains("<injected>"));
        assert!(xml.contains("&lt;script&gt;"));
        assert!(xml.contains("&quot;&gt;&lt;injected&gt;"));
        // The attribute quote cannot be closed early.
        assert!(xml.contains("id=\"&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn response_does_not_vary_by_emailaddress_param() {
        // The handler signature takes only State + HeaderMap — there is
        // no query/body extractor, so the `?emailaddress=` param and
        // the Outlook `<EMailAddress>` body are structurally unable to
        // influence the response (no account enumeration). This test
        // documents that invariant at the serializer boundary: the body
        // is a pure function of (domain, mhost) only.
        let a = render_autoconfig_xml("example.org", "mail.example.org");
        let b = render_autoconfig_xml("example.org", "mail.example.org");
        assert_eq!(a, b);
    }
}

// ---------------------------------------------------------------------------
// Vhost resolve + host-routing tests (Buckets C+D+E)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod vhost_tests {
    //! Buckets C+D+E of `webd-vhosts-phase1.md`. The tests cover three
    //! surfaces:
    //!
    //! - **C**: `[[webd.vhost]]` config parse + resolve.
    //! - **D**: Host routing + middleware (axum integration via
    //!   `ServiceExt::oneshot`).
    //! - **E**: Legacy compat + LE-only enforcement on the legacy pair.
    //!
    //! Rejection-class chains are rcgen-synthesised at test time
    //! (same approach as `cosmix-lib-daemon/tests/le_validator.rs`).
    //! The acceptance-class chain is the real LE-issued fixture
    //! committed under `cosmix-lib-daemon/tests/fixtures/` — relative
    //! `include_bytes!` keeps the fixture in one place.
    use super::*;

    use std::io::Write as _;
    use std::time::Duration;

    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::pki_types::UnixTime;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    // ── Wildcard-style cert fixture (synthetic) ──────────────────────
    // Tests below pass this to `write_chain(&td, "example", ...)` so the
    // file shape on disk is "wildcard leaf + intermediate" with valid
    // PEM blocks. Tests are wiring/parsing-shape — they do NOT validate
    // against an LE root, so a synthetic chain is sufficient. The
    // historical fixture `le-prod-wildcard.pem` (a real LE chain
    // captured from a private mesh deployment) was removed during the
    // 2026-05-29 public-repo sanitization.
    fn synth_wildcard_pem() -> Vec<u8> {
        synth_chain_pem(
            "Test Root CA",
            "Test Intermediate",
            "*.example.com",
            &["*.example.com", "example.com"],
        )
    }

    fn unix(year: i32, month: time::Month, day: u8) -> UnixTime {
        let ts = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let dt = ts
            .replace_year(year)
            .unwrap()
            .replace_month(month)
            .unwrap()
            .replace_day(day)
            .unwrap();
        UnixTime::since_unix_epoch(Duration::from_secs(dt.unix_timestamp() as u64))
    }

    /// Pinned inside the LE fixture's validity window
    /// (2026-04-21 → 2026-07-20). The validator's `notBefore`/
    /// `notAfter` checks compare against this; pinning keeps the
    /// suite deterministic across the cert's lifetime.
    fn inside_le_validity() -> UnixTime {
        unix(2026, time::Month::May, 15)
    }

    // ── rcgen helpers (mirrors cosmix-lib-daemon's helpers) ─────────

    fn synth_selfsigned_pem(cn: &str, sans: &[&str]) -> Vec<u8> {
        let key = KeyPair::generate().expect("rcgen keypair");
        let mut params =
            CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .expect("rcgen params");
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        params.distinguished_name = dn;
        let cert = params.self_signed(&key).expect("rcgen self-sign");
        cert.pem().into_bytes()
    }

    fn synth_chain_pem(
        root_cn: &str,
        intermediate_cn: &str,
        leaf_cn: &str,
        leaf_sans: &[&str],
    ) -> Vec<u8> {
        let root_key = KeyPair::generate().expect("root key");
        let mut root_params = CertificateParams::new(Vec::<String>::new()).expect("root params");
        let mut root_dn = DistinguishedName::new();
        root_dn.push(DnType::CommonName, root_cn);
        root_params.distinguished_name = root_dn;
        root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        root_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        let root_cert = root_params.self_signed(&root_key).expect("root sign");

        let int_key = KeyPair::generate().expect("int key");
        let mut int_params = CertificateParams::new(Vec::<String>::new()).expect("int params");
        let mut int_dn = DistinguishedName::new();
        int_dn.push(DnType::CommonName, intermediate_cn);
        int_params.distinguished_name = int_dn;
        int_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        int_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        let int_cert = int_params
            .signed_by(&int_key, &root_cert, &root_key)
            .expect("int sign");

        let leaf_key = KeyPair::generate().expect("leaf key");
        let mut leaf_params =
            CertificateParams::new(leaf_sans.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .expect("leaf params");
        let mut leaf_dn = DistinguishedName::new();
        leaf_dn.push(DnType::CommonName, leaf_cn);
        leaf_params.distinguished_name = leaf_dn;
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &int_cert, &int_key)
            .expect("leaf sign");

        let mut pem = leaf_cert.pem();
        pem.push_str(&int_cert.pem());
        pem.into_bytes()
    }

    /// Materialise PEM bytes + a placeholder key on disk and return
    /// `(cert_path, key_path)`. The key is synthetic — the resolver
    /// only reads it during `serve_tls`, which the unit tests do not
    /// exercise; `resolve_node_state` itself never opens the key
    /// file, so a missing/empty `key.pem` would still pass.
    fn write_chain(td: &TempDir, name: &str, pem: &[u8]) -> (PathBuf, PathBuf) {
        let cert = td.path().join(format!("{name}.cert.pem"));
        let key = td.path().join(format!("{name}.key.pem"));
        std::fs::write(&cert, pem).unwrap();
        std::fs::write(&key, b"PLACEHOLDER\n").unwrap();
        (cert, key)
    }

    /// Skeleton `ResolveInputs` with the legacy slot empty. Tests
    /// fill in `webd` and (optionally) the legacy slot per scenario.
    fn base_inputs<'a>(
        webd: &'a cosmix_config::node::WebdConfig,
        td: &TempDir,
    ) -> ResolveInputs<'a> {
        ResolveInputs {
            webd,
            legacy_tls_cert: None,
            legacy_tls_key: None,
            legacy_www_dir: td.path().join("legacy-www"),
            legacy_db_path: td.path().join("web.db"),
            legacy_jmap_upstream: "https://127.0.0.1:8443".into(),
            legacy_noded_ws: "ws://127.0.0.1:4200/ws".into(),
            legacy_docs_dir: None,
            now: inside_le_validity(),
        }
    }

    fn mkdir(td: &TempDir, name: &str) -> PathBuf {
        let p = td.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // =========================================================
    // Bucket C — `[[webd.vhost]]` config parse + resolve
    // =========================================================

    #[test]
    fn vhost_block_parses_minimal() {
        // `.conf.mix` strict-data form (the real load format post-migration):
        // a `vhost` list of maps. Map entries use `,` separators (newlines
        // are suppressed inside `{ }`).
        let conf_mix_src = r#"
vhost: [
  {
    host: "site-a.example",
    www_dir: "/var/lib/cosmix/www/site-a",
    tls_cert: "/etc/letsencrypt/live/site-a.example/fullchain.pem",
    tls_key: "/etc/letsencrypt/live/site-a.example/privkey.pem"
  }
]
"#;
        let cfg: cosmix_config::node::WebdConfig =
            cosmix_config::from_conf_mix_str(conf_mix_src).expect("WebdConfig parse");
        assert_eq!(cfg.vhost.len(), 1);
        let v = &cfg.vhost[0];
        assert_eq!(v.host, "site-a.example");
        assert!(v.aliases.is_empty());
        assert_eq!(v.www_dir, "/var/lib/cosmix/www/site-a");
        assert!(v.tls_cert.is_some());
        assert!(v.tls_key.is_some());
        assert!(v.acme.is_none());
        assert!(v.cms_db_path.is_none());
        assert!(v.jmap_upstream.is_none());
        assert!(v.noded_ws.is_none());
        assert!(v.docs_dir.is_none());
    }

    #[test]
    fn vhost_acme_and_manual_tls_together_rejected() {
        // P2-C3: lib-config's AcmeResolveError::AcmeAndManualTlsTogether
        // must surface through the webd pipeline. A row with *both*
        // an `acme = {...}` block and a manual `tls_cert`/`tls_key`
        // pair is a config error — pick one issuance path per row.
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.http_listen = Some("0.0.0.0:80".into());
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "site-a.example".into(),
            www_dir: www.to_string_lossy().into_owned(),
            tls_cert: Some("/x/cert.pem".into()),
            tls_key: Some("/x/key.pem".into()),
            acme: Some(cosmix_config::node::WebdVhostAcmeConfig {
                provider: cosmix_config::node::WebdAcmeProvider::LetsEncryptStaging,
                challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
                contact_email: "ops@example.com".into(),
            }),
            ..Default::default()
        });
        let err = resolve_node_state(base_inputs(&webd, &td))
            .expect_err("acme + manual tls must hard-fail resolve");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mutually exclusive"),
            "expected mutex error, got: {msg}"
        );
    }

    #[test]
    fn vhost_acme_only_flows_through_to_main_after_c5d1() {
        // P2-C5d1 lifted the C3 ACME-vhost rejection guard. An
        // ACME-only resolve now produces a `ResolvedNodeState` with
        // zero manual identities and a non-empty `acme_plans` —
        // `main()` is responsible for binding :80 first, running
        // `AcmeProvisioner::startup_pass`, and splicing the issued
        // identities into the rustls resolver before :443 binds.
        // The resolver path stops here; the live-cert path is
        // exercised by Bucket H (`acme_provisioner::tests`) and the
        // gauntlet on gamma.
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.http_listen = Some("0.0.0.0:80".into());
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "site-a.example".into(),
            aliases: vec!["WWW.site-a.example".into()],
            www_dir: www.to_string_lossy().into_owned(),
            acme: Some(cosmix_config::node::WebdVhostAcmeConfig {
                provider: cosmix_config::node::WebdAcmeProvider::LetsEncryptStaging,
                challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
                contact_email: "ops@example.com".into(),
            }),
            ..Default::default()
        });
        let resolved = resolve_node_state(base_inputs(&webd, &td))
            .expect("ACME-only resolve must succeed after C5d1");
        assert!(
            resolved.identities.is_empty(),
            "ACME-only resolve must produce zero manual identities, got {:?}",
            resolved.identities
        );
        assert_eq!(
            resolved.acme_plans.len(),
            1,
            "the single ACME row must flow through as a plan"
        );
        assert_eq!(resolved.acme_plans[0].fqdn, "site-a.example");
        assert!(
            resolved.vhosts.contains_key("site-a.example"),
            "vhost map must include the ACME-only host"
        );
    }

    #[test]
    fn vhost_failsoft_bad_cert_disables_only_that_vhost() {
        // B1 — fail-soft per-vhost. A 2-vhost config where vhost #1's
        // manual-PEM cert fails `validate_le_chain` (synthetic chain,
        // rejected like the legacy-path tests above) must NOT abort the
        // whole resolve. The healthy vhost #2 still resolves; #1 is
        // recorded in `disabled_vhosts`, not surfaced as an `Err`.
        let td = TempDir::new().unwrap();
        let bad_www = mkdir(&td, "bad-www");
        let good_www = mkdir(&td, "good-www");
        // #1 — manual-PEM with a synthetic (non-LE) chain → bad cert.
        let (bad_cert, bad_key) = write_chain(
            &td,
            "bad",
            &synth_selfsigned_pem("bad.example", &["bad.example"]),
        );

        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.http_listen = Some("0.0.0.0:80".into());
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "bad.example".into(),
            www_dir: bad_www.to_string_lossy().into_owned(),
            tls_cert: Some(bad_cert.to_string_lossy().into_owned()),
            tls_key: Some(bad_key.to_string_lossy().into_owned()),
            ..Default::default()
        });
        // #2 — a healthy ACME vhost (ACME rows skip startup cert
        // validation, so this is the resolve-layer's available "good"
        // vhost; a manual-PEM "good" vhost can't be built in unit tests
        // without a real LE chain).
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "good.example".into(),
            www_dir: good_www.to_string_lossy().into_owned(),
            acme: Some(cosmix_config::node::WebdVhostAcmeConfig {
                provider: cosmix_config::node::WebdAcmeProvider::LetsEncryptStaging,
                challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
                contact_email: "ops@example.com".into(),
            }),
            ..Default::default()
        });

        let resolved = resolve_node_state(base_inputs(&webd, &td))
            .expect("a single bad-cert vhost must NOT abort resolve (fail-soft)");

        // Healthy vhost served; bad one dropped from the runtime map.
        assert!(
            resolved.vhosts.contains_key("good.example"),
            "healthy vhost must still resolve"
        );
        assert!(
            !resolved.vhosts.contains_key("bad.example"),
            "bad-cert vhost must be dropped from the host-router map"
        );
        // Recorded as disabled with a cert-shaped reason.
        assert_eq!(resolved.disabled_vhosts.len(), 1);
        let dis = &resolved.disabled_vhosts[0];
        assert_eq!(dis.host, "bad.example");
        assert_eq!(dis.names, vec!["bad.example".to_string()]);
        assert!(
            dis.reason.contains("validate_le_chain") || dis.reason.contains("path validation"),
            "reason should name the cert failure, got: {}",
            dis.reason
        );
        // The healthy ACME plan survives; the bad vhost contributed none.
        assert_eq!(resolved.acme_plans.len(), 1);
        assert_eq!(resolved.acme_plans[0].fqdn, "good.example");
        // No manual identity emitted (the only manual vhost was dropped).
        assert!(resolved.identities.is_empty());
    }

    #[test]
    fn vhost_www_dir_missing_refuses_start_when_sole_vhost() {
        // Post-B1: a missing www_dir is no longer a hard per-vhost
        // abort — the row is fail-soft-disabled. But when it is the
        // ONLY vhost, the healthy subset is empty and webd still
        // refuses to start rather than bind an empty key-set. The
        // refusal names the dropped vhost's reason (the www_dir error),
        // so an operator sees *why* nothing came up. (The fail-soft
        // skip-and-serve-the-rest behaviour with a healthy sibling is
        // covered by `vhost_failsoft_bad_cert_disables_only_that_vhost`.)
        let td = TempDir::new().unwrap();
        let (cert, key) = write_chain(&td, "example", &synth_wildcard_pem());
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "example.com".into(),
            www_dir: td
                .path()
                .join("does-not-exist")
                .to_string_lossy()
                .into_owned(),
            tls_cert: Some(cert.to_string_lossy().into_owned()),
            tls_key: Some(key.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let err = resolve_node_state(base_inputs(&webd, &td))
            .expect_err("a sole vhost with a missing www_dir must refuse start");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("every one failed per-vhost validation"),
            "expected the all-disabled refusal, got: {msg}"
        );
        assert!(
            msg.contains("www_dir") && msg.contains("is not a directory"),
            "refusal must name the www_dir reason, got: {msg}"
        );
    }

    #[test]
    fn vhost_duplicate_host_fatal() {
        // Two rows with the same host (both covered by the wildcard
        // fixture) must be rejected at resolve, mirroring the
        // duplicate-server_name rejection in SniCertResolver::from_config.
        let td = TempDir::new().unwrap();
        let www_a = mkdir(&td, "www-a");
        let www_b = mkdir(&td, "www-b");
        let (cert, key) = write_chain(&td, "example", &synth_wildcard_pem());
        let mut webd = cosmix_config::node::WebdConfig::default();
        let row = cosmix_config::node::WebdVhostConfig {
            host: "example.com".into(),
            tls_cert: Some(cert.to_string_lossy().into_owned()),
            tls_key: Some(key.to_string_lossy().into_owned()),
            ..Default::default()
        };
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            www_dir: www_a.to_string_lossy().into_owned(),
            ..row.clone()
        });
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            www_dir: www_b.to_string_lossy().into_owned(),
            ..row
        });
        let err = resolve_node_state(base_inputs(&webd, &td))
            .expect_err("duplicate host must hard-fail resolve");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate host"),
            "expected duplicate-host error, got: {msg}"
        );
    }

    // =========================================================
    // Bucket D — Host routing + middleware
    // =========================================================

    /// Build a two-vhost `NodeState` (`site-a.example` + alias
    /// `www.site-a.example`, `site-b.example`) without going through
    /// `resolve_node_state` — host-routing tests want to assert the
    /// middleware's behaviour, not re-test the validator path.
    fn synth_node(td: &TempDir) -> Arc<NodeState> {
        let www_a = mkdir(td, "www-a");
        let www_b = mkdir(td, "www-b");
        std::fs::File::create(www_a.join("index.html"))
            .unwrap()
            .write_all(b"SITE-A")
            .unwrap();
        std::fs::File::create(www_b.join("index.html"))
            .unwrap()
            .write_all(b"SITE-B")
            .unwrap();
        let a = Arc::new(VhostState {
            fqdn: "site-a.example".into(),
            db: None,
            www_dir: www_a,
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        let b = Arc::new(VhostState {
            fqdn: "site-b.example".into(),
            db: None,
            www_dir: www_b,
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        // C3e: middleware tests build the directory directly via
        // `VhostDirectory::build` rather than threading through the
        // namespace + runtime-map adapter — these tests assert the
        // host-routing behaviour, not the namespace-sourcing path.
        let directory = vhost_directory::VhostDirectory::build(vec![
            vhost_directory::VhostDirectoryEntry {
                state: a,
                aliases: vec!["www.site-a.example".into()],
            },
            vhost_directory::VhostDirectoryEntry {
                state: b,
                aliases: vec![],
            },
        ])
        .expect("synth_node: VhostDirectory::build");
        let vhosts: Arc<ArcSwap<vhost_directory::VhostDirectory>> =
            Arc::new(ArcSwap::from(Arc::new(directory)));
        Arc::new(NodeState {
            service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_throttle: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            vhosts,
            http_client: reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap(),
            // Test fixture — ephemeral sealer (no login traffic).
            session: Arc::new(session::SessionSealer::ephemeral()),
            // Autoconfig admission opens "example.org" so the
            // bypass test can confirm autoconfig reaches its own
            // handler under an unrelated `Host`.
            served_mail_domains: ["example.org".into()].into_iter().collect(),
            autoconfig_mail_host: None,
            mx: None,
            acme_challenges: Arc::new(RwLock::new(HashMap::new())),
            tls_status_rx: tokio::sync::watch::channel(tls_status::TlsStatusSnapshot::default()).1,
            // Middleware tests never dispatch through PropsRouter.
            props_router: Arc::new(cosmix_props::PropsRouter::new("webd")),
            props_subscribe_granter: Arc::new(bus::subscribe_granter::NodedSubscribeGranter::new(
                bus::subscribe_granter::new_broker_handle(),
            )),
            broker_handle: bus::subscribe_granter::new_broker_handle(),
            // Bucket-D middleware tests never dispatch into the C5
            // ergonomic verbs, so the runtime + lock-map + notify
            // stay `None`. The dedicated `bus::vhost_verbs::tests`
            // module builds its own NodeState with these populated.
            vhosts_runtime: None,
            listeners_runtime: None,
            listeners_operators: Vec::new(),
            vhost_key_locks: None,
            acme_notify: None,
            acme_force_renew_queue: None,
            // Middleware tests never serve embedded handlers.
            handlers: Arc::new(ArcSwap::from(
                Arc::new(mix_handler::HandlerTable::default()),
            )),
            handler_ast_cache: mix_handler::new_ast_cache(),
            tls_reload: None,
        })
    }

    fn req(host: &str, path: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("GET")
            .uri(path)
            .header(axum::http::header::HOST, host)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn unknown_host_on_https_returns_404() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node);
        let resp = app
            .oneshot(req("notconfigured.example", "/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_host_on_plain_http_returns_400() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_http_redirect_router(node);
        let resp = app
            .oneshot(req("notconfigured.example", "/"))
            .await
            .unwrap();
        // 400 (admit middleware rejects unknown Host) — not 301, so
        // an attacker-Host cannot drive the redirect's Location.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get(axum::http::header::LOCATION).is_none());
    }

    async fn body_bytes(resp: axum::http::Response<axum::body::Body>) -> Vec<u8> {
        use http_body_util::BodyExt as _;
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn known_host_dispatches_to_correct_vhost() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node);
        let resp_a = app
            .clone()
            .oneshot(req("site-a.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp_a).await, b"SITE-A");

        let resp_b = app
            .oneshot(req("site-b.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp_b.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp_b).await, b"SITE-B");
    }

    #[tokio::test]
    async fn listener_scope_404s_a_vhost_off_its_allowlist() {
        // P2-C2 per-interface isolation, HTTP half: a vhost that IS
        // registered on this node but NOT on the listener's allowlist
        // gets the same 404 as an unknown Host (no cross-listener
        // enumeration channel). A single-listener back-compat node
        // layers every vhost here, so this check never fires there —
        // which is why the other Bucket-D tests (no `ListenerScope`)
        // are unaffected.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let allowed: HashSet<String> = ["site-a.example".to_string()].into_iter().collect();
        let scope = ListenerScope {
            id: Arc::from("wg"),
            allowed_hosts: Arc::new(allowed),
            external: false,
        };
        let app = build_router(node).layer(Extension(scope));

        // On the allowlist → served.
        let resp_a = app
            .clone()
            .oneshot(req("site-a.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp_a).await, b"SITE-A");

        // A real vhost, but off this listener's allowlist → 404, not
        // its content.
        let resp_b = app
            .oneshot(req("site-b.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp_b.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn alias_routes_to_primary() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node);
        let resp = app
            .oneshot(req("www.site-a.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, b"SITE-A");
    }

    #[tokio::test]
    async fn arc_swap_publish_makes_new_vhost_routable() {
        // C3b regression guard: an ArcSwap publish of a new
        // `VhostDirectory` containing a previously-unknown vhost must
        // make that vhost routable on BOTH HTTPS dispatch and the
        // plain-HTTP admit gate, WITHOUT rebuilding the axum router.
        // Pre-C3b this failed because the `allowed: Arc<HashSet>`
        // was snapshotted at router-build time (rev-2 BLOCKER in
        // `_doc/planned/webd-vhosts-phase3.md` §"Why HTTP dispatch
        // needs its own arc-swap"). The host_router middleware
        // already used `node.vhosts.get(host)`, so the HTTPS branch
        // would adopt the new vhost on its own; the admit branch
        // would not. After C3b both branches read through the same
        // ArcSwap, so they're in lock-step.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node.clone());
        let redirect = build_http_redirect_router(node.clone());

        // Pre-publish: site-c is unknown.
        let pre_https = app
            .clone()
            .oneshot(req("site-c.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(
            pre_https.status(),
            StatusCode::NOT_FOUND,
            "site-c should 404 on HTTPS pre-publish"
        );
        let pre_http = redirect
            .clone()
            .oneshot(req("site-c.example", "/"))
            .await
            .unwrap();
        assert_eq!(
            pre_http.status(),
            StatusCode::BAD_REQUEST,
            "site-c should be admit-rejected on plain HTTP pre-publish"
        );

        // Publish a new directory adding site-c.
        let www_c = mkdir(&td, "www-c");
        std::fs::File::create(www_c.join("index.html"))
            .unwrap()
            .write_all(b"SITE-C")
            .unwrap();
        let c = Arc::new(VhostState {
            fqdn: "site-c.example".into(),
            db: None,
            www_dir: www_c,
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        let existing = node.vhosts.load();
        let mut entries: Vec<vhost_directory::VhostDirectoryEntry> = existing
            .primaries
            .iter()
            .map(|p| vhost_directory::VhostDirectoryEntry {
                state: p.state.clone(),
                aliases: p.aliases.clone(),
            })
            .collect();
        entries.push(vhost_directory::VhostDirectoryEntry {
            state: c,
            aliases: vec![],
        });
        let new_dir = vhost_directory::VhostDirectory::build(entries).expect("build new directory");
        node.vhosts.store(Arc::new(new_dir));

        // Post-publish: site-c is routable on HTTPS and admitted on :80.
        // The SAME axum app instances are used — no router rebuild.
        let post_https = app
            .oneshot(req("site-c.example", "/index.html"))
            .await
            .unwrap();
        assert_eq!(
            post_https.status(),
            StatusCode::OK,
            "site-c should serve OK on HTTPS after publish"
        );
        assert_eq!(body_bytes(post_https).await, b"SITE-C");
        let post_http = redirect.oneshot(req("site-c.example", "/")).await.unwrap();
        // 301 (admit pass → redirect_to_https) rather than 400.
        assert_eq!(
            post_http.status(),
            StatusCode::MOVED_PERMANENTLY,
            "site-c should be admitted and redirected on plain HTTP after publish"
        );
    }

    #[tokio::test]
    async fn case_insensitive_host_match() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node);
        let resp = app
            .oneshot(req("SITE-A.EXAMPLE", "/index.html"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, b"SITE-A");
    }

    #[tokio::test]
    async fn autoconfig_path_bypasses_admit() {
        // Autoconfig has its *own* served-mail-domain admission gate
        // (`served_mail_domains`). The host_router middleware must
        // **not** 404 a request to an autoconfig path on an unknown
        // Host — autoconfig handles its own admission and would
        // never receive the request otherwise.
        //
        // The detection trick: the `/autodiscover/autodiscover.xml`
        // route is registered for POST only. A GET to that path with
        // a Host that is *not* in the vhosts map distinguishes the
        // two layers:
        //
        // - If host_router were gating autoconfig: it would `404`
        //   the unknown Host before the route matched.
        // - If the bypass works: axum's method router for the
        //   autoconfig branch matches the path, sees no GET handler,
        //   and returns `405 Method Not Allowed` with
        //   `Allow: POST`. host_router never runs.
        //
        // `405 + Allow: POST` is therefore the unambiguous proof
        // that the autoconfig branch was reached.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_router(node);
        let r = axum::http::Request::builder()
            .method("GET")
            .uri("/autodiscover/autodiscover.xml")
            .header(axum::http::header::HOST, "notconfigured.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(r).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "autoconfig branch must be reached for an unknown Host \
             (host_router would have returned 404 if it gated this path)"
        );
        let allow = resp
            .headers()
            .get(axum::http::header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            allow.contains("POST"),
            "expected `Allow: POST` from autoconfig's method router, got {allow:?}"
        );
    }

    #[tokio::test]
    async fn autoconfig_path_bypasses_admit_on_http_redirect_router() {
        // Same 405-trick proof against `build_http_redirect_router`.
        // The plain-HTTP listener composes autoconfig + redirect via
        // the same `Router::merge` + branch-isolated `.layer` pattern
        // as HTTPS; the autoconfig branch must remain unlayered there
        // too (or autoconfig-on-:80 would never reach its handler for
        // attacker-Host requests, which is the whole point of running
        // autoconfig on plain HTTP).
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_http_redirect_router(node);
        let r = axum::http::Request::builder()
            .method("GET")
            .uri("/autodiscover/autodiscover.xml")
            .header(axum::http::header::HOST, "notconfigured.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(r).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "redirect router's autoconfig branch must be reached for an \
             unknown Host (plain_http_host_admit would have returned 400 \
             if it gated this path)"
        );
        let allow = resp
            .headers()
            .get(axum::http::header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            allow.contains("POST"),
            "expected `Allow: POST` from autoconfig's method router on the \
             redirect listener, got {allow:?}"
        );
    }

    // =========================================================
    // Bucket G — P2-C4 HTTP-01 challenge route
    // =========================================================

    /// Seed a token/keyauth pair into the redirect router's
    /// challenge map. The map is shared via `Arc` with the router
    /// returned by `build_http_redirect_router(node)`, so writes
    /// here are observed by the route handler.
    async fn seed_challenge(node: &Arc<NodeState>, token: &str, keyauth: &str) {
        node.acme_challenges
            .write()
            .await
            .insert(token.into(), keyauth.into());
    }

    /// 43-char token in the LE shape (RFC 8555 §8.3 alphabet,
    /// 22+ bytes). Hard-coded so the test asserts the exact
    /// alphabet the validator accepts.
    const SAMPLE_TOKEN: &str = "5TbFkXp9q-2zR_yJhVcA4nM6sLwQ7oE0iK3uD8gN1xZ";

    #[tokio::test]
    async fn challenge_serves_known_token() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        seed_challenge(&node, SAMPLE_TOKEN, "keyauth-body").await;
        let app = build_http_redirect_router(node);
        let path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}");
        let resp = app.oneshot(req("site-a.example", &path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // RFC 8555 §8.3 example uses application/octet-stream.
        assert!(
            ct.starts_with("application/octet-stream"),
            "expected application/octet-stream content-type, got {ct:?}"
        );
        assert_eq!(body_bytes(resp).await, b"keyauth-body");
    }

    #[tokio::test]
    async fn challenge_404_on_pathological_prefix_shapes() {
        // The wildcard route + sibling bare-prefix route guarantee
        // a uniform 404 for every URL under
        // `/.well-known/acme-challenge` that is not a single
        // valid-shape token segment — Codex R1 caught that without
        // the wildcard, multi-segment and trailing-slash shapes
        // would fall into the redirect/admit branch and respond
        // 301/400 instead. Each shape below MUST be 404, never
        // 301 (redirect leak) and never 400 (admit leak).
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        seed_challenge(&node, SAMPLE_TOKEN, "keyauth-body").await;
        let app = build_http_redirect_router(node);

        // Build the `%2F`-encoded slash variant. axum
        // percent-decodes the wildcard parameter before extraction,
        // so this becomes a real `/` inside `tail` — the
        // `tail.contains('/')` reject in `acme_challenge_serve`
        // must still 404 it (decoded equivalence to the multi-segment
        // case, but the URI itself is one path segment).
        let pct_slash_path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}%2Fextra");
        // Invalid-UTF-8 percent escape — axum's `Path<String>`
        // extractor would reject this with `400` (PathRejection)
        // unless the handler accepts `Result<Path<String>, _>`
        // and maps the rejection to the uniform 404 (Codex R2
        // finding).
        let bad_utf8_path = format!(
            "/.well-known/acme-challenge/{}%FF",
            &SAMPLE_TOKEN[..SAMPLE_TOKEN.len() - 3]
        );
        for path in [
            // Multi-segment under the prefix.
            "/.well-known/acme-challenge/a/b",
            // Multi-segment where the first segment is a valid
            // token shape — proves we reject by structure, not by
            // looking up the first segment.
            &format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}/extra"),
            // Trailing slash with empty token segment.
            "/.well-known/acme-challenge/",
            // Bare prefix, no trailing slash.
            "/.well-known/acme-challenge",
            // Percent-encoded slash inside the token segment.
            pct_slash_path.as_str(),
            // Invalid-UTF-8 percent escape — must NOT surface as 400.
            bad_utf8_path.as_str(),
        ] {
            let resp = app
                .clone()
                .oneshot(req("site-a.example", path))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "path {path} must be uniform 404 (got {status})",
                status = resp.status(),
            );
            // Defence-in-depth: must not surface as a redirect (301/302).
            assert!(
                resp.headers().get(axum::http::header::LOCATION).is_none(),
                "path {path} leaked into the redirect fallback"
            );
        }
    }

    #[tokio::test]
    async fn challenge_404_on_unknown_token() {
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        // Map deliberately empty.
        let app = build_http_redirect_router(node);
        let path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}");
        let resp = app.oneshot(req("site-a.example", &path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn challenge_404_on_invalid_token_shape() {
        // Uniform 404 for every shape rejection — never 400. The
        // axum extractor rejects `/` and `..` at the path-segment
        // boundary (they never reach the handler); the handler's
        // shape gate covers the remaining cases (CRLF, oversized,
        // empty, illegal alphabet). All paths route through the
        // redirect router fallback OR `acme_challenge_serve`; both
        // surface as 404, never 400, so the route does not become
        // a probe oracle distinguishing the rejection reasons.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        let app = build_http_redirect_router(node);

        // Too short (< 22 chars).
        let resp = app
            .clone()
            .oneshot(req("site-a.example", "/.well-known/acme-challenge/short"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_ne!(resp.status(), StatusCode::BAD_REQUEST);

        // Illegal alphabet (`!` not in base64url).
        let bad_alpha = "!!!!!!!!!!!!!!!!!!!!!!!!!"; // 25 chars, all illegal.
        let path = format!("/.well-known/acme-challenge/{bad_alpha}");
        let resp = app
            .clone()
            .oneshot(req("site-a.example", &path))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Oversized (> 128 chars).
        let oversized = "A".repeat(129);
        let path = format!("/.well-known/acme-challenge/{oversized}");
        let resp = app.oneshot(req("site-a.example", &path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn challenge_route_wins_over_admit_deny() {
        // RFC 8555 §8.3 requires serving the key authorisation under
        // the requested host regardless of admission policy. LE's
        // validator connects to the authoritative A/AAAA of the
        // ACME-pending FQDN — which is NOT in NodeState::vhosts at
        // C4 (the resolver swap is a C5 concern). So a request with
        // an unknown Host must reach the challenge handler and
        // return 200 (known token) or 404 (unknown / bad-shape
        // token); a 400 from `plain_http_host_admit` would prove
        // the route is below the admit layer and would break
        // issuance.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        seed_challenge(&node, SAMPLE_TOKEN, "keyauth-body").await;
        let app = build_http_redirect_router(node);
        let path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}");
        let resp = app
            .clone()
            .oneshot(req("attacker.example.com", &path))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "challenge route must precede plain_http_host_admit — \
             a 400 here would break LE HTTP-01 validation"
        );
        assert_eq!(body_bytes(resp).await, b"keyauth-body");

        // Same Host on a token that is not in the map: 404, not 400.
        let path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}other-token-43-chars-aa");
        let resp = app
            .oneshot(req("attacker.example.com", &path))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_ne!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn challenge_route_does_not_leak_under_post() {
        // POST must NOT serve the key authorisation. The route is
        // registered as `axum::routing::get` only, so a POST falls
        // through to either axum's method router (405) or the
        // redirect fallback (which would 301). Either way it is
        // not 200 + key-auth body.
        let td = TempDir::new().unwrap();
        let node = synth_node(&td);
        seed_challenge(&node, SAMPLE_TOKEN, "keyauth-body").await;
        let app = build_http_redirect_router(node);
        let path = format!("/.well-known/acme-challenge/{SAMPLE_TOKEN}");
        let r = axum::http::Request::builder()
            .method("POST")
            .uri(&path)
            .header(axum::http::header::HOST, "site-a.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(r).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "POST must never serve the key authorisation body"
        );
        let body = body_bytes(resp).await;
        assert_ne!(
            body, b"keyauth-body",
            "POST must never return the keyauth body"
        );
    }

    #[test]
    fn acme_token_validator_accepts_le_shape() {
        // LE emits 43-char base64url-no-pad tokens; the validator
        // must accept the whole alphabet and reject everything
        // outside it.
        assert!(is_valid_acme_token(SAMPLE_TOKEN));
        // 22 is the syntactic floor.
        assert!(is_valid_acme_token(&"A".repeat(22)));
        // 21 is just under floor.
        assert!(!is_valid_acme_token(&"A".repeat(21)));
        // 128 is the upper bound (generous).
        assert!(is_valid_acme_token(&"A".repeat(128)));
        // 129 is just over.
        assert!(!is_valid_acme_token(&"A".repeat(129)));
        // Empty rejected.
        assert!(!is_valid_acme_token(""));
        // Pad chars (`=`) and `+`/`/` from std base64 rejected
        // (RFC 8555 §8.3 mandates base64url with no padding).
        assert!(!is_valid_acme_token(&format!("{}=", "A".repeat(43))));
        assert!(!is_valid_acme_token(&format!("{}+", "A".repeat(42))));
        assert!(!is_valid_acme_token(&format!("{}/", "A".repeat(42))));
        // CR/LF / null bytes rejected.
        assert!(!is_valid_acme_token(&format!("{}\r\n", "A".repeat(41))));
    }

    // =========================================================
    // Bucket E — Legacy compat + LE-only enforcement
    // =========================================================

    fn legacy_inputs<'a>(
        webd: &'a cosmix_config::node::WebdConfig,
        td: &TempDir,
        cert: PathBuf,
        key: PathBuf,
        www: PathBuf,
    ) -> ResolveInputs<'a> {
        let mut i = base_inputs(webd, td);
        i.legacy_tls_cert = Some(cert);
        i.legacy_tls_key = Some(key);
        i.legacy_www_dir = www;
        i
    }

    #[test]
    fn legacy_top_level_selfsigned_rejected() {
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let pem = synth_selfsigned_pem("example.com", &["example.com"]);
        let (cert, key) = write_chain(&td, "selfsigned", &pem);
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.tls_server_name = vec!["example.com".into()];
        let err = resolve_node_state(legacy_inputs(&webd, &td, cert, key, www))
            .expect_err("self-signed legacy chain must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("intermediate")
                || msg.contains("Intermediate")
                || msg.contains("path validation"),
            "expected LE-validator rejection, got: {msg}"
        );
    }

    #[test]
    fn legacy_top_level_internalca_rejected() {
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let pem = synth_chain_pem(
            "Cosmix Internal Root CA",
            "Cosmix Internal Issuing CA",
            "internal.example",
            &["internal.example"],
        );
        let (cert, key) = write_chain(&td, "internal", &pem);
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.tls_server_name = vec!["internal.example".into()];
        let err = resolve_node_state(legacy_inputs(&webd, &td, cert, key, www))
            .expect_err("internal-CA legacy chain must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("path validation"),
            "expected path-validation rejection, got: {msg}"
        );
    }

    #[test]
    fn legacy_top_level_digicert_rejected() {
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let pem = synth_chain_pem(
            "DigiCert Global Root CA",
            "DigiCert SHA2 Secure Server CA",
            "spoofed.example",
            &["spoofed.example"],
        );
        let (cert, key) = write_chain(&td, "digicert", &pem);
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.tls_server_name = vec!["spoofed.example".into()];
        let err = resolve_node_state(legacy_inputs(&webd, &td, cert, key, www))
            .expect_err("DigiCert-named spoof legacy chain must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("path validation"),
            "expected path-validation rejection, got: {msg}"
        );
    }

    #[test]
    fn legacy_pair_via_cli_override_validated() {
        // The CLI override path is just `legacy_tls_cert` /
        // `legacy_tls_key` being populated when `webd.tls_cert` /
        // `webd.tls_key` are not — the resolver does not know or
        // care which slot provided the paths. So testing the CLI
        // override path is exactly the same as testing the
        // config-file path with a self-signed PEM: same hard-fail.
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let pem = synth_selfsigned_pem("example.com", &["example.com"]);
        let (cert, key) = write_chain(&td, "cli-selfsigned", &pem);
        // Config has *no* tls_cert / tls_key; only tls_server_name.
        let mut webd = cosmix_config::node::WebdConfig::default();
        webd.tls_server_name = vec!["example.com".into()];
        // Pretend the CLI passed `--tls-cert` / `--tls-key`.
        let err = resolve_node_state(legacy_inputs(&webd, &td, cert, key, www))
            .expect_err("CLI-supplied self-signed must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("intermediate")
                || msg.contains("Intermediate")
                || msg.contains("path validation"),
            "expected LE-validator rejection, got: {msg}"
        );
    }

    #[test]
    fn legacy_pair_without_tls_server_name_rejected() {
        // Two parameterisations: served_mail_domains empty vs non-
        // empty. tls_server_name is the *web identity*, independent
        // of served_mail_domains (autoconfig admission). The
        // rejection must fire in both cases.
        for served_mail_domains in [Vec::<String>::new(), vec!["example.org".into()]] {
            let td = TempDir::new().unwrap();
            let www = mkdir(&td, "www");
            let (cert, key) = write_chain(&td, "example", &synth_wildcard_pem());
            let mut webd = cosmix_config::node::WebdConfig::default();
            webd.served_mail_domains = served_mail_domains.clone();
            // tls_server_name deliberately empty
            let err = resolve_node_state(legacy_inputs(&webd, &td, cert, key, www))
                .expect_err("legacy pair without tls_server_name must hard-fail");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("tls_server_name"),
                "expected error to name tls_server_name field (served_mail_domains={served_mail_domains:?}), got: {msg}"
            );
        }
    }

    #[test]
    fn webd_vhost_acme_without_http_listen_rejected() {
        // P2-C3: HTTP-01 cannot complete without a plain :80
        // listener. lib-config's HttpListenRequiredForAcme variant
        // names the first offending vhost; surface it through the
        // webd pipeline so the operator sees the exact row to fix.
        let td = TempDir::new().unwrap();
        let www = mkdir(&td, "www");
        let mut webd = cosmix_config::node::WebdConfig::default();
        // http_listen deliberately absent
        webd.vhost.push(cosmix_config::node::WebdVhostConfig {
            host: "site-a.example".into(),
            www_dir: www.to_string_lossy().into_owned(),
            acme: Some(cosmix_config::node::WebdVhostAcmeConfig {
                provider: cosmix_config::node::WebdAcmeProvider::LetsEncryptStaging,
                challenge: cosmix_config::node::WebdAcmeChallenge::Http01,
                contact_email: "ops@example.com".into(),
            }),
            ..Default::default()
        });
        let err = resolve_node_state(base_inputs(&webd, &td))
            .expect_err("acme row without http_listen must hard-fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("http_listen"),
            "expected http_listen pointer, got: {msg}"
        );
    }
}

#[cfg(test)]
mod cli_parser_tests {
    //! Clap-parser shape tests for the operator-facing subcommands
    //! added in the Phase 8e operator-CLI sweep (`vhost add`,
    //! `acme {renew,status}`). Each test exercises `Cli::try_parse_from`
    //! with no daemon or Bus plumbing — the goal is to lock the
    //! public argv shape so a later rename / required-flag drift
    //! surfaces as a test failure rather than as a runtime "no such
    //! argument" from operators. These tests do NOT exercise the
    //! cross-flag prevalidation in `run_vhost_cli` (the CLI-side
    //! ACME companion-without-provider rejection, the manual-TLS
    //! half-pair rejection, and the ACME-vs-manual mutual
    //! exclusion); provider-without-companions is left to the
    //! daemon, per the inline comments next to those checks. All
    //! of that lives downstream of the parser and would require
    //! either an integration harness or a refactor that lifts the
    //! prevalidation into a pure helper.
    use super::*;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    // --- vhost add -----------------------------------------------------

    #[test]
    fn vhost_add_parses_minimum_positionals() {
        let cli = parse(&[
            "cosmix-webd",
            "vhost",
            "add",
            "example.com",
            "/srv/www/example.com",
        ])
        .expect("vhost add <fqdn> <www_dir> should parse");
        match cli.command {
            Command::Vhost {
                action:
                    VhostAction::Add {
                        fqdn,
                        www_dir,
                        acme_provider,
                        acme_challenge,
                        acme_contact_email,
                        tls_cert_path,
                        tls_key_path,
                        disabled,
                    },
            } => {
                assert_eq!(fqdn, "example.com");
                assert_eq!(www_dir, "/srv/www/example.com");
                assert!(acme_provider.is_none());
                assert!(acme_challenge.is_none());
                assert!(acme_contact_email.is_none());
                assert!(tls_cert_path.is_none());
                assert!(tls_key_path.is_none());
                assert!(!disabled);
            }
            _ => panic!("expected Vhost::Add"),
        }
    }

    #[test]
    fn vhost_add_without_positionals_errors() {
        let Err(err) = parse(&["cosmix-webd", "vhost", "add"]) else {
            panic!("vhost add requires <fqdn> and <www_dir>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn vhost_add_without_www_dir_errors() {
        let Err(err) = parse(&["cosmix-webd", "vhost", "add", "example.com"]) else {
            panic!("vhost add requires <www_dir>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn vhost_add_accepts_full_acme_trio() {
        let cli = parse(&[
            "cosmix-webd",
            "vhost",
            "add",
            "example.com",
            "/srv/www/example.com",
            "--acme-provider",
            "letsencrypt-staging",
            "--acme-challenge",
            "http-01",
            "--acme-contact-email",
            "ops@example.com",
        ])
        .expect("vhost add with full ACME trio should parse");
        match cli.command {
            Command::Vhost {
                action:
                    VhostAction::Add {
                        acme_provider,
                        acme_challenge,
                        acme_contact_email,
                        ..
                    },
            } => {
                assert_eq!(acme_provider.as_deref(), Some("letsencrypt-staging"));
                assert_eq!(acme_challenge.as_deref(), Some("http-01"));
                assert_eq!(acme_contact_email.as_deref(), Some("ops@example.com"));
            }
            _ => panic!("expected Vhost::Add"),
        }
    }

    #[test]
    fn vhost_add_accepts_manual_tls_pair_and_disabled_flag() {
        let cli = parse(&[
            "cosmix-webd",
            "vhost",
            "add",
            "example.com",
            "/srv/www/example.com",
            "--tls-cert-path",
            "/etc/cosmix/webd/example.com.crt",
            "--tls-key-path",
            "/etc/cosmix/webd/example.com.key",
            "--disabled",
        ])
        .expect("vhost add with manual TLS + --disabled should parse");
        match cli.command {
            Command::Vhost {
                action:
                    VhostAction::Add {
                        tls_cert_path,
                        tls_key_path,
                        disabled,
                        ..
                    },
            } => {
                assert_eq!(
                    tls_cert_path.as_deref(),
                    Some("/etc/cosmix/webd/example.com.crt")
                );
                assert_eq!(
                    tls_key_path.as_deref(),
                    Some("/etc/cosmix/webd/example.com.key")
                );
                assert!(disabled);
            }
            _ => panic!("expected Vhost::Add"),
        }
    }

    // --- acme renew / status -------------------------------------------

    #[test]
    fn acme_renew_parses_with_fqdn() {
        let cli = parse(&["cosmix-webd", "acme", "renew", "example.com"])
            .expect("acme renew <fqdn> should parse");
        match cli.command {
            Command::Acme {
                action: AcmeAction::Renew { fqdn },
            } => {
                assert_eq!(fqdn, "example.com");
            }
            _ => panic!("expected Acme::Renew"),
        }
    }

    #[test]
    fn acme_status_parses_with_fqdn() {
        let cli = parse(&["cosmix-webd", "acme", "status", "example.com"])
            .expect("acme status <fqdn> should parse");
        match cli.command {
            Command::Acme {
                action: AcmeAction::Status { fqdn },
            } => {
                assert_eq!(fqdn, "example.com");
            }
            _ => panic!("expected Acme::Status"),
        }
    }

    #[test]
    fn acme_renew_without_fqdn_errors() {
        let Err(err) = parse(&["cosmix-webd", "acme", "renew"]) else {
            panic!("acme renew requires <fqdn>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn acme_status_without_fqdn_errors() {
        let Err(err) = parse(&["cosmix-webd", "acme", "status"]) else {
            panic!("acme status requires <fqdn>")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    // --- read-only inspection verbs ------------------------------------

    #[test]
    fn routes_list_parses() {
        let cli = parse(&["cosmix-webd", "routes", "list"]).expect("routes list should parse");
        assert!(matches!(
            cli.command,
            Command::Routes {
                action: RoutesAction::List
            }
        ));
    }

    #[test]
    fn stats_parses_as_leaf_command() {
        let cli = parse(&["cosmix-webd", "stats"]).expect("stats should parse as a leaf command");
        // `stats` is a leaf — no sub-action, no positional args. A
        // future drift that introduces a positional or sub-action
        // would break this binding.
        assert!(matches!(cli.command, Command::Stats));
    }

    #[test]
    fn tls_status_parses() {
        let cli = parse(&["cosmix-webd", "tls", "status"]).expect("tls status should parse");
        assert!(matches!(
            cli.command,
            Command::Tls {
                action: TlsAction::Status
            }
        ));
    }

    #[test]
    fn autoconfig_served_domains_parses() {
        let cli = parse(&["cosmix-webd", "autoconfig", "served-domains"])
            .expect("autoconfig served-domains should parse");
        assert!(matches!(
            cli.command,
            Command::Autoconfig {
                action: AutoconfigAction::ServedDomains
            }
        ));
    }

    #[test]
    fn routes_without_action_errors() {
        // `routes` alone is invalid — `list` is the only action and
        // clap should refuse to fill it in implicitly.
        let Err(err) = parse(&["cosmix-webd", "routes"]) else {
            panic!("`routes` requires a sub-action")
        };
        // Clap reports this as MissingSubcommand for a #[command(subcommand)]
        // arm that has no SubcommandRequired = false override.
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn tls_without_action_errors() {
        let Err(err) = parse(&["cosmix-webd", "tls"]) else {
            panic!("`tls` requires a sub-action")
        };
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }

    #[test]
    fn autoconfig_without_action_errors() {
        let Err(err) = parse(&["cosmix-webd", "autoconfig"]) else {
            panic!("`autoconfig` requires a sub-action")
        };
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ));
    }
}

// ---------------------------------------------------------------------------
// SSR PIM Phase 2 — session login/logout + cookie→Bearer seam e2e
// ---------------------------------------------------------------------------

#[cfg(test)]
mod session_login_tests {
    //! End-to-end coverage of the Phase 2 auth flow against a stub maild:
    //! GET /auth/login → POST /auth/login (pw→token exchange + sealed
    //! cookie) → GET an SSR `jmap()` handler (the cookie unseals to a
    //! `Bearer` that reaches the stub) → POST /auth/logout (maild revoke +
    //! cookies cleared). Plus the CSRF / bad-credential / no-session
    //! negative cases. The stub records the `Authorization` it sees so the
    //! "cookie becomes Bearer" chain is asserted, not assumed.
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::http::header;
    use base64::Engine as _;
    use http_body_util::BodyExt as _;
    use tempfile::TempDir;
    // `ServiceExt` (for `.oneshot`) comes in via `use super::*`.

    const EMAIL: &str = "user@pim.example";
    const PASSWORD: &str = "pw-correct";
    const MAILD_TOKEN: &str = "maild-token-abc123";
    const VHOST: &str = "pim.example";

    #[derive(Default)]
    struct StubState {
        /// `Authorization` header the stub's `/jmap` last saw.
        jmap_auth: Option<String>,
        /// `Authorization` headers the stub's revoke endpoint saw.
        revoke_auths: Vec<String>,
    }

    /// Spin a stub maild on an ephemeral port. `/auth/tokens/issue` mints
    /// `MAILD_TOKEN` for the right Basic creds (401 otherwise); `/jmap`
    /// records its `Authorization` and 200s only with a `Bearer`;
    /// `/auth/tokens/revoke` records its `Authorization` and 204s.
    async fn stub_maild() -> (String, Arc<Mutex<StubState>>) {
        let state = Arc::new(Mutex::new(StubState::default()));
        let s_issue = state.clone();
        let s_jmap = state.clone();
        let s_revoke = state.clone();
        let app = Router::new()
            .route(
                "/auth/tokens/issue",
                axum::routing::post(move |headers: HeaderMap| {
                    let _ = &s_issue;
                    async move {
                        // Basic-decode and check creds.
                        let ok = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|h| h.strip_prefix("Basic "))
                            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                            .and_then(|d| String::from_utf8(d).ok())
                            .map(|cred| cred == format!("{EMAIL}:{PASSWORD}"))
                            .unwrap_or(false);
                        if ok {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "token": MAILD_TOKEN,
                                    "account_id": 1,
                                    "expires_at": "2099-01-01 00:00:00",
                                })),
                            )
                                .into_response()
                        } else {
                            StatusCode::UNAUTHORIZED.into_response()
                        }
                    }
                }),
            )
            .route(
                "/jmap",
                axum::routing::post(move |headers: HeaderMap| {
                    let s = s_jmap.clone();
                    async move {
                        let auth = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|h| h.to_str().ok())
                            .map(str::to_string);
                        s.lock().unwrap().jmap_auth = auth.clone();
                        match auth {
                            Some(a) if a.starts_with("Bearer ") => (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "methodResponses": [
                                        ["Contact/query", {"ids": [], "total": 0}, "c0"]
                                    ],
                                    "sessionState": "0",
                                })),
                            )
                                .into_response(),
                            _ => StatusCode::UNAUTHORIZED.into_response(),
                        }
                    }
                }),
            )
            .route(
                "/auth/tokens/revoke",
                axum::routing::post(move |headers: HeaderMap| {
                    let s = s_revoke.clone();
                    async move {
                        if let Some(a) = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|h| h.to_str().ok())
                        {
                            s.lock().unwrap().revoke_auths.push(a.to_string());
                        }
                        StatusCode::NO_CONTENT
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    /// A single-vhost node whose `pim.example` vhost points `jmap_upstream`
    /// at the stub, with a `jmap`-capable SSR handler at `GET /pim/probe`
    /// that calls `jmap()` and returns `ok:<total>`.
    ///
    /// `mfa_break_glass: true` by default: this synthetic node has NO
    /// broker, so the P3.4 fail-closed 2FA lookup is permanently
    /// INDETERMINATE here — without break-glass every login refuses with
    /// `err=unavailable` (pinned by
    /// `login_with_indeterminate_mfa_fails_closed`).
    fn synth_jmap_node(td: &TempDir, stub: &str) -> Arc<NodeState> {
        synth_jmap_node_with(td, stub, true)
    }

    fn synth_jmap_node_with(td: &TempDir, stub: &str, mfa_break_glass: bool) -> Arc<NodeState> {
        let www = td.path().join("pim-www");
        std::fs::create_dir_all(&www).unwrap();
        // derive_handler_root(www) == www (not a `public` dir), so the
        // handler script resolves at www/probe.mix.
        std::fs::write(
            www.join("probe.mix"),
            "$r = jmap(\"Contact/query\", { accountId: \"1\" })\nreturn \"ok:\" .. (\"\" .. $r[\"total\"])\n",
        )
        .unwrap();

        // In-memory CMS DB (SCHEMA applied) so the session-epoch gate is
        // live in these tests, exactly as on a real cms_db_path vhost.
        let mem = Connection::open_in_memory().expect("in-memory cms db");
        mem.execute_batch(SCHEMA).expect("apply SCHEMA");
        let vhost = Arc::new(VhostState {
            fqdn: VHOST.into(),
            db: Some(Arc::new(tokio::sync::Mutex::new(mem))),
            www_dir: www,
            jmap_upstream: Some(stub.to_string()),
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        let directory =
            vhost_directory::VhostDirectory::build(vec![vhost_directory::VhostDirectoryEntry {
                state: vhost,
                aliases: vec![],
            }])
            .expect("synth_jmap_node: VhostDirectory::build");

        let handlers = mix_handler::HandlerTable::from_rows(vec![handlers_namespace::HandlerRow {
            route_id: "pim-probe".into(),
            vhost_fqdn: VHOST.into(),
            method: "GET".into(),
            path_pattern: "/pim/probe".into(),
            handler_kind: "mix".into(),
            handler_ref: "probe.mix".into(),
            enabled: true,
            capabilities: vec!["jmap".into()],
        }]);

        assemble_test_node(directory, handlers)
    }

    /// Fill the non-vhost `NodeState` fields with standard test defaults
    /// (ephemeral sealer, no broker, insecure http client) around a
    /// caller-built vhost directory + handler table. Shared by the
    /// single-vhost `synth_jmap_node_with` and the multi-vhost
    /// revocation-breadth test.
    fn assemble_test_node(
        directory: vhost_directory::VhostDirectory,
        handlers: mix_handler::HandlerTable,
    ) -> Arc<NodeState> {
        Arc::new(NodeState {
            service_jmap_tokens: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_throttle: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            login_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            vhosts: Arc::new(ArcSwap::from(Arc::new(directory))),
            http_client: reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap(),
            session: Arc::new(session::SessionSealer::ephemeral()),
            served_mail_domains: HashSet::new(),
            autoconfig_mail_host: None,
            mx: None,
            acme_challenges: Arc::new(RwLock::new(HashMap::new())),
            tls_status_rx: tokio::sync::watch::channel(tls_status::TlsStatusSnapshot::default()).1,
            props_router: Arc::new(cosmix_props::PropsRouter::new("webd")),
            props_subscribe_granter: Arc::new(bus::subscribe_granter::NodedSubscribeGranter::new(
                bus::subscribe_granter::new_broker_handle(),
            )),
            broker_handle: bus::subscribe_granter::new_broker_handle(),
            vhosts_runtime: None,
            listeners_runtime: None,
            listeners_operators: Vec::new(),
            vhost_key_locks: None,
            acme_notify: None,
            acme_force_renew_queue: None,
            handlers: Arc::new(ArcSwap::from(Arc::new(handlers))),
            handler_ast_cache: mix_handler::new_ast_cache(),
            tls_reload: None,
        })
    }

    fn get(host: &str, path: &str, cookies: &[(&str, &str)]) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder()
            .method("GET")
            .uri(path)
            .header(header::HOST, host);
        if !cookies.is_empty() {
            b = b.header(header::COOKIE, cookie_header(cookies));
        }
        b.body(Body::empty()).unwrap()
    }

    fn post_form(
        host: &str,
        path: &str,
        cookies: &[(&str, &str)],
        body: &str,
    ) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if !cookies.is_empty() {
            b = b.header(header::COOKIE, cookie_header(cookies));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn cookie_header(cookies: &[(&str, &str)]) -> String {
        cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Pull a `Set-Cookie` value by name from a response.
    fn set_cookie(resp: &axum::http::Response<Body>, name: &str) -> Option<String> {
        resp.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .find_map(|hv| {
                let first = hv.split(';').next()?;
                let (k, v) = first.split_once('=')?;
                if k.trim() == name {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
    }

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn full_login_jmap_logout_flow() {
        let td = TempDir::new().unwrap();
        let (stub, state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);

        // (1) GET /auth/login → 303 to the Mix /login page + a readable csrf
        //     cookie (the legacy entry point is now a PRG bounce).
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/auth/login", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
        let csrf = set_cookie(&resp, session::CSRF_COOKIE).expect("login GET sets csrf cookie");

        // (2) POST /auth/login with matching csrf → 303 + sealed session.
        let form = format!("email={EMAIL}&password={PASSWORD}&csrf={csrf}&next=/pim/probe");
        let resp = build_router(node.clone())
            .oneshot(post_form(
                VHOST,
                "/auth/login",
                &[(session::CSRF_COOKIE, &csrf)],
                &form,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "login redirects on success"
        );
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/pim/probe",
            "redirects to the validated next path"
        );
        let session_cookie =
            set_cookie(&resp, session::SESSION_COOKIE).expect("login sets a session cookie");
        let post_csrf =
            set_cookie(&resp, session::CSRF_COOKIE).expect("login refreshes the csrf cookie");

        // (3) GET the SSR jmap() handler with the session cookie → 200, and
        //     the stub /jmap saw `Bearer <maild-token>` (the cookie became
        //     the Authorization).
        let resp = build_router(node.clone())
            .oneshot(get(
                VHOST,
                "/pim/probe",
                &[(session::SESSION_COOKIE, &session_cookie)],
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "SSR handler authorises via cookie"
        );
        assert_eq!(body_string(resp).await, "ok:0");
        assert_eq!(
            state.lock().unwrap().jmap_auth.as_deref(),
            Some(format!("Bearer {MAILD_TOKEN}").as_str()),
            "the sealed cookie was forwarded to maild as a Bearer token"
        );

        // (4) POST /auth/logout with the session cookie + the post-auth
        //     csrf → 303, cookies cleared, and maild revoke called with the
        //     same Bearer.
        let resp = build_router(node.clone())
            .oneshot(post_form(
                VHOST,
                "/auth/logout",
                &[(session::SESSION_COOKIE, &session_cookie)],
                &format!("csrf={post_csrf}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        // Session cookie cleared (Max-Age=0 → empty value).
        assert_eq!(
            set_cookie(&resp, session::SESSION_COOKIE).as_deref(),
            Some("")
        );
        assert_eq!(
            state.lock().unwrap().revoke_auths,
            vec![format!("Bearer {MAILD_TOKEN}")],
            "logout revoked the token at maild"
        );
    }

    /// Single-vhost node whose GET `/cached` route is `public_cache`+`db` and
    /// renders `SELECT value FROM pc_kv WHERE k='x'` (seeded `v1`). Returns the
    /// db handle so a test can mutate the row OUT OF BAND (no POST → no
    /// generation bump) to distinguish a cache hit from a fresh render.
    fn synth_public_cache_node(
        td: &TempDir,
    ) -> (Arc<NodeState>, Arc<tokio::sync::Mutex<Connection>>) {
        let www = td.path().join("pc-www");
        std::fs::create_dir_all(&www).unwrap();
        std::fs::write(
            www.join("pubcache.mix"),
            "$rows = db_query(\"SELECT value FROM pc_kv WHERE k = 'x'\", [])\n\
             print(\"<p>\" .. $rows[0][\"value\"] .. \"</p>\")\n",
        )
        .unwrap();
        // A handler that REFLECTS the request body — used to prove a cacheable
        // render sees an empty `$BODY` (bodied-GET cache-poisoning defence).
        std::fs::write(
            www.join("reflect.mix"),
            "print(\"<p>[\" .. $BODY .. \"]</p>\")\n",
        )
        .unwrap();

        let mem = Connection::open_in_memory().unwrap();
        mem.execute_batch(SCHEMA).unwrap();
        mem.execute_batch(
            "CREATE TABLE pc_kv (k TEXT PRIMARY KEY, value TEXT);\n\
             INSERT INTO pc_kv (k, value) VALUES ('x', 'v1');",
        )
        .unwrap();
        let db = Arc::new(tokio::sync::Mutex::new(mem));

        let vhost = Arc::new(VhostState {
            fqdn: VHOST.into(),
            db: Some(db.clone()),
            www_dir: www,
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
            stats: Arc::new(stats::WebdStats::new()),
            session_epoch_cache: SessionEpochCache::default(),
            public_response_cache: public_response_cache::Cache::default(),
        });
        let directory =
            vhost_directory::VhostDirectory::build(vec![vhost_directory::VhostDirectoryEntry {
                state: vhost,
                aliases: vec![],
            }])
            .unwrap();
        let handlers = mix_handler::HandlerTable::from_rows(vec![
            handlers_namespace::HandlerRow {
                route_id: "pc".into(),
                vhost_fqdn: VHOST.into(),
                method: "GET".into(),
                path_pattern: "/cached".into(),
                handler_kind: "mix".into(),
                handler_ref: "pubcache.mix".into(),
                enabled: true,
                capabilities: vec!["db".into(), "public_cache".into()],
            },
            handlers_namespace::HandlerRow {
                route_id: "reflect".into(),
                vhost_fqdn: VHOST.into(),
                method: "GET".into(),
                path_pattern: "/reflect".into(),
                handler_kind: "mix".into(),
                handler_ref: "reflect.mix".into(),
                enabled: true,
                capabilities: vec!["public_cache".into()],
            },
        ]);
        (assemble_test_node(directory, handlers), db)
    }

    /// The public-response cache serves anonymous readers a cached render but
    /// NEVER serves it to (or stores) a request carrying a session cookie — the
    /// core auth-boundary property of the `public_cache` capability.
    #[tokio::test]
    async fn public_cache_serves_anon_hits_but_bypasses_cookied_requests() {
        let td = TempDir::new().unwrap();
        let (node, db) = synth_public_cache_node(&td);

        // (1) Anon GET → renders + caches "v1".
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/cached", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("v1"));

        // (2) Mutate the data OUT OF BAND (no POST → no generation bump), so a
        //     FRESH render would now show "v2" but a cache HIT still shows "v1".
        db.lock()
            .await
            .execute("UPDATE pc_kv SET value = 'v2' WHERE k = 'x'", [])
            .unwrap();

        // (3) Anon GET again → STILL "v1": proves the response was served from
        //     cache (the handler + DB were not re-run).
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/cached", &[]))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("v1") && !body.contains("v2"),
            "anon GET is a cache hit (got {body})"
        );

        // (4) SECURITY: a request carrying ANY session cookie is NOT a cache
        //     candidate → it renders fresh ("v2"), never the shared anon entry.
        let resp = build_router(node.clone())
            .oneshot(get(
                VHOST,
                "/cached",
                &[(session::SESSION_COOKIE, "bogus-not-a-real-session")],
            ))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("v2"),
            "a cookied request bypasses the anon cache and renders fresh (got {body})"
        );

        // (5) The cookied bypass neither read from nor wrote to the anon cache:
        //     a fresh anonymous GET still hits the original "v1".
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/cached", &[]))
            .await
            .unwrap();
        assert!(
            body_string(resp).await.contains("v1"),
            "the anon cache is intact after a cookied bypass"
        );
    }

    /// A cacheable render must see an EMPTY `$BODY`: a bodied GET (legal HTTP)
    /// cannot poison the cached bytes reflected to ordinary bodyless GETs.
    #[tokio::test]
    async fn public_cache_strips_get_body_to_prevent_poisoning() {
        let td = TempDir::new().unwrap();
        let (node, _db) = synth_public_cache_node(&td);

        // (1) Attacker sends a GET WITH a body to a cacheable route.
        let bodied = axum::http::Request::builder()
            .method("GET")
            .uri("/reflect")
            .header(header::HOST, VHOST)
            .body(Body::from("PWNED-PAYLOAD"))
            .unwrap();
        let resp = build_router(node.clone()).oneshot(bodied).await.unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("[]") && !body.contains("PWNED"),
            "the GET body is canonicalized away before render (got {body})"
        );

        // (2) An ordinary bodyless GET hits the cache and sees the same SAFE
        //     bytes — the attacker's body never reached the shared entry.
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/reflect", &[]))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            !body.contains("PWNED"),
            "the cached entry was never poisoned by the bodied GET (got {body})"
        );
    }

    /// Cookie-path revocation (2026-07 audit): a live, valid session
    /// cookie dies the moment the account's session epoch is bumped —
    /// the same UPDATE `webd.session.revoke` performs — behaving exactly
    /// like NO cookie (500 from the jmap probe, no Bearer reaching maild).
    #[tokio::test]
    async fn revoked_epoch_kills_live_cookie_session() {
        let td = TempDir::new().unwrap();
        let (stub, state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);

        // Login (break-glass fixture path) → sealed session cookie.
        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/auth/login", &[]))
            .await
            .unwrap();
        let csrf = set_cookie(&resp, session::CSRF_COOKIE).unwrap();
        let form = format!("email={EMAIL}&password={PASSWORD}&csrf={csrf}&next=/pim/probe");
        let resp = build_router(node.clone())
            .oneshot(post_form(
                VHOST,
                "/auth/login",
                &[(session::CSRF_COOKIE, &csrf)],
                &form,
            ))
            .await
            .unwrap();
        let session_cookie =
            set_cookie(&resp, session::SESSION_COOKIE).expect("login sets a session cookie");

        // Sanity: the cookie authorises the SSR jmap probe.
        let resp = build_router(node.clone())
            .oneshot(get(
                VHOST,
                "/pim/probe",
                &[(session::SESSION_COOKIE, &session_cookie)],
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "pre-revocation probe authorises"
        );

        // Bump the epoch — mirroring what `webd.session.revoke` now does:
        // clear the epoch-cache slot, THEN run the exact UPDATE. The raw
        // UPDATE alone would leave the 2s epoch-cache hit (populated by the
        // pre-revocation probe above) serving the stale pre-revocation epoch.
        {
            let dir = node.vhosts.load();
            let vh = dir.by_host.get(VHOST).unwrap().clone();
            let slot = vh.session_epoch_cache.slot(EMAIL).await;
            let mut cached = slot.lock().await;
            *cached = None;
            let db = vh.db.as_ref().unwrap().lock().await;
            db.execute(
                "INSERT INTO session_epochs (email, epoch) VALUES (?1, 1) \
                 ON CONFLICT(email) DO UPDATE SET epoch = epoch + 1",
                rusqlite::params![EMAIL],
            )
            .unwrap();
        }

        // The SAME cookie is now dead: identical to no cookie at all.
        state.lock().unwrap().jmap_auth = None;
        let resp = build_router(node.clone())
            .oneshot(get(
                VHOST,
                "/pim/probe",
                &[(session::SESSION_COOKIE, &session_cookie)],
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "revoked cookie behaves like no session"
        );
        assert_eq!(
            state.lock().unwrap().jmap_auth,
            None,
            "no Bearer reached maild under a revoked cookie"
        );
    }

    /// Multi-vhost revocation breadth (2026-07 audit — the per-vhost
    /// `session_epochs` table means a revoke is only complete if EVERY
    /// vhost the account touches is bumped). An account with a live cookie
    /// in TWO vhosts on one node: a single `webd.session.revoke` (the real
    /// verb, iterating all directory primaries) must kill BOTH cookies —
    /// the case where a per-vhost table plus an incomplete enumeration
    /// would leave one door open.
    #[tokio::test]
    async fn revoke_verb_kills_cookies_in_every_vhost() {
        const HOST_A: &str = "a.pim.example";
        const HOST_B: &str = "b.pim.example";

        let td = TempDir::new().unwrap();
        let (stub, _state) = stub_maild().await;
        let www = td.path().join("www");
        std::fs::create_dir_all(&www).unwrap();
        std::fs::write(
            www.join("probe.mix"),
            "$r = jmap(\"Contact/query\", { accountId: \"1\" })\nreturn \"ok:\" .. (\"\" .. $r[\"total\"])\n",
        )
        .unwrap();

        // Two vhosts, each with its OWN in-memory CMS DB (independent
        // session_epochs tables) and the jmap probe handler.
        let mk_db = || {
            let c = Connection::open_in_memory().unwrap();
            c.execute_batch(SCHEMA).unwrap();
            Arc::new(tokio::sync::Mutex::new(c))
        };
        let vhost = |fqdn: &str| {
            Arc::new(VhostState {
                fqdn: fqdn.into(),
                db: Some(mk_db()),
                www_dir: www.clone(),
                jmap_upstream: Some(stub.clone()),
                noded_ws: None,
                docs_dir: None,
                dev_session_email: None,
                dev_session_password: None,
                public_read_email: None,
                public_read_password: None,
                system_sender_email: None,
                system_sender_password: None,
                mfa_break_glass: true,
                stats: Arc::new(stats::WebdStats::new()),
                session_epoch_cache: SessionEpochCache::default(),
                public_response_cache: public_response_cache::Cache::default(),
            })
        };
        let directory = vhost_directory::VhostDirectory::build(vec![
            vhost_directory::VhostDirectoryEntry {
                state: vhost(HOST_A),
                aliases: vec![],
            },
            vhost_directory::VhostDirectoryEntry {
                state: vhost(HOST_B),
                aliases: vec![],
            },
        ])
        .unwrap();
        let handler_row = |route: &str, fqdn: &str| handlers_namespace::HandlerRow {
            route_id: route.into(),
            vhost_fqdn: fqdn.into(),
            method: "GET".into(),
            path_pattern: "/pim/probe".into(),
            handler_kind: "mix".into(),
            handler_ref: "probe.mix".into(),
            enabled: true,
            capabilities: vec!["jmap".into()],
        };
        let handlers = mix_handler::HandlerTable::from_rows(vec![
            handler_row("pa", HOST_A),
            handler_row("pb", HOST_B),
        ]);
        let node = assemble_test_node(directory, handlers);

        // Seal an epoch-0 cookie per vhost (fresh DB ⇒ live epoch 0), the
        // shape login mints. Both must authorise the SSR jmap probe.
        let seal = |fqdn: &str| {
            let now = session::now_secs();
            node.session
                .seal(&session::SessionPayload {
                    vhost: fqdn.into(),
                    maild_token: MAILD_TOKEN.into(),
                    email: EMAIL.into(),
                    iat: now,
                    exp: now + 3600,
                    csrf: "csrf".into(),
                    epoch: 0,
                    kind: "maild".into(),
                    customer_id: 0,
                })
                .unwrap()
        };
        let cookie_a = seal(HOST_A);
        let cookie_b = seal(HOST_B);

        for (host, cookie) in [(HOST_A, &cookie_a), (HOST_B, &cookie_b)] {
            let resp = build_router(node.clone())
                .oneshot(get(
                    host,
                    "/pim/probe",
                    &[(session::SESSION_COOKIE, cookie)],
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{host}: pre-revoke authorises"
            );
        }

        // ONE revoke through the REAL verb — it must iterate every primary.
        let mut cmd_headers = std::collections::BTreeMap::new();
        cmd_headers.insert("email".to_string(), EMAIL.to_string());
        let cmd = cosmix_client::IncomingCommand {
            from: String::new(),
            command: "webd.session.revoke".into(),
            id: None,
            args: serde_json::Value::Null,
            body: String::new(),
            headers: cmd_headers,
        };
        let (rc, body) = bus::session_verbs::dispatch("session.revoke", &cmd, &node)
            .await
            .expect("session.revoke dispatches");
        assert_eq!(rc, 0, "revoke rc=0; body={body}");
        // Both vhosts appear in the revoked set.
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let revoked = v["revoked"].as_array().unwrap();
        assert_eq!(revoked.len(), 2, "both vhosts bumped; body={body}");

        // Both cookies are now dead — no door left open.
        for (host, cookie) in [(HOST_A, &cookie_a), (HOST_B, &cookie_b)] {
            let resp = build_router(node.clone())
                .oneshot(get(
                    host,
                    "/pim/probe",
                    &[(session::SESSION_COOKIE, cookie)],
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{host}: cookie dead after one revoke across all vhosts",
            );
        }
    }

    /// P3.4 (2026-07 audit): with NO broker and NO break-glass, a correct
    /// password still refuses with `err=unavailable` — the 2FA enrollment
    /// lookup is indeterminate and MUST fail closed, and the bearer maild
    /// just issued is best-effort revoked (never sealed, never disclosed).
    #[tokio::test]
    async fn login_with_indeterminate_mfa_fails_closed() {
        let td = TempDir::new().unwrap();
        let (stub, state) = stub_maild().await;
        let node = synth_jmap_node_with(&td, &stub, false);

        let resp = build_router(node.clone())
            .oneshot(get(VHOST, "/auth/login", &[]))
            .await
            .unwrap();
        let csrf = set_cookie(&resp, session::CSRF_COOKIE).expect("login GET sets csrf cookie");

        let form = format!("email={EMAIL}&password={PASSWORD}&csrf={csrf}&next=/pim/probe");
        let resp = build_router(node.clone())
            .oneshot(post_form(
                VHOST,
                "/auth/login",
                &[(session::CSRF_COOKIE, &csrf)],
                &form,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            location.starts_with("/login?err=unavailable"),
            "fail-closed refusal, got {location}"
        );
        assert!(
            set_cookie(&resp, session::SESSION_COOKIE).is_none(),
            "no session sealed under an indeterminate 2FA lookup"
        );
        assert_eq!(
            state.lock().unwrap().revoke_auths,
            vec![format!("Bearer {MAILD_TOKEN}")],
            "the just-issued bearer was best-effort revoked"
        );
    }

    #[tokio::test]
    async fn login_rejects_csrf_mismatch() {
        let td = TempDir::new().unwrap();
        let (stub, _state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);
        // Cookie csrf and form csrf differ → 303 PRG to /login?err=expired, no
        // session minted (the form re-renders on the Mix page with a fresh token).
        let form = format!("email={EMAIL}&password={PASSWORD}&csrf=form-token&next=/");
        let resp = build_router(node)
            .oneshot(post_form(
                VHOST,
                "/auth/login",
                &[(session::CSRF_COOKIE, "cookie-token")],
                &form,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login?err=expired"
        );
        assert!(set_cookie(&resp, session::SESSION_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_rejects_bad_credentials() {
        let td = TempDir::new().unwrap();
        let (stub, _state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);
        let form = "email=user@pim.example&password=WRONG&csrf=tok&next=/";
        let resp = build_router(node)
            .oneshot(post_form(
                VHOST,
                "/auth/login",
                &[(session::CSRF_COOKIE, "tok")],
                form,
            ))
            .await
            .unwrap();
        // maild 401 → 303 PRG to /login?err=invalid (uniform with the other
        // failure branches; no account-existence oracle), no session minted.
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "maild 401 surfaces as a PRG redirect"
        );
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login?err=invalid"
        );
        assert!(set_cookie(&resp, session::SESSION_COOKIE).is_none());
    }

    #[tokio::test]
    async fn ssr_handler_without_session_is_unauthorised_upstream() {
        let td = TempDir::new().unwrap();
        let (stub, state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);
        // No session cookie → seam injects no auth → stub /jmap 401 →
        // jmap() raises → handler errors → 500. Importantly, the stub saw
        // a request with NO Bearer (proving the cookie is the only auth
        // source — no ambient header leaks in).
        let resp = build_router(node)
            .oneshot(get(VHOST, "/pim/probe", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            state.lock().unwrap().jmap_auth,
            None,
            "no Authorization reached maild without a session cookie"
        );
    }

    #[test]
    fn safe_next_blocks_open_redirects() {
        // Same-origin absolute paths pass through.
        assert_eq!(safe_next("/pim/contacts"), "/pim/contacts");
        assert_eq!(safe_next("/"), "/");
        // Off-origin / smuggled targets collapse to "/".
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("/\\evil.example"), "/");
        assert_eq!(safe_next("https://evil.example"), "/");
        assert_eq!(safe_next("/path\\with\\backslash"), "/");
        assert_eq!(safe_next("/inject\r\nSet-Cookie: x=1"), "/");
        assert_eq!(safe_next("relative"), "/");
        assert_eq!(safe_next(""), "/");
    }

    #[tokio::test]
    async fn logout_without_session_is_idempotent() {
        let td = TempDir::new().unwrap();
        let (stub, state) = stub_maild().await;
        let node = synth_jmap_node(&td, &stub);
        // No session cookie → logout just clears + redirects, no revoke.
        let resp = build_router(node)
            .oneshot(post_form(VHOST, "/auth/logout", &[], "csrf=whatever"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(state.lock().unwrap().revoke_auths.is_empty());
    }
}

#[cfg(test)]
mod jmap_proxy_tests {
    //! Part B — the JMAP reverse proxy's blob/eventsource streaming + JSON
    //! buffering split, auth propagation, and the streamed-upload byte cap.
    //! Each test drives `proxy_jmap` directly against a stub maild on an
    //! ephemeral port (no NodeState/VhostState needed).
    use super::*;
    use axum::Router;
    use axum::routing::{get, post};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Default, Clone)]
    struct Captured {
        authorization: Option<String>,
        had_cookie: bool,
        upload_body: Vec<u8>,
        saw_x_secret: bool,
    }

    /// Stub maild with the four endpoints `proxy_jmap` routes between. Returns
    /// the base URL + a capture handle (auth header seen, cookie presence,
    /// upload body). The blob endpoint returns a 512 KiB body to exercise the
    /// streaming path; the eventsource returns a finite SSE body promptly.
    async fn stub_maild() -> (String, Arc<Mutex<Captured>>) {
        let cap: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Captured::default()));
        let (cj, cu) = (cap.clone(), cap.clone());
        let app = Router::new()
            .route(
                "/jmap",
                post(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let cap = cj.clone();
                    async move {
                        let mut g = cap.lock().await;
                        g.authorization = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        g.had_cookie = headers.contains_key("cookie");
                        g.saw_x_secret = headers.contains_key("x-secret");
                        Json(serde_json::json!({
                            "methodResponses": [],
                            "echo": String::from_utf8_lossy(&body),
                        }))
                    }
                }),
            )
            .route(
                "/jmap/blob/{id}",
                get(|| async {
                    let data = vec![b'x'; 512 * 1024];
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                        data,
                    )
                }),
            )
            .route(
                "/jmap/upload/{id}",
                post(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let cap = cu.clone();
                    async move {
                        let mut g = cap.lock().await;
                        g.authorization = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        g.upload_body = body.to_vec();
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({ "blobId": "B1" })),
                        )
                    }
                }),
            )
            .route(
                "/jmap/eventsource",
                get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "event: ping\ndata: 1\n\n",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), cap)
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    /// JSON method calls stay buffered + JSON-shaped; the Authorization header
    /// is forwarded and the (webd-only) Cookie header is dropped.
    #[tokio::test]
    async fn json_buffered_auth_forwarded_cookie_dropped() {
        let (base, cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/jmap")
            .header("authorization", "Bearer tok")
            .header("cookie", "cosmix_session=secret")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"using":[],"methodCalls":[]}"#))
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy ok");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        assert!(String::from_utf8_lossy(&body).contains("methodResponses"));
        let g = cap.lock().await;
        assert_eq!(g.authorization.as_deref(), Some("Bearer tok"));
        assert!(
            !g.had_cookie,
            "the sealed session cookie must not cross to maild"
        );
    }

    /// A blob download streams through intact (no 10 MiB JSON cap, no parse).
    #[tokio::test]
    async fn blob_download_streams_through() {
        let (base, _cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/jmap/blob/Gabc")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy ok");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/octet-stream"
        );
        let body = body_bytes(resp).await;
        assert_eq!(body.len(), 512 * 1024);
        assert!(body.iter().all(|&b| b == b'x'));
    }

    /// The eventsource streams (the old resp.bytes() buffer-the-whole-body path
    /// would hang on a never-ending stream); content-type preserved.
    #[tokio::test]
    async fn eventsource_streams_with_content_type() {
        let (base, _cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/jmap/eventsource")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy ok");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/event-stream"
        );
        assert_eq!(body_bytes(resp).await, b"event: ping\ndata: 1\n\n");
    }

    /// A blob upload streams to maild intact, auth forwarded.
    #[tokio::test]
    async fn upload_streams_body_to_maild() {
        let (base, cap) = stub_maild().await;
        let payload = vec![b'A'; 200 * 1024];
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/jmap/upload/_")
            .header("authorization", "Bearer up")
            .header("content-type", "message/rfc822")
            .body(Body::from(payload.clone()))
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy ok");
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert!(String::from_utf8_lossy(&body_bytes(resp).await).contains("B1"));
        let g = cap.lock().await;
        assert_eq!(g.upload_body, payload);
        assert_eq!(g.authorization.as_deref(), Some("Bearer up"));
    }

    /// The streamed upload is byte-capped: a body past the cap aborts the
    /// upstream request (the stream yields an error mid-send) rather than
    /// pushing unbounded bytes through.
    #[tokio::test]
    async fn upload_over_cap_is_rejected() {
        let (base, _cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/jmap/upload/_")
            .header("content-type", "application/octet-stream")
            .body(Body::from(vec![b'Z'; 4096]))
            .unwrap();
        // cap = 1 KiB, body = 4 KiB → the capping stream errors, failing the send.
        let r = proxy_jmap(&reqwest::Client::new(), &base, req, 1024).await;
        assert!(r.is_err(), "an over-cap upload must fail, got {r:?}");
    }

    /// A path carrying a dot-segment is rejected with 400 BEFORE being joined
    /// into the upstream URL (where the url crate would normalize it and desync
    /// the route classification from the path actually hit).
    #[tokio::test]
    async fn dot_segment_path_rejected() {
        let (base, _cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/jmap/blob/../secret")
            .body(Body::empty())
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy returns a response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A header nominated hop-by-hop by the request's own `Connection` header is
    /// NOT forwarded to maild (RFC 7230 §6.1).
    #[tokio::test]
    async fn connection_nominated_header_dropped() {
        let (base, cap) = stub_maild().await;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/jmap")
            .header("connection", "x-secret")
            .header("x-secret", "leak")
            .body(Body::from("{}"))
            .unwrap();
        let resp = proxy_jmap(&reqwest::Client::new(), &base, req, JMAP_BLOB_UPLOAD_CAP)
            .await
            .expect("proxy ok");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            !cap.lock().await.saw_x_secret,
            "a Connection-nominated header must not be forwarded"
        );
    }
}
