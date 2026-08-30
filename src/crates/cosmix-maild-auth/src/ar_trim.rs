//! A-R (`Authentication-Results`) header trimming — RFC 8601 §5.
//!
//! A peer can forge an `Authentication-Results: maild.example.com;
//! spf=pass …` header into the message they hand us. Composing our
//! own A-R header without first removing those forged lines would
//! make us a forgery oracle: receivers further down the chain see
//! both the forged line and ours and have no way to tell which is
//! authentic.
//!
//! Per RFC 8601 §5 the verifier strips every prior A-R header whose
//! authserv-id matches our `host_identity` (case-insensitive)
//! *before* parsing remaining A-R headers (for ARC chain
//! validation) or composing the new one. This module is the pure
//! byte-level implementation; it does no DNS, allocates one output
//! buffer, and is bounded by input size.
//!
//! Invariants this implementation upholds:
//!
//! * Folded continuation lines (any line whose first byte is SP or
//!   HTAB) belong to the preceding header and are stripped together
//!   with it.
//! * Comparison of authserv-id is ASCII-case-insensitive and
//!   length-bounded — we look only at the first ~256 bytes after
//!   the colon to find the id, so a pathologically long forged
//!   header cannot drive us into quadratic behavior.
//! * The header / body boundary (the first empty line) is preserved
//!   exactly. Trimming never crosses into the body.
//! * Line endings are preserved. We work from one CRLF or LF to the
//!   next without rewriting them.
//! * Non-A-R headers and any A-R headers from other authserv-ids
//!   pass through byte-identical.

const MAX_AR_SCAN: usize = 256;
const HEADER_NAME: &[u8] = b"Authentication-Results:";

/// Trim every `Authentication-Results:` header in `message` whose
/// authserv-id matches `host_identity` (case-insensitive). Returns
/// `(trimmed_message, removed_count)`.
///
/// `host_identity` should already be lowercased; this function
/// tolerates either case but does no allocation on it.
pub fn trim_forged_authentication_results(message: &[u8], host_identity: &str) -> (Vec<u8>, u32) {
    let mut out = Vec::with_capacity(message.len());
    let mut removed: u32 = 0;
    let mut i = 0;
    let n = message.len();

    while i < n {
        // End-of-headers boundary: an empty line. After we copy it,
        // the body comes through verbatim.
        if is_empty_line_at(message, i) {
            let (line_end, _) = find_line_end(message, i);
            out.extend_from_slice(&message[i..line_end]);
            // Body — copy whatever's left and stop.
            out.extend_from_slice(&message[line_end..]);
            return (out, removed);
        }

        let (line_end, _eol_len) = find_line_end(message, i);
        let line = &message[i..line_end];

        if header_name_matches(line, HEADER_NAME) {
            // Find the authserv-id within (or extending past) this
            // line, then check if it's ours.
            let id_matches = authserv_id_matches(message, i, line_end, host_identity);
            // The full header may continue across folded lines.
            let header_end = scan_folded_header_end(message, line_end, n);
            if id_matches {
                removed = removed.saturating_add(1);
                i = header_end;
                continue;
            }
            // Not ours — pass through, including any folded
            // continuation lines.
            out.extend_from_slice(&message[i..header_end]);
            i = header_end;
            continue;
        }

        out.extend_from_slice(&message[i..line_end]);
        i = line_end;
    }

    (out, removed)
}

/// Match the leading header name case-insensitively. `name` must
/// already include the trailing colon.
fn header_name_matches(line: &[u8], name: &[u8]) -> bool {
    if line.len() < name.len() {
        return false;
    }
    line[..name.len()].eq_ignore_ascii_case(name)
}

/// Return whether the authserv-id starting after `Authentication-Results:`
/// matches `host_identity` (case-insensitive). Only the first
/// `MAX_AR_SCAN` bytes after the colon are inspected.
fn authserv_id_matches(
    message: &[u8],
    line_start: usize,
    first_line_end: usize,
    host_identity: &str,
) -> bool {
    // Slice from after the colon up to MAX_AR_SCAN bytes, but never
    // past the message end. The id might run onto a folded
    // continuation line; we look beyond the first newline up to the
    // scan budget.
    let after_colon = line_start + HEADER_NAME.len();
    let scan_end = (after_colon + MAX_AR_SCAN).min(message.len());
    // We only need bytes; folding whitespace (CRLF + WSP) is
    // harmless to skip past since we trim it before extracting.
    let _ = first_line_end;
    let scan = &message[after_colon..scan_end];

    // Skip leading FWS (space, tab, CR, LF).
    let mut start = 0;
    while start < scan.len()
        && (scan[start] == b' '
            || scan[start] == b'\t'
            || scan[start] == b'\r'
            || scan[start] == b'\n')
    {
        start += 1;
    }
    // Read id bytes — terminated by SP / HTAB / ';' / CR / LF.
    let mut end = start;
    while end < scan.len() {
        let c = scan[end];
        if c == b' ' || c == b'\t' || c == b';' || c == b'\r' || c == b'\n' {
            break;
        }
        end += 1;
    }
    let id = &scan[start..end];
    id.eq_ignore_ascii_case(host_identity.as_bytes())
}

