//! Outbound DKIM signing + ARC sealing surface.
//!
//! `sign_dkim` is fully implemented (design doc Phase 3): it looks up
//! the active selector for the From-header domain (walking parent
//! labels), loads the PEM key (RSA PKCS1/PKCS8 or Ed25519 PKCS8), runs
//! `mail_auth::dkim::DkimSigner`, and prepends the produced
//! `DKIM-Signature:` header to the message buffer.
//!
//! `seal_arc` is still stubbed — ARC sealing is design-doc Phase 4 and
//! lands in a follow-up commit.

use crate::error::{Error, Result};
use crate::types::*;
use mail_auth::common::crypto::{Ed25519Key, RsaKey, Sha256};
use mail_auth::dkim::{Canonicalization, DkimSigner};
use rustls_pki_types::{PrivateKeyDer, pem::PemObject};
use std::collections::HashMap;

#[allow(async_fn_in_trait)]
pub trait Signer: Send + Sync {
    /// DKIM-sign a message on the submission path. `domain` is the
    /// `From:` header domain (matched, not envelope-from); the
    /// signer walks parent labels until a configured selector
    /// matches, or returns `Error::NoSignerForDomain`.
    ///
    /// On success the message buffer is rewritten with the
    /// `DKIM-Signature:` header prepended ahead of the original
    /// content. RFC 6376 §3.5 requires the signature header to appear
    /// above the headers it covers; prepending is the simplest way to
    /// guarantee that without touching the rest of the header block.
    async fn sign_dkim(&self, message: &mut Vec<u8>, domain: &Domain) -> Result<()>;

    /// ARC-seal a forwarded message. See spec §ARC sealing semantics
    /// for the prior-chain → cv= → outcome table.
    async fn seal_arc(
        &self,
        message: &mut Vec<u8>,
        prior_results: &AuthResultsHeader,
        domain: &Domain,
    ) -> Result<SealOutcome>;
}

#[derive(Debug)]
pub struct MailAuthSigner {
    /// One Vec per domain — supports zero-downtime key rotation by
    /// holding two (or more) selectors live simultaneously. Exactly
    /// one entry per domain has `active_for_signing = true`; the
    /// others stay loaded so verification of inbound mail signed by
    /// the previous key continues to succeed.
    by_domain: HashMap<Domain, Vec<DkimSignerConfig>>,
}

impl MailAuthSigner {
    pub fn new(configs: Vec<DkimSignerConfig>) -> Self {
        let mut by_domain: HashMap<Domain, Vec<DkimSignerConfig>> = HashMap::new();
        for c in configs {
            by_domain.entry(c.domain.clone()).or_default().push(c);
        }
        Self { by_domain }
    }

    /// Construct the actual signing key from `pem`, discarding the
    /// result. Used by the maild config adapter at startup so a bad
    /// PEM fails fast rather than the first time someone in that
    /// domain sends mail (the real per-call parse lives in
    /// `sign_with_config`). PEM-envelope decode alone is insufficient
    /// — it accepts any PKCS-shaped blob, including a non-RSA key
    /// labelled RSA, so the construction step is what actually
    /// validates the algorithm match.
    pub fn validate_pem(algorithm: DkimAlgorithm, pem: &[u8]) -> Result<()> {
        match algorithm {
            DkimAlgorithm::RsaSha256 => {
                let der = PrivateKeyDer::from_pem_slice(pem)
                    .map_err(|e| Error::KeyLoad(format!("RSA PEM parse: {e}")))?;
                RsaKey::<Sha256>::from_key_der(der)
                    .map(|_| ())
                    .map_err(|e| Error::KeyLoad(format!("RSA key: {e}")))
            }
            DkimAlgorithm::Ed25519Sha256 => {
                let der = PrivateKeyDer::from_pem_slice(pem)
                    .map_err(|e| Error::KeyLoad(format!("Ed25519 PEM parse: {e}")))?;
                let pkcs8 = match der {
                    PrivateKeyDer::Pkcs8(d) => d,
                    _ => {
                        return Err(Error::KeyLoad(
                            "Ed25519 keys must be PKCS8 PEM (-----BEGIN PRIVATE KEY-----)".into(),
                        ));
                    }
                };
                Ed25519Key::from_pkcs8_maybe_unchecked_der(pkcs8.secret_pkcs8_der())
                    .map(|_| ())
                    .map_err(|e| Error::KeyLoad(format!("Ed25519 key: {e}")))
            }
        }
    }

