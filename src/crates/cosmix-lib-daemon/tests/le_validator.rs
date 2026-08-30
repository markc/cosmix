//! Bucket B — LE chain validator + intermediate-pin tests.
//!
//! Two sources of test fixtures:
//!
//! - **Rejection-class synthetic chains**: rcgen-built at test time
//!   (self-signed leaves, internal-CA chains, DigiCert-spoof chains).
//!   Building them on the fly keeps the repository free of vendored
//!   PEM blobs whose validity is bounded by `rcgen`'s defaults rather
//!   than the test's intent.
//!
//! (The historical "real LE chains on disk" fixture set was removed
//! during the 2026-05-29 public-repo sanitization — the certs were
//! issued for the operator's private mesh FQDN, which we don't want
//! in a public repo. Operators wanting a prod-LE integration test can
//! drop their own fixtures into `tests/fixtures/` for a local-only
//! test run; they are not part of the public test suite.)

#![cfg(feature = "tls")]

use std::time::Duration;

use cosmix_daemon::tls::le_validator::validate_le_chain;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::UnixTime;
use time::OffsetDateTime;

// ── Time anchors ─────────────────────────────────────────────────
//
// (Historical) le-prod-wildcard.pem leaf validity was 2026-04-21 → 2026-07-20.
// E8 intermediate validity is 2024-03-13 → 2027-03-12.
// Anchors below pin `now` to keep the tests deterministic across
// real-cert lifetimes.

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

fn inside_le_validity() -> UnixTime {
    unix(2026, time::Month::May, 15)
}

// `after_le_notafter` removed in the 2026-05-29 sanitization (it was
// only used by tests that depended on the LE prod cert fixture).

// ── rcgen helpers ────────────────────────────────────────────────

/// Build a 1-cert PEM containing a single self-signed leaf (issuer
/// == subject). Models the "operator pasted a self-signed cert as
/// their `tls_cert`" mistake.
fn synth_selfsigned_pem(common_name: &str, sans: &[&str]) -> Vec<u8> {
    let key = KeyPair::generate().expect("rcgen keypair");
    let mut params = CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("rcgen params");
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    let cert = params.self_signed(&key).expect("rcgen self-sign");
    cert.pem().into_bytes()
}

/// Build a 2-cert PEM [leaf, intermediate] where the intermediate is
/// signed by a root carrying `root_cn`, and the intermediate carries
/// `intermediate_cn`. Path validation against an ISRG-only anchor
/// set must reject regardless of the CN strings (they are signal for
/// human readers, not trust).
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

