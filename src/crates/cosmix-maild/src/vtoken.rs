//! vtoken registry — the maild-owned store + per-message resolver for
//! opaque (`<token>@<domain>`) sender-locked addresses.
//!
//! Plan: `~/.cosmix/_plan/2026-06-21-vtoken-dispatch-implementation.md`
//! (Stage A) + `~/.cmctl/_doc/2026-06-21-email-gateway-vtoken.md` (the scheme).
//!
//! **The hot path is Rust, the dispatch policy is Mix.** maild looks the
//! token up here (one indexed SQLite read), validates the sender-lock, and
//! injects `$VTOKEN_{VALID,USER,SERVICE}` into `inbound-filter.mix`, which
//! branches on the service to decide delivery. This module owns the registry +
//! validation (the trust-critical half); it never decides *where* a service
//! delivers — that's the filter.
//!
//! Storage is a dedicated SQLite file (the `rule_stats_store` pattern):
//! O(1) lookup by HMAC, idempotent schema migration, opened once at startup
//! and shared via `Arc`. It is *operational config*, not user content (the
//! framework storage-split rule), so it is plain SQLite, not an mds container.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::Mutex;

use crate::vtoken_secret::VtokenSecret;

/// On-disk schema version. v4 drops the now-defunct segmented tables
/// (`vtoken_registry`, `vtoken_failctl`). C9c re-minted all live tokens as
/// opaque tokens before this migration runs (C10), so dropping them is safe.
const CURRENT_SCHEMA_VERSION: i64 = 4;

/// Bound on a service NAME (the value in the `service` column; the filter
/// compares it with `==`). Not an address segment, but still bounded + ASCII.
const MAX_SERVICE_NAME_LEN: usize = 64;
/// How many `token_hmac` collisions [`VtokenStore::mint_opaque`] tolerates
/// before giving up. At ~30–50 bits of entropy a single collision is
/// astronomically unlikely; the cap is a runaway guard, not an expected path.
const MINT_MAX_RETRIES: usize = 16;

/// Whether a service NAME is valid: non-empty, bounded, `[a-z0-9_]` only, so
/// a typo can't smuggle whitespace/control bytes into a filter comparison.
pub fn is_valid_service_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SERVICE_NAME_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Canonicalize a bare addr-spec (`localpart@domain`) into the single comparable
/// form the vtoken **sender-lock** uses on BOTH sides — the message `From` and
/// the token's stored `allowed_sender` — so display-name / case / IDN noise can't
/// smuggle a mismatch. Conservative, like `email_domain_strict`: exactly one
/// `@`, non-empty halves, no whitespace/control bytes, domain ASCII-lowercased
/// with no leading/trailing/double dot. The local-part is lowercased too: RFC
/// 5321 leaves local-part case to the host, but every mailbox maild serves is
/// case-insensitive (`props::aliases` canon, `get_by_email`), so folding both
/// sides identically is correct and blocks a case-smuggled bypass.
///
/// Returns `None` on any malformed input → the caller fails closed. NOTE: no
/// IDNA U-label folding (no `idna` dep); a Unicode domain is lowercased
/// byte-wise. Because both sides canonicalize identically, an A-label vs U-label
/// difference fails closed (drop), never open.
pub fn canonical_addr(addr: &str) -> Option<String> {
    let addr = addr.trim();
    let (local, domain) = addr.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    if addr.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let domain = domain.to_ascii_lowercase();
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return None;
    }
    Some(format!("{}@{}", local.to_ascii_lowercase(), domain))
}

/// Canonicalize a recipient domain for the opaque-token HMAC: trim,
/// ASCII-lowercase, reject empty / leading-or-trailing/double dot / whitespace /
/// control. The SAME form is used at mint AND resolve so a domain-bound token
/// always rehashes identically. Lowercasing-only, like maild's existing routing
/// (no IDN U-label folding — see [`canonical_addr`]'s note).
fn canonical_domain(domain: &str) -> Option<String> {
    let d = domain.trim().to_ascii_lowercase();
    if d.is_empty()
        || d.starts_with('.')
        || d.ends_with('.')
        || d.contains("..")
        || d.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(d)
}

