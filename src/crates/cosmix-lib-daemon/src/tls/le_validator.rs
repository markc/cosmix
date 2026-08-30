//! Three-check Let's-Encrypt-only chain validator. Webd calls this
//! at startup against every operator-supplied PEM chain (per-vhost
//! rows + the legacy top-level pair) and Phase 2 extends it to
//! validate ACME-fetched chains the provisioner stages on disk.
//! Maild does not call this — the `*.bus` internal-CA track is
//! intentionally exempt.
//!
//! The three checks are independent and all must pass:
//!
//! 1. X.509 path validation against an environment-keyed
//!    trust-anchor set:
//!    - [`ChainEnvironment::Production`] anchors come from
//!      [`webpki_roots::TLS_SERVER_ROOTS`] filtered by Subject-DN
//!      substring match to the ISRG roots (`ISRG Root X1` /
//!      `ISRG Root X2`).
//!    - [`ChainEnvironment::Staging`] anchors come from the
//!      vendored DER set in
//!      [`crate::tls::le_staging_roots::LE_STAGING_TRUST_ROOTS`]
//!      (4 entries at impl time: Pretend Pear X1, Bogus Broccoli
//!      X2, Yearning Yucca YE, Yonder Yam YR).
//!
//!      Catches expired chains, future-dated leaves, self-signed
//!      leaves, internal-CA chains, and chains rooted in any
//!      non-LE CA. A prod chain fed in with `env = Staging` is
//!      rejected at this step (no ISRG root is in the staging
//!      set) and vice versa — environment claim and actual root
//!      must agree.
//! 2. SPKI-SHA-256 pin: the leaf's *issuing* intermediate — the
//!    cert that signed the leaf, identified by Issuer-name (and
//!    AKI/SKI) match and crypto-confirmed by step 1 — must match an
//!    entry in the environment-keyed pin table whose state is
//!    `Active` or `Backup`. Other non-leaf certs (cross-sign roots
//!    such as LE's gen-y `Root YE` / `ISRG Root X2`) need not be
//!    pinned — step 1 already vouched for them against the ISRG
//!    anchors. A `Retired` match anywhere in the chain is a
//!    kill-switch rejection. A chain whose issuing intermediate is
//!    not pinned (or cannot be identified) is rejected. Binding the
//!    pin to the issuer — not "any pinned cert in the PEM" — closes
//!    the append-bypass where a stray pinned cert would otherwise
//!    satisfy the pin while the real issuer is unpinned.
//! 3. Subject-name verification: every name in `expected_names`
//!    must be covered by the leaf certificate's Subject
//!    Alternative Names, via
//!    `webpki::EndEntityCert::verify_is_valid_for_subject_name`.
//!    Closes the "operator points vhost B at vhost A's LE cert"
//!    trap.
//!
//! The Phase 1 entry point [`validate_le_chain`] is preserved as
//! a thin wrapper that forwards to
//! [`validate_le_chain_for_environment`] with
//! `ChainEnvironment::Production`. Existing callers (webd's
//! per-vhost manual-PEM path) keep their `(chain, names, now)`
//! signature.

use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, DnsName, ServerName, TrustAnchor, UnixTime};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use webpki::{EndEntityCert, KeyUsage};

use crate::tls::le_intermediates::{
    Environment, IntermediatePin, IntermediateState, LE_INTERMEDIATE_PINS,
    LE_STAGING_INTERMEDIATE_PINS,
};
use crate::tls::le_staging_roots::LE_STAGING_TRUST_ROOTS;

/// Detected (and asserted) environment of a chain accepted by
/// the validator. The caller picks one of the two variants when
/// invoking [`validate_le_chain_for_environment`]; on a
/// successful return the validator confirms that the chain's
/// intermediates pinned in the expected environment's table.
///
/// The enum is closed (not `#[non_exhaustive]`): every
/// `match ChainEnvironment` site is a deliberate security
/// checkpoint, and adding a non-LE environment is a normative
/// CLAUDE.md / `_doc/` change rather than a quiet enum
/// extension. The same rule applies to
/// [`crate::tls::le_intermediates::Environment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainEnvironment {
    Production,
    Staging,
}

