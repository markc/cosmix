//! DKIM keypair generation — RSA-2048 (PKCS1 PEM) and Ed25519 (PKCS8 PEM).
//!
//! Thin wrapper around `mail_auth::dkim::generate::DkimKeyPair` that
//! emits the on-disk shapes [`crate::signer::sign_with_config`] expects
//! to read back, plus the matching `_domainkey` DNS TXT record snippet.
//!
//! Output materials:
//! - `private_pem`: ready to drop into `/var/lib/cosmix/maild/dkim/...`
//! - `public_b64`: raw base64 (no DKIM wrapping), exposed so callers
//!   can rebuild a custom TXT record if `dns_txt_record` doesn't fit.
//! - `dns_txt_record`: the full `v=DKIM1; ... p=...` value that goes
//!   into the `<selector>._domainkey.<domain>` TXT record.
//!
//! `t=s` is NOT included by default: the signer's parent-label walk-up
//! (`MailAuthSigner::lookup_active`) signs subdomain mail with the
//! org-domain key, which `t=s` would reject. Callers that pin one
//! selector to one exact domain can append `; t=s` themselves.

use crate::error::{Error, Result};
use crate::types::DkimAlgorithm;
use mail_auth::dkim::generate::DkimKeyPair;

/// What to generate. Bits for RSA must be ≥ 1024 — 2048 is the modern
/// default; 1024 is still accepted for legacy interop but flagged by
/// receivers as weak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySpec {
    Rsa { bits: usize },
    Ed25519,
}

#[derive(Debug, Clone)]
pub struct DkimKeyMaterial {
    pub algorithm: DkimAlgorithm,
    pub private_pem: String,
    pub public_b64: String,
    pub dns_txt_record: String,
}

/// Generate a fresh keypair. RSA uses the `rsa` crate (mail-auth pulls
/// it; `ring` doesn't yet do RSA keygen — see the upstream `//TODO`
/// in `mail_auth::dkim::generate`). Ed25519 routes through `ring` via
/// `mail_auth::common::crypto::Ed25519Key::generate_pkcs8`.
pub fn generate(spec: KeySpec) -> Result<DkimKeyMaterial> {
    match spec {
        KeySpec::Rsa { bits } => {
            if bits < 1024 {
                return Err(Error::KeyLoad(format!(
                    "RSA key size {bits} is below the 1024-bit floor"
                )));
            }
            let pair = DkimKeyPair::generate_rsa(bits)
                .map_err(|e| Error::KeyLoad(format!("RSA keygen: {e}")))?;
            let private_pem = pem_encode("RSA PRIVATE KEY", pair.private_key());
            let public_b64 = pair.encoded_public_key();
            let dns_txt_record = format!("v=DKIM1; k=rsa; p={public_b64}");
            Ok(DkimKeyMaterial {
                algorithm: DkimAlgorithm::RsaSha256,
                private_pem,
                public_b64,
                dns_txt_record,
            })
        }
        KeySpec::Ed25519 => {
            let pair = DkimKeyPair::generate_ed25519()
                .map_err(|e| Error::KeyLoad(format!("Ed25519 keygen: {e}")))?;
            let private_pem = pem_encode("PRIVATE KEY", pair.private_key());
            let public_b64 = pair.encoded_public_key();
            let dns_txt_record = format!("v=DKIM1; k=ed25519; p={public_b64}");
            Ok(DkimKeyMaterial {
                algorithm: DkimAlgorithm::Ed25519Sha256,
                private_pem,
                public_b64,
                dns_txt_record,
            })
        }
    }
}

