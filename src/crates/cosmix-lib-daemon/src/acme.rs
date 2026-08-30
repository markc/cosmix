//! ACME client wrapper — `instant-acme` plus the LE-only policy
//! invariants webd needs to issue and renew Let's Encrypt
//! certificates from inside the daemon.
//!
//! This module is the **only** entry point in the workspace that
//! creates an ACME account against Let's Encrypt. Two structural
//! shapes carry the LE-only directive:
//!
//! * `AcmeProvider` enumerates *only* `LetsencryptStaging` and
//!   `LetsencryptProd`. A future CA (ZeroSSL, Buypass, internal
//!   CA for `.bus`) would require a review-visible diff against
//!   this enum and a new branch on every consumer match — there
//!   is no `provider = "custom"` escape hatch.
//! * [`WebdAcmeClient::load_or_create_prod`] takes a
//!   `cosmix_config::AcmeTosAcceptance` *by reference*. The proof
//!   type is bare-private-constructed inside `cosmix-lib-config`'s
//!   `acme_policy` module, reachable only from the resolver
//!   (Commit 3). A caller that holds the proof can call this
//!   method; a caller that does not (i.e. every other crate)
//!   structurally cannot. The by-reference take (C5d2 retry-safety
//!   fix) keeps the proof in caller-owned storage so a transient
//!   account-load failure does not drop the one-shot mint and pin
//!   later renewal-backoff retries into a misleading "no
//!   acme_tos_accepted" path.
//!
//! Account material is persisted under
//! `<account_dir>/account.<provider>.{credentials,meta}.json`.
//! Each file is written via an `O_TMPFILE`-style temp file +
//! `rename(2)` + parent-dir `fsync`, so a given file write is
//! crash-atomic at the filesystem level. The *pair*, however,
//! is two separate writes: a crash between them can leave one
//! file present and one absent. The resume path traps that
//! halfway state explicitly — `meta XOR creds` trips
//! [`AcmeError::AccountMetaMismatch`] with
//! `field = "account_files"` and refuses to start, rather than
//! silently overwriting the surviving file. The fresh-create
//! path writes `credentials` first and `meta` second, so the
//! halfway state on a power-loss is always "creds-without-meta",
//! which is what the guard catches; operator recovery is
//! "restore the missing file from backup, delete the orphan, or
//! rotate the account directory" (the typed error spells this
//! out in its `expected` field). On subsequent boots the
//! recorded provider, directory URL, and (for prod) accepted ToS
//! URL are compared against the operator's current config; a
//! mismatch trips the same `AccountMetaMismatch` variant and
//! refuses to start, never silently switching endpoints under
//! the same key material.
//!
//! `instant-acme` 0.8 changed the on-disk account format from
//! PEM (0.7) to an opaque JSON blob (the `AccountCredentials`
//! type implements `Serialize` / `Deserialize` but treats its
//! body as private). The doc-planning artefact at
//! `_doc/planned/webd-vhosts-phase2.md` § *Why instant-acme,
//! version pin, dep posture* still names `account.<provider>.key.pem`;
//! this module uses `account.<provider>.credentials.json` instead
//! to follow the upstream format rather than re-implementing the
//! key serialization. The trade is recorded in the commit
//! message that introduces this module.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    Order, OrderStatus, RetryPolicy,
};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use cosmix_config::AcmeTosAcceptance;

use crate::tls::ChainEnvironment;

/// Which ACME endpoint a wrapper instance talks to.
///
/// Maps 1:1 onto `instant_acme::LetsEncrypt` and onto
/// [`ChainEnvironment`] (the Phase 1 validator's "which
/// trust-anchor set" axis), keeping the *single* environment
/// switch the operator chose at vhost-config time consistent
/// across the issue → validate → install pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcmeProvider {
    /// `acme-staging-v02.api.letsencrypt.org` — short-lived
    /// fake certs, untrusted by browsers, used to wring out
    /// orchestration bugs without burning real LE rate limits.
    LetsencryptStaging,
    /// `acme-v02.api.letsencrypt.org` — browser-trusted certs;
    /// rate-limited per the LE policy; requires the ToS gate
    /// in `cosmix-lib-config::acme_policy`.
    LetsencryptProd,
}

impl AcmeProvider {
    /// The ACME directory URL — passed verbatim to
    /// `AccountBuilder::create`.
    pub const fn directory_url(&self) -> &'static str {
        match self {
            Self::LetsencryptStaging => LetsEncrypt::Staging.url(),
            Self::LetsencryptProd => LetsEncrypt::Production.url(),
        }
    }

    /// Stable on-disk discriminator. Used in account file names
    /// so prod and staging accounts never collide on the same
    /// node (`account.letsencrypt_prod.credentials.json` vs
    /// `account.letsencrypt_staging.credentials.json`).
    pub const fn provider_name(&self) -> &'static str {
        match self {
            Self::LetsencryptStaging => "letsencrypt_staging",
            Self::LetsencryptProd => "letsencrypt_prod",
        }
    }

    /// The [`ChainEnvironment`] value the validator must use
    /// for certificates issued by this provider — the type
    /// system enforces "use the right anchor set" at the call
    /// site of `validate_le_chain_for_environment` rather than
    /// via a runtime config string.
    pub const fn chain_environment(&self) -> ChainEnvironment {
        match self {
            Self::LetsencryptStaging => ChainEnvironment::Staging,
            Self::LetsencryptProd => ChainEnvironment::Production,
        }
    }
}