/// The opaque-token alphabet: lowercase Crockford base32 (0-9 + a-z minus
/// i/l/o/u — 32 unambiguous glyphs, 5 bits each). An opaque token is a
/// fixed-length string over THIS set, which is how a single-segment local-part
/// is recognised as a token rather than a normal mailbox.
const OPAQUE_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Opaque token length by verification strength (the design's L=6 / L=10):
/// `local-auth` routing tokens are 6 chars (~30 bit — safe because sender-lock is
/// the primary auth and the MSA authenticated the mailbox owner); `external-dmarc`
/// (DMARC proves only the domain) and any unknown tier get 10 chars (~50 bit,
/// fail-safe to the longer).
fn opaque_len_for(verification_strength: &str) -> usize {
    match verification_strength {
        "local-auth" => 6,
        _ => 10,
    }
}

/// Whether a recipient local-part is OPAQUE-token-shaped: a single segment (no
/// `-`), an opaque length (6 or 10), every byte in [`OPAQUE_ALPHABET`]. A cheap
/// SHAPE pre-filter — a match triggers the HMAC lookup; it does NOT assert the
/// token exists. (The reserved-namespace + uniform-accept behaviour is C8; for C6
/// a shape match that misses the registry falls through to normal delivery, so a
/// real mailbox of this shape is not swallowed.) Disjoint from the old segmented
/// parser, which required a `-`.
pub fn is_opaque_shaped(local: &str) -> bool {
    matches!(local.len(), 6 | 10) && local.bytes().all(|b| OPAQUE_ALPHABET.contains(&b))
}

/// A random opaque token of `len` chars over [`OPAQUE_ALPHABET`] (OsRng,
/// unbiased — 32 divides 256, so masking the low 5 bits is uniform). The token is
/// the secret, shown once; only its HMAC is stored.
fn gen_opaque(len: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter()
        .map(|b| OPAQUE_ALPHABET[(b & 0x1f) as usize] as char)
        .collect()
}

/// Whether a recipient is in the ACTIVE token namespace for THIS server config —
/// an opaque vtoken only when opaque acceptance is enabled (the C9 pre-flight
/// flag). When opaque is disabled (a pre-flight account/alias shape collision, or
/// a read error) an opaque-shaped recipient is a NORMAL mailbox — normal rate
/// handling, normal synchronous delivery, normal routing — so a real mailbox of
/// the opaque shape is never funnelled into the token namespace and silently
/// dropped. Used for the C7 rate cap and the C8 fire-and-forget gate; the resolve
/// path applies the same flag via [`VtokenStore::resolve_gated`].
pub fn is_active_token_shaped(rcpt: &str, opaque_enabled: bool) -> bool {
    opaque_enabled
        && rcpt
            .rsplit_once('@')
            .is_some_and(|(local, _)| is_opaque_shaped(local))
}

/// A validity-BLIND throughput cap on the token-shaped namespace (C7). Bounds the
/// accept-everything funnel so an attacker can't force unbounded per-message
/// resolve/HMAC work, WITHOUT leaking validity — the SAME per-source and global
/// limits apply to valid and invalid token-shaped recipients identically. A
/// fixed-window counter per source IP plus a global ceiling. (The full pre-DATA
/// shed valve + per-namespace max_message_size are C8's concern; this bounds the
/// per-recipient resolve work in the meantime.)
pub struct TokenRateLimiter {
    state: std::sync::Mutex<RateWindow>,
    per_ip_limit: u32,
    global_limit: u32,
    window: std::time::Duration,
}

struct RateWindow {
    started: std::time::Instant,
    global: u32,
    per_ip: std::collections::HashMap<std::net::IpAddr, u32>,
}

impl TokenRateLimiter {
    pub fn new(per_ip_limit: u32, global_limit: u32, window: std::time::Duration) -> Self {
        Self {
            state: std::sync::Mutex::new(RateWindow {
                started: std::time::Instant::now(),
                global: 0,
                per_ip: std::collections::HashMap::new(),
            }),
            per_ip_limit,
            global_limit,
            window,
        }
    }

    /// Generous defaults: 60 token-shaped RCPTs per source IP per minute, 600
    /// globally — ample for legitimate low-volume email-to-X, a hard brake on a
    /// flood. (Hardcoded for C7; promote to config when an operator needs to tune.)
    pub fn with_defaults() -> Self {
        Self::new(60, 600, std::time::Duration::from_secs(60))
    }

    /// Whether a token-shaped RCPT from `ip` is allowed; `false` → shed it
    /// (silent-drop, validity-blind). Rolls the fixed window on the first call
    /// past its end. Never panics on a poisoned lock.
    pub fn allow(&self, ip: std::net::IpAddr) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.started.elapsed() >= self.window {
            s.started = std::time::Instant::now();
            s.global = 0;
            s.per_ip.clear();
        }
        if s.global >= self.global_limit {
            return false;
        }
        let c = s.per_ip.entry(ip).or_insert(0);
        if *c >= self.per_ip_limit {
            return false;
        }
        *c += 1;
        s.global += 1;
        true
    }
}

/// The verified sender identity threaded into [`VtokenStore::resolve`] so the
/// sender-lock is **unmissable** — a `Valid` outcome means the token resolved AND
/// the sender is proven. Built in inbound delivery from the SMTP-session ingress,
/// the canonical header `From`, and the DMARC verdict (the local-auth identity
/// comes from session truth, never a forgeable header).
#[derive(Debug, Clone, Default)]
pub struct SenderProof {
    /// The session-authenticated submitter — `Some` ONLY for a local SMTP-AUTH
    /// submission. The strong, mailbox-level identity; evaluated by itself for
    /// either token tier.
    pub local_auth_email: Option<String>,
    /// The canonical header `From` — consulted ONLY on the external path, under
    /// DMARC.
    pub header_from: Option<String>,
    /// Whether DMARC aligned+passed for the header `From` domain (external path).
    pub dmarc_aligned: bool,
}

impl SenderProof {
    /// A local authenticated submission by `email`.
    pub fn local(email: impl Into<String>) -> Self {
        Self {
            local_auth_email: Some(email.into()),
            header_from: None,
            dmarc_aligned: false,
        }
    }
    /// An external (MX) message whose canonical `From` is `from`, with the DMARC
    /// alignment verdict.
    pub fn external(from: impl Into<String>, dmarc_aligned: bool) -> Self {
        Self {
            local_auth_email: None,
            header_from: Some(from.into()),
            dmarc_aligned,
        }
    }
    /// An external message with no usable verified sender — fails closed.
    pub fn unverified() -> Self {
        Self::default()
    }
    /// A short label for logging (never the address).
    fn kind(&self) -> &'static str {
        if self.local_auth_email.is_some() {
            "local-auth"
        } else if self.dmarc_aligned {
            "external-dmarc-pass"
        } else {
            "external-unverified"
        }
    }
}

