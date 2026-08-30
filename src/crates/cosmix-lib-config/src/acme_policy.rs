//! ACME / Let's Encrypt **policy** — the structurally-unbypassable
//! gate that proves a webd node has accepted the Let's Encrypt
//! Subscriber Agreement *before* its ACME client is allowed to
//! register a production account.
//!
//! The umbrella's "Let's Encrypt only" directive plus RFC 8555 §7.3
//! ("the server MAY require the client to agree to the CA's terms of
//! service as a precondition to account creation") means a webd build
//! that silently accepted the LE Subscriber Agreement on the
//! operator's behalf would be both protocol-non-compliant and a
//! material policy regression. Phase 2 closes that hole at the type
//! system: `cosmix_daemon::acme::WebdAcmeClient::load_or_create_prod`
//! takes an [`AcmeTosAcceptance`] by reference, and the only function
//! in the workspace that can mint one is
//! [`acme_policy_internal::enforce_tos_gate_for_prod_node`] below.
//! The proof is *minted* in lib-config (here), *held* by webd on the
//! `AcmeProvisioner`, and *borrowed* by lib-daemon for each prod
//! account load/create. The by-reference take (C5d2 retry-safety
//! refactor) keeps the one-shot mint in caller-owned storage so a
//! transient prod account-load failure doesn't drop the proof and
//! pin later renewal-backoff retries into a misleading
//! "no acme_tos_accepted" path. The structural invariant — every
//! crate outside lib-config can hold and pass the proof but cannot
//! synthesise one — is unchanged by the reference take.
//!
//! The gate function is `pub(crate)` to `cosmix-lib-config`; the
//! inner constructor (`AcmeTosAcceptance::build`) is bare-private to
//! the `acme_policy_internal` module. Combined that means:
//!
//! * `cosmix-lib-config::resolver` (when it lands in Commit 3) is the
//!   only crate-internal caller able to invoke the gate.
//! * Every other crate in the workspace (`cosmix-lib-daemon`,
//!   `cosmix-webd`, anything downstream) can *hold*, *move*, and
//!   *pass* an `AcmeTosAcceptance`, but cannot construct one.
//! * Adding a new function in `cosmix-lib-config` that mints an
//!   `AcmeTosAcceptance` requires a review-visible diff against
//!   `acme_policy_internal` — there is no silent-bypass available
//!   from outside the policy module.
//!
//! The doc planning artefact at `_doc/planned/webd-vhosts-phase2.md`
//! § *Why instant-acme, version pin, dep posture* walks the
//! alternative shapes (standalone policy crate + sealed witness)
//! considered and rejected in favour of this in-crate variant. The
//! workspace dep graph has `cosmix-lib-daemon → cosmix-lib-config`
//! (not the reverse), so the in-crate home is the natural fit — no
//! circular dep, no fallback required.

pub(crate) mod acme_policy_internal {
    /// Cryptographic proof, for type-system purposes, that the
    /// LE Subscriber Agreement gate (umbrella Open Q6) has been
    /// evaluated *and passed* for the configured node. Minted by
    /// `enforce_tos_gate_for_prod_node` (lib-config), held by webd
    /// on the `AcmeProvisioner`, and borrowed by lib-daemon's
    /// `WebdAcmeClient::load_or_create_prod` for each prod account
    /// load/create. The by-reference take preserves the proof
    /// across transient ACME-account-load failures so backoff
    /// retries don't lose the one-shot mint.
    ///
    /// Constructor is bare-private — not even sibling modules
    /// in `cosmix-lib-config` can call it. The single legitimate
    /// mint path is [`enforce_tos_gate_for_prod_node`] in this
    /// same module.
    pub struct AcmeTosAcceptance {
        url: String,
    }

    impl AcmeTosAcceptance {
        /// Bare-private constructor. Reachable from
        /// [`enforce_tos_gate_for_prod_node`] only.
        //
        // `dead_code` on a `cargo check`-only build is expected
        // until the Commit 3 resolver wires the gate; the tests
        // in this module exercise it today.
        #[allow(dead_code)]
        fn build(url: String) -> Self {
            Self { url }
        }

