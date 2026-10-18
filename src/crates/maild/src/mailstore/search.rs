//! mail_search FTS5 projection — Phase 8e gate 2.
//!
//! The `mail_search` virtual table is a contentless FTS5 sidecar
//! (`_doc/mds/overview.md` §5) populated synchronously by every
//! `MailStore` mutation that touches an email row, inside the same
//! `SqliteCasMds::with_set_tx` that produced the `item` / `mail_envelopes`
//! / `mail_threads` rows. This module is the pure-function projection
//! layer — given an envelope + raw RFC 5322 bytes, return the four
//! indexed FTS columns. The actual `INSERT INTO mail_search` lives in
//! `mailstore/mod.rs` next to the writes it co-commits with.
//!
//! ## Column shape (v1.1 schema §5)
//!
//! ```text
//! CREATE VIRTUAL TABLE mail_search USING fts5(
//!     item_id           UNINDEXED,
//!     account_id        UNINDEXED,
//!     headers,           -- folded From/To/Cc/Bcc/Subject/Date
//!     subject,
//!     body_text,         -- inline plain-text only in v1.1
//!     normalized_addrs,
//!     content=''
//! );
//! ```
//!
//! `headers` is synthesised from the structured envelope (NOT lifted
//! from the raw bytes) so the FTS projection is stable across mail-
//! parser version skew and unaffected by header-section quoting
//! quirks. `body_text` is the concatenated top-level `text/plain`
//! parts from `mail_parser`; HTML-only and attachment text are out of
//! scope for v1.1 (`_doc/mds/overview.md` line 672 + 1326). If parsing
//! the raw bytes fails the body column is empty — the envelope-derived
//! columns still index, so a malformed body never sinks the row.
//!
//! ## DoS bounds
//!
//! `build_search_projection` is reachable through the Bus wire trust
//! boundary — a hostile peer can submit messages of arbitrary size.
//! Two caps bound the projection cost regardless of input shape:
//!
//! - `INPUT_PARSE_LIMIT` (8 MiB): raw message bytes are not parsed if
//!   they exceed this. The envelope-derived columns still index;
//!   `body_text` is left empty. Skipping is cheaper than parsing 25 MiB
//!   of MIME-encoded attachments to extract a few KiB of plain text.
//! - `OUTPUT_BODY_LIMIT` (1 MiB): post-extraction, `body_text` is
//!   truncated to this many UTF-8 bytes on a `char_indices` boundary.
//!   Real plain-text email bodies almost never exceed this; the cap
//!   exists to bound the SQLite/FTS5 write, not the typical case.
//!
//! Both caps are `pub(super) const` so tests can pin them. Headers and
//! `normalized_addrs` come from `EmailEnvelope`, which is itself bounded
//! upstream at parse time — no in-projection cap is needed for those.
//!
//! ## Out of scope (tracked follow-ups)
//!
//! - HTML→text body extraction (v1.2+).
//! - Attachment extraction (PDF/Office text) — explicitly v2+ per
//!   `_doc/mds/overview.md` §5.

use super::types::EmailEnvelope;

/// Skip parsing raw messages larger than this. The envelope-derived
/// columns still index; `body_text` is empty. See module docs.
pub(super) const INPUT_PARSE_LIMIT: usize = 8 * 1024 * 1024;

/// Truncate extracted plain-text body to this many UTF-8 bytes on a
/// char boundary. See module docs.
pub(super) const OUTPUT_BODY_LIMIT: usize = 1024 * 1024;

/// Four-column projection ready to feed `INSERT INTO mail_search`.
///
/// All fields are owned `String` because rusqlite's `params!` macro
/// needs values that outlive the bind; the projection is built once
/// per mutation and consumed immediately, so the allocation cost is
/// dominated by the underlying message bytes either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchProjection {
    pub headers: String,
    pub subject: String,
    pub body_text: String,
    pub normalized_addrs: String,
}

