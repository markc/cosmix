//! Compiled rule pack — what the engine sees after the loader has
//! parsed `default.conf.mix` and dispatched on the `match` `kind`.
//!
//! `RuleMatcher` is the dispatch enum; each variant holds the per-kind
//! data structure from `crate::rules`. The engine walks the
//! `Vec<CompiledRule>` once per classification, asks each matcher
//! whether it fires, and accumulates score from matches.

use std::net::IpAddr;

use cosmix_maild_auth::VerifyResult;

use crate::mime::MimeViews;
use crate::preflight::DnsblPreflightResult;
use crate::rules::{
    alignment::Alignment, attachment_count::AttachmentCount, body_regex::BodyRegex,
    body_substring::BodySubstring, header_absent::HeaderAbsent, header_present::HeaderPresent,
    header_regex::HeaderRegex, header_substring::HeaderSubstring, mail_auth::MailAuth,
    peer_ip_in_cidr::PeerIpInCidr, peer_ip_in_dnsbl::PeerIpInDnsbl,
    recipient_count::RecipientCount, structure::Structure, url_count::UrlCount,
};
use crate::types::RuleId;

/// One compiled rule as it appears in the loaded pack.
#[derive(Debug, Clone)]
pub(crate) struct CompiledRule {
    pub id: RuleId,
    pub description: String,
    pub weight: i32,
    pub matcher: RuleMatcher,
}

/// Inputs each matcher sees. Borrowed for the lifetime of one
/// classification call.
pub(crate) struct MatchInputs<'a> {
    pub mime: &'a MimeViews,
    pub mail_auth: &'a VerifyResult,
    pub peer_ip: IpAddr,
    pub envelope_to_count: u32,
    /// Pre-computed URL count (capped by `EngineConfig.url_extraction_cap`).
    pub url_count: u32,
    /// DNSBL verdicts resolved by `crate::preflight::run` before the
    /// matcher loop. `peer_ip_in_dnsbl` consults this; other matchers
    /// ignore it.
    pub dnsbl_preflight: &'a DnsblPreflightResult,
}

/// Dispatch enum for the rule kinds. `Alignment` is reserved
/// (always no-match in v1); `PeerIpInDnsbl` is wired via the
/// preflight result the engine populates before the matcher loop.
#[derive(Debug, Clone)]
pub(crate) enum RuleMatcher {
    HeaderPresent(HeaderPresent),
    HeaderAbsent(HeaderAbsent),
    HeaderRegex(HeaderRegex),
    HeaderSubstring(HeaderSubstring),
    BodyRegex(BodyRegex),
    BodySubstring(BodySubstring),
    MailAuth(MailAuth),
    PeerIpInCidr(PeerIpInCidr),
    UrlCount(UrlCount),
    AttachmentCount(AttachmentCount),
    RecipientCount(RecipientCount),
    Structure(Structure),
    /// DNSBL membership check — wired live by `crate::preflight::run`.
    /// Phase 2 commit 2b promoted this from a reserved no-match stub.
    PeerIpInDnsbl(PeerIpInDnsbl),
    /// Reserved — always no-match in v1. Payload is loaded from the
    /// rule pack (alignment mode) and held until v1.1 wires the matcher.
    #[allow(dead_code)]
    Alignment(Alignment),
}

impl RuleMatcher {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        match self {
            RuleMatcher::HeaderPresent(r) => r.matches(inputs),
            RuleMatcher::HeaderAbsent(r) => r.matches(inputs),
            RuleMatcher::HeaderRegex(r) => r.matches(inputs),
            RuleMatcher::HeaderSubstring(r) => r.matches(inputs),
            RuleMatcher::BodyRegex(r) => r.matches(inputs),
            RuleMatcher::BodySubstring(r) => r.matches(inputs),
            RuleMatcher::MailAuth(r) => r.matches(inputs),
            RuleMatcher::PeerIpInCidr(r) => r.matches(inputs),
            RuleMatcher::UrlCount(r) => r.matches(inputs),
            RuleMatcher::AttachmentCount(r) => r.matches(inputs),
            RuleMatcher::RecipientCount(r) => r.matches(inputs),
            RuleMatcher::Structure(r) => r.matches(inputs),
            RuleMatcher::PeerIpInDnsbl(r) => r.matches(inputs.dnsbl_preflight),
            RuleMatcher::Alignment(_) => false,
        }
    }
}

/// Compiled rule set held behind an `RwLock` for atomic reload.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuleSet {
    pub pack_version: String,
    pub rules: Vec<CompiledRule>,
}

impl RuleSet {
    pub(crate) fn empty() -> Self {
        Self {
            pack_version: "v0.0-empty".to_string(),
            rules: Vec::new(),
        }
    }
}