        /// The Subscriber-Agreement URL the operator pasted into
        /// `[webd] acme_tos_accepted`, **after** the gate's
        /// `trim()` normalisation — leading/trailing whitespace
        /// is stripped at mint time so subsequent-boot
        /// byte-exact mismatch checks against
        /// `account.letsencrypt_prod.meta.json` compare the
        /// canonical post-trim form rather than the operator's
        /// raw input. An operator who later edits the *visible*
        /// URL (host, path, scheme) still triggers a startup
        /// error per the doc-planning "subsequent boots" rule;
        /// whitespace-only edits do not, intentionally.
        pub fn url(&self) -> &str {
            &self.url
        }
    }

    impl std::fmt::Debug for AcmeTosAcceptance {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // Avoid the temptation to print the URL on a panic
            // backtrace or accidental `{:?}` log — the URL itself
            // is not secret, but the *presence* of the proof in a
            // particular call site is the load-bearing signal,
            // and a `Debug` impl that prints the URL would invite
            // call sites that construct one from the printed
            // representation in a future refactor (string → URL
            // round-trip is a vector for the silent-bypass). The
            // explicit non-disclosing impl forecloses that route.
            f.debug_struct("AcmeTosAcceptance")
                .field("url", &"<redacted; call .url() if needed>")
                .finish()
        }
    }

    /// Why the gate rejected the caller's config.
    #[derive(Debug, thiserror::Error)]
    pub enum AcmeTosGateError {
        /// The node has at least one `provider = letsencrypt_prod`
        /// vhost but `[webd] acme_tos_accepted` was omitted, empty,
        /// or whitespace-only.
        #[error(
            "node has a letsencrypt_prod vhost but [webd] \
             acme_tos_accepted is not set — read \
             https://letsencrypt.org/repository/ and set the \
             URL verbatim"
        )]
        MissingForProd,
        /// `acme_tos_accepted` was set but does not point at the LE
        /// Subscriber-Agreement family of URLs
        /// (`https://letsencrypt.org/...`).
        #[error(
            "acme_tos_accepted must be an https://letsencrypt.org/... URL \
             pointing at the LE Subscriber Agreement"
        )]
        NotLetsEncryptUrl,
    }

    /// The single legitimate mint path for [`AcmeTosAcceptance`].
    ///
    /// `pub(crate)` — visible to the resolver in
    /// `cosmix-lib-config` (Commit 3 wires it), invisible to every
    /// other crate. Combined with the bare-private `build` above
    /// this is the structural shape we want: external callers can
    /// hold the proof, only the resolver can produce it.
    ///
    /// Inputs:
    /// - `config_value`: the raw string from `[webd]
    ///   acme_tos_accepted` (or `None` if the operator omitted
    ///   the key). The proof stores the **trimmed** form;
    ///   leading/trailing whitespace is normalised away so the
    ///   subsequent-boot mismatch detector compares the
    ///   canonical post-trim string, not the operator's raw
    ///   bytes.
    /// - `node_has_prod_vhost`: scanned by the resolver across the
    ///   node's `[[webd.vhost]]` rows. `false` → staging-only
    ///   node → gate skipped (LE staging has no Subscriber
    ///   Agreement to record).
    ///
    /// Output: `Ok(Some(proof))` on a node with a prod vhost and
    /// a valid `acme_tos_accepted`, `Ok(None)` on a staging-only
    /// node, `Err` on a prod-vhost node with missing / malformed
    /// `acme_tos_accepted`.
    pub(crate) fn enforce_tos_gate_for_prod_node(
        config_value: Option<&str>,
        node_has_prod_vhost: bool,
    ) -> Result<Option<AcmeTosAcceptance>, AcmeTosGateError> {
        if !node_has_prod_vhost {
            // Staging-only node: the gate has nothing to
            // enforce. Return the no-proof variant so the
            // resolver records "no prod ToS proof was minted on
            // this node" structurally rather than via a sentinel
            // empty string.
            return Ok(None);
        }
        // Trim-check for the *presence* test: an operator who
        // pastes `"   "` into the TOML obviously did not mean
        // to accept anything. The stored URL is the canonical
        // post-trim string, so visible URL edits (host, path,
        // scheme) trip the subsequent-boot mismatch detector
        // in `cosmix-lib-daemon::acme`, while whitespace-only
        // edits do not — intentional, see the `url()`
        // doc-comment for the rationale.
        let raw = config_value
            .filter(|s| !s.trim().is_empty())
            .ok_or(AcmeTosGateError::MissingForProd)?;
        let url = raw.trim();
        // Pin to the LE-hosted Subscriber Agreement family.
        // `https://letsencrypt.org/documents/LE-SA-v1.5-...pdf`
        // is the current canonical shape; the prefix check also
        // accepts `https://letsencrypt.org/repository/...` and
        // any future relocation under the same authority. Any
        // other host is structurally not the LE ToS, so
        // accepting it would defeat the whole point of the
        // gate.
        const LE_PREFIX: &str = "https://letsencrypt.org/";
        if !url.starts_with(LE_PREFIX) || url.len() == LE_PREFIX.len() {
            return Err(AcmeTosGateError::NotLetsEncryptUrl);
        }
        Ok(Some(AcmeTosAcceptance::build(url.to_string())))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn staging_only_node_skips_gate() {
            let out = enforce_tos_gate_for_prod_node(None, false).unwrap();
            assert!(out.is_none());
            // Staging-only path must skip the gate *before*
            // any URL-shape check, so even a non-LE URL
            // returns `Ok(None)` here.
            let out2 =
                enforce_tos_gate_for_prod_node(Some("https://x.example/tos"), false).unwrap();
            assert!(
                out2.is_none(),
                "staging-only nodes must not mint a proof even when the operator pasted a URL"
            );
        }

        #[test]
        fn prod_node_without_url_rejects() {
            let err = enforce_tos_gate_for_prod_node(None, true)
                .expect_err("prod node without acme_tos_accepted must reject");
            assert!(matches!(err, AcmeTosGateError::MissingForProd));

            for blank in ["", "   ", "\t", "\n", " \r\n "] {
                let err = enforce_tos_gate_for_prod_node(Some(blank), true)
                    .expect_err("blank/whitespace must be treated as missing");
                assert!(
                    matches!(err, AcmeTosGateError::MissingForProd),
                    "expected MissingForProd for {blank:?}, got {err:?}"
                );
            }
        }

        #[test]
        fn prod_node_with_non_le_url_rejects() {
            for bad in [
                "http://letsencrypt.org/repository/",
                "https://example.com/tos",
                "https://www.letsencrypt.org/repository/",
                "https://letsencrypt.org",
                "https://letsencrypt.org/",
                "ftp://letsencrypt.org/x",
                "tos.txt",
                "javascript:alert(1)",
            ] {
                let err = enforce_tos_gate_for_prod_node(Some(bad), true)
                    .expect_err("non-LE URL must reject");
                assert!(
                    matches!(err, AcmeTosGateError::NotLetsEncryptUrl),
                    "expected NotLetsEncryptUrl for {bad:?}, got {err:?}"
                );
            }
        }

        #[test]
        fn prod_node_with_le_url_mints_proof() {
            let url = "https://letsencrypt.org/documents/LE-SA-v1.5-November-15-2024.pdf";
            let proof = enforce_tos_gate_for_prod_node(Some(url), true)
                .unwrap()
                .expect("prod node with valid URL must mint a proof");
            assert_eq!(proof.url(), url);

            // Trim-tolerant: the *stored* URL is the trimmed
            // form, not the operator's whitespace-padded input.
            let padded = format!("  {url}\n");
            let proof = enforce_tos_gate_for_prod_node(Some(&padded), true)
                .unwrap()
                .expect("prod node with padded valid URL must mint a proof");
            assert_eq!(proof.url(), url);
        }

        #[test]
        fn debug_impl_redacts_url() {
            // Defence-in-depth: the Debug impl must not leak the
            // URL into a panic/log path, so future call sites
            // can't accidentally come to depend on it.
            let url = "https://letsencrypt.org/documents/LE-SA-v1.5-November-15-2024.pdf";
            let proof = enforce_tos_gate_for_prod_node(Some(url), true)
                .unwrap()
                .unwrap();
            let dbg = format!("{proof:?}");
            assert!(
                !dbg.contains("letsencrypt.org"),
                "Debug impl leaked URL: {dbg}"
            );
            assert!(
                dbg.contains("redacted"),
                "Debug impl missing redaction marker: {dbg}"
            );
        }
    }
}