    /// Return the active-for-signing selector for `domain`, walking
    /// parent labels if no exact match is configured. Returns
    /// `Error::NoSignerForDomain` if no ancestor has an active
    /// signer. Pure lookup; no I/O.
    pub fn lookup_active(&self, domain: &Domain) -> Result<&DkimSignerConfig> {
        for candidate in walk_up(domain.as_str()) {
            let key = Domain(candidate.to_ascii_lowercase());
            if let Some(configs) = self.by_domain.get(&key)
                && let Some(active) = configs.iter().find(|c| c.active_for_signing)
            {
                return Ok(active);
            }
        }
        Err(Error::NoSignerForDomain(domain.as_str().to_string()))
    }
}

impl Signer for MailAuthSigner {
    async fn sign_dkim(&self, message: &mut Vec<u8>, domain: &Domain) -> Result<()> {
        let cfg = self.lookup_active(domain)?;
        let header_bytes = sign_with_config(cfg, message)?;
        // Prepend the new DKIM-Signature header. `signature.write(_,
        // true)` already terminates with CRLF so the rest of the
        // header block continues unmodified.
        let mut out = Vec::with_capacity(header_bytes.len() + message.len());
        out.extend_from_slice(&header_bytes);
        out.append(message);
        *message = out;
        Ok(())
    }

    async fn seal_arc(
        &self,
        _message: &mut Vec<u8>,
        _prior_results: &AuthResultsHeader,
        _domain: &Domain,
    ) -> Result<SealOutcome> {
        Err(Error::Internal(
            "seal_arc: ARC sealing is design-doc Phase 4".into(),
        ))
    }
}

/// Yield `domain`, then each parent by stripping leading labels.
/// `walk_up("a.b.example.org")` → `["a.b.example.org", "b.example.org",
/// "example.org", "org"]`. The TLD is included; `lookup_active` will
/// not generally find a config there, but the walker stays simple.
fn walk_up(domain: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut cur = domain;
    loop {
        out.push(cur);
        match cur.find('.') {
            Some(i) => cur = &cur[i + 1..],
            None => break,
        }
    }
    out
}

/// Infer the DKIM algorithm from a PEM-encoded private key.
///
/// Used by `dkim_rebuild` (the maild substrate-driven signer
/// reconstruction path) so the verb implementations don't have to
/// guess: the PEM on disk is the source of truth for which algorithm
/// to record in the SetupRow. PKCS1 (`-----BEGIN RSA PRIVATE KEY-----`)
/// is RSA by definition; PKCS8 is shape-only, so we inspect the inner
/// `AlgorithmIdentifier` OID against the rsaEncryption and Ed25519
/// fixed sequences.
///
/// Returns `Error::KeyLoad` if the PEM doesn't parse, is SEC1 (EC keys
/// aren't valid for DKIM), or has an unrecognised PKCS8 algorithm OID.
pub fn detect_algorithm(pem: &[u8]) -> Result<DkimAlgorithm> {
    let der = PrivateKeyDer::from_pem_slice(pem)
        .map_err(|e| Error::KeyLoad(format!("PEM parse: {e}")))?;
    match der {
        PrivateKeyDer::Pkcs1(_) => Ok(DkimAlgorithm::RsaSha256),
        PrivateKeyDer::Pkcs8(d) => extract_pkcs8_algorithm_oid(d.secret_pkcs8_der()),
        PrivateKeyDer::Sec1(_) => Err(Error::KeyLoad(
            "SEC1 EC keys are not supported for DKIM (need RSA or Ed25519)".into(),
        )),
        _ => Err(Error::KeyLoad(
            "unrecognised PEM format (expected RSA PRIVATE KEY or PRIVATE KEY)".into(),
        )),
    }
}