/// Build the FTS projection from a stored envelope + the raw message
/// bytes. Pure function — no IO, no recorder, no logger. The caller
/// owns the `INSERT INTO mail_search` write inside its `with_set_tx`.
///
/// `raw_body` may be empty (caller couldn't load the blob, or the
/// blob is legitimately zero bytes); in that case `body_text` is
/// empty and the envelope-derived columns still index. Same outcome
/// if `mail_parser` rejects the bytes.
pub(super) fn build_search_projection(
    envelope: &EmailEnvelope,
    raw_body: &[u8],
) -> SearchProjection {
    SearchProjection {
        headers: fold_headers(envelope),
        subject: envelope.subject.clone(),
        body_text: extract_plain_body(raw_body),
        normalized_addrs: normalize_addrs(envelope),
    }
}

/// Synthesise the "folded" header block (`From: …\nTo: …\n…`) from
/// the envelope. We deliberately ignore the raw header section
/// because: (a) the envelope is already the authoritative parsed
/// shape — any divergence between the two would indicate a write-path
/// bug, not search content; (b) FTS5 tokenises on word boundaries, so
/// `From:` / `To:` etc. are first-class tokens an operator can use to
/// scope a MATCH (e.g. `mail_search MATCH 'From: alice'`).
///
/// `reply_to` is folded too — IMAP `SEARCH HEADER REPLY-TO` and the
/// Bayesian retrain pipeline both treat it as a first-class header,
/// and its absence from the schema-comment's "From/To/Cc/Bcc/Subject/
/// Date" enumeration is a doc-summary artefact, not an exclusion.
fn fold_headers(env: &EmailEnvelope) -> String {
    let mut out = String::new();
    push_addr_line(&mut out, "From", std::slice::from_ref(&env.from));
    push_addr_line(&mut out, "To", &env.to);
    push_addr_line(&mut out, "Cc", &env.cc);
    push_addr_line(&mut out, "Bcc", &env.bcc);
    push_addr_line(&mut out, "Reply-To", &env.reply_to);
    if !env.subject.is_empty() {
        out.push_str("Subject: ");
        out.push_str(&env.subject);
        out.push('\n');
    }
    if env.date != 0 {
        out.push_str("Date: ");
        out.push_str(&env.date.to_string());
        out.push('\n');
    }
    out
}

fn push_addr_line(out: &mut String, name: &str, addrs: &[String]) {
    let mut first = true;
    for a in addrs.iter().filter(|a| !a.is_empty()) {
        if first {
            out.push_str(name);
            out.push_str(": ");
            first = false;
        } else {
            out.push_str(", ");
        }
        out.push_str(a);
    }
    if !first {
        out.push('\n');
    }
}

/// Lowercase, deduplicated, newline-joined addr-specs from every
/// addressing header in the envelope. The FTS tokeniser uses `@` and
/// `.` as word separators, so `'alice@example.com'` indexes as three
/// tokens (`alice`, `example`, `com`) — both `MATCH 'alice'` and
/// `MATCH 'example.com'` (which the tokeniser turns into the phrase
/// `"example com"`) resolve. Lowercasing here matches the IMAP /
/// SMTP convention that the localpart's case is preserved on the
/// wire but case-insensitive in practice; FTS5's default tokeniser
/// is already case-insensitive, but lowercasing the stored bytes
/// keeps the rebuild path bit-identical to the live writes (per the
/// `_tasks/2026-05-04-maild-phase-8e.spec.mix` acceptance bar:
/// "mail_search rebuild reproduces the projection bit-for-bit").
fn normalize_addrs(env: &EmailEnvelope) -> String {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push = |a: &str| {
        let trimmed = a.trim();
        if trimmed.is_empty() {
            return;
        }
        seen.insert(trimmed.to_ascii_lowercase());
    };
    push(&env.from);
    for a in env
        .to
        .iter()
        .chain(env.cc.iter())
        .chain(env.bcc.iter())
        .chain(env.reply_to.iter())
    {
        push(a);
    }
    let mut out = String::new();
    for (i, a) in seen.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(a);
    }
    out
}