pub use acme_policy_internal::{AcmeTosAcceptance, AcmeTosGateError};

// The gate function itself is deliberately *not* re-exported.
// It stays `pub(crate)` inside `acme_policy_internal`, reachable
// only from within `cosmix-lib-config` (called by
// `resolve_webd_acme` below — the one in-crate caller). External
// crates have access to the *proof type* and the *error type* via
// the re-exports above, never to the mint function.

use crate::node::{WebdAcmeChallenge, WebdAcmeProvider, WebdConfig};

/// Per-vhost ACME issuance plan produced by the resolver. One per
/// `[[webd.vhost]]` row with an `acme = {...}` block. Carries the
/// minimal set the webd provisioner needs to drive issuance: the
/// FQDN + alias list (SAN coverage), the provider (which ACME
/// directory to talk to), the challenge type (HTTP-01 only in
/// Phase 2), and the contact email registered with the account.
///
/// The ToS proof itself is **not** carried per-row: there is one LE
/// prod account per node, so the proof lives once on
/// [`ResolvedWebdAcme::tos_acceptance`]. The vhost plan only
/// declares which provider it expects; the webd provisioner uses
/// the node-level proof to construct the prod client.
#[derive(Debug, Clone)]
pub struct AcmeVhostPlan {
    /// Index of the source row in `WebdConfig::vhost` — used for
    /// diagnostics so a renewal error points the operator at the
    /// exact `[[webd.vhost]]` block that owns it.
    pub vhost_index: usize,
    /// Primary FQDN of the vhost. LDH-normalisation is the webd
    /// resolver's job (it shares the same `parse_request_host`
    /// parser that the runtime host-router uses); this struct
    /// carries the *resolver-validated* form so the provisioner
    /// doesn't re-parse.
    pub fqdn: String,
    /// Additional SAN-coverage names. May be empty.
    pub aliases: Vec<String>,
    pub provider: WebdAcmeProvider,
    pub challenge: WebdAcmeChallenge,
    pub contact_email: String,
}

