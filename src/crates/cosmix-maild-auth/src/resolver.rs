//! DNS resolver coordination.
//!
//! Per spec §DNS resolver the default is `hickory-resolver` configured
//! via `read_system_conf()`, *not* libc `getaddrinfo` — a synchronous
//! resolver would bypass `mail-auth`'s timeout discipline.
//!
//! `DnsResolver` wraps `mail_auth::MessageAuthenticator`, which in turn
//! holds a `hickory_resolver::TokioResolver`. Construction is fallible
//! (system-conf read may fail, custom IPs may be empty) so callers
//! handle a single `Result` at startup rather than at every verify.

use crate::error::{Error, Result};
use mail_auth::MessageAuthenticator;
use mail_auth::hickory_resolver::config::{
    NameServerConfig, NameServerConfigGroup, ProtocolConfig, ResolverConfig, ResolverOpts,
};
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Default)]
pub enum ResolverChoice {
    /// `hickory-resolver` configured from `/etc/resolv.conf` (or the
    /// platform equivalent on Windows). Default.
    #[default]
    System,
    /// Cloudflare 1.1.1.1 / 1.0.0.1 over UDP with TCP fallback.
    Cloudflare,
    /// Explicit upstream resolvers.
    Custom(Vec<IpAddr>),
}

/// Parse the `dns_resolver` config string per spec.
///
/// `"system"` → `System`, `"cloudflare"` → `Cloudflare`,
/// `"custom:1.1.1.1,1.0.0.1"` → `Custom([1.1.1.1, 1.0.0.1])`.
pub fn parse_choice(s: &str) -> Result<ResolverChoice> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("system") {
        return Ok(ResolverChoice::System);
    }
    if s.eq_ignore_ascii_case("cloudflare") {
        return Ok(ResolverChoice::Cloudflare);
    }
    if let Some(rest) = s.strip_prefix("custom:") {
        let mut ips = Vec::new();
        for part in rest.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            ips.push(part.parse::<IpAddr>().map_err(|e| {
                Error::Internal(format!("dns_resolver custom: bad IP {part:?}: {e}"))
            })?);
        }
        if ips.is_empty() {
            return Err(Error::Internal(
                "dns_resolver custom: at least one IP required".into(),
            ));
        }
        return Ok(ResolverChoice::Custom(ips));
    }
    Err(Error::Internal(format!("unknown dns_resolver {s:?}")))
}

/// Owns the `MessageAuthenticator` (and thus the underlying
/// hickory `TokioResolver`). Construction reads the system resolv.conf
/// or builds a `ResolverConfig` from the supplied IPs.
pub struct DnsResolver {
    pub choice: ResolverChoice,
    pub(crate) inner: MessageAuthenticator,
}

impl DnsResolver {
    pub fn new(choice: ResolverChoice) -> Result<Self> {
        let inner = match &choice {
            ResolverChoice::System => MessageAuthenticator::new_system_conf()
                .map_err(|e| Error::Internal(format!("system resolv.conf: {e}")))?,
            ResolverChoice::Cloudflare => MessageAuthenticator::new_cloudflare()
                .map_err(|e| Error::Internal(format!("cloudflare resolver: {e}")))?,
            ResolverChoice::Custom(ips) => {
                let mut servers: Vec<NameServerConfig> = Vec::with_capacity(ips.len() * 2);
                for ip in ips {
                    let addr = SocketAddr::new(*ip, 53);
                    // UDP first; TCP fallback for large responses (DKIM
                    // keys can exceed UDP MTU; DMARC tree-walks
                    // multiply lookups but stay small).
                    servers.push(NameServerConfig::new(addr, ProtocolConfig::Udp));
                    servers.push(NameServerConfig::new(addr, ProtocolConfig::Tcp));
                }
                let group = NameServerConfigGroup::from(servers);
                let config = ResolverConfig::from_parts(None, vec![], group);
                MessageAuthenticator::new(config, ResolverOpts::default())
                    .map_err(|e| Error::Internal(format!("custom resolver: {e}")))?
            }
        };
        Ok(Self { choice, inner })
    }

    pub(crate) fn auth(&self) -> &MessageAuthenticator {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_system_default() {
        assert!(matches!(
            parse_choice("system").unwrap(),
            ResolverChoice::System
        ));
        assert!(matches!(
            parse_choice("System").unwrap(),
            ResolverChoice::System
        ));
    }

    #[test]
    fn parse_cloudflare() {
        assert!(matches!(
            parse_choice("cloudflare").unwrap(),
            ResolverChoice::Cloudflare
        ));
    }

    #[test]
    fn parse_custom_multi() {
        match parse_choice("custom:1.1.1.1,8.8.8.8").unwrap() {
            ResolverChoice::Custom(ips) => {
                assert_eq!(ips.len(), 2);
                assert_eq!(ips[0], "1.1.1.1".parse::<IpAddr>().unwrap());
                assert_eq!(ips[1], "8.8.8.8".parse::<IpAddr>().unwrap());
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn parse_custom_empty_is_error() {
        assert!(parse_choice("custom:").is_err());
    }

    #[test]
    fn parse_unknown_kind_is_error() {
        assert!(parse_choice("librarian").is_err());
    }

    #[test]
    fn parse_custom_bad_ip_is_error() {
        assert!(parse_choice("custom:not-an-ip").is_err());
    }

    #[test]
    fn build_cloudflare_resolver() {
        // Construction must not perform any network I/O.
        let r = DnsResolver::new(ResolverChoice::Cloudflare).expect("cloudflare ok");
        assert!(matches!(r.choice, ResolverChoice::Cloudflare));
    }

    #[test]
    fn build_custom_resolver() {
        let r = DnsResolver::new(ResolverChoice::Custom(vec![
            "1.1.1.1".parse().unwrap(),
            "8.8.8.8".parse().unwrap(),
        ]))
        .expect("custom ok");
        assert!(matches!(r.choice, ResolverChoice::Custom(_)));
    }
}
