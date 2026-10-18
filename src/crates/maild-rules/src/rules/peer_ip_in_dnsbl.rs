//! `peer_ip_in_dnsbl` — matches when the connection's peer IP is
//! listed in any one of the rule's configured DNSBL zones.
//!
//! The matcher is synchronous (per `_doc/planned/maild-rules-phase2.md`
//! D1); the underlying DNS I/O runs in [`crate::preflight::run`] *before*
//! the engine's matcher loop, populating a [`DnsblPreflightResult`]
//! that the sync matcher consults.
//!
//! Match semantics: a rule with N zones matches iff *any* zone returned
//! [`crate::dnsbl::DnsblResult::Listed`]. `NotListed` and `LookupFailed`
//! both fall through to no-match — the spec's "don't poison classify
//! on a flaky DNSBL" intent applies per-zone.

use crate::dnsbl::DnsblResult;
use crate::preflight::DnsblPreflightResult;

#[derive(Debug, Clone)]
pub struct PeerIpInDnsbl {
    pub zones: Vec<String>,
}

impl PeerIpInDnsbl {
    /// True iff any configured zone has a `Listed` verdict in the
    /// preflight result. `NotListed` / `LookupFailed` count as no-match.
    pub(crate) fn matches(&self, preflight: &DnsblPreflightResult) -> bool {
        self.zones
            .iter()
            .any(|z| matches!(preflight.verdict_for(z), DnsblResult::Listed))
    }
}
