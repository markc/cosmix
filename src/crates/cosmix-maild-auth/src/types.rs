//! Public types — frozen R4 surface per spec §Public API.
//!
//! `Domain` is a borrowed-from-future `cosmix-lib-mail-types` shape;
//! until that crate exists we define it here and re-export. Consumers
//! should always import from `cosmix_maild_auth::types`.

use std::net::IpAddr;
use std::time::Duration;

// ---- Domain newtype (placeholder until cosmix-lib-mail-types lands) ----

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Domain(pub String);

impl Domain {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_ascii_lowercase())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---- Inbound verification result ----

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub spf: SpfCheck,
    pub iprev: IprevResult,
    pub dkim: DkimAggregate,
    pub dmarc: DmarcResult,
    pub arc: ArcResult,
    /// Wire-format header to prepend to the delivered message.
    /// Already trimmed of forged peer claims.
    pub authentication_results_header: AuthResultsHeader,
}

#[derive(Debug, Clone)]
pub struct AuthResultsHeader {
    pub host_identity: String,
    pub rendered: String,
    pub spf: Option<SpfFields>,
    pub iprev: Option<IprevFields>,
    pub dkim: Vec<DkimFields>,
    pub dmarc: Option<DmarcFields>,
    pub arc: Option<ArcFields>,
}