/// Build a 2-cert PEM [leaf, intermediate] where the leaf has the
/// given validity window. Used to assert that path validation
/// enforces dates without depending on the LE-issued chain's window.
fn synth_chain_pem_with_validity(
    leaf_cn: &str,
    leaf_sans: &[&str],
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
) -> Vec<u8> {
    let root_key = KeyPair::generate().expect("root key");
    let mut root_params = CertificateParams::new(Vec::<String>::new()).expect("root params");
    let mut root_dn = DistinguishedName::new();
    root_dn.push(DnType::CommonName, "synthetic test root");
    root_params.distinguished_name = root_dn;
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let root_cert = root_params.self_signed(&root_key).expect("root sign");

    let int_key = KeyPair::generate().expect("int key");
    let mut int_params = CertificateParams::new(Vec::<String>::new()).expect("int params");
    let mut int_dn = DistinguishedName::new();
    int_dn.push(DnType::CommonName, "synthetic test intermediate");
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
    leaf_params.not_before = not_before;
    leaf_params.not_after = not_after;
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

// ── Tests ────────────────────────────────────────────────────────

#[test]
fn selfsigned_chain_rejected_at_path_validation() {
    let pem = synth_selfsigned_pem("self.example", &["self.example"]);
    let err = validate_le_chain(&pem, &["self.example"], inside_le_validity())
        .expect_err("self-signed chain must be rejected");
    // A self-signed leaf produces a 1-cert PEM. The validator's
    // length pre-check short-circuits; the rejection message names
    // the missing intermediate.
    let msg = format!("{err:#}");
    assert!(
        msg.contains("intermediate") || msg.contains("Intermediate"),
        "unexpected rejection: {msg}"
    );
}

#[test]
fn internalca_chain_rejected_path_anchor_mismatch() {
    let pem = synth_chain_pem(
        "Cosmix Internal Root CA",
        "Cosmix Internal Issuing CA",
        "internal.example",
        &["internal.example"],
    );
    let err = validate_le_chain(&pem, &["internal.example"], inside_le_validity())
        .expect_err("internal-CA chain must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("path validation"),
        "expected path-validation rejection, got: {msg}"
    );
}

#[test]
fn digicert_spoof_rejected_signature_mismatch() {
    let pem = synth_chain_pem(
        "DigiCert Global Root CA",
        "DigiCert SHA2 Secure Server CA",
        "spoofed.example",
        &["spoofed.example"],
    );
    let err = validate_le_chain(&pem, &["spoofed.example"], inside_le_validity())
        .expect_err("DigiCert-named spoof chain must be rejected");
    let msg = format!("{err:#}");
    // The rejection happens at path validation — the spoofed root is
    // not in the ISRG-only anchor set, and crucially its key does not
    // chain to the real DigiCert roots either. CN strings are not
    // trust.
    assert!(
        msg.contains("path validation"),
        "expected path-validation rejection, got: {msg}"
    );
}

// Six historical prod-LE tests removed during the 2026-05-29
// public-repo sanitization: `le_prod_chain_validates_returns_production`,
// `le_prod_no_intermediate_rejected_with_clear_error`,
// `expired_leaf_rejected_path_validation`,
// `le_chain_san_does_not_cover_expected_host_rejected`,
// `le_chain_san_covers_alias_via_wildcard`,
// `rejects_prod_chain_when_env_staging`. They depended on real LE
// certs issued for the operator's mesh-private FQDN and cannot be
// kept in the public repo without leaking that domain. The synthetic
// rcgen-based tests above still cover the validator's logic
// generically (path validation, SAN coverage, anchor mismatch,
// signature mismatch, validity window). Operators wanting full
// prod-LE coverage can drop their own fixtures into
// `tests/fixtures/` for a local-only test run.

// `retired_intermediate_kills_chain` lives as a unit test inside
// `src/tls/le_validator.rs` so it can exercise the `pub(crate)` pin-
// override hook without exposing it across the crate boundary.

#[test]
fn not_yet_valid_leaf_rejected_path_validation() {
    // Use a synthetic chain so we can pin notBefore far in the future
    // without depending on the LE fixture being captured before its
    // own notBefore (which it never is — the live chain is always
    // already valid).
    let future_not_before =
        OffsetDateTime::from_unix_timestamp(unix_secs(2099, time::Month::January, 1) as i64)
            .unwrap();
    let future_not_after =
        OffsetDateTime::from_unix_timestamp(unix_secs(2099, time::Month::June, 1) as i64).unwrap();
    let pem = synth_chain_pem_with_validity(
        "future.example",
        &["future.example"],
        future_not_before,
        future_not_after,
    );
    let err = validate_le_chain(&pem, &["future.example"], inside_le_validity())
        .expect_err("not-yet-valid chain must be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("path validation"),
        "expected path-validation rejection, got: {msg}"
    );
}

// ── Bucket B-S (staging) ─────────────────────────────────────────
//
// The historical B-S set was:
//
//   1. `rejects_prod_chain_when_env_staging` — REMOVED 2026-05-29
//      sanitization (depended on the prod wildcard fixture).
//   2. `validates_le_staging_real_chain` — deferred to P2-C5's
//      instant-acme preflight; requires a real LE-staging chain.
//   3. `rejects_staging_chain_when_env_production` — deferred with #2.
//   4. `rejects_unknown_intermediate_in_staging` — mirrored as a
//      crate-internal unit test (`src/tls/le_validator.rs`).
//
// The environment-partition security property (a chain signed by the
// wrong-environment root cannot launder via the other env) is
// exercised by the `*-environment*` unit tests inside
// `src/tls/le_validator.rs` against synthetic chains.

// ── helpers used only by the not-yet-valid test ──────────────────

fn unix_secs(year: i32, month: time::Month, day: u8) -> u64 {
    let ts = OffsetDateTime::from_unix_timestamp(0).unwrap();
    let dt = ts
        .replace_year(year)
        .unwrap()
        .replace_month(month)
        .unwrap()
        .replace_day(day)
        .unwrap();
    dt.unix_timestamp() as u64
}