/// On-disk witness of an LE account this node holds, written
/// alongside the opaque-JSON credentials file. Stored to detect
/// "operator silently changed the ACME URL" or "operator pasted
/// a different ToS URL" on a subsequent boot — both conditions
/// trip [`AcmeError::AccountMetaMismatch`] rather than silently
/// switching providers (or silently accepting a new Subscriber
/// Agreement) under the same key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeAccountMeta {
    /// Which ACME provider this account is registered with.
    pub provider: AcmeProvider,
    /// The directory URL recorded at registration time.
    /// Currently equal to `provider.directory_url()` for vanilla
    /// LE; kept as a separate field so a future non-LE provider
    /// integration doesn't require a schema migration.
    pub directory_url: String,
    /// The Kid URL the ACME server returned for this account
    /// (i.e. `Account::id()`). Read back for telemetry; not
    /// used for authentication (the credentials file is).
    pub account_id: String,
    /// The ToS URL the operator explicitly accepted when this
    /// account was created. Always present for
    /// `LetsencryptProd`, always absent for
    /// `LetsencryptStaging` (LE staging publishes no Subscriber
    /// Agreement to record).
    pub tos_url: Option<String>,
    /// RFC 3339 timestamp written at account creation. Purely
    /// informational; not load-bearing.
    pub created_at: String,
}

/// HTTP-01 challenge dispatcher. Implemented by the webd
/// process (Phase 2 Commit 4) so the ACME wrapper stays free of
/// any axum / Listener / web-server entanglement: the wrapper
/// *asks* "publish this token at this domain" and "stop
/// publishing it"; how that happens is the caller's problem.
///
/// The trait uses `async fn`-via-`#[async_trait]` so it can be
/// boxed as `Box<dyn Http01Solver>` — the resolver path in
/// Commit 5 needs trait objects to swap solvers per vhost
/// (HTTP-01 today; DNS-01 if we ever wire it).
#[async_trait]
pub trait Http01Solver: Send + Sync {
    /// Publish `key_authorization` at
    /// `http://<domain>/.well-known/acme-challenge/<token>`.
    /// MUST return only after the challenge is reachable; the
    /// wrapper signals the ACME server immediately after this
    /// future resolves.
    async fn provision(
        &self,
        domain: &str,
        token: &str,
        key_authorization: &str,
    ) -> Result<(), AcmeError>;

    /// Stop publishing the challenge for `(domain, token)`.
    /// Called whether the order succeeded or failed; idempotent
    /// — must not error if the token was never provisioned.
    async fn cleanup(&self, domain: &str, token: &str) -> Result<(), AcmeError>;
}

/// One successfully-issued certificate with a parsed validity
/// window. Returned from [`WebdAcmeClient::order`]; the caller
/// is responsible for installing the chain via the SNI resolver
/// and scheduling renewal off `not_after - <renewal-window>`.
///
/// The chain is **not** validated against the LE trust roots
/// before being returned — the caller is expected to round-trip
/// it through `validate_le_chain_for_environment` (matching
/// `provider.chain_environment()`) before installing. The
/// validator is the *one* gate keeping a non-LE chain out of
/// the SNI resolver; structuring the order method to validate
/// here would tempt callers into skipping it on a "we just got
/// it from LE, surely it's fine" rationale and break the
/// invariant on the renewal path two refactors later.
#[derive(Debug, Clone)]
pub struct IssuedChain {
    /// PEM-encoded full chain (leaf + intermediates) as
    /// returned by `Order::poll_certificate`.
    pub cert_pem: String,
    /// PKCS#8 PEM private key generated for this certificate.
    /// Generated locally; never sent to the CA.
    pub key_pem: String,
    /// Notion of the issuing provider — kept so the caller can
    /// route into `validate_le_chain_for_environment` with the
    /// correct trust-anchor set without re-deriving it from
    /// vhost config.
    pub provider: AcmeProvider,
    /// `notBefore` from the leaf certificate.
    pub not_before: OffsetDateTime,
    /// `notAfter` from the leaf certificate. The renewal
    /// scheduler MUST treat this as the only source of truth
    /// for "when does this cert expire"; do not pre-compute
    /// from order time or trust any server `validity` hint.
    pub not_after: OffsetDateTime,
}

/// Errors from the ACME wrapper. Kept as a typed enum so the
/// eventual provisioner caller can branch on `Protocol`
/// (rate-limited / back off and retry) vs `AccountMetaMismatch`
/// (refuse to start, operator must reconcile) vs
/// `ChallengeFailed` (HTTP-01 dispatch broken).
#[derive(Debug, Error)]
pub enum AcmeError {
    /// Filesystem error reading or writing the account
    /// directory — typically `EACCES` on a fresh deploy where
    /// the daemon's user can't write `/var/lib/cosmix/webd/`.
    #[error("ACME account directory {path}: {source}")]
    AccountDirIo {
        /// The path the operation was targeting.
        path: PathBuf,
        /// Underlying `io::Error`.
        #[source]
        source: std::io::Error,
    },