#[derive(Debug, Clone)]
pub struct SpfFields {
    pub result: SpfResult,
    pub identity: String, // smtp.mailfrom=... or smtp.helo=...
    pub identity_kind: SpfIdentityKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpfIdentityKind {
    MailFrom,
    Helo,
}

#[derive(Debug, Clone)]
pub struct IprevFields {
    pub result: IprevOutcome,
    pub policy_iprev: Option<IpAddr>,
}

#[derive(Debug, Clone)]
pub struct DkimFields {
    pub result: DkimSignatureOutcome,
    pub d: String,
    pub s: String,
}

#[derive(Debug, Clone)]
pub struct DmarcFields {
    pub result: DmarcOutcome,
    pub header_from: String,
}

#[derive(Debug, Clone)]
pub struct ArcFields {
    pub result: ArcChainValidation,
    pub smtp_client_ip: Option<IpAddr>,
}

// ---- SPF ----

#[derive(Debug, Clone)]
pub enum SpfCheck {
    MailFrom { result: SpfResult, domain: String },
    Helo { result: SpfResult, domain: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpfResult {
    Pass,
    Fail,
    SoftFail,
    Neutral,
    None,
    TempError,
    PermError,
}

// ---- DKIM ----

#[derive(Debug, Clone)]
pub struct DkimAggregate {
    pub signatures: Vec<DkimSignatureResult>,
    pub overall: DkimOutcome,
    /// True when the inbound message attached more signatures than
    /// `VerifierConfig::max_dkim_signatures`; excess sigs are
    /// dropped (treated as if absent).
    pub capped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimOutcome {
    Pass,
    Fail,
    None,
    TempError,
    PermError,
}

#[derive(Debug, Clone)]
pub struct DkimSignatureResult {
    pub d: String, // signing domain
    pub s: String, // selector
    pub result: DkimSignatureOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimSignatureOutcome {
    Pass,
    Fail,
    Neutral,
    TempError,
    PermError,
}

// ---- DMARC ----

#[derive(Debug, Clone)]
pub struct DmarcResult {
    pub outcome: DmarcOutcome,
    pub report_record: DmarcReportRecord,
}

#[derive(Debug, Clone)]
pub enum DmarcOutcome {
    Pass,
    Fail {
        policy: DmarcPolicy,
        alignment: Alignment,
    },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarcPolicy {
    None,
    Quarantine,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Strict,
    Relaxed,
    NotAligned,
}

#[derive(Debug, Clone)]
pub struct DmarcReportRecord {
    pub org_domain: String,
    pub source_ip: IpAddr,
    pub policy_published: DmarcPolicy,
    pub policy_evaluated: DmarcPolicy,
    pub spf_aligned: bool,
    pub dkim_aligned: bool,
    pub disposition: DmarcDisposition,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarcDisposition {
    None,
    Quarantine,
    Reject,
}

// ---- iprev ----

#[derive(Debug, Clone)]
pub struct IprevResult {
    pub result: IprevOutcome,
    pub ptr: Option<String>,
    pub matched_forward: Option<IpAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IprevOutcome {
    Pass,
    Fail,
    TempError,
    PermError,
}

// ---- ARC ----

#[derive(Debug, Clone)]
pub struct ArcResult {
    pub chain_validation: ArcChainValidation,
    pub instance_count: u32,
    pub oldest_pass_chain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcChainValidation {
    None,
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOutcome {
    SealedPass,
    SealedFail,
}

// ---- Verifier configuration ----

#[derive(Debug, Clone)]
pub struct VerifierConfig {
    pub host_identity: String,
    pub spf_timeout: Duration,
    pub dkim_timeout: Duration,
    pub dmarc_timeout: Duration,
    pub arc_timeout: Duration,
    pub iprev_timeout: Duration,
    pub spf_mode: PolicyMode,
    pub dkim_mode: PolicyMode,
    pub dmarc_mode: PolicyMode,
    pub arc_mode: PolicyMode,
    pub iprev_mode: PolicyMode,
    pub max_dkim_signatures: u32,
    pub max_arc_instances: u32,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            host_identity: "localhost".into(),
            spf_timeout: Duration::from_secs(5),
            dkim_timeout: Duration::from_secs(10),
            dmarc_timeout: Duration::from_secs(5),
            arc_timeout: Duration::from_secs(5),
            iprev_timeout: Duration::from_secs(5),
            spf_mode: PolicyMode::Advisory,
            dkim_mode: PolicyMode::Advisory,
            dmarc_mode: PolicyMode::Advisory,
            arc_mode: PolicyMode::Advisory,
            iprev_mode: PolicyMode::Advisory,
            max_dkim_signatures: 10,
            max_arc_instances: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    Off,
    Advisory,
    Enforce,
}

// ---- Signer configuration ----

#[derive(Clone)]
pub struct DkimSignerConfig {
    pub domain: Domain,
    pub selector: String,
    pub algorithm: DkimAlgorithm,
    pub private_key_pem: Vec<u8>,
    pub canonicalization: (Canon, Canon),
    pub headers_to_sign: Vec<HeaderSpec>,
    pub active_for_signing: bool,
    /// `l=` body-length tag. False by default — enabling allows a
    /// downstream relay to append content without breaking DKIM, a
    /// known weakening. Enable only when interop demands it.
    pub allow_body_length_tag: bool,
}

// Manual `Debug` that redacts `private_key_pem` — derive would print
// the raw key bytes in any `{:?}` log line, error report, or panic
// message that touches this struct.
impl std::fmt::Debug for DkimSignerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkimSignerConfig")
            .field("domain", &self.domain)
            .field("selector", &self.selector)
            .field("algorithm", &self.algorithm)
            .field(
                "private_key_pem",
                &format_args!("<{} bytes redacted>", self.private_key_pem.len()),
            )
            .field("canonicalization", &self.canonicalization)
            .field("headers_to_sign", &self.headers_to_sign)
            .field("active_for_signing", &self.active_for_signing)
            .field("allow_body_length_tag", &self.allow_body_length_tag)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderSpec {
    Single(String),
    /// Oversigning — names the header twice in `h=`. The second slot
    /// is empty, so any later prepended copy of that header
    /// invalidates the signature. Defends against `From:`-injection.
    Oversign(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Canon {
    Simple,
    Relaxed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkimAlgorithm {
    RsaSha256,
    Ed25519Sha256,
}

/// Default header set for new `DkimSignerConfig`s — oversigns `From:`
/// and signs the conventional message-shape headers + `List-*` slots.
pub fn default_signed_headers() -> Vec<HeaderSpec> {
    vec![
        HeaderSpec::Oversign("From".into()),
        HeaderSpec::Single("To".into()),
        HeaderSpec::Single("Cc".into()),
        HeaderSpec::Single("Subject".into()),
        HeaderSpec::Single("Date".into()),
        HeaderSpec::Single("Message-ID".into()),
        HeaderSpec::Single("Reply-To".into()),
        HeaderSpec::Single("Sender".into()),
        HeaderSpec::Single("MIME-Version".into()),
        HeaderSpec::Single("Content-Type".into()),
        HeaderSpec::Single("Content-Transfer-Encoding".into()),
        HeaderSpec::Single("List-Id".into()),
        HeaderSpec::Single("List-Help".into()),
        HeaderSpec::Single("List-Unsubscribe".into()),
        HeaderSpec::Single("List-Subscribe".into()),
        HeaderSpec::Single("List-Post".into()),
        HeaderSpec::Single("List-Owner".into()),
        HeaderSpec::Single("List-Archive".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_lowercases_on_construction() {
        let d = Domain::new("Example.ORG");
        assert_eq!(d.as_str(), "example.org");
    }

    #[test]
    fn verifier_config_defaults_match_spec() {
        let c = VerifierConfig::default();
        assert_eq!(c.spf_timeout, Duration::from_secs(5));
        assert_eq!(c.dkim_timeout, Duration::from_secs(10));
        assert_eq!(c.max_dkim_signatures, 10);
        assert_eq!(c.max_arc_instances, 50);
    }

    #[test]
    fn default_signed_headers_oversign_from_only() {
        let hs = default_signed_headers();
        let oversigned: Vec<&str> = hs
            .iter()
            .filter_map(|h| match h {
                HeaderSpec::Oversign(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(oversigned, vec!["From"]);
    }
}
