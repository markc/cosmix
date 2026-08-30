//! Engine configuration. Frozen surface per spec §EngineConfig.

use std::net::SocketAddr;

use crate::types::MailAuthHardFailKind;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Continue-vs-HardJunk soft threshold. Default 5.0.
    pub threshold: f32,
    /// raw_score above this → HardJunk { ScoreBreach }. Default 15.0.
    pub hard_junk_threshold: f32,
    /// When true, HardJunk verdicts are downgraded to Continue with
    /// `would_junk = true`. Default false.
    pub shadow_mode: bool,
    /// Body bytes scanned per classification. Default 1_048_576 (1 MiB).
    pub body_scan_bytes: usize,
    /// Maximum URLs extracted per message. Default 1024.
    pub url_extraction_cap: u32,
    /// Maximum header lines considered. Default 256.
    pub header_line_cap: u32,
    /// Maximum bytes per header line. Default 8192.
    pub header_line_byte_cap: usize,
    /// Wall-clock budget per classify() call. Default 50.
    pub budget_ms: u32,
    /// Mail-auth combinations that produce HardJunk regardless of
    /// other rule contributions.
    pub mail_auth_hard_fail_kinds: Vec<MailAuthHardFailKind>,
    /// Per-DNSBL-query wall-clock budget in milliseconds. Default 2000.
    pub dnsbl_query_timeout_ms: u32,
    /// TTL (seconds) for cached negatives (NXDomain / NoError-no-A).
    /// Default 300.
    pub dnsbl_negative_ttl_secs: u32,
    /// Override resolvers as `ip:port` socket pairs. Empty = use system
    /// resolver (`/etc/resolv.conf`). Parsed at substrate write-time.
    pub dnsbl_resolver_addresses: Vec<SocketAddr>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            threshold: 5.0,
            hard_junk_threshold: 15.0,
            shadow_mode: false,
            body_scan_bytes: 1_048_576,
            url_extraction_cap: 1024,
            header_line_cap: 256,
            header_line_byte_cap: 8192,
            budget_ms: 50,
            mail_auth_hard_fail_kinds: vec![
                MailAuthHardFailKind::SpfFailDmarcReject,
                MailAuthHardFailKind::DkimFailDmarcReject,
            ],
            dnsbl_query_timeout_ms: 2000,
            dnsbl_negative_ttl_secs: 300,
            dnsbl_resolver_addresses: Vec::new(),
        }
    }
}