/// Extract the concatenated top-level `text/plain` body parts from
/// `raw_body`. Returns the empty string if `mail_parser` rejects the
/// bytes, the message has no `text/plain` part, or `raw_body` exceeds
/// `INPUT_PARSE_LIMIT`. HTML-only messages produce an empty body_text
/// in v1.1 — the schema comment (`v1_1.sql:89`) restricts the body
/// column to inline plain text.
///
/// Multiple `text/plain` parts (rare in modern MUAs but legal) are
/// joined with `\n` so each one is tokenised independently. The
/// joined result is truncated to `OUTPUT_BODY_LIMIT` UTF-8 bytes on a
/// codepoint boundary (via `str::is_char_boundary`) — never
/// mid-codepoint. Combining-mark grapheme clusters may still be split
/// across the cap; the cap is a DoS bound, not a typography contract.
fn extract_plain_body(raw_body: &[u8]) -> String {
    use mail_parser::{MessageParser, PartType};

    if raw_body.is_empty() || raw_body.len() > INPUT_PARSE_LIMIT {
        return String::new();
    }
    let Some(msg) = MessageParser::default().parse(raw_body) else {
        return String::new();
    };
    let mut out = String::new();
    let mut remaining = OUTPUT_BODY_LIMIT;
    for part in msg.parts.iter() {
        if let PartType::Text(text) = &part.body {
            if remaining == 0 {
                break;
            }
            if !out.is_empty() {
                out.push('\n');
                remaining = remaining.saturating_sub(1);
            }
            // Append the part, possibly truncated on a char boundary.
            let s = text.as_ref();
            if s.len() <= remaining {
                out.push_str(s);
                remaining -= s.len();
            } else {
                let cut = utf8_floor(s, remaining);
                out.push_str(&s[..cut]);
                remaining = 0;
            }
        }
    }
    out
}