/// Parse the AlgorithmIdentifier OID out of a PKCS8 PrivateKeyInfo and
/// map it to a `DkimAlgorithm`. Walks the DER TLV structure exactly
/// rather than substring-matching — substring search would mis-classify
/// any key whose private-data bytes happen to contain the RSA or
/// Ed25519 OID byte sequence (improbable but not impossible for
/// Ed25519's 32-byte seed).
///
/// PKCS8 (RFC 5208):
///   PrivateKeyInfo ::= SEQUENCE {
///       version Version,                       (INTEGER)
///       privateKeyAlgorithm AlgorithmIdentifier,
///       privateKey PrivateKey                  (OCTET STRING)
///   }
///   AlgorithmIdentifier ::= SEQUENCE {
///       algorithm OBJECT IDENTIFIER,
///       parameters ANY DEFINED BY algorithm OPTIONAL
///   }
fn extract_pkcs8_algorithm_oid(bytes: &[u8]) -> Result<DkimAlgorithm> {
    // RSA: 1.2.840.113549.1.1.1 (rsaEncryption)
    const RSA_OID: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
    // Ed25519: 1.3.101.112
    const ED25519_OID: &[u8] = &[0x2B, 0x65, 0x70];

    let outer = parse_tlv(bytes).ok_or_else(|| Error::KeyLoad("PKCS8: outer TLV".into()))?;
    if outer.tag != 0x30 {
        return Err(Error::KeyLoad("PKCS8: outer not SEQUENCE".into()));
    }
    // Inside outer: version INTEGER, then AlgorithmIdentifier SEQUENCE.
    let version = parse_tlv(outer.value).ok_or_else(|| Error::KeyLoad("PKCS8: version".into()))?;
    if version.tag != 0x02 {
        return Err(Error::KeyLoad("PKCS8: version not INTEGER".into()));
    }
    let after_version = outer
        .value
        .get(version.header_len + version.value.len()..)
        .ok_or_else(|| Error::KeyLoad("PKCS8: truncated after version".into()))?;
    let alg_ident = parse_tlv(after_version)
        .ok_or_else(|| Error::KeyLoad("PKCS8: AlgorithmIdentifier".into()))?;
    if alg_ident.tag != 0x30 {
        return Err(Error::KeyLoad(
            "PKCS8: AlgorithmIdentifier not SEQUENCE".into(),
        ));
    }
    let oid_tlv = parse_tlv(alg_ident.value).ok_or_else(|| Error::KeyLoad("PKCS8: OID".into()))?;
    if oid_tlv.tag != 0x06 {
        return Err(Error::KeyLoad("PKCS8: algorithm not OID".into()));
    }
    if oid_tlv.value == RSA_OID {
        Ok(DkimAlgorithm::RsaSha256)
    } else if oid_tlv.value == ED25519_OID {
        Ok(DkimAlgorithm::Ed25519Sha256)
    } else {
        Err(Error::KeyLoad(format!(
            "PKCS8 key: unrecognised algorithm OID ({:02x?}) — expected RSA or Ed25519",
            oid_tlv.value
        )))
    }
}

struct Tlv<'a> {
    tag: u8,
    header_len: usize,
    value: &'a [u8],
}

/// Minimal DER TLV parser — only handles short-form (≤127 bytes) and
/// long-form lengths up to 4 length-bytes. That covers every PKCS8
/// private-key wire shape we care about; reject everything else.
fn parse_tlv(input: &[u8]) -> Option<Tlv<'_>> {
    let tag = *input.first()?;
    let len_byte = *input.get(1)?;
    let (header_len, value_len) = if len_byte & 0x80 == 0 {
        (2usize, len_byte as usize)
    } else {
        let n = (len_byte & 0x7F) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut v: usize = 0;
        for &b in input.get(2..2 + n)? {
            v = (v << 8) | (b as usize);
        }
        (2 + n, v)
    };
    let value = input.get(header_len..header_len + value_len)?;
    Some(Tlv {
        tag,
        header_len,
        value,
    })
}