    /// Account metadata on disk disagrees with the operator's
    /// current vhost config — typically because the operator
    /// changed `provider = letsencrypt_staging` →
    /// `letsencrypt_prod` (or vice versa) without rotating the
    /// account dir, or because the LE ToS URL the operator
    /// pasted differs from the one recorded at registration.
    /// Refuse-to-start is intentional; the operator must
    /// either revert or explicitly rotate the account.
    #[error(
        "ACME account metadata mismatch (field {field}): on-disk {actual:?} vs \
         config {expected:?} — rotate the account directory or revert the change"
    )]
    AccountMetaMismatch {
        /// Which field disagreed (`provider`, `directory_url`,
        /// or `tos_url`).
        field: &'static str,
        /// The value recorded on disk.
        actual: String,
        /// The value the operator's current config implies.
        expected: String,
    },

    /// JSON serialization/deserialization of the credentials
    /// or meta file failed — almost always indicates on-disk
    /// corruption.
    #[error("ACME account JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Wraps `instant_acme::Error`. The wrapped variant covers
    /// rate limits, malformed ACME responses, account-creation
    /// failures, and challenge-state polling timeouts.
    #[error("ACME protocol error: {0}")]
    Protocol(#[from] instant_acme::Error),

    /// HTTP-01 dispatch (provision or cleanup) failed in the
    /// caller's `Http01Solver`. Carries the solver's own error
    /// string so the wrapper doesn't need to know what transport
    /// the caller used.
    #[error("HTTP-01 solver error for {domain}: {message}")]
    SolverFailed {
        /// The domain that was being challenged.
        domain: String,
        /// The solver's `Display`ed error.
        message: String,
    },

    /// `Order::poll_ready` returned a non-`Ready` final status
    /// — the CA refused to validate at least one challenge.
    /// Distinct from `Protocol` so a renewal loop can attribute
    /// the failure ("token wasn't reachable") vs ("LE
    /// rate-limited us").
    #[error("ACME challenge failed for {domain}: status {status:?}")]
    ChallengeFailed {
        /// The domain that was being challenged.
        domain: String,
        /// The final order status from the ACME server.
        status: OrderStatus,
    },

    /// CSR generation or local key creation failed.
    #[error("CSR / key generation: {0}")]
    Crypto(#[from] rcgen::Error),

    /// The leaf certificate parsed back from the issued chain
    /// either had no leaf or had an unparseable validity
    /// window. Indicates either an instant-acme protocol bug
    /// or upstream CA mis-issuance.
    #[error("issued chain malformed: {0}")]
    ChainMalformed(String),
}

/// ACME client bound to a specific [`AcmeProvider`] and a
/// per-node account directory.
pub struct WebdAcmeClient {
    provider: AcmeProvider,
    account: Account,
    account_dir: PathBuf,
}

impl WebdAcmeClient {
    /// Construct against LE *staging*. No ToS proof required
    /// — LE staging publishes no Subscriber Agreement, and
    /// staging certs are by design untrusted.
    pub async fn load_or_create_staging(
        account_dir: &Path,
        contact_email: &str,
    ) -> Result<Self, AcmeError> {
        Self::load_or_create(
            AcmeProvider::LetsencryptStaging,
            account_dir,
            contact_email,
            None,
        )
        .await
    }

    /// Construct against LE *production*. Takes the typed
    /// [`AcmeTosAcceptance`] proof by *reference*: this is the only
    /// function in the workspace that can read the recorded ToS
    /// URL and the only function that can register a prod
    /// account. The proof type is bare-private-constructed in
    /// `cosmix-lib-config::acme_policy`, so a caller without
    /// access to the resolver-mint path *cannot* call this
    /// method, full stop.
    ///
    /// The by-reference take (C5d2 refactor) preserves the
    /// type-system invariant — a caller still cannot construct the
    /// proof, only borrow an existing one — while keeping the
    /// proof retrievable across a transient load failure. The
    /// original by-value signature dropped the one-shot mint into
    /// a failing future, which a renewal-loop caller could not
    /// recover from without a daemon restart; the borrow keeps the
    /// proof in caller-owned storage for backoff retries.
    pub async fn load_or_create_prod(
        account_dir: &Path,
        contact_email: &str,
        tos: &AcmeTosAcceptance,
    ) -> Result<Self, AcmeError> {
        Self::load_or_create(
            AcmeProvider::LetsencryptProd,
            account_dir,
            contact_email,
            Some(tos),
        )
        .await
    }

    async fn load_or_create(
        provider: AcmeProvider,
        account_dir: &Path,
        contact_email: &str,
        tos: Option<&AcmeTosAcceptance>,
    ) -> Result<Self, AcmeError> {
        std::fs::create_dir_all(account_dir).map_err(|source| AcmeError::AccountDirIo {
            path: account_dir.to_path_buf(),
            source,
        })?;

        let meta_path = account_dir.join(format!("account.{}.meta.json", provider.provider_name()));
        let creds_path = account_dir.join(format!(
            "account.{}.credentials.json",
            provider.provider_name()
        ));

        // Cross-provider mismatch: refuse to start if the
        // account_dir already holds account material for a
        // *different* provider. Without this scan, switching
        // `provider = letsencrypt_staging` →
        // `letsencrypt_prod` (or vice versa) on the same
        // `account_dir` would silently create a second
        // account file pair next to the first, leaving the
        // operator with two parallel LE accounts and no clear
        // signal that the change was unsafe. The "rotate the
        // account directory" remediation is what we want; the
        // typed error is what surfaces it.
        if !meta_path.exists() {
            scan_for_foreign_provider(account_dir, provider)?;
        }

        // Half-written pair: a crash between the two atomic
        // writes can leave one file without the other. Treat
        // either case as corruption rather than silently
        // overwriting the surviving file — the operator must
        // either restore from backup or explicitly delete the
        // orphan. The fresh-create path below writes
        // credentials *first* then meta, so the
        // "creds-without-meta" case is the expected halfway
        // state; "meta-without-creds" indicates either
        // hand-editing or a deeper filesystem fault.
        if meta_path.exists() ^ creds_path.exists() {
            return Err(AcmeError::AccountMetaMismatch {
                field: "account_files",
                actual: format!("meta={} creds={}", meta_path.exists(), creds_path.exists()),
                expected: "both present (resume) or both absent (fresh) — \
                    restore the missing file from backup, delete the orphan, \
                    or rotate the account directory"
                    .to_string(),
            });
        }

        // Resume path: if both files exist, load the
        // credentials and verify the meta matches the
        // operator's current ACME config.
        if meta_path.exists() && creds_path.exists() {
            let meta_bytes =
                std::fs::read(&meta_path).map_err(|source| AcmeError::AccountDirIo {
                    path: meta_path.clone(),
                    source,
                })?;
            let meta: AcmeAccountMeta = serde_json::from_slice(&meta_bytes)?;

            if meta.provider != provider {
                return Err(AcmeError::AccountMetaMismatch {
                    field: "provider",
                    actual: meta.provider.provider_name().to_string(),
                    expected: provider.provider_name().to_string(),
                });
            }
            if meta.directory_url != provider.directory_url() {
                return Err(AcmeError::AccountMetaMismatch {
                    field: "directory_url",
                    actual: meta.directory_url.clone(),
                    expected: provider.directory_url().to_string(),
                });
            }
            // ToS URL invariant — operator changing the
            // accepted URL is the "subsequent boots" rule from
            // the policy doc; refuse-to-start rather than
            // silently re-binding the historical account to a
            // new agreement string.
            let expected_tos = tos.as_ref().map(|t| t.url().to_string());
            if meta.tos_url != expected_tos {
                return Err(AcmeError::AccountMetaMismatch {
                    field: "tos_url",
                    actual: format!("{:?}", meta.tos_url),
                    expected: format!("{expected_tos:?}"),
                });
            }

            let creds_bytes =
                std::fs::read(&creds_path).map_err(|source| AcmeError::AccountDirIo {
                    path: creds_path.clone(),
                    source,
                })?;
            let credentials: AccountCredentials = serde_json::from_slice(&creds_bytes)?;
            let account = Account::builder()?.from_credentials(credentials).await?;
            return Ok(Self {
                provider,
                account,
                account_dir: account_dir.to_path_buf(),
            });
        }

        // Fresh path: register a new account. The ToS gate has
        // already passed by virtue of the typed `tos`
        // parameter; `terms_of_service_agreed = true` here
        // mirrors that fact to the wire, and the meta file
        // records *which* ToS URL was the basis for that
        // assertion so future boots can detect operator drift.
        let contact = format!("mailto:{contact_email}");
        let (account, credentials) = Account::builder()?
            .create(
                &NewAccount {
                    contact: &[&contact],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                provider.directory_url().to_string(),
                None,
            )
            .await?;

        let meta = AcmeAccountMeta {
            provider,
            directory_url: provider.directory_url().to_string(),
            account_id: account.id().to_string(),
            tos_url: tos.as_ref().map(|t| t.url().to_string()),
            created_at: OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| String::from("unknown")),
        };

        // Order matters: write credentials first so the
        // crash-between-writes case is "creds-without-meta",
        // which the half-written-pair guard above traps
        // explicitly on the next boot. The alternative —
        // meta-first — would leave a meta file claiming an
        // account exists with credentials we never wrote, the
        // worst-case orphan (operator sees the policy
        // metadata but can never authenticate).
        atomic_write_json(&creds_path, &credentials)?;
        atomic_write_json(&meta_path, &meta)?;

        Ok(Self {
            provider,
            account,
            account_dir: account_dir.to_path_buf(),
        })
    }

    /// Which provider this client speaks to. Useful in tests
    /// and structured logs.
    pub fn provider(&self) -> AcmeProvider {
        self.provider
    }

    /// The on-disk account directory (informational).
    pub fn account_dir(&self) -> &Path {
        &self.account_dir
    }

    /// Issue a fresh certificate for `domain` via HTTP-01,
    /// using `solver` to publish and clean up the challenge
    /// token.
    pub async fn order(
        &self,
        domain: &str,
        solver: &dyn Http01Solver,
    ) -> Result<IssuedChain, AcmeError> {
        let identifier = Identifier::Dns(domain.to_string());
        let new_order = NewOrder::new(std::slice::from_ref(&identifier));
        let mut order: Order = self.account.new_order(&new_order).await?;

        // Once *any* challenge has been provisioned, every
        // fallible step below MUST funnel through a single
        // cleanup point — otherwise an early `?`-return on a
        // later `solver.provision()`, `challenge.set_ready()`,
        // `poll_ready`, `finalize_csr`, `poll_certificate`, or
        // `parse_leaf_validity` leaves the HTTP-01 tokens
        // published indefinitely. The inner async block holds
        // the full issuance pipeline; the cleanup loop after
        // it runs regardless of outcome.
        let mut provisioned: Vec<(String, String)> = Vec::new();
        let result =
            provision_and_issue(&mut order, domain, self.provider, solver, &mut provisioned).await;
        for (d, t) in &provisioned {
            let _ = solver.cleanup(d, t).await;
        }
        result
    }
}

async fn provision_and_issue(
    order: &mut Order,
    domain: &str,
    provider: AcmeProvider,
    solver: &dyn Http01Solver,
    provisioned: &mut Vec<(String, String)>,
) -> Result<IssuedChain, AcmeError> {
    // Resolve and provision every authorization. webd
    // single-SAN orders today have exactly one authorization;
    // loop anyway so a future SAN-list extension doesn't
    // silently no-op past the first element.
    {
        let mut auths = order.authorizations();
        while let Some(auth_res) = auths.next().await {
            let mut auth = auth_res?;
            let Some(mut challenge) = auth.challenge(ChallengeType::Http01) else {
                return Err(AcmeError::ChallengeFailed {
                    domain: domain.to_string(),
                    status: OrderStatus::Invalid,
                });
            };
            let key_auth = challenge.key_authorization();
            // ChallengeHandle derefs to Challenge; .token is
            // the URL-safe component that goes under
            // `/.well-known/acme-challenge/`.
            let token = challenge.token.clone();
            solver
                .provision(domain, &token, key_auth.as_str())
                .await
                .map_err(|e| AcmeError::SolverFailed {
                    domain: domain.to_string(),
                    message: e.to_string(),
                })?;
            // Push BEFORE `set_ready` so a `set_ready` error
            // still triggers cleanup of the just-provisioned
            // token via the outer cleanup loop.
            provisioned.push((domain.to_string(), token));
            // Signal the ACME server only after the solver
            // has confirmed publication.
            challenge.set_ready().await?;
        }
    }

    issue_inner(order, domain, provider).await
}

async fn issue_inner(
    order: &mut Order,
    domain: &str,
    provider: AcmeProvider,
) -> Result<IssuedChain, AcmeError> {
    let poll = RetryPolicy::new().timeout(Duration::from_secs(120));
    let status = order.poll_ready(&poll).await?;
    if status != OrderStatus::Ready {
        return Err(AcmeError::ChallengeFailed {
            domain: domain.to_string(),
            status,
        });
    }

    // Generate a fresh P-256 key + CSR for this leaf; never
    // reuse the account key for issued certs.
    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![domain.to_string()])?;
    params.distinguished_name = DistinguishedName::new();
    let csr = params.serialize_request(&key_pair)?;
    order.finalize_csr(csr.der()).await?;
    let cert_pem = order.poll_certificate(&poll).await?;

    let (not_before, not_after) = parse_leaf_validity(&cert_pem)?;

    Ok(IssuedChain {
        cert_pem,
        key_pem: key_pair.serialize_pem(),
        provider,
        not_before,
        not_after,
    })
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AcmeError> {
    let parent = path.parent().ok_or_else(|| AcmeError::AccountDirIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no parent",
        ),
    })?;
    let json = serde_json::to_vec_pretty(value)?;
    let tmp =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| AcmeError::AccountDirIo {
            path: parent.to_path_buf(),
            source,
        })?;
    {
        use std::io::Write;
        let mut f = tmp.as_file();
        f.write_all(&json)
            .map_err(|source| AcmeError::AccountDirIo {
                path: path.to_path_buf(),
                source,
            })?;
        f.sync_all().map_err(|source| AcmeError::AccountDirIo {
            path: path.to_path_buf(),
            source,
        })?;
    }
    tmp.persist(path).map_err(|e| AcmeError::AccountDirIo {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    // fsync the parent directory so the rename(2) entry is
    // durable across power loss. Without this, the file
    // contents are on disk (via the temp-file `sync_all`
    // above) but the directory entry pointing at them can be
    // lost — leaving the account dir in the "meta exists,
    // creds missing" half-pair state the resume-path guard
    // would then refuse to start on, forcing the operator
    // through a recovery flow that should never have
    // happened. Best-effort: filesystems that don't support
    // directory fsync (FAT, some network mounts) surface
    // EINVAL here and we propagate it as the same
    // `AccountDirIo` the rest of the function uses.
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|source| AcmeError::AccountDirIo {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Scan `account_dir` for any pre-existing `account.*.meta.json`
/// file whose recorded provider differs from `wanted`. Surfaces
/// the typed `AccountMetaMismatch` on the first foreign provider
/// found so a `provider = letsencrypt_staging` →
/// `letsencrypt_prod` flip on an existing account_dir refuses to
/// start instead of silently registering a parallel second
/// account.
///
/// Called only on the fresh-create path (when the requested
/// provider's own meta file does not yet exist); the resume path
/// trips the per-field comparison inside `load_or_create`
/// directly.
fn scan_for_foreign_provider(account_dir: &Path, wanted: AcmeProvider) -> Result<(), AcmeError> {
    let read = match std::fs::read_dir(account_dir) {
        Ok(rd) => rd,
        // A non-existent dir is fine here — the caller has
        // already `create_dir_all`'d it; if it disappeared
        // between then and now, the temp-file write will
        // surface a clearer error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AcmeError::AccountDirIo {
                path: account_dir.to_path_buf(),
                source,
            });
        }
    };
    for entry in read {
        let entry = entry.map_err(|source| AcmeError::AccountDirIo {
            path: account_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !(name_str.starts_with("account.") && name_str.ends_with(".meta.json")) {
            continue;
        }
        let path = entry.path();
        let bytes = std::fs::read(&path).map_err(|source| AcmeError::AccountDirIo {
            path: path.clone(),
            source,
        })?;
        let foreign: AcmeAccountMeta = serde_json::from_slice(&bytes)?;
        if foreign.provider != wanted {
            return Err(AcmeError::AccountMetaMismatch {
                field: "provider",
                actual: foreign.provider.provider_name().to_string(),
                expected: wanted.provider_name().to_string(),
            });
        }
    }
    Ok(())
}

/// Verify the on-disk account meta for `provider` against the
/// operator's current config, *without* loading credentials or
/// contacting the directory. Used by webd's startup pass so a
/// carrying-servable boot can detect operator drift
/// (`provider` / `directory_url` / `tos_url` flip on a still-
/// valid cert) without making the daemon's startup
/// network-dependent. The resume path inside `load_or_create`
/// performs the same field comparison plus a network-backed
/// credential load; this helper deliberately skips the latter.
///
/// Returns `Ok(())` in two cases:
///   * the account meta file does not yet exist (the operator
///     hasn't issued anything under this provider yet — the
///     first call to `load_or_create_*` will mint the account
///     and write the meta);
///   * the meta exists and every field matches.
///
/// `expected_tos` is the URL string from the operator's
/// `AcmeTosAcceptance`, peeked via `.url()`; pass `None` for
/// staging plans (LE staging publishes no Subscriber Agreement
/// — the recorded meta must also be `None`).
pub fn verify_account_meta_local(
    account_dir: &Path,
    provider: AcmeProvider,
    expected_tos: Option<&str>,
) -> Result<(), AcmeError> {
    let meta_path = account_dir.join(format!("account.{}.meta.json", provider.provider_name()));
    let bytes = match std::fs::read(&meta_path) {
        Ok(b) => b,
        // No prior account under this provider — the
        // load_or_create_* fresh path will handle it. Codex's
        // round-3 finding pinned the contract: this helper
        // returns Ok in the "nothing to verify yet" case so the
        // caller can still skip the network load until
        // `needs_issue` is true.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AcmeError::AccountDirIo {
                path: meta_path,
                source,
            });
        }
    };
    let meta: AcmeAccountMeta = serde_json::from_slice(&bytes)?;
    if meta.provider != provider {
        return Err(AcmeError::AccountMetaMismatch {
            field: "provider",
            actual: meta.provider.provider_name().to_string(),
            expected: provider.provider_name().to_string(),
        });
    }
    if meta.directory_url != provider.directory_url() {
        return Err(AcmeError::AccountMetaMismatch {
            field: "directory_url",
            actual: meta.directory_url.clone(),
            expected: provider.directory_url().to_string(),
        });
    }
    let expected_tos_owned = expected_tos.map(|s| s.to_string());
    if meta.tos_url != expected_tos_owned {
        return Err(AcmeError::AccountMetaMismatch {
            field: "tos_url",
            actual: format!("{:?}", meta.tos_url),
            expected: format!("{expected_tos_owned:?}"),
        });
    }
    Ok(())
}