/// Output of [`resolve_webd_acme`]: the (single, node-level) ToS
/// proof plus the per-vhost plans the webd provisioner will iterate.
///
/// `tos_acceptance == None` means either the node has no prod vhosts
/// at all (staging-only or no ACME), **or** the operator omitted
/// `[webd] acme_tos_accepted` on a staging-only node — both cases
/// are legitimately gate-skip. A node with a prod vhost and missing
/// ToS hard-fails before this struct is constructed.
#[derive(Debug)]
pub struct ResolvedWebdAcme {
    pub tos_acceptance: Option<AcmeTosAcceptance>,
    pub plans: Vec<AcmeVhostPlan>,
}

/// Why the ACME-config resolve step rejected the node config.
///
/// Variants are typed (not stringified) so the eventual webd caller —
/// and any future tooling that wants to surface the failure in a
/// structured form — can branch on which gate fired without
/// string-matching on the error message.
#[derive(Debug, thiserror::Error)]
pub enum AcmeResolveError {
    /// A `[[webd.vhost]]` row sets both `acme = {...}` and
    /// manual-PEM (`tls_cert` and/or `tls_key`). These are
    /// mutually exclusive — pick one issuance path per row.
    #[error(
        "[[webd.vhost]] #{index} ({fqdn}): \
         acme = {{...}} is mutually exclusive with tls_cert/tls_key — \
         pick one issuance path per row"
    )]
    AcmeAndManualTlsTogether { index: usize, fqdn: String },

    /// A vhost row asked for DNS-01. Phase 2 only ships HTTP-01;
    /// DNS-01 needs cosmix-dnsd's update surface and is reserved
    /// for Phase 4 — see `_doc/planned/webd-vhosts-phase2.md`.
    #[error(
        "[[webd.vhost]] #{index} ({fqdn}): \
         challenge = \"dns01\" is reserved for Phase 4 \
         (see _doc/planned/webd-vhosts-phase2.md) — use \"http01\""
    )]
    Dns01NotYetSupported { index: usize, fqdn: String },

    /// `contact_email` failed the resolver's sanity-check (must
    /// contain `@`, no CR/LF, ≤ 253 bytes). The check is
    /// deliberately permissive — full RFC-5322 validation is
    /// the operator's job and ACME accounts reject obviously-
    /// malformed addresses on registration anyway.
    #[error(
        "[[webd.vhost]] #{index} ({fqdn}): \
         contact_email is invalid \
         (must contain '@', no CR/LF, ≤ 253 bytes)"
    )]
    ContactEmailInvalid { index: usize, fqdn: String },

    /// The node has at least one `acme = {...}` row but no
    /// `[webd] http_listen` — HTTP-01 validation fundamentally
    /// requires plain HTTP on a publicly-reachable port (LE does
    /// not negotiate TLS during validation), so without
    /// `http_listen` no order can ever complete.
    ///
    /// `fqdn` names the first offending vhost so the operator can
    /// jump straight to the row in `node.conf.mix`.
    #[error(
        "[[webd.vhost]] #{index} ({fqdn}) requested ACME but \
         [webd] http_listen is not set — HTTP-01 needs a plain :80 \
         listener. Set [webd] http_listen = \"0.0.0.0:80\" or \
         drop the acme block."
    )]
    HttpListenRequiredForAcme { index: usize, fqdn: String },

    /// The node-level Subscriber-Agreement gate rejected the
    /// config. Re-exposes the typed
    /// [`AcmeTosGateError`] so a caller can match on
    /// `MissingForProd` vs `NotLetsEncryptUrl` without
    /// string-matching.
    #[error(transparent)]
    ToS(#[from] AcmeTosGateError),
}