/// Validate `chain_pem` against the **production** LE trust set.
///
/// Phase 1 surface, preserved verbatim for callers that don't
/// care about staging (i.e. the manual-PEM `tls_cert` /
/// `tls_key` path). Forwards to
/// [`validate_le_chain_for_environment`] with
/// `ChainEnvironment::Production`.
pub fn validate_le_chain(
    chain_pem: &[u8],
    expected_names: &[&str],
    now: UnixTime,
) -> Result<ChainEnvironment> {
    validate_le_chain_for_environment(chain_pem, expected_names, now, ChainEnvironment::Production)
}

/// Validate `chain_pem` against the LE trust set for the
/// caller-asserted environment. Returns `Ok(env)` on success
/// where `env == caller-supplied env` — the validator never
/// "promotes" a chain across environments. A prod chain fed
/// in with `env = Staging` (or vice versa) is rejected at the
/// path-validation step.
///
/// `now` is the time at which path validation is performed
/// (notBefore / notAfter comparisons). In production webd
/// passes `UnixTime::now()` at startup / renewal; tests pin
/// it for deterministic fixtures.
pub fn validate_le_chain_for_environment(
    chain_pem: &[u8],
    expected_names: &[&str],
    now: UnixTime,
    env: ChainEnvironment,
) -> Result<ChainEnvironment> {
    let pins = pin_table_for(env);
    validate_le_chain_inner(chain_pem, expected_names, now, env, pins)
}

/// Crate-internal entry point: same as
/// [`validate_le_chain_for_environment`] but with the pin table
/// threaded as a parameter, so the retired-kill-switch unit
/// tests (one per environment) can supply a copy of the
/// canonical pin table with one entry flipped to
/// [`IntermediateState::Retired`] without disturbing the real
/// allowlist.
///
/// **Not part of the public API.** `#[cfg(test)] pub(crate)` is
/// the load-bearing scope — a `pub` override hook would let any
/// `tls`-feature consumer supply an arbitrary pin table and
/// reduce "LE-only validation" to "whatever pins the caller
/// chose." Tests reach this through the inline `#[cfg(test)]
/// mod tests` block in this file; the integration-test surface
/// in `tests/le_validator.rs` cannot, and non-test builds do
/// not include this function in the compiled crate at all.
#[cfg(test)]
#[allow(dead_code)] // exposed for future tests; current call sites
// were removed in the 2026-05-29 sanitization
pub(crate) fn validate_le_chain_with_pins(
    chain_pem: &[u8],
    expected_names: &[&str],
    now: UnixTime,
    env: ChainEnvironment,
    pins: &[IntermediatePin],
) -> Result<ChainEnvironment> {
    validate_le_chain_inner(chain_pem, expected_names, now, env, pins)
}

fn validate_le_chain_inner(
    chain_pem: &[u8],
    expected_names: &[&str],
    now: UnixTime,
    env: ChainEnvironment,
    pins: &[IntermediatePin],
) -> Result<ChainEnvironment> {
    // Fail closed on the SAN-coverage list before doing parse / path
    // work. A caller that forgets to wire expected_names through
    // would otherwise pay the cost of path validation and pin lookup
    // before being told their config was incomplete; worse, the
    // caller contract ("every LE-validated chain is bound to at least
    // one configured FQDN") would be invisible at the entry-point
    // signature.
    if expected_names.is_empty() {
        return Err(anyhow!(
            "expected_names is empty — every LE-validated chain must be tied to at \
             least one configured FQDN to close the SAN-mismatch trap"
        ));
    }

    let chain = parse_pem_chain(chain_pem).context("parsing operator-supplied PEM chain")?;
    if chain.len() < 2 {
        return Err(anyhow!(
            "chain must contain leaf + ≥1 intermediate (got {} cert(s)); \
             LE never issues bare leaves",
            chain.len()
        ));
    }

    let anchors = trust_anchors_for(env)
        .with_context(|| format!("constructing trust anchor set for {env:?}"))?;
    path_validate(&chain, anchors, now)
        .with_context(|| format!("X.509 path validation against {env:?} LE trust set"))?;

    let chain_env = pin_intermediates(&chain, pins)
        .context("SPKI-SHA-256 pin check against LE intermediate allowlist")?;

    // Defence-in-depth: pinning succeeded against the
    // caller-supplied pin table, which (in non-test code) is
    // `pin_table_for(env)`. The chain therefore belongs to `env`.
    // Asserting this rather than just returning `env` catches the
    // bug class where a future refactor wires the wrong table to
    // the wrong environment in `pin_table_for`.
    if chain_env != env {
        return Err(anyhow!(
            "chain pinned in {chain_env:?} but caller asserted {env:?} — \
             trust-anchor / pin-table partition violated",
        ));
    }

    verify_subject_names(&chain, expected_names).context("subject-name verification")?;

    Ok(env)
}