/// Sign `message` per `cfg`, returning the wire-format
/// `DKIM-Signature: …\r\n` header bytes ready to prepend.
///
/// Uses `DkimSigner::sign_streaming()` rather than `sign()` deliberately:
/// mail-auth 0.7.5's non-streaming `sign()` hashes the body with the
/// header canonicalization (`Canonicalization::canonicalize` at
/// canonicalize.rs:319-320 calls `self.ch.canonical_body(...)`), so a
/// mixed config like `(Simple, Relaxed)` writes `c=simple/relaxed` on the
/// wire but computes `bh=` with the Simple body canon — every verifier
/// rejects it. The streaming path (`streaming.rs:117, 184`) feeds
/// `self.template.cb` into `BodyHasher::new`, which is what RFC 6376
/// requires. Don't switch to `sign()` again until upstream is fixed.
fn sign_with_config(cfg: &DkimSignerConfig, message: &[u8]) -> Result<Vec<u8>> {
    let header_canon = map_canon(cfg.canonicalization.0);
    let body_canon = map_canon(cfg.canonicalization.1);
    let header_names: Vec<String> = cfg
        .headers_to_sign
        .iter()
        .flat_map(expand_oversign)
        .collect();

    let signature = match cfg.algorithm {
        DkimAlgorithm::RsaSha256 => {
            let der = PrivateKeyDer::from_pem_slice(&cfg.private_key_pem)
                .map_err(|e| Error::KeyLoad(format!("RSA PEM parse: {e}")))?;
            let key = RsaKey::<Sha256>::from_key_der(der)
                .map_err(|e| Error::KeyLoad(format!("RSA key: {e}")))?;
            let signer = DkimSigner::from_key(key)
                .domain(cfg.domain.as_str())
                .selector(cfg.selector.clone())
                .headers(header_names)
                .body_length(cfg.allow_body_length_tag)
                .header_canonicalization(header_canon)
                .body_canonicalization(body_canon);
            let mut stream = signer.sign_streaming();
            stream.write(message);
            stream
                .finish()
                .map_err(|e| Error::Internal(format!("dkim sign (rsa): {e}")))?
        }
        DkimAlgorithm::Ed25519Sha256 => {
            let der = PrivateKeyDer::from_pem_slice(&cfg.private_key_pem)
                .map_err(|e| Error::KeyLoad(format!("Ed25519 PEM parse: {e}")))?;
            let pkcs8 = match der {
                PrivateKeyDer::Pkcs8(d) => d,
                _ => {
                    return Err(Error::KeyLoad(
                        "Ed25519 keys must be PKCS8 PEM (-----BEGIN PRIVATE KEY-----)".into(),
                    ));
                }
            };
            let key = Ed25519Key::from_pkcs8_maybe_unchecked_der(pkcs8.secret_pkcs8_der())
                .map_err(|e| Error::KeyLoad(format!("Ed25519 key: {e}")))?;
            let signer = DkimSigner::from_key(key)
                .domain(cfg.domain.as_str())
                .selector(cfg.selector.clone())
                .headers(header_names)
                .body_length(cfg.allow_body_length_tag)
                .header_canonicalization(header_canon)
                .body_canonicalization(body_canon);
            let mut stream = signer.sign_streaming();
            stream.write(message);
            stream
                .finish()
                .map_err(|e| Error::Internal(format!("dkim sign (ed25519): {e}")))?
        }
    };

    let mut buf = Vec::with_capacity(512);
    signature.write(&mut buf, true);
    Ok(buf)
}

fn map_canon(c: Canon) -> Canonicalization {
    match c {
        Canon::Relaxed => Canonicalization::Relaxed,
        Canon::Simple => Canonicalization::Simple,
    }
}

