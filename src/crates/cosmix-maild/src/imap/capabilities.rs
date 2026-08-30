//! Capability advertisement.
//!
//! The initial subset per the 2026-05-02 plan in
//! `_doc/maild/imap.md` §"Capability advertisement". Every entry must
//! correspond to behaviour that is implemented and tested before it
//! is advertised — non-negotiable invariant #11.
//!
//! Phase 1 advertised: `IMAP4rev2`, `IMAP4rev1`, `SASL-IR`,
//! `AUTH=PLAIN`, `AUTH=LOGIN`, `ID`. Phase 2 T3 added the LIST/LSUB/
//! NAMESPACE surface and the attributes it relies on: `NAMESPACE`,
//! `CHILDREN` (\HasChildren / \HasNoChildren), `SPECIAL-USE`
//! (\Inbox / \Sent / \Trash / \Drafts / \Archive / \Flagged /
//! \Junk / \All / \Important on LIST rows). Phase 2 T4 adds
//! `UNSELECT` (RFC 3691) — CLOSE is core IMAP4rev2 and needs no
//! capability; UNSELECT is a separate extension we now accept.
//!
//! Phase 4 T14 lands APPEND with both synchronizing literals (the
//! core IMAP4rev2 framing) and RFC 7888 `LITERAL+` non-synchronizing
//! literals — see [`crate::imap::codec::read_command`]. `LITERAL+`
//! is advertised now that the codec preserves stream sync across
//! oversize/malformed/refused literal rejections.
//!
//! Phase 4 T15–T17 lands COPY / MOVE / UID EXPUNGE end-to-end and
//! re-enables `UIDPLUS` (RFC 4315) + `MOVE` (RFC 6851) in
//! [`INITIAL`]. COPY emits `[COPYUID ...]`, MOVE emits the
//! RFC 6851 untagged COPYUID + EXPUNGE response, and UID EXPUNGE
//! filters by the supplied UID set as RFC 4315 §2.1 requires.
//!
//! Phase 5 T18 lands `IDLE` (RFC 2177): the dispatcher subscribes
//! to the selected mailbox's notifier channel and emits untagged
//! `EXISTS` / `EXPUNGE` updates until the client sends `DONE`.
//! Advertised once the dispatch arm is wired (see
//! [`crate::imap::op::idle`]).
//!
//! Remaining mailbox/extension capabilities (`ENABLE`, `CONDSTORE`,
//! `APPENDLIMIT=…`) are added in their respective phases as the
//! underlying commands land.

/// Currently-advertised capability strings. Order is the wire order
/// emitted in the untagged `* CAPABILITY` response and the
/// `[CAPABILITY ...]` response code following `LOGIN` / `AUTHENTICATE`.
pub const INITIAL: &[&str] = &[
    "IMAP4rev2",
    "IMAP4rev1",
    "SASL-IR",
    "AUTH=PLAIN",
    "AUTH=LOGIN",
    "ID",
    "NAMESPACE",
    "CHILDREN",
    "SPECIAL-USE",
    "UNSELECT",
    "LITERAL+",
    "UIDPLUS",
    "MOVE",
    "IDLE",
];

/// Render a capability list as the space-separated body of a
/// `* CAPABILITY` response. `caps` is typically [`INITIAL`] or the
/// operator override from [`crate::imap::config::ImapConfig::advertise_capabilities`].
pub fn render(caps: &[&str]) -> String {
    caps.join(" ")
}

/// Render an owned `&[String]` (operator override path) in the same
/// space-separated form. Capabilities whose semantics would corrupt
/// framing without the matching codec support (`LITERAL-`, the RFC
/// 7888 size-capped sibling of `LITERAL+`) are filtered out
/// regardless of what the operator wrote. `LITERAL+` is no longer
/// filtered now that [`crate::imap::codec::read_command`] honours
/// non-synchronizing literals end-to-end (Phase 4 T14). Other
/// unimplemented capabilities (e.g. `IDLE`) are *not* filtered:
/// their worst-case behaviour is a clean
/// `BAD command not implemented`, which is acceptable for the
/// "test client behaviour" use case the override is intended for.
///
/// The filtered set is logged at `warn` so a copy-paste config that
/// drops the design-doc baseline straight in is loud, not silent.
pub fn render_owned(caps: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::with_capacity(caps.len());
    for c in caps {
        let trimmed = c.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Reject entries that would split into multiple wire tokens.
        // A config value like `"LITERAL+ "` or `"foo bar"` would
        // otherwise smuggle the second token past the unsafe-cap
        // filter once the response is space-joined and re-parsed
        // client-side.
        if trimmed.split_whitespace().count() != 1 || trimmed.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            tracing::warn!(
                capability = %c,
                "dropping malformed operator-advertised capability (whitespace or control bytes)"
            );
            continue;
        }
        if is_unsafe_in_current_codec(trimmed) {
            tracing::warn!(
                capability = %c,
                "dropping operator-advertised capability that the current codec cannot honour"
            );
            continue;
        }
        kept.push(trimmed);
    }
    kept.join(" ")
}