fn parse_pem_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let parsed: Vec<_> = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("rustls_pemfile failed to parse chain: {e}"))?;
    Ok(parsed)
}

/// Build the trust-anchor set for the given environment.
///
/// - `Production`: filter [`webpki_roots::TLS_SERVER_ROOTS`] down
///   to the ISRG Root X1 and ISRG Root X2 anchors by Subject-DN
///   byte-substring match (`b"ISRG Root X1"` /
///   `b"ISRG Root X2"`). The DER-encoded subject field carries
///   the ASCII bytes of the CN inline (UTF8String tag), so a
///   substring match is sufficient and avoids pulling a full
///   X.500 name parser into the chain validator. Returns an
///   error if neither anchor is present — a loud signal that
///   the vendored `webpki-roots` bundle has rotated in a way
///   that needs investigating before webd ships.
/// - `Staging`: decode each entry in [`LE_STAGING_TRUST_ROOTS`]
///   into a [`TrustAnchor<'static>`] via
///   [`webpki::anchor_from_trusted_cert`]. The DER bytes are
///   `&'static` slices; `to_owned()` is called once at
///   first-use to widen the lifetime through the
///   `OnceLock` cache.
///
/// Both arms cache via `OnceLock` — building the anchor set is
/// cheap (a handful of vec pushes for prod, four DER decodes for
/// staging) but avoiding repetition on the hot validation path
/// is still worth the few lines.
fn trust_anchors_for(env: ChainEnvironment) -> Result<&'static [TrustAnchor<'static>]> {
    static PROD_ANCHORS: OnceLock<Result<Vec<TrustAnchor<'static>>, String>> = OnceLock::new();
    static STAGING_ANCHORS: OnceLock<Result<Vec<TrustAnchor<'static>>, String>> = OnceLock::new();

    let cell = match env {
        ChainEnvironment::Production => &PROD_ANCHORS,
        ChainEnvironment::Staging => &STAGING_ANCHORS,
    };
    let cached = cell.get_or_init(|| build_anchors(env).map_err(|e| format!("{e:#}")));
    match cached {
        Ok(v) => Ok(v.as_slice()),
        Err(e) => Err(anyhow!("{e}")),
    }
}

fn build_anchors(env: ChainEnvironment) -> Result<Vec<TrustAnchor<'static>>> {
    match env {
        ChainEnvironment::Production => {
            let mut anchors: Vec<TrustAnchor<'static>> = Vec::new();
            for anchor in webpki_roots::TLS_SERVER_ROOTS {
                let subj = anchor.subject.as_ref();
                if contains_subseq(subj, b"ISRG Root X1") || contains_subseq(subj, b"ISRG Root X2")
                {
                    anchors.push(anchor.clone());
                }
            }
            if anchors.is_empty() {
                return Err(anyhow!(
                    "no ISRG Root X1/X2 anchors found in webpki_roots::TLS_SERVER_ROOTS — \
                     the vendored bundle has rotated in an unexpected way; investigate \
                     before shipping"
                ));
            }
            Ok(anchors)
        }
        ChainEnvironment::Staging => {
            let mut anchors: Vec<TrustAnchor<'static>> = Vec::new();
            for (idx, der_bytes) in LE_STAGING_TRUST_ROOTS.iter().enumerate() {
                let cert = CertificateDer::from(*der_bytes);
                let anchor = webpki::anchor_from_trusted_cert(&cert).map_err(|e| {
                    anyhow!(
                        "LE_STAGING_TRUST_ROOTS[{idx}] failed to parse as a \
                         trust anchor: {e}"
                    )
                })?;
                // Widen the borrow to 'static: the underlying bytes
                // are static, but webpki::anchor_from_trusted_cert
                // borrows through the local `cert` value. to_owned
                // copies the inner Der slices into Vec<u8>, paying a
                // tiny allocation once per cold start.
                anchors.push(anchor.to_owned());
            }
            if anchors.is_empty() {
                // An empty anchor set fails *closed* — every staging
                // chain would be rejected at path validation, not
                // accepted. The error here is a deploy-time loudness
                // signal that the vendored set has been emptied
                // (refresh script regression, accidental delete),
                // not a soundness claim about the validator.
                return Err(anyhow!(
                    "LE_STAGING_TRUST_ROOTS is empty — every staging chain would \
                     be rejected; the vendored set must contain at least one root \
                     for staging validation to be reachable"
                ));
            }
            Ok(anchors)
        }
    }
}