/// Resolve the ACME slice of a [`WebdConfig`] into the node-level
/// ToS proof plus a list of per-vhost plans. This is the single
/// in-crate caller of
/// [`acme_policy_internal::enforce_tos_gate_for_prod_node`] — the
/// `pub(crate)` gate function above; combined with the bare-private
/// `AcmeTosAcceptance::build` constructor, that means **the only
/// review-visible path for minting a prod ToS proof is a diff
/// against this function or against `acme_policy_internal` itself**.
///
/// The resolver does **not** do LDH normalisation on the row's
/// `host` / `aliases` — that's the webd caller's job (it shares the
/// `parse_request_host` parser with the runtime host-router so
/// resolve-time and request-time accept the same shapes). Each
/// [`AcmeVhostPlan`] in the returned `plans` Vec carries the *raw
/// row strings*; the webd side overwrites `fqdn` / `aliases` in-place
/// with the LDH-normalised forms before handing the plan to the
/// provisioner. Keeping normalisation out of this module avoids
/// dragging the http-host parser (a daemon-side dependency) into the
/// config crate.
pub fn resolve_webd_acme(webd: &WebdConfig) -> Result<ResolvedWebdAcme, AcmeResolveError> {
    let mut plans: Vec<AcmeVhostPlan> = Vec::new();
    let mut has_prod = false;
    let mut http_listen_offender: Option<(usize, String)> = None;

    for (index, row) in webd.vhost.iter().enumerate() {
        let Some(acme) = &row.acme else {
            continue;
        };

        // Per-row gates — fire in source order so the most-specific
        // diagnostic wins (mutex with manual TLS is the most
        // common operator mistake; DNS-01 second; email shape last).
        if row.tls_cert.is_some() || row.tls_key.is_some() {
            return Err(AcmeResolveError::AcmeAndManualTlsTogether {
                index,
                fqdn: row.host.clone(),
            });
        }
        if matches!(acme.challenge, WebdAcmeChallenge::Dns01) {
            return Err(AcmeResolveError::Dns01NotYetSupported {
                index,
                fqdn: row.host.clone(),
            });
        }
        if !is_plausible_contact_email(&acme.contact_email) {
            return Err(AcmeResolveError::ContactEmailInvalid {
                index,
                fqdn: row.host.clone(),
            });
        }

        if matches!(acme.provider, WebdAcmeProvider::LetsEncryptProd) {
            has_prod = true;
        }
        if http_listen_offender.is_none() && webd.http_listen.is_none() {
            http_listen_offender = Some((index, row.host.clone()));
        }

        plans.push(AcmeVhostPlan {
            vhost_index: index,
            fqdn: row.host.clone(),
            aliases: row.aliases.clone(),
            provider: acme.provider,
            challenge: acme.challenge,
            contact_email: acme.contact_email.clone(),
        });
    }

    // Node-level: any acme row needs http_listen.
    if let Some((index, fqdn)) = http_listen_offender {
        return Err(AcmeResolveError::HttpListenRequiredForAcme { index, fqdn });
    }

    // Node-level: ToS gate fires iff a prod vhost is present.
    let tos_acceptance = acme_policy_internal::enforce_tos_gate_for_prod_node(
        webd.acme_tos_accepted.as_deref(),
        has_prod,
    )?;

    Ok(ResolvedWebdAcme {
        tos_acceptance,
        plans,
    })
}

