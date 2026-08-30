//! Strict `Host` header parser shared by every Cosmix daemon that
//! serves HTTP and needs to dispatch by Host or to redirect to a
//! Host-derived URL. The semantics match the NS 3.0 nginx `$host`
//! rendering (without port, lowercased) and reject anything that
//! is not a bare hostname — extra colons, non-numeric port,
//! non-LDH bytes, IPv6 literals, CR/LF — to close the
//! Host-reflection / response-injection vectors that any
//! permissive parser opens.
//!
//! All failures collapse to `None`. Callers map `None` to the
//! response they need (`400` for the plain-HTTP path,
//! `404` for the Host-routed admission path, etc.) — the parser
//! itself does not encode the response policy because different
//! callers want different shapes.
//!
//! Lifted from `cosmix-webd/src/main.rs`'s inline parser inside
//! `redirect_to_https` (the strictest of the two pre-existing webd
//! parsers) in webd-vhosts Phase 1 commit 1 so webd's two parsers
//! and the new `host_router` middleware route Hosts by exactly the
//! same byte-level rules.

/// Parse the bare hostname out of an HTTP `Host` header value.
///
/// Returns the lowercased hostname on success, `None` on any
/// failure. The hostname is at most 253 bytes and contains only
/// `[A-Za-z0-9.-]` per LDH (RFC 1123 §2.1). A trailing
/// `:<numeric-port>` is accepted and stripped; everything else
/// rejects.
pub fn parse_request_host(raw: &str) -> Option<String> {
    let mut parts = raw.split(':');
    let host = parts.next().unwrap_or("");
    let port_ok = match parts.next() {
        None => true,
        Some(p) => {
            parts.next().is_none()
                && !p.is_empty()
                && p.len() <= 5
                && p.bytes().all(|b| b.is_ascii_digit())
        }
    };
    let host_ok = port_ok
        && !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    if !host_ok {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rejected() {
        assert_eq!(parse_request_host(""), None);
    }

    #[test]
    fn bare_hostname_accepted_lowercased() {
        assert_eq!(
            parse_request_host("Example.Com"),
            Some("example.com".into())
        );
    }

    #[test]
    fn host_with_numeric_port_accepted_port_stripped() {
        assert_eq!(
            parse_request_host("host.example:8443"),
            Some("host.example".into())
        );
    }

    #[test]
    fn port_leading_zero_accepted() {
        // Numeric-only rule — leading zeros are still digits.
        assert_eq!(
            parse_request_host("host.example:0443"),
            Some("host.example".into())
        );
    }

    #[test]
    fn port_letters_rejected() {
        assert_eq!(parse_request_host("host.example:80a"), None);
    }

    #[test]
    fn port_too_long_rejected() {
        // `>5` chars rejects 6-digit ports.
        assert_eq!(parse_request_host("host.example:123456"), None);
    }

    #[test]
    fn double_colon_rejected() {
        assert_eq!(parse_request_host("host.example:80:extra"), None);
    }

    #[test]
    fn ipv6_literal_rejected() {
        // `[::1]:443` and bare `::1` both contain multiple `:`s.
        assert_eq!(parse_request_host("[::1]:443"), None);
        assert_eq!(parse_request_host("::1"), None);
    }

    #[test]
    fn crlf_injection_rejected() {
        assert_eq!(parse_request_host("evil.example\r\nLocation: x"), None);
        assert_eq!(parse_request_host("evil.example\n"), None);
    }

    #[test]
    fn trailing_dot_accepted() {
        // FQDN trailing-dot form is LDH-valid; Host headers commonly
        // carry it from strict clients.
        assert_eq!(
            parse_request_host("host.example."),
            Some("host.example.".into())
        );
    }

    #[test]
    fn underscore_rejected_non_ldh() {
        assert_eq!(parse_request_host("host_name.example"), None);
    }

    #[test]
    fn length_253_accepted() {
        let host = "a".repeat(253);
        assert_eq!(parse_request_host(&host), Some(host));
    }

    #[test]
    fn length_254_rejected() {
        let host = "a".repeat(254);
        assert_eq!(parse_request_host(&host), None);
    }
}