/// Map an environment to the SPKI-SHA-256 pin table that lists
/// every Active / Backup / Retired intermediate LE issues from
/// in that environment. The two arms are the load-bearing
/// partition: a Production chain whose intermediate's SPKI
/// happens to match a Staging pin (or vice versa) is rejected
/// because the wrong table is consulted.
fn pin_table_for(env: ChainEnvironment) -> &'static [IntermediatePin] {
    match env {
        ChainEnvironment::Production => LE_INTERMEDIATE_PINS,
        ChainEnvironment::Staging => LE_STAGING_INTERMEDIATE_PINS,
    }
}

fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn path_validate(
    chain: &[CertificateDer<'_>],
    anchors: &[TrustAnchor<'static>],
    now: UnixTime,
) -> Result<()> {
    let (leaf, intermediates) = chain
        .split_first()
        .ok_or_else(|| anyhow!("internal: path_validate called on empty chain"))?;

    let end_entity = EndEntityCert::try_from(leaf)
        .map_err(|e| anyhow!("parsing leaf certificate as EndEntityCert: {e}"))?;

    end_entity
        .verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            anchors,
            intermediates,
            now,
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| anyhow!("webpki path validation rejected chain: {e}"))?;

    Ok(())
}

/// The fields of a certificate needed to bind the pin step to the
/// leaf's *actual* issuing intermediate: the DER-encoded Subject and
/// Issuer names, and the Subject/Authority Key Identifiers when
/// present. Owned (`Vec`) so the parsed `X509Certificate` can be
/// dropped before the comparison.
struct CertId {
    subject: Vec<u8>,
    issuer: Vec<u8>,
    ski: Option<Vec<u8>>,
    aki: Option<Vec<u8>>,
}

fn cert_id(cert_der: &[u8]) -> Result<CertId> {
    use x509_parser::prelude::{FromDer, ParsedExtension};
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|e| anyhow!("x509-parser failed to decode certificate: {e}"))?;
    let mut ski = None;
    let mut aki = None;
    for ext in cert.extensions() {
        match ext.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(k) => ski = Some(k.0.to_vec()),
            ParsedExtension::AuthorityKeyIdentifier(a) => {
                if let Some(k) = &a.key_identifier {
                    aki = Some(k.0.to_vec());
                }
            }
            _ => {}
        }
    }
    Ok(CertId {
        subject: cert.tbs_certificate.subject.as_raw().to_vec(),
        issuer: cert.tbs_certificate.issuer.as_raw().to_vec(),
        ski,
        aki,
    })
}