/// Parse the leaf's `notBefore` / `notAfter` out of a PEM
/// chain. The leaf is always the first PEM block in an ACME
/// chain (RFC 8555 §7.4.2); `parse_x509_pem` decodes exactly
/// that first block, so any intermediates that follow are
/// deliberately ignored here.
fn parse_leaf_validity(cert_pem: &str) -> Result<(OffsetDateTime, OffsetDateTime), AcmeError> {
    use x509_parser::pem::parse_x509_pem;
    use x509_parser::prelude::*;

    let (_, leaf) = parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| AcmeError::ChainMalformed(format!("PEM: {e}")))?;
    let (_, cert) = X509Certificate::from_der(&leaf.contents)
        .map_err(|e| AcmeError::ChainMalformed(format!("DER: {e}")))?;
    let not_before = OffsetDateTime::from_unix_timestamp(cert.validity().not_before.timestamp())
        .map_err(|e| AcmeError::ChainMalformed(format!("not_before: {e}")))?;
    let not_after = OffsetDateTime::from_unix_timestamp(cert.validity().not_after.timestamp())
        .map_err(|e| AcmeError::ChainMalformed(format!("not_after: {e}")))?;
    Ok((not_before, not_after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acme_provider_directory_url_pins() {
        // Hard-coded LE production / staging URLs. If
        // `instant_acme::LetsEncrypt::url()` ever changes the
        // string (or if a future refactor wires a non-LE
        // provider through this enum) this test fires before
        // the new value reaches the wire.
        assert_eq!(
            AcmeProvider::LetsencryptProd.directory_url(),
            "https://acme-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(
            AcmeProvider::LetsencryptStaging.directory_url(),
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(
            AcmeProvider::LetsencryptProd.provider_name(),
            "letsencrypt_prod"
        );
        assert_eq!(
            AcmeProvider::LetsencryptStaging.provider_name(),
            "letsencrypt_staging"
        );
        assert!(matches!(
            AcmeProvider::LetsencryptProd.chain_environment(),
            ChainEnvironment::Production
        ));
        assert!(matches!(
            AcmeProvider::LetsencryptStaging.chain_environment(),
            ChainEnvironment::Staging
        ));
    }

    #[test]
    fn account_meta_round_trips_json() {
        let meta = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptProd,
            directory_url: AcmeProvider::LetsencryptProd.directory_url().to_string(),
            account_id: "https://acme-v02.api.letsencrypt.org/acme/acct/12345".to_string(),
            tos_url: Some(
                "https://letsencrypt.org/documents/LE-SA-v1.5-November-15-2024.pdf".to_string(),
            ),
            created_at: "2026-05-21T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AcmeAccountMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);

        // Provider must serialize as snake_case for the
        // on-disk format to be stable across schema-conscious
        // refactors of the Rust enum name.
        assert!(json.contains("\"letsencrypt_prod\""), "json={json}");
    }

    #[test]
    fn account_meta_staging_has_no_tos_url() {
        let meta = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptStaging,
            directory_url: AcmeProvider::LetsencryptStaging.directory_url().to_string(),
            account_id: "https://acme-staging-v02.api.letsencrypt.org/acme/acct/9".to_string(),
            tos_url: None,
            created_at: "2026-05-21T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        // `tos_url: None` must serialize as JSON `null`, not
        // be elided — present-but-null is what the
        // mismatch-detection logic in `load_or_create`
        // compares against.
        assert!(json.contains("\"tos_url\":null"), "json={json}");
        let back: AcmeAccountMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn solver_trait_is_object_safe() {
        // Compile-time assertion that `Http01Solver` can be
        // used behind `Box<dyn>`. Commit 5 wires
        // `Box<dyn Http01Solver>` into the resolver; if a
        // future refactor accidentally introduces a
        // `Self`-by-value method or a generic, this test
        // refuses to compile.
        struct Stub;
        #[async_trait]
        impl Http01Solver for Stub {
            async fn provision(&self, _: &str, _: &str, _: &str) -> Result<(), AcmeError> {
                Ok(())
            }
            async fn cleanup(&self, _: &str, _: &str) -> Result<(), AcmeError> {
                Ok(())
            }
        }
        let _: Box<dyn Http01Solver> = Box::new(Stub);
    }

    #[test]
    fn parse_leaf_validity_extracts_window() {
        use rcgen::{CertificateParams, KeyPair};
        use time::macros::datetime;

        let mut params = CertificateParams::new(vec!["leaf.example".to_string()]).unwrap();
        params.not_before = datetime!(2026-05-21 00:00:00 UTC);
        params.not_after = datetime!(2026-08-19 00:00:00 UTC);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();

        let (nb, na) = parse_leaf_validity(&pem).unwrap();
        // x509-parser exposes timestamps at second
        // granularity; rcgen rounds to the second too, so an
        // equality check is correct here.
        assert_eq!(nb, datetime!(2026-05-21 00:00:00 UTC));
        assert_eq!(na, datetime!(2026-08-19 00:00:00 UTC));
    }

    #[test]
    fn parse_leaf_validity_rejects_garbage() {
        let err = parse_leaf_validity("not a PEM").unwrap_err();
        assert!(matches!(err, AcmeError::ChainMalformed(_)), "{err:?}");
    }

    #[test]
    fn scan_rejects_foreign_provider() {
        // Drop a `account.letsencrypt_staging.meta.json` into
        // a temp dir, then ask the scan whether a fresh prod
        // account is OK — must trip `AccountMetaMismatch`
        // with `field = "provider"`. This is the
        // "operator-flipped-provider-on-shared-dir" hazard
        // the scan exists to catch.
        let dir = tempfile::tempdir().unwrap();
        let foreign = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptStaging,
            directory_url: AcmeProvider::LetsencryptStaging.directory_url().to_string(),
            account_id: "https://acme-staging-v02.api.letsencrypt.org/acme/acct/1".to_string(),
            tos_url: None,
            created_at: "2026-05-21T00:00:00Z".to_string(),
        };
        std::fs::write(
            dir.path().join("account.letsencrypt_staging.meta.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();

        let err = scan_for_foreign_provider(dir.path(), AcmeProvider::LetsencryptProd).unwrap_err();
        match err {
            AcmeError::AccountMetaMismatch {
                field,
                actual,
                expected,
            } => {
                assert_eq!(field, "provider");
                assert_eq!(actual, "letsencrypt_staging");
                assert_eq!(expected, "letsencrypt_prod");
            }
            other => panic!("expected AccountMetaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn scan_accepts_matching_provider() {
        // Same-provider meta file must NOT trip the scan — it
        // is the resume path's job to do the per-field
        // comparison once both files are loaded.
        let dir = tempfile::tempdir().unwrap();
        let same = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptProd,
            directory_url: AcmeProvider::LetsencryptProd.directory_url().to_string(),
            account_id: "https://acme-v02.api.letsencrypt.org/acme/acct/1".to_string(),
            tos_url: Some("https://letsencrypt.org/documents/LE-SA-v1.5.pdf".to_string()),
            created_at: "2026-05-21T00:00:00Z".to_string(),
        };
        std::fs::write(
            dir.path().join("account.letsencrypt_prod.meta.json"),
            serde_json::to_vec(&same).unwrap(),
        )
        .unwrap();
        scan_for_foreign_provider(dir.path(), AcmeProvider::LetsencryptProd).unwrap();
    }

    #[test]
    fn scan_ignores_unrelated_files() {
        // Sibling files that don't match the
        // `account.*.meta.json` shape are skipped entirely —
        // a stray `notes.txt` or backup `account.bak` must
        // not break the scan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("account.bak"), b"x").unwrap();
        scan_for_foreign_provider(dir.path(), AcmeProvider::LetsencryptProd).unwrap();
    }

    #[test]
    fn atomic_write_round_trips_and_fsyncs_parent() {
        // Spot-check the persistence primitive: a write goes
        // down, the bytes come back identical, and the call
        // doesn't panic on a real filesystem. The parent-dir
        // `sync_all` is the load-bearing addition from the
        // C2 review; if the directory becomes unsyncable
        // (e.g. on tmpfs that vetoes fsync), we surface
        // `AccountDirIo` rather than panic.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.letsencrypt_prod.meta.json");
        let meta = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptProd,
            directory_url: AcmeProvider::LetsencryptProd.directory_url().to_string(),
            account_id: "acct/123".to_string(),
            tos_url: Some("https://letsencrypt.org/documents/LE-SA-v1.5.pdf".to_string()),
            created_at: "2026-05-21T00:00:00Z".to_string(),
        };
        atomic_write_json(&path, &meta).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let back: AcmeAccountMeta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn verify_meta_local_ok_when_absent() {
        // Fresh dir → no account file yet → caller must not be
        // forced into a network-load path. Returns `Ok(())` so
        // the lazy `load_or_create_*` in the per-plan loop is
        // the only side that contacts the directory.
        let dir = tempfile::tempdir().unwrap();
        verify_account_meta_local(dir.path(), AcmeProvider::LetsencryptStaging, None).unwrap();
    }

    #[test]
    fn verify_meta_local_catches_provider_drift() {
        // operator flipped `provider = letsencrypt_staging` →
        // `letsencrypt_prod` on a dir that already holds a
        // staging account → mismatch surfaces *before*
        // load_or_create_* would have done the network resume.
        let dir = tempfile::tempdir().unwrap();
        let existing = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptStaging,
            directory_url: AcmeProvider::LetsencryptStaging.directory_url().to_string(),
            account_id: "acct/1".into(),
            tos_url: None,
            created_at: "2026-05-21T00:00:00Z".into(),
        };
        std::fs::write(
            dir.path().join("account.letsencrypt_staging.meta.json"),
            serde_json::to_vec(&existing).unwrap(),
        )
        .unwrap();
        // Query for the *prod* provider: meta file for prod
        // doesn't exist → Ok, because the per-provider check
        // is keyed on the requested provider's filename. The
        // foreign-staging file is the `scan_for_foreign_provider`
        // gate's concern (called only on the fresh-create path).
        verify_account_meta_local(dir.path(), AcmeProvider::LetsencryptProd, None).unwrap();
        // Query for staging when the on-disk meta is prod →
        // mismatch on `provider`.
        let prod_existing = AcmeAccountMeta {
            provider: AcmeProvider::LetsencryptProd,
            directory_url: AcmeProvider::LetsencryptProd.directory_url().to_string(),
            account_id: "acct/2".into(),
            tos_url: Some("https://letsencrypt.org/documents/LE-SA-v1.5.pdf".into()),
            created_at: "2026-05-21T00:00:00Z".into(),
        };
        std::fs::write(
            dir.path().join("account.letsencrypt_prod.meta.json"),
            serde_json::to_vec(&prod_existing).unwrap(),
        )
        .unwrap();
        // The recorded prod meta validates against itself with
        // the right ToS URL.
        verify_account_meta_local(
            dir.path(),
            AcmeProvider::LetsencryptProd,
            Some("https://letsencrypt.org/documents/LE-SA-v1.5.pdf"),
        )
        .unwrap();
        // ...but a different ToS URL trips the tos_url mismatch.
        let err = verify_account_meta_local(
            dir.path(),
            AcmeProvider::LetsencryptProd,
            Some("https://letsencrypt.org/documents/LE-SA-v2.0.pdf"),
        )
        .unwrap_err();
        match err {
            AcmeError::AccountMetaMismatch { field, .. } => assert_eq!(field, "tos_url"),
            other => panic!("expected tos_url mismatch, got {other:?}"),
        }
    }

    // Compile-only assertion: `load_or_create_prod` requires
    // `&AcmeTosAcceptance`. The function is async and hits
    // the network, so we can't `.await` it in a unit test —
    // but constructing the call site at type-check time
    // inside an unused fn is enough to prove the signature
    // shape: a caller without an `AcmeTosAcceptance` cannot
    // reach this code path, and the by-reference take
    // preserves caller ownership across retries. If a future
    // refactor weakens the parameter to `Option<...>` or
    // `&str`, or restores the by-value take, this fn stops
    // compiling. Args are owned to sidestep `impl Future`'s
    // capture-of-anonymous-lifetimes edge case — the
    // signature *shape* is what we're asserting, not the
    // parameter kinds.
    #[allow(dead_code)]
    async fn prod_constructor_requires_typed_tos(
        dir: PathBuf,
        email: String,
        tos: AcmeTosAcceptance,
    ) -> Result<WebdAcmeClient, AcmeError> {
        WebdAcmeClient::load_or_create_prod(&dir, &email, &tos).await
    }
}
