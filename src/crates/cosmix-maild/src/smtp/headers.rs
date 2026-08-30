//! Header parsing helpers for the SMTP path. Currently exists solely
//! to host `from_header_domain`, used by the outbound DKIM dispatch
//! to align `d=` with the message's first `From:` address.

use cosmix_maild_auth::Domain;

/// Extract the domain from the *first* `From:` address using the
/// existing `mail_parser` dependency. Returns `None` if the message
/// has no `From:` header, the header has no addr-spec, or the
/// addr-spec has no `@`. Domain is ASCII-lowercased; non-ASCII
/// bytes pass through unchanged (DNS comparison is ASCII-case-
/// insensitive, and IDN normalisation to A-labels is the caller's
/// job — `[[dkim.domain]]` rejects non-ASCII rows up front, so a
/// U-label here will simply fail to match any configured signer).
///
/// `mail_parser::MessageParser::default()` is already used elsewhere
/// in the crate (`jmap/submission.rs`, `jmap/email.rs`) — no new
/// workspace dep. `Address::Group` (RFC 5322 §3.4) is rare in
/// outbound mail but cheap to handle: first addr-spec across all
/// groups wins.
pub fn from_header_domain(raw: &[u8]) -> Option<Domain> {
    let parsed = mail_parser::MessageParser::default().parse(raw)?;
    // `Message::from()` resolves via `Vec<Header>::header_value`,
    // which scans headers in reverse and returns the LAST `From:` —
    // an attacker who can stuff a second `From:` header into the
    // outbound message body could otherwise steer `d=` away from the
    // header a downstream verifier inspects. RFC 5322 forbids
    // multiple From slots, but the parser tolerates them; we
    // deliberately pick the FIRST wire-order one.
    let part = parsed.parts.first()?;
    let from_value = part
        .headers
        .iter()
        .find(|h| h.name == mail_parser::HeaderName::From)?
        .value
        .as_address()?;
    match from_value {
        mail_parser::Address::List(list) => list.iter().find_map(addr_domain),
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .find_map(addr_domain),
    }
}

fn addr_domain(a: &mail_parser::Addr<'_>) -> Option<Domain> {
    let addr = a.address.as_deref()?;
    let (_, dom) = addr.rsplit_once('@')?;
    let dom = dom.trim();
    if dom.is_empty() {
        return None;
    }
    Some(Domain::new(dom.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(s: &str) -> Domain {
        Domain::new(s)
    }

    #[test]
    fn returns_lowercased_domain_for_simple_from() {
        let msg = b"From: alice@Example.ORG\r\nSubject: hi\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), Some(dom("example.org")));
    }

    #[test]
    fn returns_first_when_multiple_from_slots() {
        // RFC 5322 allows multiple addresses in a single From: header;
        // we pick the first one.
        let msg = b"From: alice@first.example, bob@second.example\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), Some(dom("first.example")));
    }

    #[test]
    fn handles_display_name_with_brackets() {
        let msg = b"From: \"Alice\" <alice@example.org>\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), Some(dom("example.org")));
    }

    #[test]
    fn handles_group_address_first_member() {
        let msg = b"From: Friends:alice@example.org,bob@elsewhere.example;\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), Some(dom("example.org")));
    }

    #[test]
    fn returns_none_when_no_from_header() {
        let msg = b"To: bob@example.org\r\nSubject: hi\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), None);
    }

    #[test]
    fn returns_none_when_from_has_no_at_sign() {
        let msg = b"From: bogus\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), None);
    }

    #[test]
    fn returns_none_when_message_unparseable() {
        // Completely empty input — mail_parser returns None.
        assert_eq!(from_header_domain(b""), None);
    }

    #[test]
    fn picks_first_from_header_when_duplicated() {
        // RFC 5322 forbids multiple From: but `mail_parser` tolerates
        // them; using the LAST one (mail_parser's default
        // `header_value` semantics) would let an attacker steer `d=`
        // away from the receiver-visible From by appending a second
        // header. Wire-order first must win.
        let msg = b"From: alice@first.example\r\n\
                    From: attacker@evil.example\r\n\
                    To: bob@x.example\r\n\r\nbody\r\n";
        assert_eq!(from_header_domain(msg), Some(dom("first.example")));
    }

    #[test]
    fn idn_passthrough_ascii_only_lowercase() {
        // Non-ASCII bytes pass through unchanged; ASCII letters
        // lowercased. This is `to_ascii_lowercase` semantics, by
        // design: DNS comparison is ASCII-case-insensitive and the
        // signer adapter rejects non-ASCII rows, so a U-label here
        // is best left untouched so the lookup miss is obvious.
        let msg = "From: alice@ÉXample.org\r\n\r\nbody\r\n".as_bytes();
        let got = from_header_domain(msg).expect("some");
        assert_eq!(got.as_str(), "Éxample.org");
    }
}