/// Whether `proof` satisfies the row's sender-lock — the PRIMARY auth. Fail-closed
/// everywhere. NOT counted toward any brute-force lockout.
///
/// - **local-auth ingress** (`proof.local_auth_email == Some`): accept iff the
///   authenticated email == `allowed_sender`. Session truth, evaluated for EITHER
///   token tier; it never falls through to the external branch.
/// - **external ingress**: accept iff the token is `external-dmarc` tier AND the
///   canonical header `From` == `allowed_sender` AND DMARC aligned+passed. A
///   `local-auth`-tier token (e.g. an L=6) can NEVER be satisfied externally.
/// - empty/malformed `allowed_sender` or an unknown tier → fail closed.
fn sender_lock_core(
    allowed_sender: &str,
    verification_strength: &str,
    log_id: &str,
    proof: &SenderProof,
) -> bool {
    let Some(allowed) = canonical_addr(allowed_sender) else {
        tracing::warn!(
            token = %log_id,
            "vtoken allowed_sender unset/malformed; failing sender-lock closed"
        );
        return false;
    };
    if let Some(email) = proof.local_auth_email.as_deref() {
        // Local-auth ingress: session truth only, for either tier.
        return canonical_addr(email).as_deref() == Some(allowed.as_str());
    }
    // External ingress.
    match verification_strength {
        "external-dmarc" => {
            proof.dmarc_aligned
                && proof
                    .header_from
                    .as_deref()
                    .and_then(canonical_addr)
                    .as_deref()
                    == Some(allowed.as_str())
        }
        // `local-auth` tier can't be used externally; unknown tier fails closed.
        _ => false,
    }
}

/// A resolved opaque-token row (the `vtoken_opaque` table). No PIN — the opaque
/// token IS the selector; the sender-lock is the auth. Looked up by
/// `HMAC(secret, domain‖token)`, not by the raw token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueRow {
    pub token_hmac: String,
    pub domain: String,
    pub account: String,
    pub real_email: String,
    pub allowed_sender: String,
    pub verification_strength: String,
    pub token_len: i64,
    pub service: String,
    pub active: bool,
}

/// A freshly minted opaque token — the plaintext is returned ONCE (only its HMAC
/// is stored). The address handed out is `<token>@<domain>`. `token_hmac` is the
/// stable, non-secret row id used by list/lookup/disable (a one-way hash; it
/// reveals neither the token nor the server secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedOpaque {
    pub token: String,
    pub domain: String,
    pub token_hmac: String,
}

/// Resolution outcome for a recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtokenOutcome {
    /// The local-part is not vtoken-shaped — route normally (aliases etc).
    NotAVtoken,
    /// vtoken-shaped but unknown token / inactive / sender-lock failure.
    /// The caller **accept-drops** it — silent to the sender (no bounce oracle),
    /// and it OWNS the vtoken-shaped local-part (never alias-routes); logged.
    Invalid,
    /// A valid token. Deliver to `account`; inject the context into the filter.
    Valid(VtokenContext),
}

/// What a valid token contributes to delivery + the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtokenContext {
    pub user_id: String,
    /// The content account this token delivers into.
    pub account: String,
    /// The resolved service name (`Some("posts")` for a mapped token).
    pub service: Option<String>,
}

/// The daemon-owned marker keyword stamped on a message delivered through a
/// **content-routing** vtoken service (`posts`/`pages`). The public renderer's
/// live predicate (`member of Posts AND has-marker AND NOT suppressed`,
/// unified-mail-cms §0.2 #5) requires it, so "is this a real post" is the
/// daemon's determination *at ingest* — not merely an inference from folder
/// membership. It surfaces to webd through the JMAP `keywords` projection.
///
/// Stage 1 stamps it on vtoken ingest only (the message is being created, so a
/// daemon-controlled tag is trustworthy here). The user-keyword *reservation*
/// that would make it un-forgeable on the future Thunderbird/webd
/// move-into-Posts path is deferred to Stage 3; on a dedicated content account
/// the account boundary already gates who can put anything into `Posts`, so
/// ingest-stamping is the belt the design asks for without the Stage-3 braces.
pub const CONTENT_MARKER_TAG: &str = "content";

/// Whether a resolved vtoken service name routes into the public-content tree
/// (and so earns [`CONTENT_MARKER_TAG`] at delivery). `posts`/`pages` are
/// content services; an action service (e.g. `ai`) is not — it never enters a
/// content folder, so it must not be stamped.
pub fn is_content_service(service: &str) -> bool {
    matches!(service, "posts" | "pages")
}

/// The flat top-level content folders a content-service vtoken delivers into
/// (unified-mail-cms §0.2 #3). The marker is stamped ONLY when the inbound
/// filter actually routed the message into one of these — NOT when the filter
/// sent it to `Trash`/`Junk` (the F3-reject path for spam / bounce / over-cap /
/// over-size). So a message the filter rejected from the publish route never
/// carries the marker, and therefore can never be published by a later move
/// into `Posts` (the renderer predicate is `member of Posts AND has-marker`).
pub fn is_content_folder_name(name: &str) -> bool {
    matches!(name, "Posts" | "Pages")
}