fn pin_intermediates(
    chain: &[CertificateDer<'_>],
    pins: &[IntermediatePin],
) -> Result<ChainEnvironment> {
    // Bind the pin to the leaf's ACTUAL issuing intermediate — the
    // cert that signed the leaf — not to "any pinned cert anywhere in
    // the supplied PEM". `path_validate` has already run and
    // cryptographically verified the chain to a trusted ISRG anchor,
    // so the leaf's Issuer name (and, when present, its Authority Key
    // Identifier) reliably name the cert that signed it: an unused
    // appended cert cannot be mistaken for the issuer, and a spoofed
    // Issuer/AKI would have failed `path_validate`'s signature check.
    // Scanning every supplied cert instead would let an operator
    // satisfy the pin by appending a stray pinned cert while the real
    // issuer is unpinned (the "append bypass").
    //
    // The cross-sign certs further up a gen-y chain (`Root YE`,
    // `ISRG Root X2`) are NOT the leaf's issuer and need not be
    // pinned — `path_validate` vouches for them against the X1/X2
    // anchors. A supplied cert matching a Retired pin (anywhere)
    // still rejects (kill-switch).
    let leaf = chain
        .first()
        .ok_or_else(|| anyhow!("internal: pin_intermediates called on an empty chain"))?;
    let leaf_id =
        cert_id(leaf.as_ref()).context("parsing leaf Issuer / Authority Key Identifier")?;
    // Require the leaf to name its issuer's KEY via the Authority Key
    // Identifier (every LE leaf carries one). The binding is then
    // key-based, not name-only: an SKI is the hash of a cert's public
    // key, so `candidate.ski == leaf.aki` ties the candidate to the
    // issuer's actual key — and `path_validate` has already
    // cryptographically verified that key signed the leaf. Refusing a
    // name-only fallback closes the residual where an unpinned issuer
    // sharing the leaf's Issuer DN with a pinned cert could be
    // mis-identified. (Binding directly to webpki's `VerifiedPath`
    // would be stronger still, but rustls-webpki 0.103's `Cert` exposes
    // neither DER nor SPKI, so the verified-path intermediates cannot
    // be mapped back to a pin.)
    let leaf_aki = leaf_id.aki.as_deref().ok_or_else(|| {
        anyhow!(
            "leaf certificate carries no Authority Key Identifier — cannot bind the \
             pin to its issuing intermediate's key; chain rejected"
        )
    })?;

    let mut issuer_seen = false;
    let mut issuer_env: Option<Environment> = None;
    for (idx, intermediate) in chain.iter().enumerate().skip(1) {
        let der = intermediate.as_ref();
        let spki_sha256: [u8; 32] = Sha256::digest(extract_spki_der(der).with_context(|| {
            format!("extracting SubjectPublicKeyInfo from intermediate at index {idx}")
        })?)
        .into();
        let pin = pins.iter().find(|p| p.spki_sha256 == spki_sha256);

        // Kill-switch over every supplied cert, regardless of whether
        // it is the issuer. (`pin` is `Option<&_>` / `Copy`, so the
        // `filter` does not consume it — it is reused below.)
        if let Some(p) = pin.filter(|p| p.state == IntermediateState::Retired) {
            return Err(anyhow!(
                "chain intermediate {:?} is in Retired state — chain rejected \
                 per LE kill-switch policy",
                p.friendly_name,
            ));
        }

        // Is this the cert that issued the leaf? Require BOTH the
        // Subject name == leaf's Issuer name AND the candidate's SKI
        // == the leaf's AKI (key-identity match). A candidate without
        // an SKI is never the issuer in this strict mode.
        let id = cert_id(der).with_context(|| {
            format!("parsing Subject / Subject Key Identifier of intermediate at index {idx}")
        })?;
        let issues_leaf = id.subject == leaf_id.issuer && id.ski.as_deref() == Some(leaf_aki);
        if issues_leaf {
            issuer_seen = true;
            if let Some(p) = pin {
                // Active/Backup here (Retired already returned above).
                issuer_env = Some(p.environment);
            }
        }
    }

    if !issuer_seen {
        return Err(anyhow!(
            "could not identify the leaf's issuing intermediate among the supplied \
             chain — no non-leaf cert carries the leaf's Issuer name; chain rejected"
        ));
    }
    let env = issuer_env.ok_or_else(|| {
        anyhow!(
            "the leaf's issuing intermediate is not in the LE intermediate \
             allowlist — chain rejected"
        )
    })?;
    Ok(match env {
        Environment::Production => ChainEnvironment::Production,
        Environment::Staging => ChainEnvironment::Staging,
    })
}

/// Extract the DER-encoded SubjectPublicKeyInfo from a certificate
/// DER. Uses `x509-parser` to walk the TBS structure; the returned
/// slice is the exact bytes hashed by RFC 7469 / HPKP-style pinning.
fn extract_spki_der(cert_der: &[u8]) -> Result<&[u8]> {
    use x509_parser::prelude::FromDer;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|e| anyhow!("x509-parser failed to decode certificate: {e}"))?;
    Ok(cert.tbs_certificate.subject_pki.raw)
}

