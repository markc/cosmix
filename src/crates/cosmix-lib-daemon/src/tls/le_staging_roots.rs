//! Vendored Let's Encrypt **staging** root certificates.
//!
//! LE staging deliberately ships roots that no public CA bundle
//! trusts — that is the whole point: staging chains issue
//! syntactically valid certs without polluting LE prod issuance
//! rate limits or the trust reputation of `ISRG Root X1` / `X2`.
//! Webd needs to accept staging chains so an operator can prove
//! their `http_listen` + DNS + firewall setup works end-to-end
//! before flipping `provider = letsencrypt_prod`.
//!
//! Source: <https://letsencrypt.org/docs/staging-environment/>
//! (root index) and
//! <https://github.com/letsencrypt/website/tree/main/static/certs/staging>
//! (DER bytes), fetched **2026-05-21**. The set at that date is
//! four roots:
//!
//! | Friendly name              | Key      | DER file                            |
//! |----------------------------|----------|-------------------------------------|
//! | (STAGING) Pretend Pear X1  | RSA 4096 | `staging_roots/letsencrypt-stg-root-x1.der` |
//! | (STAGING) Bogus Broccoli X2| EC P-384 | `staging_roots/letsencrypt-stg-root-x2.der` |
//! | (STAGING) Yearning Yucca YE| EC P-384 | `staging_roots/letsencrypt-stg-root-ye.der` |
//! | (STAGING) Yonder Yam YR    | RSA 4096 | `staging_roots/letsencrypt-stg-root-yr.der` |
//!
//! Refresh protocol: when LE adds, retires, or rotates a staging
//! root, download the new DER from
//! `static/certs/staging/...der` in the website repo, drop it
//! under `staging_roots/`, and add the `include_bytes!` entry +
//! a row in [`LE_STAGING_TRUST_ROOTS`]. The set is shipped as a
//! `&'static [&'static [u8]]` so adding or rotating staging roots
//! is a single-line code edit plus a rebuild. A staging-cert
//! deploy that fails validation because LE has added a fifth
//! root surfaces as a build-once / patch-once / deploy-once
//! cycle — acceptable for a *testing* environment that does not
//! serve user traffic.
//!
//! Bytes vendored as DER (not PEM) so the validator's
//! decode path is `CertificateDer::from(bytes)` → trust anchor,
//! with no base64 round-trip on every cold start.

/// `(STAGING) Pretend Pear X1` — RSA 4096 self-signed root.
pub static FAKE_LE_ROOT_X1_DER: &[u8] = include_bytes!("staging_roots/letsencrypt-stg-root-x1.der");

/// `(STAGING) Bogus Broccoli X2` — ECDSA P-384 self-signed root.
pub static FAKE_LE_ROOT_X2_DER: &[u8] = include_bytes!("staging_roots/letsencrypt-stg-root-x2.der");

/// `(STAGING) Yearning Yucca Root YE` — ECDSA P-384 self-signed
/// root. Part of LE's 2024 "gen-Y" root rollout.
pub static FAKE_LE_ROOT_YE_DER: &[u8] = include_bytes!("staging_roots/letsencrypt-stg-root-ye.der");

/// `(STAGING) Yonder Yam Root YR` — RSA 4096 self-signed root.
/// Part of LE's 2024 "gen-Y" root rollout.
pub static FAKE_LE_ROOT_YR_DER: &[u8] = include_bytes!("staging_roots/letsencrypt-stg-root-yr.der");