/// Owned handle to the vtoken registry SQLite file. Open once at startup;
/// share via `Arc<VtokenStore>`.
pub struct VtokenStore {
    pub(crate) conn: Mutex<Connection>,
    path: PathBuf,
    /// HMAC key for opaque-token lookup (C6). Loaded at open, in Rust, before any
    /// embedded Mix evaluator — never in Mix scope.
    secret: VtokenSecret,
}

impl VtokenStore {
    /// Open or create the registry DB at `path` (parent dir created if
    /// missing). Schema migrated idempotently.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let secret_path = path.with_file_name("vtoken_secret.key");
        let path_for_blocking = path.clone();
        let (conn, secret) =
            tokio::task::spawn_blocking(move || -> Result<(Connection, VtokenSecret)> {
                if let Some(parent) = path_for_blocking.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("create vtoken parent dir {}", parent.display())
                    })?;
                }
                let mut conn = Connection::open(&path_for_blocking)
                    .with_context(|| format!("open {}", path_for_blocking.display()))?;
                init_schema(&mut conn)?;
                // The opaque-token HMAC key (C6) — created here, in Rust, before any
                // embedded Mix evaluator exists, so the secret never enters Mix scope.
                let secret = VtokenSecret::load_or_create(&secret_path)?;
                Ok((conn, secret))
            })
            .await
            .map_err(|e| anyhow::anyhow!("vtoken open spawn_blocking: {e}"))??;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
            secret,
        })
    }

    /// Resolve a recipient address into an outcome. The hot-path entry point
    /// used by inbound delivery. Opaque acceptance ENABLED — for tests and any
    /// caller that does not gate the opaque namespace. Production inbound
    /// delivery calls [`resolve_gated`](Self::resolve_gated) with the C9
    /// pre-flight flag (`SmtpConfig::opaque_rcpt_enabled`).
    pub async fn resolve(&self, rcpt: &str, proof: &SenderProof) -> Result<VtokenOutcome> {
        self.resolve_gated(rcpt, proof, true).await
    }

    /// Resolve a recipient, gating the OPAQUE namespace on the C9 pre-flight
    /// flag. When `opaque_enabled` is false an opaque-shaped recipient is treated
    /// as a normal mailbox (`NotAVtoken`), so a real mailbox that collides with
    /// the opaque token shape is delivered normally and never resolved against
    /// the opaque registry.
    pub async fn resolve_gated(
        &self,
        rcpt: &str,
        proof: &SenderProof,
        opaque_enabled: bool,
    ) -> Result<VtokenOutcome> {
        let (local, domain) = match rcpt.rsplit_once('@') {
            Some((l, d)) => (l, d),
            None => return Ok(VtokenOutcome::NotAVtoken),
        };
        if opaque_enabled && is_opaque_shaped(local) {
            return self.resolve_opaque(local, domain, proof).await;
        }
        Ok(VtokenOutcome::NotAVtoken)
    }

    /// Resolve an opaque-shaped local-part (C6). A shape match that MISSES the
    /// registry falls through to normal delivery (`NotAVtoken`) — C6 must not
    /// swallow a real mailbox of the same shape (the reserved-namespace
    /// silent-drop is C8). A HIT OWNS the local-part: inactive or a sender-lock
    /// failure resolves `Invalid` (silent), never `NotAVtoken`.
    async fn resolve_opaque(
        &self,
        token: &str,
        domain: &str,
        proof: &SenderProof,
    ) -> Result<VtokenOutcome> {
        // A malformed domain can't host a token → fall through to normal delivery.
        let Some(domain_c) = canonical_domain(domain) else {
            return Ok(VtokenOutcome::NotAVtoken);
        };
        let candidates = self.secret.candidate_hashes(&domain_c, token);
        let Some(row) = self.lookup_opaque(&candidates).await? else {
            return Ok(VtokenOutcome::NotAVtoken); // miss → normal delivery (C6)
        };
        if !row.active {
            return Ok(VtokenOutcome::Invalid);
        }
        if !sender_lock_core(
            &row.allowed_sender,
            &row.verification_strength,
            &row.token_hmac,
            proof,
        ) {
            tracing::warn!(
                proof_kind = proof.kind(),
                verification_strength = %row.verification_strength,
                "opaque vtoken sender_lock_failed; accept-and-drop (silent)"
            );
            return Ok(VtokenOutcome::Invalid);
        }
        Ok(VtokenOutcome::Valid(VtokenContext {
            // user_id carries the token_hmac (the DB key) for opaque tokens — a
            // stable, non-secret id, fine for $VTOKEN_USER; a dedicated token_id
            // label is a future refinement (Codex MINOR).
            user_id: row.token_hmac,
            account: row.account,
            service: Some(row.service),
        }))
    }

    /// Look up an opaque token by its candidate HMACs (current key, then the
    /// previous key during a rotation window). Returns the first match.
    async fn lookup_opaque(&self, candidates: &[String]) -> Result<Option<OpaqueRow>> {
        let conn = self.conn.lock().await;
        for hmac in candidates {
            let row = conn
                .query_row(
                    "SELECT token_hmac, domain, account, real_email, allowed_sender, \
                            verification_strength, token_len, service, active \
                     FROM vtoken_opaque WHERE token_hmac = ?1",
                    params![hmac],
                    |r| {
                        Ok(OpaqueRow {
                            token_hmac: r.get(0)?,
                            domain: r.get(1)?,
                            account: r.get(2)?,
                            real_email: r.get(3)?,
                            allowed_sender: r.get(4)?,
                            verification_strength: r.get(5)?,
                            token_len: r.get(6)?,
                            service: r.get(7)?,
                            active: r.get::<_, i64>(8)? != 0,
                        })
                    },
                )
                .optional()
                .context("query vtoken_opaque")?;
            if row.is_some() {
                return Ok(row);
            }
        }
        Ok(None)
    }

    /// All opaque token rows (the management browse surface, C9b). No secret to
    /// redact — the plaintext token is never stored, only its HMAC. Ordered by
    /// `token_hmac` for a stable listing.
    pub async fn list_opaque(&self) -> Result<Vec<OpaqueRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT token_hmac, domain, account, real_email, allowed_sender, \
                        verification_strength, token_len, service, active \
                 FROM vtoken_opaque ORDER BY token_hmac",
            )
            .context("prepare list vtoken_opaque")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(OpaqueRow {
                    token_hmac: r.get(0)?,
                    domain: r.get(1)?,
                    account: r.get(2)?,
                    real_email: r.get(3)?,
                    allowed_sender: r.get(4)?,
                    verification_strength: r.get(5)?,
                    token_len: r.get(6)?,
                    service: r.get(7)?,
                    active: r.get::<_, i64>(8)? != 0,
                })
            })
            .context("query vtoken_opaque list")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect vtoken_opaque list")?;
        Ok(rows)
    }

    /// Look up one opaque token by its `token_hmac` row id (the management
    /// lookup, C9b). Unlike [`lookup_opaque`] this takes the stored HMAC
    /// directly (from a prior list/mint), not candidate hashes of a plaintext.
    pub async fn lookup_opaque_by_hmac(&self, token_hmac: &str) -> Result<Option<OpaqueRow>> {
        self.lookup_opaque(std::slice::from_ref(&token_hmac.to_string()))
            .await
    }

    /// Revoke an opaque token by `token_hmac` (set `active = 0`). Returns whether
    /// a row was updated. The token can no longer resolve Valid (an inactive hit
    /// resolves Invalid → silent accept-drop).
    pub async fn disable_opaque(&self, token_hmac: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE vtoken_opaque SET active = 0, updated_at = ?2 WHERE token_hmac = ?1",
                params![token_hmac, unix_secs()],
            )
            .context("disable vtoken_opaque")?;
        Ok(n > 0)
    }

    /// Revoke an opaque token by `token_hmac` ONLY if its account still matches
    /// `expected_account` — the TOCTOU-safe mutation the delegated path uses, so
    /// a concurrent change between the domain-bind and the write can't disable a
    /// row that has since moved to another account. Returns whether a row matched.
    pub async fn disable_opaque_if_account(
        &self,
        token_hmac: &str,
        expected_account: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE vtoken_opaque SET active = 0, updated_at = ?3 \
                 WHERE token_hmac = ?1 AND account = ?2",
                params![token_hmac, expected_account, unix_secs()],
            )
            .context("disable_if_account vtoken_opaque")?;
        Ok(n > 0)
    }

    /// Mint a new opaque token for `domain`, locked to `allowed_sender` at
    /// `verification_strength`, mapping to the single `service`. Length is derived
    /// from the strength (L=6 `local-auth` / L=10 `external-dmarc`). Returns the
    /// plaintext token ONCE — only its `HMAC(secret, domain‖token)` is stored.
    /// Retries on the (astronomically unlikely) HMAC PK collision.
    ///
    /// NB: argument order is security-relevant — `allowed_sender` (not
    /// `real_email`) sets the sender-lock. Callers are few and reviewed.
    #[allow(clippy::too_many_arguments)]
    pub async fn mint_opaque(
        &self,
        domain: &str,
        account: &str,
        real_email: &str,
        allowed_sender: &str,
        verification_strength: &str,
        service: &str,
        active: bool,
    ) -> Result<MintedOpaque> {
        if !matches!(verification_strength, "local-auth" | "external-dmarc") {
            anyhow::bail!("invalid verification_strength: {verification_strength}");
        }
        if !is_valid_service_name(service) {
            anyhow::bail!("invalid service name: {service}");
        }
        let domain_c = canonical_domain(domain).context("invalid recipient domain")?;
        let allowed_c =
            canonical_addr(allowed_sender).context("allowed_sender is not a valid address")?;
        let len = opaque_len_for(verification_strength);
        let conn = self.conn.lock().await;
        for _ in 0..MINT_MAX_RETRIES {
            let token = gen_opaque(len);
            let token_hmac = self.secret.hmac_hex(&domain_c, &token);
            match conn.execute(
                "INSERT INTO vtoken_opaque \
                   (token_hmac, domain, account, real_email, allowed_sender, \
                    verification_strength, token_len, service, active, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    token_hmac,
                    domain_c,
                    account,
                    real_email,
                    allowed_c,
                    verification_strength,
                    len as i64,
                    service,
                    active as i64,
                    unix_secs()
                ],
            ) {
                Ok(_) => {
                    return Ok(MintedOpaque {
                        token,
                        domain: domain_c,
                        token_hmac,
                    });
                }
                Err(e) if is_user_id_conflict(&e) => continue,
                Err(e) => return Err(anyhow::Error::new(e).context("mint_opaque insert")),
            }
        }
        anyhow::bail!("mint_opaque: exhausted {MINT_MAX_RETRIES} hmac collision retries")
    }

    /// On-disk path, for diagnostics + tests.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a rusqlite error is a uniqueness collision — the