fn is_empty_line_at(message: &[u8], i: usize) -> bool {
    if i >= message.len() {
        return false;
    }
    if message[i] == b'\n' {
        return true;
    }
    if message[i] == b'\r' && i + 1 < message.len() && message[i + 1] == b'\n' {
        return true;
    }
    false
}

/// From `i`, return the offset just past the next CRLF or LF. The
/// second tuple element is the length of the line terminator (1 for
/// LF, 2 for CRLF, 0 if EOF without terminator).
fn find_line_end(message: &[u8], i: usize) -> (usize, usize) {
    let mut j = i;
    while j < message.len() {
        if message[j] == b'\n' {
            return (j + 1, 1);
        }
        if message[j] == b'\r' && j + 1 < message.len() && message[j + 1] == b'\n' {
            return (j + 2, 2);
        }
        j += 1;
    }
    (message.len(), 0)
}

/// Walk past any folded continuation lines (lines whose first byte
/// is SP or HTAB). Returns the offset of the first non-folded byte
/// at or after `start`.
fn scan_folded_header_end(message: &[u8], start: usize, n: usize) -> usize {
    let mut i = start;
    while i < n {
        let c = message[i];
        if c == b' ' || c == b'\t' {
            let (next, _) = find_line_end(message, i);
            i = next;
            continue;
        }
        break;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8]) -> String {
        String::from_utf8(b.to_vec()).unwrap()
    }

    #[test]
    fn passes_through_when_no_forged_ar() {
        let msg = b"From: a@example.org\r\nTo: b@example.com\r\n\r\nbody\r\n";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(out, msg);
        assert_eq!(n, 0);
    }

    #[test]
    fn strips_single_forged_ar_with_our_authserv_id() {
        let msg = b"Authentication-Results: maild.example.com; spf=pass\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
        assert_eq!(s(&out), "From: a@example.org\r\n\r\nbody");
    }

    #[test]
    fn case_insensitive_match_on_authserv_id() {
        let msg = b"Authentication-Results: MAILD.EXAMPLE.COM; spf=pass\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (_, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
    }

    #[test]
    fn case_insensitive_match_on_header_name() {
        let msg = b"AUTHENTICATION-RESULTS: maild.example.com; spf=pass\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (_, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
    }

    #[test]
    fn keeps_other_authserv_ids() {
        let msg = b"Authentication-Results: gmail.com; spf=pass smtp.mailfrom=alice@x.org\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 0);
        assert_eq!(out, msg);
    }

    #[test]
    fn strips_multiple_forged_ars() {
        let msg = b"Authentication-Results: maild.example.com; spf=pass\r\n\
                   Received: from elsewhere\r\n\
                   Authentication-Results: maild.example.com; dkim=pass\r\n\
                   Authentication-Results: gmail.com; spf=pass\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 2);
        let expected = "Received: from elsewhere\r\n\
                        Authentication-Results: gmail.com; spf=pass\r\n\
                        From: a@example.org\r\n\r\nbody";
        assert_eq!(s(&out), expected);
    }

    #[test]
    fn strips_folded_continuation_with_forged_ar() {
        let msg = b"Authentication-Results: maild.example.com;\r\n\
                   \tspf=pass smtp.mailfrom=alice@example.org;\r\n\
                   \tdkim=pass header.d=example.org\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
        assert_eq!(s(&out), "From: a@example.org\r\n\r\nbody");
    }

    #[test]
    fn keeps_folded_other_authserv_id() {
        let msg = b"Authentication-Results: gmail.com;\r\n\
                   \tspf=pass smtp.mailfrom=a@x.org;\r\n\
                   \tdkim=pass\r\n\
                   From: a@example.org\r\n\r\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 0);
        assert_eq!(out, msg);
    }

    #[test]
    fn body_is_never_modified_even_if_it_looks_like_a_header() {
        let msg = b"From: a@x.org\r\n\r\n\
                    Authentication-Results: maild.example.com; spf=pass\r\n\
                    body continues";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 0);
        assert_eq!(out, msg);
    }

    #[test]
    fn lf_only_line_endings_supported() {
        let msg = b"Authentication-Results: maild.example.com; spf=pass\n\
                   From: a@x.org\n\nbody";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
        assert_eq!(s(&out), "From: a@x.org\n\nbody");
    }

    #[test]
    fn pathologically_long_forged_id_does_not_match_ours() {
        // 4 KiB of garbage in the authserv-id slot. Our scan budget
        // is 256 bytes, so we must NOT match — the id is not ours.
        let mut msg = b"Authentication-Results: ".to_vec();
        msg.extend(std::iter::repeat_n(b'x', 4096));
        msg.extend_from_slice(b"; spf=pass\r\nFrom: a@x.org\r\n\r\nbody");
        let (_, n) = trim_forged_authentication_results(&msg, "maild.example.com");
        assert_eq!(n, 0);
    }

    #[test]
    fn empty_message_safe() {
        let (out, n) = trim_forged_authentication_results(b"", "maild.example.com");
        assert_eq!(out, b"");
        assert_eq!(n, 0);
    }

    #[test]
    fn header_only_no_body() {
        let msg = b"Authentication-Results: maild.example.com; spf=pass\r\nFrom: a@x.org\r\n";
        let (out, n) = trim_forged_authentication_results(msg, "maild.example.com");
        assert_eq!(n, 1);
        assert_eq!(s(&out), "From: a@x.org\r\n");
    }
}