/// The full LE staging trust-anchor set, in source order.
///
/// `validate_le_chain_for_environment` consumes this slice when
/// `env == ChainEnvironment::Staging` to construct the
/// path-validation anchor set. Order is informational; the
/// validator builds a trust anchor per entry and rustls's path
/// engine picks the matching one.
pub static LE_STAGING_TRUST_ROOTS: &[&[u8]] = &[
    FAKE_LE_ROOT_X1_DER,
    FAKE_LE_ROOT_X2_DER,
    FAKE_LE_ROOT_YE_DER,
    FAKE_LE_ROOT_YR_DER,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_root_der_files_non_empty() {
        for (name, der) in [
            ("X1", FAKE_LE_ROOT_X1_DER),
            ("X2", FAKE_LE_ROOT_X2_DER),
            ("YE", FAKE_LE_ROOT_YE_DER),
            ("YR", FAKE_LE_ROOT_YR_DER),
        ] {
            assert!(!der.is_empty(), "staging root {name} DER must not be empty");
            assert!(
                der.len() > 200,
                "staging root {name} DER ({} bytes) too short to be a real cert",
                der.len(),
            );
        }
    }

    #[test]
    fn staging_root_set_has_four_entries() {
        assert_eq!(
            LE_STAGING_TRUST_ROOTS.len(),
            4,
            "LE staging set captured 2026-05-21 has exactly four roots; \
             update both this count and the module doc if LE rotates the set",
        );
    }

    #[test]
    fn staging_root_set_entries_unique() {
        use std::collections::HashSet;
        let mut seen: HashSet<&[u8]> = HashSet::new();
        for der in LE_STAGING_TRUST_ROOTS {
            assert!(
                seen.insert(der),
                "duplicate staging-root DER in LE_STAGING_TRUST_ROOTS"
            );
        }
    }

    #[test]
    fn staging_roots_parse_as_x509() {
        // Sanity: each vendored DER decodes as an X.509 certificate.
        // Anything that fails here means we vendored a malformed
        // file (truncated download, PEM-instead-of-DER, etc.).
        use x509_parser::prelude::FromDer;
        for (idx, der) in LE_STAGING_TRUST_ROOTS.iter().enumerate() {
            let (_, _cert) = x509_parser::certificate::X509Certificate::from_der(der)
                .unwrap_or_else(|e| panic!("staging root #{idx} failed to parse as X.509: {e}"));
        }
    }

    #[test]
    fn staging_roots_self_signed_and_marked_staging() {
        // Defence-in-depth on the vendored DER:
        // 1. Subject DN contains the literal `(STAGING)` marker so a
        //    production root accidentally dropped into `staging_roots/`
        //    fails the suite immediately rather than being trusted
        //    silently.
        // 2. Subject == Issuer (self-issued), the structural marker of
        //    a self-signed root.
        // 3. The cert verifies under *its own* SubjectPublicKeyInfo —
        //    a real cryptographic self-signature check, not just the
        //    subject==issuer naming match. Without this step a hostile
        //    vendor could ship a cert whose subject/issuer agree but
        //    whose signature is bogus; webpki would later reject it
        //    at path validation, but it would also silently rotate
        //    *out* of the trust set if a refresh shipped corrupt
        //    bytes — and a passing parse test would not catch that.
        use x509_parser::prelude::FromDer;
        for (idx, der) in LE_STAGING_TRUST_ROOTS.iter().enumerate() {
            let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der)
                .unwrap_or_else(|e| panic!("staging root #{idx} failed to parse as X.509: {e}"));

            let subj_bytes = cert.tbs_certificate.subject.as_raw();
            assert!(
                subj_bytes
                    .windows(b"(STAGING)".len())
                    .any(|w| w == b"(STAGING)"),
                "staging root #{idx} subject does not contain the '(STAGING)' marker — \
                 vendored DER may be a production root by mistake",
            );

            assert_eq!(
                cert.tbs_certificate.subject, cert.tbs_certificate.issuer,
                "staging root #{idx} is not self-issued (subject != issuer)",
            );

            // `verify_signature(None)` uses the cert's own SPKI as the
            // verifying key — the ring-backed cryptographic check that
            // distinguishes "self-issued" from "self-signed."
            cert.verify_signature(None).unwrap_or_else(|e| {
                panic!(
                    "staging root #{idx} failed self-signature verification: {e:?} — \
                     vendored DER is corrupt or not actually self-signed"
                )
            });
        }
    }
}