/// [`VtokenStore::mint_opaque`] retry signal. Matches the SPECIFIC PRIMARY KEY /
/// UNIQUE extended codes (not the broad `ConstraintViolation`) so a future NOT
/// NULL / CHECK constraint can't be misread as "collision, retry".
fn is_user_id_conflict(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                || err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

fn init_schema(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )
    .context("vtoken PRAGMAs")?;
    // Fresh-install schema (v4): only `schema_version` and `vtoken_opaque`.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 0),
            version   INTEGER NOT NULL
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS vtoken_opaque (
            token_hmac            TEXT PRIMARY KEY,
            domain                TEXT NOT NULL,
            account               TEXT NOT NULL,
            real_email            TEXT NOT NULL,
            allowed_sender        TEXT NOT NULL,
            verification_strength TEXT NOT NULL,
            token_len             INTEGER NOT NULL,
            service               TEXT NOT NULL,
            active                INTEGER NOT NULL DEFAULT 1,
            created_at            INTEGER NOT NULL DEFAULT 0,
            updated_at            INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;",
    )
    .context("vtoken schema init")?;

    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .context("count schema_version rows")?;
    match row_count {
        0 => {
            conn.execute(
                "INSERT INTO schema_version (singleton, version) VALUES (0, ?1)",
                params![CURRENT_SCHEMA_VERSION],
            )
            .context("insert schema_version")?;
        }
        1 => {
            let mut v: i64 = conn
                .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
                .context("read schema_version row")?;
            if v > CURRENT_SCHEMA_VERSION {
                return Err(anyhow::anyhow!(
                    "vtoken schema version {v} on disk is newer than supported ({CURRENT_SCHEMA_VERSION})"
                ));
            }
            // Apply migrations in sequence.
            while v < CURRENT_SCHEMA_VERSION {
                match v {
                    1..=3 => {
                        migrate_to_v4(conn)?;
                        v = 4;
                    }
                    other => {
                        return Err(anyhow::anyhow!(
                            "vtoken schema version {other} on disk is not supported (expected {CURRENT_SCHEMA_VERSION})"
                        ));
                    }
                }
            }
        }
        n => {
            return Err(anyhow::anyhow!(
                "vtoken schema_version table holds {n} rows; expected exactly one"
            ));
        }
    }
    Ok(())
}