/// Expand a `HeaderSpec` into the flat header-name list that mail-auth
/// signs. `Oversign(name)` is realised by listing the name twice in
/// `h=`; if a downstream relay prepends a fresh header of that name,
/// the signed slot count no longer matches and the signature breaks.
fn expand_oversign(spec: &HeaderSpec) -> Vec<String> {
    match spec {
        HeaderSpec::Single(n) => vec![n.clone()],
        HeaderSpec::Oversign(n) => vec![n.clone(), n.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // mail-auth's published RSA-2048 test key — fine to embed verbatim
    // (their test fixture, distributed under Apache-2.0/MIT). Keeping
    // it inline avoids a build-script roundtrip and lets the test run
    // with zero external setup.
    const RSA_TEST_PEM: &[u8] = b"\
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAv9XYXG3uK95115mB4nJ37nGeNe2CrARm1agrbcnSk5oIaEfM
ZLUR/X8gPzoiNHZcfMZEVR6bAytxUhc5EvZIZrjSuEEeny+fFd/cTvcm3cOUUbIa
UmSACj0dL2/KwW0LyUaza9z9zor7I5XdIl1M53qVd5GI62XBB76FH+Q0bWPZNkT4
NclzTLspD/MTpNCCPhySM4Kdg5CuDczTH4aNzyS0TqgXdtw6A4Sdsp97VXT9fkPW
9rso3lrkpsl/9EQ1mR/DWK6PBmRfIuSFuqnLKY6v/z2hXHxF7IoojfZLa2kZr9Ae
d4l9WheQOTA19k5r2BmlRw/W9CrgCBo0Sdj+KQIDAQABAoIBAFPChEi/OvnulReB
ECQWhOUYuNKlFKQU++2YEvZJ4+bMn5UgnE7wfJ1pj2Pr9xlfALz+OMHNrjMxGbaV
KzdrT2uCkYcf78XjnhuH9gKIiXDUv4L4N+P3u6w8yOx4bFgOS9IjS53yDOPM7SC5
g6dIg5aigHaHlffqIuFFv4yQMI/+Ai+zBKxS7wRhxK/7nnAuo28fe5MEdp57ho9/
AGlDNsdg9zCgjwhokwFE3+AaD+bkUFm4gQ1XjkUFrlmnQn8vDQ0i9toEWhCj+UPY
iOKL63MJnr90MXTXWLHoFj99wBp//mYygbF9Lj8fa28/oa8LWp3Jhb7QeMgH46iv
3aLHbTECgYEA5M2dAw+nyMw9vYlkMejhwObKYP8Mr/6zcGMLCalYvRJM5iUAM0JI
H6sM6pV9/nv167cbKocj3xYPdtE7FPOn4132MLM8Ne1f8nPE64Qrcbj5WBXvLnU8
hpWbwe2Z8h7UUMKx6q4F1/TXYkc3ScxYwfjM4mP/pLsAOgVzRSEEgrUCgYEA1qNQ
xaQHNWZ1O8WuTnqWd5JSsic6iURAmUcLeFDZY2PWhVoaQ8L/xMQhDYs1FIbLWArW
4Qq3Ibu8AbSejAKuaJz7Uf26PX+PYVUwAOO0qamCJ8d/qd6So7qWMDyAY2yXI39Y
1nMqRjr7bkEsggAZao7BKqA7ZtmogjOusBT38iUCgYEA06agJ8TDoKvOMRZ26PRU
YO0dKLzGL8eclcoI29cbj0rud7aiiMg3j5PbTuUat95TjsjDCIQaWrM9etvxm2AJ
Xfn9Uu96MyhyKQWOk46f4YMKpMElkARDCPw8KRhx39dE77AqhLyWCz8iPndCXbH6
KPTOEl4OjYOuof2Is9nnIkECgYBh948RdsnXhNlzm8nwhiGRmBbou+EK8D0v+O5y
Tyy6IcKzgSnFzgZh8EdJ4EUtBk1f9SqY8wQdgIvSl3daXorusuA/TzkngsaV3YUY
ktZOLlF7CKLrjOyPkMWmZKcROmpNyH1q/IvKHHfQnizLdXIkYd4nL5WNX0F7lE1i
j1+QhQKBgB2lviBK7rJFwlFYdQUP1NAN2dKxMZk8uJS8JglHrM0+8nRI83HbTdEQ
vB0ManEKBkbS4T5n+gRtdEqKSDmWDTXDlrBfcdCHNQLwYtBpOotCqQn/AmfjcPBl
byAbwh4+HiZ5JISoRZpiZqy67aJNVoXmdtb/E9mi7ozzytpxMNql
-----END RSA PRIVATE KEY-----
";

    const TEST_MESSAGE: &[u8] = b"\
From: alice@example.org\r\n\
To: bob@example.org\r\n\
Subject: hello\r\n\
Date: Tue, 19 May 2026 10:00:00 +1000\r\n\
\r\n\
hello world\r\n";

    fn rsa_cfg(domain: &str, selector: &str, active: bool) -> DkimSignerConfig {
        DkimSignerConfig {
            domain: Domain::new(domain),
            selector: selector.into(),
            algorithm: DkimAlgorithm::RsaSha256,
            private_key_pem: RSA_TEST_PEM.to_vec(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: vec![
                HeaderSpec::Oversign("From".into()),
                HeaderSpec::Single("To".into()),
                HeaderSpec::Single("Subject".into()),
                HeaderSpec::Single("Date".into()),
            ],
            active_for_signing: active,
            allow_body_length_tag: false,
        }
    }

    fn ed25519_cfg(domain: &str, selector: &str) -> DkimSignerConfig {
        // Generate a fresh PKCS8 DER on each test invocation; ring's
        // generate_pkcs8 returns the raw DER bytes which we wrap into
        // a PEM envelope so the same PEM-loading path is exercised.
        let pkcs8_der = Ed25519Key::generate_pkcs8().expect("generate_pkcs8");
        let pem = pkcs8_der_to_pem(&pkcs8_der);
        DkimSignerConfig {
            domain: Domain::new(domain),
            selector: selector.into(),
            algorithm: DkimAlgorithm::Ed25519Sha256,
            private_key_pem: pem.into_bytes(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: vec![
                HeaderSpec::Single("From".into()),
                HeaderSpec::Single("To".into()),
                HeaderSpec::Single("Subject".into()),
            ],
            active_for_signing: true,
            allow_body_length_tag: false,
        }
    }

    fn pkcs8_der_to_pem(der: &[u8]) -> String {
        // Manual PEM encoder — `-----BEGIN PRIVATE KEY-----` is the
        // PKCS8 label. base64 with 64-char wrap matches OpenSSL output
        // and is what `PrivateKeyDer::from_pem_slice` expects.
        use std::fmt::Write as _;
        let b64 = base64_encode(der);
        let mut out = String::from("-----BEGIN PRIVATE KEY-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            writeln!(out, "{}", std::str::from_utf8(chunk).unwrap()).unwrap();
        }
        out.push_str("-----END PRIVATE KEY-----\n");
        out
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHA: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

    #[test]
    fn walk_up_yields_self_then_ancestors() {
        let v = walk_up("a.b.example.org");
        assert_eq!(
            v,
            vec!["a.b.example.org", "b.example.org", "example.org", "org"]
        );
    }

    #[test]
    fn lookup_exact_match() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let r = s.lookup_active(&Domain::new("example.org")).unwrap();
        assert_eq!(r.selector, "2026a");
    }

    #[test]
    fn lookup_walks_up_to_parent() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let r = s
            .lookup_active(&Domain::new("mail.eng.example.org"))
            .unwrap();
        assert_eq!(r.selector, "2026a");
    }

    #[test]
    fn lookup_skips_inactive_selectors() {
        let s = MailAuthSigner::new(vec![
            rsa_cfg("example.org", "2025a", false),
            rsa_cfg("example.org", "2026a", true),
        ]);
        let r = s.lookup_active(&Domain::new("example.org")).unwrap();
        assert_eq!(r.selector, "2026a");
    }

    #[test]
    fn lookup_returns_no_signer_for_domain_when_only_inactive() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2025a", false)]);
        let err = s.lookup_active(&Domain::new("example.org")).unwrap_err();
        match err {
            Error::NoSignerForDomain(d) => assert_eq!(d, "example.org"),
            other => panic!("expected NoSignerForDomain, got {other:?}"),
        }
    }

    #[test]
    fn lookup_returns_error_when_no_ancestor_configured() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let err = s.lookup_active(&Domain::new("other.test")).unwrap_err();
        assert!(matches!(err, Error::NoSignerForDomain(_)));
    }

    #[test]
    fn expand_oversign_duplicates_name() {
        assert_eq!(
            expand_oversign(&HeaderSpec::Oversign("From".into())),
            vec!["From".to_string(), "From".to_string()]
        );
        assert_eq!(
            expand_oversign(&HeaderSpec::Single("To".into())),
            vec!["To".to_string()]
        );
    }

    #[tokio::test]
    async fn sign_dkim_rsa_prepends_well_formed_header() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let mut msg = TEST_MESSAGE.to_vec();
        s.sign_dkim(&mut msg, &Domain::new("example.org"))
            .await
            .expect("sign_dkim");
        // Header was prepended.
        assert!(
            msg.starts_with(b"DKIM-Signature: "),
            "expected DKIM-Signature prefix, got: {:?}",
            std::str::from_utf8(&msg[..msg.len().min(64)])
        );
        // Original message body preserved.
        assert!(msg.windows(TEST_MESSAGE.len()).any(|w| w == TEST_MESSAGE));
        // Signature parses back via mail-auth.
        let head_end = find_header_end(&msg).expect("header terminator");
        let header_block = &msg[..head_end];
        // a=rsa-sha256, d=example.org, s=2026a, b= present, bh= present
        assert_contains(header_block, b"a=rsa-sha256");
        assert_contains(header_block, b"d=example.org");
        assert_contains(header_block, b"s=2026a");
        assert_contains(header_block, b"; b=");
        assert_contains(header_block, b"; bh=");
        // Oversign: From appears twice in h=
        let h_tag = extract_tag(header_block, b"h=").expect("h= tag");
        let from_count = h_tag
            .split(':')
            .filter(|n| n.trim().eq_ignore_ascii_case("From"))
            .count();
        assert_eq!(
            from_count, 2,
            "oversign: From should appear twice in h=, got {from_count}: {h_tag}"
        );
    }

    #[tokio::test]
    async fn sign_dkim_ed25519_prepends_well_formed_header() {
        let s = MailAuthSigner::new(vec![ed25519_cfg("example.org", "ed-2026a")]);
        let mut msg = TEST_MESSAGE.to_vec();
        s.sign_dkim(&mut msg, &Domain::new("example.org"))
            .await
            .expect("sign_dkim");
        assert!(msg.starts_with(b"DKIM-Signature: "));
        let head_end = find_header_end(&msg).expect("header terminator");
        let header_block = &msg[..head_end];
        assert_contains(header_block, b"a=ed25519-sha256");
        assert_contains(header_block, b"d=example.org");
        assert_contains(header_block, b"s=ed-2026a");
        assert_contains(header_block, b"; b=");
    }

    #[tokio::test]
    async fn sign_dkim_walks_up_to_org_domain() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let mut msg = TEST_MESSAGE.to_vec();
        s.sign_dkim(&mut msg, &Domain::new("mail.sub.example.org"))
            .await
            .expect("sign_dkim");
        let header_block = &msg[..find_header_end(&msg).unwrap()];
        // The wire d= reflects the configured signer, not the
        // From-domain subdomain that triggered the walk-up.
        assert_contains(header_block, b"d=example.org");
    }

    #[tokio::test]
    async fn sign_dkim_returns_no_signer_when_unknown_domain() {
        let s = MailAuthSigner::new(vec![rsa_cfg("example.org", "2026a", true)]);
        let mut msg = TEST_MESSAGE.to_vec();
        let err = s
            .sign_dkim(&mut msg, &Domain::new("unrelated.test"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSignerForDomain(_)));
        // Message left untouched on failure.
        assert_eq!(msg, TEST_MESSAGE);
    }

    /// Regression guard for the mail-auth 0.7.5 non-streaming `sign()`
    /// canonicalization bug (see `sign_with_config` doc comment).
    ///
    /// Picks a body with trailing whitespace and a blank line so Simple
    /// and Relaxed body canon produce *different* canonical bytes — then
    /// signs the same message under `(Simple, Relaxed)` and
    /// `(Relaxed, Relaxed)`. Both configs ask for Relaxed body canon, so
    /// `bh=` MUST be identical. Under `sign()` the first would fall back
    /// to Simple body canon and the hashes would diverge.
    #[tokio::test]
    async fn sign_dkim_mixed_canon_uses_body_canon_for_bh() {
        const WHITESPACE_BODY: &[u8] = b"\
From: alice@example.org\r\n\
To: bob@example.org\r\n\
Subject: hello\r\n\
Date: Tue, 19 May 2026 10:00:00 +1000\r\n\
\r\n\
hello   world   \r\n\
\r\n\
trailing\r\n";

        let mut mixed = rsa_cfg("example.org", "2026a", true);
        mixed.canonicalization = (Canon::Simple, Canon::Relaxed);
        let mut matched = rsa_cfg("example.org", "2026a", true);
        matched.canonicalization = (Canon::Relaxed, Canon::Relaxed);

        let s_mixed = MailAuthSigner::new(vec![mixed]);
        let s_matched = MailAuthSigner::new(vec![matched]);

        let mut msg_a = WHITESPACE_BODY.to_vec();
        let mut msg_b = WHITESPACE_BODY.to_vec();
        s_mixed
            .sign_dkim(&mut msg_a, &Domain::new("example.org"))
            .await
            .expect("sign mixed");
        s_matched
            .sign_dkim(&mut msg_b, &Domain::new("example.org"))
            .await
            .expect("sign matched");

        let head_a = &msg_a[..find_header_end(&msg_a).unwrap()];
        let head_b = &msg_b[..find_header_end(&msg_b).unwrap()];
        let bh_a = extract_tag(head_a, b"bh=").expect("bh in mixed");
        let bh_b = extract_tag(head_b, b"bh=").expect("bh in matched");
        assert_eq!(
            bh_a.trim(),
            bh_b.trim(),
            "mixed-canon bh diverges from matched-canon bh — \
             signer regressed to non-streaming sign() and is hashing \
             body with header canon"
        );
        // And the wire `c=` tag for the mixed config reflects the
        // configured pair, not the body canon used for hashing.
        let c_a = extract_tag(head_a, b"c=").expect("c in mixed");
        assert_eq!(c_a.trim(), "simple/relaxed");
    }

    // ---- Bucket A: detect_algorithm ----

    #[test]
    fn detect_algorithm_rsa_pkcs1() {
        let alg = detect_algorithm(RSA_TEST_PEM).expect("detect");
        assert_eq!(alg, DkimAlgorithm::RsaSha256);
    }

    #[test]
    fn detect_algorithm_ed25519_pkcs8() {
        let pkcs8_der = Ed25519Key::generate_pkcs8().expect("generate_pkcs8");
        let pem = pkcs8_der_to_pem(&pkcs8_der);
        let alg = detect_algorithm(pem.as_bytes()).expect("detect");
        assert_eq!(alg, DkimAlgorithm::Ed25519Sha256);
    }

    #[test]
    fn detect_algorithm_rejects_garbage() {
        let err =
            detect_algorithm(b"-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n")
                .unwrap_err();
        assert!(matches!(err, Error::KeyLoad(_)));
    }

    #[test]
    fn detect_algorithm_rejects_non_pem() {
        let err = detect_algorithm(b"not a pem at all").unwrap_err();
        assert!(matches!(err, Error::KeyLoad(_)));
    }

    #[tokio::test]
    async fn seal_arc_is_phase_4_stub() {
        let s = MailAuthSigner::new(vec![]);
        let mut msg = TEST_MESSAGE.to_vec();
        let stub_results = AuthResultsHeader {
            host_identity: "host".into(),
            rendered: String::new(),
            spf: None,
            iprev: None,
            dkim: vec![],
            dmarc: None,
            arc: None,
        };
        let err = s
            .seal_arc(&mut msg, &stub_results, &Domain::new("example.org"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Internal(s) if s.contains("Phase 4")));
    }

    // ---- test helpers ----

    fn find_header_end(msg: &[u8]) -> Option<usize> {
        msg.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 2)
    }

    fn assert_contains(haystack: &[u8], needle: &[u8]) {
        if !haystack.windows(needle.len()).any(|w| w == needle) {
            let preview =
                std::str::from_utf8(&haystack[..haystack.len().min(400)]).unwrap_or("<binary>");
            panic!(
                "expected substring {:?} in header block; got: {preview}",
                std::str::from_utf8(needle).unwrap_or("<binary>")
            );
        }
    }

    /// Extract the value of a single DKIM tag (e.g. `b"h="`) from the
    /// `DKIM-Signature:` header bytes, returning a lossy UTF-8 view.
    /// Tag values run to the next `;` or end of header.
    fn extract_tag(header_block: &[u8], tag: &[u8]) -> Option<String> {
        // Strip soft folding (CRLF + WSP) so multi-line `h=` works.
        let mut unfolded = Vec::with_capacity(header_block.len());
        let mut i = 0;
        while i < header_block.len() {
            if i + 2 < header_block.len()
                && &header_block[i..i + 2] == b"\r\n"
                && (header_block[i + 2] == b' ' || header_block[i + 2] == b'\t')
            {
                i += 3;
                continue;
            }
            unfolded.push(header_block[i]);
            i += 1;
        }
        let pos = unfolded.windows(tag.len()).position(|w| w == tag)?;
        let start = pos + tag.len();
        let end = unfolded[start..]
            .iter()
            .position(|&b| b == b';')
            .map(|e| start + e)
            .unwrap_or(unfolded.len());
        Some(String::from_utf8_lossy(&unfolded[start..end]).into_owned())
    }
}