/// Wrap DER bytes in a 64-char-wrapped PEM envelope. `label` is the
/// `-----BEGIN <label>-----` content (e.g. `"RSA PRIVATE KEY"` for
/// PKCS1, `"PRIVATE KEY"` for PKCS8). Matches OpenSSL's output so
/// `rustls_pki_types::PrivateKeyDer::from_pem_slice` round-trips.
fn pem_encode(label: &str, der: &[u8]) -> String {
    use std::fmt::Write as _;
    let b64 = base64_encode(der);
    let mut out = String::with_capacity(b64.len() + 64);
    writeln!(out, "-----BEGIN {label}-----").unwrap();
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    writeln!(out, "-----END {label}-----").unwrap();
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let chunks = bytes.chunks_exact(3);
    let remainder = chunks.remainder();
    for c in chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(ALPHA[((n >> shift) & 0x3f) as usize] as char);
        }
    }
    match remainder.len() {
        1 => {
            let n = u32::from(remainder[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = (u32::from(remainder[0]) << 16) | (u32::from(remainder[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{MailAuthSigner, Signer};
    use crate::types::{Canon, DkimSignerConfig, Domain, HeaderSpec, default_signed_headers};

    #[test]
    fn rsa_2048_round_trips_into_signer() {
        let mat = generate(KeySpec::Rsa { bits: 2048 }).expect("keygen");
        assert!(
            mat.private_pem
                .starts_with("-----BEGIN RSA PRIVATE KEY-----")
        );
        assert!(
            mat.private_pem
                .trim_end()
                .ends_with("-----END RSA PRIVATE KEY-----")
        );
        assert!(mat.dns_txt_record.starts_with("v=DKIM1; k=rsa; p="));
        assert!(!mat.public_b64.is_empty());

        // Feed the freshly-minted key straight into the signer — proves
        // the PEM envelope matches what `PrivateKeyDer::from_pem_slice`
        // accepts and the key works end-to-end.
        let cfg = DkimSignerConfig {
            domain: Domain::new("example.org"),
            selector: "k1".into(),
            algorithm: mat.algorithm,
            private_key_pem: mat.private_pem.into_bytes(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: default_signed_headers(),
            active_for_signing: true,
            allow_body_length_tag: false,
        };
        let signer = MailAuthSigner::new(vec![cfg]);
        let mut msg = b"From: a@example.org\r\nTo: b@example.org\r\n\
            Subject: t\r\nDate: Tue, 19 May 2026 10:00:00 +1000\r\n\r\nhi\r\n"
            .to_vec();
        tokio_test_block_on(signer.sign_dkim(&mut msg, &Domain::new("example.org")))
            .expect("sign with generated rsa key");
        assert!(msg.starts_with(b"DKIM-Signature: "));
    }

    #[test]
    fn ed25519_round_trips_into_signer() {
        let mat = generate(KeySpec::Ed25519).expect("keygen");
        assert!(mat.private_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(mat.dns_txt_record.starts_with("v=DKIM1; k=ed25519; p="));

        let cfg = DkimSignerConfig {
            domain: Domain::new("example.org"),
            selector: "ed1".into(),
            algorithm: mat.algorithm,
            private_key_pem: mat.private_pem.into_bytes(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: vec![
                HeaderSpec::Oversign("From".into()),
                HeaderSpec::Single("Subject".into()),
            ],
            active_for_signing: true,
            allow_body_length_tag: false,
        };
        let signer = MailAuthSigner::new(vec![cfg]);
        let mut msg = b"From: a@example.org\r\nSubject: t\r\n\r\nhi\r\n".to_vec();
        tokio_test_block_on(signer.sign_dkim(&mut msg, &Domain::new("example.org")))
            .expect("sign with generated ed25519 key");
        assert!(msg.starts_with(b"DKIM-Signature: "));
    }

    #[test]
    fn rsa_below_floor_rejected() {
        let err = generate(KeySpec::Rsa { bits: 512 }).unwrap_err();
        match err {
            Error::KeyLoad(m) => assert!(m.contains("512"), "msg: {m}"),
            other => panic!("expected KeyLoad, got {other:?}"),
        }
    }

    #[test]
    fn pem_lines_wrap_at_64_chars() {
        let der = vec![0xAB; 200];
        let pem = pem_encode("TEST", &der);
        for line in pem.lines() {
            if line.starts_with("-----") {
                continue;
            }
            assert!(line.len() <= 64, "line too long: {line:?} ({})", line.len());
        }
    }

    #[test]
    fn base64_encode_matches_known_vector() {
        // RFC 4648 vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// Tiny synchronous executor for the few tokio-typed sign_dkim
    /// calls — keeps this module free of #[tokio::test] when only the
    /// signer is async.
    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }
}