/// Cheap sanity-check for the `contact_email` ACME field. RFC-5322
/// validation is not the resolver's job; the goal is to reject the
/// obvious-typo class of mistake (missing `@`, header-injection via
/// CR/LF, ridiculously oversized inputs).
fn is_plausible_contact_email(s: &str) -> bool {
    if s.len() > 253 {
        return false;
    }
    if !s.contains('@') {
        return false;
    }
    if s.bytes().any(|b| b == b'\r' || b == b'\n') {
        return false;
    }
    // Cheap "local-part and domain-part both non-empty" check — a
    // bare `@` slips past the contains-check above.
    let mut parts = s.splitn(2, '@');
    let local = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    // Domain must contain at least one dot — `foo@bar` is
    // syntactically valid per the RFC but the ACME directory will
    // reject it at registration, so we surface the failure here.
    if !domain.contains('.') {
        return false;
    }
    true
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::node::{WebdConfig, WebdVhostAcmeConfig, WebdVhostConfig};

    fn vhost_row(host: &str, acme: Option<WebdVhostAcmeConfig>) -> WebdVhostConfig {
        WebdVhostConfig {
            host: host.into(),
            aliases: Vec::new(),
            www_dir: "/tmp".into(),
            tls_cert: None,
            tls_key: None,
            acme,
            cms_db_path: None,
            aux_dbs: Vec::new(),
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
        }
    }

    fn staging_acme() -> WebdVhostAcmeConfig {
        WebdVhostAcmeConfig {
            provider: WebdAcmeProvider::LetsEncryptStaging,
            challenge: WebdAcmeChallenge::Http01,
            contact_email: "ops@example.com".into(),
        }
    }

    fn prod_acme() -> WebdVhostAcmeConfig {
        WebdVhostAcmeConfig {
            provider: WebdAcmeProvider::LetsEncryptProd,
            challenge: WebdAcmeChallenge::Http01,
            contact_email: "ops@example.com".into(),
        }
    }

    fn webd_with(http_listen: Option<&str>, rows: Vec<WebdVhostConfig>) -> WebdConfig {
        let mut w = WebdConfig {
            http_listen: http_listen.map(str::to_string),
            ..Default::default()
        };
        w.vhost = rows;
        w
    }

    #[test]
    fn rejects_acme_and_manual_tls_together() {
        let mut row = vhost_row("a.example", Some(staging_acme()));
        row.tls_cert = Some("/x/cert.pem".into());
        let webd = webd_with(Some("0.0.0.0:80"), vec![row]);
        let err = resolve_webd_acme(&webd).unwrap_err();
        assert!(matches!(
            err,
            AcmeResolveError::AcmeAndManualTlsTogether { ref fqdn, .. }
                if fqdn == "a.example"
        ));
    }

    #[test]
    fn rejects_dns01_with_phase4_pointer() {
        let mut acme = staging_acme();
        acme.challenge = WebdAcmeChallenge::Dns01;
        let webd = webd_with(Some("0.0.0.0:80"), vec![vhost_row("a.example", Some(acme))]);
        let err = resolve_webd_acme(&webd).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, AcmeResolveError::Dns01NotYetSupported { .. }));
        assert!(msg.contains("Phase 4"), "missing Phase 4 pointer: {msg}");
    }

    #[test]
    fn rejects_when_http_listen_absent() {
        let webd = webd_with(None, vec![vhost_row("a.example", Some(staging_acme()))]);
        let err = resolve_webd_acme(&webd).unwrap_err();
        assert!(matches!(
            err,
            AcmeResolveError::HttpListenRequiredForAcme { ref fqdn, .. }
                if fqdn == "a.example"
        ));
    }

    #[test]
    fn rejects_invalid_contact_email_shapes() {
        for bad in [
            "no-at-sign",
            "user@",
            "@example.com",
            "user@host",          // no dot in domain
            "user\r@example.com", // CR injection
            "user\n@example.com", // LF injection
            &"a".repeat(254),     // oversize
        ] {
            let mut acme = staging_acme();
            acme.contact_email = bad.to_string();
            let webd = webd_with(Some("0.0.0.0:80"), vec![vhost_row("a.example", Some(acme))]);
            let err = resolve_webd_acme(&webd).unwrap_err();
            assert!(
                matches!(err, AcmeResolveError::ContactEmailInvalid { .. }),
                "expected ContactEmailInvalid for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn staging_only_node_skips_tos_gate() {
        let webd = webd_with(
            Some("0.0.0.0:80"),
            vec![vhost_row("a.example", Some(staging_acme()))],
        );
        let out = resolve_webd_acme(&webd).expect("staging-only must resolve");
        assert!(out.tos_acceptance.is_none());
        assert_eq!(out.plans.len(), 1);
        assert_eq!(out.plans[0].fqdn, "a.example");
        assert!(matches!(
            out.plans[0].provider,
            WebdAcmeProvider::LetsEncryptStaging
        ));
    }

    #[test]
    fn prod_vhost_requires_acme_tos_accepted() {
        let webd = webd_with(
            Some("0.0.0.0:80"),
            vec![vhost_row("a.example", Some(prod_acme()))],
        );
        let err = resolve_webd_acme(&webd).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(
            err,
            AcmeResolveError::ToS(AcmeTosGateError::MissingForProd)
        ));
        assert!(
            msg.contains("letsencrypt.org/repository/"),
            "expected Subscriber-Agreement pointer, got: {msg}"
        );
    }

    #[test]
    fn prod_vhost_with_non_le_tos_url_rejects() {
        let mut webd = webd_with(
            Some("0.0.0.0:80"),
            vec![vhost_row("a.example", Some(prod_acme()))],
        );
        webd.acme_tos_accepted = Some("https://example.com/tos".into());
        let err = resolve_webd_acme(&webd).unwrap_err();
        assert!(matches!(
            err,
            AcmeResolveError::ToS(AcmeTosGateError::NotLetsEncryptUrl)
        ));
    }

    #[test]
    fn prod_vhost_with_le_tos_url_mints_proof() {
        let mut webd = webd_with(
            Some("0.0.0.0:80"),
            vec![vhost_row("a.example", Some(prod_acme()))],
        );
        webd.acme_tos_accepted =
            Some("https://letsencrypt.org/documents/LE-SA-v1.5-November-15-2024.pdf".into());
        let out = resolve_webd_acme(&webd).expect("prod with valid ToS must resolve");
        let proof = out
            .tos_acceptance
            .expect("prod vhost must produce a ToS proof");
        assert_eq!(
            proof.url(),
            "https://letsencrypt.org/documents/LE-SA-v1.5-November-15-2024.pdf"
        );
    }

    #[test]
    fn non_acme_vhost_does_not_require_http_listen() {
        // Manual-PEM vhost: http_listen absence must not be enforced
        // because no ACME plan exists to need it.
        let mut row = vhost_row("a.example", None);
        row.tls_cert = Some("/x/cert.pem".into());
        row.tls_key = Some("/x/key.pem".into());
        let webd = webd_with(None, vec![row]);
        let out = resolve_webd_acme(&webd).expect("non-acme rows skip the http_listen gate");
        assert!(out.plans.is_empty());
        assert!(out.tos_acceptance.is_none());
    }
}