/// Largest `i <= max` such that `s.is_char_boundary(i)`. Always
/// returns a valid UTF-8 split point. `max` may be `>= s.len()`.
fn utf8_floor(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EmailEnvelope {
        EmailEnvelope {
            from: "Alice@Example.COM".into(),
            to: vec!["bob@example.com".into(), "Carol@Example.com".into()],
            cc: vec!["dave@example.com".into()],
            bcc: vec![],
            reply_to: vec!["alice-reply@example.com".into()],
            subject: "Lunch tomorrow?".into(),
            date: 1_700_000_000,
            message_id: Some("<msg-1@example.com>".into()),
        }
    }

    #[test]
    fn fold_headers_emits_from_to_cc_reply_subject_date() {
        let h = fold_headers(&env());
        assert!(h.contains("From: Alice@Example.COM\n"), "{h}");
        assert!(
            h.contains("To: bob@example.com, Carol@Example.com\n"),
            "{h}"
        );
        assert!(h.contains("Cc: dave@example.com\n"), "{h}");
        assert!(h.contains("Reply-To: alice-reply@example.com\n"), "{h}");
        assert!(h.contains("Subject: Lunch tomorrow?\n"), "{h}");
        assert!(h.contains("Date: 1700000000\n"), "{h}");
        assert!(!h.contains("Bcc:"), "Bcc should not fold when empty: {h}");
    }

    #[test]
    fn fold_headers_skips_empty_addr_strings() {
        let mut e = env();
        e.to = vec![
            "bob@example.com".into(),
            String::new(),
            "carol@example.com".into(),
        ];
        let h = fold_headers(&e);
        assert!(
            h.contains("To: bob@example.com, carol@example.com\n"),
            "{h}"
        );
    }

    #[test]
    fn fold_headers_omits_empty_subject_and_zero_date() {
        let mut e = env();
        e.subject.clear();
        e.date = 0;
        let h = fold_headers(&e);
        assert!(!h.contains("Subject:"), "empty subject must not fold: {h}");
        assert!(!h.contains("Date:"), "zero date must not fold: {h}");
    }

    #[test]
    fn normalize_addrs_lowercases_dedups_sorts() {
        let n = normalize_addrs(&env());
        // BTreeSet ordering → alphabetic; all lowercased; duplicate-
        // safe (no addr appears twice).
        let expected = "alice-reply@example.com\nalice@example.com\nbob@example.com\ncarol@example.com\ndave@example.com";
        assert_eq!(n, expected);
    }

    #[test]
    fn normalize_addrs_ignores_empty_and_whitespace() {
        let mut e = env();
        e.from = "  ".into();
        e.to = vec!["".into(), "  bob@example.com  ".into()];
        e.cc = vec![];
        e.bcc = vec![];
        e.reply_to = vec![];
        let n = normalize_addrs(&e);
        assert_eq!(n, "bob@example.com");
    }

    #[test]
    fn extract_plain_body_pulls_text_part() {
        let raw = b"From: a@x\r\nTo: b@x\r\nSubject: t\r\nContent-Type: text/plain\r\n\r\nhello world\r\n";
        let body = extract_plain_body(raw);
        assert!(body.contains("hello world"), "{body:?}");
    }

    #[test]
    fn extract_plain_body_empty_on_unparseable_bytes() {
        // 0xFF is not a valid leading byte for an RFC 5322 header,
        // and mail_parser typically rejects messages with no parseable
        // header section. The contract is "no panic, empty string" —
        // even if the parser is lenient enough to accept these bytes,
        // the absence of a `text/plain` part yields an empty body.
        let raw = b"\xff\xff\xff\xff";
        assert_eq!(extract_plain_body(raw), "");
    }

    #[test]
    fn extract_plain_body_empty_on_empty_input() {
        assert_eq!(extract_plain_body(b""), "");
    }

    #[test]
    fn extract_plain_body_empty_for_html_only_message() {
        // v1.1 schema comment: "inline plain-text only". HTML-only
        // messages produce an empty body_text — the envelope-derived
        // columns still index the message.
        let raw =
            b"From: a@x\r\nTo: b@x\r\nSubject: t\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n";
        assert_eq!(extract_plain_body(raw), "");
    }

    #[test]
    fn extract_plain_body_joins_multiple_text_parts() {
        // multipart/alternative with two text/plain parts is rare on
        // the wire but the parser will produce two PartType::Text
        // entries. Both must end up in the body column so a MATCH on
        // tokens from either part resolves.
        let raw = b"From: a@x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"B\"\r\n\
\r\n--B\r\nContent-Type: text/plain\r\n\r\nfirst body\r\n--B\r\nContent-Type: text/plain\r\n\r\nsecond body\r\n--B--\r\n";
        let body = extract_plain_body(raw);
        assert!(body.contains("first body"), "{body:?}");
        assert!(body.contains("second body"), "{body:?}");
    }

    #[test]
    fn build_search_projection_round_trips_full_envelope() {
        let raw = b"From: alice@example.com\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\nbody bytes here\r\n";
        let p = build_search_projection(&env(), raw);
        assert!(
            p.headers.contains("From: Alice@Example.COM"),
            "{}",
            p.headers
        );
        assert_eq!(p.subject, "Lunch tomorrow?");
        assert!(p.body_text.contains("body bytes here"), "{}", p.body_text);
        assert!(
            p.normalized_addrs.contains("alice@example.com"),
            "{}",
            p.normalized_addrs
        );
        assert!(
            p.normalized_addrs.contains("bob@example.com"),
            "{}",
            p.normalized_addrs
        );
    }

    #[test]
    fn build_search_projection_with_empty_body_still_indexes_envelope() {
        let p = build_search_projection(&env(), b"");
        assert_eq!(p.body_text, "");
        assert!(!p.headers.is_empty());
        assert!(!p.subject.is_empty());
        assert!(!p.normalized_addrs.is_empty());
    }

    /// DoS cap: a raw message larger than `INPUT_PARSE_LIMIT` is not
    /// parsed at all. body_text is empty; envelope columns still index.
    /// This is the hostile-peer guard (Bus wire trust boundary).
    #[test]
    fn extract_plain_body_skips_input_over_parse_limit() {
        // INPUT_PARSE_LIMIT is 8 MiB; build a buffer one byte past it.
        // Synthesise a valid text/plain message header followed by
        // body bytes so a smaller parser would happily extract the
        // body — this guarantees the empty output is from our cap,
        // not from parser rejection.
        let header = b"From: a@x\r\nContent-Type: text/plain\r\n\r\n".to_vec();
        let body_len = INPUT_PARSE_LIMIT + 1 - header.len();
        let mut raw = header;
        raw.extend(std::iter::repeat_n(b'A', body_len));
        assert_eq!(raw.len(), INPUT_PARSE_LIMIT + 1);
        let body = extract_plain_body(&raw);
        assert_eq!(body, "", "raw bytes > INPUT_PARSE_LIMIT must skip parsing");
    }

    /// DoS cap: body just under `INPUT_PARSE_LIMIT` parses, but the
    /// extracted text is truncated to `OUTPUT_BODY_LIMIT` UTF-8 bytes.
    #[test]
    fn extract_plain_body_truncates_output_at_body_limit() {
        // Synthesise a plain-text body that is OUTPUT_BODY_LIMIT * 2
        // ASCII bytes long. Parsed body should be exactly
        // OUTPUT_BODY_LIMIT bytes (ASCII = each char is 1 byte, so the
        // floor is the limit itself).
        let header = b"From: a@x\r\nContent-Type: text/plain\r\n\r\n";
        let mut raw = header.to_vec();
        raw.extend(std::iter::repeat_n(b'A', OUTPUT_BODY_LIMIT * 2));
        let body = extract_plain_body(&raw);
        assert_eq!(
            body.len(),
            OUTPUT_BODY_LIMIT,
            "extracted body must be truncated to OUTPUT_BODY_LIMIT"
        );
        assert!(
            body.bytes().all(|b| b == b'A'),
            "truncated body content must be intact"
        );
    }

    /// UTF-8 boundary: cap must never split a multi-byte codepoint.
    #[test]
    fn extract_plain_body_truncation_respects_utf8_boundary() {
        // Each '日' is 3 bytes in UTF-8. Build a body whose length in
        // bytes overshoots OUTPUT_BODY_LIMIT only by 1 — the cap is
        // forced to round DOWN to a char boundary (or off by up to 2).
        // Use a small body via the constant for the test math:
        // emit (OUTPUT_BODY_LIMIT / 3 + 1) characters = OUTPUT_BODY_LIMIT+3 bytes.
        let chars = OUTPUT_BODY_LIMIT / 3 + 1;
        let body_text: String = "日".repeat(chars);
        assert_eq!(body_text.len(), chars * 3);
        let header = b"From: a@x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n";
        let mut raw = header.to_vec();
        raw.extend_from_slice(body_text.as_bytes());
        let extracted = extract_plain_body(&raw);
        assert!(
            extracted.len() <= OUTPUT_BODY_LIMIT,
            "extracted.len()={} must be <= OUTPUT_BODY_LIMIT={}",
            extracted.len(),
            OUTPUT_BODY_LIMIT,
        );
        assert!(
            std::str::from_utf8(extracted.as_bytes()).is_ok(),
            "truncated body must still be valid UTF-8"
        );
        // Length must be a multiple of 3 (every retained char is 3 bytes).
        assert_eq!(
            extracted.len() % 3,
            0,
            "extracted body must end on a UTF-8 char boundary; got len={}",
            extracted.len()
        );
    }

    #[test]
    fn utf8_floor_returns_max_for_in_range_ascii() {
        assert_eq!(utf8_floor("hello", 3), 3);
        assert_eq!(utf8_floor("hello", 5), 5);
        assert_eq!(utf8_floor("hello", 100), 5);
    }

    #[test]
    fn utf8_floor_rounds_down_to_codepoint_boundary() {
        // "日" is 3 bytes (0xE6 0x97 0xA5). max=2 falls inside the
        // first codepoint → must round to 0. max=4 falls inside the
        // second codepoint → must round to 3.
        let s = "日日";
        assert_eq!(s.len(), 6);
        assert_eq!(utf8_floor(s, 0), 0);
        assert_eq!(utf8_floor(s, 1), 0);
        assert_eq!(utf8_floor(s, 2), 0);
        assert_eq!(utf8_floor(s, 3), 3);
        assert_eq!(utf8_floor(s, 4), 3);
        assert_eq!(utf8_floor(s, 5), 3);
        assert_eq!(utf8_floor(s, 6), 6);
    }
}