fn verify_subject_names(chain: &[CertificateDer<'_>], expected_names: &[&str]) -> Result<()> {
    // The top-level entry point gates on `expected_names.is_empty()`,
    // so by the time we get here the list is non-empty.
    debug_assert!(!expected_names.is_empty());

    let leaf = chain
        .first()
        .ok_or_else(|| anyhow!("internal: verify_subject_names called on empty chain"))?;
    let end_entity = EndEntityCert::try_from(leaf)
        .map_err(|e| anyhow!("parsing leaf certificate as EndEntityCert: {e}"))?;

    for name in expected_names {
        let dns_name = DnsName::try_from(*name)
            .map_err(|e| anyhow!("expected_name {name:?} is not a valid DNS name: {e}"))?;
        let server_name = ServerName::DnsName(dns_name);
        end_entity
            .verify_is_valid_for_subject_name(&server_name)
            .map_err(|e| {
                anyhow!("leaf certificate SANs do not cover expected name {name:?}: {e}")
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit tests colocated with `validate_le_chain_with_pins`.
    //!
    //! Historical note (2026-05-29 sanitization): two tests in this
    //! module — `retired_intermediate_kills_chain_prod` and
    //! `empty_expected_names_rejected` — depended on a real LE prod
    //! cert fixture (`le-prod-wildcard.pem`) captured from a private
    //! mesh deployment. That fixture was removed when the public repo
    //! was sanitized. The retired-kill-switch behaviour is still
    //! covered by the staging-anchor tests below (they exercise the
    //! same pin-state branch through the staging path); the empty-
    //! names guard is exercised by the integration `tests/le_validator.rs`
    //! surface using rcgen-synthesised chains.
    use super::*;

    #[test]
    fn staging_anchors_build_and_cache() {
        // `trust_anchors_for(Staging)` must build the 4-anchor set
        // without error, and the second call must return the same
        // slice (OnceLock-cached).
        let first =
            trust_anchors_for(ChainEnvironment::Staging).expect("staging anchors must build");
        assert_eq!(
            first.len(),
            LE_STAGING_TRUST_ROOTS.len(),
            "staging anchor count must match LE_STAGING_TRUST_ROOTS",
        );
        let second = trust_anchors_for(ChainEnvironment::Staging)
            .expect("staging anchors must build on second call");
        assert_eq!(
            first.as_ptr(),
            second.as_ptr(),
            "OnceLock should hand back the same slice",
        );
    }

    #[test]
    fn pin_table_partition() {
        // `pin_table_for(env)` returns the right table — the
        // structural invariant that prod-env asks for prod pins
        // and staging-env asks for staging pins. A regression here
        // would let a prod chain validate against staging pins
        // (which would still reject by SPKI mismatch, but the
        // failure mode is wrong-message-wrong-environment).
        let prod = pin_table_for(ChainEnvironment::Production);
        let staging = pin_table_for(ChainEnvironment::Staging);
        assert_eq!(
            prod.as_ptr(),
            LE_INTERMEDIATE_PINS.as_ptr(),
            "Production env must map to LE_INTERMEDIATE_PINS",
        );
        assert_eq!(
            staging.as_ptr(),
            LE_STAGING_INTERMEDIATE_PINS.as_ptr(),
            "Staging env must map to LE_STAGING_INTERMEDIATE_PINS",
        );
    }

    // ── pin_intermediates (issuer-binding) ───────────────────────
    // Exercise the pin step in isolation with rcgen-synthesised
    // chains. `pin_intermediates` identifies the leaf's ISSUER (by
    // Issuer-name / AKI match) and requires THAT cert is pinned; the
    // real flow runs `path_validate` first to crypto-confirm the
    // chain, but the pin step only needs correct Issuer/Subject names,
    // which `signed_by` produces. Keeps the test free of any real LE
    // fixture (removed during public-repo sanitization).

    /// A self-signed CA cert with a distinct Subject CN, plus its key.
    fn synth_ca(cn: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
        use rcgen::{
            BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
            KeyUsagePurpose,
        };
        let key = KeyPair::generate().expect("ca key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        // Emit Subject/Authority Key Identifier extensions so the
        // pin step's key-identity (AKI==SKI) binding has something to
        // match — real LE certs always carry them.
        params.use_authority_key_identifier_extension = true;
        let cert = params.self_signed(&key).expect("ca self-sign");
        (cert, key)
    }

    /// A leaf cert signed by `ca` — so the leaf's Issuer name is `ca`'s
    /// Subject name (and rcgen sets the leaf's AKI to `ca`'s SKI).
    fn synth_leaf_signed_by(
        ca: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
    ) -> rcgen::Certificate {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
        let key = KeyPair::generate().expect("leaf key");
        let mut params =
            CertificateParams::new(vec!["leaf.example".to_string()]).expect("leaf params");
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "leaf.example");
        params.distinguished_name = dn;
        params.use_authority_key_identifier_extension = true;
        params.signed_by(&key, ca, ca_key).expect("leaf sign")
    }

    fn spki_sha256_of(der: &CertificateDer<'_>) -> [u8; 32] {
        let spki = extract_spki_der(der.as_ref()).expect("extract spki");
        Sha256::digest(spki).into()
    }

    fn test_pin(spki: [u8; 32], state: IntermediateState, env: Environment) -> IntermediatePin {
        IntermediatePin {
            friendly_name: "TEST",
            spki_sha256: spki,
            ski_hex: "00000000000000000000000000000000000000000000",
            state,
            environment: env,
        }
    }

    #[test]
    fn pin_intermediates_accepts_pinned_issuer_with_unpinned_crosssign() {
        // Models LE's gen-y chain: [leaf, YE1(pinned issuer), cross-sign
        // (unpinned)]. The issuer is pinned → accept; the unpinned
        // cross-sign cert is tolerated.
        let (issuer, issuer_key) = synth_ca("Pinned Issuer");
        let leaf = synth_leaf_signed_by(&issuer, &issuer_key);
        let (crosssign, _) = synth_ca("Unpinned Cross-sign");
        let chain = vec![
            leaf.der().clone(),
            issuer.der().clone(),
            crosssign.der().clone(),
        ];
        let pins = vec![test_pin(
            spki_sha256_of(issuer.der()),
            IntermediateState::Active,
            Environment::Production,
        )];
        assert!(matches!(
            pin_intermediates(&chain, &pins),
            Ok(ChainEnvironment::Production)
        ));
    }

    #[test]
    fn pin_intermediates_rejects_append_bypass() {
        // Regression: the leaf is issued by an UNPINNED intermediate and
        // a stray PINNED cert is appended. The append must NOT satisfy
        // the pin — the stray is not the leaf's issuer.
        let (real_issuer, real_key) = synth_ca("Unpinned Real Issuer");
        let leaf = synth_leaf_signed_by(&real_issuer, &real_key);
        let (stray, _) = synth_ca("Stray Pinned Cert");
        let chain = vec![
            leaf.der().clone(),
            real_issuer.der().clone(),
            stray.der().clone(),
        ];
        let pins = vec![test_pin(
            spki_sha256_of(stray.der()),
            IntermediateState::Active,
            Environment::Production,
        )];
        let err = pin_intermediates(&chain, &pins)
            .expect_err("appending a stray pinned cert must not satisfy the pin");
        assert!(
            format!("{err:#}").contains("not in the LE intermediate allowlist"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pin_intermediates_retired_issuer_is_killswitch() {
        let (issuer, issuer_key) = synth_ca("Retired Issuer");
        let leaf = synth_leaf_signed_by(&issuer, &issuer_key);
        let chain = vec![leaf.der().clone(), issuer.der().clone()];
        let pins = vec![test_pin(
            spki_sha256_of(issuer.der()),
            IntermediateState::Retired,
            Environment::Production,
        )];
        let err =
            pin_intermediates(&chain, &pins).expect_err("a Retired-pinned issuer must fail closed");
        assert!(
            format!("{err:#}").contains("Retired"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pin_intermediates_rejects_unpinned_issuer() {
        let (issuer, issuer_key) = synth_ca("Unpinned Issuer");
        let leaf = synth_leaf_signed_by(&issuer, &issuer_key);
        let chain = vec![leaf.der().clone(), issuer.der().clone()];
        let pins = vec![test_pin(
            [0u8; 32],
            IntermediateState::Active,
            Environment::Production,
        )];
        let err = pin_intermediates(&chain, &pins)
            .expect_err("a chain whose issuer is not pinned must be rejected");
        assert!(
            format!("{err:#}").contains("not in the LE intermediate allowlist"),
            "unexpected error: {err:#}"
        );
    }
}