/// Migrate any v1/v2/v3 database to v4 by dropping the now-defunct segmented
/// tables (`vtoken_registry`, `vtoken_failctl`). Safe because C9c re-minted
/// all live tokens as opaque tokens before this migration runs (C10). Wrapped
/// in a transaction so an interrupted migration rolls back whole.
fn migrate_to_v4(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().context("begin vtoken v*→v4 migration")?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS vtoken_registry;
         DROP TABLE IF EXISTS vtoken_failctl;",
    )
    .context("drop segmented vtoken tables for v4")?;
    tx.execute("UPDATE schema_version SET version = ?1", params![4])
        .context("bump schema_version to v4")?;
    tx.commit().context("commit vtoken v*→v4 migration")?;
    Ok(())
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_addr_folds_and_validates() {
        // Case-folds local + domain to one comparable form (both sender-lock sides).
        assert_eq!(
            canonical_addr("Alice@D.COM").as_deref(),
            Some("alice@d.com")
        );
        assert_eq!(
            canonical_addr("  Bob@Example.Org  ").as_deref(),
            Some("bob@example.org")
        );
        assert_eq!(canonical_addr("a@b.com").as_deref(), Some("a@b.com"));
        // Malformed / ambiguous → None (fail closed).
        assert_eq!(canonical_addr("nobody"), None); // no @
        assert_eq!(canonical_addr("a@b@c"), None); // two @
        assert_eq!(canonical_addr("@d.com"), None); // empty local
        assert_eq!(canonical_addr("a@"), None); // empty domain
        assert_eq!(canonical_addr("a@.d.com"), None); // leading dot
        assert_eq!(canonical_addr("a@d.com."), None); // trailing dot
        assert_eq!(canonical_addr("a@d..com"), None); // double dot
        assert_eq!(canonical_addr("a b@d.com"), None); // inner whitespace
        assert_eq!(canonical_addr("a@d\tcom"), None); // control/ws byte
        assert_eq!(canonical_addr(""), None);
    }

    #[test]
    fn opaque_shape_and_len() {
        assert!(is_opaque_shaped("k4m9p2")); // 6 base32
        assert!(is_opaque_shaped("k4m9p2x7qe")); // 10 base32
        assert!(!is_opaque_shaped("k4m9p")); // len 5
        assert!(!is_opaque_shaped("k4m9p2x")); // len 7
        assert!(!is_opaque_shaped("k4m9pi")); // 'i' not in Crockford base32
        assert!(!is_opaque_shaped("k4-9p2")); // hyphen → not opaque-shaped
        assert!(!is_opaque_shaped("MARK22")); // uppercase not in the alphabet
        assert_eq!(opaque_len_for("local-auth"), 6);
        assert_eq!(opaque_len_for("external-dmarc"), 10);
        assert_eq!(opaque_len_for("bogus"), 10); // fail-safe to the longer
    }

    #[tokio::test]
    async fn resolve_gated_disabled_treats_opaque_as_normal_mailbox() {
        // C9 fail-safe at the resolve layer: a VALID opaque token resolves Valid
        // with opaque_enabled=true, but NotAVtoken with opaque_enabled=false — so
        // a disabled run (a pre-flight collision) delivers an opaque-shaped
        // recipient as a normal mailbox instead of resolving it against the
        // opaque registry, never swallowing a real mailbox of the same shape.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = VtokenStore::open(tmp.path().join("vtokens.db"))
            .await
            .unwrap();
        let m = store
            .mint_opaque(
                "example.org",
                "blog@example.org",
                "mark@example.net",
                "mark@example.net",
                "local-auth",
                "posts",
                true,
            )
            .await
            .unwrap();
        let addr = format!("{}@example.org", m.token);
        let proof = SenderProof::local("mark@example.net");
        // Enabled → resolves as the opaque token.
        assert!(matches!(
            store.resolve_gated(&addr, &proof, true).await.unwrap(),
            VtokenOutcome::Valid(_)
        ));
        // Disabled → opaque shape ignored; falls through to NotAVtoken.
        assert_eq!(
            store.resolve_gated(&addr, &proof, false).await.unwrap(),
            VtokenOutcome::NotAVtoken
        );
    }

    #[tokio::test]
    async fn opaque_mint_resolve_roundtrip_domain_bound_and_sender_locked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = VtokenStore::open(tmp.path().join("vtokens.db"))
            .await
            .unwrap();
        let m = store
            .mint_opaque(
                "example.org",
                "blog@example.org",
                "mark@example.net",
                "mark@example.net",
                "local-auth",
                "posts",
                true,
            )
            .await
            .unwrap();
        assert_eq!(m.token.len(), 6); // local-auth → L=6
        assert_eq!(m.domain, "example.org");
        let addr = format!("{}@example.org", m.token);

        match store
            .resolve(&addr, &SenderProof::local("mark@example.net"))
            .await
            .unwrap()
        {
            VtokenOutcome::Valid(ctx) => {
                assert_eq!(ctx.account, "blog@example.org");
                assert_eq!(ctx.service.as_deref(), Some("posts"));
            }
            other => panic!("expected Valid, got {other:?}"),
        }
        // Wrong sender → Invalid (a hit OWNS the local-part).
        assert_eq!(
            store
                .resolve(&addr, &SenderProof::local("evil@example.net"))
                .await
                .unwrap(),
            VtokenOutcome::Invalid
        );
        // The SAME token under a DIFFERENT domain → miss → NotAVtoken (HMAC is
        // domain-bound).
        assert_eq!(
            store
                .resolve(
                    &format!("{}@example.net", m.token),
                    &SenderProof::local("mark@example.net")
                )
                .await
                .unwrap(),
            VtokenOutcome::NotAVtoken
        );
    }

    #[tokio::test]
    async fn opaque_external_l10_inactive_invalid_and_miss_falls_through() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = VtokenStore::open(tmp.path().join("vtokens.db"))
            .await
            .unwrap();
        let m = store
            .mint_opaque(
                "example.org",
                "blog@example.org",
                "mark@example.net",
                "mark@example.net",
                "external-dmarc",
                "posts",
                true,
            )
            .await
            .unwrap();
        assert_eq!(m.token.len(), 10); // external-dmarc → L=10
        let addr = format!("{}@example.org", m.token);
        // External From matches + DMARC aligned → Valid; not aligned → Invalid.
        assert!(matches!(
            store
                .resolve(&addr, &SenderProof::external("mark@example.net", true))
                .await
                .unwrap(),
            VtokenOutcome::Valid(_)
        ));
        assert_eq!(
            store
                .resolve(&addr, &SenderProof::external("mark@example.net", false))
                .await
                .unwrap(),
            VtokenOutcome::Invalid
        );

        // An INACTIVE opaque token → Invalid (owns the local-part).
        let mi = store
            .mint_opaque(
                "example.org",
                "blog@example.org",
                "mark@example.net",
                "mark@example.net",
                "external-dmarc",
                "posts",
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .resolve(
                    &format!("{}@example.org", mi.token),
                    &SenderProof::external("mark@example.net", true)
                )
                .await
                .unwrap(),
            VtokenOutcome::Invalid
        );

        // An unregistered opaque-shaped address → miss → NotAVtoken (normal
        // delivery; C6 must not swallow a real mailbox of this shape).
        assert_eq!(
            store
                .resolve("bbbbbbbbbb@example.org", &SenderProof::unverified())
                .await
                .unwrap(),
            VtokenOutcome::NotAVtoken
        );
    }

    #[tokio::test]
    async fn schema_init_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("vtokens.db");
        let _ = VtokenStore::open(&path).await.unwrap();
        let _ = VtokenStore::open(&path).await.unwrap();
        let store = VtokenStore::open(&path).await.unwrap();
        let n: i64 = store
            .conn
            .lock()
            .await
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn token_rate_limiter_per_ip_and_global() {
        use std::net::{IpAddr, Ipv4Addr};
        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        // per-ip 2, global 3, a long window so it doesn't roll mid-test.
        let lim = TokenRateLimiter::new(2, 3, std::time::Duration::from_secs(3600));
        assert!(lim.allow(ip1)); // ip1: 1, global 1
        assert!(lim.allow(ip1)); // ip1: 2, global 2
        assert!(!lim.allow(ip1)); // ip1 over per-ip limit (global untouched)
        assert!(lim.allow(ip2)); // ip2: 1 (independent), global 3
        assert!(!lim.allow(ip2)); // global ceiling hit
    }

    #[test]
    fn content_services_earn_the_marker() {
        assert!(is_content_service("posts"));
        assert!(is_content_service("pages"));
        assert!(!is_content_service("ai")); // action service — no content folder
        assert!(!is_content_service("")); // bare 2-segment token (service None → "")
        assert!(!is_content_service("todos")); // private surface, not publicly served
    }

    #[test]
    fn only_content_folders_earn_the_marker_destination() {
        assert!(is_content_folder_name("Posts"));
        assert!(is_content_folder_name("Pages"));
        // The F3-reject destinations must NOT earn the marker — else a move from
        // Trash into Posts would publish a filter-rejected message.
        assert!(!is_content_folder_name("Trash"));
        assert!(!is_content_folder_name("Junk"));
        assert!(!is_content_folder_name("Inbox"));
        assert!(!is_content_folder_name("Drafts"));
    }

    #[tokio::test]
    async fn migrates_v3_db_drops_segmented_tables() {
        // Hand-build a v3 DB that has both legacy tables + vtoken_opaque,
        // then open the store and verify v4 drops both.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("vtokens.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE vtoken_registry (
                    user_id TEXT PRIMARY KEY, pin_hash TEXT NOT NULL, pin_prev_hash TEXT,
                    account TEXT NOT NULL, real_email TEXT NOT NULL,
                    services TEXT NOT NULL DEFAULT '{}', active INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0,
                    allowed_sender TEXT NOT NULL DEFAULT '', verification_strength TEXT NOT NULL DEFAULT 'external-dmarc',
                    token_len INTEGER NOT NULL DEFAULT 0
                 ) WITHOUT ROWID;
                 CREATE TABLE vtoken_failctl (
                    user_id TEXT PRIMARY KEY, fail_count INTEGER NOT NULL DEFAULT 0,
                    window_start INTEGER NOT NULL DEFAULT 0, locked_until INTEGER NOT NULL DEFAULT 0
                 ) WITHOUT ROWID;
                 CREATE TABLE schema_version (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 0), version INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE vtoken_opaque (
                    token_hmac TEXT PRIMARY KEY, domain TEXT NOT NULL, account TEXT NOT NULL,
                    real_email TEXT NOT NULL, allowed_sender TEXT NOT NULL,
                    verification_strength TEXT NOT NULL, token_len INTEGER NOT NULL,
                    service TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0
                 ) WITHOUT ROWID;
                 INSERT INTO schema_version (singleton, version) VALUES (0, 3);",
            )
            .unwrap();
        }
        // Open → migrates to v4.
        let store = VtokenStore::open(&path).await.unwrap();
        let conn = store.conn.lock().await;
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4, "schema version must be 4 after migration");
        // Both legacy tables must be gone.
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('vtoken_registry','vtoken_failctl')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "vtoken_registry and vtoken_failctl must be dropped");
    }

    #[tokio::test]
    async fn migrates_v2_live_shape_to_v4() {
        // The EXACT on-disk shape of a live 0.3.33 DB: schema v2 (hashed PINs,
        // NO sender-lock columns), NO vtoken_opaque table yet, and an active
        // segmented row. Opening with the 0.4.0 store must NOT crash: init
        // creates vtoken_opaque, the migration drops the segmented tables and
        // lands at v4. (Guards the live production cutover — a startup crash here
        // would take down all mail.)
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("vtokens.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE vtoken_registry (
                    user_id TEXT PRIMARY KEY, pin_hash TEXT NOT NULL, pin_prev_hash TEXT,
                    account TEXT NOT NULL, real_email TEXT NOT NULL,
                    services TEXT NOT NULL DEFAULT '{}', active INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0
                 ) WITHOUT ROWID;
                 CREATE TABLE vtoken_failctl (
                    user_id TEXT PRIMARY KEY, fail_count INTEGER NOT NULL DEFAULT 0,
                    window_start INTEGER NOT NULL DEFAULT 0, locked_until INTEGER NOT NULL DEFAULT 0
                 ) WITHOUT ROWID;
                 CREATE TABLE schema_version (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 0), version INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 INSERT INTO schema_version (singleton, version) VALUES (0, 2);
                 INSERT INTO vtoken_registry (user_id, pin_hash, account, real_email)
                   VALUES ('q14effzvm0l0lsmi', 'deadbeefhash', 'blog@example.org', 'admin@example.net');",
            )
            .unwrap();
        }
        let store = VtokenStore::open(&path).await.unwrap();
        let conn = store.conn.lock().await;
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4, "v2 live DB must migrate to v4");
        let dropped: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('vtoken_registry','vtoken_failctl')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dropped, 0, "segmented tables must be dropped");
        let opaque: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = 'vtoken_opaque'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(opaque, 1, "vtoken_opaque must be created by init_schema");
    }
}