fn is_unsafe_in_current_codec(cap: &str) -> bool {
    // ASCII case-insensitive match without allocating. `LITERAL-`
    // (RFC 7888 size-capped sibling of `LITERAL+`) is not implemented
    // yet — the codec accepts every well-formed `{N+}` up to
    // `max_literal_bytes`, but `LITERAL-` mandates a 4096-byte cap
    // and silent client-side enforcement that we do not advertise
    // or verify.
    cap.eq_ignore_ascii_case("LITERAL-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_owned_keeps_literal_plus_override() {
        // Phase 4 T14: LITERAL+ is implemented end-to-end; operator
        // overrides may now include it without being silently dropped.
        let caps = vec![
            "IMAP4rev2".to_string(),
            "LITERAL+".to_string(),
            "AUTH=PLAIN".to_string(),
        ];
        let out = render_owned(&caps);
        assert!(out.contains("LITERAL+"), "{out}");
        assert!(out.contains("IMAP4rev2"));
        assert!(out.contains("AUTH=PLAIN"));
    }

    #[test]
    fn render_owned_drops_literal_dash_override() {
        // `LITERAL-` (RFC 7888 size-capped variant) is still
        // unimplemented; the codec does not enforce its 4096-byte
        // cap, so advertising it would mislead clients.
        let caps = vec!["IMAP4rev2".to_string(), "literal-".to_string()];
        let out = render_owned(&caps);
        assert!(!out.to_ascii_uppercase().contains("LITERAL-"), "{out}");
    }

    #[test]
    fn render_owned_keeps_uidplus_now_that_copy_move_expunge_landed() {
        // Phase 4 T15–T17 shipped COPY / MOVE / UID EXPUNGE; UIDPLUS
        // is no longer filtered and is advertised in [`INITIAL`] as
        // well. APPENDUID was already emitted on every successful
        // APPEND; the capability now matches the underlying surface.
        let caps = vec!["IMAP4rev2".to_string(), "UIDPLUS".to_string()];
        let out = render_owned(&caps);
        assert!(out.to_ascii_uppercase().contains("UIDPLUS"), "{out}");
        assert!(out.contains("IMAP4rev2"));
    }

    #[test]
    fn render_owned_keeps_other_unimplemented_caps() {
        // Unimplemented caps survive the operator-override path (their
        // worst-case behaviour is a clean `BAD command not implemented`).
        // Using `IDLE` as the representative — its dispatch arm doesn't
        // exist yet but the wire shape is harmless.
        let caps = vec!["IMAP4rev2".to_string(), "IDLE".to_string()];
        let out = render_owned(&caps);
        assert!(out.contains("IDLE"));
    }

    #[test]
    fn render_owned_keeps_literal_plus_with_trailing_whitespace() {
        // Whitespace trimming is preserved; LITERAL+ is now supported
        // so a surrounding space should not corrupt the advertisement.
        let caps = vec!["IMAP4rev2".to_string(), "LITERAL+ ".to_string()];
        let out = render_owned(&caps);
        assert!(out.contains("LITERAL+"), "{out}");
    }

    #[test]
    fn render_owned_drops_literal_dash_with_leading_whitespace() {
        let caps = vec!["IMAP4rev2".to_string(), " literal-".to_string()];
        let out = render_owned(&caps);
        assert!(!out.to_ascii_uppercase().contains("LITERAL-"), "{out}");
    }

    #[test]
    fn render_owned_drops_multi_token_smuggling() {
        // A single config entry that contains internal whitespace
        // would produce two wire tokens past a naive filter — refuse
        // the whole entry rather than partially advertise it. The
        // contained `LITERAL+` token would itself be safe now, but
        // smuggling shape is what we reject here.
        let caps = vec!["IMAP4rev2".to_string(), "FOO LITERAL+".to_string()];
        let out = render_owned(&caps);
        assert!(!out.contains("LITERAL+"), "{out}");
        assert!(!out.contains("FOO"), "{out}");
    }

    #[test]
    fn render_owned_drops_control_bytes() {
        let caps = vec!["IMAP4rev2".to_string(), "AUTH=\x00PLAIN".to_string()];
        let out = render_owned(&caps);
        assert!(!out.contains('\x00'), "{out:?}");
        assert!(out.contains("IMAP4rev2"));
    }
}
